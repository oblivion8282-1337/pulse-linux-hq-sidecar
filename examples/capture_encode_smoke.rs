//! Capture-Encode-Smoke: Portal → PipeWire-DMABUF → Zero-Copy-CUDA-Import →
//! NVENC → Datei. Der volle Screen-Capture-Encode-Pfad ohne CPU-Roundtrip.
//!
//! ```text
//! cargo run --release --example capture_encode_smoke -- /tmp/pulse_capture.mp4 [codec] [fps] [frames]
//! ffprobe -v error -show_streams /tmp/pulse_capture.mp4
//! ```
//! Portal-Dialog: User wählt Quelle (Abbruch → Exit 60). NVIDIA-only —
//! der VAAPI-Import-Pfad (av_hwframe_map DRM_PRIME) folgt separat.
//!
//! Hinweis: Compositors schicken Frames nur bei Damage — auf statischem
//! Schirm ggf. Fenster bewegen, sonst läuft der recv-Timeout ab.

use std::time::Duration;

use ffmpeg_next as ffmpeg;
use ffmpeg::ffi::{AVPixelFormat, av_frame_free};

use pulse_linux_hq_sidecar::capture::{pipewire_stream::PipewireCapture, portal};
use pulse_linux_hq_sidecar::encode::{EncoderConfig, VideoEncoder, hw, nv_import::NvDmabufImporter};
use pulse_linux_hq_sidecar::system::drm;

fn main() -> anyhow::Result<()> {
    let _ = ffmpeg::init();

    let out = std::env::args().nth(1).unwrap_or_else(|| "/tmp/pulse_capture.mp4".to_string());
    let codec = std::env::args().nth(2).unwrap_or_else(|| "h264".to_string());
    let fps: u32 = std::env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(60);
    let n_frames: u64 = std::env::args().nth(4).and_then(|s| s.parse().ok()).unwrap_or(300);

    let (vendor, _render_node) = drm::detect()
        .ok_or_else(|| anyhow::anyhow!("keine DRM-Render-Node gefunden"))?;
    if !matches!(vendor, drm::Vendor::Nvidia) {
        anyhow::bail!(
            "vendor={:?} — dieses Smoke testet den NVENC-Import; VAAPI-Import folgt separat",
            vendor.slug()
        );
    }

    // Portal → PipeWire-DMABUF-Frames.
    let session = match portal::open(true) {
        Ok(s) => s,
        Err(e) if portal::is_portal_canceled(&e) => {
            eprintln!("[capture_encode] Portal abgebrochen → Exit 60");
            std::process::exit(portal::EXIT_PORTAL_CANCELED);
        }
        Err(e) => return Err(e),
    };
    eprintln!(
        "[capture_encode] portal: node={} {}x{}",
        session.node_id, session.width, session.height
    );
    let (rx, mut cap) =
        PipewireCapture::start(session.pw_fd, session.node_id, session.width, session.height)?;

    // Erster Frame bestimmt die realen (negotiierten) Maße.
    let first = rx
        .recv_timeout(Duration::from_secs(15))
        .map_err(|_| anyhow::anyhow!("kein DMABUF-Frame in 15s (Dialog? Damage? — Fenster bewegen)"))?;
    let (w, h) = (first.width, first.height);
    eprintln!(
        "[capture_encode] erster Frame: {}x{} fourcc={:#010x} modifier={:#018x} → encodiere {n_frames} Frames {codec}@{fps}fps nach {out}",
        w, h, first.drm_fourcc, first.modifier
    );

    // CUDA-Pool mit sw_format BGR0 (NVENC nimmt RGB direkt; Primary-Context
    // teilt sich FFmpeg mit dem Importer).
    let hw_ctx = hw::HwContext::create(
        hw::HwDeviceKind::Cuda,
        None,
        w,
        h,
        AVPixelFormat::AV_PIX_FMT_RGB0,
    )?;
    let cfg = EncoderConfig {
        vendor,
        codec: codec.clone(),
        fps,
        bitrate_kbps: 8000,
        width: w,
        height: h,
    };
    let mut enc = VideoEncoder::create(&cfg, &hw_ctx, &out)?;
    let mut importer = NvDmabufImporter::new(w, h)?;
    eprintln!("[capture_encode] Encoder + Importer bereit");

    let started = std::time::Instant::now();
    let mut frame = first;
    let mut sent: u64 = 0;
    loop {
        let mut hw_frame = importer.import(&frame, &hw_ctx)?;
        // fds gehören uns — nach dem Import schließen.
        for p in &frame.planes {
            unsafe { libc::close(p.fd) };
        }
        enc.send_hw(hw_frame, sent as i64)?;
        unsafe { av_frame_free(&mut hw_frame) };
        sent += 1;
        if sent % 60 == 0 {
            eprintln!("[capture_encode] {sent}/{n_frames} frames …");
        }
        if sent >= n_frames {
            break;
        }
        frame = match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(f) => f,
            Err(_) => {
                eprintln!("[capture_encode] 5s ohne Frame (kein Damage?) — beende mit {sent} Frames");
                break;
            }
        };
    }
    let elapsed = started.elapsed().as_secs_f64();
    eprintln!(
        "[capture_encode] {sent} Frames in {elapsed:.2}s ({:.1} fps), finalisiere …",
        sent as f64 / elapsed.max(0.001)
    );

    cap.stop();
    enc.finish()?;
    eprintln!("[capture_encode] ✅ fertig: {out}");
    eprintln!("[capture_encode] prüfen: ffprobe -v error -show_streams {out}");
    Ok(())
}
