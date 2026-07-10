//! Vendor-Encoder-Optionen, orientiert an GSR (`src/main.cpp` open_video_hardware).
//!
//! GSR nutzt selbst ffmpeg-Encoder (`h264_nvenc`/`h264_vaapi`) via av_dict —
//! die Settings werden hier nahezu 1:1 nachgebaut. Nur H264 + AV1 (kein HEVC).
//!
//! Rate-Control-Option-Strings unterscheiden sich pro Vendor:
//!   NVENC:  `rc`  = constqp | vbr | cbr
//!   VAAPI:  `rc_mode` = CQP | VBR | CBR  (GROSS)

use ffmpeg_next as ffmpeg;
use ffmpeg::Dictionary;

use crate::system::drm::Vendor;

/// ffmpeg-Encoder-Name für Vendor + Pulse-Codec-Id (h264/av1).
pub fn encoder_name(vendor: Vendor, codec: &str) -> Option<&'static str> {
    match (vendor, codec) {
        (Vendor::Nvidia, "h264") => Some("h264_nvenc"),
        (Vendor::Nvidia, "av1") => Some("av1_nvenc"),
        (Vendor::Amd | Vendor::Intel, "h264") => Some("h264_vaapi"),
        (Vendor::Amd | Vendor::Intel, "av1") => Some("av1_vaapi"),
        _ => None,
    }
}

/// av_dict-Optionen für den Encoder-Open. CBR, Ultra-Low-Latency, kein B-Ref —
/// GSRs Performance-Tune.
pub fn vendor_opts(vendor: Vendor) -> Dictionary<'static> {
    let mut opts = Dictionary::new();
    match vendor {
        Vendor::Nvidia => {
            // GSR main.cpp: tune="ll", rc="cbr", b_ref_mode=0, coder=cabac.
            opts.set("tune", "ll");
            opts.set("rc", "cbr");
            opts.set("b_ref_mode", "0");
            opts.set("coder", "cabac");
            // preset/Multipass/rc-lookahead nur bei tune=quality (hier nicht).
        }
        Vendor::Amd | Vendor::Intel => {
            // GSR main.cpp: rc_mode="CBR", async_depth=3, low_power je Capability,
            // coder=cabac, tier=main (AV1).
            opts.set("rc_mode", "CBR");
            opts.set("async_depth", "3");
            opts.set("coder", "cabac");
            // low_power: Phase 4 erstmal aus (EncSlice); Phase 6 capability-gesteuert.
        }
    }
    opts
}
