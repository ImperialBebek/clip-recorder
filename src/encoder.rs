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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Av1,
    H264,
}

impl Codec {
    pub fn name(self) -> &'static str {
        match self {
            Codec::Av1 => "av1",
            Codec::H264 => "h264",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Codec::Av1 => "AV1",
            Codec::H264 => "H.264",
        }
    }

    pub fn parser(self) -> &'static str {
        match self {
            Codec::Av1 => "av1parse",
            Codec::H264 => "h264parse",
        }
    }

    pub fn caps(self) -> &'static str {
        match self {
            Codec::Av1 => "video/x-av1,stream-format=obu-stream,alignment=tu",
            Codec::H264 => "video/x-h264,stream-format=avc,alignment=au",
        }
    }

    pub fn from_caps(caps: &gst::CapsRef) -> Option<Codec> {
        match caps.structure(0)?.name().as_str() {
            "video/x-av1" => Some(Codec::Av1),
            "video/x-h264" => Some(Codec::H264),
            _ => None,
        }
    }

    /// AV1 matches H.264 quality at about 60% of the bitrate.
    pub fn kbps(self, h264_kbps: u32) -> u32 {
        match self {
            Codec::H264 => h264_kbps,
            Codec::Av1 => h264_kbps * 3 / 5,
        }
    }
}

/// Encoders to consider: the forced one, else every usable one in `AUTO_ORDER`.
pub fn candidates(forced: Option<Encoder>) -> Vec<Encoder> {
    match forced {
        Some(e) => vec![e],
        None => Encoder::AUTO_ORDER
            .into_iter()
            .filter(|e| e.available())
            .collect(),
    }
}

/// Encoder/codec pairs to try in order: AV1 on every encoder first, then H.264.
pub fn attempts(
    encoders: &[Encoder],
    allow_av1: bool,
    supports: impl Fn(Encoder, Codec) -> bool,
) -> Vec<(Encoder, Codec)> {
    let mut out = Vec::new();
    if allow_av1 {
        for &enc in encoders {
            if supports(enc, Codec::Av1) {
                out.push((enc, Codec::Av1));
            }
        }
    }
    for &enc in encoders {
        if supports(enc, Codec::H264) {
            out.push((enc, Codec::H264));
        }
    }
    out
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

    pub fn supports(self, codec: Codec) -> bool {
        match codec {
            Codec::H264 => self.available(),
            Codec::Av1 => match self {
                Encoder::Va => has("vaav1enc") || has("vaav1lpenc"),
                Encoder::Nvenc => has("nvav1enc"),
                Encoder::Vaapi | Encoder::X264 => false,
            },
        }
    }

    /// Elements from raw frames to encoded access units, colour conversion included.
    /// Hardware paths convert on the GPU so DMA-BUF frames never touch the CPU.
    pub fn elements(self, codec: Codec, kbps: u32, gop: u32) -> Result<Vec<gst::Element>> {
        if !self.supports(codec) {
            bail!("{} cannot encode {}", self.label(), codec.label());
        }
        if codec == Codec::Av1 {
            return self.av1_elements(kbps, gop);
        }
        let vendor = va_vendor();
        let va_kbps = va_bitrate(kbps, &vendor).to_string();
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
                            ("target-usage", va_target_usage(&vendor)),
                            ("ref-frames", "1"),
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

impl Encoder {
    fn av1_elements(self, kbps: u32, gop: u32) -> Result<Vec<gst::Element>> {
        let kbps = kbps.to_string();
        let gop = gop.to_string();
        Ok(match self {
            Encoder::Va => {
                let enc = if has("vaav1enc") {
                    "vaav1enc"
                } else {
                    "vaav1lpenc"
                };
                vec![
                    make("vapostproc", &[])?,
                    capsfilter("video/x-raw(memory:VAMemory),format=NV12")?,
                    make(
                        enc,
                        &[
                            ("rate-control", "cbr"),
                            ("bitrate", &kbps),
                            ("cpb-size", &kbps),
                            ("key-int-max", &gop),
                            ("hierarchical-level", "1"),
                            ("ref-frames", "1"),
                            ("target-usage", va_target_usage(&va_vendor())),
                        ],
                    )?,
                ]
            }
            Encoder::Nvenc => {
                let props: &[(&str, &str)] = &[
                    ("rc-mode", "cbr"),
                    ("bitrate", &kbps),
                    ("gop-size", &gop),
                    ("bframes", "0"),
                    ("zerolatency", "true"),
                ];
                if has("cudaupload") && has("cudaconvert") {
                    vec![
                        make("cudaupload", &[])?,
                        make("cudaconvert", &[])?,
                        make("nvav1enc", props)?,
                    ]
                } else {
                    vec![make("videoconvert", &[])?, make("nvav1enc", props)?]
                }
            }
            Encoder::Vaapi | Encoder::X264 => bail!("{} cannot encode AV1", self.label()),
        })
    }
}

/// PCI vendor id of the GPU behind `vah264enc` (the first render node), e.g. "0x1002" for AMD.
fn va_vendor() -> String {
    std::fs::read_to_string("/sys/class/drm/renderD128/device/vendor")
        .map(|v| v.trim().to_owned())
        .unwrap_or_default()
}

/// Intel reads target-usage 7 as fastest, but Mesa hands it to AMD as quality
/// level 7: levels 4 and up took ~4.9 ms of VCN time per 1440p frame against
/// ~2.7 ms for levels 1-3 (with one reference frame), for under 0.1 dB PSNR.
fn va_target_usage(vendor: &str) -> &'static str {
    if vendor == "0x1002" {
        "2"
    } else {
        "7"
    }
}

/// The bitrate to ask the VA encoder for so clips come out at `kbps`: AMD's
/// CBR writes about 20% over its target on game footage (measured 14.4 Mb/s
/// for 12 Mb/s), so it is asked for 5/6 of the target.
fn va_bitrate(kbps: u32, vendor: &str) -> u32 {
    if vendor == "0x1002" {
        kbps * 5 / 6
    } else {
        kbps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amd_va_uses_fast_target_usage() {
        assert_eq!(va_target_usage("0x1002"), "2");
        assert_eq!(va_target_usage("0x8086"), "7");
    }

    #[test]
    fn calibrates_amd_va_bitrate() {
        assert_eq!(va_bitrate(12_000, "0x1002"), 10_000);
        assert_eq!(va_bitrate(12_000, "0x8086"), 12_000);
        assert_eq!(va_bitrate(12_000, ""), 12_000);
    }

    #[test]
    fn av1_scales_bitrate() {
        assert_eq!(Codec::Av1.kbps(12_000), 7_200);
        assert_eq!(Codec::Av1.kbps(8_000), 4_800);
        assert_eq!(Codec::Av1.kbps(20_000), 12_000);
        assert_eq!(Codec::H264.kbps(12_000), 12_000);
    }

    #[test]
    fn codec_from_caps() {
        gst::init().unwrap();
        for c in [Codec::Av1, Codec::H264] {
            let caps: gst::Caps = c.caps().parse().unwrap();
            assert_eq!(Codec::from_caps(&caps), Some(c));
        }
        let caps: gst::Caps = "video/x-raw".parse().unwrap();
        assert_eq!(Codec::from_caps(&caps), None);
    }

    #[test]
    fn attempts_order() {
        let all = [Encoder::Va, Encoder::Vaapi, Encoder::X264];
        let sup = |e: Encoder, c: Codec| c == Codec::H264 || e == Encoder::Va;
        assert_eq!(
            attempts(&all, true, sup),
            vec![
                (Encoder::Va, Codec::Av1),
                (Encoder::Va, Codec::H264),
                (Encoder::Vaapi, Codec::H264),
                (Encoder::X264, Codec::H264)
            ]
        );
        assert_eq!(
            attempts(&all, false, sup),
            vec![
                (Encoder::Va, Codec::H264),
                (Encoder::Vaapi, Codec::H264),
                (Encoder::X264, Codec::H264)
            ]
        );
        assert_eq!(
            attempts(&[Encoder::X264], true, sup),
            vec![(Encoder::X264, Codec::H264)]
        );
        assert!(attempts(&[], true, sup).is_empty());
        // A hybrid PC where only NVIDIA encodes AV1 still prefers the NVIDIA
        // AV1 over the iGPU's H.264.
        let hybrid = |e: Encoder, c: Codec| c == Codec::H264 || e == Encoder::Nvenc;
        assert_eq!(
            attempts(&[Encoder::Va, Encoder::Nvenc], true, hybrid),
            vec![
                (Encoder::Nvenc, Codec::Av1),
                (Encoder::Va, Codec::H264),
                (Encoder::Nvenc, Codec::H264)
            ]
        );
    }
}
