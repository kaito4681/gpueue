//! GPU detection.
//!
//! The spec calls for NVML detection at start-up (in `PCI_BUS_ID` order).
//! That is not implemented yet: [`StaticGpuDetector`] reports the GPUs listed
//! in the configuration file. A future `NvmlGpuDetector` implements the same
//! trait.

use puzuz_core::model::GpuIndex;

use crate::config::GpuConfig;

/// A GPU found at start-up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedGpu {
    pub index: GpuIndex,
    pub uuid: String,
    pub name: String,
    pub memory_total_bytes: Option<u64>,
}

pub trait GpuDetector: Send + Sync + 'static {
    /// The GPUs present now, in `CUDA_DEVICE_ORDER=PCI_BUS_ID` order.
    fn detect(&self) -> Result<Vec<DetectedGpu>, String>;
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
