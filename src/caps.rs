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

use std::sync::Mutex;

use crate::encode;
use crate::system::drm;

/// Kandidaten in Präferenzordnung (kein HEVC).
const CANDIDATES: &[&str] = &["h264", "av1"];

/// Hardware-encodierbare Video-Codecs auf dieser Maschine, in Präferenzordnung.
/// Nur DEFINITIVE Ergebnisse werden gecacht (Probe öffnet echte Encoder —
/// einmal reicht). Schlug eine Probe mit `Err` fehl (transienter Treiber-/
/// Init-Fehler, GPU-Reset, Session gerade hochgefahren), wird beim nächsten
/// Aufruf neu probiert — der Sidecar bleibt warm, ein dauerhaft gecachtes
/// Fehl-Ergebnis würde HQ-Streaming sonst bis zum Prozess-Neustart abschalten.
pub fn available_video_codecs() -> Vec<&'static str> {
    static CACHE: Mutex<Option<Vec<&'static str>>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(v) = cache.as_ref() {
        return v.clone();
    }
    let (codecs, definitive) = probe_all();
    if definitive {
        *cache = Some(codecs.clone());
    } else {
        tracing::warn!(
            target: "stream",
            "Codec-Probe unvollständig — Ergebnis wird nicht gecacht (transient?)"
        );
    }
    codecs
}

/// `(codecs, definitive)` — `definitive=false`, wenn irgendein Schritt mit
/// einem echten Fehler (nicht „HW kann's nicht") endete.
fn probe_all() -> (Vec<&'static str>, bool) {
    let Some((vendor, render_node)) = drm::detect() else {
        tracing::warn!(target: "stream", "keine bekannte GPU erkannt — keine HW-Codecs gemeldet");
        return (Vec::new(), false);
    };
    let mut out = Vec::new();
    let mut definitive = true;
    for &c in CANDIDATES {
        match encode::probe_encoder(vendor, &render_node, c) {
            Ok(true) => out.push(c),
            Ok(false) => tracing::info!(
                target: "stream", codec = c, vendor = vendor.slug(),
                "HW-Encode nicht verfügbar → wird nicht angeboten"
            ),
            Err(e) => {
                definitive = false;
                tracing::warn!(
                    target: "stream", codec = c,
                    "Codec-Probe fehlgeschlagen ({e:#}) — konservativ nicht anbieten"
                );
            }
        }
    }
    tracing::info!(
        target: "stream", vendor = vendor.slug(), codecs = ?out,
        "HW-Encode-Probe abgeschlossen"
    );
    (out, definitive)
}

/// Kann diese Maschine den Pulse-Codec (h264/av1) per Hardware encodieren?
pub fn supports_codec(codec_id: &str) -> bool {
    available_video_codecs().contains(&codec_id)
}
