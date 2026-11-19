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

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub duration: u32,
    pub fps: u32,
    pub quality: Quality,
    pub encoder: Option<Encoder>,
    pub audio: AudioSource,
    pub output: PathBuf,
    pub hotkey: String,
    pub pause_without_game: bool,
    pub toast: bool,
    pub sound: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            duration: 60,
            fps: 60,
            quality: Quality::High,
            encoder: None,
            audio: AudioSource::Auto,
            output: crate::home().join("Videos").join("Clips"),
            hotkey: "alt+s".into(),
            pause_without_game: false,
            toast: true,
            sound: false,
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
                "output" if !v.is_empty() => c.output = expand(v),
                "hotkey" if !v.is_empty() => c.hotkey = v.to_owned(),
                "pause_without_game" => {
                    c.pause_without_game = bool_of(v).unwrap_or(c.pause_without_game)
                }
                "toast" => c.toast = bool_of(v).unwrap_or(c.toast),
                "sound" => c.sound = bool_of(v).unwrap_or(c.sound),
                _ => {}
            }
        }
        c
    }

    pub fn save(&self) -> Result<()> {
        let p = path();
        std::fs::create_dir_all(p.parent().unwrap())?;
        let text = format!(
            "duration = {}\nfps = {}\nquality = {}\nencoder = {}\naudio = {}\noutput = {}\nhotkey = {}\npause_without_game = {}\ntoast = {}\nsound = {}\n",
            self.duration,
            self.fps,
            self.quality.name(),
            self.encoder.map_or("auto", |e| e.name()),
            self.audio.name(),
            self.output.display(),
            self.hotkey,
            self.pause_without_game,
            self.toast,
            self.sound,
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
            output,
            hotkey,
            pause_without_game,
            toast,
            sound
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
