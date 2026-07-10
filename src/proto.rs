//! Wire types — Request, Response, Event.
//!
//! Wire layout matches `streaming/gsr-sidecar/control.py` and
//! `streaming/{win,mac}-hq-sidecar/src/proto.rs` (byte-for-byte the same shapes):
//!
//! - Request:   `{"op": "...", "id": <number>?, ...op-specific params}`
//! - Response:  `{"id": <mirrored>, "ok": <bool>, ...op-specific fields}`
//!              On error: `{"id": ..., "ok": false, "error": "..."}`
//! - Event:     `{"ev": "...", ...event-specific fields}`  (no id/ok)
//!
//! Responses are flat objects (op-specific fields sit alongside `id`+`ok`,
//! never nested under `data`). Each op handler returns a `serde_json::Map`
//! that the dispatcher merges with `id`+`ok` before writing.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Inbound stdin line, before dispatch.
#[derive(Debug, Deserialize)]
pub struct Request {
    pub op: String,
    /// JS uses `number`; we accept i64 plus null/missing. The Python sidecar
    /// echoes back whatever shape it received — we mirror that.
    #[serde(default)]
    pub id: Option<i64>,
    /// Op-specific params (everything other than `op` + `id`).
    #[serde(flatten)]
    pub params: Map<String, Value>,
}

/// Outbound stdout line (response variant). Flat: op fields alongside id+ok.
#[derive(Debug, Serialize)]
pub struct Response {
    pub id: Option<i64>,
    pub ok: bool,
    #[serde(flatten)]
    pub fields: Map<String, Value>,
}

impl Response {
    pub fn ok(id: Option<i64>, fields: Map<String, Value>) -> Self {
        Self { id, ok: true, fields }
    }

    pub fn error(id: Option<i64>, msg: impl Into<String>) -> Self {
        let mut fields = Map::new();
        fields.insert("error".to_string(), Value::String(msg.into()));
        Self { id, ok: false, fields }
    }
}

/// Outbound stdout line (event variant). Async, no `id`/`ok`.
///
/// Variant names match the Linux sidecar's `ev` field exactly.
#[derive(Debug, Serialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
#[allow(dead_code)]
pub enum Event {
    State { state: StreamState, running: bool, uptime_s: f64 },
    Fps { fps: f64, uptime_s: f64 },
    Log { line: String },
    Error { message: String },
    Stopped { code: Option<i32> },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub enum StreamState {
    Idle,
    Starting,
    Live,
    Error,
    Stopped,
}
