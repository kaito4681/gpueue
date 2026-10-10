//! Tracks spawn and restore requests the helper has not answered yet.
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
//! scheduler may pick the job again right away. Each launch is therefore
//! identified by a sequence number, and settling a launch leaves a newer
//! launch of the same job in flight.
//!
//! Settled launches are also remembered until the next reconciliation: the
//! helper's list of live jobs that comes with a (re)connect may have been
//! taken before such a spawn reached it, so those jobs are double-checked
//! instead of being marked lost right away.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use puzuz_core::model::JobId;
use tokio::sync::watch;

#[derive(Debug, Default)]
struct State {
    /// The launches in flight, with the sequence number of each.
    in_flight: HashMap<JobId, u64>,
    /// The sequence number of the latest launch.
    last: u64,
    /// Launches settled since the last [`Launches::take_settled`].
    settled: HashSet<JobId>,
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
        state.in_flight.insert(id, seq);
    }

    /// Returns a guard that settles the current launch of `id` when
    /// dropped, so a launch is settled even if the request future is
    /// cancelled. Taken before the request is sent: a later launch of the
    /// job (once this one has put it back in line) stays in flight.
    pub fn settle_on_drop(&self, id: JobId) -> SettleGuard<'_> {
        let seq = self.state.lock().unwrap().in_flight.get(&id).copied();
        SettleGuard {
            launches: self,
            id,
            seq,
        }
    }

    fn settle(&self, id: JobId, seq: Option<u64>) {
        {
            let mut state = self.state.lock().unwrap();
            if seq.is_some() && state.in_flight.get(&id).copied() == seq {
                state.in_flight.remove(&id);
            }
            state.settled.insert(id);
        }
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }

    pub fn in_flight(&self, id: JobId) -> bool {
        self.state.lock().unwrap().in_flight.contains_key(&id)
    }

    /// The launches settled since the previous call.
    pub fn take_settled(&self) -> HashSet<JobId> {
        std::mem::take(&mut self.state.lock().unwrap().settled)
    }

    /// Resolves once the launch of `id` (if any) has settled.
    pub async fn wait(&self, id: JobId) {
        self.wait_until(|state| !state.in_flight.contains_key(&id))
            .await;
    }

    /// Resolves once no launch is in flight.
    pub async fn wait_idle(&self) {
        self.wait_until(|state| state.in_flight.is_empty()).await;
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
}

impl Drop for SettleGuard<'_> {
    fn drop(&mut self) {
        self.launches.settle(self.id, self.seq);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

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
}
