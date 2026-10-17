//! The daemon's sources of time, behind a trait so tests can control
//! them.
//!
//! Wall-clock time ([`Clock::now_ms`]) stamps state changes and bills run
//! time (see `priority.rs`); monotonic time ([`Clock::instant`]) measures
//! the intervals of the checkpoint size reminders (see `usage.rs`). Timers
//! (grace periods, re-checks, retries) are tokio timers and do not go
//! through the clock.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpueue_core::model::Timestamp;

/// A source of wall-clock and monotonic time.
pub trait Clock: Send + Sync + 'static {
    /// Wall-clock time in Unix epoch milliseconds.
    fn now_ms(&self) -> Timestamp;

    /// Monotonic time, for measuring intervals.
    fn instant(&self) -> Instant;
}

/// The system's clocks.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> Timestamp {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as Timestamp)
            .unwrap_or(0)
    }

    fn instant(&self) -> Instant {
        Instant::now()
    }
}

/// A clock that stands still until [`ManualClock::advance`]d, for tests.
/// Clones share the same time, so one instance can outlive a daemon and be
/// handed to the next one.
#[derive(Debug, Clone)]
pub struct ManualClock {
    /// The wall-clock and monotonic time it started at.
    base: (Timestamp, Instant),
    /// How far it has been advanced.
    elapsed: Arc<Mutex<Duration>>,
}

impl ManualClock {
    /// A clock standing at the system's current time.
    pub fn new() -> Self {
        Self {
            base: (SystemClock.now_ms(), Instant::now()),
            elapsed: Arc::default(),
        }
    }

    /// Moves both times forward by `by`.
    pub fn advance(&self, by: Duration) {
        *self.elapsed.lock().unwrap() += by;
    }

    fn elapsed(&self) -> Duration {
        *self.elapsed.lock().unwrap()
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> Timestamp {
        self.base.0 + self.elapsed().as_millis() as Timestamp
    }

    fn instant(&self) -> Instant {
        self.base.1 + self.elapsed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manual_clock_moves_only_when_advanced() {
        let clock = ManualClock::new();
        let (wall, mono) = (clock.now_ms(), clock.instant());
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!((clock.now_ms(), clock.instant()), (wall, mono));
        clock.clone().advance(Duration::from_millis(1500));
        assert_eq!(clock.now_ms(), wall + 1500);
        assert_eq!(clock.instant() - mono, Duration::from_millis(1500));
    }
}
