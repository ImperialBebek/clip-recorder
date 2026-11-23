//! The on-screen toast (`clip-recorder osd`). A child process fed one state
//! line at a time on stdin (see `daemon::Osd`).
//!
//! It is drawn as a wlr-layer-shell overlay: the compositor places it in the
//! top-right corner, keeps it above fullscreen games, and never gives it
//! keyboard focus or pointer input, so the game keeps focus. Slint renders
//! into a shared-memory buffer with its software renderer. Where the
//! compositor has no layer-shell (GNOME), the same events become desktop
//! notifications.

use std::cell::Cell;
use std::os::fd::{AsRawFd, RawFd};
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType, TargetPixel,
};
use slint::platform::{Platform, WindowAdapter};
use slint::{ComponentHandle, PhysicalSize, PlatformError, Timer, TimerMode};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::{wl_output, wl_shm, wl_surface};
use smithay_client_toolkit::reexports::client::{Connection, EventQueue, QueueHandle};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    registry_handlers,
};

use crate::ui::Osd;

const WIDTH: u32 = 320;
const HEIGHT: u32 = 60;
const MARGIN: i32 = 20;

#[derive(Debug)]
enum Event {
    /// Seconds saved, video label, bytes.
    Saved(String, String, u64),
    /// Short reason ("Disk full").
    Failed(String),
    /// A game started: its name and the save hotkey.
    Game(String, String),
    /// A heads-up with a heading and detail; the flag marks a problem.
    Notice(bool, String, String),
}

fn parse(line: &str) -> Option<Event> {
    let (kind, rest) = line.split_once(' ').unwrap_or((line, ""));
    let f: Vec<&str> = rest.split('\t').collect();
    Some(match kind {
        "saved" => Event::Saved(
            f[0].to_owned(),
            f.get(1).unwrap_or(&"").to_string(),
            f.get(2).and_then(|b| b.parse().ok()).unwrap_or(0),
        ),
        "failed" => Event::Failed(f[0].to_owned()),
        "game" => Event::Game(f[0].to_owned(), f.get(1).unwrap_or(&"").to_string()),
        "info" | "warn" => Event::Notice(
            kind == "warn",
            f[0].to_owned(),
            f.get(1).unwrap_or(&"").to_string(),
        ),
        _ => return None, // "saving": nothing shows while saving
    })
}

fn megabytes(bytes: u64) -> String {
    format!("{:.0} MB", bytes as f64 / 1e6)
}

/// The text of a toast: (state, heading, detail, seconds on screen).
fn describe(ev: &Event) -> (i32, String, String, f32) {
    match ev {
        Event::Saved(secs, video, bytes) => {
            let detail = match (video.is_empty(), *bytes) {
                (true, 0) => String::new(),
                (true, b) => megabytes(b),
                (false, 0) => video.clone(),
                (false, b) => format!("{video} · {}", megabytes(b)),
            };
            (1, format!("Saved {secs} s"), detail, 2.4)
        }
        Event::Failed(reason) => (2, "Couldn't save".into(), reason.clone(), 6.0),
        Event::Notice(warn, heading, detail) => (
            if *warn { 2 } else { 3 },
            heading.clone(),
            detail.clone(),
            6.0,
        ),
        Event::Game(name, hotkey) => (
            3,
            name.clone(),
            format!("Press {hotkey} to save a clip"),
            5.0,
        ),
    }
}

/// Reads whole lines from stdin without blocking the loop that also serves
/// Wayland.
struct Lines {
    buf: Vec<u8>,
    open: bool,
}

impl Lines {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            open: true,
        }
    }

    /// Call when stdin is readable; returns the complete lines read.
    fn read(&mut self) -> Vec<String> {
        let mut chunk = [0u8; 1024];
        // SAFETY: reading into a local buffer from fd 0.
        let n = unsafe { libc::read(0, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n <= 0 {
            self.open = false;
        } else {
            self.buf.extend_from_slice(&chunk[..n as usize]);
        }
        let mut out = Vec::new();
        while let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=i).collect();
            out.push(String::from_utf8_lossy(&line).trim_end().to_owned());
        }
        out
    }
}

/// Waits until one of the fds is readable or the timeout passes; returns which.
fn wait(fds: &[RawFd], timeout: Duration) -> Vec<bool> {
    let mut p: Vec<libc::pollfd> = fds
        .iter()
        .map(|&fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: `p` is a valid array of pollfd for the given length.
    unsafe { libc::poll(p.as_mut_ptr(), p.len() as libc::nfds_t, ms) };
    p.iter()
        .map(|p| p.revents & (libc::POLLIN | libc::POLLHUP) != 0)
        .collect()
}

pub fn run() -> Result<()> {
    match Overlay::connect() {
        Ok(o) => o.run(),
        Err(e) => {
            eprintln!("toast: no layer-shell overlay ({e:#}), using notifications");
            notifications()
        }
    }
}

/// Fallback: the same events as desktop notifications.
fn notifications() -> Result<()> {
    let mut lines = Lines::new();
    while lines.open {
        wait(&[0], Duration::from_secs(60));
        for line in lines.read() {
            if let Some(ev) = parse(&line) {
                let (_, heading, detail, _) = describe(&ev);
                crate::daemon::notify(&heading, &detail);
            }
        }
    }
    Ok(())
}

// --- Slint on a Wayland buffer ---------------------------------------------

/// One pixel of a wl_shm `Argb8888` buffer: premultiplied, 0xAARRGGBB.
#[derive(Clone, Copy)]
#[repr(transparent)]
struct Argb(u32);

impl TargetPixel for Argb {
    fn blend(&mut self, c: PremultipliedRgbaColor) {
        let inv = 255 - c.alpha as u32;
        let [b, g, r, a] = self.0.to_le_bytes().map(u32::from);
        let mix = |old: u32, new: u8| (old * inv / 255 + new as u32).min(255);
        let out = [
            mix(b, c.blue) as u8,
            mix(g, c.green) as u8,
            mix(r, c.red) as u8,
            mix(a, c.alpha) as u8,
        ];
        self.0 = u32::from_le_bytes(out);
    }

    fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        Argb(0xff00_0000 | (r as u32) << 16 | (g as u32) << 8 | b as u32)
    }

    fn background() -> Self {
        Argb(0)
    }
}

struct ToastPlatform(Rc<MinimalSoftwareWindow>);

impl Platform for ToastPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.0.clone())
    }
}

// --- Wayland ---------------------------------------------------------------

struct Overlay {
    queue: EventQueue<Wl>,
    wl: Wl,
}

struct Wl {
    registry: RegistryState,
    output: OutputState,
    compositor: CompositorState,
    shm: Shm,
    layer_shell: LayerShell,
    pool: SlotPool,
    layer: Option<LayerSurface>,
    configured: bool,
    closed: bool,
}

impl Overlay {
    fn connect() -> Result<Self> {
        let conn = Connection::connect_to_env()?;
        let (globals, queue) = registry_queue_init::<Wl>(&conn)?;
        let qh = queue.handle();
        let compositor = CompositorState::bind(&globals, &qh).map_err(|e| anyhow!("{e}"))?;
        let layer_shell = LayerShell::bind(&globals, &qh).map_err(|e| anyhow!("{e}"))?;
        let shm = Shm::bind(&globals, &qh).map_err(|e| anyhow!("{e}"))?;
        let pool = SlotPool::new((WIDTH * HEIGHT * 4 * 2) as usize, &shm)?;
        let wl = Wl {
            registry: RegistryState::new(&globals),
            output: OutputState::new(&globals, &qh),
            compositor,
            shm,
            layer_shell,
            pool,
            layer: None,
            configured: false,
            closed: false,
        };
        Ok(Self { queue, wl })
    }

    fn run(mut self) -> Result<()> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        window.set_size(PhysicalSize::new(WIDTH, HEIGHT));
        slint::platform::set_platform(Box::new(ToastPlatform(window.clone())))
            .map_err(|e| anyhow!("{e}"))?;
        let ui = Osd::new()?;
        ui.show()?;

        // Progress bar: drains while the toast is up.
        let shown = Rc::new(Cell::new(None::<(Instant, f32)>));
        let tick = Timer::default();
        {
            let (ui, shown) = (ui.as_weak(), shown.clone());
            tick.start(TimerMode::Repeated, Duration::from_millis(16), move || {
                let (Some(ui), Some((at, secs))) = (ui.upgrade(), shown.get()) else {
                    return;
                };
                ui.set_progress((1.0 - at.elapsed().as_secs_f32() / secs).max(0.0));
            });
        }

        let mut lines = Lines::new();
        let mut deadline: Option<Instant> = None;
        // Runs until stdin closes (the recorder is done with us) and the last
        // toast has been shown.
        loop {
            slint::platform::update_timers_and_animations();
            if self.wl.closed
                || deadline.is_some_and(|d| Instant::now() >= d)
                || (!lines.open && deadline.is_none())
            {
                break;
            }

            self.queue.dispatch_pending(&mut self.wl)?;
            self.queue.flush()?;
            if deadline.is_some() && self.wl.configured {
                self.draw(&window)?;
            }

            let Some(guard) = self.queue.prepare_read() else {
                continue; // events arrived meanwhile: dispatch them first
            };
            let mut fds = vec![guard.connection_fd().as_raw_fd()];
            if lines.open {
                fds.push(0);
            }
            let timeout = if deadline.is_some() {
                Duration::from_millis(16)
            } else {
                Duration::from_secs(60)
            };
            let ready = wait(&fds, timeout);
            if ready[0] {
                let _ = guard.read();
            } else {
                drop(guard);
            }
            if lines.open && ready.get(1).copied().unwrap_or(false) {
                for line in lines.read() {
                    let Some(ev) = parse(&line) else { continue };
                    let (state, heading, detail, secs) = describe(&ev);
                    // Store hits are just file reads; a slug the store
                    // doesn't know shows the fallback dot rather than
                    // paying for a live scan on the way to a 5 s toast.
                    let icon = match &ev {
                        Event::Game(name, _) => {
                            crate::games::cached_icon(&crate::clips::slug(name))
                                .and_then(|p| slint::Image::load_from_path(&p).ok())
                                .unwrap_or_default()
                        }
                        _ => slint::Image::default(),
                    };
                    ui.set_game_icon(icon);
                    ui.set_state(state);
                    ui.set_heading(heading.into());
                    ui.set_detail(detail.into());
                    ui.set_progress(1.0);
                    shown.set(Some((Instant::now(), secs)));
                    deadline = Some(Instant::now() + Duration::from_secs_f32(secs));
                    self.wl.show(&self.queue.handle());
                }
            }
        }
        Ok(())
    }

    fn draw(&mut self, window: &MinimalSoftwareWindow) -> Result<()> {
        let Some(layer) = self.wl.layer.clone() else {
            return Ok(());
        };
        let stride = WIDTH as i32 * 4;
        let (buffer, canvas) = self.wl.pool.create_buffer(
            WIDTH as i32,
            HEIGHT as i32,
            stride,
            wl_shm::Format::Argb8888,
        )?;
        canvas.fill(0);
        // SAFETY: Argb is a repr(transparent) u32 and the canvas is a
        // page-aligned mapping whose length is a multiple of 4.
        let (head, pixels, _) = unsafe { canvas.align_to_mut::<Argb>() };
        debug_assert!(head.is_empty());
        let drawn = window.draw_if_needed(|renderer| {
            renderer.render(pixels, WIDTH as usize);
        });
        if !drawn {
            return Ok(());
        }
        layer
            .wl_surface()
            .damage_buffer(0, 0, WIDTH as i32, HEIGHT as i32);
        buffer.attach_to(layer.wl_surface())?;
        layer.commit();
        Ok(())
    }
}

impl Wl {
    /// Creates the overlay surface on first use.
    fn show(&mut self, qh: &QueueHandle<Wl>) {
        if self.layer.is_some() {
            return;
        }
        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some("clip-recorder"),
            None,
        );
        layer.set_anchor(Anchor::TOP | Anchor::RIGHT);
        layer.set_margin(MARGIN, MARGIN, 0, 0);
        layer.set_size(WIDTH, HEIGHT);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        // An empty input region: clicks go to the game underneath.
        if let Ok(region) = smithay_client_toolkit::compositor::Region::new(&self.compositor) {
            layer
                .wl_surface()
                .set_input_region(Some(region.wl_region()));
        }
        layer.commit();
        self.layer = Some(layer);
    }
}

impl CompositorHandler for Wl {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for Wl {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for Wl {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.closed = true;
    }
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        _: LayerSurfaceConfigure,
        _: u32,
    ) {
        self.configured = true;
    }
}

impl ShmHandler for Wl {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for Wl {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
}

delegate_compositor!(Wl);
delegate_output!(Wl);
delegate_shm!(Wl);
delegate_layer!(Wl);
delegate_registry!(Wl);
