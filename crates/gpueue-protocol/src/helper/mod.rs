//! daemon ⇄ helper protocol: newline-delimited JSON over one persistent
//! connection to the helper's socket.
//!
//! Flow:
//! 1. The daemon connects and sends [`DaemonMessage::Hello`]; the helper
//!    answers with [`HelperMessage::Hello`]. Each side drops the connection
//!    if [`Hello::is_compatible`] fails.
//! 2. The daemon sends [`DaemonMessage::Request`]s, each with a fresh
//!    [`RequestId`]; the helper answers each with exactly one
//!    [`HelperMessage::Response`] carrying the same id. Responses may arrive
//!    out of order.
//! 3. The helper pushes [`HelperMessage::Event`]s at any time. Events are
//!    persisted by the helper before being sent and carry an increasing
//!    [`EventSeq`]; after a reconnect the helper replays every event the
//!    daemon has not acknowledged with [`Request::AckEvents`]. The daemon
//!    must therefore handle duplicates idempotently.
//! 4. After (re)connecting, the daemon reconciles its database with
//!    [`Request::ListRunning`].
//!
//! Long operations (checkpoint, restore) are answered with
//! [`ResponseResult::Accepted`] once started; their outcome arrives as an
//! event so it survives a daemon restart in the middle.

mod framing;

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Re-exported so that the helper can use the ids without depending on
/// `gpueue-core` itself.
pub use gpueue_core::model::{GpuIndex, JobId, Uid};

pub use framing::{FrameError, LineBuffer, MAX_LINE_LEN, decode_line, encode_line};

/// Version of this protocol. Bumped on any incompatible change.
///
/// Peers only talk when their versions are equal ([`Hello::is_compatible`]),
/// so a peer must be able to decode every message the other side may send.
/// The rule is therefore:
///
/// - Adding a variant to an enum *without* an `Unknown` fallback
///   ([`DaemonMessage`], [`HelperMessage`], [`Request`], [`ResponseResult`],
///   [`Event`], [`CheckpointOutcome`], [`RestoreOutcome`]), removing or
///   renaming any variant or field, or adding a required field is
///   incompatible: bump this version.
/// - Enums with an `Unknown` fallback ([`HelperErrorCode`], [`RunningPhase`],
///   [`ExitStatus`]) may gain variants without a bump: an older peer decodes
///   them as `Unknown`.
/// - New optional fields (`#[serde(default)]`) need no bump.
///
/// `tests/helper.rs` keeps a snapshot of the variant names per version and
/// fails when they change without a bump.
pub const PROTOCOL_VERSION: u32 = 1;

/// Default path of the helper socket.
pub const DEFAULT_SOCKET_PATH: &str = "/run/gpueue/helper.sock";

/// Linux signal numbers used by the daemon.
pub mod signal {
    pub const SIGINT: i32 = 2;
    pub const SIGKILL: i32 = 9;
    pub const SIGTERM: i32 = 15;
    pub const SIGCONT: i32 = 18;
    pub const SIGSTOP: i32 = 19;
}

/// Identifies a request on one connection; chosen by the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestId(pub u64);

/// Sequence number of a helper event. Strictly increasing across helper
/// restarts (it is persisted with the event queue).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventSeq(pub u64);

/// A Unix group id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Gid(pub u32);

// ---------------------------------------------------------------------------
// Envelopes
// ---------------------------------------------------------------------------

/// Everything the daemon sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonMessage {
    Hello(Hello),
    Request(RequestEnvelope),
}

/// Everything the helper sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HelperMessage {
    Hello(Hello),
    Response(Response),
    Event(EventEnvelope),
}

/// Sent once by each side right after connecting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u32,
    pub binary_version: String,
    /// Helper only: the programs (`criu`, `cuda-checkpoint`, `nvidia-smi`)
    /// it did not find when it started, and so cannot use until it is
    /// restarted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_programs: Vec<String>,
}

impl Hello {
    /// A Hello for this build.
    pub fn current() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            binary_version: crate::BINARY_VERSION.to_owned(),
            missing_programs: Vec::new(),
        }
    }

    /// Whether a peer that sent `other` can be talked to.
    pub fn is_compatible(&self, other: &Hello) -> bool {
        self.protocol_version == other.protocol_version
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestEnvelope {
    pub id: RequestId,
    #[serde(flatten)]
    pub request: Request,
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// Operations the helper performs. The helper validates every field itself
/// and never trusts the daemon's view of ownership (it refuses uid 0, opens
/// cwd and log files with the owner's credentials, and so on).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Start a job. Answered with [`ResponseResult::Spawned`].
    Spawn(SpawnRequest),
    /// Send a signal to every process of a job. Answered with
    /// [`ResponseResult::Ok`].
    Signal { job: JobId, signal: i32 },
    /// Checkpoint a running job and stop it. Answered with
    /// [`ResponseResult::Accepted`], then [`Event::CheckpointFinished`], or
    /// with [`HelperErrorCode::CheckpointRefused`] when the pre-checks
    /// already show the dump cannot succeed (the job is left alone).
    Checkpoint {
        job: JobId,
        image_dir: PathBuf,
        #[serde(default)]
        options: CheckpointOptions,
    },
    /// Restore a checkpointed job. Answered with
    /// [`ResponseResult::Accepted`], then [`Event::RestoreFinished`].
    Restore(RestoreRequest),
    /// Delete a checkpoint image (images are only writable by root).
    /// Answered with [`ResponseResult::Ok`].
    DeleteCheckpoint { job: JobId, image_dir: PathBuf },
    /// List the jobs the helper knows to be alive. Answered with
    /// [`ResponseResult::Running`].
    ListRunning,
    /// The daemon has durably recorded every event up to and including
    /// `up_to`; the helper may forget them. Answered with
    /// [`ResponseResult::Ok`].
    AckEvents { up_to: EventSeq },
}

impl Request {
    /// The wire name of the operation (the `op` field).
    pub fn op(&self) -> &'static str {
        match self {
            Request::Spawn(_) => "spawn",
            Request::Signal { .. } => "signal",
            Request::Checkpoint { .. } => "checkpoint",
            Request::Restore(_) => "restore",
            Request::DeleteCheckpoint { .. } => "delete_checkpoint",
            Request::ListRunning => "list_running",
            Request::AckEvents { .. } => "ack_events",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnRequest {
    pub job: JobId,
    /// The owner; never 0.
    pub uid: Uid,
    pub gid: Gid,
    /// Supplementary groups. `None` lets the helper look them up for `uid`
    /// (`initgroups`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub groups: Option<Vec<Gid>>,
    pub cwd: PathBuf,
    /// The command line, run as `shell + [command]` (e.g. `sh -c <command>`).
    pub command: String,
    pub shell: Vec<String>,
    /// The job's environment, without the CUDA variables: the helper sets
    /// `CUDA_VISIBLE_DEVICES` from `gpus` and `CUDA_DEVICE_ORDER=PCI_BUS_ID`.
    pub envs: BTreeMap<String, String>,
    pub gpus: Vec<GpuIndex>,
    /// stdout and stderr are appended to this file, opened as the owner.
    pub log_path: PathBuf,
    /// `add --no-checkpoint`: when the helper shuts down, leave this job to
    /// be stopped by systemd (SIGTERM, then SIGKILL) instead of dumping it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_checkpoint: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreRequest {
    pub job: JobId,
    /// The owner the image was dumped from; the helper checks it matches.
    pub uid: Uid,
    pub image_dir: PathBuf,
    /// GPUs the job had; the helper checks they are the same model and
    /// driver as at dump time.
    pub gpus: Vec<GpuIndex>,
    /// The job was paused when it was dumped: leave the restored processes
    /// stopped, as SIGSTOP leaves them (a later SIGCONT resumes them), so
    /// that they do not run a single instruction before the owner resumes
    /// the job. A helper that predates this field ignores it and resumes
    /// them; its [`RestoreOutcome::Success`] then lacks `stopped`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub leave_stopped: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointOptions {
    /// Close established TCP connections instead of refusing to dump
    /// (`criu --tcp-close`).
    #[serde(default)]
    pub tcp_close: bool,
    /// Keep the job running after the dump (reserved; not in initial scope).
    #[serde(default)]
    pub leave_running: bool,
    /// Give up after this many seconds (the job is left running).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    pub id: RequestId,
    #[serde(flatten)]
    pub result: ResponseResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum ResponseResult {
    Ok,
    Spawned {
        /// PID of the job's main process, in the helper's PID namespace.
        pid: u32,
    },
    /// A long operation has started; its outcome follows as an event.
    Accepted,
    Running {
        jobs: Vec<RunningJob>,
    },
    Error {
        code: HelperErrorCode,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningJob {
    pub job: JobId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub phase: RunningPhase,
    /// The uid the job runs as. Absent from helpers that predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<Uid>,
    /// The GPUs the job was started with, so the daemon can count the
    /// slots of a job its database does not account for. Absent from
    /// helpers that predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpus: Option<Vec<GpuIndex>>,
}

/// What the helper is doing with a live job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunningPhase {
    Running,
    /// Stopped with SIGSTOP.
    Stopped,
    Checkpointing,
    Restoring,
    /// A phase added by a newer helper. Never sent by this version.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HelperErrorCode {
    /// The request failed validation (uid 0, relative paths, ...).
    InvalidRequest,
    /// The owner may not access the cwd, log file or image.
    PermissionDenied,
    /// The helper does not know this job.
    JobNotFound,
    /// The job is already running (spawn/restore) or busy (checkpoint).
    Busy,
    /// The helper is shutting down and accepts no new spawns, restores or
    /// checkpoints.
    ShuttingDown,
    SpawnFailed,
    /// The helper declined to dump the job before touching it (its mounts
    /// could not be restored, not enough free space, ...). The job keeps
    /// running as it was; the message says why.
    CheckpointRefused,
    /// In a failed [`CheckpointOutcome`]: the dump succeeded (the job's
    /// processes are gone), but the helper could not finalize the
    /// checkpoint. The image is kept; the helper finalizes it before a
    /// restore or at its next start, so the job counts as checkpointed.
    CheckpointUnfinalized,
    Internal,
    /// The helper's job credential policy (`job_min_uid`,
    /// `job_denied_groups`, `job_allowed_users`) refuses to run jobs as the
    /// job's owner or group; the message says why.
    PolicyDenied,
    /// A code added by a newer peer. Never sent by this version.
    #[serde(other)]
    Unknown,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub seq: EventSeq,
    #[serde(flatten)]
    pub event: Event,
}

/// Something that happened to a job, reported by the helper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    JobExited {
        job: JobId,
        status: ExitStatus,
    },
    CheckpointFinished {
        job: JobId,
        outcome: CheckpointOutcome,
    },
    RestoreFinished {
        job: JobId,
        outcome: RestoreOutcome,
    },
}

/// How a job's main process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExitStatus {
    Exited {
        code: i32,
    },
    Signaled {
        signal: i32,
    },
    /// The job is gone but its exit status could not be recovered. Also
    /// what a status kind added by a newer helper decodes as.
    #[serde(other)]
    Unknown,
}

impl ExitStatus {
    pub fn success(self) -> bool {
        matches!(self, ExitStatus::Exited { code: 0 })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum CheckpointOutcome {
    /// The image is complete (marker written) and the job is stopped.
    Success { image_size_bytes: u64 },
    /// The dump failed.
    ///
    /// Without `code`, the dump was attempted: CRIU rolled back and the job
    /// is still running, but it has been frozen and its CUDA state toggled,
    /// so terminating it is up to the daemon. CRIU's log, if any, was kept
    /// for administrators and `reason` names its path.
    Failed {
        reason: String,
        /// Why, when it is not an attempted dump that CRIU rolled back:
        /// `checkpoint_refused` when the pre-checks failed before the job
        /// was touched (it keeps running as it was), and
        /// `checkpoint_unfinalized` when the dump succeeded but the image
        /// is not finalized yet. Absent from older helpers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<HelperErrorCode>,
    },
}

impl CheckpointOutcome {
    /// A dump that was attempted and failed.
    pub fn failed(reason: impl Into<String>) -> Self {
        CheckpointOutcome::Failed {
            reason: reason.into(),
            code: None,
        }
    }

    /// A dump refused before the job was touched.
    pub fn refused(reason: impl Into<String>) -> Self {
        CheckpointOutcome::Failed {
            reason: reason.into(),
            code: Some(HelperErrorCode::CheckpointRefused),
        }
    }

    /// A dump that succeeded but could not be finalized (see
    /// [`HelperErrorCode::CheckpointUnfinalized`]).
    pub fn unfinalized(reason: impl Into<String>) -> Self {
        CheckpointOutcome::Failed {
            reason: reason.into(),
            code: Some(HelperErrorCode::CheckpointUnfinalized),
        }
    }

    /// Whether the dump was refused before the job was touched.
    pub fn is_refused(&self) -> bool {
        matches!(
            self,
            CheckpointOutcome::Failed {
                code: Some(HelperErrorCode::CheckpointRefused),
                ..
            }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum RestoreOutcome {
    /// The job runs again (or, with `stopped`, is stopped); the helper has
    /// removed the image.
    Success {
        pid: u32,
        /// The processes were left stopped, as asked with
        /// [`RestoreRequest::leave_stopped`]. Absent from older helpers,
        /// which always resume them.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        stopped: bool,
    },
    /// The image was left in place.
    Failed {
        reason: String,
        /// Why, when it is not a failure of the restore itself. Only
        /// `shutting_down` so far: the helper was stopping and abandoned the
        /// restore before anything ran (the daemon queues the job again).
        /// Absent from older helpers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<HelperErrorCode>,
    },
}

impl RestoreOutcome {
    /// A failure of the restore itself.
    pub fn failed(reason: impl Into<String>) -> Self {
        RestoreOutcome::Failed {
            reason: reason.into(),
            code: None,
        }
    }
}
