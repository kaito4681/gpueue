//! The helper over its real socket, with a fake launcher, host and
//! checkpoint executor. Runs unprivileged: the "daemon" is this process's
//! own uid, and jobs are owned by it too.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use puzuz_checkpoint::layout::{CheckpointDir, DirStatus, FAILED_LOGS_SUBDIR};
use puzuz_checkpoint::plan::Action;
use puzuz_checkpoint::{CudaMode, Executor};
use puzuz_helper::access::VerifyRequest;
use puzuz_helper::config::SHUTDOWN_MANIFEST;
use puzuz_helper::engine::{ManifestEntry, ManifestResult};
use puzuz_helper::host::DumpHazards;
use puzuz_helper::launcher::fake::FakeLauncher;
use puzuz_helper::launcher::{Launcher, UnitState};
use puzuz_helper::testing::{FakeExecState, FakeExecutor, FakeHost};
use puzuz_helper::{ExecutorFactory, Helper, HelperConfig, ShutdownManifest, server};
use puzuz_protocol::helper::{
    CheckpointOptions, CheckpointOutcome, DaemonMessage, Event, EventEnvelope, EventSeq,
    ExitStatus, Gid, GpuIndex, Hello, HelperErrorCode, HelperMessage, JobId, PROTOCOL_VERSION,
    Request, RequestEnvelope, RequestId, ResponseResult, RestoreOutcome, RestoreRequest,
    RunningPhase, SpawnRequest, Uid, decode_line, encode_line, signal,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::watch;

type TestHelper = Helper<Arc<FakeLauncher>, Arc<FakeHost>>;

fn uid() -> u32 {
    let uid = nix::unistd::getuid().as_raw();
    assert_ne!(uid, 0, "run these tests as an unprivileged user");
    uid
}

fn gid() -> u32 {
    nix::unistd::getgid().as_raw()
}

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    launcher: Arc<FakeLauncher>,
    host: Arc<FakeHost>,
    exec: Arc<Mutex<FakeExecState>>,
    helper: Arc<TestHelper>,
    stop: watch::Sender<bool>,
    /// `cuda_checkpoint_mode` of the helper (taken by [`Env::restart`]).
    mode: CudaMode,
}

impl Env {
    async fn new() -> Self {
        Self::with_daemon_uid(uid()).await
    }

    async fn with_daemon_uid(daemon_uid: u32) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_owned();
        let launcher = Arc::new(FakeLauncher::new());
        let exec = Arc::new(Mutex::new(FakeExecState::default()));
        let host = Arc::new(FakeHost::default());
        host.add_user(uid(), "tester", gid(), &[gid()]);
        host.set_mount_points(vec!["/".into(), "/proc".into(), "/tmp".into()]);
        let (helper, stop) = start(
            &root,
            daemon_uid,
            launcher.clone(),
            host.clone(),
            exec.clone(),
            CudaMode::Manual,
        )
        .await;
        Self {
            _tmp: tmp,
            root,
            launcher,
            host,
            exec,
            helper,
            stop,
            mode: CudaMode::Manual,
        }
    }

    /// Simulate a helper restart on the same state and units.
    async fn restart(&mut self) {
        self.restart_with(with_mode(config(&self.root, uid()), &self.root, self.mode))
            .await;
    }

    /// [`Self::restart`] with another configuration.
    async fn restart_with(&mut self, cfg: HelperConfig) {
        let _ = self.stop.send(true);
        let (helper, stop) = start_with(
            cfg,
            self.launcher.clone(),
            self.host.clone(),
            self.exec.clone(),
        )
        .await;
        self.helper = helper;
        self.stop = stop;
    }

    fn socket(&self) -> PathBuf {
        self.root.join("helper.sock")
    }

    fn base(&self) -> PathBuf {
        self.root.join("checkpoints")
    }

    async fn client(&self) -> Client {
        Client::connect(&self.socket(), Hello::current()).await
    }

    fn spawn_req(&self, job: u64) -> SpawnRequest {
        SpawnRequest {
            job: JobId(job),
            uid: Uid(uid()),
            gid: Gid(gid()),
            groups: None,
            cwd: self.root.clone(),
            command: "python train.py".into(),
            shell: vec!["sh".into(), "-c".into()],
            envs: BTreeMap::from([("PATH".into(), "/usr/bin".into())]),
            gpus: vec![GpuIndex(0)],
            log_path: self
                .root
                .join("logs")
                .join(uid().to_string())
                .join(format!("{job}.log")),
            no_checkpoint: false,
        }
    }
}

fn config(root: &Path, daemon_uid: u32) -> HelperConfig {
    HelperConfig {
        socket_path: root.join("helper.sock"),
        daemon_uid,
        daemon_gid: gid(),
        state_dir: root.join("state"),
        checkpoint_base: root.join("checkpoints"),
        log_base: root.join("logs"),
        criu_root: None,
        poll_interval: Duration::from_millis(20),
        ..HelperConfig::default()
    }
}

async fn start(
    root: &Path,
    daemon_uid: u32,
    launcher: Arc<FakeLauncher>,
    host: Arc<FakeHost>,
    exec: Arc<Mutex<FakeExecState>>,
    mode: CudaMode,
) -> (Arc<TestHelper>, watch::Sender<bool>) {
    let cfg = with_mode(config(root, daemon_uid), root, mode);
    start_with(cfg, launcher, host, exec).await
}

fn with_mode(mut cfg: HelperConfig, root: &Path, mode: CudaMode) -> HelperConfig {
    cfg.checkpoint.mode = mode;
    // CRIU's plugins, for the plugin mode (empty unless a test adds one).
    cfg.checkpoint.criu.plugin_dir = Some(root.join("criu-plugins"));
    cfg
}

async fn start_with(
    cfg: HelperConfig,
    launcher: Arc<FakeLauncher>,
    host: Arc<FakeHost>,
    exec: Arc<Mutex<FakeExecState>>,
) -> (Arc<TestHelper>, watch::Sender<bool>) {
    let factory: ExecutorFactory =
        Arc::new(move || Box::new(FakeExecutor(exec.clone())) as Box<dyn Executor + Send>);
    let socket = cfg.socket_path.clone();
    let helper = Helper::new(cfg, launcher, host, factory).unwrap();
    helper.recover().await.unwrap();
    // Owned by us so that a test can connect as a "wrong" uid; the helper
    // still checks the peer against `daemon_uid`.
    let listener = server::bind(&socket, uid()).unwrap();
    let (stop, stop_rx) = watch::channel(false);
    tokio::spawn(server::serve(helper.clone(), listener, stop_rx.clone()));
    tokio::spawn(helper.clone().monitor(stop_rx));
    (helper, stop)
}

struct Client {
    rd: BufReader<OwnedReadHalf>,
    wr: OwnedWriteHalf,
    next_id: u64,
    events: Vec<EventEnvelope>,
    /// Responses read while waiting for another one.
    responses: BTreeMap<RequestId, ResponseResult>,
}

const WAIT: Duration = Duration::from_secs(5);

impl Client {
    async fn connect(path: &Path, hello: Hello) -> Self {
        let (rd, wr) = UnixStream::connect(path).await.unwrap().into_split();
        let mut c = Self {
            rd: BufReader::new(rd),
            wr,
            next_id: 1,
            events: Vec::new(),
            responses: BTreeMap::new(),
        };
        c.send(&DaemonMessage::Hello(hello)).await;
        match c.read().await {
            Some(HelperMessage::Hello(h)) => assert_eq!(h.protocol_version, PROTOCOL_VERSION),
            other => panic!("expected hello, got {other:?}"),
        }
        c
    }

    async fn send(&mut self, msg: &DaemonMessage) {
        self.wr.write_all(&encode_line(msg).unwrap()).await.unwrap();
    }

    /// The next message, or `None` at EOF.
    async fn read(&mut self) -> Option<HelperMessage> {
        let mut line = String::new();
        let n = tokio::time::timeout(WAIT, self.rd.read_line(&mut line))
            .await
            .expect("timed out waiting for the helper")
            .unwrap_or(0);
        (n > 0).then(|| decode_line(line.as_bytes()).unwrap())
    }

    async fn request(&mut self, request: Request) -> ResponseResult {
        let id = self.send_request(request).await;
        self.response(id).await
    }

    /// Sends a request without waiting for its response.
    async fn send_request(&mut self, request: Request) -> RequestId {
        let id = RequestId(self.next_id);
        self.next_id += 1;
        self.send(&DaemonMessage::Request(RequestEnvelope { id, request }))
            .await;
        id
    }

    /// The response to request `id`.
    async fn response(&mut self, id: RequestId) -> ResponseResult {
        if let Some(r) = self.responses.remove(&id) {
            return r;
        }
        loop {
            match self.read().await.expect("connection closed") {
                HelperMessage::Response(r) if r.id == id => return r.result,
                HelperMessage::Response(r) => {
                    self.responses.insert(r.id, r.result);
                }
                HelperMessage::Event(e) => self.events.push(e),
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    /// Sends every request before reading any response; the responses in
    /// request order.
    async fn request_all(&mut self, requests: Vec<Request>) -> Vec<ResponseResult> {
        let mut ids = Vec::new();
        for r in requests {
            ids.push(self.send_request(r).await);
        }
        let mut out = Vec::new();
        for id in ids {
            out.push(self.response(id).await);
        }
        out
    }

    async fn event(&mut self) -> EventEnvelope {
        if !self.events.is_empty() {
            return self.events.remove(0);
        }
        match self.read().await.expect("connection closed") {
            HelperMessage::Event(e) => e,
            other => panic!("expected an event, got {other:?}"),
        }
    }

    /// Asserts that no event arrives within a short time.
    async fn no_event(&mut self) {
        assert!(self.events.is_empty(), "unexpected {:?}", self.events);
        let mut line = String::new();
        let r =
            tokio::time::timeout(Duration::from_millis(200), self.rd.read_line(&mut line)).await;
        assert!(r.is_err(), "unexpected message {line}");
    }

    async fn running(&mut self) -> Vec<(JobId, RunningPhase)> {
        match self.request(Request::ListRunning).await {
            ResponseResult::Running { jobs } => {
                jobs.into_iter().map(|j| (j.job, j.phase)).collect()
            }
            other => panic!("{other:?}"),
        }
    }
}

fn err_code(r: &ResponseResult) -> HelperErrorCode {
    match r {
        ResponseResult::Error { code, .. } => *code,
        other => panic!("expected an error, got {other:?}"),
    }
}

#[tokio::test]
async fn rejects_other_uids_without_a_word() {
    let env = Env::with_daemon_uid(uid() + 1).await;
    let stream = UnixStream::connect(env.socket()).await.unwrap();
    let (rd, mut wr) = stream.into_split();
    let _ = wr
        .write_all(&encode_line(&DaemonMessage::Hello(Hello::current())).unwrap())
        .await;
    let mut line = String::new();
    let n = tokio::time::timeout(WAIT, BufReader::new(rd).read_line(&mut line))
        .await
        .unwrap()
        .unwrap_or(0);
    assert_eq!(n, 0, "got {line}");
}

#[tokio::test]
async fn version_mismatch_gets_our_hello_then_eof() {
    let env = Env::new().await;
    let stream = UnixStream::connect(env.socket()).await.unwrap();
    let (rd, mut wr) = stream.into_split();
    let theirs = Hello {
        protocol_version: PROTOCOL_VERSION + 1,
        binary_version: "9.9.9".into(),
    };
    wr.write_all(&encode_line(&DaemonMessage::Hello(theirs)).unwrap())
        .await
        .unwrap();
    let mut rd = BufReader::new(rd);
    let mut line = String::new();
    rd.read_line(&mut line).await.unwrap();
    let msg: HelperMessage = decode_line(line.as_bytes()).unwrap();
    assert!(matches!(msg, HelperMessage::Hello(h) if h.protocol_version == PROTOCOL_VERSION));
    line.clear();
    let n = tokio::time::timeout(WAIT, rd.read_line(&mut line))
        .await
        .unwrap()
        .unwrap_or(0);
    assert_eq!(n, 0);
}

#[tokio::test]
async fn unknown_requests_get_an_error_and_the_session_goes_on() {
    let env = Env::new().await;
    let mut c = env.client().await;
    c.wr.write_all(b"{\"type\":\"request\",\"id\":41,\"op\":\"teleport\",\"job\":1}\n")
        .await
        .unwrap();
    assert_eq!(
        err_code(&c.response(RequestId(41)).await),
        HelperErrorCode::InvalidRequest
    );
    assert_eq!(c.running().await, vec![]);
}

#[tokio::test]
async fn a_new_session_replaces_the_old_one() {
    let env = Env::new().await;
    let mut old = env.client().await;
    assert_eq!(old.running().await, vec![]);
    let mut new = env.client().await;
    // The old connection is closed by the helper.
    let mut line = String::new();
    let n = tokio::time::timeout(WAIT, old.rd.read_line(&mut line))
        .await
        .expect("the old session was not closed")
        .unwrap_or(0);
    assert_eq!(n, 0, "got {line}");
    assert_eq!(new.running().await, vec![]);
}

#[tokio::test]
async fn connections_without_a_hello_are_capped() {
    let env = Env::new().await;
    // Connections that never send their Hello hold a handshake slot each.
    let mut idle = Vec::new();
    for _ in 0..server::MAX_HANDSHAKES {
        idle.push(UnixStream::connect(env.socket()).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    // One more is closed at once.
    let extra = UnixStream::connect(env.socket()).await.unwrap();
    let (rd, _wr) = extra.into_split();
    let mut line = String::new();
    let n = tokio::time::timeout(WAIT, BufReader::new(rd).read_line(&mut line))
        .await
        .expect("the extra connection was kept")
        .unwrap_or(0);
    assert_eq!(n, 0);
    // Once they are gone, the daemon gets in again.
    drop(idle);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut c = env.client().await;
    assert_eq!(c.running().await, vec![]);
}

#[tokio::test]
async fn spawn_validates_and_sets_cuda_env() {
    let env = Env::new().await;
    let mut c = env.client().await;

    let mut root = env.spawn_req(1);
    root.uid = Uid(0);
    assert_eq!(
        err_code(&c.request(Request::Spawn(root)).await),
        HelperErrorCode::InvalidRequest
    );
    let mut foreign = env.spawn_req(1);
    foreign.groups = Some(vec![Gid(gid() + 12345)]);
    assert_eq!(
        err_code(&c.request(Request::Spawn(foreign)).await),
        HelperErrorCode::PermissionDenied
    );

    let r = c.request(Request::Spawn(env.spawn_req(1))).await;
    assert!(matches!(r, ResponseResult::Spawned { .. }), "{r:?}");
    assert_eq!(
        err_code(&c.request(Request::Spawn(env.spawn_req(1))).await),
        HelperErrorCode::Busy
    );
    let spawned = env.launcher.spawned();
    assert_eq!(spawned.len(), 1);
    assert_eq!(spawned[0].envs["CUDA_VISIBLE_DEVICES"], "0");
    assert_eq!(spawned[0].envs["CUDA_DEVICE_ORDER"], "PCI_BUS_ID");
    assert_eq!(spawned[0].argv, ["sh", "-c", "python train.py"]);
    // The shim input (with the environment) is gone once the job runs.
    assert!(!spawned[0].files.spec().exists());
    // The per-user log directory was created, group-writable for the daemon.
    let log_dir = env.root.join("logs").join(uid().to_string());
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(&log_dir).unwrap().permissions(),
    );
    assert_eq!(mode & 0o7777, 0o2770);
    assert_eq!(c.running().await, [(JobId(1), RunningPhase::Running)]);

    // Signals are forwarded and STOP/CONT tracked.
    assert_eq!(
        c.request(Request::Signal {
            job: JobId(1),
            signal: signal::SIGSTOP
        })
        .await,
        ResponseResult::Ok
    );
    assert_eq!(c.running().await, [(JobId(1), RunningPhase::Stopped)]);
    assert_eq!(
        err_code(
            &c.request(Request::Signal {
                job: JobId(2),
                signal: signal::SIGTERM
            })
            .await
        ),
        HelperErrorCode::JobNotFound
    );
    assert_eq!(
        env.launcher.unit(JobId(1)).unwrap().signals,
        [signal::SIGSTOP]
    );
}

#[tokio::test]
async fn exit_events_are_persisted_replayed_and_acked() {
    let mut env = Env::new().await;
    let mut c = env.client().await;
    assert!(matches!(
        c.request(Request::Spawn(env.spawn_req(1))).await,
        ResponseResult::Spawned { .. }
    ));
    env.launcher.exit(JobId(1), ExitStatus::Exited { code: 3 });
    let ev = c.event().await;
    assert_eq!(ev.seq, EventSeq(1));
    assert_eq!(
        ev.event,
        Event::JobExited {
            job: JobId(1),
            status: ExitStatus::Exited { code: 3 }
        }
    );
    assert!(c.running().await.is_empty());
    assert!(env.launcher.unit(JobId(1)).is_none(), "unit released");
    drop(c);

    // Not acknowledged: replayed on the next connection, before the answer
    // to the first request (the daemon reconciles with list_running).
    let mut c = env.client().await;
    assert!(c.running().await.is_empty());
    assert_eq!(c.events.len(), 1, "replay must precede the response");
    assert_eq!(c.event().await.seq, EventSeq(1));
    assert_eq!(
        c.request(Request::AckEvents { up_to: EventSeq(1) }).await,
        ResponseResult::Ok
    );
    drop(c);
    let mut c = env.client().await;
    c.no_event().await;
    drop(c);

    // Sequence numbers keep increasing across a helper restart, and the
    // exit status file written by the job's init wins over systemd's view.
    env.restart().await;
    let mut c = env.client().await;
    let spawned = c.request(Request::Spawn(env.spawn_req(2))).await;
    assert!(matches!(spawned, ResponseResult::Spawned { .. }));
    let files = env.launcher.spawned().last().unwrap().files.clone();
    std::fs::write(files.exit_status(), br#"{"kind":"signaled","signal":15}"#).unwrap();
    let ev = c.event().await;
    assert_eq!(ev.seq, EventSeq(2));
    assert_eq!(
        ev.event,
        Event::JobExited {
            job: JobId(2),
            status: ExitStatus::Signaled { signal: 15 }
        }
    );
}

#[tokio::test]
async fn recovery_readopts_running_jobs_and_reports_exits() {
    let mut env = Env::new().await;
    let mut c = env.client().await;
    for job in [7, 8] {
        assert!(matches!(
            c.request(Request::Spawn(env.spawn_req(job))).await,
            ResponseResult::Spawned { .. }
        ));
    }
    drop(c);
    // The helper goes away; job 8 exits while it is gone.
    let _ = env.stop.send(true);
    env.launcher.exit(JobId(8), ExitStatus::Exited { code: 0 });
    env.restart().await;

    let mut c = env.client().await;
    let ev = c.event().await;
    assert_eq!(
        ev.event,
        Event::JobExited {
            job: JobId(8),
            status: ExitStatus::Exited { code: 0 }
        }
    );
    assert_eq!(c.running().await, [(JobId(7), RunningPhase::Running)]);
}

#[tokio::test]
async fn checkpoint_then_restore() {
    let env = Env::new().await;
    env.helper.config();
    let mut c = env.client().await;
    let job = JobId(3);
    assert!(matches!(
        c.request(Request::Spawn(env.spawn_req(3))).await,
        ResponseResult::Spawned { .. }
    ));

    // The image directory is fixed by the helper.
    assert_eq!(
        err_code(
            &c.request(Request::Checkpoint {
                job,
                image_dir: "/tmp/elsewhere".into(),
                options: Default::default(),
            })
            .await
        ),
        HelperErrorCode::InvalidRequest
    );

    // Manual mode: the job's init (host PID 10001, ns PID 1) uses CUDA.
    env.host.set_cuda_pids(vec![10_001]);
    let image_dir = env.base().join("3");
    let r = c
        .request(Request::Checkpoint {
            job,
            image_dir: image_dir.clone(),
            options: Default::default(),
        })
        .await;
    assert_eq!(r, ResponseResult::Accepted);
    let ev = c.event().await;
    let Event::CheckpointFinished {
        outcome: CheckpointOutcome::Success { image_size_bytes },
        ..
    } = ev.event
    else {
        panic!("{ev:?}")
    };
    assert!(image_size_bytes > 0);
    let dir = CheckpointDir::for_job(&env.base(), 3);
    let DirStatus::Complete(meta) = dir.status() else {
        panic!("{:?}", dir.status())
    };
    assert_eq!(meta.mode, CudaMode::Manual);
    assert_eq!(meta.cuda_ns_pids, [1]);
    assert!(c.running().await.is_empty());
    assert!(env.launcher.unit(job).is_none(), "dumped unit released");
    assert_eq!(
        std::mem::take(&mut env.exec.lock().unwrap().actions),
        [
            None,
            Some(Action::CudaLock { pid: 10_001 }),
            Some(Action::CudaCheckpoint { pid: 10_001 }),
            Some(Action::CriuDump { root_pid: 10_001 }),
        ]
    );
    // No job_exited for a dumped job.
    c.no_event().await;

    // A checkpointed job cannot be spawned again.
    assert_eq!(
        err_code(&c.request(Request::Spawn(env.spawn_req(3))).await),
        HelperErrorCode::Busy
    );

    let restore = |uid: u32| {
        Request::Restore(RestoreRequest {
            job,
            uid: Uid(uid),
            image_dir: image_dir.clone(),
            gpus: vec![GpuIndex(0)],
        })
    };
    assert_eq!(
        err_code(&c.request(restore(uid() + 1)).await),
        HelperErrorCode::PermissionDenied
    );
    // A mount point of the job's namespace that no longer exists is
    // reported before criu would fail on it.
    let mounts = dir.root().join(puzuz_helper::engine::MOUNTS_FILE);
    let saved = std::fs::read(&mounts).unwrap();
    std::fs::write(&mounts, br#"["/", "/run/gone-after-reboot"]"#).unwrap();
    assert_eq!(
        err_code(&c.request(restore(uid())).await),
        HelperErrorCode::InvalidRequest
    );
    // So is the host path of an external bind mount (e.g. one that was
    // hidden under another mount at dump time).
    std::fs::write(
        &mounts,
        br#"{"mount_points": ["/"], "external_sources": ["/run/nvidia-persistenced/socket-gone"]}"#,
    )
    .unwrap();
    assert_eq!(
        err_code(&c.request(restore(uid())).await),
        HelperErrorCode::InvalidRequest
    );
    std::fs::write(&mounts, saved).unwrap();
    assert_eq!(c.request(restore(uid())).await, ResponseResult::Accepted);
    let ev = c.event().await;
    let Event::RestoreFinished {
        outcome: RestoreOutcome::Success { pid },
        ..
    } = ev.event
    else {
        panic!("{ev:?}")
    };
    assert_eq!(c.running().await, [(job, RunningPhase::Running)]);
    assert_eq!(env.launcher.unit(job).unwrap().pid, pid);
    assert_eq!(
        dir.status(),
        DirStatus::Missing,
        "image removed after restore"
    );
    let restored = env.launcher.restored();
    assert_eq!(restored[0].unit, "puzuz-job-3-r1.service");
    assert!(restored[0].command.has_arg("--manage-cgroups=ignore"));
    // The CUDA state is restored into the new host PID of ns PID 1.
    assert_eq!(
        env.exec.lock().unwrap().actions,
        [
            None,
            Some(Action::CudaRestore { pid }),
            Some(Action::CudaUnlock { pid }),
        ]
    );
    assert!(restored[0].command.has_arg("--restore-detached"));
    assert_eq!(restored[0].pidfile, dir.restore_pidfile());
    // criu checks the restored tasks before it resumes them.
    let script = restored[0].command.value_of("--action-script").unwrap();
    let request = env.root.join("state/jobs/3/restore-check.json");
    assert_eq!(script, env.root.join("state/jobs/3/restore-check.sh"));
    let body = std::fs::read_to_string(script).unwrap();
    assert!(
        body.starts_with("#!/bin/sh\nexec '")
            && body.ends_with(&format!(" verify-restore-hook '{}'\n", request.display())),
        "{body}"
    );
    let check: puzuz_helper::access::RestoredCheck =
        serde_json::from_slice(&std::fs::read(&request).unwrap()).unwrap();
    assert_eq!(check.owner_uid, uid());
    assert!(check.allowed_gids.contains(&gid()));
    assert_eq!(check.private_tmpfs, ["/tmp", "/dev/shm"]);
    assert_eq!(check.exempt.len(), 1, "the init's exit-status file");
    assert!(env.launcher.unit(job).unwrap().signals.is_empty());

    // Once it exits, it is reported like any other job.
    env.launcher.exit(job, ExitStatus::Exited { code: 0 });
    assert!(matches!(c.event().await.event, Event::JobExited { .. }));
}

/// Spawns job `job` and checkpoints it successfully; the image directory.
async fn spawn_and_dump(env: &Env, c: &mut Client, job: u64) -> PathBuf {
    assert!(matches!(
        c.request(Request::Spawn(env.spawn_req(job))).await,
        ResponseResult::Spawned { .. }
    ));
    let image_dir = env.base().join(job.to_string());
    let r = c
        .request(Request::Checkpoint {
            job: JobId(job),
            image_dir: image_dir.clone(),
            options: Default::default(),
        })
        .await;
    assert_eq!(r, ResponseResult::Accepted);
    let ev = c.event().await;
    assert!(
        matches!(
            ev.event,
            Event::CheckpointFinished {
                outcome: CheckpointOutcome::Success { .. },
                ..
            }
        ),
        "{ev:?}"
    );
    image_dir
}

fn restore_req(job: u64, image_dir: &Path) -> Request {
    Request::Restore(RestoreRequest {
        job: JobId(job),
        uid: Uid(uid()),
        image_dir: image_dir.to_owned(),
        gpus: vec![GpuIndex(0)],
    })
}

/// The exploit the post-restore check stops: CRIU (root) re-opened a file
/// by a path the owner redirected after the dump. The restored tasks are
/// killed before they ever run, and the image is kept.
#[tokio::test]
async fn a_restored_job_holding_files_its_owner_may_not_open_is_killed() {
    let env = Env::new().await;
    let mut c = env.client().await;
    let image_dir = spawn_and_dump(&env, &mut c, 3).await;
    env.launcher.set_restore_check_problems(vec![
        "fd 3 of process 4242 (/etc/passwd): its owner may not read and write it".into(),
    ]);
    assert_eq!(
        c.request(restore_req(3, &image_dir)).await,
        ResponseResult::Accepted
    );
    let ev = c.event().await;
    let Event::RestoreFinished {
        outcome: RestoreOutcome::Failed { reason, .. },
        ..
    } = ev.event
    else {
        panic!("{ev:?}")
    };
    assert!(reason.contains("/etc/passwd"), "{reason}");
    assert!(reason.contains("image is kept"), "{reason}");
    assert!(c.running().await.is_empty());
    assert!(
        env.launcher.unit(JobId(3)).is_none(),
        "restore unit released"
    );
    let dir = CheckpointDir::for_job(&env.base(), 3);
    assert!(matches!(dir.status(), DirStatus::Complete(_)));
    // Never resumed, and no CUDA steps either.
    assert!(
        !env.exec
            .lock()
            .unwrap()
            .actions
            .iter()
            .any(|a| matches!(a, Some(Action::CudaRestore { .. }))),
    );
    // Once the paths are right again, it restores.
    env.launcher.set_restore_check_problems(vec![]);
    assert_eq!(
        c.request(restore_req(3, &image_dir)).await,
        ResponseResult::Accepted
    );
    assert!(matches!(
        c.event().await.event,
        Event::RestoreFinished {
            outcome: RestoreOutcome::Success { .. },
            ..
        }
    ));
}

/// A tiny `files.img` with the given regular file paths and bound socket
/// paths (see `puzuz_helper::images`).
fn files_image(files: &[&str], sockets: &[&str]) -> Vec<u8> {
    fn varint(mut v: u64, out: &mut Vec<u8>) {
        while v >= 0x80 {
            out.push((v as u8) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }
    fn bytes(n: u64, b: &[u8], out: &mut Vec<u8>) {
        varint((n << 3) | 2, out);
        varint(b.len() as u64, out);
        out.extend_from_slice(b);
    }
    let entry = |kind: u64, field: u64, inner_field: u64, name: &str| {
        let mut inner = Vec::new();
        bytes(inner_field, name.as_bytes(), &mut inner);
        let mut fe = vec![0x08];
        varint(kind, &mut fe);
        bytes(field, &inner, &mut fe);
        fe
    };
    let mut out = Vec::new();
    out.extend_from_slice(&0x5456_4319u32.to_le_bytes());
    out.extend_from_slice(&0x1234_5678u32.to_le_bytes());
    let entries = files
        .iter()
        .map(|f| entry(1, 3, 6, f))
        .chain(sockets.iter().map(|s| entry(5, 16, 11, s)));
    for e in entries {
        out.extend_from_slice(&(e.len() as u32).to_le_bytes());
        out.extend_from_slice(&e);
    }
    out
}

#[tokio::test]
async fn restore_checks_the_images_paths_first() {
    let env = Env::new().await;
    let mut c = env.client().await;
    let image_dir = spawn_and_dump(&env, &mut c, 3).await;
    let images = CheckpointDir::for_job(&env.base(), 3).images_dir();
    let exit_status = env.root.join("state/jobs/3/exit-status");
    std::fs::write(
        images.join("files.img"),
        files_image(
            &[
                "/home/alice/x/passwd",
                "/tmp/private",
                "/proc/1/status",
                exit_status.to_str().unwrap(),
            ],
            &["/tmp/ok.sock"],
        ),
    )
    .unwrap();
    // The owner's path now goes through a symlink.
    env.host.set_access_problems(
        vec!["/home/alice/x/passwd: a directory on the way is a symlink".into()],
        vec![],
    );
    let r = c.request(restore_req(3, &image_dir)).await;
    assert_eq!(err_code(&r), HelperErrorCode::PermissionDenied, "{r:?}");
    let checked: Vec<String> = env
        .host
        .verified()
        .into_iter()
        .filter_map(|r| match r {
            VerifyRequest::Paths(p) => Some(p.paths),
            VerifyRequest::Restored(_) => None,
        })
        .flatten()
        .collect();
    // The job's own mounts and the helper's file are not the owner's to
    // reach.
    assert_eq!(checked, ["/home/alice/x/passwd"]);
    assert!(env.launcher.restored().is_empty(), "criu never ran");

    // A socket bound on a host filesystem is refused outright.
    env.host.set_access_problems(vec![], vec![]);
    std::fs::write(
        images.join("files.img"),
        files_image(&["/home/alice/x/f"], &["/home/alice/agent.sock"]),
    )
    .unwrap();
    let r = c.request(restore_req(3, &image_dir)).await;
    assert_eq!(err_code(&r), HelperErrorCode::PermissionDenied, "{r:?}");
    let ResponseResult::Error { message, .. } = r else {
        unreachable!()
    };
    assert!(message.contains("/home/alice/agent.sock"), "{message}");

    std::fs::write(
        images.join("files.img"),
        files_image(&["/home/alice/x/f"], &[]),
    )
    .unwrap();
    assert_eq!(
        c.request(restore_req(3, &image_dir)).await,
        ResponseResult::Accepted
    );
    assert!(matches!(
        c.event().await.event,
        Event::RestoreFinished {
            outcome: RestoreOutcome::Success { .. },
            ..
        }
    ));
}

#[tokio::test]
async fn restore_refuses_groups_the_owner_left_since_the_dump() {
    let env = Env::new().await;
    let mut c = env.client().await;
    env.host.set_process_groups(Some(vec![gid(), 4242]));
    let image_dir = spawn_and_dump(&env, &mut c, 3).await;
    let owner: puzuz_helper::jobs::JobRecord = serde_json::from_slice(
        &std::fs::read(image_dir.join(puzuz_helper::engine::OWNER_FILE)).unwrap(),
    )
    .unwrap();
    assert_eq!(owner.dump_groups, Some(vec![gid(), 4242]));
    let r = c.request(restore_req(3, &image_dir)).await;
    assert_eq!(err_code(&r), HelperErrorCode::PermissionDenied, "{r:?}");
    let ResponseResult::Error { message, .. } = r else {
        unreachable!()
    };
    assert!(
        message.contains("4242") && message.contains("no longer in"),
        "{message}"
    );
    // Back in the group: fine.
    env.host.add_user(uid(), "tester", gid(), &[gid(), 4242]);
    assert_eq!(
        c.request(restore_req(3, &image_dir)).await,
        ResponseResult::Accepted
    );
}

#[tokio::test]
async fn dump_refuses_sockets_bound_on_host_filesystems() {
    let env = Env::new().await;
    let mut c = env.client().await;
    assert!(matches!(
        c.request(Request::Spawn(env.spawn_req(3))).await,
        ResponseResult::Spawned { .. }
    ));
    env.host.set_dump_hazards(DumpHazards {
        bound_sockets: vec!["/home/alice/agent.sock".into()],
        ..Default::default()
    });
    let r = c
        .request(Request::Checkpoint {
            job: JobId(3),
            image_dir: env.base().join("3"),
            options: Default::default(),
        })
        .await;
    assert_eq!(err_code(&r), HelperErrorCode::CheckpointRefused, "{r:?}");
    assert_eq!(c.running().await, [(JobId(3), RunningPhase::Running)]);
}

/// Deleted files on the host's filesystems go into the image instead of
/// being linked back by root (`--link-remap`) through a path the owner
/// controls.
#[test]
fn deleted_host_files_are_dumped_as_ghosts() {
    let ck = puzuz_helper::config::default_checkpoint_config();
    assert!(ck.criu.link_remap);
    let none = puzuz_helper::engine::dump_options(&ck, &DumpHazards::default());
    assert_eq!(none, ck);
    let h = DumpHazards {
        deleted_files: vec![("/home/a/x".into(), 5 << 20), ("/home/a/y".into(), 10)],
        ..Default::default()
    };
    let ghost = puzuz_helper::engine::dump_options(&ck, &h);
    assert!(!ghost.criu.link_remap);
    assert_eq!(
        ghost.criu.extra_dump_args,
        [
            "--ghost-limit".into(),
            ((5u64 << 20) + 1).to_string().into()
        ] as [std::ffi::OsString; 2]
    );
}

/// The helper stopped in the middle of a restore: its next start kills
/// whatever the restore unit runs (unchecked, maybe without its CUDA
/// state) and reports the restore as failed; the image is kept.
#[tokio::test]
async fn an_interrupted_restore_is_killed_and_reported() {
    let mut env = Env::new().await;
    let mut c = env.client().await;
    let image_dir = spawn_and_dump(&env, &mut c, 3).await;
    drop(c);
    let _ = env.stop.send(true);
    // What a helper that died right after starting the restore unit
    // leaves behind.
    let files = puzuz_helper::jobs::JobFiles::new(&env.root.join("state/jobs"), JobId(3));
    let mut record = files.read_record().unwrap();
    record.restores = 1;
    record.unit = "puzuz-job-3-r1.service".into();
    files.write_record(&record).unwrap();
    env.launcher
        .restore(&puzuz_helper::launcher::RestoreSpec {
            job: JobId(3),
            unit: record.unit.clone(),
            command: puzuz_checkpoint::CommandSpec::new("criu"),
            pidfile: image_dir.join("restore.pid"),
            timeout: Duration::from_secs(1),
            criu_root: None,
        })
        .await
        .unwrap();
    env.restart().await;
    let mut c = env.client().await;
    // The checkpoint's event (never acknowledged) is replayed first.
    let mut ev = c.event().await;
    while matches!(ev.event, Event::CheckpointFinished { .. }) {
        ev = c.event().await;
    }
    assert!(
        matches!(
            &ev.event,
            Event::RestoreFinished {
                job: JobId(3),
                outcome: RestoreOutcome::Failed { .. }
            }
        ),
        "{ev:?}"
    );
    assert!(c.running().await.is_empty());
    assert!(
        env.launcher.unit(JobId(3)).is_none(),
        "unit killed and released"
    );
    assert!(matches!(
        CheckpointDir::for_job(&env.base(), 3).status(),
        DirStatus::Complete(_)
    ));
    // And it can be restored again.
    assert_eq!(
        c.request(restore_req(3, &image_dir)).await,
        ResponseResult::Accepted
    );
    let ev = c.event().await;
    assert!(
        matches!(
            ev.event,
            Event::RestoreFinished {
                outcome: RestoreOutcome::Success { .. },
                ..
            }
        ),
        "{ev:?}"
    );
}

/// An image is restored in the CUDA mode it was dumped in, even after
/// `cuda_checkpoint_mode` changed: a plugin-mode image with CRIU's plugin
/// directory and no CUDA steps of the helper, a manual-mode one with the
/// empty plugin directory and the helper's restore/unlock.
#[tokio::test]
async fn restore_uses_the_mode_of_the_image() {
    for (dumped, now) in [
        (CudaMode::Plugin, CudaMode::Manual),
        (CudaMode::Manual, CudaMode::Plugin),
    ] {
        let mut env = Env::new().await;
        env.mode = dumped;
        env.restart().await;
        let plugins = env.root.join("criu-plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(plugins.join("cuda_plugin.so"), b"").unwrap();
        let mut c = env.client().await;
        let job = JobId(5);
        assert!(matches!(
            c.request(Request::Spawn(env.spawn_req(5))).await,
            ResponseResult::Spawned { .. }
        ));
        env.host.set_cuda_pids(vec![10_001]);
        let image_dir = env.base().join("5");
        let r = c
            .request(Request::Checkpoint {
                job,
                image_dir: image_dir.clone(),
                options: Default::default(),
            })
            .await;
        assert_eq!(r, ResponseResult::Accepted);
        let ev = c.event().await;
        assert!(
            matches!(
                ev.event,
                Event::CheckpointFinished {
                    outcome: CheckpointOutcome::Success { .. },
                    ..
                }
            ),
            "{ev:?}"
        );
        let DirStatus::Complete(meta) = CheckpointDir::for_job(&env.base(), 5).status() else {
            panic!("no image")
        };
        assert_eq!(meta.mode, dumped);
        assert_eq!(
            c.request(Request::AckEvents { up_to: ev.seq }).await,
            ResponseResult::Ok
        );
        drop(c);

        env.mode = now;
        env.restart().await;
        let mut c = env.client().await;
        env.exec.lock().unwrap().actions.clear();
        let r = c
            .request(Request::Restore(RestoreRequest {
                job,
                uid: Uid(uid()),
                image_dir,
                gpus: vec![GpuIndex(0)],
            }))
            .await;
        assert_eq!(r, ResponseResult::Accepted);
        let ev = c.event().await;
        assert!(
            matches!(
                ev.event,
                Event::RestoreFinished {
                    outcome: RestoreOutcome::Success { .. },
                    ..
                }
            ),
            "{ev:?}"
        );
        let manual = dumped == CudaMode::Manual;
        let restored = env.launcher.restored();
        let plugin_dir = if manual {
            env.helper.config().checkpoint.criu.empty_plugin_dir.clone()
        } else {
            plugins
        };
        assert_eq!(
            restored[0].command.value_of("-L"),
            Some(plugin_dir.as_os_str()),
            "plugin directory of a {dumped:?} image"
        );
        let cuda_steps = env
            .exec
            .lock()
            .unwrap()
            .actions
            .iter()
            .any(|a| matches!(a, Some(Action::CudaRestore { .. })));
        assert_eq!(cuda_steps, manual, "CUDA steps of a {dumped:?} image");
    }
}

/// In plugin mode a CUDA job is not dumped without CRIU's CUDA plugin
/// (the dump would fail and end the job): the dump is refused and the job
/// keeps running. A job without CUDA does not need the plugin.
#[tokio::test]
async fn plugin_mode_refuses_to_dump_cuda_jobs_without_the_plugin() {
    let mut env = Env::new().await;
    env.mode = CudaMode::Plugin;
    env.restart().await;
    let mut c = env.client().await;
    for job in [6, 7] {
        assert!(matches!(
            c.request(Request::Spawn(env.spawn_req(job))).await,
            ResponseResult::Spawned { .. }
        ));
    }
    env.host.set_cuda_pids(vec![10_001]);
    let r = c
        .request(Request::Checkpoint {
            job: JobId(6),
            image_dir: env.base().join("6"),
            options: Default::default(),
        })
        .await;
    assert_eq!(r, ResponseResult::Accepted);
    let ev = c.event().await;
    let Event::CheckpointFinished {
        outcome: CheckpointOutcome::Failed { reason, code },
        ..
    } = ev.event
    else {
        panic!("{ev:?}")
    };
    assert_eq!(code, Some(HelperErrorCode::CheckpointRefused), "{reason}");
    assert!(reason.contains("cuda_plugin.so"), "{reason}");
    assert!(
        !env.exec
            .lock()
            .unwrap()
            .actions
            .iter()
            .any(|a| matches!(a, Some(Action::CriuDump { .. }))),
        "criu ran"
    );
    assert!(
        c.running()
            .await
            .contains(&(JobId(6), RunningPhase::Running))
    );

    env.host.set_cuda_pids(Vec::new());
    let r = c
        .request(Request::Checkpoint {
            job: JobId(7),
            image_dir: env.base().join("7"),
            options: Default::default(),
        })
        .await;
    assert_eq!(r, ResponseResult::Accepted);
    let ev = c.event().await;
    assert!(
        matches!(
            ev.event,
            Event::CheckpointFinished {
                outcome: CheckpointOutcome::Success { .. },
                ..
            }
        ),
        "{ev:?}"
    );
}

#[tokio::test]
async fn failed_dump_leaves_the_job_running_and_stopped_jobs_are_resumed_first() {
    let env = Env::new().await;
    let mut c = env.client().await;
    let job = JobId(4);
    assert!(matches!(
        c.request(Request::Spawn(env.spawn_req(4))).await,
        ResponseResult::Spawned { .. }
    ));
    c.request(Request::Signal {
        job,
        signal: signal::SIGSTOP,
    })
    .await;
    env.exec.lock().unwrap().fail_on = Some(|a| matches!(a, Action::CriuDump { .. }));
    let r = c
        .request(Request::Checkpoint {
            job,
            image_dir: env.base().join("4"),
            options: Default::default(),
        })
        .await;
    assert_eq!(r, ResponseResult::Accepted);
    let ev = c.event().await;
    let Event::CheckpointFinished {
        outcome: CheckpointOutcome::Failed { reason, code: None },
        ..
    } = ev.event
    else {
        panic!("{ev:?}")
    };
    assert!(reason.contains("injected"), "{reason}");
    // SIGCONT before the dump, SIGSTOP again after the failure.
    assert_eq!(
        env.launcher.unit(job).unwrap().signals,
        [signal::SIGSTOP, signal::SIGCONT, signal::SIGSTOP]
    );
    assert_eq!(c.running().await, [(job, RunningPhase::Stopped)]);

    // CRIU's log was moved out of the checkpoint directory, which is gone,
    // and the reason names it.
    let logs = env.base().join(FAILED_LOGS_SUBDIR);
    let kept: Vec<PathBuf> = std::fs::read_dir(&logs)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(kept.len(), 1, "{kept:?}");
    let name = kept[0].file_name().unwrap().to_str().unwrap();
    assert!(name.starts_with("4-") && name.ends_with(".log"), "{name}");
    assert!(
        reason.contains(&format!("(CRIU log: {})", kept[0].display())),
        "{reason}"
    );
    assert!(
        std::fs::read_to_string(&kept[0])
            .unwrap()
            .contains("fake criu log")
    );
    assert!(!env.base().join("4").exists());

    // The daemon's cleanup after the job ends leaves the log alone.
    env.launcher.exit(job, ExitStatus::Signaled { signal: 9 });
    assert!(matches!(c.event().await.event, Event::JobExited { .. }));
    assert_eq!(
        c.request(Request::DeleteCheckpoint {
            job,
            image_dir: env.base().join("4"),
        })
        .await,
        ResponseResult::Ok
    );
    assert!(kept[0].exists());
}

#[tokio::test]
async fn a_dump_past_its_deadline_is_killed_and_the_job_resumed() {
    let mut env = Env::new().await;
    let mut c = env.client().await;
    assert!(matches!(
        c.request(Request::Spawn(env.spawn_req(8))).await,
        ResponseResult::Spawned { .. }
    ));
    env.exec.lock().unwrap().hang_on = Some(|a| matches!(a, Action::CriuDump { .. }));
    // The daemon's cap on the whole operation: 1 s.
    let started = std::time::Instant::now();
    let res = c
        .request(Request::Checkpoint {
            job: JobId(8),
            image_dir: env.base().join("8"),
            options: CheckpointOptions {
                timeout_secs: Some(1),
                ..Default::default()
            },
        })
        .await;
    assert_eq!(res, ResponseResult::Accepted);
    let env_event = c.event().await;
    let Event::CheckpointFinished { job, outcome } = env_event.event else {
        panic!("expected a checkpoint event")
    };
    assert_eq!(
        c.request(Request::AckEvents {
            up_to: env_event.seq
        })
        .await,
        ResponseResult::Ok
    );
    assert_eq!(job, JobId(8));
    let CheckpointOutcome::Failed { reason, code: None } = outcome else {
        panic!("{outcome:?}")
    };
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(
        reason.contains("did not finish within its deadline of 1s"),
        "{reason}"
    );
    assert!(reason.contains("the job was resumed"), "{reason}");
    assert!(reason.contains("criu was killed mid-dump"), "{reason}");
    assert!(reason.contains("CRIU log:"), "{reason}");
    // The interrupted dump was followed by a thaw of the job's tree, and
    // the job is still there, running.
    assert_eq!(env.exec.lock().unwrap().thawed.len(), 1);
    assert_eq!(c.running().await, [(JobId(8), RunningPhase::Running)]);
    assert!(!env.base().join("8").join("COMPLETE").exists());

    // The configured deadline applies without a cap from the daemon.
    let mut cfg = env.helper.config().clone();
    cfg.checkpoint.timeouts.deadline_base = Duration::from_secs(1);
    cfg.checkpoint.timeouts.deadline_per_gib = Duration::ZERO;
    env.restart_with(cfg).await;
    let mut c = env.client().await;
    env.exec.lock().unwrap().hang_on = Some(|a| matches!(a, Action::CriuDump { .. }));
    assert_eq!(
        c.request(Request::Checkpoint {
            job: JobId(8),
            image_dir: env.base().join("8"),
            options: Default::default(),
        })
        .await,
        ResponseResult::Accepted
    );
    let outcome = loop {
        if let Event::CheckpointFinished { outcome, .. } = c.event().await.event {
            break outcome;
        }
    };
    assert!(
        matches!(&outcome, CheckpointOutcome::Failed { reason, .. } if reason.contains("deadline of 1s")),
        "{outcome:?}"
    );
    assert_eq!(c.running().await, [(JobId(8), RunningPhase::Running)]);
}

#[tokio::test]
async fn dump_refuses_a_job_that_depends_on_a_hidden_mount() {
    let env = Env::new().await;
    let mut c = env.client().await;
    let job = JobId(5);
    assert!(matches!(
        c.request(Request::Spawn(env.spawn_req(5))).await,
        ResponseResult::Spawned { .. }
    ));
    env.host
        .set_unreachable_mounts(vec!["/run/nvidia-persistenced/socket".into()]);
    let r = c
        .request(Request::Checkpoint {
            job,
            image_dir: env.base().join("5"),
            options: Default::default(),
        })
        .await;
    // Refused in the answer; no outcome event follows.
    let ResponseResult::Error {
        code: HelperErrorCode::CheckpointRefused,
        message,
    } = r
    else {
        panic!("{r:?}")
    };
    assert!(
        message.contains("/run/nvidia-persistenced/socket"),
        "{message}"
    );
    c.no_event().await;
    assert!(
        env.exec.lock().unwrap().actions.is_empty(),
        "nothing was run against the job"
    );
    assert!(env.launcher.unit(job).unwrap().signals.is_empty());
    assert!(!env.base().join("5").exists());
    assert_eq!(c.running().await, [(job, RunningPhase::Running)]);

    // Once the problem is gone the job can be dumped.
    env.host.set_unreachable_mounts(vec![]);
    let r = c
        .request(Request::Checkpoint {
            job,
            image_dir: env.base().join("5"),
            options: Default::default(),
        })
        .await;
    assert_eq!(r, ResponseResult::Accepted);
    assert!(matches!(
        c.event().await.event,
        Event::CheckpointFinished {
            outcome: CheckpointOutcome::Success { .. },
            ..
        }
    ));
}

#[tokio::test]
async fn dump_refuses_without_free_space_and_keeps_a_stopped_job_stopped() {
    let env = Env::new().await;
    let mut c = env.client().await;
    let job = JobId(7);
    assert!(matches!(
        c.request(Request::Spawn(env.spawn_req(7))).await,
        ResponseResult::Spawned { .. }
    ));
    c.request(Request::Signal {
        job,
        signal: signal::SIGSTOP,
    })
    .await;
    env.host.set_available(1);
    let r = c
        .request(Request::Checkpoint {
            job,
            image_dir: env.base().join("7"),
            options: Default::default(),
        })
        .await;
    assert_eq!(err_code(&r), HelperErrorCode::CheckpointRefused, "{r:?}");
    c.no_event().await;
    // Not even resumed for the dump.
    assert_eq!(env.launcher.unit(job).unwrap().signals, [signal::SIGSTOP]);
    assert_eq!(c.running().await, [(job, RunningPhase::Stopped)]);
    assert!(env.exec.lock().unwrap().actions.is_empty());
}

#[tokio::test]
async fn delete_checkpoint_refuses_live_jobs() {
    let env = Env::new().await;
    let mut c = env.client().await;
    assert!(matches!(
        c.request(Request::Spawn(env.spawn_req(6))).await,
        ResponseResult::Spawned { .. }
    ));
    let del = Request::DeleteCheckpoint {
        job: JobId(6),
        image_dir: env.base().join("6"),
    };
    assert_eq!(
        err_code(&c.request(del.clone()).await),
        HelperErrorCode::Busy
    );
    c.request(Request::Checkpoint {
        job: JobId(6),
        image_dir: env.base().join("6"),
        options: Default::default(),
    })
    .await;
    c.event().await;
    assert_eq!(c.request(del).await, ResponseResult::Ok);
    assert!(!env.base().join("6").exists());
    // ... after which the id may be spawned again.
    assert!(matches!(
        c.request(Request::Spawn(env.spawn_req(6))).await,
        ResponseResult::Spawned { .. }
    ));
}

#[tokio::test]
async fn shutdown_reports_refused_and_failed_dumps_apart() {
    let env = Env::new().await;
    let mut c = env.client().await;
    for job in [20, 21] {
        assert!(matches!(
            c.request(Request::Spawn(env.spawn_req(job))).await,
            ResponseResult::Spawned { .. }
        ));
    }
    // Job 20's dump fails; job 21's is refused (the hidden mount shows up
    // after the first dump, as if it had been mounted meanwhile).
    env.exec.lock().unwrap().fail_on = Some(|a| matches!(a, Action::CriuDump { .. }));
    let host = env.host.clone();
    let manifest = env
        .helper
        .shutdown(
            move |job| {
                if job == JobId(21) {
                    host.set_unreachable_mounts(vec!["/hidden".into()]);
                }
            },
            |_| {},
        )
        .await;
    let [first, second] = &manifest.jobs[..] else {
        panic!("{manifest:?}")
    };
    let ManifestResult::Failed {
        reason,
        refused: false,
    } = &first.result
    else {
        panic!("{first:?}")
    };
    assert_eq!(first.job, JobId(20));
    assert!(reason.contains("CRIU log:"), "{reason}");
    let ManifestResult::Failed {
        reason,
        refused: true,
    } = &second.result
    else {
        panic!("{second:?}")
    };
    assert_eq!(second.job, JobId(21));
    assert!(reason.contains("/hidden"), "{reason}");
    let on_disk: ShutdownManifest =
        serde_json::from_slice(&std::fs::read(env.base().join(SHUTDOWN_MANIFEST)).unwrap())
            .unwrap();
    assert_eq!(on_disk, manifest);

    // The daemon hears about both, told apart.
    let outcomes = events(&mut c, 2, |e| match e {
        Event::CheckpointFinished { job, outcome } => Some((job, outcome.is_refused())),
        _ => None,
    })
    .await;
    assert_eq!(outcomes, [(JobId(20), false), (JobId(21), true)]);
    // Both are left for systemd to stop.
    assert_eq!(
        c.running().await,
        [
            (JobId(20), RunningPhase::Running),
            (JobId(21), RunningPhase::Running)
        ]
    );
}

#[tokio::test]
async fn shutdown_checkpoints_everything_except_no_checkpoint_jobs() {
    let env = Env::new().await;
    let mut c = env.client().await;
    let mut skip = env.spawn_req(11);
    skip.no_checkpoint = true;
    for req in [env.spawn_req(10), skip] {
        assert!(matches!(
            c.request(Request::Spawn(req)).await,
            ResponseResult::Spawned { .. }
        ));
    }

    let dumped = Arc::new(Mutex::new(Vec::new()));
    let d = dumped.clone();
    let manifest = env
        .helper
        .shutdown(move |job| d.lock().unwrap().push(job), |_| {})
        .await;
    assert_eq!(*dumped.lock().unwrap(), [JobId(10)]);
    assert_eq!(manifest.jobs.len(), 2);
    assert!(matches!(
        manifest.jobs[0],
        ManifestEntry {
            job: JobId(10),
            result: ManifestResult::Checkpointed { .. }
        }
    ));
    assert_eq!(
        manifest.jobs[1],
        ManifestEntry {
            job: JobId(11),
            result: ManifestResult::Skipped
        }
    );
    let on_disk: ShutdownManifest =
        serde_json::from_slice(&std::fs::read(env.base().join(SHUTDOWN_MANIFEST)).unwrap())
            .unwrap();
    assert_eq!(on_disk, manifest);

    // The daemon still hears about it, and new work is refused.
    assert!(matches!(
        c.event().await.event,
        Event::CheckpointFinished {
            job: JobId(10),
            outcome: CheckpointOutcome::Success { .. }
        }
    ));
    assert_eq!(
        err_code(&c.request(Request::Spawn(env.spawn_req(12))).await),
        HelperErrorCode::ShuttingDown
    );
    assert_eq!(
        err_code(
            &c.request(Request::Restore(RestoreRequest {
                job: JobId(10),
                uid: Uid(uid()),
                image_dir: env.base().join("10"),
                gpus: vec![],
            }))
            .await
        ),
        HelperErrorCode::ShuttingDown
    );
    // The no_checkpoint job is left to systemd.
    assert_eq!(
        env.launcher.unit(JobId(11)).unwrap().state,
        UnitState::Running
    );
}

/// Collects `n` events, panicking on any that `pick` rejects.
async fn events<T>(c: &mut Client, n: usize, pick: impl Fn(Event) -> Option<T>) -> Vec<T> {
    let mut out = Vec::new();
    while out.len() < n {
        let ev = c.event().await;
        match pick(ev.event.clone()) {
            Some(v) => out.push(v),
            None => panic!("unexpected {ev:?}"),
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_spawns_and_restores() {
    let env = Env::new().await;
    env.launcher.set_delay(Some(Duration::from_millis(30)));
    let mut c = env.client().await;

    // Many spawns at once, into a log directory that does not exist yet.
    let first: Vec<u64> = (100..124).collect();
    let results = c
        .request_all(
            first
                .iter()
                .map(|&j| Request::Spawn(env.spawn_req(j)))
                .collect(),
        )
        .await;
    let mut pids = std::collections::BTreeSet::new();
    for (job, r) in first.iter().zip(results) {
        let ResponseResult::Spawned { pid } = r else {
            panic!("job {job}: {r:?}")
        };
        assert!(pids.insert(pid), "pid {pid} reused");
    }
    assert!(
        env.launcher.max_concurrent() > 1,
        "the spawns did not overlap"
    );
    assert_eq!(c.running().await.len(), first.len());

    // Checkpoint some of them (the helper dumps one at a time).
    let dumped: Vec<u64> = first[..8].to_vec();
    let results = c
        .request_all(
            dumped
                .iter()
                .map(|&j| Request::Checkpoint {
                    job: JobId(j),
                    image_dir: env.base().join(j.to_string()),
                    options: Default::default(),
                })
                .collect(),
        )
        .await;
    assert!(
        results.iter().all(|r| *r == ResponseResult::Accepted),
        "{results:?}"
    );
    let mut done = events(&mut c, dumped.len(), |e| match e {
        Event::CheckpointFinished {
            job,
            outcome: CheckpointOutcome::Success { .. },
        } => Some(job.0),
        _ => None,
    })
    .await;
    done.sort_unstable();
    assert_eq!(done, dumped);

    // Restore them while new jobs are spawned and the job list is read.
    let second: Vec<u64> = (200..208).collect();
    let mut requests = Vec::new();
    for (&r, &s) in dumped.iter().zip(&second) {
        requests.push(Request::Restore(RestoreRequest {
            job: JobId(r),
            uid: Uid(uid()),
            image_dir: env.base().join(r.to_string()),
            gpus: vec![GpuIndex(0)],
        }));
        requests.push(Request::Spawn(env.spawn_req(s)));
        requests.push(Request::ListRunning);
    }
    for (i, r) in c.request_all(requests).await.into_iter().enumerate() {
        match i % 3 {
            0 => assert_eq!(r, ResponseResult::Accepted),
            1 => assert!(matches!(r, ResponseResult::Spawned { .. }), "{r:?}"),
            _ => assert!(matches!(r, ResponseResult::Running { .. }), "{r:?}"),
        }
    }
    let mut restored = events(&mut c, dumped.len(), |e| match e {
        Event::RestoreFinished {
            job,
            outcome: RestoreOutcome::Success { .. },
        } => Some(job.0),
        _ => None,
    })
    .await;
    restored.sort_unstable();
    assert_eq!(restored, dumped);

    let all: Vec<u64> = first.iter().chain(&second).copied().collect();
    let running = c.running().await;
    assert_eq!(running.len(), all.len());
    assert!(running.iter().all(|(_, p)| *p == RunningPhase::Running));
    // One live unit per job: the restored ones under new, distinct names.
    let units = env.launcher.list().await.unwrap();
    assert_eq!(units.len(), all.len(), "{units:?}");

    // Each job's exit is reported exactly once.
    for &j in &all {
        env.launcher.exit(JobId(j), ExitStatus::Exited { code: 0 });
    }
    let mut exited = events(&mut c, all.len(), |e| match e {
        Event::JobExited { job, .. } => Some(job.0),
        _ => None,
    })
    .await;
    exited.sort_unstable();
    let mut expected = all.clone();
    expected.sort_unstable();
    assert_eq!(exited, expected);
    c.no_event().await;
    assert!(c.running().await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stopping_helper_abandons_queued_launches_and_refuses_new_ones() {
    let env = Env::new().await;
    let mut c = env.client().await;
    assert!(matches!(
        c.request(Request::Spawn(env.spawn_req(1))).await,
        ResponseResult::Spawned { .. }
    ));
    // Spawns whose units systemd has not started yet (the fake's delay).
    env.launcher.set_delay(Some(Duration::from_secs(30)));
    let mut queued = Vec::new();
    for j in 2..=5 {
        queued.push(c.send_request(Request::Spawn(env.spawn_req(j))).await);
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let drain = tokio::spawn({
        let helper = env.helper.clone();
        async move { helper.drain().await }
    });

    // They are abandoned and refused with `shutting_down` (the daemon
    // offers them again), leaving nothing behind...
    for id in queued {
        assert_eq!(
            err_code(&c.response(id).await),
            HelperErrorCode::ShuttingDown
        );
    }
    tokio::time::timeout(WAIT, drain).await.unwrap().unwrap();
    for j in 2..=5 {
        assert!(env.launcher.unit(JobId(j)).is_none());
        assert!(!env.root.join("state/jobs").join(j.to_string()).exists());
    }
    // ...new launches are refused...
    assert_eq!(
        err_code(&c.request(Request::Spawn(env.spawn_req(6))).await),
        HelperErrorCode::ShuttingDown
    );
    assert_eq!(
        err_code(
            &c.request(Request::Restore(RestoreRequest {
                job: JobId(7),
                uid: Uid(uid()),
                image_dir: env.base().join("7"),
                gpus: vec![],
            }))
            .await
        ),
        HelperErrorCode::ShuttingDown
    );
    // ...and the running job is left alone.
    assert_eq!(c.running().await, [(JobId(1), RunningPhase::Running)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stopping_helper_reports_an_abandoned_restore_as_not_started() {
    let env = Env::new().await;
    let mut c = env.client().await;
    let job = JobId(8);
    assert!(matches!(
        c.request(Request::Spawn(env.spawn_req(8))).await,
        ResponseResult::Spawned { .. }
    ));
    let image_dir = env.base().join("8");
    assert_eq!(
        c.request(Request::Checkpoint {
            job,
            image_dir: image_dir.clone(),
            options: Default::default(),
        })
        .await,
        ResponseResult::Accepted
    );
    let ev = c.event().await;
    assert!(
        matches!(
            ev.event,
            Event::CheckpointFinished {
                outcome: CheckpointOutcome::Success { .. },
                ..
            }
        ),
        "{ev:?}"
    );

    // The restore's unit is not started yet (the fake's delay) when the
    // helper is stopped: it is abandoned, tagged `shutting_down`.
    env.launcher.set_delay(Some(Duration::from_secs(30)));
    let request = Request::Restore(RestoreRequest {
        job,
        uid: Uid(uid()),
        image_dir,
        gpus: vec![],
    });
    assert_eq!(c.request(request).await, ResponseResult::Accepted);
    tokio::time::sleep(Duration::from_millis(100)).await;
    tokio::time::timeout(WAIT, env.helper.drain())
        .await
        .unwrap();
    let ev = c.event().await;
    let Event::RestoreFinished {
        outcome: RestoreOutcome::Failed { code, .. },
        ..
    } = ev.event
    else {
        panic!("{ev:?}")
    };
    assert_eq!(code, Some(HelperErrorCode::ShuttingDown));
    // The image is still there to restore later.
    assert!(matches!(
        CheckpointDir::for_job(&env.base(), 8).status(),
        DirStatus::Complete(_)
    ));
    assert!(c.running().await.is_empty());
}
