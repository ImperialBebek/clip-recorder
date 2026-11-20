//! The recorder process: capture pipelines, rolling buffer, hotkey, tray and
//! control socket. Windows are separate short-lived processes (see `ui`).

use std::future::Future;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::pin::Pin;
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
use crate::config::{self, Capture, Config};
use crate::encoder::Encoder;
use crate::platform::{self, hotkey, ipc, kwin, portal, shortcuts, tray};
use crate::ring::Ring;
use crate::{clips, save, sysinfo};

pub struct Flags {
    /// Explicit `--bitrate`, overriding the quality preset.
    pub kbps: Option<u32>,
    pub hotkey: bool,
    pub reselect: bool,
    /// Autostart: stay in the tray instead of opening the library.
    pub background: bool,
    pub test_source: bool,
    /// Tray and save toast. Off with `--headless`.
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
    /// Effective settings: the config file plus command-line overrides.
    cfg: Config,
    /// The config file as last read, to tell what a reload changed.
    disk: Config,
    flags: Flags,
    /// The screen to record; `None` until one is chosen (first run, or the
    /// desktop revoked the old choice).
    source: Option<VideoSource>,
    /// The picker was already offered, so it is not offered again on its own.
    setup_prompted: bool,
    /// KDE: the recorder follows the game's window instead of a picked screen.
    kwin: Option<kwin::Kwin>,
    /// The game window being recorded (KWin mode), with its stream.
    window: Option<(String, kwin::Stream)>,
    /// A window whose stream failed; not retried while it stays open.
    failed_window: Option<String>,
    ring: Arc<Mutex<Ring>>,
    recorder: Option<Recorder>,
    bus_tx: mpsc::UnboundedSender<BusEvent>,
    manual_pause: bool,
    auto_pause: bool,
    /// Set when the user resumes while auto-paused; holds off auto-pause until
    /// a game shows up and goes away again.
    auto_override: bool,
    game: Option<String>,
    gpu: Option<String>,
    warning: String,
    hotkey_backend: &'static str,
    saving: Arc<AtomicBool>,
    /// Version of a newer release, once the daily check finds one.
    update: Option<String>,
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
        let Some(source) = &self.source else {
            return Ok(());
        };
        let tx = self.bus_tx.clone();
        let rec = Recorder::start(source, &self.settings(), self.ring.clone(), move |ev| {
            let _ = tx.send(ev);
        })?;
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
        } else if self.source.is_none() {
            if self.kwin.is_some() {
                "idle"
            } else {
                "setup"
            }
        } else if self.recorder.is_none() {
            "error"
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
            (
                "capture",
                if self.kwin.is_some() {
                    "game"
                } else {
                    "screen"
                }
                .into(),
            ),
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
            ("version", env!("CARGO_PKG_VERSION").into()),
            ("update", self.update.clone().unwrap_or_default()),
            ("args", launch_args()),
            ("unit", systemd_unit().unwrap_or_default()),
        ];
        lines
            .iter()
            .map(|(k, v)| format!("{k}={}\n", v.replace('\n', " ")))
            .collect()
    }

    /// A toast that is always shown (unless headless): setup and failures
    /// matter even with `toast = false`.
    fn notice(&self, warn: bool, heading: &str, detail: &str) {
        if self.flags.ui {
            show_notice(warn, heading, detail);
        }
    }

    /// Tells the user a game was found and how to save a clip.
    fn announce_game(&self) {
        let Some(name) = self.game.clone() else {
            return;
        };
        let hotkey = pretty_combo(&self.cfg.hotkey);
        if let Some(mut o) = Osd::open() {
            o.send(&format!("game {name}\t{hotkey}"));
        }
    }

    fn reload(&mut self, force: &[&str]) -> Result<()> {
        let disk = Config::load();
        let mut new = self.cfg.clone();
        new.apply_changes(&self.disk, &disk, force);
        self.disk = disk;
        let restart = new.fps != self.cfg.fps
            || new.quality != self.cfg.quality
            || new.encoder != self.cfg.encoder
            || new.audio != self.cfg.audio;
        if new.duration != self.cfg.duration {
            self.ring.lock().unwrap().set_window(secs(new.duration));
        }
        if new.hotkey != self.cfg.hotkey {
            eprintln!("hotkey change takes effect after a restart");
            new.hotkey = self.cfg.hotkey.clone();
        }
        let _ = std::fs::create_dir_all(&new.output);
        self.cfg = new;
        self.auto_pause = self.auto_pause && self.cfg.pause_without_game;
        if restart {
            self.restart()?;
        }
        self.sync_running()
    }

    fn save(&self, reply: Option<ipc::Reply>) {
        if self.source.is_none() {
            let (heading, detail, err) = if self.kwin.is_some() {
                (
                    "No game running",
                    "Start a game to record it",
                    "no game window is being recorded",
                )
            } else {
                (
                    "No screen chosen",
                    "Tray icon → Choose screen",
                    "no screen chosen yet; run `clip-recorder reselect`",
                )
            };
            self.notice(true, heading, detail);
            if let Some(r) = reply {
                let _ = r.send(Err(err.into()));
            }
            return;
        }
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

fn show_notice(warn: bool, heading: &str, detail: &str) {
    if let Some(mut o) = Osd::open() {
        o.send(&format!(
            "{} {heading}\t{detail}",
            if warn { "warn" } else { "info" }
        ));
    }
}

type Picking = Pin<Box<dyn Future<Output = Result<portal::PortalStream>>>>;

/// The portal's screen picker. `intro` first says what the dialog is for, since
/// the dialog itself is the compositor's and can't explain this app's choice.
fn pick(reselect: bool, intro: bool) -> Picking {
    Box::pin(async move {
        if intro {
            show_notice(
                false,
                "Pick your game screen",
                "One time only, then it's automatic",
            );
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
        portal::open(reselect).await
    })
}

fn pick_failure_detail(e: &anyhow::Error) -> &'static str {
    if format!("{e:#}").contains("not available") {
        "No screencast portal running"
    } else {
        "See the terminal log"
    }
}

async fn sync_tray(tray: &Option<ksni::Handle<tray::Tray>>, d: &Daemon) {
    if let Some(t) = tray {
        let (paused, seconds) = (d.paused(), d.cfg.duration);
        let (setup, waiting) = (
            d.source.is_none() && d.kwin.is_none(),
            d.source.is_none() && d.kwin.is_some(),
        );
        t.update(|t| {
            t.paused = paused;
            t.seconds = seconds;
            t.setup = setup;
            t.waiting = waiting;
        })
        .await;
    }
}

type Streaming = Pin<Box<dyn Future<Output = Result<kwin::Stream>>>>;

/// The game window that has focus right now, if any.
fn active_game(wins: &[kwin::Window], skip: Option<&str>) -> Option<(String, String)> {
    wins.iter()
        .filter(|w| w.active && skip != Some(w.uuid.as_str()))
        .find_map(|w| Some((w.uuid.clone(), sysinfo::game_of_window(&w.app_id, w.pid)?)))
}

/// The game window to record: the focused one, else any open game.
fn pick_game(wins: &[kwin::Window], skip: Option<&str>) -> Option<(String, String)> {
    active_game(wins, skip).or_else(|| {
        wins.iter()
            .filter(|w| skip != Some(w.uuid.as_str()))
            .find_map(|w| Some((w.uuid.clone(), sysinfo::game_of_window(&w.app_id, w.pid)?)))
    })
}

impl Daemon {
    /// Stops recording the game window and forgets it (KWin mode).
    fn drop_window(&mut self) {
        self.stop();
        self.source = None;
        self.window = None;
        self.game = None;
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

/// This process's arguments, tab-separated, for the updater to relaunch with.
/// Empty when one can't survive that trip (not UTF-8, or has a tab or newline),
/// so the updater falls back to a plain `run`.
fn launch_args() -> String {
    let args: Option<Vec<String>> = std::env::args_os()
        .skip(1)
        .map(|a| a.into_string().ok().filter(|a| !a.contains(['\t', '\n'])))
        .collect();
    args.map(|a| a.join("\t")).unwrap_or_default()
}

/// The systemd user unit running this process, e.g. `clip-recorder.service`.
/// Only when this process is the unit's main process, so a compositor service
/// that merely launched the recorder is never restarted.
fn systemd_unit() -> Option<String> {
    static UNIT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    UNIT.get_or_init(|| {
        std::env::var_os("INVOCATION_ID")?;
        let cg = std::fs::read_to_string("/proc/self/cgroup").ok()?;
        let unit = cg.lines().last()?.rsplit('/').next()?.to_owned();
        if !unit.ends_with(".service") {
            return None;
        }
        let out = Command::new("systemctl")
            .args(["--user", "show", "-p", "MainPID", "--value", &unit])
            .output()
            .ok()?;
        let main: u32 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
        (main == std::process::id()).then_some(unit)
    })
    .clone()
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

/// Two-note chime on the default sink, so a save is audible without looking.
fn play_click() {
    for freq in [880, 1320] {
        play_tone(freq, 9);
    }
}

/// A sine tone of `buffers` x 10 ms.
fn play_tone(freq: u32, buffers: u32) {
    let Ok(p) = gst::parse::launch(&format!(
        "audiotestsrc wave=sine freq={freq} volume=0.2 num-buffers={buffers} samplesperbuffer=441 ! audio/x-raw,rate=44100 ! autoaudiosink"
    )) else {
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

    // Held for the whole run: dropping it closes the portal session.
    let mut stream: Option<portal::PortalStream> = None;
    // The picker (or a silent restore of the remembered screen) in flight. It
    // runs inside the loop so the tray, hotkey and socket stay responsive while
    // a dialog is open or the user hasn't answered it yet.
    // KDE: follow the game's window, no picker. `--reselect` or
    // `capture = screen` ask for the portal instead.
    let kwin = if flags.test_source || cfg.capture == Capture::Screen || flags.reselect {
        None
    } else {
        match kwin::Kwin::connect() {
            Ok(k) => Some(k),
            Err(e) => {
                if std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.contains("KDE")) {
                    eprintln!("kwin: {e:#} (using the screen portal)");
                }
                None
            }
        }
    };
    let mut picking: Option<Picking> = if flags.test_source || kwin.is_some() {
        None
    } else if flags.reselect {
        Some(pick(true, false))
    } else if portal::has_token() {
        Some(pick(false, false))
    } else {
        None // first run: the first tick offers the picker, once no game is running
    };

    let (bus_tx, mut bus_rx) = mpsc::unbounded_channel();
    let mut d = Daemon {
        ring: Arc::new(Mutex::new(Ring::new(secs(cfg.duration)))),
        source: flags.test_source.then_some(VideoSource::Test),
        setup_prompted: false,
        kwin,
        window: None,
        failed_window: None,
        recorder: None,
        bus_tx,
        manual_pause: false,
        auto_pause: false,
        auto_override: false,
        game: None,
        gpu: sysinfo::gpu_name(),
        warning: String::new(),
        hotkey_backend: "none",
        saving: Arc::new(AtomicBool::new(false)),
        update: None,
        disk: Config::load(),
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
            hotkey: pretty_combo(&d.cfg.hotkey),
            update: None,
            setup: d.source.is_none() && d.kwin.is_none(),
            waiting: d.source.is_none() && d.kwin.is_some(),
            pick: d.kwin.is_none(),
        })
        .await
    } else {
        None
    };

    // Opening the app by hand shows the library; autostart stays in the tray,
    // except until the welcome screen has been seen once.
    if d.flags.ui && !d.flags.test_source && (!d.flags.background || !crate::ui::onboarded()) {
        tray::open_window("library");
    }

    let (update_tx, mut update_rx) = mpsc::unbounded_channel::<String>();
    if d.flags.ui && !d.flags.test_source {
        tokio::spawn(async move {
            loop {
                // Re-read each time so `check_updates = false` stops the requests too.
                if !Config::load().check_updates {
                    tokio::time::sleep(Duration::from_secs(24 * 3600)).await;
                    continue;
                }
                match tokio::task::spawn_blocking(crate::update::check).await {
                    Ok(Ok(Some(rel))) => {
                        let _ = update_tx.send(rel.version);
                    }
                    Ok(Err(e)) => eprintln!("update check: {e:#}"),
                    _ => {}
                }
                tokio::time::sleep(Duration::from_secs(24 * 3600)).await;
            }
        });
    }

    let mut streaming: Option<(String, String, Streaming)> = None;
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
                            _ => !d.paused(),
                        };
                        if !d.manual_pause && d.auto_pause {
                            d.auto_pause = false;
                            d.auto_override = true;
                        }
                        let r = d.sync_running().map(|_| String::new()).map_err(|e| format!("{e:#}"));
                        answer(reply, r);
                    }
                    c if c == "reload" || c.starts_with("reload ") => {
                        let force: Vec<&str> = c.split_whitespace().skip(1).collect();
                        let r = d.reload(&force).map(|_| String::new()).map_err(|e| format!("{e:#}"));
                        answer(reply, r);
                    }
                    "open-folder" => {
                        open_path(&d.cfg.output);
                        answer(reply, Ok(String::new()));
                    }
                    "reselect" if d.kwin.is_some() => answer(
                        reply,
                        Err("recording follows your game's window; set `capture = screen` to pick a screen".into()),
                    ),
                    "reselect" if !d.flags.test_source => {
                        // Keep recording the old screen until a new one is picked;
                        // cancelling the picker changes nothing.
                        if picking.is_some() {
                            answer(reply, Err("the screen picker is already open".into()));
                        } else {
                            picking = Some(pick(true, false));
                            answer(reply, Ok(String::new()));
                        }
                    }
                    "quit" => {
                        answer(reply, Ok(String::new()));
                        break Ok(());
                    }
                    other => answer(reply, Err(format!("unknown command '{other}'"))),
                }
                sync_tray(&tray, &d).await;
            }
            r = async { picking.as_mut().unwrap().await }, if picking.is_some() => {
                picking = None;
                d.setup_prompted = true;
                match r {
                    Ok(s) => {
                        d.stop();
                        d.source = Some(VideoSource::Screen(platform::Screen {
                            fd: Some(s.fd.as_raw_fd()),
                            node: s.node,
                        }));
                        stream = Some(s);
                        if let Err(e) = d.sync_running() {
                            eprintln!("{e:#}");
                            d.notice(true, "Can't start recording", "See the terminal log");
                        }
                    }
                    Err(e) if e.is::<portal::Cancelled>() => {
                        if d.source.is_none() {
                            d.notice(true, "Setup cancelled", "Tray icon → Choose screen");
                        }
                    }
                    Err(e) => {
                        eprintln!("screen capture: {e:#}");
                        if !d.flags.ui && d.source.is_none() {
                            break Err(e);
                        }
                        d.notice(true, "Can't record the screen", pick_failure_detail(&e));
                    }
                }
                sync_tray(&tray, &d).await;
            }
            r = async { streaming.as_mut().unwrap().2.as_mut().await }, if streaming.is_some() => {
                let (uuid, name, _) = streaming.take().unwrap();
                let still_open = d.kwin.as_ref().is_some_and(|k| k.windows().iter().any(|w| w.uuid == uuid));
                match r {
                    Ok(stream) if still_open => {
                        d.stop();
                        d.source = Some(VideoSource::Screen(platform::Screen { fd: None, node: stream.node }));
                        d.window = Some((uuid, stream));
                        d.game = Some(name);
                        match d.sync_running() {
                            Ok(()) => {
                                if d.flags.ui && d.cfg.toast && d.recorder.is_some() {
                                    d.announce_game();
                                }
                            }
                            Err(e) => {
                                eprintln!("{e:#}");
                                d.notice(true, "Can't start recording", "See the terminal log");
                                d.failed_window = Some(d.window.as_ref().unwrap().0.clone());
                                d.drop_window();
                            }
                        }
                    }
                    Ok(_) => {} // the window closed while KWin was setting up
                    Err(e) => {
                        eprintln!("game window capture: {e:#}");
                        d.notice(true, &format!("Can't record {name}"), "See the terminal log");
                        d.failed_window = Some(uuid);
                    }
                }
                sync_tray(&tray, &d).await;
            }
            _ = usr1.recv() => if !d.paused() { d.save(None) },
            Some(v) = update_rx.recv() => {
                if d.update.as_deref() != Some(v.as_str()) {
                    notify("Update available", &format!("Clip Recorder v{v} is out. Update from the tray menu or run `clip-recorder update`."));
                    d.update = Some(v.clone());
                    if let Some(t) = &tray {
                        t.update(|t| t.update = Some(v)).await;
                    }
                }
            }
            Some(ev) = bus_rx.recv() => match ev {
                BusEvent::VideoFailed(e) if d.kwin.is_some() => {
                    // The window's stream ended under the recorder; wait for the next game.
                    eprintln!("game window capture stopped: {e}");
                    if let Some((uuid, _)) = &d.window {
                        d.failed_window = Some(uuid.clone());
                    }
                    d.drop_window();
                    sync_tray(&tray, &d).await;
                }
                BusEvent::VideoFailed(e) => break Err(anyhow::anyhow!("video capture stopped: {e}")),
                BusEvent::AudioFailed(e) => {
                    eprintln!("audio capture stopped ({e}), continuing with video only");
                    if let Some(r) = d.recorder.as_mut() {
                        r.stop_audio();
                    }
                }
            },
            _ = tick.tick() => { if d.kwin.is_some() {
                let wins = d.kwin.as_ref().map(kwin::Kwin::windows).unwrap_or_default();
                if d.failed_window.as_ref().is_some_and(|u| !wins.iter().any(|w| &w.uuid == u)) {
                    d.failed_window = None;
                }
                let gone = d.window.as_ref().is_some_and(|(uuid, s)| s.is_closed() || !wins.iter().any(|w| &w.uuid == uuid));
                // Another game took focus: follow it. Focus on a non-game window
                // (browser, Discord) keeps the current recording.
                let switch = d.window.as_ref().is_some_and(|(cur, _)| {
                    active_game(&wins, d.failed_window.as_deref()).is_some_and(|(uuid, _)| &uuid != cur)
                });
                if gone || switch {
                    d.drop_window();
                    sync_tray(&tray, &d).await;
                }
                if d.window.is_none() && streaming.is_none() {
                    if let Some((uuid, name)) = pick_game(&wins, d.failed_window.as_deref()) {
                        let fut = d.kwin.as_ref().unwrap().stream(&uuid);
                        streaming = Some((uuid, name, Box::pin(fut)));
                    }
                }
            } else {
                let before = d.game.take();
                d.game = sysinfo::running_game();
                let new_game = d.game.is_some() && d.game != before;
                // First run (or a revoked choice): offer the picker once, never
                // on top of a game.
                if d.source.is_none()
                    && picking.is_none()
                    && !d.setup_prompted
                    && (d.game.is_none() || !d.flags.ui)
                {
                    d.setup_prompted = true;
                    picking = Some(pick(false, d.flags.ui));
                }
                if d.game.is_some() {
                    d.auto_override = false;
                }
                let idle = d.cfg.pause_without_game
                    && !d.flags.test_source
                    && !d.auto_override
                    && d.game.is_none();
                if idle != d.auto_pause {
                    d.auto_pause = idle;
                    if let Err(e) = d.sync_running() {
                        eprintln!("{e:#}");
                    }
                    sync_tray(&tray, &d).await;
                }
                // Only now: a game starting from idle resumes the recorder just
                // above, and the toast must see the recorder as running.
                if new_game {
                    if d.source.is_none() {
                        d.notice(true, "Choose a screen to record", "Tray icon → Choose screen");
                    } else if d.recorder.is_some() {
                        if d.flags.ui && d.cfg.toast {
                            d.announce_game();
                        }
                    } else if !d.paused() {
                        d.notice(true, "Not recording", "See the terminal log");
                    }
                }
            } }
            _ = tokio::signal::ctrl_c() => break Ok(()),
            _ = term.recv() => break Ok(()),
        }
    };
    d.stop();
    drop(stream);
    let _ = std::fs::remove_file(ipc::socket_path());
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(uuid: &str, app_id: &str, active: bool) -> kwin::Window {
        kwin::Window {
            uuid: uuid.into(),
            pid: 0,
            app_id: app_id.into(),
            title: String::new(),
            active,
        }
    }

    #[test]
    fn game_window_is_picked_by_app_id() {
        let wins = [
            win("a", "firefox", true),
            win("b", "steam", false),
            win("c", "steam_app_1", false),
        ];
        assert_eq!(pick_game(&wins, None).map(|g| g.0), Some("c".into()));
        assert_eq!(
            pick_game(&wins[..2], None),
            None,
            "no game among browser and Steam"
        );
    }

    #[test]
    fn focus_moving_between_games_is_followed_but_not_to_other_windows() {
        // Recording game "a"; "b" is another open game.
        let on_b = [
            win("a", "steam_app_1", false),
            win("b", "steam_app_2", true),
        ];
        assert_eq!(active_game(&on_b, None).map(|g| g.0), Some("b".into()));
        // Focus on a browser: no game is active, so nothing to switch to.
        let on_browser = [
            win("a", "steam_app_1", false),
            win("b", "steam_app_2", false),
            win("c", "firefox", true),
        ];
        assert_eq!(active_game(&on_browser, None), None);
        // A game whose stream failed is not switched to again.
        assert_eq!(active_game(&on_b, Some("b")), None);
    }

    #[test]
    fn active_game_wins_and_failed_one_is_skipped() {
        let wins = [
            win("a", "steam_app_1", false),
            win("b", "steam_app_2", true),
        ];
        assert_eq!(pick_game(&wins, None).map(|g| g.0), Some("b".into()));
        assert_eq!(pick_game(&wins, Some("b")).map(|g| g.0), Some("a".into()));
    }
}
