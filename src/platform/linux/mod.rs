pub mod hotkey;
pub mod ipc;
pub mod portal;
pub mod shortcuts;
pub mod tray;

use std::path::PathBuf;

use anyhow::{bail, Result};
use gstreamer as gst;

use crate::capture::AudioSource;
use crate::gst_util::make;

/// A screen stream handed out by the ScreenCast portal as a PipeWire node.
pub struct Screen {
    pub fd: i32,
    pub node: u32,
}

impl Screen {
    pub fn element(&self) -> Result<gst::Element> {
        make(
            "pipewiresrc",
            &[
                ("fd", &self.fd.to_string()),
                ("path", &self.node.to_string()),
                ("do-timestamp", "true"),
                // Compositors only send frames on damage; resend the last one
                // so a static screen still produces keyframes.
                ("keepalive-time", "1000"),
                ("always-copy", "false"),
            ],
        )
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

pub fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::home().join(".local").join("state"))
        .join("clip-recorder")
}
