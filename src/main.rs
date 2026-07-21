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
    // Bytes statt `read_line`: ein einziges Nicht-UTF-8-Byte würde dort als
    // `InvalidData`-Error den ganzen Prozess (und einen laufenden Stream) hart
    // beenden — stattdessen antwortet `handle_request_bytes` deterministisch
    // mit `{"id":null,"ok":false}`.
    let mut line: Vec<u8> = Vec::new();

    loop {
        line.clear();
        let n = reader.read_until(b'\n', &mut line)?;
        if n == 0 {
            break; // EOF on stdin (Electron closed our stdin) → shut down.
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }

        // Wie mac-hq-sidecar: kein self-exit nach `stop` — der Sidecar bleibt
        // über Streams hinweg warm und `sidecar.ts` hält den Child am Leben
        // (Windows-only respawn). Der Loop läuft bis stdin-EOF.
        let response = dispatch::handle_request_bytes(&line);
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

    // EOF on stdin → Stream ZUERST stoppen: solange der Writer lebt, erreichen
    // die Terminal-Events (state:stopped/stopped) noch die Leitung — vorher
    // verpufften sie zwischen shutdown() und stop().
    let _ = pulse_linux_hq_sidecar::stream_controller::StreamController::singleton().stop();

    // Dann den Writer beenden. Drop the emitter-internal sender clone first,
    // otherwise the OnceLock holds it for the whole process lifetime and the
    // join hangs forever.
    events::shutdown();
    drop(out_tx);
    // Bounded join: hält der Parent das stdout-Read-Ende offen, liest aber
    // nicht mehr (eingefrorenes Electron), bliebe writeln!/flush auf der
    // vollen Pipe ewig stehen — und mit ihm der ganze Shutdown.
    let (join_tx, join_rx) = std::sync::mpsc::channel::<()>();
    thread::spawn(move || {
        let _ = writer.join();
        let _ = join_tx.send(());
    });
    let _ = join_rx.recv_timeout(std::time::Duration::from_secs(3));

    Ok(())
}
