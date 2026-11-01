//! Domain types shared by every gpueue component.
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
    /// Root or a member of the `gpueue-admin` group.
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
    /// The job's place among its owner's jobs (pueue's `-o/--priority`);
    /// higher first. With multifactor priority it only orders the owner's
    /// own jobs (see [`crate::priority`]); with basic priority it orders
    /// every job.
    pub priority: i32,
    pub label: Option<String>,
    /// Do not checkpoint on shutdown; just SIGTERM, then SIGKILL.
    pub no_checkpoint: bool,
    /// Subtracted from the multifactor priority (Slurm's `--nice`). Users
    /// may only set values `>= 0`; negative values are for administrators.
    /// Can be changed after submission (`gpueue nice`).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub nice: i32,
}

fn is_zero(value: &i32) -> bool {
    *value == 0
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
    /// Added to the multifactor priority; set by administrators (Slurm's
    /// site factor). Not copied by `restart`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub site_factor: i32,
    /// Set by `gpueue top` (Slurm's `scontrol top`): jobs with a value come
    /// first among their owner's jobs, higher values first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_seq: Option<u64>,
}

/// Lifecycle state of a job.
///
/// Mirrors pueue's task states and adds checkpoint-related ones.
///
/// # JSON representation
///
/// This is the representation used on the wire (`GET /v1/jobs`, events,
/// `status --json`) and in storage (`jobs.state_data`). It is a flat object:
///
/// - `state` names the variant ([`JobState::name`]), and the variant's
///   fields sit next to it:
///   `{"state":"queued","enqueued_at":1700000000000}`, `{"state":"running"}`.
/// - `done` and `terminating` carry a [`JobResult`] whose fields are
///   flattened into the same object: `result` names the result variant and
///   its fields sit alongside, e.g.
///   `{"state":"done","result":"success"}`,
///   `{"state":"done","result":"failed","exit_code":1}`,
///   `{"state":"terminating","result":"checkpoint_failed","reason":"..."}`.
///
/// Every key is unique across a state and its result (`state`,
/// `enqueued_at`, `enqueue_at`, `result`, `exit_code`, `reason`), so each
/// variant is unambiguous. Schema versions up to 3 stored the result nested
/// (`{"state":"done","result":{"result":"failed","exit_code":1}}`); migration
/// 4 rewrites those rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum JobState {
    /// Waiting for a free slot (group parallelism and GPUs).
    Queued {
        enqueued_at: Timestamp,
    },
    /// Held back until explicitly enqueued, optionally at a given time.
    Stashed {
        enqueue_at: Option<Timestamp>,
    },
    Running,
    Paused,
    /// A checkpoint is being written.
    Checkpointing,
    /// Saved to disk and stopped; resumed with `restore`.
    Checkpointed,
    /// Waiting for a free slot to be restored from its checkpoint.
    RestoreQueued {
        enqueued_at: Timestamp,
    },
    /// A checkpoint is being restored.
    Restoring,
    /// Being stopped (SIGTERM, then SIGKILL) after a failed checkpoint. The
    /// process may still hold its GPUs, so the job keeps its slots until the
    /// exit is observed; it then becomes `Done` with `result`.
    Terminating {
        #[serde(flatten)]
        result: JobResult,
    },
    Done {
        #[serde(flatten)]
        result: JobResult,
    },
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
            JobState::Terminating { .. } => "terminating",
            JobState::Done { .. } => "done",
        }
    }

    /// Whether the job waits to be restored from its checkpoint image
    /// (removing it deletes the image).
    pub fn awaits_restore(&self) -> bool {
        matches!(
            self,
            JobState::Checkpointed | JobState::RestoreQueued { .. }
        )
    }

    /// Whether the job currently holds group and GPU slots.
    pub fn occupies_slot(&self) -> bool {
        matches!(
            self,
            JobState::Running
                | JobState::Paused
                | JobState::Checkpointing
                | JobState::Restoring
                | JobState::Terminating { .. }
        )
    }
}

/// How a job ended. Serialized as a flat object tagged by `result`, e.g.
/// `{"result":"failed","exit_code":1}`; inside a [`JobState`] these keys are
/// flattened into the state object (see there).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum JobResult {
    Success,
    Failed {
        exit_code: i32,
    },
    FailedToSpawn {
        reason: String,
    },
    Killed,
    /// A dependency did not finish successfully.
    DependencyFailed,
    /// Checkpointing failed, so the job was terminated instead.
    CheckpointFailed {
        reason: String,
    },
    /// The job disappeared while the daemon was not watching it.
    Lost,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    pub name: String,
    pub parallel: u32,
    pub paused: bool,
    /// The group's priority (Slurm's partition `PriorityJobFactor`); its
    /// share of the highest group priority is the partition factor of the
    /// group's jobs. Set by administrators (`gpueue group priority`).
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub priority: u32,
}

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gpu {
    pub index: GpuIndex,
    pub uuid: String,
    pub name: String,
    /// Maximum number of jobs allowed on this GPU at once.
    pub parallel: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every state with its exact JSON. This is the wire and storage format;
    /// changing it needs a storage migration.
    fn golden() -> Vec<(JobState, &'static str)> {
        vec![
            (
                JobState::Queued { enqueued_at: 5 },
                r#"{"state":"queued","enqueued_at":5}"#,
            ),
            (
                JobState::Stashed { enqueue_at: None },
                r#"{"state":"stashed","enqueue_at":null}"#,
            ),
            (
                JobState::Stashed {
                    enqueue_at: Some(9),
                },
                r#"{"state":"stashed","enqueue_at":9}"#,
            ),
            (JobState::Running, r#"{"state":"running"}"#),
            (JobState::Paused, r#"{"state":"paused"}"#),
            (JobState::Checkpointing, r#"{"state":"checkpointing"}"#),
            (JobState::Checkpointed, r#"{"state":"checkpointed"}"#),
            (
                JobState::RestoreQueued { enqueued_at: 7 },
                r#"{"state":"restore_queued","enqueued_at":7}"#,
            ),
            (JobState::Restoring, r#"{"state":"restoring"}"#),
            (
                JobState::Terminating {
                    result: JobResult::CheckpointFailed {
                        reason: "criu".into(),
                    },
                },
                r#"{"state":"terminating","result":"checkpoint_failed","reason":"criu"}"#,
            ),
            (
                JobState::Done {
                    result: JobResult::Success,
                },
                r#"{"state":"done","result":"success"}"#,
            ),
            (
                JobState::Done {
                    result: JobResult::Failed { exit_code: -3 },
                },
                r#"{"state":"done","result":"failed","exit_code":-3}"#,
            ),
            (
                JobState::Done {
                    result: JobResult::FailedToSpawn {
                        reason: "no such file".into(),
                    },
                },
                r#"{"state":"done","result":"failed_to_spawn","reason":"no such file"}"#,
            ),
            (
                JobState::Done {
                    result: JobResult::Killed,
                },
                r#"{"state":"done","result":"killed"}"#,
            ),
            (
                JobState::Done {
                    result: JobResult::DependencyFailed,
                },
                r#"{"state":"done","result":"dependency_failed"}"#,
            ),
            (
                JobState::Done {
                    result: JobResult::CheckpointFailed {
                        reason: "dump".into(),
                    },
                },
                r#"{"state":"done","result":"checkpoint_failed","reason":"dump"}"#,
            ),
            (
                JobState::Done {
                    result: JobResult::Lost,
                },
                r#"{"state":"done","result":"lost"}"#,
            ),
        ]
    }

    #[test]
    fn job_state_json_is_flat_and_round_trips() {
        for (state, json) in golden() {
            assert_eq!(serde_json::to_string(&state).unwrap(), json);
            assert_eq!(serde_json::from_str::<JobState>(json).unwrap(), state);
        }
    }

    #[test]
    fn job_state_json_key_order_does_not_matter() {
        let state: JobState =
            serde_json::from_str(r#"{"exit_code":1,"result":"failed","state":"done"}"#).unwrap();
        assert_eq!(
            state,
            JobState::Done {
                result: JobResult::Failed { exit_code: 1 }
            }
        );
    }

    #[test]
    fn incomplete_or_old_job_state_json_is_rejected() {
        for json in [
            // A finished job needs a result ...
            r#"{"state":"done"}"#,
            // ... with its fields.
            r#"{"state":"done","result":"failed"}"#,
            r#"{"state":"terminating","result":"checkpoint_failed"}"#,
            r#"{"state":"done","result":"exploded"}"#,
            // The nested shape of schema versions <= 3.
            r#"{"state":"done","result":{"result":"failed","exit_code":1}}"#,
            r#"{"state":"queued"}"#,
            r#"{"state":"unknown"}"#,
        ] {
            assert!(
                serde_json::from_str::<JobState>(json).is_err(),
                "{json} should be rejected"
            );
        }
    }

    #[test]
    fn job_result_json_is_tagged_by_result() {
        let result = JobResult::Failed { exit_code: 2 };
        let json = serde_json::to_string(&result).unwrap();
        assert_eq!(json, r#"{"result":"failed","exit_code":2}"#);
        assert_eq!(serde_json::from_str::<JobResult>(&json).unwrap(), result);
    }
}
