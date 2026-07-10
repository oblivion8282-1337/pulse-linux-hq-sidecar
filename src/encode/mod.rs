//! Encode-Pipeline: HW-Encoder (NVENC/VAAPI) → Muxer → Output (Datei oder RTMPS).
//!
//! Adaptiert von `win-hq-sidecar/src/encode/encoder_hw.rs` (`FfmpegHwEncoder`):
//! ffmpeg-next High-Level-API für Output/Stream/Encoder/Packet, rohes FFI nur
//! für `hw_frames_ctx` am `AVCodecContext` und `avcodec_send_frame`. Statt des
//! Windows-D3D11-Capture-Pools kommt hier der eigene [`hw::HwContext`] (CUDA für
//! Nvidia, VAAPI für AMD/Intel) zum Zug.
//!
//! Phase 4 (diese Datei): Video-only, synthetische Frames → Datei. Audio + der
//! asynchrone Pacing-Loop + RTMPS-Push kommen in Phase 5.

pub mod hw;
pub mod mux_writer;
pub mod opts;

use anyhow::{Context, Result, anyhow};
use ffmpeg_next as ffmpeg;
use ffmpeg::{Dictionary, Packet, Rational, codec, format, ffi::*};

use hw::HwContext;
use mux_writer::MuxWriter;
use crate::system::drm::Vendor;

#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub vendor: Vendor,
    pub codec: String, // "h264" | "av1"
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub width: u32,
    pub height: u32,
}

pub struct VideoEncoder {
    mux: MuxWriter,
    encoder: codec::encoder::Video,
    video_stream_idx: usize,
    encoder_time_base: Rational,
    stream_time_base: Rational,
}

impl VideoEncoder {
    /// Öffne Output + Encoder mit dem gegebenen HW-Context. `output_path` ist
    /// eine Datei (Phase 4) oder eine `rtmp(s)://`/`srt://`-URL (Phase 5).
    /// `write_header` wird hier gerufen; danach geht jeder `write_interleaved`
    /// asynchron über den MuxWriter-Thread.
    pub fn create(cfg: &EncoderConfig, hw: &HwContext, output_path: &str) -> Result<Self> {
        ffmpeg::init().context("ffmpeg::init")?;

        let mut output = match url_format_hint(output_path) {
            Some(fmt) => {
                let mut o = Dictionary::new();
                o.set("rw_timeout", "10000000"); // 10s — sonst blockt ein toter Socket ewig
                if output_path.to_ascii_lowercase().starts_with("rtmps://") {
                    o.set("tls_verify", "0"); // self-signed MediaMTX (GnuTLS honoriert das)
                }
                format::output_as_with(output_path, fmt, o)
                    .with_context(|| format!("format::output_as_with({output_path}, {fmt})"))?
            }
            None => format::output(output_path)
                .with_context(|| format!("format::output({output_path})"))?,
        };

        let codec_name = opts::encoder_name(cfg.vendor, &cfg.codec)
            .ok_or_else(|| anyhow!("kein Encoder für vendor={:?} codec={}", cfg.vendor, cfg.codec))?;
        let codec_descriptor = codec::encoder::find_by_name(codec_name)
            .ok_or_else(|| anyhow!("encoder '{codec_name}' nicht im gelinkten FFmpeg registriert"))?;

        let global_header = output.format().flags().contains(format::Flags::GLOBAL_HEADER);

        let mut stream = output.add_stream(codec_descriptor).context("add_stream")?;
        let stream_idx = stream.index();

        let mut encoder = codec::context::Context::new_with_codec(codec_descriptor)
            .encoder()
            .video()?;
        encoder.set_width(cfg.width);
        encoder.set_height(cfg.height);
        encoder.set_format(hw.ffmpeg_pixel());
        encoder.set_time_base(Rational::new(1, cfg.fps as i32));
        encoder.set_frame_rate(Some(Rational::new(cfg.fps as i32, 1)));
        encoder.set_bit_rate((cfg.bitrate_kbps as usize).saturating_mul(1000));
        encoder.set_max_bit_rate((cfg.bitrate_kbps as usize).saturating_mul(1000));
        encoder.set_gop(cfg.fps.saturating_mul(2)); // keyint=2.0s (GSR)
        // Low-Latency: kein B-Frame (GSR Performance-Tune).
        encoder.set_max_b_frames(0);
        if global_header {
            encoder.set_flags(codec::Flags::GLOBAL_HEADER);
        }

        // hw_frames_ctx VOR open an die AVCodecContext hängen (ffmpeg-next
        // exponiert das Feld nicht → `as_mut_ptr`). NVENC/VAAPI brauchen den
        // Frames-Pool als Input-Quelle.
        unsafe {
            let ctx_ptr = encoder.as_mut_ptr();
            let new_ref = av_buffer_ref(hw.frames_ref());
            if new_ref.is_null() {
                return Err(anyhow!("av_buffer_ref(hw_frames_ref) returned NULL"));
            }
            (*ctx_ptr).hw_frames_ctx = new_ref;
        }

        let o = opts::vendor_opts(cfg.vendor);
        let opened = encoder
            .open_with(o)
            .with_context(|| format!("open hw encoder '{codec_name}' (vendor={:?})", cfg.vendor))?;
        stream.set_parameters(&opened);

        output.write_header().context("write_header")?;

        let stream_time_base = output.stream(stream_idx).unwrap().time_base();
        let encoder_time_base = Rational::new(1, cfg.fps as i32);

        let mux = MuxWriter::start(output).context("start mux-writer")?;

        Ok(Self {
            mux,
            encoder: opened,
            video_stream_idx: stream_idx,
            encoder_time_base,
            stream_time_base,
        })
    }

    /// Schicke einen HW-Frame (CUDA/VAAPI, `*mut AVFrame`) in den Encoder.
    /// `pts` in Encoder-Timebase (1/fps), strikt monoton.
    pub fn send_hw(&mut self, frame: *mut AVFrame, pts: i64) -> Result<()> {
        unsafe {
            (*frame).pts = pts;
            let ret = avcodec_send_frame(self.encoder.as_mut_ptr(), frame);
            if ret < 0 {
                return Err(anyhow!("avcodec_send_frame failed (rc={ret})"));
            }
        }
        self.drain_video()
    }

    fn drain_video(&mut self) -> Result<()> {
        loop {
            let mut packet = Packet::empty();
            match self.encoder.receive_packet(&mut packet) {
                Ok(()) => {}
                Err(ffmpeg::Error::Eof) => break,
                Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::error::EAGAIN => break,
                Err(e) => return Err(e.into()),
            }
            packet.set_stream(self.video_stream_idx);
            packet.rescale_ts(self.encoder_time_base, self.stream_time_base);
            self.mux.send(packet)?;
        }
        Ok(())
    }

    /// Finalisieren: EOF an Video, restliche Pakete, MuxWriter-Flush (schreibt
    /// den Trailer / sauberen RTMP-Close).
    pub fn finish(&mut self) -> Result<()> {
        self.encoder.send_eof().context("video send_eof")?;
        self.drain_video()?;
        self.mux.finish()
    }
}

/// Output-Format-Hint nach URL-Schema: rtmp(s)→flv, srt→mpegts, sonst None
/// (Datei → auto-detect anhand Erweiterung). Wie mac/win.
pub fn url_format_hint(url: &str) -> Option<&'static str> {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("rtmp://") || lower.starts_with("rtmps://") {
        Some("flv")
    } else if lower.starts_with("srt://") {
        Some("mpegts")
    } else {
        None
    }
}
