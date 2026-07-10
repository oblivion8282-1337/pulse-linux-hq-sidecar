//! Diagnose-Logging (stderr).
//!
//! **stdout ist heilig** — dort läuft nur das JSON-RPC-Protokoll. Alle
//! Diagnostik geht auf **stderr**. Pulse (`desktop/electron/sidecar-log.ts`)
//! tee't jede stderr-Zeile zeitgestempelt und token-redacted nach
//! `<userData>/sidecar.log` (mit Rotation) — für die experimentelle Rust-
//! Version wird diese Datei zusätzlich auf den Server hochgeladen. Deshalb
//! braucht der Sidecar KEIN eigenes Datei-Logging; er muss nur sauber,
//! stufig und mit Modul-Tags auf stderr loggen.
//!
//! Steuerung über `PULSE_HQ_LOG` (wie `RUST_LOG`): Default `info`; z.B.
//! `PULSE_HQ_LOG=debug` oder gezielt `PULSE_HQ_LOG=info,pipewire=debug,nvenc=debug`.
//! Targets im Code: `pipewire`, `nvenc`, `vaapi`, `audio`, `egl`, `stream`, `mux`.
//!
//! Zeitstempel bewusst weggelassen: Pulse stempelt beim Tee, und im Terminal
//! genügt die Zeilenreihenfolge. (Vermeidet die `time`-Feature-Falle.)

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

use tracing_subscriber::EnvFilter;

static INITIALISED: AtomicBool = AtomicBool::new(false);

/// Richtet den globalen Tracing-Subscriber ein (idempotent, einmalig).
/// Muss ganz früh in `main()` laufen, vor dem ersten Log.
pub fn init() {
    if INITIALISED.swap(true, Ordering::SeqCst) {
        return;
    }
    let filter = EnvFilter::try_from_env("PULSE_HQ_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    // ANSI-Farben nur, wenn stderr ein echtes Terminal ist (Dev). Unter Pulse
    // ist stderr eine Pipe → keine Farb-Escapes in der sidecar.log.
    let ansi = std::io::stderr().is_terminal();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(ansi)
        .with_target(true)
        .without_time()
        .finish();
    // `set_global_default` scheitert nur, wenn schon einer gesetzt ist — dann
    // ist unser Guard oben eh gegriffen; ignorieren.
    let _ = tracing::subscriber::set_global_default(subscriber);
}
