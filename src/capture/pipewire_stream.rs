//! PipeWire-Stream-Consumer: verbindet sich auf dem Portal-fd + node_id und
//! liefert DMABUF-Frames (fd + offset + stride pro Plane).
//!
//! Portiert GSRs `pipewire_video.c`-Ansatz auf pipewire-rs 0.10:
//! - EnumFormat: Video/Raw, BGRx+BGRA (→ DRM XRGB8888/ARGB8888), Size, Framerate
//!   — UND pro Format die via EGL abgefragten DRM-Modifier als Choice-Enum
//!   (`SPA_FORMAT_VIDEO_modifier`, Flags MANDATORY|DONT_FIXATE). Mutter/niri
//!   liefern DMABUF nur mit expliziten Modifiern; ohne die Property matcht
//!   kein Format ("no more input formats"). Zusätzlich je ein POD ohne
//!   Modifier als SHM-Fallback für andere Compositors.
//! - param_changed(Format): ist der Modifier noch ein Choice (DONT_FIXATE-
//!   Tanz aus der PipeWire-DMABUF-Doku), fixieren wir auf den Default und
//!   re-announcen die EnumFormats. Ist er fixiert: `VideoInfoRaw` parsen
//!   (echte Größe/Format/Modifier) und ParamBuffers senden
//!   (`dataType = 1<<SPA_DATA_DmaBuf` bzw. MemFd/MemPtr ohne Modifier).
//! - process: `dequeue_buffer`, pro Plane `data.fd()`+`chunk.offset/stride`
//!   extrahieren, fd dupen (PipeWire besitzt das Original), `queue_buffer`.
//!
//! Threading: libpipewire ist pro-Mainloop single-threaded und pipewire-rs
//! nutzt `Rc` (nicht `Send`) → MainLoop+Context+Core+Stream leben auf EINEM
//! Worker-Thread; nach außen geht nur der `mpsc::Receiver<DmabufFrame>` (Send).

use std::io::Cursor;
use std::os::fd::{OwnedFd, RawFd};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::{self, JoinHandle};

use drm_fourcc::DrmFourcc;
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::spa::buffer::DataType;
use pw::spa::param::ParamType;
use pw::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pw::spa::param::video::{VideoFormat, VideoInfoRaw};
use pw::spa::pod::deserialize::PodDeserializer;
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{ChoiceValue, Pod, Property, PropertyFlags, Value};
use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Direction, Fraction, Rectangle, SpaTypes};

use super::egl_modifiers;

/// Eine DMABUF-Plane (ein fd kann mehrere Plane beschreiben, GSR dup't pro Plane).
#[derive(Debug)]
pub struct DmabufPlane {
    pub fd: RawFd,
    pub offset: u32,
    pub stride: i32,
}

/// Die Plane BESITZT ihren dup'ten fd — Drop schließt ihn. Damit leaken auch
/// Frames nicht, die nie einen Consumer erreichen (Kanal voll, Receiver weg,
/// beim Stop noch gequeue'd) — vorher Aufgabe des Callers (`close_planes`),
/// was genau diese Pfade übersah.
impl Drop for DmabufPlane {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

/// Ein capturter Frame: DMABUF-Planes + Maße + negotiiertes DRM-Format.
/// Die fds gehören den Planes und schließen sich beim Drop.
#[derive(Debug)]
pub struct DmabufFrame {
    pub planes: Vec<DmabufPlane>,
    pub width: u32,
    pub height: u32,
    /// DRM-Fourcc des negotiierten Formats (XRGB8888/ARGB8888).
    pub drm_fourcc: u32,
    /// DRM-Format-Modifier des Buffers (für av_hwframe_map / CUDA-Import).
    pub modifier: u64,
    pub pts: u64,
    /// Stabile Identität des zugrundeliegenden PipeWire-Buffers (Hash über die
    /// ORIGINAL-fds + Offsets, vor dem dup). Der Compositor reicht dieselben
    /// 2–8 Buffer im Kreis — der NVENC-Importer cachet EGLImage+GL-Textur pro
    /// Buffer statt sie pro Frame neu zu bauen. 0 = kein Key (nicht cachen).
    pub buffer_key: u64,
    /// Buffer-Generation: hochgezählt bei jedem `remove_buffer` und jeder
    /// Format-Neuverhandlung. Wechselt die Epoche, wirft der Importer seinen
    /// Cache komplett weg — schützt vor fd-Nummern-Recycling (gleiche Nummer,
    /// anderer Buffer).
    pub epoch: u64,
}

/// User-Daten für die Stream-Listener (auf dem Worker-Thread).
struct StreamData {
    frame_tx: SyncSender<DmabufFrame>,
    width: u32,
    height: u32,
    drm_fourcc: u32,
    modifier: u64,
    /// Serialisierte EnumFormat-PODs — für den Re-Announce beim Fixieren.
    enum_format_bytes: Vec<Vec<u8>>,
    /// Bereits als fixiert announcter Modifier (Schleifen-Guard: schickt der
    /// Server danach weiter ein Choice, akzeptieren wir den Default statt
    /// endlos zu re-announcen).
    announced_modifier: Option<i64>,
    shm_warned: bool,
    /// Buffer-Generation für den Importer-Cache (s. `DmabufFrame::epoch`).
    epoch: u64,
}

/// FNV-1a über die Original-fds + Offsets eines Buffers — stabiler Cache-Key,
/// solange der Buffer lebt (fd-Nummern sind prozessweit eindeutig, solange
/// offen; Recycling nach Buffer-Abbau fängt die `epoch` ab).
fn buffer_key_of(planes: impl Iterator<Item = (i32, u32)>) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |v: u64| {
        h ^= v;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    for (fd, offset) in planes {
        mix(fd as u64);
        mix(offset as u64);
    }
    // 0 ist als "kein Key" reserviert.
    if h == 0 { 1 } else { h }
}

/// PipeWire-Capture-Session. `stop` beendet den Worker-Thread.
pub struct PipewireCapture {
    /// pw::channel — weckt den Mainloop cross-thread (mpsc könnte das nicht:
    /// `mainloop.run()` blockt und pollt keine fremden Channels).
    stop_tx: pw::channel::Sender<()>,
    worker: Option<JoinHandle<()>>,
}

impl PipewireCapture {
    /// Starte den Capture-Worker. `pw_fd` vom Portal (`open_pipewire_remote`),
    /// `node_id` vom Portal-`Start`.
    pub fn start(pw_fd: OwnedFd, node_id: u32, width: u32, height: u32) -> anyhow::Result<(Receiver<DmabufFrame>, Self)> {
        // Bounded: stallt der Consumer (RTMPS-Backpressure, Encoder hängt),
        // dürfen sich nicht unbegrenzt Frames mit je 1–4 dup'ten DMABUF-fds
        // stapeln (EMFILE). `try_send` im process-Callback verwirft dann den
        // ältesten Zustand nicht — der neue Frame droppt (fds schließen sich)
        // und der Consumer holt beim nächsten Drain den letzten gequeue'ten.
        let (frame_tx, frame_rx) = sync_channel::<DmabufFrame>(8);
        let (stop_tx, stop_rx) = pw::channel::channel::<()>();

        let worker = thread::Builder::new()
            .name("pipewire-capture".into())
            .spawn(move || {
                if let Err(e) = run_pipewire(pw_fd, node_id, width, height, frame_tx, stop_rx) {
                    tracing::error!(target: "pipewire", "Capture-Thread: {e:#}");
                }
            })?;
        Ok((frame_rx, Self { stop_tx, worker: Some(worker) }))
    }

    /// Stoppe den Worker (Mainloop-quit + join). Schließt die
    /// PipeWire-Verbindung. Idempotent.
    pub fn stop(&mut self) {
        let _ = self.stop_tx.send(());
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

/// Ohne Drop liefe der Capture-Thread nach jedem Fehl-Start (kein Frame in
/// 10s, kein GPU-Importer, Encoder-open scheitert) für immer weiter — mit
/// offener Portal-Session (Screenshare-Indikator an) und fd-dup pro Frame.
/// Die frühen `return Err`-Pfade in `run_stream` erreichen `cap.stop()` nie;
/// Drop macht das Teardown Pfad-unabhängig.
impl Drop for PipewireCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// SPA-VideoFormat → DRM-Fourcc (GSR spa_video_format_to_drm_format).
fn video_format_to_drm_fourcc(fmt: VideoFormat) -> Option<DrmFourcc> {
    match fmt {
        VideoFormat::BGRx => Some(DrmFourcc::Xrgb8888),
        VideoFormat::BGRA => Some(DrmFourcc::Argb8888),
        _ => None,
    }
}

/// Baue ein EnumFormat-POD. Mit `modifiers` kommt die Modifier-Property als
/// Choice-Enum von Longs mit MANDATORY|DONT_FIXATE dazu (DMABUF-Verhandlung
/// laut PipeWire-DMABUF-Doku); ohne bleibt es ein SHM-taugliches Format.
fn build_format_pod(
    fmt: VideoFormat,
    modifiers: Option<&[u64]>,
    width: u32,
    height: u32,
) -> anyhow::Result<Vec<u8>> {
    let mut properties = vec![
        spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(FormatProperties::VideoFormat, Id, fmt),
    ];
    if let Some(mods) = modifiers.filter(|m| !m.is_empty()) {
        properties.push(Property {
            key: FormatProperties::VideoModifier.as_raw(),
            flags: PropertyFlags::MANDATORY | PropertyFlags::DONT_FIXATE,
            value: Value::Choice(ChoiceValue::Long(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Enum {
                    default: mods[0] as i64,
                    alternatives: mods.iter().map(|&m| m as i64).collect(),
                },
            ))),
        });
    }
    properties.push(spa::pod::property!(
        FormatProperties::VideoSize,
        Choice,
        Range,
        Rectangle,
        Rectangle { width, height },
        Rectangle { width: 1, height: 1 },
        Rectangle { width: 16384, height: 16384 }
    ));
    properties.push(spa::pod::property!(
        FormatProperties::VideoFramerate,
        Choice,
        Range,
        Fraction,
        Fraction { num: 60, denom: 1 },
        Fraction { num: 0, denom: 1 },
        // Deckt den vollen fps-Bereich des Encoders ab (clamp 1..=1000 in
        // ops::start) — der Compositor liefert eh nur bei Damage.
        Fraction { num: 1000, denom: 1 }
    ));

    let obj = spa::pod::Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties,
    };
    Ok(PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(obj))
        .map_err(|e| anyhow::anyhow!("serialize EnumFormat: {e:?}"))?
        .0
        .into_inner())
}

/// Zustand der Modifier-Property in einem vom Server geschickten Format.
enum ModifierState {
    /// Keine Modifier-Property → SHM-Format.
    Absent,
    /// Fixierter Modifier (plain Long oder `Choice None` — SPA stellt
    /// fixierte Werte oft als None-Choice dar).
    Fixed(i64),
    /// Noch unfixiert (DONT_FIXATE-Flag oder Enum mit >1 Alternativen) —
    /// Client muss fixieren und re-announcen (PipeWire-DMABUF-Doku).
    Unfixated(i64),
}

fn modifier_state(properties: &[Property]) -> ModifierState {
    let Some(prop) = properties
        .iter()
        .find(|p| p.key == spa::sys::SPA_FORMAT_VIDEO_modifier)
    else {
        return ModifierState::Absent;
    };
    let dont_fixate = prop.flags.contains(PropertyFlags::DONT_FIXATE);
    match &prop.value {
        Value::Long(v) => ModifierState::Fixed(*v),
        Value::Choice(ChoiceValue::Long(Choice(_, choice))) => match choice {
            ChoiceEnum::None(v) => ModifierState::Fixed(*v),
            ChoiceEnum::Enum { default, alternatives } => {
                if dont_fixate || alternatives.len() > 1 {
                    ModifierState::Unfixated(*default)
                } else {
                    ModifierState::Fixed(*default)
                }
            }
            ChoiceEnum::Range { default, .. } | ChoiceEnum::Step { default, .. } => {
                ModifierState::Unfixated(*default)
            }
            ChoiceEnum::Flags { default, .. } => ModifierState::Fixed(*default),
        },
        _ => ModifierState::Absent,
    }
}

/// ParamBuffers-POD: welche Buffer-Datentypen wir akzeptieren.
fn build_buffers_pod(data_type_mask: i32) -> Option<Vec<u8>> {
    let obj = spa::pod::Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: ParamType::Buffers.as_raw(),
        properties: vec![Property::new(
            spa::sys::SPA_PARAM_BUFFERS_dataType,
            Value::Int(data_type_mask),
        )],
    };
    let ser = PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(obj)).ok()?;
    Some(ser.0.into_inner())
}

fn run_pipewire(
    pw_fd: OwnedFd,
    node_id: u32,
    width: u32,
    height: u32,
    frame_tx: SyncSender<DmabufFrame>,
    stop_rx: pw::channel::Receiver<()>,
) -> anyhow::Result<()> {
    pw::init();

    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    // Stop-Signal (cross-thread) → Mainloop beenden. Der AttachedReceiver
    // muss bis zum Loop-Ende leben.
    let _stop_receiver = stop_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_fd_rc(pw_fd, None)?;

    // Modifier pro DRM-Fourcc via EGL (wie GSR eglQueryDmaBufModifiersEXT).
    let formats = [VideoFormat::BGRx, VideoFormat::BGRA];
    let fourccs: Vec<u32> = formats
        .iter()
        .filter_map(|&f| video_format_to_drm_fourcc(f))
        .map(|f| f as u32)
        .collect();
    let modifier_map = egl_modifiers::query_dmabuf_modifiers(&fourccs);
    for (fourcc, mods) in &modifier_map {
        tracing::debug!(
            target: "pipewire",
            "fourcc {:#010x}: {} Modifier ({:#018x} …)",
            fourcc,
            mods.len(),
            mods.first().copied().unwrap_or(0)
        );
    }

    // EnumFormat-PODs: pro Format erst die DMABUF-Variante (mit Modifier-
    // Choice), dann die SHM-Fallback-Variante ohne Modifier. Die Byte-Vecs
    // wandern in StreamData, damit param_changed sie beim Fixieren
    // re-announcen kann (Pod ist unsized — from_bytes liefert nur &Pod).
    let mut enum_format_bytes: Vec<Vec<u8>> = Vec::new();
    for &fmt in &formats {
        let fourcc = video_format_to_drm_fourcc(fmt).map(|f| f as u32);
        let mods = fourcc.and_then(|f| modifier_map.get(&f)).cloned().unwrap_or_default();
        enum_format_bytes.push(build_format_pod(fmt, Some(&mods), width, height)?);
    }
    for &fmt in &formats {
        enum_format_bytes.push(build_format_pod(fmt, None, width, height)?);
    }

    let data = StreamData {
        frame_tx,
        width,
        height,
        drm_fourcc: 0,
        modifier: 0,
        enum_format_bytes: enum_format_bytes.clone(),
        announced_modifier: None,
        shm_warned: false,
        epoch: 0,
    };

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
        .state_changed({
            let mainloop = mainloop.clone();
            move |_s, _ud, old, new| {
                tracing::debug!(target: "pipewire", "PW-State: {old:?} -> {new:?}");
                // Quelle weg (gestreamtes Fenster geschlossen, Compositor trennt):
                // Streaming/Paused → Unconnected oder Error. Mainloop beenden →
                // Capture-Thread endet → frame_tx droppt → der Pacing-Loop sieht
                // Disconnected und beendet den Stream sauber, statt das letzte
                // Bild ewig zu duplizieren (Zuschauer-Standbild bei weiter
                // „Live"). `Paused` selbst ist KEIN Ende (transient bei
                // Neuverhandlung/Minimieren); der initiale Unconnected-Zustand
                // (old = Connecting) auch nicht.
                use pw::stream::StreamState as S;
                let source_died = matches!(new, S::Error(_))
                    || (matches!(new, S::Unconnected)
                        && matches!(old, S::Streaming | S::Paused));
                if source_died {
                    tracing::warn!(
                        target: "pipewire",
                        "Stream-Quelle beendet ({old:?} -> {new:?}) — Capture endet"
                    );
                    mainloop.quit();
                }
            }
        })
        .param_changed(|s, ud, id, param| {
            tracing::debug!(target: "pipewire", "param_changed id={id} param={}", param.is_some());
            let Some(param) = param else { return };
            if id != ParamType::Format.as_raw() {
                return;
            }

            let Ok((_, value)) = PodDeserializer::deserialize_any_from(param.as_bytes()) else {
                tracing::warn!(target: "pipewire", "Format-Deserialize fehlgeschlagen");
                return;
            };
            let Value::Object(mut obj) = value else { return };

            // DONT_FIXATE-Tanz (PipeWire-DMABUF-Doku): schickt der Server den
            // Modifier noch unfixiert, fixieren wir auf den Default und
            // re-announcen die EnumFormats (fixiertes zuerst, Originale als
            // Fallback). Guard: haben wir GENAU diesen Modifier schon
            // announcet und der Server schickt weiter ein Choice (z. B. als
            // None-Choice serialisiertes Echo), akzeptieren wir den Default
            // statt endlos zu re-announcen.
            let state = modifier_state(&obj.properties);
            let (has_modifier, modifier) = match state {
                ModifierState::Absent => (false, 0i64),
                ModifierState::Fixed(v) => (true, v),
                ModifierState::Unfixated(default) => {
                    if ud.announced_modifier != Some(default) {
                        ud.announced_modifier = Some(default);
                        tracing::debug!(
                            target: "pipewire",
                            "Format mit Modifier-Choice → fixiere auf {default:#018x}"
                        );
                        obj.id = ParamType::EnumFormat.as_raw();
                        if let Some(prop) = obj
                            .properties
                            .iter_mut()
                            .find(|p| p.key == spa::sys::SPA_FORMAT_VIDEO_modifier)
                        {
                            prop.value = Value::Long(default);
                            prop.flags = PropertyFlags::MANDATORY;
                        }
                        let Ok(ser) =
                            PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(obj))
                        else {
                            return;
                        };
                        let fixated = ser.0.into_inner();
                        let mut pods: Vec<&Pod> =
                            Vec::with_capacity(1 + ud.enum_format_bytes.len());
                        if let Some(p) = Pod::from_bytes(&fixated) {
                            pods.push(p);
                        }
                        pods.extend(
                            ud.enum_format_bytes.iter().filter_map(|b| Pod::from_bytes(b)),
                        );
                        if let Err(e) = s.update_params(&mut pods) {
                            // Verhandlung bliebe sonst still stehen (kein Frame,
                            // kein Fehler) — mindestens die Diagnose loggen.
                            tracing::warn!(target: "pipewire", "update_params (Fixierung) fehlgeschlagen: {e}");
                        }
                        return;
                    }
                    tracing::debug!(
                        target: "pipewire",
                        "Modifier-Choice nach Fixierung — akzeptiere Default {default:#018x}"
                    );
                    (true, default)
                }
            };

            // Fixiertes Format: echte Größe/Format/Modifier übernehmen.
            // Neuverhandlung = neue Buffer → Importer-Cache-Epoche wechseln.
            ud.epoch += 1;
            let mut info = VideoInfoRaw::new();
            if info.parse(param).is_err() {
                tracing::warn!(target: "pipewire", "Format-Parse fehlgeschlagen");
                return;
            }
            ud.width = info.size().width;
            ud.height = info.size().height;
            ud.modifier = modifier as u64;
            ud.drm_fourcc = video_format_to_drm_fourcc(info.format())
                .map(|f| f as u32)
                .unwrap_or(0);
            tracing::info!(
                target: "pipewire",
                format = ?info.format(),
                width = ud.width,
                height = ud.height,
                modifier = format!("{:#018x}", ud.modifier),
                dmabuf = has_modifier,
                "Format fixiert"
            );

            // ParamBuffers: mit Modifier ist DMABUF verhandelt → nur DmaBuf
            // anfordern; ohne Modifier SHM (MemFd/MemPtr).
            let data_type_mask: i32 = if has_modifier {
                1 << spa::sys::SPA_DATA_DmaBuf
            } else {
                (1 << spa::sys::SPA_DATA_MemFd) | (1 << spa::sys::SPA_DATA_MemPtr)
            };
            let Some(buffers) = build_buffers_pod(data_type_mask) else { return };
            if let Some(pod) = Pod::from_bytes(&buffers) {
                let mut params = [pod];
                if let Err(e) = s.update_params(&mut params) {
                    tracing::warn!(target: "pipewire", "update_params (ParamBuffers) fehlgeschlagen: {e}");
                }
            }
        })
        .remove_buffer(|_s, ud, _buf| {
            // Buffer wird abgebaut → seine fd-Nummern können recycelt werden.
            // Epoche wechseln, damit der Importer-Cache nicht auf einen
            // NEUEN Buffer mit ALTER fd-Nummer zeigt.
            ud.epoch += 1;
        })
        .process(|s, ud| {
            let Some(mut buffer) = s.dequeue_buffer() else { return };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            // Nur DmaBuf-Daten auswerten (was wir angefordert haben). SHM
            // (MemFd/MemPtr) folgt, wenn ein Compositor ohne DMABUF auftaucht.
            if datas[0].type_() != DataType::DmaBuf {
                if !ud.shm_warned {
                    ud.shm_warned = true;
                    tracing::warn!(
                        target: "pipewire",
                        "Buffer-Typ {:?} (kein DmaBuf) — SHM-Consumer noch nicht implementiert",
                        datas[0].type_()
                    );
                }
                return;
            }
            // Cache-Key aus den ORIGINAL-fds (stabil pro Buffer) — vor dem dup.
            let buffer_key = buffer_key_of(
                datas.iter().filter(|d| d.fd() >= 0).map(|d| (d.fd(), d.chunk().offset())),
            );
            let mut planes = Vec::with_capacity(datas.len());
            for d in datas.iter() {
                let fd = d.fd();
                if fd < 0 {
                    continue;
                }
                let chunk = d.chunk();
                // dup (CLOEXEC — Kinder sollen keine in-flight DMABUF-fds
                // erben): PipeWire besitzt den Original-fd; der Encoder braucht
                // einen eigenen (Plane-Drop schließt ihn nach dem Encode).
                let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
                if dup < 0 {
                    // Teil-dup'te Frames NICHT senden: fehlt eine Plane,
                    // verrutschen die Indizes und der Importer cachet unter
                    // `buffer_key` dauerhaft ein falsch gebautes EGLImage.
                    // Bereits dup'te fds schließt der Plane-Drop.
                    tracing::warn!(target: "pipewire", "F_DUPFD_CLOEXEC fehlgeschlagen — Frame verworfen");
                    return;
                }
                planes.push(DmabufPlane {
                    fd: dup,
                    offset: chunk.offset(),
                    stride: chunk.stride(),
                });
            }
            if !planes.is_empty() {
                // Voll oder Receiver weg → Frame droppt, Plane-fds schließen sich.
                let _ = ud.frame_tx.try_send(DmabufFrame {
                    planes,
                    width: ud.width,
                    height: ud.height,
                    drm_fourcc: ud.drm_fourcc,
                    modifier: ud.modifier,
                    pts: 0, // PTS vom Capture-Clock — folgt mit A/V-Sync.
                    buffer_key,
                    epoch: ud.epoch,
                });
            }
            // buffer wird beim Drop zurückgequeue'd.
        })
        .register()?;

    let pods: Vec<&Pod> = enum_format_bytes
        .iter()
        .filter_map(|b| Pod::from_bytes(b))
        .collect();
    let mut params = pods;

    stream.connect(
        Direction::Input,
        Some(node_id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    tracing::info!(target: "pipewire", node_id, "Stream verbunden, Mainloop läuft");
    mainloop.run();
    tracing::debug!(target: "pipewire", "Mainloop beendet (stop)");
    Ok(())
}

#[cfg(test)]
mod frame_drop_tests {
    use super::*;

    fn fd_is_open(fd: RawFd) -> bool {
        (unsafe { libc::fcntl(fd, libc::F_GETFD) }) != -1
    }

    /// Ein gedroppter Frame muss seine (dup'ten) fds schließen — Frames, die im
    /// Kanal verworfen werden (Stop, toter Receiver, voller Kanal), leaken sonst.
    #[test]
    fn dropping_a_frame_closes_its_plane_fds() {
        let mut fds = [0 as RawFd; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let frame = DmabufFrame {
            planes: vec![
                DmabufPlane { fd: fds[0], offset: 0, stride: 0 },
                DmabufPlane { fd: fds[1], offset: 0, stride: 0 },
            ],
            width: 1,
            height: 1,
            drm_fourcc: 0,
            modifier: 0,
            pts: 0,
            buffer_key: 1,
            epoch: 0,
        };
        drop(frame);
        assert!(!fd_is_open(fds[0]), "Plane-fd 0 muss nach Drop geschlossen sein");
        assert!(!fd_is_open(fds[1]), "Plane-fd 1 muss nach Drop geschlossen sein");
    }
}
