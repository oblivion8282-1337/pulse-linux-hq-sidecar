//! Stream-/server-profile + audio-mode table.
//!
//! Wire-compatible with `streaming/gsr-sidecar/profiles.py` and
//! `streaming/{win,mac}-hq-sidecar/src/profiles.rs` — the `list_profiles` response
//! (names, codec/audio/container/bitrate/fps values, `needs_custom_build`,
//! notes) has the exact same shape and the exact same strings on all platforms,
//! so the renderer (`web/src/lib/stream/`) is platform-blind.
//!
//! `ServerProfile::from_channel` builds the push URL for the Pulse channel path
//! — if media-svc already passed a `push_url` (token inside), it's used
//! verbatim; otherwise we reconstruct it in the same form as on Linux:
//!
//! ```text
//! RTMP: rtmp://<host>:1935/channel-<id>?user=<user>&pass=<token>
//! SRT:  srt://<host>:8890?streamid=publish:channel-<id>:<user>:<token>&pkt_size=1316
//! ```

use serde::Serialize;

/// Codec/bitrate/fps/container preset. Wire-compatible with `StreamProfile`
/// from `gsr-sidecar/profiles.py`.
#[derive(Debug, Clone, Serialize)]
pub struct StreamProfile {
    pub name: &'static str,
    pub codec: &'static str,
    pub audio_codec: &'static str,
    pub container: &'static str,
    pub bitrate_kbps: u32,
    pub fps: u32,
    pub needs_custom_build: bool,
    pub notes: &'static str,
}

/// Push target — either verbatim from media-svc (`push_url`) or reconstructed
/// from `mediamtx_endpoint` + `push_protocol` + `token`.
#[derive(Debug, Clone)]
#[allow(dead_code)] // fields read by build_argv / the future start pipeline.
pub struct ServerProfile {
    pub name: String,
    pub push_protocol: String,
    pub push_host: String,
    pub push_port: u16,
    pub push_path: String,
    pub auth_user: String,
    pub push_url: Option<String>,
}

#[allow(dead_code)]
impl ServerProfile {
    /// Pulse channel path — mirror of `ServerProfile.from_channel` in
    /// `profiles.py`. A `push_url` from media-svc is authoritative.
    pub fn from_channel(
        channel_id: &str,
        token: &str,
        mediamtx_endpoint: &str,
        push_protocol: &str,
        push_url: Option<String>,
    ) -> Self {
        let (host, endpoint_port) = parse_endpoint(mediamtx_endpoint);
        let default_port: u16 = if push_protocol == "rtmp" { 1935 } else { 8890 };
        let push_port = endpoint_port.unwrap_or(default_port);

        let auth_user = if token.is_empty() {
            "publisher".to_string()
        } else {
            token.chars().take(16).collect::<String>()
        };

        let channel_path = format!("channel-{channel_id}");

        Self {
            name: channel_path.clone(),
            push_protocol: push_protocol.to_string(),
            push_host: host,
            push_port,
            push_path: channel_path,
            auth_user,
            push_url,
        }
    }
}

/// `host` or `host:port` → (host, port?). Ignores the IPv6 bracket form
/// (`[::]:1935`) just like the Python variant.
fn parse_endpoint(endpoint: &str) -> (String, Option<u16>) {
    if endpoint.starts_with('[') {
        return (endpoint.to_string(), None);
    }
    match endpoint.split_once(':') {
        Some((host, port_str)) => match port_str.parse::<u16>() {
            Ok(port) => (host.to_string(), Some(port)),
            Err(_) => (endpoint.to_string(), None),
        },
        None => (endpoint.to_string(), None),
    }
}

// ── Static profile table ─────────────────────────────────────────────────────
//
// 1:1 from `gsr-sidecar/profiles.py`. Names + notes in German like the original,
// so the settings modal finds identical strings on every platform.
// Nur H264 + AV1 (HEVC wird auf Linux nicht angeboten — Nutzerentscheidung).

pub const PROFILES: &[StreamProfile] = &[
    StreamProfile {
        name: "AV1 Effizient",
        codec: "av1",
        audio_codec: "opus",
        container: "flv",
        bitrate_kbps: 4000,
        fps: 60,
        needs_custom_build: true,
        notes: "Halbe Bandbreite, gleiche Qualität. Browser muss AV1 können.",
    },
    StreamProfile {
        name: "H.264 Standard",
        codec: "h264",
        audio_codec: "opus",
        container: "flv",
        bitrate_kbps: 4000,
        fps: 60,
        needs_custom_build: true,
        notes: "Universelle Browser-Kompat, Audio in WebRTC.",
    },
    StreamProfile {
        name: "H.264 Sparmodus",
        codec: "h264",
        audio_codec: "opus",
        container: "flv",
        bitrate_kbps: 4000,
        fps: 60,
        needs_custom_build: true,
        notes: "Halbe Bandbreite, leicht pixeliger bei Bewegung.",
    },
    StreamProfile {
        name: "Custom",
        codec: "h264",
        audio_codec: "opus",
        container: "flv",
        bitrate_kbps: 4000,
        fps: 60,
        needs_custom_build: true,
        notes: "Override-Sektion in der UI nutzen.",
    },
];

#[allow(dead_code)]
pub fn profile_by_name(name: &str) -> Option<&'static StreamProfile> {
    PROFILES.iter().find(|p| p.name == name)
}

/// Audio modes with the labels the renderer shows in the settings modal. The
/// values map to PipeWire-Quellen (Desktop-Loopback / Mikrofon / App-Node) —
/// übersetzt im Capture-Stage.
pub const AUDIO_MODES: &[&str] = &["Aus", "Desktop", "Mikrofon", "Desktop + Mikrofon"];

pub const APP_LABEL_PREFIX: &str = "App: ";
