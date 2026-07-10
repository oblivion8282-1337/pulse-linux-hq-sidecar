//! `list_windows` — Fenster-Enumeration.
//!
//! Phase 1: Stub (`[]`). Phase 4+: wlr-foreign-toplevel (Wayland) oder X11
//! (via ashpd / xcb). Der Portal-Dialog wählt sonst die Quelle. Shape wie
//! Windows-Sidecar: `windows: [{id, title, app, width, height}]`.

use anyhow::Result;
use serde_json::{Map, Value, json};

pub fn handle(_params: Map<String, Value>) -> Result<Map<String, Value>> {
    Ok(super::json_to_map(json!({ "windows": [] })))
}
