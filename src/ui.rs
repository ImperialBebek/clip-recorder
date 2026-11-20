//! The windows: the clips library window. Each runs as its own
//! short-lived process (`clip-recorder library`) and talks to the
//! recorder over the control socket, so the recorder never loads a GUI.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::{Duration, SystemTime};

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

/// Recounts the selected cards; no selection means nothing left to confirm.
fn sync_selection(ui: &Library) {
    let n = ui.get_clips().iter().filter(|c| c.selected).count();
    ui.set_selected_count(n as i32);
    if n == 0 {
        ui.set_confirming(false);
    }
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
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
    // Abstract names are shared by every user on the machine; scope it to ours.
    let uid = std::fs::metadata("/proc/self").map_or(0, |m| m.uid());
    let name = format!("clip-recorder-library-{uid}");
    let addr = SocketAddr::from_abstract_name(name.as_bytes()).ok()?;
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
                    let selected = old.iter().any(|o| o.path == path && o.selected);
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
                        selected,
                    }
                })
                .collect();
            ui.set_clips(ModelRc::new(VecModel::from(cards)));
            if ui.get_clips().row_count() == 0 {
                ui.set_select_mode(false);
            }
            sync_selection(&ui);
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

    // Selection state shared by click and drag: the last clicked card (anchor
    // for Shift+click) and the selection a drag started from.
    let anchor: Rc<Cell<Option<usize>>> = Rc::default();
    let base: Rc<RefCell<Vec<bool>>> = Rc::default();
    {
        let weak = ui.as_weak();
        let anchor = anchor.clone();
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
                open_path(&c.path);
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
        let output = output.clone();
        let refresh = refresh.clone();
        ui.on_delete_selected(move || {
            let Some(ui) = weak.upgrade() else { return };
            let paths: Vec<PathBuf> = ui
                .get_clips()
                .iter()
                .filter(|c| c.selected)
                .map(|c| PathBuf::from(c.path.as_str()))
                .collect();
            let dir = output.borrow().clone();
            for gone in clips::delete(dir.as_ref(), &paths) {
                let _ = std::fs::remove_file(thumb_path(&gone));
            }
            ui.set_confirming(false);
            ui.set_select_mode(false);
            refresh();
        });
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
