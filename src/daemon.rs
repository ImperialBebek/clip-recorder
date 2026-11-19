//! The recorder process: capture pipelines, rolling buffer, hotkey, tray and
//! control socket. Windows are separate short-lived processes (see `ui`).

use std::io::Write;
use std::os::fd::AsRawFd;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;

use crate::capture::{AudioSource, BusEvent, Recorder, Settings, VideoSource};
use crate::config::{self, Config};
use crate::encoder::Encoder;
use crate::platform::{self, hotkey, ipc, portal, shortcuts, tray};
use crate::ring::Ring;
use crate::{clips, save, sysinfo};

pub struct Flags {
    /// Explicit `--bitrate`, overriding the quality preset.
    pub kbps: Option<u32>,
    pub hotkey: bool,
    pub reselect: bool,
    pub test_source: bool,
    /// Tray, first-run window and OSD. Off with `--headless`.
    pub ui: bool,
}

fn secs(s: u32) -> gst::ClockTime {
    gst::ClockTime::from_seconds(s.into())
}

/// `alt+s` -> `Alt+S`.
pub fn pretty_combo(combo: &str) -> String {
    combo
        .split('+')
        .map(|p| {
            let p = p.trim();
            let mut c = p.chars();
            match c.next() {
                Some(f) if p.len() == 1 => f.to_uppercase().collect(),
                Some(f) => f.to_uppercase().chain(c).collect(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

struct Daemon {
    cfg: Config,
    flags: Flags,
    source: VideoSource,
    ring: Arc<Mutex<Ring>>,
    recorder: Option<Recorder>,
    bus_tx: mpsc::UnboundedSender<BusEvent>,
    manual_pause: bool,
    auto_pause: bool,
    game: Option<String>,
    gpu: Option<String>,
    warning: String,
    hotkey_backend: &'static str,
    saving: Arc<AtomicBool>,
}

impl Daemon {
    fn settings(&self) -> Settings {
        let audio = match (self.flags.test_source, self.cfg.audio) {
            (true, AudioSource::Auto) => AudioSource::Test,
            (_, a) => a,
        };
        Settings {
            fps: self.cfg.fps,
            kbps: self.flags.kbps.unwrap_or(self.cfg.quality.kbps()),
            encoder: self.cfg.encoder,
            audio,
        }
    }

    fn start(&mut self) -> Result<()> {
        if self.recorder.is_some() {
            return Ok(());
        }
        let tx = self.bus_tx.clone();
        let rec = Recorder::start(
            &self.source,
            &self.settings(),
            self.ring.clone(),
            move |ev| {
                let _ = tx.send(ev);
            },
        )?;
        self.warning.clear();
        if self.cfg.encoder.is_none() && !rec.encoder.is_hardware() {
            self.warning = "No hardware encoder found, recording on the CPU (x264)".into();
            if self.flags.ui {
                notify("Clip Recorder", &self.warning);
            }
        }
        eprintln!(
            "recording: {}s buffer, {} fps, {} kbit/s, encoder {}, audio {}",
            self.cfg.duration,
            self.cfg.fps,
            self.settings().kbps,
            rec.encoder.name(),
            rec.audio_name.unwrap_or("off"),
        );
        self.recorder = Some(rec);
        Ok(())
    }

    /// Stops the pipelines and drops the buffer, which frees its RAM.
    fn stop(&mut self) {
        if let Some(mut r) = self.recorder.take() {
            r.stop();
        }
        self.ring.lock().unwrap().clear();
    }

    fn restart(&mut self) -> Result<()> {
        let was_running = self.recorder.is_some();
        self.stop();
        if was_running {
            self.start()?;
        }
        Ok(())
    }

    fn paused(&self) -> bool {
        self.manual_pause || self.auto_pause
    }

    fn sync_running(&mut self) -> Result<()> {
        if self.paused() {
            self.stop();
            Ok(())
        } else {
            self.start()
        }
    }

    fn status(&self) -> String {
        let ring = self.ring.lock().unwrap();
        let state = if self.manual_pause {
            "paused"
        } else if self.auto_pause {
            "idle"
        } else {
            "live"
        };
        let rec = self.recorder.as_ref();
        let encoder = rec.map(|r| r.encoder).or(self.cfg.encoder);
        let lines = [
            ("state", state.to_owned()),
            ("window", self.cfg.duration.to_string()),
            ("buffered_ms", ring.buffered().mseconds().to_string()),
            ("ram", ring.bytes().to_string()),
            ("encoder", encoder.map_or("Auto", Encoder::label).to_owned()),
            (
                "encoder_setting",
                self.cfg.encoder.map_or("auto", |e| e.name()).to_owned(),
            ),
            ("codec", "H.264".into()),
            ("video", ring.video_label(self.cfg.fps).unwrap_or_default()),
            ("fps", self.cfg.fps.to_string()),
            ("kbps", self.settings().kbps.to_string()),
            ("quality", self.cfg.quality.name().into()),
            (
                "audio",
                rec.and_then(|r| r.audio_name).unwrap_or("off").into(),
            ),
            ("audio_setting", self.cfg.audio.name().into()),
            ("output", config::tilde(&self.cfg.output)),
            ("output_abs", self.cfg.output.display().to_string()),
            ("hotkey", pretty_combo(&self.cfg.hotkey)),
            ("hotkey_backend", self.hotkey_backend.into()),
            ("game", self.game.clone().unwrap_or_default()),
            ("gpu", self.gpu.clone().unwrap_or_default()),
            ("warning", self.warning.clone()),
            (
                "pause_without_game",
                self.cfg.pause_without_game.to_string(),
            ),
            ("toast", self.cfg.toast.to_string()),
            ("sound", self.cfg.sound.to_string()),
            ("session", sysinfo::session_type().into()),
            ("saving", self.saving.load(Ordering::SeqCst).to_string()),
        ];
        lines
            .iter()
            .map(|(k, v)| format!("{k}={}\n", v.replace('\n', " ")))
            .collect()
    }

    fn reload(&mut self) -> Result<()> {
        let new = Config::load();
        let restart = new.fps != self.cfg.fps
            || new.quality != self.cfg.quality
            || new.encoder != self.cfg.encoder
            || new.audio != self.cfg.audio;
        if new.duration != self.cfg.duration {
            self.ring.lock().unwrap().set_window(secs(new.duration));
        }
        if new.hotkey != self.cfg.hotkey {
            eprintln!("hotkey change takes effect after a restart");
        }
        let _ = std::fs::create_dir_all(&new.output);
        self.cfg = Config {
            hotkey: self.cfg.hotkey.clone(),
            ..new
        };
        self.auto_pause = self.auto_pause && self.cfg.pause_without_game;
        if restart {
            self.restart()?;
        }
        self.sync_running()
    }

    fn save(&self, reply: Option<ipc::Reply>) {
        if self.saving.swap(true, Ordering::SeqCst) {
            if let Some(r) = reply {
                let _ = r.send(Err("a clip is already being saved".into()));
            }
            return;
        }
        let ring = self.ring.lock().unwrap();
        let snap = ring.snapshot();
        let video = ring.video_label(self.cfg.fps).unwrap_or_default();
        drop(ring);
        let path = clips::new_path(&self.cfg.output, self.game.as_deref());
        let (toast, sound, ui) = (self.cfg.toast, self.cfg.sound, self.flags.ui);
        let dir = config::tilde(&self.cfg.output);
        let window = self.cfg.duration;
        let saving = self.saving.clone();
        tokio::task::spawn_blocking(move || {
            let mut osd = (ui && toast).then(Osd::open).flatten();
            if let Some(o) = &mut osd {
                o.send(&format!("saving {window}"));
            }
            let result = save::write_mp4(snap, &path);
            saving.store(false, Ordering::SeqCst);
            let msg = match result {
                Ok(len) => {
                    let s = len.mseconds() as f64 / 1000.0;
                    let bytes = std::fs::metadata(&path).map_or(0, |m| m.len());
                    eprintln!("saved {s:.1}s clip: {}", path.display());
                    match &mut osd {
                        Some(o) => o.send(&format!("saved {s:.0}\t{video}\t{bytes}")),
                        None if ui && toast => notify("Clip saved", &path.display().to_string()),
                        None => {}
                    }
                    if sound {
                        play_click();
                    }
                    Ok(path.display().to_string())
                }
                Err(e) => {
                    eprintln!("save failed: {e:#}");
                    let reason = save_error_reason(&e);
                    match &mut osd {
                        Some(o) => o.send(&format!("failed {reason}\t{dir}")),
                        None if ui => notify("Couldn't save clip", &reason),
                        None => {}
                    }
                    Err(format!("{e:#}"))
                }
            };
            if let Some(r) = reply {
                let _ = r.send(msg);
            }
        });
    }
}

fn save_error_reason(e: &anyhow::Error) -> String {
    let text = format!("{e:#}").to_lowercase();
    if text.contains("no space") {
        "Disk full".into()
    } else if text.contains("no video") || text.contains("empty") {
        "Buffer is empty".into()
    } else if text.contains("permission") {
        "No write permission".into()
    } else {
        "Save failed".into()
    }
}

/// The on-screen toast, a child process fed one state line at a time.
struct Osd(Option<std::process::Child>);

impl Osd {
    fn open() -> Option<Self> {
        let exe = std::env::current_exe().ok()?;
        Command::new(exe)
            .arg("osd")
            .stdin(Stdio::piped())
            .spawn()
            .ok()
            .map(|c| Osd(Some(c)))
    }
    fn send(&mut self, line: &str) {
        if let Some(stdin) = self.0.as_mut().and_then(|c| c.stdin.as_mut()) {
            let _ = writeln!(stdin, "{line}");
        }
    }
}

impl Drop for Osd {
    /// Closing stdin lets the toast finish its animation and exit on its own.
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            drop(child.stdin.take());
            std::thread::spawn(move || child.wait());
        }
    }
}

pub fn notify(title: &str, body: &str) {
    if let Ok(mut child) = Command::new("notify-send")
        .args([
            "-a",
            "Clip Recorder",
            "-i",
            "media-record",
            "-t",
            "3000",
            title,
            body,
        ])
        .spawn()
    {
        std::thread::spawn(move || child.wait());
    }
}

/// Short click on the default sink.
fn play_click() {
    let Ok(p) = gst::parse::launch(
        "audiotestsrc wave=sine freq=1600 volume=0.25 num-buffers=4 samplesperbuffer=441 ! autoaudiosink",
    ) else {
        return;
    };
    let _ = p.set_state(gst::State::Playing);
    if let Some(bus) = p.bus() {
        bus.timed_pop_filtered(
            gst::ClockTime::from_seconds(1),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        );
    }
    let _ = p.set_state(gst::State::Null);
}

fn open_path(p: &std::path::Path) {
    if let Ok(mut child) = Command::new("xdg-open").arg(p).spawn() {
        std::thread::spawn(move || child.wait());
    }
}

/// Shows the first-run window; true when the user chose to allow capture.
fn first_run() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return true;
    };
    match Command::new(exe).arg("first-run").status() {
        Ok(s) => s.success(),
        Err(_) => true,
    }
}

async fn install_hotkey(combo: &str, tx: &mpsc::UnboundedSender<ipc::Request>) -> &'static str {
    let save = |tx: mpsc::UnboundedSender<ipc::Request>| {
        move || {
            let _ = tx.send(ipc::Request {
                cmd: "save".into(),
                reply: None,
            });
        }
    };
    match shortcuts::spawn(combo, save(tx.clone())).await {
        Ok(()) => return "portal",
        Err(e) => eprintln!("hotkey: GlobalShortcuts portal unavailable ({e}), trying /dev/input"),
    }
    let parsed = match hotkey::Hotkey::parse(combo) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("hotkey: {e}");
            return "none";
        }
    };
    if hotkey::spawn(parsed, save(tx.clone())) > 0 {
        return "evdev";
    }
    eprintln!(
        "hotkey: no readable keyboard in /dev/input; add yourself to the `input` group \
         (sudo usermod -aG input $USER, then log in again) or bind `clip-recorder save` \
         to a shortcut in your desktop"
    );
    "none"
}

pub async fn run(cfg: Config, flags: Flags) -> Result<()> {
    gst::init()?;
    std::fs::create_dir_all(&cfg.output)
        .with_context(|| format!("cannot create {}", cfg.output.display()))?;
    if !config::Config::exists() {
        let _ = Config::load().save();
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<ipc::Request>();
    ipc::serve(tx.clone())?;

    if flags.ui && !flags.test_source && !portal::has_token() && !first_run() {
        let _ = std::fs::remove_file(ipc::socket_path());
        return Ok(());
    }

    // Held for the whole run: dropping it closes the portal session.
    let mut stream = if flags.test_source {
        None
    } else {
        Some(portal::open(flags.reselect).await?)
    };
    let source_of = |s: &Option<portal::PortalStream>| match s {
        Some(p) => VideoSource::Screen(platform::Screen {
            fd: p.fd.as_raw_fd(),
            node: p.node,
        }),
        None => VideoSource::Test,
    };

    let (bus_tx, mut bus_rx) = mpsc::unbounded_channel();
    let mut d = Daemon {
        ring: Arc::new(Mutex::new(Ring::new(secs(cfg.duration)))),
        source: source_of(&stream),
        recorder: None,
        bus_tx,
        manual_pause: false,
        auto_pause: false,
        game: None,
        gpu: sysinfo::gpu_name(),
        warning: String::new(),
        hotkey_backend: "none",
        saving: Arc::new(AtomicBool::new(false)),
        cfg,
        flags,
    };
    d.start()?;

    if d.flags.hotkey {
        d.hotkey_backend = install_hotkey(&d.cfg.hotkey.clone(), &tx).await;
    }
    eprintln!(
        "press {} (or run `clip-recorder save`) to save the last {} s to {}",
        pretty_combo(&d.cfg.hotkey),
        d.cfg.duration,
        d.cfg.output.display()
    );

    let tray = if d.flags.ui {
        tray::spawn(tray::Tray {
            tx: tx.clone(),
            paused: false,
            seconds: d.cfg.duration,
        })
        .await
    } else {
        None
    };

    let mut usr1 = signal(SignalKind::user_defined1())?;
    let mut term = signal(SignalKind::terminate())?;
    let mut tick = tokio::time::interval(Duration::from_secs(3));

    let outcome: Result<()> = loop {
        tokio::select! {
            Some(req) = rx.recv() => {
                let ipc::Request { cmd, reply } = req;
                let answer = |r: Option<ipc::Reply>, v: Result<String, String>| {
                    if let Some(r) = r {
                        let _ = r.send(v);
                    }
                };
                match cmd.as_str() {
                    "save" if d.paused() => answer(reply, Err("buffer is paused".into())),
                    "save" => d.save(reply),
                    "status" => answer(reply, Ok(d.status())),
                    "pause" | "resume" | "toggle-pause" => {
                        d.manual_pause = match cmd.as_str() {
                            "pause" => true,
                            "resume" => false,
                            _ => !d.manual_pause,
                        };
                        if !d.manual_pause {
                            d.auto_pause = false;
                        }
                        let r = d.sync_running().map(|_| String::new()).map_err(|e| format!("{e:#}"));
                        answer(reply, r);
                    }
                    "reload" => {
                        let r = d.reload().map(|_| String::new()).map_err(|e| format!("{e:#}"));
                        answer(reply, r);
                    }
                    "open-folder" => {
                        open_path(&d.cfg.output);
                        answer(reply, Ok(String::new()));
                    }
                    "reselect" if !d.flags.test_source => {
                        // Keep recording the old screen until a new one is picked;
                        // cancelling the picker changes nothing.
                        let r = match portal::open(true).await {
                            Ok(s) => {
                                d.stop();
                                stream = Some(s);
                                d.source = source_of(&stream);
                                d.sync_running().map(|_| String::new()).map_err(|e| format!("{e:#}"))
                            }
                            Err(e) => Err(format!("{e:#}")),
                        };
                        answer(reply, r);
                    }
                    "quit" => {
                        answer(reply, Ok(String::new()));
                        break Ok(());
                    }
                    other => answer(reply, Err(format!("unknown command '{other}'"))),
                }
                if let Some(t) = &tray {
                    let (paused, seconds) = (d.paused(), d.cfg.duration);
                    t.update(|t| { t.paused = paused; t.seconds = seconds; }).await;
                }
            }
            _ = usr1.recv() => if !d.paused() { d.save(None) },
            Some(ev) = bus_rx.recv() => match ev {
                BusEvent::VideoFailed(e) => break Err(anyhow::anyhow!("video capture stopped: {e}")),
                BusEvent::AudioFailed(e) => {
                    eprintln!("audio capture stopped ({e}), continuing with video only");
                    if let Some(r) = d.recorder.as_mut() {
                        r.stop_audio();
                    }
                }
            },
            _ = tick.tick() => {
                d.game = sysinfo::running_game();
                let idle = d.cfg.pause_without_game && !d.flags.test_source && d.game.is_none();
                if idle != d.auto_pause {
                    d.auto_pause = idle;
                    if let Err(e) = d.sync_running() {
                        eprintln!("{e:#}");
                    }
                    if let Some(t) = &tray {
                        let paused = d.paused();
                        t.update(|t| t.paused = paused).await;
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => break Ok(()),
            _ = term.recv() => break Ok(()),
        }
    };
    d.stop();
    drop(stream);
    let _ = std::fs::remove_file(ipc::socket_path());
    outcome
}
