//! DRM-Format-Modifier-Abfrage via EGL (`eglQueryDmaBufModifiersEXT`).
//!
//! Mutter/niri liefern ScreenCast-DMABUFs nur mit expliziten Modifiern — ein
//! EnumFormat OHNE `SPA_FORMAT_VIDEO_modifier`-Choice wird abgelehnt
//! ("no more input formats"). GSR löst das identisch: die vom GPU-Treiber
//! unterstützten Modifier pro DRM-Fourcc via EGL abfragen und als Choice-Enum
//! anbieten (`pipewire_video.c` / `egl.c`).
//!
//! libEGL wird per dlopen geladen (kein Link-Time-Dep); Display über
//! `EGL_EXT_platform_device` (headless, kein Wayland-Connect nötig — NVIDIA
//! und Mesa unterstützen beide die Device-Plattform). Wir vereinigen die
//! Modifier ALLER Devices: zu viel anzubieten ist harmlos (der Compositor
//! schneidet auf seine Menge), zu wenig lässt die Verhandlung scheitern.

use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_void};

/// `DRM_FORMAT_MOD_INVALID` — "impliziter Modifier" (Treiber-intern).
pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
/// `DRM_FORMAT_MOD_LINEAR`.
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

const EGL_PLATFORM_DEVICE_EXT: u32 = 0x313F;
const EGL_EXTENSIONS: i32 = 0x3055;
const EGL_TRUE: u32 = 1;

type EglDisplay = *mut c_void;
type EglDeviceExt = *mut c_void;

type FnGetProcAddress = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type FnQueryDevicesExt = unsafe extern "C" fn(i32, *mut EglDeviceExt, *mut i32) -> u32;
type FnGetPlatformDisplayExt =
    unsafe extern "C" fn(u32, *mut c_void, *const i32) -> EglDisplay;
type FnInitialize = unsafe extern "C" fn(EglDisplay, *mut i32, *mut i32) -> u32;
type FnTerminate = unsafe extern "C" fn(EglDisplay) -> u32;
type FnQueryString = unsafe extern "C" fn(EglDisplay, i32) -> *const c_char;
type FnQueryDmaBufModifiersExt =
    unsafe extern "C" fn(EglDisplay, i32, i32, *mut u64, *mut u32, *mut i32) -> u32;

/// Frage die unterstützten DRM-Modifier pro Fourcc ab (Union über alle
/// EGL-Devices). Liefert für jeden angefragten Fourcc einen Eintrag; Fourccs
/// ohne Treiber-Modifier bekommen den Fallback `[LINEAR, INVALID]`, damit die
/// Verhandlung zumindest versucht werden kann.
pub fn query_dmabuf_modifiers(fourccs: &[u32]) -> HashMap<u32, Vec<u64>> {
    let mut out: HashMap<u32, Vec<u64>> = HashMap::new();

    if let Err(e) = query_into(fourccs, &mut out) {
        eprintln!("[egl-modifiers] EGL-Abfrage fehlgeschlagen ({e}) — Fallback LINEAR/INVALID");
    }

    for &fourcc in fourccs {
        let mods = out.entry(fourcc).or_default();
        if mods.is_empty() {
            mods.push(DRM_FORMAT_MOD_LINEAR);
        }
        // Impliziter Modifier immer als letzte Alternative (OBS macht das
        // ebenso) — erlaubt Compositors ohne explizite Modifier den Match.
        if !mods.contains(&DRM_FORMAT_MOD_INVALID) {
            mods.push(DRM_FORMAT_MOD_INVALID);
        }
    }
    out
}

fn query_into(fourccs: &[u32], out: &mut HashMap<u32, Vec<u64>>) -> Result<(), String> {
    unsafe {
        let lib = libloading::Library::new("libEGL.so.1")
            .or_else(|_| libloading::Library::new("libEGL.so"))
            .map_err(|e| format!("libEGL laden: {e}"))?;

        let get_proc: libloading::Symbol<FnGetProcAddress> = lib
            .get(b"eglGetProcAddress\0")
            .map_err(|e| format!("eglGetProcAddress: {e}"))?;
        let initialize: libloading::Symbol<FnInitialize> =
            lib.get(b"eglInitialize\0").map_err(|e| format!("eglInitialize: {e}"))?;
        let terminate: libloading::Symbol<FnTerminate> =
            lib.get(b"eglTerminate\0").map_err(|e| format!("eglTerminate: {e}"))?;
        let query_string: libloading::Symbol<FnQueryString> =
            lib.get(b"eglQueryString\0").map_err(|e| format!("eglQueryString: {e}"))?;

        let load = |name: &CStr| -> *mut c_void { get_proc(name.as_ptr()) };
        let query_devices = load(c"eglQueryDevicesEXT");
        let get_platform_display = load(c"eglGetPlatformDisplayEXT");
        let query_modifiers = load(c"eglQueryDmaBufModifiersEXT");
        if query_devices.is_null() || get_platform_display.is_null() || query_modifiers.is_null()
        {
            return Err("EGL-Extensions (device platform / dmabuf modifiers) fehlen".into());
        }
        let query_devices: FnQueryDevicesExt = std::mem::transmute(query_devices);
        let get_platform_display: FnGetPlatformDisplayExt =
            std::mem::transmute(get_platform_display);
        let query_modifiers: FnQueryDmaBufModifiersExt = std::mem::transmute(query_modifiers);

        let mut devices = [std::ptr::null_mut(); 16];
        let mut n_devices: i32 = 0;
        if query_devices(devices.len() as i32, devices.as_mut_ptr(), &mut n_devices) != EGL_TRUE
        {
            return Err("eglQueryDevicesEXT fehlgeschlagen".into());
        }

        let mut any_display = false;
        for &dev in devices.iter().take(n_devices.max(0) as usize) {
            let dpy = get_platform_display(EGL_PLATFORM_DEVICE_EXT, dev, std::ptr::null());
            if dpy.is_null() {
                continue;
            }
            let (mut major, mut minor) = (0i32, 0i32);
            if initialize(dpy, &mut major, &mut minor) != EGL_TRUE {
                continue;
            }
            let exts = query_string(dpy, EGL_EXTENSIONS);
            let has_modifiers = !exts.is_null()
                && CStr::from_ptr(exts)
                    .to_string_lossy()
                    .contains("EGL_EXT_image_dma_buf_import_modifiers");
            if has_modifiers {
                any_display = true;
                for &fourcc in fourccs {
                    let mut count: i32 = 0;
                    if query_modifiers(dpy, fourcc as i32, 0, std::ptr::null_mut(),
                        std::ptr::null_mut(), &mut count) != EGL_TRUE || count <= 0
                    {
                        continue;
                    }
                    let mut mods = vec![0u64; count as usize];
                    let mut external = vec![0u32; count as usize];
                    let mut written: i32 = 0;
                    if query_modifiers(dpy, fourcc as i32, count, mods.as_mut_ptr(),
                        external.as_mut_ptr(), &mut written) == EGL_TRUE
                    {
                        mods.truncate(written.max(0) as usize);
                        let entry = out.entry(fourcc).or_default();
                        for m in mods {
                            if !entry.contains(&m) {
                                entry.push(m);
                            }
                        }
                    }
                }
            }
            terminate(dpy);
        }

        if !any_display {
            return Err("kein EGL-Display mit dmabuf-modifier-Support gefunden".into());
        }
        Ok(())
    }
}
