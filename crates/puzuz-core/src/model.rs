//! Domain types shared by every puzuz component.
//!
//! Timestamps are Unix epoch milliseconds (`i64`) to keep this crate free of
//! time-library dependencies and trivially storable in SQLite.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Unix epoch milliseconds.
pub type Timestamp = i64;

/// Name of the group every job belongs to unless told otherwise.
pub const DEFAULT_GROUP: &str = "default";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JobId(pub u64);

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A Unix user id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Uid(pub u32);

impl fmt::Display for Uid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A GPU index in `CUDA_DEVICE_ORDER=PCI_BUS_ID` order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GpuIndex(pub u32);

impl fmt::Display for GpuIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Who is asking. Derived from `SO_PEERCRED`, never from the request body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caller {
    pub uid: Uid,
    /// Root or a member of the `puzuz-admin` group.
    pub is_admin: bool,
}

/// Everything needed to (re)spawn a job, fixed at submission time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSpec {
    /// The command line, run through the configured shell (`sh -c`).
    pub command: String,
    pub cwd: PathBuf,
    /// The submitting client's environment.
    pub envs: BTreeMap<String, String>,
    pub group: String,
    /// GPUs the job needs; empty means a CPU-only job.
    pub gpus: Vec<GpuIndex>,
    /// Jobs that must finish successfully before this one may start.
    pub dependencies: Vec<JobId>,
    pub priority: i32,
    pub label: Option<String>,
    /// Do not checkpoint on shutdown; just SIGTERM, then SIGKILL.
    pub no_checkpoint: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub id: JobId,
    pub owner: Uid,
    pub spec: JobSpec,
    pub state: JobState,
    pub created_at: Timestamp,
    /// Set when the job first started running; kept across checkpoint/restore.
    pub started_at: Option<Timestamp>,
    pub finished_at: Option<Timestamp>,
}

/// Lifecycle state of a job.
///
/// Mirrors pueue's task states and adds checkpoint-related ones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum JobState {
    /// Waiting for a free slot (group parallelism and GPUs).
    Queued { enqueued_at: Timestamp },
    /// Held back until explicitly enqueued, optionally at a given time.
    Stashed { enqueue_at: Option<Timestamp> },
    Running,
    Paused,
    /// A checkpoint is being written.
    Checkpointing,
    /// Saved to disk and stopped; resumed with `restore`.
    Checkpointed,
    /// Waiting for a free slot to be restored from its checkpoint.
    RestoreQueued { enqueued_at: Timestamp },
    /// A checkpoint is being restored.
    Restoring,
    Done { result: JobResult },
}

impl JobState {
    /// A short, stable name used in messages and storage.
    pub fn name(&self) -> &'static str {
        match self {
            JobState::Queued { .. } => "queued",
            JobState::Stashed { .. } => "stashed",
            JobState::Running => "running",
            JobState::Paused => "paused",
            JobState::Checkpointing => "checkpointing",
            JobState::Checkpointed => "checkpointed",
            JobState::RestoreQueued { .. } => "restore_queued",
            JobState::Restoring => "restoring",
            JobState::Done { .. } => "done",
        }
    }

    /// Whether the job currently holds group and GPU slots.
    pub fn occupies_slot(&self) -> bool {
        matches!(
            self,
            JobState::Running | JobState::Paused | JobState::Checkpointing | JobState::Restoring
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum JobResult {
    Success,
    Failed { exit_code: i32 },
    FailedToSpawn { reason: String },
    Killed,
    /// A dependency did not finish successfully.
    DependencyFailed,
    /// Checkpointing failed, so the job was terminated instead.
    CheckpointFailed { reason: String },
    /// The job disappeared while the daemon was not watching it.
    Lost,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    pub name: String,
    pub parallel: u32,
    pub paused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gpu {
    pub index: GpuIndex,
    pub uuid: String,
    pub name: String,
    /// Maximum number of jobs allowed on this GPU at once.
    pub parallel: u32,
}
