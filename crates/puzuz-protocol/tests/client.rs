//! Runs the HTTP client against a small fake daemon on a temporary Unix
//! socket.

use std::convert::Infallible;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use puzuz_core::model::{GpuIndex, Group, JobId};
use puzuz_protocol::api::*;
use puzuz_protocol::client::{Client, ClientError};
use tokio::net::UnixListener;

#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    uri: String,
    version: Option<String>,
    body: String,
}

type Body = http_body_util::combinators::BoxBody<Bytes, Infallible>;

fn full(status: StatusCode, content_type: &str, body: impl Into<Bytes>) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", content_type)
        .body(Full::new(body.into()).boxed())
        .unwrap()
}

fn json(status: StatusCode, value: impl serde::Serialize) -> Response<Body> {
    full(
        status,
        CONTENT_TYPE_JSON,
        serde_json::to_vec(&value).unwrap(),
    )
}

/// A body that yields `chunks` with a short pause in between, to exercise
/// streaming.
fn chunked(content_type: &str, chunks: Vec<String>) -> Response<Body> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(4);
    tokio::spawn(async move {
        for chunk in chunks {
            if tx.send(Bytes::from(chunk)).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    Response::builder()
        .header("content-type", content_type)
        .body(ChannelBody(rx).boxed())
        .unwrap()
}

/// A streaming body fed by a channel.
struct ChannelBody(tokio::sync::mpsc::Receiver<Bytes>);

impl hyper::body::Body for ChannelBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        self.0
            .poll_recv(cx)
            .map(|chunk| chunk.map(|bytes| Ok(Frame::data(bytes))))
    }
}

fn group(name: &str, parallel: u32) -> Group {
    Group {
        name: name.into(),
        parallel,
        paused: false,
    }
}

/// The fake daemon's routing table.
async fn route(req: Request<Incoming>, log: Arc<Mutex<Vec<Recorded>>>) -> Response<Body> {
    let method = req.method().to_string();
    let uri = req.uri().to_string();
    let version = req
        .headers()
        .get(VERSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = req.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8_lossy(&body).into_owned();
    log.lock().unwrap().push(Recorded {
        method: method.clone(),
        uri: uri.clone(),
        version: version.clone(),
        body: body.clone(),
    });

    if version.as_deref() == Some("0.0.0-old") && uri != "/v1/version" {
        return json(
            StatusCode::BAD_REQUEST,
            ApiError::new(
                ErrorCode::VersionMismatch,
                "client 0.0.0-old != daemon 9.9.9",
            ),
        );
    }

    match (method.as_str(), uri.as_str()) {
        ("GET", "/v1/version") => json(
            StatusCode::OK,
            VersionResponse {
                api_version: 1,
                daemon_version: "9.9.9".into(),
                helper: HelperStatus {
                    connected: true,
                    version: None,
                },
                checkpoints: None,
                gpu_detection: None,
                schema_version: None,
                orphan_jobs: None,
            },
        ),
        ("GET", "/v1/groups") => json(
            StatusCode::OK,
            ListGroupsResponse {
                groups: vec![group("default", 1)],
            },
        ),
        ("POST", "/v1/groups") => json(StatusCode::CREATED, group("gpu", 2)),
        ("DELETE", "/v1/groups/a%20b") => full(StatusCode::NO_CONTENT, CONTENT_TYPE_JSON, ""),
        ("PUT", "/v1/groups/default/parallel") => json(StatusCode::OK, group("default", 4)),
        ("POST", "/v1/jobs") => json(StatusCode::CREATED, AddJobResponse::new(JobId(5))),
        ("GET", "/v1/jobs/404") => json(
            StatusCode::NOT_FOUND,
            ApiError::new(ErrorCode::JobNotFound, "job 404 does not exist"),
        ),
        ("POST", "/v1/jobs/bulk") => json(
            StatusCode::OK,
            BulkJobResponse::new(vec![
                JobActionResult::ok(JobId(1)),
                JobActionResult::error(
                    JobId(2),
                    ApiError::new(ErrorCode::InvalidState, "job 2 is not running"),
                ),
            ]),
        ),
        ("DELETE", "/v1/jobs/3") => json(StatusCode::OK, JobActionResult::ok(JobId(3))),
        ("POST", "/v1/jobs/3/kill") => json(StatusCode::OK, JobActionResult::ok(JobId(3))),
        ("POST", "/v1/jobs/switch") => full(StatusCode::NO_CONTENT, CONTENT_TYPE_JSON, ""),
        ("POST", "/v1/daemon/shutdown") => full(StatusCode::ACCEPTED, CONTENT_TYPE_JSON, ""),
        ("PUT", "/v1/gpus/parallel") => json(StatusCode::OK, ListGpusResponse { gpus: vec![] }),
        ("GET", "/v1/gpus") => {
            let mut response = json(StatusCode::OK, ListGpusResponse { gpus: vec![] });
            response.headers_mut().insert(
                WARNINGS_HEADER,
                "checkpoint-failed=4, from-the-future=1".parse().unwrap(),
            );
            response
        }
        ("GET", "/v1/jobs/1/log?tail=2&follow=true") => chunked(
            CONTENT_TYPE_LOG,
            vec!["line 1\n".into(), "line 2\n".into(), "line 3\n".into()],
        ),
        ("GET", "/v1/events") => {
            let events = [
                Event::JobRemoved { id: JobId(1) },
                Event::GroupRemoved { name: "x".into() },
                Event::DaemonShutdown,
            ];
            let mut wire: String = events.iter().map(Event::to_sse).collect();
            // An unknown event type and a keep-alive comment are skipped.
            wire.insert_str(
                0,
                ": ping\n\nevent: future\ndata: {\"type\":\"future\"}\n\n",
            );
            // Split in awkward places.
            let (a, b) = wire.split_at(wire.len() / 3);
            let (b, c) = b.split_at(b.len() / 2);
            chunked(CONTENT_TYPE_EVENTS, vec![a.into(), b.into(), c.into()])
        }
        ("DELETE", "/v1/groups/teapot") => {
            full(StatusCode::IM_A_TEAPOT, "text/plain", "short and stout")
        }
        _ => json(
            StatusCode::NOT_FOUND,
            ApiError::new(ErrorCode::BadRequest, format!("no route {method} {uri}")),
        ),
    }
}

struct FakeDaemon {
    _dir: tempfile::TempDir,
    socket: PathBuf,
    log: Arc<Mutex<Vec<Recorded>>>,
}

impl FakeDaemon {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("puzuz.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let server_log = log.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let log = server_log.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req| {
                        let log = log.clone();
                        async move { Ok::<_, Infallible>(route(req, log).await) }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        Self {
            _dir: dir,
            socket,
            log,
        }
    }

    fn client(&self) -> Client {
        Client::new(&self.socket)
    }

    fn last(&self) -> Recorded {
        self.log.lock().unwrap().last().cloned().unwrap()
    }
}

#[tokio::test]
async fn sends_version_header_and_decodes_json() {
    let daemon = FakeDaemon::start();
    let client = daemon.client();

    let groups = client.list_groups().await.unwrap();
    assert_eq!(groups.groups, vec![group("default", 1)]);
    let recorded = daemon.last();
    assert_eq!(recorded.method, "GET");
    assert_eq!(recorded.uri, "/v1/groups");
    assert_eq!(
        recorded.version.as_deref(),
        Some(puzuz_protocol::BINARY_VERSION)
    );
    assert_eq!(recorded.body, "");
}

#[tokio::test]
async fn collects_warnings_from_responses() {
    let daemon = FakeDaemon::start();
    let client = daemon.client();
    assert!(client.take_warnings().is_empty());

    client.list_gpus().await.unwrap();
    // Clones share the collected warnings; repeats are kept once.
    client.clone().list_gpus().await.unwrap();
    client.list_groups().await.unwrap();
    assert_eq!(
        client.take_warnings(),
        [Warning::CheckpointFailed { job: JobId(4) }]
    );
    assert!(client.take_warnings().is_empty());
}

#[tokio::test]
async fn typed_methods_use_the_right_routes_and_bodies() {
    let daemon = FakeDaemon::start();
    let client = daemon.client();

    let created = client
        .create_group(&CreateGroupRequest {
            name: "gpu".into(),
            parallel: Some(2),
        })
        .await
        .unwrap();
    assert_eq!(created, group("gpu", 2));
    assert_eq!(daemon.last().body, r#"{"name":"gpu","parallel":2}"#);

    client.delete_group("a b").await.unwrap();
    assert_eq!(daemon.last().uri, "/v1/groups/a%20b");

    let updated = client.set_group_parallel("default", 4).await.unwrap();
    assert_eq!(updated.parallel, 4);
    assert_eq!(daemon.last().method, "PUT");
    assert_eq!(daemon.last().body, r#"{"parallel":4}"#);

    let result = client
        .job_action(JobId(3), &JobAction::Remove)
        .await
        .unwrap();
    assert!(result.is_ok());
    assert_eq!(daemon.last().method, "DELETE");

    client
        .job_action(JobId(3), &JobAction::Kill(KillOptions { signal: Some(9) }))
        .await
        .unwrap();
    assert_eq!(daemon.last().uri, "/v1/jobs/3/kill");
    assert_eq!(daemon.last().body, r#"{"signal":9}"#);

    let bulk = client
        .bulk(&BulkJobRequest {
            target: JobSelector::Ids {
                ids: vec![JobId(1), JobId(2)],
            },
            action: JobAction::Checkpoint,
        })
        .await
        .unwrap();
    assert!(!bulk.all_ok());
    assert_eq!(
        daemon.last().body,
        r#"{"target":{"kind":"ids","ids":[1,2]},"action":"checkpoint"}"#
    );

    client
        .switch(&SwitchRequest {
            first: JobId(1),
            second: JobId(2),
        })
        .await
        .unwrap();
    client
        .shutdown(&ShutdownRequest {
            no_checkpoint: true,
        })
        .await
        .unwrap();
    assert_eq!(daemon.last().uri, "/v1/daemon/shutdown");

    client
        .set_gpus_parallel(&SetGpusParallelRequest {
            gpus: Some(vec![GpuIndex(0)]),
            parallel: 2,
        })
        .await
        .unwrap();
    assert_eq!(daemon.last().body, r#"{"gpus":[0],"parallel":2}"#);

    let version = client.daemon_version().await.unwrap();
    assert_eq!(version.daemon_version, "9.9.9");
}

#[tokio::test]
async fn maps_error_bodies_to_api_errors() {
    let daemon = FakeDaemon::start();
    let err = daemon.client().get_job(JobId(404)).await.unwrap_err();
    match &err {
        ClientError::Api { status, error } => {
            assert_eq!(*status, 404);
            assert_eq!(error.code, ErrorCode::JobNotFound);
        }
        other => panic!("unexpected error {other:?}"),
    }
    assert_eq!(err.code(), Some(ErrorCode::JobNotFound));
    assert_eq!(err.to_string(), "job 404 does not exist");
}

#[tokio::test]
async fn reports_version_mismatch() {
    let daemon = FakeDaemon::start();
    let client = daemon.client().with_version("0.0.0-old");
    let err = client.list_groups().await.unwrap_err();
    assert_eq!(err.code(), Some(ErrorCode::VersionMismatch));
    // The version endpoint still answers.
    assert_eq!(
        client.daemon_version().await.unwrap().daemon_version,
        "9.9.9"
    );
}

#[tokio::test]
async fn non_json_errors_are_unexpected_responses() {
    let daemon = FakeDaemon::start();
    let err = daemon.client().delete_group("teapot").await.unwrap_err();
    match err {
        ClientError::UnexpectedResponse { status, message } => {
            assert_eq!(status, 418);
            assert_eq!(message, "short and stout");
        }
        other => panic!("unexpected error {other:?}"),
    }
}

#[tokio::test]
async fn missing_socket_is_reported_as_daemon_down() {
    let dir = tempfile::tempdir().unwrap();
    let client = Client::new(dir.path().join("nope.sock"));
    let err = client.list_groups().await.unwrap_err();
    assert!(err.is_daemon_down(), "{err:?}");
    assert!(err.to_string().contains("nope.sock"));
}

#[tokio::test]
async fn refused_socket_is_reported_as_daemon_down() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stale.sock");
    // Bind and drop: the socket file stays but nobody listens.
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    let err = Client::new(&path).list_groups().await.unwrap_err();
    assert!(err.is_daemon_down(), "{err:?}");
}

#[tokio::test]
async fn streams_log_bytes() {
    let daemon = FakeDaemon::start();
    let mut stream = daemon
        .client()
        .job_log(
            JobId(1),
            &LogQuery {
                tail: Some(2),
                follow: true,
            },
        )
        .await
        .unwrap();
    let first = stream.next_chunk().await.unwrap().unwrap();
    assert_eq!(&first[..], b"line 1\n");
    let rest = stream.collect_all().await.unwrap();
    assert_eq!(rest, b"line 2\nline 3\n");
}

#[tokio::test]
async fn decodes_event_stream() {
    let daemon = FakeDaemon::start();
    let mut events = daemon.client().events().await.unwrap();
    let mut received = Vec::new();
    while let Some(event) = events.next_event().await.unwrap() {
        received.push(event);
    }
    assert_eq!(
        received,
        vec![
            Event::JobRemoved { id: JobId(1) },
            Event::GroupRemoved { name: "x".into() },
            Event::DaemonShutdown,
        ]
    );
}

#[tokio::test]
async fn add_job_posts_flattened_spec() {
    let daemon = FakeDaemon::start();
    let request = AddJobRequest {
        spec: puzuz_core::model::JobSpec {
            command: "echo hi".into(),
            cwd: "/tmp".into(),
            envs: Default::default(),
            group: "default".into(),
            gpus: vec![GpuIndex(1)],
            dependencies: vec![],
            priority: 0,
            label: None,
            no_checkpoint: false,
        },
        stashed: false,
        enqueue_at: None,
        immediate: true,
    };
    let response = daemon.client().add_job(&request).await.unwrap();
    assert_eq!(response.id, JobId(5));
    let recorded = daemon.last();
    assert_eq!(recorded.method, "POST");
    let body: serde_json::Value = serde_json::from_str(&recorded.body).unwrap();
    assert_eq!(body["command"], "echo hi");
    assert_eq!(body["gpus"], serde_json::json!([1]));
    assert_eq!(body["immediate"], true);
}

#[tokio::test]
async fn unencodable_request_is_an_error_not_a_panic() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    // Nothing listens on the socket: encoding must fail before connecting.
    let dir = tempfile::tempdir().unwrap();
    let client = Client::new(dir.path().join("nope.sock"));
    let request = AddJobRequest {
        spec: puzuz_core::model::JobSpec {
            command: "true".into(),
            cwd: PathBuf::from(OsStr::from_bytes(b"/tmp/\xff")),
            envs: Default::default(),
            group: "default".into(),
            gpus: vec![],
            dependencies: vec![],
            priority: 0,
            label: None,
            no_checkpoint: false,
        },
        stashed: false,
        enqueue_at: None,
        immediate: false,
    };
    let err = client.add_job(&request).await.unwrap_err();
    assert!(matches!(err, ClientError::Encode(_)), "{err:?}");
}
