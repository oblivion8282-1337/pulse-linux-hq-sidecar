//! Capture-Smoke: Portal → PipeWire → DMABUF-Frames dumpen.
//!
//! Öffnet den Portal-Dialog (User wählt Quelle), verbindet den PipeWire-Stream
//! auf node_id+fd und printet für N ankommende DMABUF-Frames die Plane-Infos
//! (fds, offsets, strides, Maße). Noch kein Encode — isoliert die Capture-Kette.
//!
//! ```text
//! cargo run --release --example capture_smoke [--frames 10]
//! ```
//! User-Abbruch im Dialog → Exit 60.

use pulse_linux_hq_sidecar::capture::{pipewire_stream::PipewireCapture, portal};

fn main() {
    let n_frames: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);

    let session = match portal::open(true, &std::sync::atomic::AtomicBool::new(false)) {
        Ok(s) => s,
        Err(e) if portal::is_portal_canceled(&e) => {
            eprintln!("[capture_smoke] Portal abgebrochen → Exit 60");
            std::process::exit(portal::EXIT_PORTAL_CANCELED);
        }
        Err(e) => {
            eprintln!("[capture_smoke] portal::open: {e:#}");
            std::process::exit(1);
        }
    };
    eprintln!(
        "[capture_smoke] portal: node={} {}x{}",
        session.node_id, session.width, session.height
    );

    let (rx, mut cap) = match PipewireCapture::start(
        session.pw_fd,
        session.node_id,
        session.width,
        session.height,
    ) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[capture_smoke] PipewireCapture::start: {e:#}");
            std::process::exit(1);
        }
    };

    eprintln!("[capture_smoke] warte auf {n_frames} DMABUF-Frames …");
    for i in 0..n_frames {
        match rx.wait_take(std::time::Duration::from_secs(60)).and_then(|o| o.ok_or_else(|| anyhow::anyhow!("kein Frame in 60s"))) {
            Ok(f) => {
                let planes: Vec<String> = f
                    .planes
                    .iter()
                    .map(|p| format!("fd={} off={} stride={}", p.fd, p.offset, p.stride))
                    .collect();
                println!(
                    "[capture_smoke] frame {i}: {}x{} fourcc={:#010x} modifier={:#018x} planes=[{}]",
                    f.width,
                    f.height,
                    f.drm_fourcc,
                    f.modifier,
                    planes.join(", ")
                );
                // fds schließen (Smoke verbraucht sie nicht).
                for p in &f.planes {
                    unsafe { libc::close(p.fd) };
                }
            }
            Err(_) => {
                eprintln!("[capture_smoke] channel geschlossen (Capture-Thread beendet?)");
                break;
            }
        }
    }
    cap.stop();
    eprintln!("[capture_smoke] done.");
}
