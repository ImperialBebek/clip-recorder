//! Focusing an already-mapped window: winit only consumes an XDG
//! activation token at window-creation time, so a second `clip-recorder
//! library` (or a tray double-click) hands its token to the open window
//! over the library socket instead, and the window activates itself here.

use anyhow::{bail, Context, Result};
use slint::winit_030::winit::raw_window_handle::{
    HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
};
use slint::winit_030::winit::window::Window;
use smithay_client_toolkit::reexports::client::globals::{
    registry_queue_init, GlobalListContents,
};
use smithay_client_toolkit::reexports::client::protocol::{wl_registry, wl_surface};
use smithay_client_toolkit::reexports::client::{Connection, Dispatch, Proxy, QueueHandle};
use smithay_client_toolkit::reexports::protocols::xdg::activation::v1::client::xdg_activation_v1;
use wayland_backend::client::{Backend, ObjectId};

/// Asks the compositor to focus `window` using an XDG activation token.
pub fn activate(window: &Window, token: &str) -> Result<()> {
    let RawDisplayHandle::Wayland(display) = window.display_handle()?.as_raw() else {
        bail!("not a Wayland window");
    };
    let RawWindowHandle::Wayland(surface) = window.window_handle()?.as_raw() else {
        bail!("not a Wayland window");
    };
    // SAFETY: `display` is the live wl_display winit gave us; the backend
    // only borrows it and never disconnects it.
    let backend = unsafe { Backend::from_foreign_display(display.display.as_ptr().cast()) };
    let conn = Connection::from_backend(backend);
    // SAFETY: `surface` is `window`'s live wl_surface; the proxy wraps the
    // foreign object and dropping it cannot send a destroy.
    let surface_id = unsafe {
        ObjectId::from_ptr(
            wl_surface::WlSurface::interface(),
            surface.surface.as_ptr().cast(),
        )
    }
    .context("invalid wl_surface")?;
    let surface = wl_surface::WlSurface::from_id(&conn, surface_id)?;
    let (globals, _queue) = registry_queue_init::<State>(&conn)?;
    let activation: xdg_activation_v1::XdgActivationV1 =
        globals.bind(&_queue.handle(), 1..=1, ()).context("no xdg_activation_v1")?;
    activation.activate(token.to_owned(), &surface);
    activation.destroy();
    conn.flush()?;
    Ok(())
}

struct State;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_activation_v1::XdgActivationV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &xdg_activation_v1::XdgActivationV1,
        _: xdg_activation_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
