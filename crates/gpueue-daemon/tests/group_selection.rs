//! `pause`/`start`/`kill` with a group (`-g`) or all-jobs (`-a`) selection:
//! administrators pause/resume whole groups and act on every job; other
//! users act on their own jobs only and never change a group's paused flag.
//!
//! The daemon runs in-process with the in-memory `FakeHelper`, driven over
//! HTTP on its Unix socket.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpueue_core::model::{JobId, JobResult, JobSpec, JobState};
use gpueue_daemon::api::TEST_UID_HEADER;
use gpueue_daemon::{
    Config, Daemon, DaemonOptions, FakeHelper, StaticGpuDetector, StaticUserDb, SystemClock,
};
use gpueue_protocol::BINARY_VERSION;
use gpueue_protocol::api::{
    AddJobRequest, AddJobResponse, BulkJobResponse, JobView, ListGroupsResponse, VERSION_HEADER,
};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};

const ALICE: u32 = 1000;
const BOB: u32 = 1001;
const ADMIN: u32 = 500;

struct Client {
    socket: PathBuf,
    uid: u32,
}

impl Client {
    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Bytes) {
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
            .header(VERSION_HEADER, BINARY_VERSION)
            .header(TEST_UID_HEADER, self.uid.to_string())
            .body(Full::new(Bytes::from(body)))
            .unwrap();
        let response = sender.send_request(request).await.unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, body)
    }

    async fn json<T: serde::de::DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        expected: StatusCode,
    ) -> T {
        let (status, body) = self.call(method, path, body).await;
        assert_eq!(status, expected, "{}", String::from_utf8_lossy(&body));
        serde_json::from_slice(&body).unwrap()
    }

    async fn add(&self, group: &str) -> JobId {
        let request = AddJobRequest {
            spec: JobSpec {
                command: "train".to_owned(),
                cwd: PathBuf::from("/tmp"),
                envs: BTreeMap::new(),
                group: group.to_owned(),
                gpus: Vec::new(),
                dependencies: Vec::new(),
                priority: 0,
                label: None,
                no_checkpoint: false,
                nice: 0,
            },
            stashed: false,
            enqueue_at: None,
            immediate: false,
        };
        let body = serde_json::to_value(&request).unwrap();
        let reply: AddJobResponse = self
            .json(Method::POST, "/v1/jobs", Some(body), StatusCode::CREATED)
            .await;
        reply.id
    }

    async fn bulk(&self, target: Value, action: &str, options: Value) -> BulkJobResponse {
        let mut body = json!({ "target": target, "action": action });
        if let (Some(body), Some(options)) = (body.as_object_mut(), options.as_object()) {
            body.extend(options.clone());
        }
        self.json(Method::POST, "/v1/jobs/bulk", Some(body), StatusCode::OK)
            .await
    }

    async fn job(&self, id: JobId) -> JobView {
        self.json(Method::GET, &format!("/v1/jobs/{id}"), None, StatusCode::OK)
            .await
    }

    /// Group name to paused flag.
    async fn groups(&self) -> HashMap<String, bool> {
        let reply: ListGroupsResponse = self
            .json(Method::GET, "/v1/groups", None, StatusCode::OK)
            .await;
        reply
            .groups
            .into_iter()
            .map(|g| (g.name, g.paused))
            .collect()
    }

    /// Polls until the job is in `state` (5 s at most).
    async fn wait_state(&self, id: JobId, state: &JobState) {
        for _ in 0..250 {
            if self.job(id).await.state == *state {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!(
            "job {id} never became {state:?}: {:?}",
            self.job(id).await.state
        );
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    socket: PathBuf,
    daemon: Option<Daemon>,
}

impl Harness {
    /// A daemon with groups `default` and `gpu`, both running 4 jobs at a
    /// time.
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("gpueue.sock");
        let config = Config {
            api_socket: socket.clone(),
            grace_secs: 1,
            ..Config::default()
        }
        .with_data_dir(dir.path());
        let daemon = Daemon::start(DaemonOptions {
            config,
            helper: Arc::new(FakeHelper::new()),
            users: Arc::new(StaticUserDb::with_admins([ADMIN])),
            gpus: Arc::new(StaticGpuDetector::anonymous(0)),
            clock: Arc::new(SystemClock),
            trust_uid_header: true,
        })
        .await
        .unwrap();
        let h = Self {
            _dir: dir,
            socket,
            daemon: Some(daemon),
        };
        let admin = h.client(ADMIN);
        let _: Value = admin
            .json(
                Method::POST,
                "/v1/groups",
                Some(json!({"name": "gpu", "parallel": 4})),
                StatusCode::CREATED,
            )
            .await;
        let _: Value = admin
            .json(
                Method::PUT,
                "/v1/groups/default/parallel",
                Some(json!({"parallel": 4})),
                StatusCode::OK,
            )
            .await;
        h
    }

    /// Adds a job to a new group `probe` and waits until it runs: the
    /// scheduling pass that started it saw every job added before it.
    async fn probe_scheduling(&self) -> JobId {
        let admin = self.client(ADMIN);
        let _: Value = admin
            .json(
                Method::POST,
                "/v1/groups",
                Some(json!({"name": "probe", "parallel": 1})),
                StatusCode::CREATED,
            )
            .await;
        let probe = admin.add("probe").await;
        admin.wait_state(probe, &JobState::Running).await;
        probe
    }

    /// Kills and removes the job of [`Harness::probe_scheduling`] and its
    /// group.
    async fn remove_probe(&self, probe: JobId) {
        let admin = self.client(ADMIN);
        let (status, body) = admin
            .call(
                Method::POST,
                &format!("/v1/jobs/{probe}/kill"),
                Some(json!({})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let killed = JobState::Done {
            result: gpueue_core::model::JobResult::Killed,
        };
        admin.wait_state(probe, &killed).await;
        let (status, body) = admin
            .call(Method::DELETE, &format!("/v1/jobs/{probe}"), None)
            .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let (status, body) = admin.call(Method::DELETE, "/v1/groups/probe", None).await;
        assert_eq!(
            status,
            StatusCode::NO_CONTENT,
            "{}",
            String::from_utf8_lossy(&body)
        );
    }

    fn client(&self, uid: u32) -> Client {
        Client {
            socket: self.socket.clone(),
            uid,
        }
    }

    /// Running jobs: alice's in `default` and `gpu`, bob's in `default`.
    async fn running_jobs(&self) -> Jobs {
        let alice = self.client(ALICE);
        let bob = self.client(BOB);
        let jobs = Jobs {
            alice_default: alice.add("default").await,
            alice_gpu: alice.add("gpu").await,
            bob_default: bob.add("default").await,
        };
        for id in jobs.all() {
            alice.wait_state(id, &JobState::Running).await;
        }
        jobs
    }

    async fn stop(mut self) {
        if let Some(daemon) = self.daemon.take() {
            daemon.stop().await;
        }
    }
}

struct Jobs {
    alice_default: JobId,
    alice_gpu: JobId,
    bob_default: JobId,
}

impl Jobs {
    fn all(&self) -> [JobId; 3] {
        [self.alice_default, self.alice_gpu, self.bob_default]
    }
}

fn ids(reply: &BulkJobResponse) -> Vec<JobId> {
    assert!(reply.all_ok(), "{reply:?}");
    let mut ids: Vec<JobId> = reply.results.iter().map(|r| r.id).collect();
    ids.sort();
    ids
}

fn groups(pairs: &[(&str, bool)]) -> HashMap<String, bool> {
    pairs.iter().map(|(n, p)| (n.to_string(), *p)).collect()
}

fn sorted(mut names: Vec<String>) -> Vec<String> {
    names.sort();
    names
}

fn all() -> Value {
    json!({"kind": "all"})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn normal_user_pause_and_start_all_touch_only_own_jobs() {
    let h = Harness::new().await;
    let jobs = h.running_jobs().await;
    let alice = h.client(ALICE);
    let unpaused = groups(&[("default", false), ("gpu", false)]);

    let reply = alice.bulk(all(), "pause", json!({})).await;
    assert_eq!(ids(&reply), [jobs.alice_default, jobs.alice_gpu]);
    assert!(reply.groups.is_empty());
    alice
        .wait_state(jobs.alice_default, &JobState::Paused)
        .await;
    alice.wait_state(jobs.alice_gpu, &JobState::Paused).await;
    assert_eq!(alice.job(jobs.bob_default).await.state, JobState::Running);
    assert_eq!(alice.groups().await, unpaused);

    // New jobs still start: no group was paused.
    let queued = h.client(BOB).add("gpu").await;
    alice.wait_state(queued, &JobState::Running).await;

    let reply = alice.bulk(all(), "start", json!(null)).await;
    assert_eq!(ids(&reply), [jobs.alice_default, jobs.alice_gpu]);
    assert!(reply.groups.is_empty());
    alice
        .wait_state(jobs.alice_default, &JobState::Running)
        .await;
    alice.wait_state(jobs.alice_gpu, &JobState::Running).await;
    assert_eq!(alice.groups().await, unpaused);

    // `--wait` only pauses groups, so it does nothing for a normal user.
    let reply = alice.bulk(all(), "pause", json!({"wait": true})).await;
    assert!(reply.results.is_empty() && reply.groups.is_empty());
    assert_eq!(alice.groups().await, unpaused);
    assert_eq!(alice.job(jobs.alice_gpu).await.state, JobState::Running);

    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_pause_and_start_all_change_every_group_and_job() {
    let h = Harness::new().await;
    let jobs = h.running_jobs().await;
    let admin = h.client(ADMIN);
    assert_eq!(
        admin.groups().await,
        groups(&[("default", false), ("gpu", false)])
    );

    let reply = admin.bulk(all(), "pause", json!({})).await;
    assert_eq!(ids(&reply), jobs.all());
    assert_eq!(sorted(reply.groups), ["default", "gpu"]);
    for id in jobs.all() {
        admin.wait_state(id, &JobState::Paused).await;
    }
    assert_eq!(
        admin.groups().await,
        groups(&[("default", true), ("gpu", true)])
    );

    // Paused groups start nothing new. A job added after it to a group
    // that is not paused shows when a scheduling pass has seen both.
    let queued = h.client(ALICE).add("gpu").await;
    let probe = h.probe_scheduling().await;
    assert!(matches!(
        admin.job(queued).await.state,
        JobState::Queued { .. }
    ));
    h.remove_probe(probe).await;

    let reply = admin.bulk(all(), "start", json!(null)).await;
    // Only paused jobs are resumed; the queued one starts with its group.
    assert_eq!(ids(&reply), jobs.all());
    assert_eq!(sorted(reply.groups), ["default", "gpu"]);
    for id in jobs.all().into_iter().chain([queued]) {
        admin.wait_state(id, &JobState::Running).await;
    }
    assert_eq!(
        admin.groups().await,
        groups(&[("default", false), ("gpu", false)])
    );

    // `--wait` pauses all groups but lets running jobs run.
    let reply = admin.bulk(all(), "pause", json!({"wait": true})).await;
    assert!(reply.results.is_empty());
    assert_eq!(sorted(reply.groups), ["default", "gpu"]);
    assert_eq!(
        admin.groups().await,
        groups(&[("default", true), ("gpu", true)])
    );
    for id in jobs.all() {
        assert_eq!(admin.job(id).await.state, JobState::Running);
    }

    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_all_pauses_groups_only_for_admins() {
    let h = Harness::new().await;
    let jobs = h.running_jobs().await;
    let alice = h.client(ALICE);
    let admin = h.client(ADMIN);
    let killed = JobState::Done {
        result: JobResult::Killed,
    };

    let reply = alice.bulk(all(), "kill", json!({})).await;
    assert_eq!(ids(&reply), [jobs.alice_default, jobs.alice_gpu]);
    assert!(reply.groups.is_empty());
    alice.wait_state(jobs.alice_default, &killed).await;
    alice.wait_state(jobs.alice_gpu, &killed).await;
    assert_eq!(alice.job(jobs.bob_default).await.state, JobState::Running);
    assert_eq!(
        alice.groups().await,
        groups(&[("default", false), ("gpu", false)])
    );

    // As in pueue, an administrator's `kill -a` also pauses all groups.
    let reply = admin.bulk(all(), "kill", json!({})).await;
    assert_eq!(ids(&reply), [jobs.bob_default]);
    assert_eq!(sorted(reply.groups), ["default", "gpu"]);
    admin.wait_state(jobs.bob_default, &killed).await;
    assert_eq!(
        admin.groups().await,
        groups(&[("default", true), ("gpu", true)])
    );

    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn group_selection_pauses_the_group_only_for_admins() {
    let h = Harness::new().await;
    let jobs = h.running_jobs().await;
    let alice = h.client(ALICE);
    let admin = h.client(ADMIN);
    let default = json!({"kind": "group", "group": "default"});

    let reply = alice.bulk(default.clone(), "pause", json!({})).await;
    assert_eq!(ids(&reply), [jobs.alice_default]);
    assert!(reply.groups.is_empty());
    alice
        .wait_state(jobs.alice_default, &JobState::Paused)
        .await;
    assert_eq!(alice.job(jobs.bob_default).await.state, JobState::Running);
    assert_eq!(alice.job(jobs.alice_gpu).await.state, JobState::Running);
    assert_eq!(
        alice.groups().await,
        groups(&[("default", false), ("gpu", false)])
    );

    let reply = admin.bulk(default.clone(), "pause", json!({})).await;
    assert_eq!(ids(&reply), [jobs.bob_default]);
    assert_eq!(reply.groups, ["default"]);
    admin.wait_state(jobs.bob_default, &JobState::Paused).await;
    assert_eq!(admin.job(jobs.alice_gpu).await.state, JobState::Running);
    assert_eq!(
        admin.groups().await,
        groups(&[("default", true), ("gpu", false)])
    );

    // A normal user's `start -g` does not resume the group.
    let reply = alice.bulk(default.clone(), "start", json!(null)).await;
    assert_eq!(ids(&reply), [jobs.alice_default]);
    assert!(reply.groups.is_empty());
    assert_eq!(
        alice.groups().await,
        groups(&[("default", true), ("gpu", false)])
    );

    let reply = admin.bulk(default, "start", json!(null)).await;
    assert_eq!(ids(&reply), [jobs.bob_default]);
    assert_eq!(reply.groups, ["default"]);
    assert_eq!(
        admin.groups().await,
        groups(&[("default", false), ("gpu", false)])
    );

    h.stop().await;
}
