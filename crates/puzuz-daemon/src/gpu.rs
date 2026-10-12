//! GPU detection.
//!
//! At start-up the daemon asks a [`GpuDetector`] which GPUs exist. In
//! production that is [`NvmlGpuDetector`] (NVML, loaded at run time with
//! `dlopen`, so the daemon also runs on machines without the NVIDIA driver),
//! wrapped in a [`FallbackGpuDetector`] that falls back to the `[[gpus]]`
//! list of the configuration file ([`StaticGpuDetector`]) when NVML cannot be
//! used. `gpu_detection = "static"` skips NVML altogether. A failure of NVML
//! is logged with its reason (including whether `/dev/nvidiactl` is missing
//! or cannot be opened, the usual cause under a sandboxing systemd unit),
//! and `GET /v1/version` reports where the list came from
//! ([`GpuDetectionStatus`]) so that `puzuz doctor` can tell.
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
use std::path::{Path, PathBuf};

use puzuz_core::model::GpuIndex;
use puzuz_protocol::api::{DetectedGpuSummary, GpuDetectionStatus, GpuSource};

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

/// What a detection found, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    pub gpus: Vec<DetectedGpu>,
    pub source: GpuSource,
    /// Why NVML could not be used, when the list came from the
    /// configuration instead.
    pub nvml_error: Option<String>,
}

impl Detection {
    /// What `GET /v1/version` reports about it.
    pub fn status(&self, mode: GpuDetection) -> GpuDetectionStatus {
        GpuDetectionStatus {
            mode,
            source: self.source,
            nvml_error: self.nvml_error.clone(),
            gpus: self
                .gpus
                .iter()
                .map(|gpu| DetectedGpuSummary {
                    index: gpu.index,
                    uuid: gpu.uuid.clone(),
                    name: gpu.name.clone(),
                })
                .collect(),
        }
    }
}

pub trait GpuDetector: Send + Sync + 'static {
    /// The GPUs present now, in `CUDA_DEVICE_ORDER=PCI_BUS_ID` order.
    fn detect(&self) -> Result<Vec<DetectedGpu>, String>;

    /// [`detect`](GpuDetector::detect) and where the list came from. By
    /// default the list counts as configured (fixed lists, tests).
    fn detection(&self) -> Result<Detection, String> {
        self.detect().map(|gpus| Detection {
            gpus,
            source: GpuSource::Config,
            nvml_error: None,
        })
    }
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
        self.detection().map(|detection| detection.gpus)
    }

    fn detection(&self) -> Result<Detection, String> {
        let configured = &self.fallback.0;
        match self.primary.detection() {
            Ok(detection) => {
                tracing::info!(
                    "detected {} GPU(s) with NVML (gpu_detection = \"auto\")",
                    detection.gpus.len()
                );
                let uuids = |gpus: &[DetectedGpu]| -> Vec<(GpuIndex, String)> {
                    gpus.iter().map(|g| (g.index, g.uuid.clone())).collect()
                };
                if !configured.is_empty() && uuids(configured) != uuids(&detection.gpus) {
                    tracing::warn!(
                        "the [[gpus]] list in the configuration does not match the GPUs NVML \
                         detected; it is only used when NVML fails, so update or remove it"
                    );
                }
                Ok(detection)
            }
            Err(e) => {
                if configured.is_empty() {
                    tracing::warn!(
                        "GPU detection with NVML failed: {e}; running WITHOUT GPUs (there is no \
                         [[gpus]] list in the configuration to fall back to)"
                    );
                } else {
                    tracing::warn!(
                        "GPU detection with NVML failed: {e}; using the {} GPU(s) of the \
                         [[gpus]] list in the configuration instead",
                        configured.len()
                    );
                }
                Ok(Detection {
                    gpus: configured.clone(),
                    source: GpuSource::Config,
                    nvml_error: Some(e),
                })
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
        let mut message = format!("cannot initialize NVML: {}", errors.join("; "));
        if let Some(reason) = device_node_problem(Path::new("/dev")) {
            message.push_str(&format!(" ({reason})"));
        }
        Err(message)
    }
}

/// Why NVML probably cannot talk to the driver: `/dev/nvidiactl` is missing
/// or cannot be opened by this process. `None` if it can be opened (or
/// there is no NVIDIA hardware to speak of).
pub fn device_node_problem(dev: &Path) -> Option<String> {
    let ctl = dev.join("nvidiactl");
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&ctl)
    {
        Ok(_) => None,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(format!(
            "{} does not exist: the NVIDIA kernel module is not loaded, or the device nodes are \
             hidden from the daemon, e.g. by PrivateDevices=yes in its systemd unit",
            ctl.display()
        )),
        Err(e) => Some(format!(
            "{} cannot be opened: {e}; check DevicePolicy=/DeviceAllow= of the daemon's systemd \
             unit and the permissions of /dev/nvidia*",
            ctl.display()
        )),
    }
}

impl GpuDetector for NvmlGpuDetector {
    fn detection(&self) -> Result<Detection, String> {
        self.detect().map(|gpus| Detection {
            gpus,
            source: GpuSource::Nvml,
            nvml_error: None,
        })
    }

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
            fallback: configured.clone(),
        };
        assert_eq!(working.detect().unwrap(), nvml.0);
    }

    struct FakeNvml(Vec<DetectedGpu>);
    impl GpuDetector for FakeNvml {
        fn detect(&self) -> Result<Vec<DetectedGpu>, String> {
            Ok(self.0.clone())
        }
        fn detection(&self) -> Result<Detection, String> {
            Ok(Detection {
                gpus: self.0.clone(),
                source: GpuSource::Nvml,
                nvml_error: None,
            })
        }
    }

    #[test]
    fn detection_reports_the_source_and_why_nvml_failed() {
        let configured = StaticGpuDetector::anonymous(2);
        let failed = FallbackGpuDetector {
            primary: Box::new(Failing),
            fallback: configured.clone(),
        }
        .detection()
        .unwrap();
        assert_eq!(failed.source, GpuSource::Config);
        assert_eq!(
            failed.nvml_error.as_deref(),
            Some("libnvidia-ml.so.1: not found")
        );
        let status = failed.status(GpuDetection::Auto);
        assert_eq!(status.mode, GpuDetection::Auto);
        assert_eq!(status.gpus.len(), 2);
        assert_eq!(status.gpus[1].uuid, "GPU-test-1");

        let nvml = FallbackGpuDetector {
            primary: Box::new(FakeNvml(StaticGpuDetector::anonymous(1).0)),
            fallback: configured.clone(),
        }
        .detection()
        .unwrap();
        assert_eq!(nvml.source, GpuSource::Nvml);
        assert_eq!(nvml.nvml_error, None);
        assert_eq!(nvml.gpus.len(), 1);

        let fixed = configured.detection().unwrap();
        assert_eq!(fixed.source, GpuSource::Config);
        assert_eq!(fixed.nvml_error, None);
    }

    #[test]
    fn device_node_problems_are_explained() {
        use std::os::unix::fs::PermissionsExt;

        let dev = tempfile::tempdir().unwrap();
        let missing = device_node_problem(dev.path()).unwrap();
        assert!(missing.contains("does not exist"), "{missing}");
        assert!(missing.contains("PrivateDevices"), "{missing}");

        let ctl = dev.path().join("nvidiactl");
        std::fs::write(&ctl, "").unwrap();
        assert_eq!(device_node_problem(dev.path()), None);

        // Root opens anything; the permission case only shows otherwise.
        if !nix::unistd::geteuid().is_root() {
            std::fs::set_permissions(&ctl, std::fs::Permissions::from_mode(0o444)).unwrap();
            let denied = device_node_problem(dev.path()).unwrap();
            assert!(denied.contains("cannot be opened"), "{denied}");
            assert!(denied.contains("DeviceAllow"), "{denied}");
        }
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
