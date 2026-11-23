use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use crate::encoder::Encoder;
use crate::gst_util::{add_chain, capsfilter, has, make};
use crate::platform;
use crate::platform::streams::{self, AppStream, Category, StreamEvent, Watcher};
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
    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::PipeWire => "pipewire",
            Self::Pulse => "pulse",
            Self::Test => "test",
            Self::None => "none",
        }
    }

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
    /// Microphone, saved as its own audio track.
    pub mic: AudioSource,
    /// PipeWire/Pulse source name for the microphone; `None` = the system default.
    pub mic_device: Option<String>,
    /// Per-app tracks in automatic mode.
    pub track_game: bool,
    pub track_voice: bool,
    pub track_other: bool,
}

/// One per-app category pipeline: a silence-fed mixer that app streams can
/// join and leave at any time.
pub struct CategoryShared {
    pub pipeline: gst::Pipeline,
    pub mixer: gst::Element,
    /// pw node id -> (branch bin, ghost pad, requested mixer pad).
    pub branches: HashMap<u32, (gst::Bin, gst::Pad, gst::Pad)>,
}

pub struct Recorder {
    pub video: gst::Pipeline,
    pub encoder: Encoder,
    pub audio_name: Option<&'static str>,
    pub mic_name: Option<&'static str>,
    /// Aux track titles in ring order, e.g. ["Game", "Voice", "Mic"].
    pub tracks: Vec<String>,
    audio: Option<gst::Pipeline>,
    mic: Option<gst::Pipeline>,
    cats: Vec<(Category, Arc<Mutex<CategoryShared>>)>,
    watcher: Option<Watcher>,
    /// One per bus-watch thread; cleared on stop so the threads exit.
    video_alive: Arc<AtomicBool>,
    audio_alive: Arc<AtomicBool>,
    mic_alive: Arc<AtomicBool>,
    cat_alives: Vec<Arc<AtomicBool>>,
}

pub enum BusEvent {
    Video(String),
    Audio(String),
    Mic(String),
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
        let video_alive = Arc::new(AtomicBool::new(true));
        let audio_alive = Arc::new(AtomicBool::new(true));
        let mic_alive = Arc::new(AtomicBool::new(true));
        watch_bus(&video, &video_alive, {
            let events = events.clone();
            move |msg| events(BusEvent::Video(msg))
        });

        // The aux tracks' titles are fixed before any pipeline pushes, so a
        // packet always lands on the right index. Categories come first in
        // Game/Voice/Other order, the desktop mix or mic after that.
        let enabled: [(Category, &str, bool); 3] = [
            (Category::Game, "Game", settings.track_game),
            (Category::Voice, "Voice", settings.track_voice),
            (Category::Other, "Other", settings.track_other),
        ];
        let want_mic = settings.mic != AudioSource::None;
        let mut titles: Vec<String> = Vec::new();
        match settings.audio {
            AudioSource::Auto => {
                titles.extend(enabled.iter().filter(|e| e.2).map(|e| e.1.to_string()));
            }
            AudioSource::None => {}
            // Explicit pipewire/pulse stays a single whole-desktop track.
            AudioSource::Test => titles.push("Game".into()),
            _ => titles.push("Desktop".into()),
        }
        if want_mic {
            titles.push("Mic".into());
        }
        ring.lock().unwrap().set_aux(&titles);

        let mut cats = Vec::new();
        let mut cat_alives = Vec::new();
        let mut watcher = None;
        let mut audio = None;
        let mut audio_name = None;
        let mut split = false;
        if settings.audio == AudioSource::Auto && !titles.iter().any(|t| t != "Mic") {
            audio_name = None;
        } else if settings.audio == AudioSource::Auto {
            let mut failed = false;
            for (i, (cat, _, _)) in enabled.iter().filter(|e| e.2).enumerate() {
                match build_category(i, ring.clone()).and_then(|(p, mixer, first)| {
                    start_checked(&p, &clock, base, first).map(|_| (p, mixer))
                }) {
                    Ok((p, mixer)) => {
                        let shared = Arc::new(Mutex::new(CategoryShared {
                            pipeline: p.clone(),
                            mixer,
                            branches: HashMap::new(),
                        }));
                        let alive = Arc::new(AtomicBool::new(true));
                        watch_cat_bus(&shared, &alive, {
                            let events = events.clone();
                            move |msg| events(BusEvent::Audio(msg))
                        });
                        cat_alives.push(alive);
                        cats.push((*cat, shared));
                    }
                    Err(e) => {
                        eprintln!("{} track could not start: {e:#}", cat.name());
                        failed = true;
                        break;
                    }
                }
            }
            if failed {
                stop_cats(&mut cats);
            } else {
                let shares: Vec<(Category, Arc<Mutex<CategoryShared>>)> = cats.clone();
                match streams::watch(move |ev| route_event(&shares, ev)) {
                    Ok(w) => {
                        watcher = Some(w);
                        audio_name = Some("auto");
                        split = true;
                    }
                    Err(e) => {
                        eprintln!("per-app audio unavailable ({e:#}), using the desktop mix");
                        stop_cats(&mut cats);
                    }
                }
            }
            if !split {
                // The category tracks are gone; retitle the first aux track so
                // the desktop mix is never labelled as game-only audio.
                titles.clear();
                titles.push("Desktop".into());
                if want_mic {
                    titles.push("Mic".into());
                }
                ring.lock().unwrap().set_aux(&titles);
            }
        }
        if !split && settings.audio != AudioSource::None {
            let want = if settings.audio == AudioSource::Auto
                && audio_name.is_none()
                && !enabled.iter().any(|e| e.2)
            {
                AudioSource::None
            } else {
                settings.audio
            };
            for &src in platform::audio_candidates(want) {
                let attempt = build_audio(src, 0, ring.clone())
                    .and_then(|(p, first)| start_checked(&p, &clock, base, first).map(|_| p));
                match attempt {
                    Ok(p) => {
                        audio_name = Some(source_name(src));
                        audio = Some(p);
                        break;
                    }
                    Err(e) => eprintln!("audio {}: {e:#}", source_name(src)),
                }
            }
            if let Some(p) = &audio {
                watch_bus(p, &audio_alive, {
                    let events = events.clone();
                    move |msg| events(BusEvent::Audio(msg))
                });
            } else if want != AudioSource::None {
                eprintln!("audio capture unavailable, recording video only");
            }
        }

        let mut mic = None;
        let mic_idx = if want_mic {
            titles.len() - 1
        } else {
            usize::MAX
        };
        for &src in platform::audio_candidates(settings.mic) {
            let attempt = build_mic(src, settings.mic_device.as_deref(), mic_idx, ring.clone())
                .and_then(|(p, first)| start_checked(&p, &clock, base, first).map(|_| p));
            match attempt {
                Ok(p) => {
                    mic = Some((p, source_name(src)));
                    break;
                }
                Err(e) => eprintln!("mic {}: {e:#}", source_name(src)),
            }
        }
        if let Some((p, _)) = &mic {
            watch_bus(p, &mic_alive, move |msg| events(BusEvent::Mic(msg)));
        } else if want_mic {
            eprintln!("microphone unavailable, recording without it");
        }

        Ok(Self {
            video,
            encoder,
            audio_name,
            mic_name: mic.as_ref().map(|(_, n)| *n),
            tracks: ring
                .lock()
                .unwrap()
                .aux
                .iter()
                .map(|(t, _)| t.clone())
                .collect(),
            audio,
            mic: mic.map(|(p, _)| p),
            cats,
            watcher,
            video_alive,
            audio_alive,
            mic_alive,
            cat_alives,
        })
    }

    /// "game:1 voice:0 other:2" — live per-app branches, for `status`.
    pub fn stream_counts(&self) -> String {
        [Category::Game, Category::Voice, Category::Other]
            .iter()
            .map(|cat| {
                let n = self
                    .cats
                    .iter()
                    .find(|(c, _)| c == cat)
                    .map(|(_, s)| s.lock().unwrap().branches.len())
                    .unwrap_or(0);
                format!("{}:{n}", cat.name())
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn stop_audio(&mut self) {
        drop(self.watcher.take());
        self.audio_alive.store(false, Ordering::Relaxed);
        for alive in &self.cat_alives {
            alive.store(false, Ordering::Relaxed);
        }
        self.cat_alives.clear();
        stop_cats(&mut self.cats);
        if let Some(p) = self.audio.take() {
            let _ = p.set_state(gst::State::Null);
        }
    }

    pub fn stop_mic(&mut self) {
        self.mic_alive.store(false, Ordering::Relaxed);
        if let Some(p) = self.mic.take() {
            let _ = p.set_state(gst::State::Null);
        }
    }

    pub fn stop(&mut self) {
        self.video_alive.store(false, Ordering::Relaxed);
        self.stop_audio();
        self.stop_mic();
        let _ = self.video.set_state(gst::State::Null);
    }

    #[cfg(test)]
    fn pipeline_weaks(&self) -> Vec<gst::glib::object::WeakRef<gst::Pipeline>> {
        let mut weaks = vec![self.video.downgrade()];
        for p in [&self.audio, &self.mic].into_iter().flatten() {
            weaks.push(p.downgrade());
        }
        for (_, shared) in &self.cats {
            weaks.push(shared.lock().unwrap().pipeline.downgrade());
        }
        weaks
    }
}

fn stop_cats(cats: &mut Vec<(Category, Arc<Mutex<CategoryShared>>)>) {
    for (_, shared) in cats.drain(..) {
        let _ = shared.lock().unwrap().pipeline.set_state(gst::State::Null);
    }
}

/// Routes a watcher event to the right category pipeline.
fn route_event(cats: &[(Category, Arc<Mutex<CategoryShared>>)], ev: StreamEvent) {
    match ev {
        StreamEvent::Added(stream) => {
            if let Some((_, shared)) = cats.iter().find(|(c, _)| *c == stream.category) {
                add_branch(shared, &stream);
            } else {
                eprintln!("{} stream ignored: {}", stream.category.name(), stream.name);
            }
        }
        StreamEvent::Removed(id) => {
            for (_, shared) in cats {
                remove_branch(shared, id);
            }
        }
    }
}

/// Adds one app's stream to a category mixer as `pipewiresrc` in a branch bin.
fn add_branch(shared: &Arc<Mutex<CategoryShared>>, stream: &AppStream) {
    let mut s = shared.lock().unwrap();
    if s.branches.contains_key(&stream.id) {
        return;
    }
    let build = || -> Result<(gst::Bin, gst::Pad, gst::Pad)> {
        // Never open an untargeted pipewiresrc: it would record the default
        // source (even the microphone) instead of this app's stream.
        let prop =
            platform::pw_target_prop().context("pipewiresrc cannot target a single app stream")?;
        let target = if prop == "target-object" {
            stream.serial.clone()
        } else {
            stream.id.to_string()
        };
        let bin = gst::Bin::with_name(&format!("app-{}", stream.id));
        let src = make(
            "pipewiresrc",
            &[
                (prop, target.as_str()),
                ("do-timestamp", "true"),
                (
                    "stream-properties",
                    "props,media.category=(string)Capture,node.dont-reconnect=(boolean)true,node.dont-fallback=(boolean)true",
                ),
            ],
        )?;
        let conv = make("audioconvert", &[])?;
        let resample = make("audioresample", &[])?;
        let caps = capsfilter("audio/x-raw,rate=48000,channels=2")?;
        let queue = make("queue", &[])?;
        bin.add_many([&src, &conv, &resample, &caps, &queue])?;
        gst::Element::link_many([&src, &conv, &resample, &caps, &queue])?;
        let ghost = gst::GhostPad::with_target(&queue.static_pad("src").unwrap())?;
        bin.add_pad(ghost.upcast_ref::<gst::Pad>())?;
        s.pipeline.add(&bin)?;
        let sink = match s.mixer.request_pad_simple("sink_%u") {
            Some(sink) => sink,
            None => {
                let _ = s.pipeline.remove(&bin);
                bail!("audiomixer gave no sink pad");
            }
        };
        if let Err(e) = ghost.link(&sink) {
            s.mixer.release_request_pad(&sink);
            let _ = s.pipeline.remove(&bin);
            return Err(e.into());
        }
        Ok((bin, ghost.upcast(), sink))
    };
    match build() {
        Ok((bin, ghost, sink)) => {
            // pipewiresrc blocks in its state change until the stream is
            // linked, so never under the lock or on the watcher thread.
            bin.call_async(|b| {
                let _ = b.sync_state_with_parent();
            });
            s.branches.insert(stream.id, (bin, ghost, sink));
            eprintln!(
                "{} stream added: {} (#{})",
                stream.category.name(),
                stream.name,
                stream.id
            );
        }
        Err(e) => eprintln!(
            "{} stream {} failed to add: {e:#}",
            stream.category.name(),
            stream.name
        ),
    }
}

/// Removes a branch bin and frees its mixer pad.
fn remove_branch(shared: &Arc<Mutex<CategoryShared>>, id: u32) {
    let mut s = shared.lock().unwrap();
    let Some((bin, ghost, sink)) = s.branches.remove(&id) else {
        return;
    };
    let _ = ghost.unlink(&sink);
    s.mixer.release_request_pad(&sink);
    let _ = s.pipeline.remove(&bin);
    bin.call_async(|b| {
        let _ = b.set_state(gst::State::Null);
    });
    eprintln!("app stream removed: #{id}");
}

/// Like `watch_bus`, but an error inside an `app-*` branch bin only removes
/// that branch; the category track keeps running on its silence source. Holds
/// the shared state weakly so a stopped Recorder can be freed.
fn watch_cat_bus(
    shared: &Arc<Mutex<CategoryShared>>,
    alive: &Arc<AtomicBool>,
    on_fail: impl Fn(String) + Send + 'static,
) {
    let bus = shared
        .lock()
        .unwrap()
        .pipeline
        .bus()
        .expect("pipeline has a bus");
    let weak = Arc::downgrade(shared);
    let alive = alive.clone();
    std::thread::spawn(move || {
        while alive.load(Ordering::Relaxed) {
            let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) else {
                continue;
            };
            match msg.view() {
                gst::MessageView::Error(e) => {
                    let text = format!(
                        "{} ({})",
                        e.error(),
                        e.debug().map(|d| d.to_string()).unwrap_or_default()
                    );
                    if let Some(id) = branch_of(&msg) {
                        let Some(shared) = weak.upgrade() else {
                            return;
                        };
                        eprintln!("app stream #{id} failed ({text}), removing its branch");
                        remove_branch(&shared, id);
                        continue;
                    }
                    on_fail(text);
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

/// The `app-<id>` bin an error message came from, if any.
fn branch_of(msg: &gst::Message) -> Option<u32> {
    let mut parent = msg.src()?.downcast_ref::<gst::Element>()?.parent();
    while let Some(obj) = parent {
        if let Some(id) = obj.name().strip_prefix("app-").and_then(|n| n.parse().ok()) {
            return Some(id);
        }
        parent = obj.parent();
    }
    None
}

fn source_name(src: AudioSource) -> &'static str {
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

/// Caps the frame rate without touching frame memory (works on DMA-BUF):
/// `videorate max-rate` doesn't engage on variable-rate (`framerate=0/1`)
/// input and older GStreamer can't renegotiate it, so a pad probe drops
/// frames above the cap by timestamp while a second probe rewrites a variable
/// or faster caps rate to a fixed `{fps}/1` so CBR stays honest (a slower
/// fixed rate is left as is). `identity` is used rather
/// than `capssetter`, which answers upstream caps queries with ANY and would
/// strip the DMA-BUF constraints pipewiresrc negotiates against.
fn rate_cap(fps: u32) -> Result<Vec<gst::Element>> {
    let ident = make("identity", &[("silent", "true")])?;
    let interval = gst::ClockTime::SECOND / u64::from(fps);
    let next_due = Mutex::new(None);
    ident
        .static_pad("sink")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
            let mut next_due = next_due.lock().unwrap();
            match info.buffer().and_then(|buf| buf.pts()) {
                Some(pts) if keep_frame(&mut next_due, pts, interval) => gst::PadProbeReturn::Ok,
                Some(_) => gst::PadProbeReturn::Drop,
                None => gst::PadProbeReturn::Ok,
            }
        });
    ident.static_pad("src").unwrap().add_probe(
        gst::PadProbeType::EVENT_DOWNSTREAM,
        move |_pad, info| {
            let Some(gst::PadProbeData::Event(event)) = info.data.as_mut() else {
                return gst::PadProbeReturn::Ok;
            };
            let gst::EventView::Caps(caps_event) = event.view() else {
                return gst::PadProbeReturn::Ok;
            };
            let mut caps = caps_event.caps().to_owned();
            let cap = gst::Fraction::new(fps as i32, 1);
            for i in 0..caps.size() {
                if let Some(structure) = caps.make_mut().structure_mut(i) {
                    let rate = structure.get::<gst::Fraction>("framerate").ok();
                    if rate.is_none_or(|r| r.numer() == 0 || r > cap) {
                        structure.set("framerate", cap);
                    }
                }
            }
            *event = gst::event::Caps::builder(&caps).build();
            gst::PadProbeReturn::Ok
        },
    );
    Ok(vec![ident])
}

/// Whether a frame at `pts` fits the `interval` cadence; tolerates jitter up
/// to half an interval and resynchronizes after a gap of more than one.
fn keep_frame(
    next_due: &mut Option<gst::ClockTime>,
    pts: gst::ClockTime,
    interval: gst::ClockTime,
) -> bool {
    let Some(due) = *next_due else {
        *next_due = Some(pts + interval);
        return true;
    };
    if pts + interval / 2 < due {
        return false;
    }
    *next_due = Some(if due + interval < pts {
        pts + interval
    } else {
        due + interval
    });
    true
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
    chain.extend(rate_cap(s.fps.max(1))?);
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
    aux: usize,
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
    let (sink, first) = appsink(move |caps, buf| ring.lock().unwrap().push_aux(aux, caps, buf));
    chain.push(sink);
    add_chain(&pipeline, &chain)?;
    Ok((pipeline, first))
}

/// Same shape as `build_audio`, sourced from the microphone instead.
fn build_mic(
    src: AudioSource,
    device: Option<&str>,
    aux: usize,
    ring: Arc<Mutex<Ring>>,
) -> Result<(gst::Pipeline, mpsc::Receiver<()>)> {
    let pipeline = gst::Pipeline::with_name("mic");
    let source = platform::mic_element(src, device)?;
    let mut chain = vec![
        source,
        make("audioconvert", &[])?,
        make("audioresample", &[])?,
        capsfilter("audio/x-raw,rate=48000,channels=2")?,
    ];
    chain.extend(audio_encoder()?);
    let (sink, first) = appsink(move |caps, buf| ring.lock().unwrap().push_aux(aux, caps, buf));
    chain.push(sink);
    add_chain(&pipeline, &chain)?;
    Ok((pipeline, first))
}

/// A per-app category track: silence keeps it continuous, and each playing
/// app joins the mixer as a removable `pipewiresrc` branch.
fn build_category(
    aux: usize,
    ring: Arc<Mutex<Ring>>,
) -> Result<(gst::Pipeline, gst::Element, mpsc::Receiver<()>)> {
    let pipeline = gst::Pipeline::with_name("category");
    let silence = make("audiotestsrc", &[("wave", "silence"), ("is-live", "true")])?;
    let mixer = make("audiomixer", &[])?;
    let mut chain = vec![
        mixer.clone(),
        make("audioconvert", &[])?,
        make("audioresample", &[])?,
        capsfilter("audio/x-raw,rate=48000,channels=2")?,
    ];
    chain.extend(audio_encoder()?);
    let (sink, first) = appsink(move |caps, buf| ring.lock().unwrap().push_aux(aux, caps, buf));
    chain.push(sink);
    pipeline.add(&silence)?;
    add_chain(&pipeline, &chain)?;
    silence.link(&mixer)?;
    Ok((pipeline, mixer, first))
}

/// AAC plays everywhere an MP4 does; Opus is the fallback when no AAC encoder is installed.
pub(crate) fn audio_encoder() -> Result<Vec<gst::Element>> {
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

/// Reports pipeline failure on `on_fail`; exits when `alive` goes false or
/// the pipeline dies. Holds the bus only, never the pipeline itself.
fn watch_bus(
    pipeline: &gst::Pipeline,
    alive: &Arc<AtomicBool>,
    on_fail: impl Fn(String) + Send + 'static,
) {
    let bus = pipeline.bus().expect("pipeline has a bus");
    let alive = alive.clone();
    std::thread::spawn(move || {
        while alive.load(Ordering::Relaxed) {
            let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) else {
                continue;
            };
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Records the test video plus all per-app tracks and a test-tone "Mic"
    /// against the real PipeWire session, then muxes an MP4 to /tmp/clip-test.
    /// Play music in an app classified Other while this runs.
    #[test]
    #[ignore = "records live PipeWire app streams; run manually"]
    fn split_capture() {
        gst::init().unwrap();
        let ring = Arc::new(Mutex::new(Ring::new(gst::ClockTime::from_seconds(30))));
        let settings = Settings {
            fps: 30,
            kbps: 4000,
            encoder: None,
            audio: AudioSource::Auto,
            mic: AudioSource::Test,
            mic_device: None,
            track_game: true,
            track_voice: true,
            track_other: true,
        };
        let mut rec = Recorder::start(&VideoSource::Test, &settings, ring.clone(), |_| {})
            .expect("recorder could not start");
        assert_eq!(rec.tracks, ["Game", "Voice", "Other", "Mic"]);
        std::thread::sleep(Duration::from_secs(12));
        eprintln!("streams: {}", rec.stream_counts());
        assert_ne!(
            rec.stream_counts(),
            "game:0 voice:0 other:0",
            "no app stream attached"
        );
        let snap = ring.lock().unwrap().snapshot();
        rec.stop();
        std::fs::create_dir_all("/tmp/clip-test").unwrap();
        let secs = crate::save::write_mp4(snap, Path::new("/tmp/clip-test/split.mp4")).unwrap();
        eprintln!("wrote {}s to /tmp/clip-test/split.mp4", secs.seconds());
    }

    /// A stopped Recorder must free every pipeline and end every bus thread;
    /// pause and game switches go through `stop` many times a session.
    #[test]
    #[ignore = "uses real GStreamer pipelines; run manually"]
    fn stopped_recorder_frees_pipelines() {
        gst::init().unwrap();
        let settings = Settings {
            fps: 30,
            kbps: 4000,
            encoder: None,
            audio: AudioSource::Test,
            mic: AudioSource::Test,
            mic_device: None,
            track_game: true,
            track_voice: false,
            track_other: false,
        };
        for _ in 0..3 {
            let ring = Arc::new(Mutex::new(Ring::new(gst::ClockTime::from_seconds(5))));
            let mut rec = Recorder::start(&VideoSource::Test, &settings, ring, |_| {})
                .expect("recorder could not start");
            let weaks = rec.pipeline_weaks();
            std::thread::sleep(Duration::from_millis(300));
            rec.stop();
            drop(rec);
            let deadline = Instant::now() + Duration::from_secs(1);
            while weaks.iter().any(|w| w.upgrade().is_some()) {
                assert!(Instant::now() < deadline, "pipeline outlived its Recorder");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    /// The pad probe drops frames above the cap on `framerate=0/1` input
    /// (what KWin's game capture negotiates) without ever duplicating one,
    /// and `capssetter` fixes the output caps at the configured rate.
    #[test]
    fn rate_cap_limits_variable_rate_input() {
        gst::init().unwrap();
        for (in_fps, lo, hi) in [(120u32, 115usize, 125usize), (40, 78, 82)] {
            let pipeline = gst::Pipeline::new();
            let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let caps_seen = Arc::new(Mutex::new(None));
            let mut chain = vec![
                make(
                    "videotestsrc",
                    &[
                        ("is-live", "true"),
                        ("num-buffers", &(in_fps * 2).to_string()),
                        ("pattern", "ball"),
                    ],
                )
                .unwrap(),
                capsfilter(&format!(
                    "video/x-raw,width=320,height=180,framerate={in_fps}/1"
                ))
                .unwrap(),
            ];
            let setter = make("capssetter", &[("join", "true")]).unwrap();
            setter.set_property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("framerate", gst::Fraction::new(0, 1))
                    .build(),
            );
            chain.push(setter);
            chain.extend(rate_cap(60).unwrap());
            let (sink, _rx) = appsink({
                let count = count.clone();
                let caps_seen = caps_seen.clone();
                move |caps, _| {
                    if let Some(caps) = caps {
                        *caps_seen.lock().unwrap() = Some(caps);
                    }
                    count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            });
            chain.push(sink);
            add_chain(&pipeline, &chain).unwrap();
            pipeline.set_state(gst::State::Playing).unwrap();
            let eos = pipeline
                .bus()
                .unwrap()
                .timed_pop_filtered(
                    gst::ClockTime::from_seconds(10),
                    &[gst::MessageType::Eos, gst::MessageType::Error],
                )
                .expect("no EOS within 10 s");
            assert!(matches!(eos.view(), gst::MessageView::Eos(_)));
            pipeline.set_state(gst::State::Null).unwrap();
            let n = count.load(std::sync::atomic::Ordering::Relaxed);
            assert!((lo..=hi).contains(&n), "{in_fps} fps in -> {n} out");
            let caps = caps_seen.lock().unwrap().clone().unwrap();
            let framerate = caps
                .structure(0)
                .unwrap()
                .get::<gst::Fraction>("framerate")
                .unwrap();
            assert_eq!(framerate, gst::Fraction::new(60, 1));
        }
    }

    /// A fixed source slower than the cap passes untouched; the output caps
    /// still say `{fps}/1` (capssetter only relabels the rate).
    #[test]
    fn rate_cap_passes_slower_fixed_rate() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let caps_seen = Arc::new(Mutex::new(None));
        let mut chain = vec![
            make(
                "videotestsrc",
                &[("is-live", "true"), ("num-buffers", "60")],
            )
            .unwrap(),
            capsfilter("video/x-raw,width=320,height=180,framerate=30/1").unwrap(),
        ];
        chain.extend(rate_cap(60).unwrap());
        let (sink, _rx) = appsink({
            let count = count.clone();
            let caps_seen = caps_seen.clone();
            move |caps, _| {
                if let Some(caps) = caps {
                    *caps_seen.lock().unwrap() = Some(caps);
                }
                count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        });
        chain.push(sink);
        add_chain(&pipeline, &chain).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let msg = pipeline
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(10),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("no EOS within 10 s");
        pipeline.set_state(gst::State::Null).unwrap();
        assert!(matches!(msg.view(), gst::MessageView::Eos(_)), "{msg:?}");
        assert_eq!(count.load(std::sync::atomic::Ordering::Relaxed), 60);
        let caps = caps_seen.lock().unwrap().clone().unwrap();
        let framerate = caps
            .structure(0)
            .unwrap()
            .get::<gst::Fraction>("framerate")
            .unwrap();
        assert_eq!(framerate, gst::Fraction::new(30, 1));
    }

    /// Regression for the KDE capture failure: `capssetter` answers upstream
    /// caps queries with ANY, which stripped the DMA-BUF constraints
    /// pipewiresrc negotiates against. `identity` must let downstream
    /// constraints propagate upstream untouched.
    #[test]
    fn rate_cap_passes_caps_queries_through() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let cap = rate_cap(60).unwrap();
        let filter = capsfilter("video/x-raw,format=NV12,width=320,height=180").unwrap();
        let sink = make("fakesink", &[]).unwrap();
        add_chain(&pipeline, &[cap[0].clone(), filter, sink]).unwrap();
        let caps = cap[0].static_pad("sink").unwrap().query_caps(None);
        assert!(!caps.is_any());
        for i in 0..caps.size() {
            let structure = caps.structure(i).unwrap();
            assert_eq!(structure.get::<&str>("format").unwrap(), "NV12");
        }
    }

    /// `capssetter join=true` merges the framerate into the negotiated caps
    /// without forcing system memory: GLMemory buffers and their feature
    /// reach the sink untouched.
    #[test]
    #[ignore = "needs a GL context; run manually"]
    fn rate_cap_keeps_memory_features() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let caps_seen = Arc::new(Mutex::new(None));
        let mut chain = vec![
            make(
                "videotestsrc",
                &[("num-buffers", "240"), ("is-live", "true")],
            )
            .unwrap(),
            make("glupload", &[]).unwrap(),
            capsfilter("video/x-raw(memory:GLMemory),framerate=120/1").unwrap(),
        ];
        // Same variable-rate input the screen capture negotiates.
        let setter = make("capssetter", &[("join", "true")]).unwrap();
        setter.set_property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("framerate", gst::Fraction::new(0, 1))
                .build(),
        );
        chain.push(setter);
        chain.extend(rate_cap(60).unwrap());
        let (sink, _rx) = appsink({
            let caps_seen = caps_seen.clone();
            move |caps, _| {
                if let Some(caps) = caps {
                    *caps_seen.lock().unwrap() = Some(caps);
                }
            }
        });
        chain.push(sink);
        add_chain(&pipeline, &chain).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let eos = pipeline
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(10),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("no EOS within 10 s");
        assert!(
            matches!(eos.view(), gst::MessageView::Eos(_)),
            "not negotiated: {eos:?}"
        );
        pipeline.set_state(gst::State::Null).unwrap();
        let caps = caps_seen.lock().unwrap().clone().unwrap().to_string();
        assert!(
            caps.contains("memory:GLMemory") && caps.contains("framerate=(fraction)60/1"),
            "unexpected caps: {caps}"
        );
    }

    #[test]
    fn keeps_cadence_and_drops_above_cap() {
        let interval = gst::ClockTime::SECOND / 60;
        let run = |pts: &[u64]| {
            let mut next_due = None;
            pts.iter()
                .filter(|&&p| keep_frame(&mut next_due, gst::ClockTime::from_nseconds(p), interval))
                .count()
        };
        let spaced = |fps: u64| {
            (0..fps)
                .map(|i| i * 1_000_000_000 / fps)
                .collect::<Vec<_>>()
        };
        assert_eq!(run(&spaced(60)), 60);
        assert_eq!(run(&spaced(40)), 40);
        assert!((59..=61).contains(&run(&spaced(180))));
        // 108 fps with ±2 ms deterministic jitter.
        let jittered = (0..108u64)
            .map(|i| {
                let offset = ((i * 37) % 5) * 1_000_000;
                i * 1_000_000_000 / 108 + 2_000_000 - offset
            })
            .collect::<Vec<_>>();
        assert!((58..=61).contains(&run(&jittered)));
    }
}
