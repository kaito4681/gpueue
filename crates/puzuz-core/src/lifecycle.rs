//! Pure job state-transition logic.
//!
//! Every user action (pause, start, kill, ...) and every runtime event reported
//! by the helper (spawned, exited, checkpoint finished, ...) is checked against
//! the job's current [`JobState`]. A valid action yields a [`Transition`]: the
//! next state (if it changes) plus a list of [`Effect`]s the daemon has to carry
//! out. Nothing here performs I/O; the daemon executes the effects and then
//! persists the new state with [`Transition::apply`].
//!
//! The rules follow pueue 4.0.4 where pueue has an equivalent command and the
//! puzuz spec for the checkpoint additions:
//!
//! | action       | accepted states                                       |
//! |--------------|-------------------------------------------------------|
//! | `pause`      | Running                                               |
//! | `start`      | Paused (resume), Queued / Stashed (force start)       |
//! | `kill`       | Running, Paused                                       |
//! | `remove`     | Queued, Stashed, Done, Checkpointed, RestoreQueued    |
//! | `restart`    | Done, Checkpointed (new job; in place: Done only)     |
//! | `stash`      | Queued                                                |
//! | `enqueue`    | Stashed                                               |
//! | `switch`     | Queued, Stashed                                       |
//! | `clean`      | Done                                                  |
//! | `checkpoint` | Running, Paused                                       |
//! | `restore`    | Checkpointed                                          |
//!
//! Checkpointed jobs are only resumed by `restore`; `start` and `kill` reject
//! them, and [`hint`] tells the user which command to use instead.

use crate::error::Error;
use crate::model::{Job, JobId, JobResult, JobState, Timestamp};

/// A side effect the daemon must perform for a transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Spawn the job's process from its spec.
    Spawn,
    /// Send SIGSTOP to the job's process group.
    SignalStop,
    /// Send SIGCONT to the job's process group.
    SignalContinue,
    /// Kill the job's whole process tree (pueue's `kill` without `--signal`).
    Kill,
    /// Send this signal number to the job's process group (`kill --signal`).
    SendSignal(i32),
    /// SIGTERM, then SIGKILL once the configured grace period has elapsed.
    Terminate,
    /// Dump the job with CRIU / cuda-checkpoint. The dump stops the process.
    Checkpoint,
    /// Restore the job from its checkpoint image.
    Restore,
    /// Delete the job's checkpoint image, if any (must be idempotent).
    DeleteCheckpointImage,
    /// Delete the job's log file.
    DeleteLog,
    /// Truncate the job's log file (in-place restart).
    ResetLog,
    /// Submit a new job with the same spec (pueue's default `restart`).
    /// The original job is left untouched.
    CreateRestartedCopy { stashed: bool },
    /// Remember why restoring failed so `status` can show it; the job stays
    /// Checkpointed and keeps its image.
    RecordRestoreError(String),
}

/// The outcome of a valid action or event.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Transition {
    /// The new state, or `None` if the state does not change.
    pub next: Option<JobState>,
    /// Side effects, in the order they must be performed.
    pub effects: Vec<Effect>,
}

impl Transition {
    fn to(next: JobState) -> Self {
        Transition {
            next: Some(next),
            effects: Vec::new(),
        }
    }

    fn unchanged() -> Self {
        Transition::default()
    }

    fn with(mut self, effect: Effect) -> Self {
        self.effects.push(effect);
        self
    }

    /// Write the new state into `job` and maintain its timestamps:
    ///
    /// - entering `Running` for the first time sets `started_at`;
    /// - entering `Done` sets `finished_at`;
    /// - going back to `Queued` / `Stashed` (in-place restart) clears both.
    pub fn apply(&self, job: &mut Job, now: Timestamp) {
        let Some(next) = &self.next else {
            return;
        };
        match next {
            JobState::Running if job.started_at.is_none() => job.started_at = Some(now),
            JobState::Done { .. } => job.finished_at = Some(now),
            JobState::Queued { .. } | JobState::Stashed { .. } => {
                job.started_at = None;
                job.finished_at = None;
            }
            _ => {}
        }
        job.state = next.clone();
    }
}

/// User-facing actions, used for error messages and hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Pause,
    Start,
    Kill,
    Remove,
    Restart,
    Stash,
    Enqueue,
    Switch,
    Clean,
    Checkpoint,
    Restore,
}

impl Action {
    /// Past participle used in "job N cannot be {action} while it is {state}".
    pub fn verb(self) -> &'static str {
        match self {
            Action::Pause => "paused",
            Action::Start => "started",
            Action::Kill => "killed",
            Action::Remove => "removed",
            Action::Restart => "restarted",
            Action::Stash => "stashed",
            Action::Enqueue => "enqueued",
            Action::Switch => "switched",
            Action::Clean => "cleaned",
            Action::Checkpoint => "checkpointed",
            Action::Restore => "restored",
        }
    }
}

/// A suggestion to show next to an [`Error::InvalidTransition`], for the
/// combinations where users predictably reach for the wrong command.
pub fn hint(action: Action, state: &JobState) -> Option<&'static str> {
    match (action, state) {
        (Action::Start, JobState::Checkpointed) => {
            Some("checkpointed jobs are resumed with `puzuz restore`")
        }
        (Action::Start, JobState::RestoreQueued { .. }) => Some(
            "the job is waiting to be restored; use `puzuz restore --immediate` to restore it now",
        ),
        (Action::Kill, JobState::Checkpointed | JobState::RestoreQueued { .. }) => {
            Some("the job is not running; use `puzuz remove` to delete it and its checkpoint")
        }
        (Action::Kill, JobState::Queued { .. } | JobState::Stashed { .. }) => {
            Some("the job is not running; use `puzuz remove` to delete it")
        }
        (Action::Remove, JobState::Running | JobState::Paused) => {
            Some("kill the job first with `puzuz kill`")
        }
        (Action::Restore, JobState::Paused) => Some("use `puzuz start` to resume a paused job"),
        (Action::Restart, JobState::Checkpointed) => Some(
            "a checkpointed job cannot be restarted in place; restart without --in-place \
             creates a new job and keeps the checkpoint",
        ),
        _ => None,
    }
}

fn invalid(job: &Job, action: &'static str) -> Error {
    Error::InvalidTransition {
        id: job.id,
        action,
        state: job.state.name(),
    }
}

fn reject(job: &Job, action: Action) -> Error {
    invalid(job, action.verb())
}

/// Whether the job's state may still have a checkpoint image on disk.
fn may_have_image(state: &JobState) -> bool {
    matches!(
        state,
        JobState::Checkpointed
            | JobState::RestoreQueued { .. }
            | JobState::Done {
                result: JobResult::CheckpointFailed { .. }
            }
    )
}

// ---------------------------------------------------------------------------
// User actions
// ---------------------------------------------------------------------------

/// `pause`: stop a running job with SIGSTOP.
pub fn pause(job: &Job) -> Result<Transition, Error> {
    match job.state {
        JobState::Running => Ok(Transition::to(JobState::Paused).with(Effect::SignalStop)),
        _ => Err(reject(job, Action::Pause)),
    }
}

/// `start`: resume a paused job, or start a queued/stashed job immediately
/// regardless of free slots (as pueue does).
///
/// Checkpointed jobs are rejected: they are resumed by [`restore`].
pub fn start(job: &Job) -> Result<Transition, Error> {
    match job.state {
        JobState::Paused => Ok(Transition::to(JobState::Running).with(Effect::SignalContinue)),
        JobState::Queued { .. } | JobState::Stashed { .. } => {
            Ok(Transition::to(JobState::Running).with(Effect::Spawn))
        }
        _ => Err(reject(job, Action::Start)),
    }
}

/// `kill`: kill a running or paused job, or send it `signal` if given.
///
/// The state does not change here; the job becomes `Done` when the helper
/// reports its exit (see [`exited`]). Checkpointed jobs are not running and
/// are rejected; they are deleted with [`remove`].
pub fn kill(job: &Job, signal: Option<i32>) -> Result<Transition, Error> {
    match job.state {
        JobState::Running | JobState::Paused => {
            let effect = match signal {
                Some(signal) => Effect::SendSignal(signal),
                None => Effect::Kill,
            };
            Ok(Transition::unchanged().with(effect))
        }
        _ => Err(reject(job, Action::Kill)),
    }
}

/// `remove`: delete a job that is not running. The daemon deletes the job
/// record after performing the effects (`next` is `None`).
///
/// Unlike pueue this also accepts Checkpointed and RestoreQueued jobs and
/// deletes their image. Callers must additionally check
/// [`blocking_dependants`].
pub fn remove(job: &Job) -> Result<Transition, Error> {
    match job.state {
        JobState::Queued { .. }
        | JobState::Stashed { .. }
        | JobState::Done { .. }
        | JobState::Checkpointed
        | JobState::RestoreQueued { .. } => Ok(deletion(job)),
        _ => Err(reject(job, Action::Remove)),
    }
}

fn deletion(job: &Job) -> Transition {
    let mut transition = Transition::unchanged();
    if may_have_image(&job.state) {
        transition.effects.push(Effect::DeleteCheckpointImage);
    }
    transition.effects.push(Effect::DeleteLog);
    transition
}

/// Whether `clean` removes this job: finished jobs only, and with
/// `successful_only` only those that succeeded.
pub fn is_cleanable(job: &Job, successful_only: bool) -> bool {
    match &job.state {
        JobState::Done { result } => !successful_only || *result == JobResult::Success,
        _ => false,
    }
}

/// `clean` for one job: like [`remove`] but restricted to [`is_cleanable`].
pub fn clean(job: &Job, successful_only: bool) -> Result<Transition, Error> {
    if is_cleanable(job, successful_only) {
        Ok(deletion(job))
    } else {
        Err(reject(job, Action::Clean))
    }
}

/// Jobs that prevent `id` from being removed: unfinished jobs depending on it
/// that are not part of the same removal (`to_delete`). Mirrors pueue's
/// `is_task_removable`, including transitive dependants.
pub fn blocking_dependants(jobs: &[Job], id: JobId, to_delete: &[JobId]) -> Vec<JobId> {
    let mut blocking = Vec::new();
    let mut pending = vec![id];
    let mut seen = vec![id];
    while let Some(current) = pending.pop() {
        for job in jobs {
            if seen.contains(&job.id)
                || matches!(job.state, JobState::Done { .. })
                || !job.spec.dependencies.contains(&current)
            {
                continue;
            }
            seen.push(job.id);
            if to_delete.contains(&job.id) {
                // Removed together; its own dependants must be checked too.
                pending.push(job.id);
            } else {
                blocking.push(job.id);
            }
        }
    }
    blocking.sort();
    blocking
}

/// `restart`.
///
/// - Default (pueue's behaviour): a new job with the same spec is created and
///   the original is left as is. Allowed for Done and Checkpointed jobs; a
///   Checkpointed original keeps its image.
/// - `in_place`: the job itself is re-queued (or stashed) and its log reset.
///   Only for Done jobs, since a Checkpointed job would lose its checkpoint.
pub fn restart(
    job: &Job,
    in_place: bool,
    stashed: bool,
    now: Timestamp,
) -> Result<Transition, Error> {
    match (&job.state, in_place) {
        (JobState::Done { .. } | JobState::Checkpointed, false) => {
            Ok(Transition::unchanged().with(Effect::CreateRestartedCopy { stashed }))
        }
        (JobState::Done { .. }, true) => {
            let next = if stashed {
                JobState::Stashed { enqueue_at: None }
            } else {
                JobState::Queued { enqueued_at: now }
            };
            Ok(Transition::to(next).with(Effect::ResetLog))
        }
        _ => Err(reject(job, Action::Restart)),
    }
}

/// `stash`: hold back a queued job, optionally until `enqueue_at`.
pub fn stash(job: &Job, enqueue_at: Option<Timestamp>) -> Result<Transition, Error> {
    match job.state {
        JobState::Queued { .. } => Ok(Transition::to(JobState::Stashed { enqueue_at })),
        _ => Err(reject(job, Action::Stash)),
    }
}

/// `enqueue`: queue a stashed job now, or schedule it for `enqueue_at`.
pub fn enqueue(
    job: &Job,
    enqueue_at: Option<Timestamp>,
    now: Timestamp,
) -> Result<Transition, Error> {
    match job.state {
        JobState::Stashed { .. } => Ok(Transition::to(match enqueue_at {
            Some(at) => JobState::Stashed {
                enqueue_at: Some(at),
            },
            None => JobState::Queued { enqueued_at: now },
        })),
        _ => Err(reject(job, Action::Enqueue)),
    }
}

/// Enqueue a stashed job whose `enqueue_at` time has come; `None` if the job
/// is not due.
pub fn enqueue_if_due(job: &Job, now: Timestamp) -> Option<Transition> {
    match job.state {
        JobState::Stashed {
            enqueue_at: Some(at),
        } if at <= now => Some(Transition::to(JobState::Queued { enqueued_at: now })),
        _ => None,
    }
}

/// `switch`: both jobs must be queued or stashed and distinct. The daemon
/// swaps their ids (and dependency references) itself.
pub fn check_switch(a: &Job, b: &Job) -> Result<(), Error> {
    for job in [a, b] {
        if !matches!(
            job.state,
            JobState::Queued { .. } | JobState::Stashed { .. }
        ) {
            return Err(reject(job, Action::Switch));
        }
    }
    if a.id == b.id {
        return Err(Error::InvalidTransition {
            id: a.id,
            action: "switched with itself",
            state: a.state.name(),
        });
    }
    Ok(())
}

/// `checkpoint`: dump a running job and stop it.
///
/// A paused job is resumed first (`SignalContinue`, then `Checkpoint`): the
/// CUDA driver cannot checkpoint a stopped process, so the job comes back
/// running after `restore`.
pub fn checkpoint(job: &Job) -> Result<Transition, Error> {
    match job.state {
        JobState::Running => Ok(Transition::to(JobState::Checkpointing).with(Effect::Checkpoint)),
        JobState::Paused => Ok(Transition::to(JobState::Checkpointing)
            .with(Effect::SignalContinue)
            .with(Effect::Checkpoint)),
        _ => Err(reject(job, Action::Checkpoint)),
    }
}

/// `restore`: put a checkpointed job back in line, or with `immediate`
/// restore it right away regardless of free slots.
pub fn restore(job: &Job, immediate: bool, now: Timestamp) -> Result<Transition, Error> {
    match job.state {
        JobState::Checkpointed if immediate => {
            Ok(Transition::to(JobState::Restoring).with(Effect::Restore))
        }
        JobState::Checkpointed => Ok(Transition::to(JobState::RestoreQueued { enqueued_at: now })),
        JobState::RestoreQueued { .. } if immediate => {
            Ok(Transition::to(JobState::Restoring).with(Effect::Restore))
        }
        _ => Err(reject(job, Action::Restore)),
    }
}

/// What to do with a job when the whole system is shut down
/// (`puzuz shutdown` or helper stop).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShutdownAction {
    /// Nothing is running; leave the job as is.
    Keep,
    Checkpoint(Transition),
    /// `--no-checkpoint` (per job or for the shutdown): SIGTERM, then SIGKILL.
    Terminate,
}

/// Decide how a job is handled on shutdown. `checkpoint_enabled` is false for
/// `puzuz shutdown --no-checkpoint`.
pub fn shutdown_action(job: &Job, checkpoint_enabled: bool) -> ShutdownAction {
    match job.state {
        JobState::Running | JobState::Paused => {
            if checkpoint_enabled && !job.spec.no_checkpoint {
                match checkpoint(job) {
                    Ok(transition) => ShutdownAction::Checkpoint(transition),
                    Err(_) => ShutdownAction::Terminate,
                }
            } else {
                ShutdownAction::Terminate
            }
        }
        _ => ShutdownAction::Keep,
    }
}

// ---------------------------------------------------------------------------
// Scheduler dispatch
// ---------------------------------------------------------------------------

/// The scheduler picked this job for a free slot: spawn a queued job or
/// restore a restore-queued one. The job occupies its slot from now on.
pub fn dispatch(job: &Job) -> Result<Transition, Error> {
    match job.state {
        JobState::Queued { .. } => Ok(Transition::to(JobState::Running).with(Effect::Spawn)),
        JobState::RestoreQueued { .. } => {
            Ok(Transition::to(JobState::Restoring).with(Effect::Restore))
        }
        _ => Err(invalid(job, "dispatched")),
    }
}

// ---------------------------------------------------------------------------
// Runtime events
// ---------------------------------------------------------------------------

/// How a job's process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    Code(i32),
    /// Terminated by this signal number.
    Signal(i32),
}

impl ExitStatus {
    /// The job result for this exit, as pueue reports it: exit code 0 is
    /// success, any other code a failure, and death by signal `Killed`.
    pub fn result(self) -> JobResult {
        match self {
            ExitStatus::Code(0) => JobResult::Success,
            ExitStatus::Code(exit_code) => JobResult::Failed { exit_code },
            ExitStatus::Signal(_) => JobResult::Killed,
        }
    }
}

/// A runtime event reported by the helper (or found during reconciliation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Spawned,
    SpawnFailed {
        reason: String,
    },
    Exited(ExitStatus),
    CheckpointSucceeded,
    CheckpointFailed {
        reason: String,
    },
    RestoreSucceeded,
    RestoreFailed {
        reason: String,
    },
    /// The process is gone and its exit status is unknown (e.g. found missing
    /// after a daemon restart or a crash).
    Lost,
}

impl Event {
    fn verb(&self) -> &'static str {
        match self {
            Event::Spawned => "marked as spawned",
            Event::SpawnFailed { .. } => "marked as failed to spawn",
            Event::Exited(_) => "marked as exited",
            Event::CheckpointSucceeded => "marked as checkpointed",
            Event::CheckpointFailed { .. } => "marked as checkpoint failed",
            Event::RestoreSucceeded => "marked as restored",
            Event::RestoreFailed { .. } => "marked as restore failed",
            Event::Lost => "marked as lost",
        }
    }
}

fn done(result: JobResult) -> Transition {
    Transition::to(JobState::Done { result })
}

/// Apply a runtime event to a job.
///
/// An event that does not fit the current state yields
/// [`Error::InvalidTransition`]; the daemon should log it and keep the state.
///
/// Notes for the helper/daemon:
/// - CRIU stops the process after a successful dump; that must be reported as
///   `CheckpointSucceeded`, not `Exited`. An `Exited` while `Checkpointing`
///   means the job ended on its own before the dump completed.
/// - After a checkpoint failure the job is `Terminating{CheckpointFailed}`
///   and keeps its slots until its `Exited` (or `Lost`) arrives; it then
///   becomes `Done` with the checkpoint-failure result.
pub fn on_event(job: &Job, event: &Event) -> Result<Transition, Error> {
    let state = &job.state;
    let transition = match (event, state) {
        // A spawn is dispatched as Running; this only confirms it.
        (Event::Spawned, JobState::Running) => Transition::unchanged(),
        (Event::SpawnFailed { reason }, JobState::Running) => done(JobResult::FailedToSpawn {
            reason: reason.clone(),
        }),

        (Event::Exited(status), JobState::Running | JobState::Paused | JobState::Checkpointing) => {
            done(status.result())
        }
        (Event::Exited(_) | Event::Lost, JobState::Terminating { result }) => done(result.clone()),

        (Event::CheckpointSucceeded, JobState::Checkpointing) => {
            Transition::to(JobState::Checkpointed)
        }
        // On an OS shutdown the helper dumps running jobs on its own after
        // the daemon has stopped; the next daemon gets the replayed event.
        (Event::CheckpointSucceeded, JobState::Running | JobState::Paused) => {
            Transition::to(JobState::Checkpointed)
        }
        // A failed shutdown dump leaves the job running until systemd stops
        // it; its exit (or `Lost`) follows.
        (Event::CheckpointFailed { .. }, JobState::Running | JobState::Paused) => {
            Transition::unchanged()
        }
        // CRIU leaves the process running when a dump fails; stop it, giving
        // it a chance to save its own checkpoint on SIGTERM.
        (Event::CheckpointFailed { reason }, JobState::Checkpointing) => {
            Transition::to(JobState::Terminating {
                result: JobResult::CheckpointFailed {
                    reason: reason.clone(),
                },
            })
            .with(Effect::Terminate)
            .with(Effect::DeleteCheckpointImage)
        }

        (Event::RestoreSucceeded, JobState::Restoring) => {
            Transition::to(JobState::Running).with(Effect::DeleteCheckpointImage)
        }
        // The restore was given up as interrupted (`Lost` below, e.g. after
        // a helper or daemon restart) but did finish: the helper runs the
        // job, so follow it. The helper only restores on request, so a
        // checkpointed job never gets this otherwise.
        (Event::RestoreSucceeded, JobState::Checkpointed) => {
            Transition::to(JobState::Running).with(Effect::DeleteCheckpointImage)
        }
        (Event::RestoreFailed { reason }, JobState::Restoring) => {
            Transition::to(JobState::Checkpointed).with(Effect::RecordRestoreError(reason.clone()))
        }
        // The restore was already given up as interrupted (`Lost` below)
        // when the helper's own answer arrived: keep its more precise reason.
        (Event::RestoreFailed { reason }, JobState::Checkpointed) => {
            Transition::unchanged().with(Effect::RecordRestoreError(reason.clone()))
        }

        (Event::Lost, JobState::Running | JobState::Paused) => done(JobResult::Lost),
        // An unfinished dump is useless.
        (Event::Lost, JobState::Checkpointing) => {
            done(JobResult::Lost).with(Effect::DeleteCheckpointImage)
        }
        // The image is only deleted after a successful restore, so it is
        // still there to try again.
        (Event::Lost, JobState::Restoring) => Transition::to(JobState::Checkpointed).with(
            Effect::RecordRestoreError("the restore was interrupted".to_owned()),
        ),

        _ => return Err(invalid(job, event.verb())),
    };
    Ok(transition)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::*;
    use crate::model::{DEFAULT_GROUP, JobSpec, Uid};

    const NOW: Timestamp = 1_000;

    fn job(id: u64, state: JobState) -> Job {
        Job {
            id: JobId(id),
            owner: Uid(1000),
            spec: JobSpec {
                command: "true".to_owned(),
                cwd: PathBuf::from("/tmp"),
                envs: BTreeMap::new(),
                group: DEFAULT_GROUP.to_owned(),
                gpus: Vec::new(),
                dependencies: Vec::new(),
                priority: 0,
                label: None,
                no_checkpoint: false,
            },
            state,
            created_at: 0,
            started_at: None,
            finished_at: None,
        }
    }

    fn all_states() -> Vec<JobState> {
        vec![
            JobState::Queued { enqueued_at: 1 },
            JobState::Stashed { enqueue_at: None },
            JobState::Stashed {
                enqueue_at: Some(5),
            },
            JobState::Running,
            JobState::Paused,
            JobState::Checkpointing,
            JobState::Checkpointed,
            JobState::RestoreQueued { enqueued_at: 1 },
            JobState::Restoring,
            JobState::Terminating {
                result: JobResult::CheckpointFailed {
                    reason: "x".to_owned(),
                },
            },
            JobState::Done {
                result: JobResult::Success,
            },
            JobState::Done {
                result: JobResult::Failed { exit_code: 1 },
            },
            JobState::Done {
                result: JobResult::CheckpointFailed {
                    reason: "x".to_owned(),
                },
            },
        ]
    }

    /// Assert that `action` is accepted exactly for the states matched by `ok`.
    fn assert_accepts(
        action: impl Fn(&Job) -> Result<Transition, Error>,
        ok: impl Fn(&JobState) -> bool,
        verb: &str,
    ) {
        for state in all_states() {
            let result = action(&job(7, state.clone()));
            if ok(&state) {
                assert!(result.is_ok(), "{verb}: {state:?} should be accepted");
            } else {
                assert!(
                    matches!(
                        result,
                        Err(Error::InvalidTransition { id: JobId(7), action, state: name })
                            if action == verb && name == state.name()
                    ),
                    "{verb}: {state:?} should be rejected, got {result:?}"
                );
            }
        }
    }

    #[test]
    fn pause_only_running() {
        assert_accepts(pause, |s| matches!(s, JobState::Running), "paused");
        let t = pause(&job(1, JobState::Running)).unwrap();
        assert_eq!(t.next, Some(JobState::Paused));
        assert_eq!(t.effects, vec![Effect::SignalStop]);
    }

    #[test]
    fn start_resumes_paused_and_force_starts_queued_or_stashed() {
        assert_accepts(
            start,
            |s| {
                matches!(
                    s,
                    JobState::Paused | JobState::Queued { .. } | JobState::Stashed { .. }
                )
            },
            "started",
        );
        let t = start(&job(1, JobState::Paused)).unwrap();
        assert_eq!(t.next, Some(JobState::Running));
        assert_eq!(t.effects, vec![Effect::SignalContinue]);

        let t = start(&job(1, JobState::Stashed { enqueue_at: None })).unwrap();
        assert_eq!(t.next, Some(JobState::Running));
        assert_eq!(t.effects, vec![Effect::Spawn]);
    }

    #[test]
    fn start_does_not_resume_checkpointed() {
        assert!(start(&job(1, JobState::Checkpointed)).is_err());
        assert!(
            hint(Action::Start, &JobState::Checkpointed)
                .unwrap()
                .contains("restore")
        );
    }

    #[test]
    fn kill_running_or_paused() {
        assert_accepts(
            |j| kill(j, None),
            |s| matches!(s, JobState::Running | JobState::Paused),
            "killed",
        );
        let t = kill(&job(1, JobState::Paused), None).unwrap();
        assert_eq!(t.next, None);
        assert_eq!(t.effects, vec![Effect::Kill]);
        let t = kill(&job(1, JobState::Running), Some(15)).unwrap();
        assert_eq!(t.effects, vec![Effect::SendSignal(15)]);
    }

    #[test]
    fn kill_checkpointed_suggests_remove() {
        assert!(kill(&job(1, JobState::Checkpointed), None).is_err());
        assert!(
            hint(Action::Kill, &JobState::Checkpointed)
                .unwrap()
                .contains("remove")
        );
    }

    #[test]
    fn remove_states_and_image_deletion() {
        assert_accepts(
            remove,
            |s| {
                matches!(
                    s,
                    JobState::Queued { .. }
                        | JobState::Stashed { .. }
                        | JobState::Done { .. }
                        | JobState::Checkpointed
                        | JobState::RestoreQueued { .. }
                )
            },
            "removed",
        );
        let t = remove(&job(1, JobState::Checkpointed)).unwrap();
        assert_eq!(t.next, None);
        assert_eq!(
            t.effects,
            vec![Effect::DeleteCheckpointImage, Effect::DeleteLog]
        );

        let t = remove(&job(1, JobState::RestoreQueued { enqueued_at: 1 })).unwrap();
        assert!(t.effects.contains(&Effect::DeleteCheckpointImage));

        let t = remove(&job(1, JobState::Queued { enqueued_at: 1 })).unwrap();
        assert_eq!(t.effects, vec![Effect::DeleteLog]);
        assert!(hint(Action::Remove, &JobState::Running).is_some());
    }

    #[test]
    fn clean_eligibility() {
        let success = job(
            1,
            JobState::Done {
                result: JobResult::Success,
            },
        );
        let failed = job(
            2,
            JobState::Done {
                result: JobResult::Failed { exit_code: 3 },
            },
        );
        let ckpt_failed = job(
            3,
            JobState::Done {
                result: JobResult::CheckpointFailed {
                    reason: "dump".to_owned(),
                },
            },
        );
        assert!(is_cleanable(&success, false));
        assert!(is_cleanable(&success, true));
        assert!(is_cleanable(&failed, false));
        assert!(!is_cleanable(&failed, true));
        assert!(!is_cleanable(&job(4, JobState::Checkpointed), false));
        assert!(!is_cleanable(&job(5, JobState::Running), false));

        assert_eq!(
            clean(&success, false).unwrap().effects,
            vec![Effect::DeleteLog]
        );
        assert_eq!(
            clean(&ckpt_failed, false).unwrap().effects,
            vec![Effect::DeleteCheckpointImage, Effect::DeleteLog]
        );
        assert!(clean(&failed, true).is_err());
        assert_accepts(
            |j| clean(j, false),
            |s| matches!(s, JobState::Done { .. }),
            "cleaned",
        );
    }

    #[test]
    fn blocking_dependants_mirrors_pueue() {
        let mut a = job(5, JobState::Queued { enqueued_at: 1 });
        a.spec.dependencies = vec![JobId(1)];
        let mut b = job(6, JobState::Queued { enqueued_at: 1 });
        b.spec.dependencies = vec![JobId(5)];
        let mut finished = job(
            7,
            JobState::Done {
                result: JobResult::Success,
            },
        );
        finished.spec.dependencies = vec![JobId(1)];
        let base = job(
            1,
            JobState::Done {
                result: JobResult::Success,
            },
        );
        let jobs = vec![base, a, b, finished];

        assert_eq!(blocking_dependants(&jobs, JobId(1), &[]), vec![JobId(5)]);
        assert_eq!(
            blocking_dependants(&jobs, JobId(1), &[JobId(1), JobId(5)]),
            vec![JobId(6)]
        );
        assert!(blocking_dependants(&jobs, JobId(1), &[JobId(1), JobId(5), JobId(6)]).is_empty());
        assert!(blocking_dependants(&jobs, JobId(6), &[]).is_empty());
    }

    #[test]
    fn restart_creates_copy_for_done_and_checkpointed() {
        assert_accepts(
            |j| restart(j, false, false, NOW),
            |s| matches!(s, JobState::Done { .. } | JobState::Checkpointed),
            "restarted",
        );
        let t = restart(&job(1, JobState::Checkpointed), false, true, NOW).unwrap();
        assert_eq!(t.next, None);
        assert_eq!(
            t.effects,
            vec![Effect::CreateRestartedCopy { stashed: true }]
        );
        assert!(!t.effects.contains(&Effect::DeleteCheckpointImage));
    }

    #[test]
    fn restart_in_place_only_for_done() {
        assert_accepts(
            |j| restart(j, true, false, NOW),
            |s| matches!(s, JobState::Done { .. }),
            "restarted",
        );
        let done = job(
            1,
            JobState::Done {
                result: JobResult::Killed,
            },
        );
        let t = restart(&done, true, false, NOW).unwrap();
        assert_eq!(t.next, Some(JobState::Queued { enqueued_at: NOW }));
        assert_eq!(t.effects, vec![Effect::ResetLog]);
        let t = restart(&done, true, true, NOW).unwrap();
        assert_eq!(t.next, Some(JobState::Stashed { enqueue_at: None }));
        assert!(hint(Action::Restart, &JobState::Checkpointed).is_some());
    }

    #[test]
    fn stash_and_enqueue() {
        assert_accepts(
            |j| stash(j, None),
            |s| matches!(s, JobState::Queued { .. }),
            "stashed",
        );
        assert_eq!(
            stash(&job(1, JobState::Queued { enqueued_at: 1 }), Some(50))
                .unwrap()
                .next,
            Some(JobState::Stashed {
                enqueue_at: Some(50)
            })
        );

        assert_accepts(
            |j| enqueue(j, None, NOW),
            |s| matches!(s, JobState::Stashed { .. }),
            "enqueued",
        );
        let stashed = job(1, JobState::Stashed { enqueue_at: None });
        assert_eq!(
            enqueue(&stashed, None, NOW).unwrap().next,
            Some(JobState::Queued { enqueued_at: NOW })
        );
        assert_eq!(
            enqueue(&stashed, Some(2_000), NOW).unwrap().next,
            Some(JobState::Stashed {
                enqueue_at: Some(2_000)
            })
        );
    }

    #[test]
    fn enqueue_when_due() {
        let scheduled = job(
            1,
            JobState::Stashed {
                enqueue_at: Some(NOW),
            },
        );
        assert_eq!(
            enqueue_if_due(&scheduled, NOW).unwrap().next,
            Some(JobState::Queued { enqueued_at: NOW })
        );
        assert!(enqueue_if_due(&scheduled, NOW - 1).is_none());
        assert!(enqueue_if_due(&job(2, JobState::Stashed { enqueue_at: None }), NOW).is_none());
        assert!(enqueue_if_due(&job(3, JobState::Queued { enqueued_at: 0 }), NOW).is_none());
    }

    #[test]
    fn switch_rules() {
        let queued = job(1, JobState::Queued { enqueued_at: 1 });
        let stashed = job(2, JobState::Stashed { enqueue_at: None });
        let running = job(3, JobState::Running);
        assert!(check_switch(&queued, &stashed).is_ok());
        assert!(check_switch(&queued, &queued).is_err());
        assert_eq!(
            check_switch(&queued, &running),
            Err(Error::InvalidTransition {
                id: JobId(3),
                action: "switched",
                state: "running",
            })
        );
        assert!(check_switch(&job(4, JobState::Checkpointed), &queued).is_err());
    }

    #[test]
    fn checkpoint_running_or_paused() {
        assert_accepts(
            checkpoint,
            |s| matches!(s, JobState::Running | JobState::Paused),
            "checkpointed",
        );
        let t = checkpoint(&job(1, JobState::Running)).unwrap();
        assert_eq!(t.next, Some(JobState::Checkpointing));
        assert_eq!(t.effects, vec![Effect::Checkpoint]);
        let t = checkpoint(&job(1, JobState::Paused)).unwrap();
        assert_eq!(t.effects, vec![Effect::SignalContinue, Effect::Checkpoint]);
    }

    #[test]
    fn restore_rules() {
        assert_accepts(
            |j| restore(j, false, NOW),
            |s| matches!(s, JobState::Checkpointed),
            "restored",
        );
        assert_accepts(
            |j| restore(j, true, NOW),
            |s| matches!(s, JobState::Checkpointed | JobState::RestoreQueued { .. }),
            "restored",
        );
        let t = restore(&job(1, JobState::Checkpointed), false, NOW).unwrap();
        assert_eq!(t.next, Some(JobState::RestoreQueued { enqueued_at: NOW }));
        assert!(t.effects.is_empty());
        let t = restore(&job(1, JobState::Checkpointed), true, NOW).unwrap();
        assert_eq!(t.next, Some(JobState::Restoring));
        assert_eq!(t.effects, vec![Effect::Restore]);
        assert!(hint(Action::Restore, &JobState::Paused).is_some());
    }

    #[test]
    fn dispatch_rules() {
        assert_accepts(
            dispatch,
            |s| matches!(s, JobState::Queued { .. } | JobState::RestoreQueued { .. }),
            "dispatched",
        );
        let t = dispatch(&job(1, JobState::Queued { enqueued_at: 1 })).unwrap();
        assert_eq!(t.next, Some(JobState::Running));
        assert_eq!(t.effects, vec![Effect::Spawn]);
        let t = dispatch(&job(1, JobState::RestoreQueued { enqueued_at: 1 })).unwrap();
        assert_eq!(t.next, Some(JobState::Restoring));
        assert_eq!(t.effects, vec![Effect::Restore]);
    }

    #[test]
    fn shutdown_actions() {
        let running = job(1, JobState::Running);
        assert!(matches!(
            shutdown_action(&running, true),
            ShutdownAction::Checkpoint(_)
        ));
        assert_eq!(shutdown_action(&running, false), ShutdownAction::Terminate);
        let mut opted_out = running.clone();
        opted_out.spec.no_checkpoint = true;
        assert_eq!(shutdown_action(&opted_out, true), ShutdownAction::Terminate);
        assert!(matches!(
            shutdown_action(&job(2, JobState::Paused), true),
            ShutdownAction::Checkpoint(_)
        ));
        assert_eq!(
            shutdown_action(&job(3, JobState::Queued { enqueued_at: 0 }), true),
            ShutdownAction::Keep
        );
        assert_eq!(
            shutdown_action(&job(4, JobState::Checkpointed), true),
            ShutdownAction::Keep
        );
    }

    #[test]
    fn exit_status_results() {
        assert_eq!(ExitStatus::Code(0).result(), JobResult::Success);
        assert_eq!(
            ExitStatus::Code(2).result(),
            JobResult::Failed { exit_code: 2 }
        );
        assert_eq!(ExitStatus::Signal(9).result(), JobResult::Killed);
    }

    #[test]
    fn spawn_events() {
        let running = job(1, JobState::Running);
        assert_eq!(
            on_event(&running, &Event::Spawned),
            Ok(Transition::unchanged())
        );
        let t = on_event(
            &running,
            &Event::SpawnFailed {
                reason: "no such dir".to_owned(),
            },
        )
        .unwrap();
        assert_eq!(
            t.next,
            Some(JobState::Done {
                result: JobResult::FailedToSpawn {
                    reason: "no such dir".to_owned()
                }
            })
        );
        assert!(
            on_event(
                &job(2, JobState::Queued { enqueued_at: 0 }),
                &Event::Spawned
            )
            .is_err()
        );
    }

    #[test]
    fn exit_events() {
        for state in [JobState::Running, JobState::Paused, JobState::Checkpointing] {
            let t = on_event(&job(1, state), &Event::Exited(ExitStatus::Code(1))).unwrap();
            assert_eq!(
                t.next,
                Some(JobState::Done {
                    result: JobResult::Failed { exit_code: 1 }
                })
            );
        }
        let t = on_event(
            &job(1, JobState::Running),
            &Event::Exited(ExitStatus::Signal(9)),
        )
        .unwrap();
        assert_eq!(
            t.next,
            Some(JobState::Done {
                result: JobResult::Killed
            })
        );
        // Exit after checkpoint failure finishes with the CheckpointFailed result.
        let failed = JobResult::CheckpointFailed {
            reason: "r".to_owned(),
        };
        let terminating = job(
            1,
            JobState::Terminating {
                result: failed.clone(),
            },
        );
        for event in [Event::Exited(ExitStatus::Signal(15)), Event::Lost] {
            assert_eq!(
                on_event(&terminating, &event).unwrap().next,
                Some(JobState::Done {
                    result: failed.clone()
                })
            );
        }
        let already_done = job(1, JobState::Done { result: failed });
        assert!(on_event(&already_done, &Event::Exited(ExitStatus::Signal(15))).is_err());
        let success = job(
            1,
            JobState::Done {
                result: JobResult::Success,
            },
        );
        assert!(on_event(&success, &Event::Exited(ExitStatus::Code(0))).is_err());
        assert!(
            on_event(
                &job(1, JobState::Checkpointed),
                &Event::Exited(ExitStatus::Code(0))
            )
            .is_err()
        );
    }

    #[test]
    fn checkpoint_events() {
        let ckpt = job(1, JobState::Checkpointing);
        let t = on_event(&ckpt, &Event::CheckpointSucceeded).unwrap();
        assert_eq!(t.next, Some(JobState::Checkpointed));
        assert!(t.effects.is_empty());

        let t = on_event(
            &ckpt,
            &Event::CheckpointFailed {
                reason: "criu: dump failed".to_owned(),
            },
        )
        .unwrap();
        assert_eq!(
            t.next,
            Some(JobState::Terminating {
                result: JobResult::CheckpointFailed {
                    reason: "criu: dump failed".to_owned()
                }
            })
        );
        assert!(t.next.as_ref().unwrap().occupies_slot());
        assert_eq!(
            t.effects,
            vec![Effect::Terminate, Effect::DeleteCheckpointImage]
        );
        assert!(on_event(&job(1, JobState::Checkpointed), &Event::CheckpointSucceeded).is_err());
    }

    #[test]
    fn shutdown_dump_by_the_helper() {
        // The helper dumped a running job while the daemon was down.
        for state in [JobState::Running, JobState::Paused] {
            let t = on_event(&job(1, state.clone()), &Event::CheckpointSucceeded).unwrap();
            assert_eq!(t.next, Some(JobState::Checkpointed));
            assert!(t.effects.is_empty());
            let t = on_event(
                &job(1, state),
                &Event::CheckpointFailed {
                    reason: "x".to_owned(),
                },
            )
            .unwrap();
            assert_eq!(t.next, None);
            assert!(t.effects.is_empty());
        }
    }

    #[test]
    fn restore_events() {
        let restoring = job(1, JobState::Restoring);
        let t = on_event(&restoring, &Event::RestoreSucceeded).unwrap();
        assert_eq!(t.next, Some(JobState::Running));
        assert_eq!(t.effects, vec![Effect::DeleteCheckpointImage]);

        let t = on_event(
            &restoring,
            &Event::RestoreFailed {
                reason: "driver mismatch".to_owned(),
            },
        )
        .unwrap();
        assert_eq!(t.next, Some(JobState::Checkpointed));
        assert_eq!(
            t.effects,
            vec![Effect::RecordRestoreError("driver mismatch".to_owned())]
        );
        assert!(!t.effects.contains(&Effect::DeleteCheckpointImage));
        // A success reported after the restore was given up as interrupted
        // brings the job back to running.
        let t = on_event(&job(1, JobState::Checkpointed), &Event::RestoreSucceeded).unwrap();
        assert_eq!(t.next, Some(JobState::Running));
        assert_eq!(t.effects, vec![Effect::DeleteCheckpointImage]);

        // A failure reported after the restore was given up as interrupted
        // only replaces the recorded reason.
        let t = on_event(
            &job(1, JobState::Checkpointed),
            &Event::RestoreFailed {
                reason: "criu restore failed".to_owned(),
            },
        )
        .unwrap();
        assert_eq!(t.next, None);
        assert_eq!(
            t.effects,
            vec![Effect::RecordRestoreError("criu restore failed".to_owned())]
        );
        let failed = Event::RestoreFailed {
            reason: "x".to_owned(),
        };
        assert!(on_event(&job(1, JobState::Running), &failed).is_err());
        assert!(on_event(&job(1, JobState::RestoreQueued { enqueued_at: 0 }), &failed).is_err());
    }

    #[test]
    fn lost_events() {
        for state in [JobState::Running, JobState::Paused] {
            let t = on_event(&job(1, state), &Event::Lost).unwrap();
            assert_eq!(
                t.next,
                Some(JobState::Done {
                    result: JobResult::Lost
                })
            );
        }
        let t = on_event(&job(1, JobState::Checkpointing), &Event::Lost).unwrap();
        assert_eq!(t.effects, vec![Effect::DeleteCheckpointImage]);
        let t = on_event(&job(1, JobState::Restoring), &Event::Lost).unwrap();
        assert_eq!(t.next, Some(JobState::Checkpointed));
        assert!(on_event(&job(1, JobState::Checkpointed), &Event::Lost).is_err());
        assert!(on_event(&job(1, JobState::Queued { enqueued_at: 0 }), &Event::Lost).is_err());
    }

    #[test]
    fn apply_maintains_timestamps() {
        let mut j = job(1, JobState::Queued { enqueued_at: 0 });
        dispatch(&j).unwrap().apply(&mut j, 10);
        assert_eq!(j.state, JobState::Running);
        assert_eq!(j.started_at, Some(10));

        checkpoint(&j).unwrap().apply(&mut j, 20);
        on_event(&j, &Event::CheckpointSucceeded)
            .unwrap()
            .apply(&mut j, 30);
        restore(&j, true, 40).unwrap().apply(&mut j, 40);
        on_event(&j, &Event::RestoreSucceeded)
            .unwrap()
            .apply(&mut j, 50);
        assert_eq!(j.state, JobState::Running);
        assert_eq!(j.started_at, Some(10), "kept across checkpoint/restore");

        on_event(&j, &Event::Exited(ExitStatus::Code(0)))
            .unwrap()
            .apply(&mut j, 60);
        assert_eq!(j.finished_at, Some(60));

        restart(&j, true, false, 70).unwrap().apply(&mut j, 70);
        assert_eq!(j.state, JobState::Queued { enqueued_at: 70 });
        assert_eq!((j.started_at, j.finished_at), (None, None));

        // An unchanged transition leaves the job alone.
        let before = j.clone();
        Transition::unchanged().apply(&mut j, 80);
        assert_eq!(j, before);
    }
}
