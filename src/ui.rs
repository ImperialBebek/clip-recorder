//! The windows: the clips library and the save toast. Each runs as its own
//! short-lived process (`clip-recorder library`, `osd`) and talks to the
//! recorder over the control socket, so the recorder never loads a GUI.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use slint::winit_030::WinitWindowAccessor;
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};

use crate::clips;
use crate::config::{self, Config, Quality};
use crate::platform::ipc;

slint::include_modules!();

type Status = HashMap<String, String>;

fn spawn_detached(args: &[&str]) {
    if let Ok(exe) = std::env::current_exe() {
        let _ = Command::new(exe).args(args).spawn();
    }
}

fn open_path(p: &str) {
    let _ = Command::new("xdg-open").arg(p).spawn();
}

fn get<'a>(s: &'a Status, k: &str) -> &'a str {
    s.get(k).map(String::as_str).unwrap_or("")
}

fn num(s: &Status, k: &str) -> u64 {
    get(s, k).parse().unwrap_or(0)
}

fn request_async(cmd: &'static str) {
    std::thread::spawn(move || {
        if let Err(e) = ipc::request(cmd) {
            eprintln!("{cmd}: {e:#}");
        }
    });
}

const QUALITY: [Quality; 3] = [Quality::Light, Quality::High, Quality::Max];
const DURATIONS: [u32; 3] = [30, 60, 120];
const AUDIO_KBPS: u64 = 160;

/// Writes one change to the config file and tells the recorder to reload it.
fn change(key: &'static str, f: impl FnOnce(&mut Config) + Send + 'static) {
    // One writer at a time, so two quick edits can't each save a stale copy.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    std::thread::spawn(move || {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut c = Config::load();
        f(&mut c);
        if let Err(e) = c.save() {
            eprintln!("{e:#}");
            return;
        }
        let _ = ipc::request(&format!("reload {key}"));
    });
}

/// "Today 14:52", "Yesterday 22:14", "Mon 20:03", "Nov 02".
fn when(t: SystemTime) -> String {
    use gst::glib::DateTime;
    let secs = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let (Ok(at), Ok(now)) = (DateTime::from_unix_local(secs), DateTime::now_local()) else {
        return String::new();
    };
    let day = |d: &DateTime| d.ymd();
    let fmt = |f: &str| at.format(f).map(|s| s.to_string()).unwrap_or_default();
    let days_ago = (0..7).find(|&n| {
        now.add_days(-n)
            .map(|d| day(&d) == day(&at))
            .unwrap_or(false)
    });
    match days_ago {
        Some(0) => fmt("Today %H:%M"),
        Some(1) => fmt("Yesterday %H:%M"),
        Some(_) => fmt("%a %H:%M"),
        None => fmt("%b %d"),
    }
}

fn length(secs: Option<f64>) -> String {
    secs.map(|s| {
        let s = s.round() as u64;
        format!("{}:{:02}", s / 60, s % 60)
    })
    .unwrap_or_default()
}

fn thumb_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::home().join(".cache"))
        .join("clip-recorder/thumbs")
}

fn thumb_path(clip: &Path) -> PathBuf {
    let name = clip.file_stem().unwrap_or_default().to_string_lossy();
    thumb_dir().join(format!("{name}.png"))
}

/// A cached thumbnail that is newer than its clip.
fn cached_thumb(clip: &Path, modified: SystemTime) -> Option<slint::Image> {
    let p = thumb_path(clip);
    let t = std::fs::metadata(&p).ok()?.modified().ok()?;
    (t >= modified).then(|| slint::Image::load_from_path(&p).ok())?
}

/// Decodes the frame at 1 s (or the first one) and stores it as a 480x270 PNG.
fn make_thumb(clip: &Path, out: &Path) -> Result<()> {
    let pipeline = gst::parse::launch(
        "filesrc name=src ! decodebin ! videoconvert ! videoscale \
         ! video/x-raw,format=RGB,width=480,height=270,pixel-aspect-ratio=1/1 \
         ! pngenc ! appsink name=sink sync=false",
    )?
    .downcast::<gst::Pipeline>()
    .map_err(|_| anyhow::anyhow!("not a pipeline"))?;
    pipeline
        .by_name("src")
        .context("no filesrc")?
        .set_property("location", clip.to_string_lossy().as_ref());
    let sink = pipeline
        .by_name("sink")
        .and_downcast::<gst_app::AppSink>()
        .context("no appsink")?;
    let result = (|| {
        pipeline.set_state(gst::State::Paused)?;
        let (r, ..) = pipeline.state(gst::ClockTime::from_seconds(5));
        r?;
        if pipeline
            .seek_simple(
                gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                gst::ClockTime::from_seconds(1),
            )
            .is_ok()
        {
            let _ = pipeline.state(gst::ClockTime::from_seconds(5));
        }
        let sample = sink
            .try_pull_preroll(gst::ClockTime::from_seconds(5))
            .context("no frame")?;
        let buf = sample.buffer().context("empty frame")?;
        let map = buf.map_readable()?;
        std::fs::create_dir_all(out.parent().unwrap())?;
        std::fs::write(out, map.as_slice())?;
        Ok(())
    })();
    let _ = pipeline.set_state(gst::State::Null);
    result
}

/// Makes missing thumbnails off the UI thread and hands each one back.
fn thumb_worker(ui: slint::Weak<Library>) -> std::sync::mpsc::Sender<PathBuf> {
    let (tx, rx) = std::sync::mpsc::channel::<PathBuf>();
    std::thread::spawn(move || {
        if gst::init().is_err() {
            return;
        }
        for clip in rx {
            let out = thumb_path(&clip);
            if let Err(e) = make_thumb(&clip, &out) {
                eprintln!("thumbnail {}: {e:#}", clip.display());
                continue;
            }
            let ui = ui.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = ui.upgrade() else { return };
                let Ok(img) = slint::Image::load_from_path(&out) else {
                    return;
                };
                let model = ui.get_clips();
                let path = clip.display().to_string();
                if let Some(i) = model.iter().position(|c| c.path == path) {
                    let mut c = model.row_data(i).unwrap();
                    c.thumb = img;
                    model.set_row_data(i, c);
                }
            });
        }
    });
    tx
}

/// Only one library window: a second `clip-recorder library` raises the first.
fn single_instance(ui: slint::Weak<Library>) -> Option<std::os::unix::net::UnixListener> {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
    let addr = SocketAddr::from_abstract_name(b"clip-recorder-library").ok()?;
    match UnixListener::bind_addr(&addr) {
        Ok(l) => {
            let listener = l.try_clone().ok()?;
            std::thread::spawn(move || {
                for _ in listener.incoming() {
                    let ui = ui.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui.upgrade() {
                            ui.window().with_winit_window(|w| w.focus_window());
                        }
                    });
                }
            });
            Some(l)
        }
        Err(_) => {
            let _ = UnixStream::connect_addr(&addr);
            None
        }
    }
}

fn hotkey_keys(combo: &str) -> ModelRc<SharedString> {
    let combo = if combo.is_empty() { "Alt+S" } else { combo };
    let v: Vec<SharedString> = combo.split('+').map(SharedString::from).collect();
    ModelRc::new(VecModel::from(v))
}

pub fn library() -> Result<()> {
    gst::init()?;
    let ui = Library::new()?;
    let Some(_instance) = single_instance(ui.as_weak()) else {
        return Ok(());
    };
    let thumbs = thumb_worker(ui.as_weak());
    let output = Rc::new(RefCell::new(String::new()));
    let listed: Rc<RefCell<Option<Vec<(PathBuf, SystemTime)>>>> = Rc::default();

    let refresh = {
        let ui = ui.as_weak();
        let output = output.clone();
        move || {
            let Some(ui) = ui.upgrade() else { return };
            let s = ipc::status().ok();
            let cfg = Config::load();
            let (window, quality, kbps, video, dir, folder, hotkey) = match &s {
                Some(s) => (
                    num(s, "window") as u32,
                    get(s, "quality").to_owned(),
                    num(s, "kbps"),
                    get(s, "video").to_owned(),
                    get(s, "output_abs").to_owned(),
                    get(s, "output").to_owned(),
                    get(s, "hotkey").to_owned(),
                ),
                None => (
                    cfg.duration,
                    cfg.quality.name().to_owned(),
                    cfg.quality.kbps() as u64,
                    String::new(),
                    cfg.output.display().to_string(),
                    config::tilde(&cfg.output),
                    String::new(),
                ),
            };
            let (live, label) = match s.as_ref().map(|s| get(s, "state")) {
                Some("live") => (true, format!("Buffering · {window} s")),
                Some("idle") => (false, "Idle · no game".to_owned()),
                Some("paused") => (false, "Paused".to_owned()),
                _ => (false, "Stopped · start".to_owned()),
            };
            ui.set_live(live);
            ui.set_state_label(label.into());
            ui.set_seconds(window as i32);
            if !hotkey.is_empty() || s.is_none() {
                ui.set_hotkey(hotkey_keys(&hotkey));
            }
            let q = QUALITY.iter().position(|x| x.name() == quality);
            ui.set_quality(q.map_or(-1, |i| i as i32));
            let d = DURATIONS.iter().position(|&x| x == window);
            ui.set_duration(d.map_or(-1, |i| i as i32));
            let mbps = kbps as f64 / 1000.0;
            let note = if video.is_empty() {
                format!("{mbps:.0} Mb/s")
            } else {
                format!("{video} · {mbps:.0} Mb/s")
            };
            ui.set_quality_note(note.into());
            let ram = (kbps + AUDIO_KBPS) * 1000 / 8 * window as u64;
            ui.set_duration_note(format!("~{} MB RAM", (ram + 500_000) / 1_000_000).into());

            let dir_changed = *output.borrow() != dir;
            *output.borrow_mut() = dir.clone();
            let clips = clips::list(dir.as_ref());
            let sig: Vec<_> = clips.iter().map(|c| (c.path.clone(), c.modified)).collect();
            if !dir_changed && listed.borrow().as_ref() == Some(&sig) {
                return;
            }
            *listed.borrow_mut() = Some(sig);
            let (bytes, _) = clips::usage(dir.as_ref());
            ui.set_folder_text(format!("{folder} · {} MB", (bytes + 500_000) / 1_000_000).into());
            let old = ui.get_clips();
            let cards: Vec<ClipCard> = clips
                .into_iter()
                .map(|c| {
                    let path = c.path.display().to_string();
                    let thumb = old
                        .iter()
                        .find(|o| o.path == path && o.thumb.size().width > 0)
                        .map(|o| o.thumb)
                        .or_else(|| cached_thumb(&c.path, c.modified))
                        .unwrap_or_else(|| {
                            let _ = thumbs.send(c.path.clone());
                            slint::Image::default()
                        });
                    ClipCard {
                        title: c.title.into(),
                        when: when(c.modified).into(),
                        length: length(c.seconds).into(),
                        path: path.into(),
                        thumb,
                    }
                })
                .collect();
            ui.set_clips(ModelRc::new(VecModel::from(cards)));
        }
    };
    refresh();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(1000), refresh);

    ui.on_open_clip(|p| open_path(&p));
    ui.on_set_quality(|i| {
        if let Some(&q) = QUALITY.get(i as usize) {
            change("quality", move |c| c.quality = q);
        }
    });
    ui.on_set_duration(|i| {
        if let Some(&secs) = DURATIONS.get(i as usize) {
            change("duration", move |c| c.duration = secs);
        }
    });
    // Paused or idle: resume the running recorder; stopped: start one.
    ui.on_start_recorder(|| {
        if ipc::status().is_ok() {
            request_async("resume");
        } else {
            spawn_detached(&["run"]);
        }
    });
    {
        let weak = ui.as_weak();
        ui.on_drag(move || {
            if let Some(ui) = weak.upgrade() {
                ui.window().with_winit_window(|w| {
                    let _ = w.drag_window();
                });
            }
        });
    }
    ui.on_close_window(|| {
        let _ = slint::quit_event_loop();
    });
    ui.run()?;
    Ok(())
}

enum OsdEvent {
    Saving,
    /// Seconds saved.
    Saved(String),
    /// Short reason ("Disk full").
    Failed(String),
    Closed,
}

fn parse_osd(line: &str) -> Option<OsdEvent> {
    let (kind, rest) = line.split_once(' ').unwrap_or((line, ""));
    let f: Vec<&str> = rest.split('\t').collect();
    Some(match kind {
        "saving" => OsdEvent::Saving,
        "saved" => OsdEvent::Saved(f[0].to_owned()),
        "failed" => OsdEvent::Failed(f[0].to_owned()),
        _ => return None,
    })
}

/// The toast. Reads state lines from stdin (see `daemon::Osd`). Nothing shows
/// while saving; "saved" shows for 2.4 s, "failed" stays until clicked.
pub fn osd() -> Result<()> {
    let ui = Osd::new()?;
    if let Some((w, _)) = screen_size() {
        ui.window()
            .set_position(slint::PhysicalPosition::new(w as i32 - 340, 20));
    }
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines().map_while(Result::ok) {
            if let Some(ev) = parse_osd(&line) {
                let w = weak.clone();
                let _ = slint::invoke_from_event_loop(move || apply_osd(w, ev));
            }
        }
        let _ = slint::invoke_from_event_loop(move || apply_osd(weak, OsdEvent::Closed));
    });

    let started = Rc::new(std::cell::Cell::new(Instant::now()));
    let state = Rc::new(std::cell::Cell::new(0));
    let tick = {
        let ui = ui.as_weak();
        move || {
            let Some(ui) = ui.upgrade() else { return };
            match ui.get_state() {
                0 => {
                    let t = started.get().elapsed().as_secs_f32();
                    ui.set_progress(0.2 + 0.6 * (1.0 - (-t * 3.0).exp()));
                }
                1 => {
                    if state.get() != 1 {
                        state.set(1);
                        started.set(Instant::now());
                    }
                    let left = 1.0 - started.get().elapsed().as_secs_f32() / 2.4;
                    ui.set_progress(left.max(0.0));
                    if left <= 0.0 {
                        let _ = slint::quit_event_loop();
                    }
                }
                _ => {}
            }
        }
    };
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(16), tick);
    ui.on_dismiss(|| {
        let _ = slint::quit_event_loop();
    });
    ui.on_open(|| {
        spawn_detached(&["library"]);
        let _ = slint::quit_event_loop();
    });
    slint::run_event_loop_until_quit()?;
    Ok(())
}

fn apply_osd(ui: slint::Weak<Osd>, ev: OsdEvent) {
    let Some(ui) = ui.upgrade() else { return };
    match ev {
        OsdEvent::Saving => ui.set_state(0),
        OsdEvent::Saved(secs) => {
            ui.set_state(1);
            ui.set_heading(format!("Saved {secs} s").into());
            ui.set_detail("Open in Clips".into());
            ui.set_progress(1.0);
            let _ = ui.show();
        }
        OsdEvent::Failed(reason) => {
            ui.set_state(2);
            ui.set_heading("Couldn't save".into());
            ui.set_detail(reason.into());
            let _ = ui.show();
        }
        // The recorder went away mid-save: don't leave a spinner behind.
        OsdEvent::Closed if ui.get_state() == 0 => {
            let _ = slint::quit_event_loop();
        }
        OsdEvent::Closed => {}
    }
}

/// Primary screen size on X11 via xrandr's "current WxH"; Wayland places the
/// toast itself.
fn screen_size() -> Option<(u32, u32)> {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        return None;
    }
    let out = Command::new("xrandr").arg("--current").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let cur = text.split("current ").nth(1)?;
    let mut it = cur
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty());
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}
