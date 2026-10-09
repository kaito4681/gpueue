//! GPU detection.
//!
//! At start-up the daemon asks a [`GpuDetector`] which GPUs exist. In
//! production that is [`NvmlGpuDetector`] (NVML, loaded at run time with
//! `dlopen`, so the daemon also runs on machines without the NVIDIA driver),
//! wrapped in a [`FallbackGpuDetector`] that falls back to the `[[gpus]]`
//! list of the configuration file ([`StaticGpuDetector`]) when NVML cannot be
//! used. `gpu_detection = "static"` skips NVML altogether.
//!
//! Indices follow `CUDA_DEVICE_ORDER=PCI_BUS_ID`: GPUs are sorted by PCI
//! domain, bus, device and function, which is the order CUDA uses with that
//! setting (and the order users pass to `--gpu`).
//!
//! What the daemon does with the result (see `Daemon::start`):
//! - every detected GPU is upserted into the `gpus` table, keeping the slot
//!   limit (`parallel`) of a GPU already known at that index;
//! - a GPU in the table that was not detected stays there (so its limit
//!   survives a temporary disappearance) but is *unavailable*: it starts no
//!   jobs, jobs that need it stay queued (or checkpointed), new jobs asking
//!   for it are rejected, and `GET /v1/gpus` reports `available: false`;
//! - a different UUID at a known index is logged: the index keeps its limit,
//!   because users address GPUs by index.

use std::ffi::OsStr;
use std::path::PathBuf;

use puzuz_core::model::GpuIndex;

use crate::config::{Config, GpuConfig, GpuDetection};

/// A GPU found at start-up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedGpu {
    pub index: GpuIndex,
    pub uuid: String,
    pub name: String,
    pub memory_total_bytes: Option<u64>,
    /// The NVIDIA driver version (the same for every GPU of a machine).
    pub driver_version: Option<String>,
}

pub trait GpuDetector: Send + Sync + 'static {
    /// The GPUs present now, in `CUDA_DEVICE_ORDER=PCI_BUS_ID` order.
    fn detect(&self) -> Result<Vec<DetectedGpu>, String>;
}

/// The detector `puzuz daemon` uses for `config`.
pub fn detector_for(config: &Config) -> Box<dyn GpuDetector> {
    let configured = StaticGpuDetector::from_config(&config.gpus);
    match config.gpu_detection {
        GpuDetection::Static => Box::new(configured),
        GpuDetection::Auto => Box::new(FallbackGpuDetector {
            primary: Box::new(NvmlGpuDetector {
                library: config.nvml_library.clone(),
            }),
            fallback: configured,
        }),
    }
}

/// Reports a fixed list (from `[[gpus]]` in the configuration, or tests).
#[derive(Debug, Default, Clone)]
pub struct StaticGpuDetector(pub Vec<DetectedGpu>);

impl StaticGpuDetector {
    pub fn from_config(gpus: &[GpuConfig]) -> Self {
        Self(
            gpus.iter()
                .map(|gpu| DetectedGpu {
                    index: GpuIndex(gpu.index),
                    uuid: gpu.uuid.clone(),
                    name: gpu.name.clone(),
                    memory_total_bytes: gpu.memory_total_bytes,
                    driver_version: None,
                })
                .collect(),
        )
    }

    /// `count` anonymous GPUs numbered from 0 (for tests).
    pub fn anonymous(count: u32) -> Self {
        Self(
            (0..count)
                .map(|i| DetectedGpu {
                    index: GpuIndex(i),
                    uuid: format!("GPU-test-{i}"),
                    name: "Test GPU".to_owned(),
                    memory_total_bytes: None,
                    driver_version: None,
                })
                .collect(),
        )
    }
}

impl GpuDetector for StaticGpuDetector {
    fn detect(&self) -> Result<Vec<DetectedGpu>, String> {
        Ok(self.0.clone())
    }
}

/// Uses `primary` (NVML); when it fails (no driver, no library), warns and
/// reports `fallback` (the configured list) instead. Never fails.
pub struct FallbackGpuDetector {
    pub primary: Box<dyn GpuDetector>,
    pub fallback: StaticGpuDetector,
}

impl GpuDetector for FallbackGpuDetector {
    fn detect(&self) -> Result<Vec<DetectedGpu>, String> {
        match self.primary.detect() {
            Ok(gpus) => {
                if !self.fallback.0.is_empty() {
                    tracing::warn!(
                        "GPUs were detected with NVML; the [[gpus]] list in the configuration \
                         is ignored (set gpu_detection = \"static\" to use it instead)"
                    );
                }
                Ok(gpus)
            }
            Err(e) if self.fallback.0.is_empty() => {
                tracing::warn!("GPU detection failed ({e}); running without GPUs");
                Ok(Vec::new())
            }
            Err(e) => {
                tracing::warn!(
                    "GPU detection failed ({e}); using the [[gpus]] list from the configuration"
                );
                Ok(self.fallback.0.clone())
            }
        }
    }
}

/// Library names tried when `nvml_library` is not configured. The
/// unversioned name only exists with development packages installed.
const NVML_LIBRARIES: &[&str] = &["libnvidia-ml.so.1", "libnvidia-ml.so"];

/// Detects GPUs with NVML.
#[derive(Debug, Default, Clone)]
pub struct NvmlGpuDetector {
    /// Path or name of `libnvidia-ml.so`; `None` tries [`NVML_LIBRARIES`].
    pub library: Option<PathBuf>,
}

impl NvmlGpuDetector {
    fn init(&self) -> Result<nvml_wrapper::Nvml, String> {
        let candidates: Vec<&OsStr> = match &self.library {
            Some(path) => vec![path.as_os_str()],
            None => NVML_LIBRARIES.iter().map(OsStr::new).collect(),
        };
        let mut errors = Vec::new();
        for library in candidates {
            match nvml_wrapper::Nvml::builder().lib_path(library).init() {
                Ok(nvml) => return Ok(nvml),
                Err(e) => errors.push(format!("{}: {e}", library.to_string_lossy())),
            }
        }
        Err(format!("cannot initialize NVML: {}", errors.join("; ")))
    }
}

impl GpuDetector for NvmlGpuDetector {
    fn detect(&self) -> Result<Vec<DetectedGpu>, String> {
        let nvml = self.init()?;
        let driver_version = nvml.sys_driver_version().ok();
        let count = nvml
            .device_count()
            .map_err(|e| format!("NVML cannot count devices: {e}"))?;
        let mut devices = Vec::new();
        for i in 0..count {
            let device = nvml
                .device_by_index(i)
                .map_err(|e| format!("NVML cannot open device {i}: {e}"))?;
            let pci = device
                .pci_info()
                .map_err(|e| format!("NVML cannot read the PCI address of device {i}: {e}"))?;
            devices.push(NvmlDevice {
                bus_id: pci.bus_id,
                uuid: device
                    .uuid()
                    .map_err(|e| format!("NVML cannot read the UUID of device {i}: {e}"))?,
                name: device.name().unwrap_or_else(|_| "NVIDIA GPU".to_owned()),
                memory_total_bytes: device.memory_info().ok().map(|m| m.total),
            });
        }
        Ok(order_by_pci_bus(devices, driver_version))
    }
}

/// One device as NVML reports it, before ordering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvmlDevice {
    /// `domain:bus:device.function` in hexadecimal, e.g. `00000000:3B:00.0`.
    pub bus_id: String,
    pub uuid: String,
    pub name: String,
    pub memory_total_bytes: Option<u64>,
}

/// Numbers devices in PCI bus order (`CUDA_DEVICE_ORDER=PCI_BUS_ID`). NVML's
/// own enumeration order is not guaranteed to match.
pub fn order_by_pci_bus(
    mut devices: Vec<NvmlDevice>,
    driver_version: Option<String>,
) -> Vec<DetectedGpu> {
    devices.sort_by_cached_key(|d| (pci_sort_key(&d.bus_id), d.bus_id.to_ascii_lowercase()));
    devices
        .into_iter()
        .enumerate()
        .map(|(i, d)| DetectedGpu {
            index: GpuIndex(i as u32),
            uuid: d.uuid,
            name: d.name,
            memory_total_bytes: d.memory_total_bytes,
            driver_version: driver_version.clone(),
        })
        .collect()
}

/// `(domain, bus, device, function)` of a PCI bus id; unparsable ids sort
/// last.
fn pci_sort_key(bus_id: &str) -> [u64; 4] {
    let parse = || -> Option<[u64; 4]> {
        let mut parts = bus_id.trim().split(':');
        let (a, b, c) = (parts.next()?, parts.next()?, parts.next());
        // `domain:bus:device.function`, or `bus:device.function`.
        let (domain, bus, rest) = match c {
            Some(rest) => (a, b, rest),
            None => ("0", a, b),
        };
        let (device, function) = rest.split_once('.')?;
        let hex = |s: &str| u64::from_str_radix(s, 16).ok();
        Some([hex(domain)?, hex(bus)?, hex(device)?, hex(function)?])
    };
    parse().unwrap_or([u64::MAX; 4])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(bus_id: &str, uuid: &str) -> NvmlDevice {
        NvmlDevice {
            bus_id: bus_id.to_owned(),
            uuid: uuid.to_owned(),
            name: "NVIDIA A100".to_owned(),
            memory_total_bytes: Some(80 << 30),
        }
    }

    #[test]
    fn devices_are_numbered_in_pci_bus_order() {
        let gpus = order_by_pci_bus(
            vec![
                device("00000000:B1:00.0", "GPU-c"),
                device("00000001:01:00.0", "GPU-d"),
                device("00000000:3b:00.0", "GPU-b"),
                device("garbage", "GPU-e"),
                device("00000000:1A:00.0", "GPU-a"),
            ],
            Some("570.86.10".to_owned()),
        );
        let order: Vec<(u32, &str)> = gpus.iter().map(|g| (g.index.0, g.uuid.as_str())).collect();
        assert_eq!(
            order,
            [
                (0, "GPU-a"),
                (1, "GPU-b"),
                (2, "GPU-c"),
                (3, "GPU-d"),
                (4, "GPU-e")
            ]
        );
        assert!(
            gpus.iter()
                .all(|g| g.driver_version.as_deref() == Some("570.86.10"))
        );
        assert_eq!(gpus[0].memory_total_bytes, Some(80 << 30));
    }

    #[test]
    fn pci_keys_parse_with_and_without_domain() {
        assert_eq!(pci_sort_key("0000:3B:00.1"), [0, 0x3b, 0, 1]);
        assert_eq!(pci_sort_key("3b:02.0"), [0, 0x3b, 2, 0]);
        assert_eq!(pci_sort_key("x"), [u64::MAX; 4]);
    }

    struct Failing;
    impl GpuDetector for Failing {
        fn detect(&self) -> Result<Vec<DetectedGpu>, String> {
            Err("libnvidia-ml.so.1: not found".to_owned())
        }
    }

    #[test]
    fn fallback_uses_the_configured_list_only_when_nvml_fails() {
        let configured = StaticGpuDetector::anonymous(2);
        let failing = FallbackGpuDetector {
            primary: Box::new(Failing),
            fallback: configured.clone(),
        };
        assert_eq!(failing.detect().unwrap(), configured.0);

        let none = FallbackGpuDetector {
            primary: Box::new(Failing),
            fallback: StaticGpuDetector::default(),
        };
        assert!(none.detect().unwrap().is_empty());

        let nvml = StaticGpuDetector::anonymous(1);
        let working = FallbackGpuDetector {
            primary: Box::new(nvml.clone()),
            fallback: configured,
        };
        assert_eq!(working.detect().unwrap(), nvml.0);
    }

    #[test]
    fn missing_nvml_library_is_an_error_not_a_crash() {
        let detector = NvmlGpuDetector {
            library: Some(PathBuf::from("/nonexistent/libnvidia-ml.so.1")),
        };
        let err = detector.detect().unwrap_err();
        assert!(err.contains("/nonexistent/libnvidia-ml.so.1"), "{err}");
    }

    #[test]
    fn detector_for_honours_static_mode() {
        let config = Config::parse(
            "gpu_detection = \"static\"\n[[gpus]]\nindex = 0\nuuid = \"GPU-x\"\nname = \"X\"\n",
        )
        .unwrap();
        let gpus = detector_for(&config).detect().unwrap();
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].uuid, "GPU-x");
    }
}
