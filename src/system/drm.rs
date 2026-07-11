//! DRM-Vendor- + Render-Node-Erkennung (sysfs, ohne DRM-ioctl-FFI).
//!
//! Liest `/sys/class/drm/renderD*/device/driver` (Symlink-Basename) und mappt
//! den Treibernamen auf den Pulse-Vendor-Slug — dieselbe Logik wie GSRs
//! `drmGetVersion`-basierte Erkennung, nur ohne libdrm-FFI:
//!
//!   nvidia / nvidia-drm → "nvidia"  (NVENC-Pfad)
//!   amdgpu              → "amd"     (VAAPI-Pfad)
//!   i915 / xe           → "intel"   (VAAPI-Pfad)
//!
//! Bei mehreren GPUs wird die dedizierte dGPU (nvidia/amd) vor der Intel-iGPU
//! bevorzugt — analog zum DXGI-`HIGH_PERFORMANCE`-Default des Windows-Sidecars.

use std::fs;
use std::path::{Path, PathBuf};

/// Erkannter Vendor-Slug oder `None` wenn keine passende DRM-Render-Node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    Nvidia,
    Amd,
    Intel,
}

impl Vendor {
    pub fn slug(self) -> &'static str {
        match self {
            Vendor::Nvidia => "nvidia",
            Vendor::Amd => "amd",
            Vendor::Intel => "intel",
        }
    }

    /// Encoder-Familie die dieser Vendor unter Linux nutzt (VAAPI vs NVENC).
    pub fn encoder_family(self) -> &'static str {
        match self {
            Vendor::Nvidia => "nvenc",
            Vendor::Amd | Vendor::Intel => "vaapi",
        }
    }
}

/// Treibername → Vendor.
fn driver_to_vendor(driver: &str) -> Option<Vendor> {
    match driver {
        "nvidia" | "nvidia-drm" => Some(Vendor::Nvidia),
        "amdgpu" => Some(Vendor::Amd),
        "i915" | "xe" => Some(Vendor::Intel),
        _ => None,
    }
}

/// Eine gefundene DRM-Render-Node mit ihrem Vendor.
struct Node {
    path: PathBuf,
    vendor: Vendor,
}

fn enumerate_render_nodes() -> Vec<Node> {
    let mut nodes = Vec::new();
    let class = Path::new("/sys/class/drm");
    let Ok(entries) = fs::read_dir(class) else {
        return nodes;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("renderD") {
            continue;
        }
        // /sys/class/drm/renderD128/device/driver → Symlink auf den Treiber-Ordner.
        let driver_link = entry.path().join("device/driver");
        let Some(driver_path) = fs::read_link(&driver_link).ok() else {
            continue;
        };
        let driver = driver_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let Some(vendor) = driver_to_vendor(driver) else {
            continue;
        };
        let dev_path = Path::new("/dev/dri").join(name);
        nodes.push(Node { path: dev_path, vendor });
    }
    nodes
}

/// String → Vendor für den `PULSE_HQ_VENDOR`-Override.
fn parse_vendor(s: &str) -> Option<Vendor> {
    match s.trim().to_ascii_lowercase().as_str() {
        "nvidia" => Some(Vendor::Nvidia),
        "amd" => Some(Vendor::Amd),
        "intel" => Some(Vendor::Intel),
        _ => None,
    }
}

/// Detektiere den primären Vendor + dessen Render-Node-Pfad.
///
/// `PULSE_HQ_VENDOR=nvidia|amd|intel` erzwingt eine bestimmte Encode-GPU
/// (Multi-GPU-Test: AMD-iGPU-VAAPI trotz NVIDIA-dGPU — Voraussetzung: die
/// aufgenommene Quelle liegt AUF dieser GPU, sonst scheitert der Zero-Copy-
/// Import). Ist der erzwungene Vendor nicht als Render-Node da → `None` (klarer
/// Fehler statt stillem Fallback auf die falsche Karte).
///
/// Ohne Override: bevorzugt dGPU (nvidia/amd) vor Intel-iGPU. `None` wenn keine
/// bekannte Render-Node gefunden wurde (→ Encoder-Pfade schlagen sauber fehl).
pub fn detect() -> Option<(Vendor, String)> {
    let nodes = enumerate_render_nodes();

    if let Ok(forced) = std::env::var("PULSE_HQ_VENDOR") {
        let want = parse_vendor(&forced);
        if want.is_none() {
            tracing::warn!(target: "stream", "PULSE_HQ_VENDOR='{forced}' unbekannt — ignoriert");
        }
        if let Some(want) = want {
            let node = nodes.iter().find(|n| n.vendor == want);
            match node {
                Some(n) => tracing::info!(
                    target: "stream", vendor = want.slug(), node = %n.path.display(),
                    "PULSE_HQ_VENDOR-Override aktiv"
                ),
                None => tracing::error!(
                    target: "stream", vendor = want.slug(),
                    "PULSE_HQ_VENDOR erzwingt Vendor, aber keine passende Render-Node vorhanden"
                ),
            }
            return node.map(|n| (n.vendor, n.path.to_string_lossy().to_string()));
        }
    }

    // Bevorzuge dGPU.
    let pick = nodes
        .iter()
        .find(|n| n.vendor == Vendor::Nvidia || n.vendor == Vendor::Amd)
        .or_else(|| nodes.iter().find(|n| n.vendor == Vendor::Intel))?;
    let path = pick.path.to_string_lossy().to_string();
    Some((pick.vendor, path))
}

/// Render-Node-Pfad für einen bestimmten Vendor (für den VAAPI-hwdevice), falls
/// auf dieser Maschine vorhanden. NVENC/CUDA braucht keinen Render-Node.
pub fn render_node_for(vendor: Vendor) -> Option<String> {
    enumerate_render_nodes()
        .into_iter()
        .find(|n| n.vendor == vendor)
        .map(|n| n.path.to_string_lossy().to_string())
}
