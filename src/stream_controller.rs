//! Stream controller — besitzt die eine aktive Capture→Encode→Push-Session.
//!
//! `start` spawnt einen Worker-Thread, der die [`VideoEncoder`] + HW-Context
//! aufbaut, Frames durch den Encoder pumpt (Phase 5: synthetische Quelle, s.
//! `capture::SyntheticSource`; Phase 6: PipeWire-DMABUFs) und `state`/`fps`/
//! `error`/`stopped`-Events emittiert. `stop` signalisiert den Worker und joint
//! ihn. Der Linux-Sidecar self-exit'et nicht nach stop — er bleibt warm.
//!
//! Threading + Event-Serialisation 1:1 von mac-hq-sidecar (Single-Writer-Thread
//! via `events::emit`).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use ffmpeg_next as ffmpeg;

use crate::capture::SyntheticSource;
use crate::encode::{EncoderConfig, VideoEncoder, hw};
use crate::events;
use crate::proto::{Event, StreamState};
use crate::system::drm::{self, Vendor};

/// Aufgelöste Parameter für einen Stream (gebaut von `ops::start`).
pub struct StartParams {
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub push_url: String,
    pub enable_audio: bool,
    pub av_offset_ms: i32,
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

fn emit(event: Event) {
    if let Ok(v) = serde_json::to_value(event) {
        events::emit(v);
    }
}

impl StreamController {
    pub fn singleton() -> &'static StreamController {
        INSTANCE.get_or_init(|| StreamController { active: Mutex::new(None) })
    }

    /// Start a stream. `argv` is the redacted diagnostic argv (for `state`).
    pub fn start(&self, params: StartParams, argv: Vec<String>) -> Result<()> {
        let mut guard = self.active.lock().unwrap();
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
        let guard = self.active.lock().unwrap();
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

/// Worker body: synthetische Quelle → HW-Encode → RTMPS-Push bis stop.
fn run_stream(params: StartParams, stop_rx: Receiver<()>, shared: &Shared) -> Result<()> {
    *shared.started_at.lock().unwrap() = Some(Instant::now());
    emit(Event::State {
        state: StreamState::Starting,
        running: true,
        uptime_s: 0.0,
    });

    let (vendor, render_node) = drm::detect()
        .ok_or_else(|| anyhow!("keine DRM-Render-Node gefunden"))?;
    let kind = hw::kind_for(vendor);
    let dev_arg = if matches!(kind, hw::HwDeviceKind::Vaapi) {
        Some(render_node.as_str())
    } else {
        None
    };
    let hw_ctx = hw::HwContext::create(
        kind,
        dev_arg,
        params.width,
        params.height,
        ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_NV12,
    )?;

    let cfg = EncoderConfig {
        vendor,
        codec: params.codec.clone(),
        fps: params.fps,
        bitrate_kbps: params.bitrate_kbps,
        width: params.width,
        height: params.height,
    };
    let mut enc = VideoEncoder::create(&cfg, &hw_ctx, &params.push_url)?;

    shared.live.store(true, Ordering::SeqCst);
    let started = Instant::now();
    emit(Event::State {
        state: StreamState::Live,
        running: true,
        uptime_s: 0.0,
    });

    // Audio noch nicht (Phase 6: PipeWire-Audio + Opus). Hinweis via Log.
    if params.enable_audio {
        emit(Event::Log {
            line: "[stream] Audio angefordert, aber noch nicht implementiert (kommt mit PipeWire-Capture, Phase 6)".to_string(),
        });
    }

    // swscale BGRA→NV12 (CPU) — einmalig. Phase 6 ersetzt das durch Zero-Copy.
    let mut scaler = ffmpeg::software::scaling::Context::get(
        ffmpeg::format::Pixel::BGRA,
        params.width,
        params.height,
        ffmpeg::format::Pixel::NV12,
        params.width,
        params.height,
        ffmpeg::software::scaling::Flags::BILINEAR,
    )?;

    let mut src = SyntheticSource::new(params.width, params.height, params.fps);
    let frame_interval = src.frame_interval();
    let mut next_emit = Instant::now();
    let mut window_start = Instant::now();
    let mut window_frames = 0u64;
    let mut pts: i64 = 0;

    let run_result = (|| -> Result<()> {
        loop {
            match stop_rx.try_recv() {
                Ok(()) | Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }

            let bgra = src.next_bgra();
            let mut nv12 = ffmpeg::frame::Video::empty();
            scaler.run(&bgra, &mut nv12)?;
            nv12.set_pts(Some(pts));

            let mut hw_frame = hw_ctx.upload_swframe(&nv12, pts)?;
            enc.send_hw(hw_frame, pts)?;
            unsafe { ffmpeg::ffi::av_frame_free(&mut hw_frame) };
            pts += 1;
            window_frames += 1;

            if window_start.elapsed() >= Duration::from_secs(1) {
                let fps = window_frames as f64 / window_start.elapsed().as_secs_f64();
                shared.fps_milli.store((fps * 1000.0) as u64, Ordering::SeqCst);
                emit(Event::Fps { fps, uptime_s: started.elapsed().as_secs_f64() });
                window_start = Instant::now();
                window_frames = 0;
            }

            next_emit += frame_interval;
            let now = Instant::now();
            if next_emit > now {
                thread::sleep(next_emit - now);
            } else {
                next_emit = now;
            }
        }
        Ok(())
    })();

    let finish_result = enc.finish();
    run_result.and(finish_result)
}

// `Vendor` wird für künftige Vendor-Checks im Modul gebraucht.
#[allow(unused_imports)]
use Vendor as _Vendor;
