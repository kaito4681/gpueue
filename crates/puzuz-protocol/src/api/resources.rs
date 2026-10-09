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
    /// Maximum number of jobs at once.
    pub parallel: u32,
    /// Slots currently taken.
    pub used: u32,
    /// Jobs holding a slot on this GPU.
    pub jobs: Vec<GpuJob>,
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperStatus {
    pub connected: bool,
    /// The helper's binary version from its last Hello.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}
