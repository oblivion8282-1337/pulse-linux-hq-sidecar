//! Encoder-Fähigkeits-Probe — welche Video-Codecs DIESE Maschine per Hardware
//! encodieren kann (VAAPI für AMD/Intel, NVENC für Nvidia), über das gelinkte
//! FFmpeg.
//!
//! Drives `list_profiles` (der Renderer zeigt nur Codecs die die HW kann) und
//! den `health`-Report. Gate nach *Fähigkeit*, nie nach Modellname.
//!
//! Phase 1 (diese Datei): statisch — H264 + AV1 werden als verfügbar gemeldet,
//! damit `list_profiles` funktioniert sobald das Protokoll steht. Phase 3 ersetzt
//! `available_video_codecs` durch eine echte Probe (system::drm für den Vendor,
//! system::va_probe für VAAPI-Codecs, open-probe für NVENC). HEVC wird auf Linux
//! nicht angeboten (Nutzerentscheidung: nur H264 + AV1).

/// Hardware-encodierbare Video-Codecs auf dieser Maschine, in Präferenzordnung.
///
/// Phase 1: statisch `["h264","av1"]`. Phase 3: echte VAAPI/NVENC-Probe.
pub fn available_video_codecs() -> &'static [&'static str] {
    // TODO(phase3): ersetzen durch echte Probe:
    //   vendor = system::drm::detect_vendor()
    //   VAAPI (amd/intel) → system::va_probe::codecs(render_node)
    //   NVENC (nvidia)    → open-probe je codec (Turing+ für AV1)
    &["h264", "av1"]
}

/// Kann diese Maschine den Pulse-Codec (h264/av1) per Hardware encodieren?
pub fn supports_codec(codec_id: &str) -> bool {
    available_video_codecs().contains(&codec_id)
}
