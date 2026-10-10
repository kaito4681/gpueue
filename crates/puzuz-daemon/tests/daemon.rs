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
use puzuz_core::model::{GpuIndex, JobId, JobResult, JobSpec, JobState};
use puzuz_daemon::api::TEST_UID_HEADER;
use puzuz_daemon::helper::FakeOutcome;
use puzuz_daemon::{
    Config, Daemon, DaemonOptions, DetectedGpu, FakeHelper, GpuDetector, StaticGpuDetector,
    StaticUserDb,
};
use puzuz_protocol::BINARY_VERSION;
use puzuz_protocol::api::{
    AddJobRequest, AddJobResponse, ApiError, BulkJobResponse, CleanResponse, ErrorCode, JobView,
    ListGpusResponse, ListJobsResponse, VERSION_HEADER, VersionResponse, WARNINGS_HEADER, Warning,
};
use puzuz_protocol::helper::signal;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

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

    // Logs: raw bytes, tail, and only for the owner.
    let log = h.dir.path().join(format!("logs/{ALICE}/{id}.log"));
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, "one\ntwo\nthree\n").unwrap();
    let reply = alice.get(&format!("/v1/jobs/{id}/log")).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(&reply.body[..], b"one\ntwo\nthree\n");
    let reply = alice.get(&format!("/v1/jobs/{id}/log?tail=2")).await;
    assert_eq!(&reply.body[..], b"two\nthree\n");
    let reply = alice.get(&format!("/v1/jobs/{id}/log?follow=true")).await;
    assert_eq!(
        &reply.body[..],
        b"one\ntwo\nthree\n",
        "follow ends with the job"
    );
    let reply = h.client(BOB).get(&format!("/v1/jobs/{id}/log")).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(reply.error().code, ErrorCode::PermissionDenied);
    let reply = h
        .client(ADMIN)
        .get(&format!("/v1/jobs/{id}/log?tail=1"))
        .await;
    assert_eq!(&reply.body[..], b"three\n");

    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn log_follow_streams_appended_output() {
    let mut h = Harness::new(0).await;
    let alice = h.client(ALICE);
    let id = alice.add(&job_request("train", &[])).await;
    alice.wait_state(id, &JobState::Running).await;
    let log = h.dir.path().join(format!("logs/{ALICE}/{id}.log"));
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

    // Bob watches the event stream.
    let response = bob.open(Method::GET, "/v1/events", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut events = response.into_body();

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

    // Bob's events carry only redacted views.
    let mut text = String::new();
    while !text.contains("event: job_added") || !text.contains("\"result\":\"killed\"") {
        let frame = tokio::time::timeout(Duration::from_secs(5), events.frame())
            .await
            .expect("an event")
            .unwrap()
            .unwrap();
        if let Ok(data) = frame.into_data() {
            text.push_str(&String::from_utf8_lossy(&data));
        }
    }
    assert!(!text.contains("secret-command"), "{text}");
    assert!(!text.contains("\"details\""), "{text}");

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
    let puzuz_protocol::api::ActionOutcome::Ok { new_id: Some(copy) } = result.outcome else {
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
            outcome: puzuz_protocol::helper::RestoreOutcome::Failed {
                reason: "criu restore failed (unit inactive)".to_owned(),
            },
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
