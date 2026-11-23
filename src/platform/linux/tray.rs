//! StatusNotifierItem tray icon (KDE, GNOME with the AppIndicator extension,
//! waybar, ...). Double-click opens or focuses the library; the menu has
//! Save clip, Open library and Quit.

use ksni::menu::StandardItem;
use ksni::{MenuItem, TrayMethods};
use tokio::sync::mpsc;

use super::ipc::{self, Request};

pub struct Tray {
    pub tx: mpsc::UnboundedSender<Request>,
    pub paused: bool,
    pub seconds: u32,
    /// "Alt+S"
    pub hotkey: String,
    /// A newer release, when one is out.
    pub update: Option<String>,
    /// No screen chosen yet: nothing is being recorded.
    pub setup: bool,
    /// Following the game's window and no game is running.
    pub waiting: bool,
    /// The screen can be chosen (portal capture, not the game's window).
    pub pick: bool,
    /// SNI has no double-click event; pair the clicks ourselves.
    pub last_click: Option<std::time::Instant>,
}

fn send(t: &Tray, cmd: &str) {
    let _ = t.tx.send(Request {
        cmd: cmd.into(),
        reply: None,
    });
}

/// Starts another instance of this binary for a window (`library`),
/// so the recorder itself never links a GUI into its address space. The
/// child gets `XDG_ACTIVATION_TOKEN` when KDE just gave us one.
pub fn open_window(which: &str, activation_token: Option<&str>) {
    if let Ok(exe) = std::env::current_exe() {
        let mut cmd = std::process::Command::new(exe);
        cmd.arg(which);
        if let Some(token) = activation_token {
            cmd.env("XDG_ACTIVATION_TOKEN", token);
        }
        if let Ok(mut child) = cmd.spawn() {
            std::thread::spawn(move || child.wait());
        }
    }
}

/// Two `activate` calls this close together are one double-click.
fn is_double(prev: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    prev.is_some_and(|t| now.duration_since(t) < std::time::Duration::from_millis(400))
}

/// KDE hands us an activation token per click; it can land a hair after the
/// `activate` call itself, so give it a few retries.
fn activation_token() -> Option<String> {
    for _ in 0..10 {
        if let Some(t) = ksni::current_xdg_activation_token() {
            return Some(t);
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    None
}

/// Focus the open library if there is one, else open it.
fn open_or_focus(token: Option<String>) {
    let line = token
        .as_deref()
        .map_or("focus".to_owned(), |t| format!("focus {t}"));
    if !ipc::tell_library(&line) {
        open_window("library", token.as_deref());
    }
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "clip-recorder".into()
    }
    fn title(&self) -> String {
        "Clip Recorder".into()
    }
    fn icon_name(&self) -> String {
        if self.setup {
            "dialog-warning".into()
        } else if self.paused {
            "media-playback-pause".into()
        } else {
            "media-record".into()
        }
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "Clip Recorder".into(),
            description: if self.setup {
                "Needs setup: choose a screen".into()
            } else if self.waiting {
                "Waiting for a game".into()
            } else if self.paused {
                "Buffer paused".into()
            } else {
                format!("Live · {} s buffer", self.seconds)
            },
            ..Default::default()
        }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        let now = std::time::Instant::now();
        self.last_click = if is_double(self.last_click, now) {
            open_or_focus(activation_token());
            None
        } else {
            Some(now)
        };
    }
    fn menu(&self) -> Vec<MenuItem<Self>> {
        let screen = |label: &str| -> MenuItem<Self> {
            StandardItem {
                label: label.into(),
                icon_name: "video-display".into(),
                activate: Box::new(|t: &mut Self| send(t, "reselect")),
                ..Default::default()
            }
            .into()
        };
        let mut items = Vec::new();
        if self.setup {
            items.push(screen("Choose screen…"));
        }
        items.extend([
            StandardItem {
                label: "Save clip".into(),
                icon_name: "media-record".into(),
                enabled: !self.paused && !self.setup && !self.waiting,
                shortcut: vec![self.hotkey.split('+').map(str::to_owned).collect()],
                activate: Box::new(|t: &mut Self| send(t, "save")),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Open library".into(),
                icon_name: "folder-videos".into(),
                activate: Box::new(|_: &mut Self| open_or_focus(activation_token())),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
        ]);
        if self.pick && !self.setup {
            items.insert(2, screen("Change screen…"));
        }
        if let Some(v) = &self.update {
            items.push(
                StandardItem {
                    label: format!("Update to v{v}"),
                    icon_name: "system-software-update".into(),
                    activate: Box::new(|_: &mut Self| {
                        if let Ok(exe) = std::env::current_exe() {
                            if let Ok(mut c) = std::process::Command::new(exe)
                                .args(["update", "--notify"])
                                .spawn()
                            {
                                std::thread::spawn(move || c.wait());
                            }
                        }
                    }),
                    ..Default::default()
                }
                .into(),
            );
        }
        items.push(
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|t: &mut Self| send(t, "quit")),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

pub async fn spawn(tray: Tray) -> Option<ksni::Handle<Tray>> {
    match tray.spawn().await {
        Ok(h) => Some(h),
        Err(e) => {
            eprintln!("tray: {e} (no StatusNotifier host; use `clip-recorder menu` instead)");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_click_timing() {
        let t0 = std::time::Instant::now();
        assert!(is_double(
            Some(t0),
            t0 + std::time::Duration::from_millis(150)
        ));
        assert!(!is_double(
            Some(t0),
            t0 + std::time::Duration::from_millis(450)
        ));
        assert!(!is_double(None, t0));
    }
}
