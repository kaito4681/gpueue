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
//! A thread is only attached to once its whole process is in its group stop
//! (every thread `T`), so no stop signal is still on its way, and only if it
//! is a thread of that process and in the cgroup of the job's init: the
//! thread id comes from `cuda-checkpoint`, and a wrong one must not release a
//! thread of someone else's process.
//!
//! Signals that reach a released thread would run the job's handlers on it.
//! A traced thread reports them to the tracer before they are delivered
//! (a signal-delivery-stop): they are held back and sent to the process
//! again once its threads are detached, so they stay pending until the job
//! is continued. (Their `siginfo` is not kept: they arrive as sent by the
//! helper.)
//!
//! Every `ptrace` request must come from the thread that attached, so
//! [`with_threads_running`] does all of it on a thread of its own and runs
//! the work on another one. It never blocks in `waitpid`: every wait polls
//! against a deadline, a thread that does not stop in time has its process
//! killed, and the tracer thread exits in the end, which detaches the kernel
//! from anything still traced.

use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::ptrace;
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;

/// How often stops and states are polled.
const POLL: Duration = Duration::from_millis(5);
/// How long a process may take to finish its group stop before its thread
/// is released, and an attached thread to report its stop.
const SETTLE: Duration = Duration::from_secs(2);
/// How long a released thread may take to stop again once the work is done
/// (also when the work used up the whole deadline).
const STOP_GRACE: Duration = Duration::from_secs(5);

/// A thread under our `ptrace`.
struct Tracee {
    /// The process (thread group) of the thread.
    pid: Pid,
    tid: Pid,
    /// Signals held back from it, to send to `pid` again.
    held: Vec<Signal>,
    /// In a ptrace stop whose wait status was collected: it only moves on
    /// when resumed (or detached), and no further status comes until then.
    in_stop: bool,
    /// It has exited (or was killed).
    gone: bool,
    /// Detached already (or given up on).
    detached: bool,
}

impl Tracee {
    /// Attaches to `tid`, which must be a thread of `pid` in the cgroup of
    /// `init`, and waits for the stop it is in (its process is group-stopped).
    fn attach(init: u32, pid: u32, tid: u32) -> Result<Self, String> {
        let mut t = Tracee {
            pid: Pid::from_raw(to_raw(pid)?),
            tid: Pid::from_raw(to_raw(tid)?),
            held: Vec::new(),
            in_stop: false,
            gone: false,
            detached: false,
        };
        ptrace::seize(t.tid, ptrace::Options::empty())
            .map_err(|e| format!("attaching to thread {tid} of process {pid}: {e}"))?;
        // From here on `t` is dropped (and detached) on every early return.
        // Checked once seized: a traced thread's id cannot be reused.
        check_member(init, pid, tid)?;
        // A thread in a group stop traps as soon as it is seized.
        let until = Instant::now() + SETTLE;
        while !t.in_stop && !t.gone {
            if !t.poll()? {
                if Instant::now() >= until {
                    return Err(format!(
                        "thread {tid} of process {pid} did not report its stop when attached to"
                    ));
                }
                thread::sleep(POLL);
            }
        }
        if t.gone {
            return Err(format!("thread {tid} of process {pid} is gone"));
        }
        Ok(t)
    }

    fn error(&self, what: &str, e: Errno) -> String {
        format!("{what} thread {} of process {}: {e}", self.tid, self.pid)
    }

    /// Handles one pending wait status without blocking. Returns whether
    /// there was one.
    fn poll(&mut self) -> Result<bool, String> {
        match waitpid(self.tid, Some(WaitPidFlag::__WALL | WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => Ok(false),
            Ok(status) => {
                self.note(status);
                Ok(true)
            }
            Err(Errno::ECHILD) => {
                // Not (any longer) ours to wait for.
                self.gone = true;
                Ok(true)
            }
            Err(e) => Err(self.error("waiting for", e)),
        }
    }

    /// Records what a wait status says about the thread.
    fn note(&mut self, status: WaitStatus) {
        match status {
            // A signal about to be delivered: held back (the thread goes on
            // without it when continued).
            WaitStatus::Stopped(_, sig) => {
                self.held.push(sig);
                self.in_stop = true;
            }
            WaitStatus::PtraceEvent(..) | WaitStatus::PtraceSyscall(_) => self.in_stop = true,
            WaitStatus::Exited(..) | WaitStatus::Signaled(..) => {
                self.gone = true;
                self.in_stop = false;
            }
            _ => {}
        }
    }

    /// Lets the stopped thread run.
    fn resume(&mut self) -> Result<(), String> {
        if self.gone || !self.in_stop {
            return Ok(());
        }
        match ptrace::cont(self.tid, None) {
            Ok(()) => {
                self.in_stop = false;
                Ok(())
            }
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

    /// Stops the thread (unless it is in a stop already) and detaches from
    /// it, leaving it stopped with the rest of its process. Gives up at
    /// `until`: `Err` with `true` when the thread did not stop in time (it
    /// stays attached until the tracer thread ends).
    fn detach(&mut self, until: Instant) -> Result<(), (String, bool)> {
        if self.detached {
            return Ok(());
        }
        self.detached = true;
        if !self.gone && !self.in_stop {
            if let Err(e) = ptrace::interrupt(self.tid) {
                if e == Errno::ESRCH && !self.alive() {
                    self.gone = true;
                } else {
                    return Err((self.error("interrupting", e), false));
                }
            }
            while !self.gone && !self.in_stop {
                match self.poll() {
                    Ok(true) => {}
                    Ok(false) if Instant::now() >= until => {
                        return Err((
                            format!(
                                "thread {} of process {} did not stop within {}s",
                                self.tid,
                                self.pid,
                                STOP_GRACE.as_secs()
                            ),
                            true,
                        ));
                    }
                    Ok(false) => thread::sleep(POLL),
                    Err(e) => return Err((e, false)),
                }
            }
        }
        if self.gone {
            return Ok(());
        }
        match ptrace::detach(self.tid, None) {
            Ok(()) => {
                self.in_stop = false;
                Ok(())
            }
            Err(Errno::ESRCH) if !self.alive() => {
                self.gone = true;
                Ok(())
            }
            Err(e) => Err((self.error("detaching from", e), false)),
        }
    }

    fn alive(&self) -> bool {
        Path::new(&format!("/proc/{}/task/{}", self.pid, self.tid)).exists()
    }
}

impl Drop for Tracee {
    /// Never leaves a thread running under our trace on an early return.
    fn drop(&mut self) {
        if let Err((e, _)) = self.detach(Instant::now() + STOP_GRACE) {
            eprintln!("puzuz-helper: {e}");
        }
    }
}

fn to_raw(id: u32) -> Result<i32, String> {
    i32::try_from(id).map_err(|_| format!("invalid thread id {id}"))
}

/// `tid` is a thread of `pid`, in the same cgroup as `init`.
fn check_member(init: u32, pid: u32, tid: u32) -> Result<(), String> {
    let task = format!("/proc/{pid}/task/{tid}");
    let cgroup = fs::read_to_string(format!("{task}/cgroup"))
        .map_err(|_| format!("thread {tid} is not a thread of process {pid}"))?;
    let job = fs::read_to_string(format!("/proc/{init}/cgroup"))
        .map_err(|e| format!("reading the cgroup of process {init}: {e}"))?;
    if cgroup != job {
        return Err(format!(
            "thread {tid} of process {pid} is not in the cgroup of the job"
        ));
    }
    Ok(())
}

/// Waits until every thread of `pid` is in a group stop (`T`).
fn wait_group_stopped(pid: u32) -> Result<(), String> {
    let until = Instant::now() + SETTLE;
    loop {
        let mut running = None;
        let tasks = fs::read_dir(format!("/proc/{pid}/task"))
            .map_err(|e| format!("listing the threads of process {pid}: {e}"))?;
        for task in tasks.flatten() {
            let Ok(stat) = fs::read_to_string(task.path().join("stat")) else {
                continue;
            };
            match puzuz_checkpoint::procfs::parse_stat_state(&stat) {
                Some('T' | 'Z' | 'X') | None => {}
                Some(state) => running = Some((task.file_name(), state)),
            }
        }
        let Some((tid, state)) = running else {
            return Ok(());
        };
        if Instant::now() >= until {
            return Err(format!(
                "process {pid} is not stopped (thread {} is in state {state})",
                tid.to_string_lossy()
            ));
        }
        thread::sleep(POLL);
    }
}

/// Runs `work` while the threads `threads` (`(process, thread)` pairs) of
/// stopped processes run, then stops them again; the rest of their
/// processes stays stopped throughout (see the module documentation). The
/// processes must belong to the job whose namespace init is `init` (same
/// cgroup). Signals that reached the threads in the meantime are sent to
/// their processes again afterwards. Fails, without running `work`, if a
/// process is not stopped or a thread cannot be attached to. A thread that
/// does not stop again within a few seconds of the work's end has its
/// process killed (with SIGKILL), and the call fails.
pub fn with_threads_running<R: Send>(
    init: u32,
    threads: &[(u32, u32)],
    work: impl FnOnce() -> R + Send,
) -> Result<R, String> {
    // A tracer thread of our own: when it ends, the kernel detaches
    // whatever it still traces.
    thread::scope(|s| {
        s.spawn(|| trace(init, threads, work))
            .join()
            .unwrap_or_else(|_| Err("the tracer thread panicked".to_owned()))
    })
}

fn trace<R: Send>(
    init: u32,
    threads: &[(u32, u32)],
    work: impl FnOnce() -> R + Send,
) -> Result<R, String> {
    // Before attaching to anything (a traced thread is in state `t`).
    for &(pid, _) in threads {
        wait_group_stopped(pid)?;
    }
    let mut tracees = Vec::with_capacity(threads.len());
    for &(pid, tid) in threads {
        tracees.push(Tracee::attach(init, pid, tid)?);
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
    let until = Instant::now() + STOP_GRACE;
    let mut detached = Ok(());
    let mut stuck = Vec::new();
    for t in &mut tracees {
        if let Err((e, timed_out)) = t.detach(until) {
            if timed_out {
                stuck.push(t.pid);
            }
            if detached.is_ok() {
                detached = Err(e);
            }
        }
    }
    if !stuck.is_empty() {
        // Running job code would be worse than losing the job (whose image
        // is kept): kill the processes whose threads are out of control.
        for t in &tracees {
            let _ = kill(t.pid, Signal::SIGKILL);
        }
        let until = Instant::now() + STOP_GRACE;
        for t in &mut tracees {
            while !t.gone && Instant::now() < until {
                if !matches!(t.poll(), Ok(true)) {
                    thread::sleep(POLL);
                }
            }
        }
    } else {
        for t in &tracees {
            for &sig in &t.held {
                let _ = kill(t.pid, sig);
            }
        }
    }
    let out = result?;
    detached.map(|()| out)
}

#[cfg(test)]
mod tests {
    use std::process::{Child, Command};

    use super::*;

    fn states(pid: u32) -> Vec<char> {
        let mut out = Vec::new();
        for entry in fs::read_dir(format!("/proc/{pid}/task")).unwrap() {
            let stat = fs::read_to_string(entry.unwrap().path().join("stat")).unwrap();
            out.push(puzuz_checkpoint::procfs::parse_stat_state(&stat).unwrap());
        }
        out
    }

    fn tracer_of(pid: u32) -> String {
        fs::read_to_string(format!("/proc/{pid}/status"))
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("TracerPid:"))
            .unwrap()
            .trim()
            .to_owned()
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

    impl Killed {
        fn pid(&self) -> u32 {
            self.0.id()
        }
    }

    /// A shell loop of builtins only (no child whose SIGCHLD it would wait
    /// for) that appends to `file` and traps USR1 into `marker`.
    fn looping(file: &Path, marker: &Path) -> Killed {
        let script = format!(
            "trap 'echo x >> {m}' USR1; i=0; while :; do i=$((i + 1)); \
             if [ $((i % 500)) -eq 0 ]; then echo x >> {f}; fi; done",
            f = file.display(),
            m = marker.display()
        );
        let child = Killed(Command::new("sh").arg("-c").arg(&script).spawn().unwrap());
        wait_for("the loop starts", || file.exists());
        child
    }

    fn stop(child: &Killed) {
        kill(Pid::from_raw(child.pid() as i32), Signal::SIGSTOP).unwrap();
        wait_for("the loop stops", || {
            states(child.pid()).iter().all(|&s| s == 'T')
        });
    }

    /// Whether ptrace works here at all (seccomp or Yama may refuse it).
    fn ptrace_allowed(e: &str) -> bool {
        if e.contains("EPERM") {
            eprintln!("skipping: ptrace is not permitted here ({e})");
            return false;
        }
        true
    }

    /// A stopped single-threaded shell loop is released: it runs (its
    /// counter file grows) while the work runs, and is stopped again
    /// afterwards; SIGCONT then resumes it. A signal sent while it was
    /// released is held back until it is continued. Skipped where ptrace
    /// is not permitted.
    #[test]
    fn system_released_thread_runs_and_stops_again() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("count");
        let marker = dir.path().join("usr1");
        let child = looping(&file, &marker);
        let pid = child.pid();
        let raw = Pid::from_raw(pid as i32);
        stop(&child);
        let before = fs::read_to_string(&file).unwrap().len();

        let grew = match with_threads_running(pid, &[(pid, pid)], || {
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
            Err(e) if !ptrace_allowed(&e) => return,
            Err(e) => panic!("{e}"),
        };
        assert!(grew, "the released thread did not run");
        assert!(!marker.exists(), "the USR1 handler ran while released");

        // Stopped again, not traced, and it stays so.
        wait_for("stopped again", || states(pid).iter().all(|&s| s == 'T'));
        assert_eq!(tracer_of(pid), "0");
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

    /// When a later thread cannot be attached to (here: the thread id of
    /// another process), the call fails at once without running the work,
    /// and the thread attached to before is detached, still stopped.
    #[test]
    fn system_a_failed_attach_detaches_the_others_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let a = looping(&dir.path().join("a"), &dir.path().join("ma"));
        let b = looping(&dir.path().join("b"), &dir.path().join("mb"));
        stop(&a);
        stop(&b);
        let logged = (
            fs::read_to_string(dir.path().join("a")).unwrap(),
            fs::read_to_string(dir.path().join("b")).unwrap(),
        );
        let start = Instant::now();
        let res = with_threads_running(
            a.pid(),
            &[(a.pid(), a.pid()), (a.pid(), b.pid())],
            || -> u8 { panic!("the work must not run") },
        );
        let Err(e) = res else { panic!("accepted") };
        if !ptrace_allowed(&e) {
            return;
        }
        assert!(e.contains("is not a thread of process"), "{e}");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
        for p in [a.pid(), b.pid()] {
            assert_eq!(tracer_of(p), "0", "process {p} still traced");
            // A detached thread passes through the kernel back into the
            // group stop (briefly not `T`) without returning to its code.
            wait_for("stopped again", || states(p).iter().all(|&s| s == 'T'));
        }
        thread::sleep(Duration::from_millis(200));
        assert_eq!(fs::read_to_string(dir.path().join("a")).unwrap(), logged.0);
        assert_eq!(fs::read_to_string(dir.path().join("b")).unwrap(), logged.1);
    }

    /// A process that is not stopped is refused, without attaching to it.
    #[test]
    fn system_a_running_process_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let a = looping(&dir.path().join("a"), &dir.path().join("ma"));
        let res = with_threads_running(a.pid(), &[(a.pid(), a.pid())], || -> u8 {
            panic!("the work must not run")
        });
        let Err(e) = res else { panic!("accepted") };
        assert!(e.contains("is not stopped"), "{e}");
        assert_eq!(tracer_of(a.pid()), "0");
    }
}
