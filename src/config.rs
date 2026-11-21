//! Persistent settings, written by the settings window and read by the recorder.
//! Plain `key = value` lines in `~/.config/clip-recorder/config`.

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::capture::AudioSource;
use crate::encoder::Encoder;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quality {
    Light,
    High,
    Max,
}

impl Quality {
    pub fn kbps(self) -> u32 {
        match self {
            Quality::Light => 12_000,
            Quality::High => 25_000,
            Quality::Max => 50_000,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Quality::Light => "light",
            Quality::High => "high",
            Quality::Max => "max",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "light" => Quality::Light,
            "high" => Quality::High,
            "max" => Quality::Max,
            _ => return None,
        })
    }
}

/// What the recorder captures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capture {
    /// The game's own window where the desktop allows it (KDE Plasma), else
    /// the screen picked once through the portal.
    Auto,
    /// Always the screen picked through the portal.
    Screen,
}

impl Capture {
    pub fn name(self) -> &'static str {
        match self {
            Capture::Auto => "auto",
            Capture::Screen => "screen",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "auto" => Capture::Auto,
            "screen" => Capture::Screen,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub duration: u32,
    pub fps: u32,
    pub quality: Quality,
    pub encoder: Option<Encoder>,
    pub audio: AudioSource,
    /// Microphone, saved as a second audio track.
    pub mic: AudioSource,
    /// PipeWire/Pulse source name (node.name) for the microphone; empty = the system default.
    pub mic_device: String,
    /// Per-app audio tracks in automatic mode.
    pub track_game: bool,
    pub track_voice: bool,
    pub track_other: bool,
    /// Read at startup.
    pub capture: Capture,
    pub output: PathBuf,
    pub hotkey: String,
    pub pause_without_game: bool,
    pub toast: bool,
    pub sound: bool,
    /// Look for a newer release once a day.
    pub check_updates: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            duration: 60,
            fps: 60,
            quality: Quality::High,
            encoder: None,
            audio: AudioSource::Auto,
            mic: AudioSource::Auto,
            mic_device: String::new(),
            track_game: true,
            track_voice: true,
            track_other: false,
            capture: Capture::Auto,
            output: crate::home().join("Videos").join("Clips"),
            hotkey: "alt+s".into(),
            pause_without_game: false,
            toast: true,
            sound: true,
            check_updates: true,
        }
    }
}

pub fn path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::home().join(".config"))
        .join("clip-recorder")
        .join("config")
}

fn bool_of(v: &str) -> Option<bool> {
    match v {
        "true" | "on" | "yes" | "1" => Some(true),
        "false" | "off" | "no" | "0" => Some(false),
        _ => None,
    }
}

impl Config {
    pub fn exists() -> bool {
        path().exists()
    }

    /// Unknown keys and bad values are ignored so an old or hand-edited file
    /// never stops the recorder from starting.
    pub fn load() -> Self {
        let mut c = Self::default();
        let Ok(text) = std::fs::read_to_string(path()) else {
            return c;
        };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "duration" => c.duration = v.parse().unwrap_or(c.duration).max(1),
                "fps" => c.fps = v.parse().unwrap_or(c.fps).max(1),
                "quality" => c.quality = Quality::parse(v).unwrap_or(c.quality),
                "encoder" => c.encoder = Encoder::parse(v).unwrap_or(c.encoder),
                "audio" => c.audio = AudioSource::parse(v).unwrap_or(c.audio),
                "mic" => c.mic = AudioSource::parse(v).unwrap_or(c.mic),
                "mic_device" => c.mic_device = v.to_owned(),
                "track_game" => c.track_game = bool_of(v).unwrap_or(c.track_game),
                "track_voice" => c.track_voice = bool_of(v).unwrap_or(c.track_voice),
                "track_other" => c.track_other = bool_of(v).unwrap_or(c.track_other),
                "capture" => c.capture = Capture::parse(v).unwrap_or(c.capture),
                "output" if !v.is_empty() => c.output = expand(v),
                "hotkey" if !v.is_empty() => c.hotkey = v.to_owned(),
                "pause_without_game" => {
                    c.pause_without_game = bool_of(v).unwrap_or(c.pause_without_game)
                }
                "toast" => c.toast = bool_of(v).unwrap_or(c.toast),
                "sound" => c.sound = bool_of(v).unwrap_or(c.sound),
                "check_updates" => c.check_updates = bool_of(v).unwrap_or(c.check_updates),
                _ => {}
            }
        }
        c
    }

    pub fn save(&self) -> Result<()> {
        let p = path();
        std::fs::create_dir_all(p.parent().unwrap())?;
        let text = format!(
            "duration = {}\nfps = {}\nquality = {}\nencoder = {}\naudio = {}\nmic = {}\nmic_device = {}\ntrack_game = {}\ntrack_voice = {}\ntrack_other = {}\ncapture = {}\noutput = {}\nhotkey = {}\npause_without_game = {}\ntoast = {}\nsound = {}\ncheck_updates = {}\n",
            self.duration,
            self.fps,
            self.quality.name(),
            self.encoder.map_or("auto", |e| e.name()),
            self.audio.name(),
            self.mic.name(),
            self.mic_device,
            self.track_game,
            self.track_voice,
            self.track_other,
            self.capture.name(),
            self.output.display(),
            self.hotkey,
            self.pause_without_game,
            self.toast,
            self.sound,
            self.check_updates,
        );
        std::fs::write(&p, text).with_context(|| format!("cannot write {}", p.display()))
    }
}

impl Config {
    /// Applies the fields that differ between two versions of the file, so a
    /// reload keeps command-line overrides for everything the user didn't touch.
    ///
    /// `force` names keys to take from `new` even when unchanged, e.g. a key the
    /// settings window just wrote back to the value already in the file.
    pub fn apply_changes(&mut self, old: &Config, new: &Config, force: &[&str]) {
        macro_rules! take {
            ($($f:ident),*) => {$(
                if old.$f != new.$f || force.contains(&stringify!($f)) {
                    self.$f = new.$f.clone();
                }
            )*};
        }
        take!(
            duration,
            fps,
            quality,
            encoder,
            audio,
            mic,
            mic_device,
            track_game,
            track_voice,
            track_other,
            output,
            hotkey,
            pause_without_game,
            toast,
            sound,
            check_updates
        );
    }
}

pub fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => crate::home().join(rest),
        None => PathBuf::from(p),
    }
}

/// `/home/me/Videos/Clips` -> `~/Videos/Clips`, for display.
pub fn tilde(p: &std::path::Path) -> String {
    match p.strip_prefix(crate::home()) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => p.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_round_trips() {
        for q in [Quality::Light, Quality::High, Quality::Max] {
            assert_eq!(Quality::parse(q.name()), Some(q));
        }
    }
}
