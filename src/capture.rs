use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use crate::encoder::Encoder;
use crate::gst_util::{add_chain, capsfilter, has, make};
use crate::platform;
use crate::ring::Ring;

pub enum VideoSource {
    /// The screen, from the platform's capture API.
    Screen(platform::Screen),
    /// Synthetic moving pattern, for testing without a desktop session.
    Test,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioSource {
    Auto,
    PipeWire,
    Pulse,
    /// A sine tone, paired with `--test-source`.
    Test,
    None,
}

impl AudioSource {
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "auto" => Self::Auto,
            "pipewire" | "pw" => Self::PipeWire,
            "pulse" | "pulseaudio" => Self::Pulse,
            "none" | "off" => Self::None,
            _ => bail!("unknown audio source '{s}' (auto, pipewire, pulse, none)"),
        })
    }
}

pub struct Settings {
    pub fps: u32,
    pub kbps: u32,
    pub encoder: Option<Encoder>,
    pub audio: AudioSource,
}

pub struct Recorder {
    pub video: gst::Pipeline,
    pub audio: Option<gst::Pipeline>,
    pub encoder: Encoder,
    pub audio_name: Option<&'static str>,
}

pub enum BusEvent {
    VideoFailed(String),
    AudioFailed(String),
}

impl Recorder {
    pub fn start(
        source: &VideoSource,
        settings: &Settings,
        ring: Arc<Mutex<Ring>>,
        events: impl Fn(BusEvent) + Send + Clone + 'static,
    ) -> Result<Self> {
        // Both pipelines run on one clock with one base time, so their
        // timestamps line up when the clip is muxed.
        let clock = gst::SystemClock::obtain();
        let base = clock.time();

        let candidates: Vec<Encoder> = match settings.encoder {
            Some(e) => vec![e],
            None => Encoder::AUTO_ORDER
                .into_iter()
                .filter(|e| e.available())
                .collect(),
        };
        if candidates.is_empty() {
            bail!("no H.264 encoder found; install gst-plugins-bad (va), gstreamer-vaapi, or gst-plugins-ugly (x264)");
        }

        let mut video = None;
        for enc in candidates {
            let attempt = build_video(source, settings, enc, ring.clone())
                .and_then(|(p, first)| start_checked(&p, &clock, base, first).map(|_| p));
            match attempt {
                Ok(p) => {
                    video = Some((p, enc));
                    break;
                }
                Err(e) => eprintln!("encoder {}: {e:#}", enc.name()),
            }
        }
        let (video, encoder) =
            video.ok_or_else(|| anyhow!("no encoder could start, see errors above"))?;
        watch_bus(&video, {
            let events = events.clone();
            move |msg| events(BusEvent::VideoFailed(msg))
        });

        let mut audio = None;
        for &src in platform::audio_candidates(settings.audio) {
            let attempt = build_audio(src, ring.clone())
                .and_then(|(p, first)| start_checked(&p, &clock, base, first).map(|_| p));
            match attempt {
                Ok(p) => {
                    audio = Some((p, audio_name(src)));
                    break;
                }
                Err(e) => eprintln!("audio {}: {e:#}", audio_name(src)),
            }
        }
        if let Some((p, _)) = &audio {
            watch_bus(p, move |msg| events(BusEvent::AudioFailed(msg)));
        } else if settings.audio != AudioSource::None {
            eprintln!("audio capture unavailable, recording video only");
        }

        Ok(Self {
            video,
            encoder,
            audio_name: audio.as_ref().map(|(_, n)| *n),
            audio: audio.map(|(p, _)| p),
        })
    }

    pub fn stop_audio(&mut self) {
        if let Some(p) = self.audio.take() {
            let _ = p.set_state(gst::State::Null);
        }
    }

    pub fn stop(&mut self) {
        self.stop_audio();
        let _ = self.video.set_state(gst::State::Null);
    }
}

fn audio_name(src: AudioSource) -> &'static str {
    match src {
        AudioSource::PipeWire => "pipewire",
        AudioSource::Pulse => "pulse",
        AudioSource::Test => "test",
        _ => "none",
    }
}

fn appsink(
    on_buffer: impl Fn(Option<gst::Caps>, gst::Buffer) + Send + 'static,
) -> (gst::Element, mpsc::Receiver<()>) {
    let (tx, rx) = mpsc::sync_channel(1);
    let sink = gst_app::AppSink::builder()
        .sync(false)
        .callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    if let Some(mut buf) = sample.buffer_owned() {
                        // Encoders may shift their output segment (x264enc adds
                        // 1000 h); running time is what both pipelines share.
                        if let Some(seg) = sample
                            .segment()
                            .and_then(|s| s.downcast_ref::<gst::ClockTime>())
                        {
                            let b = buf.make_mut();
                            let pts = b.pts().and_then(|t| seg.to_running_time(t));
                            let dts = b.dts().and_then(|t| seg.to_running_time(t));
                            b.set_pts(pts);
                            b.set_dts(dts);
                        }
                        on_buffer(sample.caps_owned(), buf);
                        let _ = tx.try_send(());
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        )
        .build();
    (sink.upcast(), rx)
}

fn build_video(
    source: &VideoSource,
    s: &Settings,
    enc: Encoder,
    ring: Arc<Mutex<Ring>>,
) -> Result<(gst::Pipeline, mpsc::Receiver<()>)> {
    let pipeline = gst::Pipeline::with_name("video");
    let fps = s.fps.to_string();
    let mut chain = match source {
        VideoSource::Screen(screen) => vec![screen.element()?],
        VideoSource::Test => vec![
            make("videotestsrc", &[("is-live", "true"), ("pattern", "ball")])?,
            capsfilter(&format!(
                "video/x-raw,width=1280,height=720,framerate={fps}/1"
            ))?,
        ],
    };
    // Caps the frame rate without touching frame memory (works on DMA-BUF).
    chain.push(make(
        "videorate",
        &[("drop-only", "true"), ("max-rate", &fps)],
    )?);
    chain.extend(enc.elements(s.kbps, s.fps.max(1))?);
    chain.push(make("h264parse", &[("config-interval", "-1")])?);
    chain.push(capsfilter("video/x-h264,stream-format=avc,alignment=au")?);
    let (sink, first) = appsink(move |caps, buf| ring.lock().unwrap().push_video(caps, buf));
    chain.push(sink);
    add_chain(&pipeline, &chain)?;
    Ok((pipeline, first))
}

fn build_audio(
    src: AudioSource,
    ring: Arc<Mutex<Ring>>,
) -> Result<(gst::Pipeline, mpsc::Receiver<()>)> {
    let pipeline = gst::Pipeline::with_name("audio");
    let source = platform::audio_element(src)?;
    let mut chain = vec![
        source,
        make("audioconvert", &[])?,
        make("audioresample", &[])?,
        capsfilter("audio/x-raw,rate=48000,channels=2")?,
    ];
    chain.extend(audio_encoder()?);
    let (sink, first) = appsink(move |caps, buf| ring.lock().unwrap().push_audio(caps, buf));
    chain.push(sink);
    add_chain(&pipeline, &chain)?;
    Ok((pipeline, first))
}

/// AAC plays everywhere an MP4 does; Opus is the fallback when no AAC encoder is installed.
fn audio_encoder() -> Result<Vec<gst::Element>> {
    for (name, props) in [
        ("fdkaacenc", &[("bitrate", "160000")][..]),
        ("avenc_aac", &[("bitrate", "160000")][..]),
        ("voaacenc", &[("bitrate", "160000")][..]),
    ] {
        if has(name) {
            return Ok(vec![make(name, props)?, make("aacparse", &[])?]);
        }
    }
    Ok(vec![
        make("opusenc", &[("bitrate", "160000")])?,
        make("opusparse", &[])?,
    ])
}

/// Starts the pipeline and waits for the first encoded buffer, so a broken
/// encoder fails here and the next one can be tried.
fn start_checked(
    pipeline: &gst::Pipeline,
    clock: &gst::Clock,
    base: gst::ClockTime,
    first: mpsc::Receiver<()>,
) -> Result<()> {
    pipeline.use_clock(Some(clock));
    pipeline.set_start_time(gst::ClockTime::NONE);
    pipeline.set_base_time(base);
    let bus = pipeline.bus().expect("pipeline has a bus");
    if let Err(e) = pipeline.set_state(gst::State::Playing) {
        let err = pop_error(&bus).unwrap_or_else(|| e.to_string());
        let _ = pipeline.set_state(gst::State::Null);
        bail!(err);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if first.try_recv().is_ok() {
            return Ok(());
        }
        if let Some(err) = pop_error(&bus) {
            let _ = pipeline.set_state(gst::State::Null);
            bail!(err);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!("{}: no data after 5s, continuing anyway", pipeline.name());
    Ok(())
}

fn pop_error(bus: &gst::Bus) -> Option<String> {
    let msg = bus.pop_filtered(&[gst::MessageType::Error])?;
    match msg.view() {
        gst::MessageView::Error(e) => Some(format!(
            "{} ({})",
            e.error(),
            e.debug().map(|d| d.to_string()).unwrap_or_default()
        )),
        _ => None,
    }
}

fn watch_bus(pipeline: &gst::Pipeline, on_fail: impl Fn(String) + Send + 'static) {
    let bus = pipeline.bus().expect("pipeline has a bus");
    std::thread::spawn(move || {
        for msg in bus.iter_timed(gst::ClockTime::NONE) {
            match msg.view() {
                gst::MessageView::Error(e) => {
                    on_fail(format!(
                        "{} ({})",
                        e.error(),
                        e.debug().map(|d| d.to_string()).unwrap_or_default()
                    ));
                    return;
                }
                gst::MessageView::Eos(_) => {
                    on_fail("stream ended".into());
                    return;
                }
                _ => {}
            }
        }
    });
}
