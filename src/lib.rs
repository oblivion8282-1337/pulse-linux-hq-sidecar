//! Pulse Linux HQ-streaming sidecar — library crate.
//!
//! Wire-äquivalent zu `streaming/gsr-sidecar/control.py` (Linux/Python) und
//! `streaming/{win,mac}-hq-sidecar/` (Rust): eine JSON-Zeile pro stdin = Request,
//! eine JSON-Zeile pro stdout = Response (spiegelt `id`) oder Event (`{"ev":...}`,
//! kein `id`). Siehe `streaming/README.md` für das Protokoll.
//!
//! Stack: PipeWire/Portal-Capture (Wayland) → VAAPI (AMD/Intel) / NVENC (Nvidia)
//! via ffmpeg-next als Bibliothek → FLV-Mux → RTMPS-Push an MediaMTX. Kein
//! externes `gpu-screen-recorder`-Binary mehr (der Umweg des Python-GSR-Sidecars).
//!
//! `main.rs` ist ein dünner Binary-Wrapper über diesen Modulen (Layout wie
//! mac-hq-sidecar). Siehe den Plan und das README für die Roadmap.

pub mod caps;
pub mod capture;
pub mod dispatch;
pub mod encode;
pub mod events;
pub mod ops;
pub mod profiles;
pub mod proto;
pub mod stream_controller;
pub mod system;
