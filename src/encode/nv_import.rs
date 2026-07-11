//! Zero-Copy-Import: DMABUF (PipeWire-Capture) → ffmpeg-CUDA-Frame (NVENC).
//!
//! niri/Mutter liefern auf NVIDIA Block-Linear-DMABUFs (Modifier
//! `0x03...`) — reines `cuImportExternalMemory` kann nur lineare Layouts,
//! deshalb der GSR-Weg über die Grafik-Treiber-Interop-Kette:
//!
//! DMABUF-fds → `eglCreateImageKHR` (EGL_LINUX_DMA_BUF_EXT + Modifier)
//!   → GL-Textur (`glEGLImageTargetTexture2DOES`)
//!   → `glCopyImageSubData` in eine EIGENE RGBA8-Staging-Textur — CUDA kann
//!     EGLImage-gebundene Texturen nicht registrieren (INVALID_VALUE), GSR
//!     kopiert deshalb ebenfalls erst in eigene Texturen
//!   → `cuGraphicsGLRegisterImage` (einmalig, auf der Staging-Textur) /
//!     `cuGraphicsSubResourceGetMappedArray`
//!   → `cuMemcpy2D` (ARRAY→DEVICE) in den linearen ffmpeg-CUDA-Frame
//!     (sw_format BGR0 — NVENC nimmt RGB direkt, keine CPU-Kopie nötig).
//!
//! Der GPU-seitige Copy detiled dabei Block-Linear→Linear; ein CPU-Roundtrip
//! findet nie statt. Voraussetzung: FFmpegs CUDA-Device nutzt den
//! **Primary-Context** (Flag in `hw::HwContext::create`), denn unser Interop
//! läuft ebenfalls auf dem Primary-Context — Device-Pointer sind pro Context.
//!
//! libEGL/libcuda werden per dlopen geladen (kein Link-Time-Dep, wie
//! `egl_modifiers`). EGL-Display über `EGL_EXT_platform_device`, Context
//! surfaceless + configless (EGL_KHR_no_config_context /
//! EGL_KHR_surfaceless_context — NVIDIA kann beides). Devices werden
//! durchprobiert, bis eins einen NVIDIA-GL-Context liefert.
//!
//! Threading: EGL-Context ist thread-affin (`eglMakeCurrent` in `new`) —
//! Importer auf DEM Thread erzeugen und benutzen, der encodiert. Bewusst
//! nicht `Send`.

use std::ffi::{CStr, c_char, c_void};
use std::ptr;

use anyhow::{Result, anyhow};
use ffmpeg_next::ffi::{AVFrame, av_frame_free};

use crate::capture::pipewire_stream::DmabufFrame;
use crate::capture::egl_modifiers::DRM_FORMAT_MOD_INVALID;
use super::hw::HwContext;

// ── EGL/GL-Konstanten (eglext.h / gl.h) ─────────────────────────────────────
const EGL_PLATFORM_DEVICE_EXT: u32 = 0x313F;
const EGL_OPENGL_API: u32 = 0x30A2;
const EGL_NONE: i32 = 0x3038;
const EGL_TRUE: u32 = 1;
const EGL_WIDTH: i32 = 0x3057;
const EGL_HEIGHT: i32 = 0x3056;
const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: i32 = 0x3271;
// fd/offset/pitch pro Plane 0..3 (Plane 3 liegt bei 0x3440ff).
const EGL_DMA_BUF_PLANE_FD_EXT: [i32; 4] = [0x3272, 0x3275, 0x3278, 0x3440];
const EGL_DMA_BUF_PLANE_OFFSET_EXT: [i32; 4] = [0x3273, 0x3276, 0x3279, 0x3441];
const EGL_DMA_BUF_PLANE_PITCH_EXT: [i32; 4] = [0x3274, 0x3277, 0x327A, 0x3442];
const EGL_DMA_BUF_PLANE_MODIFIER_LO_EXT: [i32; 4] = [0x3443, 0x3445, 0x3447, 0x3449];
const EGL_DMA_BUF_PLANE_MODIFIER_HI_EXT: [i32; 4] = [0x3444, 0x3446, 0x3448, 0x344A];

const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_VERSION: u32 = 0x1F02;
const GL_NO_ERROR: u32 = 0;
const GL_RGBA8: u32 = 0x8058;
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
const GL_NEAREST: i32 = 0x2600;
// Framebuffer-Blit (Downscale-Pfad): Quelle ≠ Zielgröße → glBlitFramebuffer
// mit LINEAR-Filter statt glCopyImageSubData (das kann nur 1:1).
const GL_READ_FRAMEBUFFER: u32 = 0x8CA8;
const GL_DRAW_FRAMEBUFFER: u32 = 0x8CA9;
const GL_FRAMEBUFFER: u32 = 0x8D40;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
const GL_COLOR_BUFFER_BIT: u32 = 0x0000_4000;
const GL_LINEAR: u32 = 0x2601;

// ── CUDA-Konstanten/-Typen (cuda.h) ─────────────────────────────────────────
const CUDA_SUCCESS: i32 = 0;
const CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY: u32 = 1;
const CU_MEMORYTYPE_DEVICE: u32 = 2;
const CU_MEMORYTYPE_ARRAY: u32 = 3;

type EglDisplay = *mut c_void;
type EglContext = *mut c_void;
type EglImage = *mut c_void;
type CuContext = *mut c_void;
type CuArray = *mut c_void;
type CuGraphicsResource = *mut c_void;

/// `CUDA_MEMCPY2D` (cuda.h, v2-ABI: alle Größen `size_t`).
#[repr(C)]
struct CudaMemcpy2D {
    src_x_in_bytes: usize,
    src_y: usize,
    src_memory_type: u32,
    src_host: *const c_void,
    src_device: u64,
    src_array: CuArray,
    src_pitch: usize,
    dst_x_in_bytes: usize,
    dst_y: usize,
    dst_memory_type: u32,
    dst_host: *mut c_void,
    dst_device: u64,
    dst_array: CuArray,
    dst_pitch: usize,
    width_in_bytes: usize,
    height: usize,
}

// ── Funktions-Signaturen ────────────────────────────────────────────────────
type FnGetProcAddress = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type FnEglQueryDevices = unsafe extern "C" fn(i32, *mut *mut c_void, *mut i32) -> u32;
type FnEglGetPlatformDisplay = unsafe extern "C" fn(u32, *mut c_void, *const i32) -> EglDisplay;
type FnEglInitialize = unsafe extern "C" fn(EglDisplay, *mut i32, *mut i32) -> u32;
type FnEglTerminate = unsafe extern "C" fn(EglDisplay) -> u32;
type FnEglBindApi = unsafe extern "C" fn(u32) -> u32;
type FnEglCreateContext =
    unsafe extern "C" fn(EglDisplay, *mut c_void, EglContext, *const i32) -> EglContext;
type FnEglDestroyContext = unsafe extern "C" fn(EglDisplay, EglContext) -> u32;
type FnEglMakeCurrent =
    unsafe extern "C" fn(EglDisplay, *mut c_void, *mut c_void, EglContext) -> u32;
type FnEglCreateImage =
    unsafe extern "C" fn(EglDisplay, EglContext, u32, *mut c_void, *const i32) -> EglImage;
type FnEglDestroyImage = unsafe extern "C" fn(EglDisplay, EglImage) -> u32;
type FnEglGetError = unsafe extern "C" fn() -> i32;

type FnGlGenTextures = unsafe extern "C" fn(i32, *mut u32);
type FnGlDeleteTextures = unsafe extern "C" fn(i32, *const u32);
type FnGlBindTexture = unsafe extern "C" fn(u32, u32);
type FnGlEglImageTargetTexture2D = unsafe extern "C" fn(u32, EglImage);
type FnGlGetError = unsafe extern "C" fn() -> u32;
type FnGlGetString = unsafe extern "C" fn(u32) -> *const c_char;
type FnGlTexStorage2D = unsafe extern "C" fn(u32, i32, u32, i32, i32);
type FnGlTexParameteri = unsafe extern "C" fn(u32, u32, i32);
#[allow(clippy::type_complexity)]
type FnGlCopyImageSubData = unsafe extern "C" fn(
    u32, u32, i32, i32, i32, i32, // src: name, target, level, x, y, z
    u32, u32, i32, i32, i32, i32, // dst: name, target, level, x, y, z
    i32, i32, i32, // width, height, depth
);
type FnGlGenFramebuffers = unsafe extern "C" fn(i32, *mut u32);
type FnGlDeleteFramebuffers = unsafe extern "C" fn(i32, *const u32);
type FnGlBindFramebuffer = unsafe extern "C" fn(u32, u32);
type FnGlFramebufferTexture2D = unsafe extern "C" fn(u32, u32, u32, u32, i32);
#[allow(clippy::type_complexity)]
type FnGlBlitFramebuffer = unsafe extern "C" fn(
    i32, i32, i32, i32, // src x0 y0 x1 y1
    i32, i32, i32, i32, // dst x0 y0 x1 y1
    u32, u32, // mask, filter
);

type FnCuInit = unsafe extern "C" fn(u32) -> i32;
type FnCuDeviceGet = unsafe extern "C" fn(*mut i32, i32) -> i32;
type FnCuPrimaryCtxRetain = unsafe extern "C" fn(*mut CuContext, i32) -> i32;
type FnCuPrimaryCtxRelease = unsafe extern "C" fn(i32) -> i32;
type FnCuCtxPushCurrent = unsafe extern "C" fn(CuContext) -> i32;
type FnCuCtxPopCurrent = unsafe extern "C" fn(*mut CuContext) -> i32;
type FnCuGraphicsGlRegisterImage =
    unsafe extern "C" fn(*mut CuGraphicsResource, u32, u32, u32) -> i32;
type FnCuGraphicsMapResources =
    unsafe extern "C" fn(u32, *mut CuGraphicsResource, *mut c_void) -> i32;
type FnCuGraphicsSubResourceGetMappedArray =
    unsafe extern "C" fn(*mut CuArray, CuGraphicsResource, u32, u32) -> i32;
type FnCuGraphicsUnmapResources =
    unsafe extern "C" fn(u32, *mut CuGraphicsResource, *mut c_void) -> i32;
type FnCuGraphicsUnregisterResource = unsafe extern "C" fn(CuGraphicsResource) -> i32;
type FnCuMemcpy2D = unsafe extern "C" fn(*const CudaMemcpy2D) -> i32;
type FnCuCtxSynchronize = unsafe extern "C" fn() -> i32;

/// Staging-Textur samt (einmalig) registrierter CUDA-Resource.
struct Staging {
    tex: u32,
    width: u32,
    height: u32,
    cu_res: CuGraphicsResource,
}

/// DMABUF→CUDA-Importer. Hält EGL-Display+Context (current auf dem
/// erzeugenden Thread) und den retained CUDA-Primary-Context.
pub struct NvDmabufImporter {
    // Libraries müssen so lange leben wie die Funktions-Pointer.
    _egl_lib: libloading::Library,
    _cuda_lib: libloading::Library,

    dpy: EglDisplay,
    ctx: EglContext,

    egl_terminate: FnEglTerminate,
    egl_destroy_context: FnEglDestroyContext,
    egl_make_current: FnEglMakeCurrent,
    egl_create_image: FnEglCreateImage,
    egl_destroy_image: FnEglDestroyImage,
    egl_get_error: FnEglGetError,

    gl_gen_textures: FnGlGenTextures,
    gl_delete_textures: FnGlDeleteTextures,
    gl_bind_texture: FnGlBindTexture,
    gl_image_target_texture: FnGlEglImageTargetTexture2D,
    gl_get_error: FnGlGetError,
    gl_tex_storage_2d: FnGlTexStorage2D,
    gl_tex_parameteri: FnGlTexParameteri,
    gl_copy_image_sub_data: FnGlCopyImageSubData,
    gl_gen_framebuffers: FnGlGenFramebuffers,
    gl_delete_framebuffers: FnGlDeleteFramebuffers,
    gl_bind_framebuffer: FnGlBindFramebuffer,
    gl_framebuffer_texture_2d: FnGlFramebufferTexture2D,
    gl_blit_framebuffer: FnGlBlitFramebuffer,

    /// RGBA8-Staging-Textur (einmal bei CUDA registriert) — Ziel des
    /// GPU-Copies/-Blits aus der EGLImage-Textur, Quelle des cuMemcpy2D.
    /// Hat IMMER die Ausgabe-Größe (`out_w`×`out_h`) — weicht die Capture-
    /// Größe ab, skaliert der Blit (LINEAR) beim Kopieren.
    staging: Option<Staging>,
    /// FBO-Paar für den Blit-Pfad (read = EGLImage-Textur, draw = Staging).
    fbos: [u32; 2],
    out_w: u32,
    out_h: u32,

    cu_device: i32,
    cu_ctx: CuContext,
    cu_primary_ctx_release: FnCuPrimaryCtxRelease,
    cu_ctx_push: FnCuCtxPushCurrent,
    cu_ctx_pop: FnCuCtxPopCurrent,
    cu_register_image: FnCuGraphicsGlRegisterImage,
    cu_map_resources: FnCuGraphicsMapResources,
    cu_get_mapped_array: FnCuGraphicsSubResourceGetMappedArray,
    cu_unmap_resources: FnCuGraphicsUnmapResources,
    cu_unregister_resource: FnCuGraphicsUnregisterResource,
    cu_memcpy_2d: FnCuMemcpy2D,
    cu_ctx_synchronize: FnCuCtxSynchronize,
}

macro_rules! egl_proc {
    ($get:expr, $name:literal, $ty:ty) => {{
        let p = $get(concat!($name, "\0").as_ptr() as *const c_char);
        if p.is_null() {
            return Err(anyhow!(concat!("eglGetProcAddress(", $name, ") → NULL")));
        }
        std::mem::transmute::<*mut c_void, $ty>(p)
    }};
}

macro_rules! cu_sym {
    ($lib:expr, $name:literal, $ty:ty) => {{
        let s: libloading::Symbol<$ty> = $lib
            .get(concat!($name, "\0").as_bytes())
            .map_err(|e| anyhow!(concat!("libcuda: ", $name, ": {}"), e))?;
        *s
    }};
}

impl NvDmabufImporter {
    /// Lade libEGL+libcuda, baue GL-Context auf dem NVIDIA-Device und retaine
    /// den CUDA-Primary-Context. MUSS auf dem Thread laufen, der auch
    /// `import` ruft (eglMakeCurrent ist thread-affin).
    ///
    /// `out_w`/`out_h`: Ausgabe-Größe (= Encoder-Größe). Weicht die Capture-
    /// Größe davon ab, skaliert der Import per Framebuffer-Blit auf der GPU.
    pub fn new(out_w: u32, out_h: u32) -> Result<Self> {
        unsafe {
            let egl_lib = libloading::Library::new("libEGL.so.1")
                .or_else(|_| libloading::Library::new("libEGL.so"))
                .map_err(|e| anyhow!("libEGL laden: {e}"))?;
            let cuda_lib = libloading::Library::new("libcuda.so.1")
                .or_else(|_| libloading::Library::new("libcuda.so"))
                .map_err(|e| anyhow!("libcuda laden: {e} — NVIDIA-Treiber installiert?"))?;

            let get_proc: libloading::Symbol<FnGetProcAddress> = egl_lib
                .get(b"eglGetProcAddress\0")
                .map_err(|e| anyhow!("eglGetProcAddress: {e}"))?;
            let get_proc = *get_proc;

            let egl_initialize = egl_proc!(get_proc, "eglInitialize", FnEglInitialize);
            let egl_terminate = egl_proc!(get_proc, "eglTerminate", FnEglTerminate);
            let egl_bind_api = egl_proc!(get_proc, "eglBindAPI", FnEglBindApi);
            let egl_create_context = egl_proc!(get_proc, "eglCreateContext", FnEglCreateContext);
            let egl_destroy_context =
                egl_proc!(get_proc, "eglDestroyContext", FnEglDestroyContext);
            let egl_make_current = egl_proc!(get_proc, "eglMakeCurrent", FnEglMakeCurrent);
            let egl_get_error = egl_proc!(get_proc, "eglGetError", FnEglGetError);
            let egl_query_devices = egl_proc!(get_proc, "eglQueryDevicesEXT", FnEglQueryDevices);
            let egl_get_platform_display =
                egl_proc!(get_proc, "eglGetPlatformDisplayEXT", FnEglGetPlatformDisplay);
            let egl_create_image = egl_proc!(get_proc, "eglCreateImageKHR", FnEglCreateImage);
            let egl_destroy_image = egl_proc!(get_proc, "eglDestroyImageKHR", FnEglDestroyImage);

            let gl_gen_textures = egl_proc!(get_proc, "glGenTextures", FnGlGenTextures);
            let gl_delete_textures = egl_proc!(get_proc, "glDeleteTextures", FnGlDeleteTextures);
            let gl_bind_texture = egl_proc!(get_proc, "glBindTexture", FnGlBindTexture);
            let gl_image_target_texture = egl_proc!(
                get_proc,
                "glEGLImageTargetTexture2DOES",
                FnGlEglImageTargetTexture2D
            );
            let gl_get_error = egl_proc!(get_proc, "glGetError", FnGlGetError);
            let gl_get_string = egl_proc!(get_proc, "glGetString", FnGlGetString);
            let gl_tex_storage_2d = egl_proc!(get_proc, "glTexStorage2D", FnGlTexStorage2D);
            let gl_tex_parameteri = egl_proc!(get_proc, "glTexParameteri", FnGlTexParameteri);
            let gl_copy_image_sub_data =
                egl_proc!(get_proc, "glCopyImageSubData", FnGlCopyImageSubData);
            let gl_gen_framebuffers =
                egl_proc!(get_proc, "glGenFramebuffers", FnGlGenFramebuffers);
            let gl_delete_framebuffers =
                egl_proc!(get_proc, "glDeleteFramebuffers", FnGlDeleteFramebuffers);
            let gl_bind_framebuffer = egl_proc!(get_proc, "glBindFramebuffer", FnGlBindFramebuffer);
            let gl_framebuffer_texture_2d =
                egl_proc!(get_proc, "glFramebufferTexture2D", FnGlFramebufferTexture2D);
            let gl_blit_framebuffer =
                egl_proc!(get_proc, "glBlitFramebuffer", FnGlBlitFramebuffer);

            // NVIDIA-Device suchen: Kandidaten durchprobieren, Context bauen,
            // GL_VENDOR prüfen. (Mesa-Devices würden bei
            // cuGraphicsGLRegisterImage scheitern.)
            let mut devices = [ptr::null_mut(); 16];
            let mut n_devices: i32 = 0;
            if egl_query_devices(devices.len() as i32, devices.as_mut_ptr(), &mut n_devices)
                != EGL_TRUE
            {
                return Err(anyhow!("eglQueryDevicesEXT fehlgeschlagen"));
            }

            let mut found: Option<(EglDisplay, EglContext)> = None;
            for &dev in devices.iter().take(n_devices.max(0) as usize) {
                let dpy = egl_get_platform_display(EGL_PLATFORM_DEVICE_EXT, dev, ptr::null());
                if dpy.is_null() {
                    continue;
                }
                let (mut major, mut minor) = (0i32, 0i32);
                if egl_initialize(dpy, &mut major, &mut minor) != EGL_TRUE {
                    continue;
                }
                if egl_bind_api(EGL_OPENGL_API) != EGL_TRUE {
                    egl_terminate(dpy);
                    continue;
                }
                // configless (EGL_KHR_no_config_context) + surfaceless.
                let attribs = [EGL_NONE];
                let ctx =
                    egl_create_context(dpy, ptr::null_mut(), ptr::null_mut(), attribs.as_ptr());
                if ctx.is_null() {
                    egl_terminate(dpy);
                    continue;
                }
                if egl_make_current(dpy, ptr::null_mut(), ptr::null_mut(), ctx) != EGL_TRUE {
                    egl_destroy_context(dpy, ctx);
                    egl_terminate(dpy);
                    continue;
                }
                let version_ptr = gl_get_string(GL_VERSION);
                let vendor = if version_ptr.is_null() {
                    String::new()
                } else {
                    CStr::from_ptr(version_ptr).to_string_lossy().into_owned()
                };
                tracing::debug!(target: "nvenc", "EGL-Device-GL: {vendor}");
                if vendor.to_ascii_lowercase().contains("nvidia") {
                    found = Some((dpy, ctx));
                    break;
                }
                egl_make_current(dpy, ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
                egl_destroy_context(dpy, ctx);
                egl_terminate(dpy);
            }
            let (dpy, ctx) = found
                .ok_or_else(|| anyhow!("kein EGL-Device mit NVIDIA-GL-Context gefunden"))?;

            // CUDA: Primary-Context retainen (denselben nutzt FFmpeg via
            // AV_CUDA_USE_PRIMARY_CONTEXT).
            let cu_init = cu_sym!(cuda_lib, "cuInit", FnCuInit);
            let cu_device_get = cu_sym!(cuda_lib, "cuDeviceGet", FnCuDeviceGet);
            let cu_primary_ctx_retain =
                cu_sym!(cuda_lib, "cuDevicePrimaryCtxRetain", FnCuPrimaryCtxRetain);
            let cu_primary_ctx_release =
                cu_sym!(cuda_lib, "cuDevicePrimaryCtxRelease", FnCuPrimaryCtxRelease);
            let cu_ctx_push = cu_sym!(cuda_lib, "cuCtxPushCurrent_v2", FnCuCtxPushCurrent);
            let cu_ctx_pop = cu_sym!(cuda_lib, "cuCtxPopCurrent_v2", FnCuCtxPopCurrent);
            let cu_register_image =
                cu_sym!(cuda_lib, "cuGraphicsGLRegisterImage", FnCuGraphicsGlRegisterImage);
            let cu_map_resources =
                cu_sym!(cuda_lib, "cuGraphicsMapResources", FnCuGraphicsMapResources);
            let cu_get_mapped_array = cu_sym!(
                cuda_lib,
                "cuGraphicsSubResourceGetMappedArray",
                FnCuGraphicsSubResourceGetMappedArray
            );
            let cu_unmap_resources =
                cu_sym!(cuda_lib, "cuGraphicsUnmapResources", FnCuGraphicsUnmapResources);
            let cu_unregister_resource = cu_sym!(
                cuda_lib,
                "cuGraphicsUnregisterResource",
                FnCuGraphicsUnregisterResource
            );
            let cu_memcpy_2d = cu_sym!(cuda_lib, "cuMemcpy2D_v2", FnCuMemcpy2D);
            let cu_ctx_synchronize = cu_sym!(cuda_lib, "cuCtxSynchronize", FnCuCtxSynchronize);

            let r = cu_init(0);
            if r != CUDA_SUCCESS {
                return Err(anyhow!("cuInit failed (rc={r})"));
            }
            let mut cu_device: i32 = 0;
            let r = cu_device_get(&mut cu_device, 0);
            if r != CUDA_SUCCESS {
                return Err(anyhow!("cuDeviceGet(0) failed (rc={r})"));
            }
            let mut cu_ctx: CuContext = ptr::null_mut();
            let r = cu_primary_ctx_retain(&mut cu_ctx, cu_device);
            if r != CUDA_SUCCESS || cu_ctx.is_null() {
                return Err(anyhow!("cuDevicePrimaryCtxRetain failed (rc={r})"));
            }

            let mut me = Self {
                _egl_lib: egl_lib,
                _cuda_lib: cuda_lib,
                dpy,
                ctx,
                egl_terminate,
                egl_destroy_context,
                egl_make_current,
                egl_create_image,
                egl_destroy_image,
                egl_get_error,
                gl_gen_textures,
                gl_delete_textures,
                gl_bind_texture,
                gl_image_target_texture,
                gl_get_error,
                gl_tex_storage_2d,
                gl_tex_parameteri,
                gl_copy_image_sub_data,
                gl_gen_framebuffers,
                gl_delete_framebuffers,
                gl_bind_framebuffer,
                gl_framebuffer_texture_2d,
                gl_blit_framebuffer,
                staging: None,
                fbos: [0; 2],
                out_w,
                out_h,
                cu_device,
                cu_ctx,
                cu_primary_ctx_release,
                cu_ctx_push,
                cu_ctx_pop,
                cu_register_image,
                cu_map_resources,
                cu_get_mapped_array,
                cu_unmap_resources,
                cu_unregister_resource,
                cu_memcpy_2d,
                cu_ctx_synchronize,
            };
            // FBO-Paar für den Skalier-Blit + Staging in Ausgabe-Größe —
            // beides einmalig (Context ist current auf diesem Thread).
            (me.gl_gen_framebuffers)(2, me.fbos.as_mut_ptr());
            me.ensure_staging()?;
            Ok(me)
        }
    }

    /// Importiere einen DMABUF-Frame in ein frisches HW-Frame aus dem Pool
    /// (sw_format muss BGR0 sein — 4 Byte/Pixel). Die fds des DmabufFrame
    /// bleiben beim Caller (er schließt sie nach dem Import).
    /// Caller besitzt das zurückgegebene Frame (`av_frame_free`).
    pub fn import(&mut self, frame: &DmabufFrame, hw: &HwContext) -> Result<*mut AVFrame> {
        self.ensure_staging()?;
        let mut dst = hw.alloc_hwframe()?;
        match self.copy_into(frame, dst) {
            Ok(()) => Ok(dst),
            Err(e) => {
                unsafe { av_frame_free(&mut dst) };
                Err(e)
            }
        }
    }

    /// Staging-Textur (RGBA8, Ausgabe-Größe) anlegen und einmalig bei CUDA
    /// registrieren.
    fn ensure_staging(&mut self) -> Result<()> {
        let (width, height) = (self.out_w, self.out_h);
        if let Some(s) = &self.staging {
            if s.width == width && s.height == height {
                return Ok(());
            }
        }
        self.drop_staging();
        unsafe {
            let mut tex: u32 = 0;
            (self.gl_gen_textures)(1, &mut tex);
            (self.gl_bind_texture)(GL_TEXTURE_2D, tex);
            // NEAREST → Textur ist ohne Mipmaps "complete" (Register-Vorgabe).
            (self.gl_tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            (self.gl_tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            (self.gl_tex_storage_2d)(GL_TEXTURE_2D, 1, GL_RGBA8, width as i32, height as i32);
            (self.gl_bind_texture)(GL_TEXTURE_2D, 0);
            let gl_err = (self.gl_get_error)();
            if gl_err != GL_NO_ERROR {
                (self.gl_delete_textures)(1, &tex);
                return Err(anyhow!("Staging-Textur anlegen failed (glError={gl_err:#06x})"));
            }

            let r = (self.cu_ctx_push)(self.cu_ctx);
            if r != CUDA_SUCCESS {
                (self.gl_delete_textures)(1, &tex);
                return Err(anyhow!("cuCtxPushCurrent failed (rc={r})"));
            }
            let mut res: CuGraphicsResource = ptr::null_mut();
            let r = (self.cu_register_image)(
                &mut res,
                tex,
                GL_TEXTURE_2D,
                CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY,
            );
            let mut old: CuContext = ptr::null_mut();
            (self.cu_ctx_pop)(&mut old);
            if r != CUDA_SUCCESS {
                (self.gl_delete_textures)(1, &tex);
                return Err(anyhow!("cuGraphicsGLRegisterImage(staging) failed (rc={r})"));
            }
            self.staging = Some(Staging { tex, width, height, cu_res: res });
        }
        Ok(())
    }

    fn drop_staging(&mut self) {
        if let Some(s) = self.staging.take() {
            unsafe {
                if (self.cu_ctx_push)(self.cu_ctx) == CUDA_SUCCESS {
                    (self.cu_unregister_resource)(s.cu_res);
                    let mut old: CuContext = ptr::null_mut();
                    (self.cu_ctx_pop)(&mut old);
                }
                (self.gl_delete_textures)(1, &s.tex);
            }
        }
    }

    fn copy_into(&self, frame: &DmabufFrame, dst: *mut AVFrame) -> Result<()> {
        if frame.planes.is_empty() || frame.planes.len() > 4 {
            return Err(anyhow!("DmabufFrame mit {} Planes", frame.planes.len()));
        }

        unsafe {
            // EGLImage aus den DMABUF-Planes (+ Modifier, außer INVALID).
            let mut attribs: Vec<i32> = vec![
                EGL_WIDTH,
                frame.width as i32,
                EGL_HEIGHT,
                frame.height as i32,
                EGL_LINUX_DRM_FOURCC_EXT,
                frame.drm_fourcc as i32,
            ];
            for (i, plane) in frame.planes.iter().enumerate() {
                attribs.extend_from_slice(&[
                    EGL_DMA_BUF_PLANE_FD_EXT[i],
                    plane.fd,
                    EGL_DMA_BUF_PLANE_OFFSET_EXT[i],
                    plane.offset as i32,
                    EGL_DMA_BUF_PLANE_PITCH_EXT[i],
                    plane.stride,
                ]);
                if frame.modifier != DRM_FORMAT_MOD_INVALID {
                    attribs.extend_from_slice(&[
                        EGL_DMA_BUF_PLANE_MODIFIER_LO_EXT[i],
                        (frame.modifier & 0xFFFF_FFFF) as i32,
                        EGL_DMA_BUF_PLANE_MODIFIER_HI_EXT[i],
                        (frame.modifier >> 32) as i32,
                    ]);
                }
            }
            attribs.push(EGL_NONE);

            let image = (self.egl_create_image)(
                self.dpy,
                ptr::null_mut(), // EGL_NO_CONTEXT bei DMA_BUF-Import
                EGL_LINUX_DMA_BUF_EXT,
                ptr::null_mut(),
                attribs.as_ptr(),
            );
            if image.is_null() {
                return Err(anyhow!(
                    "eglCreateImageKHR(dmabuf) failed (eglError={:#06x})",
                    (self.egl_get_error)()
                ));
            }
            // Ab hier: image freigeben bei jedem Ausstieg.
            let result = self.copy_image(image, frame, dst);
            (self.egl_destroy_image)(self.dpy, image);
            result
        }
    }

    unsafe fn copy_image(&self, image: EglImage, frame: &DmabufFrame, dst: *mut AVFrame) -> Result<()> {
        let staging = self.staging.as_ref().expect("ensure_staging vor copy_image");
        unsafe {
            // GL-Textur aus dem EGLImage (pro Frame — die fds wechseln).
            let mut tex: u32 = 0;
            (self.gl_gen_textures)(1, &mut tex);
            (self.gl_bind_texture)(GL_TEXTURE_2D, tex);
            (self.gl_tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            (self.gl_tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            (self.gl_image_target_texture)(GL_TEXTURE_2D, image);
            let gl_err = (self.gl_get_error)();
            if gl_err != GL_NO_ERROR {
                (self.gl_bind_texture)(GL_TEXTURE_2D, 0);
                (self.gl_delete_textures)(1, &tex);
                return Err(anyhow!(
                    "glEGLImageTargetTexture2DOES failed (glError={gl_err:#06x})"
                ));
            }
            (self.gl_bind_texture)(GL_TEXTURE_2D, 0);

            // EGLImage-Textur → Staging. Gleiche Größe: Raw-Texel-Copy (Detile
            // passiert im Treiber). Andere Größe: Framebuffer-Blit mit LINEAR-
            // Filter = GPU-Downscale in einem Schritt. CUDA sieht danach die
            // Staging-Textur; die Map-Operation synchronisiert implizit mit
            // vorherigem GL.
            if frame.width == staging.width && frame.height == staging.height {
                (self.gl_copy_image_sub_data)(
                    tex, GL_TEXTURE_2D, 0, 0, 0, 0,
                    staging.tex, GL_TEXTURE_2D, 0, 0, 0, 0,
                    staging.width as i32, staging.height as i32, 1,
                );
            } else {
                (self.gl_bind_framebuffer)(GL_READ_FRAMEBUFFER, self.fbos[0]);
                (self.gl_framebuffer_texture_2d)(
                    GL_READ_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0,
                );
                (self.gl_bind_framebuffer)(GL_DRAW_FRAMEBUFFER, self.fbos[1]);
                (self.gl_framebuffer_texture_2d)(
                    GL_DRAW_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, staging.tex, 0,
                );
                (self.gl_blit_framebuffer)(
                    0, 0, frame.width as i32, frame.height as i32,
                    0, 0, staging.width as i32, staging.height as i32,
                    GL_COLOR_BUFFER_BIT, GL_LINEAR,
                );
                // Texturen detachen, bevor `tex` gelöscht wird (dangling
                // Attachment) — und den FBO-Bind zurücksetzen.
                (self.gl_framebuffer_texture_2d)(
                    GL_READ_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, 0, 0,
                );
                (self.gl_framebuffer_texture_2d)(
                    GL_DRAW_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, 0, 0,
                );
                (self.gl_bind_framebuffer)(GL_FRAMEBUFFER, 0);
            }
            let gl_err = (self.gl_get_error)();
            (self.gl_delete_textures)(1, &tex);
            if gl_err != GL_NO_ERROR {
                return Err(anyhow!("GL copy/blit → staging failed (glError={gl_err:#06x})"));
            }

            self.cuda_copy(staging.cu_res, dst)
        }
    }

    unsafe fn cuda_copy(&self, res: CuGraphicsResource, dst: *mut AVFrame) -> Result<()> {
        unsafe {
            let r = (self.cu_ctx_push)(self.cu_ctx);
            if r != CUDA_SUCCESS {
                return Err(anyhow!("cuCtxPushCurrent failed (rc={r})"));
            }
            let mut res = res;
            let result = (|| -> Result<()> {
                let r = (self.cu_map_resources)(1, &mut res, ptr::null_mut());
                if r != CUDA_SUCCESS {
                    return Err(anyhow!("cuGraphicsMapResources failed (rc={r})"));
                }
                let result = (|| -> Result<()> {
                    let mut array: CuArray = ptr::null_mut();
                    let r = (self.cu_get_mapped_array)(&mut array, res, 0, 0);
                    if r != CUDA_SUCCESS {
                        return Err(anyhow!(
                            "cuGraphicsSubResourceGetMappedArray failed (rc={r})"
                        ));
                    }

                    // ARRAY (Staging-Textur, Ausgabe-Größe) → DEVICE (linear,
                    // ffmpeg data[0]). Beide sind out_w×out_h — min() nur als
                    // Schutzgurt.
                    let dst_w = (*dst).width.max(0) as usize;
                    let dst_h = (*dst).height.max(0) as usize;
                    let copy_w = (self.out_w as usize).min(dst_w) * 4;
                    let copy_h = (self.out_h as usize).min(dst_h);
                    let cpy = CudaMemcpy2D {
                        src_x_in_bytes: 0,
                        src_y: 0,
                        src_memory_type: CU_MEMORYTYPE_ARRAY,
                        src_host: ptr::null(),
                        src_device: 0,
                        src_array: array,
                        src_pitch: 0,
                        dst_x_in_bytes: 0,
                        dst_y: 0,
                        dst_memory_type: CU_MEMORYTYPE_DEVICE,
                        dst_host: ptr::null_mut(),
                        dst_device: (*dst).data[0] as u64,
                        dst_array: ptr::null_mut(),
                        dst_pitch: (*dst).linesize[0].max(0) as usize,
                        width_in_bytes: copy_w,
                        height: copy_h,
                    };
                    let r = (self.cu_memcpy_2d)(&cpy);
                    if r != CUDA_SUCCESS {
                        return Err(anyhow!("cuMemcpy2D failed (rc={r})"));
                    }
                    // NVENC liest auf FFmpegs eigenem Stream → erst syncen,
                    // dann darf der Encoder das Frame sehen.
                    let r = (self.cu_ctx_synchronize)();
                    if r != CUDA_SUCCESS {
                        return Err(anyhow!("cuCtxSynchronize failed (rc={r})"));
                    }
                    Ok(())
                })();
                (self.cu_unmap_resources)(1, &mut res, ptr::null_mut());
                result
            })();
            let mut old: CuContext = ptr::null_mut();
            (self.cu_ctx_pop)(&mut old);
            result
        }
    }
}

impl Drop for NvDmabufImporter {
    fn drop(&mut self) {
        self.drop_staging();
        unsafe {
            if self.fbos != [0; 2] {
                (self.gl_delete_framebuffers)(2, self.fbos.as_ptr());
            }
            (self.egl_make_current)(self.dpy, ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
            (self.egl_destroy_context)(self.dpy, self.ctx);
            (self.egl_terminate)(self.dpy);
            (self.cu_primary_ctx_release)(self.cu_device);
        }
    }
}
