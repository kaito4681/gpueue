//! End-to-end tests: the daemon runs in-process on a temporary socket and
//! database, with the in-memory `FakeHelper`, and is driven over HTTP on the
//! Unix socket with a minimal hyper client.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use puzuz_core::model::{GpuIndex, JobId, JobResult, JobSpec, JobState, Uid};
use puzuz_daemon::api::TEST_UID_HEADER;
use puzuz_daemon::helper::FakeOutcome;
use puzuz_daemon::{
    Config, Daemon, DaemonOptions, DetectedGpu, FakeHelper, FallbackGpuDetector, GpuDetector,
    LaunchCheckHook, StaticGpuDetector, StaticUserDb,
};
use puzuz_protocol::BINARY_VERSION;
use puzuz_protocol::api::{
    AddJobRequest, AddJobResponse, ApiError, BulkJobResponse, CleanResponse, ErrorCode,
    GpuDetection, GpuSource, JobView, ListGpusResponse, ListJobsResponse, VERSION_HEADER,
    VersionResponse, WARNINGS_HEADER, Warning,
};
use puzuz_protocol::helper::{RunningPhase, signal};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

const ALICE: u32 = 1000;
const BOB: u32 = 1001;
const ADMIN: u32 = 500;

// ---------------------------------------------------------------------------
// Test client
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Client {
    socket: PathBuf,
    uid: u32,
    version: String,
}

struct Reply {
    status: StatusCode,
    headers: hyper::HeaderMap,
    body: Bytes,
}

impl Reply {
    fn json<T: DeserializeOwned>(&self) -> T {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| {
            panic!(
                "bad JSON ({e}) in {} reply: {}",
                self.status,
                String::from_utf8_lossy(&self.body)
            )
        })
    }

    /// The warnings the daemon attached for the caller.
    fn warnings(&self) -> Vec<Warning> {
        self.headers
            .get(WARNINGS_HEADER)
            .map(|v| Warning::parse_header(v.to_str().unwrap()))
            .unwrap_or_default()
    }

    fn error(&self) -> ApiError {
        assert!(
            !self.status.is_success(),
            "expected an error, got {}",
            self.status
        );
        self.json()
    }
}

impl Client {
    fn new(socket: &Path, uid: u32) -> Self {
        Self {
            socket: socket.to_owned(),
            uid,
            version: BINARY_VERSION.to_owned(),
        }
    }

    async fn open(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> hyper::Response<hyper::body::Incoming> {
        let stream = tokio::net::UnixStream::connect(&self.socket).await.unwrap();
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        tokio::spawn(conn);
        let body = body.map(|v| v.to_string()).unwrap_or_default();
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost")
            .header("content-type", "application/json")
            .header(VERSION_HEADER, &self.version)
            .header(TEST_UID_HEADER, self.uid.to_string())
            .body(Full::new(Bytes::from(body)))
            .unwrap();
        sender.send_request(request).await.unwrap()
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Reply {
        let response = self.open(method, path, body).await;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            body,
        }
    }

    async fn get(&self, path: &str) -> Reply {
        self.call(Method::GET, path, None).await
    }

    async fn post(&self, path: &str, body: Value) -> Reply {
        self.call(Method::POST, path, Some(body)).await
    }

    async fn add(&self, request: &AddJobRequest) -> JobId {
        let reply = self
            .post("/v1/jobs", serde_json::to_value(request).unwrap())
            .await;
        assert_eq!(
            reply.status,
            StatusCode::CREATED,
            "{}",
            String::from_utf8_lossy(&reply.body)
        );
        reply.json::<AddJobResponse>().id
    }

    async fn job(&self, id: JobId) -> JobView {
        let reply = self.get(&format!("/v1/jobs/{id}")).await;
        assert_eq!(reply.status, StatusCode::OK);
        reply.json()
    }

    async fn action(&self, id: JobId, action: &str, body: Value) -> Reply {
        self.post(&format!("/v1/jobs/{id}/{action}"), body).await
    }

    /// Polls until `pred` holds for the job (5 s at most).
    async fn wait_for(&self, id: JobId, what: &str, pred: impl Fn(&JobView) -> bool) -> JobView {
        for _ in 0..250 {
            let job = self.job(id).await;
            if pred(&job) {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!(
            "job {id} never became {what}: {:?}",
            self.job(id).await.state
        );
    }

    async fn wait_state(&self, id: JobId, state: &JobState) -> JobView {
        self.wait_for(id, state.name(), |j| j.state == *state).await
    }
}

fn job_request(command: &str, gpus: &[u32]) -> AddJobRequest {
    AddJobRequest {
        spec: JobSpec {
            command: command.to_owned(),
            cwd: PathBuf::from("/tmp"),
            envs: BTreeMap::from([("SECRET".to_owned(), "hunter2".to_owned())]),
            group: "default".to_owned(),
            gpus: gpus.iter().copied().map(GpuIndex).collect(),
            dependencies: Vec::new(),
            priority: 0,
            label: Some("experiment".to_owned()),
            no_checkpoint: false,
            nice: 0,
        },
        stashed: false,
        enqueue_at: None,
        immediate: false,
    }
}

fn done(result: JobResult) -> JobState {
    JobState::Done { result }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Harness {
    dir: tempfile::TempDir,
    helper: FakeHelper,
    daemon: Option<Daemon>,
    /// `checkpoint_size_limit_bytes` and
    /// `checkpoint_size_warning_interval_secs` of the next start.
    checkpoint_limit: Option<u64>,
    warning_interval_secs: u64,
    /// The `[priority]` table of the next start.
    priority: puzuz_daemon::config::PriorityConfig,
}

impl Harness {
    async fn new(gpus: u32) -> Self {
        Self::with_gpus(StaticGpuDetector::anonymous(gpus)).await
    }

    async fn with_gpus(detector: impl GpuDetector) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let helper = FakeHelper::new();
        let mut harness = Self {
            dir,
            helper,
            daemon: None,
            checkpoint_limit: None,
            warning_interval_secs: Config::default().checkpoint_size_warning_interval_secs,
            priority: Config::default().priority,
        };
        harness.start_with(detector).await;
        harness
    }

    fn config(&self) -> Config {
        Config {
            api_socket: self.dir.path().join("puzuz.sock"),
            grace_secs: 1,
            checkpoint_size_limit_bytes: self.checkpoint_limit,
            checkpoint_size_warning_interval_secs: self.warning_interval_secs,
            priority: self.priority.clone(),
            ..Config::default()
        }
        .with_data_dir(self.dir.path())
    }

    async fn start(&mut self, gpus: u32) {
        self.start_with(StaticGpuDetector::anonymous(gpus)).await;
    }

    async fn start_with(&mut self, detector: impl GpuDetector) {
        let daemon = Daemon::start(DaemonOptions {
            config: self.config(),
            helper: Arc::new(self.helper.clone()),
            users: Arc::new(StaticUserDb::with_admins([ADMIN])),
            gpus: Arc::new(detector),
            trust_uid_header: true,
        })
        .await
        .unwrap();
        self.daemon = Some(daemon);
    }

    async fn stop(&mut self) {
        if let Some(daemon) = self.daemon.take() {
            daemon.stop().await;
        }
    }

    fn client(&self, uid: u32) -> Client {
        Client::new(&self.dir.path().join("puzuz.sock"), uid)
    }

    /// Waits until the fake helper runs the job. A job is `Running` in the
    /// daemon before its spawn request has reached the helper; API actions
    /// wait for that by themselves, but simulated process events (exit,
    /// vanish) need the fake to know the job.
    async fn wait_spawned(&self, id: JobId) {
        for _ in 0..250 {
            if self.helper.is_running(id) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("job {id} never reached the helper");
    }

    /// The job's process exits with `code` (once it has been spawned).
    async fn exit(&self, id: JobId, code: i32) {
        self.wait_spawned(id).await;
        self.helper.exit(id, code);
    }

    async fn set_default_parallel(&self, parallel: u32) {
        let reply = self
            .client(ADMIN)
            .call(
                Method::PUT,
                "/v1/groups/default/parallel",
                Some(json!({ "parallel": parallel })),
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_start_exit_status_and_log() {
    let mut h = Harness::new(1).await;
    let alice = h.client(ALICE);

    let id = alice.add(&job_request("echo hi", &[0])).await;
    let job = alice.wait_state(id, &JobState::Running).await;
    assert!(job.started_at.is_some());
    h.wait_spawned(id).await;
    let spawned = h.helper.spawned();
    assert_eq!(spawned.len(), 1);
    assert_eq!(spawned[0].job, id);
    assert_eq!(spawned[0].uid.0, ALICE);
    assert_eq!(spawned[0].command, "echo hi");
    assert_eq!(spawned[0].shell, ["sh", "-c"]);
    assert_eq!(spawned[0].envs["SECRET"], "hunter2");
    assert_eq!(
        spawned[0].log_path,
        h.dir.path().join(format!("logs/{ALICE}/{id}.log"))
    );

    h.exit(id, 0).await;
    let job = alice.wait_state(id, &done(JobResult::Success)).await;
    assert!(job.finished_at.is_some());
    let details = job.details.expect("the owner sees details");
    assert_eq!(details.command, "echo hi");

    let failing = alice.add(&job_request("false", &[])).await;
    alice.wait_state(failing, &JobState::Running).await;
    h.exit(failing, 3).await;
    alice
        .wait_state(failing, &done(JobResult::Failed { exit_code: 3 }))
        .await;

    // The list shows both.
    let list: ListJobsResponse = alice.get("/v1/jobs").await.json();
    assert_eq!(list.jobs.len(), 2);
    // Without the environments, which only the single-job view carries.
    for job in &list.jobs {
        let details = job.details.as_ref().expect("the owner sees details");
        assert_eq!(details.envs, None, "job {}", job.id);
    }
    let raw = String::from_utf8(alice.get("/v1/jobs").await.body.to_vec()).unwrap();
    assert!(!raw.contains("envs") && !raw.contains("hunter2"), "{raw}");
    // Finished jobs carry their result flat inside `state`.
    assert!(
        raw.contains(r#""state":{"state":"done","result":"failed","exit_code":3}"#),
        "{raw}"
    );
    let secret = BTreeMap::from([("SECRET".to_owned(), "hunter2".to_owned())]);
    for caller in [ALICE, ADMIN] {
        let view = h.client(caller).job(id).await;
        assert_eq!(view.details.unwrap().envs.as_ref(), Some(&secret));
    }
    // The environments are listed on request, still only to the owner.
    let list: ListJobsResponse = alice.get("/v1/jobs?details=envs").await.json();
    assert!(
        list.jobs
            .iter()
            .all(|job| job.details.as_ref().unwrap().envs.as_ref() == Some(&secret))
    );
    let list = String::from_utf8(
        h.client(BOB)
            .get("/v1/jobs?details=envs")
            .await
            .body
            .to_vec(),
    )
    .unwrap();
    assert!(
        !list.contains("envs") && !list.contains("hunter2"),
        "{list}"
    );
    assert!(h.client(BOB).job(id).await.details.is_none());
    let reply = alice.get("/v1/jobs?details=bogus").await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);

    h.stop().await;
}

/// The uid running the tests: the daemon only reads logs owned by the
/// job's owner, so jobs whose logs the tests write belong to it.
fn me() -> u32 {
    let uid = nix::unistd::getuid().as_raw();
    assert!(uid != 0 && uid != ADMIN, "run the tests as a regular user");
    uid
}

/// Someone who is neither [`me`] nor an administrator.
fn not_me() -> u32 {
    if me() == ALICE { BOB } else { ALICE }
}

/// Finishes a job of [`me`] and writes its log; returns the job and the
/// log's path.
async fn finished_job_with_log(h: &Harness, content: &str) -> (JobId, PathBuf) {
    let owner = h.client(me());
    let id = owner.add(&job_request("echo", &[])).await;
    owner.wait_state(id, &JobState::Running).await;
    h.exit(id, 0).await;
    owner.wait_state(id, &done(JobResult::Success)).await;
    let log = h.dir.path().join(format!("logs/{}/{id}.log", me()));
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, content).unwrap();
    (id, log)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logs_are_raw_bytes_for_the_owner_only() {
    let mut h = Harness::new(0).await;
    let owner = h.client(me());
    let (id, _log) = finished_job_with_log(&h, "one\ntwo\nthree\n").await;
    let reply = owner.get(&format!("/v1/jobs/{id}/log")).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(&reply.body[..], b"one\ntwo\nthree\n");
    let reply = owner.get(&format!("/v1/jobs/{id}/log?tail=2")).await;
    assert_eq!(&reply.body[..], b"two\nthree\n");
    let reply = owner.get(&format!("/v1/jobs/{id}/log?follow=true")).await;
    assert_eq!(
        &reply.body[..],
        b"one\ntwo\nthree\n",
        "follow ends with the job"
    );
    let reply = h.client(not_me()).get(&format!("/v1/jobs/{id}/log")).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(reply.error().code, ErrorCode::PermissionDenied);
    let reply = h
        .client(ADMIN)
        .get(&format!("/v1/jobs/{id}/log?tail=1"))
        .await;
    assert_eq!(&reply.body[..], b"three\n");

    // `restart --in-place` empties the log before the job is back in line.
    let reply = owner
        .action(id, "restart", json!({"in_place": true, "stashed": true}))
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(owner.get(&format!("/v1/jobs/{id}/log")).await.body.len(), 0);

    h.stop().await;
}

/// The owner controls their log directory: a log replaced by a symlink
/// (to the daemon's database, another user's log, ...) is neither read nor
/// truncated, and a refused truncation leaves the job as it was.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logs_replaced_by_symlinks_are_neither_read_nor_truncated() {
    let mut h = Harness::new(0).await;
    let owner = h.client(me());
    let (id, log) = finished_job_with_log(&h, "mine\n").await;
    // Stands for the database or another user's log.
    let target = h.dir.path().join("secret");
    std::fs::write(&target, "secret\n").unwrap();
    std::fs::remove_file(&log).unwrap();
    std::os::unix::fs::symlink(&target, &log).unwrap();

    for query in ["", "?tail=1", "?follow=true"] {
        let reply = owner.get(&format!("/v1/jobs/{id}/log{query}")).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{query}");
        assert!(reply.error().message.contains("symbolic link"), "{query}");
    }
    let reply = owner.action(id, "restart", json!({"in_place": true})).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert!(reply.error().message.contains("symbolic link"));
    // Not requeued with a log that could not be reset.
    assert_eq!(owner.job(id).await.state, done(JobResult::Success));
    h.stop().await;
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "secret\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn log_follow_streams_appended_output() {
    let mut h = Harness::new(0).await;
    let alice = h.client(me());
    let id = alice.add(&job_request("train", &[])).await;
    alice.wait_state(id, &JobState::Running).await;
    let log = h.dir.path().join(format!("logs/{}/{id}.log", me()));
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, "start\n").unwrap();

    let response = alice
        .open(Method::GET, &format!("/v1/jobs/{id}/log?follow=true"), None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let mut received = Vec::new();
    let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
    received.extend_from_slice(&first);
    assert_eq!(received, b"start\n");

    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
    file.write_all(b"epoch 1\n").unwrap();
    let next = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert_eq!(&next[..], b"epoch 1\n");

    file.write_all(b"bye\n").unwrap();
    h.exit(id, 0).await;
    while let Some(frame) = body.frame().await {
        received.extend_from_slice(&frame.unwrap().into_data().unwrap());
    }
    assert!(
        received.ends_with(b"bye\n"),
        "{:?}",
        String::from_utf8_lossy(&received)
    );

    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gpu_slots_are_shared_across_users() {
    let mut h = Harness::new(2).await;
    h.set_default_parallel(4).await;
    let alice = h.client(ALICE);
    let bob = h.client(BOB);

    let a0 = alice.add(&job_request("a", &[0])).await;
    alice.wait_state(a0, &JobState::Running).await;
    let b0 = bob.add(&job_request("b", &[0])).await;
    let b1 = bob.add(&job_request("c", &[1])).await;
    bob.wait_state(b1, &JobState::Running).await;
    // GPU 0 has one slot, taken by alice.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(matches!(bob.job(b0).await.state, JobState::Queued { .. }));

    let gpus: ListGpusResponse = bob.get("/v1/gpus").await.json();
    assert_eq!(gpus.gpus.len(), 2);
    assert_eq!(gpus.gpus[0].used, 1);
    assert_eq!(gpus.gpus[0].jobs[0].id, a0);
    assert_eq!(gpus.gpus[0].jobs[0].owner.0, ALICE);
    assert_eq!(gpus.gpus[1].jobs[0].owner.0, BOB);

    // A job on both GPUs waits for both.
    let both = alice.add(&job_request("d", &[0, 1])).await;

    h.exit(a0, 0).await;
    bob.wait_state(b0, &JobState::Running).await;
    assert!(matches!(
        alice.job(both).await.state,
        JobState::Queued { .. }
    ));

    // Raising the GPU limit lets it start.
    let reply = bob
        .call(
            Method::PUT,
            "/v1/gpus/parallel",
            Some(json!({ "parallel": 2 })),
        )
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    let reply = h
        .client(ADMIN)
        .call(
            Method::PUT,
            "/v1/gpus/parallel",
            Some(json!({ "parallel": 2 })),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    alice.wait_state(both, &JobState::Running).await;
    h.wait_spawned(both).await;
    let request = &h.helper.spawned()[3];
    assert_eq!(request.gpus, [GpuIndex(0), GpuIndex(1)]);

    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn other_users_see_redacted_jobs_and_cannot_touch_them() {
    let mut h = Harness::new(1).await;
    let alice = h.client(ALICE);
    let bob = h.client(BOB);
    let admin = h.client(ADMIN);

    // Bob watches the event stream, and so does alice.
    let response = bob.open(Method::GET, "/v1/events", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut events = response.into_body();
    let response = alice.open(Method::GET, "/v1/events", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut own_events = response.into_body();

    let id = alice.add(&job_request("secret-command", &[0])).await;
    alice.wait_state(id, &JobState::Running).await;

    let seen = bob.job(id).await;
    assert!(seen.is_redacted());
    assert_eq!(seen.label, None);
    assert_eq!(seen.owner.0, ALICE);
    assert_eq!(seen.gpus, [GpuIndex(0)]);
    let list = String::from_utf8(bob.get("/v1/jobs").await.body.to_vec()).unwrap();
    assert!(
        !list.contains("secret-command") && !list.contains("hunter2"),
        "{list}"
    );
    assert!(!admin.job(id).await.is_redacted());

    for (action, body) in [
        ("kill", json!({})),
        ("pause", json!({})),
        ("checkpoint", json!({})),
    ] {
        let reply = bob.action(id, action, body).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{action}");
    }
    let reply = bob
        .call(Method::DELETE, &format!("/v1/jobs/{id}"), None)
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    // Bulk requests report per job.
    let reply: BulkJobResponse = bob
        .post(
            "/v1/jobs/bulk",
            json!({"target": {"kind": "ids", "ids": [id]}, "action": "kill"}),
        )
        .await
        .json();
    assert!(!reply.all_ok());
    // `-a` for bob does not include alice's job.
    let reply: BulkJobResponse = bob
        .post(
            "/v1/jobs/bulk",
            json!({"target": {"kind": "all"}, "action": "kill"}),
        )
        .await
        .json();
    assert!(reply.results.is_empty());
    // Admin-only operations.
    let reply = bob
        .post("/v1/groups", json!({"name": "gpu", "parallel": 2}))
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    let reply = bob.post("/v1/daemon/shutdown", json!({})).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);

    // The admin may kill it.
    let reply = admin.action(id, "kill", json!({})).await;
    assert_eq!(reply.status, StatusCode::OK);
    alice.wait_state(id, &done(JobResult::Killed)).await;
    assert_eq!(h.helper.signals(), [(id, signal::SIGTERM)]);

    // Reads an event stream until the job was added and killed.
    async fn until_killed(events: &mut hyper::body::Incoming) -> String {
        let mut text = String::new();
        while !text.contains("event: job_added")
            || !text.contains(r#""state":{"state":"done","result":"killed"}"#)
        {
            let frame = tokio::time::timeout(Duration::from_secs(5), events.frame())
                .await
                .expect("an event")
                .unwrap()
                .unwrap();
            if let Ok(data) = frame.into_data() {
                text.push_str(&String::from_utf8_lossy(&data));
            }
        }
        text
    }

    // Bob's events carry only redacted views.
    let text = until_killed(&mut events).await;
    assert!(!text.contains("secret-command"), "{text}");
    assert!(!text.contains("\"details\""), "{text}");
    // Alice's carry details, but no environment (like the job list).
    let text = until_killed(&mut own_events).await;
    assert!(text.contains("secret-command"), "{text}");
    assert!(
        !text.contains("envs") && !text.contains("hunter2"),
        "{text}"
    );

    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checkpoint_and_restore() {
    let mut h = Harness::new(1).await;
    let alice = h.client(ALICE);

    let id = alice.add(&job_request("train", &[0])).await;
    alice.wait_state(id, &JobState::Running).await;
    let reply = alice.action(id, "checkpoint", json!(null)).await;
    assert_eq!(reply.status, StatusCode::OK);
    let job = alice.wait_state(id, &JobState::Checkpointed).await;
    assert_eq!(job.checkpoint_size_bytes, Some(4096));
    assert!(h.helper.has_image(id));
    assert!(!h.helper.is_running(id));

    // `start` and `kill` point to restore/remove.
    let reply = alice.action(id, "start", json!(null)).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert!(reply.error().message.contains("restore"));

    // The GPU is free again: another job may take it.
    let other = alice.add(&job_request("other", &[0])).await;
    alice.wait_state(other, &JobState::Running).await;

    // Restore waits for the slot.
    let reply = alice.action(id, "restore", json!({})).await;
    assert_eq!(reply.status, StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(matches!(
        alice.job(id).await.state,
        JobState::RestoreQueued { .. }
    ));
    h.exit(other, 0).await;
    alice.wait_state(id, &JobState::Running).await;
    assert!(h.helper.is_running(id));
    assert!(!h.helper.has_image(id));
    assert_eq!(alice.job(id).await.checkpoint_size_bytes, None);

    // A failed restore keeps the job checkpointed with the reason.
    alice.action(id, "checkpoint", json!(null)).await;
    alice.wait_state(id, &JobState::Checkpointed).await;
    h.helper
        .set_restore_outcome(FakeOutcome::Fail("GPU model changed".to_owned()));
    let reply = alice
        .action(id, "restore", json!({"immediate": true}))
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    let job = alice
        .wait_for(id, "checkpointed with an error", |j| {
            j.state == JobState::Checkpointed
                && j.details.as_ref().and_then(|d| d.last_error.as_deref())
                    == Some("GPU model changed")
        })
        .await;
    assert_eq!(job.checkpoint_size_bytes, Some(4096));
    assert!(h.helper.has_image(id));
    // Other users do not see the reason.
    assert!(h.client(BOB).job(id).await.details.is_none());

    // Removing the job deletes the image.
    let reply = alice
        .call(Method::DELETE, &format!("/v1/jobs/{id}"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(!h.helper.has_image(id));
    assert_eq!(
        alice.get(&format!("/v1/jobs/{id}")).await.status,
        StatusCode::NOT_FOUND
    );

    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_checkpoint_terminates_the_job_while_holding_its_gpu() {
    let mut h = Harness::new(1).await;
    let alice = h.client(ALICE);
    h.helper
        .set_checkpoint_outcome(FakeOutcome::Fail("criu: tcp connection".to_owned()));
    h.helper.set_ignore_sigterm(true);

    let id = alice.add(&job_request("train", &[0])).await;
    alice.wait_state(id, &JobState::Running).await;
    let waiting = alice.add(&job_request("next", &[0])).await;

    alice.action(id, "checkpoint", json!(null)).await;
    let reason = JobResult::CheckpointFailed {
        reason: "criu: tcp connection".to_owned(),
    };
    alice
        .wait_state(
            id,
            &JobState::Terminating {
                result: reason.clone(),
            },
        )
        .await;
    // Still holding the GPU while it shuts down.
    assert!(matches!(
        alice.job(waiting).await.state,
        JobState::Queued { .. }
    ));
    let redacted = h.client(BOB).job(id).await;
    assert_eq!(
        redacted.state,
        JobState::Terminating {
            result: JobResult::CheckpointFailed {
                reason: puzuz_core::authz::REDACTED.to_owned()
            }
        }
    );

    // SIGTERM is ignored; SIGKILL follows after the 1 s grace period.
    let job = alice.wait_state(id, &done(reason)).await;
    assert_eq!(
        job.details.unwrap().last_error.as_deref(),
        Some("criu: tcp connection")
    );
    assert_eq!(
        h.helper.signals(),
        [(id, signal::SIGTERM), (id, signal::SIGKILL)]
    );
    alice.wait_state(waiting, &JobState::Running).await;

    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_checkpoint_fails_the_command_and_leaves_the_job_running() {
    let mut h = Harness::new(1).await;
    let alice = h.client(ALICE);
    let admin = h.client(ADMIN);
    let id = alice.add(&job_request("train", &[0])).await;
    admin.wait_state(id, &JobState::Running).await;
    h.wait_spawned(id).await;

    // Refused by the helper's synchronous pre-checks: the command fails
    // with the reason, and nothing is done to the job.
    h.helper
        .set_checkpoint_rejection(Some("not enough free space for the image"));
    let reply = admin.action(id, "checkpoint", json!(null)).await;
    let error = reply.error();
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(error.code, ErrorCode::InvalidState);
    assert_eq!(
        error.message,
        "checkpoint refused: not enough free space for the image; the job keeps running"
    );
    let job = admin.job(id).await;
    assert_eq!(job.state, JobState::Running);
    assert_eq!(
        last_error(&job),
        Some("checkpoint refused: not enough free space for the image")
    );
    assert!(h.helper.is_running(id));
    assert!(h.helper.signals().is_empty());
    // The owner hears about it (the admin asked).
    assert_eq!(
        alice.get("/v1/groups").await.warnings(),
        [Warning::CheckpointFailed { job: id }]
    );

    // Refused only when the dump was due (the request was accepted): the
    // job is back to running, its owner warned.
    h.helper.set_checkpoint_rejection(None);
    h.helper
        .set_checkpoint_outcome(FakeOutcome::Refuse("hidden mount /run/x".to_owned()));
    let reply = admin.action(id, "checkpoint", json!(null)).await;
    assert_eq!(reply.status, StatusCode::OK);
    admin
        .wait_for(id, "running with the refusal", |j| {
            j.state == JobState::Running
                && last_error(j) == Some("checkpoint refused: hidden mount /run/x")
        })
        .await;
    assert!(h.helper.is_running(id));
    assert!(h.helper.signals().is_empty());
    assert_eq!(
        alice.get("/v1/groups").await.warnings(),
        [Warning::CheckpointFailed { job: id }]
    );

    // A paused job was resumed for the dump; it is paused again.
    assert_eq!(
        alice.action(id, "pause", json!({})).await.status,
        StatusCode::OK
    );
    alice.wait_state(id, &JobState::Paused).await;
    alice.action(id, "checkpoint", json!(null)).await;
    alice
        .wait_for(id, "paused again", |j| {
            j.state == JobState::Paused && h.helper.signals().len() == 3
        })
        .await;
    assert_eq!(
        h.helper.signals(),
        [
            (id, signal::SIGSTOP),
            (id, signal::SIGCONT),
            (id, signal::SIGSTOP)
        ]
    );
    // Also when refused up front.
    h.helper
        .set_checkpoint_rejection(Some("not enough free space for the image"));
    let reply = alice.action(id, "checkpoint", json!(null)).await;
    assert_eq!(
        reply.error().message,
        "checkpoint refused: not enough free space for the image; the job stays paused"
    );
    assert_eq!(alice.job(id).await.state, JobState::Paused);
    assert_eq!(h.helper.signals().len(), 5);
    assert_eq!(h.helper.jobs()[&id].phase, RunningPhase::Stopped);
    h.helper.set_checkpoint_rejection(None);

    // Once the problem is gone it can be checkpointed.
    h.helper.set_checkpoint_outcome(FakeOutcome::Succeed);
    alice.action(id, "checkpoint", json!(null)).await;
    alice.wait_state(id, &JobState::Checkpointed).await;

    h.stop().await;
}

/// A checkpoint request that is lost (the connection breaks, or the helper
/// does not answer in time) does not leave the job checkpointing forever:
/// the reconciliation after a reconnect, or a later check, finds the
/// helper running it and puts it back as it was.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_checkpoint_requests_do_not_leave_jobs_checkpointing() {
    let mut h = Harness::new(0).await;
    let alice = h.client(ALICE);
    let id = alice.add(&job_request("a", &[])).await;
    alice.wait_state(id, &JobState::Running).await;
    h.wait_spawned(id).await;
    h.helper.set_checkpoint_outcome(FakeOutcome::LoseRequest);
    let lost = |j: &JobView| last_error(j).is_some_and(|e| e.contains("lost"));

    // Found by the reconciliation after a reconnect.
    let reply = alice.action(id, "checkpoint", json!(null)).await;
    assert_eq!(reply.error().code, ErrorCode::HelperUnavailable);
    assert_eq!(alice.job(id).await.state, JobState::Checkpointing);
    h.helper.disconnect();
    h.helper.reconnect();
    alice
        .wait_for(id, "running again", |j| {
            j.state == JobState::Running && lost(j)
        })
        .await;
    assert!(h.helper.signals().is_empty());

    // Found by the check after the timeout; a paused job (resumed for the
    // dump) is paused again.
    alice.action(id, "pause", json!({})).await;
    alice.wait_state(id, &JobState::Paused).await;
    let reply = alice.action(id, "checkpoint", json!(null)).await;
    assert_eq!(reply.error().code, ErrorCode::HelperUnavailable);
    assert_eq!(alice.job(id).await.state, JobState::Checkpointing);
    alice
        .wait_for(id, "paused again", |j| j.state == JobState::Paused)
        .await;
    assert_eq!(h.helper.jobs()[&id].phase, RunningPhase::Stopped);
    assert_eq!(
        h.helper.signals(),
        [
            (id, signal::SIGSTOP),
            (id, signal::SIGCONT),
            (id, signal::SIGSTOP)
        ]
    );

    // A dump still in progress is left alone.
    h.helper.set_checkpoint_outcome(FakeOutcome::Hold);
    alice.action(id, "checkpoint", json!(null)).await;
    h.helper.disconnect();
    h.helper.reconnect();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(alice.job(id).await.state, JobState::Checkpointing);
    h.helper.dump(id);
    alice.wait_state(id, &JobState::Checkpointed).await;
    h.stop().await;
}

/// The helper replays the events the daemon applied but did not
/// acknowledge (it stopped in between); they are not applied twice. A
/// replayed refusal would otherwise end the job, which has been running
/// again since, as "checkpoint failed".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replayed_helper_events_are_applied_once() {
    let mut h = Harness::new(0).await;
    h.set_default_parallel(2).await;
    let alice = h.client(ALICE);
    let refused = alice.add(&job_request("a", &[])).await;
    let exited = alice.add(&job_request("b", &[])).await;
    for id in [refused, exited] {
        alice.wait_state(id, &JobState::Running).await;
        h.wait_spawned(id).await;
    }
    h.helper.set_ignore_acks(true);
    h.helper
        .set_checkpoint_outcome(FakeOutcome::Refuse("hidden mount /run/x".to_owned()));
    let reply = alice.action(refused, "checkpoint", json!(null)).await;
    assert_eq!(reply.status, StatusCode::OK);
    alice
        .wait_for(refused, "running with the refusal", |j| {
            j.state == JobState::Running
                && last_error(j) == Some("checkpoint refused: hidden mount /run/x")
        })
        .await;
    h.exit(exited, 3).await;
    let failed = done(JobResult::Failed { exit_code: 3 });
    alice.wait_state(exited, &failed).await;
    assert_eq!(h.helper.unacked().len(), 2);

    // After a daemon restart, and after a helper reconnect.
    for daemon_restart in [true, false] {
        if daemon_restart {
            h.stop().await;
            h.start(0).await;
        } else {
            h.helper.disconnect();
            h.helper.reconnect();
        }
        let alice = h.client(ALICE);
        // The replayed events are acknowledged (or would be).
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(alice.job(refused).await.state, JobState::Running);
        assert_eq!(alice.job(exited).await.state, failed);
        assert!(h.helper.signals().is_empty(), "{:?}", h.helper.signals());
        assert!(h.helper.is_running(refused));
    }
    h.helper.set_ignore_acks(false);
    h.helper.disconnect();
    h.helper.reconnect();
    for _ in 0..100 {
        if h.helper.unacked().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(h.helper.unacked().is_empty(), "{:?}", h.helper.unacked());

    // New events still apply.
    h.helper.exit(refused, 0);
    alice.wait_state(refused, &done(JobResult::Success)).await;
    h.stop().await;
}

/// A job paused when it is checkpointed is resumed for the dump (CRIU and
/// cuda-checkpoint need it running) and paused again once restored, still
/// holding its GPU. One running when checkpointed comes back running.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_paused_before_its_checkpoint_is_restored_paused() {
    let mut h = Harness::new(1).await;
    let alice = h.client(ALICE);
    let id = alice.add(&job_request("train", &[0])).await;
    alice.wait_state(id, &JobState::Running).await;
    h.wait_spawned(id).await;
    assert_eq!(
        alice.action(id, "pause", json!({})).await.status,
        StatusCode::OK
    );
    alice.wait_state(id, &JobState::Paused).await;
    let reply = alice.action(id, "checkpoint", json!(null)).await;
    assert_eq!(reply.status, StatusCode::OK);
    alice.wait_state(id, &JobState::Checkpointed).await;
    let reply = alice
        .action(id, "restore", json!({"immediate": true}))
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    alice.wait_state(id, &JobState::Paused).await;
    assert_eq!(
        h.helper.signals(),
        [
            (id, signal::SIGSTOP),
            (id, signal::SIGCONT),
            (id, signal::SIGSTOP)
        ]
    );
    assert_eq!(h.helper.jobs()[&id].phase, RunningPhase::Stopped);
    assert!(!h.helper.has_image(id));
    assert_eq!(alice.job(id).await.checkpoint_size_bytes, None);
    // It keeps its GPU while paused.
    let other = alice.add(&job_request("other", &[0])).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(matches!(
        alice.job(other).await.state,
        JobState::Queued { .. }
    ));
    let reply = alice
        .call(Method::DELETE, &format!("/v1/jobs/{other}"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK);

    // `start` resumes it; checkpointed while running, it is restored running.
    assert_eq!(
        alice.action(id, "start", json!(null)).await.status,
        StatusCode::OK
    );
    alice.wait_state(id, &JobState::Running).await;
    alice.action(id, "checkpoint", json!(null)).await;
    alice.wait_state(id, &JobState::Checkpointed).await;
    alice
        .action(id, "restore", json!({"immediate": true}))
        .await;
    alice.wait_state(id, &JobState::Running).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(alice.job(id).await.state, JobState::Running);
    assert_eq!(h.helper.signals().len(), 4, "{:?}", h.helper.signals());
    assert_eq!(h.helper.jobs()[&id].phase, RunningPhase::Running);

    h.stop().await;
}

/// On an OS shutdown the helper dumps a paused job on its own (resuming it
/// first); after the reboot it is restored paused as well.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_paused_at_an_os_shutdown_is_restored_paused() {
    let mut h = Harness::new(0).await;
    h.set_default_parallel(2).await;
    let alice = h.client(ALICE);
    let paused = alice.add(&job_request("a", &[])).await;
    let running = alice.add(&job_request("b", &[])).await;
    for id in [paused, running] {
        alice.wait_state(id, &JobState::Running).await;
        h.wait_spawned(id).await;
    }
    alice.action(paused, "pause", json!({})).await;
    alice.wait_state(paused, &JobState::Paused).await;
    h.stop().await;
    h.helper.dump(paused);
    h.helper.dump(running);

    h.start(0).await;
    let alice = h.client(ALICE);
    for id in [paused, running] {
        alice.wait_state(id, &JobState::Checkpointed).await;
        let reply = alice
            .action(id, "restore", json!({"immediate": true}))
            .await;
        assert_eq!(reply.status, StatusCode::OK);
    }
    alice.wait_state(paused, &JobState::Paused).await;
    alice.wait_state(running, &JobState::Running).await;
    assert_eq!(
        h.helper.signals(),
        [(paused, signal::SIGSTOP), (paused, signal::SIGSTOP)]
    );
    h.stop().await;
}

/// `pause` is refused while a dump is in progress (SIGSTOP would make
/// cuda-checkpoint hang); the job is checkpointed as asked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_is_refused_while_the_job_is_checkpointed() {
    let mut h = Harness::new(0).await;
    let alice = h.client(ALICE);
    let id = alice.add(&job_request("a", &[])).await;
    alice.wait_state(id, &JobState::Running).await;
    h.wait_spawned(id).await;
    // The dump never finishes on its own.
    h.helper.set_checkpoint_outcome(FakeOutcome::Hold);
    assert_eq!(
        alice.action(id, "checkpoint", json!(null)).await.status,
        StatusCode::OK
    );
    alice.wait_state(id, &JobState::Checkpointing).await;
    let reply = alice.action(id, "pause", json!({})).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    let error = reply.error();
    assert_eq!(error.code, ErrorCode::InvalidState);
    assert!(error.message.contains("checkpoint"), "{}", error.message);
    assert!(h.helper.signals().is_empty());
    h.helper.dump(id);
    alice.wait_state(id, &JobState::Checkpointed).await;
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_leaves_jobs_whose_checkpoint_is_refused_running() {
    // Refused up front, or only when the dump was due.
    for up_front in [true, false] {
        let mut h = Harness::new(0).await;
        h.set_default_parallel(2).await;
        let alice = h.client(ALICE);
        let refused = alice.add(&job_request("a", &[])).await;
        let mut request = job_request("b", &[]);
        request.spec.no_checkpoint = true;
        let stopped = alice.add(&request).await;
        for id in [refused, stopped] {
            alice.wait_state(id, &JobState::Running).await;
            h.wait_spawned(id).await;
        }
        if up_front {
            h.helper
                .set_checkpoint_rejection(Some("hidden mount /run/x"));
        } else {
            h.helper
                .set_checkpoint_outcome(FakeOutcome::Refuse("hidden mount /run/x".to_owned()));
        }

        let reply = h.client(ADMIN).post("/v1/daemon/shutdown", json!({})).await;
        assert_eq!(reply.status, StatusCode::ACCEPTED);
        let daemon = h.daemon.take().unwrap();
        // Without retrying the refused job until the settle wait (11 s)
        // runs out.
        tokio::time::timeout(Duration::from_secs(5), daemon.shutdown_requested())
            .await
            .expect("shutdown finishes");

        let job = alice.job(refused).await;
        assert_eq!(job.state, JobState::Running, "up front: {up_front}");
        assert_eq!(
            last_error(&job),
            Some("checkpoint refused: hidden mount /run/x")
        );
        assert!(h.helper.is_running(refused));
        // The other job was handled as usual.
        assert_eq!(alice.job(stopped).await.state, done(JobResult::Killed));
        assert_eq!(h.helper.signals(), [(stopped, signal::SIGTERM)]);
        daemon.stop().await;

        // The next daemon finds it running.
        h.start(0).await;
        let alice = h.client(ALICE);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(alice.job(refused).await.state, JobState::Running);
        h.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_os_shutdown_dumps_end_as_checkpoint_failed_after_the_reboot() {
    let mut h = Harness::new(0).await;
    h.set_default_parallel(3).await;
    let alice = h.client(ALICE);
    let failed = alice.add(&job_request("a", &[])).await;
    let refused = alice.add(&job_request("b", &[])).await;
    let paused = alice.add(&job_request("c", &[])).await;
    for id in [failed, refused, paused] {
        alice.wait_state(id, &JobState::Running).await;
        h.wait_spawned(id).await;
    }
    alice.action(paused, "pause", json!({})).await;
    alice.wait_state(paused, &JobState::Paused).await;
    h.stop().await;

    // OS shutdown: the helper dumps the jobs on its own; every dump fails
    // or is refused, and systemd stops the jobs. One exit is recorded, the
    // others are not (the helper had already exited).
    use puzuz_protocol::helper::{CheckpointOutcome, Event, ExitStatus};
    for (id, outcome) in [
        (
            failed,
            CheckpointOutcome::failed("criu: tcp connection (CRIU log: /x.log)"),
        ),
        (refused, CheckpointOutcome::refused("hidden mount /run/x")),
        (
            paused,
            CheckpointOutcome::failed("cuda-checkpoint timed out"),
        ),
    ] {
        h.helper
            .emit_event(Event::CheckpointFinished { job: id, outcome });
    }
    h.helper.emit_event(Event::JobExited {
        job: failed,
        status: ExitStatus::Signaled { signal: 15 },
    });
    for id in [failed, refused, paused] {
        h.helper.vanish(id);
    }

    // After the reboot the replayed events and the reconciliation agree.
    h.start(0).await;
    let alice = h.client(ALICE);
    // Polled as the admin: the owner's requests would take the warnings.
    let admin = h.client(ADMIN);
    for (id, reason) in [
        (failed, "criu: tcp connection (CRIU log: /x.log)"),
        (refused, "checkpoint refused: hidden mount /run/x"),
        (paused, "cuda-checkpoint timed out"),
    ] {
        let job = admin
            .wait_state(
                id,
                &done(JobResult::CheckpointFailed {
                    reason: reason.to_owned(),
                }),
            )
            .await;
        assert_eq!(last_error(&job), Some(reason));
    }
    assert_eq!(
        alice.get("/v1/groups").await.warnings(),
        [
            Warning::CheckpointFailed { job: failed },
            Warning::CheckpointFailed { job: refused },
            Warning::CheckpointFailed { job: paused },
        ]
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_checkpoints_running_jobs_then_stops() {
    let mut h = Harness::new(2).await;
    h.set_default_parallel(4).await;
    let alice = h.client(ALICE);

    let saved = alice.add(&job_request("train", &[0])).await;
    let mut request = job_request("preprocess", &[]);
    request.spec.no_checkpoint = true;
    let killed = alice.add(&request).await;
    alice.wait_state(saved, &JobState::Running).await;
    alice.wait_state(killed, &JobState::Running).await;
    let reply = alice.action(saved, "pause", json!({})).await;
    assert_eq!(reply.status, StatusCode::OK);
    alice.wait_state(saved, &JobState::Paused).await;
    let mut stashed = job_request("later", &[]);
    stashed.stashed = true;
    let untouched = alice.add(&stashed).await;

    let reply = h.client(ADMIN).post("/v1/daemon/shutdown", json!({})).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    let daemon = h.daemon.take().unwrap();
    tokio::time::timeout(Duration::from_secs(10), daemon.shutdown_requested())
        .await
        .expect("shutdown finishes");

    assert_eq!(alice.job(saved).await.state, JobState::Checkpointed);
    assert_eq!(alice.job(killed).await.state, done(JobResult::Killed));
    assert!(matches!(
        alice.job(untouched).await.state,
        JobState::Stashed { .. }
    ));
    // The paused job was resumed before the dump.
    assert!(h.helper.signals().contains(&(saved, signal::SIGCONT)));
    // No new work is accepted.
    let reply = alice
        .post(
            "/v1/jobs",
            serde_json::to_value(job_request("x", &[])).unwrap(),
        )
        .await;
    assert_eq!(reply.error().code, ErrorCode::ShuttingDown);

    daemon.stop().await;
    assert!(!h.dir.path().join("puzuz.sock").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_restart_reconciles_with_the_helper() {
    let mut h = Harness::new(0).await;
    h.set_default_parallel(3).await;
    let alice = h.client(ALICE);
    let finished = alice.add(&job_request("a", &[])).await;
    let lost = alice.add(&job_request("b", &[])).await;
    let survivor = alice.add(&job_request("c", &[])).await;
    let queued = alice.add(&job_request("d", &[])).await;
    for id in [finished, lost, survivor] {
        alice.wait_state(id, &JobState::Running).await;
        h.wait_spawned(id).await;
    }

    // Stopping the daemon leaves the jobs alone.
    h.stop().await;
    assert_eq!(h.helper.jobs().len(), 3);
    assert!(h.helper.signals().is_empty());

    // Meanwhile: one job exits (the helper queues the event), one vanishes,
    // and the helper runs a job the daemon never heard of.
    h.exit(finished, 0).await;
    h.helper.vanish(lost);
    h.helper.insert_running(JobId(99));

    h.start(0).await;
    let alice = h.client(ALICE);
    alice.wait_state(finished, &done(JobResult::Success)).await;
    alice.wait_state(lost, &done(JobResult::Lost)).await;
    assert_eq!(alice.job(survivor).await.state, JobState::Running);
    // Freed slots are used again.
    alice.wait_state(queued, &JobState::Running).await;
    // The replayed exit event was acknowledged.
    for _ in 0..100 {
        if h.helper.unacked().iter().all(|e| {
            !matches!(e.event, puzuz_protocol::helper::Event::JobExited { job, .. } if job == finished)
        }) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(h.helper.unacked().is_empty(), "{:?}", h.helper.unacked());
    // The orphan is left running.
    assert!(h.helper.is_running(JobId(99)));

    // Helper reconnects are reconciled too.
    h.helper.disconnect();
    h.helper.vanish(survivor);
    h.helper.reconnect();
    alice.wait_state(survivor, &done(JobResult::Lost)).await;

    h.stop().await;
}

/// A dependant keeps its dependency from being removed, so users can only
/// depend on their own jobs (administrators on any). A repeated dependency
/// is recorded once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dependencies_are_own_jobs_and_recorded_once() {
    let mut h = Harness::new(0).await;
    let alice = h.client(ALICE);
    let bob = h.client(BOB);
    let mut stashed = job_request("b", &[]);
    stashed.stashed = true;
    let bobs = bob.add(&stashed).await;

    let mut request = job_request("a", &[]);
    request.spec.dependencies = vec![bobs];
    let reply = alice
        .post("/v1/jobs", serde_json::to_value(&request).unwrap())
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    let error = reply.error();
    assert_eq!(error.code, ErrorCode::PermissionDenied);
    assert!(
        error.message.contains(&bobs.to_string()),
        "{}",
        error.message
    );
    // Nothing was added, and Bob's job is still his to remove.
    let list: ListJobsResponse = alice.get("/v1/jobs").await.json();
    assert_eq!(list.jobs.len(), 1);
    let reply = bob
        .call(Method::DELETE, &format!("/v1/jobs/{bobs}"), None)
        .await;
    assert!(reply.status.is_success(), "{}", reply.status);

    let bobs = bob.add(&stashed).await;
    request.spec.dependencies = vec![bobs];
    request.stashed = true;
    h.client(ADMIN).add(&request).await;

    let alices = alice.add(&stashed).await;
    request.spec.dependencies = vec![alices, alices, bobs, alices];
    let reply = alice
        .post("/v1/jobs", serde_json::to_value(&request).unwrap())
        .await;
    assert_eq!(reply.error().code, ErrorCode::PermissionDenied);
    request.spec.dependencies = vec![alices, alices];
    let id = alice.add(&request).await;
    assert_eq!(alice.job(id).await.dependencies, [alices]);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn validation_versions_groups_and_bulk_operations() {
    let mut h = Harness::new(1).await;
    let alice = h.client(ALICE);
    let admin = h.client(ADMIN);

    // Unknown dependency, GPU, group; relative cwd; root.
    let mut request = job_request("x", &[]);
    request.spec.dependencies = vec![JobId(42)];
    let reply = alice
        .post("/v1/jobs", serde_json::to_value(&request).unwrap())
        .await;
    assert_eq!(reply.error().code, ErrorCode::BadRequest);
    assert!(reply.error().message.contains("42"));
    let reply = alice
        .post(
            "/v1/jobs",
            serde_json::to_value(job_request("x", &[7])).unwrap(),
        )
        .await;
    assert_eq!(reply.error().code, ErrorCode::BadRequest);
    let mut request = job_request("x", &[]);
    request.spec.group = "nope".to_owned();
    let reply = alice
        .post("/v1/jobs", serde_json::to_value(&request).unwrap())
        .await;
    assert_eq!(reply.error().code, ErrorCode::GroupNotFound);
    let mut request = job_request("x", &[]);
    request.spec.cwd = PathBuf::from("relative");
    let reply = alice
        .post("/v1/jobs", serde_json::to_value(&request).unwrap())
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let reply = h
        .client(0)
        .post(
            "/v1/jobs",
            serde_json::to_value(job_request("x", &[])).unwrap(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let reply = alice.post("/v1/jobs", json!({"garbage": true})).await;
    assert_eq!(reply.error().code, ErrorCode::BadRequest);

    // Version check (but /v1/version always answers).
    let mut old = alice.clone();
    old.version = "0.0.1".to_owned();
    let reply = old.get("/v1/jobs").await;
    assert_eq!(reply.error().code, ErrorCode::VersionMismatch);
    let version: VersionResponse = old.get("/v1/version").await.json();
    assert_eq!(version.daemon_version, BINARY_VERSION);
    assert!(version.helper.connected);

    // Groups.
    let reply = admin
        .post("/v1/groups", json!({"name": "gpu", "parallel": 2}))
        .await;
    assert_eq!(reply.status, StatusCode::CREATED);
    let reply = admin.post("/v1/groups", json!({"name": "gpu"})).await;
    assert_eq!(reply.error().code, ErrorCode::AlreadyExists);

    // Dependencies, stash, enqueue, switch.
    let first = alice.add(&job_request("first", &[])).await;
    alice.wait_state(first, &JobState::Running).await;
    let mut request = job_request("after", &[]);
    request.spec.dependencies = vec![first];
    let dependant = alice.add(&request).await;
    let mut request = job_request("stashed", &[]);
    request.stashed = true;
    let stashed = alice.add(&request).await;
    let reply = alice
        .post(
            "/v1/jobs/switch",
            json!({"first": dependant, "second": stashed}),
        )
        .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    assert_eq!(
        alice.job(dependant).await.details.unwrap().command,
        "stashed"
    );
    assert_eq!(alice.job(stashed).await.details.unwrap().command, "after");
    let (dependant, stashed) = (stashed, dependant);
    let reply = alice.action(stashed, "enqueue", json!({})).await;
    assert_eq!(reply.status, StatusCode::OK);

    // A failed dependency fails its dependants.
    h.exit(first, 1).await;
    alice
        .wait_state(dependant, &done(JobResult::DependencyFailed))
        .await;
    alice.wait_state(stashed, &JobState::Running).await;

    // Pause and resume.
    assert_eq!(
        alice.action(stashed, "pause", json!({})).await.status,
        StatusCode::OK
    );
    alice.wait_state(stashed, &JobState::Paused).await;
    assert_eq!(
        alice.action(stashed, "start", json!(null)).await.status,
        StatusCode::OK
    );
    alice.wait_state(stashed, &JobState::Running).await;
    // Removing a running job is refused with a hint.
    let reply = alice
        .call(Method::DELETE, &format!("/v1/jobs/{stashed}"), None)
        .await;
    assert_eq!(reply.error().code, ErrorCode::InvalidState);
    assert!(reply.error().message.contains("kill"));

    // Restart creates a new job.
    let reply = alice.action(first, "restart", json!({})).await;
    let result: puzuz_protocol::api::JobActionResult = reply.json();
    let puzuz_protocol::api::ActionOutcome::Ok {
        new_id: Some(copy), ..
    } = result.outcome
    else {
        panic!("{result:?}");
    };
    alice
        .wait_for(copy, "queued or running", |j| {
            matches!(j.state, JobState::Queued { .. } | JobState::Running)
        })
        .await;

    // Clean removes finished jobs (only the caller's).
    let bob = h.client(BOB);
    let bobs = bob.add(&job_request("bob", &[])).await;
    let cleaned: CleanResponse = alice.post("/v1/jobs/clean", json!({})).await.json();
    assert_eq!(cleaned.removed, vec![first, dependant]);
    assert_eq!(
        alice.get(&format!("/v1/jobs/{first}")).await.status,
        StatusCode::NOT_FOUND
    );

    // Reset kills and removes everything of alice's, but not bob's job.
    let reset: BulkJobResponse = alice.post("/v1/jobs/reset", json!({})).await.json();
    assert!(reset.all_ok(), "{reset:?}");
    for _ in 0..250 {
        let list: ListJobsResponse = alice.get("/v1/jobs").await.json();
        if list.jobs.iter().all(|j| j.owner.0 != ALICE) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let list: ListJobsResponse = alice.get("/v1/jobs").await.json();
    assert_eq!(list.jobs.iter().map(|j| j.id).collect::<Vec<_>>(), [bobs]);

    // Deleting a group.
    let reply = admin.call(Method::DELETE, "/v1/groups/gpu", None).await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);

    h.stop().await;
}

// ---------------------------------------------------------------------------
// GPU detection, checkpoint bookkeeping and warnings
// ---------------------------------------------------------------------------

fn a100(index: u32) -> DetectedGpu {
    DetectedGpu {
        index: GpuIndex(index),
        uuid: format!("GPU-a100-{index}"),
        name: "NVIDIA A100".to_owned(),
        memory_total_bytes: Some(80 << 30),
        driver_version: Some("570.86.10".to_owned()),
    }
}

async fn gpus(client: &Client) -> ListGpusResponse {
    let reply = client.get("/v1/gpus").await;
    assert_eq!(reply.status, StatusCode::OK);
    reply.json()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undetected_gpus_are_kept_but_start_nothing() {
    let mut h = Harness::with_gpus(StaticGpuDetector(vec![a100(0), a100(1)])).await;
    h.set_default_parallel(4).await;
    let alice = h.client(ALICE);
    let admin = h.client(ADMIN);

    let listed = gpus(&alice).await.gpus;
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().all(|g| g.available));
    assert_eq!(listed[1].memory_total_bytes, Some(80 << 30));
    assert_eq!(listed[1].driver_version.as_deref(), Some("570.86.10"));
    let reply = admin
        .call(
            Method::PUT,
            "/v1/gpus/1/parallel",
            Some(json!({ "parallel": 2 })),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    let mut stashed = job_request("needs gpu 1", &[1]);
    stashed.stashed = true;
    let waiting = alice.add(&stashed).await;

    // GPU 1 disappears.
    h.stop().await;
    h.start_with(StaticGpuDetector(vec![a100(0)])).await;
    let alice = h.client(ALICE);
    let listed = gpus(&alice).await.gpus;
    assert_eq!(listed.len(), 2, "the missing GPU stays listed");
    assert!(listed[0].available);
    assert!(!listed[1].available);
    assert_eq!(listed[1].parallel, 2, "its limit is kept");
    assert_eq!(listed[1].memory_total_bytes, None);

    // Queued jobs that need it wait; new ones and forced starts are refused.
    let reply = alice.action(waiting, "enqueue", json!({})).await;
    assert_eq!(reply.status, StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let state = alice.job(waiting).await.state;
    assert!(matches!(state, JobState::Queued { .. }), "{state:?}");
    let reply = alice.action(waiting, "start", json!(null)).await;
    assert_eq!(reply.error().code, ErrorCode::InvalidState);
    assert!(reply.error().message.contains("not detected"));
    let reply = alice
        .post(
            "/v1/jobs",
            serde_json::to_value(job_request("x", &[1])).unwrap(),
        )
        .await;
    assert_eq!(reply.error().code, ErrorCode::InvalidState);
    // Other GPUs work as usual.
    let other = alice.add(&job_request("on gpu 0", &[0])).await;
    alice.wait_state(other, &JobState::Running).await;

    // It comes back: the waiting job starts.
    h.stop().await;
    h.start_with(StaticGpuDetector(vec![a100(0), a100(1)]))
        .await;
    let alice = h.client(ALICE);
    assert!(gpus(&alice).await.gpus.iter().all(|g| g.available));
    alice.wait_state(waiting, &JobState::Running).await;

    h.stop().await;
}

struct BrokenNvml;
impl GpuDetector for BrokenNvml {
    fn detect(&self) -> Result<Vec<DetectedGpu>, String> {
        Err("cannot initialize NVML: NVIDIA driver is not loaded".to_owned())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn version_reports_how_gpus_were_detected() {
    let mut h = Harness::with_gpus(StaticGpuDetector(vec![a100(0)])).await;
    let version: VersionResponse = h.client(ALICE).get("/v1/version").await.json();
    let detection = version.gpu_detection.expect("reported");
    assert_eq!(detection.mode, GpuDetection::Auto);
    assert_eq!(detection.source, GpuSource::Config);
    assert_eq!(detection.nvml_error, None);
    assert_eq!(detection.gpus.len(), 1);
    assert_eq!(detection.gpus[0].uuid, "GPU-a100-0");

    // NVML fails: the configured list is used, and the reason is reported.
    h.stop().await;
    h.start_with(FallbackGpuDetector {
        primary: Box::new(BrokenNvml),
        fallback: StaticGpuDetector(vec![a100(0), a100(1)]),
    })
    .await;
    let version: VersionResponse = h.client(ALICE).get("/v1/version").await.json();
    let detection = version.gpu_detection.expect("reported");
    assert_eq!(detection.source, GpuSource::Config);
    assert!(
        detection
            .nvml_error
            .as_deref()
            .is_some_and(|e| e.contains("driver is not loaded")),
        "{detection:?}"
    );
    assert_eq!(detection.gpus.len(), 2);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checkpoint_records_gpu_model_driver_and_size() {
    let mut h = Harness::with_gpus(StaticGpuDetector(vec![a100(0)])).await;
    let alice = h.client(ALICE);

    let id = alice.add(&job_request("train", &[0])).await;
    alice.wait_state(id, &JobState::Running).await;
    alice.action(id, "checkpoint", json!(null)).await;
    let job = alice.wait_state(id, &JobState::Checkpointed).await;
    assert_eq!(job.checkpoint_size_bytes, Some(4096));

    let store = puzuz_core::storage::Store::open(h.dir.path().join("daemon/state.sqlite")).unwrap();
    let record = puzuz_core::storage::StoreOps::get_checkpoint(&store, id)
        .unwrap()
        .unwrap();
    assert_eq!(record.gpu_model.as_deref(), Some("NVIDIA A100"));
    assert_eq!(record.driver_version.as_deref(), Some("570.86.10"));
    assert_eq!(record.size_bytes, Some(4096));
    assert_eq!(
        record.status,
        puzuz_core::storage::CheckpointStatus::Complete
    );

    // CPU-only jobs record no GPU.
    let cpu = alice.add(&job_request("cpu", &[])).await;
    alice.wait_state(cpu, &JobState::Running).await;
    alice.action(cpu, "checkpoint", json!(null)).await;
    alice.wait_state(cpu, &JobState::Checkpointed).await;
    let record = puzuz_core::storage::StoreOps::get_checkpoint(&store, cpu)
        .unwrap()
        .unwrap();
    assert_eq!(record.gpu_model, None);
    assert_eq!(record.driver_version, None);

    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_is_warned_once_about_a_failed_checkpoint() {
    let mut h = Harness::new(1).await;
    let alice = h.client(ALICE);
    let bob = h.client(BOB);
    let admin = h.client(ADMIN);
    h.helper
        .set_checkpoint_outcome(FakeOutcome::Fail("criu: tcp connection".to_owned()));

    let id = alice.add(&job_request("train", &[0])).await;
    // Poll as the admin: the owner's requests would take the warning.
    admin.wait_state(id, &JobState::Running).await;
    // Asked by the admin, so the failure cannot ride on the owner's reply.
    let reply = admin.action(id, "checkpoint", json!(null)).await;
    assert_eq!(reply.status, StatusCode::OK);
    admin
        .wait_for(id, "done", |j| matches!(j.state, JobState::Done { .. }))
        .await;

    // Only the owner gets it, on their next command, once.
    assert!(bob.get("/v1/jobs").await.warnings().is_empty());
    assert!(admin.get("/v1/jobs").await.warnings().is_empty());
    // Not on /v1/version, which every client may call first.
    assert!(alice.get("/v1/version").await.warnings().is_empty());
    let reply = alice.get("/v1/groups").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.warnings(), [Warning::CheckpointFailed { job: id }]);
    assert!(alice.get("/v1/jobs").await.warnings().is_empty());

    // The reason stays visible in the job view.
    assert_eq!(
        alice.job(id).await.details.unwrap().last_error.as_deref(),
        Some("criu: tcp connection")
    );

    // A warning survives a daemon restart until it is delivered.
    let second = alice.add(&job_request("again", &[0])).await;
    admin.wait_state(second, &JobState::Running).await;
    admin.action(second, "checkpoint", json!(null)).await;
    admin
        .wait_for(second, "done", |j| matches!(j.state, JobState::Done { .. }))
        .await;
    h.stop().await;
    h.start(1).await;
    let alice = h.client(ALICE);
    assert_eq!(
        alice.get("/v1/jobs").await.warnings(),
        [Warning::CheckpointFailed { job: second }]
    );

    h.stop().await;
}

// ---------------------------------------------------------------------------
// Checkpoint images over `checkpoint_size_limit_bytes` (the fake helper's
// images are 4096 bytes each).
// ---------------------------------------------------------------------------

const LIMIT: u64 = 5000;
const CAROL: u32 = 1002;

impl Harness {
    /// A daemon with a checkpoint size limit of [`LIMIT`].
    async fn with_limit(warning_interval_secs: u64) -> Self {
        let mut h = Harness::new(1).await;
        h.stop().await;
        h.checkpoint_limit = Some(LIMIT);
        h.warning_interval_secs = warning_interval_secs;
        h.start(1).await;
        h.set_default_parallel(8).await;
        h
    }

    /// Runs a job of `owner` and has the owner checkpoint it, polling as
    /// carol, who owns no jobs and is not an administrator (so the polls
    /// cannot swallow anybody's warnings).
    async fn checkpoint_as(&self, owner: u32) -> JobId {
        let client = self.client(owner);
        let carol = self.client(CAROL);
        let id = client.add(&job_request("train", &[])).await;
        carol.wait_state(id, &JobState::Running).await;
        let reply = client.action(id, "checkpoint", json!(null)).await;
        assert_eq!(reply.status, StatusCode::OK);
        carol.wait_state(id, &JobState::Checkpointed).await;
        id
    }
}

/// Polls `/v1/groups` until the daemon attaches warnings (5 s at most).
async fn next_warnings(client: &Client) -> Vec<Warning> {
    for _ in 0..250 {
        let warnings = client.get("/v1/groups").await.warnings();
        if !warnings.is_empty() {
            return warnings;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no warnings arrived for uid {}", client.uid);
}

fn usage(total_bytes: u64) -> Warning {
    Warning::CheckpointUsageOverLimit {
        total_bytes,
        limit_bytes: LIMIT,
    }
}

fn over_limit(job: JobId, total_bytes: u64) -> Warning {
    Warning::CheckpointOverLimit {
        job,
        total_bytes,
        limit_bytes: LIMIT,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checkpoints_over_the_size_limit_warn_owners_and_admins() {
    let mut h = Harness::with_limit(3600).await;
    let alice = h.client(ALICE);
    let bob = h.client(BOB);
    let admin = h.client(ADMIN);

    // Under the limit: nobody is warned.
    let first = h.checkpoint_as(ALICE).await;
    assert!(alice.get("/v1/groups").await.warnings().is_empty());
    assert!(admin.get("/v1/groups").await.warnings().is_empty());
    let version: VersionResponse = admin.get("/v1/version").await.json();
    let storage = version.checkpoints.expect("the daemon reports its usage");
    assert_eq!((storage.images, storage.total_bytes), (1, 4096));
    assert_eq!(storage.limit_bytes, Some(LIMIT));
    assert!(!storage.over_limit());

    // The second image crosses it.
    let second = h.checkpoint_as(ALICE).await;
    // The owner is told once, about the checkpoint that crossed it.
    assert_eq!(next_warnings(&alice).await, [over_limit(second, 8192)]);
    assert!(alice.get("/v1/jobs").await.warnings().is_empty());
    // Administrators are reminded, rate-limited.
    assert_eq!(admin.get("/v1/jobs").await.warnings(), [usage(8192)]);
    assert!(admin.get("/v1/jobs").await.warnings().is_empty());
    assert!(admin.get("/v1/groups").await.warnings().is_empty());
    // Others are not, and /v1/version never carries warnings.
    assert!(bob.get("/v1/jobs").await.warnings().is_empty());
    let reply = admin.get("/v1/version").await;
    assert!(reply.warnings().is_empty());
    let storage = reply.json::<VersionResponse>().checkpoints.unwrap();
    assert_eq!((storage.images, storage.total_bytes), (2, 8192));
    assert!(storage.over_limit());

    // Back under the limit (images are only removed by people)...
    let reply = alice
        .call(Method::DELETE, &format!("/v1/jobs/{second}"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(admin.get("/v1/jobs").await.warnings().is_empty());
    // ...and over it again: administrators hear about it right away.
    let third = h.checkpoint_as(BOB).await;
    assert_eq!(next_warnings(&admin).await, [usage(8192)]);
    assert!(admin.get("/v1/jobs").await.warnings().is_empty());
    assert_eq!(next_warnings(&bob).await, [over_limit(third, 8192)]);
    assert!(alice.get("/v1/jobs").await.warnings().is_empty());
    assert_eq!(alice.job(first).await.checkpoint_size_bytes, Some(4096));

    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admins_are_reminded_once_per_interval() {
    let mut h = Harness::with_limit(1).await;
    let admin = h.client(ADMIN);

    h.checkpoint_as(ALICE).await;
    h.checkpoint_as(ALICE).await;
    assert_eq!(next_warnings(&admin).await, [usage(8192)]);
    assert!(admin.get("/v1/jobs").await.warnings().is_empty());
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(admin.get("/v1/jobs").await.warnings(), [usage(8192)]);
    assert!(admin.get("/v1/jobs").await.warnings().is_empty());

    // An administrator's own checkpoint: the owner warning carries the same
    // numbers, so no separate reminder comes with it.
    let own = h.checkpoint_as(ADMIN).await;
    // The admin's own requests above may have taken a reminder; wait until
    // another one would be due (which also lets the daemon queue the owner
    // warning after recording the image).
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(next_warnings(&admin).await, [over_limit(own, 12288)]);
    assert!(admin.get("/v1/jobs").await.warnings().is_empty());

    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_size_warnings_without_a_limit() {
    let mut h = Harness::new(1).await;
    h.set_default_parallel(8).await;
    let admin = h.client(ADMIN);
    h.checkpoint_as(ALICE).await;
    h.checkpoint_as(ALICE).await;
    assert!(admin.get("/v1/jobs").await.warnings().is_empty());
    assert!(h.client(ALICE).get("/v1/jobs").await.warnings().is_empty());
    let version: VersionResponse = admin.get("/v1/version").await.json();
    let storage = version.checkpoints.unwrap();
    assert_eq!((storage.images, storage.total_bytes), (2, 8192));
    assert_eq!(storage.limit_bytes, None);

    h.stop().await;
}

// ---------------------------------------------------------------------------
// Restore failures: every reason ends up in the owner's view, survives a
// daemon restart and is reported to the owner once.
// ---------------------------------------------------------------------------

impl Harness {
    /// The daemon's database, opened next to the running daemon.
    fn store(&self) -> puzuz_core::storage::Store {
        puzuz_core::storage::Store::open(self.dir.path().join("daemon/state.sqlite")).unwrap()
    }

    /// Runs a job of alice and checkpoints it.
    async fn checkpointed_job(&self) -> JobId {
        let admin = self.client(ADMIN);
        let id = self.client(ALICE).add(&job_request("train", &[])).await;
        admin.wait_state(id, &JobState::Running).await;
        admin.action(id, "checkpoint", json!(null)).await;
        admin.wait_state(id, &JobState::Checkpointed).await;
        id
    }
}

fn last_error(job: &JobView) -> Option<&str> {
    job.details.as_ref()?.last_error.as_deref()
}

/// Waits (polling as the admin, so the owner's warning stays pending) until
/// the job is checkpointed again with `reason`.
async fn wait_restore_error(h: &Harness, id: JobId, reason: &str) -> JobView {
    h.client(ADMIN)
        .wait_for(id, "checkpointed with the reason", |j| {
            j.state == JobState::Checkpointed && last_error(j) == Some(reason)
        })
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restore_failure_without_a_checkpoint_record_is_kept() {
    use puzuz_core::storage::{JobErrorKind, StoreOps};

    let mut h = Harness::new(0).await;
    let id = h.checkpointed_job().await;
    // The job has an image but the daemon lost its record of it.
    h.stop().await;
    assert!(h.store().delete_checkpoint(id).unwrap());
    h.start(0).await;
    let admin = h.client(ADMIN);

    let reason = "criu restore failed (unit inactive)";
    h.helper
        .set_restore_outcome(FakeOutcome::Fail(reason.to_owned()));
    let reply = admin
        .action(id, "restore", json!({"immediate": true}))
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    wait_restore_error(&h, id, reason).await;
    assert!(h.helper.has_image(id));

    // Stored durably with the job, visible to its owner only.
    let error = h.store().get_job_error(id).unwrap().unwrap();
    assert_eq!(
        (error.kind, error.message.as_str()),
        (JobErrorKind::Restore, reason)
    );
    assert!(h.client(BOB).job(id).await.details.is_none());
    let body = String::from_utf8(h.client(BOB).get("/v1/jobs").await.body.to_vec()).unwrap();
    assert!(!body.contains("unit inactive"), "leaked: {body}");

    // It survives a restart; the owner is warned once.
    h.stop().await;
    h.start(0).await;
    let alice = h.client(ALICE);
    assert!(h.client(BOB).get("/v1/jobs").await.warnings().is_empty());
    assert_eq!(
        alice.get("/v1/jobs").await.warnings(),
        [Warning::RestoreFailed { job: id }]
    );
    assert!(alice.get("/v1/jobs").await.warnings().is_empty());
    assert_eq!(last_error(&alice.job(id).await), Some(reason));

    // A successful restore clears it.
    h.helper.set_restore_outcome(FakeOutcome::Succeed);
    alice
        .action(id, "restore", json!({"immediate": true}))
        .await;
    let job = alice.wait_state(id, &JobState::Running).await;
    assert_eq!(last_error(&job), None);
    assert_eq!(h.store().get_job_error(id).unwrap(), None);

    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restore_refused_by_the_helper_keeps_the_reason() {
    let mut h = Harness::new(0).await;
    let id = h.checkpointed_job().await;
    let admin = h.client(ADMIN);

    // The helper's up-front checks (GPU compatibility, missing mounts, ...)
    // refuse the request itself.
    let reason = "checkpoint is not compatible with this host: [\"driver 570 != 580\"]";
    h.helper.set_restore_rejection(Some(reason));
    admin
        .action(id, "restore", json!({"immediate": true}))
        .await;
    let job = wait_restore_error(&h, id, reason).await;
    assert_eq!(job.checkpoint_size_bytes, Some(4096));
    assert_eq!(
        h.client(ALICE).get("/v1/groups").await.warnings(),
        [Warning::RestoreFailed { job: id }]
    );

    // The same failure again is not a new warning; a different one is.
    admin
        .action(id, "restore", json!({"immediate": true}))
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    wait_restore_error(&h, id, reason).await;
    assert!(
        h.client(ALICE)
            .get("/v1/groups")
            .await
            .warnings()
            .is_empty()
    );

    h.helper.set_restore_rejection(None);
    h.helper
        .set_restore_outcome(FakeOutcome::Fail("criu: Can't stat mountpoint".to_owned()));
    admin
        .action(id, "restore", json!({"immediate": true}))
        .await;
    wait_restore_error(&h, id, "criu: Can't stat mountpoint").await;
    assert_eq!(
        h.client(ALICE).get("/v1/groups").await.warnings(),
        [Warning::RestoreFailed { job: id }]
    );

    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_restore_is_recorded_and_refined_by_a_late_answer() {
    use puzuz_core::storage::StoreOps;

    let mut h = Harness::new(0).await;
    let id = h.checkpointed_job().await;
    // The daemon went away while restoring and the helper forgot the job.
    h.stop().await;
    h.store()
        .update_job_state(id, &JobState::Restoring, None, None)
        .unwrap();
    h.start(0).await;
    wait_restore_error(&h, id, "the restore was interrupted").await;

    // The helper's own reason, arriving late, replaces the guess.
    h.helper
        .emit_event(puzuz_protocol::helper::Event::RestoreFinished {
            job: id,
            outcome: puzuz_protocol::helper::RestoreOutcome::failed(
                "criu restore failed (unit inactive)",
            ),
        });
    wait_restore_error(&h, id, "criu restore failed (unit inactive)").await;
    assert_eq!(
        h.client(ALICE).get("/v1/jobs").await.warnings(),
        [Warning::RestoreFailed { job: id }]
    );

    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_failure_reason_is_the_owners_last_error() {
    let mut h = Harness::new(0).await;
    let alice = h.client(ALICE);
    h.helper
        .set_spawn_failure(Some("cwd /tmp/gone does not exist"));
    let id = alice.add(&job_request("x", &[])).await;
    let job = alice
        .wait_for(id, "failed to spawn", |j| {
            matches!(j.state, JobState::Done { .. })
        })
        .await;
    assert_eq!(last_error(&job), Some("cwd /tmp/gone does not exist"));
    assert!(h.client(BOB).job(id).await.details.is_none());

    h.stop().await;
}

// ---------------------------------------------------------------------------
// The launch window: the daemon records a job as Running (or Restoring)
// before the helper has answered its spawn (or restore) request. The fake
// holds those requests to make the window deterministic.
// ---------------------------------------------------------------------------

/// Sends `action` for `id` in the background.
fn send_action(
    client: &Client,
    id: JobId,
    action: &'static str,
    body: Value,
) -> tokio::task::JoinHandle<Reply> {
    let client = client.clone();
    tokio::spawn(async move { client.action(id, action, body).await })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actions_in_the_spawn_window_wait_for_the_helper() {
    let mut h = Harness::new(0).await;
    h.set_default_parallel(3).await;
    let alice = h.client(ALICE);
    h.helper.hold_launches();

    let checkpointed = alice.add(&job_request("a", &[])).await;
    let paused = alice.add(&job_request("b", &[])).await;
    let killed = alice.add(&job_request("c", &[])).await;
    for id in [checkpointed, paused, killed] {
        alice.wait_state(id, &JobState::Running).await;
    }
    h.helper.wait_held(3).await;

    let checkpoint = send_action(&alice, checkpointed, "checkpoint", json!(null));
    let pause = send_action(&alice, paused, "pause", json!({}));
    let kill = send_action(&alice, killed, "kill", json!({}));
    // Nothing reaches the helper (or the database) before the spawns are
    // answered.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!checkpoint.is_finished());
    assert!(!pause.is_finished());
    assert!(!kill.is_finished());
    for id in [checkpointed, paused, killed] {
        assert_eq!(alice.job(id).await.state, JobState::Running);
    }
    assert!(h.helper.signals().is_empty());

    h.helper.release_launches();
    for (reply, what) in [(checkpoint, "checkpoint"), (pause, "pause"), (kill, "kill")] {
        let reply = reply.await.unwrap();
        assert_eq!(
            reply.status,
            StatusCode::OK,
            "{what}: {}",
            String::from_utf8_lossy(&reply.body)
        );
    }
    let job = alice
        .wait_state(checkpointed, &JobState::Checkpointed)
        .await;
    assert_eq!(job.checkpoint_size_bytes, Some(4096));
    assert!(h.helper.has_image(checkpointed));
    alice.wait_state(paused, &JobState::Paused).await;
    assert_eq!(
        h.helper.jobs()[&paused].phase,
        puzuz_protocol::helper::RunningPhase::Stopped
    );
    alice.wait_state(killed, &done(JobResult::Killed)).await;
    assert!(!h.helper.is_running(killed));

    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawns_refused_by_a_stopping_helper_are_requeued() {
    let mut h = Harness::new(0).await;
    h.set_default_parallel(4).await;
    let alice = h.client(ALICE);
    h.helper.set_stopping(true);
    let mut ids = Vec::new();
    for i in 0..4 {
        ids.push(alice.add(&job_request(&format!("j{i}"), &[])).await);
    }
    // Refused and put back in line, not failed...
    tokio::time::sleep(Duration::from_millis(1500)).await;
    for &id in &ids {
        let state = alice.job(id).await.state;
        assert!(
            matches!(state, JobState::Queued { .. } | JobState::Running),
            "job {id}: {state:?}"
        );
    }
    // ...and offered again once the (next) helper accepts spawns.
    h.helper.set_stopping(false);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !ids.iter().all(|&id| h.helper.is_running(id)) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "jobs were not respawned"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    for &id in &ids {
        alice.wait_state(id, &JobState::Running).await;
    }
    h.stop().await;
}

/// e2e scenario 6 (helper restart) flake: a spawn refused by a stopping
/// helper left the job `Running` for another second although nothing ran,
/// so a check of the job's state right after the restart passed while the
/// spawn was still to be retried.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_spawn_refused_by_a_stopping_helper_is_queued_until_retried() {
    let mut h = Harness::new(0).await;
    h.set_default_parallel(2).await;
    let alice = h.client(ALICE);
    h.helper.hold_launches();
    h.helper.refuse_launches(2);
    let first = alice.add(&job_request("first", &[])).await;
    let second = alice.add(&job_request("second", &[])).await;
    let refused = tokio::time::Instant::now();

    // Back in line right away, not shown as running while nothing runs.
    for id in [first, second] {
        let deadline = refused + Duration::from_millis(500);
        loop {
            let state = alice.job(id).await.state;
            if matches!(state, JobState::Queued { .. }) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "job {id} is still {state:?} after the helper refused it"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    // Offered again only after a pause (not in a tight loop), then started.
    h.helper.wait_held(2).await;
    assert!(
        refused.elapsed() >= Duration::from_millis(900),
        "retried after {:?}",
        refused.elapsed()
    );
    h.helper.release_launches();
    for id in [first, second] {
        h.wait_spawned(id).await;
        alice.wait_state(id, &JobState::Running).await;
    }
    assert_eq!(h.helper.spawned().len(), 2);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawns_cut_off_by_a_helper_restart_are_resolved_not_lost() {
    let mut h = Harness::new(0).await;
    h.set_default_parallel(2).await;
    let alice = h.client(ALICE);
    h.helper.hold_launches();
    let started = alice.add(&job_request("started", &[])).await;
    let never = alice.add(&job_request("never", &[])).await;
    h.helper.wait_held(2).await;

    // The helper restarts while both requests are in flight: the answers
    // are lost. It had started one job (and re-discovers its unit); the
    // other request never reached it.
    h.helper.disconnect();
    h.helper.lose_held_launches();
    tokio::time::sleep(Duration::from_millis(100)).await;
    h.helper.insert_running(started);
    h.helper.reconnect();

    // The job it never started goes back in line and is started again.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !h.helper.is_running(never) {
        assert!(tokio::time::Instant::now() < deadline, "not respawned");
        let state = alice.job(never).await.state;
        assert!(
            matches!(state, JobState::Running | JobState::Queued { .. }),
            "{state:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        h.helper
            .spawned()
            .iter()
            .filter(|r| r.job == started)
            .count(),
        0,
        "the re-discovered job was spawned again"
    );
    tokio::time::sleep(Duration::from_millis(2500)).await;
    for id in [started, never] {
        assert_eq!(alice.job(id).await.state, JobState::Running, "job {id}");
    }
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_spawn_fails_the_waiting_action_cleanly() {
    let mut h = Harness::new(1).await;
    let alice = h.client(ALICE);
    h.helper.hold_launches();
    h.helper.set_spawn_failure(Some("no such directory"));

    let id = alice.add(&job_request("train", &[0])).await;
    alice.wait_state(id, &JobState::Running).await;
    h.helper.wait_held(1).await;
    let checkpoint = send_action(&alice, id, "checkpoint", json!(null));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!checkpoint.is_finished());

    h.helper.release_launches();
    let reply = checkpoint.await.unwrap();
    assert_eq!(reply.error().code, ErrorCode::InvalidState);
    let job = alice
        .wait_state(
            id,
            &done(JobResult::FailedToSpawn {
                reason: "no such directory".to_owned(),
            }),
        )
        .await;
    assert_eq!(job.checkpoint_size_bytes, None);
    assert!(h.helper.signals().is_empty());

    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restore_in_flight_rejects_checkpoint_and_kill_then_completes() {
    let mut h = Harness::new(1).await;
    let alice = h.client(ALICE);

    let id = alice.add(&job_request("train", &[0])).await;
    alice.wait_state(id, &JobState::Running).await;
    let reply = alice.action(id, "checkpoint", json!(null)).await;
    assert_eq!(reply.status, StatusCode::OK);
    alice.wait_state(id, &JobState::Checkpointed).await;

    h.helper.hold_launches();
    let restore = send_action(&alice, id, "restore", json!({"immediate": true}));
    h.helper.wait_held(1).await;
    alice.wait_state(id, &JobState::Restoring).await;
    // Rejected right away (nothing to signal or dump yet), not queued.
    for (action, body) in [
        ("checkpoint", json!(null)),
        ("kill", json!({})),
        ("pause", json!({})),
    ] {
        let reply = alice.action(id, action, body).await;
        assert_eq!(reply.error().code, ErrorCode::InvalidState, "{action}");
    }
    assert!(h.helper.signals().is_empty());

    h.helper.release_launches();
    assert_eq!(restore.await.unwrap().status, StatusCode::OK);
    alice.wait_state(id, &JobState::Running).await;
    assert!(h.helper.is_running(id));
    // Restored: checkpointing works again.
    let reply = alice.action(id, "checkpoint", json!(null)).await;
    assert_eq!(reply.status, StatusCode::OK);
    alice.wait_state(id, &JobState::Checkpointed).await;

    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_helper_reconnect_during_a_spawn_does_not_lose_the_job() {
    let mut h = Harness::new(0).await;
    let alice = h.client(ALICE);
    h.helper.hold_launches();

    let id = alice.add(&job_request("train", &[])).await;
    alice.wait_state(id, &JobState::Running).await;
    h.helper.wait_held(1).await;
    // The live-job list sent on reconnect predates the spawn.
    h.helper.disconnect();
    h.helper.reconnect();
    tokio::time::sleep(Duration::from_millis(200)).await;
    h.helper.release_launches();
    h.wait_spawned(id).await;
    // Past the one-second re-check, the job is still running.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(alice.job(id).await.state, JobState::Running);
    let reply = alice.action(id, "checkpoint", json!(null)).await;
    assert_eq!(reply.status, StatusCode::OK);
    alice.wait_state(id, &JobState::Checkpointed).await;

    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_restart_during_a_spawn_marks_the_job_lost() {
    let mut h = Harness::new(0).await;
    let alice = h.client(ALICE);
    h.helper.hold_launches();

    let id = alice.add(&job_request("train", &[])).await;
    alice.wait_state(id, &JobState::Running).await;
    h.helper.wait_held(1).await;
    // The daemon stops before the helper has answered; the request is
    // dropped unanswered, so the helper never starts the job.
    h.stop().await;
    h.helper.wait_held(0).await;
    h.helper.release_launches();
    assert!(h.helper.spawned().is_empty());

    h.start(0).await;
    let alice = h.client(ALICE);
    alice.wait_state(id, &done(JobResult::Lost)).await;
    assert!(!h.helper.is_running(id));

    h.stop().await;
}

// ---------------------------------------------------------------------------
// Launches the helper cannot carry out right now: restores abandoned by a
// stopping helper are requeued like spawns, and immediate launches wait for
// a disconnected helper instead of failing.
// ---------------------------------------------------------------------------

/// The owner has no pending warning and the job no recorded error.
async fn assert_no_restore_error(h: &Harness, id: JobId) {
    use puzuz_core::storage::StoreOps;

    assert_eq!(h.store().get_job_error(id).unwrap(), None);
    assert_eq!(last_error(&h.client(ADMIN).job(id).await), None);
    assert!(
        h.client(ALICE)
            .get("/v1/groups")
            .await
            .warnings()
            .is_empty()
    );
}

/// Polls `id` for `duration`, failing if it is ever plain checkpointed (a
/// failed restore) instead of waiting to be restored again.
async fn assert_never_checkpointed(client: &Client, id: JobId, duration: Duration) {
    let deadline = tokio::time::Instant::now() + duration;
    while tokio::time::Instant::now() < deadline {
        let state = client.job(id).await.state;
        assert!(
            matches!(state, JobState::RestoreQueued { .. } | JobState::Restoring),
            "job {id}: {state:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restores_refused_by_a_stopping_helper_are_requeued_not_failed() {
    let mut h = Harness::new(0).await;
    h.set_default_parallel(2).await;
    let first = h.checkpointed_job().await;
    let second = h.checkpointed_job().await;
    let admin = h.client(ADMIN);
    h.helper.set_stopping(true);

    // `restore --immediate`: put back in line (still ignoring slots), not
    // failed; the reply says the restore is pending.
    let reply = admin
        .action(first, "restore", json!({"immediate": true}))
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json::<Value>()["launch_pending"], json!(true));
    // A queued restore picked by the scheduler: the same.
    let reply = admin.action(second, "restore", json!({})).await;
    assert_eq!(reply.status, StatusCode::OK);
    for id in [first, second] {
        assert_never_checkpointed(&admin, id, Duration::from_millis(1500)).await;
        assert_no_restore_error(&h, id).await;
        assert!(h.helper.has_image(id));
    }

    // Restored once the (next) helper accepts restores.
    h.helper.set_stopping(false);
    for id in [first, second] {
        admin.wait_state(id, &JobState::Running).await;
        assert!(h.helper.is_running(id));
        assert_no_restore_error(&h, id).await;
    }
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restores_abandoned_by_a_stopping_helper_are_requeued_not_failed() {
    let mut h = Harness::new(0).await;
    let id = h.checkpointed_job().await;
    let admin = h.client(ADMIN);

    // Accepted, then abandoned before systemd started the unit; the helper
    // goes on stopping and refuses the retries.
    h.helper.set_abandon_restores(true);
    let reply = admin
        .action(id, "restore", json!({"immediate": true}))
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_never_checkpointed(&admin, id, Duration::from_millis(1500)).await;
    assert_no_restore_error(&h, id).await;

    // The helper restarts: the restore is retried and succeeds.
    h.helper.disconnect();
    h.helper.set_abandon_restores(false);
    h.helper.set_stopping(false);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_never_checkpointed(&admin, id, Duration::from_millis(200)).await;
    h.helper.reconnect();
    admin.wait_state(id, &JobState::Running).await;
    assert!(h.helper.is_running(id));
    assert_no_restore_error(&h, id).await;
    h.stop().await;
}

/// Holds an action whose spawn or restore request failed before it finds
/// out whether the launch is retried (see `Daemon::set_launch_check_hook`),
/// so that tests decide how far the retry gets first.
struct LaunchCheckGate(mpsc::UnboundedReceiver<(JobId, oneshot::Sender<()>)>);

impl LaunchCheckGate {
    /// Waits for an action to reach the check; it goes on once the
    /// returned sender is dropped.
    async fn reached(&mut self) -> (JobId, oneshot::Sender<()>) {
        tokio::time::timeout(Duration::from_secs(10), self.0.recv())
            .await
            .expect("no action reached the launch check")
            .unwrap()
    }
}

impl Harness {
    fn gate_launch_checks(&self) -> LaunchCheckGate {
        let (tx, rx) = mpsc::unbounded_channel();
        let hook: LaunchCheckHook = Arc::new(move |id| {
            let (release, released) = oneshot::channel::<()>();
            let _ = tx.send((id, release));
            Box::pin(async move {
                let _ = released.await;
            })
        });
        self.daemon
            .as_ref()
            .unwrap()
            .set_launch_check_hook(Some(hook));
        LaunchCheckGate(rx)
    }
}

fn launch_pending(reply: &Reply) -> bool {
    reply.json::<Value>()["launch_pending"] == json!(true)
}

/// Jobs for launching actions: a checkpointed one, a queued one (the only
/// slot is taken by a running job) and a finished one.
async fn launch_targets(h: &Harness) -> (JobId, JobId, JobId) {
    let alice = h.client(ALICE);
    h.set_default_parallel(2).await;
    let checkpointed = h.checkpointed_job().await;
    let blocker = alice.add(&job_request("blocker", &[])).await;
    alice.wait_state(blocker, &JobState::Running).await;
    let finished = alice.add(&job_request("finished", &[])).await;
    h.exit(finished, 0).await;
    alice.wait_state(finished, &done(JobResult::Success)).await;
    h.set_default_parallel(1).await;
    let queued = alice.add(&job_request("queued", &[])).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(matches!(
        alice.job(queued).await.state,
        JobState::Queued { .. }
    ));
    (checkpointed, queued, finished)
}

fn add_immediately(client: &Client, command: &str) -> tokio::task::JoinHandle<Reply> {
    let client = client.clone();
    let request = serde_json::to_value(AddJobRequest {
        immediate: true,
        ..job_request(command, &[])
    })
    .unwrap();
    tokio::spawn(async move { client.post("/v1/jobs", request).await })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn launches_retried_successfully_before_the_action_looks_are_reported_launched() {
    let mut h = Harness::new(0).await;
    let (checkpointed, queued, finished) = launch_targets(&h).await;
    let alice = h.client(ALICE);
    let mut gate = h.gate_launch_checks();

    // A stopping helper refuses the launch; the daemon puts the job back in
    // line and the next helper launches it before the action looks: the
    // action reports the launch, not an error nor a pending launch.
    for (id, action, body) in [
        (checkpointed, "restore", json!({"immediate": true})),
        (queued, "start", json!({})),
        (finished, "restart", json!({"immediate": true})),
    ] {
        h.helper.refuse_launches(1);
        let reply = send_action(&alice, id, action, body);
        let (launched, release) = gate.reached().await;
        alice.wait_state(launched, &JobState::Running).await;
        h.wait_spawned(launched).await;
        drop(release);
        let reply = reply.await.unwrap();
        assert_eq!(reply.status, StatusCode::OK, "{action} {id}");
        assert!(!launch_pending(&reply), "{action} {id}");
        if action == "restart" {
            assert_eq!(reply.json::<Value>()["new_id"], json!(launched.0));
        } else {
            assert_eq!(launched, id);
        }
        assert_no_restore_error(&h, id).await;
    }

    // `add --immediate` alike.
    h.helper.refuse_launches(1);
    let reply = add_immediately(&alice, "immediate");
    let (launched, release) = gate.reached().await;
    alice.wait_state(launched, &JobState::Running).await;
    h.wait_spawned(launched).await;
    drop(release);
    let reply = reply.await.unwrap();
    assert_eq!(reply.status, StatusCode::CREATED);
    let added: AddJobResponse = reply.json();
    assert_eq!(added.id, launched);
    assert!(!added.launch_pending);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn launches_still_being_retried_when_the_action_looks_are_pending() {
    let mut h = Harness::new(0).await;
    let (checkpointed, queued, _) = launch_targets(&h).await;
    let alice = h.client(ALICE);
    let mut gate = h.gate_launch_checks();

    // The retry is in flight (held by the helper) when the action looks.
    h.helper.hold_launches();
    for (held, (id, action, body)) in [
        (checkpointed, "restore", json!({"immediate": true})),
        (queued, "start", json!({})),
    ]
    .into_iter()
    .enumerate()
    {
        h.helper.refuse_launches(1);
        let reply = send_action(&alice, id, action, body);
        let (launched, release) = gate.reached().await;
        assert_eq!(launched, id);
        h.helper.wait_held(held + 1).await;
        drop(release);
        let reply = reply.await.unwrap();
        assert_eq!(reply.status, StatusCode::OK, "{action} {id}");
        assert!(launch_pending(&reply), "{action} {id}");
    }
    h.helper.refuse_launches(1);
    let reply = add_immediately(&alice, "immediate");
    let (immediate, release) = gate.reached().await;
    h.helper.wait_held(3).await;
    drop(release);
    let reply = reply.await.unwrap();
    assert_eq!(reply.status, StatusCode::CREATED);
    assert!(reply.json::<AddJobResponse>().launch_pending);
    h.helper.release_launches();
    for id in [checkpointed, queued, immediate] {
        alice.wait_state(id, &JobState::Running).await;
        h.wait_spawned(id).await;
    }
    assert_no_restore_error(&h, checkpointed).await;
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn launches_requeued_but_not_retried_yet_when_the_action_looks_are_pending() {
    let mut h = Harness::new(0).await;
    let id = h.checkpointed_job().await;
    let admin = h.client(ADMIN);
    let mut gate = h.gate_launch_checks();

    // The helper goes away before reading the request: the job is put back
    // in line and waits for the helper when the action looks.
    h.helper.hold_launches();
    let reply = send_action(&admin, id, "restore", json!({"immediate": true}));
    h.helper.wait_held(1).await;
    h.helper.disconnect();
    h.helper.release_launches();
    let (launched, release) = gate.reached().await;
    assert_eq!(launched, id);
    assert!(matches!(
        admin.job(id).await.state,
        JobState::RestoreQueued { .. }
    ));
    drop(release);
    let reply = reply.await.unwrap();
    assert_eq!(reply.status, StatusCode::OK);
    assert!(launch_pending(&reply));

    h.helper.reconnect();
    admin.wait_state(id, &JobState::Running).await;
    assert_no_restore_error(&h, id).await;
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn launches_that_fail_for_a_real_reason_report_it() {
    let mut h = Harness::new(0).await;
    let (checkpointed, queued, _) = launch_targets(&h).await;
    let admin = h.client(ADMIN);
    let mut gate = h.gate_launch_checks();
    let reason = "checkpoint is not compatible with this host";
    h.helper.set_restore_rejection(Some(reason));

    // Refused for a real reason right away: the error.
    let reply = send_action(&admin, checkpointed, "restore", json!({"immediate": true}));
    drop(gate.reached().await);
    let reply = reply.await.unwrap();
    assert_eq!(reply.error().code, ErrorCode::Internal);
    assert!(
        reply.error().message.contains(reason),
        "{}",
        reply.error().message
    );
    wait_restore_error(&h, checkpointed, reason).await;

    // Refused by a stopping helper, then the retry is refused for a real
    // reason before the action looks: that error, not a pending launch.
    let reason = "the GPU driver changed";
    h.helper.set_restore_rejection(Some(reason));
    h.helper.refuse_launches(1);
    let reply = send_action(&admin, checkpointed, "restore", json!({"immediate": true}));
    let (launched, release) = gate.reached().await;
    assert_eq!(launched, checkpointed);
    wait_restore_error(&h, checkpointed, reason).await;
    drop(release);
    let reply = reply.await.unwrap();
    assert_eq!(reply.error().code, ErrorCode::Internal);
    assert!(
        reply.error().message.contains(reason),
        "{}",
        reply.error().message
    );

    // A spawn the helper refuses for a real reason fails the job, and the
    // action succeeds, retried or not.
    h.helper.set_spawn_failure(Some("no such directory"));
    h.helper.refuse_launches(1);
    let reply = send_action(&admin, queued, "start", json!({}));
    let (launched, release) = gate.reached().await;
    assert_eq!(launched, queued);
    let failed = done(JobResult::FailedToSpawn {
        reason: "no such directory".to_owned(),
    });
    admin.wait_state(queued, &failed).await;
    drop(release);
    let reply = reply.await.unwrap();
    assert_eq!(reply.status, StatusCode::OK);
    assert!(!launch_pending(&reply));
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restore_cut_off_by_a_helper_restart_is_requeued_not_failed() {
    let mut h = Harness::new(0).await;
    let id = h.checkpointed_job().await;
    let admin = h.client(ADMIN);
    h.helper.hold_launches();
    let restore = send_action(&admin, id, "restore", json!({"immediate": true}));
    h.helper.wait_held(1).await;

    // The helper restarts before reading the request: its answer is lost
    // and the restarted helper knows nothing about it.
    h.helper.disconnect();
    h.helper.lose_held_launches();
    let reply = restore.await.unwrap();
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json::<Value>()["launch_pending"], json!(true));
    tokio::time::sleep(Duration::from_millis(100)).await;
    h.helper.reconnect();

    // Requeued (not marked interrupted) and restored again.
    let job = admin
        .wait_for(id, "restored", |j| {
            assert!(j.state != JobState::Checkpointed, "restore given up");
            j.state == JobState::Running
        })
        .await;
    assert_eq!(last_error(&job), None);
    assert!(h.helper.is_running(id));
    assert_no_restore_error(&h, id).await;
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn immediate_launches_wait_for_a_disconnected_helper() {
    let mut h = Harness::new(0).await;
    h.set_default_parallel(2).await;
    let alice = h.client(ALICE);
    let checkpointed = h.checkpointed_job().await;
    let blocker = alice.add(&job_request("blocker", &[])).await;
    alice.wait_state(blocker, &JobState::Running).await;
    let done_job = alice.add(&job_request("done", &[])).await;
    h.exit(done_job, 0).await;
    alice.wait_state(done_job, &done(JobResult::Success)).await;
    // Every slot is taken from now on.
    h.set_default_parallel(1).await;
    let queued = alice.add(&job_request("queued", &[])).await;
    let waiting = alice.add(&job_request("waiting", &[])).await;
    let stashed = alice
        .add(&AddJobRequest {
            stashed: true,
            ..job_request("stashed", &[])
        })
        .await;

    h.helper.disconnect();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // `add --immediate`: created, and pending.
    let reply = alice
        .post(
            "/v1/jobs",
            serde_json::to_value(AddJobRequest {
                immediate: true,
                ..job_request("immediate", &[])
            })
            .unwrap(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED);
    let added: AddJobResponse = reply.json();
    assert!(added.launch_pending);
    let immediate = added.id;

    // `start` of queued and stashed jobs, `restore --immediate`, `restart
    // --immediate`: pending too.
    let mut copy = None;
    for (id, action, body) in [
        (queued, "start", json!({})),
        (stashed, "start", json!({})),
        (checkpointed, "restore", json!({"immediate": true})),
        (done_job, "restart", json!({"immediate": true})),
    ] {
        let reply = alice.action(id, action, body).await;
        assert_eq!(reply.status, StatusCode::OK, "{action} {id}");
        let result: Value = reply.json();
        assert_eq!(result["launch_pending"], json!(true), "{action} {id}");
        if action == "restart" {
            copy = Some(JobId(result["new_id"].as_u64().unwrap()));
        }
    }
    let copy = copy.unwrap();
    // Actions on a live process still need the helper.
    let reply = alice.action(blocker, "pause", json!({})).await;
    assert_eq!(reply.error().code, ErrorCode::HelperUnavailable);

    tokio::time::sleep(Duration::from_millis(300)).await;
    for id in [immediate, queued, stashed, copy] {
        assert!(
            matches!(alice.job(id).await.state, JobState::Queued { .. }),
            "job {id}"
        );
    }
    assert!(matches!(
        alice.job(checkpointed).await.state,
        JobState::RestoreQueued { .. }
    ));

    // Launched as soon as the helper is back, ignoring the full slots;
    // the plain queued job keeps waiting.
    h.helper.reconnect();
    for id in [immediate, queued, stashed, copy, checkpointed] {
        alice.wait_state(id, &JobState::Running).await;
        h.wait_spawned(id).await;
    }
    alice.wait_state(blocker, &JobState::Running).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(matches!(
        alice.job(waiting).await.state,
        JobState::Queued { .. }
    ));
    h.stop().await;
}

/// A job the helper runs although the database has it finished (e.g. after
/// the database was restored from a backup) is an orphan: its GPU slots
/// stay taken and a queued job with its id is not started; administrators
/// see it in `status` and `doctor` and may kill it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orphans_keep_their_gpus_and_admins_can_kill_them() {
    let mut h = Harness::new(1).await;
    let alice = h.client(ALICE);
    let admin = h.client(ADMIN);
    let first = alice.add(&job_request("first", &[0])).await;
    h.exit(first, 0).await;
    alice.wait_state(first, &done(JobResult::Success)).await;

    h.stop().await;
    h.helper
        .insert_running_as(first, Uid(ALICE), &[GpuIndex(0)]);
    h.start(1).await;
    let mut orphans = Vec::new();
    for _ in 0..250 {
        let version: VersionResponse = admin.get("/v1/version").await.json();
        orphans = version.orphan_jobs.unwrap();
        if !orphans.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(orphans, [first]);

    // GPU 0 is taken by the orphan.
    let next = alice.add(&job_request("next", &[0])).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(matches!(
        alice.job(next).await.state,
        JobState::Queued { .. }
    ));
    let gpus: ListGpusResponse = alice.get("/v1/gpus").await.json();
    assert_eq!(gpus.gpus[0].used, 1);

    // Only administrators see it.
    let listed: ListJobsResponse = admin.get("/v1/jobs").await.json();
    assert_eq!(listed.orphans.len(), 1);
    assert_eq!(listed.orphans[0].id, first);
    assert_eq!(listed.orphans[0].owner, Some(Uid(ALICE)));
    assert_eq!(listed.orphans[0].gpus, [GpuIndex(0)]);
    let listed: ListJobsResponse = alice.get("/v1/jobs").await.json();
    assert!(listed.orphans.is_empty());

    // Its owner cannot kill it (the job is finished as far as the daemon
    // knows); an administrator can.
    let reply = alice.action(first, "kill", json!({})).await;
    assert_eq!(reply.error().code, ErrorCode::InvalidState);
    assert!(h.helper.is_running(first));
    let reply = admin.action(first, "kill", json!({})).await;
    assert_eq!(reply.status, StatusCode::OK);
    alice.wait_state(next, &JobState::Running).await;
    assert!(!h.helper.is_running(first));
    let version: VersionResponse = admin.get("/v1/version").await.json();
    assert_eq!(version.orphan_jobs, Some(Vec::new()));
    // The finished job is untouched.
    assert_eq!(alice.job(first).await.state, done(JobResult::Success));
    h.stop().await;
}

/// `switch` swaps the "launch regardless of slots" mark with the ids.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switch_swaps_the_forced_launch_mark() {
    let mut h = Harness::new(0).await;
    let alice = h.client(ALICE);
    let busy = alice.add(&job_request("busy", &[])).await;
    alice.wait_state(busy, &JobState::Running).await;
    let a = alice.add(&job_request("a", &[])).await;
    let b = alice.add(&job_request("b", &[])).await;

    // `start a` while the helper is away: a waits, to be launched
    // regardless of slots once it is back.
    h.helper.disconnect();
    let reply = alice.action(a, "start", json!({})).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(launch_pending(&reply));
    let reply = alice
        .post("/v1/jobs/switch", json!({"first": a, "second": b}))
        .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    assert_eq!(alice.job(b).await.details.unwrap().command, "a");

    h.helper.reconnect();
    // The job started with `start` (now `b`) is launched; the other waits
    // for the slot `busy` holds.
    alice.wait_state(b, &JobState::Running).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(matches!(alice.job(a).await.state, JobState::Queued { .. }));
    h.stop().await;
}

/// Connections and streams are limited per user: one over the limit is
/// answered with `too_many_requests`, and served again once others close.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connections_and_streams_are_limited_per_user() {
    const MAX_CONNECTIONS_PER_UID: usize = 64;
    const MAX_STREAMS_PER_UID: usize = 16;
    let mut h = Harness::new(0).await;
    let alice = h.client(ALICE);
    let bob = h.client(BOB);

    // Streams count per caller.
    let mut streams = Vec::new();
    for _ in 0..MAX_STREAMS_PER_UID {
        let response = alice.open(Method::GET, "/v1/events", None).await;
        assert_eq!(response.status(), StatusCode::OK);
        streams.push(response);
    }
    let reply = alice.get("/v1/events").await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(reply.error().code, ErrorCode::TooManyRequests);
    let id = alice.add(&job_request("x", &[])).await;
    let reply = alice.get(&format!("/v1/jobs/{id}/log?follow=true")).await;
    assert_eq!(reply.error().code, ErrorCode::TooManyRequests);
    // Others are not affected.
    let other = bob.open(Method::GET, "/v1/events", None).await;
    assert_eq!(other.status(), StatusCode::OK);
    drop(other);
    streams.pop();
    let mut reopened = None;
    for _ in 0..250 {
        let response = alice.open(Method::GET, "/v1/events", None).await;
        if response.status() == StatusCode::OK {
            reopened = Some(response);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(reopened.is_some(), "a closed stream frees its place");
    drop(reopened);
    drop(streams);

    // Connections count per peer uid (this process's, whatever the test
    // header says).
    let socket = h.dir.path().join("puzuz.sock");
    let mut idle = Vec::new();
    for _ in 0..MAX_CONNECTIONS_PER_UID {
        idle.push(tokio::net::UnixStream::connect(&socket).await.unwrap());
    }
    let mut refused = false;
    for _ in 0..250 {
        let reply = alice.get("/v1/version").await;
        if reply.status == StatusCode::TOO_MANY_REQUESTS {
            assert_eq!(reply.error().code, ErrorCode::TooManyRequests);
            refused = true;
            break;
        }
        // Connections of the streams above may still be closing.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(refused);
    drop(idle);
    let mut served = false;
    for _ in 0..250 {
        if alice.get("/v1/version").await.status == StatusCode::OK {
            served = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(served);
    h.stop().await;
}

// ---------------------------------------------------------------------------
// Priority and fair share
// ---------------------------------------------------------------------------

/// A daemon whose `[priority]` table is changed by `f`.
async fn harness_with_priority(
    gpus: u32,
    f: impl FnOnce(&mut puzuz_daemon::config::PriorityConfig),
) -> Harness {
    let mut h = Harness::new(gpus).await;
    h.stop().await;
    f(&mut h.priority);
    h.start(gpus).await;
    h
}

fn bulk_results(reply: &Reply) -> Vec<(u64, Option<ErrorCode>)> {
    assert_eq!(
        reply.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    reply
        .json::<BulkJobResponse>()
        .results
        .into_iter()
        .map(|r| {
            let code = match r.outcome {
                puzuz_protocol::api::ActionOutcome::Ok { .. } => None,
                puzuz_protocol::api::ActionOutcome::Error { error } => Some(error.code),
            };
            (r.id.0, code)
        })
        .collect()
}

/// The ids `GET /v1/priority` lists, in scheduling order.
async fn priority_order(client: &Client) -> Vec<u64> {
    let reply = client.get("/v1/priority").await;
    assert_eq!(reply.status, StatusCode::OK);
    let response: puzuz_protocol::api::PriorityResponse = reply.json();
    response.jobs.iter().map(|p| p.id.0).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lower_usage_user_starts_first() {
    let mut h = harness_with_priority(1, |p| p.priority_calc_period_secs = 1).await;
    let alice = h.client(ALICE);
    let bob = h.client(BOB);

    // Bob uses the only GPU for a while.
    let running = bob.add(&job_request("train", &[0])).await;
    bob.wait_state(running, &JobState::Running).await;
    // Both queue a GPU job; Bob's is older.
    let bobs = bob.add(&job_request("more", &[0])).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    let alices = alice.add(&job_request("first", &[0])).await;
    // Wait until Bob's usage has been recorded.
    let mut charged = false;
    for _ in 0..100 {
        let shares: puzuz_protocol::api::SharesResponse = alice.get("/v1/shares").await.json();
        if shares
            .users
            .iter()
            .any(|u| u.uid == Uid(BOB) && u.raw_usage > 0.0)
        {
            let a = shares.users.iter().find(|u| u.uid == Uid(ALICE)).unwrap();
            let b = shares.users.iter().find(|u| u.uid == Uid(BOB)).unwrap();
            assert!(a.fairshare > b.fairshare, "{shares:?}");
            assert!(a.active && b.active);
            charged = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(charged, "bob was never charged");
    assert_eq!(priority_order(&alice).await, [alices.0, bobs.0]);

    h.exit(running, 0).await;
    alice.wait_state(alices, &JobState::Running).await;
    assert!(matches!(bob.job(bobs).await.state, JobState::Queued { .. }));

    // Usage survives a restart.
    h.stop().await;
    h.start(1).await;
    let shares: puzuz_protocol::api::SharesResponse =
        h.client(ADMIN).get("/v1/shares").await.json();
    let b = shares.users.iter().find(|u| u.uid == Uid(BOB)).unwrap();
    assert!(b.raw_usage > 0.0, "{shares:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn basic_priority_keeps_pueues_order() {
    let h = harness_with_priority(0, |p| {
        p.priority_type = puzuz_daemon::config::PriorityType::Basic;
    })
    .await;
    let alice = h.client(ALICE);
    let bob = h.client(BOB);
    let blocker = alice.add(&job_request("block", &[])).await;
    alice.wait_state(blocker, &JobState::Running).await;
    let a = alice.add(&job_request("a", &[])).await;
    let mut request = job_request("b", &[]);
    request.spec.priority = 5;
    let b = bob.add(&request).await;
    // Bob's -o 5 wins across users, as in pueue.
    assert_eq!(priority_order(&alice).await, [b.0, a.0]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn o_priority_and_top_order_only_the_owners_jobs() {
    let h = Harness::new(0).await;
    let alice = h.client(ALICE);
    let bob = h.client(BOB);
    let blocker = alice.add(&job_request("block", &[])).await;
    alice.wait_state(blocker, &JobState::Running).await;

    let a1 = alice.add(&job_request("a1", &[])).await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    let b1 = bob.add(&job_request("b1", &[])).await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    let mut request = job_request("a2", &[]);
    request.spec.priority = 100;
    let a2 = alice.add(&request).await;
    let a3 = alice.add(&job_request("a3", &[])).await;
    // a2 takes a1's place (alice's oldest); bob's job is not overtaken by
    // more than alice's own waiting time.
    assert_eq!(priority_order(&alice).await, [a2.0, b1.0, a1.0, a3.0]);

    // `top` moves a3 to the front of alice's queue.
    let reply = alice.post("/v1/jobs/top", json!({ "ids": [a3.0] })).await;
    assert_eq!(bulk_results(&reply), [(a3.0, None)]);
    assert_eq!(priority_order(&alice).await, [a3.0, b1.0, a2.0, a1.0]);
    // Not bob's.
    let reply = bob
        .post("/v1/jobs/top", json!({ "ids": [a1.0, b1.0] }))
        .await;
    assert_eq!(
        bulk_results(&reply),
        [(a1.0, Some(ErrorCode::PermissionDenied)), (b1.0, None)]
    );
    // An administrator may move anyone's job; unknown jobs fail alone.
    let reply = h
        .client(ADMIN)
        .post("/v1/jobs/top", json!({ "ids": [a1.0, 999] }))
        .await;
    assert_eq!(
        bulk_results(&reply),
        [(a1.0, None), (999, Some(ErrorCode::JobNotFound))]
    );
    assert_eq!(priority_order(&alice).await, [a1.0, b1.0, a3.0, a2.0]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn top_can_be_disabled_for_users() {
    let h = harness_with_priority(0, |p| p.enable_user_top = false).await;
    let alice = h.client(ALICE);
    let id = alice
        .add(&AddJobRequest {
            stashed: true,
            ..job_request("x", &[])
        })
        .await;
    let reply = alice.post("/v1/jobs/top", json!({ "ids": [id.0] })).await;
    assert_eq!(
        bulk_results(&reply),
        [(id.0, Some(ErrorCode::PermissionDenied))]
    );
    let reply = h
        .client(ADMIN)
        .post("/v1/jobs/top", json!({ "ids": [id.0] }))
        .await;
    assert_eq!(bulk_results(&reply), [(id.0, None)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nice_site_shares_and_group_priority_authorization() {
    let h = Harness::new(0).await;
    let alice = h.client(ALICE);
    let bob = h.client(BOB);
    let admin = h.client(ADMIN);
    let stashed = |command: &str| AddJobRequest {
        stashed: true,
        ..job_request(command, &[])
    };

    // Negative nice at submission is for administrators.
    let mut request = stashed("x");
    request.spec.nice = -1;
    let reply = alice
        .post("/v1/jobs", serde_json::to_value(&request).unwrap())
        .await;
    assert_eq!(reply.error().code, ErrorCode::PermissionDenied);
    request.spec.nice = 10;
    let id = alice.add(&request).await;
    assert_eq!(alice.job(id).await.nice, 10);

    // Users may raise the nice value of their own jobs only.
    let reply = alice
        .post("/v1/jobs/nice", json!({ "ids": [id.0], "nice": 20 }))
        .await;
    assert_eq!(bulk_results(&reply), [(id.0, None)]);
    let reply = alice
        .post("/v1/jobs/nice", json!({ "ids": [id.0], "nice": -5 }))
        .await;
    let results = bulk_results(&reply);
    assert_eq!(results, [(id.0, Some(ErrorCode::PermissionDenied))]);
    let reply = bob
        .post("/v1/jobs/nice", json!({ "ids": [id.0], "nice": 5 }))
        .await;
    assert_eq!(
        bulk_results(&reply),
        [(id.0, Some(ErrorCode::PermissionDenied))]
    );
    let reply = admin
        .post("/v1/jobs/nice", json!({ "ids": [id.0], "nice": -5 }))
        .await;
    assert_eq!(bulk_results(&reply), [(id.0, None)]);
    assert_eq!(alice.job(id).await.nice, -5);

    // Site factors are for administrators.
    let reply = alice
        .post("/v1/jobs/site", json!({ "ids": [id.0], "site": 100 }))
        .await;
    assert_eq!(reply.error().code, ErrorCode::PermissionDenied);
    let reply = admin
        .post("/v1/jobs/site", json!({ "ids": [id.0], "site": 100 }))
        .await;
    assert_eq!(bulk_results(&reply), [(id.0, None)]);

    // So are shares and group priorities.
    let reply = alice
        .call(
            Method::PUT,
            &format!("/v1/shares/{ALICE}"),
            Some(json!({ "shares": 100 })),
        )
        .await;
    assert_eq!(reply.error().code, ErrorCode::PermissionDenied);
    let reply = admin
        .call(
            Method::PUT,
            &format!("/v1/shares/{BOB}"),
            Some(json!({ "shares": 3 })),
        )
        .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    let shares: puzuz_protocol::api::SharesResponse = alice.get("/v1/shares").await.json();
    let b = shares.users.iter().find(|u| u.uid == Uid(BOB)).unwrap();
    assert_eq!(b.shares, 3);
    assert!(!b.active);

    let reply = alice
        .call(
            Method::PUT,
            "/v1/groups/default/priority",
            Some(json!({ "priority": 5 })),
        )
        .await;
    assert_eq!(reply.error().code, ErrorCode::PermissionDenied);
    let reply = admin
        .call(
            Method::PUT,
            "/v1/groups/default/priority",
            Some(json!({ "priority": 5 })),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    let group: puzuz_core::model::Group = reply.json();
    assert_eq!(group.priority, 5);
    let reply = admin
        .call(
            Method::PUT,
            "/v1/groups/nope/priority",
            Some(json!({ "priority": 5 })),
        )
        .await;
    assert_eq!(reply.error().code, ErrorCode::GroupNotFound);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn priority_view_hides_commands_and_shows_factors() {
    let h = Harness::new(0).await;
    let alice = h.client(ALICE);
    let bob = h.client(BOB);
    let blocker = alice.add(&job_request("block", &[])).await;
    alice.wait_state(blocker, &JobState::Running).await;
    let mut request = job_request("secret-command", &[]);
    request.spec.nice = 7;
    let id = alice.add(&request).await;

    let reply = bob.get("/v1/priority").await;
    assert_eq!(reply.status, StatusCode::OK);
    let text = String::from_utf8_lossy(&reply.body);
    assert!(!text.contains("secret-command"), "{text}");
    assert!(!text.contains("hunter2"), "{text}");
    let response: puzuz_protocol::api::PriorityResponse = reply.json();
    assert_eq!(
        response.priority_type,
        puzuz_daemon::config::PriorityType::Multifactor
    );
    assert_eq!(response.weights.fairshare, 10000);
    // Only pending jobs are listed.
    assert_eq!(response.jobs.len(), 1);
    let p = &response.jobs[0];
    assert_eq!((p.id, p.owner, p.nice), (id, Uid(ALICE), 7));
    assert_eq!(p.factors.fairshare, 1.0);
    assert!((p.priority - (p.weighted.age + 10000.0 - 7.0)).abs() < 1e-6);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restarted_copy_does_not_inherit_a_negative_nice() {
    let h = Harness::new(0).await;
    let alice = h.client(ALICE);
    let id = alice.add(&job_request("x", &[])).await;
    alice.wait_state(id, &JobState::Running).await;
    let reply = h
        .client(ADMIN)
        .post("/v1/jobs/nice", json!({ "ids": [id.0], "nice": -5 }))
        .await;
    assert_eq!(bulk_results(&reply), [(id.0, None)]);
    h.exit(id, 0).await;
    alice.wait_state(id, &done(JobResult::Success)).await;
    // Finished jobs cannot be reniced.
    let reply = alice
        .post("/v1/jobs/nice", json!({ "ids": [id.0], "nice": 1 }))
        .await;
    assert_eq!(
        bulk_results(&reply),
        [(id.0, Some(ErrorCode::InvalidState))]
    );
    let reply = alice
        .action(id, "restart", json!({ "stashed": true }))
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    let result: puzuz_protocol::api::JobActionResult = reply.json();
    let puzuz_protocol::api::ActionOutcome::Ok {
        new_id: Some(new_id),
        ..
    } = result.outcome
    else {
        panic!("no new job: {result:?}");
    };
    assert_eq!(alice.job(new_id).await.nice, 0);
}

/// `uid`'s raw usage as `GET /v1/shares` shows it (0 if not listed).
async fn raw_usage(client: &Client, uid: u32) -> f64 {
    let shares: puzuz_protocol::api::SharesResponse = client.get("/v1/shares").await.json();
    shares
        .users
        .iter()
        .find(|u| u.uid == Uid(uid))
        .map_or(0.0, |u| u.raw_usage)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_running_time_is_billed() {
    // The default calculation period (5 min) never passes here: all usage
    // comes from jobs leaving the running state.
    let mut h = Harness::new(0).await;
    let alice = h.client(ALICE);
    let bob = h.client(BOB);
    let id = alice.add(&job_request("train", &[])).await;
    alice.wait_state(id, &JobState::Running).await;
    h.wait_spawned(id).await;
    tokio::time::sleep(Duration::from_millis(600)).await;

    // Pausing charges the run so far (CPU-only: billing 1 per second).
    assert_eq!(
        alice.action(id, "pause", json!({})).await.status,
        StatusCode::OK
    );
    alice.wait_state(id, &JobState::Paused).await;
    let paused = raw_usage(&alice, ALICE).await;
    assert!((0.5..2.0).contains(&paused), "{paused}");
    // Paused time is not billed.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(raw_usage(&alice, ALICE).await, paused);

    // Resumed, then checkpointed: the second run is charged.
    assert_eq!(
        alice.action(id, "start", json!(null)).await.status,
        StatusCode::OK
    );
    alice.wait_state(id, &JobState::Running).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        alice.action(id, "checkpoint", json!(null)).await.status,
        StatusCode::OK
    );
    alice.wait_state(id, &JobState::Checkpointed).await;
    let checkpointed = raw_usage(&alice, ALICE).await;
    assert!(
        (0.3..1.5).contains(&(checkpointed - paused)),
        "{paused} -> {checkpointed}"
    );

    // Waiting checkpointed, waiting to be restored and being removed while
    // waiting cost nothing, although the job's started_at is old.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let other = bob.add(&job_request("other", &[])).await;
    bob.wait_state(other, &JobState::Running).await;
    assert_eq!(
        alice.action(id, "restore", json!({})).await.status,
        StatusCode::OK
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(matches!(
        alice.job(id).await.state,
        JobState::RestoreQueued { .. }
    ));
    // (Killing it is refused: it does not run; it can only be removed.)
    assert_eq!(
        alice.action(id, "kill", json!({})).await.status,
        StatusCode::CONFLICT
    );
    let reply = alice
        .call(Method::DELETE, &format!("/v1/jobs/{id}"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(raw_usage(&alice, ALICE).await, checkpointed);
    h.exit(other, 0).await;
    bob.wait_state(other, &done(JobResult::Success)).await;
    assert!(raw_usage(&bob, BOB).await > 0.0);

    // A restored job is billed from the restore on, before any calculation.
    let id = alice.add(&job_request("again", &[])).await;
    alice.wait_state(id, &JobState::Running).await;
    alice.action(id, "checkpoint", json!(null)).await;
    alice.wait_state(id, &JobState::Checkpointed).await;
    let before = raw_usage(&alice, ALICE).await;
    alice.action(id, "restore", json!({})).await;
    alice.wait_state(id, &JobState::Running).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    h.exit(id, 0).await;
    alice.wait_state(id, &done(JobResult::Success)).await;
    let after = raw_usage(&alice, ALICE).await;
    assert!(
        (0.3..1.5).contains(&(after - before)),
        "{before} -> {after}"
    );

    // Usage and run records survive a restart.
    h.stop().await;
    h.start(0).await;
    assert!(raw_usage(&h.client(ADMIN), ALICE).await >= after * 0.99);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn users_may_only_raise_nice_values() {
    let h = Harness::new(0).await;
    let alice = h.client(ALICE);
    let id = alice
        .add(&AddJobRequest {
            stashed: true,
            ..job_request("x", &[])
        })
        .await;
    let reply = h
        .client(ADMIN)
        .post("/v1/jobs/nice", json!({ "ids": [id.0], "nice": 10000 }))
        .await;
    assert_eq!(bulk_results(&reply), [(id.0, None)]);
    let reply = alice
        .post("/v1/jobs/nice", json!({ "ids": [id.0], "nice": 0 }))
        .await;
    let response: BulkJobResponse = reply.json();
    let puzuz_protocol::api::ActionOutcome::Error { error } = &response.results[0].outcome else {
        panic!("{response:?}");
    };
    assert_eq!(error.code, ErrorCode::PermissionDenied);
    assert!(error.message.contains("10000"), "{}", error.message);
    let reply = alice
        .post("/v1/jobs/nice", json!({ "ids": [id.0], "nice": 20000 }))
        .await;
    assert_eq!(bulk_results(&reply), [(id.0, None)]);
    assert_eq!(alice.job(id).await.nice, 20000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_in_place_drops_administrators_grants() {
    let h = Harness::new(0).await;
    let alice = h.client(ALICE);
    let admin = h.client(ADMIN);
    let id = alice.add(&job_request("x", &[])).await;
    alice.wait_state(id, &JobState::Running).await;
    for (path, body) in [
        ("/v1/jobs/nice", json!({ "ids": [id.0], "nice": -5 })),
        ("/v1/jobs/site", json!({ "ids": [id.0], "site": 3 })),
        ("/v1/jobs/top", json!({ "ids": [id.0] })),
    ] {
        assert_eq!(bulk_results(&admin.post(path, body).await), [(id.0, None)]);
    }
    h.exit(id, 0).await;
    alice.wait_state(id, &done(JobResult::Success)).await;
    // Keep it queued: the default group runs one job at a time.
    let blocker = alice.add(&job_request("block", &[])).await;
    alice.wait_state(blocker, &JobState::Running).await;
    let reply = alice
        .action(id, "restart", json!({ "in_place": true }))
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(alice.job(id).await.nice, 0);
    let response: puzuz_protocol::api::PriorityResponse = alice.get("/v1/priority").await.json();
    let p = response.jobs.iter().find(|p| p.id == id).unwrap();
    assert_eq!((p.nice, p.site), (0, 0));
}
