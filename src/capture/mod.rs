//! Capture-Quellen.
//!
//! Phase 5: `SyntheticSource` — BGRA-Testpattern, damit der `start`-Op einen
//! echten RTMPS-Stream treibt OHNE dass der Screen-Picker steht.
//! Phase 6: `portal` (xdg-desktop-portal ScreenCast) + `pipewire_stream`
//! (DMABUF-Frames) — ersetzt die synthetische Quelle durch echtes Screen-Capture
//! mit Zero-Copy-DMABUF-Handoff in den Encoder.

pub mod portal;
pub mod pipewire_stream;

use std::time::Duration;

use ffmpeg_next as ffmpeg;
use ffmpeg::format;

/// Synthetische BGRA-Quelle: wanderndes Testpattern, ein Frame pro Tick.
pub struct SyntheticSource {
    width: u32,
    height: u32,
    fps: u32,
    frame: u64,
}

impl SyntheticSource {
    pub fn new(width: u32, height: u32, fps: u32) -> Self {
        Self { width, height, fps, frame: 0 }
    }

    pub fn frame_interval(&self) -> Duration {
        Duration::from_secs_f64(1.0 / self.fps.max(1) as f64)
    }

    /// Erzeuge den nächsten BGRA-Frame ( Caller macht swscale→NV12 + Upload).
    pub fn next_bgra(&mut self) -> ffmpeg::frame::Video {
        let mut bgra = ffmpeg::frame::Video::new(format::Pixel::BGRA, self.width, self.height);
        let stride = bgra.stride(0);
        let data = bgra.data_mut(0);
        let phase = (self.frame % 60) as u8;
        for y in 0..self.height as usize {
            for x in 0..self.width as usize {
                let off = y * stride + x * 4;
                if off + 3 < data.len() {
                    data[off] = (x as u8).wrapping_add(phase);
                    data[off + 1] = (y as u8).wrapping_add(phase);
                    data[off + 2] = ((x + y) as u8).wrapping_add(phase);
                    data[off + 3] = 255;
                }
            }
        }
        bgra.set_pts(Some(self.frame as i64));
        self.frame += 1;
        bgra
    }
}
