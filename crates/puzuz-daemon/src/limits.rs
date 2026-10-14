//! Per-user limits on what the API keeps open: connections, and the
//! long-lived streams (`log --follow`, `/v1/events`) among the requests.
//!
//! Every user may connect to the API socket, and each open connection or
//! stream costs the daemon a file descriptor and some memory. Without a
//! limit, one user could exhaust the daemon's file descriptors (EMFILE) and
//! lock everybody out. Callers are counted by uid; a request over the limit
//! is answered with `too_many_requests`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Connections one uid may have open at once.
pub const MAX_CONNECTIONS_PER_UID: usize = 64;

/// Connections open at once in total, well below the daemon's file
/// descriptor limit (`LimitNOFILE=` in `puzuz.service`).
pub const MAX_CONNECTIONS: usize = 4096;

/// `log --follow` and `/v1/events` streams one uid may have open at once.
pub const MAX_STREAMS_PER_UID: usize = 16;

/// How long a connection may take to send a request's headers, and how
/// long a keep-alive connection may stay idle between requests.
pub const HEADER_TIMEOUT: Duration = Duration::from_secs(20);

/// Counts holders per uid, up to a limit per uid and one in total.
#[derive(Debug)]
pub struct UidLimiter {
    per_uid: usize,
    total: usize,
    state: Mutex<Counts>,
}

#[derive(Debug, Default)]
struct Counts {
    by_uid: HashMap<u32, usize>,
    total: usize,
}

/// One holder counted by a [`UidLimiter`]; released when dropped.
#[derive(Debug)]
pub struct UidPermit {
    limiter: Arc<UidLimiter>,
    uid: u32,
}

impl UidLimiter {
    pub fn new(per_uid: usize, total: usize) -> Arc<Self> {
        Arc::new(Self {
            per_uid,
            total,
            state: Mutex::default(),
        })
    }

    /// Counts one more holder for `uid`, unless that would exceed a limit.
    pub fn try_acquire(self: &Arc<Self>, uid: u32) -> Option<UidPermit> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let held = state.by_uid.get(&uid).copied().unwrap_or(0);
        if held >= self.per_uid || state.total >= self.total {
            return None;
        }
        state.by_uid.insert(uid, held + 1);
        state.total += 1;
        Some(UidPermit {
            limiter: self.clone(),
            uid,
        })
    }

    /// How many holders `uid` has.
    #[cfg(test)]
    pub fn held(&self, uid: u32) -> usize {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.by_uid.get(&uid).copied().unwrap_or(0)
    }
}

impl Drop for UidPermit {
    fn drop(&mut self) {
        let mut state = self.limiter.state.lock().unwrap_or_else(|e| e.into_inner());
        state.total = state.total.saturating_sub(1);
        if let Some(held) = state.by_uid.get_mut(&self.uid) {
            *held -= 1;
            if *held == 0 {
                state.by_uid.remove(&self.uid);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::UidLimiter;

    #[test]
    fn limits_per_uid_and_in_total() {
        let limiter = UidLimiter::new(2, 3);
        let a1 = limiter.try_acquire(1).unwrap();
        let _a2 = limiter.try_acquire(1).unwrap();
        assert!(limiter.try_acquire(1).is_none(), "per-uid limit");
        let _b1 = limiter.try_acquire(2).unwrap();
        assert!(limiter.try_acquire(3).is_none(), "total limit");
        assert_eq!(limiter.held(1), 2);
        drop(a1);
        assert_eq!(limiter.held(1), 1);
        let _a3 = limiter.try_acquire(1).unwrap();
        assert_eq!(limiter.held(3), 0);
    }
}
