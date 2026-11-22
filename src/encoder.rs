use anyhow::{bail, Result};
use gstreamer as gst;

use crate::gst_util::{capsfilter, has, make};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoder {
    /// GStreamer `va` plugin (VA-API, Intel/AMD, GStreamer >= 1.22).
    Va,
    /// Legacy `gstreamer-vaapi` plugin.
    Vaapi,
    /// NVIDIA NVENC (`nvcodec` plugin).
    Nvenc,
    /// Software fallback.
    X264,
}

impl Encoder {
    pub const AUTO_ORDER: [Encoder; 4] =
        [Encoder::Va, Encoder::Vaapi, Encoder::Nvenc, Encoder::X264];

    pub fn parse(s: &str) -> Result<Option<Encoder>> {
        Ok(match s {
            "auto" => None,
            "va" => Some(Encoder::Va),
            "vaapi" => Some(Encoder::Vaapi),
            "nvenc" | "nvidia" => Some(Encoder::Nvenc),
            "x264" | "software" | "sw" => Some(Encoder::X264),
            _ => bail!("unknown encoder '{s}' (auto, va, vaapi, nvenc, x264)"),
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Encoder::Va => "va",
            Encoder::Vaapi => "vaapi",
            Encoder::Nvenc => "nvenc",
            Encoder::X264 => "x264",
        }
    }

    /// How the UI names the encoder: "VA-API", "NVENC", "Software".
    pub fn label(self) -> &'static str {
        match self {
            Encoder::Va | Encoder::Vaapi => "VA-API",
            Encoder::Nvenc => "NVENC",
            Encoder::X264 => "Software",
        }
    }

    pub fn is_hardware(self) -> bool {
        self != Encoder::X264
    }

    pub fn available(self) -> bool {
        match self {
            Encoder::Va => has("vah264enc") || has("vah264lpenc"),
            Encoder::Vaapi => has("vaapih264enc"),
            Encoder::Nvenc => has("nvh264enc") || has("nvcudah264enc"),
            Encoder::X264 => has("x264enc"),
        }
    }

    /// Elements from raw frames to H.264 access units, colour conversion included.
    /// Hardware paths convert on the GPU so DMA-BUF frames never touch the CPU.
    pub fn elements(self, kbps: u32, gop: u32) -> Result<Vec<gst::Element>> {
        let va_kbps = va_bitrate(kbps, &va_encoder_name()).to_string();
        let kbps = kbps.to_string();
        let gop = gop.to_string();
        Ok(match self {
            Encoder::Va => {
                let enc = if has("vah264enc") {
                    "vah264enc"
                } else {
                    "vah264lpenc"
                };
                vec![
                    make("vapostproc", &[])?,
                    capsfilter("video/x-raw(memory:VAMemory),format=NV12")?,
                    make(
                        enc,
                        &[
                            ("rate-control", "cbr"),
                            ("bitrate", &va_kbps),
                            ("cpb-size", &va_kbps),
                            ("key-int-max", &gop),
                            ("b-frames", "0"),
                            ("target-usage", "7"),
                        ],
                    )?,
                ]
            }
            Encoder::Vaapi => vec![
                make("vaapipostproc", &[])?,
                make(
                    "vaapih264enc",
                    &[
                        ("rate-control", "cbr"),
                        ("bitrate", &kbps),
                        ("keyframe-period", &gop),
                        ("max-bframes", "0"),
                        ("tune", "low-power"),
                    ],
                )?,
            ],
            Encoder::Nvenc => {
                if has("nvcudah264enc") && has("cudaupload") && has("cudaconvert") {
                    vec![
                        make("cudaupload", &[])?,
                        make("cudaconvert", &[])?,
                        make(
                            "nvcudah264enc",
                            &[
                                ("rc-mode", "cbr"),
                                ("bitrate", &kbps),
                                ("gop-size", &gop),
                                ("b-frames", "0"),
                                ("zerolatency", "true"),
                            ],
                        )?,
                    ]
                } else {
                    vec![
                        make("videoconvert", &[])?,
                        make(
                            "nvh264enc",
                            &[
                                ("rc-mode", "cbr"),
                                ("bitrate", &kbps),
                                ("gop-size", &gop),
                                ("bframes", "0"),
                                ("zerolatency", "true"),
                            ],
                        )?,
                    ]
                }
            }
            Encoder::X264 => vec![
                make("videoconvert", &[])?,
                capsfilter("video/x-raw,format=I420")?,
                make(
                    "x264enc",
                    &[
                        ("tune", "zerolatency"),
                        ("speed-preset", "ultrafast"),
                        ("bitrate", &kbps),
                        ("key-int-max", &gop),
                        ("bframes", "0"),
                    ],
                )?,
            ],
        })
    }
}

/// The VA H.264 encoder's long name, e.g. "VA-API H.264 Encoder in AMD Radeon RX 9060 XT".
fn va_encoder_name() -> String {
    ["vah264enc", "vah264lpenc"]
        .iter()
        .find_map(|f| gst::ElementFactory::find(f))
        .and_then(|f| f.metadata("long-name").map(str::to_owned))
        .unwrap_or_default()
}

/// The bitrate to ask the VA encoder for so clips come out at `kbps`: AMD's
/// CBR writes about 20% over its target on game footage (measured 14.4 Mb/s
/// for 12 Mb/s), so it is asked for 5/6 of the target.
fn va_bitrate(kbps: u32, encoder_name: &str) -> u32 {
    if encoder_name.contains("AMD") {
        kbps * 5 / 6
    } else {
        kbps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calibrates_amd_va_bitrate() {
        assert_eq!(va_bitrate(12_000, "VA-API H.264 Encoder in AMD Radeon RX 9060 XT"), 10_000);
        assert_eq!(va_bitrate(12_000, "VA-API H.264 Encoder in Intel(R) Arc(tm) Graphics"), 12_000);
        assert_eq!(va_bitrate(12_000, ""), 12_000);
    }
}
