//! Global stdout event emitter.
//!
//! Lets worker threads (the StreamController, FPS tracker, etc.) emit events
//! without knowing the `main.rs` write logic, via an
//! `OnceLock<Mutex<Option<Sender<Value>>>>`. `main.rs` initialises it at boot
//! with the writer-thread channel; workers call `emit(...)` and the single
//! writer thread serialises it onto stdout (one JSON line per event).
//!
//! Pattern 1:1 from `streaming/gsr-sidecar/control.py::_output_queue` and
//! `streaming/{win,mac}-hq-sidecar/src/events.rs` — a single writer thread
//! resolves the race between response writers and event writers on stdout.

use serde_json::Value;
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

static EMITTER: OnceLock<Mutex<Option<Sender<Value>>>> = OnceLock::new();

/// Initialise the emitter with the writer-thread channel. Called once by `main`.
pub fn init(tx: Sender<Value>) {
    let _ = EMITTER.set(Mutex::new(Some(tx)));
}

/// Send an event to the stdout writer thread. Non-blocking; dropped on
/// disconnect (e.g. the writer thread already exited).
#[allow(dead_code)]
pub fn emit(event: Value) {
    if let Some(m) = EMITTER.get() {
        if let Ok(guard) = m.lock() {
            if let Some(tx) = guard.as_ref() {
                let _ = tx.send(event);
            }
        }
    }
}

/// Drop the emitter-internal sender. Called by `main` at shutdown — otherwise
/// the static `OnceLock` holds a sender clone for the whole process lifetime,
/// the writer thread never sees a disconnect, and `writer.join()` hangs forever.
pub fn shutdown() {
    if let Some(m) = EMITTER.get() {
        if let Ok(mut guard) = m.lock() {
            *guard = None;
        }
    }
}
