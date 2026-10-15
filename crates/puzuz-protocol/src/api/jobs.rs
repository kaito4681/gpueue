//! Job-related request and response bodies.

use std::collections::BTreeMap;
use std::path::PathBuf;

use puzuz_core::authz;
use puzuz_core::model::{
    Caller, GpuIndex, Job, JobId, JobResult, JobSpec, JobState, Timestamp, Uid,
};
use serde::{Deserialize, Serialize};

use super::ApiError;

/// How the daemon handled a non-administrator's request to launch a job
/// right away (`start` of a queued or stashed job, `add --immediate`,
/// `restart --immediate`, `restore --immediate`): only administrators
/// launch jobs immediately, so the job was queued (for a restore, queued
/// for restore) and competes with other users' jobs by priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueuedLaunch {
    /// First among the caller's own queued jobs, as `puzuz top` does.
    First,
    /// Like any other job, without moving it (`enable_user_top = false`,
    /// or `priority_type = "basic"`, where the owner's order does not
    /// apply).
    InLine,
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

/// A job as seen by a particular caller.
///
/// Everybody may see every job (so users can tell who occupies a GPU), but
/// only the owner and administrators receive [`JobDetails`]. For everyone
/// else `details` is absent: the daemon must build views with
/// [`JobView::for_caller`] so the command, cwd and environment never leave it.
///
/// The environment is large (a full login environment per job), so only
/// `GET /v1/jobs/{id}` includes it; `GET /v1/jobs` (unless asked with
/// `?details=envs`) and events send views without it (see
/// [`JobDetails::envs`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobView {
    pub id: JobId,
    pub owner: Uid,
    pub group: String,
    pub gpus: Vec<GpuIndex>,
    pub dependencies: Vec<JobId>,
    /// pueue's `-o/--priority`. With multifactor priority it orders the
    /// owner's jobs only (see `puzuz prio`).
    pub priority: i32,
    /// Subtracted from the multifactor priority (`puzuz nice`); omitted
    /// when 0.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub nice: i32,
    pub label: Option<String>,
    pub no_checkpoint: bool,
    pub state: JobState,
    pub created_at: Timestamp,
    pub started_at: Option<Timestamp>,
    pub finished_at: Option<Timestamp>,
    /// Size of the stored checkpoint image, if any. Filled in by the daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_size_bytes: Option<u64>,
    /// Owner/admin-only fields; `None` in the redacted view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<JobDetails>,
}

/// Fields only the owner and administrators may see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobDetails {
    pub command: String,
    pub cwd: PathBuf,
    /// The submitting client's environment. Present in single-job views
    /// (`GET /v1/jobs/{id}`, and `GET /v1/jobs?details=envs`); absent from
    /// job lists and events, which would otherwise repeat every job's whole
    /// environment. Absent never means "empty environment": an empty one is
    /// sent as `{}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envs: Option<BTreeMap<String, String>>,
    /// The last error recorded for the job: why its last restore, dump or
    /// spawn failed (e.g. a CRIU log summary). Kept private because it can
    /// mention paths and command lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

impl JobView {
    /// The view with every field, for the owner or an administrator.
    /// `last_error` holds the reason carried by the job's result, if any;
    /// the daemon fills in errors kept elsewhere (failed restores and dumps).
    pub fn full(job: &Job) -> Self {
        let mut view = Self::redacted(job);
        view.label = job.spec.label.clone();
        view.state = job.state.clone();
        let reason = match &job.state {
            JobState::Done { result } | JobState::Terminating { result } => match result {
                JobResult::FailedToSpawn { reason } | JobResult::CheckpointFailed { reason } => {
                    Some(reason.clone())
                }
                _ => None,
            },
            _ => None,
        };
        view.details = Some(JobDetails {
            command: job.spec.command.clone(),
            cwd: job.spec.cwd.clone(),
            envs: Some(job.spec.envs.clone()),
            last_error: reason,
        });
        view
    }

    /// The view for users other than the owner: no command, cwd, envs or
    /// label, and free-text failure reasons are replaced by
    /// [`authz::REDACTED`].
    pub fn redacted(job: &Job) -> Self {
        Self {
            id: job.id,
            owner: job.owner,
            group: job.spec.group.clone(),
            gpus: job.spec.gpus.clone(),
            dependencies: job.spec.dependencies.clone(),
            priority: job.spec.priority,
            nice: job.spec.nice,
            label: None,
            no_checkpoint: job.spec.no_checkpoint,
            state: authz::redact_state(&job.state),
            created_at: job.created_at,
            started_at: job.started_at,
            finished_at: job.finished_at,
            checkpoint_size_bytes: None,
            details: None,
        }
    }

    /// The full view for the owner and administrators, the redacted one for
    /// everybody else.
    pub fn for_caller(job: &Job, caller: &Caller) -> Self {
        if authz::can_view_details(caller, job) {
            Self::full(job)
        } else {
            Self::redacted(job)
        }
    }

    pub fn is_redacted(&self) -> bool {
        self.details.is_none()
    }

    /// Drops the environment (see [`JobDetails::envs`]); for job lists and
    /// events.
    pub fn without_envs(mut self) -> Self {
        if let Some(details) = self.details.as_mut() {
            details.envs = None;
        }
        self
    }
}

// ---------------------------------------------------------------------------
// GET /v1/jobs, POST /v1/jobs
// ---------------------------------------------------------------------------

/// Query of `GET /v1/jobs`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListJobsQuery {
    /// Only jobs in this group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Extra fields to include in every view (`details=envs`). By default
    /// the list leaves out each job's environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<ListDetails>,
}

/// Opt-in extras of `GET /v1/jobs` (`?details=...`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListDetails {
    /// Include [`JobDetails::envs`] (still only in views the caller may see
    /// in full).
    Envs,
}

impl ListDetails {
    pub fn as_str(self) -> &'static str {
        match self {
            ListDetails::Envs => "envs",
        }
    }
}

impl ListJobsQuery {
    /// Renders the query string (without `?`), empty if there is none.
    pub fn to_query_string(&self) -> String {
        let mut q = QueryBuilder::default();
        if let Some(group) = &self.group {
            q.push("group", group);
        }
        if let Some(details) = self.details {
            q.push("details", details.as_str());
        }
        q.finish()
    }

    /// Whether the views should include [`JobDetails::envs`].
    pub fn include_envs(&self) -> bool {
        self.details == Some(ListDetails::Envs)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListJobsResponse {
    pub jobs: Vec<JobView>,
    /// Administrators only: jobs the helper runs although the daemon's
    /// database has them finished, queued, checkpointed or not at all
    /// (e.g. after the database was restored from a backup). Their GPUs
    /// count as taken, a queued job with the same id is not started, and an
    /// administrator can stop them with `puzuz kill <id>`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub orphans: Vec<OrphanJob>,
}

/// A job the helper runs that the daemon does not account for (see
/// [`ListJobsResponse::orphans`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrphanJob {
    pub id: JobId,
    /// The uid it runs as, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<Uid>,
    /// The GPUs it was started with.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpus: Vec<GpuIndex>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

/// Body of `POST /v1/jobs` (`puzuz add`). The owner is taken from the
/// connection's peer credentials, never from the body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddJobRequest {
    #[serde(flatten)]
    pub spec: JobSpec,
    /// Add as stashed instead of queued (`--stashed`).
    #[serde(default)]
    pub stashed: bool,
    /// With `stashed`, enqueue automatically at this time (`--delay`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enqueue_at: Option<Timestamp>,
    /// Start right away, ignoring group and GPU slots (`--immediate`); only
    /// for administrators, anybody else's job is queued first among their
    /// own jobs ([`AddJobResponse::queued`]).
    #[serde(default)]
    pub immediate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddJobResponse {
    pub id: JobId,
    /// An administrator's `--immediate` while the helper is not connected:
    /// the job is queued and starts (still ignoring every limit) as soon as
    /// the helper is back.
    #[serde(default, skip_serializing_if = "is_false")]
    pub launch_pending: bool,
    /// A non-administrator's `--immediate`: the job was queued, not started
    /// ([`QueuedLaunch`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued: Option<QueuedLaunch>,
}

impl AddJobResponse {
    pub fn new(id: JobId) -> Self {
        Self {
            id,
            launch_pending: false,
            queued: None,
        }
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero(value: &i32) -> bool {
    *value == 0
}

// ---------------------------------------------------------------------------
// Job actions
// ---------------------------------------------------------------------------

/// An operation on existing jobs.
///
/// Single-job form: `POST /v1/jobs/{id}/{action}` (`DELETE /v1/jobs/{id}` for
/// remove) with the variant's options as the body. Bulk form:
/// `POST /v1/jobs/bulk` with a [`BulkJobRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum JobAction {
    Pause(PauseOptions),
    /// Resume a paused job (not a checkpointed one; that is `restore`).
    Start,
    Kill(KillOptions),
    /// Checkpoint a running job and stop it.
    Checkpoint,
    Restore(RestoreOptions),
    /// Run the job again as a new job; the checkpoint image is kept.
    Restart(RestartOptions),
    Stash(StashOptions),
    Enqueue(EnqueueOptions),
    /// Delete the job, its log and its checkpoint image.
    Remove,
}

impl JobAction {
    /// The path segment of the single-job endpoint, and the wire name.
    pub fn name(&self) -> &'static str {
        match self {
            JobAction::Pause(_) => "pause",
            JobAction::Start => "start",
            JobAction::Kill(_) => "kill",
            JobAction::Checkpoint => "checkpoint",
            JobAction::Restore(_) => "restore",
            JobAction::Restart(_) => "restart",
            JobAction::Stash(_) => "stash",
            JobAction::Enqueue(_) => "enqueue",
            JobAction::Remove => "remove",
        }
    }

    /// The JSON body of the single-job endpoint, or `None` if it takes none.
    pub fn options_json(&self) -> Option<serde_json::Value> {
        let value = match self {
            JobAction::Pause(o) => serde_json::to_value(o),
            JobAction::Kill(o) => serde_json::to_value(o),
            JobAction::Restore(o) => serde_json::to_value(o),
            JobAction::Restart(o) => serde_json::to_value(o),
            JobAction::Stash(o) => serde_json::to_value(o),
            JobAction::Enqueue(o) => serde_json::to_value(o),
            JobAction::Start | JobAction::Checkpoint | JobAction::Remove => return None,
        };
        // Plain structs of primitives always serialize.
        Some(value.expect("action options serialize"))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseOptions {
    /// With a group or all selector: pause the group(s) but let running
    /// jobs finish (`pause --wait`). Pausing groups is admin-only, so this
    /// does nothing for other users.
    #[serde(default)]
    pub wait: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KillOptions {
    /// Signal number to send; `None` means the default (SIGTERM, then SIGKILL
    /// after the grace period).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreOptions {
    /// Restore now without waiting for free group/GPU slots (`-i`); only
    /// for administrators, anybody else's job is queued for restore first
    /// among their own jobs.
    #[serde(default)]
    pub immediate: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartOptions {
    /// Start the new job right away (`-k/--start-immediately`).
    #[serde(default)]
    pub immediate: bool,
    /// Add the new job as stashed (`-s/--stashed`).
    #[serde(default)]
    pub stashed: bool,
    /// Reuse the job instead of creating a new one (`-i/--in-place`).
    #[serde(default)]
    pub in_place: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StashOptions {
    /// Enqueue automatically at this time (`--delay`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enqueue_at: Option<Timestamp>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnqueueOptions {
    /// Enqueue at this time instead of now (`--delay`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<Timestamp>,
}

/// Which jobs a bulk request applies to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobSelector {
    Ids {
        ids: Vec<JobId>,
    },
    /// Every job of the group the caller may modify (`-g`). For an
    /// administrator, `pause` and `kill` also pause the group itself and
    /// `start` resumes it, as in pueue.
    Group {
        group: String,
    },
    /// The caller's own jobs, or every job for administrators (`-a`). For
    /// an administrator, `pause` and `kill` also pause every group and
    /// `start` resumes every group, as in pueue.
    All,
}

/// Body of `POST /v1/jobs/bulk`, e.g.
/// `{"target":{"kind":"ids","ids":[1,2]},"action":"kill","signal":9}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BulkJobRequest {
    pub target: JobSelector,
    #[serde(flatten)]
    pub action: JobAction,
}

/// Body of `200 OK` from `POST /v1/jobs/bulk`. Failures are reported per
/// job; the request as a whole only fails for malformed input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BulkJobResponse {
    pub results: Vec<JobActionResult>,
    /// Groups whose paused flag this request set: paused by `pause` and
    /// `kill`, resumed by `start`. Only administrators change it, with a
    /// `group` or `all` target; empty otherwise (omitted from the JSON).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
}

impl BulkJobResponse {
    /// A response that changed no group.
    pub fn new(results: Vec<JobActionResult>) -> Self {
        Self {
            results,
            groups: Vec::new(),
        }
    }

    pub fn all_ok(&self) -> bool {
        self.results.iter().all(JobActionResult::is_ok)
    }
}

/// Result of an action on one job. Also the success body of the single-job
/// endpoints (where failures are reported as an HTTP error instead).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobActionResult {
    pub id: JobId,
    #[serde(flatten)]
    pub outcome: ActionOutcome,
}

impl JobActionResult {
    pub fn ok(id: JobId) -> Self {
        Self {
            id,
            outcome: ActionOutcome::Ok {
                new_id: None,
                launch_pending: false,
                queued: None,
            },
        }
    }

    pub fn error(id: JobId, error: ApiError) -> Self {
        Self {
            id,
            outcome: ActionOutcome::Error { error },
        }
    }

    pub fn is_ok(&self) -> bool {
        matches!(self.outcome, ActionOutcome::Ok { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ActionOutcome {
    Ok {
        /// The id of the job created by `restart` (unless in place).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        new_id: Option<JobId>,
        /// The job (for `restart`, the new one) was to be started or
        /// restored right away by an administrator (`start`, `restore
        /// --immediate`, `restart --immediate`), but the helper is not
        /// connected: it is queued and launched, still ignoring every
        /// limit, as soon as the helper is back.
        #[serde(default, skip_serializing_if = "is_false")]
        launch_pending: bool,
        /// The same request by a non-administrator: the job (for `restart`,
        /// the new one) was queued, not launched ([`QueuedLaunch`]).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        queued: Option<QueuedLaunch>,
    },
    Error {
        error: ApiError,
    },
}

// ---------------------------------------------------------------------------
// clean, reset, switch
// ---------------------------------------------------------------------------

/// Body of `POST /v1/jobs/clean`: remove finished jobs the caller may modify.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanRequest {
    /// Only remove successfully finished jobs (`-s`).
    #[serde(default)]
    pub successful_only: bool,
    /// Only this group (`-g`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanResponse {
    pub removed: Vec<JobId>,
}

/// Body of `POST /v1/jobs/reset`: kill and remove every job the caller may
/// modify (own jobs, or all jobs for administrators). Answered with a
/// [`BulkJobResponse`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetRequest {
    /// Only this group (`-g`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

/// Body of `POST /v1/jobs/switch`: swap the queue positions of two jobs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchRequest {
    pub first: JobId,
    pub second: JobId,
}

// ---------------------------------------------------------------------------
// GET /v1/jobs/{id}/log
// ---------------------------------------------------------------------------

/// Query of `GET /v1/jobs/{id}/log`. The response is the raw log bytes
/// (`application/octet-stream`); with `follow`, appended output keeps being
/// streamed (chunked) until the job finishes or the client disconnects.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogQuery {
    /// Only the last N lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail: Option<u64>,
    #[serde(default)]
    pub follow: bool,
}

impl LogQuery {
    /// Renders the query string (without `?`), empty if there is none.
    pub fn to_query_string(&self) -> String {
        let mut q = QueryBuilder::default();
        if let Some(tail) = self.tail {
            q.push("tail", &tail.to_string());
        }
        if self.follow {
            q.push("follow", "true");
        }
        q.finish()
    }
}

#[derive(Default)]
struct QueryBuilder(String);

impl QueryBuilder {
    fn push(&mut self, key: &str, value: &str) {
        if !self.0.is_empty() {
            self.0.push('&');
        }
        self.0.push_str(key);
        self.0.push('=');
        self.0.push_str(&super::routes::percent_encode(value));
    }

    fn finish(self) -> String {
        self.0
    }
}
