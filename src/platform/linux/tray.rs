//! StatusNotifierItem tray icon (KDE, GNOME with the AppIndicator extension,
//! waybar, ...). Left click opens the styled menu window; right click shows
//! the same actions as a native menu for panels that only do dbusmenu.

use ksni::menu::StandardItem;
use ksni::{MenuItem, TrayMethods};
use tokio::sync::mpsc;

use super::ipc::Request;

pub struct Tray {
    pub tx: mpsc::UnboundedSender<Request>,
    pub paused: bool,
    pub seconds: u32,
}

fn send(t: &Tray, cmd: &str) {
    let _ = t.tx.send(Request {
        cmd: cmd.into(),
        reply: None,
    });
}

/// Starts another instance of this binary for a window (`menu`, `settings`),
/// so the recorder itself never links a GUI into its address space.
pub fn open_window(which: &str) {
    if let Ok(exe) = std::env::current_exe() {
        if let Ok(mut child) = std::process::Command::new(exe).arg(which).spawn() {
            std::thread::spawn(move || child.wait());
        }
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
        if self.paused {
            "media-playback-pause".into()
        } else {
            "media-record".into()
        }
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "Clip Recorder".into(),
            description: if self.paused {
                "Buffer paused".into()
            } else {
                format!("Live · {} s buffer", self.seconds)
            },
            ..Default::default()
        }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        open_window("menu");
    }
    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: format!("Save last {} s", self.seconds),
                icon_name: "media-record".into(),
                activate: Box::new(|t: &mut Self| send(t, "save")),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: if self.paused {
                    "Resume buffer"
                } else {
                    "Pause buffer"
                }
                .into(),
                activate: Box::new(|t: &mut Self| send(t, "toggle-pause")),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Open clips folder".into(),
                icon_name: "folder-videos".into(),
                activate: Box::new(|t: &mut Self| send(t, "open-folder")),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Settings…".into(),
                icon_name: "preferences-system".into(),
                activate: Box::new(|_: &mut Self| open_window("settings")),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|t: &mut Self| send(t, "quit")),
                ..Default::default()
            }
            .into(),
        ]
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
