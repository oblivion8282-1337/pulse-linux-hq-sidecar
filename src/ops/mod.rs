//! Op handlers — one module per JSON-RPC op.
//!
//! Every handler is a free function `fn handle(params) -> Result<Map>`. Sync.
//!
//! Implementierungs-Status (Roadmap siehe Plan):
//!
//! | Op                     | Status          | Real-impl unlocks                       |
//! |------------------------|-----------------|-----------------------------------------|
//! | health                 | static          | DRM-Vendor + VAAPI/NVENC-Codec-Probe    |
//! | gpu_info               | stub            | DRM-Vendor + codec_query (vaapi/nvenc)  |
//! | list_profiles          | real            | ported from profiles.py (H264+AV1)      |
//! | list_monitors          | stub (`[]`)     | PipeWire/Portal-Display-Enumeration     |
//! | list_windows           | stub (`[]`)     | wlr-foreign-toplevel / Portal           |
//! | list_application_audio | stub (`[]`)     | PipeWire-Node-Enumeration                |
//! | build_argv             | real            | diagnostic argv (token-redacted)        |
//! | start                  | stub (error)    | PipeWire + VAAPI/NVENC + RTMPS (Phase 5)|
//! | stop                   | idempotent      | StreamController                        |
//! | state                  | idle            | StreamController snapshot               |

pub mod build_argv;
pub mod gpu_info;
pub mod health;
pub mod list_application_audio;
pub mod list_monitors;
pub mod list_profiles;
pub mod list_windows;
pub mod start;
pub mod state;
pub mod stop;
