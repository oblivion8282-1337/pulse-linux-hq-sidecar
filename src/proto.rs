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
    /// `uptime_s`/`fps` als Ints — Parität zu control.py (der Renderer rendert
    /// die Werte direkt; Floats ergäben „59.94000000004 fps" in der UI).
    State { state: StreamState, running: bool, uptime_s: u64 },
    Fps { fps: u64, uptime_s: u64 },
    Log { line: String },
    Error { message: String },
    /// `code` bei None weggelassen (nicht null) — wie control.py; Some(60) =
    /// Portal-Abbruch durch den User.
    Stopped {
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<i32>,
    },
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

#[cfg(test)]
mod event_shape_tests {
    use super::*;

    /// Parität zu control.py: `fps`/`uptime_s` sind Ints auf der Leitung —
    /// der Renderer rendert die Werte direkt („59.94000000004 fps" sonst).
    #[test]
    fn fps_event_uses_integers() {
        let v = serde_json::to_value(Event::Fps { fps: 59, uptime_s: 12 }).unwrap();
        assert!(v["fps"].is_u64(), "fps muss int sein: {v}");
        assert!(v["uptime_s"].is_u64(), "uptime_s muss int sein: {v}");
    }

    #[test]
    fn state_event_uses_integer_uptime() {
        let v = serde_json::to_value(Event::State {
            state: StreamState::Live,
            running: true,
            uptime_s: 3,
        })
        .unwrap();
        assert!(v["uptime_s"].is_u64(), "uptime_s muss int sein: {v}");
    }

    /// Parität zu control.py: `code` wird bei None WEGGELASSEN (nicht null),
    /// bei Some mitgeschickt (Exit 60 = Portal-Abbruch).
    #[test]
    fn stopped_code_is_omitted_when_none() {
        let v = serde_json::to_value(Event::Stopped { code: None }).unwrap();
        assert!(
            v.as_object().unwrap().get("code").is_none(),
            "code:null darf nicht serialisiert werden: {v}"
        );
        let v = serde_json::to_value(Event::Stopped { code: Some(60) }).unwrap();
        assert_eq!(v["code"], serde_json::json!(60));
    }
}
