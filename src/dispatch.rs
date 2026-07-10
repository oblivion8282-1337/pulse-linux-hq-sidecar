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
