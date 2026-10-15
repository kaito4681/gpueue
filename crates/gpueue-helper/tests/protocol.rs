//! The helper over its real socket, with a fake launcher, host and
//! checkpoint executor. Runs unprivileged: the "daemon" is this process's
//! own uid, and jobs are owned by it too.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpueue_checkpoint::layout::{CheckpointDir, DirStatus, FAILED_LOGS_SUBDIR};
use gpueue_checkpoint::plan::Action;
use gpueue_checkpoint::{CudaMode, Executor};
use gpueue_helper::access::VerifyRequest;
use gpueue_helper::config::SHUTDOWN_MANIFEST;
use gpueue_helper::engine::{ManifestEntry, ManifestResult};
use gpueue_helper::gcroots::GcRoots;
use gpueue_helper::host::DumpHazards;
use gpueue_helper::launcher::fake::FakeLauncher;
use gpueue_helper::launcher::{Launcher, UnitState};
use gpueue_helper::testing::{FakeExecState, FakeExecutor, FakeHost};
use gpueue_helper::{ExecutorFactory, Helper, HelperConfig, ShutdownManifest, server};
use gpueue_protocol::helper::{
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
        missing_programs: Vec::new(),
        programs: Vec::new(),
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
    // TSTP and ALRM are forwarded to the job; signals the shim cannot
    // forward (a fault signal, a real-time signal) are refused.
    for sig in [20, 14] {
        assert_eq!(
            c.request(Request::Signal {
                job: JobId(1),
                signal: sig
            })
            .await,
            ResponseResult::Ok
        );
    }
    for sig in [11, 17, 34, 64] {
        assert_eq!(
            err_code(
                &c.request(Request::Signal {
                    job: JobId(1),
                    signal: sig
                })
                .await
            ),
            HelperErrorCode::InvalidRequest,
            "signal {sig}"
        );
    }
    assert_eq!(
        env.launcher.unit(JobId(1)).unwrap().signals,
        [signal::SIGSTOP, 20, 14]
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
async fn nix_gc_roots_follow_jobs_and_their_checkpoints() {
    let mut env = Env::new().await;
    // A helper "in the Nix store".
    let store = env.root.join("store");
    let gcroots = env.root.join("gcroots");
    let bin = store.join("abc-gpueue-0.1.0/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&gcroots).unwrap();
    let exe = bin.join("gpueue-helper");
    std::fs::write(&exe, b"").unwrap();
    let mut cfg = with_mode(config(&env.root, uid()), &env.root, CudaMode::Manual);
    cfg.gc_roots = GcRoots::new(&gcroots, &store, &exe);
    env.restart_with(cfg.clone()).await;
    let root = |job: u64| gcroots.join(format!("gpueue/job-{job}"));
    let rooted = |job: u64| std::fs::symlink_metadata(root(job)).is_ok();

    let mut c = env.client().await;
    for job in [1, 2] {
        assert!(matches!(
            c.request(Request::Spawn(env.spawn_req(job))).await,
            ResponseResult::Spawned { .. }
        ));
    }
    assert_eq!(
        std::fs::read_link(root(1)).unwrap(),
        store.join("abc-gpueue-0.1.0")
    );

    // A job that exits needs nothing any more.
    env.launcher.exit(JobId(1), ExitStatus::Exited { code: 0 });
    assert!(matches!(c.event().await.event, Event::JobExited { .. }));
    assert!(!rooted(1));

    // A checkpoint keeps the root until it is deleted.
    let image_dir = env.base().join("2");
    let r = c
        .request(Request::Checkpoint {
            job: JobId(2),
            image_dir: image_dir.clone(),
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
    assert!(rooted(2));
    assert_eq!(
        c.request(Request::DeleteCheckpoint {
            job: JobId(2),
            image_dir
        })
        .await,
        ResponseResult::Ok
    );
    assert!(!rooted(2));
    drop(c);

    // Roots of jobs the helper does not know go when it starts.
    std::os::unix::fs::symlink(store.join("abc-gpueue-0.1.0"), root(9)).unwrap();
    env.restart_with(cfg).await;
    assert!(!rooted(9));
}

/// Job units run a content-addressed copy of the helper (see
/// `gpueue_helper::helperbin`), which an upgrade does not replace: a copy
/// stays while a job record or a checkpoint needs it, also across helper
/// restarts with another binary, and goes at the next start after that.
#[tokio::test]
async fn helper_copies_follow_jobs_and_their_checkpoints() {
    use gpueue_helper::helperbin;
    // The next event, acknowledged (so that restarts do not replay it).
    async fn next(c: &mut Client) -> Event {
        let ev = c.event().await;
        assert_eq!(
            c.request(Request::AckEvents { up_to: ev.seq }).await,
            ResponseResult::Ok
        );
        ev.event
    }
    let mut env = Env::new().await;
    let root = env.root.clone();
    let lib = root.join("lib");
    let build = |name: &str, contents: &[u8]| {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("gpueue-helper");
        std::fs::write(&exe, contents).unwrap();
        helperbin::install(&lib, &exe).unwrap()
    };
    let config_with = |copy: &Path| {
        let mut cfg = with_mode(config(&root, uid()), &root, CudaMode::Manual);
        cfg.helper_copy_dir = Some(lib.clone());
        cfg.shim_exe = Some(copy.to_owned());
        cfg
    };
    let v1 = build("v1", b"\x7fELF helper 1");
    env.restart_with(config_with(&v1)).await;

    let mut c = env.client().await;
    for job in [1, 2, 3] {
        assert!(matches!(
            c.request(Request::Spawn(env.spawn_req(job))).await,
            ResponseResult::Spawned { .. }
        ));
    }
    assert!(env.launcher.spawned().iter().all(|s| s.exe == v1));
    let base = env.base();
    let image_dir = |job: u64| base.join(job.to_string());
    for job in [2, 3] {
        let r = c
            .request(Request::Checkpoint {
                job: JobId(job),
                image_dir: image_dir(job),
                options: Default::default(),
            })
            .await;
        assert_eq!(r, ResponseResult::Accepted);
        assert!(matches!(
            next(&mut c).await,
            Event::CheckpointFinished {
                outcome: CheckpointOutcome::Success { .. },
                ..
            }
        ));
    }
    drop(c);

    // An upgrade: another binary, another copy. Job 1 still runs v1, the
    // checkpoints need it too; a copy nothing needs goes.
    let v2 = build("v2", b"\x7fELF helper 2");
    let stale = build("v0", b"\x7fELF helper 0");
    env.restart_with(config_with(&v2)).await;
    assert!(v1.exists() && v2.exists());
    assert!(!stale.exists(), "an unused copy is removed at start-up");
    let mut c = env.client().await;
    assert!(matches!(
        c.request(Request::Spawn(env.spawn_req(4))).await,
        ResponseResult::Spawned { .. }
    ));
    assert_eq!(env.launcher.spawned().last().unwrap().exe, v2);

    // A restore puts the job back on the copy it was dumped with; the copy
    // stays as long as the restored job runs.
    assert_eq!(
        c.request(restore_req(2, &image_dir(2))).await,
        ResponseResult::Accepted
    );
    let ev = next(&mut c).await;
    assert!(
        matches!(
            ev,
            Event::RestoreFinished {
                outcome: RestoreOutcome::Success { .. },
                ..
            }
        ),
        "{ev:?}"
    );
    // The copy of an image is not removed by hand ...
    std::fs::rename(&v1, env.root.join("v1-moved")).unwrap();
    let r = c.request(restore_req(3, &image_dir(3))).await;
    assert_eq!(err_code(&r), HelperErrorCode::InvalidRequest, "{r:?}");
    assert!(format!("{r:?}").contains("helper binary"), "{r:?}");
    std::fs::rename(env.root.join("v1-moved"), &v1).unwrap();
    assert_eq!(
        c.request(Request::DeleteCheckpoint {
            job: JobId(3),
            image_dir: image_dir(3)
        })
        .await,
        ResponseResult::Ok
    );
    env.launcher.exit(JobId(1), ExitStatus::Exited { code: 0 });
    assert!(matches!(next(&mut c).await, Event::JobExited { .. }));
    drop(c);
    env.restart_with(config_with(&v2)).await;
    assert!(v1.exists(), "the restored job 2 still runs v1");

    // Once no job and no checkpoint needs v1, the next start removes it.
    let mut c = env.client().await;
    env.launcher.exit(JobId(2), ExitStatus::Exited { code: 0 });
    assert!(matches!(next(&mut c).await, Event::JobExited { .. }));
    drop(c);
    env.restart_with(config_with(&v2)).await;
    assert!(!v1.exists() && v2.exists());
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
            leave_stopped: false,
        })
    };
    assert_eq!(
        err_code(&c.request(restore(uid() + 1)).await),
        HelperErrorCode::PermissionDenied
    );
    // A mount point of the job's namespace that no longer exists is
    // reported before criu would fail on it.
    let mounts = dir.root().join(gpueue_helper::engine::MOUNTS_FILE);
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
        outcome: RestoreOutcome::Success {
            pid,
            stopped: false,
        },
        ..
    } = ev.event
    else {
        panic!("{ev:?}")
    };
    assert_eq!(c.running().await, [(job, RunningPhase::Running)]);
    assert!(env.host.cuda_released().is_empty());
    assert_eq!(env.launcher.unit(job).unwrap().pid, pid);
    assert_eq!(
        dir.status(),
        DirStatus::Missing,
        "image removed after restore"
    );
    let restored = env.launcher.restored();
    assert_eq!(restored[0].unit, "gpueue-job-3-r1.service");
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
    let check: gpueue_helper::access::RestoredCheck =
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

/// With `gpu_device_isolation` (the default), job units may open the
/// device nodes of their GPUs only, found by minor number; restore units
/// get those of the GPUs of the restore.
#[tokio::test]
async fn units_may_open_only_their_gpus() {
    let mut env = Env::new().await;
    let mut c = env.client().await;
    // GPU index 0 is /dev/nvidia4, index 1 /dev/nvidia2.
    env.host.set_gpu_minors(vec![4, 2]);
    let devices = |nodes: &[&str]| {
        let mut all: Vec<String> = nodes.iter().map(|n| (*n).to_owned()).collect();
        all.extend(
            [
                "/dev/nvidiactl",
                "/dev/nvidia-uvm",
                "/dev/nvidia-uvm-tools",
                "/dev/nvidia-modeset",
                "char-nvidia-uvm",
            ]
            .map(String::from),
        );
        Some(
            all.into_iter()
                .map(|p| (p, "rw".to_owned()))
                .collect::<Vec<_>>(),
        )
    };

    let image_dir = spawn_and_dump(&env, &mut c, 1).await;
    assert_eq!(
        env.launcher.spawned()[0].devices,
        devices(&["/dev/nvidia4"])
    );
    let r = c
        .request(Request::Restore(RestoreRequest {
            job: JobId(1),
            uid: Uid(uid()),
            image_dir,
            gpus: vec![GpuIndex(1), GpuIndex(0)],
            leave_stopped: false,
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
    assert_eq!(
        env.launcher.restored()[0].devices,
        devices(&["/dev/nvidia2", "/dev/nvidia4"])
    );

    // A CPU job: no NVIDIA device at all.
    let mut cpu = env.spawn_req(2);
    cpu.gpus.clear();
    assert!(matches!(
        c.request(Request::Spawn(cpu)).await,
        ResponseResult::Spawned { .. }
    ));
    assert_eq!(env.launcher.spawned()[1].devices, Some(Vec::new()));
    assert_eq!(env.launcher.spawned()[1].envs["CUDA_VISIBLE_DEVICES"], "");

    // A GPU the driver does not know adds no node of its own.
    let mut unknown = env.spawn_req(3);
    unknown.gpus = vec![GpuIndex(7)];
    assert!(matches!(
        c.request(Request::Spawn(unknown)).await,
        ResponseResult::Spawned { .. }
    ));
    assert_eq!(env.launcher.spawned()[2].devices, devices(&[]));

    // `gpu_device_isolation = false`: no device restrictions.
    drop(c);
    let mut cfg = with_mode(config(&env.root, uid()), &env.root, env.mode);
    cfg.gpu_device_isolation = false;
    env.restart_with(cfg).await;
    let mut c = env.client().await;
    assert!(matches!(
        c.request(Request::Spawn(env.spawn_req(4))).await,
        ResponseResult::Spawned { .. }
    ));
    assert_eq!(env.launcher.spawned()[3].devices, None);
}

/// A job paused (stopped) when it was dumped is restored stopped when the
/// daemon asks for it: the restore hook stops the tasks before CRIU lets
/// them go, the CUDA state is restored with only the restore threads
/// running (no runnable check, which a stopped process fails), and the
/// helper sends no signal of its own. A later SIGCONT resumes it, and its
/// exit is reported as usual.
#[tokio::test]
async fn a_job_paused_at_its_dump_is_restored_stopped() {
    let env = Env::new().await;
    let mut c = env.client().await;
    let job = JobId(3);
    assert!(matches!(
        c.request(Request::Spawn(env.spawn_req(3))).await,
        ResponseResult::Spawned { .. }
    ));
    let signal_job = |signal| Request::Signal { job, signal };
    assert_eq!(
        c.request(signal_job(signal::SIGSTOP)).await,
        ResponseResult::Ok
    );
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
    assert!(matches!(
        c.event().await.event,
        Event::CheckpointFinished {
            outcome: CheckpointOutcome::Success { .. },
            ..
        }
    ));
    env.exec.lock().unwrap().actions.clear();

    let r = c
        .request(Request::Restore(RestoreRequest {
            job,
            uid: Uid(uid()),
            image_dir,
            gpus: vec![GpuIndex(0)],
            leave_stopped: true,
        }))
        .await;
    assert_eq!(r, ResponseResult::Accepted);
    let ev = c.event().await;
    let Event::RestoreFinished {
        outcome: RestoreOutcome::Success { pid, stopped: true },
        ..
    } = ev.event
    else {
        panic!("{ev:?}")
    };
    assert_eq!(c.running().await, [(job, RunningPhase::Stopped)]);
    let check: gpueue_helper::access::RestoredCheck = serde_json::from_slice(
        &std::fs::read(env.root.join("state/jobs/3/restore-check.json")).unwrap(),
    )
    .unwrap();
    assert!(check.stop, "the restore hook stops the tasks");
    assert_eq!(env.host.cuda_released(), [vec![pid]]);
    assert_eq!(
        env.exec.lock().unwrap().actions,
        [
            Some(Action::CudaRestore { pid }),
            Some(Action::CudaUnlock { pid }),
        ]
    );
    let unit = env.launcher.unit(job).unwrap();
    assert_eq!(unit.name, "gpueue-job-3-r1.service");
    assert!(unit.signals.is_empty(), "{:?}", unit.signals);

    // `gpueue start`.
    assert_eq!(
        c.request(signal_job(signal::SIGCONT)).await,
        ResponseResult::Ok
    );
    assert_eq!(c.running().await, [(job, RunningPhase::Running)]);
    assert_eq!(env.launcher.unit(job).unwrap().signals, [signal::SIGCONT]);
    env.launcher.exit(job, ExitStatus::Exited { code: 0 });
    assert!(matches!(
        c.event().await.event,
        Event::JobExited {
            status: ExitStatus::Exited { code: 0 },
            ..
        }
    ));
}

/// Should a task of a job restored stopped still run once `criu restore`
/// is done (job code has run), the restore fails: the job is killed and
/// the image kept for another try.
#[tokio::test]
async fn a_restored_job_that_did_not_stop_is_killed() {
    let env = Env::new().await;
    let mut c = env.client().await;
    let image_dir = spawn_and_dump(&env, &mut c, 5).await;
    env.host.set_unstopped_threads(vec![(10_005, 10_006)]);
    let Request::Restore(mut req) = restore_req(5, &image_dir) else {
        unreachable!()
    };
    req.leave_stopped = true;
    assert_eq!(
        c.request(Request::Restore(req)).await,
        ResponseResult::Accepted
    );
    let ev = c.event().await;
    let Event::RestoreFinished {
        outcome: RestoreOutcome::Failed { reason, code: None },
        ..
    } = ev.event
    else {
        panic!("{ev:?}")
    };
    assert!(reason.contains("did not stop"), "{reason}");
    assert!(reason.contains("the image is kept"), "{reason}");
    assert!(c.running().await.is_empty());
    assert!(env.launcher.unit(JobId(5)).is_none(), "unit released");
    assert!(matches!(
        CheckpointDir::for_job(&env.base(), 5).status(),
        DirStatus::Complete(_)
    ));
    // It can be restored again once it stops as it should.
    env.host.set_unstopped_threads(Vec::new());
    let Request::Restore(mut req) = restore_req(5, &image_dir) else {
        unreachable!()
    };
    req.leave_stopped = true;
    assert_eq!(
        c.request(Request::Restore(req)).await,
        ResponseResult::Accepted
    );
    let ev = c.event().await;
    assert!(
        matches!(
            ev.event,
            Event::RestoreFinished {
                outcome: RestoreOutcome::Success { stopped: true, .. },
                ..
            }
        ),
        "{ev:?}"
    );
    assert_eq!(c.running().await, [(JobId(5), RunningPhase::Stopped)]);
}

/// A dumped job stays listed, as checkpointing, until its outcome is sent:
/// releasing its unit may take a while, and the daemon takes a job the
/// helper neither lists nor has reported for lost (deleting its image).
#[tokio::test]
async fn dumped_job_is_listed_until_its_outcome_is_sent() {
    let env = Env::new().await;
    let mut c = env.client().await;
    let job = JobId(3);
    assert!(matches!(
        c.request(Request::Spawn(env.spawn_req(3))).await,
        ResponseResult::Spawned { .. }
    ));
    env.launcher.hold_releases();
    let r = c
        .request(Request::Checkpoint {
            job,
            image_dir: env.base().join("3"),
            options: Default::default(),
        })
        .await;
    assert_eq!(r, ResponseResult::Accepted);
    env.launcher.wait_held_releases(1).await;
    assert_eq!(c.running().await, [(job, RunningPhase::Checkpointing)]);
    assert!(c.events.is_empty(), "{:?}", c.events);

    env.launcher.let_releases_go();
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
    assert!(c.running().await.is_empty());
    assert!(env.launcher.unit(job).is_none(), "dumped unit released");
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
        leave_stopped: false,
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

/// A refusal of the check of the restored tasks is reported as such,
/// whatever its text says (here a path that reads like "the check did not
/// run"), not as a failure of criu.
#[tokio::test]
async fn a_refused_restore_reports_the_refusal_whatever_it_says() {
    let env = Env::new().await;
    let mut c = env.client().await;
    let image_dir = spawn_and_dump(&env, &mut c, 3).await;
    env.launcher.set_restore_check_problems(vec![
        "fd 3 of process 4242 (/srv/the check did not run): its owner may not read it".into(),
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
    assert!(
        reason.contains("files or credentials its owner may not have"),
        "{reason}"
    );
    assert!(reason.contains("/srv/the check did not run"), "{reason}");
    assert!(!reason.contains("criu restore failed"), "{reason}");
}

/// A tiny `files.img` with the given regular file paths and bound socket
/// paths (see `gpueue_helper::images`).
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
            _ => None,
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
async fn job_policy_refuses_spawns_and_restores() {
    let mut env = Env::new().await;
    let mut c = env.client().await;
    env.host.add_user(uid(), "tester", gid(), &[gid(), 4242]);
    env.host.set_process_groups(Some(vec![gid(), 4242]));
    // Dumped before the policy denied group 4242.
    let image_dir = spawn_and_dump(&env, &mut c, 3).await;

    let group_file = env.root.join("group");
    std::fs::write(&group_file, "docker:x:4242:tester\n").unwrap();
    let policy = |min_uid: u32| gpueue_helper::validate::JobPolicy {
        min_uid,
        min_uid_source: gpueue_config::job_policy::MinUidSource::Config,
        denied_groups: vec!["docker".into()],
        allowed_users: vec![],
        group_file: group_file.clone(),
    };
    let mut cfg = with_mode(config(&env.root, uid()), &env.root, env.mode);
    cfg.job_policy = policy(uid() + 1);
    env.restart_with(cfg.clone()).await;
    let mut c = env.client().await;
    let r = c.request(Request::Spawn(env.spawn_req(1))).await;
    assert_eq!(err_code(&r), HelperErrorCode::PolicyDenied, "{r:?}");
    let ResponseResult::Error { message, .. } = r else {
        unreachable!()
    };
    assert_eq!(
        message,
        format!(
            "gpueue refuses to run jobs as tester: uid {} is below job_min_uid {} (job_min_uid \
             in the configuration)",
            uid(),
            uid() + 1
        )
    );

    cfg.job_policy = policy(1);
    env.restart_with(cfg).await;
    let mut c = env.client().await;
    // The denied group is dropped from a new job ...
    let r = c.request(Request::Spawn(env.spawn_req(1))).await;
    assert!(matches!(r, ResponseResult::Spawned { .. }), "{r:?}");
    // ... but CRIU would restore it into the checkpointed one.
    let r = c.request(restore_req(3, &image_dir)).await;
    assert_eq!(err_code(&r), HelperErrorCode::PolicyDenied, "{r:?}");
    let ResponseResult::Error { message, .. } = r else {
        unreachable!()
    };
    assert!(
        message.contains("[\"docker\"]") && message.contains("job_denied_groups"),
        "{message}"
    );
}

#[tokio::test]
async fn restore_refuses_groups_the_owner_left_since_the_dump() {
    let env = Env::new().await;
    let mut c = env.client().await;
    env.host.set_process_groups(Some(vec![gid(), 4242]));
    let image_dir = spawn_and_dump(&env, &mut c, 3).await;
    let owner: gpueue_helper::jobs::JobRecord = serde_json::from_slice(
        &std::fs::read(image_dir.join(gpueue_helper::engine::OWNER_FILE)).unwrap(),
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
    let ck = gpueue_helper::config::default_checkpoint_config();
    assert!(ck.criu.link_remap);
    let none = gpueue_helper::engine::dump_options(&ck, &DumpHazards::default());
    assert_eq!(none, ck);
    let h = DumpHazards {
        deleted_files: vec![("/home/a/x".into(), 5 << 20), ("/home/a/y".into(), 10)],
        ..Default::default()
    };
    let ghost = gpueue_helper::engine::dump_options(&ck, &h);
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
    let files = gpueue_helper::jobs::JobFiles::new(&env.root.join("state/jobs"), JobId(3));
    let mut record = files.read_record().unwrap();
    record.restores = 1;
    record.unit = "gpueue-job-3-r1.service".into();
    files.write_record(&record).unwrap();
    env.launcher
        .restore(&gpueue_helper::launcher::RestoreSpec {
            job: JobId(3),
            unit: record.unit.clone(),
            command: gpueue_checkpoint::CommandSpec::new("criu"),
            pidfile: image_dir.join("restore.pid"),
            timeout: Duration::from_secs(1),
            criu_root: None,
            devices: None,
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
                leave_stopped: false,
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

/// CRIU cannot dump nested PID or user namespaces (`unshare`, `bwrap`),
/// and the helper's scans would not see their processes: refused up front.
#[tokio::test]
async fn dump_refuses_a_job_with_nested_namespaces() {
    let env = Env::new().await;
    let mut c = env.client().await;
    let job = JobId(5);
    assert!(matches!(
        c.request(Request::Spawn(env.spawn_req(5))).await,
        ResponseResult::Spawned { .. }
    ));
    env.host
        .set_nested_namespace_processes(vec!["4242 (unshare)".into()]);
    let r = c
        .request(Request::Checkpoint {
            job,
            image_dir: env.base().join("5"),
            options: Default::default(),
        })
        .await;
    let ResponseResult::Error {
        code: HelperErrorCode::CheckpointRefused,
        message,
    } = r
    else {
        panic!("{r:?}")
    };
    assert!(
        message.contains("PID or user namespace of their own (4242 (unshare))"),
        "{message}"
    );
    c.no_event().await;
    assert!(env.exec.lock().unwrap().actions.is_empty());
    assert_eq!(c.running().await, [(job, RunningPhase::Running)]);
}

/// CRIU binds the external mount sources of an image by their host paths,
/// as root, following symlinks: a restore whose sources lie below a
/// directory someone other than root can change is refused.
#[tokio::test]
async fn restore_refuses_bind_sources_that_others_can_redirect() {
    use gpueue_helper::engine::{MOUNTS_FILE, SavedMounts};
    let env = Env::new().await;
    let mut c = env.client().await;
    let image_dir = spawn_and_dump(&env, &mut c, 5).await;
    let path = CheckpointDir::for_job(&env.base(), 5)
        .root()
        .join(MOUNTS_FILE);
    let SavedMounts::Full {
        mount_points,
        open_files,
        held_files,
        detached_mounts,
        ..
    } = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap()
    else {
        panic!("old format")
    };
    let src = env.root.to_str().unwrap().to_owned();
    let saved = SavedMounts::Full {
        mount_points,
        external_sources: vec![src.clone()],
        source_mounts: Vec::new(),
        open_files,
        held_files,
        detached_mounts,
    };
    std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
    env.host.set_bind_source_problems(vec![(
        src.clone(),
        "/home/alice is not a directory that only root can change".into(),
    )]);
    let r = c.request(restore_req(5, &image_dir)).await;
    let ResponseResult::Error {
        code: HelperErrorCode::InvalidRequest,
        message,
    } = r
    else {
        panic!("{r:?}")
    };
    assert!(
        message.contains(&format!(
            "{src}: /home/alice is not a directory that only root can change"
        )),
        "{message}"
    );
    assert!(env.launcher.restored().is_empty());
    env.host.set_bind_source_problems(Vec::new());
    assert_eq!(
        c.request(restore_req(5, &image_dir)).await,
        ResponseResult::Accepted
    );
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

/// A FUSE mount that is gone from the host (its daemon exited) is left out
/// of the dump of a job that does not use it, and restored as an empty,
/// inaccessible directory while it is not back (once its mount point
/// exists); mounts below it are left out. A job that uses it is not
/// dumped, and the reason names the mount.
#[tokio::test]
async fn dead_mounts_are_detached_unless_the_job_uses_them() {
    use gpueue_helper::deadmounts::{DeadMount, DeadMounts, DetachedMount, PLACEHOLDER_DIR};
    use gpueue_helper::engine::{MOUNTS_FILE, SavedMounts};
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new().await;
    let mut c = env.client().await;
    let mp = env.root.join("nfs").to_str().unwrap().to_owned();
    let below = format!("{mp}/x");
    let dead = DeadMounts {
        dead: vec![DeadMount {
            id: 101,
            mount_point: mp.clone(),
            fstype: "fuse.sshfs".into(),
            dev: "0:60".into(),
            why: "is no longer mounted on the host".into(),
        }],
        below: vec![below.clone()],
        used: Vec::new(),
    };
    env.host.set_dead_mounts(dead.clone());
    // A restore needs every recorded mount point on the host: neither the
    // detached one nor the one below it is recorded.
    env.host
        .set_mount_points(vec!["/".into(), mp.clone(), below.clone()]);
    let image_dir = spawn_and_dump(&env, &mut c, 5).await;
    let args = env.exec.lock().unwrap().dump_args.clone();
    let args = args.last().unwrap();
    let pair = |a: &str, b: &str| args.windows(2).any(|w| w[0] == a && w[1] == b);
    assert!(
        pair("--external", &format!("mnt[{mp}]:gpueue-detached-0")),
        "{args:?}"
    );
    assert!(pair("--skip-mnt", &below), "{args:?}");
    let saved: SavedMounts = serde_json::from_slice(
        &std::fs::read(
            CheckpointDir::for_job(&env.base(), 5)
                .root()
                .join(MOUNTS_FILE),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        saved.detached_mounts(),
        [DetachedMount {
            mount_point: mp.clone(),
            fstype: "fuse.sshfs".into(),
            key: "gpueue-detached-0".into(),
            why: "is no longer mounted on the host".into(),
        }]
    );
    assert_eq!(saved.required_paths(), ["/"]);

    // CRIU needs the mount point itself.
    let r = c.request(restore_req(5, &image_dir)).await;
    let ResponseResult::Error {
        code: HelperErrorCode::InvalidRequest,
        message,
    } = r
    else {
        panic!("{r:?}")
    };
    assert!(
        message.contains(&format!(
            "the mount points [\"{mp}\"] of filesystems that were left out of the checkpoint"
        )),
        "{message}"
    );
    std::fs::create_dir(&mp).unwrap();
    let r = c.request(restore_req(5, &image_dir)).await;
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
    let placeholder = env.base().join(PLACEHOLDER_DIR);
    let restored = env.launcher.restored();
    let args = restored[0].command.args_lossy();
    let want = format!("mnt[gpueue-detached-0]:{}", placeholder.display());
    assert!(
        args.windows(2)
            .any(|w| w[0] == "--external" && w[1] == want),
        "{args:?}"
    );
    let mode = std::fs::metadata(&placeholder)
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o7777, 0);

    // A job that uses the mount.
    let mut used = dead;
    used.used = vec![(mp.clone(), format!("the working directory {mp}/run"))];
    env.host.set_dead_mounts(used);
    assert!(matches!(
        c.request(Request::Spawn(env.spawn_req(6))).await,
        ResponseResult::Spawned { .. }
    ));
    let dumps = env.exec.lock().unwrap().dump_args.len();
    let r = c
        .request(Request::Checkpoint {
            job: JobId(6),
            image_dir: env.base().join("6"),
            options: Default::default(),
        })
        .await;
    let ResponseResult::Error {
        code: HelperErrorCode::CheckpointRefused,
        message,
    } = r
    else {
        panic!("{r:?}")
    };
    assert!(
        message.contains(&format!(
            "{mp} (fuse.sshfs) is no longer mounted on the host; the job uses it: the working \
             directory {mp}/run"
        )),
        "{message}"
    );
    assert_eq!(env.exec.lock().unwrap().dump_args.len(), dumps);
    assert!(!env.base().join("6").exists());
    assert_eq!(c.running().await.len(), 2, "both jobs run");
    std::fs::set_permissions(&placeholder, std::fs::Permissions::from_mode(0o700)).unwrap();
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
                leave_stopped: false,
            }))
            .await
        ),
        HelperErrorCode::ShuttingDown
    );
    assert_eq!(
        err_code(
            &c.request(Request::Checkpoint {
                job: JobId(11),
                image_dir: env.base().join("11"),
                options: Default::default(),
            })
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

/// A checkpoint asked for while an OS shutdown waits for the operations in
/// flight is refused, and the job is dumped by the shutdown, not left out
/// of its manifest.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_checkpoint_asked_for_during_a_shutdown_is_refused_and_the_job_dumped() {
    let env = Env::new().await;
    let mut c = env.client().await;
    assert!(matches!(
        c.request(Request::Spawn(env.spawn_req(1))).await,
        ResponseResult::Spawned { .. }
    ));
    // A spawn in flight holds the shutdown up.
    env.launcher.set_delay(Some(Duration::from_millis(500)));
    let spawn = c.send_request(Request::Spawn(env.spawn_req(2))).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let shutdown = tokio::spawn({
        let helper = env.helper.clone();
        async move { helper.shutdown(|_| {}, |_| {}).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        err_code(
            &c.request(Request::Checkpoint {
                job: JobId(1),
                image_dir: env.base().join("1"),
                options: Default::default(),
            })
            .await
        ),
        HelperErrorCode::ShuttingDown
    );
    assert!(matches!(
        c.response(spawn).await,
        ResponseResult::Spawned { .. }
    ));
    let manifest = tokio::time::timeout(WAIT, shutdown).await.unwrap().unwrap();
    let mut dumped: Vec<JobId> = manifest
        .jobs
        .iter()
        .filter(|e| matches!(e.result, ManifestResult::Checkpointed { .. }))
        .map(|e| e.job)
        .collect();
    dumped.sort();
    assert_eq!(dumped, [JobId(1), JobId(2)], "{manifest:?}");
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
            leave_stopped: false,
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
                leave_stopped: false,
            }))
            .await
        ),
        HelperErrorCode::ShuttingDown
    );
    // ...so are checkpoints (a dump that began now could be cut off by
    // the stop)...
    assert_eq!(
        err_code(
            &c.request(Request::Checkpoint {
                job: JobId(1),
                image_dir: env.base().join("1"),
                options: Default::default(),
            })
            .await
        ),
        HelperErrorCode::ShuttingDown
    );
    // ...and the running job is left alone.
    assert_eq!(c.running().await, [(JobId(1), RunningPhase::Running)]);
    assert!(!env.base().join("1").exists());
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
        leave_stopped: false,
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

/// The restore checks run off the async workers (an access check stuck on
/// a filesystem that does not answer leaves the helper responsive), and
/// the job is claimed before them: its checkpoint cannot be deleted while
/// it is checked. A failed check releases the claim.
#[tokio::test]
async fn restore_checks_do_not_block_the_helper_and_hold_off_deletes() {
    let env = Env::new().await;
    let mut c = env.client().await;
    let image_dir = spawn_and_dump(&env, &mut c, 3).await;
    let images = CheckpointDir::for_job(&env.base(), 3).images_dir();
    std::fs::write(
        images.join("files.img"),
        files_image(&["/home/alice/x/data"], &[]),
    )
    .unwrap();
    env.host.hold_access_checks(true);
    let restore = c.send_request(restore_req(3, &image_dir)).await;
    for _ in 0..500 {
        if !env.host.verified().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!env.host.verified().is_empty(), "the check never started");
    // Still answering while the check hangs.
    assert_eq!(c.running().await, [(JobId(3), RunningPhase::Restoring)]);
    let del = Request::DeleteCheckpoint {
        job: JobId(3),
        image_dir: image_dir.clone(),
    };
    assert_eq!(
        err_code(&c.request(del.clone()).await),
        HelperErrorCode::Busy
    );
    assert!(matches!(
        CheckpointDir::for_job(&env.base(), 3).status(),
        DirStatus::Complete(_)
    ));
    // The check fails: the claim goes, and the delete works.
    env.host.set_access_problems(
        vec!["/home/alice/x/data: a directory on the way is a symlink".into()],
        vec![],
    );
    env.host.hold_access_checks(false);
    let r = c.response(restore).await;
    assert_eq!(err_code(&r), HelperErrorCode::PermissionDenied, "{r:?}");
    assert!(c.running().await.is_empty());
    assert_eq!(c.request(del).await, ResponseResult::Ok);
}

/// After a successful `criu dump` the job's processes are gone: a failure
/// to finalize the checkpoint keeps the image, reports it apart, and the
/// next restore finalizes it.
#[tokio::test]
async fn a_dump_that_cannot_be_finalized_keeps_its_image() {
    let env = Env::new().await;
    let mut c = env.client().await;
    assert!(matches!(
        c.request(Request::Spawn(env.spawn_req(4))).await,
        ResponseResult::Spawned { .. }
    ));
    // Writing the COMPLETE marker fails (its temporary file cannot be
    // created).
    env.exec.lock().unwrap().after_dump = Some(|images| {
        std::fs::create_dir(images.parent().unwrap().join("COMPLETE.tmp")).unwrap();
    });
    let image_dir = env.base().join("4");
    assert_eq!(
        c.request(Request::Checkpoint {
            job: JobId(4),
            image_dir: image_dir.clone(),
            options: Default::default(),
        })
        .await,
        ResponseResult::Accepted
    );
    let ev = c.event().await;
    let Event::CheckpointFinished {
        outcome:
            CheckpointOutcome::Failed {
                reason,
                code: Some(HelperErrorCode::CheckpointUnfinalized),
            },
        ..
    } = ev.event
    else {
        panic!("{ev:?}")
    };
    assert!(reason.contains("image is kept"), "{reason}");
    let dir = CheckpointDir::for_job(&env.base(), 4);
    assert!(dir.is_unfinalized_dump(), "{:?}", dir.status());
    assert!(c.running().await.is_empty());
    assert!(
        env.launcher.unit(JobId(4)).is_none(),
        "the unit is released"
    );
    // A spawn of the same id is refused (the job is checkpointed).
    assert_eq!(
        err_code(&c.request(Request::Spawn(env.spawn_req(4))).await),
        HelperErrorCode::Busy
    );
    // Still failing: the restore fails and keeps the image.
    let r = c.request(restore_req(4, &image_dir)).await;
    assert_eq!(err_code(&r), HelperErrorCode::Internal, "{r:?}");
    assert!(dir.is_unfinalized_dump());
    // Fixed: the restore finalizes it first.
    std::fs::remove_dir(dir.root().join("COMPLETE.tmp")).unwrap();
    assert_eq!(
        c.request(restore_req(4, &image_dir)).await,
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

/// A helper that stopped between a successful `criu dump` and the end of
/// its finalization: the next start finalizes the image and reports the
/// checkpoint, instead of removing the "incomplete" directory and
/// reporting the job as exited.
#[tokio::test]
async fn a_dump_interrupted_before_its_finalization_is_finished_at_start() {
    let mut env = Env::new().await;
    let mut c = env.client().await;
    assert!(matches!(
        c.request(Request::Spawn(env.spawn_req(5))).await,
        ResponseResult::Spawned { .. }
    ));
    // Make the finalization fail, then turn it into a "crash": put the
    // record and the directory back into the state right after CRIU
    // exited.
    env.exec.lock().unwrap().after_dump = Some(|images| {
        std::fs::create_dir(images.parent().unwrap().join("COMPLETE.tmp")).unwrap();
    });
    c.request(Request::Checkpoint {
        job: JobId(5),
        image_dir: env.base().join("5"),
        options: Default::default(),
    })
    .await;
    c.event().await;
    c.request(Request::AckEvents {
        up_to: EventSeq(u64::MAX),
    })
    .await;
    drop(c);
    let _ = env.stop.send(true);
    let dir = CheckpointDir::for_job(&env.base(), 5);
    std::fs::remove_dir(dir.root().join("COMPLETE.tmp")).unwrap();
    std::fs::remove_file(dir.dumped_marker_path()).unwrap();
    std::fs::write(dir.dump_log(), "(00.5) Dumping finished successfully\n").unwrap();
    let files = gpueue_helper::jobs::JobFiles::new(&env.root.join("state/jobs"), JobId(5));
    let mut record = files.read_record().unwrap();
    record.phase = gpueue_helper::jobs::RecordPhase::Live;
    record.dumping = true;
    files.write_record(&record).unwrap();
    env.restart().await;
    let mut c = env.client().await;
    let ev = c.event().await;
    assert!(
        matches!(
            ev.event,
            Event::CheckpointFinished {
                job: JobId(5),
                outcome: CheckpointOutcome::Success { .. },
            }
        ),
        "{ev:?}"
    );
    assert!(matches!(dir.status(), DirStatus::Complete(_)));
    let record = files.read_record().unwrap();
    assert_eq!(record.phase, gpueue_helper::jobs::RecordPhase::Checkpointed);
    assert!(!record.dumping);
    assert!(c.running().await.is_empty());
}

/// A restore that succeeded but could not record that the job runs again
/// (e.g. a full disk) is undone: the restored job is killed and the image
/// is kept. Otherwise the record would still say "being restored" with the
/// image gone, and the next start of the helper would kill the job (see
/// `Helper::recover`), leaving nothing to restore it from.
#[tokio::test]
async fn a_restore_that_cannot_be_recorded_is_undone_and_keeps_the_image() {
    let env = Env::new().await;
    let mut c = env.client().await;
    let image_dir = spawn_and_dump(&env, &mut c, 3).await;
    // Once the restored unit runs, writes of the job record fail.
    env.launcher.set_after_restore(Some(|spec| {
        let script = spec.command.value_of("--action-script").unwrap();
        let job_dir = Path::new(script).parent().unwrap();
        std::fs::create_dir(job_dir.join("job.json.tmp")).unwrap();
    }));
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
    assert!(reason.contains("image is kept"), "{reason}");
    assert!(c.running().await.is_empty());
    assert!(
        env.launcher.unit(JobId(3)).is_none(),
        "restored unit killed and released"
    );
    let dir = CheckpointDir::for_job(&env.base(), 3);
    assert!(matches!(dir.status(), DirStatus::Complete(_)));
    // Once the record can be written again, the job restores.
    env.launcher.set_after_restore(None);
    std::fs::remove_dir(env.root.join("state/jobs/3/job.json.tmp")).unwrap();
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
    assert_eq!(c.running().await, [(JobId(3), RunningPhase::Running)]);
}

/// Checkpoints restored job 3 again, then puts the record back into its
/// state during the dump (as a helper that stopped right after CRIU exited
/// would leave it) and starts a unit of the job's last name, as systemd may
/// still report the dumped unit (deactivating). Returns the job's files.
async fn redump_restored_job_and_stop(env: &Env, c: &mut Client) -> gpueue_helper::jobs::JobFiles {
    let image_dir = env.base().join("3");
    let r = c
        .request(Request::Checkpoint {
            job: JobId(3),
            image_dir,
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
    c.request(Request::AckEvents {
        up_to: EventSeq(u64::MAX),
    })
    .await;
    let _ = env.stop.send(true);
    let files = gpueue_helper::jobs::JobFiles::new(&env.root.join("state/jobs"), JobId(3));
    let mut record = files.read_record().unwrap();
    assert_eq!(record.unit_kind, gpueue_helper::jobs::UnitKind::Restored);
    record.phase = gpueue_helper::jobs::RecordPhase::Live;
    record.dumping = true;
    files.write_record(&record).unwrap();
    files
}

/// Starts a unit named `unit` that the fake launcher reports as running.
async fn start_unit(env: &Env, unit: &str) {
    env.launcher
        .restore(&gpueue_helper::launcher::RestoreSpec {
            job: JobId(3),
            unit: unit.into(),
            command: gpueue_checkpoint::CommandSpec::new("criu"),
            pidfile: env.root.join("unused.pid"),
            timeout: Duration::from_secs(1),
            criu_root: None,
            devices: None,
        })
        .await
        .unwrap();
}

/// A restored job checkpointed again, with the helper stopping after the
/// dump but before it recorded it: the new image is the only copy of the
/// job and is kept, even if its unit still looks running at the next start.
#[tokio::test]
async fn a_new_dump_of_a_restored_job_survives_a_helper_restart() {
    let mut env = Env::new().await;
    let mut c = env.client().await;
    let image_dir = spawn_and_dump(&env, &mut c, 3).await;
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
    let files = redump_restored_job_and_stop(&env, &mut c).await;
    drop(c);
    start_unit(&env, "gpueue-job-3-r1.service").await;
    env.restart().await;
    let mut c = env.client().await;
    let ev = c.event().await;
    assert!(
        matches!(
            ev.event,
            Event::CheckpointFinished {
                job: JobId(3),
                outcome: CheckpointOutcome::Success { .. },
            }
        ),
        "{ev:?}"
    );
    let dir = CheckpointDir::for_job(&env.base(), 3);
    assert!(matches!(dir.status(), DirStatus::Complete(_)));
    let record = files.read_record().unwrap();
    assert_eq!(record.phase, gpueue_helper::jobs::RecordPhase::Checkpointed);
    assert!(!record.dumping);
    assert!(c.running().await.is_empty());
    assert!(
        env.launcher.unit(JobId(3)).is_none(),
        "dumped unit released"
    );
    // And it restores from the new image.
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

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// The image a job was restored from, left behind because removing it
/// failed, is not taken for a new dump of the job: a helper that stopped
/// while a dump (refused because of that image) was set up finds the job
/// running and adopts it.
#[tokio::test]
async fn the_image_a_running_job_was_restored_from_is_not_a_new_dump() {
    let mut env = Env::new().await;
    let mut c = env.client().await;
    let image_dir = spawn_and_dump(&env, &mut c, 3).await;
    let saved = env.root.join("saved-image");
    copy_dir(&image_dir, &saved);
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
    c.request(Request::AckEvents {
        up_to: EventSeq(u64::MAX),
    })
    .await;
    drop(c);
    let _ = env.stop.send(true);
    // What criu restore leaves in the image it restored from.
    copy_dir(&saved, &image_dir);
    std::fs::write(
        CheckpointDir::for_job(&env.base(), 3).restore_log(),
        "(00.1) Restore finished successfully\n",
    )
    .unwrap();
    let files = gpueue_helper::jobs::JobFiles::new(&env.root.join("state/jobs"), JobId(3));
    let mut record = files.read_record().unwrap();
    record.dumping = true;
    files.write_record(&record).unwrap();
    env.restart().await;
    let mut c = env.client().await;
    c.no_event().await;
    assert_eq!(c.running().await, [(JobId(3), RunningPhase::Running)]);
    assert!(!files.read_record().unwrap().dumping);
    assert_eq!(
        CheckpointDir::for_job(&env.base(), 3).status(),
        DirStatus::Missing,
        "the stale image is removed"
    );
}

/// Restores job `job` from its image and then puts the image back, with
/// the log `criu restore` writes into it, as a restore whose removal of the
/// image failed (or a helper that stopped before it) leaves it.
async fn restore_and_leave_the_image(env: &Env, c: &mut Client, job: u64) {
    let image_dir = env.base().join(job.to_string());
    let saved = env.root.join(format!("saved-image-{job}"));
    copy_dir(&image_dir, &saved);
    assert_eq!(
        c.request(restore_req(job, &image_dir)).await,
        ResponseResult::Accepted
    );
    assert!(matches!(
        c.event().await.event,
        Event::RestoreFinished {
            outcome: RestoreOutcome::Success { .. },
            ..
        }
    ));
    copy_dir(&saved, &image_dir);
    std::fs::write(
        CheckpointDir::for_job(&env.base(), job).restore_log(),
        "(00.1) Restore finished successfully\n",
    )
    .unwrap();
}

async fn checkpoint_succeeds(c: &mut Client, env: &Env, job: u64) {
    let r = c
        .request(Request::Checkpoint {
            job: JobId(job),
            image_dir: env.base().join(job.to_string()),
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

/// The image a running job was restored from, left behind, does not
/// refuse the job's next dump: it is stale and replaced by the new image.
#[tokio::test]
async fn a_left_over_image_a_job_was_restored_from_does_not_block_its_next_dump() {
    let env = Env::new().await;
    let mut c = env.client().await;
    spawn_and_dump(&env, &mut c, 3).await;
    restore_and_leave_the_image(&env, &mut c, 3).await;
    checkpoint_succeeds(&mut c, &env, 3).await;
    let dir = CheckpointDir::for_job(&env.base(), 3);
    assert!(matches!(dir.status(), DirStatus::Complete(_)));
    assert!(!dir.restore_log().exists(), "a fresh image");
    // A complete image the job was not restored from is never replaced.
    assert_eq!(
        c.request(restore_req(3, &env.base().join("3"))).await,
        ResponseResult::Accepted
    );
    assert!(matches!(
        c.event().await.event,
        Event::RestoreFinished {
            outcome: RestoreOutcome::Success { .. },
            ..
        }
    ));
    copy_dir(&env.root.join("saved-image-3"), &env.base().join("3"));
    assert!(!dir.restore_log().exists());
    let r = c
        .request(Request::Checkpoint {
            job: JobId(3),
            image_dir: env.base().join("3"),
            options: Default::default(),
        })
        .await;
    assert_eq!(r, ResponseResult::Accepted);
    let ev = c.event().await;
    assert!(
        matches!(
            &ev.event,
            Event::CheckpointFinished {
                outcome: CheckpointOutcome::Failed { .. },
                ..
            }
        ),
        "{ev:?}"
    );
    assert!(matches!(dir.status(), DirStatus::Complete(_)));
    assert_eq!(c.running().await, [(JobId(3), RunningPhase::Running)]);
}

/// The image a job was restored from, left behind, goes when the restored
/// job exits, whether the helper sees it exit or finds it exited at its
/// next start: the job id may be reused (`restart --in-place`) and dumped
/// again.
#[tokio::test]
async fn a_left_over_image_a_job_was_restored_from_goes_when_the_job_exits() {
    let mut env = Env::new().await;
    let mut c = env.client().await;
    for job in [3, 4] {
        spawn_and_dump(&env, &mut c, job).await;
        restore_and_leave_the_image(&env, &mut c, job).await;
    }
    env.launcher.exit(JobId(3), ExitStatus::Exited { code: 0 });
    let ev = c.event().await;
    assert!(
        matches!(ev.event, Event::JobExited { job: JobId(3), .. }),
        "{ev:?}"
    );
    assert_eq!(
        CheckpointDir::for_job(&env.base(), 3).status(),
        DirStatus::Missing
    );
    c.request(Request::AckEvents {
        up_to: EventSeq(u64::MAX),
    })
    .await;
    drop(c);
    let _ = env.stop.send(true);
    env.launcher.exit(JobId(4), ExitStatus::Exited { code: 0 });
    env.restart().await;
    let mut c = env.client().await;
    let ev = c.event().await;
    assert!(
        matches!(ev.event, Event::JobExited { job: JobId(4), .. }),
        "{ev:?}"
    );
    assert_eq!(
        CheckpointDir::for_job(&env.base(), 4).status(),
        DirStatus::Missing
    );
    // The ids are used again, and their jobs dumped.
    for job in [3, 4] {
        spawn_and_dump(&env, &mut c, job).await;
    }
}

/// A restore retries removing the image it restored from; one it could not
/// remove (here: half-way, the `COMPLETE` marker gone) is replaced by the
/// job's next dump.
#[tokio::test]
async fn an_image_a_restore_could_not_remove_is_replaced_by_the_next_dump() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new().await;
    let mut c = env.client().await;
    let image_dir = spawn_and_dump(&env, &mut c, 3).await;
    let dir = CheckpointDir::for_job(&env.base(), 3);
    // `criu restore` writes its log into the image (the fake does not).
    std::fs::write(dir.restore_log(), "(00.1) Restore finished successfully\n").unwrap();
    let images = dir.images_dir();
    std::fs::set_permissions(&images, std::fs::Permissions::from_mode(0o555)).unwrap();
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
    assert!(dir.images_dir().exists());
    assert!(!matches!(dir.status(), DirStatus::Complete(_)));
    std::fs::set_permissions(&images, std::fs::Permissions::from_mode(0o700)).unwrap();
    checkpoint_succeeds(&mut c, &env, 3).await;
    assert!(matches!(dir.status(), DirStatus::Complete(_)));
    assert!(!dir.restore_log().exists());

    // Removed only as far as the `COMPLETE` marker, the image looks like
    // a dump that was not finalized, which a new dump never replaces;
    // unless the job was restored from it.
    restore_and_leave_the_image(&env, &mut c, 3).await;
    std::fs::remove_file(dir.marker_path()).unwrap();
    assert!(dir.is_unfinalized_dump());
    checkpoint_succeeds(&mut c, &env, 3).await;
    assert!(matches!(dir.status(), DirStatus::Complete(_)));
    assert!(!dir.restore_log().exists());
}

/// An interrupted restore whose unit is gone (the helper stopped before it
/// started it, or systemd already cleaned it up) is reported too, once.
#[tokio::test]
async fn an_interrupted_restore_without_its_unit_is_reported() {
    let mut env = Env::new().await;
    let mut c = env.client().await;
    let image_dir = spawn_and_dump(&env, &mut c, 3).await;
    c.request(Request::AckEvents {
        up_to: EventSeq(u64::MAX),
    })
    .await;
    drop(c);
    let _ = env.stop.send(true);
    let files = gpueue_helper::jobs::JobFiles::new(&env.root.join("state/jobs"), JobId(3));
    let mut record = files.read_record().unwrap();
    record.restores = 1;
    record.unit = "gpueue-job-3-r1.service".into();
    record.restoring = true;
    files.write_record(&record).unwrap();
    env.restart().await;
    let mut c = env.client().await;
    let ev = c.event().await;
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
    assert!(!files.read_record().unwrap().restoring);
    c.request(Request::AckEvents {
        up_to: EventSeq(u64::MAX),
    })
    .await;
    drop(c);
    env.restart().await;
    let mut c = env.client().await;
    c.no_event().await;
    assert_eq!(
        c.request(restore_req(3, &image_dir)).await,
        ResponseResult::Accepted
    );
}

#[tokio::test]
async fn dump_counts_private_tmpfs_and_refuses_reuseport_sockets() {
    let env = Env::new().await;
    let mut c = env.client().await;
    assert!(matches!(
        c.request(Request::Spawn(env.spawn_req(3))).await,
        ResponseResult::Spawned { .. }
    ));
    let checkpoint = Request::Checkpoint {
        job: JobId(3),
        image_dir: env.base().join("3"),
        options: Default::default(),
    };
    // RSS is 1 MiB (FakeHost): 2 MiB free are enough without the tmpfs
    // contents, not with them.
    env.host.set_available(2 << 20);
    env.host.set_private_tmpfs_bytes(4 << 20);
    let r = c.request(checkpoint.clone()).await;
    let ResponseResult::Error {
        code: HelperErrorCode::CheckpointRefused,
        message,
    } = r
    else {
        panic!("{r:?}")
    };
    assert!(message.contains("private tmpfs"), "{message}");
    env.host.set_private_tmpfs_bytes(0);
    env.host.set_dump_hazards(DumpHazards {
        reuseport_sockets: vec!["0.0.0.0:8080".into()],
        ..Default::default()
    });
    let r = c.request(checkpoint).await;
    let ResponseResult::Error {
        code: HelperErrorCode::CheckpointRefused,
        message,
    } = r
    else {
        panic!("{r:?}")
    };
    assert!(
        message.contains("SO_REUSEPORT") && message.contains("0.0.0.0:8080"),
        "{message}"
    );
    assert_eq!(c.running().await, [(JobId(3), RunningPhase::Running)]);
    assert!(env.exec.lock().unwrap().actions.is_empty());
}

/// The files a job holds are listed again by the `post-dump` hook of
/// `criu dump`, while CRIU has the job stopped: a file the job opened after
/// the helper's list (made while it ran) is recorded as held too, with its
/// attributes, so that a restore after a reboot recognises it (e2e scenario
/// 15: a job that creates a state file on sshfs every few moments).
#[tokio::test]
async fn files_opened_just_before_the_dump_are_held() {
    use gpueue_helper::access::HeldFile;
    use gpueue_helper::engine::{DUMP_HOOK_SCRIPT, MOUNTS_FILE, SavedMounts};
    use std::os::unix::fs::MetadataExt;
    let env = Env::new().await;
    let mut c = env.client().await;
    let early = env.base().join("early.txt");
    let late = env.base().join("late.txt");
    std::fs::create_dir_all(env.base()).unwrap();
    std::fs::write(&early, "a").unwrap();
    std::fs::write(&late, "b").unwrap();
    let held = |p: &Path| {
        let m = std::fs::metadata(p).unwrap();
        HeldFile {
            path: p.to_str().unwrap().to_owned(),
            dev: m.dev(),
            ino: m.ino(),
            attrs: None,
        }
    };
    env.host.set_dump_hazards(DumpHazards {
        held_files: vec![held(&early)],
        ..Default::default()
    });
    env.exec.lock().unwrap().dump_hook_held = Some(vec![held(&early), held(&late)]);
    spawn_and_dump(&env, &mut c, 3).await;
    let dir = CheckpointDir::for_job(&env.base(), 3);
    let script = std::fs::read_to_string(dir.root().join(DUMP_HOOK_SCRIPT)).unwrap();
    assert!(script.contains(" dump-hook "), "{script}");
    let saved: SavedMounts =
        serde_json::from_slice(&std::fs::read(dir.root().join(MOUNTS_FILE)).unwrap()).unwrap();
    let files = saved.held_files();
    assert_eq!(files.len(), 2, "{files:?}");
    for (f, p) in files.iter().zip([&early, &late]) {
        assert_eq!(Path::new(&f.path), p.as_path());
        assert!(f.attrs.is_some(), "{f:?}");
    }
}

/// The `gpueue-helper` binary: `GPUEUE_HELPER_EXE` if set (the test
/// executables run elsewhere than where cargo built them, e.g. in the
/// NixOS VM of packaging/nix/tests/helper-tests.nix), else the one cargo
/// built for the tests.
fn helper_exe() -> PathBuf {
    let exe = std::env::var_os("GPUEUE_HELPER_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_gpueue-helper")));
    assert!(
        exe.is_file(),
        "{} is missing; set GPUEUE_HELPER_EXE to the gpueue-helper binary",
        exe.display()
    );
    exe
}

/// The real host looks the held files up in a `verify-access` child: the
/// very file gets its attributes, a replaced or missing one none.
#[test]
fn the_real_host_looks_held_files_up_in_a_child() {
    use gpueue_helper::access::HeldFile;
    use gpueue_helper::host::{HostProbe, SystemHost};
    use std::os::unix::fs::MetadataExt;
    let tmp = tempfile::tempdir().unwrap();
    let held = |name: &str| {
        let p = tmp.path().join(name);
        std::fs::write(&p, name).unwrap();
        let m = std::fs::metadata(&p).unwrap();
        HeldFile {
            path: p.to_str().unwrap().to_owned(),
            dev: m.dev(),
            ino: m.ino(),
            attrs: None,
        }
    };
    let same = held("same");
    let mut replaced = held("replaced");
    replaced.ino += 1;
    let mut missing = held("missing");
    missing.path.push_str(".gone");
    let host = SystemHost::new("nvidia-smi".into(), Default::default(), helper_exe());
    let attrs = host
        .held_file_attrs(&[same.clone(), replaced, missing])
        .unwrap();
    assert_eq!(attrs.len(), 3);
    assert_eq!(attrs[0], gpueue_helper::access::held_file_attrs(&same));
    assert!(attrs[0].is_some());
    assert_eq!(attrs[1], None);
    assert_eq!(attrs[2], None);
    assert_eq!(host.held_file_attrs(&[]).unwrap(), []);
}

/// The files the job held are looked up after the dump with a deadline
/// (on the real host in a child process, as a filesystem that does not
/// answer could block the lookup). If that fails, the image is kept as an
/// unfinalized dump, and a helper start goes on and finalizes it later.
#[tokio::test]
async fn a_failed_lookup_of_held_files_leaves_the_dump_unfinalized() {
    use gpueue_helper::access::HeldFile;
    use gpueue_helper::engine::{MOUNTS_FILE, SavedMounts};
    use std::os::unix::fs::MetadataExt;
    let mut env = Env::new().await;
    let mut c = env.client().await;
    let file = env.base().join("held.txt");
    std::fs::create_dir_all(env.base()).unwrap();
    std::fs::write(&file, "a").unwrap();
    let m = std::fs::metadata(&file).unwrap();
    env.host.set_dump_hazards(DumpHazards {
        held_files: vec![HeldFile {
            path: file.to_str().unwrap().to_owned(),
            dev: m.dev(),
            ino: m.ino(),
            attrs: None,
        }],
        ..Default::default()
    });
    env.host
        .set_held_attrs_failure(Some("the access check did not finish within 20s"));
    assert!(matches!(
        c.request(Request::Spawn(env.spawn_req(4))).await,
        ResponseResult::Spawned { .. }
    ));
    let r = c
        .request(Request::Checkpoint {
            job: JobId(4),
            image_dir: env.base().join("4"),
            options: Default::default(),
        })
        .await;
    assert_eq!(r, ResponseResult::Accepted);
    let ev = c.event().await;
    let Event::CheckpointFinished {
        outcome:
            CheckpointOutcome::Failed {
                reason,
                code: Some(HelperErrorCode::CheckpointUnfinalized),
            },
        ..
    } = ev.event
    else {
        panic!("{ev:?}")
    };
    assert!(reason.contains("did not finish within"), "{reason}");
    let dir = CheckpointDir::for_job(&env.base(), 4);
    assert!(dir.is_unfinalized_dump(), "{:?}", dir.status());
    c.request(Request::AckEvents {
        up_to: EventSeq(u64::MAX),
    })
    .await;
    drop(c);
    // A start while it still fails is not held up and keeps the image.
    env.restart().await;
    assert!(dir.is_unfinalized_dump(), "{:?}", dir.status());
    // Once it works, the next start finalizes the image.
    env.host.set_held_attrs_failure(None);
    env.restart().await;
    assert!(matches!(dir.status(), DirStatus::Complete(_)));
    let saved: SavedMounts =
        serde_json::from_slice(&std::fs::read(dir.root().join(MOUNTS_FILE)).unwrap()).unwrap();
    assert!(saved.held_files()[0].attrs.is_some());
}
