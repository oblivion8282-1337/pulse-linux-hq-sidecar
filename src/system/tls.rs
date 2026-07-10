//! TLS-Backend des gelinkten FFmpeg — de-riskt die `tls_verify=0`-Frage.
//!
//! Pulse's MediaMTX nutzt self-signed certs (Token in der URL ist die echte
//! Auth, TLS nur obfuscation). FFmpeg honoriert `tls_verify=0` für RTMPS, aber
//! das Verhalten hängt vom TLS-Backend ab: macOS SecureTransport blockiert
//! Bulk-Writes nach dem Handshake (deshalb baut mac-hq-sidecar ein eigenes
//! OpenSSL-FFmpeg); GnuTLS/OpenSSL/mbedtls auf Linux honoren `tls_verify=0`
//! sauber. Wir lesen `avformat_configuration()` (der `./configure`-String) und
//! substring-matchen das `--enable-<backend>`.

use std::ffi::CStr;

use ffmpeg_next as ffmpeg;

/// Welches TLS-Backend das gelinkte libavformat nutzt, oder `None` wenn keines.
///
/// Wird im `health`-Report als `gsr.tls_backend` exponiert — der Renderer /
/// Operator sieht so, ob RTMPS-push mit self-signed certs funktionieren wird.
pub fn detect() -> Option<&'static str> {
    let _ = ffmpeg::init();
    // SAFETY: avformat_configuration() gibt einen statischen C-String zurück
    // (Lebensdauer = Prozess). Kein Ownership-Transfer.
    let cfg_ptr = unsafe { ffmpeg::ffi::avformat_configuration() };
    if cfg_ptr.is_null() {
        return None;
    }
    let cfg = unsafe { CStr::from_ptr(cfg_ptr) }.to_string_lossy();
    if cfg.contains("--enable-gnutls") {
        Some("gnutls")
    } else if cfg.contains("--enable-openssl") {
        Some("openssl")
    } else if cfg.contains("--enable-libtls") {
        Some("libtls")
    } else if cfg.contains("--enable-mbedtls") {
        Some("mbedtls")
    } else if cfg.contains("--enable-securetransport") {
        Some("securetransport")
    } else {
        None
    }
}
