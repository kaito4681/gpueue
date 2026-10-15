//! Groups, GPUs and daemon-level request and response bodies.

use gpueue_core::model::{GpuIndex, Group, JobId, Uid};
use serde::{Deserialize, Deserializer, Serialize};

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
/// GPUs at once (`gpueue gpu parallel N [--gpu LIST]`).
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
///
/// It must therefore stay decodable by older and newer clients: the
/// diagnostic fields are optional and decode as `None` when this client
/// does not understand them (e.g. a `gpu_detection` mode added later).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionResponse {
    pub api_version: u32,
    pub daemon_version: String,
    pub helper: HelperStatus,
    /// How much space checkpoint images take (for `gpueue doctor`, which
    /// cannot read the root-only image directory itself). Aggregates only,
    /// so every caller may see it. Absent from daemons that predate it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_option"
    )]
    pub checkpoints: Option<CheckpointStorage>,
    /// How the daemon found its GPUs at start-up (for `gpueue doctor`).
    /// Absent from daemons that predate it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_option"
    )]
    pub gpu_detection: Option<GpuDetectionStatus>,
    /// The schema version of the daemon's database (`PRAGMA user_version`),
    /// which `gpueue update` compares with what a release supports before
    /// installing it. Absent from daemons that predate it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_option"
    )]
    pub schema_version: Option<i64>,
    /// Jobs the helper runs that the daemon's database does not account
    /// for (orphans, see [`ListJobsResponse::orphans`](super::ListJobsResponse::orphans)),
    /// for `gpueue doctor`. Ids only. Absent from daemons that predate it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_option"
    )]
    pub orphan_jobs: Option<Vec<JobId>>,
}

/// Decodes an optional field, yielding `None` instead of an error when the
/// value does not match this build's type.
fn lenient_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| serde_json::from_value(value).ok()))
}

/// `gpu_detection` in the configuration file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuDetection {
    /// NVML, falling back to the configured `[[gpus]]` list when NVML
    /// cannot be used.
    #[default]
    Auto,
    /// Only the configured `[[gpus]]` list.
    Static,
}

impl GpuDetection {
    pub fn as_str(self) -> &'static str {
        match self {
            GpuDetection::Auto => "auto",
            GpuDetection::Static => "static",
        }
    }
}

/// Where the daemon's GPU list came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuSource {
    /// Detected with NVML.
    Nvml,
    /// The `[[gpus]]` list of the configuration file: `gpu_detection =
    /// "static"`, or NVML failed (then `nvml_error` says why).
    Config,
    /// A source added by a newer daemon. Never sent by this version.
    #[serde(other)]
    Unknown,
}

/// The result of GPU detection at daemon start-up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuDetectionStatus {
    pub mode: GpuDetection,
    pub source: GpuSource,
    /// Why NVML could not be used (`gpu_detection = "auto"` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nvml_error: Option<String>,
    /// The GPUs detected, by index. GPUs the daemon knew before but did not
    /// detect are not listed (`GET /v1/gpus` shows them as unavailable).
    pub gpus: Vec<DetectedGpuSummary>,
}

/// One detected GPU, as [`GpuDetectionStatus`] lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedGpuSummary {
    pub index: GpuIndex,
    pub uuid: String,
    pub name: String,
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
