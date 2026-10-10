//! Runs the `puzuz` binary against a fake daemon on a temporary Unix socket.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use puzuz_core::model::{Group, JobId, JobResult, JobState, Uid};
use puzuz_protocol::api::*;
use tokio::net::UnixListener;

type Body = http_body_util::combinators::BoxBody<Bytes, Infallible>;

#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    uri: String,
    body: String,
}

fn json(status: StatusCode, value: impl serde::Serialize) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", CONTENT_TYPE_JSON)
        .body(Full::new(Bytes::from(serde_json::to_vec(&value).unwrap())).boxed())
        .unwrap()
}

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

/// A response that sends `chunks`, each after `delay`.
fn delayed(content_type: &str, delay: Duration, chunks: Vec<String>) -> Response<Body> {
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tokio::spawn(async move {
        for chunk in chunks {
            tokio::time::sleep(delay).await;
            if tx.send(Bytes::from(chunk)).await.is_err() {
                return;
            }
        }
    });
    Response::builder()
        .header("content-type", content_type)
        .body(ChannelBody(rx).boxed())
        .unwrap()
}

/// A job as `GET /v1/jobs` lists it: without its environment.
fn job(id: u64, state: JobState) -> JobView {
    JobView {
        id: JobId(id),
        owner: Uid(4242),
        group: "default".into(),
        gpus: vec![],
        dependencies: vec![],
        priority: 0,
        label: None,
        no_checkpoint: false,
        state,
        created_at: 0,
        started_at: Some(1_000),
        finished_at: None,
        checkpoint_size_bytes: None,
        details: Some(JobDetails {
            command: "python train.py".into(),
            cwd: PathBuf::from("/work"),
            envs: None,
            last_error: None,
        }),
    }
}

fn jobs() -> Vec<JobView> {
    vec![
        job(1, JobState::Running),
        job(
            2,
            JobState::Done {
                result: JobResult::Success,
            },
        ),
    ]
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    VersionMismatch,
    /// Like `Normal`, with checkpoint size warnings on every response.
    SizeWarnings,
}

async fn route(
    req: Request<Incoming>,
    mode: Mode,
    log: Arc<Mutex<Vec<Recorded>>>,
) -> Response<Body> {
    let method = req.method().to_string();
    let uri = req.uri().to_string();
    let body = req.into_body().collect().await.unwrap().to_bytes();
    log.lock().unwrap().push(Recorded {
        method: method.clone(),
        uri: uri.clone(),
        body: String::from_utf8_lossy(&body).into_owned(),
    });

    if mode == Mode::VersionMismatch {
        if uri == "/v1/version" {
            return json(
                StatusCode::OK,
                VersionResponse {
                    api_version: 1,
                    daemon_version: "9.9.9".into(),
                    helper: HelperStatus {
                        connected: true,
                        version: None,
                    },
                    checkpoints: None,
                },
            );
        }
        return json(
            StatusCode::BAD_REQUEST,
            ApiError::new(ErrorCode::VersionMismatch, "version mismatch"),
        );
    }

    match (method.as_str(), uri.as_str()) {
        ("GET", "/v1/groups") => json(
            StatusCode::OK,
            ListGroupsResponse {
                groups: vec![Group {
                    name: "default".into(),
                    parallel: 1,
                    paused: false,
                }],
            },
        ),
        ("GET", "/v1/jobs") => json(StatusCode::OK, ListJobsResponse { jobs: jobs() }),
        ("GET", "/v1/jobs/2") => {
            // A single job comes with its environment.
            let mut job = jobs().remove(1);
            job.details.as_mut().unwrap().envs = Some(BTreeMap::from([(
                "SECRET".to_owned(),
                "hunter2".to_owned(),
            )]));
            json(StatusCode::OK, job)
        }
        ("POST", "/v1/jobs") => json(StatusCode::CREATED, AddJobResponse::new(JobId(5))),
        ("POST", "/v1/jobs/bulk") => json(
            StatusCode::OK,
            BulkJobResponse {
                results: vec![
                    JobActionResult::ok(JobId(1)),
                    JobActionResult::error(
                        JobId(2),
                        ApiError::new(ErrorCode::InvalidState, "job 2 is not running"),
                    ),
                ],
            },
        ),
        ("GET", "/v1/jobs/2/log?tail=16") => Response::builder()
            .header("content-type", CONTENT_TYPE_LOG)
            .body(Full::new(Bytes::from_static(b"epoch 1\nepoch 2\n\xff raw\n")).boxed())
            .unwrap(),
        ("GET", "/v1/events") => {
            let mut done = jobs().remove(0);
            done.state = JobState::Done {
                result: JobResult::Success,
            };
            let events = vec![
                ": keep-alive\n\n".to_owned(),
                Event::JobUpdated { job: done }.to_sse(),
            ];
            delayed(CONTENT_TYPE_EVENTS, Duration::from_millis(100), events)
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
    /// Runs the server; dropped (and the server stopped) with the fake.
    _runtime: tokio::runtime::Runtime,
}

impl FakeDaemon {
    fn start(mode: Mode) -> Self {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("puzuz.sock");
        let log = Arc::new(Mutex::new(Vec::new()));
        let listener = {
            let _guard = runtime.enter();
            UnixListener::bind(&socket).unwrap()
        };
        let server_log = log.clone();
        runtime.spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let log = server_log.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req| {
                        let log = log.clone();
                        async move {
                            let mut response = route(req, mode, log).await;
                            if mode == Mode::SizeWarnings {
                                let warnings = Warning::encode_header(&[
                                    Warning::CheckpointOverLimit {
                                        job: JobId(7),
                                        total_bytes: 3 << 30,
                                        limit_bytes: 2 << 30,
                                    },
                                    Warning::CheckpointUsageOverLimit {
                                        total_bytes: 3 << 30,
                                        limit_bytes: 2 << 30,
                                    },
                                ]);
                                response
                                    .headers_mut()
                                    .insert(WARNINGS_HEADER, warnings.parse().unwrap());
                            }
                            Ok::<_, Infallible>(response)
                        }
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
            _runtime: runtime,
        }
    }

    fn requests(&self) -> Vec<Recorded> {
        self.log.lock().unwrap().clone()
    }
}

fn puzuz(socket: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_puzuz"))
        .arg("--socket")
        .arg(socket)
        .arg("--color")
        .arg("never")
        .args(args)
        .env("PUZUZ_TEST_MARKER", "present")
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn status_prints_a_table() {
    let daemon = FakeDaemon::start(Mode::Normal);
    let output = puzuz(&daemon.socket, &["status"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.starts_with("Group \"default\" (1 parallel): running\n"),
        "{text}"
    );
    assert!(text.contains(" Owner "), "{text}");
    assert!(text.contains("python train.py"), "{text}");
    assert!(text.contains("Running"), "{text}");
    assert!(text.contains("Success"), "{text}");
}

#[test]
fn checkpoint_size_warnings_go_to_stderr() {
    let daemon = FakeDaemon::start(Mode::SizeWarnings);
    let output = puzuz(&daemon.socket, &["group"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("default"), "{}", stdout(&output));
    assert!(!stdout(&output).contains("warning"));
    let text = stderr(&output);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert!(
        lines[0].starts_with("puzuz: warning: the checkpoint of job 7 brought checkpoint images to 3.0 GiB, over the limit of 2.0 GiB"),
        "{text}"
    );
    assert!(lines[0].contains("`puzuz remove 7`"), "{text}");
    assert!(
        lines[1].starts_with("puzuz: warning: checkpoint images take 3.0 GiB, over checkpoint_size_limit_bytes (2.0 GiB)"),
        "{text}"
    );
    assert!(lines[1].contains("`puzuz status`"), "{text}");
}

#[test]
fn status_query_and_json() {
    let daemon = FakeDaemon::start(Mode::Normal);
    let output = puzuz(&daemon.socket, &["status", "--json", "status=running"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["groups"][0]["name"], "default");
    let jobs = value["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0]["id"], 1);
    assert_eq!(jobs[0]["state"], serde_json::json!({"state": "running"}));

    // Finished jobs carry their result flat inside `state`.
    let output = puzuz(&daemon.socket, &["status", "--json", "status=success"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        value["jobs"][0]["state"],
        serde_json::json!({"state": "done", "result": "success"})
    );
    assert_eq!(
        value["jobs"][0]["details"],
        serde_json::json!({"command": "python train.py", "cwd": "/work"})
    );

    let output = puzuz(&daemon.socket, &["status", "status=bogus"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("unknown status `bogus`"));
}

#[test]
fn partial_failure_reports_each_job_and_exits_non_zero() {
    let daemon = FakeDaemon::start(Mode::Normal);
    let output = puzuz(&daemon.socket, &["kill", "-s", "INT", "1", "2"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "Sending signal SIGINT to jobs: 1\n");
    assert_eq!(stderr(&output), "Job 2: job 2 is not running\n");
    let request = daemon.requests().pop().unwrap();
    assert_eq!(request.uri, "/v1/jobs/bulk");
    assert_eq!(
        request.body,
        r#"{"target":{"kind":"ids","ids":[1,2]},"action":"kill","signal":2}"#
    );
}

#[test]
fn group_selection_uses_the_bulk_endpoint() {
    let daemon = FakeDaemon::start(Mode::Normal);
    let output = puzuz(&daemon.socket, &["pause", "-g", "gpu"]);
    // The fake answers with one failure.
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "Group \"gpu\" is being paused.\n");
    let request = daemon.requests().pop().unwrap();
    assert_eq!(
        request.body,
        r#"{"target":{"kind":"group","group":"gpu"},"action":"pause","wait":false}"#
    );
}

#[test]
fn add_sends_cwd_and_environment() {
    let daemon = FakeDaemon::start(Mode::Normal);
    let output = puzuz(
        &daemon.socket,
        &["add", "--gpu", "0,1", "-w", "/", "--", "echo", "hi"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "New job added (id 5).\n");
    let add = daemon
        .requests()
        .into_iter()
        .find(|r| r.method == "POST" && r.uri == "/v1/jobs")
        .unwrap();
    let body: serde_json::Value = serde_json::from_str(&add.body).unwrap();
    assert_eq!(body["command"], "echo hi");
    assert_eq!(body["cwd"], "/");
    assert_eq!(body["gpus"], serde_json::json!([0, 1]));
    assert_eq!(body["envs"]["PUZUZ_TEST_MARKER"], "present");

    let output = puzuz(&daemon.socket, &["add", "-p", "true"]);
    assert_eq!(stdout(&output), "5\n");
}

#[test]
fn log_writes_raw_bytes_to_stdout() {
    let daemon = FakeDaemon::start(Mode::Normal);
    let output = puzuz(&daemon.socket, &["log", "2"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(output.stdout, b"epoch 1\nepoch 2\n\xff raw\n");
    let header = stderr(&output);
    assert!(header.contains("Job 2:"), "{header}");
    assert!(header.contains("completed successfully"), "{header}");
    assert!(header.contains("output:"), "{header}");
    assert!(!header.contains("last 15 lines"), "{header}");
}

#[test]
fn log_json_has_a_flat_state_and_no_environment() {
    let daemon = FakeDaemon::start(Mode::Normal);
    let output = puzuz(&daemon.socket, &["log", "--json", "2"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let job = &value["2"]["job"];
    assert_eq!(
        job["state"],
        serde_json::json!({"state": "done", "result": "success"})
    );
    // Fetched with `GET /v1/jobs/2`, which includes the environment, but
    // `log --json` leaves it out like for listed jobs.
    assert_eq!(
        job["details"],
        serde_json::json!({"command": "python train.py", "cwd": "/work"})
    );
    assert_eq!(value["2"]["output"], "epoch 1\nepoch 2\n\u{fffd} raw\n");
}

#[test]
fn wait_returns_when_the_job_finishes() {
    let daemon = FakeDaemon::start(Mode::Normal);
    let output = puzuz(&daemon.socket, &["wait", "1"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("Found active job 1 with status Running"),
        "{text}"
    );
    assert!(text.contains("Job 1 succeeded with 0"), "{text}");
}

#[test]
fn missing_daemon_gets_a_friendly_error() {
    let dir = tempfile::tempdir().unwrap();
    let output = puzuz(&dir.path().join("missing.sock"), &["status"]);
    assert_eq!(output.status.code(), Some(1));
    let text = stderr(&output);
    assert!(text.contains("the daemon is not running"), "{text}");
    assert!(text.contains("systemctl status puzuz"), "{text}");
}

#[test]
fn version_mismatch_explains_how_to_fix_it() {
    let daemon = FakeDaemon::start(Mode::VersionMismatch);
    let output = puzuz(&daemon.socket, &["status"]);
    assert_eq!(output.status.code(), Some(1));
    let text = stderr(&output);
    assert!(text.contains("daemon 9.9.9"), "{text}");
    assert!(text.contains("sudo systemctl restart puzuz"), "{text}");
}
