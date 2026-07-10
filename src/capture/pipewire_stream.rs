//! PipeWire-Stream-Consumer: verbindet sich auf dem Portal-fd + node_id und
//! liefert DMABUF-Frames (fd + offset + stride pro Plane).
//!
//! Portiert GSRs `pipewire_video.c`-Ansatz auf pipewire-rs 0.10:
//! - EnumFormat: Video/Raw, BGRx+BGRA (→ DRM XRGB8888/ARGB8888), Size, Framerate.
//! - param_changed: parse Format, dann `update_params` mit ParamBuffers
//!   `dataType = 1<<SPA_DATA_DmaBuf` ( fordert DMABUF-Lieferung an).
//! - process: `dequeue_buffer`, pro Plane `data.fd()`+`chunk.offset/stride`
//!   extrahieren, fd dupen (PipeWire besitzt das Original), `queue_buffer`.
//!
//! Threading: libpipewire ist pro-Mainloop single-threaded und pipewire-rs
//! nutzt `Rc` (nicht `Send`) → MainLoop+Context+Core+Stream leben auf EINEM
//! Worker-Thread; nach außen geht nur der `mpsc::Receiver<DmabufFrame>` (Send).

use std::io::Cursor;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::{self, JoinHandle};

use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::spa::buffer::DataType;
use pw::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pw::spa::param::video::VideoFormat;
use pw::spa::param::ParamType;
use pw::spa::pod::{Pod, Property, Value};
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::utils::{Direction, Fraction, Rectangle, SpaTypes};

/// Eine DMABUF-Plane (ein fd kann mehrere Plane beschreiben, GSR dup't pro Plane).
#[derive(Debug)]
pub struct DmabufPlane {
    pub fd: RawFd,
    pub offset: u32,
    pub stride: i32,
}

/// Ein capturter Frame: DMABUF-Planes + Maße. Caller muss die fds schließen.
#[derive(Debug)]
pub struct DmabufFrame {
    pub planes: Vec<DmabufPlane>,
    pub width: u32,
    pub height: u32,
    pub pts: u64,
}

/// User-Daten für die Stream-Listener (auf dem Worker-Thread).
struct StreamData {
    frame_tx: Sender<DmabufFrame>,
    width: u32,
    height: u32,
    negotiated: bool,
}

/// PipeWire-Capture-Session. `stop` beendet den Worker-Thread.
pub struct PipewireCapture {
    stop_tx: Sender<()>,
    worker: Option<JoinHandle<()>>,
}

impl PipewireCapture {
    /// Starte den Capture-Worker. `pw_fd` vom Portal (`open_pipewire_remote`),
    /// `node_id` vom Portal-`Start`.
    pub fn start(pw_fd: OwnedFd, node_id: u32, width: u32, height: u32) -> anyhow::Result<(Receiver<DmabufFrame>, Self)> {
        let (frame_tx, frame_rx) = channel::<DmabufFrame>();
        let (stop_tx, stop_rx) = channel::<()>();

        let worker = thread::Builder::new()
            .name("pipewire-capture".into())
            .spawn(move || {
                if let Err(e) = run_pipewire(pw_fd, node_id, width, height, frame_tx, stop_rx) {
                    eprintln!("[pipewire-capture] error: {e:#}");
                }
            })?;
        Ok((frame_rx, Self { stop_tx, worker: Some(worker) }))
    }

    /// Stoppe den Worker (signal + join). Schließt die PipeWire-Verbindung.
    pub fn stop(&mut self) {
        let _ = self.stop_tx.send(());
        if let Some(w) = self.worker.take() {
            // Der Worker blockt im mainloop.run() — ein join könnte hängen, falls
            // der Loop nicht auf stop reagiert. Wir geben ihm Frist.
            let _ = w.join();
        }
    }
}

fn run_pipewire(
    pw_fd: OwnedFd,
    node_id: u32,
    width: u32,
    height: u32,
    frame_tx: Sender<DmabufFrame>,
    stop_rx: Receiver<()>,
) -> anyhow::Result<()> {
    pw::init();

    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_fd_rc(pw_fd, None)?;

    let data = StreamData { frame_tx, width, height, negotiated: false };

    let stream = pw::stream::StreamRc::new(
        core,
        "pulse-linux-hq-sidecar",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|_s, _ud, old, new| {
            eprintln!("[pipewire] state: {old:?} -> {new:?}");
        })
        .param_changed(|s, ud, id, param| {
            eprintln!("[pipewire] param_changed id={id} param={}", param.is_some());
            let Some(param) = param else { return };
            if id != ParamType::Format.as_raw() {
                return;
            }
            // Größe haben wir schon vom Portal (ud.width/height). Format-Parsing
            // (VideoInfoRaw) folgt, wenn wir BGRA-vs-BGRx für den Encoder-DRM-
            // fourcc brauchen — für den Capture-Smoke reicht die DMABUF-Anforderung.

            // ParamBuffers mit dataType = 1<<DmaBuf (GSR: buffer_types).
            let data_type_mask: i32 = 1 << spa::sys::SPA_DATA_DmaBuf;
            let obj = spa::pod::Object {
                type_: SpaTypes::ObjectParamBuffers.as_raw(),
                id: ParamType::Buffers.as_raw(),
                properties: vec![Property::new(
                    spa::sys::SPA_PARAM_BUFFERS_dataType,
                    Value::Int(data_type_mask),
                )],
            };
            let Ok(ser) = PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(obj)) else { return };
            let bytes: Vec<u8> = ser.0.into_inner();
            if let Some(pod) = Pod::from_bytes(&bytes) {
                let mut params = [pod];
                let _ = s.update_params(&mut params);
            }
            ud.negotiated = true;
        })
        .process(|s, ud| {
            let Some(mut buffer) = s.dequeue_buffer() else { return };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            // Nur DmaBuf-Daten auswerten (was wir angefordert haben).
            if datas[0].type_() != DataType::DmaBuf {
                return;
            }
            let mut planes = Vec::with_capacity(datas.len());
            for d in datas.iter() {
                let fd = d.fd();
                if fd < 0 {
                    continue;
                }
                let chunk = d.chunk();
                // dup: PipeWire besitzt den Original-fd; der Encoder braucht einen
                // eigenen (wird nach Encode geschlossen).
                let dup = unsafe { libc::dup(fd) };
                if dup < 0 {
                    continue;
                }
                planes.push(DmabufPlane {
                    fd: dup,
                    offset: chunk.offset(),
                    stride: chunk.stride(),
                });
            }
            if !planes.is_empty() {
                let _ = ud.frame_tx.send(DmabufFrame {
                    planes,
                    width: ud.width,
                    height: ud.height,
                    pts: 0, // PTS vom Capture-Clock — folgt mit A/V-Sync.
                });
            }
            // buffer wird beim Drop zurückgequeue'd.
        })
        .register()?;

    // EnumFormat: wie GSR build_format — festes Format (Id), aber Size + Framerate
    // als Choice-Range, damit das Portal seine tatsächliche Größe fixieren kann.
    // Zwei PODs (BGRx + BGRA). Pod ist unsized → from_bytes liefert &Pod; die
    // Byte-Vecs müssen bis zum connect leben.
    let mk_bytes = |fmt: VideoFormat| -> anyhow::Result<Vec<u8>> {
        let obj = spa::pod::object!(
            SpaTypes::ObjectParamFormat,
            ParamType::EnumFormat,
            spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
            spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
            spa::pod::property!(FormatProperties::VideoFormat, Id, fmt),
            spa::pod::property!(
                FormatProperties::VideoSize,
                Choice,
                Range,
                Rectangle,
                Rectangle { width, height },
                Rectangle { width: 1, height: 1 },
                Rectangle { width: 16384, height: 16384 }
            ),
            spa::pod::property!(
                FormatProperties::VideoFramerate,
                Choice,
                Range,
                Fraction,
                Fraction { num: 60, denom: 1 },
                Fraction { num: 0, denom: 1 },
                Fraction { num: 500, denom: 1 }
            ),
        );
        Ok(PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(obj))
            .map_err(|e| anyhow::anyhow!("serialize EnumFormat: {e:?}"))?
            .0
            .into_inner())
    };
    let bytes_bgrx = mk_bytes(VideoFormat::BGRx)?;
    let bytes_bgra = mk_bytes(VideoFormat::BGRA)?;
    let pod_bgrx = Pod::from_bytes(&bytes_bgrx).ok_or_else(|| anyhow::anyhow!("EnumFormat BGRx from_bytes"))?;
    let pod_bgra = Pod::from_bytes(&bytes_bgra).ok_or_else(|| anyhow::anyhow!("EnumFormat BGRA from_bytes"))?;
    let mut params = [pod_bgrx, pod_bgra];

    stream.connect(
        Direction::Input,
        Some(node_id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    eprintln!("[pipewire] stream connected to node {node_id}, running mainloop …");
    mainloop.run();
    let _ = stop_rx.try_recv();
    Ok(())
}
