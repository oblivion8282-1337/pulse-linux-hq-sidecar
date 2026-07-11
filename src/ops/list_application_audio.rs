//! `list_application_audio` — Apps mit laufendem Audio-Output.
//!
//! PipeWire-Registry-Enumeration (`capture::audio_router::list_applications`):
//! Nodes mit `media.class == "Stream/Output/Audio"`, Name = `application.name`
//! (Fallback `node.name`), dedupliziert + sortiert. Shape wie die anderen
//! Sidecars: `applications: [name, ...]`. Fehler (kein PipeWire erreichbar) →
//! leere Liste statt Error — die UI zeigt dann schlicht keine Apps an.

use anyhow::Result;
use serde_json::{Map, Value, json};

pub fn handle(_params: Map<String, Value>) -> Result<Map<String, Value>> {
    let apps = crate::capture::audio_router::list_applications().unwrap_or_else(|e| {
        tracing::warn!(target: "audio", "list_applications: {e:#}");
        Vec::new()
    });
    Ok(super::json_to_map(json!({ "applications": apps })))
}
