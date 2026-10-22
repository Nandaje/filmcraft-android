//! GPU backend selection, hardware acceleration configuration, and startup crash-recovery sentinel
//! for Android.
//!
//! By default, FilmCraft on Android runs with **hardware GPU acceleration ON**:
//! - Primary backend: **Vulkan 1.1+** (`wgpu::Backends::VULKAN`)
//! - Secondary fallback: **OpenGL ES 3.0+** (`wgpu::Backends::GL`)
//! - Power preference: **`wgpu::PowerPreference::HighPerformance`**
//! - Device limits: unlocks the hardware adapter's full texture dimensions
//!   (`max_texture_dimension_1d/2d/3d`, up to 8K/16K on modern Adreno, Mali, and Xclipse GPUs),
//!   buffer sizes, and compute workgroup limits so [`filmcraft_gpu::GpuCompositor`],
//!   [`filmcraft_gpu::ExportRenderer`], and WGSL compute video effects (`fx::FxStage`) run on the
//!   GPU without hitting `wgpu`'s conservative WebGL2 defaults.
//!
//! If an Android GPU driver crashes during `eframe` / `wgpu` surface initialization before the
//! first frame completes, a `gpu-starting.json` sentinel records the interrupted backend and the
//! next launch steps down (`Vulkan` → `Gl` → `Cpu`) so the user is never stranded in a crash loop.

#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use eframe::egui_wgpu::{WgpuConfiguration, WgpuSetup, WgpuSetupCreateNew};
use eframe::wgpu;
use serde::{Deserialize, Serialize};

const SENTINEL_FILE: &str = "gpu-starting.json";

/// Android GPU rendering backend preference.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GpuBackend {
    /// Automatic hardware acceleration: try Vulkan first, then OpenGL ES (`VULKAN | GL`).
    #[default]
    Auto,
    /// Force hardware Vulkan (`wgpu::Backends::VULKAN`).
    Vulkan,
    /// Force hardware OpenGL ES (`wgpu::Backends::GL`).
    Gl,
    /// Software / CPU fallback mode (prefers fallback adapters or GL).
    Cpu,
}

impl GpuBackend {
    /// Short identifier used in logs, status messages, and sentinel files.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Vulkan => "vulkan",
            Self::Gl => "gl",
            Self::Cpu => "cpu",
        }
    }

    /// Human-readable label shown in the UI.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Auto => "Hardware GPU (Vulkan / GLES Auto)",
            Self::Vulkan => "Hardware GPU (Vulkan)",
            Self::Gl => "Hardware GPU (OpenGL ES)",
            Self::Cpu => "Software Fallback (GLES / CPU)",
        }
    }

    /// Parse a backend name from environment or config string.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" | "gpu" | "hw" | "hardware" => Some(Self::Auto),
            "vk" | "vulkan" => Some(Self::Vulkan),
            "gl" | "gles" | "opengl" | "opengles" => Some(Self::Gl),
            "cpu" | "sw" | "software" | "fallback" => Some(Self::Cpu),
            _ => None,
        }
    }

    /// Convert to `wgpu::Backends` bitflags.
    #[must_use]
    pub fn wgpu_backends(self) -> wgpu::Backends {
        match self {
            Self::Auto => wgpu::Backends::VULKAN | wgpu::Backends::GL,
            Self::Vulkan => wgpu::Backends::VULKAN,
            Self::Gl | Self::Cpu => wgpu::Backends::GL | wgpu::Backends::VULKAN,
        }
    }

    /// Next safer backend if startup crashed while initializing `self`.
    #[must_use]
    pub const fn fallback_after_crash(self) -> Self {
        match self {
            Self::Auto | Self::Vulkan => Self::Gl,
            Self::Gl | Self::Cpu => Self::Cpu,
        }
    }

    /// Whether hardware GPU acceleration is enabled for this backend mode.
    #[must_use]
    pub const fn is_hardware_accelerated(self) -> bool {
        !matches!(self, Self::Cpu)
    }
}

/// Persistent sentinel written before `wgpu` surface creation and cleared once the first frame
/// renders cleanly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartupSentinel {
    pub backend: GpuBackend,
    pub attempt: u32,
}

///Resolved GPU startup plan for the current launch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpuStartupPlan {
    pub backend: GpuBackend,
    pub recovered_from_crash: bool,
    pub previous_crashed_backend: Option<GpuBackend>,
    pub sentinel_path: PathBuf,
}

impl GpuStartupPlan {
    /// Clear the startup sentinel after the first frame has been presented.
    pub fn mark_startup_succeeded(&self) {
        if self.sentinel_path.exists() {
            let _ = std::fs::remove_file(&self.sentinel_path);
        }
    }

    /// Human-readable status note for the FilmCraft status bar or graphics notice.
    #[must_use]
    pub fn status_note(&self) -> String {
        if self.recovered_from_crash {
            format!("GPU recovery active: switched to {}", self.backend.label())
        } else {
            format!("GPU acceleration ON: {}", self.backend.label())
        }
    }
}

/// Determine the GPU backend to use for this launch and write the startup sentinel file.
#[must_use]
pub fn prepare_startup(data_dir: &Path, env_override: Option<&str>) -> GpuStartupPlan {
    let sentinel_path = data_dir.join(SENTINEL_FILE);

    if let Some(explicit) = env_override.and_then(GpuBackend::parse) {
        write_sentinel(&sentinel_path, &StartupSentinel { backend: explicit, attempt: 1 });
        return GpuStartupPlan {
            backend: explicit,
            recovered_from_crash: false,
            previous_crashed_backend: None,
            sentinel_path,
        };
    }

    if let Some(prev) = read_sentinel(&sentinel_path) {
        let next = prev.backend.fallback_after_crash();
        let attempt = prev.attempt.saturating_add(1);
        log::warn!(
            "previous Android GPU startup did not finish on backend '{}' (attempt {}); falling back to '{}'",
            prev.backend.as_str(),
            prev.attempt,
            next.as_str()
        );
        write_sentinel(&sentinel_path, &StartupSentinel { backend: next, attempt });
        return GpuStartupPlan {
            backend: next,
            recovered_from_crash: true,
            previous_crashed_backend: Some(prev.backend),
            sentinel_path,
        };
    }

    let initial = GpuBackend::Auto;
    write_sentinel(&sentinel_path, &StartupSentinel { backend: initial, attempt: 1 });
    GpuStartupPlan {
        backend: initial,
        recovered_from_crash: false,
        previous_crashed_backend: None,
        sentinel_path,
    }
}

fn read_sentinel(path: &Path) -> Option<StartupSentinel> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_sentinel(path: &Path, sentinel: &StartupSentinel) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_vec_pretty(sentinel) {
        let _ = filmcraft_format::atomic_write(path, &json);
    }
}

/// Score an Android `wgpu::Adapter` so hardware GPUs (Adreno, Mali, Immortalis, Xclipse, PowerVR)
/// and Vulkan backends are selected ahead of software rasterizers.
#[must_use]
pub fn adapter_rank(info: &wgpu::AdapterInfo, preferred: GpuBackend) -> (u8, u8) {
    let device_score = match info.device_type {
        wgpu::DeviceType::DiscreteGpu if preferred.is_hardware_accelerated() => 0,
        wgpu::DeviceType::IntegratedGpu if preferred.is_hardware_accelerated() => 1,
        wgpu::DeviceType::VirtualGpu if preferred.is_hardware_accelerated() => 2,
        wgpu::DeviceType::Other if preferred.is_hardware_accelerated() => 3,
        wgpu::DeviceType::Cpu if !preferred.is_hardware_accelerated() => 0,
        wgpu::DeviceType::Cpu => 4,
        _ => 3,
    };

    let backend_score = match (preferred, info.backend) {
        (GpuBackend::Auto | GpuBackend::Vulkan, wgpu::Backend::Vulkan) => 0,
        (GpuBackend::Auto | GpuBackend::Vulkan, wgpu::Backend::Gl) => 1,
        (GpuBackend::Gl | GpuBackend::Cpu, wgpu::Backend::Gl) => 0,
        (GpuBackend::Gl | GpuBackend::Cpu, wgpu::Backend::Vulkan) => 1,
        _ => 2,
    };

    (device_score, backend_score)
}

/// Pick the best adapter index from a list of `(AdapterInfo, feature_count)` pairs.
///
/// Mirrors `apps/filmcraft/src/graphics.rs` by breaking ties using the richer feature set when
/// duplicate driver entries are present.
#[must_use]
pub fn pick_best_adapter_index(
    adapters: &[(wgpu::AdapterInfo, usize)],
    preferred: GpuBackend,
) -> Option<usize> {
    adapters
        .iter()
        .enumerate()
        .min_by_key(|(idx, (info, features))| {
            let (dev_rank, backend_rank) = adapter_rank(info, preferred);
            (dev_rank, backend_rank, std::cmp::Reverse(*features), *idx)
        })
        .map(|(idx, _)| idx)
}

/// Build an `eframe::egui_wgpu::WgpuConfiguration` tuned for Android hardware GPU acceleration and
/// FilmCraft's GPU video compositor (`filmcraft_gpu`).
#[must_use]
pub fn configure(backend: GpuBackend) -> WgpuConfiguration {
    let mut cfg = WgpuConfiguration::default();

    let power_preference = if backend.is_hardware_accelerated() {
        wgpu::PowerPreference::HighPerformance
    } else {
        wgpu::PowerPreference::LowPower
    };

    let base_setup = match cfg.wgpu_setup {
        WgpuSetup::CreateNew(setup) => setup,
        WgpuSetup::Existing(_) => WgpuSetupCreateNew::without_display_handle(),
    };

    let mut instance_descriptor = base_setup.instance_descriptor;
    instance_descriptor.backends = backend.wgpu_backends();
    // Same workaround as desktop `apps/filmcraft/src/graphics.rs`: clear indirect-call validation
    // so driver validation layers do not reject valid pipelines.
    instance_descriptor.flags.remove(wgpu::InstanceFlags::VALIDATION_INDIRECT_CALL);

    // Unlock full hardware texture, buffer, and compute limits so 4K/8K footage, LUTs, scopes, and
    // WGSL compute effects run at native resolution on mobile GPUs.
    let device_descriptor = Arc::new(move |adapter: &wgpu::Adapter| {
        let adapter_limits = adapter.limits();
        let base_limits = wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter_limits.clone());
        let limits = wgpu::Limits {
            max_texture_dimension_1d: adapter_limits.max_texture_dimension_1d,
            max_texture_dimension_2d: adapter_limits.max_texture_dimension_2d,
            max_texture_dimension_3d: adapter_limits.max_texture_dimension_3d,
            max_buffer_size: adapter_limits.max_buffer_size,
            max_storage_buffer_binding_size: adapter_limits.max_storage_buffer_binding_size,
            max_compute_workgroup_storage_size: adapter_limits.max_compute_workgroup_storage_size,
            max_compute_invocations_per_workgroup: adapter_limits.max_compute_invocations_per_workgroup,
            max_compute_workgroup_size_x: adapter_limits.max_compute_workgroup_size_x,
            max_compute_workgroup_size_y: adapter_limits.max_compute_workgroup_size_y,
            max_compute_workgroup_size_z: adapter_limits.max_compute_workgroup_size_z,
            max_compute_workgroups_per_dimension: adapter_limits.max_compute_workgroups_per_dimension,
            ..base_limits
        };
        wgpu::DeviceDescriptor {
            label: Some("filmcraft-android-gpu-device"),
            required_features: wgpu::Features::empty(),
            required_limits: limits,
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }
    });

    let custom_select = Arc::new(move |adapters: &[wgpu::Adapter], surface: Option<&wgpu::Surface<'_>>| {
        let compatible: Vec<&wgpu::Adapter> = adapters
            .iter()
            .filter(|a| surface.is_none_or(|s| a.is_surface_supported(s)))
            .collect();
        let pool: Vec<&wgpu::Adapter> = if compatible.is_empty() { adapters.iter().collect() } else { compatible };
        let scored: Vec<(wgpu::AdapterInfo, usize)> = pool
            .iter()
            .map(|a| (a.get_info(), a.features().iter().count()))
            .collect();
        let idx = pick_best_adapter_index(&scored, backend).ok_or_else(|| "no suitable Android GPU adapter found".to_owned())?;
        let chosen = pool[idx];
        let info = chosen.get_info();
        log::info!(
            "FilmCraft Android selected GPU adapter: '{}' ({:?}, {:?})",
            info.name,
            info.backend,
            info.device_type
        );
        Ok(chosen.clone())
    });

    cfg.wgpu_setup = WgpuSetup::CreateNew(WgpuSetupCreateNew {
        instance_descriptor,
        power_preference,
        native_adapter_selector: Some(custom_select),
        device_descriptor,
        ..base_setup
    });
    cfg.surface.present_mode = wgpu::PresentMode::AutoVsync;
    cfg.surface.desired_maximum_frame_latency = Some(2);
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_adapter(name: &str, backend: wgpu::Backend, device_type: wgpu::DeviceType) -> wgpu::AdapterInfo {
        wgpu::AdapterInfo {
            name: name.to_string(),
            vendor: 0,
            device: 0,
            device_type,
            device_pci_bus_id: String::new(),
            driver: String::new(),
            driver_info: String::new(),
            backend,
            subgroup_min_size: 1,
            subgroup_max_size: 64,
            transient_saves_memory: Some(false),
            limit_bucket: None,
        }
    }

    #[test]
    fn gpu_acceleration_is_enabled_by_default_and_steps_down_on_crash() {
        let dir = tempfile::tempdir().unwrap();

        // 1st launch: defaults to Auto (Vulkan + GL hardware acceleration ON).
        let plan1 = prepare_startup(dir.path(), None);
        assert_eq!(plan1.backend, GpuBackend::Auto);
        assert!(plan1.backend.is_hardware_accelerated());
        assert!(!plan1.recovered_from_crash);
        assert!(plan1.sentinel_path.exists());

        // Simulate a crash before the first frame completes: 2nd launch steps down to GL.
        let plan2 = prepare_startup(dir.path(), None);
        assert_eq!(plan2.backend, GpuBackend::Gl);
        assert!(plan2.recovered_from_crash);
        assert_eq!(plan2.previous_crashed_backend, Some(GpuBackend::Auto));

        // Simulate another crash on GL: 3rd launch steps down to Cpu fallback.
        let plan3 = prepare_startup(dir.path(), None);
        assert_eq!(plan3.backend, GpuBackend::Cpu);
        assert!(plan3.recovered_from_crash);
        assert_eq!(plan3.previous_crashed_backend, Some(GpuBackend::Gl));

        // Once the first frame succeeds, the sentinel is removed and the next launch restores Auto.
        plan3.mark_startup_succeeded();
        assert!(!plan3.sentinel_path.exists());
        let plan4 = prepare_startup(dir.path(), None);
        assert_eq!(plan4.backend, GpuBackend::Auto);
        assert!(!plan4.recovered_from_crash);
    }

    #[test]
    fn adapter_selector_prefers_hardware_vulkan_over_gles_and_software() {
        let adapters = vec![
            (fake_adapter("llvmpipe", wgpu::Backend::Vulkan, wgpu::DeviceType::Cpu), 10),
            (fake_adapter("Adreno (TM) 740 GLES", wgpu::Backend::Gl, wgpu::DeviceType::IntegratedGpu), 8),
            (fake_adapter("Adreno (TM) 740 Vulkan", wgpu::Backend::Vulkan, wgpu::DeviceType::IntegratedGpu), 12),
        ];
        assert_eq!(pick_best_adapter_index(&adapters, GpuBackend::Auto), Some(2));
        assert_eq!(pick_best_adapter_index(&adapters, GpuBackend::Gl), Some(1));
        assert_eq!(pick_best_adapter_index(&adapters, GpuBackend::Cpu), Some(0));
    }

    #[test]
    fn duplicate_vulkan_adapters_pick_the_fuller_feature_set() {
        let adapters = vec![
            (fake_adapter("Mali-G715", wgpu::Backend::Vulkan, wgpu::DeviceType::IntegratedGpu), 4),
            (fake_adapter("Mali-G715", wgpu::Backend::Vulkan, wgpu::DeviceType::IntegratedGpu), 14),
        ];
        assert_eq!(pick_best_adapter_index(&adapters, GpuBackend::Auto), Some(1));
    }

    #[test]
    fn wgpu_configuration_clears_indirect_validation_and_requests_high_performance() {
        let cfg = configure(GpuBackend::Auto);
        match cfg.wgpu_setup {
            WgpuSetup::CreateNew(setup) => {
                assert_eq!(setup.power_preference, wgpu::PowerPreference::HighPerformance);
                assert!(setup.instance_descriptor.backends.contains(wgpu::Backends::VULKAN));
                assert!(setup.instance_descriptor.backends.contains(wgpu::Backends::GL));
                assert!(!setup.instance_descriptor.flags.contains(wgpu::InstanceFlags::VALIDATION_INDIRECT_CALL));
            }
            WgpuSetup::Existing(_) => panic!("expected CreateNew setup"),
        }
    }
}
