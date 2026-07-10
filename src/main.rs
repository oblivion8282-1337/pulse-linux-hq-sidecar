//! Pulse — Linux HQ-streaming sidecar (entry point).
//!
//! Wire-format-equivalent to `streaming/gsr-sidecar/control.py` (Linux/Python)
//! and `streaming/{win,mac}-hq-sidecar/`: one JSON object per stdin line is a
//! request, one JSON object per stdout line is either a response (mirrors the
//! request `id`) or an async event (`{"ev": "...", ...}`, no `id`). See
//! `streaming/README.md` for the protocol.
//!
//! Identical protocol = `desktop/electron/sidecar.ts` only needs a platform
//! branch on which binary to spawn — every op name, request field, response
//! field and event payload matches the other sidecars.
//!
//! Threading: one writer thread serialises all stdout writes (responses + async
//! events from the stream controller). Pattern from `control.py`.

use std::io::{self, BufRead, Write};
use std::thread;

use pulse_linux_hq_sidecar::{dispatch, events, logging};

fn main() -> anyhow::Result<()> {
    // Diagnose-Logging (stderr) VOR allem anderen — Pulse tee't stderr in
    // sidecar.log. stdout bleibt exklusiv dem JSON-RPC-Protokoll.
    logging::init();
    tracing::info!(
        target: "stream",
        version = env!("CARGO_PKG_VERSION"),
        pid = std::process::id(),
        "pulse-linux-hq-sidecar startet"
    );

    let (out_tx, out_rx) = std::sync::mpsc::channel::<serde_json::Value>();
    events::init(out_tx.clone());

    // Writer thread: serialised stdout output.
    let writer = thread::Builder::new()
        .name("stdout-writer".into())
        .spawn(move || {
            let stdout = io::stdout();
            let mut out = stdout.lock();
            while let Ok(value) = out_rx.recv() {
                let json = match serde_json::to_string(&value) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!(target: "stream", error = %e, "Event-Serialisierung fehlgeschlagen");
                        continue;
                    }
                };
                if writeln!(out, "{json}").is_err() {
                    break;
                }
                if out.flush().is_err() {
                    break;
                }
            }
        })?;

    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break; // EOF on stdin (Electron closed our stdin) → shut down.
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Wie mac-hq-sidecar: kein self-exit nach `stop` — der Sidecar bleibt
        // über Streams hinweg warm und `sidecar.ts` hält den Child am Leben
        // (Windows-only respawn). Der Loop läuft bis stdin-EOF.
        let response = dispatch::handle_request_line(trimmed);
        match serde_json::to_value(&response) {
            Ok(v) => {
                if out_tx.send(v).is_err() {
                    break; // writer thread gone → shut down
                }
            }
            Err(e) => {
                tracing::error!(target: "stream", error = %e, "Response-Serialisierung fehlgeschlagen");
            }
        }
    }

    // EOF on stdin → let the writer thread finish. Drop the emitter-internal
    // sender clone first, otherwise the OnceLock holds it for the whole process
    // lifetime and `writer.join()` hangs forever.
    events::shutdown();
    drop(out_tx);
    let _ = writer.join();

    // Stop any running stream on shutdown (StreamController kommt in Phase 5).
    let _ = pulse_linux_hq_sidecar::stream_controller::StreamController::singleton().stop();

    Ok(())
}
