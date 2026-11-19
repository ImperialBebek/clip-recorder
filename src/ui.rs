//! The windows: tray menu, settings, first-run check and the save toast. Each
//! runs as its own short-lived process (`clip-recorder menu`, ...) and talks to
//! the recorder over the control socket, so the recorder never loads a GUI.

use std::collections::HashMap;
use std::io::BufRead;
use std::process::Command;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::Result;
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};

use crate::capture::AudioSource;
use crate::config::{self, Config, Quality};
use crate::encoder::Encoder;
use crate::platform::ipc;
use crate::{clips, sysinfo};

slint::include_modules!();

type Status = HashMap<String, String>;

fn has_font(family: &str) -> bool {
    Command::new("fc-list")
        .arg(":family")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(family))
        .unwrap_or(false)
}

/// The design's faces are PP Valve (display) and PP Mori (body); they are
/// licensed, so the repo bundles open stand-ins and uses the real ones only
/// when they are installed on the system.
fn apply_fonts(theme: Theme) {
    if has_font("PP Valve") {
        theme.set_display_font("PP Valve".into());
    }
    if has_font("PP Mori") {
        theme.set_body_font("PP Mori".into());
    }
}

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

fn mmss(secs: u64) -> String {
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_000_000.0)
}

fn keys(s: &Status) -> ModelRc<SharedString> {
    let hk = get(s, "hotkey");
    let hk = if hk.is_empty() { "Alt+S" } else { hk };
    let v: Vec<SharedString> = hk.split('+').map(SharedString::from).collect();
    ModelRc::new(VecModel::from(v))
}

fn strings(v: &[&str]) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(
        v.iter().map(|s| SharedString::from(*s)).collect::<Vec<_>>(),
    ))
}

/// Buffer readouts shared by the menu and settings: fill, "00:42 / 01:00", RAM.
fn buffer(s: &Status) -> (f32, String, String) {
    let window = num(s, "window").max(1);
    let held = num(s, "buffered_ms") / 1000;
    let fill = (num(s, "buffered_ms") as f32 / (window as f32 * 1000.0)).min(1.0);
    let ram = num(s, "ram");
    let ram = match ram {
        0 => "0 MB RAM".to_owned(),
        1..=999_999 => "<1 MB RAM".to_owned(),
        b => format!("~{} MB RAM", (b + 500_000) / 1_000_000),
    };
    (fill, format!("{} / {}", mmss(held), mmss(window)), ram)
}

fn state_label(s: &Status, long: bool) -> (bool, String) {
    match get(s, "state") {
        "live" if long => (true, format!("Live · {} s buffer", get(s, "window"))),
        "live" => (true, "Live".into()),
        "idle" => (false, "Idle · no game".into()),
        "paused" => (false, "Paused".into()),
        _ => (false, "Stopped".into()),
    }
}

fn encoder_line(s: &Status) -> String {
    [get(s, "encoder"), get(s, "codec"), get(s, "video")]
        .iter()
        .filter(|x| !x.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" · ")
}

fn request_async(cmd: &'static str) {
    std::thread::spawn(move || {
        if let Err(e) = ipc::request(cmd) {
            eprintln!("{cmd}: {e:#}");
        }
    });
}

pub fn menu() -> Result<()> {
    let ui = TrayMenu::new()?;
    apply_fonts(ui.global::<Theme>());

    let refresh = {
        let ui = ui.as_weak();
        move || {
            let Some(ui) = ui.upgrade() else { return };
            let Ok(s) = ipc::status() else {
                ui.set_live(false);
                ui.set_state_label("Stopped".into());
                return;
            };
            let (live, label) = state_label(&s, false);
            let (fill, text, ram) = buffer(&s);
            ui.set_live(live);
            ui.set_paused(!live);
            ui.set_state_label(label.into());
            ui.set_fill(fill);
            ui.set_buffer_text(text.into());
            ui.set_ram_text(ram.into());
            ui.set_encoder_text(encoder_line(&s).into());
            ui.set_seconds(num(&s, "window") as i32);
            ui.set_hotkey(keys(&s));
            ui.set_folder(get(&s, "output").into());
            let dir = get(&s, "output_abs");
            let recent: Vec<RecentClip> = clips::recent(dir.as_ref(), 3)
                .into_iter()
                .map(|c| RecentClip {
                    title: c.title.into(),
                    length: c
                        .seconds
                        .map(|x| format!("{x:.0} s"))
                        .unwrap_or_default()
                        .into(),
                    path: c.path.display().to_string().into(),
                })
                .collect();
            let model = ui.get_recent();
            if model.row_count() != recent.len()
                || model.iter().zip(&recent).any(|(a, b)| a.path != b.path)
            {
                ui.set_recent(ModelRc::new(VecModel::from(recent)));
            }
        }
    };
    refresh();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(250), refresh);

    let quit = || {
        let _ = slint::quit_event_loop();
    };
    ui.on_save(move || {
        let _ = ipc::send("save");
        quit();
    });
    ui.on_toggle_pause(|| request_async("toggle-pause"));
    ui.on_open_clip(move |p| {
        open_path(&p);
        quit();
    });
    ui.on_open_folder(move || {
        let _ = ipc::send("open-folder");
        quit();
    });
    ui.on_settings(move || {
        spawn_detached(&["settings"]);
        quit();
    });
    ui.on_quit(move || {
        let _ = ipc::request("quit");
        quit();
    });
    ui.on_dismiss(quit);
    ui.run()?;
    Ok(())
}

const LENGTHS: [u32; 7] = [15, 30, 60, 90, 120, 180, 300];
const FPS: [u32; 5] = [30, 60, 90, 120, 144];
const ENCODERS: [Option<Encoder>; 5] = [
    None,
    Some(Encoder::Va),
    Some(Encoder::Vaapi),
    Some(Encoder::Nvenc),
    Some(Encoder::X264),
];
const AUDIO: [AudioSource; 4] = [
    AudioSource::Auto,
    AudioSource::PipeWire,
    AudioSource::Pulse,
    AudioSource::None,
];
const QUALITY: [Quality; 3] = [Quality::Light, Quality::High, Quality::Max];

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

fn choose_folder() -> Option<std::path::PathBuf> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    rt.block_on(async {
        let files = ashpd::desktop::file_chooser::OpenFileRequest::default()
            .title("Save clips to")
            .directory(true)
            .send()
            .await
            .ok()?
            .response()
            .ok()?;
        file_uri_path(files.uris().first()?.as_str())
    })
}

/// `file:///home/me/My%20Clips` -> `/home/me/My Clips`.
fn file_uri_path(uri: &str) -> Option<std::path::PathBuf> {
    let raw = uri.strip_prefix("file://")?.as_bytes();
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' && i + 2 < raw.len() {
            if let Ok(b) = u8::from_str_radix(std::str::from_utf8(&raw[i + 1..i + 3]).ok()?, 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(raw[i]);
        i += 1;
    }
    use std::os::unix::ffi::OsStringExt;
    Some(std::ffi::OsString::from_vec(out).into())
}

pub fn settings() -> Result<()> {
    let ui = Settings::new()?;
    apply_fonts(ui.global::<Theme>());
    let gpu = sysinfo::gpu_name();
    ui.set_version_text(
        format!(
            "v{} · {}",
            env!("CARGO_PKG_VERSION"),
            sysinfo::session_type()
        )
        .into(),
    );
    ui.set_pipewire_text(
        sysinfo::pipewire_version()
            .map(|v| format!("PipeWire {v}"))
            .unwrap_or_else(|| "PipeWire not found".into())
            .into(),
    );
    ui.set_length_options(strings(&[
        "15 s", "30 s", "60 s", "90 s", "120 s", "180 s", "300 s",
    ]));
    ui.set_fps_options(strings(&[
        "30 fps", "60 fps", "90 fps", "120 fps", "144 fps",
    ]));
    ui.set_audio_options(strings(&["Auto", "PipeWire", "PulseAudio", "Off"]));

    let refresh = {
        let ui = ui.as_weak();
        let gpu = gpu.clone();
        move || {
            let Some(ui) = ui.upgrade() else { return };
            let Ok(s) = ipc::status() else {
                ui.set_connected(false);
                ui.set_live(false);
                ui.set_state_label("Stopped".into());
                return;
            };
            ui.set_connected(true);
            let (live, label) = state_label(&s, true);
            let (fill, text, ram) = buffer(&s);
            ui.set_live(live);
            ui.set_paused(!live);
            ui.set_state_label(label.into());
            ui.set_fill(fill);
            ui.set_buffer_text(text.into());
            ui.set_ram_text(ram.into());
            ui.set_encoder_line(encoder_line(&s).into());
            ui.set_hotkey(keys(&s));
            ui.set_hotkey_text(get(&s, "hotkey").into());
            ui.set_hotkey_backend(
                match get(&s, "hotkey_backend") {
                    "portal" => "Registered through the GlobalShortcuts portal. Your desktop lists it in its shortcut settings.",
                    "evdev" => "Read from /dev/input; works on every compositor while you are in the input group.",
                    _ => "No global hotkey right now. Bind `clip-recorder save` in your desktop's shortcut settings.",
                }
                .into(),
            );
            ui.set_gpu_text(
                gpu.as_ref()
                    .map(|g| format!("Detected: {g}"))
                    .unwrap_or_default()
                    .into(),
            );
            ui.set_warning(get(&s, "warning").into());

            let vendor = gpu
                .as_deref()
                .and_then(|g| g.split_whitespace().next())
                .unwrap_or("");
            let active = get(&s, "encoder");
            let auto = match (get(&s, "encoder_setting"), vendor) {
                ("auto", "") => format!("Auto ({active})"),
                ("auto", v) => format!("Auto ({active} · {v})"),
                _ => "Auto".into(),
            };
            ui.set_encoder_options(strings(&[
                &auto,
                "VA-API (va)",
                "VA-API (vaapi)",
                "NVENC",
                "Software (x264)",
            ]));
            let enc = Encoder::parse(get(&s, "encoder_setting")).ok().flatten();
            ui.set_encoder_index(ENCODERS.iter().position(|e| *e == enc).unwrap_or(0) as i32);

            let q = QUALITY
                .iter()
                .position(|q| q.name() == get(&s, "quality"))
                .unwrap_or(1);
            ui.set_quality_index(q as i32);
            ui.set_bitrate_text(format!("{} Mb/s", num(&s, "kbps") / 1000).into());
            ui.set_output_text(get(&s, "output").into());
            let game = get(&s, "game");
            let slug = if game.is_empty() {
                "Game".to_owned()
            } else {
                game.replace(' ', "_")
            };
            ui.set_name_example(format!("{slug}_2026-11-19_145230.mp4").into());

            let window = num(&s, "window") as u32;
            let idx = LENGTHS.iter().position(|l| *l == window);
            if idx.is_none() {
                let mut opts: Vec<String> = LENGTHS.iter().map(|l| format!("{l} s")).collect();
                opts.push(format!("{window} s"));
                ui.set_length_options(ModelRc::new(VecModel::from(
                    opts.into_iter().map(SharedString::from).collect::<Vec<_>>(),
                )));
            }
            ui.set_length_index(idx.unwrap_or(LENGTHS.len()) as i32);
            let fps = num(&s, "fps") as u32;
            ui.set_fps_index(FPS.iter().position(|f| *f == fps).map_or(-1, |i| i as i32));
            let audio = AudioSource::parse(get(&s, "audio_setting")).unwrap_or(AudioSource::Auto);
            ui.set_audio_index(AUDIO.iter().position(|a| *a == audio).unwrap_or(0) as i32);
            ui.set_audio_active(
                match get(&s, "audio") {
                    "pipewire" => "PipeWire desktop audio",
                    "pulse" => "PulseAudio monitor",
                    "test" => "test tone",
                    _ => "video only",
                }
                .into(),
            );
            ui.set_pause_without_game(get(&s, "pause_without_game") == "true");
            ui.set_toast(get(&s, "toast") == "true");
            ui.set_sound(get(&s, "sound") == "true");
            let (bytes, n) = clips::usage(get(&s, "output_abs").as_ref());
            ui.set_storage_text(
                format!("{n} clips, {} in {}", mb(bytes), get(&s, "output")).into(),
            );
        }
    };
    refresh();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(250), refresh);

    ui.on_save_now(|| request_async("save"));
    ui.on_toggle_pause(|| request_async("toggle-pause"));
    ui.on_open_folder(|| request_async("open-folder"));
    ui.on_reselect(|| request_async("reselect"));
    ui.on_start_recorder(|| spawn_detached(&["run"]));
    ui.on_set_length(|i| {
        change("duration", move |c| {
            c.duration = LENGTHS.get(i as usize).copied().unwrap_or(c.duration)
        })
    });
    ui.on_set_fps(|i| {
        change("fps", move |c| {
            c.fps = FPS.get(i as usize).copied().unwrap_or(c.fps)
        })
    });
    ui.on_set_encoder(|i| {
        change("encoder", move |c| {
            c.encoder = ENCODERS.get(i as usize).copied().unwrap_or(c.encoder)
        })
    });
    ui.on_set_audio(|i| {
        change("audio", move |c| {
            c.audio = AUDIO.get(i as usize).copied().unwrap_or(c.audio)
        })
    });
    ui.on_set_quality(|i| {
        change("quality", move |c| {
            c.quality = QUALITY.get(i as usize).copied().unwrap_or(c.quality)
        })
    });
    ui.on_set_pause_without_game(|v| {
        change("pause_without_game", move |c| c.pause_without_game = v)
    });
    ui.on_set_toast(|v| change("toast", move |c| c.toast = v));
    ui.on_set_sound(|v| change("sound", move |c| c.sound = v));
    ui.on_choose_folder(|| {
        std::thread::spawn(|| {
            if let Some(dir) = choose_folder() {
                change("output", move |c| c.output = dir);
            }
        });
    });
    ui.run()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn file_uris() {
        assert_eq!(
            super::file_uri_path("file:///home/me/My%20Clips").unwrap(),
            std::path::PathBuf::from("/home/me/My Clips")
        );
    }
}

fn check(ok: bool, title: &str, mono: &str, detail: &str, keys: &[&str]) -> Check {
    Check {
        ok,
        title: title.into(),
        mono: mono.into(),
        detail: detail.into(),
        keys: strings(keys),
    }
}

async fn has_global_shortcuts() -> bool {
    ashpd::desktop::global_shortcuts::GlobalShortcuts::new()
        .await
        .is_ok()
}

fn system_checks() -> Vec<Check> {
    let _ = gstreamer::init();
    let cfg = Config::load();
    let gpu = sysinfo::gpu_name();
    let on_gpu = gpu.map(|g| format!(" on {g}")).unwrap_or_default();
    let hw = Encoder::AUTO_ORDER
        .into_iter()
        .filter(|e| e.is_hardware() && e.available())
        .map(|e| e.label())
        .next();
    let encoder = match hw {
        Some(label) => check(
            true,
            "Hardware encoder",
            &format!("{label} · H.264"),
            &on_gpu,
            &[],
        ),
        None => check(
            false,
            "Hardware encoder",
            "x264",
            "· no VA-API or NVENC plugin found; clips are encoded on the CPU",
            &[],
        ),
    };
    let pipewire = match sysinfo::pipewire_version() {
        Some(v) => check(
            true,
            "PipeWire",
            &v,
            "· desktop audio is captured from the default output",
            &[],
        ),
        None => check(
            false,
            "PipeWire",
            "",
            "Not found. Screen capture needs PipeWire and xdg-desktop-portal.",
            &[],
        ),
    };
    let capture = check(
        false,
        "Screen capture permission",
        "",
        &format!(
            "{} asks once through the desktop portal. Pick the monitor your games run on.",
            sysinfo::session_type()
        ),
        &[],
    );
    let shortcuts = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map(|rt| rt.block_on(has_global_shortcuts()))
        .unwrap_or(false);
    let hk: Vec<String> = crate::daemon::pretty_combo(&cfg.hotkey)
        .split('+')
        .map(str::to_owned)
        .collect();
    let hk: Vec<&str> = hk.iter().map(String::as_str).collect();
    let hotkey = if shortcuts {
        check(
            true,
            "Global hotkey",
            "",
            "Registered through the GlobalShortcuts portal",
            &hk,
        )
    } else {
        check(
            false,
            "Global hotkey",
            "",
            "No GlobalShortcuts portal; uses /dev/input, or bind `clip-recorder save`",
            &hk,
        )
    };
    let _ = config::tilde(&cfg.output);
    vec![encoder, pipewire, capture, hotkey]
}

/// Returns true when the user chose "Allow screen capture".
pub fn first_run() -> Result<bool> {
    let ui = FirstRun::new()?;
    apply_fonts(ui.global::<Theme>());
    ui.set_checks(ModelRc::new(VecModel::from(system_checks())));
    let allowed = Rc::new(std::cell::Cell::new(false));
    let a = allowed.clone();
    ui.on_allow(move || {
        a.set(true);
        let _ = slint::quit_event_loop();
    });
    ui.on_later(|| {
        let _ = slint::quit_event_loop();
    });
    ui.run()?;
    Ok(allowed.get())
}

enum OsdEvent {
    Saving(String),
    Saved(String),
    Failed(String),
    Closed,
}

fn parse_osd(line: &str) -> Option<OsdEvent> {
    let (kind, rest) = line.split_once(' ').unwrap_or((line, ""));
    let f: Vec<&str> = rest.split('\t').collect();
    Some(match kind {
        "saving" => OsdEvent::Saving(format!("Muxing last {} s", f[0])),
        "saved" => {
            let bytes: u64 = f.get(2).and_then(|b| b.parse().ok()).unwrap_or(0);
            let parts: Vec<String> = [
                format!("Last {} s", f[0]),
                f.get(1).unwrap_or(&"").to_string(),
                mb(bytes),
            ]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect();
            OsdEvent::Saved(parts.join(" · "))
        }
        "failed" => OsdEvent::Failed(format!("{} · {}", f[0], f.get(1).unwrap_or(&""))),
        _ => return None,
    })
}

/// The toast. Reads state lines from stdin (see `daemon::Osd`); hides 2.4 s
/// after "saved", stays after "failed" until clicked.
pub fn osd() -> Result<()> {
    let ui = Osd::new()?;
    apply_fonts(ui.global::<Theme>());
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
    ui.show()?;
    slint::run_event_loop_until_quit()?;
    Ok(())
}

fn apply_osd(ui: slint::Weak<Osd>, ev: OsdEvent) {
    let Some(ui) = ui.upgrade() else { return };
    match ev {
        OsdEvent::Saving(d) => {
            ui.set_state(0);
            ui.set_heading("Saving clip…".into());
            ui.set_detail(d.into());
        }
        OsdEvent::Saved(d) => {
            ui.set_state(1);
            ui.set_heading("Clip saved".into());
            ui.set_detail(d.into());
            ui.set_progress(1.0);
        }
        OsdEvent::Failed(d) => {
            ui.set_state(2);
            ui.set_heading("Couldn't save clip".into());
            ui.set_detail(d.into());
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
