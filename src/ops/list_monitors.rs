//! `list_monitors` — Display-Enumeration.
//!
//! Phase 1: Stub (`[]`). Der Linux-Capture-Pfad nutzt standardmäßig den
//! Wayland-Portal-Dialog (wie der Python-GSR-Sidecar) — `list_monitors` wird
//! nur gebraucht, wenn wir einen In-App-Display-Picker anbieten wollen (Phase 4+:
//! DRM-Connectoren oder Portal-Display-Liste). Shape wie Windows-Sidecar:
//! `monitors: [{index (1-basiert), name, primary, width, height, refresh_hz}]`.

use anyhow::Result;
use serde_json::{Map, Value, json};

pub fn handle(_params: Map<String, Value>) -> Result<Map<String, Value>> {
    Ok(super::json_to_map(json!({ "monitors": [] })))
}
