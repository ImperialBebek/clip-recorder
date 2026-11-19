use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Result};
use evdev::{EventSummary, KeyCode};

const MODIFIERS: [(&str, [KeyCode; 2]); 4] = [
    ("alt", [KeyCode::KEY_LEFTALT, KeyCode::KEY_RIGHTALT]),
    ("ctrl", [KeyCode::KEY_LEFTCTRL, KeyCode::KEY_RIGHTCTRL]),
    ("shift", [KeyCode::KEY_LEFTSHIFT, KeyCode::KEY_RIGHTSHIFT]),
    ("super", [KeyCode::KEY_LEFTMETA, KeyCode::KEY_RIGHTMETA]),
];

#[derive(Debug, PartialEq, Eq)]
pub struct Hotkey {
    /// Which of `MODIFIERS` must be held.
    mods: [bool; 4],
    key: KeyCode,
}

impl Hotkey {
    pub fn parse(s: &str) -> Result<Self> {
        let mut mods = [false; 4];
        let mut key = None;
        for part in s.split('+').map(|p| p.trim().to_ascii_lowercase()) {
            let alias = match part.as_str() {
                "control" => "ctrl",
                "meta" | "win" | "logo" => "super",
                p => p,
            };
            if let Some(i) = MODIFIERS.iter().position(|(n, _)| *n == alias) {
                mods[i] = true;
            } else if key.is_none() {
                let name = format!("KEY_{}", part.to_ascii_uppercase());
                key = Some(
                    name.parse::<KeyCode>()
                        .map_err(|_| anyhow!("unknown key '{part}' in hotkey '{s}'"))?,
                );
            } else {
                bail!("hotkey '{s}' has more than one non-modifier key");
            }
        }
        Ok(Self {
            mods,
            key: key.ok_or_else(|| anyhow!("hotkey '{s}' has no key"))?,
        })
    }

    fn matches(&self, pressed: &HashSet<KeyCode>) -> bool {
        MODIFIERS
            .iter()
            .zip(self.mods)
            .all(|((_, keys), want)| keys.iter().any(|k| pressed.contains(k)) == want)
    }
}

/// Listens on every keyboard in /dev/input. Works on Wayland and X11 alike,
/// and does not grab the device, so the game still sees the keypress.
/// Needs read access to /dev/input (the `input` group). Returns the number of
/// keyboards being watched.
pub fn spawn(hotkey: Hotkey, on_press: impl Fn() + Send + Sync + 'static) -> usize {
    let hotkey = Arc::new(hotkey);
    let on_press = Arc::new(on_press);
    let pressed = Arc::new(Mutex::new(HashSet::new()));
    let mut count = 0;
    for (_, mut dev) in evdev::enumerate() {
        let is_keyboard = dev
            .supported_keys()
            .is_some_and(|k| k.contains(hotkey.key) && k.contains(KeyCode::KEY_A));
        if !is_keyboard {
            continue;
        }
        count += 1;
        let (hotkey, on_press, pressed) = (hotkey.clone(), on_press.clone(), pressed.clone());
        std::thread::spawn(move || loop {
            let Ok(events) = dev.fetch_events() else {
                return;
            };
            for ev in events {
                let EventSummary::Key(_, code, value) = ev.destructure() else {
                    continue;
                };
                let mut pressed = pressed.lock().unwrap();
                match value {
                    0 => {
                        pressed.remove(&code);
                    }
                    1 => {
                        pressed.insert(code);
                        if code == hotkey.key && hotkey.matches(&pressed) {
                            on_press();
                        }
                    }
                    _ => {}
                }
            }
        });
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_combos() {
        let hk = Hotkey::parse("Alt+S").unwrap();
        assert_eq!(hk.key, KeyCode::KEY_S);
        assert_eq!(hk.mods, [true, false, false, false]);
        let hk = Hotkey::parse("ctrl+shift+f9").unwrap();
        assert_eq!(hk.key, KeyCode::KEY_F9);
        assert_eq!(hk.mods, [false, true, true, false]);
        assert!(Hotkey::parse("alt").is_err());
        assert!(Hotkey::parse("alt+nope").is_err());
    }

    #[test]
    fn requires_exact_modifiers() {
        let hk = Hotkey::parse("alt+s").unwrap();
        let mut pressed = HashSet::from([KeyCode::KEY_RIGHTALT, KeyCode::KEY_S]);
        assert!(hk.matches(&pressed));
        pressed.insert(KeyCode::KEY_LEFTCTRL);
        assert!(!hk.matches(&pressed));
    }
}
