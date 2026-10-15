//! The `/v1` HTTP API (axum), served on the daemon's Unix socket.

mod extract;
mod log;
mod sse;

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use gpueue_core::model::{GpuIndex, JobId, Uid};
use gpueue_protocol::BINARY_VERSION;
use gpueue_protocol::api::routes::{self, JobActionKind};
use gpueue_protocol::api::{
    API_VERSION, AddJobRequest, AddJobResponse, BulkJobRequest, CleanRequest, CreateGroupRequest,
    EnqueueOptions, ErrorCode, HelperStatus, JobAction, JobActionResult, JobView, KillOptions,
    ListGpusResponse, ListGroupsResponse, ListJobsQuery, ListJobsResponse, NiceRequest,
    PauseOptions, ResetRequest, RestartOptions, RestoreOptions, SetGpusParallelRequest,
    SetGroupPriorityRequest, SetParallelRequest, SetSharesRequest, ShutdownRequest, SiteRequest,
    StashOptions, SwitchRequest, TopRequest, VERSION_HEADER, VersionResponse, WARNINGS_HEADER,
    Warning,
};

use crate::error::{ApiResult, Failure};
use crate::state::{Core, PendingWarnings};
pub use extract::{Auth, PeerCred, TEST_UID_HEADER};
use extract::{JsonBody, OptionalJson};

pub type AppState = Arc<Core>;

/// Runs a job operation to completion even if the client goes away: the
/// handler future is dropped when the connection closes, and an operation
/// cut off between its commit and its helper requests would leave a job
/// marked `Running` that was never spawned (and its launch unsettled, see
/// [`crate::launch`]).
async fn detached<T, F>(operation: F) -> ApiResult<T>
where
    F: Future<Output = ApiResult<T>> + Send + 'static,
    T: Send + 'static,
{
    match tokio::spawn(operation).await {
        Ok(result) => result,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(_) => Err(Failure::internal("the operation was cancelled")),
    }
}

pub fn router(core: Arc<Core>) -> Router {
    Router::new()
        .route(routes::JOBS, get(list_jobs).post(add_job))
        .route(routes::JOBS_BULK, post(bulk))
        .route(routes::JOBS_CLEAN, post(clean))
        .route(routes::JOBS_RESET, post(reset))
        .route(routes::JOBS_SWITCH, post(switch))
        .route(routes::JOBS_NICE, post(nice))
        .route(routes::JOBS_TOP, post(top))
        .route(routes::JOBS_SITE, post(site))
        .route(routes::PRIORITY, get(priorities))
        .route(routes::SHARES, get(shares))
        .route(routes::SHARE_TEMPLATE, put(set_shares))
        .route(routes::GROUP_PRIORITY_TEMPLATE, put(set_group_priority))
        .route(routes::JOB_TEMPLATE, get(get_job).delete(remove_job))
        .route(routes::JOB_LOG_TEMPLATE, get(log::job_log))
        .route(routes::JOB_ACTION_TEMPLATE, post(job_action))
        .route(routes::GROUPS, get(list_groups).post(create_group))
        .route(routes::GROUP_TEMPLATE, delete(delete_group))
        .route(routes::GROUP_PARALLEL_TEMPLATE, put(set_group_parallel))
        .route(routes::GPUS, get(list_gpus))
        .route(routes::GPUS_PARALLEL, put(set_gpus_parallel))
        .route(routes::GPU_PARALLEL_TEMPLATE, put(set_gpu_parallel))
        .route(routes::DAEMON_SHUTDOWN, post(shutdown))
        .route(routes::VERSION, get(version))
        .route(routes::EVENTS, get(sse::events))
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(
            core.clone(),
            attach_warnings,
        ))
        .layer(middleware::from_fn(check_version))
        .with_state(core)
}

/// Hands the caller's pending warnings (a failed checkpoint of one of their
/// jobs, checkpoint images over the size limit) to the CLI in the
/// [`WARNINGS_HEADER`] of a successful response, once. Only requests from the gpueue CLI (with the [`VERSION_HEADER`]) get
/// them, and `/v1/version` never does.
async fn attach_warnings(State(core): State<AppState>, request: Request, next: Next) -> Response {
    let uid = if request.headers().contains_key(VERSION_HEADER)
        && request.uri().path() != routes::VERSION
    {
        extract::peer_uid(
            request.headers(),
            request.extensions(),
            core.trust_uid_header,
        )
        .ok()
    } else {
        None
    };
    // Looked up before the handler runs so the reply is not delayed; a
    // failure caused by this very request is reported on the next one.
    let pending = match uid {
        Some(uid) => core.pending_warnings(Uid(uid)).await.unwrap_or_else(|e| {
            tracing::warn!("cannot read pending warnings: {}", e.0.message);
            PendingWarnings::default()
        }),
        None => PendingWarnings::default(),
    };
    let mut response = next.run(request).await;
    if pending.warnings.is_empty() || !response.status().is_success() {
        return response;
    }
    let value = match HeaderValue::from_str(&Warning::encode_header(&pending.warnings)) {
        Ok(value) => value,
        Err(e) => {
            tracing::warn!("cannot encode warnings: {e}");
            return response;
        }
    };
    let Some(uid) = uid else {
        return response;
    };
    if let Err(e) = core.acknowledge_warnings(Uid(uid), &pending).await {
        tracing::warn!("cannot record delivered warnings: {}", e.0.message);
    }
    response.headers_mut().insert(WARNINGS_HEADER, value);
    response
}

async fn not_found() -> Failure {
    Failure::new(ErrorCode::BadRequest, "no such endpoint")
}

/// Rejects clients of a different version (except on `/v1/version`, so
/// mismatched clients can still report versions). Requests without the
/// header (e.g. `curl`) are accepted.
async fn check_version(request: Request, next: Next) -> Response {
    if request.uri().path() != routes::VERSION
        && let Some(client) = request.headers().get(VERSION_HEADER)
    {
        let client = client.to_str().unwrap_or("?");
        if client != BINARY_VERSION {
            let message = if is_newer(client, BINARY_VERSION) {
                format!(
                    "the daemon (gpueue {BINARY_VERSION}) is older than this client (gpueue \
                     {client}); an administrator can restart it with `sudo systemctl restart \
                     gpueue` (running jobs keep running)"
                )
            } else {
                format!(
                    "this client (gpueue {client}) is older than the daemon (gpueue \
                     {BINARY_VERSION}); update the gpueue command"
                )
            };
            return Failure::new(ErrorCode::VersionMismatch, message).into_response();
        }
    }
    next.run(request).await
}

/// Compares dotted version numbers; anything unparsable counts as older.
fn is_newer(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.split(['.', '-', '+'])
            .map_while(|part| part.parse().ok())
            .collect()
    };
    parse(a) > parse(b)
}

fn parse_job_id(raw: &str) -> ApiResult<JobId> {
    raw.parse()
        .map(JobId)
        .map_err(|_| Failure::bad_request(format!("invalid job id {raw:?}")))
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

async fn list_jobs(
    State(core): State<AppState>,
    Auth(caller): Auth,
    query: Result<Query<ListJobsQuery>, axum::extract::rejection::QueryRejection>,
) -> ApiResult<Json<ListJobsResponse>> {
    let Query(query) = query.map_err(|e| Failure::bad_request(e.body_text()))?;
    let include_envs = query.include_envs();
    let mut jobs = core.list_jobs(caller, query.group).await?;
    // Every job's whole environment would make the list huge; it is sent by
    // `GET /v1/jobs/{id}`, or here on request.
    if !include_envs {
        jobs = jobs.into_iter().map(JobView::without_envs).collect();
    }
    // Only administrators can do something about orphans.
    let orphans = if caller.is_admin {
        core.orphans().iter().map(|orphan| orphan.view()).collect()
    } else {
        Vec::new()
    };
    Ok(Json(ListJobsResponse { jobs, orphans }))
}

async fn add_job(
    State(core): State<AppState>,
    Auth(caller): Auth,
    JsonBody(request): JsonBody<AddJobRequest>,
) -> ApiResult<(StatusCode, Json<AddJobResponse>)> {
    let response = detached(async move { core.add_job(caller, request).await }).await?;
    Ok((StatusCode::CREATED, Json(response)))
}

async fn get_job(
    State(core): State<AppState>,
    Auth(caller): Auth,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let id = parse_job_id(&id)?;
    let info = core.job_info(id).await?;
    Ok(Json(info.view(&caller)).into_response())
}

async fn remove_job(
    State(core): State<AppState>,
    Auth(caller): Auth,
    Path(id): Path<String>,
) -> ApiResult<Json<JobActionResult>> {
    let id = parse_job_id(&id)?;
    detached(async move {
        core.job_action(caller, id, JobAction::Remove, Vec::new())
            .await
    })
    .await?;
    Ok(Json(JobActionResult::ok(id)))
}

async fn job_action(
    State(core): State<AppState>,
    Auth(caller): Auth,
    Path((id, action)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> ApiResult<Json<JobActionResult>> {
    let id = parse_job_id(&id)?;
    let kind = JobActionKind::from_segment(&action)
        .filter(|kind| *kind != JobActionKind::Remove)
        .ok_or_else(|| Failure::bad_request(format!("unknown job action {action:?}")))?;
    fn opts<T: serde::de::DeserializeOwned + Default>(body: &[u8]) -> ApiResult<T> {
        extract::parse_optional(body)
    }
    let action = match kind {
        JobActionKind::Pause => JobAction::Pause(opts::<PauseOptions>(&body)?),
        JobActionKind::Start => JobAction::Start,
        JobActionKind::Kill => JobAction::Kill(opts::<KillOptions>(&body)?),
        JobActionKind::Checkpoint => JobAction::Checkpoint,
        JobActionKind::Restore => JobAction::Restore(opts::<RestoreOptions>(&body)?),
        JobActionKind::Restart => JobAction::Restart(opts::<RestartOptions>(&body)?),
        JobActionKind::Stash => JobAction::Stash(opts::<StashOptions>(&body)?),
        JobActionKind::Enqueue => JobAction::Enqueue(opts::<EnqueueOptions>(&body)?),
        JobActionKind::Remove => unreachable!("filtered above"),
    };
    let done = detached(async move { core.job_action_by_id(caller, id, action, Vec::new()).await })
        .await?;
    Ok(Json(JobActionResult {
        id,
        outcome: done.outcome(),
    }))
}

async fn bulk(
    State(core): State<AppState>,
    Auth(caller): Auth,
    JsonBody(request): JsonBody<BulkJobRequest>,
) -> ApiResult<Response> {
    let response = detached(async move { core.bulk(caller, request).await }).await?;
    Ok(Json(response).into_response())
}

async fn clean(
    State(core): State<AppState>,
    Auth(caller): Auth,
    OptionalJson(request): OptionalJson<CleanRequest>,
) -> ApiResult<Response> {
    let response = detached(async move { core.clean(caller, request).await }).await?;
    Ok(Json(response).into_response())
}

async fn reset(
    State(core): State<AppState>,
    Auth(caller): Auth,
    OptionalJson(request): OptionalJson<ResetRequest>,
) -> ApiResult<Response> {
    let response = detached(async move { core.reset(caller, request).await }).await?;
    Ok(Json(response).into_response())
}

async fn switch(
    State(core): State<AppState>,
    Auth(caller): Auth,
    JsonBody(request): JsonBody<SwitchRequest>,
) -> ApiResult<StatusCode> {
    core.switch(caller, request).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Priority and fair share
// ---------------------------------------------------------------------------

async fn nice(
    State(core): State<AppState>,
    Auth(caller): Auth,
    JsonBody(request): JsonBody<NiceRequest>,
) -> ApiResult<Response> {
    Ok(Json(core.nice(caller, request).await?).into_response())
}

async fn top(
    State(core): State<AppState>,
    Auth(caller): Auth,
    JsonBody(request): JsonBody<TopRequest>,
) -> ApiResult<Response> {
    Ok(Json(core.top(caller, request).await?).into_response())
}

async fn site(
    State(core): State<AppState>,
    Auth(caller): Auth,
    JsonBody(request): JsonBody<SiteRequest>,
) -> ApiResult<Response> {
    Ok(Json(core.site(caller, request).await?).into_response())
}

async fn priorities(State(core): State<AppState>, Auth(_): Auth) -> ApiResult<Response> {
    Ok(Json(core.priorities().await?).into_response())
}

async fn shares(State(core): State<AppState>, Auth(_): Auth) -> ApiResult<Response> {
    Ok(Json(core.shares().await?).into_response())
}

async fn set_shares(
    State(core): State<AppState>,
    Auth(caller): Auth,
    Path(uid): Path<String>,
    JsonBody(request): JsonBody<SetSharesRequest>,
) -> ApiResult<StatusCode> {
    let uid = Uid(uid
        .parse()
        .map_err(|_| Failure::bad_request(format!("invalid uid {uid:?}")))?);
    core.set_shares(caller, uid, request.shares).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn set_group_priority(
    State(core): State<AppState>,
    Auth(caller): Auth,
    Path(name): Path<String>,
    JsonBody(request): JsonBody<SetGroupPriorityRequest>,
) -> ApiResult<Response> {
    let group = core
        .set_group_priority(caller, name, request.priority)
        .await?;
    Ok(Json(group).into_response())
}

// ---------------------------------------------------------------------------
// Groups
// ---------------------------------------------------------------------------

async fn list_groups(State(core): State<AppState>, Auth(_): Auth) -> ApiResult<Response> {
    let groups = core.list_groups().await?;
    Ok(Json(ListGroupsResponse { groups }).into_response())
}

async fn create_group(
    State(core): State<AppState>,
    Auth(caller): Auth,
    JsonBody(request): JsonBody<CreateGroupRequest>,
) -> ApiResult<Response> {
    let group = core.create_group(caller, request).await?;
    Ok((StatusCode::CREATED, Json(group)).into_response())
}

async fn delete_group(
    State(core): State<AppState>,
    Auth(caller): Auth,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    core.delete_group(caller, name).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn set_group_parallel(
    State(core): State<AppState>,
    Auth(caller): Auth,
    Path(name): Path<String>,
    JsonBody(request): JsonBody<SetParallelRequest>,
) -> ApiResult<Response> {
    let group = core
        .set_group_parallel(caller, name, request.parallel)
        .await?;
    Ok(Json(group).into_response())
}

// ---------------------------------------------------------------------------
// GPUs
// ---------------------------------------------------------------------------

async fn list_gpus(State(core): State<AppState>, Auth(_): Auth) -> ApiResult<Response> {
    let gpus = core.list_gpus().await?;
    Ok(Json(ListGpusResponse { gpus }).into_response())
}

async fn set_gpus_parallel(
    State(core): State<AppState>,
    Auth(caller): Auth,
    JsonBody(request): JsonBody<SetGpusParallelRequest>,
) -> ApiResult<Response> {
    let gpus = core.set_gpus_parallel(caller, request).await?;
    Ok(Json(ListGpusResponse { gpus }).into_response())
}

async fn set_gpu_parallel(
    State(core): State<AppState>,
    Auth(caller): Auth,
    Path(index): Path<String>,
    JsonBody(request): JsonBody<SetParallelRequest>,
) -> ApiResult<Response> {
    let index = GpuIndex(
        index
            .parse()
            .map_err(|_| Failure::bad_request(format!("invalid GPU index {index:?}")))?,
    );
    let gpus = core
        .set_gpus_parallel(
            caller,
            SetGpusParallelRequest {
                gpus: Some(vec![index]),
                parallel: request.parallel,
            },
        )
        .await?;
    let view = gpus
        .into_iter()
        .find(|g| g.index == index)
        .ok_or_else(|| Failure::from(gpueue_core::Error::GpuNotFound(index)))?;
    Ok(Json(view).into_response())
}

// ---------------------------------------------------------------------------
// Daemon
// ---------------------------------------------------------------------------

async fn shutdown(
    State(core): State<AppState>,
    Auth(caller): Auth,
    OptionalJson(request): OptionalJson<ShutdownRequest>,
) -> ApiResult<StatusCode> {
    gpueue_core::authz::require_admin_for(&caller, gpueue_core::authz::AdminOperation::Shutdown)?;
    if !core.is_shutting_down() {
        let core = core.clone();
        tokio::spawn(async move { core.shutdown(request.no_checkpoint).await });
    }
    Ok(StatusCode::ACCEPTED)
}

async fn version(State(core): State<AppState>) -> Json<VersionResponse> {
    let helper = core.helper.state();
    let checkpoints = core
        .checkpoint_storage()
        .await
        .inspect_err(|e| tracing::warn!("cannot sum up checkpoint sizes: {}", e.0.message))
        .ok();
    Json(VersionResponse {
        api_version: API_VERSION,
        daemon_version: BINARY_VERSION.to_owned(),
        helper: HelperStatus {
            connected: helper.connected,
            version: helper.version,
        },
        checkpoints,
        gpu_detection: Some(core.gpu_detection.clone()),
        // The database is at this version once the daemon has opened it.
        schema_version: Some(gpueue_core::storage::SCHEMA_VERSION),
        orphan_jobs: Some(core.orphans().iter().map(|orphan| orphan.job).collect()),
    })
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn version_comparison() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.10.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("garbage", "0.1.0"));
    }
}
