//! Async client of the daemon API: HTTP/1.1 over the daemon's Unix socket.
//!
//! Every call opens a fresh connection, which is cheap on a local socket and
//! keeps the client free of connection-pool state. Every request carries the
//! [`VERSION_HEADER`]; error responses are decoded into [`ApiError`].
//!
//! Streaming endpoints return a [`ByteStream`] (job logs) or an
//! [`EventStream`] (Server-Sent Events) that yield data as it arrives.

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{ACCEPT, CONTENT_TYPE, HOST};
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use puzuz_core::model::{GpuIndex, Group, JobId, Uid};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::net::UnixStream;

use crate::BINARY_VERSION;
use crate::api::routes::{JobActionKind, Route};
use crate::api::{
    AddJobRequest, AddJobResponse, ApiError, BulkJobRequest, BulkJobResponse, CONTENT_TYPE_EVENTS,
    CONTENT_TYPE_JSON, CONTENT_TYPE_LOG, CleanRequest, CleanResponse, CreateGroupRequest,
    ErrorCode, Event, GpuView, JobAction, JobActionResult, JobView, ListGpusResponse,
    ListGroupsResponse, ListJobsQuery, ListJobsResponse, LogQuery, Method, NiceRequest,
    PriorityResponse, ResetRequest, SetGpusParallelRequest, SetGroupPriorityRequest,
    SetParallelRequest, SetSharesRequest, SharesResponse, ShutdownRequest, SiteRequest, SseDecoder,
    SwitchRequest, TopRequest, VERSION_HEADER, VersionResponse, WARNINGS_HEADER, Warning,
};

/// Error bodies longer than this are cut when they are not valid JSON.
const MAX_RAW_ERROR_LEN: usize = 512;

/// Why a request failed.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The socket could not be connected to: the daemon is not running
    /// (`NotFound`, `ConnectionRefused`) or the caller may not use it
    /// (`PermissionDenied`).
    #[error("cannot connect to the daemon socket {}: {source}", path.display())]
    Connect {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The daemon answered with an error body.
    #[error("{error}")]
    Api { status: u16, error: ApiError },
    /// The daemon answered with something that is not part of the API.
    #[error("unexpected response from the daemon (HTTP {status}): {message}")]
    UnexpectedResponse { status: u16, message: String },
    /// The HTTP exchange itself failed (connection reset, malformed HTTP, ...).
    #[error("HTTP error while talking to the daemon: {0}")]
    Http(#[from] hyper::Error),
    /// The request body could not be encoded as JSON (e.g. a path that is
    /// not valid UTF-8). Nothing was sent.
    #[error("cannot encode the request: {0}")]
    Encode(#[source] serde_json::Error),
}

impl ClientError {
    /// The daemon's error body, if the daemon answered with one.
    pub fn api_error(&self) -> Option<&ApiError> {
        match self {
            ClientError::Api { error, .. } => Some(error),
            _ => None,
        }
    }

    /// The machine-readable error code, if the daemon answered with one.
    pub fn code(&self) -> Option<ErrorCode> {
        self.api_error().map(|error| error.code)
    }

    /// Whether the socket is missing or nobody listens on it, i.e. the
    /// daemon is not running.
    pub fn is_daemon_down(&self) -> bool {
        matches!(
            self,
            ClientError::Connect { source, .. }
                if matches!(
                    source.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                )
        )
    }
}

pub type Result<T, E = ClientError> = std::result::Result<T, E>;

/// Client of one daemon socket. Cheap to clone (clones share the collected
/// [`Warning`]s).
#[derive(Debug, Clone)]
pub struct Client {
    socket: PathBuf,
    version: String,
    warnings: Arc<Mutex<Vec<Warning>>>,
}

impl Client {
    /// A client sending this binary's [`BINARY_VERSION`].
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            version: BINARY_VERSION.to_owned(),
            warnings: Arc::default(),
        }
    }

    /// Overrides the version sent in [`VERSION_HEADER`] (for tests).
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    /// The version sent in [`VERSION_HEADER`].
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Takes the warnings the daemon attached to responses so far (see
    /// [`WARNINGS_HEADER`]). The daemon sends each warning once, so the
    /// caller should show them to the user.
    pub fn take_warnings(&self) -> Vec<Warning> {
        std::mem::take(&mut *self.warnings.lock().unwrap_or_else(|e| e.into_inner()))
    }

    // -----------------------------------------------------------------------
    // Jobs
    // -----------------------------------------------------------------------

    /// `GET /v1/jobs`.
    pub async fn list_jobs(&self, query: &ListJobsQuery) -> Result<ListJobsResponse> {
        let query = query.to_query_string();
        self.json(&Route::ListJobs, &query, None::<&()>).await
    }

    /// `POST /v1/jobs`.
    pub async fn add_job(&self, request: &AddJobRequest) -> Result<AddJobResponse> {
        self.json(&Route::AddJob, "", Some(request)).await
    }

    /// `GET /v1/jobs/{id}`.
    pub async fn get_job(&self, id: JobId) -> Result<JobView> {
        self.json(&Route::GetJob(id), "", None::<&()>).await
    }

    /// `POST /v1/jobs/{id}/{action}` (`DELETE /v1/jobs/{id}` for remove).
    pub async fn job_action(&self, id: JobId, action: &JobAction) -> Result<JobActionResult> {
        let route = Route::JobAction(id, JobActionKind::from(action));
        let body = action.options_json();
        self.json(&route, "", body.as_ref()).await
    }

    /// `POST /v1/jobs/bulk`.
    pub async fn bulk(&self, request: &BulkJobRequest) -> Result<BulkJobResponse> {
        self.json(&Route::BulkJobAction, "", Some(request)).await
    }

    /// `POST /v1/jobs/clean`.
    pub async fn clean(&self, request: &CleanRequest) -> Result<CleanResponse> {
        self.json(&Route::CleanJobs, "", Some(request)).await
    }

    /// `POST /v1/jobs/reset`.
    pub async fn reset(&self, request: &ResetRequest) -> Result<BulkJobResponse> {
        self.json(&Route::ResetJobs, "", Some(request)).await
    }

    /// `POST /v1/jobs/switch`.
    pub async fn switch(&self, request: &SwitchRequest) -> Result<()> {
        self.empty(&Route::SwitchJobs, "", Some(request)).await
    }

    /// `GET /v1/jobs/{id}/log`: the raw log bytes, streamed. With
    /// `query.follow`, the stream lasts until the job finishes.
    pub async fn job_log(&self, id: JobId, query: &LogQuery) -> Result<ByteStream> {
        let response = self
            .send(
                &Route::JobLog(id),
                &query.to_query_string(),
                None,
                CONTENT_TYPE_LOG,
            )
            .await?;
        Ok(ByteStream {
            body: response.into_body(),
        })
    }

    // -----------------------------------------------------------------------
    // Priority and fair share
    // -----------------------------------------------------------------------

    /// `POST /v1/jobs/nice`.
    pub async fn nice(&self, request: &NiceRequest) -> Result<BulkJobResponse> {
        self.json(&Route::NiceJobs, "", Some(request)).await
    }

    /// `POST /v1/jobs/top`.
    pub async fn top(&self, request: &TopRequest) -> Result<BulkJobResponse> {
        self.json(&Route::TopJobs, "", Some(request)).await
    }

    /// `POST /v1/jobs/site`.
    pub async fn site(&self, request: &SiteRequest) -> Result<BulkJobResponse> {
        self.json(&Route::SiteJobs, "", Some(request)).await
    }

    /// `GET /v1/priority`.
    pub async fn priority(&self) -> Result<PriorityResponse> {
        self.json(&Route::Priority, "", None::<&()>).await
    }

    /// `GET /v1/shares`.
    pub async fn shares(&self) -> Result<SharesResponse> {
        self.json(&Route::ListShares, "", None::<&()>).await
    }

    /// `PUT /v1/shares/{uid}`.
    pub async fn set_shares(&self, uid: Uid, shares: u32) -> Result<()> {
        self.empty(
            &Route::SetShares(uid),
            "",
            Some(&SetSharesRequest { shares }),
        )
        .await
    }

    /// `PUT /v1/groups/{name}/priority`.
    pub async fn set_group_priority(&self, name: &str, priority: u32) -> Result<Group> {
        let route = Route::SetGroupPriority(name.to_owned());
        self.json(&route, "", Some(&SetGroupPriorityRequest { priority }))
            .await
    }

    // -----------------------------------------------------------------------
    // Groups, GPUs, daemon
    // -----------------------------------------------------------------------

    /// `GET /v1/groups`.
    pub async fn list_groups(&self) -> Result<ListGroupsResponse> {
        self.json(&Route::ListGroups, "", None::<&()>).await
    }

    /// `POST /v1/groups`.
    pub async fn create_group(&self, request: &CreateGroupRequest) -> Result<Group> {
        self.json(&Route::CreateGroup, "", Some(request)).await
    }

    /// `DELETE /v1/groups/{name}`.
    pub async fn delete_group(&self, name: &str) -> Result<()> {
        self.empty(&Route::DeleteGroup(name.to_owned()), "", None::<&()>)
            .await
    }

    /// `PUT /v1/groups/{name}/parallel`.
    pub async fn set_group_parallel(&self, name: &str, parallel: u32) -> Result<Group> {
        let route = Route::SetGroupParallel(name.to_owned());
        self.json(&route, "", Some(&SetParallelRequest { parallel }))
            .await
    }

    /// `GET /v1/gpus`.
    pub async fn list_gpus(&self) -> Result<ListGpusResponse> {
        self.json(&Route::ListGpus, "", None::<&()>).await
    }

    /// `PUT /v1/gpus/{index}/parallel`.
    pub async fn set_gpu_parallel(&self, index: GpuIndex, parallel: u32) -> Result<GpuView> {
        let route = Route::SetGpuParallel(index);
        self.json(&route, "", Some(&SetParallelRequest { parallel }))
            .await
    }

    /// `PUT /v1/gpus/parallel`.
    pub async fn set_gpus_parallel(
        &self,
        request: &SetGpusParallelRequest,
    ) -> Result<ListGpusResponse> {
        self.json(&Route::SetGpusParallel, "", Some(request)).await
    }

    /// `POST /v1/daemon/shutdown`.
    pub async fn shutdown(&self, request: &ShutdownRequest) -> Result<()> {
        self.empty(&Route::Shutdown, "", Some(request)).await
    }

    /// `GET /v1/version`. The daemon answers this even on a version
    /// mismatch, so it can be used to explain one.
    pub async fn daemon_version(&self) -> Result<VersionResponse> {
        self.json(&Route::Version, "", None::<&()>).await
    }

    /// `GET /v1/events`: the event stream. Subscribe before reading the state
    /// you want to keep up to date, so no change is missed in between.
    pub async fn events(&self) -> Result<EventStream> {
        let response = self
            .send(&Route::Events, "", None, CONTENT_TYPE_EVENTS)
            .await?;
        Ok(EventStream {
            body: ByteStream {
                body: response.into_body(),
            },
            decoder: SseDecoder::new(),
            pending: VecDeque::new(),
        })
    }

    // -----------------------------------------------------------------------
    // Plumbing
    // -----------------------------------------------------------------------

    /// Sends a request with an optional JSON body and decodes a JSON answer.
    async fn json<B, T>(&self, route: &Route, query: &str, body: Option<&B>) -> Result<T>
    where
        B: Serialize + ?Sized,
        T: DeserializeOwned,
    {
        let body = body.map(encode_json).transpose()?;
        let response = self.send(route, query, body, CONTENT_TYPE_JSON).await?;
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await?.to_bytes();
        serde_json::from_slice(&bytes).map_err(|err| ClientError::UnexpectedResponse {
            status,
            message: format!("invalid JSON body: {err}"),
        })
    }

    /// Sends a request whose successful answer has no meaningful body.
    async fn empty<B>(&self, route: &Route, query: &str, body: Option<&B>) -> Result<()>
    where
        B: Serialize + ?Sized,
    {
        let body = body.map(encode_json).transpose()?;
        let response = self.send(route, query, body, CONTENT_TYPE_JSON).await?;
        // Drain the body so the connection shuts down cleanly.
        response.into_body().collect().await?;
        Ok(())
    }

    /// Connects, sends one request and returns the response if its status is
    /// a success; error statuses are turned into [`ClientError`]s.
    async fn send(
        &self,
        route: &Route,
        query: &str,
        body: Option<Vec<u8>>,
        accept: &str,
    ) -> Result<Response<Incoming>> {
        let stream =
            UnixStream::connect(&self.socket)
                .await
                .map_err(|source| ClientError::Connect {
                    path: self.socket.clone(),
                    source,
                })?;
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        // The connection task ends by itself once the response body has been
        // read (or dropped) and `sender` is gone.
        tokio::spawn(async move {
            let _ = connection.await;
        });

        let mut uri = route.path();
        if !query.is_empty() {
            uri.push('?');
            uri.push_str(query);
        }
        let has_body = body.is_some();
        let mut builder = Request::builder()
            .method(hyper_method(route.method()))
            .uri(uri)
            .header(HOST, "localhost")
            .header(ACCEPT, accept)
            .header(VERSION_HEADER, &self.version);
        if has_body {
            builder = builder.header(CONTENT_TYPE, CONTENT_TYPE_JSON);
        }
        let request = builder
            .body(Full::new(Bytes::from(body.unwrap_or_default())))
            .map_err(|err| ClientError::UnexpectedResponse {
                status: 0,
                message: format!("could not build the request: {err}"),
            })?;

        let response = sender.send_request(request).await?;
        if response.status().is_success() {
            if let Some(value) = response.headers().get(WARNINGS_HEADER)
                && let Ok(value) = value.to_str()
            {
                let mut warnings = self.warnings.lock().unwrap_or_else(|e| e.into_inner());
                for warning in Warning::parse_header(value) {
                    if !warnings.contains(&warning) {
                        warnings.push(warning);
                    }
                }
            }
            Ok(response)
        } else {
            Err(error_from_response(response).await)
        }
    }
}

fn encode_json<B: Serialize + ?Sized>(body: &B) -> Result<Vec<u8>> {
    // Request types are plain data, but paths (`PathBuf`) only serialize when
    // they are valid UTF-8.
    serde_json::to_vec(body).map_err(ClientError::Encode)
}

fn hyper_method(method: Method) -> hyper::Method {
    match method {
        Method::Get => hyper::Method::GET,
        Method::Post => hyper::Method::POST,
        Method::Put => hyper::Method::PUT,
        Method::Delete => hyper::Method::DELETE,
    }
}

/// Turns a non-success response into an error, decoding the [`ApiError`]
/// body when there is one.
async fn error_from_response(response: Response<Incoming>) -> ClientError {
    let status = response.status().as_u16();
    let bytes = match response.into_body().collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(err) => return ClientError::Http(err),
    };
    match serde_json::from_slice::<ApiError>(&bytes) {
        Ok(error) => ClientError::Api { status, error },
        Err(_) => {
            let mut message = String::from_utf8_lossy(&bytes).trim().to_owned();
            if message.len() > MAX_RAW_ERROR_LEN {
                let mut cut = MAX_RAW_ERROR_LEN;
                while !message.is_char_boundary(cut) {
                    cut -= 1;
                }
                message.truncate(cut);
                message.push_str("...");
            }
            if message.is_empty() {
                message = "empty error body".to_owned();
            }
            ClientError::UnexpectedResponse { status, message }
        }
    }
}

/// A streamed response body (job logs).
#[derive(Debug)]
pub struct ByteStream {
    body: Incoming,
}

impl ByteStream {
    /// The next chunk of data, or `None` at the end of the stream.
    pub async fn next_chunk(&mut self) -> Result<Option<Bytes>> {
        while let Some(frame) = self.body.frame().await {
            if let Ok(data) = frame?.into_data()
                && !data.is_empty()
            {
                return Ok(Some(data));
            }
        }
        Ok(None)
    }

    /// Reads the rest of the stream into memory.
    pub async fn collect_all(mut self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        while let Some(chunk) = self.next_chunk().await? {
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }
}

/// The decoded `GET /v1/events` stream.
#[derive(Debug)]
pub struct EventStream {
    body: ByteStream,
    decoder: SseDecoder,
    pending: VecDeque<Event>,
}

impl EventStream {
    /// The next event, or `None` when the daemon closed the stream. Events of
    /// types this client does not know are skipped.
    pub async fn next_event(&mut self) -> Result<Option<Event>> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }
            let Some(chunk) = self.body.next_chunk().await? else {
                return Ok(None);
            };
            self.pending.extend(
                self.decoder
                    .push(&chunk)
                    .iter()
                    .filter_map(|message| message.parse_event().ok()),
            );
        }
    }
}
