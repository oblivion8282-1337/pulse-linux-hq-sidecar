//! Audio-Encode-Pfad — libopus für FLV (Opus-in-FLV ist ab FFmpeg ≥6.1 nativ,
//! kein Patch nötig; wir linken FFmpeg 8).
//!
//! Portiert aus `mac-hq-sidecar/src/encode/audio.rs`. Der PipeWire-Sink-Monitor
//! (`capture::audio`) liefert interleaved Float32-Stereo @48kHz — genau libopus'
//! Eingabeformat (`AV_SAMPLE_FMT_FLT`). Wir akkumulieren in ein FIFO und emittieren
//! 960-Sample-Frames (20ms). Anders als der Mac läuft der Push auf einem eigenen
//! Encode-Thread und schiebt Packets über einen [`MuxSender`] (der Muxer
//! interleaved Video+Audio nach DTS).

use std::collections::VecDeque;

use anyhow::{Context, Result, anyhow};
use ffmpeg_next as ffmpeg;
use ffmpeg::{ChannelLayout, Dictionary, Packet, Rational, codec, format, frame};

use super::mux_writer::MuxSender;

/// 20ms @48kHz = 960 Samples pro Kanal — der Standard-libopus-Frame.
pub const OPUS_FRAME_SAMPLES: usize = 960;

/// Ab dieser Abweichung zwischen Wanduhr-Anker und interner pts-Zeitlinie wird
/// re-verankert (100 ms @48 kHz). Klein genug, dass hörbarer A/V-Versatz nach
/// einer Capture-Lücke korrigiert wird; groß genug, dass FIFO-Restbestand und
/// Batch-Jitter nie einen Sprung auslösen.
const RESYNC_THRESHOLD_SAMPLES: i64 = 4800;

/// Audio-pts-Zeitlinie: verankert den ersten Frame an der Stream-Wanduhr und
/// RE-ankert nach Capture-Lücken. PipeWire liefert bei suspendiertem Node
/// (Stille) nichts — zählte man danach stur weiter (`+960` pro Frame), liefe
/// der Ton dem Video dauerhaft um exakt die Lückenlänge voraus.
struct PtsTimeline {
    out_pts: i64,
    anchored: bool,
}

impl PtsTimeline {
    fn new() -> Self {
        Self { out_pts: 0, anchored: false }
    }

    /// `anchor_samples` = Wanduhr-Position des aktuellen Batches (Samples seit
    /// Stream-Epoche). Liefert den pts für den nächsten Opus-Frame; springt
    /// bei einer Lücke nach VORN, nie zurück (pts bleiben monoton).
    fn align(&mut self, anchor_samples: i64) -> i64 {
        let anchor = anchor_samples.max(0);
        if !self.anchored {
            self.out_pts = anchor;
            self.anchored = true;
        } else if anchor - self.out_pts > RESYNC_THRESHOLD_SAMPLES {
            tracing::info!(
                target: "audio",
                gap_samples = anchor - self.out_pts,
                "Capture-Lücke — Audio-pts re-verankert"
            );
            self.out_pts = anchor;
        }
        self.out_pts
    }

    /// Nach einem emittierten Frame weiterzählen.
    fn advance(&mut self, samples: i64) {
        self.out_pts += samples;
    }
}

#[cfg(test)]
mod timeline_tests {
    use super::{OPUS_FRAME_SAMPLES, PtsTimeline, RESYNC_THRESHOLD_SAMPLES};

    const FRAME: i64 = OPUS_FRAME_SAMPLES as i64;

    #[test]
    fn anchors_first_batch_and_ignores_jitter() {
        let mut t = PtsTimeline::new();
        assert_eq!(t.align(1000), 1000);
        t.advance(FRAME);
        // Kleiner Batch-Jitter (< Schwelle) darf NICHT springen.
        assert_eq!(t.align(1000 + FRAME + 100), 1000 + FRAME);
    }

    /// Capture-Lücke (Node suspendiert): der Anker läuft der Zeitlinie weit
    /// voraus → re-ankern, sonst ist der Ton dauerhaft um die Lücke versetzt.
    #[test]
    fn reanchors_after_capture_gap() {
        let mut t = PtsTimeline::new();
        t.align(0);
        t.advance(FRAME);
        let gap_anchor = FRAME + RESYNC_THRESHOLD_SAMPLES + 48_000; // ~1s Lücke
        assert_eq!(t.align(gap_anchor), gap_anchor);
    }

    /// pts bleiben monoton: ein rückwärts laufender Anker (Capture eilt der
    /// Wanduhr voraus) darf die Zeitlinie nie zurückdrehen.
    #[test]
    fn never_jumps_backwards() {
        let mut t = PtsTimeline::new();
        t.align(48_000);
        t.advance(FRAME);
        assert_eq!(t.align(0), 48_000 + FRAME);
    }
}

pub struct AudioEncoder {
    encoder: codec::encoder::Audio,
    frame: frame::Audio,
    /// Interleaved Stereo-Float32-FIFO.
    fifo: VecDeque<f32>,
    channels: usize,
    stream_idx: usize,
    encoder_time_base: Rational,
    stream_time_base: Rational,
    /// Output-pts-Zeitlinie (Samples, 1/sample_rate-Einheiten).
    timeline: PtsTimeline,
}

impl AudioEncoder {
    /// libopus-Encoder anlegen + Audio-Stream zu `output` hinzufügen. MUSS VOR
    /// `output.write_header()` laufen.
    pub fn create(
        output: &mut format::context::Output,
        sample_rate: u32,
        bitrate_kbps: u32,
    ) -> Result<Self> {
        let codec = codec::encoder::find_by_name("libopus")
            .ok_or_else(|| anyhow!("libopus-Encoder nicht im gelinkten FFmpeg"))?;
        let global_header = output
            .format()
            .flags()
            .contains(format::Flags::GLOBAL_HEADER);

        let mut stream = output.add_stream(codec).context("add_stream audio")?;
        let stream_idx = stream.index();

        let mut enc = codec::context::Context::new_with_codec(codec)
            .encoder()
            .audio()?;
        // libopus akzeptiert nur interleaved Float32.
        enc.set_format(format::Sample::F32(format::sample::Type::Packed));
        enc.set_rate(sample_rate as i32);
        enc.set_channel_layout(ChannelLayout::STEREO);
        enc.set_bit_rate((bitrate_kbps as usize).saturating_mul(1000));
        enc.set_time_base(Rational::new(1, sample_rate as i32));
        if global_header {
            enc.set_flags(codec::Flags::GLOBAL_HEADER);
        }
        let encoder = enc.open_with(Dictionary::new()).context("open libopus encoder")?;
        stream.set_parameters(&encoder);

        let frame = frame::Audio::new(
            format::Sample::F32(format::sample::Type::Packed),
            OPUS_FRAME_SAMPLES,
            ChannelLayout::STEREO,
        );

        Ok(Self {
            encoder,
            frame,
            fifo: VecDeque::new(),
            channels: 2,
            stream_idx,
            encoder_time_base: Rational::new(1, sample_rate as i32),
            stream_time_base: Rational::new(1, sample_rate as i32),
            timeline: PtsTimeline::new(),
        })
    }

    /// Vom Muxer zugewiesene Stream-Timebase setzen (nach `write_header` lesen).
    pub fn set_stream_time_base(&mut self, tb: Rational) {
        self.stream_time_base = tb;
    }

    /// Interleaved Stereo-Samples akkumulieren und volle 20ms-Opus-Frames
    /// emittieren. `anchor_samples` = Wanduhr-Position DIESES Batches (Samples
    /// seit Stream-Epoche, mit Video geteilt) — verankert den ersten Frame-pts
    /// und re-ankert nach Capture-Lücken (s. [`PtsTimeline`]).
    pub fn push(&mut self, samples: &[f32], mux: &MuxSender, anchor_samples: i64) -> Result<()> {
        let mut pts = self.timeline.align(anchor_samples);
        self.fifo.extend(samples.iter().copied());
        let chunk = OPUS_FRAME_SAMPLES * self.channels;
        while self.fifo.len() >= chunk {
            {
                let plane = self.frame.data_mut(0);
                let n = chunk.min(plane.len() / 4);
                for i in 0..n {
                    let v = self.fifo.pop_front().unwrap_or(0.0);
                    plane[i * 4..i * 4 + 4].copy_from_slice(&v.to_ne_bytes());
                }
            }
            self.frame.set_pts(Some(pts));
            self.timeline.advance(OPUS_FRAME_SAMPLES as i64);
            pts = self.timeline.out_pts;
            self.encoder.send_frame(&self.frame).context("audio send_frame")?;
            self.drain(mux)?;
        }
        Ok(())
    }

    fn drain(&mut self, mux: &MuxSender) -> Result<()> {
        loop {
            let mut packet = Packet::empty();
            match self.encoder.receive_packet(&mut packet) {
                Ok(()) => {
                    packet.set_stream(self.stream_idx);
                    packet.rescale_ts(self.encoder_time_base, self.stream_time_base);
                    mux.send(packet)?;
                }
                Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::error::EAGAIN => break,
                Err(ffmpeg::Error::Eof) => break,
                Err(e) => return Err(e).context("audio receive_packet"),
            }
        }
        Ok(())
    }

    pub fn flush(&mut self, mux: &MuxSender) -> Result<()> {
        self.encoder.send_eof().context("audio send_eof")?;
        self.drain(mux)
    }

    pub fn stream_idx(&self) -> usize {
        self.stream_idx
    }
}
