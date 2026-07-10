//! `start` — begin a capture→encode→push stream.
//!
//! Phase 1: Stub — validiert nur die Request-Parameter und meldet dann einen
//! Klartext-Fehler, weil die Pipeline (PipeWire → VAAPI/NVENC → RTMPS) erst in
//! Phase 5 gebaut wird. Die Parameter-Auflösung ist bereits real, damit Phase 5
//! nur den StreamController-Aufruf einklinken muss.
//!
//! Löst den Request (profile + overrides + capture source + push_url) auf.
//! `channel.push_url` (von media-svc, Token drin) ist verbindlich — Pulse
//! streamt immer in einen Voice-Channel. Der Linux-Capture-Default ist `"portal"`
//! (Wayland-Portal-Dialog wählt die Quelle), wie beim Python-GSR-Sidecar.

use anyhow::{Context, Result, anyhow};
use serde_json::{Map, Value};

use crate::profiles::profile_by_name;
use crate::stream_controller::{StartParams, StreamController};

pub fn handle(params: Map<String, Value>) -> Result<Map<String, Value>> {
    let profile_name = params
        .get("profile")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("profile (Name) ist Pflicht"))?;
    let profile = profile_by_name(profile_name)
        .ok_or_else(|| anyhow!("Unknown stream profile: {profile_name}"))?;

    let channel = params
        .get("channel")
        .and_then(Value::as_object)
        .context("channel ist Pflicht (Pulse streamt immer in einen Voice-Channel)")?;
    let push_url = channel
        .get("push_url")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow!("channel.push_url ist Pflicht (media-svc reicht die rtmps://-URL durch)")
        })?;

    let overrides = params.get("overrides").and_then(Value::as_object);
    let codec = overrides
        .and_then(|o| o.get("codec"))
        .and_then(Value::as_str)
        .unwrap_or(profile.codec)
        .to_string();
    let fps = overrides
        .and_then(|o| o.get("fps"))
        .and_then(Value::as_u64)
        .unwrap_or(profile.fps as u64)
        .clamp(1, 120) as u32;
    let bitrate_kbps = overrides
        .and_then(|o| o.get("bitrate_kbps"))
        .and_then(Value::as_u64)
        .unwrap_or(profile.bitrate_kbps as u64) as u32;
    let av_offset_ms = params
        .get("av_offset_ms")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .clamp(-1000, 1000) as i32;

    let audio_obj = params.get("audio").and_then(Value::as_object);
    let audio_mode = audio_obj
        .and_then(|a| a.get("mode"))
        .and_then(Value::as_str)
        .unwrap_or("Aus");
    let enable_audio = !matches!(audio_mode, "Aus");

    // Auflösung: Override oder 1080p-Default (Phase 4+: native Display-Größe).
    let (width, height) = resolve_resolution(overrides);

    let argv = build_redacted_argv(&push_url, profile.name, &codec, fps, bitrate_kbps, width, height);

    // Phase 5: echten Stream starten. Phase 1: Fehler.
    StreamController::singleton().start(
        StartParams {
            codec,
            width,
            height,
            fps,
            bitrate_kbps,
            push_url,
            enable_audio,
            av_offset_ms,
        },
        argv.clone(),
    )?;

    let mut out = Map::new();
    out.insert(
        "argv".to_string(),
        Value::Array(argv.into_iter().map(Value::String).collect()),
    );
    Ok(out)
}

fn even(n: u32) -> u32 {
    n & !1
}

fn resolve_resolution(overrides: Option<&Map<String, Value>>) -> (u32, u32) {
    if let Some(res) = overrides
        .and_then(|o| o.get("resolution"))
        .and_then(Value::as_str)
    {
        if let Some((w, h)) = res.split_once('x') {
            if let (Ok(w), Ok(h)) = (w.trim().parse::<u32>(), h.trim().parse::<u32>()) {
                if w > 0 && h > 0 {
                    return (even(w), even(h));
                }
            }
        }
    }
    (1920, 1080)
}

fn build_redacted_argv(
    push_url: &str,
    profile_name: &str,
    codec: &str,
    fps: u32,
    bitrate_kbps: u32,
    width: u32,
    height: u32,
) -> Vec<String> {
    vec![
        "pulse-linux-hq-sidecar".to_string(),
        "--profile".to_string(),
        profile_name.to_string(),
        "--codec".to_string(),
        codec.to_string(),
        "--size".to_string(),
        format!("{width}x{height}"),
        "--fps".to_string(),
        fps.to_string(),
        "--bitrate".to_string(),
        format!("{bitrate_kbps}k"),
        "--out".to_string(),
        redact(push_url),
    ]
}

fn redact(url: &str) -> String {
    let mut s = url.to_string();
    for pat in ["pass=", "token=", "streamid=publish:"] {
        if let Some(idx) = s.find(pat) {
            let start = idx + pat.len();
            let end = s[start..]
                .find(|c: char| c == '&' || c == ' ')
                .map(|i| start + i)
                .unwrap_or(s.len());
            s.replace_range(start..end, "***");
        }
    }
    s
}
