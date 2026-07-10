# CLAUDE.md — pulse-linux-hq-sidecar

Projektanweisungen für Claude Code. Gilt für die Arbeit in diesem Repo
(`/home/michael/Dokumente/Linux_Rust_Sidecar/`).

## Was das ist
Rust-Neubau des Pulse **Linux HQ-Streaming-Sidecars**. Ersetzt den Python-`gsr-sidecar`
(im Pulse-Repo `pulse/streaming/gsr-sidecar/`), der das externe `gpu-screen-recorder`-
Binary als Subprocess spawned. Hier: **FFmpeg als Bibliothek** (wie die Windows/macOS-
Rust-Sidecars `pulse/streaming/{win,mac}-hq-sidecar/`), kein zweites Programm.

**Am Pulse-Repo (`/home/michael/Dokumente/pulse/`) wird nichts geändert** — nur dieser
Ordner. Vorbild/Vorlage ist `mac-hq-sidecar` (nächste Verwandtschaft: backendfrei +
ffmpeg-as-lib + kein Tokio im Main-Loop).

## Wire-Protokoll (heilig — nicht brechen)
stdio JSON-RPC über newline-delimited JSON, **byte-identisch** zu
`pulse/streaming/gsr-sidecar/control.py` (und win/mac). Siehe `streaming/README.md` im
Pulse-Repo für die Spec.
- Request: `{"op":"...","id":<num>?,"params"}` · Response: `{"id","ok","fields"}` (flach!)
  · Event: `{"ev":"..."}` (kein id/ok).
- Ops: `health, gpu_info, list_profiles, list_monitors, list_windows,
  list_application_audio, build_argv, start, stop, state`.
- States: `idle|starting|live|error|stopped`. Events: `state, fps, log, error, stopped`.
- Token in URLs (pass=/token=) wird in `argv`/Logs **redacted** (`***`).

Verbatim-portierte Dateien (nicht ohne Not anfassen): `proto.rs, dispatch.rs, events.rs,
main.rs, profiles.rs, encode/mux_writer.rs, ops/{list_profiles,stop,state}.rs`.

## Architektur-Entscheidungen (Nutzer-Vorgaben — einhalten)
- **System-FFmpeg** via pkg-config (Arch n8.1.2, `--enable-gnutls --enable-libdrm
  --enable-nvenc --enable-vulkan`). `ffmpeg-next = "8.1"`. Für Flatpak-Auslieferung:
  `org.freedesktop.Platform.ffmpeg`-Extension (System-FFmpeg ist GPL → **nicht bündeln**).
- **Encoder v1: VAAPI (AMD/Intel) + NVENC (Nvidia), beide Zero-Copy verbindlich.**
  Codecs **nur H264 + AV1** (kein HEVC — nicht anbieten, nicht proben, keine hevc_mux-Tests).
- **Screen-Picker (Portal/PipeWire-Capture) wird zuletzt gebaut** — zuerst Pipeline mit
  synthetischer Quelle (`capture::SyntheticSource`) zum Laufen bringen.
- **WHIP out-of-scope** (RTMPS→MediaMTX→WHEP wie win/mac).
- Encoder-Settings orientieren sich an GSR (`~/.cache/pulse/gsr/gpu-screen-recorder/src/main.cpp`):
  NVENC `tune=ll/rc=cbr/b_ref_mode=0/coder=cabac`, VAAPI `rc_mode=CBR/async_depth=3/coder=cabac`.
  GSR nutzt selbst ffmpeg-Encoder (`h264_nvenc`/`h264_vaapi`) via av_dict — Settings ~1:1.

## ffmpeg-next-Fallen (schon gelöst — nicht wieder reinlaufen)
- Enum-Varianten nur **qualifiziert** verwendbar: `AVPixelFormat::AV_PIX_FMT_CUDA`,
  `AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA` (bare-Variante via `use ffi::*` geht NICHT).
- `AVFrame.format` ist `c_int` → `pix_fmt() as i32` casten. `AVHWFramesContext.format`
  ist `AVPixelFormat` (kein Cast).
- `set_max_b_frames(usize)` (nicht `set_b_frames`, nicht Option). `set_pts(Option<i64>)`.
  `Dictionary<'static>`-Lifetime bei owned Return ohne Input-Ref.
- `Pod::from_bytes` liefert `Option<&Pod>` (Pod ist unsized) → `params: [&Pod; N]`,
  die Byte-Vecs müssen bis zum `connect`/`update_params` leben.
- `Request::response()` ist **synchrone** Methode auf `ashpd::Request` (kein `.await`).

## Build & Test
```bash
cargo build --release
echo '{"op":"health","id":1}' | ./target/release/pulse-linux-hq-sidecar
cargo run --release --example tls_probe -- rtmps://localhost:11936/test
cargo run --release --example encode_smoke -- /tmp/smoke.mp4 h264 1280 720 30 120
cargo run --release --example capture_smoke 5     # Portal-Dialog — User wählt Quelle
```
Lokales MediaMTX: `docker compose -f test/docker-compose.yml up -d` (RTMPS :11936,
API :9997, HLS :8888). Self-signed Cert: `openssl req -x509 -newkey rsa:2048 -nodes
-keyout test/certs/key.pem -out test/certs/cert.pem -days 3650 -subj "/CN=localhost"`.
**`test/certs/` ist gitignored — Private Keys niemals committen.**

## Dev-Umgebung
- **NVIDIA RTX 4090 (Ada)**, niri (Wayland), PipeWire 1.6.7. NVENC-Pfad live testbar
  (H264+AV1). **VAAPI-Pfad nicht runtime-testbar** hier (keine AMD/Intel-dGPU) — nur per
  Analogie implementiert.
- xdg-desktop-Portal ScreenCast nutzt hier den **GNOME-Backend** (niri implementiert
  `org.gnome.Mutter.ScreenCast`); konfiguriert in `~/.config/xdg-desktop-portal/portals.conf`.
- libclang liegt als `/usr/lib/libclang.so` (kein pkg-config-File, aber bindgen findet es).
- Ports: 1936 ist durch `passt` belegt → RTMPS läuft auf **11936**; WebRTC-ICE-UDP auf 18189.

## Task 6 — Stand
Ex-Blocker **gelöst** (Commit `6277015`): Die SPA-Format-Verhandlung brauchte explizite
DRM-Modifier. `src/capture/egl_modifiers.rs` fragt sie via `eglQueryDmaBufModifiersEXT`
ab (dlopen libEGL, Device-Plattform), `pipewire_stream.rs` bietet sie als Choice-Enum
(`MANDATORY|DONT_FIXATE`) an und macht den Fixierungs-Tanz aus der PipeWire-DMABUF-Doku.
**Falle:** SPA stellt fixierte Werte als `Choice None` dar — wer nur `is_choice()` prüft,
re-announced endlos. Live verifiziert: BGRx 1920×1080, NVIDIA-Modifier
`0x0300000000606010`, DMABUF-Frames fließen (`capture_smoke`).
SPA_PARAM-ids: EnumFormat=3, Format=4, Buffers=5, Meta=6, Latency=15, Tag=17, Props=2.
**Als Nächstes:** Zero-Copy-Import DMABUF→Encoder (NVENC: EGLImage/CUDA-Interop wegen
Block-Linear-Modifier; VAAPI: `av_hwframe_map` DRM_PRIME), dann Audio (Opus, 2-Stream-
FLV), `SyntheticSource` im StreamController ersetzen, `test_driver`-Example.

## Memory / Plan
- Projekt-Memory: `~/.claude/projects/-home-michael-Dokumente-Linux-Rust-Sidecar/memory/`
  (`linux-rust-sidecar-rebuild.md` — vollständiger Stand/Phasen/Fällen).
- Plan: `~/.claude/plans/shiny-meandering-tide.md`.
