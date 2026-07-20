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

pub mod audio;
pub mod hw;
pub mod mux_writer;
pub mod nv_import;
pub mod opts;
pub mod va_import;

use anyhow::{Context, Result, anyhow};
use ffmpeg_next as ffmpeg;
use ffmpeg::{Dictionary, Packet, Rational, codec, format, ffi::*};

use audio::AudioEncoder;
use hw::HwContext;
use mux_writer::{MuxSender, MuxWriter};
use crate::redact::redact_url;
use crate::system::drm::Vendor;

/// Optionale Audio-Konfiguration für [`VideoEncoder::create_with_audio`].
#[derive(Debug, Clone)]
pub struct AudioParams {
    pub sample_rate: u32,
    pub bitrate_kbps: u32,
}

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
        let (enc, _no_audio) =
            Self::create_with_audio(cfg, hw.ffmpeg_pixel(), hw.frames_ref(), output_path, None)?;
        Ok(enc)
    }

    /// Wie [`create`], aber vom [`HwContext`] entkoppelt (nimmt HW-Pixelformat +
    /// den zu bindenden Frames-Kontext direkt) und mit optionalem Audio-Stream
    /// (libopus). Der NVENC-Pfad übergibt `hw.ffmpeg_pixel()`+`hw.frames_ref()`;
    /// der VAAPI-Pfad übergibt `Pixel::VAAPI` + den NV12-Frames-Kontext vom
    /// `scale_vaapi`-Filter-Ausgang. Der Audio-Stream wird VOR `write_header`
    /// hinzugefügt; der zurückgegebene [`AudioEncoder`] läuft auf einem eigenen
    /// Thread und teilt sich den Muxer über [`VideoEncoder::mux_sender`].
    ///
    /// SAFETY: `frames_ctx` muss ein gültiger `AVHWFramesContext`-`AVBufferRef`
    /// sein, der `hw_pixel` entspricht, und mindestens bis `write_header` leben.
    ///
    /// [`create`]: VideoEncoder::create
    pub fn create_with_audio(
        cfg: &EncoderConfig,
        hw_pixel: format::Pixel,
        frames_ctx: *mut AVBufferRef,
        output_path: &str,
        audio: Option<AudioParams>,
    ) -> Result<(Self, Option<AudioEncoder>)> {
        ffmpeg::init().context("ffmpeg::init")?;

        let mut output = match url_format_hint(output_path) {
            Some(fmt) => {
                let mut o = Dictionary::new();
                if fmt == "whip" {
                    // Der WHIP-Muxer macht sein eigenes I/O (ICE/DTLS/SRTP) —
                    // rw_timeout/tls_verify sind AVIO-Optionen und greifen hier
                    // nicht. handshake_timeout (5s Default) begrenzt den Aufbau.
                    o.set("handshake_timeout", "10000");
                } else {
                    o.set("rw_timeout", "10000000"); // 10s — sonst blockt ein toter Socket ewig
                    // Stirbt der Audio-Pfad mid-stream (Track angekündigt,
                    // aber keine Pakete mehr), hält av_interleaved_write_frame
                    // Video sonst bis zum Default-Delta (10 s!) zurück —
                    // 1 s begrenzt den Schaden auf leichte Zusatz-Latenz.
                    o.set("max_interleave_delta", "1000000");
                    if output_path.to_ascii_lowercase().starts_with("rtmps://") {
                        o.set("tls_verify", "0"); // self-signed MediaMTX (GnuTLS honoriert das)
                    }
                }
                // Fehlerkontext IMMER über redact_url: `output_path` trägt das
                // Stream-Token, und dieser anyhow-Kontext landet als
                // Event::Error roh im stdio-Protokoll (Renderer-Banner) und
                // auf stderr (sidecar.log) — Kontrakt in `redact.rs`.
                format::output_as_with(output_path, fmt, o).with_context(|| {
                    format!("format::output_as_with({}, {fmt})", redact_url(output_path))
                })?
            }
            None => format::output(output_path)
                .with_context(|| format!("format::output({})", redact_url(output_path)))?,
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
        encoder.set_format(hw_pixel);
        encoder.set_time_base(Rational::new(1, cfg.fps as i32));
        encoder.set_frame_rate(Some(Rational::new(cfg.fps as i32, 1)));
        let bitrate_bps = (cfg.bitrate_kbps as usize).saturating_mul(1000);
        encoder.set_bit_rate(bitrate_bps);
        encoder.set_max_bit_rate(bitrate_bps);
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
            let new_ref = av_buffer_ref(frames_ctx);
            if new_ref.is_null() {
                return Err(anyhow!("av_buffer_ref(frames_ctx) returned NULL"));
            }
            (*ctx_ptr).hw_frames_ctx = new_ref;
        }

        let o = opts::vendor_opts(cfg.vendor);
        let opened = encoder
            .open_with(o)
            .with_context(|| format!("open hw encoder '{codec_name}' (vendor={:?})", cfg.vendor))?;
        stream.set_parameters(&opened);

        // Audio-Stream VOR write_header hinzufügen (der Video-Stream-Borrow ist
        // nach set_parameters freigegeben). Scheitert der Audio-Encoder
        // (libopus fehlt/Open-Fehler), läuft der Stream VIDEO-ONLY weiter —
        // ein reines Audio-Problem darf das HQ-Streaming nicht killen. Der
        // Track wird dann gar nicht erst angekündigt (ein deklarierter, aber
        // stummer Track ließe den Interleave-Muxer puffern).
        let mut audio_enc = audio.as_ref().and_then(|a| {
            match AudioEncoder::create(&mut output, a.sample_rate, a.bitrate_kbps) {
                Ok(enc) => Some(enc),
                Err(e) => {
                    tracing::warn!(
                        target: "stream",
                        "Audio-Encoder nicht verfügbar ({e:#}) — Stream läuft ohne Ton"
                    );
                    None
                }
            }
        });

        output.write_header().context("write_header")?;

        let stream_time_base = output.stream(stream_idx).unwrap().time_base();
        let encoder_time_base = Rational::new(1, cfg.fps as i32);

        // Vom Muxer zugewiesene Audio-Stream-Timebase nachreichen.
        if let Some(ae) = audio_enc.as_mut() {
            let tb = output.stream(ae.stream_idx()).unwrap().time_base();
            ae.set_stream_time_base(tb);
        }

        let mux = MuxWriter::start(output).context("start mux-writer")?;

        Ok((
            Self {
                mux,
                encoder: opened,
                video_stream_idx: stream_idx,
                encoder_time_base,
                stream_time_base,
            },
            audio_enc,
        ))
    }

    /// Cloneable Muxer-Sender für den Audio-Encode-Thread.
    pub fn mux_sender(&self) -> Result<MuxSender> {
        self.mux.sender()
    }

    /// Schicke einen HW-Frame (CUDA/VAAPI, `*mut AVFrame`) in den Encoder.
    /// `pts` in Encoder-Timebase (1/fps), strikt monoton.
    pub fn send_hw(&mut self, frame: *mut AVFrame, pts: i64) -> Result<()> {
        unsafe {
            (*frame).pts = pts;
            let mut ret = avcodec_send_frame(self.encoder.as_mut_ptr(), frame);
            if ret == AVERROR(libc::EAGAIN) {
                // Encoder-Input voll (kleiner NVENC-Surface-Pool / VAAPI
                // async_depth) — laut send/receive-Kontrakt KEIN Fehler:
                // erst drainen, dann genau einmal nachschieben. Bleibt es
                // EAGAIN, wird der Frame verworfen (CFR dupliziert eh).
                self.drain_video()?;
                ret = avcodec_send_frame(self.encoder.as_mut_ptr(), frame);
                if ret == AVERROR(libc::EAGAIN) {
                    tracing::debug!(target: "stream", "Encoder-Queue voll — Frame übersprungen");
                    return Ok(());
                }
            }
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

/// Probe-Auflösung: klein, aber über AV1-Mindestmaßen/Alignment.
const PROBE_W: u32 = 1280;
const PROBE_H: u32 = 720;

/// Kann DIESE Hardware den Encoder für `codec` (`h264`/`av1`) wirklich öffnen?
///
/// Der EINZIGE verlässliche Test: HW-Frames-Kontext bauen + Encoder öffnen.
/// Dass `find_by_name` den Encoder findet, sagt NICHTS über die GPU — FFmpeg
/// linkt `av1_nvenc` auch auf einer Karte ohne AV1-Encode (z. B. RTX 30xx:
/// AV1 nur decode). NVENC/VAAPI melden erst beim `open`, ob die Hardware den
/// Codec trägt.
///
/// `Ok(true|false)` = Probe lief sauber. `Err` = Device selbst nicht
/// initialisierbar (Treiber fehlt) → Caller behandelt konservativ (nicht
/// anbieten).
pub fn probe_encoder(vendor: Vendor, render_node: &str, codec_id: &str) -> Result<bool> {
    let Some(name) = opts::encoder_name(vendor, codec_id) else {
        return Ok(false);
    };
    ffmpeg::init().context("ffmpeg::init")?;
    let Some(desc) = codec::encoder::find_by_name(name) else {
        return Ok(false); // Encoder nicht ins FFmpeg gelinkt
    };

    let kind = hw::kind_for(vendor);
    let (dev_arg, sw) = match vendor {
        // Eingangsformat wie der echte Pfad: NVENC RGB0 (Blit-Ergebnis),
        // VAAPI NV12 (scale_vaapi-Ausgang).
        Vendor::Nvidia => (None, AVPixelFormat::AV_PIX_FMT_RGB0),
        Vendor::Amd | Vendor::Intel => (Some(render_node), AVPixelFormat::AV_PIX_FMT_NV12),
    };
    let hwctx = HwContext::create(kind, dev_arg, PROBE_W, PROBE_H, sw)?;

    // FFmpeg-Logs während der Probe dämpfen — ein fehlgeschlagener open loggt
    // sonst laute AV_LOG_ERROR-Zeilen in die sidecar.log, obwohl "geht nicht"
    // hier der ERWARTETE Ausgang ist. `av_log_set_level` ist PROZESS-global:
    // (a) parallele Proben serialisiert der Lock (sonst Race beim Restore —
    // eine Probe könnte den FATAL-Wert der anderen als "prev" einfangen);
    // (b) läuft gerade ein Stream, wird NICHT gedämpft — sonst fehlten genau
    // während der Probe die Fehlerlogs eines parallelen Push-Problems.
    static PROBE_LOG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _serialize = PROBE_LOG_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let quiet = !crate::stream_controller::StreamController::singleton()
        .state()
        .running;
    let prev = unsafe { av_log_get_level() };
    if quiet {
        unsafe { av_log_set_level(AV_LOG_FATAL) };
    }
    let ok = probe_open(desc, &hwctx, vendor);
    if quiet {
        unsafe { av_log_set_level(prev) };
    }
    Ok(ok)
}

/// Encoder-Context bauen, Frames-Pool binden, `open` versuchen. Kein Muxer,
/// kein Output — nur der Fähigkeits-Test.
fn probe_open(desc: ffmpeg::Codec, hwctx: &HwContext, vendor: Vendor) -> bool {
    let Ok(mut enc) = codec::context::Context::new_with_codec(desc)
        .encoder()
        .video()
    else {
        return false;
    };
    enc.set_width(PROBE_W);
    enc.set_height(PROBE_H);
    enc.set_format(hwctx.ffmpeg_pixel());
    enc.set_time_base(Rational::new(1, 30));
    enc.set_frame_rate(Some(Rational::new(30, 1)));
    enc.set_bit_rate(2_000_000);
    unsafe {
        let ctx = enc.as_mut_ptr();
        let new_ref = av_buffer_ref(hwctx.frames_ref());
        if new_ref.is_null() {
            return false;
        }
        (*ctx).hw_frames_ctx = new_ref;
    }
    enc.open_with(opts::vendor_opts(vendor)).is_ok()
}

/// Output-Format-Hint nach URL-Schema: rtmp(s)→flv, srt→mpegts,
/// http(s)→whip (WebRTC-Ingest, Gäste-Publish auf App-gehosteten Instanzen —
/// media-svc mintet dort `https://<host>/whep/<path>/whip?token=…`), sonst None
/// (Datei → auto-detect anhand Erweiterung). Wie mac/win (+WHIP nur Linux).
pub fn url_format_hint(url: &str) -> Option<&'static str> {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("rtmp://") || lower.starts_with("rtmps://") {
        Some("flv")
    } else if lower.starts_with("srt://") {
        Some("mpegts")
    } else if lower.starts_with("http://") || lower.starts_with("https://") {
        Some("whip")
    } else {
        None
    }
}

/// Ist die Push-URL ein WHIP-Ziel? (Für den AV1→H.264-Fallback in `ops::start` —
/// der ffmpeg-8.1-WHIP-Muxer kann kein AV1.)
pub fn is_whip_url(url: &str) -> bool {
    url_format_hint(url) == Some("whip")
}

#[cfg(test)]
mod format_hint_tests {
    use super::{is_whip_url, url_format_hint};

    #[test]
    fn hints_by_scheme() {
        assert_eq!(url_format_hint("rtmp://h:1935/x"), Some("flv"));
        assert_eq!(url_format_hint("RTMPS://h:1936/x?user=pulse&pass=t"), Some("flv"));
        assert_eq!(url_format_hint("srt://h:8890?streamid=publish:x"), Some("mpegts"));
        assert_eq!(url_format_hint("http://127.0.0.1:8889/channel-1/whip"), Some("whip"));
        assert_eq!(
            url_format_hint("https://host/whep/channel-1-2-abc/whip?token=t"),
            Some("whip")
        );
        assert_eq!(url_format_hint("/tmp/out.mp4"), None);
    }

    #[test]
    fn whip_detection() {
        assert!(is_whip_url("https://host/whep/channel-1/whip?token=t"));
        assert!(!is_whip_url("rtmps://host:1936/channel-1?user=pulse&pass=t"));
        assert!(!is_whip_url("/tmp/out.mp4"));
    }
}
