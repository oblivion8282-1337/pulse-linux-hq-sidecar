//! Request → op-handler dispatch.
//!
//! Every op handler signature is `fn(params: Map<String, Value>) -> Result<Map<String, Value>>`.
//! Returning an `Err` becomes `{"ok": false, "error": "..."}`; returning `Ok(map)`
//! becomes `{"ok": true, ...map}`.
//!
//! No `exit_after` flag (wie mac-hq-sidecar, ungleich Windows): der Sidecar
//! bleibt über Streams hinweg warm — siehe `main.rs`.

use serde_json::{Map, Value};

use crate::ops;
use crate::proto::{Request, Response};

/// Parse one stdin line and dispatch to the matching op handler. Parse failures
/// map to `{"id": null, "ok": false, ...}` so the parent (Electron's
/// `sidecar.ts`) sees a deterministic shape.
/// Eine rohe stdin-Zeile (Bytes) behandeln. Ungültiges UTF-8 wird zur
/// deterministischen `{"id":null,"ok":false}`-Response statt den Prozess zu
/// killen (`read_line` würde mit `InvalidData` abbrechen — und damit einen
/// laufenden Stream hart beenden).
pub fn handle_request_bytes(line: &[u8]) -> Response {
    match std::str::from_utf8(line) {
        Ok(s) => handle_request_line(s.trim()),
        Err(e) => Response::error(None, format!("invalid JSON request: not valid UTF-8: {e}")),
    }
}

pub fn handle_request_line(line: &str) -> Response {
    let req: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            return Response::error(None, format!("invalid JSON request: {e}"));
        }
    };
    dispatch(req)
}

fn dispatch(req: Request) -> Response {
    let id = req.id;
    let result: anyhow::Result<Map<String, Value>> = match req.op.as_str() {
        "health" => ops::health::handle(req.params),
        "gpu_info" => ops::gpu_info::handle(req.params),
        "list_profiles" => ops::list_profiles::handle(req.params),
        "list_monitors" => ops::list_monitors::handle(req.params),
        "list_windows" => ops::list_windows::handle(req.params),
        "list_application_audio" => ops::list_application_audio::handle(req.params),
        "build_argv" => ops::build_argv::handle(req.params),
        "start" => ops::start::handle(req.params),
        "stop" => ops::stop::handle(req.params),
        "state" => ops::state::handle(req.params),
        unknown => Err(anyhow::anyhow!("unknown op: {unknown}")),
    };
    match result {
        Ok(fields) => Response::ok(id, fields),
        Err(e) => Response::error(id, format!("{e:#}")),
    }
}

#[cfg(test)]
mod request_bytes_tests {
    use super::handle_request_bytes;

    #[test]
    fn invalid_utf8_yields_error_response_instead_of_dying() {
        // 0xFF ist in UTF-8 nie gültig.
        let resp = handle_request_bytes(b"\xff\xfe{\"op\":\"health\"}");
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["ok"], serde_json::Value::Bool(false));
        assert!(v["id"].is_null());
        assert!(v["error"].as_str().unwrap_or_default().contains("invalid"));
    }

    #[test]
    fn valid_utf8_dispatches_normally() {
        let resp = handle_request_bytes(b" {\"op\":\"list_profiles\",\"id\":7} \n");
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["ok"], serde_json::Value::Bool(true));
        assert_eq!(v["id"], serde_json::json!(7));
    }
}
