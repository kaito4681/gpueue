//! Tests of how the daemon follows the helper: events, reconnects,
//! reconciliation and the checks that depend on them.

use std::future::Future;

use gpueue_core::model::JobSpec;
use gpueue_protocol::helper::Event;

use super::*;
use crate::helper::{FakeHelper, FakeOutcome};
use crate::users::StaticUserDb;

fn core_with(helper: FakeHelper, config: Config, store: Store) -> Arc<Core> {
    Core::new(
        config,
        store,
        Arc::new(helper),
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

fn core(helper: &FakeHelper) -> Arc<Core> {
    let config = Config {
        grace_secs: 1,
        ..Config::default()
    };
    core_with(helper.clone(), config, Store::open_in_memory().unwrap())
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
    }
}

/// A job in `state` (`started_at` set), live in the helper.
async fn live_job(core: &Arc<Core>, helper: &FakeHelper, state: JobState) -> Job {
    let job = core
        .db
        .call(move |s| {
            let job = s.insert_job(Uid(1000), &spec(), &state, 0)?;
            s.update_job_state(job.id, &state, Some(5), None)?;
            s.get_job(job.id).map(Option::unwrap)
        })
        .await
        .unwrap();
    helper.insert_running(job.id);
    job
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

async fn wait_state(core: &Arc<Core>, id: JobId, state: JobState) {
    eventually(&format!("job {id} is {}", state.name()), || {
        let state = state.clone();
        async move { job(core, id).await.state == state }
    })
    .await;
}

/// Starts the helper connection and the notice loop, and waits until the
/// daemon has reconciled.
async fn start(core: &Arc<Core>) {
    let notices = core.helper.connect();
    tokio::spawn(core.clone().notice_loop(notices));
    eventually("the daemon is reconciled", || async { core.helper_ready() }).await;
}

fn success() -> JobState {
    JobState::Done {
        result: JobResult::Success,
    }
}

/// Finding: a helper event whose transaction failed was acknowledged all
/// the same, so the helper forgot it and the job stayed running. It is
/// now acknowledged only once recorded, and tried again until it is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_event_that_cannot_be_recorded_is_retried_not_acknowledged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gpueue.db");
    let helper = FakeHelper::new();
    let store = Store::open(&path).unwrap();
    // Attempts fail soon rather than after the default 5 s.
    store.set_busy_timeout(Duration::from_millis(100)).unwrap();
    let core = core_with(helper.clone(), Config::default(), store);
    let job = live_job(&core, &helper, JobState::Running).await;
    start(&core).await;

    // Another connection holds the write lock past the busy timeout.
    let (release, held) = std::sync::mpsc::channel::<()>();
    let (locked, is_locked) = std::sync::mpsc::channel::<()>();
    let blocker = std::thread::spawn(move || {
        let mut other = Store::open(&path).unwrap();
        other
            .transaction(|_| {
                locked.send(()).unwrap();
                held.recv().unwrap();
                Ok::<_, StorageError>(())
            })
            .unwrap();
    });
    is_locked.recv().unwrap();
    helper.exit(job.id, 0);
    // Attempts fail after the busy timeout, and are repeated.
    eventually("recording the event failed twice", || async {
        core.storage_failures.load(Ordering::SeqCst) >= 2
    })
    .await;
    assert_eq!(
        helper.unacked().len(),
        1,
        "not acknowledged while unrecorded"
    );
    assert_eq!(job_state(&core, job.id).await, JobState::Running);
    release.send(()).unwrap();
    blocker.join().unwrap();

    wait_state(&core, job.id, success()).await;
    eventually("the event is acknowledged", || async {
        helper.unacked().is_empty()
    })
    .await;
    assert_eq!(core.last_helper_event().await.0, 1);
    core.cancel.cancel();
}

/// The storage retry backs off and returns the value once an attempt
/// succeeds (as used for reconciliation reads and event records).
#[tokio::test]
async fn storage_attempts_are_repeated_with_a_delay() {
    let helper = FakeHelper::new();
    let core = core(&helper);
    let attempts = std::sync::atomic::AtomicUsize::new(0);
    let started = tokio::time::Instant::now();
    let value = core
        .retry_storage("test", || {
            let n = attempts.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < 3 {
                    Err(Failure::internal("disk I/O error"))
                } else {
                    Ok(n)
                }
            }
        })
        .await;
    assert_eq!(value, Some(3));
    // 200 + 400 + 800 ms.
    assert!(started.elapsed() >= Duration::from_millis(1400));
    // A daemon that stops gives up.
    core.cancel.cancel();
    let none: Option<()> = core
        .retry_storage("test", || async { Err(Failure::internal("x")) })
        .await;
    assert_eq!(none, None);
}

/// Finding: a dump the helper resumed the job after (failed) was reported
/// after the poller had taken the request for lost and put the job back to
/// running; the late failure then left it terminating without any signal.
/// Now an attempted dump still terminates the job, a refused one leaves
/// it alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dump_outcome_after_the_request_was_taken_for_lost_is_applied() {
    for attempted in [true, false] {
        let helper = FakeHelper::new();
        let core = core(&helper);
        let job = live_job(&core, &helper, JobState::Running).await;
        start(&core).await;
        // The checkpoint request goes unanswered; the helper lists the job
        // as running (it has just resumed it), so the poller gives up the
        // checkpoint.
        helper.set_checkpoint_outcome(FakeOutcome::LoseRequest);
        let t = lifecycle::checkpoint(&job).unwrap();
        let effects = t.effects.clone();
        let id = job.id;
        let job = core
            .write({
                let core = core.clone();
                move |tx, ch| {
                    let mut job = tx.get_job(id)?.unwrap();
                    core.apply_in_tx(tx, &mut job, &t, core.now_ms(), ch)?;
                    Ok(job)
                }
            })
            .await
            .unwrap();
        let _ = core.run_effects(&job, &effects).await;
        eventually("the checkpoint is given up", || async {
            core.abandoned_checkpoints.lock().unwrap().contains_key(&id)
        })
        .await;
        assert_eq!(job_state(&core, id).await, JobState::Running);

        let outcome = if attempted {
            CheckpointOutcome::failed("criu dump failed")
        } else {
            CheckpointOutcome::refused("not enough space")
        };
        helper.emit_event(Event::CheckpointFinished { job: id, outcome });
        if attempted {
            // SIGTERM ends the fake job.
            wait_state(
                &core,
                id,
                JobState::Done {
                    result: JobResult::CheckpointFailed {
                        reason: "criu dump failed".to_owned(),
                    },
                },
            )
            .await;
            assert!(helper.signals().contains(&(id, signal::SIGTERM)));
        } else {
            eventually("the late refusal is applied", || async {
                helper.unacked().is_empty()
            })
            .await;
            assert_eq!(job_state(&core, id).await, JobState::Running);
            assert!(helper.signals().is_empty());
            assert!(helper.is_running(id));
        }
        assert!(!core.abandoned_checkpoints.lock().unwrap().contains_key(&id));
        core.cancel.cancel();
    }
}

async fn job_state(core: &Arc<Core>, id: JobId) -> JobState {
    job(core, id).await.state
}

/// Finding: a failed dump with an error code this daemon does not know
/// (from a newer helper) was taken for an attempted dump, and the job
/// killed. It is taken as refused: the job keeps running.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dump_failure_with_an_unknown_code_leaves_the_job_alone() {
    let helper = FakeHelper::new();
    let core = core(&helper);
    let job = live_job(&core, &helper, JobState::Checkpointing).await;
    start(&core).await;
    helper.emit_event(Event::CheckpointFinished {
        job: job.id,
        outcome: CheckpointOutcome::Failed {
            reason: "something new".to_owned(),
            code: Some(HelperErrorCode::Unknown),
        },
    });
    wait_state(&core, job.id, JobState::Running).await;
    assert!(helper.signals().is_empty());
    let record = core
        .read(move |s| s.get_checkpoint(job.id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.status, CheckpointStatus::Failed);
    core.cancel.cancel();
}

/// A failed dump the helper rolled back cleanly (a deleted file over the
/// ghost limit, a file lock) leaves the job running, paused again if it
/// was paused, with the reason recorded for its owner.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rolled_back_dump_leaves_the_job_running() {
    for paused in [false, true] {
        let helper = FakeHelper::new();
        let core = core(&helper);
        let job = live_job(&core, &helper, JobState::Checkpointing).await;
        let id = job.id;
        let record = core.new_checkpoint_record(&job, paused, core.now_ms());
        core.write(move |tx, _| Ok(tx.put_checkpoint(&record)?))
            .await
            .unwrap();
        start(&core).await;
        helper.emit_event(Event::CheckpointFinished {
            job: id,
            outcome: CheckpointOutcome::rolled_back("criu dump failed; the job was resumed"),
        });
        if paused {
            wait_state(&core, id, JobState::Paused).await;
            assert_eq!(helper.signals(), [(id, signal::SIGSTOP)]);
        } else {
            wait_state(&core, id, JobState::Running).await;
            assert!(helper.signals().is_empty());
        }
        assert!(helper.is_running(id));
        let record = core
            .read(move |s| s.get_checkpoint(id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.status, CheckpointStatus::Failed);
        assert_eq!(
            record.failure_reason.as_deref(),
            Some(
                "checkpoint failed, the job keeps running: criu dump failed; the job was \
                 resumed"
            )
        );
        core.cancel.cancel();
    }
}

/// Finding: a launch record left from a request whose answer was lost was
/// never cleared when the helper listed the job, so a later check could
/// put the running job back in line (and run it twice). Reconciliation
/// clears it, and a record of an older launch is ignored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_unconfirmed_launch_does_not_requeue_a_running_job() {
    let helper = FakeHelper::new();
    let core = core(&helper);
    let listed = live_job(&core, &helper, JobState::Running).await;
    let gone = live_job(&core, &helper, JobState::Running).await;
    // Both spawns went unanswered; later launches of `gone` were
    // confirmed.
    core.launches.begin(listed.id);
    let first = core.launches.latest(listed.id);
    core.launches
        .settle_on_drop(listed.id)
        .finish(LaunchOutcome::Unconfirmed);
    core.unconfirmed_launches
        .lock()
        .unwrap()
        .insert(listed.id, (JobState::Running, first));
    core.launches.begin(gone.id);
    let old = core.launches.latest(gone.id);
    core.launches
        .settle_on_drop(gone.id)
        .finish(LaunchOutcome::Done);
    core.launches.begin(gone.id);
    core.launches
        .settle_on_drop(gone.id)
        .finish(LaunchOutcome::Done);
    core.unconfirmed_launches
        .lock()
        .unwrap()
        .insert(gone.id, (JobState::Running, old));
    helper.vanish(gone.id);
    start(&core).await;
    assert!(
        !core
            .unconfirmed_launches
            .lock()
            .unwrap()
            .contains_key(&listed.id),
        "the helper listed the job"
    );

    // `gone` vanishes from the helper: it is lost, not run again.
    core.spawn_lost_check(gone.id, gone.started_at);
    wait_state(
        &core,
        gone.id,
        JobState::Done {
            result: JobResult::Lost,
        },
    )
    .await;
    // `listed` is not found once: it stays as it is.
    helper.vanish(listed.id);
    core.spawn_lost_check(listed.id, listed.started_at);
    wait_state(
        &core,
        listed.id,
        JobState::Done {
            result: JobResult::Lost,
        },
    )
    .await;
    assert!(helper.spawned().is_empty());
    core.cancel.cancel();
}

/// Finding: a helper whose numbering was reset and that replays more
/// events than the daemon had applied lost its first ones (taken for
/// duplicates). The helper's epoch tells the daemon that they are new.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_long_replay_after_a_numbering_reset_is_applied_in_full() {
    let helper = FakeHelper::new();
    let core = core(&helper);
    let mut jobs = Vec::new();
    for _ in 0..7 {
        jobs.push(live_job(&core, &helper, JobState::Running).await.id);
    }
    start(&core).await;
    // Two events applied (seq 1 and 2).
    for &id in &jobs[..2] {
        helper.exit(id, 0);
        wait_state(&core, id, success()).await;
    }
    helper.disconnect();
    helper.reset_event_numbering();
    // Five exits while disconnected, numbered 1 to 5 again.
    for &id in &jobs[2..] {
        helper.exit(id, 0);
    }
    assert_eq!(helper.unacked().len(), 5);
    helper.reconnect();
    for &id in &jobs[2..] {
        wait_state(&core, id, success()).await;
    }
    eventually("the replay is acknowledged", || async {
        helper.unacked().is_empty()
    })
    .await;
    // The numbering is followed from there on.
    let (last, epoch) = core.last_helper_event().await;
    assert_eq!((last, epoch), (5, Some(2)));
    core.cancel.cancel();
}

/// Finding: after a reconnect, the daemon counted as ready (and could
/// launch jobs) before it had reconciled the new connection, while the
/// notice loop had not handled the end of the previous one yet.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_connection_is_not_ready_before_it_is_reconciled() {
    let helper = FakeHelper::new();
    let core = core(&helper);
    start(&core).await;
    let notices = helper.reconnect_silently();
    assert!(helper.state().connected);
    assert_eq!(helper.state().session, 2);
    assert!(!core.helper_ready(), "session 2 is not reconciled yet");
    helper.deliver(notices);
    eventually("the new connection is reconciled", || async {
        core.helper_ready()
    })
    .await;
    core.cancel.cancel();
}

/// Flake of `a_spawn_refused_by_a_stopping_helper_is_queued_until_retried`:
/// a scheduling pass that checked the launch pause before a refusal set it
/// and got the database lock only after the refused job was back in line
/// launched it again at once. The pause is checked under the lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::await_holding_lock)]
async fn a_pass_that_waited_for_the_database_respects_a_new_launch_pause() {
    let helper = FakeHelper::new();
    let core = core(&helper);
    start(&core).await;
    let id = core
        .db
        .call(|s| s.insert_job(Uid(1000), &spec(), &JobState::Queued { enqueued_at: 0 }, 0))
        .await
        .unwrap()
        .id;
    let db = core.db.0.clone();
    let lock = db.lock().unwrap();
    let pass = tokio::spawn({
        let core = core.clone();
        async move { core.schedule_once().await }
    });
    // The pass has decided to launch and waits for the database; now a
    // refusal pauses launches.
    tokio::time::sleep(Duration::from_millis(200)).await;
    core.pause_launches();
    drop(lock);
    pass.await.unwrap().unwrap();
    assert!(matches!(
        job_state(&core, id).await,
        JobState::Queued { .. }
    ));
    assert!(helper.spawned().is_empty());
    core.cancel.cancel();
}

/// Finding: the SIGKILL that `kill` sends after the grace period found its
/// run by `started_at` alone, which a restore keeps, so it could kill the
/// job's next run. It is now tied to the launch of the process too.
///
/// On paused tokio time: the test's sleep ends only once the kill timer has
/// fired and done its work (time does not move on while the timer's
/// database read runs on the blocking pool).
#[tokio::test(start_paused = true)]
async fn the_kill_timer_spares_the_next_run_after_a_restore() {
    for restored in [false, true] {
        let helper = FakeHelper::new();
        let core = core(&helper);
        let job = live_job(&core, &helper, JobState::Running).await;
        start(&core).await;
        helper.set_ignore_sigterm(true);
        core.terminate(&job).await.unwrap();
        if restored {
            // Checkpointed and restored within the grace period: a new
            // launch of the same run (same `started_at`).
            core.launches.begin(job.id);
            core.launches
                .settle_on_drop(job.id)
                .finish(LaunchOutcome::Done);
        }
        // Past the 1 s grace period.
        tokio::time::sleep(Duration::from_secs(2)).await;
        let killed = helper.signals().contains(&(job.id, signal::SIGKILL));
        assert_eq!(killed, !restored, "restored: {restored}");
        core.cancel.cancel();
    }
}
