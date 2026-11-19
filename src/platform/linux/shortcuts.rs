//! Global hotkey through the xdg-desktop-portal GlobalShortcuts API (KDE,
//! GNOME 48+, Hyprland): no `input` group needed, and the desktop shows the
//! binding in its own shortcut settings.

use anyhow::Result;
use ashpd::desktop::global_shortcuts::{BindShortcutsOptions, GlobalShortcuts, NewShortcut};
use ashpd::desktop::CreateSessionOptions;
use futures_util::StreamExt;

pub const APP_ID: &str = "io.github.imperialbebek.ClipRecorder";

/// `alt+s` -> `ALT+s`, the trigger syntax of the XDG shortcuts spec.
pub fn trigger(combo: &str) -> String {
    combo
        .split('+')
        .map(|p| match p.trim().to_lowercase().as_str() {
            "alt" => "ALT".to_owned(),
            "ctrl" | "control" => "CTRL".to_owned(),
            "shift" => "SHIFT".to_owned(),
            "super" | "meta" | "win" | "logo" => "LOGO".to_owned(),
            k if k.len() == 1 => k.to_owned(),
            k => {
                let mut c = k.chars();
                c.next()
                    .map(|f| f.to_uppercase().chain(c).collect())
                    .unwrap_or_default()
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// Binds the save shortcut and calls `on_press` on every activation. Returns
/// once the binding is in place; errors mean the portal is missing or refused.
pub async fn spawn(combo: &str, on_press: impl Fn() + Send + 'static) -> Result<()> {
    if let Ok(id) = APP_ID.try_into() {
        let _ = ashpd::register_host_app(id).await;
    }
    let portal = GlobalShortcuts::new().await?;
    let session = portal
        .create_session(CreateSessionOptions::default())
        .await?;
    let shortcut = NewShortcut::new("save-clip", "Save the last clip")
        .preferred_trigger(trigger(combo).as_str());
    portal
        .bind_shortcuts(&session, &[shortcut], None, BindShortcutsOptions::default())
        .await?
        .response()?;
    let mut activated = portal.receive_activated().await?;
    tokio::spawn(async move {
        let _keep = session;
        while let Some(ev) = activated.next().await {
            if ev.shortcut_id() == "save-clip" {
                on_press();
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn triggers() {
        assert_eq!(super::trigger("alt+s"), "ALT+s");
        assert_eq!(super::trigger("Ctrl+Shift+f9"), "CTRL+SHIFT+F9");
    }
}
