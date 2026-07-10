//! `list_application_audio` — Apps mit Audio-Output.
//!
//! Phase 1: Stub (`[]`). Phase 4+: PipeWire-Node-Enumeration (Registry, gefiltert
//! auf Audio-Output-Nodes) → Node-Name → Prozess-Name via `sysinfo`. Shape wie
//! die anderen Sidecars: `applications: [name, ...]`.

use anyhow::Result;
use serde_json::{Map, Value, json};

pub fn handle(_params: Map<String, Value>) -> Result<Map<String, Value>> {
    Ok(super::json_to_map(json!({ "applications": [] })))
}
