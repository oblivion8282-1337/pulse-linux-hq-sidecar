# pulse-linux-hq-sidecar

Rust-Neubau des Pulse **Linux HQ-Streaming-Sidecars** — ersetzt den Python-`gsr-sidecar`
(`pulse/streaming/gsr-sidecar/`), der das externe `gpu-screen-recorder`-Binary spawned.
Wie die Windows/macOS-Rust-Sidecars: **FFmpeg als Bibliothek** (kein zweites Programm),
gleiches stdio-JSON-RPC-Protokoll wie `gsr-sidecar/control.py`.

## Stack
- **Capture** (Phase 6, in Arbeit): xdg-desktop-portal ScreenCast → PipeWire-DMABUF.
- **Encode**: VAAPI (AMD/Intel) / NVENC (Nvidia) via `ffmpeg-next` 8.1 (System-FFmpeg,
  pkg-config). Codecs: **nur H264 + AV1** (kein HEVC). Encoder-Optionen orientieren sich
  an GSR (`tune=ll`/`rc=cbr`/`b_ref_mode=0` für NVENC; `rc_mode=CBR`/`async_depth=3` für VAAPI).
- **Push**: FLV-Mux → RTMPS an MediaMTX (`tls_verify=0`, GnuTLS-Backend im System-FFmpeg —
  kein Custom-Build nötig, anders als bei macOS). Viewer holen per WHEP.
- **Threading**: `std::thread` + `mpsc`, kein Tokio im Main-Loop (nur scoped für die
  Portal-Verhandlung via `ashpd`).

## Stand
- ✅ Protokoll-Skelett (wire-identisch zu `control.py`): health/gpu_info/state/stop/
  build_argv/start.
- ✅ TLS-De-Risk: `tls_backend=gnutls`, RTMPS-Connect mit `tls_verify=0` gegen self-signed
  MediaMTX-Cert funktioniert (`examples/tls_probe.rs`).
- ✅ DRM-Vendor-Erkennung (sysfs: nvidia/amd/intel + `/dev/dri/renderDXXX`).
- ✅ NVENC-Encode (H264 + AV1) → Datei + RTMPS-Push, via HLS lesbar verifiziert
  (`examples/encode_smoke.rs`).
- ✅ `start`-Op treibt über JSON-RPC einen echten NVENC→RTMPS-Stream (synthetische Quelle).
- ✅ Portal-Capture: Dialog → PipeWire-`node_id`+`fd`+`restore_token`
  (`examples/capture_smoke.rs`).
- ⏳ PipeWire-DMABUF-Consumer: Format-Verhandlung blockiert (vermutlich fehlender
  DMA-BUF-Modifier im EnumFormat — s. Code-Kommentare in `src/capture/pipewire_stream.rs`).
- ⏳ Zero-Copy-Handoff (NVENC `cuImportExternalMemory` / VAAPI `av_hwframe_map`), Audio
  (Opus, 2-Stream-FLV), `test_driver`-Example.

## Lokales Test-MediaMTX
```bash
docker compose -f test/docker-compose.yml up -d   # RTMPS :11936, API :9997
# Self-signed Cert erzeugen:
mkdir -p test/certs && openssl req -x509 -newkey rsa:2048 -nodes \
  -keyout test/certs/key.pem -out test/certs/cert.pem -days 3650 -subj "/CN=localhost"
```

## Build
```bash
cargo build --release
echo '{"op":"health","id":1}' | ./target/release/pulse-linux-hq-sidecar
cargo run --release --example tls_probe -- rtmps://localhost:11936/test
cargo run --release --example encode_smoke -- /tmp/smoke.mp4 h264 1280 720 30 120
cargo run --release --example capture_smoke 5   # öffnet den Portal-Dialog
```

System-Voraussetzungen: FFmpeg 8.1 (`--enable-gnutls --enable-libdrm --enable-nvenc`),
libpipewire-0.3, libclang (für ffmpeg-sys bindgen).
