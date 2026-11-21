pub mod activate;
pub mod hotkey;
pub mod ipc;
pub mod kwin;
pub mod portal;
pub mod shortcuts;
pub mod streams;
pub mod tray;

use std::path::PathBuf;

use anyhow::{bail, Result};
use gstreamer as gst;
use gstreamer::prelude::*;

use crate::capture::AudioSource;
use crate::gst_util::make;

/// A screen stream handed out by the ScreenCast portal as a PipeWire node.
pub struct Screen {
    /// The portal's PipeWire connection; `None` for the default one.
    pub fd: Option<i32>,
    pub node: u32,
}

impl Screen {
    pub fn element(&self) -> Result<gst::Element> {
        let fd = self.fd.map(|fd| fd.to_string());
        let node = self.node.to_string();
        let mut props: Vec<(&str, &str)> = vec![
            ("path", &node),
            ("do-timestamp", "true"),
            // Compositors only send frames on damage; resend the last one
            // so a static screen still produces keyframes.
            ("keepalive-time", "1000"),
            ("always-copy", "false"),
        ];
        if let Some(fd) = &fd {
            props.push(("fd", fd));
        }
        make("pipewiresrc", &props)
    }
}

/// Audio sources to try, in order, for the requested setting.
pub fn audio_candidates(want: AudioSource) -> &'static [AudioSource] {
    match want {
        AudioSource::Auto => &[AudioSource::PipeWire, AudioSource::Pulse],
        AudioSource::PipeWire => &[AudioSource::PipeWire],
        AudioSource::Pulse => &[AudioSource::Pulse],
        AudioSource::Test => &[AudioSource::Test],
        AudioSource::None => &[],
    }
}

/// Records the default output (what you hear), not the microphone.
pub fn audio_element(src: AudioSource) -> Result<gst::Element> {
    match src {
        AudioSource::PipeWire => make(
            "pipewiresrc",
            &[
                ("do-timestamp", "true"),
                (
                    "stream-properties",
                    "props,stream.capture.sink=(string)true,media.category=(string)Capture,media.role=(string)Game",
                ),
            ],
        ),
        AudioSource::Pulse => make("pulsesrc", &[("device", "@DEFAULT_MONITOR@"), ("do-timestamp", "true")]),
        AudioSource::Test => make("audiotestsrc", &[("is-live", "true"), ("volume", "0.2")]),
        _ => bail!("no audio source"),
    }
}

/// Records the microphone. `device` is a PipeWire/Pulse source name;
/// `None` records the default input. Uses the same backend list as
/// `audio_candidates`.
pub fn mic_element(src: AudioSource, device: Option<&str>) -> Result<gst::Element> {
    match src {
        AudioSource::PipeWire => {
            let mut props: Vec<(&str, &str)> = vec![
                ("do-timestamp", "true"),
                (
                    "stream-properties",
                    "props,media.category=(string)Capture,media.role=(string)Communication",
                ),
            ];
            if let Some(dev) = device {
                props.push(("target-object", dev));
            }
            make("pipewiresrc", &props)
        }
        AudioSource::Pulse => {
            let mut props: Vec<(&str, &str)> = vec![("do-timestamp", "true")];
            if let Some(dev) = device {
                props.push(("device", dev));
            }
            make("pulsesrc", &props)
        }
        // A different pitch from the desktop test tone, so the tracks differ.
        AudioSource::Test => make(
            "audiotestsrc",
            &[("is-live", "true"), ("volume", "0.2"), ("freq", "660")],
        ),
        _ => bail!("no microphone source"),
    }
}

pub struct MicDevice {
    pub id: String,
    pub label: String,
}

/// Microphones (capture sources, not output monitors), as the user would name them.
pub fn mic_devices() -> Vec<MicDevice> {
    let monitor = gst::DeviceMonitor::new();
    monitor.add_filter(Some("Audio/Source"), None);
    if monitor.start().is_err() {
        return Vec::new();
    }
    let mut seen = std::collections::HashSet::new();
    let mut mics = Vec::new();
    for dev in monitor.devices() {
        let props = dev.properties();
        let prop = |k: &str| props.as_ref().and_then(|p| p.get::<String>(k).ok());
        if prop("device.class").as_deref() == Some("monitor") {
            continue;
        }
        let Some(id) = prop("node.name").or_else(|| prop("device.name")) else {
            continue;
        };
        if !seen.insert(id.clone()) {
            continue;
        }
        mics.push(MicDevice {
            id,
            label: dev.display_name().to_string(),
        });
    }
    monitor.stop();
    mics
}

pub fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::home().join(".local").join("state"))
        .join("clip-recorder")
}
