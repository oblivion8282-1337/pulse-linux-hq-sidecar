//! `gpu_info` — GPU-Vendor + Codec-Set.
//!
//! Echte DRM-Vendor-Erkennung (`system::drm`: sysfs renderD*/driver →
//! nvidia/amd/intel) + Render-Node-Pfad (`card_path`). Codecs aktuell statisch
//! (h264+av1); die echte Open-Probe pro Vendor kommt mit den HW-Modulen (Phase 4).
//! Shape wie die anderen Sidecars: `{ok, vendor, card_path, display_server, video_codecs}`.

use anyhow::Result;
use serde_json::{Map, Value, json};

use crate::caps;
use crate::system::drm;

pub fn handle(_params: Map<String, Value>) -> Result<Map<String, Value>> {
    let (vendor, card_path) = match drm::detect() {
        Some((v, path)) => (Value::String(v.slug().to_string()), Value::String(path)),
        None => (Value::String("unknown".to_string()), Value::Null),
    };

    Ok(super::json_to_map(json!({
        "vendor": vendor,
        "card_path": card_path,
        "display_server": std::env::var("XDG_SESSION_TYPE").unwrap_or_default(),
        "video_codecs": caps::available_video_codecs(),
    })))
}
