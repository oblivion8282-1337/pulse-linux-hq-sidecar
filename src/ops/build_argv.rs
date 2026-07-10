//! `build_argv` — baut die diagnostische Argumentliste OHNE zu starten.
//!
//! Wie bei den anderen Sidecars rein diagnostisch: der Renderer zeigt die
//! argv in einem Stats-Panel. Die echte Pipeline (Phase 5) treibt FFmpeg per
//! API-Aufrufen, nicht indem sie diese argv exec't — sie ist eine
//! *repräsentative* argv, kein literal command line. Token-redacted.
//!
//! Shape: `{ok, binary, argv}`.

use anyhow::{Result, anyhow};
use serde_json::{Map, Value};

use crate::profiles::profile_by_name;

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
        .ok_or_else(|| anyhow!("channel ist Pflicht"))?;
    let push_url = channel
        .get("push_url")
        .and_then(Value::as_str)
        .unwrap_or("");

    let overrides = params.get("overrides").and_then(Value::as_object);
    let codec = overrides
        .and_then(|o| o.get("codec"))
        .and_then(Value::as_str)
        .unwrap_or(profile.codec);
    let fps = overrides
        .and_then(|o| o.get("fps"))
        .and_then(Value::as_u64)
        .unwrap_or(profile.fps as u64);
    let bitrate_kbps = overrides
        .and_then(|o| o.get("bitrate_kbps"))
        .and_then(Value::as_u64)
        .unwrap_or(profile.bitrate_kbps as u64);
    let resolution = overrides
        .and_then(|o| o.get("resolution"))
        .and_then(Value::as_str)
        .unwrap_or("Native");
    let capture = params.get("capture").and_then(Value::as_str).unwrap_or("portal");
    let audio_mode = params
        .get("audio")
        .and_then(|a| a.get("mode"))
        .and_then(Value::as_str)
        .unwrap_or("Aus");

    let argv = build_argv(profile.name, codec, fps as u32, bitrate_kbps as u32, resolution, capture, audio_mode, push_url);

    let mut out = Map::new();
    out.insert("binary".to_string(), Value::String("pulse-linux-hq-sidecar".to_string()));
    out.insert(
        "argv".to_string(),
        Value::Array(argv.into_iter().map(Value::String).collect()),
    );
    Ok(out)
}

fn build_argv(
    profile: &str,
    codec: &str,
    fps: u32,
    bitrate_kbps: u32,
    resolution: &str,
    capture: &str,
    audio_mode: &str,
    push_url: &str,
) -> Vec<String> {
    let argv = vec![
        "pulse-linux-hq-sidecar".to_string(),
        "--profile".to_string(),
        profile.to_string(),
        "--capture".to_string(),
        capture.to_string(),
        "--codec".to_string(),
        codec.to_string(),
        "--fps".to_string(),
        fps.to_string(),
        "--bitrate".to_string(),
        format!("{bitrate_kbps}k"),
        "--audio".to_string(),
        audio_mode.to_string(),
        "--resolution".to_string(),
        resolution.to_string(),
        "--out".to_string(),
        redact(push_url),
    ];
    argv
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
