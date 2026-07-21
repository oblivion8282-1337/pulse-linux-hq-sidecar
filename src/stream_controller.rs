//! Stream controller — besitzt die eine aktive Capture→Encode→Push-Session.
//!
//! `start` spawnt einen Worker-Thread, der die echte Capture→Encode→Push-Kette
//! aufbaut (Portal-Dialog → PipeWire-DMABUF → Zero-Copy-Import → NVENC/VAAPI →
//! RTMPS), Frames in konstanter Bildrate durch den Encoder pumpt und
//! `state`/`fps`/`error`/`stopped`-Events emittiert. `stop` signalisiert den
//! Worker und joint ihn. Der Linux-Sidecar self-exit'et nicht nach stop — er
//! bleibt warm.
//!
//! Threading + Event-Serialisation 1:1 von mac-hq-sidecar (Single-Writer-Thread
//! via `events::emit`).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use ffmpeg_next as ffmpeg;

use crate::capture::audio::{self, AudioCapture, AudioSelection};
use crate::capture::pipewire_stream::{DmabufFrame, FrameMailbox, PipewireCapture};
use crate::capture::portal;
use crate::encode::audio::AudioEncoder;
use crate::encode::mux_writer::MuxSender;
use crate::encode::nv_import::NvDmabufImporter;
use crate::encode::va_import::VaapiImporter;
use crate::encode::{AudioParams, EncoderConfig, VideoEncoder, hw};
use crate::events;
use crate::proto::{Event, StreamState};
use crate::system::drm::{self, Vendor};

/// Vendor-spezifischer Zero-Copy-Importer + der Frames-Kontext, den der Encoder
/// binden muss. NVENC: EGL/CUDA-Interop, Encoder bindet den BGR0-Pool.
/// VAAPI (AMD/Intel): DRM_PRIME→scale_vaapi-Filtergraph, Encoder bindet den
/// NV12-Buffersink-Ausgang.
enum FrameImporter {
    Nvenc { imp: NvDmabufImporter, hw: hw::HwContext },
    Vaapi { imp: VaapiImporter },
}

impl FrameImporter {
    /// HW-Pixelformat + Frames-Kontext für `VideoEncoder::create_with_audio`.
    fn encoder_binding(&self) -> (ffmpeg::format::Pixel, *mut ffmpeg::ffi::AVBufferRef) {
        match self {
            FrameImporter::Nvenc { hw, .. } => (hw.ffmpeg_pixel(), hw.frames_ref()),
            FrameImporter::Vaapi { imp } => {
                (ffmpeg::format::Pixel::VAAPI, imp.output_frames_ctx())
            }
        }
    }

    /// Importiere einen DMABUF-Frame → encoder-fertiges HW-`AVFrame`.
    fn import(&mut self, frame: &DmabufFrame) -> Result<*mut ffmpeg::ffi::AVFrame> {
        match self {
            FrameImporter::Nvenc { imp, hw } => imp.import(frame, hw),
            FrameImporter::Vaapi { imp } => imp.import(frame),
        }
    }
}

/// Standard-Audio-Bitrate (Opus), bis Profile eine eigene mitliefern.
const AUDIO_BITRATE_KBPS: u32 = 128;

/// Audio-Nebenpfad: PipeWire-Sink-Monitor → Opus → Muxer. Läuft auf zwei
/// Threads (PW-Capture + Encode) parallel zum Video-Pacing-Loop.
struct AudioPipeline {
    cap: AudioCapture,
    worker: Option<JoinHandle<()>>,
}

impl AudioPipeline {
    /// `record_start`: gemeinsamer Monotonic-Nullpunkt mit dem Video-Loop (GSR-
    /// Modell — beide Spuren ankern an DERSELBEN Uhr). `av_offset_ms`: manueller
    /// Feinabgleich (positiv = Ton später).
    fn start(
        mut enc: AudioEncoder,
        mux: MuxSender,
        record_start: Instant,
        av_offset_ms: i32,
        selection: &AudioSelection,
    ) -> Result<Self> {
        let (rx, cap) = AudioCapture::start(selection, record_start)?;
        let worker = thread::Builder::new()
            .name("hq-audio-encode".into())
            .spawn(move || {
                // Jeder Sample-Batch trägt seine CAPTURE-Zeit relativ zu
                // record_start (in Samples, im PW-Callback gestempelt) als
                // Anker: der erste verankert die Audio-Zeitlinie (Audio
                // beginnt bei genau der Video-Zeit, zu der es wirklich
                // einsetzt — kein fixer Offset; GSR: force_no_audio_offset),
                // spätere lassen `PtsTimeline` echte Capture-Lücken erkennen.
                // Empfangszeit wäre falsch: ein Consumer-Stau sähe wie eine
                // Lücke aus und versetzte den Ton permanent.
                let offset_samples =
                    av_offset_ms as i64 * audio::SAMPLE_RATE as i64 / 1000;
                while let Ok((samples, capture_anchor)) = rx.recv() {
                    let anchor = capture_anchor + offset_samples;
                    if let Err(e) = enc.push(&samples, &mux, anchor) {
                        emit(Event::Log { line: format!("[audio] push: {e:#}") });
                        break;
                    }
                }
                if let Err(e) = enc.flush(&mux) {
                    emit(Event::Log { line: format!("[audio] flush: {e:#}") });
                }
                // `mux` (MuxSender) droppt hier → gibt den Muxer-Trailer frei.
            })
            .map_err(|e| anyhow!("spawn hq-audio-encode: {e}"))?;
        Ok(Self { cap, worker: Some(worker) })
    }

    /// Capture stoppen (→ Sample-Channel schließt → Encode-Thread flush+Ende).
    fn stop(&mut self) {
        self.cap.stop();
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

/// Gewünschte Ausgabe-Auflösung. `Exact` ist eine BOX, in die aspektwahrend
/// eingepasst wird (16:9-Monitor + 16:9-Token → exakt der Token; 21:9-Monitor
/// wird NICHT verzerrt). Es wird nie hochskaliert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionRequest {
    Native,
    Exact(u32, u32),
}

impl ResolutionRequest {
    /// Wire-Format: Token (`Native`/`4K`/`1440p`/`1080p`/`720p`/`480p`, wie der
    /// Python-Sidecar `RESOLUTION_TARGETS`) oder literal `WxH`. Unbekanntes →
    /// Native (kein Fehler — ein Streaming-Start soll daran nicht scheitern).
    pub fn parse(s: Option<&str>) -> Self {
        let Some(s) = s.map(str::trim) else {
            return Self::Native;
        };
        match s {
            "" | "Native" => Self::Native,
            "4K" => Self::Exact(3840, 2160),
            "1440p" => Self::Exact(2560, 1440),
            "1080p" => Self::Exact(1920, 1080),
            "720p" => Self::Exact(1280, 720),
            "480p" => Self::Exact(854, 480),
            other => other
                .split_once('x')
                .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)))
                .filter(|&(w, h): &(u32, u32)| w > 0 && h > 0)
                .map(|(w, h)| Self::Exact(w, h))
                .unwrap_or(Self::Native),
        }
    }

    /// Ausgabemaße für eine native Capture-Größe: aspektwahrend in die Box
    /// einpassen, nie hochskalieren, Maße auf gerade Werte runden (Encoder-
    /// Anforderung bei 4:2:0).
    pub fn target_for(&self, native_w: u32, native_h: u32) -> (u32, u32) {
        let even = |n: u32| (n & !1).max(2);
        match *self {
            Self::Native => (even(native_w), even(native_h)),
            Self::Exact(box_w, box_h) => {
                let scale = f64::min(
                    box_w as f64 / native_w.max(1) as f64,
                    box_h as f64 / native_h.max(1) as f64,
                )
                .min(1.0); // kein Upscale
                let w = (native_w as f64 * scale).round() as u32;
                let h = (native_h as f64 * scale).round() as u32;
                (even(w), even(h))
            }
        }
    }
}

impl std::fmt::Display for ResolutionRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Native => write!(f, "native"),
            Self::Exact(w, h) => write!(f, "{w}x{h}"),
        }
    }
}

#[cfg(test)]
mod resolution_tests {
    use super::ResolutionRequest as R;

    #[test]
    fn parse_tokens() {
        assert_eq!(R::parse(None), R::Native);
        assert_eq!(R::parse(Some("Native")), R::Native);
        assert_eq!(R::parse(Some("1080p")), R::Exact(1920, 1080));
        assert_eq!(R::parse(Some("4K")), R::Exact(3840, 2160));
        assert_eq!(R::parse(Some("854x480")), R::Exact(854, 480));
        assert_eq!(R::parse(Some("Quatsch")), R::Native); // unbekannt → Native
        assert_eq!(R::parse(Some("0x100")), R::Native);
    }

    #[test]
    fn target_scales_down_keeps_aspect_never_up() {
        // 4K-Monitor + 1080p-Wunsch → exakt 1080p.
        assert_eq!(R::Exact(1920, 1080).target_for(3840, 2160), (1920, 1080));
        // Kein Upscale: Quelle kleiner als Box → nativ.
        assert_eq!(R::Exact(1920, 1080).target_for(1280, 720), (1280, 720));
        // 21:9 wird eingepasst, nicht verzerrt (Höhe < 1080).
        let (w, h) = R::Exact(1920, 1080).target_for(3440, 1440);
        assert_eq!(w, 1920);
        assert!(h < 1080 && h % 2 == 0, "aspektwahrend + gerade: {h}");
        // Native rundet nur auf gerade Maße.
        assert_eq!(R::Native.target_for(1279, 719), (1278, 718));
    }
}

/// Aufgelöste Parameter für einen Stream (gebaut von `ops::start`).
pub struct StartParams {
    pub codec: String,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub push_url: String,
    pub audio: AudioSelection,
    pub av_offset_ms: i32,
    pub show_cursor: bool,
    pub resolution: ResolutionRequest,
}

pub struct StreamSnapshot {
    pub running: bool,
    pub state: String,
    pub fps: Option<f64>,
    pub uptime_s: Option<f64>,
    pub argv_redacted: Option<Vec<String>>,
}

struct Shared {
    running: AtomicBool,
    live: AtomicBool,
    fps_milli: AtomicU64,
    started_at: Mutex<Option<Instant>>,
    /// Von `stop()` gesetzt, BEVOR das Stop-Signal gesendet wird. Die
    /// Startphase (Portal-Dialog) pollt dieses Flag — der `stop_rx`-Channel
    /// hilft dort nicht, weil `portal::open` async blockt.
    stop_requested: AtomicBool,
}

struct Active {
    stop_tx: std::sync::mpsc::Sender<()>,
    worker: JoinHandle<()>,
    shared: Arc<Shared>,
    argv: Vec<String>,
}

pub struct StreamController {
    active: Mutex<Option<Active>>,
}

static INSTANCE: OnceLock<StreamController> = OnceLock::new();

/// Schickt ein Event aufs Protokoll (stdout) UND spiegelt es ins Diagnose-Log
/// (stderr → Pulse `sidecar.log`), damit der Verlauf eines Streams auch ohne
/// sichtbares Stream-Log-Fenster nachvollziehbar ist. `fps` bewusst nur auf
/// `debug` (sonst 60 Zeilen/s Rauschen).
fn emit(event: Event) {
    match &event {
        Event::Log { line } => tracing::info!(target: "stream", "{line}"),
        Event::Error { message } => tracing::error!(target: "stream", "{message}"),
        Event::State { state, running, .. } => {
            tracing::info!(target: "stream", ?state, running, "state")
        }
        Event::Stopped { code } => tracing::info!(target: "stream", ?code, "stopped"),
        Event::Fps { fps, .. } => tracing::debug!(target: "stream", fps, "fps"),
    }
    if let Ok(v) = serde_json::to_value(event) {
        events::emit(v);
    }
}

/// Besitzt ein `AVFrame` — Drop gibt es frei. Ohne Guard leakte jeder frühe
/// `?`-Fehlerpfad zwischen Kandidaten-Import und Teardown (häufigster Fall:
/// `Connection refused` beim RTMPS-Open) das zuletzt importierte HW-Frame
/// samt Ref auf den GPU-Frame-Pool — dutzende MB VRAM pro Fehlstart.
struct OwnedFrame(*mut ffmpeg::ffi::AVFrame);

impl OwnedFrame {
    /// Ersetzt das gehaltene Frame (das alte wird freigegeben).
    fn replace(&mut self, new: *mut ffmpeg::ffi::AVFrame) {
        unsafe {
            if !self.0.is_null() {
                ffmpeg::ffi::av_frame_free(&mut self.0);
            }
        }
        self.0 = new;
    }

    fn raw(&self) -> *mut ffmpeg::ffi::AVFrame {
        self.0
    }
}

impl Drop for OwnedFrame {
    fn drop(&mut self) {
        self.replace(std::ptr::null_mut());
    }
}

/// Drop-Guard im Worker-Thread: setzt die Shared-Flags auch dann zurück, wenn
/// der Worker PANICT (Unwind läuft am regulären Pfad vorbei). Ohne das bliebe
/// `running = true` stehen — `reap_finished` griffe nie, `state` meldete ewig
/// "starting", jeder neue `start` scheiterte mit "ein Stream läuft bereits",
/// und der Parent bekäme weder `error` noch `stopped`.
struct WorkerDoneGuard(Arc<Shared>);

impl Drop for WorkerDoneGuard {
    fn drop(&mut self) {
        self.0.running.store(false, Ordering::SeqCst);
        self.0.live.store(false, Ordering::SeqCst);
        if thread::panicking() {
            // `emit` ist panic-sicher (no-op ohne Init, kein Lock-Panic).
            // Gleiche Eventfolge wie der reguläre Fehlerpfad: error +
            // state:error als TERMINALZUSTAND (kein stopped — control.py-
            // Parität, die UI soll den Fehler zeigen).
            emit(Event::Error {
                message: "Stream-Worker abgestürzt (Panic) — Details in sidecar.log".to_string(),
            });
            emit(Event::State { state: StreamState::Error, running: false, uptime_s: 0 });
        }
    }
}

/// Wartet auf den ersten Capture-Frame — bricht aber sofort ab, wenn `stop`
/// signalisiert wird (`Ok(None)`). Vorher blockte die Startphase hier bis zu
/// 10 s, ohne den Stop zu sehen: `stop()` joint den Worker, d. h. die gesamte
/// RPC-Schleife (und der Shutdown bei stdin-EOF) hing solange fest.
fn wait_first_frame(
    frames: &FrameMailbox,
    stop_rx: &Receiver<()>,
    timeout: Duration,
) -> Result<Option<DmabufFrame>> {
    let deadline = Instant::now() + timeout;
    loop {
        match stop_rx.try_recv() {
            Ok(()) | Err(TryRecvError::Disconnected) => return Ok(None),
            Err(TryRecvError::Empty) => {}
        }
        let slice = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(100));
        // Err aus wait_take = Capture-Thread schon wieder weg → propagieren.
        if let Some(f) = frames.wait_take(slice)? {
            return Ok(Some(f));
        }
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "kein Bild vom Compositor in {}s (ist die Quelle sichtbar?)",
                timeout.as_secs()
            ));
        }
    }
}

/// Räumt einen bereits beendeten (aber nie per `stop` abgeholten) Stream ab.
///
/// Endet der Worker von selbst — Ingest-Fehler (`Connection refused`), EOF, GPU-
/// Fehler —, setzt er nur `shared.running = false`, lässt aber `active = Some(..)`
/// stehen (nur `stop` ruft `take()`). Ohne dieses Einsammeln blockiert der
/// nächste `start` fälschlich mit „ein Stream läuft bereits" und `state` meldet
/// „starting" statt „idle", bis der User manuell stoppt. `worker.join()` kehrt
/// sofort zurück, weil der Thread bereits beendet ist. Läuft nie im Worker-Thread
/// selbst (nur aus `start`/`state`), daher kein Self-Join. Muss unter gehaltenem
/// `active`-Lock aufgerufen werden.
fn reap_finished(guard: &mut Option<Active>) {
    let finished = guard
        .as_ref()
        .is_some_and(|a| !a.shared.running.load(Ordering::SeqCst));
    if finished {
        if let Some(dead) = guard.take() {
            let _ = dead.worker.join();
        }
    }
}

impl StreamController {
    pub fn singleton() -> &'static StreamController {
        INSTANCE.get_or_init(|| StreamController { active: Mutex::new(None) })
    }

    /// Start a stream. `argv` is the redacted diagnostic argv (for `state`).
    pub fn start(&self, params: StartParams, argv: Vec<String>) -> Result<()> {
        let mut guard = self.active.lock().unwrap();
        reap_finished(&mut guard);
        if guard.is_some() {
            return Err(anyhow!("ein Stream läuft bereits"));
        }
        let (stop_tx, stop_rx) = channel::<()>();
        let shared = Arc::new(Shared {
            running: AtomicBool::new(true),
            live: AtomicBool::new(false),
            fps_milli: AtomicU64::new(0),
            started_at: Mutex::new(None),
            stop_requested: AtomicBool::new(false),
        });
        let shared_worker = shared.clone();
        let worker = thread::Builder::new()
            .name("hq-stream".into())
            .spawn(move || {
                // Räumt die Flags auch bei Panic (Unwind) ab — s. WorkerDoneGuard.
                let _done = WorkerDoneGuard(shared_worker.clone());
                let result = run_stream(params, stop_rx, &shared_worker);
                shared_worker.running.store(false, Ordering::SeqCst);
                shared_worker.live.store(false, Ordering::SeqCst);
                match result {
                    // Fehler, während der User ohnehin gestoppt hat (Race
                    // Quelle-weg vs. Stop, Abbruchfehler im Teardown): der
                    // Stop war gewollt — sauberes Ende, Fehler nur ins Log.
                    Err(e) if shared_worker.stop_requested.load(Ordering::SeqCst) => {
                        emit(Event::Log {
                            line: format!("[stream] Fehler beim Stop (ignoriert): {e:#}"),
                        });
                        emit(Event::State {
                            state: StreamState::Stopped,
                            running: false,
                            uptime_s: 0,
                        });
                        emit(Event::Stopped { code: None });
                    }
                    // Parität zu control.py: nach einem Fehler bleibt der
                    // Terminalzustand `error` — KEIN `stopped` hinterher
                    // (das flippte die UI auf neutrales „Beendet" statt des
                    // roten Fehler-Labels).
                    Err(e) => {
                        emit(Event::Error { message: format!("{e:#}") });
                        emit(Event::State {
                            state: StreamState::Error,
                            running: false,
                            uptime_s: 0,
                        });
                    }
                    // Sauberes Ende (User-Stop, Quelle beendet, Portal-Abbruch
                    // = code 60 — wie GSRs Exit-Code, kein Fehler).
                    Ok(code) => {
                        emit(Event::State {
                            state: StreamState::Stopped,
                            running: false,
                            uptime_s: 0,
                        });
                        emit(Event::Stopped { code });
                    }
                }
            })
            .map_err(|e| anyhow!("spawn hq-stream thread: {e}"))?;

        *guard = Some(Active { stop_tx, worker, shared, argv });
        Ok(())
    }

    /// Stop the active stream (idempotent). Blocks until the worker finished.
    pub fn stop(&self) -> Result<()> {
        let active = self.active.lock().unwrap().take();
        if let Some(active) = active {
            // Flag ZUERST: die Startphase (Portal-Dialog/First-Frame-Wait)
            // sieht nur das Flag, nicht den Channel.
            active.shared.stop_requested.store(true, Ordering::SeqCst);
            let _ = active.stop_tx.send(());
            let _ = active.worker.join();
        }
        Ok(())
    }

    pub fn state(&self) -> StreamSnapshot {
        let mut guard = self.active.lock().unwrap();
        reap_finished(&mut guard);
        match guard.as_ref() {
            Some(a) => {
                let running = a.shared.running.load(Ordering::SeqCst);
                let live = a.shared.live.load(Ordering::SeqCst);
                let fps = a.shared.fps_milli.load(Ordering::SeqCst) as f64 / 1000.0;
                let uptime = a
                    .shared
                    .started_at
                    .lock()
                    .unwrap()
                    .map(|t| t.elapsed().as_secs_f64());
                StreamSnapshot {
                    running,
                    state: if live { "live" } else { "starting" }.to_string(),
                    fps: if fps > 0.0 { Some(fps) } else { None },
                    uptime_s: uptime,
                    argv_redacted: Some(a.argv.clone()),
                }
            }
            None => StreamSnapshot {
                running: false,
                state: "idle".to_string(),
                fps: None,
                uptime_s: None,
                argv_redacted: None,
            },
        }
    }
}

/// Worker body: Portal→PipeWire-DMABUF→Zero-Copy-Import→HW-Encode→RTMPS-Push
/// bis stop. Konstante Bildrate durch Frame-Duplikation (Compositor liefert
/// nur bei Damage; ein Live-Stream braucht CFR).
///
/// `Ok(code)` = sauberes Ende; `Some(60)` = Portal-Abbruch durch den User
/// (GSR-Exit-Code-Konvention, KEIN Fehler — control.py-Parität: der Dialog-
/// Wegklick erzeugt keinen roten Fehler, nur `stopped {"code":60}`).
fn run_stream(params: StartParams, stop_rx: Receiver<()>, shared: &Shared) -> Result<Option<i32>> {
    *shared.started_at.lock().unwrap() = Some(Instant::now());
    emit(Event::State {
        state: StreamState::Starting,
        running: true,
        uptime_s: 0,
    });

    // detect() liefert die Default-GPU (dGPU-bevorzugt bzw. PULSE_HQ_VENDOR).
    // Die tatsächliche Encode-GPU wird erst nach dem ersten Frame bestimmt
    // (Multi-GPU: der Compositor kann den Monitor auf einer anderen Karte halten).
    let orig_vendor = drm::detect()
        .map(|(v, _)| v)
        .ok_or_else(|| anyhow!("keine DRM-Render-Node gefunden"))?;

    // 1) Portal-Dialog: User wählt Monitor/Fenster. Blockt bis zur Auswahl.
    emit(Event::Log {
        line: "[stream] öffne Portal-Dialog zur Quellenauswahl …".to_string(),
    });
    let session = match portal::open(params.show_cursor, &shared.stop_requested) {
        Ok(s) => s,
        // stop während des Dialogs = kein Fehler — sauber beenden.
        Err(_) if shared.stop_requested.load(Ordering::SeqCst) => return Ok(None),
        Err(e) if portal::is_portal_canceled(&e) => {
            emit(Event::Log {
                line: "[stream] Quellenauswahl abgebrochen".to_string(),
            });
            return Ok(Some(portal::EXIT_PORTAL_CANCELED));
        }
        Err(e) => return Err(anyhow!("Portal-Verhandlung: {e:#}")),
    };
    emit(Event::Log {
        line: format!(
            "[stream] Quelle gewählt: node={} {}x{}",
            session.node_id, session.width, session.height
        ),
    });

    // 2) PipeWire-Capture auf fd + node_id starten.
    let (frames, mut cap) = PipewireCapture::start(
        session.pw_fd,
        session.node_id,
        session.width,
        session.height,
    )?;

    // 3) Auf den ersten DMABUF-Frame warten → verbindliche (negotiierte) Maße.
    //    Stop-abbrechbar: `stop()` joint den Worker — bliebe das Warten blind
    //    für den Stop, hinge die ganze RPC-Schleife bis zu 10 s fest.
    let Some(first) = wait_first_frame(&frames, &stop_rx, Duration::from_secs(10))? else {
        return Ok(None); // stop während der Startphase → sauber beenden
    };
    let (width, height) = (first.width, first.height);

    // Ausgabe-Auflösung: gewünschte Box aspektwahrend auf die native Größe
    // anwenden (kein Upscale). Die Skalierung selbst macht die GPU im Importer
    // (NVENC: GL-Blit ins Staging; VAAPI: scale_vaapi).
    let (out_w, out_h) = params.resolution.target_for(width, height);
    if (out_w, out_h) != (width, height) {
        emit(Event::Log {
            line: format!("[stream] skaliere {width}x{height} → {out_w}x{out_h} (GPU)"),
        });
    } else {
        emit(Event::Log {
            line: format!("[stream] streame in nativer Auflösung {width}x{height}"),
        });
    }

    // 4+6) Importer auf der GPU wählen, die den aufgenommenen Buffer BESITZT.
    //    detect() bevorzugt blind die dGPU; auf Multi-GPU (dGPU + iGPU) kann der
    //    Compositor den Monitor aber auf der anderen Karte halten, und ein
    //    LINEAR-Modifier (0x0) verrät den Besitzer NICHT. Also: den ersten Frame
    //    der Reihe nach auf jedem Kandidaten importieren — wer ihn nehmen
    //    kann, besitzt ihn (Cross-GPU-Import scheitert sonst mit
    //    glEGLImageTargetTexture2DOES 0x0502 bzw. VAAPI-hwmap).
    //
    //    Kandidaten sind einzelne KARTEN (Render-Nodes), nicht Hersteller: zwei
    //    Karten desselben Herstellers (Ryzen-iGPU + AMD-dGPU) wären auf Vendor-
    //    Ebene ununterscheidbar — nur die zufällig erste würde je probiert.
    //    Hersteller-Reihenfolge: Modifier-Hinweis (falls getilt), detect-
    //    Default, Rest; innerhalb eines Herstellers sysfs-Reihenfolge.
    //    Overrides: PULSE_HQ_RENDER_NODE erzwingt genau EINE Karte (Notbremse
    //    für Support-Fälle), PULSE_HQ_VENDOR alle Karten SEINES Herstellers —
    //    beide erlauben keine Ausweichkarte anderer Hersteller.
    let candidates: Vec<(Vendor, String)> =
        if let Some(node) = std::env::var_os("PULSE_HQ_RENDER_NODE") {
            let node = node.to_string_lossy().into_owned();
            let vendor = drm::vendor_of_node(&node).ok_or_else(|| {
                anyhow!("PULSE_HQ_RENDER_NODE={node}: keine bekannte DRM-Render-Node")
            })?;
            vec![(vendor, node)]
        } else {
            let vendor_order: Vec<Vendor> = if std::env::var_os("PULSE_HQ_VENDOR").is_some() {
                vec![orig_vendor]
            } else {
                // Dedupliziert unter Beibehaltung der ersten Position.
                let mut c = Vec::new();
                for v in drm::vendor_from_modifier(first.modifier)
                    .into_iter()
                    .chain(std::iter::once(orig_vendor))
                    .chain(drm::present_vendors())
                {
                    if !c.contains(&v) {
                        c.push(v);
                    }
                }
                c
            };
            candidate_nodes(&vendor_order, &drm::render_nodes())
        };

    let build_importer = |cand: Vendor, node: &str| -> Result<FrameImporter> {
        match cand {
            Vendor::Nvidia => {
                // RGB0 (nicht BGR0): der GL-Blit kopiert komponentenweise
                // BGRx→RGBA8, die Staging-Bytes liegen danach als R,G,B,X.
                let hw_ctx = hw::HwContext::create(
                    hw::HwDeviceKind::Cuda,
                    None,
                    out_w,
                    out_h,
                    ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_RGB0,
                )?;
                let imp = NvDmabufImporter::new(out_w, out_h)?;
                Ok(FrameImporter::Nvenc { imp, hw: hw_ctx })
            }
            Vendor::Amd | Vendor::Intel => {
                let imp = VaapiImporter::new(
                    node,
                    first.drm_fourcc,
                    width,
                    height,
                    params.fps,
                    out_w,
                    out_h,
                )?;
                Ok(FrameImporter::Vaapi { imp })
            }
        }
    };

    let mut chosen: Option<(Vendor, String, FrameImporter, *mut ffmpeg::ffi::AVFrame)> = None;
    let mut last_err: Option<anyhow::Error> = None;
    for (cand, node) in candidates {
        match build_importer(cand, &node).and_then(|mut imp| {
            let frame = imp.import(&first)?;
            Ok((imp, frame))
        }) {
            Ok((imp, frame)) => {
                chosen = Some((cand, node, imp, frame));
                break;
            }
            Err(e) => {
                // Auch als Event: WELCHE Karte abgelehnt hat, ist im Support-
                // Fall die halbe Diagnose (falsche Karte vs. Puffer-Format).
                emit(Event::Log {
                    line: format!(
                        "[stream] Import auf {} ({}) fehlgeschlagen: {e:#}",
                        display_node(&node),
                        cand.slug()
                    ),
                });
                tracing::warn!(
                    target: "stream", vendor = cand.slug(), node = %node,
                    "GPU-Import fehlgeschlagen, nächster Kandidat: {e:#}"
                );
                last_err = Some(e);
            }
        }
    }
    // `first` droppt am Blockende bzw. beim Early-Return — die Plane-fds
    // schließen sich selbst (Drop) und geben den GPU-Puffer frei (wichtig
    // genau dann, wenn die GPU eh schon in Speichernot ist und der User
    // mehrfach neu startet).
    let Some((vendor, node, mut importer, last_hw_raw)) = chosen else {
        return Err(
            last_err.unwrap_or_else(|| anyhow!("kein GPU-Importer für den aufgenommenen Buffer"))
        );
    };
    // Ab hier besitzt der Guard das Frame — jeder Fehlerpfad bis zum
    // Teardown gibt es automatisch frei.
    let mut last_hw = OwnedFrame(last_hw_raw);
    drop(first);
    // Stop-abbrechbar auch HIER: zwischen First-Frame und Live-Loop liegen
    // Netz-Operationen mit bis zu ~10-30 s Timeouts (TCP+TLS+RTMP-Handshake)
    // — stop()/stdin-EOF joint den Worker und fröre solange die RPC-Schleife.
    if shared.stop_requested.load(Ordering::SeqCst) {
        return Ok(None);
    }
    emit(Event::Log {
        line: format!(
            "[stream] Encode-Pfad: {} auf {}",
            if matches!(vendor, Vendor::Nvidia) { "NVENC" } else { "VAAPI" },
            display_node(&node)
        ),
    });
    if vendor != orig_vendor {
        emit(Event::Log {
            line: format!(
                "[stream] Encode-GPU auf {} umgestellt (Aufnahme liegt nicht auf Default-GPU {})",
                vendor.slug(),
                orig_vendor.slug()
            ),
        });
    }

    // 5) Encoder mit dem vom Importer vorgegebenen HW-Pixel + Frames-Kontext.
    let (hw_pixel, frames_ctx) = importer.encoder_binding();
    let cfg = EncoderConfig {
        vendor,
        codec: params.codec.clone(),
        fps: params.fps,
        bitrate_kbps: params.bitrate_kbps,
        width: out_w,
        height: out_h,
    };
    let audio_params = params.audio.enabled().then(|| AudioParams {
        sample_rate: audio::SAMPLE_RATE,
        bitrate_kbps: AUDIO_BITRATE_KBPS,
    });
    if shared.stop_requested.load(Ordering::SeqCst) {
        return Ok(None);
    }
    let (mut enc, audio_enc) =
        VideoEncoder::create_with_audio(&cfg, hw_pixel, frames_ctx, &params.push_url, audio_params)?;
    if shared.stop_requested.load(Ordering::SeqCst) {
        return Ok(None);
    }

    // 7) GEMEINSAMER Zeit-Nullpunkt für Video UND Audio (GSR-Modell): beide
    //    Spuren leiten ihre pts aus DERSELBEN Monotonic-Uhr ab → kein Drift,
    //    kein fixer Audio-Offset nötig. Direkt vor „live" gesetzt, nachdem der
    //    erste Frame bereit ist (= Content-Start).
    let record_start = Instant::now();

    // Audio-Nebenpfad starten (teilt sich den Muxer über einen MuxSender),
    // verankert an record_start + av_offset_ms.
    let mut audio_pipeline = match audio_enc {
        Some(ae) => match enc.mux_sender().and_then(|s| {
            AudioPipeline::start(ae, s, record_start, params.av_offset_ms, &params.audio)
        }) {
            Ok(p) => {
                let off = if params.av_offset_ms != 0 {
                    format!(" (av_offset={}ms)", params.av_offset_ms)
                } else {
                    String::new()
                };
                emit(Event::Log {
                    line: format!("[stream] Audio: {} → Opus{off}", params.audio.describe()),
                });
                Some(p)
            }
            Err(e) => {
                emit(Event::Log { line: format!("[stream] Audio deaktiviert ({e:#})") });
                None
            }
        },
        None => None,
    };

    // Uptime-Nullpunkt = record_start (NICHT der Worker-Start vor dem
    // Portal-Dialog) — sonst meldet `state` Minuten mehr Uptime als die
    // fps-Events, wenn der Dialog lange offen stand.
    *shared.started_at.lock().unwrap() = Some(record_start);
    shared.live.store(true, Ordering::SeqCst);
    emit(Event::State {
        state: StreamState::Live,
        running: true,
        uptime_s: 0,
    });

    let frame_interval = Duration::from_secs_f64(1.0 / params.fps.max(1) as f64);
    let mut next_tick = Instant::now();
    // Nächster erlaubter pts (strikte Monotonie-Untergrenze). Der reale pts wird
    // pro Tick aus record_start abgeleitet (s. u.), nicht simpel hochgezählt.
    let mut next_pts: i64 = 0;
    let mut window_start = Instant::now();
    let mut window_frames = 0u64;

    let run_result = (|| -> Result<()> {
        loop {
            match stop_rx.try_recv() {
                Ok(()) | Err(TryRecvError::Disconnected) => break,
                Err(TryRecvError::Empty) => {}
            }

            // Neuesten Frame abholen; Err = Capture-Quelle weg (Fenster
            // geschlossen) → SAUBERES Ende (state:stopped + stopped), kein
            // roter Fehler: das schlichte Schließen der gestreamten App ist
            // kein Fehlverhalten. (Der frühere `?` routete das in den
            // error-Terminalzustand — Widerspruch zum Fenster-zu-Fix.)
            let taken = match frames.take() {
                Ok(t) => t,
                Err(e) => {
                    emit(Event::Log { line: format!("[stream] {e:#} — Stream endet") });
                    break;
                }
            };
            if let Some(frame) = taken {
                match importer.import(&frame) {
                    Ok(hw) => {
                        last_hw.replace(hw);
                    }
                    Err(e) => emit(Event::Log {
                        line: format!("[stream] Frame-Import übersprungen: {e:#}"),
                    }),
                }
                // frame droppt hier → Plane-fds zu.
            }

            // Video-pts aus DERSELBEN Uhr wie der Audio-Anker ableiten (GSR:
            // `pts = (now - record_start) * fps`), nicht simpel hochzählen —
            // sonst driftet das sleep-basierte Pacing gegen die echte
            // Audio-Zeit. `max(next_pts)` erzwingt strikte Monotonie (falls ein
            // Tick minimal zu früh kommt).
            let clock_pts =
                (record_start.elapsed().as_secs_f64() * params.fps.max(1) as f64).round() as i64;
            let pts = clock_pts.max(next_pts);
            next_pts = pts + 1;

            // Aktuelles (ggf. dupliziertes) Bild encodieren.
            enc.send_hw(last_hw.raw(), pts)?;
            window_frames += 1;

            if window_start.elapsed() >= Duration::from_secs(1) {
                let fps = window_frames as f64 / window_start.elapsed().as_secs_f64();
                shared.fps_milli.store((fps * 1000.0) as u64, Ordering::SeqCst);
                emit(Event::Fps {
                    fps: fps.round().max(0.0) as u64,
                    uptime_s: record_start.elapsed().as_secs(),
                });
                window_start = Instant::now();
                window_frames = 0;
            }

            next_tick += frame_interval;
            let now = Instant::now();
            if next_tick > now {
                thread::sleep(next_tick - now);
            } else {
                next_tick = now;
            }
        }
        Ok(())
    })();

    // Teardown: Video- und Audio-Capture stoppen. Audio ZUERST beenden, damit
    // sein MuxSender droppt — sonst kann der Muxer-Trailer (write_trailer beim
    // Drop des letzten Senders) in enc.finish() nicht schreiben.
    cap.stop();
    if let Some(mut ap) = audio_pipeline.take() {
        ap.stop();
    }
    drop(last_hw); // GPU-Frame vor dem Encoder-Finish freigeben
    let finish_result = enc.finish();
    match (run_result, finish_result) {
        (Ok(()), Ok(())) => Ok(None),
        // Sauberes Ende, aber der Abschluss (Trailer auf totem Socket …)
        // scheiterte: der User wollte stoppen — das ist ein Log, kein roter
        // Terminalfehler.
        (Ok(()), Err(e)) => {
            emit(Event::Log {
                line: format!("[stream] Abschluss-Fehler beim Beenden (ignoriert): {e:#}"),
            });
            Ok(None)
        }
        (Err(e), _) => Err(e),
    }
}

/// Import-Kandidaten in Hersteller-Reihenfolge zu konkreten Render-Nodes
/// auflösen. AMD/Intel: JEDE Karte des Herstellers einzeln (VAAPI bindet an
/// den Node-Pfad). NVIDIA: ein Versuch genügt — der NVENC-Importer läuft über
/// CUDA und ignoriert den Node-Pfad (leerer Pfad, falls nvidia-drm keine
/// Render-Node zeigt).
fn candidate_nodes(vendor_order: &[Vendor], nodes: &[(Vendor, String)]) -> Vec<(Vendor, String)> {
    let mut out = Vec::new();
    for &v in vendor_order {
        let mut of_vendor = nodes.iter().filter(|(nv, _)| *nv == v).cloned();
        match v {
            Vendor::Nvidia => out.push(of_vendor.next().unwrap_or((v, String::new()))),
            Vendor::Amd | Vendor::Intel => out.extend(of_vendor),
        }
    }
    out
}

/// Node-Pfad fürs Log; der NVENC-Pfad hat keinen (CUDA wählt die Karte selbst).
fn display_node(node: &str) -> &str {
    if node.is_empty() { "CUDA-Default" } else { node }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    fn fresh_shared() -> Arc<Shared> {
        Arc::new(Shared {
            running: AtomicBool::new(true),
            live: AtomicBool::new(true),
            fps_milli: AtomicU64::new(0),
            started_at: Mutex::new(None),
            stop_requested: AtomicBool::new(false),
        })
    }

    /// Panict der Worker, muss der Guard `running`/`live` zurücksetzen — sonst
    /// hängt der Controller für immer in "ein Stream läuft bereits".
    #[test]
    fn worker_panic_clears_running_flag() {
        let shared = fresh_shared();
        let s2 = shared.clone();
        let h = thread::spawn(move || {
            let _guard = WorkerDoneGuard(s2);
            panic!("boom (Test)");
        });
        assert!(h.join().is_err());
        assert!(!shared.running.load(Ordering::SeqCst), "running muss nach Panic false sein");
        assert!(!shared.live.load(Ordering::SeqCst), "live muss nach Panic false sein");
    }

    /// `stop` während des Wartens auf den ersten Frame muss SOFORT abbrechen
    /// (Ok(None)), nicht erst nach dem vollen Timeout.
    #[test]
    fn wait_first_frame_aborts_on_stop() {
        let frames = FrameMailbox::new();
        let (stop_tx, stop_rx) = channel::<()>();
        stop_tx.send(()).unwrap();
        let t0 = Instant::now();
        let r = wait_first_frame(&frames, &stop_rx, Duration::from_secs(10)).unwrap();
        assert!(r.is_none(), "Stop muss Ok(None) liefern");
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "Stop muss sofort greifen, nicht erst nach dem Timeout"
        );
    }

    #[test]
    fn wait_first_frame_times_out_without_frame() {
        let frames = FrameMailbox::new();
        let (_stop_tx, stop_rx) = channel::<()>();
        assert!(wait_first_frame(&frames, &stop_rx, Duration::from_millis(200)).is_err());
    }
}

#[cfg(test)]
mod candidate_tests {
    use super::*;

    #[test]
    fn same_vendor_cards_are_separate_candidates() {
        // Der Support-Fall: Ryzen-iGPU + RX-6000-dGPU = zwei AMD-Nodes. Beide
        // müssen probiert werden, egal welche zuerst enumeriert wurde.
        let nodes = vec![
            (Vendor::Amd, "/dev/dri/renderD128".to_string()),
            (Vendor::Amd, "/dev/dri/renderD129".to_string()),
        ];
        let c = candidate_nodes(&[Vendor::Amd], &nodes);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].1, "/dev/dri/renderD128");
        assert_eq!(c[1].1, "/dev/dri/renderD129");
    }

    #[test]
    fn vendor_order_wins_over_node_order() {
        let nodes = vec![
            (Vendor::Nvidia, "/dev/dri/renderD128".to_string()),
            (Vendor::Amd, "/dev/dri/renderD129".to_string()),
        ];
        // Modifier-Hinweis sagt AMD → AMD-Node vor der NVIDIA-Karte.
        let c = candidate_nodes(&[Vendor::Amd, Vendor::Nvidia], &nodes);
        assert_eq!(c[0], (Vendor::Amd, "/dev/dri/renderD129".to_string()));
        assert_eq!(c[1], (Vendor::Nvidia, "/dev/dri/renderD128".to_string()));
    }

    #[test]
    fn nvidia_once_and_without_node_if_absent() {
        // Zwei NVIDIA-Nodes → ein Kandidat (CUDA wählt selbst); ganz ohne
        // NVIDIA-Node bleibt NVENC mit leerem Pfad probierbar.
        let nodes = vec![
            (Vendor::Nvidia, "/dev/dri/renderD128".to_string()),
            (Vendor::Nvidia, "/dev/dri/renderD129".to_string()),
        ];
        assert_eq!(candidate_nodes(&[Vendor::Nvidia], &nodes).len(), 1);
        assert_eq!(
            candidate_nodes(&[Vendor::Nvidia], &[]),
            vec![(Vendor::Nvidia, String::new())]
        );
    }
}
