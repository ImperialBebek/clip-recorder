//! Clip playback for the library's built-in player: a filesrc/qtdemux
//! pipeline with one decoded branch per MP4 track, plus the offline
//! analysis (waveform peaks, filmstrip frames) the timeline lanes show.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use crate::gst_util::{capsfilter, has, make};

pub struct Player {
    pipeline: gst::Pipeline,
    track_volumes: Arc<Mutex<Vec<gst::Element>>>,
    master: gst::Element,
    playing: Arc<AtomicBool>,
    eos: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    seek_state: Arc<Mutex<(bool, Option<gst::ClockTime>)>>,
}

impl Player {
    /// Starts paused at 0; `on_frame` gets every decoded frame (also preroll
    /// frames, so seeking while paused updates the picture); the copy only
    /// happens while `want_frame` says a new frame is needed (preroll frames
    /// always come through); `on_tracks` is called once with the audio track
    /// names, in audio_N order, when the pipeline has prerolled; `on_error` is
    /// called once with the first pipeline error, if any.
    pub fn open(
        path: &Path,
        on_frame: impl Fn(slint::SharedPixelBuffer<slint::Rgba8Pixel>) + Send + Sync + 'static,
        want_frame: impl Fn() -> bool + Send + Sync + 'static,
        on_tracks: impl FnOnce(Vec<String>) + Send + 'static,
        on_error: impl FnOnce(String) + Send + 'static,
    ) -> Result<Self> {
        gst::init()?;
        let pipeline = gst::Pipeline::with_name("player");
        let src = make("filesrc", &[("location", path_str(path)?)])?;
        let demux = make("qtdemux", &[])?;
        let mixer = make("audiomixer", &[])?;
        let master = make("volume", &[])?;
        let audiosink = make("autoaudiosink", &[("sync", "true")])?;
        pipeline.add_many([&src, &demux, &mixer, &master, &audiosink])?;
        src.link(&demux)?;
        gst::Element::link_many([&mixer, &master, &audiosink])?;

        let track_volumes: Arc<Mutex<Vec<gst::Element>>> = Arc::default();
        let titles: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
        let playing = Arc::new(AtomicBool::new(false));
        let eos = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let seek_state: Arc<Mutex<(bool, Option<gst::ClockTime>)>> = Arc::default();
        let on_frame = Arc::new(on_frame);
        let want_frame = Arc::new(want_frame);

        demux.connect_pad_added({
            let pipeline = pipeline.downgrade();
            let mixer = mixer.downgrade();
            let track_volumes = track_volumes.clone();
            let titles = titles.clone();
            let on_frame = on_frame.clone();
            let want_frame = want_frame.clone();
            move |_, pad| {
                let name = pad.name().to_string();
                if name.starts_with("video") {
                    let Some(pipeline) = pipeline.upgrade() else {
                        return;
                    };
                    if let Err(e) = add_video(&pipeline, pad, on_frame.clone(), want_frame.clone())
                    {
                        eprintln!("player video: {e:#}");
                    }
                } else if let Some(i) = name
                    .strip_prefix("audio_")
                    .and_then(|n| n.parse::<usize>().ok())
                {
                    titles.lock().unwrap().resize(i + 1, None);
                    // The per-stream title tag names the lane.
                    pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, {
                        let titles = titles.clone();
                        move |_, info| {
                            if let Some(gst::PadProbeData::Event(ev)) = &info.data {
                                if let gst::EventView::Tag(tag) = ev.view() {
                                    let tags = tag.tag();
                                    if tags.scope() == gst::TagScope::Stream {
                                        if let Some(t) = tags.get::<gst::tags::Title>() {
                                            titles.lock().unwrap()[i] = Some(t.get().to_owned());
                                        }
                                    }
                                }
                            }
                            gst::PadProbeReturn::Ok
                        }
                    });
                    let (Some(pipeline), Some(mixer)) = (pipeline.upgrade(), mixer.upgrade())
                    else {
                        return;
                    };
                    if let Err(e) = add_audio(&pipeline, pad, &mixer, &track_volumes) {
                        eprintln!("player audio: {e:#}");
                    }
                }
            }
        });

        // A clip without audio leaves the mixer's sink pads unlinked and it
        // would never preroll; take the whole audio side out then.
        demux.connect_no_more_pads({
            let pipeline = pipeline.downgrade();
            let track_volumes = track_volumes.clone();
            let tail = [mixer.downgrade(), master.downgrade(), audiosink.downgrade()];
            move |_| {
                if !track_volumes.lock().unwrap().is_empty() {
                    return;
                }
                let Some(pipeline) = pipeline.upgrade() else {
                    return;
                };
                let tail: Vec<gst::Element> = tail.iter().filter_map(|w| w.upgrade()).collect();
                for e in &tail {
                    let _ = e.set_state(gst::State::Null);
                }
                let _ = pipeline.remove_many(&tail);
            }
        });

        pipeline.set_state(gst::State::Paused)?;

        let mut on_tracks = Some(on_tracks);
        let mut on_error = Some(on_error);
        std::thread::spawn({
            let bus = pipeline.bus().expect("pipeline has a bus");
            let titles = titles.clone();
            let playing = playing.clone();
            let eos = eos.clone();
            let alive = alive.clone();
            let seek_state = seek_state.clone();
            let pipeline = pipeline.downgrade();
            move || {
                while alive.load(Ordering::SeqCst) {
                    let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) else {
                        continue;
                    };
                    match msg.view() {
                        gst::MessageView::AsyncDone(_) => {
                            if let Some(f) = on_tracks.take() {
                                f(track_names(&titles.lock().unwrap()));
                            }
                            // A finished seek may release the queued one.
                            let next = {
                                let mut st = seek_state.lock().unwrap();
                                if st.0 {
                                    match st.1.take() {
                                        Some(pos) => Some(pos),
                                        None => {
                                            st.0 = false;
                                            None
                                        }
                                    }
                                } else {
                                    None
                                }
                            };
                            if let (Some(pos), Some(p)) = (next, pipeline.upgrade()) {
                                issue_seek(&p, &seek_state, pos);
                            }
                        }
                        gst::MessageView::Eos(_) => {
                            eos.store(true, Ordering::SeqCst);
                            playing.store(false, Ordering::SeqCst);
                            if let Some(p) = pipeline.upgrade() {
                                let _ = p.set_state(gst::State::Paused);
                            }
                        }
                        gst::MessageView::Error(e) => {
                            eprintln!("player: {}", e.error());
                            eos.store(true, Ordering::SeqCst);
                            playing.store(false, Ordering::SeqCst);
                            if let Some(f) = on_error.take() {
                                f(e.error().to_string());
                            }
                        }
                        _ => {}
                    }
                }
            }
        });

        Ok(Self {
            pipeline,
            track_volumes,
            master,
            playing,
            eos,
            alive,
            seek_state,
        })
    }

    pub fn play(&self) {
        if self.eos.swap(false, Ordering::SeqCst) {
            self.seek(gst::ClockTime::ZERO);
        }
        let _ = self.pipeline.set_state(gst::State::Playing);
        self.playing.store(true, Ordering::SeqCst);
    }

    pub fn pause(&self) {
        let _ = self.pipeline.set_state(gst::State::Paused);
        self.playing.store(false, Ordering::SeqCst);
    }

    pub fn playing(&self) -> bool {
        self.playing.load(Ordering::SeqCst)
    }

    pub fn ended(&self) -> bool {
        self.eos.load(Ordering::SeqCst)
    }

    /// Always FLUSH | ACCURATE. Seeks are coalesced: while one is in flight
    /// (waiting for AsyncDone) only the latest target is kept, so scrubbing
    /// can't pile up flushes.
    pub fn seek(&self, pos: gst::ClockTime) {
        self.eos.store(false, Ordering::SeqCst);
        let mut st = self.seek_state.lock().unwrap();
        if st.0 {
            st.1 = Some(pos);
            return;
        }
        st.0 = true;
        drop(st);
        issue_seek(&self.pipeline, &self.seek_state, pos);
    }

    /// A seek is in flight or queued.
    pub fn seeking(&self) -> bool {
        let st = self.seek_state.lock().unwrap();
        st.0 || st.1.is_some()
    }

    pub fn position(&self) -> Option<gst::ClockTime> {
        self.pipeline.query_position()
    }

    pub fn duration(&self) -> Option<gst::ClockTime> {
        self.pipeline.query_duration()
    }

    pub fn set_volume(&self, v: f64) {
        self.master.set_property("volume", v);
    }

    pub fn set_muted(&self, m: bool) {
        self.master.set_property("mute", m);
    }

    pub fn set_track_muted(&self, audio_index: usize, muted: bool) {
        if let Some(vol) = self.track_volumes.lock().unwrap().get(audio_index) {
            vol.set_property("mute", muted);
        }
    }

    pub fn set_track_volume(&self, audio_index: usize, v: f64) {
        if let Some(vol) = self.track_volumes.lock().unwrap().get(audio_index) {
            vol.set_property("volume", v);
        }
    }

    #[cfg(test)]
    fn pipeline_weak(&self) -> gst::glib::object::WeakRef<gst::Pipeline> {
        self.pipeline.downgrade()
    }
}

fn issue_seek(
    pipeline: &gst::Pipeline,
    state: &Mutex<(bool, Option<gst::ClockTime>)>,
    pos: gst::ClockTime,
) {
    if pipeline
        .seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, pos)
        .is_err()
    {
        state.lock().unwrap().0 = false;
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Peak per bin (0..1) of audio track `audio_index`, decoded to mono F32.
pub fn waveform(path: &Path, audio_index: usize, bins: usize) -> Result<Vec<f32>> {
    gst::init()?;
    let pipeline = gst::Pipeline::new();
    let src = make("filesrc", &[("location", path_str(path)?)])?;
    let demux = make("qtdemux", &[])?;
    let decode = make("decodebin", &[])?;
    let conv = make("audioconvert", &[])?;
    let caps = capsfilter("audio/x-raw,format=F32LE,channels=1")?;
    let samples = Arc::new(Mutex::new(Vec::<f32>::new()));
    let sink = gst_app::AppSink::builder()
        .sync(false)
        .callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample({
                    let samples = samples.clone();
                    move |sink| {
                        let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        if let Some(map) = sample.buffer().and_then(|b| b.map_readable().ok()) {
                            let mut lock = samples.lock().unwrap();
                            lock.extend(
                                map.as_slice()
                                    .as_chunks::<4>()
                                    .0
                                    .iter()
                                    .map(|c| f32::from_le_bytes(*c)),
                            );
                        }
                        Ok(gst::FlowSuccess::Ok)
                    }
                })
                .build(),
        )
        .build();
    let sink_el: gst::Element = sink.clone().upcast();
    pipeline.add_many([&src, &demux, &decode, &conv, &caps, &sink_el])?;
    src.link(&demux)?;
    gst::Element::link_many([&conv, &caps, &sink_el])?;
    decode.connect_pad_added({
        let conv = conv.clone();
        move |_, pad| {
            let _ = pad.link(&conv.static_pad("sink").unwrap());
        }
    });
    let want = format!("audio_{audio_index}");
    let linked = Arc::new(AtomicBool::new(false));
    demux.connect_pad_added({
        let decode = decode.clone();
        let linked = linked.clone();
        move |_, pad| {
            if pad.name() == want && pad.link(&decode.static_pad("sink").unwrap()).is_ok() {
                linked.store(true, Ordering::SeqCst);
            }
        }
    });
    pipeline.set_state(gst::State::Playing)?;
    let bus = pipeline.bus().expect("pipeline has a bus");
    let result = loop {
        let Some(msg) = bus.timed_pop_filtered(
            gst::ClockTime::from_seconds(60),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        ) else {
            break Err(anyhow!("timed out decoding audio"));
        };
        match msg.view() {
            gst::MessageView::Eos(_) => break Ok(()),
            gst::MessageView::Error(e) => break Err(anyhow!("{}", e.error())),
            _ => {}
        }
    };
    let _ = pipeline.set_state(gst::State::Null);
    result?;
    if !linked.load(Ordering::SeqCst) {
        bail!("no audio track {audio_index}");
    }
    let samples = samples.lock().unwrap();
    let n = samples.len();
    if n == 0 {
        return Ok(vec![0.0; bins]);
    }
    Ok((0..bins)
        .map(|b| {
            let (lo, hi) = (b * n / bins, ((b + 1) * n / bins).max(b * n / bins + 1));
            samples[lo..hi.min(n)]
                .iter()
                .fold(0.0f32, |m, &v| m.max(v.abs()))
        })
        .collect())
}

/// `count` evenly spaced frames, `w`x`h` RGB (paused preroll plus KEY_UNIT
/// seeks, the same trick the thumbnailer uses).
pub fn filmstrip(
    path: &Path,
    count: usize,
    w: u32,
    h: u32,
) -> Result<Vec<slint::SharedPixelBuffer<slint::Rgb8Pixel>>> {
    gst::init()?;
    let pipeline = gst::parse::launch(&format!(
        "filesrc name=src ! decodebin ! videoconvert ! videoscale \
         ! video/x-raw,format=RGB,width={w},height={h} \
         ! appsink name=sink sync=false max-buffers=1 drop=true"
    ))?
    .downcast::<gst::Pipeline>()
    .map_err(|_| anyhow!("not a pipeline"))?;
    pipeline
        .by_name("src")
        .context("no filesrc")?
        .set_property("location", path_str(path)?);
    let sink = pipeline
        .by_name("sink")
        .and_downcast::<gst_app::AppSink>()
        .context("no appsink")?;
    pipeline.set_state(gst::State::Paused)?;
    let (r, ..) = pipeline.state(gst::ClockTime::from_seconds(5));
    r?;
    let dur = pipeline
        .query_duration::<gst::ClockTime>()
        .context("clip has no duration")?;
    pipeline.set_state(gst::State::Playing)?;
    let mut frames = Vec::new();
    for i in 0..count {
        let pos = gst::ClockTime::from_seconds_f64(
            dur.seconds_f64() * (2 * i + 1) as f64 / (2 * count) as f64,
        );
        if pipeline
            .seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT, pos)
            .is_err()
        {
            continue;
        }
        let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_seconds(5)) else {
            continue;
        };
        if let Some(frame) = rgb_frame(&sample) {
            frames.push(frame);
        }
    }
    let _ = pipeline.set_state(gst::State::Null);
    Ok(frames)
}

/// Display names: a track's own `title` tag when present, else positional:
/// 1 track → "Audio", exactly 2 → "Game","Mic" (clips saved before tagging),
/// otherwise "Audio N".
fn track_names(titles: &[Option<String>]) -> Vec<String> {
    titles
        .iter()
        .enumerate()
        .map(|(i, t)| {
            t.clone().unwrap_or_else(|| match titles.len() {
                1 => "Audio".into(),
                2 => ["Game", "Mic"][i].into(),
                _ => format!("Audio {}", i + 1),
            })
        })
        .collect()
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str().context("non-UTF-8 clip path")
}

/// Decodes one demuxer pad to the on-screen RGBA appsink.
fn add_video(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    on_frame: Arc<impl Fn(slint::SharedPixelBuffer<slint::Rgba8Pixel>) + Send + Sync + 'static>,
    want_frame: Arc<impl Fn() -> bool + Send + Sync + 'static>,
) -> Result<()> {
    let queue = make("queue", &[])?;
    let decode = make("decodebin", &[])?;
    let conv = if has("vapostproc") {
        make("vapostproc", &[])?
    } else {
        make("videoconvert", &[("n-threads", "0")])?
    };
    // RGBx: vapostproc writes garbage into the alpha byte of RGBA.
    let caps = capsfilter("video/x-raw,format=RGBx")?;
    let grab = {
        let on_frame = on_frame.clone();
        let want_frame = want_frame.clone();
        move |sink: &gst_app::AppSink| frame(sink, false, &*on_frame, &*want_frame)
    };
    let sink = gst_app::AppSink::builder()
        .sync(true)
        .max_buffers(1)
        .drop(true)
        .callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(grab.clone())
                .new_preroll(move |sink| frame(sink, true, &*on_frame, &*want_frame))
                .build(),
        )
        .build();
    let sink_el: gst::Element = sink.clone().upcast();
    pipeline.add_many([&queue, &decode, &conv, &caps, &sink_el])?;
    queue.link(&decode)?;
    gst::Element::link_many([&conv, &caps, &sink_el])?;
    decode.connect_pad_added({
        let conv = conv.clone();
        move |_, pad| {
            let _ = pad.link(&conv.static_pad("sink").unwrap());
        }
    });
    pad.link(&queue.static_pad("sink").unwrap())?;
    for e in [&queue, &decode, &conv, &caps, &sink_el] {
        e.sync_state_with_parent()?;
    }
    Ok(())
}

/// Decodes one demuxer pad into its own mixer slot, through a per-track
/// `volume` element so the lane can be muted alone.
fn add_audio(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    mixer: &gst::Element,
    volumes: &Arc<Mutex<Vec<gst::Element>>>,
) -> Result<()> {
    let queue = make("queue", &[])?;
    let decode = make("decodebin", &[])?;
    let conv = make("audioconvert", &[])?;
    let resample = make("audioresample", &[])?;
    let vol = make("volume", &[])?;
    pipeline.add_many([&queue, &decode, &conv, &resample, &vol])?;
    queue.link(&decode)?;
    gst::Element::link_many([&conv, &resample, &vol])?;
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
    for e in [&queue, &decode, &conv, &resample, &vol] {
        e.sync_state_with_parent()?;
    }
    volumes.lock().unwrap().push(vol);
    Ok(())
}

fn frame(
    sink: &gst_app::AppSink,
    preroll: bool,
    on_frame: &impl Fn(slint::SharedPixelBuffer<slint::Rgba8Pixel>),
    want_frame: &impl Fn() -> bool,
) -> Result<gst::FlowSuccess, gst::FlowError> {
    let sample = if preroll {
        sink.pull_preroll()
    } else {
        sink.pull_sample()
    }
    .map_err(|_| gst::FlowError::Eos)?;
    // Preroll frames always update the picture; played frames are only copied
    // when the UI can take another one.
    if preroll || want_frame() {
        if let Some(frame) = rgba_frame(&sample) {
            on_frame(frame);
        }
    }
    Ok(gst::FlowSuccess::Ok)
}

fn rgba_frame(sample: &gst::Sample) -> Option<slint::SharedPixelBuffer<slint::Rgba8Pixel>> {
    let (w, h) = size(sample)?;
    let map = sample.buffer()?.map_readable().ok()?;
    if map.size() < stride(w, 4) * h {
        return None;
    }
    let mut out = slint::SharedPixelBuffer::new(w as u32, h as u32);
    let (src, dst, row) = (map.as_slice(), out.make_mut_bytes(), w * 4);
    for r in 0..h {
        let d = &mut dst[r * row..(r + 1) * row];
        d.copy_from_slice(&src[r * stride(w, 4)..r * stride(w, 4) + row]);
        // RGBx leaves alpha undefined; opaque beats whatever the driver left.
        for px in d.as_chunks_mut::<4>().0 {
            px[3] = 255;
        }
    }
    Some(out)
}

fn rgb_frame(sample: &gst::Sample) -> Option<slint::SharedPixelBuffer<slint::Rgb8Pixel>> {
    let (w, h) = size(sample)?;
    let map = sample.buffer()?.map_readable().ok()?;
    if map.size() < stride(w, 3) * h {
        return None;
    }
    let mut out = slint::SharedPixelBuffer::new(w as u32, h as u32);
    copy_rows(map.as_slice(), out.make_mut_bytes(), w, h, 3);
    Some(out)
}

/// appsink gets the default unpacked layout (no VideoMeta): 4-aligned rows.
fn stride(w: usize, bpp: usize) -> usize {
    (w * bpp + 3) & !3
}

fn size(sample: &gst::Sample) -> Option<(usize, usize)> {
    let s = sample.caps()?.structure(0)?;
    let (w, h) = (
        s.get::<i32>("width").ok()? as usize,
        s.get::<i32>("height").ok()? as usize,
    );
    (w > 0 && h > 0).then_some((w, h))
}

/// Rows can carry padding beyond `w * bpp` bytes; copy row by row when the
/// buffer isn't tightly packed.
fn copy_rows(src: &[u8], dst: &mut [u8], w: usize, h: usize, bpp: usize) {
    let (row, stride) = (w * bpp, stride(w, bpp));
    if stride == row {
        dst.copy_from_slice(&src[..row * h]);
    } else {
        for r in 0..h {
            dst[r * row..(r + 1) * row].copy_from_slice(&src[r * stride..r * stride + row]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_names_prefers_tags_and_falls_back_positionally() {
        let tagged = [Some("Game".into()), Some("Mic".into())];
        assert_eq!(track_names(&tagged), ["Game", "Mic"]);
        let mixed = [Some("Game".into()), None];
        assert_eq!(track_names(&mixed), ["Game", "Mic"]);
        assert_eq!(track_names(&[None]), ["Audio"]);
        assert_eq!(track_names(&[None, None]), ["Game", "Mic"]);
        assert_eq!(
            track_names(&[None, None, None]),
            ["Audio 1", "Audio 2", "Audio 3"]
        );
    }

    #[test]
    #[ignore = "needs a generated clip"]
    fn peaks_and_frames() {
        let clip = Path::new("/tmp/clip-play/test.mp4");
        let game = waveform(clip, 0, 600).unwrap();
        let mic = waveform(clip, 1, 600).unwrap();
        assert_eq!(game.len(), 600);
        assert_eq!(mic.len(), 600);
        assert!(game.iter().any(|&p| p > 0.01), "game track silent");
        assert!(mic.iter().any(|&p| p > 0.01), "mic track silent");
        let frames = filmstrip(clip, 10, 160, 90).unwrap();
        assert!(!frames.is_empty());
        assert_eq!(frames[0].size().width, 160);
    }

    #[test]
    #[ignore = "needs generated + real clips"]
    fn reports_track_names() {
        for (path, want) in [
            ("/tmp/clip-play/test.mp4", vec!["Game", "Mic"]),
            ("/tmp/clip-play/real.mp4", vec!["Audio"]),
        ] {
            let (tx, rx) = std::sync::mpsc::channel();
            let player = Player::open(
                Path::new(path),
                |_| {},
                || true,
                move |names| {
                    eprintln!("{path}: {names:?}");
                    tx.send(names).unwrap();
                },
                |_| {},
            )
            .unwrap();
            assert_eq!(
                rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap(),
                want
            );
            eprintln!("{path}: duration {:?}", player.duration());
            drop(player);
        }
    }

    #[test]
    #[ignore = "needs clips on disk; measures playback"]
    fn plays_both_clips() {
        use std::sync::atomic::{AtomicU64, Ordering};
        for path in ["/tmp/clip-play/test.mp4", "/tmp/clip-play/real.mp4"] {
            let frames = Arc::new(AtomicU64::new(0));
            let on_frame = {
                let frames = frames.clone();
                move |_| {
                    frames.fetch_add(1, Ordering::Relaxed);
                }
            };
            let player = Player::open(Path::new(path), on_frame, || true, |_| {}, |_| {}).unwrap();
            eprintln!("{path}: duration {:?}", player.duration());
            player.play();
            std::thread::sleep(std::time::Duration::from_secs(8));
            let pos = player.position().unwrap_or_default();
            eprintln!(
                "{path}: {} frames, pos {pos:?}, playing {}",
                frames.load(Ordering::Relaxed),
                player.playing()
            );
            assert!(pos > gst::ClockTime::ZERO);
            assert!(frames.load(Ordering::Relaxed) > 0);
            player.pause();
            drop(player);
        }
    }

    #[test]
    #[ignore = "needs a generated clip"]
    fn dropped_pipeline_is_freed() {
        for _ in 0..5 {
            let player = Player::open(
                Path::new("/tmp/clip-play/test.mp4"),
                |_| {},
                || true,
                |_| {},
                |_| {},
            )
            .unwrap();
            let weak = player.pipeline_weak();
            drop(player);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
            while weak.upgrade().is_some() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert!(weak.upgrade().is_none(), "pipeline outlived its Player");
        }
    }

    #[test]
    #[ignore = "needs a real clip; writes /tmp/player_frame.png"]
    fn preroll_frame_is_opaque() {
        let (tx, rx) = std::sync::mpsc::channel();
        let player = Player::open(
            Path::new("/tmp/clip-play/real.mp4"),
            move |buf| {
                let _ = tx.send(buf);
            },
            || true,
            |_| {},
            |_| {},
        )
        .unwrap();
        let buf = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("no preroll frame");
        assert!(
            buf.as_bytes().chunks_exact(4).all(|px| px[3] == 255),
            "frame has non-opaque alpha"
        );
        let (w, h) = (buf.size().width, buf.size().height);
        let pipeline = gst::parse::launch(
            "appsrc name=src ! video/x-raw,format=RGBA ! videoconvert ! pngenc \
             ! filesink name=sink",
        )
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let src = pipeline
            .by_name("src")
            .unwrap()
            .downcast::<gst_app::AppSrc>()
            .unwrap();
        src.set_caps(Some(
            &gst::Caps::builder("video/x-raw")
                .field("format", "RGBA")
                .field("width", w as i32)
                .field("height", h as i32)
                .build(),
        ));
        pipeline
            .by_name("sink")
            .unwrap()
            .set_property("location", "/tmp/player_frame.png");
        pipeline.set_state(gst::State::Playing).unwrap();
        let mut gstbuf = gst::Buffer::with_size(buf.as_bytes().len()).unwrap();
        {
            let mut map = gstbuf.get_mut().unwrap().map_writable().unwrap();
            map.copy_from_slice(buf.as_bytes());
        }
        src.push_buffer(gstbuf).unwrap();
        src.end_of_stream().unwrap();
        let bus = pipeline.bus().unwrap();
        for msg in bus.iter_timed(gst::ClockTime::from_seconds(10)) {
            if matches!(
                msg.view(),
                gst::MessageView::Eos(_) | gst::MessageView::Error(_)
            ) {
                break;
            }
        }
        let _ = pipeline.set_state(gst::State::Null);
        assert!(Path::new("/tmp/player_frame.png").exists());
        drop(player);
    }

    #[test]
    #[ignore = "needs a generated clip"]
    fn seeks_coalesce_to_latest() {
        let player = Player::open(
            Path::new("/tmp/clip-play/test.mp4"),
            |_| {},
            || true,
            |_| {},
            |_| {},
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        player.play();
        let dur = player.duration().unwrap().seconds_f64();
        let mut last = 0.0f64;
        for i in 1..=50 {
            last = dur * i as f64 / 51.0;
            player.seek(gst::ClockTime::from_seconds_f64(last));
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while player.seeking() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!player.seeking(), "seeks still pending after 3 s");
        let pos = player.position().unwrap().seconds_f64();
        assert!(
            (pos - last).abs() < 0.1,
            "position {pos} != last seek target {last}"
        );
        drop(player);
    }
}
