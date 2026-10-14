//! CLI ⇄ daemon API: REST + JSON over HTTP on a Unix socket.
//!
//! Conventions:
//! - Every path starts with `/v1` ([`API_PREFIX`]). A breaking change gets a
//!   new prefix (`/v2`) instead of changing these types incompatibly.
//! - Every request carries the [`VERSION_HEADER`] header with the client's
//!   [`crate::BINARY_VERSION`]; the daemon answers a mismatch with
//!   [`ErrorCode::VersionMismatch`].
//! - Errors are an HTTP status plus an [`ApiError`] JSON body.
//! - Operations on several jobs go through [`routes::Route::BulkJobAction`],
//!   which always answers `200 OK` with one [`JobActionResult`] per job.
//! - Request bodies with only optional fields may be empty; the daemon then
//!   uses `Default::default()`.

mod error;
mod events;
mod jobs;
mod priority;
mod resources;
pub mod routes;
mod warnings;

pub use error::{ApiError, ErrorCode};
pub use events::{Event, SseDecoder, SseMessage};
pub use jobs::{
    ActionOutcome, AddJobRequest, AddJobResponse, BulkJobRequest, BulkJobResponse, CleanRequest,
    CleanResponse, EnqueueOptions, JobAction, JobActionResult, JobDetails, JobSelector, JobView,
    KillOptions, ListDetails, ListJobsQuery, ListJobsResponse, LogQuery, OrphanJob, PauseOptions,
    ResetRequest, RestartOptions, RestoreOptions, StashOptions, SwitchRequest,
};
pub use priority::{
    NiceRequest, PriorityResponse, PriorityWeights, SetGroupPriorityRequest, SetSharesRequest,
    SharesResponse, SiteRequest, TopRequest,
};
pub use resources::{
    CheckpointStorage, CreateGroupRequest, DetectedGpuSummary, GpuDetection, GpuDetectionStatus,
    GpuJob, GpuSource, GpuView, HelperStatus, ListGpusResponse, ListGroupsResponse,
    SetGpusParallelRequest, SetParallelRequest, ShutdownRequest, VersionResponse,
};
pub use routes::{Method, Route};
pub use warnings::{WARNINGS_HEADER, Warning};

/// Major API version; matches the path prefix.
pub const API_VERSION: u32 = 1;

/// Prefix of every API path.
pub const API_PREFIX: &str = "/v1";

/// Request header carrying the client's binary version (lowercase, as HTTP/2
/// and most libraries normalize header names).
pub const VERSION_HEADER: &str = "x-puzuz-version";

/// Default path of the daemon socket.
pub const DEFAULT_SOCKET_PATH: &str = "/run/puzuz/puzuz.sock";

/// Content type of JSON bodies.
pub const CONTENT_TYPE_JSON: &str = "application/json";

/// Content type of the raw log stream.
pub const CONTENT_TYPE_LOG: &str = "application/octet-stream";

/// Content type of the event stream.
pub const CONTENT_TYPE_EVENTS: &str = "text/event-stream";
