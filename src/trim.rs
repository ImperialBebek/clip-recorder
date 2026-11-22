//! Cutting a saved clip: `keyframes` finds where a stream copy may start,
//! `snap_start` lands the trim-in handle on one, and `export` writes a new
//! MP4 that copies the video bitstream untouched and folds every audio
//! track into one mixed track with the player's lane volumes baked in.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use crate::gst_util::{capsfilter, make};

/// Seconds of every video keyframe, ascending. Only demuxes and parses —
/// no decoding — so this is cheap enough to run for every opened clip.
pub fn keyframes(path: &Path) -> Result<Vec<f64>> {
    gst::init()?;
    let pipeline = gst::Pipeline::with_name("keyframes");
    let src = make("filesrc", &[("location", path_str(path)?)])?;
    let demux = make("qtdemux", &[])?;
    let parse = make("h264parse", &[])?;
    let times = Arc::new(Mutex::new(Vec::<f64>::new()));
    let sink = gst_app::AppSink::builder()
        .sync(false)
        .callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample({
                    let times = times.clone();
                    move |sink| {
                        let sample =
                            sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        if let Some(buf) = sample.buffer() {
                            if !buf.flags().contains(gst::BufferFlags::DELTA_UNIT) {
                                if let Some(t) = buf.pts().or(buf.dts()) {
                                    times.lock().unwrap().push(t.seconds_f64());
                                }
                            }
                        }
                        Ok(gst::FlowSuccess::Ok)
                    }
                })
                .build(),
        )
        .build();
    let sink_el: gst::Element = sink.clone().upcast();
    pipeline.add_many([&src, &demux, &parse, &sink_el])?;
    src.link(&demux)?;
    parse.link(&sink_el)?;
    demux.connect_pad_added({
        let parse = parse.clone();
        let pipeline = pipeline.downgrade();
        move |_, pad| {
            if pad.name().starts_with("video") {
                let _ = pad.link(&parse.static_pad("sink").unwrap());
            } else if let Some(pipeline) = pipeline.upgrade() {
                drain_pad(&pipeline, pad);
            }
        }
    });
    pipeline.set_state(gst::State::Playing)?;
    let bus = pipeline.bus().expect("pipeline has a bus");
    let result = wait_eos(&bus, 60);
    let _ = pipeline.set_state(gst::State::Null);
    result?;
    let mut times = std::mem::take(&mut *times.lock().unwrap());
    times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    times.dedup();
    Ok(times)
}

/// The last keyframe at or before `t`; before the first one it's the first
/// one (there is no earlier point a copy could start), and 0 with no index.
pub fn snap_start(keys: &[f64], t: f64) -> f64 {
    match keys.iter().rfind(|&&k| k <= t + 1e-3) {
        Some(&k) => k,
        None => keys.first().copied().unwrap_or(0.0),
    }
}

/// Writes `src`'s `start..end` range to `out`. `start` must sit on a
/// keyframe (see `snap_start`): the video packets are copied verbatim, so a
/// cut can only begin where a GOP begins. `mix[i]` is the volume and mute
/// of audio track `audio_i`, in demux order; all tracks become one "Mix"
/// track. Ends mid-GOP are fine — the demuxer just stops there.
pub fn export(
    src: &Path,
    start: f64,
    end: f64,
    mix: &[(f64, bool)],
    out: &Path,
) -> Result<()> {
    if end <= start {
        bail!("empty selection");
    }
    gst::init()?;
    let pipeline = gst::Pipeline::with_name("trim-export");
    let filesrc = make("filesrc", &[("location", path_str(src)?)])?;
    let demux = make("qtdemux", &[])?;
    let mux = make("mp4mux", &[("faststart", "true")])?;
    let sink = make("filesink", &[("location", path_str(out)?)])?;
    pipeline.add_many([&filesrc, &demux, &mux, &sink])?;
    filesrc.link(&demux)?;
    mux.link(&sink)?;

    // The audio side (mixer + encoder) is built on the first audio pad so a
    // clip without audio never hangs a mixer with nothing to mix.
    let mixer: Arc<Mutex<Option<gst::Element>>> = Arc::default();
    let video_ok = Arc::new(AtomicBool::new(false));
    demux.connect_pad_added({
        let pipeline = pipeline.downgrade();
        let mux = mux.downgrade();
        let mixer = mixer.clone();
        let video_ok = video_ok.clone();
        let mix = mix.to_vec();
        move |_, pad| {
            let name = pad.name().to_string();
            if name.starts_with("video") {
                let (Some(pipeline), Some(mux)) =
                    (pipeline.upgrade(), mux.upgrade())
                else {
                    return;
                };
                match link_video(&pipeline, pad, &mux) {
                    Ok(()) => video_ok.store(true, Ordering::SeqCst),
                    Err(e) => {
                        eprintln!("export video: {e:#}");
                        drain_pad(&pipeline, pad);
                    }
                }
            } else if let Some(i) = name
                .strip_prefix("audio_")
                .and_then(|n| n.parse::<usize>().ok())
            {
                let (Some(pipeline), Some(mux)) =
                    (pipeline.upgrade(), mux.upgrade())
                else {
                    return;
                };
                if mixer.lock().unwrap().is_none() {
                    match audio_out(&pipeline, &mux) {
                        Ok(m) => *mixer.lock().unwrap() = Some(m),
                        Err(e) => {
                            eprintln!("export audio mix: {e:#}");
                            drain_pad(&pipeline, pad);
                            return;
                        }
                    }
                }
                let linked = {
                    let locked = mixer.lock().unwrap();
                    locked.as_ref().map(|m| {
                        let (vol, mute) = mix.get(i).copied().unwrap_or((1.0, false));
                        link_audio(&pipeline, pad, m, vol, mute)
                    })
                };
                match linked {
                    Some(Ok(())) => {}
                    Some(Err(e)) => {
                        eprintln!("export {name}: {e:#}");
                        drain_pad(&pipeline, pad);
                    }
                    // The mix chain failed earlier; still drain the pad.
                    None => drain_pad(&pipeline, pad),
                }
            } else if let Some(pipeline) = pipeline.upgrade() {
                drain_pad(&pipeline, pad);
            }
        }
    });

    let result = (|| -> Result<()> {
        pipeline.set_state(gst::State::Paused)?;
        // The demuxer's pads appear while prerolling; the pause settling
        // means every branch is linked.
        let (r, ..) = pipeline.state(gst::ClockTime::from_seconds(15));
        r?;
        if !video_ok.load(Ordering::SeqCst) {
            bail!("clip has no video stream to copy");
        }
        // The demuxer is the element that actually serves the range; seeking
        // it directly keeps the event off the mux and sinks.
        demux
            .seek(
                1.0,
                gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                gst::SeekType::Set,
                gst::ClockTime::from_seconds_f64(start),
                gst::SeekType::Set,
                gst::ClockTime::from_seconds_f64(end),
            )
            .map_err(|e| anyhow!("seek failed: {e}"))?;
        // Wait out the flush and re-preroll, then run the segment to its end.
        let bus = pipeline.bus().expect("pipeline has a bus");
        loop {
            let Some(msg) = bus.timed_pop_filtered(
                gst::ClockTime::from_seconds(15),
                &[gst::MessageType::AsyncDone, gst::MessageType::Error],
            ) else {
                bail!("timed out re-prerolling after the seek");
            };
            match msg.view() {
                gst::MessageView::AsyncDone(_) => break,
                gst::MessageView::Error(e) => bail!("{}", e.error()),
                _ => {}
            }
        }
        pipeline.set_state(gst::State::Playing)?;
        wait_eos(&bus, (60.0 + 3.0 * (end - start)) as u64)
    })();
    let _ = pipeline.set_state(gst::State::Null);
    if result.is_err() {
        let _ = std::fs::remove_file(out);
    }
    result
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str().context("non-UTF-8 clip path")
}

fn wait_eos(bus: &gst::Bus, timeout_secs: u64) -> Result<()> {
    loop {
        let Some(msg) = bus.timed_pop_filtered(
            gst::ClockTime::from_seconds(timeout_secs),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        ) else {
            bail!("timed out writing the clip");
        };
        match msg.view() {
            gst::MessageView::Eos(_) => return Ok(()),
            gst::MessageView::Error(e) => {
                bail!(
                    "{} ({})",
                    e.error(),
                    e.debug().map(|d| d.to_string()).unwrap_or_default()
                )
            }
            _ => {}
        }
    }
}

/// Streams the demuxer emits that nobody needs still have to be consumed,
/// or pushing on their unlinked pad stalls the whole demuxer.
fn drain_pad(pipeline: &gst::Pipeline, pad: &gst::Pad) {
    let Ok(fake) = make("fakesink", &[("sync", "false"), ("async", "false")]) else {
        return;
    };
    if pipeline.add(&fake).is_ok() {
        let _ = fake.sync_state_with_parent();
        let _ = pad.link(&fake.static_pad("sink").unwrap());
    }
}

/// demux video pad -> queue -> parse -> mux. The queue gives the branch its
/// own thread; without it the muxer's pad wait would block the demuxer.
fn link_video(pipeline: &gst::Pipeline, pad: &gst::Pad, mux: &gst::Element) -> Result<()> {
    let queue = make("queue", &[])?;
    let parse = make("h264parse", &[])?;
    pipeline.add_many([&queue, &parse])?;
    queue.link(&parse)?;
    parse.link(mux).context("mp4mux does not accept the video")?;
    pad.link(&queue.static_pad("sink").unwrap())?;
    for e in [&queue, &parse] {
        e.sync_state_with_parent()?;
    }
    Ok(())
}

/// mixer -> normalize -> encoder -> title tag -> mux, matching what the
/// recorder itself writes. Returns the mixer the branches plug into.
fn audio_out(pipeline: &gst::Pipeline, mux: &gst::Element) -> Result<gst::Element> {
    let mixer = make("audiomixer", &[])?;
    let conv = make("audioconvert", &[])?;
    let resample = make("audioresample", &[])?;
    let caps = capsfilter("audio/x-raw,rate=48000,channels=2")?;
    let mut chain = vec![mixer.clone(), conv, resample, caps];
    chain.extend(crate::capture::audio_encoder()?);
    chain.push(make("taginject", &[("tags", "title=Mix")])?);
    pipeline.add_many(&chain)?;
    gst::Element::link_many(&chain)?;
    chain
        .last()
        .unwrap()
        .link(mux)
        .context("mp4mux does not accept the mixed audio")?;
    for e in &chain {
        e.sync_state_with_parent()?;
    }
    Ok(mixer)
}

/// One demuxer audio pad decoded, normalized to 48 kHz stereo and fed to
/// its own mixer slot through a `volume` carrying the lane's settings.
fn link_audio(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    mixer: &gst::Element,
    volume: f64,
    muted: bool,
) -> Result<()> {
    let queue = make("queue", &[])?;
    let decode = make("decodebin", &[])?;
    let conv = make("audioconvert", &[])?;
    let resample = make("audioresample", &[])?;
    let caps = capsfilter("audio/x-raw,rate=48000,channels=2")?;
    let vol = make(
        "volume",
        &[
            ("volume", &format!("{volume}")),
            ("mute", if muted { "true" } else { "false" }),
        ],
    )?;
    pipeline.add_many([&queue, &decode, &conv, &resample, &caps, &vol])?;
    queue.link(&decode)?;
    gst::Element::link_many([&conv, &resample, &caps, &vol])?;
    decode.connect_pad_added({
        let conv = conv.clone();
        move |_, pad| {
            let _ = pad.link(&conv.static_pad("sink").unwrap());
        }
    });
    let sink = mixer
        .request_pad_simple("sink_%u")
        .context("audiomixer has no pad")?;
    vol.static_pad("src").unwrap().link(&sink)?;
    pad.link(&queue.static_pad("sink").unwrap())?;
    for e in [&queue, &decode, &conv, &resample, &caps, &vol] {
        e.sync_state_with_parent()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snap_start_picks_the_keyframe_at_or_before() {
        let keys = [0.0, 1.0, 2.0, 3.0];
        assert_eq!(snap_start(&keys, 2.0), 2.0, "exact hit");
        assert_eq!(snap_start(&keys, 2.7), 2.0, "between keyframes");
        assert_eq!(snap_start(&keys, 2.0 + 0.0005), 2.0, "within epsilon");
        assert_eq!(snap_start(&keys, 9.9), 3.0, "past the end");
    }

    #[test]
    fn snap_start_edges() {
        assert_eq!(snap_start(&[1.0, 2.0], 0.5), 1.0, "before the first");
        assert_eq!(snap_start(&[], 1.5), 0.0, "empty index");
    }

    /// Demuxes `path` enough to count streams and grab the first video
    /// sample after `seek`; the output side of a byte-identity check.
    fn probe_clip(path: &Path, seek: Option<f64>) -> Result<(Vec<u8>, usize, usize)> {
        gst::init()?;
        let pipeline = gst::Pipeline::new();
        let src = make("filesrc", &[("location", path_str(path)?)])?;
        let demux = make("qtdemux", &[])?;
        let sink = gst_app::AppSink::builder().sync(false).build();
        let sink_el: gst::Element = sink.clone().upcast();
        let counts = Arc::new(Mutex::new((0usize, 0usize)));
        pipeline.add_many([&src, &demux, &sink_el])?;
        src.link(&demux)?;
        demux.connect_pad_added({
            let sink = sink_el.clone();
            let counts = counts.clone();
            let pipeline = pipeline.downgrade();
            move |_, pad| {
                let name = pad.name().to_string();
                if name.starts_with("video") {
                    counts.lock().unwrap().0 += 1;
                    let _ = pad.link(&sink.static_pad("sink").unwrap());
                } else {
                    if name.starts_with("audio") {
                        counts.lock().unwrap().1 += 1;
                    }
                    if let Some(pipeline) = pipeline.upgrade() {
                        drain_pad(&pipeline, pad);
                    }
                }
            }
        });
        pipeline.set_state(gst::State::Paused)?;
        let (r, ..) = pipeline.state(gst::ClockTime::from_seconds(10));
        r?;
        if let Some(t) = seek {
            pipeline
                .seek_simple(
                    gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                    gst::ClockTime::from_seconds_f64(t),
                )
                .map_err(|e| anyhow!("seek failed: {e}"))?;
            let bus = pipeline.bus().expect("pipeline has a bus");
            let _ = bus.timed_pop_filtered(
                gst::ClockTime::from_seconds(10),
                &[gst::MessageType::AsyncDone, gst::MessageType::Error],
            );
        }
        pipeline.set_state(gst::State::Playing)?;
        let sample = sink
            .try_pull_sample(gst::ClockTime::from_seconds(10))
            .context("no video sample")?;
        let buf = sample.buffer().context("empty sample")?;
        let bytes = buf.map_readable()?.as_slice().to_vec();
        let counts = *counts.lock().unwrap();
        let _ = pipeline.set_state(gst::State::Null);
        Ok((bytes, counts.0, counts.1))
    }

    #[test]
    #[ignore = "needs a generated clip"]
    fn exports_a_trimmed_copy() {
        let src = Path::new("/tmp/clip-play/test.mp4");
        let src_dur = crate::clips::mp4_seconds(src).unwrap();
        let keys = keyframes(src).unwrap();
        assert!(!keys.is_empty(), "no keyframes found");
        let start = snap_start(&keys, 2.3);
        // The fixture is ~5.7 s, so a mid-clip cut exercises the stop path.
        let end = src_dur - 1.0;
        let out = std::env::temp_dir().join("clip-recorder-trim-test.mp4");
        let _ = std::fs::remove_file(&out);
        export(src, start, end, &[(1.0, false), (0.5, true)], &out).unwrap();

        let secs = crate::clips::mp4_seconds(&out).unwrap();
        assert!(
            (secs - (end - start)).abs() < 0.15,
            "duration {secs} != {}",
            end - start
        );
        let (first, videos, audios) = probe_clip(&out, None).unwrap();
        assert_eq!((videos, audios), (1, 1), "expected 1 video + 1 mixed audio");
        // The first stored packet is the keyframe packet, unmolested:
        // identical bytes means the video was copied, not re-encoded.
        let (src_first, ..) = probe_clip(src, Some(start)).unwrap();
        assert_eq!(first, src_first, "first video packet differs from source");
        std::fs::remove_file(&out).unwrap();
    }

    #[test]
    #[ignore = "needs a generated clip"]
    fn exports_the_whole_clip() {
        let src = Path::new("/tmp/clip-play/test.mp4");
        let src_dur = crate::clips::mp4_seconds(src).unwrap();
        let out = std::env::temp_dir().join("clip-recorder-trim-full.mp4");
        let _ = std::fs::remove_file(&out);
        export(src, 0.0, src_dur, &[(1.0, false), (1.0, false)], &out).unwrap();
        let secs = crate::clips::mp4_seconds(&out).unwrap();
        assert!(
            (secs - src_dur).abs() < 0.15,
            "duration {secs} != {src_dur}"
        );
        std::fs::remove_file(&out).unwrap();
    }
}
