//! System-Introspection: TLS-Backend des gelinkten FFmpeg, DRM-Vendor,
//! VAAPI/NVENC-Codec-Probe.
//!
//! Phase 2: `tls` (avformat_configuration → GnuTLS/OpenSSL/…). Phase 3: `drm`
//! (sysfs-Vendor-Erkennung). Codec-Open-Probe kommt mit den HW-Modulen (Phase 4).

pub mod drm;
pub mod tls;
