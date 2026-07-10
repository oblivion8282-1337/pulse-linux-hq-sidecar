//! PipeWire-Audio-Capture: System-Ausgabeton (Sink-Monitor) → interleaved
//! Float32-Stereo @48kHz, direkt als Opus-Encoder-Input.
//!
//! Anders als der Video-Pfad braucht Audio **kein** Portal — der Sink-Monitor
//! (`PW_KEY_STREAM_CAPTURE_SINK=true`) ist für jeden Client lesbar. Wir
//! verbinden auf den Default-Graph (`connect_rc(None)`) und fordern F32LE /
//! 48000 / 2ch im EnumFormat an; PipeWire konvertiert (Adapter) automatisch.
//!
//! Threading wie `pipewire_stream`: MainLoop+Context+Stream leben auf EINEM
//! Worker-Thread (pipewire-rs nutzt `Rc`), nach außen geht nur der
//! `mpsc::Receiver<Vec<f32>>` (Send). Stop über `pw::channel` → `quit()`.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::{self, JoinHandle};

use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use spa::param::audio::{AudioFormat, AudioInfoRaw};
use spa::param::format::{MediaSubtype, MediaType};
use spa::param::format_utils;
use spa::pod::Pod;

/// Ziel-Sample-Rate — Opus arbeitet nativ mit 48kHz.
pub const SAMPLE_RATE: u32 = 48_000;
/// Stereo (interleaved FL,FR).
pub const CHANNELS: u32 = 2;

struct AudioData {
    sample_tx: Sender<Vec<f32>>,
    info: AudioInfoRaw,
}

/// Laufende Audio-Capture-Session. `stop` beendet den Worker-Thread.
pub struct AudioCapture {
    stop_tx: pw::channel::Sender<()>,
    worker: Option<JoinHandle<()>>,
}

impl AudioCapture {
    /// Starte die Sink-Monitor-Capture. Liefert interleaved F32-Stereo-Chunks
    /// (Länge variabel, ein Chunk pro PipeWire-`process`).
    pub fn start() -> anyhow::Result<(Receiver<Vec<f32>>, Self)> {
        let (sample_tx, sample_rx) = channel::<Vec<f32>>();
        let (stop_tx, stop_rx) = pw::channel::channel::<()>();

        let worker = thread::Builder::new()
            .name("pipewire-audio".into())
            .spawn(move || {
                if let Err(e) = run_audio(sample_tx, stop_rx) {
                    tracing::error!(target: "audio", "Audio-Capture-Thread: {e:#}");
                }
            })?;
        Ok((sample_rx, Self { stop_tx, worker: Some(worker) }))
    }

    pub fn stop(&mut self) {
        let _ = self.stop_tx.send(());
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

fn run_audio(sample_tx: Sender<Vec<f32>>, stop_rx: pw::channel::Receiver<()>) -> anyhow::Result<()> {
    pw::init();

    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let _stop_receiver = stop_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    // Angefordertes Format (EnumFormat). `AudioData.info` bleibt Default und
    // nimmt beim `param_changed` das tatsächlich negotiierte Format auf.
    let mut req = AudioInfoRaw::new();
    req.set_format(AudioFormat::F32LE);
    req.set_rate(SAMPLE_RATE);
    req.set_channels(CHANNELS);
    let data = AudioData { sample_tx, info: AudioInfoRaw::new() };

    let stream = pw::stream::StreamRc::new(
        core,
        "pulse-linux-hq-sidecar-audio",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Music",
            // Sink-Monitor: das, was der User hört (System-Ausgabe).
            *pw::keys::STREAM_CAPTURE_SINK => "true",
        },
    )?;

    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|_s, _ud, old, new| {
            tracing::debug!(target: "audio", "PW-State: {old:?} -> {new:?}");
        })
        .param_changed(|_s, ud, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = format_utils::parse_format(param) else {
                return;
            };
            if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
                return;
            }
            if ud.info.parse(param).is_ok() {
                tracing::info!(
                    target: "audio",
                    rate = ud.info.rate(),
                    channels = ud.info.channels(),
                    "Audio-Format ausgehandelt"
                );
            }
        })
        .process(|stream, ud| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data = &mut datas[0];
            let size = data.chunk().size() as usize;
            if size == 0 {
                return;
            }
            if let Some(bytes) = data.data() {
                let n = size.min(bytes.len()) / std::mem::size_of::<f32>();
                let mut samples = Vec::with_capacity(n);
                for i in 0..n {
                    let off = i * 4;
                    samples.push(f32::from_le_bytes([
                        bytes[off],
                        bytes[off + 1],
                        bytes[off + 2],
                        bytes[off + 3],
                    ]));
                }
                // Fehlt der Consumer, ist der Stream vorbei — Fehler ignorieren.
                let _ = ud.sample_tx.send(samples);
            }
        })
        .register()?;

    // EnumFormat aus AudioInfoRaw (F32LE/48k/2ch).
    let obj = pw::spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: req.into(),
    };
    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .map_err(|e| anyhow::anyhow!("serialize audio EnumFormat: {e:?}"))?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).ok_or_else(|| anyhow::anyhow!("audio EnumFormat from_bytes"))?];

    stream.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    tracing::info!(target: "audio", "Audio-Capture verbunden, Mainloop läuft");
    mainloop.run();
    tracing::debug!(target: "audio", "Audio-Mainloop beendet (stop)");
    Ok(())
}
