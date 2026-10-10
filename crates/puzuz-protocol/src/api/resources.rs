//! Groups, GPUs and daemon-level request and response bodies.

use puzuz_core::model::{GpuIndex, Group, JobId, Uid};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Groups
// ---------------------------------------------------------------------------

/// Body of `200 OK` from `GET /v1/groups`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListGroupsResponse {
    pub groups: Vec<Group>,
}

/// Body of `POST /v1/groups` (admin only). Answered with the created
/// [`Group`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateGroupRequest {
    pub name: String,
    /// Defaults to 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel: Option<u32>,
}

/// Body of `PUT /v1/groups/{name}/parallel` and `PUT /v1/gpus/{index}/parallel`
/// (admin only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetParallelRequest {
    pub parallel: u32,
}

// ---------------------------------------------------------------------------
// GPUs
// ---------------------------------------------------------------------------

/// Body of `200 OK` from `GET /v1/gpus`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListGpusResponse {
    pub gpus: Vec<GpuView>,
}

/// A GPU and who is using it. Visible to everybody.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuView {
    pub index: GpuIndex,
    pub uuid: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_total_bytes: Option<u64>,
    /// NVIDIA driver version reported by detection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver_version: Option<String>,
    /// Whether the GPU was detected when the daemon started. A GPU that was
    /// known before but is missing now stays listed (with its slot limit)
    /// but starts no jobs; jobs that need it stay queued. Omitted when true.
    #[serde(default = "available_default", skip_serializing_if = "is_available")]
    pub available: bool,
    /// Maximum number of jobs at once.
    pub parallel: u32,
    /// Slots currently taken.
    pub used: u32,
    /// Jobs holding a slot on this GPU.
    pub jobs: Vec<GpuJob>,
}

fn available_default() -> bool {
    true
}

fn is_available(available: &bool) -> bool {
    *available
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuJob {
    pub id: JobId,
    pub owner: Uid,
}

/// Body of `PUT /v1/gpus/parallel` (admin only): set the limit of several
/// GPUs at once (`puzuz gpu parallel N [--gpu LIST]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetGpusParallelRequest {
    /// `None` means every GPU.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpus: Option<Vec<GpuIndex>>,
    pub parallel: u32,
}

// ---------------------------------------------------------------------------
// Daemon
// ---------------------------------------------------------------------------

/// Body of `POST /v1/daemon/shutdown` (admin only). Answered with
/// `202 Accepted` and an empty body; progress is visible on `/v1/events`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShutdownRequest {
    /// Kill running jobs instead of checkpointing them.
    #[serde(default)]
    pub no_checkpoint: bool,
}

/// Body of `200 OK` from `GET /v1/version`. This endpoint ignores the
/// version header so that mismatched clients can still report versions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionResponse {
    pub api_version: u32,
    pub daemon_version: String,
    pub helper: HelperStatus,
    /// How much space checkpoint images take (for `puzuz doctor`, which
    /// cannot read the root-only image directory itself). Aggregates only,
    /// so every caller may see it. Absent from daemons that predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoints: Option<CheckpointStorage>,
}

/// Checkpoint image usage, as the daemon accounts for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointStorage {
    /// Number of complete images.
    pub images: u64,
    /// Total size of the complete images.
    pub total_bytes: u64,
    /// `checkpoint_size_limit_bytes`; `None` without a limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_bytes: Option<u64>,
    /// Free space (for unprivileged users) on the filesystem holding the
    /// image directory; `None` when the daemon cannot tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_bytes: Option<u64>,
}

impl CheckpointStorage {
    /// Whether the images exceed the configured limit.
    pub fn over_limit(&self) -> bool {
        self.limit_bytes
            .is_some_and(|limit| self.total_bytes > limit)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperStatus {
    pub connected: bool,
    /// The helper's binary version from its last Hello.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}
