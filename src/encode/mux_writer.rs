//! Async muxer-writer — decouples `write_interleaved` from the pacing loop.
//!
//! Verbatim aus `streaming/{win,mac}-hq-sidecar/src/encode/mux_writer.rs`
//! (platform-agnostic). Der Muxer (`AVFormatContext`) lebt auf einem eigenen
//! Thread; der Pacing-Loop encodiert nur und schiebt fertige Packets in eine
//! bounded Queue. Ein Keyframe-Socket-Stall staut sich dann in der Queue statt
//! die Capture/Encode-Kadenz einzufrieren.

use std::sync::mpsc::{SyncSender, sync_channel};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result, anyhow};
use ffmpeg_next as ffmpeg;
use ffmpeg::{Packet, format};

/// Queue depth — must absorb a keyframe burst without blocking the encoder.
const QUEUE_CAPACITY: usize = 256;

/// `ffmpeg::Packet` isn't `Send` (ffmpeg-next marks it conservatively). The
/// hand-off is sound: the packet is created on the pacing thread, *moved* over
/// the channel to exactly one writer thread and consumed there — no aliasing.
struct SendPacket(Packet);
// SAFETY: see above.
unsafe impl Send for SendPacket {}

/// Same for the `Output` context: moved once to the writer thread, never
/// touched by the producer afterwards.
struct SendOutput(format::context::Output);
// SAFETY: see above.
unsafe impl Send for SendOutput {}

/// Queue-Nachricht: Packet oder das Shutdown-Sentinel aus `finish()`. Ohne
/// Sentinel endete der Writer-Loop erst, wenn ALLE Sender (inkl. jedes
/// `MuxSender`-Clones des Audio-Threads) gedroppt sind — ein Ordering-Fehler
/// im Caller machte `finish()` dann zum ewigen Hänger. (Abweichung von
/// win/mac-hq-sidecar; dort besteht dasselbe Risiko noch.)
enum MuxMsg {
    Packet(SendPacket),
    Shutdown,
}

/// Cloneable handle for pushing packets to the muxer from multiple producer
/// threads (video pacing loop + audio encode thread). All packets land in the
/// same bounded queue and are interleaved by the writer thread via
/// `write_interleaved` (DTS order).
#[derive(Clone)]
pub struct MuxSender(SyncSender<MuxMsg>);

impl MuxSender {
    /// Push a finished packet (stream index set, timestamps rescaled to the
    /// stream timebase). Blocks only when the queue is full (= writer stuck).
    pub fn send(&self, packet: Packet) -> Result<()> {
        self.0
            .send(MuxMsg::Packet(SendPacket(packet)))
            .map_err(|_| anyhow!("mux-writer thread is gone"))
    }
}

pub struct MuxWriter {
    tx: Option<SyncSender<MuxMsg>>,
    worker: Option<JoinHandle<Result<()>>>,
}

impl MuxWriter {
    /// Takes the fully-configured output context (`write_header` already run,
    /// all streams added) and starts the writer thread.
    pub fn start(output: format::context::Output) -> Result<Self> {
        let (tx, rx) = sync_channel::<MuxMsg>(QUEUE_CAPACITY);
        let out = SendOutput(output);
        let worker = thread::Builder::new()
            .name("mux-writer".into())
            .spawn(move || -> Result<()> {
                let mut output = out.0;
                for msg in rx {
                    let pkt = match msg {
                        MuxMsg::Packet(p) => p,
                        // finish() → Trailer schreiben, egal welche Sender-
                        // Clones noch leben (deren spätere Sends schlagen fehl).
                        MuxMsg::Shutdown => break,
                    };
                    if let Err(e) = pkt.0.write_interleaved(&mut output) {
                        tracing::error!(target: "mux", "write_interleaved fehlgeschlagen: {e:#}");
                        return Err(e).context("mux-writer: write_interleaved");
                    }
                    // Push the bytes onto the wire after every packet (live
                    // low-latency). AVFMT_FLAG_FLUSH_PACKETS is unreliable for the
                    // FLV/RTMP path, so flush the AVIO context explicitly.
                    unsafe {
                        let ctx = output.as_mut_ptr();
                        let pb = (*ctx).pb;
                        if !pb.is_null() {
                            ffmpeg::ffi::avio_flush(pb);
                        }
                    }
                }
                // Channel closed = EOF → write the FLV trailer (clean RTMP/TLS close).
                output.write_trailer().context("mux-writer: write_trailer")?;
                Ok(())
            })
            .context("spawn mux-writer thread")?;
        Ok(Self { tx: Some(tx), worker: Some(worker) })
    }

    /// Push a finished packet (stream index set, timestamps rescaled to the
    /// stream timebase). Blocks only when the queue is full (= writer stuck).
    pub fn send(&self, packet: Packet) -> Result<()> {
        match &self.tx {
            Some(tx) => tx
                .send(MuxMsg::Packet(SendPacket(packet)))
                .map_err(|_| anyhow!("mux-writer thread is gone")),
            None => Err(anyhow!("mux-writer already finished")),
        }
    }

    /// A cloneable sender for a second producer thread (audio). `finish()`
    /// beendet den Writer über ein Shutdown-Sentinel — lebende Clones blocken
    /// den Trailer NICHT mehr, ihre späteren Sends schlagen nur fehl. Sauberes
    /// Draining verlangt trotzdem: Audio zuerst stoppen, dann `finish()`.
    pub fn sender(&self) -> Result<MuxSender> {
        match &self.tx {
            Some(tx) => Ok(MuxSender(tx.clone())),
            None => Err(anyhow!("mux-writer already finished")),
        }
    }

    /// Close the queue, wait for the writer thread (which writes the trailer)
    /// and propagate its result. Beendet den Writer über das Shutdown-Sentinel
    /// — hängt damit auch dann nicht, wenn noch ein `MuxSender`-Clone lebt.
    pub fn finish(&mut self) -> Result<()> {
        if let Some(tx) = self.tx.take() {
            // Fehler = Writer bereits weg (Fehler-Exit) → join liefert dessen Result.
            let _ = tx.send(MuxMsg::Shutdown);
        }
        match self.worker.take() {
            Some(w) => match w.join() {
                Ok(result) => result,
                Err(_) => Err(anyhow!("mux-writer thread panicked")),
            },
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod finish_tests {
    use super::*;
    use std::time::Duration;

    fn null_output_with_stream() -> format::context::Output {
        let path = std::env::temp_dir().join("pulse-mux-finish-test");
        let mut output = format::output_as(&path, "null").expect("null muxer");
        unsafe {
            let st = ffmpeg::ffi::avformat_new_stream(output.as_mut_ptr(), std::ptr::null());
            assert!(!st.is_null());
            let par = (*st).codecpar;
            (*par).codec_type = ffmpeg::ffi::AVMediaType::AVMEDIA_TYPE_VIDEO;
            (*par).codec_id = ffmpeg::ffi::AVCodecID::AV_CODEC_ID_RAWVIDEO;
            (*par).width = 16;
            (*par).height = 16;
        }
        output.write_header().expect("write_header (null)");
        output
    }

    /// `finish()` darf NICHT darauf warten, dass auch alle geklonten
    /// `MuxSender` (Audio-Thread) gedroppt sind — ein Ordering-Fehler im
    /// Caller (z. B. Video-Fehlerpfad ruft `finish`, während Audio noch läuft)
    /// würde sonst zum ewigen Hänger im Stop-Pfad statt zu einem Fehler.
    #[test]
    fn finish_returns_even_while_a_clone_sender_is_alive() {
        let mut w = MuxWriter::start(null_output_with_stream()).unwrap();
        let audio_sender = w.sender().unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let h = std::thread::spawn(move || {
            let _ = done_tx.send(w.finish().is_ok());
        });
        let finished = done_rx.recv_timeout(Duration::from_secs(5));
        drop(audio_sender); // erst NACH dem Timeout-Fenster droppen
        assert!(finished.is_ok(), "finish() hängt, solange ein Clone-Sender lebt");
        let _ = h.join();
    }
}
