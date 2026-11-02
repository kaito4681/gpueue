//! Tests of preemption: checkpointing a job for the scheduler and
//! restoring it by itself (see [`super::preempt`]).

use std::future::Future;

use gpueue_core::model::{JobSpec, YieldMode};

use super::preempt::{PREEMPT_REFUSAL_MEMORY_MS, PREEMPTION_NOTE};
use super::*;
use crate::helper::{FakeHelper, FakeOutcome};
use crate::users::StaticUserDb;

/// The owner of the test jobs: the user running the tests, so that their
/// logs pass the daemon's ownership checks.
fn owner() -> Uid {
    Uid(nix::unistd::getuid().as_raw())
}

struct Setup {
    core: Arc<Core>,
    helper: FakeHelper,
    dir: tempfile::TempDir,
}

fn config(dir: &std::path::Path, preemption: bool) -> Config {
    let mut config = Config {
        data_dir: dir.to_owned(),
        grace_secs: 1,
        ..Config::default()
    };
    config.priority.preemption = preemption;
    config
}

fn core_on(helper: &FakeHelper, config: Config, store: Store) -> Arc<Core> {
    Core::new(
        config,
        store,
        Arc::new(helper.clone()),
        Arc::new(StaticUserDb::with_admins([])),
        HashMap::new(),
        GpuDetectionStatus {
            mode: crate::config::GpuDetection::Auto,
            source: gpueue_protocol::api::GpuSource::Config,
            nvml_error: None,
            gpus: Vec::new(),
        },
        false,
        Arc::new(crate::clock::SystemClock),
    )
}

/// A daemon with preemption enabled (or not), connected to a fake helper,
/// whose default group runs one job at a time.
async fn setup_with(preemption: bool) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let helper = FakeHelper::new();
    let store = Store::open(dir.path().join("state.sqlite")).unwrap();
    store.set_group_parallel("default", 1).unwrap();
    let core = core_on(&helper, config(dir.path(), preemption), store);
    start(&core).await;
    Setup { core, helper, dir }
}

async fn setup() -> Setup {
    setup_with(true).await
}

fn spec() -> JobSpec {
    JobSpec {
        command: "x".to_owned(),
        cwd: PathBuf::from("/"),
        envs: Default::default(),
        group: "default".to_owned(),
        gpus: Vec::new(),
        dependencies: Vec::new(),
        priority: 0,
        label: None,
        no_checkpoint: false,
        nice: 0,
        yield_mode: YieldMode::None,
    }
}

/// A job in `state` (`started_at` set) with an empty log, live in the
/// helper.
async fn live_job_with(s: &Setup, state: JobState, spec: JobSpec) -> Job {
    let job = s
        .core
        .db
        .call(move |db| {
            let job = db.insert_job(owner(), &spec, &state, 0)?;
            db.update_job_state(job.id, &state, Some(5), None)?;
            db.get_job(job.id).map(Option::unwrap)
        })
        .await
        .unwrap();
    let log = s.core.log_path(job.owner, job.id);
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, "epoch 1\n").unwrap();
    s.helper.insert_running(job.id);
    job
}

async fn live_job(s: &Setup, state: JobState) -> Job {
    live_job_with(s, state, spec()).await
}

async fn job(core: &Arc<Core>, id: JobId) -> Job {
    core.db.call(move |s| s.get_job(id).unwrap().unwrap()).await
}

/// Waits (5 s at most) until `pred` holds.
async fn eventually<F, Fut>(what: &str, mut pred: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    for _ in 0..250 {
        if pred().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("never: {what}");
}

async fn wait_for(core: &Arc<Core>, id: JobId, what: &str, pred: impl Fn(&Job) -> bool) -> Job {
    eventually(&format!("job {id}: {what}"), || async {
        pred(&job(core, id).await)
    })
    .await;
    job(core, id).await
}

async fn wait_state(core: &Arc<Core>, id: JobId, state: JobState) -> Job {
    let name = state.name();
    wait_for(core, id, name, move |job| job.state == state).await
}

fn restore_queued(job: &Job) -> bool {
    matches!(job.state, JobState::RestoreQueued { .. })
}

/// Starts the helper connection and the notice loop, and waits until the
/// daemon has reconciled.
async fn start(core: &Arc<Core>) {
    let notices = core.helper.connect();
    tokio::spawn(core.clone().notice_loop(notices));
    eventually("the daemon is reconciled", || async { core.helper_ready() }).await;
}

fn log(s: &Setup, job: &Job) -> String {
    std::fs::read_to_string(s.core.log_path(job.owner, job.id)).unwrap()
}

/// A preempted job is checkpointed (with a note in its log), waits to be
/// restored, and is restored by the scheduler once its slot is free; then
/// it runs again unmarked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_preempted_job_is_restored_once_there_is_room() {
    let s = setup().await;
    let core = &s.core;
    let victim = live_job(&s, JobState::Running).await;
    let id = victim.id;
    let before = core.now_ms();
    assert_eq!(core.preempt_jobs(vec![id]).await.unwrap(), [id]);
    let preempted = wait_for(core, id, "restore queued", restore_queued).await;
    let at = preempted.preempted_at.expect("marked as preempted");
    assert!(at >= before);
    assert!(s.helper.has_image(id));
    assert!(!s.helper.is_running(id));
    assert_eq!(log(&s, &victim), format!("epoch 1\n{PREEMPTION_NOTE}\n"));
    let record = core.read(move |db| db.get_checkpoint(id)).await.unwrap();
    assert_eq!(record.unwrap().status, CheckpointStatus::Complete);
    // Not a failure its owner is warned about.
    let warnings = core.pending_warnings(victim.owner).await.unwrap();
    assert!(warnings.warnings.is_empty(), "{:?}", warnings.warnings);

    // Another job has taken the slot: no restore.
    let other = live_job(&s, JobState::Running).await;
    core.schedule_once().await.unwrap();
    assert!(restore_queued(&job(core, id).await));
    assert!(s.helper.restore_requests().is_empty());

    // It ends; the preempted job is restored and runs again.
    s.helper.exit(other.id, 0);
    wait_for(core, other.id, "done", |j| {
        matches!(j.state, JobState::Done { .. })
    })
    .await;
    let restore_at = core.now_ms();
    core.schedule_once().await.unwrap();
    let restored = wait_state(core, id, JobState::Running).await;
    assert_eq!(restored.preempted_at, None);
    assert_eq!(restored.started_at, victim.started_at, "the same run");
    assert_eq!(s.helper.restore_requests(), [(id, false)]);
    assert!(s.helper.is_running(id));
    assert!(
        core.running_since()
            .get(&id)
            .is_some_and(|t| *t >= restore_at)
    );
    assert!(core.preempt_refused(core.now_ms()).is_empty());
    core.cancel.cancel();
}

/// A job paused when it was preempted is restored paused (stopped by the
/// helper), and stays paused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_preempted_paused_job_is_restored_paused() {
    let s = setup().await;
    let core = &s.core;
    let id = live_job(&s, JobState::Paused).await.id;
    assert_eq!(core.preempt_jobs(vec![id]).await.unwrap(), [id]);
    wait_for(core, id, "restore queued", restore_queued).await;
    // Resumed for the dump (cuda-checkpoint hangs on a stopped process).
    assert_eq!(s.helper.signals(), [(id, signal::SIGCONT)]);
    let record = core.read(move |db| db.get_checkpoint(id)).await.unwrap();
    assert!(record.unwrap().paused);

    core.schedule_once().await.unwrap();
    let restored = wait_state(core, id, JobState::Paused).await;
    assert_eq!(restored.preempted_at, None);
    assert_eq!(s.helper.restore_requests(), [(id, true)]);
    core.cancel.cancel();
}

/// A preemption checkpoint the helper refuses, up front or at dump time,
/// or rolls back leaves the job running, unmarked, and not to be preempted
/// again for a while (or until it stops running).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_preemption_leaves_the_job_running_and_is_remembered() {
    for upfront in [true, false] {
        let s = setup().await;
        let core = &s.core;
        let id = live_job(&s, JobState::Running).await.id;
        if upfront {
            s.helper.set_checkpoint_rejection(Some("hidden mounts"));
        } else {
            s.helper
                .set_checkpoint_outcome(FakeOutcome::Refuse("a file lock".to_owned()));
        }
        assert_eq!(core.preempt_jobs(vec![id]).await.unwrap(), [id]);
        eventually("the refusal is remembered", || async {
            core.preempt_refused(core.now_ms()).contains(&id)
        })
        .await;
        let refused = job(core, id).await;
        assert_eq!(refused.state, JobState::Running, "upfront: {upfront}");
        assert_eq!(refused.preempted_at, None);
        assert!(s.helper.is_running(id));
        assert!(s.helper.signals().is_empty());
        let record = core.read(move |db| db.get_checkpoint(id)).await.unwrap();
        assert_eq!(record.unwrap().status, CheckpointStatus::Failed);

        // Forgotten after a while.
        let later = core.now_ms() + PREEMPT_REFUSAL_MEMORY_MS;
        assert!(core.preempt_refused(later).is_empty());
        core.cancel.cancel();
    }

    // And once the job stops running.
    let s = setup().await;
    let core = &s.core;
    let id = live_job(&s, JobState::Running).await.id;
    s.helper.set_checkpoint_rejection(Some("hidden mounts"));
    core.preempt_jobs(vec![id]).await.unwrap();
    eventually("the refusal is remembered", || async {
        core.preempt_refused(core.now_ms()).contains(&id)
    })
    .await;
    s.helper.exit(id, 0);
    wait_for(core, id, "done", |j| {
        matches!(j.state, JobState::Done { .. })
    })
    .await;
    assert!(core.preempt_refused(core.now_ms()).is_empty());
    core.cancel.cancel();
}

/// A failed preemption checkpoint ends the job, as a failed checkpoint
/// does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_preemption_ends_the_job() {
    let s = setup().await;
    let core = &s.core;
    let id = live_job(&s, JobState::Running).await.id;
    s.helper
        .set_checkpoint_outcome(FakeOutcome::Fail("criu dump failed".to_owned()));
    assert_eq!(core.preempt_jobs(vec![id]).await.unwrap(), [id]);
    let result = JobResult::CheckpointFailed {
        reason: "criu dump failed".to_owned(),
    };
    // SIGTERM ends the fake job.
    let done = wait_state(core, id, JobState::Done { result }).await;
    assert_eq!(done.preempted_at, None);
    assert!(s.helper.signals().contains(&(id, signal::SIGTERM)));
    assert!(core.preempt_refused(core.now_ms()).is_empty());
    core.cancel.cancel();
}

/// A dump still running when the daemon restarts ends as a preemption:
/// the job waits to be restored, not checkpointed for good.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_preemption_survives_a_daemon_restart() {
    let s = setup().await;
    let id = live_job(&s, JobState::Running).await.id;
    s.helper.set_checkpoint_outcome(FakeOutcome::Hold);
    assert_eq!(s.core.preempt_jobs(vec![id]).await.unwrap(), [id]);
    eventually("the dump is running", || async {
        s.helper.jobs().get(&id).map(|j| j.phase) == Some(hp::RunningPhase::Checkpointing)
    })
    .await;
    let marked = job(&s.core, id).await;
    assert_eq!(marked.state, JobState::Checkpointing);
    assert!(marked.preempted_at.is_some());
    // The daemon stops; the dump finishes meanwhile.
    s.core.cancel.cancel();
    s.helper.close();
    s.helper.dump(id);

    let store = Store::open(s.dir.path().join("state.sqlite")).unwrap();
    let core = core_on(&s.helper, config(s.dir.path(), true), store);
    start(&core).await;
    let queued = wait_for(&core, id, "restore queued", restore_queued).await;
    assert_eq!(queued.preempted_at, marked.preempted_at);
    core.schedule_once().await.unwrap();
    let restored = wait_state(&core, id, JobState::Running).await;
    assert_eq!(restored.preempted_at, None);
    core.cancel.cancel();
}

/// Jobs that are not running or paused, or have `--no-checkpoint`, are
/// not preempted; nor is anything with preemption disabled or without the
/// helper.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jobs_that_cannot_be_preempted_are_left_alone() {
    let s = setup().await;
    let core = &s.core;
    let queued = core
        .db
        .call(|db| db.insert_job(owner(), &spec(), &JobState::Queued { enqueued_at: 0 }, 0))
        .await
        .unwrap()
        .id;
    let opted_out = live_job_with(
        &s,
        JobState::Running,
        JobSpec {
            no_checkpoint: true,
            ..spec()
        },
    )
    .await
    .id;
    let missing = JobId(999);
    assert!(
        core.preempt_jobs(vec![queued, opted_out, missing])
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(job(core, opted_out).await.state, JobState::Running);
    assert_eq!(job(core, opted_out).await.preempted_at, None);

    let running = live_job(&s, JobState::Running).await.id;
    s.helper.disconnect();
    eventually("the helper is gone", || async { !core.helper_ready() }).await;
    assert!(core.preempt_jobs(vec![running]).await.unwrap().is_empty());
    assert_eq!(job(core, running).await.state, JobState::Running);
    core.cancel.cancel();

    let s = setup_with(false).await;
    let running = live_job(&s, JobState::Running).await.id;
    assert!(s.core.preempt_jobs(vec![running]).await.unwrap().is_empty());
    assert_eq!(job(&s.core, running).await.state, JobState::Running);
    s.core.cancel.cancel();
}

/// `running_since`: when a job last started running, by spawn or restore;
/// not moved by a pause and resume, gone once it stops.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn running_since_follows_spawns_and_restores() {
    let s = setup().await;
    let core = &s.core;
    let id = core
        .db
        .call(|db| db.insert_job(owner(), &spec(), &JobState::Queued { enqueued_at: 0 }, 0))
        .await
        .unwrap()
        .id;
    let spawn_at = core.now_ms();
    core.schedule_once().await.unwrap();
    wait_state(core, id, JobState::Running).await;
    let since = core.running_since()[&id];
    assert!(since >= spawn_at);

    let caller = Caller {
        uid: owner(),
        is_admin: false,
    };
    let action = |action| core.job_action(caller, id, action, Arc::default());
    action(api::JobAction::Pause(Default::default()))
        .await
        .unwrap();
    action(api::JobAction::Start).await.unwrap();
    assert_eq!(job(core, id).await.state, JobState::Running);
    assert_eq!(core.running_since()[&id], since);

    // Preempted and restored: since the restore.
    core.preempt_jobs(vec![id]).await.unwrap();
    wait_for(core, id, "restore queued", restore_queued).await;
    assert!(!core.running_since().contains_key(&id));
    core.schedule_once().await.unwrap();
    wait_state(core, id, JobState::Running).await;
    assert!(core.running_since()[&id] >= since);

    s.helper.exit(id, 0);
    wait_for(core, id, "done", |j| {
        matches!(j.state, JobState::Done { .. })
    })
    .await;
    assert!(!core.running_since().contains_key(&id));
    core.cancel.cancel();
}
