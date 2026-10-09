//! The `/v1` HTTP API (axum), served on the daemon's Unix socket.

mod extract;
mod log;
mod sse;

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use puzuz_core::model::{GpuIndex, JobId};
use puzuz_protocol::BINARY_VERSION;
use puzuz_protocol::api::routes::{self, JobActionKind};
use puzuz_protocol::api::{
    API_VERSION, AddJobRequest, AddJobResponse, BulkJobRequest, CleanRequest, CreateGroupRequest,
    EnqueueOptions, ErrorCode, HelperStatus, JobAction, JobActionResult, KillOptions,
    ListGpusResponse, ListGroupsResponse, ListJobsQuery, ListJobsResponse, PauseOptions,
    ResetRequest, RestartOptions, RestoreOptions, SetGpusParallelRequest, SetParallelRequest,
    ShutdownRequest, StashOptions, SwitchRequest, VERSION_HEADER, VersionResponse,
};

use crate::error::{ApiResult, Failure};
use crate::state::Core;
pub use extract::{Auth, PeerCred, TEST_UID_HEADER};
use extract::{JsonBody, OptionalJson};

pub type AppState = Arc<Core>;

pub fn router(core: Arc<Core>) -> Router {
    Router::new()
        .route(routes::JOBS, get(list_jobs).post(add_job))
        .route(routes::JOBS_BULK, post(bulk))
        .route(routes::JOBS_CLEAN, post(clean))
        .route(routes::JOBS_RESET, post(reset))
        .route(routes::JOBS_SWITCH, post(switch))
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
        .layer(middleware::from_fn(check_version))
        .with_state(core)
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
                    "the daemon (puzuz {BINARY_VERSION}) is older than this client (puzuz \
                     {client}); an administrator can restart it with `sudo systemctl restart \
                     puzuz` (running jobs keep running)"
                )
            } else {
                format!(
                    "this client (puzuz {client}) is older than the daemon (puzuz \
                     {BINARY_VERSION}); update the puzuz command"
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
    let jobs = core.list_jobs(caller, query.group).await?;
    Ok(Json(ListJobsResponse { jobs }))
}

async fn add_job(
    State(core): State<AppState>,
    Auth(caller): Auth,
    JsonBody(request): JsonBody<AddJobRequest>,
) -> ApiResult<(StatusCode, Json<AddJobResponse>)> {
    let id = core.add_job(caller, request).await?;
    Ok((StatusCode::CREATED, Json(AddJobResponse { id })))
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
    core.job_action(caller, id, JobAction::Remove, Vec::new())
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
    let new_id = core.job_action(caller, id, action, Vec::new()).await?;
    Ok(Json(JobActionResult {
        id,
        outcome: puzuz_protocol::api::ActionOutcome::Ok { new_id },
    }))
}

async fn bulk(
    State(core): State<AppState>,
    Auth(caller): Auth,
    JsonBody(request): JsonBody<BulkJobRequest>,
) -> ApiResult<Response> {
    Ok(Json(core.bulk(caller, request).await?).into_response())
}

async fn clean(
    State(core): State<AppState>,
    Auth(caller): Auth,
    OptionalJson(request): OptionalJson<CleanRequest>,
) -> ApiResult<Response> {
    Ok(Json(core.clean(caller, request).await?).into_response())
}

async fn reset(
    State(core): State<AppState>,
    Auth(caller): Auth,
    OptionalJson(request): OptionalJson<ResetRequest>,
) -> ApiResult<Response> {
    Ok(Json(core.reset(caller, request).await?).into_response())
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
        .ok_or_else(|| Failure::from(puzuz_core::Error::GpuNotFound(index)))?;
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
    puzuz_core::authz::require_admin_for(&caller, puzuz_core::authz::AdminOperation::Shutdown)?;
    if !core.is_shutting_down() {
        let core = core.clone();
        tokio::spawn(async move { core.shutdown(request.no_checkpoint).await });
    }
    Ok(StatusCode::ACCEPTED)
}

async fn version(State(core): State<AppState>) -> Json<VersionResponse> {
    let helper = core.helper.state();
    Json(VersionResponse {
        api_version: API_VERSION,
        daemon_version: BINARY_VERSION.to_owned(),
        helper: HelperStatus {
            connected: helper.connected,
            version: helper.version,
        },
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
