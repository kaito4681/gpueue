//! Tests of preemption: checkpointing a job for the scheduler and
//! restoring it by itself (see [`super::preempt`]).

use std::future::Future;

use gpueue_core::model::{JobSpec, YieldMode};

use super::preempt::{PREEMPT_REFUSAL_MEMORY_MS, PREEMPTION_NOTE};
use super::*;
use crate::clock::{Clock, ManualClock};
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
    core_with_clock(helper, config, store, Arc::new(crate::clock::SystemClock))
}

/// The GPU the daemon detects: GPU 0 (one slot).
fn gpu0() -> DetectedGpu {
    DetectedGpu {
        index: GpuIndex(0),
        uuid: "GPU-0".to_owned(),
        name: "test".to_owned(),
        memory_total_bytes: None,
        driver_version: None,
    }
}

fn core_with_clock(
    helper: &FakeHelper,
    config: Config,
    store: Store,
    clock: Arc<dyn Clock>,
) -> Arc<Core> {
    Core::new(
        config,
        store,
        Arc::new(helper.clone()),
        Arc::new(StaticUserDb::with_admins([])),
        HashMap::from([(GpuIndex(0), gpu0())]),
        GpuDetectionStatus {
            mode: crate::config::GpuDetection::Auto,
            source: gpueue_protocol::api::GpuSource::Config,
            nvml_error: None,
            gpus: Vec::new(),
        },
        false,
        clock,
    )
}

/// A daemon with preemption enabled (or not), connected to a fake helper,
/// whose default group runs one job at a time, with GPU 0 (one slot).
/// The scheduler does not run by itself: tests call
/// [`Core::schedule_once`].
async fn setup_with(preemption: bool) -> Setup {
    setup_full(preemption, 0, Arc::new(crate::clock::SystemClock)).await
}

/// [`setup_with`], with `preempt_exempt_secs` and the daemon's clock.
async fn setup_full(preemption: bool, exempt_secs: u64, clock: Arc<dyn Clock>) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let helper = FakeHelper::new();
    let store = Store::open(dir.path().join("state.sqlite")).unwrap();
    store.set_group_parallel("default", 1).unwrap();
    let gpu = gpu0();
    store.upsert_gpu(gpu.index, &gpu.uuid, &gpu.name).unwrap();
    let mut config = config(dir.path(), preemption);
    config.priority.preempt_exempt_secs = exempt_secs;
    let core = core_with_clock(&helper, config, store, clock);
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

// ---------------------------------------------------------------------
// The scheduler's preemptions (`Core::schedule_once`)
// ---------------------------------------------------------------------

fn job_spec(group: &str, gpus: &[u32], yield_mode: YieldMode) -> JobSpec {
    JobSpec {
        group: group.to_owned(),
        gpus: gpus.iter().copied().map(GpuIndex).collect(),
        yield_mode,
        ..spec()
    }
}

/// A queued job with `spec`.
async fn queued(core: &Arc<Core>, spec: JobSpec) -> JobId {
    core.db
        .call(move |db| db.insert_job(owner(), &spec, &JobState::Queued { enqueued_at: 0 }, 0))
        .await
        .unwrap()
        .id
}

/// Creates the group `name` (unlimited slots) with `tier`.
async fn group(core: &Arc<Core>, name: &'static str, tier: i32) {
    core.db
        .call(move |db| {
            db.create_group(name, 0)?;
            db.set_group_tier(name, tier)
        })
        .await
        .unwrap();
}

/// Waits until `id` runs in the helper and its spawn or restore request
/// has been answered (until then it cannot be preempted).
async fn wait_launched(s: &Setup, id: JobId) {
    let core = &s.core;
    eventually(&format!("job {id} launched"), || async {
        s.helper.is_running(id) && !core.launch_in_flight(id)
    })
    .await;
}

/// Waits until `id` has been preempted: checkpointed and waiting, marked,
/// to be restored.
async fn wait_preempted(core: &Arc<Core>, id: JobId) -> Job {
    wait_for(core, id, "preempted", |j| {
        restore_queued(j) && j.preempted_at.is_some()
    })
    .await
}

/// The job exits successfully and is done.
async fn finish(s: &Setup, id: JobId) {
    s.helper.exit(id, 0);
    wait_state(
        &s.core,
        id,
        JobState::Done {
            result: JobResult::Success,
        },
    )
    .await;
}

fn is_queued(job: &Job) -> bool {
    matches!(job.state, JobState::Queued { .. })
}

/// The whole cycle, driven by the scheduler: a `--yield` job on GPU 0 is
/// checkpointed for a job that needs GPU 0, which starts once the dump has
/// finished (not in the pass that preempts); once that job has finished
/// the preempted job is restored, unmarked, and runs again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_scheduler_preempts_a_yielding_job_and_restores_it_afterwards() {
    let s = setup().await;
    let core = &s.core;
    group(core, "gpu", 0).await;
    let victim = queued(core, job_spec("gpu", &[0], YieldMode::All)).await;
    core.schedule_once().await.unwrap();
    wait_launched(&s, victim).await;

    s.helper.set_checkpoint_outcome(FakeOutcome::Hold);
    let urgent = queued(core, job_spec("gpu", &[0], YieldMode::None)).await;
    core.schedule_once().await.unwrap();
    // Marked in the pass that chose it; the urgent job waits for the dump.
    let marked = job(core, victim).await;
    assert_eq!(marked.state, JobState::Checkpointing);
    assert!(marked.preempted_at.is_some());
    assert!(is_queued(&job(core, urgent).await));
    eventually("the dump is requested", || async {
        s.helper.checkpoint_requests() == [victim]
    })
    .await;
    // Not preempted again, and nothing started, while the dump runs.
    core.schedule_once().await.unwrap();
    assert!(is_queued(&job(core, urgent).await));

    s.helper.dump(victim);
    wait_preempted(core, victim).await;
    core.schedule_once().await.unwrap();
    wait_state(core, urgent, JobState::Running).await;
    wait_launched(&s, urgent).await;
    // The preempted job waits while the urgent job runs.
    core.schedule_once().await.unwrap();
    assert!(restore_queued(&job(core, victim).await));
    assert!(s.helper.restore_requests().is_empty());

    finish(&s, urgent).await;
    core.schedule_once().await.unwrap();
    let restored = wait_state(core, victim, JobState::Running).await;
    assert_eq!(restored.preempted_at, None);
    assert_eq!(s.helper.restore_requests(), [(victim, false)]);
    assert_eq!(s.helper.checkpoint_requests(), [victim]);
    core.cancel.cancel();
}

/// A `--yield=group` job is preempted by a job of its own group that does
/// not yield, not by a job of another group of the same tier; a group of a
/// higher tier preempts it too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_that_yields_to_its_group_is_preempted_by_its_group_only() {
    let s = setup().await;
    let core = &s.core;
    group(core, "team", 0).await;
    group(core, "peer", 0).await;
    group(core, "urgent", 1).await;
    let victim = queued(core, job_spec("team", &[0], YieldMode::Group)).await;
    core.schedule_once().await.unwrap();
    wait_launched(&s, victim).await;

    // The same tier, another group: it waits.
    let peer = queued(core, job_spec("peer", &[0], YieldMode::None)).await;
    for _ in 0..3 {
        core.schedule_once().await.unwrap();
    }
    assert_eq!(job(core, victim).await.state, JobState::Running);
    assert!(is_queued(&job(core, peer).await));
    assert!(s.helper.checkpoint_requests().is_empty());
    // Out of the way, so that it does not take the slot freed below.
    core.db
        .call(move |db| {
            db.update_job_state(peer, &JobState::Stashed { enqueue_at: None }, None, None)
        })
        .await
        .unwrap();

    // Its own group.
    let mate = queued(core, job_spec("team", &[0], YieldMode::None)).await;
    core.schedule_once().await.unwrap();
    wait_preempted(core, victim).await;
    core.schedule_once().await.unwrap();
    wait_state(core, mate, JobState::Running).await;
    wait_launched(&s, mate).await;
    finish(&s, mate).await;
    core.schedule_once().await.unwrap();
    wait_state(core, victim, JobState::Running).await;
    wait_launched(&s, victim).await;

    // A higher tier.
    let urgent = queued(core, job_spec("urgent", &[0], YieldMode::None)).await;
    core.schedule_once().await.unwrap();
    wait_preempted(core, victim).await;
    core.schedule_once().await.unwrap();
    wait_state(core, urgent, JobState::Running).await;
    assert_eq!(s.helper.checkpoint_requests(), [victim, victim]);
    core.cancel.cancel();
}

/// A preemption the helper refuses is asked for once: the job keeps its
/// slot and is not offered again for `PREEMPT_REFUSAL_MEMORY_MS`, however
/// often the scheduler runs; after that it is tried again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_preemption_is_not_retried_within_the_window() {
    let clock = ManualClock::new();
    let s = setup_full(true, 0, Arc::new(clock.clone())).await;
    let core = &s.core;
    let victim = queued(core, job_spec("default", &[], YieldMode::All)).await;
    core.schedule_once().await.unwrap();
    wait_launched(&s, victim).await;

    s.helper.set_checkpoint_rejection(Some("a file lock"));
    let urgent = queued(core, spec()).await;
    core.schedule_once().await.unwrap();
    eventually("the refusal is remembered", || async {
        core.preempt_refused(core.now_ms()).contains(&victim)
    })
    .await;
    for _ in 0..3 {
        core.schedule_once().await.unwrap();
    }
    clock.advance(Duration::from_millis(
        u64::try_from(PREEMPT_REFUSAL_MEMORY_MS).unwrap() - 1,
    ));
    core.schedule_once().await.unwrap();
    assert_eq!(s.helper.checkpoint_requests(), [victim]);
    let running = job(core, victim).await;
    assert_eq!(running.state, JobState::Running);
    assert_eq!(running.preempted_at, None);
    assert!(is_queued(&job(core, urgent).await));

    // The window has passed.
    s.helper.set_checkpoint_rejection(None);
    clock.advance(Duration::from_millis(1));
    core.schedule_once().await.unwrap();
    wait_preempted(core, victim).await;
    assert_eq!(s.helper.checkpoint_requests(), [victim, victim]);
    core.cancel.cancel();
}

/// With `preemption = false` nothing is preempted, but the tiers and
/// yields still order the waiting jobs, for the scheduler and for
/// `gpueue prio`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_preemption_yields_only_order_the_waiting_jobs() {
    let s = setup_with(false).await;
    let core = &s.core;
    group(core, "gpu", 0).await;
    group(core, "urgent", 1).await;
    let running = queued(core, job_spec("gpu", &[0], YieldMode::All)).await;
    core.schedule_once().await.unwrap();
    wait_launched(&s, running).await;

    // Older, but yields; then a job of the default tier; then a higher tier.
    let yielding = queued(core, job_spec("gpu", &[0], YieldMode::All)).await;
    let normal = queued(core, job_spec("gpu", &[0], YieldMode::None)).await;
    let urgent = queued(core, job_spec("urgent", &[0], YieldMode::None)).await;
    for _ in 0..3 {
        core.schedule_once().await.unwrap();
    }
    assert_eq!(job(core, running).await.state, JobState::Running);
    assert!(s.helper.checkpoint_requests().is_empty());
    let order: Vec<JobId> = core
        .priorities()
        .await
        .unwrap()
        .jobs
        .iter()
        .map(|p| p.id)
        .collect();
    assert_eq!(order, [urgent, normal, yielding]);

    for next in [urgent, normal, yielding] {
        let current = core
            .db
            .call(|db| {
                Ok::<_, StorageError>(
                    db.list_unfinished_jobs_brief()?
                        .into_iter()
                        .find(|j| j.state == JobState::Running)
                        .map(|j| j.id),
                )
            })
            .await
            .unwrap()
            .unwrap();
        wait_launched(&s, current).await;
        finish(&s, current).await;
        core.schedule_once().await.unwrap();
        wait_state(core, next, JobState::Running).await;
    }
    assert!(s.helper.checkpoint_requests().is_empty());
    core.cancel.cancel();
}

/// `preempt_exempt_secs`: a job is not preempted until it has run that
/// long since it started, nor since it was restored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recently_started_jobs_are_exempt_from_preemption() {
    let clock = ManualClock::new();
    let s = setup_full(true, 60, Arc::new(clock.clone())).await;
    let core = &s.core;
    let victim = queued(core, job_spec("default", &[], YieldMode::All)).await;
    core.schedule_once().await.unwrap();
    wait_launched(&s, victim).await;

    let first = queued(core, spec()).await;
    core.schedule_once().await.unwrap();
    clock.advance(Duration::from_millis(59_999));
    core.schedule_once().await.unwrap();
    assert!(s.helper.checkpoint_requests().is_empty());
    assert_eq!(job(core, victim).await.state, JobState::Running);
    clock.advance(Duration::from_millis(1));
    core.schedule_once().await.unwrap();
    wait_preempted(core, victim).await;
    core.schedule_once().await.unwrap();
    wait_state(core, first, JobState::Running).await;
    wait_launched(&s, first).await;

    // Restored: exempt again, from the restore.
    finish(&s, first).await;
    clock.advance(Duration::from_secs(3600));
    core.schedule_once().await.unwrap();
    wait_state(core, victim, JobState::Running).await;
    wait_launched(&s, victim).await;
    let second = queued(core, spec()).await;
    clock.advance(Duration::from_millis(59_999));
    core.schedule_once().await.unwrap();
    assert_eq!(s.helper.checkpoint_requests(), [victim]);
    clock.advance(Duration::from_millis(1));
    core.schedule_once().await.unwrap();
    wait_preempted(core, victim).await;
    core.schedule_once().await.unwrap();
    wait_state(core, second, JobState::Running).await;
    core.cancel.cancel();
}

/// A preempted job its owner stashes is held back as checkpointed: not
/// restored once there is room.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stashed_preempted_job_is_not_restored_by_itself() {
    let s = setup().await;
    let core = &s.core;
    let victim = queued(core, job_spec("default", &[], YieldMode::All)).await;
    core.schedule_once().await.unwrap();
    wait_launched(&s, victim).await;
    let urgent = queued(core, spec()).await;
    core.schedule_once().await.unwrap();
    wait_preempted(core, victim).await;
    core.schedule_once().await.unwrap();
    wait_state(core, urgent, JobState::Running).await;
    wait_launched(&s, urgent).await;

    let caller = Caller {
        uid: owner(),
        is_admin: false,
    };
    core.job_action(
        caller,
        victim,
        api::JobAction::Stash(Default::default()),
        Arc::default(),
    )
    .await
    .unwrap();
    let stashed = job(core, victim).await;
    assert_eq!(stashed.state, JobState::Checkpointed);
    assert_eq!(stashed.preempted_at, None);

    finish(&s, urgent).await;
    for _ in 0..3 {
        core.schedule_once().await.unwrap();
    }
    assert_eq!(job(core, victim).await.state, JobState::Checkpointed);
    assert!(s.helper.restore_requests().is_empty());
    assert!(s.helper.has_image(victim));
    core.cancel.cancel();
}

/// Two waiting jobs that need the slot of one preemptible job: it is
/// dumped once, the first waiting job takes its slot, and the second one
/// waits for that job (it is not a victim) before the preempted job is
/// restored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_waiting_jobs_preempt_one_job_once() {
    let s = setup().await;
    let core = &s.core;
    let victim = queued(core, job_spec("default", &[], YieldMode::All)).await;
    core.schedule_once().await.unwrap();
    wait_launched(&s, victim).await;

    s.helper.set_checkpoint_outcome(FakeOutcome::Hold);
    let a = queued(core, spec()).await;
    let b = queued(core, spec()).await;
    core.schedule_once().await.unwrap();
    eventually("the dump is requested", || async {
        !s.helper.checkpoint_requests().is_empty()
    })
    .await;
    for _ in 0..3 {
        core.schedule_once().await.unwrap();
    }
    assert_eq!(s.helper.checkpoint_requests(), [victim]);

    s.helper.dump(victim);
    wait_preempted(core, victim).await;
    core.schedule_once().await.unwrap();
    let (first, second) = if job(core, a).await.state == JobState::Running {
        (a, b)
    } else {
        (b, a)
    };
    wait_state(core, first, JobState::Running).await;
    wait_launched(&s, first).await;
    core.schedule_once().await.unwrap();
    assert!(is_queued(&job(core, second).await));
    assert!(restore_queued(&job(core, victim).await));

    finish(&s, first).await;
    core.schedule_once().await.unwrap();
    wait_state(core, second, JobState::Running).await;
    wait_launched(&s, second).await;
    assert!(restore_queued(&job(core, victim).await));
    finish(&s, second).await;
    core.schedule_once().await.unwrap();
    wait_state(core, victim, JobState::Running).await;
    assert_eq!(s.helper.checkpoint_requests(), [victim]);
    core.cancel.cancel();
}

/// A job an administrator launches in a scheduling pass is not preempted
/// in that pass, nor while its launch is unanswered, even though a job
/// waiting for its slot may preempt it; once it runs, it yields like any
/// other job.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_launched_in_a_pass_is_not_preempted_by_it() {
    let s = setup().await;
    let core = &s.core;
    let waiting = queued(core, spec()).await;
    let forced = queued(core, job_spec("default", &[], YieldMode::All)).await;
    s.helper.hold_launches();
    core.forced.lock().unwrap().insert(forced);
    // The forced launch comes first and takes the only slot.
    core.schedule_once().await.unwrap();
    for _ in 0..2 {
        let launched = job(core, forced).await;
        assert_eq!(launched.state, JobState::Running);
        assert_eq!(launched.preempted_at, None);
        assert!(is_queued(&job(core, waiting).await));
        assert!(core.launch_in_flight(forced));
        // Again, with the launch unanswered.
        core.schedule_once().await.unwrap();
    }

    s.helper.release_launches();
    wait_launched(&s, forced).await;
    core.schedule_once().await.unwrap();
    wait_preempted(core, forced).await;
    core.schedule_once().await.unwrap();
    wait_state(core, waiting, JobState::Running).await;
    assert_eq!(s.helper.checkpoint_requests(), [forced]);
    core.cancel.cancel();
}
