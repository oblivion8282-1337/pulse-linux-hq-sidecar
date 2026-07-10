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

pub struct AudioEncoder {
    encoder: codec::encoder::Audio,
    frame: frame::Audio,
    /// Interleaved Stereo-Float32-FIFO.
    fifo: VecDeque<f32>,
    channels: usize,
    stream_idx: usize,
    encoder_time_base: Rational,
    stream_time_base: Rational,
    /// Output-pts in Samples (1/sample_rate-Einheiten).
    out_pts: i64,
    /// Ob der erste Frame-pts an die Stream-Epoche verankert wurde.
    anchored: bool,
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
            out_pts: 0,
            anchored: false,
        })
    }

    /// Vom Muxer zugewiesene Stream-Timebase setzen (nach `write_header` lesen).
    pub fn set_stream_time_base(&mut self, tb: Rational) {
        self.stream_time_base = tb;
    }

    /// Interleaved Stereo-Samples akkumulieren und volle 20ms-Opus-Frames
    /// emittieren. `anchor_samples` verankert den ERSTEN Frame-pts an der
    /// Stream-Wanduhr-Epoche (mit Video geteilt) — startet Audio-Capture später
    /// als Video, wird seine Zeitlinie entsprechend versetzt statt bei 0.
    pub fn push(&mut self, samples: &[f32], mux: &MuxSender, anchor_samples: i64) -> Result<()> {
        if !self.anchored {
            self.out_pts = anchor_samples.max(0);
            self.anchored = true;
        }
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
            self.frame.set_pts(Some(self.out_pts));
            self.out_pts += OPUS_FRAME_SAMPLES as i64;
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
