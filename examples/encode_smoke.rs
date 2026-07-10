//! Encode-Smoke: synthetische Testpattern-Frames → HW-Encoder (NVENC/VAAPI) → Datei.
//!
//! Validiert die Encoder-Konfig + HW-Context + Upload-Pfad OHNE Screen-Capture
//! (Picker kommt in Phase 6). Generiert BGRA-Testpattern, swscale→NV12, upload
//! per `av_hwframe_transfer_data` in den CUDA/VAAPI-Pool, encodiert H264/AV1.
//!
//! ```text
//! cargo run --release --example encode_smoke -- /tmp/pulse_smoke.mp4
//! cargo run --release --example encode_smoke -- /tmp/pulse_smoke.mp4 av1 1280 720 30 120
//! ffprobe /tmp/pulse_smoke.mp4
//! ```

use ffmpeg_next as ffmpeg;
use ffmpeg::{format, ffi::*};

use pulse_linux_hq_sidecar::encode::{EncoderConfig, VideoEncoder, hw};
use pulse_linux_hq_sidecar::system::drm;

fn main() -> anyhow::Result<()> {
    let _ = ffmpeg::init();

    let out = std::env::args().nth(1).unwrap_or_else(|| "/tmp/pulse_smoke.mp4".to_string());
    let codec = std::env::args().nth(2).unwrap_or_else(|| "h264".to_string());
    let width: u32 = std::env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(1280);
    let height: u32 = std::env::args().nth(4).and_then(|s| s.parse().ok()).unwrap_or(720);
    let fps: u32 = std::env::args().nth(5).and_then(|s| s.parse().ok()).unwrap_or(30);
    let n_frames: u64 = std::env::args().nth(6).and_then(|s| s.parse().ok()).unwrap_or(120);

    let (vendor, render_node) = drm::detect()
        .ok_or_else(|| anyhow::anyhow!("keine DRM-Render-Node gefunden (keine NVIDIA/AMD/Intel-GPU?)"))?;
    eprintln!("[encode_smoke] vendor={:?} render_node={:?} codec={codec} {width}x{height}@{fps}fps → {out}", vendor.slug(), render_node);

    let kind = hw::kind_for(vendor);
    let dev_arg = if matches!(kind, hw::HwDeviceKind::Vaapi) {
        Some(render_node.as_str())
    } else {
        None // CUDA nimmt Device 0
    };
    let hw_ctx = hw::HwContext::create(kind, dev_arg, width, height)?;
    eprintln!("[encode_smoke] HW-Context ({:?}) angelegt", kind);

    let cfg = EncoderConfig {
        vendor,
        codec: codec.clone(),
        fps,
        bitrate_kbps: 4000,
        width,
        height,
    };
    let mut enc = VideoEncoder::create(&cfg, &hw_ctx, &out)?;
    eprintln!("[encode_smoke] Encoder offen, encodiere {n_frames} Frames …");

    // swscale BGRA→NV12 (CPU) — einmaliger Scaler.
    let mut scaler = ffmpeg::software::scaling::Context::get(
        format::Pixel::BGRA,
        width,
        height,
        format::Pixel::NV12,
        width,
        height,
        ffmpeg::software::scaling::Flags::BILINEAR,
    )?;

    let started = std::time::Instant::now();
    // Bei Live-Output (rtmp/srt) echtzeit-pacen: ein Frame pro 1/fps-Intervall,
    // sonst pusht NVENC 600 Frames in 0,2s und MediaMTX sieht nur einen kurzen
    // Burst. Für Datei-Output kein Pacing (schnellst mögliches Encodieren).
    let live = out.starts_with("rtmp") || out.starts_with("srt");
    let frame_interval = std::time::Duration::from_secs_f64(1.0 / fps.max(1) as f64);
    let mut next_emit = std::time::Instant::now();

    for i in 0..n_frames {
        // BGRA-Testpattern: wandernder Farbverlauf pro Frame.
        let mut bgra = ffmpeg::frame::Video::new(format::Pixel::BGRA, width, height);
        let stride = bgra.stride(0);
        let data = bgra.data_mut(0);
        let phase = (i % 60) as u8;
        for y in 0..height as usize {
            for x in 0..width as usize {
                let off = y * stride + x * 4;
                if off + 3 < data.len() {
                    data[off] = (x as u8).wrapping_add(phase);
                    data[off + 1] = (y as u8).wrapping_add(phase);
                    data[off + 2] = ((x + y) as u8).wrapping_add(phase);
                    data[off + 3] = 255;
                }
            }
        }
        bgra.set_pts(Some(i as i64));

        let mut nv12 = ffmpeg::frame::Video::empty();
        scaler.run(&bgra, &mut nv12)?;
        nv12.set_pts(Some(i as i64));

        let mut hw_frame = hw_ctx.upload_swframe(&nv12, i as i64)?;
        enc.send_hw(hw_frame, i as i64)?;
        unsafe { av_frame_free(&mut hw_frame) }; // freigeben nach send

        if live {
            next_emit += frame_interval;
            let now = std::time::Instant::now();
            if next_emit > now {
                std::thread::sleep(next_emit - now);
            } else {
                next_emit = now; // hinterher → resync, nicht spiralen
            }
        }
    }
    eprintln!("[encode_smoke] encodiert in {:.2}s, finalisiere …", started.elapsed().as_secs_f64());

    enc.finish()?;
    eprintln!("[encode_smoke] ✅ fertig: {out}");
    eprintln!("[encode_smoke] prüfen: ffprobe -v error -show_streams {out}");
    Ok(())
}
