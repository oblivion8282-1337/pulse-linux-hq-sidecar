//! PipeWire-Audio-Capture → interleaved Float32-Stereo @48kHz, direkt als
//! Opus-Encoder-Input. Modusabhängig (siehe [`AudioSelection`]):
//!
//! - **Desktop / App**: eigener Capture-Sink via [`audio_router`] (Null-Sink,
//!   auf den nur die gewünschten App-Streams gelinkt werden — Desktop schließt
//!   Pulse selbst + user-Excludes aus, App linkt genau eine App). Der Stream
//!   hier hängt am Monitor DIESES Sinks (`TARGET_OBJECT` + CAPTURE_SINK).
//! - **Mikrofon**: Default-Input (AUTOCONNECT, kein CAPTURE_SINK).
//!
//! Audio braucht **kein** Portal. Wir verbinden auf den Default-Graph
//! (`connect_rc(None)`) und fordern F32LE / 48000 / 2ch im EnumFormat an;
//! PipeWire konvertiert (Adapter) automatisch.
//!
//! Threading wie `pipewire_stream`: MainLoop+Context+Stream(+Router) leben auf
//! EINEM Worker-Thread (pipewire-rs nutzt `Rc`), nach außen geht nur der
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

use super::audio_router::{AudioRouter, RouteMode};

/// Ziel-Sample-Rate — Opus arbeitet nativ mit 48kHz.
pub const SAMPLE_RATE: u32 = 48_000;
/// Stereo (interleaved FL,FR).
pub const CHANNELS: u32 = 2;

/// Node-Name der eigenen Electron-Audio-Streams (via `PULSE_PROP` in
/// `desktop/electron/main.ts`). Bei Desktop-Capture IMMER ausgeschlossen,
/// damit Pulses Voice-Wiedergabe nicht als Echo im Stream landet — gleiche
/// Konvention wie der Python-Sidecar (`profiles.py::PULSE_SELF_NODE_NAME`).
pub const PULSE_SELF_NODE_NAME: &str = "Pulse";

/// UI-Prefix für App-spezifisches Capture (`"App: <name>"` auf der Leitung,
/// wie `APP_AUDIO_PREFIX` im Frontend / `APP_LABEL_PREFIX` in Python).
const APP_AUDIO_PREFIX: &str = "App: ";

/// Aufgelöster Audio-Modus eines Streams.
#[derive(Debug, Clone)]
pub enum AudioSelection {
    Off,
    /// Default-Mikrofon.
    Mic,
    /// System-Ton = alle App-Streams außer `exclude` (enthält immer "Pulse").
    Desktop { exclude: Vec<String> },
    /// Nur der Ton EINER App.
    App { name: String },
}

impl AudioSelection {
    /// Wire-`audio.mode` + `excluded_apps` → Selection. Unbekanntes → Off
    /// (ein Streaming-Start soll an Audio nie scheitern). "Desktop + Mikrofon"
    /// wird als Desktop behandelt (Mikrofon-Mix noch nicht implementiert —
    /// Warnung loggt `ops::start`).
    pub fn parse(mode: &str, mut excluded_apps: Vec<String>) -> Self {
        let mode = mode.trim();
        let mode = mode.strip_suffix(" (offline)").unwrap_or(mode).trim();
        if let Some(app) = mode.strip_prefix(APP_AUDIO_PREFIX) {
            let app = app.trim();
            if !app.is_empty() {
                return Self::App { name: app.to_string() };
            }
            return Self::Off;
        }
        match mode {
            "Mikrofon" => Self::Mic,
            "Desktop" | "Desktop + Mikrofon" => {
                if !excluded_apps
                    .iter()
                    .any(|e| e.eq_ignore_ascii_case(PULSE_SELF_NODE_NAME))
                {
                    excluded_apps.push(PULSE_SELF_NODE_NAME.to_string());
                }
                Self::Desktop { exclude: excluded_apps }
            }
            _ => Self::Off,
        }
    }

    pub fn enabled(&self) -> bool {
        !matches!(self, Self::Off)
    }

    /// Menschlich lesbare Kurzform fürs Stream-Log.
    pub fn describe(&self) -> String {
        match self {
            Self::Off => "aus".to_string(),
            Self::Mic => "Mikrofon".to_string(),
            Self::Desktop { exclude } => format!("Desktop (ohne {})", exclude.join(", ")),
            Self::App { name } => format!("nur App \u{201e}{name}\u{201c}"),
        }
    }
}

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
    /// Starte die Capture für den gegebenen Modus. Liefert interleaved
    /// F32-Stereo-Chunks (Länge variabel, ein Chunk pro PipeWire-`process`).
    pub fn start(selection: &AudioSelection) -> anyhow::Result<(Receiver<Vec<f32>>, Self)> {
        if !selection.enabled() {
            anyhow::bail!("AudioCapture::start mit AudioSelection::Off");
        }
        let selection = selection.clone();
        let (sample_tx, sample_rx) = channel::<Vec<f32>>();
        let (stop_tx, stop_rx) = pw::channel::channel::<()>();

        let worker = thread::Builder::new()
            .name("pipewire-audio".into())
            .spawn(move || {
                if let Err(e) = run_audio(selection, sample_tx, stop_rx) {
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

fn run_audio(
    selection: AudioSelection,
    sample_tx: Sender<Vec<f32>>,
    stop_rx: pw::channel::Receiver<()>,
) -> anyhow::Result<()> {
    pw::init();

    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let _stop_receiver = stop_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    // Modusabhängig: Desktop/App bekommen einen Router (eigener Capture-Sink,
    // auf den nur die gewünschten Quellen gelinkt werden) und der Stream hängt
    // an DESSEN Monitor; Mikrofon connectet direkt auf den Default-Input.
    // Der Router muss bis Mainloop-Ende leben (hält Sink + Links).
    let _router = match &selection {
        AudioSelection::Desktop { exclude } => Some(AudioRouter::start(
            &core,
            RouteMode::All { exclude: exclude.clone() },
        )?),
        AudioSelection::App { name } => {
            Some(AudioRouter::start(&core, RouteMode::App { name: name.clone() })?)
        }
        AudioSelection::Mic => None,
        AudioSelection::Off => unreachable!("start() weist Off ab"),
    };

    let mut props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Music",
    };
    if let Some(router) = &_router {
        // Monitor unseres eigenen Capture-Sinks — NICHT der Default-Sink.
        // ("target.object" literal: die pw::keys-Konstante ist hinter einem
        // höheren Version-Feature-Gate, der Key selbst ist seit 0.3.44 stabil.)
        props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
        props.insert("target.object", router.sink_name());
    }

    // Angefordertes Format (EnumFormat). `AudioData.info` bleibt Default und
    // nimmt beim `param_changed` das tatsächlich negotiierte Format auf.
    let mut req = AudioInfoRaw::new();
    req.set_format(AudioFormat::F32LE);
    req.set_rate(SAMPLE_RATE);
    req.set_channels(CHANNELS);
    let data = AudioData { sample_tx, info: AudioInfoRaw::new() };

    let stream = pw::stream::StreamRc::new(core, "pulse-linux-hq-sidecar-audio", props)?;

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
            let offset = data.chunk().offset() as usize;
            let size = data.chunk().size() as usize;
            if size == 0 {
                return;
            }
            if let Some(bytes) = data.data() {
                let samples = samples_from_chunk(bytes, offset, size);
                if samples.is_empty() {
                    return;
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

/// Gültigen Sample-Bereich aus einem SPA-Chunk schneiden und als F32LE
/// dekodieren. `offset`/`size` kommen aus `chunk()` — der Server darf Buffers
/// mit `offset != 0` liefern (SHM-Ringpuffer), der Ausschnitt beginnt dann
/// NICHT bei Byte 0. Out-of-Range-Werte werden defensiv geclampt.
fn samples_from_chunk(bytes: &[u8], offset: usize, size: usize) -> Vec<f32> {
    let start = offset.min(bytes.len());
    let end = start.saturating_add(size).min(bytes.len());
    // chunks_exact(4) verwirft einen angebrochenen Rest-Sample selbst — kein
    // manuelles Runden auf die f32-Grenze nötig.
    bytes[start..end]
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

#[cfg(test)]
mod chunk_tests {
    use super::samples_from_chunk;

    fn le(vals: &[f32]) -> Vec<u8> {
        vals.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    #[test]
    fn respects_chunk_offset() {
        // Buffer: [garbage garbage | 1.0 2.0], Chunk sagt offset=8, size=8.
        let mut bytes = le(&[9.9, 8.8, 1.0, 2.0]);
        assert_eq!(samples_from_chunk(&bytes, 8, 8), vec![1.0, 2.0]);
        // offset=0 bleibt wie gehabt.
        bytes.truncate(8);
        assert_eq!(samples_from_chunk(&bytes, 0, 8), vec![9.9, 8.8]);
    }

    #[test]
    fn clamps_out_of_range_offset_and_size() {
        let bytes = le(&[1.0, 2.0]);
        // offset hinter dem Buffer → leer statt Panik.
        assert!(samples_from_chunk(&bytes, 64, 8).is_empty());
        // size über das Buffer-Ende hinaus → auf den Rest geclampt.
        assert_eq!(samples_from_chunk(&bytes, 4, 999), vec![2.0]);
        // krumme size → auf Sample-Grenze abgerundet.
        assert_eq!(samples_from_chunk(&bytes, 0, 7), vec![1.0]);
    }
}

#[cfg(test)]
mod selection_tests {
    use super::AudioSelection as S;

    #[test]
    fn parse_modes() {
        assert!(matches!(S::parse("Aus", vec![]), S::Off));
        assert!(matches!(S::parse("Unbekannt", vec![]), S::Off));
        assert!(matches!(S::parse("Mikrofon", vec![]), S::Mic));
        match S::parse("App: Firefox", vec![]) {
            S::App { name } => assert_eq!(name, "Firefox"),
            other => panic!("erwartet App, war {other:?}"),
        }
        assert!(matches!(S::parse("App: ", vec![]), S::Off)); // leerer Name
    }

    #[test]
    fn desktop_always_excludes_pulse() {
        match S::parse("Desktop", vec!["Spotify".into()]) {
            S::Desktop { exclude } => {
                assert!(exclude.iter().any(|e| e == "Spotify"));
                assert!(exclude.iter().any(|e| e == "Pulse"));
            }
            other => panic!("erwartet Desktop, war {other:?}"),
        }
        // Case-insensitiv: kein Duplikat, wenn "pulse" schon drin ist.
        match S::parse("Desktop + Mikrofon", vec!["pulse".into()]) {
            S::Desktop { exclude } => assert_eq!(exclude.len(), 1),
            other => panic!("erwartet Desktop, war {other:?}"),
        }
    }
}
