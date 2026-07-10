//! HW-Context: VAAPI- (AMD/Intel) oder CUDA- (Nvidia) Device + Frame-Pool.
//!
//! Erzeugt `av_hwdevice_ctx_create` + `av_hwframe_ctx_alloc`/`init` für den
//! Vendor. Der Encoder bekommt eine eigene Ref auf den Frames-Pool. Für
//! synthetische Frames (Phase 4) uploaden wir einen CPU-Frame per
//! `av_hwframe_transfer_data` in den HW-Pool — der Zero-Copy-DMABUF-Pfad
//! (Phase 6, `av_hwframe_map` aus einem DRM_PRIME-Frame) ersetzt das später für
//! echte Capture-Frames.
//!
//! sw_format = NV12 (Encoder-Input). BGRA→NV12-Convert läuft in Phase 4 über
//! swscale (CPU); in Phase 6 auf der GPU (PipeWire liefert ggf. direkt NV12,
//! sonst `scale_vaapi`).

use std::ptr;

use anyhow::{Result, anyhow};
use ffmpeg_next as ffmpeg;
use ffmpeg::ffi::*;

use crate::system::drm::Vendor;

/// Welche HW-Device-Art für den Vendor angelegt wird.
pub fn kind_for(vendor: Vendor) -> HwDeviceKind {
    match vendor {
        Vendor::Nvidia => HwDeviceKind::Cuda,
        Vendor::Amd | Vendor::Intel => HwDeviceKind::Vaapi,
    }
}

#[derive(Debug, Clone, Copy)]
pub enum HwDeviceKind {
    Cuda,
    Vaapi,
}

impl HwDeviceKind {
    fn av_type(self) -> AVHWDeviceType {
        match self {
            HwDeviceKind::Cuda => AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
            HwDeviceKind::Vaapi => AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
        }
    }

    /// ffmpeg-Pixelformat der HW-Frames (AV_PIX_FMT_CUDA / AV_PIX_FMT_VAAPI).
    pub fn pix_fmt(self) -> AVPixelFormat {
        match self {
            HwDeviceKind::Cuda => AVPixelFormat::AV_PIX_FMT_CUDA,
            HwDeviceKind::Vaapi => AVPixelFormat::AV_PIX_FMT_VAAPI,
        }
    }

    pub fn ffmpeg_pixel(self) -> ffmpeg::format::Pixel {
        match self {
            HwDeviceKind::Cuda => ffmpeg::format::Pixel::CUDA,
            HwDeviceKind::Vaapi => ffmpeg::format::Pixel::VAAPI,
        }
    }
}

pub struct HwContext {
    dev_ref: *mut AVBufferRef,
    frames_ref: *mut AVBufferRef,
    kind: HwDeviceKind,
    width: i32,
    height: i32,
}

impl HwContext {
    /// Lege Device + Frames-Pool an.
    ///
    /// `device_arg`: für VAAPI der Render-Node-Pfad (`/dev/dri/renderDXXX`);
    /// für CUDA `None` (FFmpeg nimmt CUDA-Device 0).
    /// `sw_format`: Pixel-Format der Pool-Frames — NV12 für den Upload-Pfad
    /// (synthetische Quelle), BGR0 für den DMABUF-Import (NVENC nimmt RGB
    /// direkt und wandelt intern).
    pub fn create(
        kind: HwDeviceKind,
        device_arg: Option<&str>,
        w: u32,
        h: u32,
        sw_format: AVPixelFormat,
    ) -> Result<Self> {
        let dev_c = device_arg.map(|s| std::ffi::CString::new(s).unwrap());
        let dev_ptr = dev_c.as_ref().map(|c| c.as_ptr()).unwrap_or(ptr::null());

        // hwcontext_cuda.h (nicht in den Bindings, Wert ist stabile Public-API):
        // CUDA soll den Primary-Context des Devices nutzen statt einen eigenen
        // zu erzeugen — nur so teilen sich FFmpeg und unser CUDA-GL-Interop
        // (nv_import, ebenfalls Primary-Context) denselben CUcontext.
        const AV_CUDA_USE_PRIMARY_CONTEXT: i32 = 1 << 0;
        let flags = match kind {
            HwDeviceKind::Cuda => AV_CUDA_USE_PRIMARY_CONTEXT,
            HwDeviceKind::Vaapi => 0,
        };

        let mut dev_ref: *mut AVBufferRef = ptr::null_mut();
        let mut frames_ref: *mut AVBufferRef;
        unsafe {
            let r = av_hwdevice_ctx_create(&mut dev_ref, kind.av_type(), dev_ptr, ptr::null_mut(), flags);
            if r < 0 || dev_ref.is_null() {
                return Err(anyhow!(
                    "av_hwdevice_ctx_create({:?}) failed (rc={r}) — Treiber/VAAPI/NVENC geladen?",
                    kind
                ));
            }

            frames_ref = av_hwframe_ctx_alloc(dev_ref);
            if frames_ref.is_null() {
                av_buffer_unref(&mut dev_ref);
                return Err(anyhow!("av_hwframe_ctx_alloc returned NULL"));
            }
            let fc = (*frames_ref).data as *mut AVHWFramesContext;
            (*fc).format = kind.pix_fmt();
            (*fc).sw_format = sw_format;
            (*fc).width = w as i32;
            (*fc).height = h as i32;
            // Kleiner Pool für Encode-Input; der Encoder puffert selbst.
            (*fc).initial_pool_size = 4;
            let r = av_hwframe_ctx_init(frames_ref);
            if r < 0 {
                av_buffer_unref(&mut frames_ref);
                av_buffer_unref(&mut dev_ref);
                return Err(anyhow!("av_hwframe_ctx_init failed (rc={r})"));
            }
        }

        Ok(Self { dev_ref, frames_ref, kind, width: w as i32, height: h as i32 })
    }

    pub fn frames_ref(&self) -> *mut AVBufferRef {
        self.frames_ref
    }

    pub fn kind(&self) -> HwDeviceKind {
        self.kind
    }

    pub fn ffmpeg_pixel(&self) -> ffmpeg::format::Pixel {
        self.kind.ffmpeg_pixel()
    }

    /// Hole ein leeres HW-Frame aus dem Pool (Format CUDA/VAAPI, Maße des
    /// Pools, `hw_frames_ctx` gesetzt). Caller besitzt das Frame und muss es
    /// freigeben (`av_frame_free`).
    pub fn alloc_hwframe(&self) -> Result<*mut AVFrame> {
        unsafe {
            let mut hw = av_frame_alloc();
            if hw.is_null() {
                return Err(anyhow!("av_frame_alloc returned NULL"));
            }
            (*hw).format = self.kind.pix_fmt() as i32;
            (*hw).width = self.width;
            (*hw).height = self.height;
            // Encoder braucht den Frames-Ctx am Frame.
            let fc = av_buffer_ref(self.frames_ref);
            if fc.is_null() {
                av_frame_free(&mut hw);
                return Err(anyhow!("av_buffer_ref(frames) returned NULL"));
            }
            (*hw).hw_frames_ctx = fc;

            let r = av_hwframe_get_buffer(self.frames_ref, hw, 0);
            if r < 0 {
                av_frame_free(&mut hw);
                return Err(anyhow!("av_hwframe_get_buffer failed (rc={r})"));
            }
            Ok(hw)
        }
    }

    /// Lade einen CPU-Frame (sw_format des Pools) in den HW-Pool und gib das
    /// `AVFrame` (Format CUDA/VAAPI) zurück. Caller besitzt das Frame und muss
    /// es freigeben (`av_frame_free`).
    ///
    /// `sw` muss `format`/`width`/`height` passend zum Pool haben.
    pub fn upload_swframe(&self, sw: &ffmpeg::frame::Video, pts: i64) -> Result<*mut AVFrame> {
        let sw_ptr = unsafe { sw.as_ptr() };
        unsafe {
            let mut hw = self.alloc_hwframe()?;
            (*hw).pts = pts;
            // CPU → GPU Kopie (NV12 sw → CUDA/VAAPI hw).
            let r = av_hwframe_transfer_data(hw, sw_ptr, 0);
            if r < 0 {
                av_frame_free(&mut hw);
                return Err(anyhow!("av_hwframe_transfer_data failed (rc={r})"));
            }
            Ok(hw)
        }
    }
}

impl Drop for HwContext {
    fn drop(&mut self) {
        unsafe {
            if !self.frames_ref.is_null() {
                av_buffer_unref(&mut self.frames_ref);
            }
            if !self.dev_ref.is_null() {
                av_buffer_unref(&mut self.dev_ref);
            }
        }
    }
}

// SAFETY: AVBufferRef refcounts sind atomar; der HwContext wird vom
// Encoder-Thread gehalten und nicht nebenläufig geteilt (wie mac/win).
unsafe impl Send for HwContext {}
