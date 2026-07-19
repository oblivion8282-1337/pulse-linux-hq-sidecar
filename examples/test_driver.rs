//! Test-Driver für den Linux-HQ-Sidecar — Rust-native Repro-/Smoke-Tool.
//!
//! Spawnt `pulse-linux-hq-sidecar`, redet JSON-RPC über stdin/stdout, capturet
//! stderr separat. Alle Streams werden zeitgestempelt in Konsole + Log-File
//! getee'd. Portiert aus `win-hq-sidecar/examples/test_driver.rs`.
//!
//! ```text
//! cargo build --release                                   # erst Sidecar bauen
//! cargo run --release --example test_driver               # default: protocol
//! cargo run --release --example test_driver -- protocol   # nicht-interaktiv, kein Dialog
//! cargo run --release --example test_driver -- health
//! cargo run --release --example test_driver -- video_only [push_url]
//! cargo run --release --example test_driver -- audio_mux  [push_url]
//! cargo run --release --example test_driver -- av1_mux    [push_url]
//! ```
//!
//! `$PULSE_HQ_SIDECAR_BIN` überschreibt den Auto-Resolver
//! (default: `target/release/pulse-linux-hq-sidecar` → `target/debug/...`).
//!
//! Szenarien:
//! - `protocol` — sweep über ALLE nicht-interaktiven Ops (health, gpu_info,
//!   list_monitors, list_windows, list_application_audio, build_argv, state)
//!   + unknown-op + invalid-json. **Kein Portal-Dialog** —
//!   der wiederholbare Wire-Protocol-Smoke.
//! - `health` — nur `health` + Exit.
//! - `video_only` — start mit audio=Aus. Erwartet `state=live` + ≥1 `fps`-Event
//!   binnen 20s, läuft 10s, dann `stop`. **Portal-Dialog:** Quelle wählen.
//! - `audio_mux` — wie video_only, aber audio.mode=System (H.264 + Opus).
//! - `av1_mux` — audio_mux mit codec-override av1.
//!
//! HEVC-Szenarien gibt es bewusst nicht (Projekt: nur H264 + AV1).

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

const DEFAULT_PUSH_URL: &str = "rtmps://localhost:11936/live/test?token=test";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const STATE_LIVE_TIMEOUT: Duration = Duration::from_secs(20);
const FIRST_FPS_TIMEOUT: Duration = Duration::from_secs(15);
const STREAM_RUN_DURATION: Duration = Duration::from_secs(10);

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let scenario = args.next().unwrap_or_else(|| "protocol".to_string());
    let push_url = args.next().unwrap_or_else(|| DEFAULT_PUSH_URL.to_string());

    let log = LogWriter::new(&scenario)?;
    log.write("driver", &format!("scenario={scenario} push_url={push_url}"));

    let bin = resolve_sidecar_bin()?;
    log.write("driver", &format!("sidecar bin: {}", bin.display()));

    let mut driver = Driver::spawn(bin, log.clone())?;
    let result = match scenario.as_str() {
        "protocol" => scenario_protocol(&mut driver, &push_url),
        "health" => scenario_health(&mut driver),
        "video_only" => scenario_full(&mut driver, &push_url, "Aus", None),
        "audio_mux" => scenario_full(&mut driver, &push_url, "System", None),
        "av1_mux" => scenario_full(&mut driver, &push_url, "System", Some("av1")),
        other => Err(anyhow::anyhow!(
            "unknown scenario: {other} (use: protocol | health | video_only | audio_mux | av1_mux)"
        )),
    };

    driver.shutdown();

    match &result {
        Ok(()) => log.write("driver", "scenario OK ✅"),
        Err(e) => log.write("driver", &format!("scenario FAILED ❌: {e:#}")),
    }
    log.write("driver", &format!("log saved: {}", log.path().display()));
    result
}

// ── Szenarien ───────────────────────────────────────────────────────────────

fn scenario_health(driver: &mut Driver) -> anyhow::Result<()> {
    let resp = driver.send("health", Map::new())?;
    if !response_ok(&resp) {
        anyhow::bail!("health response not ok: {resp}");
    }
    driver.log("driver", "health roundtrip OK");
    Ok(())
}

/// Nicht-interaktiver Wire-Protocol-Sweep. Kein Portal, kein Stream — prüft nur,
/// dass jede Op die erwartete Response-Form liefert (flach, `ok:true`, `id`
/// echoed) und Fehlerfälle sauber gemeldet werden.
fn scenario_protocol(driver: &mut Driver, push_url: &str) -> anyhow::Result<()> {
    // Parameterlose Read-Ops: alle müssen ok:true liefern.
    for op in [
        "health",
        "gpu_info",
        "list_monitors",
        "list_windows",
        "list_application_audio",
        "state",
    ] {
        let resp = driver.send(op, Map::new())?;
        if !response_ok(&resp) {
            anyhow::bail!("op {op} response not ok: {resp}");
        }
        if resp.get("id").is_none() {
            anyhow::bail!("op {op} response missing id (wire-protocol): {resp}");
        }
        driver.log("driver", &format!("op {op} OK"));
    }

    // build_argv braucht profile + channel (wie start, aber ohne Stream).
    let mut ba = Map::new();
    ba.insert("profile".into(), Value::String("H.264 Standard".into()));
    ba.insert("channel".into(), json!({ "push_url": push_url }));
    let resp = driver.send("build_argv", ba)?;
    if !response_ok(&resp) {
        anyhow::bail!("build_argv not ok: {resp}");
    }
    // Token muss in der argv redacted sein.
    let argv_str = resp.get("argv").map(|v| v.to_string()).unwrap_or_default();
    if argv_str.contains("token=test") {
        anyhow::bail!("build_argv hat den Token NICHT redacted: {argv_str}");
    }
    driver.log("driver", "build_argv OK (Token redacted)");

    // Fehlerfall 1: unbekannte Op → Response mit ok:false (kein Crash).
    let resp = driver.send("bogus_op_xyz", Map::new())?;
    if response_ok(&resp) {
        anyhow::bail!("unknown op sollte ok:false liefern: {resp}");
    }
    driver.log("driver", &format!("unknown-op sauber abgelehnt: {resp}"));

    // Fehlerfall 2: kaputtes JSON → Sidecar darf nicht sterben, muss antworten.
    driver.send_raw("das ist kein json")?;
    let resp = driver.wait_any_response(REQUEST_TIMEOUT).ok_or_else(|| {
        anyhow::anyhow!("keine Response auf invalid-json (Sidecar tot?)")
    })?;
    driver.log("driver", &format!("invalid-json sauber beantwortet: {resp}"));

    // Sidecar lebt noch? Finaler health-Roundtrip.
    let resp = driver.send("health", Map::new())?;
    if !response_ok(&resp) {
        anyhow::bail!("Sidecar nach Fehlerfällen nicht mehr gesund: {resp}");
    }
    driver.log("driver", "Protokoll-Sweep komplett — Sidecar gesund");
    Ok(())
}

fn scenario_full(
    driver: &mut Driver,
    push_url: &str,
    audio_mode: &str,
    override_codec: Option<&str>,
) -> anyhow::Result<()> {
    let resp = driver.send("health", Map::new())?;
    if !response_ok(&resp) {
        anyhow::bail!("health failed: {resp}");
    }

    let mut params = Map::new();
    params.insert("profile".into(), Value::String("H.264 Standard".into()));
    params.insert(
        "channel".into(),
        json!({ "id": "test-channel", "token": "", "push_url": push_url }),
    );
    params.insert("capture".into(), Value::String("portal".into()));
    params.insert("audio".into(), json!({ "mode": audio_mode, "excluded_apps": [] }));
    if let Some(c) = override_codec {
        params.insert("overrides".into(), json!({ "codec": c }));
    }

    driver.log("driver", "start → JETZT im Portal-Dialog die Quelle wählen …");
    let t_start = Instant::now();
    let resp = driver.send("start", params)?;
    if !response_ok(&resp) {
        anyhow::bail!("start failed: {resp}");
    }

    let live = driver.wait_event(
        |v| {
            v.get("ev").and_then(Value::as_str) == Some("state")
                && v.get("state").and_then(Value::as_str) == Some("live")
        },
        STATE_LIVE_TIMEOUT,
    );
    // Ein error-Event vor live abfangen (z. B. Dialog abgebrochen).
    if live.is_none() {
        anyhow::bail!("state=live nicht binnen {STATE_LIVE_TIMEOUT:?} erreicht (Dialog abgebrochen?)");
    }
    driver.log("driver", &format!("state=live nach {:?}", t_start.elapsed()));

    let t_live = Instant::now();
    driver
        .wait_event(|v| v.get("ev").and_then(Value::as_str) == Some("fps"), FIRST_FPS_TIMEOUT)
        .ok_or_else(|| anyhow::anyhow!("kein fps-Event binnen {FIRST_FPS_TIMEOUT:?} (Mux-Hang?)"))?;
    driver.log("driver", &format!("erstes fps-Event nach {:?}", t_live.elapsed()));

    let run_until = Instant::now() + STREAM_RUN_DURATION;
    let mut fps_count = 1u32;
    while Instant::now() < run_until {
        let remaining = run_until.saturating_duration_since(Instant::now());
        if driver
            .wait_event(
                |v| v.get("ev").and_then(Value::as_str) == Some("fps"),
                remaining.min(Duration::from_secs(3)),
            )
            .is_some()
        {
            fps_count += 1;
        }
    }
    driver.log("driver", &format!("{fps_count} fps-Events in {STREAM_RUN_DURATION:?}"));

    let resp = driver.send("stop", Map::new())?;
    if !response_ok(&resp) {
        anyhow::bail!("stop failed: {resp}");
    }
    // stopped-Event abwarten (sauberer Lebenszyklus).
    driver
        .wait_event(|v| v.get("ev").and_then(Value::as_str) == Some("stopped"), Duration::from_secs(10))
        .ok_or_else(|| anyhow::anyhow!("kein stopped-Event nach stop"))?;
    driver.log("driver", &format!("stop + stopped OK, {fps_count} fps-Events gesehen"));
    Ok(())
}

// ── Driver ──────────────────────────────────────────────────────────────────

struct Driver {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    incoming: Receiver<Incoming>,
    next_id: AtomicI64,
    log: LogWriter,
    pending_events: Vec<Value>,
}

#[derive(Debug)]
enum Incoming {
    Response { id: Option<i64>, body: Value },
    Event(Value),
    StdoutEof,
}

impl Driver {
    fn spawn(bin: PathBuf, log: LogWriter) -> anyhow::Result<Self> {
        let mut child = Command::new(&bin)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawn {}: {e}", bin.display()))?;

        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        let (tx, rx) = channel::<Incoming>();
        let tx_stdout = tx.clone();
        let log_stdout = log.clone();
        thread::Builder::new()
            .name("driver-stdout".into())
            .spawn(move || stdout_reader_loop(stdout, tx_stdout, log_stdout))?;
        let log_stderr = log.clone();
        thread::Builder::new()
            .name("driver-stderr".into())
            .spawn(move || stderr_reader_loop(stderr, log_stderr))?;

        Ok(Self {
            child: Some(child),
            stdin: Some(stdin),
            incoming: rx,
            next_id: AtomicI64::new(1),
            log,
            pending_events: Vec::new(),
        })
    }

    fn log(&self, source: &str, msg: &str) {
        self.log.write(source, msg);
    }

    fn send(&mut self, op: &str, params: Map<String, Value>) -> anyhow::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut req = params;
        req.insert("op".into(), Value::String(op.to_string()));
        req.insert("id".into(), Value::Number(id.into()));
        let line = serde_json::to_string(&Value::Object(req))?;
        self.write_line(&line)?;

        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                anyhow::bail!("response timeout for op={op} id={id}");
            }
            match self.incoming.recv_timeout(remaining) {
                Ok(Incoming::Response { id: rid, body }) if rid == Some(id) => return Ok(body),
                Ok(Incoming::Response { id: rid, body }) => {
                    self.log("driver", &format!("dropped stale response id={rid:?}: {body}"));
                }
                Ok(Incoming::Event(v)) => self.pending_events.push(v),
                Ok(Incoming::StdoutEof) => anyhow::bail!("sidecar stdout closed before response"),
                Err(RecvTimeoutError::Timeout) => anyhow::bail!("response timeout op={op} id={id}"),
                Err(RecvTimeoutError::Disconnected) => anyhow::bail!("driver channel disconnected"),
            }
        }
    }

    /// Rohe Zeile senden (für den invalid-json-Fehlerfall). Kein Response-Warten.
    fn send_raw(&mut self, raw: &str) -> anyhow::Result<()> {
        self.write_line(raw)
    }

    fn write_line(&mut self, line: &str) -> anyhow::Result<()> {
        self.log("→sidecar", line);
        let stdin = self.stdin.as_mut().ok_or_else(|| anyhow::anyhow!("stdin closed"))?;
        writeln!(stdin, "{line}")?;
        stdin.flush()?;
        Ok(())
    }

    /// Nächste Response beliebiger id abwarten (für invalid-json).
    fn wait_any_response(&mut self, timeout: Duration) -> Option<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match self.incoming.recv_timeout(remaining) {
                Ok(Incoming::Response { body, .. }) => return Some(body),
                Ok(Incoming::Event(v)) => self.pending_events.push(v),
                Ok(Incoming::StdoutEof) | Err(_) => return None,
            }
        }
    }

    fn wait_event<F>(&mut self, mut pred: F, timeout: Duration) -> Option<Value>
    where
        F: FnMut(&Value) -> bool,
    {
        if let Some(pos) = self.pending_events.iter().position(|v| pred(v)) {
            return Some(self.pending_events.remove(pos));
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match self.incoming.recv_timeout(remaining) {
                Ok(Incoming::Event(v)) => {
                    if pred(&v) {
                        return Some(v);
                    }
                    self.pending_events.push(v);
                }
                Ok(Incoming::Response { id, body }) => {
                    self.log("driver", &format!("stale response (waiting event) id={id:?}: {body}"));
                }
                Ok(Incoming::StdoutEof) => return None,
                Err(_) => return None,
            }
        }
    }

    fn shutdown(&mut self) {
        drop(self.stdin.take()); // stdin schließen → Sidecar bricht read-loop ab
        if let Some(mut child) = self.child.take() {
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(5) {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        self.log("driver", &format!("sidecar exited: {status}"));
                        return;
                    }
                    Ok(None) => thread::sleep(Duration::from_millis(100)),
                    Err(e) => {
                        self.log("driver", &format!("try_wait error: {e}"));
                        break;
                    }
                }
            }
            self.log("driver", "sidecar läuft nach 5s noch → kill");
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn stdout_reader_loop(stdout: impl std::io::Read, tx: Sender<Incoming>, log: LogWriter) {
    let reader = BufReader::new(stdout);
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                log.write("sidecar-out", &format!("read error: {e}"));
                break;
            }
        };
        let trimmed = line.trim_start_matches('\u{feff}').trim();
        if trimmed.is_empty() {
            continue;
        }
        log.write("sidecar-out", trimmed);
        let parsed: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                log.write("sidecar-out", &format!("[unparseable JSON: {e}]"));
                continue;
            }
        };
        let msg = if parsed.get("ev").is_some() {
            Incoming::Event(parsed)
        } else {
            let id = parsed.get("id").and_then(Value::as_i64);
            Incoming::Response { id, body: parsed }
        };
        if tx.send(msg).is_err() {
            break;
        }
    }
    let _ = tx.send(Incoming::StdoutEof);
}

fn stderr_reader_loop(stderr: impl std::io::Read, log: LogWriter) {
    let reader = BufReader::new(stderr);
    for line in reader.lines() {
        match line {
            Ok(l) => log.write("sidecar-err", &l),
            Err(e) => {
                log.write("sidecar-err", &format!("read error: {e}"));
                break;
            }
        }
    }
}

// ── LogWriter (thread-safe tee zu console + file) ───────────────────────────

#[derive(Clone)]
struct LogWriter {
    inner: Arc<Mutex<File>>,
    started: Instant,
    path: PathBuf,
}

impl LogWriter {
    fn new(scenario: &str) -> anyhow::Result<Self> {
        let ts = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let manifest = std::env::var("CARGO_MANIFEST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default());
        let target = manifest.join("target");
        std::fs::create_dir_all(&target).ok();
        let path = target.join(format!("test-driver-{scenario}-{ts}.log"));
        let file = File::create(&path)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(file)),
            started: Instant::now(),
            path,
        })
    }

    fn path(&self) -> &PathBuf {
        &self.path
    }

    fn write(&self, source: &str, msg: &str) {
        let offset = self.started.elapsed();
        let formatted = format!("[+{:>7.3}s] [{source:<11}] {msg}", offset.as_secs_f64());
        println!("{formatted}");
        if let Ok(mut file) = self.inner.lock() {
            let _ = writeln!(file, "{formatted}");
            let _ = file.flush();
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn response_ok(v: &Value) -> bool {
    v.get("ok").and_then(Value::as_bool).unwrap_or(false)
}

fn resolve_sidecar_bin() -> anyhow::Result<PathBuf> {
    if let Ok(env_bin) = std::env::var("PULSE_HQ_SIDECAR_BIN") {
        let p = PathBuf::from(env_bin);
        if p.exists() {
            return Ok(p);
        }
        anyhow::bail!("PULSE_HQ_SIDECAR_BIN zeigt auf nicht-existenten Pfad: {}", p.display());
    }
    let manifest = std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default());
    for sub in ["target/release", "target/debug"] {
        let candidate = manifest.join(sub).join("pulse-linux-hq-sidecar");
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    anyhow::bail!("kein Sidecar-Binary gefunden — erst `cargo build --release` oder $PULSE_HQ_SIDECAR_BIN setzen")
}
