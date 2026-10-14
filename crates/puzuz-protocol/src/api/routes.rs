//! Endpoints of the `/v1` API.
//!
//! [`Route`] renders concrete request paths for the client; the `*_TEMPLATE`
//! constants are the same paths in axum's `{param}` syntax for the server.

use std::fmt;

use puzuz_core::model::{GpuIndex, JobId, Uid};

use super::JobAction;

pub const JOBS: &str = "/v1/jobs";
pub const JOB_TEMPLATE: &str = "/v1/jobs/{id}";
/// `{action}` is [`JobAction::name`] (except `remove`, which is `DELETE` on
/// [`JOB_TEMPLATE`]).
pub const JOB_ACTION_TEMPLATE: &str = "/v1/jobs/{id}/{action}";
pub const JOB_LOG_TEMPLATE: &str = "/v1/jobs/{id}/log";
pub const JOBS_BULK: &str = "/v1/jobs/bulk";
pub const JOBS_CLEAN: &str = "/v1/jobs/clean";
pub const JOBS_RESET: &str = "/v1/jobs/reset";
pub const JOBS_SWITCH: &str = "/v1/jobs/switch";
pub const JOBS_NICE: &str = "/v1/jobs/nice";
pub const JOBS_TOP: &str = "/v1/jobs/top";
pub const JOBS_SITE: &str = "/v1/jobs/site";
pub const GROUPS: &str = "/v1/groups";
pub const GROUP_TEMPLATE: &str = "/v1/groups/{name}";
pub const GROUP_PARALLEL_TEMPLATE: &str = "/v1/groups/{name}/parallel";
pub const GROUP_PRIORITY_TEMPLATE: &str = "/v1/groups/{name}/priority";
pub const PRIORITY: &str = "/v1/priority";
pub const SHARES: &str = "/v1/shares";
pub const SHARE_TEMPLATE: &str = "/v1/shares/{uid}";
pub const GPUS: &str = "/v1/gpus";
pub const GPUS_PARALLEL: &str = "/v1/gpus/parallel";
pub const GPU_PARALLEL_TEMPLATE: &str = "/v1/gpus/{index}/parallel";
pub const DAEMON_SHUTDOWN: &str = "/v1/daemon/shutdown";
pub const VERSION: &str = "/v1/version";
pub const EVENTS: &str = "/v1/events";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    Get,
    Post,
    Put,
    Delete,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
        }
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Every endpoint, with its request and success response types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// `GET /v1/jobs` — query [`super::ListJobsQuery`] → [`super::ListJobsResponse`]
    /// (views without `envs` unless `?details=envs`).
    ListJobs,
    /// `POST /v1/jobs` — [`super::AddJobRequest`] → `201` [`super::AddJobResponse`].
    AddJob,
    /// `GET /v1/jobs/{id}` → [`super::JobView`] (with `envs` for the owner and
    /// administrators).
    GetJob(JobId),
    /// `POST /v1/jobs/{id}/{action}` (`DELETE /v1/jobs/{id}` for remove) —
    /// the action's options → [`super::JobActionResult`].
    JobAction(JobId, JobActionKind),
    /// `POST /v1/jobs/bulk` — [`super::BulkJobRequest`] → [`super::BulkJobResponse`].
    BulkJobAction,
    /// `POST /v1/jobs/clean` — [`super::CleanRequest`] → [`super::CleanResponse`].
    CleanJobs,
    /// `POST /v1/jobs/reset` — [`super::ResetRequest`] → [`super::BulkJobResponse`].
    ResetJobs,
    /// `POST /v1/jobs/switch` — [`super::SwitchRequest`] → `204`.
    SwitchJobs,
    /// `GET /v1/jobs/{id}/log` — query [`super::LogQuery`] → raw bytes.
    JobLog(JobId),
    /// `POST /v1/jobs/nice` — [`super::NiceRequest`] → [`super::BulkJobResponse`].
    NiceJobs,
    /// `POST /v1/jobs/top` — [`super::TopRequest`] → [`super::BulkJobResponse`].
    TopJobs,
    /// `POST /v1/jobs/site` — [`super::SiteRequest`] → [`super::BulkJobResponse`].
    SiteJobs,
    /// `GET /v1/priority` → [`super::PriorityResponse`].
    Priority,
    /// `GET /v1/shares` → [`super::SharesResponse`].
    ListShares,
    /// `PUT /v1/shares/{uid}` — [`super::SetSharesRequest`] → `204`.
    SetShares(Uid),
    /// `PUT /v1/groups/{name}/priority` — [`super::SetGroupPriorityRequest`] → group.
    SetGroupPriority(String),
    /// `GET /v1/groups` → [`super::ListGroupsResponse`].
    ListGroups,
    /// `POST /v1/groups` — [`super::CreateGroupRequest`] → `201` group.
    CreateGroup,
    /// `DELETE /v1/groups/{name}` → `204`.
    DeleteGroup(String),
    /// `PUT /v1/groups/{name}/parallel` — [`super::SetParallelRequest`] → group.
    SetGroupParallel(String),
    /// `GET /v1/gpus` → [`super::ListGpusResponse`].
    ListGpus,
    /// `PUT /v1/gpus/{index}/parallel` — [`super::SetParallelRequest`] → [`super::GpuView`].
    SetGpuParallel(GpuIndex),
    /// `PUT /v1/gpus/parallel` — [`super::SetGpusParallelRequest`] → [`super::ListGpusResponse`].
    SetGpusParallel,
    /// `POST /v1/daemon/shutdown` — [`super::ShutdownRequest`] → `202`.
    Shutdown,
    /// `GET /v1/version` → [`super::VersionResponse`].
    Version,
    /// `GET /v1/events` → SSE stream of [`super::Event`].
    Events,
}

/// [`JobAction`] without its options, for routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobActionKind {
    Pause,
    Start,
    Kill,
    Checkpoint,
    Restore,
    Restart,
    Stash,
    Enqueue,
    Remove,
}

impl JobActionKind {
    pub const ALL: [JobActionKind; 9] = [
        JobActionKind::Pause,
        JobActionKind::Start,
        JobActionKind::Kill,
        JobActionKind::Checkpoint,
        JobActionKind::Restore,
        JobActionKind::Restart,
        JobActionKind::Stash,
        JobActionKind::Enqueue,
        JobActionKind::Remove,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            JobActionKind::Pause => "pause",
            JobActionKind::Start => "start",
            JobActionKind::Kill => "kill",
            JobActionKind::Checkpoint => "checkpoint",
            JobActionKind::Restore => "restore",
            JobActionKind::Restart => "restart",
            JobActionKind::Stash => "stash",
            JobActionKind::Enqueue => "enqueue",
            JobActionKind::Remove => "remove",
        }
    }

    /// Parses the `{action}` path segment.
    pub fn from_segment(segment: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == segment)
    }
}

impl From<&JobAction> for JobActionKind {
    fn from(action: &JobAction) -> Self {
        match action {
            JobAction::Pause(_) => JobActionKind::Pause,
            JobAction::Start => JobActionKind::Start,
            JobAction::Kill(_) => JobActionKind::Kill,
            JobAction::Checkpoint => JobActionKind::Checkpoint,
            JobAction::Restore(_) => JobActionKind::Restore,
            JobAction::Restart(_) => JobActionKind::Restart,
            JobAction::Stash(_) => JobActionKind::Stash,
            JobAction::Enqueue(_) => JobActionKind::Enqueue,
            JobAction::Remove => JobActionKind::Remove,
        }
    }
}

impl Route {
    pub fn method(&self) -> Method {
        match self {
            Route::ListJobs
            | Route::GetJob(_)
            | Route::JobLog(_)
            | Route::ListGroups
            | Route::ListGpus
            | Route::Version
            | Route::Events
            | Route::Priority
            | Route::ListShares => Method::Get,
            Route::JobAction(_, JobActionKind::Remove) | Route::DeleteGroup(_) => Method::Delete,
            Route::SetGroupParallel(_)
            | Route::SetGpuParallel(_)
            | Route::SetGpusParallel
            | Route::SetShares(_)
            | Route::SetGroupPriority(_) => Method::Put,
            Route::AddJob
            | Route::JobAction(..)
            | Route::BulkJobAction
            | Route::CleanJobs
            | Route::ResetJobs
            | Route::SwitchJobs
            | Route::NiceJobs
            | Route::TopJobs
            | Route::SiteJobs
            | Route::CreateGroup
            | Route::Shutdown => Method::Post,
        }
    }

    /// The concrete request path (without query string), with path
    /// parameters percent-encoded.
    pub fn path(&self) -> String {
        match self {
            Route::ListJobs | Route::AddJob => JOBS.to_owned(),
            Route::GetJob(id) | Route::JobAction(id, JobActionKind::Remove) => {
                format!("{JOBS}/{id}")
            }
            Route::JobAction(id, kind) => format!("{JOBS}/{id}/{}", kind.as_str()),
            Route::BulkJobAction => JOBS_BULK.to_owned(),
            Route::CleanJobs => JOBS_CLEAN.to_owned(),
            Route::ResetJobs => JOBS_RESET.to_owned(),
            Route::SwitchJobs => JOBS_SWITCH.to_owned(),
            Route::NiceJobs => JOBS_NICE.to_owned(),
            Route::TopJobs => JOBS_TOP.to_owned(),
            Route::SiteJobs => JOBS_SITE.to_owned(),
            Route::Priority => PRIORITY.to_owned(),
            Route::ListShares => SHARES.to_owned(),
            Route::SetShares(uid) => format!("{SHARES}/{uid}"),
            Route::SetGroupPriority(name) => {
                format!("{GROUPS}/{}/priority", percent_encode(name))
            }
            Route::JobLog(id) => format!("{JOBS}/{id}/log"),
            Route::ListGroups | Route::CreateGroup => GROUPS.to_owned(),
            Route::DeleteGroup(name) => format!("{GROUPS}/{}", percent_encode(name)),
            Route::SetGroupParallel(name) => {
                format!("{GROUPS}/{}/parallel", percent_encode(name))
            }
            Route::ListGpus => GPUS.to_owned(),
            Route::SetGpuParallel(index) => format!("{GPUS}/{index}/parallel"),
            Route::SetGpusParallel => GPUS_PARALLEL.to_owned(),
            Route::Shutdown => DAEMON_SHUTDOWN.to_owned(),
            Route::Version => VERSION.to_owned(),
            Route::Events => EVENTS.to_owned(),
        }
    }
}

/// Percent-encodes everything but RFC 3986 unreserved characters, for path
/// segments and query values.
pub fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}
