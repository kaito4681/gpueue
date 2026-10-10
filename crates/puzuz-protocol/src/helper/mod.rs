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
/// `puzuz-core` itself.
pub use puzuz_core::model::{GpuIndex, JobId, Uid};

pub use framing::{FrameError, LineBuffer, MAX_LINE_LEN, decode_line, encode_line};

/// Version of this protocol. Bumped on any incompatible change.
pub const PROTOCOL_VERSION: u32 = 1;

/// Default path of the helper socket.
pub const DEFAULT_SOCKET_PATH: &str = "/run/puzuz/helper.sock";

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
}

impl Hello {
    /// A Hello for this build.
    pub fn current() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            binary_version: crate::BINARY_VERSION.to_owned(),
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
    /// [`ResponseResult::Accepted`], then [`Event::CheckpointFinished`].
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
    /// The helper is shutting down and accepts no new spawns or restores.
    ShuttingDown,
    SpawnFailed,
    Internal,
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
    /// The job is gone but its exit status could not be recovered.
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
    /// The dump failed; CRIU rolled back and the job is still running.
    /// Terminating it is up to the daemon.
    Failed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum RestoreOutcome {
    /// The job runs again; the helper has removed the image.
    Success { pid: u32 },
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
