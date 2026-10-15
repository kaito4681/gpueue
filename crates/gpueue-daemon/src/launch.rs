//! Tracks spawn and restore requests the helper has not answered yet
//! (and, in a second instance, checkpoint requests: see
//! `Core::reconcile`).
//!
//! The daemon records a launched job as `Running` (or `Restoring`) in the
//! same transaction that picks it, and only then asks the helper to start
//! it. Until the helper answers, it does not know the job, so a signal or
//! checkpoint request sent in between would be rejected with "job not
//! found". Every launch is therefore registered here when its transaction
//! commits (still under the database lock) and settled once the helper
//! answered (or the request failed). Operations that act on the live
//! process wait for the launch to settle first, then re-read the job.
//!
//! A job can be launched again before the request of its previous launch
//! has returned: a spawn or restore the helper refuses (or abandons) because
//! it is stopping is put back in line by the request's own handler, and the
//! scheduler may pick the job again (once its short launch pause is over)
//! before that handler has returned. Each launch is therefore
//! identified by a sequence number, and settling a launch leaves a newer
//! launch of the same job in flight.
//!
//! Each job's latest launch is remembered with its outcome (see
//! [`LaunchOutcome`]): an action whose launch the helper did not carry out
//! learns from it what became of the retry (see [`Launches::retry_status`]),
//! however late it gets to look.
//!
//! Settled launches are also remembered until the next reconciliation: the
//! helper's list of live jobs that comes with a (re)connect may have been
//! taken before such a spawn reached it, so those jobs are double-checked
//! instead of being marked lost right away. Reconciliation only looks at
//! jobs that hold slots, so a job's entry is dropped once it no longer
//! holds slots (it finished, was put back in line or checkpointed, see
//! [`Launches::prune_settled`]) or is removed: the set does not grow with
//! every launch of a long helper session.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use gpueue_core::model::JobId;
use tokio::sync::watch;

use crate::error::Failure;

/// What became of a spawn or restore request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchOutcome {
    /// The request succeeded: the helper started the job or accepted the
    /// restore (or the spawn failure was recorded as the job's failure).
    Done,
    /// Nothing was started and the job was put back in line.
    Requeued,
    /// Unknown whether the helper got the request; the job is put back in
    /// line if it never did.
    Unconfirmed,
    /// The request failed with this error.
    Failed(Failure),
}

/// What became of a launch, or of the job's launches that followed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryStatus {
    /// The job has not been launched since; the outcome of the launch
    /// itself (`None`: unknown, or not settled yet).
    NotRelaunched(Option<LaunchOutcome>),
    /// The latest launch since is in flight.
    RetryInFlight,
    /// The latest launch since has settled with this outcome (`None`: its
    /// request was abandoned, e.g. because the daemon stopped).
    RetrySettled(Option<LaunchOutcome>),
}

/// The latest launch of a job.
#[derive(Debug)]
struct Record {
    seq: u64,
    in_flight: bool,
    /// Set once settled, unless the request was abandoned.
    outcome: Option<LaunchOutcome>,
    /// The job stopped holding slots while this launch was in flight
    /// (e.g. it exited before its spawn request returned): its settling is
    /// of no interest to the next reconciliation.
    released: bool,
}

#[derive(Debug, Default)]
struct State {
    /// The latest launch of each job.
    jobs: HashMap<JobId, Record>,
    /// The sequence number of the latest launch.
    last: u64,
    /// Launches settled since the last [`Launches::take_settled`].
    settled: HashSet<JobId>,
}

impl State {
    fn in_flight(&self, id: JobId) -> bool {
        self.jobs.get(&id).is_some_and(|r| r.in_flight)
    }
}

#[derive(Debug)]
pub struct Launches {
    state: Mutex<State>,
    changed: watch::Sender<u64>,
}

impl Default for Launches {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
            changed: watch::channel(0).0,
        }
    }
}

impl Launches {
    /// Registers a launch whose request is about to be sent.
    pub fn begin(&self, id: JobId) {
        let mut state = self.state.lock().unwrap();
        state.last += 1;
        let seq = state.last;
        state.jobs.insert(
            id,
            Record {
                seq,
                in_flight: true,
                outcome: None,
                released: false,
            },
        );
    }

    /// The sequence number of the latest launch of `id`.
    pub fn latest(&self, id: JobId) -> Option<u64> {
        self.state.lock().unwrap().jobs.get(&id).map(|r| r.seq)
    }

    /// What became of launch `seq` of `id`, or of the launches of the job
    /// that followed it. Consistent with the database when called under
    /// its lock, since launches are registered under it.
    pub fn retry_status(&self, id: JobId, seq: u64) -> RetryStatus {
        match self.state.lock().unwrap().jobs.get(&id) {
            Some(record) if record.seq > seq && record.in_flight => RetryStatus::RetryInFlight,
            Some(record) if record.seq > seq => RetryStatus::RetrySettled(record.outcome.clone()),
            Some(record) if record.seq == seq && !record.in_flight => {
                RetryStatus::NotRelaunched(record.outcome.clone())
            }
            _ => RetryStatus::NotRelaunched(None),
        }
    }

    /// Swaps the records of two jobs whose ids `switch` swapped.
    pub fn swap(&self, a: JobId, b: JobId) {
        let mut state = self.state.lock().unwrap();
        let (ra, rb) = (state.jobs.remove(&a), state.jobs.remove(&b));
        if let Some(record) = ra {
            state.jobs.insert(b, record);
        }
        if let Some(record) = rb {
            state.jobs.insert(a, record);
        }
        let (sa, sb) = (state.settled.remove(&a), state.settled.remove(&b));
        if sa {
            state.settled.insert(b);
        }
        if sb {
            state.settled.insert(a);
        }
    }

    /// Forgets the launches of a removed job.
    pub fn forget(&self, id: JobId) {
        let mut state = self.state.lock().unwrap();
        if !state.in_flight(id) {
            state.jobs.remove(&id);
        }
        state.settled.remove(&id);
    }

    /// Drops `id` from the launches settled since the last
    /// [`Launches::take_settled`]: the job no longer holds slots, so the
    /// next reconciliation does not look at it.
    pub fn prune_settled(&self, id: JobId) {
        let mut state = self.state.lock().unwrap();
        state.settled.remove(&id);
        if let Some(record) = state.jobs.get_mut(&id)
            && record.in_flight
        {
            record.released = true;
        }
    }

    /// Returns a guard that settles the current launch of `id` when
    /// dropped (with the outcome given to [`SettleGuard::finish`], if any),
    /// so a launch is settled even if the request future is cancelled.
    /// Taken before the request is sent: a later launch of the job (once
    /// this one has put it back in line) stays in flight.
    pub fn settle_on_drop(&self, id: JobId) -> SettleGuard<'_> {
        let seq = self
            .state
            .lock()
            .unwrap()
            .jobs
            .get(&id)
            .filter(|r| r.in_flight)
            .map(|r| r.seq);
        SettleGuard {
            launches: self,
            id,
            seq,
            outcome: None,
        }
    }

    fn settle(&self, id: JobId, seq: Option<u64>, outcome: Option<LaunchOutcome>) {
        {
            let mut state = self.state.lock().unwrap();
            let mut track = true;
            if let Some(record) = state.jobs.get_mut(&id)
                && seq == Some(record.seq)
                && record.in_flight
            {
                record.in_flight = false;
                record.outcome = outcome;
                track = !record.released;
            }
            if track {
                state.settled.insert(id);
            }
        }
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }

    pub fn in_flight(&self, id: JobId) -> bool {
        self.state.lock().unwrap().in_flight(id)
    }

    /// The launches settled since the previous call.
    pub fn take_settled(&self) -> HashSet<JobId> {
        std::mem::take(&mut self.state.lock().unwrap().settled)
    }

    /// Resolves once the launch of `id` (if any) has settled.
    pub async fn wait(&self, id: JobId) {
        self.wait_until(|state| !state.in_flight(id)).await;
    }

    /// Resolves once no launch is in flight.
    pub async fn wait_idle(&self) {
        self.wait_until(|state| !state.jobs.values().any(|r| r.in_flight))
            .await;
    }

    async fn wait_until(&self, done: impl Fn(&State) -> bool) {
        // Subscribe before checking so a settle in between is not missed.
        let mut rx = self.changed.subscribe();
        loop {
            if done(&self.state.lock().unwrap()) {
                return;
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

/// See [`Launches::settle_on_drop`].
pub struct SettleGuard<'a> {
    launches: &'a Launches,
    id: JobId,
    /// The launch this guard settles (`None`: none was in flight).
    seq: Option<u64>,
    outcome: Option<LaunchOutcome>,
}

impl SettleGuard<'_> {
    /// Settles the launch with `outcome`.
    pub fn finish(mut self, outcome: LaunchOutcome) {
        self.outcome = Some(outcome);
    }
}

impl Drop for SettleGuard<'_> {
    fn drop(&mut self) {
        self.launches.settle(self.id, self.seq, self.outcome.take());
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use gpueue_protocol::api::ErrorCode;

    use super::*;

    #[tokio::test]
    async fn waiters_resume_when_the_launch_settles() {
        let launches = Arc::new(Launches::default());
        let id = JobId(1);
        // Nothing in flight: no wait.
        launches.wait(id).await;
        launches.begin(id);
        assert!(launches.in_flight(id));
        let waiter = tokio::spawn({
            let launches = launches.clone();
            async move { launches.wait(id).await }
        });
        let idle = tokio::spawn({
            let launches = launches.clone();
            async move { launches.wait_idle().await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished());
        assert!(!idle.is_finished());
        // Another job settling does not release the waiter.
        launches.begin(JobId(2));
        drop(launches.settle_on_drop(JobId(2)));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished());
        drop(launches.settle_on_drop(id));
        waiter.await.unwrap();
        idle.await.unwrap();
        assert!(!launches.in_flight(id));
        assert_eq!(launches.take_settled(), HashSet::from([id, JobId(2)]));
        assert!(launches.take_settled().is_empty());
    }

    #[test]
    fn settling_a_launch_leaves_a_newer_launch_of_the_job_in_flight() {
        let launches = Launches::default();
        let id = JobId(1);
        launches.begin(id);
        let first = launches.settle_on_drop(id);
        // The first request put the job back in line and it was launched
        // again before that request returned.
        launches.begin(id);
        let second = launches.settle_on_drop(id);
        drop(first);
        assert!(launches.in_flight(id));
        drop(second);
        assert!(!launches.in_flight(id));
        // A guard taken with nothing in flight settles nothing later.
        let stale = launches.settle_on_drop(id);
        launches.begin(id);
        drop(stale);
        assert!(launches.in_flight(id));
    }

    #[test]
    fn the_outcome_of_a_retry_is_kept_for_the_launch_it_follows() {
        let launches = Launches::default();
        let id = JobId(1);
        launches.begin(id);
        let first_seq = launches.latest(id).unwrap();
        let first = launches.settle_on_drop(id);
        assert_eq!(
            launches.retry_status(id, first_seq),
            RetryStatus::NotRelaunched(None)
        );
        // Requeued by its own handler, then launched again before the
        // first request returned.
        launches.begin(id);
        let second = launches.settle_on_drop(id);
        first.finish(LaunchOutcome::Requeued);
        assert_eq!(
            launches.retry_status(id, first_seq),
            RetryStatus::RetryInFlight
        );
        second.finish(LaunchOutcome::Done);
        assert_eq!(
            launches.retry_status(id, first_seq),
            RetryStatus::RetrySettled(Some(LaunchOutcome::Done))
        );
        // Nothing follows the latest launch: its own outcome.
        let second_seq = launches.latest(id).unwrap();
        assert_eq!(
            launches.retry_status(id, second_seq),
            RetryStatus::NotRelaunched(Some(LaunchOutcome::Done))
        );

        // A failed retry keeps its error; an abandoned one has no outcome.
        launches.begin(id);
        let failure = Failure::new(ErrorCode::Internal, "boom");
        launches
            .settle_on_drop(id)
            .finish(LaunchOutcome::Failed(failure.clone()));
        assert_eq!(
            launches.retry_status(id, second_seq),
            RetryStatus::RetrySettled(Some(LaunchOutcome::Failed(failure)))
        );
        launches.begin(id);
        drop(launches.settle_on_drop(id));
        assert_eq!(
            launches.retry_status(id, second_seq),
            RetryStatus::RetrySettled(None)
        );

        // Forgotten once removed, but not while in flight.
        launches.begin(id);
        launches.forget(id);
        assert!(launches.in_flight(id));
        drop(launches.settle_on_drop(id));
        launches.forget(id);
        assert_eq!(launches.latest(id), None);
    }

    #[test]
    fn settled_launches_are_pruned() {
        let launches = Launches::default();
        for n in 1..=3 {
            launches.begin(JobId(n));
            drop(launches.settle_on_drop(JobId(n)));
        }
        // Job 1 no longer holds slots; job 2 was removed.
        launches.prune_settled(JobId(1));
        launches.forget(JobId(2));
        assert_eq!(launches.take_settled(), HashSet::from([JobId(3)]));
        // A job that stops holding slots before its launch settles.
        launches.begin(JobId(4));
        launches.prune_settled(JobId(4));
        drop(launches.settle_on_drop(JobId(4)));
        assert!(!launches.in_flight(JobId(4)));
        assert!(launches.take_settled().is_empty());
        // Its next launch is tracked again.
        launches.begin(JobId(4));
        drop(launches.settle_on_drop(JobId(4)));
        assert_eq!(launches.take_settled(), HashSet::from([JobId(4)]));
    }
}
