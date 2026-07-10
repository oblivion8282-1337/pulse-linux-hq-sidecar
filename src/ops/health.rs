//! `health` — capability probe.
//!
//! Wire-form mirrors `gsr-sidecar/control.py::op_health`:
//!
//! ```jsonc
//! {"ok": true, "gsr": {"available": ..., "source": ..., "is_flatpak": ...,
//!                       "path": ..., "version": ..., "vendor": ...,
//!                       "display_server": ..., "video_codecs": [...],
//!                       "capture_options": [...], "has_flv_patch": ...,
//!                       "tls_backend": "gnutls"|"openssl"|...}}
//! ```
//!
//! Auf Linux ist der Encoder VAAPI (AMD/Intel) bzw. NVENC (Nvidia) — beides
//! über das gelinkte FFmpeg. `video_codecs` ist die echt hardware-encodierbare
//! Menge (Phase 3: echte Probe; Phase 1: statisch h264+av1). `tls_backend`
//! verrät, ob `tls_verify=0` für self-signed MediaMTX-certs mit dem
//! System-FFmpeg funktioniert (GnuTLS/OpenSSL ja; siehe tls_probe-Example).
//!
//! Anders als der Python-GSR-Sidecar: `available=true` heißt hier „Sidecar
//! selbst kann capturen+encoden+pushen" — es gibt kein externes
//! gpu-screen-recorder-Binary mehr, das gefunden werden müsste. `has_flv_patch`
//! entfällt (ffmpeg-as-lib muxed Opus in FLV ohne Patch) → `null`.

use anyhow::Result;
use serde_json::{Map, Value, json};

use crate::caps;
use crate::system::{drm, tls};

pub fn handle(_params: Map<String, Value>) -> Result<Map<String, Value>> {
    let path = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string));

    // Echte DRM-Vendor-Erkennung (sysfs). Liefert Vendor-Slug + Render-Node.
    let (vendor_slug, available) = match drm::detect() {
        Some((v, _)) => (v.slug(), true),
        None => ("unknown", false),
    };

    let mut gsr = json!({
        "available": available,
        "source": "builtin",
        "is_flatpak": std::path::Path::new("/.flatpak-info").exists(),
        "vendor": vendor_slug,
        "display_server": detect_display_server(),
        // Codecs (Phase 4: echte Open-Probe pro Vendor; aktuell statisch h264+av1).
        "video_codecs": caps::available_video_codecs(),
        // PipeWire/Portal kann Display, Window oder Region capturen.
        "capture_options": ["display", "window", "region"],
        // entfällt (ffmpeg-as-lib muxed Opus→FLV ohne GSR-Patch).
        "has_flv_patch": Value::Null,
        // Echt aus avformat_configuration() — verrät, ob tls_verify=0 für
        // RTMPS mit self-signed MediaMTX-certs greift (gnutls/openssl: ja).
        "tls_backend": tls::detect(),
    });
    if let Some(p) = path {
        gsr["path"] = Value::String(p);
    }

    let mut out = Map::new();
    out.insert("gsr".to_string(), gsr);
    Ok(out)
}

fn detect_display_server() -> &'static str {
    match std::env::var("XDG_SESSION_TYPE").as_deref() {
        Ok("wayland") => "wayland",
        Ok("x11") => "x11",
        _ => "unknown",
    }
}
