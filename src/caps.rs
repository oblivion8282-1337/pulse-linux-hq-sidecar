//! Encoder-Fähigkeits-Probe — welche Video-Codecs DIESE Maschine per Hardware
//! encodieren kann (VAAPI für AMD/Intel, NVENC für Nvidia), über das gelinkte
//! FFmpeg.
//!
//! Treibt `list_profiles` (der Renderer zeigt nur Codecs, die die HW kann) und
//! den `health`/`gpu_info`-Report. Gate nach *Fähigkeit*, nie nach Modellname.
//!
//! Echte Probe (`encode::probe_encoder`): pro Codec wird der Encoder mit einem
//! HW-Frames-Kontext tatsächlich geöffnet. Nur was sich öffnen lässt, gilt als
//! verfügbar — so verschwindet AV1 auf Karten ohne AV1-Encode (RTX 30xx, ältere
//! AMD-iGPUs) automatisch aus der UI, statt beim Streamen zu crashen. Ergebnis
//! wird einmal pro Prozess gecacht (die Probe legt CUDA/VAAPI-Kontexte an).
//! HEVC wird auf Linux nicht angeboten (Nutzerentscheidung: nur H264 + AV1).

use std::sync::OnceLock;

use crate::encode;
use crate::system::drm;

/// Kandidaten in Präferenzordnung (kein HEVC).
const CANDIDATES: &[&str] = &["h264", "av1"];

/// Hardware-encodierbare Video-Codecs auf dieser Maschine, in Präferenzordnung.
/// Ergebnis gecacht (Probe öffnet echte Encoder — einmal reicht).
pub fn available_video_codecs() -> &'static [&'static str] {
    static CACHE: OnceLock<Vec<&'static str>> = OnceLock::new();
    CACHE.get_or_init(probe_all).as_slice()
}

fn probe_all() -> Vec<&'static str> {
    let Some((vendor, render_node)) = drm::detect() else {
        tracing::warn!(target: "stream", "keine bekannte GPU erkannt — keine HW-Codecs gemeldet");
        return Vec::new();
    };
    let mut out = Vec::new();
    for &c in CANDIDATES {
        match encode::probe_encoder(vendor, &render_node, c) {
            Ok(true) => out.push(c),
            Ok(false) => tracing::info!(
                target: "stream", codec = c, vendor = vendor.slug(),
                "HW-Encode nicht verfügbar → wird nicht angeboten"
            ),
            Err(e) => tracing::warn!(
                target: "stream", codec = c,
                "Codec-Probe fehlgeschlagen ({e:#}) — konservativ nicht anbieten"
            ),
        }
    }
    tracing::info!(
        target: "stream", vendor = vendor.slug(), codecs = ?out,
        "HW-Encode-Probe abgeschlossen"
    );
    out
}

/// Kann diese Maschine den Pulse-Codec (h264/av1) per Hardware encodieren?
pub fn supports_codec(codec_id: &str) -> bool {
    available_video_codecs().contains(&codec_id)
}
