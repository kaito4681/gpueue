//! The persistent event queue.
//!
//! Events are appended (and fsynced) to `events.jsonl` before they are sent,
//! and stay there until the daemon acknowledges them with `ack_events`.
//! `state.json` remembers the acknowledged sequence number and the next one
//! to hand out, so sequence numbers keep increasing across helper restarts
//! even when the log has been compacted to nothing. It also holds the
//! queue's epoch, a random number picked when the queue starts out (or when
//! a `state.json` from before epochs is first read), which the Hello
//! reports: should the state directory be wiped, the numbering starts over
//! under a new epoch, and the daemon knows that the sequence numbers are
//! new.
//!
//! Crash safety: an append that was cut short leaves an unparsable last line,
//! which is ignored on load (the event was never sent, because sending
//! happens after the fsync). Compaction writes `state.json` first and then
//! atomically replaces the log, so a crash in between only means that some
//! acknowledged events are filtered out on the next load.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use gpueue_checkpoint::layout::write_atomic;
use gpueue_protocol::helper::{Event, EventEnvelope, EventSeq};
use serde::{Deserialize, Serialize};

const LOG_FILE: &str = "events.jsonl";
const STATE_FILE: &str = "state.json";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
struct StoreState {
    /// Every event with `seq <= acked` may be forgotten.
    acked: u64,
    /// The next sequence number to hand out (at least 1).
    next: u64,
    /// See the [module documentation](self); 0 until one is picked.
    #[serde(default)]
    epoch: u64,
}

/// A new epoch: nonzero, and small enough for the daemon to store as a
/// signed 64-bit integer.
fn new_epoch() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mixed = nanos ^ u64::from(std::process::id()).rotate_left(40);
    (mixed & i64::MAX as u64).max(1)
}

/// The on-disk event queue. Not thread-safe by itself; the helper wraps it
/// in a mutex.
#[derive(Debug)]
pub struct EventStore {
    dir: PathBuf,
    state: StoreState,
    /// Unacknowledged events, in sequence order.
    pending: Vec<EventEnvelope>,
    log: File,
}

impl EventStore {
    /// Open (creating if needed) the queue in `dir`.
    pub fn open(dir: &Path) -> io::Result<Self> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let mut state: StoreState = match fs::read(dir.join(STATE_FILE)) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => StoreState::default(),
            Err(e) => return Err(e),
        };
        let mut pending = Vec::new();
        match File::open(dir.join(LOG_FILE)) {
            Ok(f) => {
                for line in BufReader::new(f).lines() {
                    let line = line?;
                    if line.trim().is_empty() {
                        continue;
                    }
                    // A torn final line from a crash mid-append is skipped.
                    let Ok(env) = serde_json::from_str::<EventEnvelope>(&line) else {
                        continue;
                    };
                    state.next = state.next.max(env.seq.0 + 1);
                    if env.seq.0 > state.acked {
                        pending.push(env);
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        state.next = state.next.max(state.acked + 1).max(1);
        if state.epoch == 0 {
            // Persisted by the compaction below.
            state.epoch = new_epoch();
        }
        pending.sort_by_key(|e| e.seq);
        pending.dedup_by_key(|e| e.seq);

        let mut store = Self {
            dir: dir.to_owned(),
            state,
            pending,
            log: open_log(&dir.join(LOG_FILE))?,
        };
        // Start from a clean log: drops torn lines and acknowledged events.
        store.compact()?;
        Ok(store)
    }

    /// Persist `event` and return it with its sequence number.
    pub fn append(&mut self, event: Event) -> io::Result<EventEnvelope> {
        let env = EventEnvelope {
            seq: EventSeq(self.state.next),
            event,
        };
        let mut line = serde_json::to_vec(&env)?;
        line.push(b'\n');
        self.log.write_all(&line)?;
        self.log.sync_data()?;
        self.state.next += 1;
        self.pending.push(env.clone());
        Ok(env)
    }

    /// Events not yet acknowledged, oldest first.
    pub fn pending(&self) -> &[EventEnvelope] {
        &self.pending
    }

    /// The highest acknowledged sequence number (0 if none).
    pub fn acked(&self) -> EventSeq {
        EventSeq(self.state.acked)
    }

    /// The queue's epoch (see the [module documentation](self)).
    pub fn epoch(&self) -> u64 {
        self.state.epoch
    }

    /// The sequence number the next event will get.
    pub fn next_seq(&self) -> EventSeq {
        EventSeq(self.state.next)
    }

    /// Forget every event up to and including `up_to`. Acknowledging a
    /// sequence number that was never handed out is clamped.
    pub fn ack(&mut self, up_to: EventSeq) -> io::Result<()> {
        let up_to = up_to.0.min(self.state.next - 1);
        if up_to <= self.state.acked {
            return Ok(());
        }
        self.state.acked = up_to;
        self.pending.retain(|e| e.seq.0 > up_to);
        self.compact()
    }

    fn compact(&mut self) -> io::Result<()> {
        write_atomic(
            &self.dir.join(STATE_FILE),
            &serde_json::to_vec(&self.state)?,
        )?;
        let mut buf = Vec::new();
        for env in &self.pending {
            serde_json::to_writer(&mut buf, env)?;
            buf.push(b'\n');
        }
        let path = self.dir.join(LOG_FILE);
        write_atomic(&path, &buf)?;
        self.log = open_log(&path)?;
        Ok(())
    }
}

fn open_log(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpueue_protocol::helper::{ExitStatus, JobId};

    fn exited(id: u64, code: i32) -> Event {
        Event::JobExited {
            job: JobId(id),
            status: ExitStatus::Exited { code },
        }
    }

    #[test]
    fn append_ack_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = EventStore::open(dir.path()).unwrap();
        assert_eq!(s.append(exited(1, 0)).unwrap().seq, EventSeq(1));
        assert_eq!(s.append(exited(2, 1)).unwrap().seq, EventSeq(2));
        assert_eq!(s.append(exited(3, 2)).unwrap().seq, EventSeq(3));
        drop(s);

        let mut s = EventStore::open(dir.path()).unwrap();
        assert_eq!(s.pending().len(), 3);
        s.ack(EventSeq(2)).unwrap();
        assert_eq!(s.pending().len(), 1);
        drop(s);

        let mut s = EventStore::open(dir.path()).unwrap();
        assert_eq!(s.pending()[0].seq, EventSeq(3));
        s.ack(EventSeq(100)).unwrap();
        assert!(s.pending().is_empty());
        assert_eq!(s.acked(), EventSeq(3));
        drop(s);

        // Numbering continues even though the log is now empty.
        let mut s = EventStore::open(dir.path()).unwrap();
        assert_eq!(s.append(exited(4, 0)).unwrap().seq, EventSeq(4));
    }

    #[test]
    fn torn_last_line_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = EventStore::open(dir.path()).unwrap();
        s.append(exited(1, 0)).unwrap();
        drop(s);
        let mut f = OpenOptions::new()
            .append(true)
            .open(dir.path().join(LOG_FILE))
            .unwrap();
        f.write_all(br#"{"seq":2,"event":"job_ex"#).unwrap();
        drop(f);

        let mut s = EventStore::open(dir.path()).unwrap();
        assert_eq!(s.pending().len(), 1);
        assert_eq!(s.append(exited(2, 0)).unwrap().seq, EventSeq(2));
        drop(s);
        let s = EventStore::open(dir.path()).unwrap();
        assert_eq!(s.pending().len(), 2);
    }

    #[test]
    fn stale_acked_events_in_log_are_filtered() {
        // Simulates a crash between writing state.json and compacting.
        let dir = tempfile::tempdir().unwrap();
        let mut s = EventStore::open(dir.path()).unwrap();
        s.append(exited(1, 0)).unwrap();
        s.append(exited(2, 0)).unwrap();
        drop(s);
        write_atomic(&dir.path().join(STATE_FILE), br#"{"acked":1,"next":3}"#).unwrap();
        let s = EventStore::open(dir.path()).unwrap();
        assert_eq!(s.pending().len(), 1);
        assert_eq!(s.next_seq(), EventSeq(3));
    }

    /// The epoch stays with the queue and changes when it starts over; a
    /// `state.json` from before epochs gets one.
    #[test]
    fn epoch_is_kept_until_the_state_is_wiped() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = EventStore::open(dir.path()).unwrap();
        let epoch = s.epoch();
        assert!(epoch > 0 && epoch <= i64::MAX as u64);
        s.append(exited(1, 0)).unwrap();
        s.ack(EventSeq(1)).unwrap();
        drop(s);
        assert_eq!(EventStore::open(dir.path()).unwrap().epoch(), epoch);

        write_atomic(&dir.path().join(STATE_FILE), br#"{"acked":1,"next":2}"#).unwrap();
        let s = EventStore::open(dir.path()).unwrap();
        let upgraded = s.epoch();
        assert!(upgraded > 0);
        assert_eq!(s.next_seq(), EventSeq(2));
        drop(s);
        assert_eq!(EventStore::open(dir.path()).unwrap().epoch(), upgraded);

        // A wiped queue starts under a new epoch.
        std::thread::sleep(std::time::Duration::from_millis(1));
        let wiped = tempfile::tempdir().unwrap();
        assert_ne!(EventStore::open(wiped.path()).unwrap().epoch(), upgraded);
    }
}
