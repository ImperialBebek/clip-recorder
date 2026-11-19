//! Everything OS-specific: screen and audio sources, the global hotkey and the
//! control socket. The rest of the recorder (encoding, ring buffer, muxing,
//! CLI) is portable; a Windows port adds a sibling module with the same API.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(not(target_os = "linux"))]
compile_error!("clip-recorder currently supports Linux only");
