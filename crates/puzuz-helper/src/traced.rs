//! Lets single threads of a stopped job run while the rest stays stopped.
//!
//! `cuda-checkpoint` restores a process's CUDA state through a thread of the
//! process itself (its "restore thread"), so it hangs on a stopped process.
//! To restore the CUDA state of a job that must stay paused without running
//! any of its own code (see [`crate::engine`]), the helper does what CRIU's
//! CUDA plugin does while the tasks are still under CRIU's control: it
//! attaches to the restore threads alone with `ptrace`, lets them run, runs
//! `cuda-checkpoint`, and stops and detaches them again. Every other thread
//! stays in its group stop the whole time, and a detached thread whose
//! process is still stopped re-enters that stop (the kernel re-arms the group
//! stop on detach), so the job is as stopped as before.
//!
//! Signals that reach a released thread would run the job's handlers on it.
//! A traced thread reports them to the tracer before they are delivered
//! (a signal-delivery-stop): they are held back and sent to the process
//! again once its threads are detached, so they stay pending until the job
//! is continued. (Their `siginfo` is not kept: they arrive as sent by the
//! helper.)
//!
//! Every `ptrace` request must come from the thread that attached, so
//! [`with_threads_running`] does all of it on the calling thread and runs
//! the work on another one.

use std::thread;
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::ptrace;
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;

/// How often the released threads are checked for stops to answer.
const POLL: Duration = Duration::from_millis(5);

/// A thread under our `ptrace`.
struct Tracee {
    /// The process (thread group) of the thread.
    pid: Pid,
    tid: Pid,
    /// Signals held back from it, to send to `pid` again.
    held: Vec<Signal>,
    /// It has exited (or was killed).
    gone: bool,
    /// Detached already.
    detached: bool,
}

impl Tracee {
    /// Attaches to `tid` (of process `pid`) and waits until it is in a
    /// ptrace stop.
    fn attach(pid: u32, tid: u32) -> Result<Self, String> {
        let mut t = Tracee {
            pid: Pid::from_raw(to_raw(pid)?),
            tid: Pid::from_raw(to_raw(tid)?),
            held: Vec::new(),
            gone: false,
            detached: false,
        };
        ptrace::seize(t.tid, ptrace::Options::empty())
            .map_err(|e| format!("attaching to thread {tid} of process {pid}: {e}"))?;
        // A thread in a group stop traps as soon as it is seized; a running
        // one needs an interrupt.
        if !t.poll()? {
            ptrace::interrupt(t.tid).map_err(|e| t.error("interrupting", e))?;
            t.wait_stop()?;
        }
        Ok(t)
    }

    fn error(&self, what: &str, e: Errno) -> String {
        format!("{what} thread {} of process {}: {e}", self.tid, self.pid)
    }

    /// Handles one pending wait status without blocking. Returns whether
    /// the thread is now in a ptrace stop (or gone).
    fn poll(&mut self) -> Result<bool, String> {
        match waitpid(self.tid, Some(WaitPidFlag::__WALL | WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => Ok(false),
            Ok(status) => {
                self.note(status);
                Ok(true)
            }
            Err(e) => Err(self.error("waiting for", e)),
        }
    }

    /// Blocks until the thread is in a ptrace stop (or gone).
    fn wait_stop(&mut self) -> Result<(), String> {
        loop {
            match waitpid(self.tid, Some(WaitPidFlag::__WALL)) {
                Ok(WaitStatus::Continued(_)) => {}
                Ok(status) => {
                    self.note(status);
                    return Ok(());
                }
                Err(Errno::EINTR) => {}
                Err(e) => return Err(self.error("waiting for", e)),
            }
        }
    }

    /// Records what a wait status says about the thread.
    fn note(&mut self, status: WaitStatus) {
        match status {
            // A signal about to be delivered: held back (the thread goes on
            // without it when continued).
            WaitStatus::Stopped(_, sig) => self.held.push(sig),
            WaitStatus::Exited(..) | WaitStatus::Signaled(..) => self.gone = true,
            // Group stops, the interrupt trap and other events.
            _ => {}
        }
    }

    /// Lets the stopped thread run.
    fn resume(&mut self) -> Result<(), String> {
        if self.gone {
            return Ok(());
        }
        match ptrace::cont(self.tid, None) {
            Ok(()) => Ok(()),
            Err(Errno::ESRCH) if !self.alive() => {
                self.gone = true;
                Ok(())
            }
            Err(e) => Err(self.error("resuming", e)),
        }
    }

    /// While the work runs: answers every stop of the thread by letting it
    /// run on.
    fn service(&mut self) -> Result<(), String> {
        while !self.gone && self.poll()? {
            self.resume()?;
        }
        Ok(())
    }

    /// Stops the thread and detaches from it, leaving it stopped with the
    /// rest of its process.
    fn detach(&mut self) -> Result<(), String> {
        if self.detached {
            return Ok(());
        }
        self.detached = true;
        if self.gone {
            return Ok(());
        }
        match ptrace::interrupt(self.tid) {
            Ok(()) => self.wait_stop()?,
            Err(Errno::ESRCH) => {
                // Already in a stop of ours (it trapped after the last
                // poll), or gone.
                self.poll()?;
            }
            Err(e) => return Err(self.error("interrupting", e)),
        }
        if self.gone {
            return Ok(());
        }
        match ptrace::detach(self.tid, None) {
            Ok(()) => Ok(()),
            Err(Errno::ESRCH) if !self.alive() => {
                self.gone = true;
                Ok(())
            }
            Err(e) => Err(self.error("detaching from", e)),
        }
    }

    fn alive(&self) -> bool {
        std::path::Path::new(&format!("/proc/{}/task/{}", self.pid, self.tid)).exists()
    }
}

impl Drop for Tracee {
    /// Never leaves a thread traced (and with it its process unkillable by
    /// a stop) on an early return.
    fn drop(&mut self) {
        let _ = self.detach();
    }
}

fn to_raw(id: u32) -> Result<i32, String> {
    i32::try_from(id).map_err(|_| format!("invalid thread id {id}"))
}

/// Runs `work` while the threads `threads` (`(process, thread)` pairs) of
/// stopped processes run, then stops them again; the rest of their
/// processes stays stopped throughout (see the module documentation).
/// Signals that reached the threads in the meantime are sent to their
/// processes again afterwards. Fails, without running `work`, if a thread
/// cannot be attached to.
pub fn with_threads_running<R: Send>(
    threads: &[(u32, u32)],
    work: impl FnOnce() -> R + Send,
) -> Result<R, String> {
    let mut tracees = Vec::with_capacity(threads.len());
    for &(pid, tid) in threads {
        tracees.push(Tracee::attach(pid, tid)?);
    }
    for t in &mut tracees {
        t.resume()?;
    }
    let result = thread::scope(|s| {
        let worker = s.spawn(work);
        let mut serviced = Ok(());
        while !worker.is_finished() {
            for t in &mut tracees {
                if serviced.is_ok() {
                    serviced = t.service();
                }
            }
            thread::sleep(POLL);
        }
        let out = worker
            .join()
            .map_err(|_| "the work on the released threads panicked".to_owned());
        serviced.and(out)
    });
    let mut detached = Ok(());
    for t in &mut tracees {
        if let Err(e) = t.detach()
            && detached.is_ok()
        {
            detached = Err(e);
        }
    }
    for t in &tracees {
        for &sig in &t.held {
            let _ = kill(t.pid, sig);
        }
    }
    let out = result?;
    detached.map(|()| out)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::{Child, Command};
    use std::time::Instant;

    use super::*;

    fn states(pid: u32) -> Vec<char> {
        let mut out = Vec::new();
        for entry in fs::read_dir(format!("/proc/{pid}/task")).unwrap() {
            let stat = fs::read_to_string(entry.unwrap().path().join("stat")).unwrap();
            out.push(puzuz_checkpoint::procfs::parse_stat_state(&stat).unwrap());
        }
        out
    }

    fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
        let start = Instant::now();
        while !cond() {
            assert!(start.elapsed() < Duration::from_secs(10), "{what}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    struct Killed(Child);
    impl Drop for Killed {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A stopped single-threaded shell loop is released: it runs (its
    /// counter file grows) while the work runs, and is stopped again
    /// afterwards; SIGCONT then resumes it. A signal sent while it was
    /// released is held back until it is continued. Skipped where ptrace
    /// is not permitted (seccomp, Yama).
    #[test]
    fn system_released_thread_runs_and_stops_again() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("count");
        let marker = dir.path().join("usr1");
        // Builtins only: no child whose SIGCHLD the shell would wait for.
        let script = format!(
            "trap 'echo x >> {m}' USR1; i=0; while :; do i=$((i + 1)); \
             if [ $((i % 500)) -eq 0 ]; then echo x >> {f}; fi; done",
            f = file.display(),
            m = marker.display()
        );
        let child = Killed(Command::new("sh").arg("-c").arg(&script).spawn().unwrap());
        let pid = child.0.id();
        let raw = Pid::from_raw(pid as i32);
        wait_for("the loop starts", || file.exists());
        kill(raw, Signal::SIGSTOP).unwrap();
        wait_for("the loop stops", || states(pid).iter().all(|&s| s == 'T'));
        let before = fs::read_to_string(&file).unwrap().len();

        let grew = match with_threads_running(&[(pid, pid)], || {
            let start = Instant::now();
            // The handler must not run while the thread is released.
            kill(raw, Signal::SIGUSR1).unwrap();
            while start.elapsed() < Duration::from_secs(5) {
                if fs::read_to_string(&file).unwrap().len() > before + 4 {
                    return true;
                }
                thread::sleep(Duration::from_millis(10));
            }
            false
        }) {
            Ok(grew) => grew,
            Err(e) if e.contains("EPERM") => {
                eprintln!("skipping: ptrace is not permitted here ({e})");
                return;
            }
            Err(e) => panic!("{e}"),
        };
        assert!(grew, "the released thread did not run");
        assert!(!marker.exists(), "the USR1 handler ran while released");

        // Stopped again, and it stays so.
        wait_for("stopped again", || states(pid).iter().all(|&s| s == 'T'));
        let stopped_at = fs::read_to_string(&file).unwrap().len();
        thread::sleep(Duration::from_millis(200));
        assert_eq!(fs::read_to_string(&file).unwrap().len(), stopped_at);
        assert!(!marker.exists());

        // SIGCONT resumes it, and the held-back USR1 is delivered then.
        kill(raw, Signal::SIGCONT).unwrap();
        wait_for("running again", || {
            fs::read_to_string(&file).unwrap().len() > stopped_at
        });
        wait_for("USR1 delivered", || marker.exists());
    }
}
