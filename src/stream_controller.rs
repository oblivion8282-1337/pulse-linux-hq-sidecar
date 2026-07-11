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
use crate::capture::pipewire_stream::{DmabufFrame, PipewireCapture};
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
        let (rx, cap) = AudioCapture::start(selection)?;
        let worker = thread::Builder::new()
            .name("hq-audio-encode".into())
            .spawn(move || {
                // Der erste Sample-Batch verankert die Audio-Zeitlinie: sein
                // Empfangszeitpunkt relativ zu record_start (in Samples) wird
                // der pts des ersten Opus-Frames. So beginnt Audio bei genau der
                // Video-Zeit, zu der es wirklich einsetzt — kein fixer Offset
                // (GSR schaltet den bei Livestream auch ab: force_no_audio_offset).
                let offset_samples =
                    av_offset_ms as i64 * audio::SAMPLE_RATE as i64 / 1000;
                let mut anchored = false;
                while let Ok(samples) = rx.recv() {
                    let anchor = if anchored {
                        0 // nach dem ersten push ignoriert AudioEncoder den Wert
                    } else {
                        anchored = true;
                        let elapsed = record_start.elapsed().as_secs_f64();
                        (elapsed * audio::SAMPLE_RATE as f64) as i64 + offset_samples
                    };
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
        });
        let shared_worker = shared.clone();
        let worker = thread::Builder::new()
            .name("hq-stream".into())
            .spawn(move || {
                let result = run_stream(params, stop_rx, &shared_worker);
                shared_worker.running.store(false, Ordering::SeqCst);
                shared_worker.live.store(false, Ordering::SeqCst);
                if let Err(e) = result {
                    emit(Event::Error { message: format!("{e:#}") });
                    emit(Event::State {
                        state: StreamState::Error,
                        running: false,
                        uptime_s: 0.0,
                    });
                }
                emit(Event::State {
                    state: StreamState::Stopped,
                    running: false,
                    uptime_s: 0.0,
                });
                emit(Event::Stopped { code: None });
            })
            .map_err(|e| anyhow!("spawn hq-stream thread: {e}"))?;

        *guard = Some(Active { stop_tx, worker, shared, argv });
        Ok(())
    }

    /// Stop the active stream (idempotent). Blocks until the worker finished.
    pub fn stop(&self) -> Result<()> {
        let active = self.active.lock().unwrap().take();
        if let Some(active) = active {
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
fn run_stream(params: StartParams, stop_rx: Receiver<()>, shared: &Shared) -> Result<()> {
    *shared.started_at.lock().unwrap() = Some(Instant::now());
    emit(Event::State {
        state: StreamState::Starting,
        running: true,
        uptime_s: 0.0,
    });

    let (vendor, render_node) =
        drm::detect().ok_or_else(|| anyhow!("keine DRM-Render-Node gefunden"))?;

    // 1) Portal-Dialog: User wählt Monitor/Fenster. Blockt bis zur Auswahl.
    emit(Event::Log {
        line: "[stream] öffne Portal-Dialog zur Quellenauswahl …".to_string(),
    });
    let session = portal::open(params.show_cursor).map_err(|e| {
        if portal::is_portal_canceled(&e) {
            anyhow!("Quellenauswahl abgebrochen")
        } else {
            anyhow!("Portal-Verhandlung: {e:#}")
        }
    })?;
    emit(Event::Log {
        line: format!(
            "[stream] Quelle gewählt: node={} {}x{}",
            session.node_id, session.width, session.height
        ),
    });

    // 2) PipeWire-Capture auf fd + node_id starten.
    let (rx, mut cap) = PipewireCapture::start(
        session.pw_fd,
        session.node_id,
        session.width,
        session.height,
    )?;

    // 3) Auf den ersten DMABUF-Frame warten → verbindliche (negotiierte) Maße.
    let first = rx
        .recv_timeout(Duration::from_secs(10))
        .map_err(|_| anyhow!("kein Bild vom Compositor in 10s (ist die Quelle sichtbar?)"))?;
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

    // 4) Vendor-spezifischen Importer bauen. NVENC: BGR0-HW-Pool + CUDA-Interop.
    //    VAAPI: DRM_PRIME→scale_vaapi-Filtergraph (NV12-Ausgang).
    let mut importer = match vendor {
        Vendor::Nvidia => {
            // RGB0 (nicht BGR0): der GL-Blit im Importer kopiert komponenten-
            // weise BGRx→RGBA8, die Staging-Bytes liegen danach als R,G,B,X.
            let hw_ctx = hw::HwContext::create(
                hw::HwDeviceKind::Cuda,
                None,
                out_w,
                out_h,
                ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_RGB0,
            )?;
            let imp = NvDmabufImporter::new(out_w, out_h)?;
            FrameImporter::Nvenc { imp, hw: hw_ctx }
        }
        Vendor::Amd | Vendor::Intel => {
            emit(Event::Log {
                line: "[stream] VAAPI-Capture-Pfad (AMD/Intel) — auf dieser Hardware nicht getestet".to_string(),
            });
            let imp = VaapiImporter::new(
                &render_node,
                first.drm_fourcc,
                width,
                height,
                params.fps,
                out_w,
                out_h,
            )?;
            FrameImporter::Vaapi { imp }
        }
    };

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
    let (mut enc, audio_enc) =
        VideoEncoder::create_with_audio(&cfg, hw_pixel, frames_ctx, &params.push_url, audio_params)?;

    // 6) Ersten Frame importieren → last_hw ist die Duplikationsquelle.
    let mut last_hw: *mut ffmpeg::ffi::AVFrame = importer.import(&first)?;
    close_planes(&first);

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

    shared.live.store(true, Ordering::SeqCst);
    emit(Event::State {
        state: StreamState::Live,
        running: true,
        uptime_s: 0.0,
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

            // Alle seit dem letzten Tick eingetroffenen Frames abholen, nur den
            // neuesten behalten (Damage kann mehrere geliefert haben; ältere
            // wären ohnehin veraltet). fds der verworfenen Frames schließen.
            let mut newest: Option<DmabufFrame> = None;
            loop {
                match rx.try_recv() {
                    Ok(f) => {
                        if let Some(old) = newest.replace(f) {
                            close_planes(&old);
                        }
                    }
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                }
            }
            if let Some(frame) = newest {
                match importer.import(&frame) {
                    Ok(hw) => {
                        unsafe { ffmpeg::ffi::av_frame_free(&mut last_hw) };
                        last_hw = hw;
                    }
                    Err(e) => emit(Event::Log {
                        line: format!("[stream] Frame-Import übersprungen: {e:#}"),
                    }),
                }
                close_planes(&frame);
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
            enc.send_hw(last_hw, pts)?;
            window_frames += 1;

            if window_start.elapsed() >= Duration::from_secs(1) {
                let fps = window_frames as f64 / window_start.elapsed().as_secs_f64();
                shared.fps_milli.store((fps * 1000.0) as u64, Ordering::SeqCst);
                emit(Event::Fps { fps, uptime_s: record_start.elapsed().as_secs_f64() });
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
    unsafe {
        if !last_hw.is_null() {
            ffmpeg::ffi::av_frame_free(&mut last_hw);
        }
    }
    let finish_result = enc.finish();
    run_result.and(finish_result)
}

/// DMABUF-fds eines Frames schließen (wir besitzen die dup'ten fds).
fn close_planes(f: &DmabufFrame) {
    for p in &f.planes {
        unsafe { libc::close(p.fd) };
    }
}
