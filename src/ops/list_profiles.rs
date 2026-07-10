//! `list_profiles` — stream/server/audio-mode catalog.
//!
//! 1:1 from `gsr-sidecar/control.py::op_list_profiles`. Shape:
//!
//! ```jsonc
//! {"ok": true,
//!  "profiles": [{name, codec, audio_codec, container, bitrate_kbps, fps,
//!                needs_custom_build, notes}, ...],
//!  "servers": [],
//!  "audio_modes": ["Aus", "Desktop", "Mikrofon", "Desktop + Mikrofon"],
//!  "app_label_prefix": "App: "}
//! ```

use anyhow::Result;
use serde_json::{Map, Value, json};

use crate::caps;
use crate::profiles::{APP_LABEL_PREFIX, AUDIO_MODES, PROFILES};

pub fn handle(_params: Map<String, Value>) -> Result<Map<String, Value>> {
    // Only advertise profiles whose codec THIS machine can hardware-encode, so
    // the renderer never offers e.g. AV1 on hardware without an AV1 encoder.
    let profiles: Vec<Value> = PROFILES
        .iter()
        .filter(|p| caps::supports_codec(p.codec))
        .map(|p| {
            json!({
                "name": p.name,
                "codec": p.codec,
                "audio_codec": p.audio_codec,
                "container": p.container,
                "bitrate_kbps": p.bitrate_kbps,
                "fps": p.fps,
                "needs_custom_build": p.needs_custom_build,
                "notes": p.notes,
            })
        })
        .collect();

    let mut out = Map::new();
    out.insert("profiles".to_string(), Value::Array(profiles));
    // `servers` stays empty — Pulse always streams into a voice channel, no
    // server catalog. Shape-compat with the renderer type `GsrListProfiles`.
    out.insert("servers".to_string(), Value::Array(vec![]));
    out.insert(
        "audio_modes".to_string(),
        Value::Array(AUDIO_MODES.iter().map(|s| Value::String((*s).to_string())).collect()),
    );
    out.insert(
        "app_label_prefix".to_string(),
        Value::String(APP_LABEL_PREFIX.to_string()),
    );
    Ok(out)
}
