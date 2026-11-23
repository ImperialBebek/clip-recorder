//! The windows: the clips library window. Each runs as its own
//! short-lived process (`clip-recorder library`) and talks to the
//! recorder over the control socket, so the recorder never loads a GUI.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use slint::winit_030::WinitWindowAccessor;
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};

use crate::capture::AudioSource;
use crate::clips;
use crate::config::{self, Config, Quality};
use crate::platform::{self, ipc};

slint::include_modules!();

type Status = HashMap<String, String>;

/// The export thread's report: (clip generation, error text if it failed).
type ExportDone = Arc<Mutex<Option<(u64, Option<String>)>>>;

/// What `games::lookup` resolved for a slug, with the icon already loaded.
struct GameUi {
    name: Option<String>,
    icon: Option<slint::Image>,
}

/// The lookup worker's reports: (slug, name, icon path to load on the UI thread).
type GameDone = Arc<Mutex<Vec<(String, Option<String>, Option<PathBuf>)>>>;

/// First character of `name` upper-cased, for the badge when a game has no icon.
fn initial(name: &str) -> SharedString {
    name.chars()
        .next()
        .map(|c| c.to_uppercase().collect::<String>())
        .unwrap_or_default()
        .into()
}

fn spawn_detached(args: &[&str]) {
    if let Ok(exe) = std::env::current_exe() {
        let _ = Command::new(exe).args(args).spawn();
    }
}

fn open_path(p: impl AsRef<Path>) {
    let _ = Command::new("xdg-open").arg(p.as_ref()).spawn();
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

/// Recounts the selected cards into the footer's "n selected" label.
fn sync_selection(ui: &Library) {
    let n = ui.get_clips().iter().filter(|c| c.selected).count();
    ui.set_selected_count(n as i32);
}

/// Left edge of the card grid, matching the 24 px padding in `library.slint`.
const GRID_PAD: f32 = 24.0;

/// Grid geometry in logical pixels, as laid out in `library.slint`.
#[derive(Clone, Copy)]
struct Grid {
    card_w: f32,
    card_h: f32,
    gap: f32,
    cols: usize,
    count: usize,
}

impl Grid {
    /// The card under a point, if the point is on a card and not in a gap.
    fn hit(&self, x: f32, y: f32) -> Option<usize> {
        let (cx, cy) = (x - GRID_PAD, y);
        if cx < 0.0 || cy < 0.0 {
            return None;
        }
        let (col, row) = (
            (cx / (self.card_w + self.gap)) as usize,
            (cy / (self.card_h + self.gap)) as usize,
        );
        let in_card = cx - col as f32 * (self.card_w + self.gap) <= self.card_w
            && cy - row as f32 * (self.card_h + self.gap) <= self.card_h;
        let i = row * self.cols + col;
        (col < self.cols && in_card && i < self.count).then_some(i)
    }

    /// Every card a rectangle (x0, y0)-(x1, y1) touches.
    fn band(&self, x0: f32, y0: f32, x1: f32, y1: f32) -> Vec<usize> {
        (0..self.count)
            .filter(|&i| {
                let left = GRID_PAD + (i % self.cols) as f32 * (self.card_w + self.gap);
                let top = (i / self.cols) as f32 * (self.card_h + self.gap);
                left < x1 && left + self.card_w > x0 && top < y1 && top + self.card_h > y0
            })
            .collect()
    }
}

/// Sets `selected` on every card with `f(index, was_selected)`.
fn map_selection(ui: &Library, f: impl Fn(usize, bool) -> bool) {
    let model = ui.get_clips();
    for i in 0..model.row_count() {
        let mut c = model.row_data(i).unwrap();
        let on = f(i, c.selected);
        if on != c.selected {
            c.selected = on;
            model.set_row_data(i, c);
        }
    }
    sync_selection(ui);
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

/// Keyed by the full path, so same-named clips in different folders differ.
fn thumb_path(clip: &Path) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    clip.hash(&mut h);
    let name = clip.file_stem().unwrap_or_default().to_string_lossy();
    thumb_dir().join(format!("{name}-{:016x}.png", h.finish()))
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

/// Makes missing thumbnails off the UI thread and hands each one back
/// through `done` (the poll timer loads the image and stores it on the card).
fn thumb_worker(done: Arc<Mutex<Vec<(PathBuf, PathBuf)>>>) -> std::sync::mpsc::Sender<PathBuf> {
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
            done.lock().unwrap().push((clip, out));
        }
    });
    tx
}

/// Resolves game slugs to display names and icon paths (Steam manifests can
/// sit on a spinning library disk). Images load back on the UI thread.
fn game_worker(done: GameDone) -> std::sync::mpsc::Sender<Vec<String>> {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<String>>();
    std::thread::spawn(move || {
        for slugs in rx {
            let found = crate::games::lookup(&slugs);
            done.lock().unwrap().extend(slugs.into_iter().map(|slug| {
                let i = found.get(&slug);
                (
                    slug,
                    i.and_then(|i| i.name.clone()),
                    i.and_then(|i| i.icon.clone()),
                )
            }));
        }
    });
    tx
}

fn onboarded_marker() -> PathBuf {
    crate::platform::state_dir().join("onboarded")
}

/// Whether the welcome screen was seen (dismissed) before.
pub fn onboarded() -> bool {
    onboarded_marker().exists()
}

fn set_onboarded() {
    let p = onboarded_marker();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, "");
}

/// Only one library window: a second `clip-recorder library` raises the first.
/// The peer's uid on a Unix socket, or `None` when the kernel won't say.
fn peer_uid(stream: &std::os::unix::net::UnixStream) -> Option<libc::uid_t> {
    use std::os::unix::io::AsRawFd;
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` is a valid ucred the size of `len`, filled on success.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (rc == 0).then_some(cred.uid)
}

fn single_instance(ui: slint::Weak<Library>) -> Option<std::os::unix::net::UnixListener> {
    use std::io::Read;
    use std::os::unix::net::UnixListener;
    let addr = ipc::library_addr()?;
    match UnixListener::bind_addr(&addr) {
        Ok(l) => {
            let listener = l.try_clone().ok()?;
            std::thread::spawn(move || {
                // Abstract sockets have no filesystem permissions; only our
                // own uid may talk to the library.
                // SAFETY: getuid takes no input and cannot fail.
                let own_uid = unsafe { libc::getuid() };
                for mut stream in listener.incoming().flatten() {
                    if peer_uid(&stream) != Some(own_uid) {
                        continue;
                    }
                    let mut line = String::new();
                    if stream.read_to_string(&mut line).is_err() {
                        continue;
                    }
                    let ui = ui.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = ui.upgrade() else { return };
                        match ipc::parse_library_msg(&line) {
                            Some(ipc::LibraryMsg::Focus(token)) => {
                                ui.window().with_winit_window(|w| {
                                    match token.filter(|t| !t.is_empty()) {
                                        Some(t) => {
                                            if platform::activate::activate(w, &t).is_err() {
                                                w.focus_window();
                                            }
                                        }
                                        None => w.focus_window(),
                                    }
                                });
                            }
                            Some(ipc::LibraryMsg::Quit) => {
                                if ui.get_onboarding() {
                                    set_onboarded();
                                }
                                let _ = slint::quit_event_loop();
                            }
                            None => {}
                        }
                    });
                }
            });
            Some(l)
        }
        Err(_) => None,
    }
}

/// Hands a move or resize to the compositor. The compositor then owns the
/// pointer, so Slint never sees the button go up and its touch area would keep
/// the pointer grabbed; release it once the current event is done.
fn compositor_drag(
    ui: &Library,
    x: f32,
    y: f32,
    start: impl FnOnce(&slint::winit_030::winit::window::Window),
) {
    ui.window().with_winit_window(start);
    let weak = ui.as_weak();
    Timer::single_shot(Duration::ZERO, move || {
        if let Some(ui) = weak.upgrade() {
            ui.window()
                .dispatch_event(slint::platform::WindowEvent::PointerReleased {
                    position: slint::LogicalPosition::new(x, y),
                    button: slint::platform::PointerEventButton::Left,
                });
        }
    });
}

/// `text` cut to at most `max` characters, with an ellipsis when it was longer.
fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        let head: String = text.chars().take(max - 1).collect();
        format!("{}…", head.trim_end())
    }
}

fn hotkey_keys(combo: &str) -> ModelRc<SharedString> {
    let combo = if combo.is_empty() { "Alt+S" } else { combo };
    let v: Vec<SharedString> = combo.split('+').map(SharedString::from).collect();
    ModelRc::new(VecModel::from(v))
}

/// The always-first lane of the player's timeline.
fn video_lane() -> PlayerTrack {
    PlayerTrack {
        name: "Video".into(),
        audio: false,
        muted: false,
        wave: "".into(),
        volume: 1.0,
    }
}

/// Path commands for a waveform lane: the peaks mirrored around the centre,
/// in a `bins x 2` viewbox.
fn wave_path(peaks: &[f32]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(peaks.len() * 14);
    s.push_str("M 0 1");
    for (i, &p) in peaks.iter().enumerate() {
        let _ = write!(s, " L {} {:.2}", i, 1.0 - p);
    }
    let _ = write!(s, " L {} 1", peaks.len());
    for (i, &p) in peaks.iter().enumerate().rev() {
        let _ = write!(s, " L {} {:.2}", i + 1, 1.0 + p);
    }
    s.push_str(" Z");
    s
}

/// The export thread's job: compute the keyframe index here if the player
/// hasn't finished it yet, snap the cut onto one, and write the clip.
fn export_trimmed(
    src: &Path,
    keys: Vec<f64>,
    tin: f64,
    tout: f64,
    mix: &[(f64, bool)],
    out: &Path,
) -> Result<()> {
    let keys = if keys.is_empty() {
        crate::trim::keyframes(src).unwrap_or_default()
    } else {
        keys
    };
    let start = crate::trim::snap_start(&keys, tin);
    // Written under a hidden, non-.mp4 name and renamed when complete, so an
    // export cut short (library closed, error) never shows up as a clip.
    let name = out.file_name().unwrap_or_default().to_string_lossy();
    let part = out.with_file_name(format!(".{name}.part"));
    let result = crate::trim::export(src, start, tout, mix, &part)
        .and_then(|()| Ok(std::fs::rename(&part, out)?));
    if result.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    result
}

/// Seconds as `m:ss`.
fn clock(t: f32) -> String {
    let t = t.max(0.0).round() as u64;
    format!("{}:{:02}", t / 60, t % 60)
}

pub fn library() -> Result<()> {
    gst::init()?;
    // KDE passes this to launched apps; keep it from leaking to xdg-open.
    let activation = std::env::var("XDG_ACTIVATION_TOKEN").ok();
    if activation.is_some() {
        std::env::remove_var("XDG_ACTIVATION_TOKEN");
    }
    let ui = Library::new()?;
    let Some(_instance) = single_instance(ui.as_weak()) else {
        // Another library is open: hand it our token so it can activate.
        ipc::tell_library(&activation.map_or("focus".to_owned(), |t| format!("focus {t}")));
        return Ok(());
    };
    // Every card the folder holds; `clips` on the window is the filtered
    // view of this, so filtering never re-lists files or re-loads thumbs.
    let master: Rc<RefCell<Vec<ClipCard>>> = Rc::default();
    // Resolved display name + icon per game slug, filled by the lookup worker.
    let game_infos: Rc<RefCell<HashMap<String, GameUi>>> = Rc::default();
    // Slugs the lookup worker is already resolving, so each is asked once.
    let game_pending: Rc<RefCell<HashSet<String>>> = Rc::default();
    // Worker results drained by the poll timer (Rc state can't cross threads):
    // thumbnails -> (clip, png), games -> (slug, name, icon path).
    let thumb_done: Arc<Mutex<Vec<(PathBuf, PathBuf)>>> = Arc::default();
    let game_done: GameDone = Arc::default();
    let thumbs = thumb_worker(thumb_done.clone());
    let games = game_worker(game_done.clone());
    let output = Rc::new(RefCell::new(String::new()));
    let listed: Rc<RefCell<Option<Vec<(PathBuf, SystemTime)>>>> = Rc::default();
    // Device ids backing the "Microphone" choices, aligned with indices 2+.
    let mic_ids: Rc<RefCell<Vec<String>>> = Rc::default();

    // "Off", "System default", then one row per capture source. A configured
    // device that is gone stays selectable as "Unavailable device".
    let load_mics = {
        let ui = ui.as_weak();
        let mic_ids = mic_ids.clone();
        move || {
            let Some(ui) = ui.upgrade() else { return };
            let devs = platform::mic_devices();
            let cfg = Config::load();
            let mut options = vec!["Off".into(), "System default".into()];
            let mut selected = if cfg.mic == AudioSource::None {
                0
            } else if cfg.mic_device.is_empty() {
                1
            } else {
                -1
            };
            mic_ids.borrow_mut().clear();
            for d in devs {
                if selected == -1 && d.id == cfg.mic_device {
                    selected = options.len() as i32;
                }
                options.push(d.label);
                mic_ids.borrow_mut().push(d.id);
            }
            if selected == -1 {
                options.push("Unavailable device".into());
                mic_ids.borrow_mut().push(cfg.mic_device.clone());
                selected = options.len() as i32 - 1;
            }
            let options: Vec<SharedString> = options.into_iter().map(Into::into).collect();
            ui.set_mics(ModelRc::new(VecModel::from(options)));
            ui.set_mic(selected);
            ui.set_app_tracks(ModelRc::new(VecModel::from(vec![
                ToggleRow {
                    label: "Game".into(),
                    detail: "only the game".into(),
                    on: cfg.track_game,
                },
                ToggleRow {
                    label: "Voice chat".into(),
                    detail: "Discord, TeamSpeak, Mumble…".into(),
                    on: cfg.track_voice,
                },
                ToggleRow {
                    label: "Other apps".into(),
                    detail: "browser, music, everything else".into(),
                    on: cfg.track_other,
                },
            ])));
        }
    };
    let load_mics = Rc::new(load_mics);
    load_mics();

    // The open clip: the pipeline, which library row it belongs to (the list
    // can change under it), and a generation counter so async results from a
    // previous clip are dropped. Shared with GStreamer threads, so atomics.
    let player: Rc<RefCell<Option<crate::player::Player>>> = Rc::default();
    let player_path: Rc<RefCell<Option<PathBuf>>> = Rc::default();
    let player_gen = Arc::new(AtomicU64::new(0));
    // Keyframe times of the open clip, filled in on a worker thread; the
    // generation keeps a stale result for a previous clip from being used.
    let trim_keys: Arc<Mutex<(u64, Vec<f64>)>> = Arc::default();
    // Export status handed back from the export thread to the poll timer.
    let export_done: ExportDone = Arc::default();
    let pos_timer = Rc::new(Timer::default());

    // Opens `path` in the player page, replacing whatever played before.
    let open_clip = {
        let player = player.clone();
        let player_path = player_path.clone();
        let player_gen = player_gen.clone();
        let trim_keys = trim_keys.clone();
        let pos_timer = pos_timer.clone();
        move |ui: &Library, path: &Path| {
            let gen = player_gen.fetch_add(1, Ordering::SeqCst) + 1;
            *player.borrow_mut() = None;
            let weak = ui.as_weak();
            let scheduled = Arc::new(AtomicBool::new(false));
            let shown_frame = Arc::new(AtomicBool::new(false));
            let want_frame = {
                let scheduled = scheduled.clone();
                move || !scheduled.load(Ordering::SeqCst)
            };
            let on_frame = {
                let weak = weak.clone();
                let player_gen = player_gen.clone();
                let latest = Arc::new(Mutex::new(
                    None::<slint::SharedPixelBuffer<slint::Rgba8Pixel>>,
                ));
                let scheduled = scheduled.clone();
                let shown_frame = shown_frame.clone();
                move |buf| {
                    *latest.lock().unwrap() = Some(buf);
                    if scheduled.swap(true, Ordering::SeqCst) {
                        return;
                    }
                    let weak = weak.clone();
                    let player_gen = player_gen.clone();
                    let latest = latest.clone();
                    let scheduled = scheduled.clone();
                    let shown_frame = shown_frame.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        scheduled.store(false, Ordering::SeqCst);
                        if player_gen.load(Ordering::SeqCst) != gen {
                            return;
                        }
                        let Some(buf) = latest.lock().unwrap().take() else { return };
                        if let Some(ui) = weak.upgrade() {
                            ui.set_frame(slint::Image::from_rgba8(buf));
                            shown_frame.store(true, Ordering::SeqCst);
                        }
                    });
                }
            };
            let on_tracks = {
                let weak = weak.clone();
                let player_gen = player_gen.clone();
                let path = path.to_path_buf();
                move |names: Vec<String>| {
                    let weak = weak.clone();
                    let player_gen = player_gen.clone();
                    let path = path.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if player_gen.load(Ordering::SeqCst) != gen {
                            return;
                        }
                        let Some(ui) = weak.upgrade() else { return };
                        let mut lanes = vec![video_lane()];
                        lanes.extend(names.iter().map(|n| PlayerTrack {
                            name: n.as_str().into(),
                            audio: true,
                            muted: false,
                            wave: "".into(),
                            volume: 1.0,
                        }));
                        ui.set_tracks(ModelRc::new(VecModel::from(lanes)));
                        // Waveforms and the filmstrip decode slowly; fill them
                        // in as they finish on one worker.
                        let weak = ui.as_weak();
                        let player_gen = player_gen.clone();
                        std::thread::spawn(move || {
                            for (i, _) in names.iter().enumerate() {
                                if player_gen.load(Ordering::SeqCst) != gen {
                                    return;
                                }
                                let Ok(peaks) = crate::player::waveform(&path, i, 600) else {
                                    continue;
                                };
                                let weak = weak.clone();
                                let player_gen = player_gen.clone();
                                let _ = slint::invoke_from_event_loop(move || {
                                    if player_gen.load(Ordering::SeqCst) != gen {
                                        return;
                                    }
                                    let Some(ui) = weak.upgrade() else { return };
                                    let model = ui.get_tracks();
                                    if let Some(mut t) = model.row_data(i + 1) {
                                        t.wave = wave_path(&peaks).into();
                                        model.set_row_data(i + 1, t);
                                    }
                                });
                            }
                            if player_gen.load(Ordering::SeqCst) != gen {
                                return;
                            }
                            if let Ok(frames) = crate::player::filmstrip(&path, 10, 160, 90) {
                                let _ = slint::invoke_from_event_loop(move || {
                                    if player_gen.load(Ordering::SeqCst) != gen {
                                        return;
                                    }
                                    let Some(ui) = weak.upgrade() else { return };
                                    let imgs: Vec<slint::Image> = frames
                                        .into_iter()
                                        .map(slint::Image::from_rgb8)
                                        .collect();
                                    ui.set_filmstrip(ModelRc::new(VecModel::from(imgs)));
                                });
                            }
                        });
                    });
                }
            };
            // A decoder that fails before showing a frame leaves a blank
            // player; back out to the grid and open the clip externally then.
            let on_error = {
                let weak = weak.clone();
                let player_gen = player_gen.clone();
                let path = path.to_path_buf();
                move |err: String| {
                    eprintln!("player {}: {err}", path.display());
                    if shown_frame.load(Ordering::SeqCst) {
                        return;
                    }
                    let weak = weak.clone();
                    let player_gen = player_gen.clone();
                    let path = path.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if player_gen.load(Ordering::SeqCst) != gen {
                            return;
                        }
                        let Some(ui) = weak.upgrade() else { return };
                        ui.invoke_player_back();
                        open_path(&path);
                    });
                }
            };
            let p = match crate::player::Player::open(
                path, on_frame, want_frame, on_tracks, on_error,
            ) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("player {}: {e:#}", path.display());
                    open_path(path);
                    return;
                }
            };
            p.set_volume(ui.get_volume() as f64);
            p.set_muted(ui.get_muted());
            p.play();
            *player.borrow_mut() = Some(p);
            *player_path.borrow_mut() = Some(path.to_path_buf());
            let card = ui
                .get_clips()
                .iter()
                .find(|c| Path::new(c.path.as_str()) == path);
            ui.set_player_title(card.as_ref().map(|c| c.title.clone()).unwrap_or_else(|| {
                path.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
                    .into()
            }));
            ui.set_player_icon(card.as_ref().map(|c| c.icon.clone()).unwrap_or_default());
            ui.set_player_initial(
                card.as_ref().map(|c| c.initial.clone()).unwrap_or_default(),
            );
            ui.set_frame(slint::Image::default());
            ui.set_filmstrip(ModelRc::new(VecModel::from(
                Vec::<slint::Image>::new(),
            )));
            ui.set_tracks(ModelRc::new(VecModel::from(vec![video_lane()])));
            ui.set_pos(0.0);
            ui.set_dur(0.0);
            ui.set_trim_in(0.0);
            ui.set_trim_out(0.0);
            ui.set_export_status("".into());
            ui.set_playing(true);
            ui.set_player_open(true);
            // Where the trims can snap to; a stale result for the previous
            // clip is dropped via the generation check.
            *trim_keys.lock().unwrap() = (gen, Vec::new());
            {
                let player_gen = player_gen.clone();
                let trim_keys = trim_keys.clone();
                let path = path.to_path_buf();
                std::thread::spawn(move || {
                    let Ok(keys) = crate::trim::keyframes(&path) else {
                        return;
                    };
                    let mut slot = trim_keys.lock().unwrap();
                    if player_gen.load(Ordering::SeqCst) == gen {
                        *slot = (gen, keys);
                    }
                });
            }
            pos_timer.start(
                TimerMode::Repeated,
                Duration::from_millis(33),
                {
                    let weak = weak.clone();
                    let player = player.clone();
                    move || {
                        let Some(ui) = weak.upgrade() else { return };
                        let borrow = player.borrow();
                        let Some(p) = borrow.as_ref() else { return };
                        if let Some(d) = p.duration() {
                            ui.set_dur(d.seconds_f64() as f32);
                        }
                        let dur = ui.get_dur();
                        // The end of the clip is the default trim-out.
                        if dur > 0.0 && ui.get_trim_out() <= 0.0 {
                            ui.set_trim_out(dur);
                        }
                        let (tin, tout) = (ui.get_trim_in(), ui.get_trim_out());
                        // The scrub position owns the playhead while the user
                        // drags; a queued seek owns it right after release.
                        let mut pos = if ui.get_scrubbing() {
                            ui.get_scrub_pos()
                        } else if p.seeking() {
                            ui.get_pos()
                        } else if p.ended() && dur > 0.0 {
                            dur
                        } else {
                            let pos = p
                                .position()
                                .map_or(ui.get_pos(), |t| t.seconds_f64() as f32);
                            let pos = if dur > 0.0 { pos.min(dur) } else { pos };
                            ui.set_pos(pos);
                            pos
                        };
                        // A trimmed range loops: at trim-out, jump back to
                        // trim-in. Covering the whole clip keeps normal EOS.
                        if p.playing()
                            && !ui.get_scrubbing()
                            && tout > tin
                            && tout < dur - 0.05
                            && pos >= tout
                        {
                            p.seek(gst::ClockTime::from_seconds_f64(tin as f64));
                            pos = tin;
                            ui.set_pos(pos);
                        }
                        ui.set_playing(p.playing());
                        ui.set_player_time(
                            format!("{} / {}", clock(pos), clock(dur)).into(),
                        );
                        ui.set_trim_label(
                            format!(
                                "{} – {} · {}s",
                                clock(tin),
                                clock(tout),
                                (tout - tin).max(0.0).round() as u64
                            )
                            .into(),
                        );
                    }
                },
            );
        }
    };
    let open_clip = Rc::new(open_clip);

    // The Undo toast: the batch it can restore (dir, original paths) and the
    // clock driving its 6 s bar. A newer trash replaces the older batch.
    let trashed: Rc<RefCell<(PathBuf, Vec<PathBuf>)>> = Rc::default();
    let toast_at: Rc<Cell<Option<Instant>>> = Rc::default();
    let toast_timer = Rc::new(Timer::default());
    let show_toast = {
        let weak = ui.as_weak();
        let toast_at = toast_at.clone();
        let toast_timer = toast_timer.clone();
        move |ui: &Library, message: String, undo: bool| {
            ui.set_toast_message(message.into());
            ui.set_toast_undo(undo);
            ui.set_toast_frac(1.0);
            ui.set_toast_visible(true);
            toast_at.set(Some(Instant::now()));
            let weak2 = weak.clone();
            let toast_at2 = toast_at.clone();
            let timer2 = toast_timer.clone();
            toast_timer.start(
                TimerMode::Repeated,
                Duration::from_millis(33),
                move || {
                    let Some(ui) = weak2.upgrade() else { return };
                    let Some(at) = toast_at2.get() else { return };
                    let left = 1.0 - at.elapsed().as_secs_f32() / 6.0;
                    if left <= 0.0 {
                        ui.set_toast_frac(0.0);
                        ui.set_toast_visible(false);
                        toast_at2.set(None);
                        timer2.stop();
                    } else {
                        ui.set_toast_frac(left);
                    }
                },
            );
        }
    };
    let show_toast = Rc::new(show_toast);

    // Moves `paths` to the Trash. On success it stores the batch for Undo
    // and returns the trashed paths so the caller can word its toast; a
    // clip that wouldn't go is left alone, not deleted.
    let trash_files = {
        let output = output.clone();
        let trashed = trashed.clone();
        let show_toast = show_toast.clone();
        move |ui: &Library, paths: Vec<PathBuf>| -> Vec<PathBuf> {
            if paths.is_empty() {
                return Vec::new();
            }
            let dir = PathBuf::from(output.borrow().as_str());
            let gone = clips::trash(&dir, &paths);
            if gone.is_empty() {
                (*show_toast)(ui, "Couldn't move to Trash".into(), false);
            } else {
                *trashed.borrow_mut() = (dir, gone.clone());
            }
            gone
        }
    };
    let trash_files = Rc::new(trash_files);

    // The clips the delete dialog would remove permanently: the selected
    // cards, or the clip open in the player, staged when the dialog opens.
    let dialog_paths: Rc<RefCell<Vec<PathBuf>>> = Rc::default();
    let open_dialog = {
        let master = master.clone();
        let dialog_paths = dialog_paths.clone();
        move |ui: &Library, paths: Vec<PathBuf>| {
            if paths.is_empty() {
                return;
            }
            *dialog_paths.borrow_mut() = paths.clone();
            let cards = master.borrow();
            let rows: Vec<ClipCard> = paths
                .iter()
                .filter_map(|p| {
                    let path = p.display().to_string();
                    cards.iter().find(|c| c.path == path).cloned()
                })
                .collect();
            ui.set_dialog_rows(ModelRc::new(VecModel::from(rows)));
            let secs: f64 = paths.iter().filter_map(|p| clips::mp4_seconds(p)).sum();
            let bytes: u64 = paths
                .iter()
                .filter_map(|p| std::fs::metadata(p).ok().map(|m| m.len()))
                .sum();
            let mb = (bytes + 500_000) / 1_000_000;
            let folder = ui.get_folder_text();
            let (title, meta, body) = if paths.len() == 1 {
                (
                    "Delete this clip?".to_owned(),
                    format!("1 clip · {} · {mb} MB", clock(secs as f32)),
                    format!("It moves to the Trash and leaves {folder}. You can undo right after."),
                )
            } else {
                (
                    format!("Delete {} clips?", paths.len()),
                    format!(
                        "{} clips · {} total · {mb} MB",
                        paths.len(),
                        clock(secs as f32)
                    ),
                    format!("They move to the Trash and leave {folder}. You can undo right after."),
                )
            };
            ui.set_dialog_title(title.into());
            ui.set_dialog_meta(meta.into());
            ui.set_dialog_body(body.into());
            ui.set_dialog_open(true);
        }
    };
    let open_dialog = Rc::new(open_dialog);

    // Rebuilds the visible `clips` model and the sidebar from `master`.
    // `keep_sel` carries the current selection over (thumbnail and game-icon
    // updates); a filter change passes false so selection and the delete
    // confirmation start over.
    let apply_filter = {
        let master = master.clone();
        let game_infos = game_infos.clone();
        move |ui: &Library, keep_sel: bool| {
            // A selected game that no longer has clips drops back to "All".
            if !ui.get_game_filter().is_empty()
                && !master
                    .borrow()
                    .iter()
                    .any(|c| c.game == ui.get_game_filter())
            {
                ui.set_game_filter("".into());
            }
            let gf = ui.get_game_filter();
            let exported = ui.get_exported_only();
            let selected: HashSet<SharedString> = if keep_sel {
                ui.get_clips()
                    .iter()
                    .filter(|c| c.selected)
                    .map(|c| c.path)
                    .collect()
            } else {
                HashSet::new()
            };
            let infos = game_infos.borrow();
            let cards = master.borrow();
            // The master list is newest-first, so first appearance order is
            // "sorted by most recent clip" for the chips.
            let mut seen = HashSet::new();
            let mut chips = Vec::new();
            for c in cards.iter() {
                if seen.insert(c.game.clone()) {
                    let info = infos.get(c.game.as_str());
                    let name = info
                        .and_then(|g| g.name.clone())
                        .unwrap_or_else(|| c.title.trim_end().to_owned());
                    chips.push(GameChip {
                        key: c.game.clone(),
                        name: name.clone().into(),
                        icon: info.and_then(|g| g.icon.clone()).unwrap_or_default(),
                        initial: initial(&name),
                        count: cards.iter().filter(|x| x.game == c.game).count() as i32,
                    });
                }
            }
            let title = if gf.is_empty() {
                "All clips".to_owned()
            } else {
                chips
                    .iter()
                    .find(|c| c.key == gf)
                    .map(|c| c.name.to_string())
                    .unwrap_or_default()
            };
            ui.set_header_title(title.into());
            ui.set_games(ModelRc::new(VecModel::from(chips)));
            ui.set_total_count(cards.len() as i32);
            let visible: Vec<ClipCard> = cards
                .iter()
                .filter(|c| (gf.is_empty() || c.game == gf) && (!exported || c.exported))
                .cloned()
                .map(|mut c| {
                    c.selected = selected.contains(&c.path);
                    c
                })
                .collect();
            // The scope is every clip the game filter lets through; the
            // Exported segment can narrow it further.
            let scope = cards
                .iter()
                .filter(|c| gf.is_empty() || c.game == gf)
                .count();
            ui.set_header_count(
                if exported {
                    format!("{} of {scope} clips", visible.len())
                } else if scope == 1 {
                    "1 clip".to_owned()
                } else {
                    format!("{scope} clips")
                }
                .into(),
            );
            ui.set_clips(ModelRc::new(VecModel::from(visible)));
            if ui.get_clips().row_count() == 0 {
                ui.set_select_mode(false);
            }
            sync_selection(ui);
        }
    };
    let apply_filter = Rc::new(apply_filter);

    let refresh = {
        let ui = ui.as_weak();
        let output = output.clone();
        let master = master.clone();
        let game_infos = game_infos.clone();
        let game_pending = game_pending.clone();
        let games = games.clone();
        let apply_filter = apply_filter.clone();
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
            let live_game = s
                .as_ref()
                .filter(|s| get(s, "state") == "live")
                .map(|s| get(s, "game"))
                .filter(|g| !g.is_empty())
                .map(str::to_owned);
            let (live, label) = match s.as_ref().map(|s| get(s, "state")) {
                Some("live") => (
                    true,
                    live_game
                        .as_deref()
                        .map_or_else(|| "Recording screen".to_owned(), |g| shorten(g, 26)),
                ),
                Some("idle") => (false, "Waiting for a game".to_owned()),
                Some("paused") => (false, "Paused".to_owned()),
                Some("setup") => (false, "Choose a screen".to_owned()),
                Some("error") => (false, "Not recording".to_owned()),
                _ => (false, "Stopped · start".to_owned()),
            };
            ui.set_screen_picker(s.as_ref().is_none_or(|s| get(s, "capture") != "game"));
            ui.set_live(live);
            ui.set_state_label(label.into());
            // The pill shows the live game's icon; its slug shares the clips'
            // naming space, so the same lookup as the cards applies.
            if let Some(game) = &live_game {
                let slug = clips::slug(game);
                let info = game_infos
                    .borrow()
                    .get(&slug)
                    .map(|i| (i.icon.clone(), i.name.clone()))
                    .unwrap_or((None, None));
                ui.set_pill_icon(info.0.unwrap_or_default());
                ui.set_pill_initial(initial(info.1.as_deref().unwrap_or(game)));
                let mut pending = game_pending.borrow_mut();
                if !game_infos.borrow().contains_key(&slug) && pending.insert(slug.clone()) {
                    let _ = games.send(vec![slug]);
                }
            } else {
                ui.set_pill_icon(slint::Image::default());
                ui.set_pill_initial(SharedString::default());
            }
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
            let mic_on = s
                .as_ref()
                .map_or(cfg.mic != AudioSource::None, |s| {
                    get(s, "mic_setting") != "none"
                });
            let app_tracks = match cfg.audio {
                AudioSource::None => 0,
                AudioSource::Auto => {
                    u64::from(cfg.track_game)
                        + u64::from(cfg.track_voice)
                        + u64::from(cfg.track_other)
                }
                _ => 1,
            };
            let tracks = app_tracks + u64::from(mic_on);
            let ram = (kbps + AUDIO_KBPS * tracks) * 1000 / 8 * window as u64;
            ui.set_duration_note(format!("~{} MB RAM", (ram + 500_000) / 1_000_000).into());

            let dir_changed = *output.borrow() != dir;
            *output.borrow_mut() = dir.clone();
            let clips = clips::list(dir.as_ref());
            let sig: Vec<_> = clips.iter().map(|c| (c.path.clone(), c.modified)).collect();
            if !dir_changed && listed.borrow().as_ref() == Some(&sig) {
                return;
            }
            *listed.borrow_mut() = Some(sig);
            let (bytes, count) = clips::usage(dir.as_ref());
            ui.set_folder_text(folder.into());
            ui.set_usage_text(
                format!("{count} clips · {} MB", (bytes + 500_000) / 1_000_000).into(),
            );
            let old = ui.get_clips();
            let infos = game_infos.borrow();
            let cards: Vec<ClipCard> = clips
                .into_iter()
                .map(|c| {
                    let path = c.path.display().to_string();
                    let selected = old.iter().any(|o| o.path == path && o.selected);
                    let thumb = old
                        .iter()
                        .find(|o| o.path == path && o.thumb.size().width > 0)
                        .map(|o| o.thumb)
                        .or_else(|| {
                            master
                                .borrow()
                                .iter()
                                .find(|o| o.path == path && o.thumb.size().width > 0)
                                .map(|o| o.thumb.clone())
                        })
                        .or_else(|| cached_thumb(&c.path, c.modified))
                        .unwrap_or_else(|| {
                            let _ = thumbs.send(c.path.clone());
                            slint::Image::default()
                        });
                    let info = infos.get(c.game.as_str());
                    let title = info
                        .and_then(|g| g.name.clone())
                        .unwrap_or_else(|| c.title.clone());
                    ClipCard {
                        initial: initial(&title),
                        title: title.into(),
                        when: when(c.modified).into(),
                        length: length(c.seconds).into(),
                        path: path.into(),
                        thumb,
                        selected,
                        game: c.game.into(),
                        exported: c.exported,
                        icon: info.and_then(|g| g.icon.clone()).unwrap_or_default(),
                        size: format!("{} MB", (c.bytes + 500_000) / 1_000_000).into(),
                    }
                })
                .collect();
            drop(infos);
            *master.borrow_mut() = cards;
            // Names/icons for slugs not seen before resolve on the worker.
            let new_slugs: Vec<String> = {
                let infos = game_infos.borrow();
                let mut pending = game_pending.borrow_mut();
                master
                    .borrow()
                    .iter()
                    .filter_map(|c| {
                        let g = c.game.to_string();
                        (!infos.contains_key(&g) && pending.insert(g.clone())).then_some(g)
                    })
                    .collect()
            };
            if !new_slugs.is_empty() {
                let _ = games.send(new_slugs);
            }
            apply_filter(&ui, true);
        }
    };
    let refresh = Rc::new(refresh);
    refresh();
    let timer = Timer::default();
    {
        let refresh = refresh.clone();
        timer.start(
            TimerMode::Repeated,
            Duration::from_millis(1000),
            move || refresh(),
        );
    }
    // Picks up the worker threads' results; faster than the grid refresh so
    // "Exported" shows promptly. `refresh`, `master` and friends can't be
    // captured by the workers (they're Rc), so everything comes back through
    // shared slots and is applied here on the UI thread.
    let export_timer = Timer::default();
    {
        let weak = ui.as_weak();
        let player_gen = player_gen.clone();
        let export_done = export_done.clone();
        let thumb_done = thumb_done.clone();
        let game_done = game_done.clone();
        let master = master.clone();
        let game_infos = game_infos.clone();
        let game_pending = game_pending.clone();
        let apply_filter = apply_filter.clone();
        let refresh = refresh.clone();
        export_timer.start(
            TimerMode::Repeated,
            Duration::from_millis(100),
            move || {
                let Some(ui) = weak.upgrade() else { return };
                if let Some((gen, msg)) = export_done.lock().unwrap().take() {
                    ui.set_exporting(false);
                    if player_gen.load(Ordering::SeqCst) == gen {
                        ui.set_export_status(
                            msg.unwrap_or_else(|| "Exported".into()).into(),
                        );
                    }
                    refresh();
                }
                let thumbs = std::mem::take(&mut *thumb_done.lock().unwrap());
                if !thumbs.is_empty() {
                    {
                        let mut cards = master.borrow_mut();
                        for (clip, png) in thumbs {
                            let Ok(img) = slint::Image::load_from_path(&png)
                            else {
                                continue;
                            };
                            let path = clip.display().to_string();
                            if let Some(c) = cards.iter_mut().find(|c| c.path == path) {
                                c.thumb = img;
                            }
                        }
                    }
                    apply_filter(&ui, true);
                }
                let found = std::mem::take(&mut *game_done.lock().unwrap());
                if !found.is_empty() {
                    {
                        let mut infos = game_infos.borrow_mut();
                        for (slug, name, icon) in found {
                            game_pending.borrow_mut().remove(&slug);
                            let icon = icon
                                .and_then(|p| slint::Image::load_from_path(&p).ok());
                            infos.insert(slug, GameUi { name, icon });
                        }
                    }
                    // Cards that were drawn with fallbacks get the resolved
                    // name and icon now.
                    let infos = game_infos.borrow();
                    for c in master.borrow_mut().iter_mut() {
                        let Some(g) = infos.get(c.game.as_str()) else {
                            continue;
                        };
                        if let Some(name) = &g.name {
                            c.title = name.clone().into();
                            c.initial = initial(name);
                        }
                        if let Some(icon) = &g.icon {
                            c.icon = icon.clone();
                        }
                    }
                    apply_filter(&ui, true);
                }
            },
        );
    }

    // Selection state shared by click and drag: the last clicked card (anchor
    // for Shift+click) and the selection a drag started from.
    let anchor: Rc<Cell<Option<usize>>> = Rc::default();
    let base: Rc<RefCell<Vec<bool>>> = Rc::default();
    {
        let weak = ui.as_weak();
        let anchor = anchor.clone();
        let open_clip = open_clip.clone();
        ui.on_grid_click(move |x, y, w, h, gap, cols, ctrl, shift| {
            let Some(ui) = weak.upgrade() else { return };
            let grid = Grid {
                card_w: w,
                card_h: h,
                gap,
                cols: cols.max(1) as usize,
                count: ui.get_clips().row_count(),
            };
            let Some(i) = grid.hit(x, y) else { return };
            if shift {
                ui.set_select_mode(true);
                let from = anchor.get().unwrap_or(i);
                let (lo, hi) = (from.min(i), from.max(i));
                map_selection(&ui, |n, on| on || (lo..=hi).contains(&n));
            } else if ctrl || ui.get_select_mode() {
                ui.set_select_mode(true);
                map_selection(&ui, |n, on| on != (n == i));
                anchor.set(Some(i));
            } else if let Some(c) = ui.get_clips().row_data(i) {
                (*open_clip)(&ui, Path::new(c.path.as_str()));
            }
            if shift && anchor.get().is_none() {
                anchor.set(Some(i));
            }
        });
    }
    {
        let weak = ui.as_weak();
        let base = base.clone();
        ui.on_band_start(move |extend| {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_select_mode(true);
            *base.borrow_mut() = ui
                .get_clips()
                .iter()
                .map(|c| extend && c.selected)
                .collect();
        });
    }
    {
        let weak = ui.as_weak();
        let base = base.clone();
        ui.on_band_select(move |x0, y0, x1, y1, w, h, gap, cols| {
            let Some(ui) = weak.upgrade() else { return };
            let grid = Grid {
                card_w: w,
                card_h: h,
                gap,
                cols: cols.max(1) as usize,
                count: ui.get_clips().row_count(),
            };
            let hits = grid.band(x0, y0, x1, y1);
            let base = base.borrow();
            map_selection(&ui, |n, _| {
                base.get(n).copied().unwrap_or(false) || hits.contains(&n)
            });
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_toggle_select_mode(move || {
            let Some(ui) = weak.upgrade() else { return };
            let model = ui.get_clips();
            for i in 0..model.row_count() {
                let mut c = model.row_data(i).unwrap();
                if c.selected {
                    c.selected = false;
                    model.set_row_data(i, c);
                }
            }
            ui.set_select_mode(!ui.get_select_mode());
            sync_selection(&ui);
        });
    }
    {
        let weak = ui.as_weak();
        // `toggle`: the footer button flips between all and none; Ctrl+A only selects.
        ui.on_select_all(move |toggle| {
            let Some(ui) = weak.upgrade() else { return };
            let model = ui.get_clips();
            let on = !(toggle && model.iter().all(|c| c.selected));
            ui.set_select_mode(true);
            map_selection(&ui, |_, _| on);
        });
    }
    {
        let weak = ui.as_weak();
        let open_dialog = open_dialog.clone();
        ui.on_ask_delete_selected(move || {
            let Some(ui) = weak.upgrade() else { return };
            let paths: Vec<PathBuf> = ui
                .get_clips()
                .iter()
                .filter(|c| c.selected)
                .map(|c| PathBuf::from(c.path.as_str()))
                .collect();
            (*open_dialog)(&ui, paths);
        });
    }
    {
        let weak = ui.as_weak();
        let trashed = trashed.clone();
        let toast_at = toast_at.clone();
        let toast_timer = toast_timer.clone();
        let refresh = refresh.clone();
        ui.on_undo_trash(move || {
            let Some(ui) = weak.upgrade() else { return };
            if !ui.get_toast_visible() {
                return;
            }
            let (dir, paths) = trashed.borrow().clone();
            if !paths.is_empty() {
                let n = clips::untrash(&dir, &paths).len();
                if n < paths.len() {
                    eprintln!("undo trash: {n} of {} clips restored", paths.len());
                }
                trashed.borrow_mut().1.clear();
            }
            toast_at.set(None);
            toast_timer.stop();
            ui.set_toast_visible(false);
            refresh();
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_dialog_keep(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_dialog_open(false);
            }
        });
    }
    {
        let weak = ui.as_weak();
        let trash_files = trash_files.clone();
        let show_toast = show_toast.clone();
        let dialog_paths = dialog_paths.clone();
        let refresh = refresh.clone();
        let open_clip = open_clip.clone();
        ui.on_dialog_confirm(move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_dialog_open(false);
            let paths = std::mem::take(&mut *dialog_paths.borrow_mut());
            if paths.is_empty() {
                return;
            }
            // For the player dialog: where the clip sat in the list, to pick
            // what plays next (next older, else previous, else back).
            let idx = if ui.get_player_open() {
                ui.get_clips()
                    .iter()
                    .position(|c| Path::new(c.path.as_str()) == paths[0])
            } else {
                None
            };
            let title = ui.get_player_title().to_string();
            let gone = (*trash_files)(&ui, paths);
            if gone.is_empty() {
                return;
            }
            (*show_toast)(
                &ui,
                if ui.get_player_open() {
                    format!("Moved {title} to Trash")
                } else if gone.len() == 1 {
                    "Moved 1 clip to Trash".to_owned()
                } else {
                    format!("Moved {} clips to Trash", gone.len())
                },
                true,
            );
            ui.set_select_mode(false);
            refresh();
            if ui.get_player_open() {
                let model = ui.get_clips();
                let n = model.row_count();
                let next = match idx {
                    Some(i) if i < n => Some(i),
                    Some(i) if i > 0 => Some(i - 1),
                    _ => None,
                };
                match next.and_then(|i| model.row_data(i)) {
                    Some(c) => (*open_clip)(&ui, Path::new(c.path.as_str())),
                    None => ui.invoke_player_back(),
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        let apply_filter = apply_filter.clone();
        ui.on_set_game_filter(move |key| {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_game_filter(key);
            apply_filter(&ui, false);
        });
    }
    {
        let weak = ui.as_weak();
        let apply_filter = apply_filter.clone();
        ui.on_toggle_exported(move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_exported_only(!ui.get_exported_only());
            apply_filter(&ui, false);
        });
    }
    ui.on_change_screen(|| request_async("reselect"));
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
    {
        let weak = ui.as_weak();
        let mic_ids = mic_ids.clone();
        ui.on_set_mic(move |i| {
            if let Some(ui) = weak.upgrade() {
                ui.set_mic(i);
            }
            match i {
                0 => change("mic", |c| c.mic = AudioSource::None),
                1 => change("mic mic_device", |c| {
                    if c.mic == AudioSource::None {
                        c.mic = AudioSource::Auto;
                    }
                    c.mic_device.clear();
                }),
                n => {
                    if let Some(id) = mic_ids.borrow().get(n as usize - 2).cloned() {
                        change("mic mic_device", move |c| {
                            if c.mic == AudioSource::None {
                                c.mic = AudioSource::Auto;
                            }
                            c.mic_device = id;
                        });
                    }
                }
            }
        });
    }
    {
        let load_mics = load_mics.clone();
        ui.on_load_mics(move || load_mics());
    }
    {
        let weak = ui.as_weak();
        ui.on_toggle_app_track(move |i| {
            let Some(ui) = weak.upgrade() else { return };
            let rows = ui.get_app_tracks();
            let Some(mut row) = rows.row_data(i as usize) else {
                return;
            };
            row.on = !row.on;
            let on = row.on;
            rows.set_row_data(i as usize, row);
            match i {
                0 => change("track_game", move |c| c.track_game = on),
                1 => change("track_voice", move |c| c.track_voice = on),
                2 => change("track_other", move |c| c.track_other = on),
                _ => {}
            }
        });
    }
    {
        let weak = ui.as_weak();
        let player = player.clone();
        ui.on_toggle_play(move || {
            let borrow = player.borrow();
            let Some(p) = borrow.as_ref() else { return };
            if p.playing() {
                p.pause();
                return;
            }
            p.play();
            // Start inside the trimmed range when the playhead is outside it;
            // the seek is issued after play so it also wins over the EOS
            // restart play() does on its own.
            if let Some(ui) = weak.upgrade() {
                let (tin, tout, pos) =
                    (ui.get_trim_in(), ui.get_trim_out(), ui.get_pos());
                if tout > tin && !(pos >= tin && pos < tout) {
                    p.seek(gst::ClockTime::from_seconds_f64(tin as f64));
                    ui.set_pos(tin);
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        let player = player.clone();
        ui.on_seek(move |secs| {
            let Some(ui) = weak.upgrade() else { return };
            let secs = secs.max(0.0);
            // Follow the finger immediately so the pipeline's stale position
            // can't flash the old spot after release.
            ui.set_pos(secs);
            if let Some(p) = player.borrow().as_ref() {
                p.seek(gst::ClockTime::from_seconds_f64(secs as f64));
            }
        });
    }
    // `dir`: -1 goes to the newer clip, +1 to the older one. No wrap-around.
    let move_clip = {
        let player_path = player_path.clone();
        let open_clip = open_clip.clone();
        move |ui: &Library, dir: i32| {
            let Some(cur) = player_path.borrow().clone() else { return };
            let model = ui.get_clips();
            let Some(i) = model.iter().position(|c| Path::new(c.path.as_str()) == cur) else {
                return;
            };
            if let Some(c) = model.row_data((i as i32 + dir) as usize) {
                (*open_clip)(ui, Path::new(c.path.as_str()));
            }
        }
    };
    let move_clip = Rc::new(move_clip);
    {
        let weak = ui.as_weak();
        let move_clip = move_clip.clone();
        ui.on_prev_clip(move || {
            if let Some(ui) = weak.upgrade() {
                (*move_clip)(&ui, -1);
            }
        });
    }
    {
        let weak = ui.as_weak();
        let move_clip = move_clip.clone();
        ui.on_next_clip(move || {
            if let Some(ui) = weak.upgrade() {
                (*move_clip)(&ui, 1);
            }
        });
    }
    {
        let weak = ui.as_weak();
        let player = player.clone();
        ui.on_set_volume(move |v| {
            let Some(ui) = weak.upgrade() else { return };
            let v = v.clamp(0.0, 1.0);
            ui.set_volume(v);
            if let Some(p) = player.borrow().as_ref() {
                p.set_volume(v as f64);
            }
        });
    }
    {
        let weak = ui.as_weak();
        let player = player.clone();
        ui.on_toggle_mute(move || {
            let Some(ui) = weak.upgrade() else { return };
            let m = !ui.get_muted();
            ui.set_muted(m);
            if let Some(p) = player.borrow().as_ref() {
                p.set_muted(m);
            }
        });
    }
    {
        let weak = ui.as_weak();
        let player = player.clone();
        ui.on_toggle_track(move |i| {
            let Some(ui) = weak.upgrade() else { return };
            let model = ui.get_tracks();
            let Some(mut t) = model.row_data(i as usize) else { return };
            if !t.audio {
                return;
            }
            t.muted = !t.muted;
            let m = t.muted;
            model.set_row_data(i as usize, t);
            if let Some(p) = player.borrow().as_ref() {
                p.set_track_muted(i as usize - 1, m);
            }
        });
    }
    {
        let weak = ui.as_weak();
        let player = player.clone();
        ui.on_set_track_volume(move |i, v| {
            let Some(ui) = weak.upgrade() else { return };
            let model = ui.get_tracks();
            let Some(mut t) = model.row_data(i as usize) else { return };
            if !t.audio {
                return;
            }
            t.volume = v;
            model.set_row_data(i as usize, t);
            if let Some(p) = player.borrow().as_ref() {
                p.set_track_volume(i as usize - 1, v as f64);
            }
        });
    }
    {
        let weak = ui.as_weak();
        let player_gen = player_gen.clone();
        let trim_keys = trim_keys.clone();
        ui.on_set_trim_in(move |t| {
            let Some(ui) = weak.upgrade() else { return };
            let dur = ui.get_dur() as f64;
            if dur <= 0.0 {
                return;
            }
            let tout = ui.get_trim_out() as f64;
            let keys = {
                let slot = trim_keys.lock().unwrap();
                if slot.0 == player_gen.load(Ordering::SeqCst) {
                    slot.1.clone()
                } else {
                    Vec::new()
                }
            };
            let snapped = crate::trim::snap_start(&keys, (t as f64).clamp(0.0, dur));
            // Keep at least a second selected: land on the latest keyframe
            // that still leaves room, or keep the current trim-in.
            let snapped = if tout - snapped >= 1.0 {
                snapped
            } else {
                match keys.iter().rev().find(|&&k| tout - k >= 1.0) {
                    Some(&k) => k,
                    None => return,
                }
            };
            ui.set_trim_in(snapped as f32);
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_set_trim_out(move |t| {
            let Some(ui) = weak.upgrade() else { return };
            let (dur, tin) = (ui.get_dur(), ui.get_trim_in());
            if dur <= 0.0 || tin + 1.0 > dur {
                return;
            }
            ui.set_trim_out(t.clamp(tin + 1.0, dur));
        });
    }
    {
        let weak = ui.as_weak();
        let player_path = player_path.clone();
        let player_gen = player_gen.clone();
        let trim_keys = trim_keys.clone();
        let export_done = export_done.clone();
        ui.on_export_trim(move || {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_exporting() {
                return;
            }
            let Some(src) = player_path.borrow().clone() else {
                return;
            };
            let (tin, tout) = (ui.get_trim_in() as f64, ui.get_trim_out() as f64);
            if tout <= tin {
                return;
            }
            // Lane 0 is the video; audio lanes map to tracks in order.
            let mix: Vec<(f64, bool)> = ui
                .get_tracks()
                .iter()
                .skip(1)
                .filter(|t| t.audio)
                .map(|t| (t.volume as f64, t.muted))
                .collect();
            let Some(dir) = src.parent().map(|d| d.to_path_buf()) else {
                return;
            };
            let stem = src.file_stem().unwrap_or_default().to_string_lossy();
            let out = clips::unique_path(&dir, &format!("{stem}_trim.mp4"));
            let gen = player_gen.load(Ordering::SeqCst);
            let keys = {
                let slot = trim_keys.lock().unwrap();
                if slot.0 == gen {
                    slot.1.clone()
                } else {
                    Vec::new()
                }
            };
            ui.set_exporting(true);
            ui.set_export_status("Exporting…".into());
            let export_done = export_done.clone();
            std::thread::spawn(move || {
                let result = export_trimmed(&src, keys, tin, tout, &mix, &out);
                let msg = result.err().map(|e| shorten(&format!("{e:#}"), 40));
                *export_done.lock().unwrap() = Some((gen, msg));
            });
        });
    }
    {
        let player_path = player_path.clone();
        ui.on_open_external(move || {
            if let Some(p) = player_path.borrow().as_ref() {
                open_path(p);
            }
        });
    }
    {
        let player_path = player_path.clone();
        ui.on_show_folder(move || {
            if let Some(dir) = player_path.borrow().as_ref().and_then(|p| p.parent()) {
                open_path(dir);
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_toggle_fullscreen(move || {
            if let Some(ui) = weak.upgrade() {
                let f = !ui.get_fullscreen();
                ui.set_fullscreen(f);
                ui.window().set_fullscreen(f);
            }
        });
    }
    {
        let weak = ui.as_weak();
        let player = player.clone();
        let player_path = player_path.clone();
        let player_gen = player_gen.clone();
        let pos_timer = pos_timer.clone();
        ui.on_player_back(move || {
            let Some(ui) = weak.upgrade() else { return };
            player_gen.fetch_add(1, Ordering::SeqCst);
            *player.borrow_mut() = None;
            *player_path.borrow_mut() = None;
            pos_timer.stop();
            if ui.get_fullscreen() {
                ui.set_fullscreen(false);
                ui.window().set_fullscreen(false);
            }
            ui.set_player_open(false);
        });
    }
    {
        let weak = ui.as_weak();
        let player = player.clone();
        let player_path = player_path.clone();
        let open_dialog = open_dialog.clone();
        ui.on_ask_delete_current(move || {
            let Some(ui) = weak.upgrade() else { return };
            let Some(path) = player_path.borrow().clone() else { return };
            // Footage can't keep rolling under a dialog about deleting it.
            if let Some(p) = player.borrow().as_ref() {
                p.pause();
            }
            (*open_dialog)(&ui, vec![path]);
        });
    }
    // Paused or idle: resume the running recorder; stopped: start one.
    ui.on_start_recorder(|| {
        if let Ok(s) = ipc::status() {
            // Nothing to resume without a screen: open the picker instead.
            request_async(if get(&s, "state") == "setup" {
                "reselect"
            } else {
                "resume"
            });
        } else {
            spawn_detached(&["run"]);
        }
    });
    {
        let weak = ui.as_weak();
        ui.on_drag(move |x, y| {
            if let Some(ui) = weak.upgrade() {
                compositor_drag(&ui, x, y, |w| {
                    let _ = w.drag_window();
                });
            }
        });
    }
    {
        use slint::winit_030::winit::window::ResizeDirection as D;
        let weak = ui.as_weak();
        ui.on_resize(move |dir, x, y| {
            let dir = match dir {
                0 => D::North,
                1 => D::South,
                2 => D::West,
                3 => D::East,
                4 => D::NorthWest,
                5 => D::NorthEast,
                6 => D::SouthWest,
                _ => D::SouthEast,
            };
            if let Some(ui) = weak.upgrade() {
                compositor_drag(&ui, x, y, |w| {
                    let _ = w.drag_resize_window(dir);
                });
            }
        });
    }
    ui.set_onboarding(!onboarded());
    {
        let weak = ui.as_weak();
        ui.on_finish_onboarding(move || {
            set_onboarded();
            if let Some(ui) = weak.upgrade() {
                ui.set_onboarding(false);
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_close_window(move || {
            // Closing the window also counts as having read the welcome screen.
            if weak.upgrade().is_some_and(|ui| ui.get_onboarding()) {
                set_onboarded();
            }
            let _ = slint::quit_event_loop();
        });
    }
    if let Some(token) = activation {
        // Launchers like the KDE app menu activate the surface we ask for;
        // it only exists once the window is up, so wait a beat.
        let weak = ui.as_weak();
        Timer::single_shot(Duration::from_millis(100), move || {
            if let Some(ui) = weak.upgrade() {
                ui.window()
                    .with_winit_window(|w| {
                        let _ = platform::activate::activate(w, &token);
                    });
            }
        });
    }
    // Visual-check aid: CLIP_UI_STATE=dialog|toast|popover|select|player|
    // player-dialog opens that state once the thumbnails have had a moment.
    if let Ok(state) = std::env::var("CLIP_UI_STATE") {
        let weak = ui.as_weak();
        let open_dialog = open_dialog.clone();
        let show_toast = show_toast.clone();
        let open_clip = open_clip.clone();
        Timer::single_shot(Duration::from_millis(2000), move || {
            let Some(ui) = weak.upgrade() else { return };
            let model = ui.get_clips();
            let paths: Vec<PathBuf> = model
                .iter()
                .take(if state == "dialog" { 6 } else { 2 })
                .map(|c| PathBuf::from(c.path.as_str()))
                .collect();
            let first = paths.first().cloned();
            match state.as_str() {
                "popover" => ui.set_popover(true),
                "toast" => (*show_toast)(&ui, "Moved 2 clips to Trash".into(), true),
                "select" | "dialog" => {
                    ui.set_select_mode(true);
                    map_selection(&ui, |i, _| i < 2);
                    if state == "dialog" {
                        (*open_dialog)(&ui, paths);
                    }
                }
                "player" | "player-dialog" | "player-toast" => {
                    if let Some(p) = first {
                        (*open_clip)(&ui, &p);
                        if state == "player-toast" {
                            (*show_toast)(&ui, "Moved clip to Trash".into(), true);
                        }
                        if state == "player-dialog" {
                            (*open_dialog)(&ui, vec![p]);
                        }
                    }
                }
                _ => {}
            }
        });
    }
    ui.run()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_names_are_shortened() {
        assert_eq!(shorten("Deadlock", 26), "Deadlock");
        let long = shorten("The Witcher 3: Wild Hunt - Game of the Year Edition", 26);
        assert!(long.chars().count() <= 26);
        assert!(long.ends_with('…'));
    }

    fn grid() -> Grid {
        // 3 columns of 100x60 cards, 10 px apart, 7 cards.
        Grid {
            card_w: 100.0,
            card_h: 60.0,
            gap: 10.0,
            cols: 3,
            count: 7,
        }
    }

    #[test]
    fn hit_finds_cards_and_skips_gaps() {
        let g = grid();
        assert_eq!(g.hit(30.0, 10.0), Some(0));
        assert_eq!(g.hit(24.0 + 110.0 + 5.0, 10.0), Some(1));
        assert_eq!(g.hit(24.0 + 5.0, 70.0 + 5.0), Some(3));
        assert_eq!(g.hit(24.0 + 105.0, 10.0), None, "gap between columns");
        assert_eq!(g.hit(30.0, 65.0), None, "gap between rows");
        assert_eq!(g.hit(10.0, 10.0), None, "left padding");
        assert_eq!(
            g.hit(24.0 + 5.0, 140.0 + 70.0 + 5.0),
            None,
            "past the last row"
        );
        assert_eq!(
            g.hit(24.0 + 110.0 + 5.0, 140.0 + 5.0),
            None,
            "empty slot after card 6"
        );
        assert_eq!(g.hit(24.0 + 5.0, 140.0 + 5.0), Some(6));
    }

    #[test]
    fn band_selects_touched_cards() {
        let g = grid();
        // Covers the right half of card 0 and the left half of card 1, top row only.
        assert_eq!(g.band(80.0, 10.0, 160.0, 20.0), vec![0, 1]);
        // Dragging down through the first column.
        assert_eq!(g.band(30.0, 10.0, 40.0, 150.0), vec![0, 3, 6]);
        // Only the gap: nothing.
        assert!(g.band(125.0, 10.0, 130.0, 20.0).is_empty());
    }
}
