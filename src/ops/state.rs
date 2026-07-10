//! `state` — current stream status.
//!
//! Shape (same as the other sidecars): `{ok, running, state, fps, uptime_s, argv}`,
//! read from the [`StreamController`] snapshot.

use anyhow::Result;
use serde_json::{Map, Number, Value};

use crate::stream_controller::StreamController;

pub fn handle(_params: Map<String, Value>) -> Result<Map<String, Value>> {
    let s = StreamController::singleton().state();
    let mut out = Map::new();
    out.insert("running".to_string(), Value::Bool(s.running));
    out.insert("state".to_string(), Value::String(s.state));
    out.insert(
        "fps".to_string(),
        s.fps
            .and_then(Number::from_f64)
            .map(Value::Number)
            .unwrap_or(Value::Null),
    );
    out.insert(
        "uptime_s".to_string(),
        s.uptime_s
            .and_then(Number::from_f64)
            .map(Value::Number)
            .unwrap_or(Value::Null),
    );
    out.insert(
        "argv".to_string(),
        match s.argv_redacted {
            Some(v) => Value::Array(v.into_iter().map(Value::String).collect()),
            None => Value::Null,
        },
    );
    Ok(out)
}
