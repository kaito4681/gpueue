//! Running plans: the [`Executor`] trait, plan runners with rollback and an
//! operation [`Deadline`], and a [`SystemExecutor`] that runs real processes
//! with outer timeouts.
//!
//! When a step runs out of time, its whole process group and every
//! descendant are killed (`criu` forks helpers). What that leaves behind is
//! undone by the rollback ([`plan::rollback_plan`]): an interrupted
//! `criu dump` is followed by a [`Step::Thaw`] of the job, and CUDA
//! processes that were locked or checkpointed are restored and unlocked.

use std::io::{self, Read};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::command::{CommandSpec, DeviceMap, RestoreTarget};
use crate::plan::{self, Action, CheckpointConfig, PlanError, Step};
use crate::probe::{CudaError, classify_cuda_error};
use crate::procfs::{self, Unrunnable};

/// Captured output of a successful step.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StepOutput {
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, thiserror::Error)]
pub enum StepError {
    #[error("failed to spawn {program}: {source}")]
    Spawn {
        program: String,
        #[source]
        source: io::Error,
    },
    #[error("timed out after {after:?} and was killed")]
    TimedOut { after: Duration, output: StepOutput },
    /// The operation's [`Deadline`] passed while the step ran (`ran`: it
    /// was killed) or before it could start (it did not run).
    #[error(
        "{} the operation's deadline of {}s",
        if *.ran { "was killed at" } else { "was not started: past" },
        .budget.as_secs()
    )]
    DeadlineExceeded {
        budget: Duration,
        ran: bool,
        output: StepOutput,
    },
    /// Killed by a signal it did not get from us (e.g. an OOM kill or an
    /// administrator's `kill -9`).
    #[error("was killed by signal {signal}{}", stderr_suffix(.output))]
    Signaled { signal: i32, output: StepOutput },
    #[error("exited unsuccessfully (code {code:?}): {}", .output.stderr.trim())]
    Failed {
        code: Option<i32>,
        output: StepOutput,
    },
    #[error("process {pid} cannot be touched: {reason:?}")]
    Unrunnable { pid: u32, reason: Unrunnable },
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// `": <stderr>"`, or nothing when the tool printed nothing.
fn stderr_suffix(output: &StepOutput) -> String {
    match output.stderr.trim() {
        "" => String::new(),
        e => format!(": {e}"),
    }
}

impl StepError {
    /// Classify a failed `cuda-checkpoint` call by its output.
    pub fn cuda_error(&self) -> Option<CudaError> {
        match self {
            StepError::Failed { output, .. } => Some(classify_cuda_error(&format!(
                "{}\n{}",
                output.stderr, output.stdout
            ))),
            _ => None,
        }
    }

    /// The step was killed (for running out of time, or by a signal from
    /// elsewhere), so it may have stopped half-way.
    pub fn is_killed(&self) -> bool {
        matches!(
            self,
            StepError::TimedOut { .. }
                | StepError::Signaled { .. }
                | StepError::DeadlineExceeded { ran: true, .. }
        )
    }
}

/// When a whole checkpoint or restore must be finished by. Each step's
/// timeout is shortened to the time left; rollback steps are not bound by
/// it (they have their own timeouts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadline {
    pub at: Instant,
    /// The total time the operation was given, for messages.
    pub budget: Duration,
}

impl Deadline {
    pub fn after(budget: Duration) -> Self {
        let now = Instant::now();
        Self {
            at: now
                .checked_add(budget)
                .unwrap_or_else(|| now + plan::UNBOUNDED),
            budget,
        }
    }

    pub fn remaining(&self) -> Duration {
        self.at.saturating_duration_since(Instant::now())
    }
}

/// Runs single steps. The helper uses [`SystemExecutor`]; tests use fakes.
pub trait Executor {
    fn execute(&mut self, step: &Step) -> Result<StepOutput, StepError>;
}

/// A step together with its result.
#[derive(Debug)]
pub struct StepRecord {
    pub step: Step,
    pub result: Result<StepOutput, StepError>,
}

/// A plan stopped at `failed`.
#[derive(Debug)]
pub struct PlanFailure {
    /// Steps that succeeded before the failure.
    pub completed: Vec<StepRecord>,
    pub failed: Step,
    pub error: StepError,
    /// Rollback steps that were attempted (best effort), in order.
    pub rollback: Vec<StepRecord>,
}

impl PlanFailure {
    /// True when every attempted rollback step succeeded and the rollback was
    /// not cut short.
    pub fn rolled_back_cleanly(&self) -> bool {
        self.rollback.iter().all(|r| r.result.is_ok())
    }
}

impl std::fmt::Display for PlanFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.failed {
            Step::EnsureRunnable { .. } => write!(f, "precheck failed: {}", self.error),
            Step::Thaw { root_pid } => write!(f, "resuming {root_pid} failed: {}", self.error),
            Step::Run { command, .. } => write!(f, "`{command}` failed: {}", self.error),
        }
    }
}

impl std::error::Error for PlanFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// Run `steps` in order, stopping at the first failure. No rollback.
pub fn run_plan<E: Executor + ?Sized>(
    exec: &mut E,
    steps: Vec<Step>,
) -> Result<Vec<StepRecord>, Box<PlanFailure>> {
    run_plan_until(exec, steps, None)
}

/// [`run_plan`] with an operation deadline: each command's timeout is cut
/// to the time left, and a command that would start after the deadline
/// fails with [`StepError::DeadlineExceeded`] without running. Checks
/// without a command ([`Step::EnsureRunnable`], [`Step::Thaw`]) always run.
pub fn run_plan_until<E: Executor + ?Sized>(
    exec: &mut E,
    steps: Vec<Step>,
    deadline: Option<&Deadline>,
) -> Result<Vec<StepRecord>, Box<PlanFailure>> {
    let mut completed = Vec::with_capacity(steps.len());
    for step in steps {
        let result = match (&step, deadline) {
            (
                Step::Run {
                    action,
                    command,
                    timeout,
                },
                Some(d),
            ) => {
                let left = d.remaining();
                if left.is_zero() {
                    Err(StepError::DeadlineExceeded {
                        budget: d.budget,
                        ran: false,
                        output: StepOutput::default(),
                    })
                } else if left < *timeout {
                    let cut = Step::Run {
                        action: *action,
                        command: command.clone(),
                        timeout: left,
                    };
                    exec.execute(&cut).map_err(|e| match e {
                        StepError::TimedOut { output, .. } => StepError::DeadlineExceeded {
                            budget: d.budget,
                            ran: true,
                            output,
                        },
                        e => e,
                    })
                } else {
                    exec.execute(&step)
                }
            }
            _ => exec.execute(&step),
        };
        match result {
            Ok(out) => completed.push(StepRecord {
                step,
                result: Ok(out),
            }),
            Err(error) => {
                return Err(Box::new(PlanFailure {
                    completed,
                    failed: step,
                    error,
                    rollback: Vec::new(),
                }));
            }
        }
    }
    Ok(completed)
}

/// Run `steps` (within `deadline`, if any); on failure, undo what was done
/// so far with [`plan::rollback_plan`]. The rollback is not bound by the
/// deadline and is best effort: it continues past failing steps, except
/// that it stops if a [`Step::Thaw`] or the runnable precheck fails (a
/// CUDA call on a frozen process would hang; a tree that cannot be
/// thawed is gone or stuck).
pub fn run_with_rollback<E: Executor + ?Sized>(
    exec: &mut E,
    cfg: &CheckpointConfig,
    steps: Vec<Step>,
    deadline: Option<&Deadline>,
) -> Result<Vec<StepRecord>, Box<PlanFailure>> {
    let mut failure = match run_plan_until(exec, steps, deadline) {
        Ok(done) => return Ok(done),
        Err(f) => f,
    };
    let done: Vec<Action> = failure
        .completed
        .iter()
        .filter_map(|r| r.step.action())
        .collect();
    let killed = failure.error.is_killed();
    for step in plan::rollback_plan(cfg, &done, failure.failed.action().as_ref(), killed) {
        let result = exec.execute(&step);
        let stop =
            matches!(step, Step::EnsureRunnable { .. } | Step::Thaw { .. }) && result.is_err();
        failure.rollback.push(StepRecord { step, result });
        if stop {
            break;
        }
    }
    Err(failure)
}

/// Why a restore failed.
#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error("criu restore failed: {0}")]
    Criu(Box<PlanFailure>),
    #[error("could not find restored CUDA processes: {0}")]
    ResolvePids(String),
    /// The tree is running but CUDA could not be resumed; the job should be
    /// killed.
    #[error("CUDA restore failed: {0}")]
    Cuda(Box<PlanFailure>),
}

/// Full restore: `criu restore`, then (manual mode) map the saved CUDA PIDs
/// to their new host PIDs via `resolve_pids`, then restore + unlock them.
/// `resolve_pids` is not called in plugin mode.
pub fn run_restore<E, F>(
    exec: &mut E,
    cfg: &CheckpointConfig,
    target: &RestoreTarget,
    device_map: Option<&DeviceMap>,
    resolve_pids: F,
    deadline: Option<&Deadline>,
) -> Result<Vec<StepRecord>, RestoreError>
where
    E: Executor + ?Sized,
    F: FnOnce() -> Result<Vec<u32>, String>,
{
    let criu = plan::restore_criu_plan(cfg, target)?;
    let mut records = run_plan_until(exec, criu, deadline).map_err(RestoreError::Criu)?;
    if cfg.mode == crate::command::CudaMode::Manual {
        let pids = resolve_pids().map_err(RestoreError::ResolvePids)?;
        let cuda = plan::restore_cuda_plan(cfg, &pids, device_map);
        records.extend(run_plan_until(exec, cuda, deadline).map_err(RestoreError::Cuda)?);
    }
    Ok(records)
}

/// Runs steps as real child processes.
#[derive(Debug, Clone)]
pub struct SystemExecutor {
    pub proc_root: PathBuf,
    pub cgroup_root: PathBuf,
    /// How often a running child is polled for exit.
    pub poll_interval: Duration,
}

impl Default for SystemExecutor {
    fn default() -> Self {
        Self {
            proc_root: PathBuf::from("/proc"),
            cgroup_root: PathBuf::from("/sys/fs/cgroup"),
            poll_interval: Duration::from_millis(20),
        }
    }
}

impl Executor for SystemExecutor {
    fn execute(&mut self, step: &Step) -> Result<StepOutput, StepError> {
        match step {
            Step::EnsureRunnable { pids } => {
                for &pid in pids {
                    if let Some(reason) =
                        procfs::check_runnable(&self.proc_root, &self.cgroup_root, pid)?
                    {
                        return Err(StepError::Unrunnable { pid, reason });
                    }
                }
                Ok(StepOutput::default())
            }
            Step::Thaw { root_pid } => self.thaw(*root_pid),
            Step::Run {
                command, timeout, ..
            } => run_with_timeout(command, *timeout, self.poll_interval),
        }
    }
}

impl SystemExecutor {
    /// [`Step::Thaw`]: SIGCONT the stopped tasks and thaw the frozen
    /// cgroups of the tree, then wait (briefly) until none is stopped.
    fn thaw(&self, root: u32) -> Result<StepOutput, StepError> {
        let plan = procfs::plan_thaw(&self.proc_root, &self.cgroup_root, root)?;
        let mut done = Vec::new();
        for dir in &plan.frozen {
            std::fs::write(dir.join("cgroup.freeze"), "0")?;
            done.push(format!("thawed {}", dir.display()));
        }
        for &pid in &plan.stopped {
            signal(pid, rustix::process::Signal::CONT);
            done.push(format!("SIGCONT {pid}"));
        }
        // Signals and thawing take effect asynchronously.
        let until = Instant::now() + THAW_SETTLE_TIMEOUT;
        loop {
            let left = procfs::plan_thaw(&self.proc_root, &self.cgroup_root, root)?;
            if left.is_empty() {
                break;
            }
            if Instant::now() >= until {
                return Err(StepError::Unrunnable {
                    pid: left.stopped.first().copied().unwrap_or(root),
                    reason: match left.frozen.first() {
                        Some(dir) => Unrunnable::CgroupFrozen(dir.clone()),
                        None => Unrunnable::Stopped('T'),
                    },
                });
            }
            thread::sleep(self.poll_interval);
        }
        Ok(StepOutput {
            stdout: done.join("\n"),
            stderr: String::new(),
        })
    }
}

/// How long [`Step::Thaw`] waits for the tree to run again.
const THAW_SETTLE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to wait for output pipes to close after the child exited (a
/// grandchild may keep them open).
const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Run `spec` with stdin from `/dev/null`, capturing stdout/stderr, and kill
/// it if it is still running after `timeout`. The child runs in its own
/// process group; on timeout the group and every descendant of the child
/// (also those that left the group) are killed with SIGKILL.
pub fn run_with_timeout(
    spec: &CommandSpec,
    timeout: Duration,
    poll_interval: Duration,
) -> Result<StepOutput, StepError> {
    let mut child = spec
        .to_command()
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| StepError::Spawn {
            program: spec.program.display().to_string(),
            source,
        })?;
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());

    let start = Instant::now();
    let deadline = start
        .checked_add(timeout)
        .unwrap_or_else(|| start + plan::UNBOUNDED);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            kill_tree(&mut child)?;
            break None;
        }
        thread::sleep(poll_interval.min(deadline.saturating_duration_since(Instant::now())));
    };
    let drain_deadline = Instant::now() + PIPE_DRAIN_TIMEOUT;
    let output = StepOutput {
        stdout: collect(stdout, drain_deadline),
        stderr: collect(stderr, drain_deadline),
    };
    match status {
        None => Err(StepError::TimedOut {
            after: timeout,
            output,
        }),
        Some(s) if s.success() => Ok(output),
        Some(s) if s.signal().is_some() => Err(StepError::Signaled {
            signal: s.signal().unwrap_or_default(),
            output,
        }),
        Some(s) => Err(StepError::Failed {
            code: s.code(),
            output,
        }),
    }
}

fn signal(pid: u32, sig: rustix::process::Signal) {
    if let Some(pid) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    {
        // Fails only if the process is gone.
        let _ = rustix::process::kill_process(pid, sig);
    }
}

/// Kill `child`, its process group and all its descendants. The tree is
/// stopped first so that nothing forks or reaps while it is collected, and
/// the descendants are collected before the child dies (afterwards they
/// would be reparented and could no longer be found).
fn kill_tree(child: &mut Child) -> io::Result<()> {
    let proc_root = std::path::Path::new("/proc");
    let pid = child.id();
    let mut victims = procfs::descendants(proc_root, pid).unwrap_or_default();
    for &p in std::iter::once(&pid).chain(&victims) {
        signal(p, rustix::process::Signal::STOP);
    }
    for p in procfs::descendants(proc_root, pid).unwrap_or_default() {
        if !victims.contains(&p) {
            victims.push(p);
        }
    }
    if let Some(pgid) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    {
        let _ = rustix::process::kill_process_group(pgid, rustix::process::Signal::KILL);
    }
    for &p in &victims {
        signal(p, rustix::process::Signal::KILL);
    }
    match child.kill() {
        // Already exited between try_wait and kill.
        Err(e) if e.kind() == io::ErrorKind::InvalidInput => {}
        r => r?,
    }
    child.wait().map(drop)
}

fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    if let Some(mut pipe) = pipe {
        thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            let _ = tx.send(buf);
        });
    }
    rx
}

fn collect(rx: mpsc::Receiver<Vec<u8>>, deadline: Instant) -> String {
    rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::CudaMode;
    use crate::plan::tests::{cfg, dump_target, restore_target};
    use std::collections::HashMap;
    use std::path::Path;

    /// Records every step and fails the ones it was told to.
    #[derive(Default)]
    struct FakeExecutor {
        log: Vec<Step>,
        fail: HashMap<Option<Action>, String>,
        fail_runnable: bool,
    }

    impl FakeExecutor {
        fn failing(action: Action, stderr: &str) -> Self {
            let mut f = Self::default();
            f.fail.insert(Some(action), stderr.to_owned());
            f
        }
        fn actions(&self) -> Vec<Option<Action>> {
            self.log.iter().map(Step::action).collect()
        }
    }

    impl Executor for FakeExecutor {
        fn execute(&mut self, step: &Step) -> Result<StepOutput, StepError> {
            self.log.push(step.clone());
            if let Step::EnsureRunnable { pids } = step
                && self.fail_runnable
            {
                return Err(StepError::Unrunnable {
                    pid: pids[0],
                    reason: Unrunnable::Stopped('T'),
                });
            }
            match self.fail.get(&step.action()) {
                Some(stderr) => Err(StepError::Failed {
                    code: Some(1),
                    output: StepOutput {
                        stdout: String::new(),
                        stderr: stderr.clone(),
                    },
                }),
                None => Ok(StepOutput::default()),
            }
        }
    }

    use Action::*;

    #[test]
    fn successful_manual_dump_runs_everything_in_order() {
        let c = cfg(CudaMode::Manual);
        let mut ex = FakeExecutor::default();
        let done = run_with_rollback(
            &mut ex,
            &c,
            plan::dump_plan(&c, &dump_target(), &[101, 102]),
            None,
        )
        .unwrap();
        assert_eq!(done.len(), 6);
        assert_eq!(
            ex.actions(),
            [
                None,
                Some(CudaLock { pid: 101 }),
                Some(CudaLock { pid: 102 }),
                Some(CudaCheckpoint { pid: 101 }),
                Some(CudaCheckpoint { pid: 102 }),
                Some(CriuDump { root_pid: 100 }),
            ]
        );
    }

    #[test]
    fn criu_dump_failure_restores_and_unlocks() {
        let c = cfg(CudaMode::Manual);
        let mut ex = FakeExecutor::failing(CriuDump { root_pid: 100 }, "Dumping FAILED");
        let f = run_with_rollback(
            &mut ex,
            &c,
            plan::dump_plan(&c, &dump_target(), &[101, 102]),
            None,
        )
        .unwrap_err();
        assert_eq!(f.failed.action(), Some(CriuDump { root_pid: 100 }));
        assert_eq!(f.completed.len(), 5);
        assert!(f.rolled_back_cleanly());
        assert_eq!(f.rollback[0].step, Step::Thaw { root_pid: 100 });
        assert_eq!(
            ex.actions()[6..],
            [
                None,
                None,
                Some(CudaRestore { pid: 101 }),
                Some(CudaRestore { pid: 102 }),
                Some(CudaUnlock { pid: 101 }),
                Some(CudaUnlock { pid: 102 }),
            ]
        );
        assert!(f.to_string().contains("criu dump"));
    }

    #[test]
    fn lock_timeout_unlocks_previous_and_classifies_error() {
        let c = cfg(CudaMode::Manual);
        let mut ex = FakeExecutor::failing(
            CudaLock { pid: 102 },
            "Could not lock: \"device not ready\"",
        );
        let f = run_with_rollback(
            &mut ex,
            &c,
            plan::dump_plan(&c, &dump_target(), &[101, 102]),
            None,
        )
        .unwrap_err();
        assert_eq!(f.error.cuda_error(), Some(CudaError::LockTimedOut));
        assert_eq!(
            ex.actions(),
            [
                None,
                Some(CudaLock { pid: 101 }),
                Some(CudaLock { pid: 102 }),
                None,
                Some(CudaUnlock { pid: 101 }),
            ]
        );
        assert!(!ex.actions().contains(&Some(CriuDump { root_pid: 100 })));
    }

    #[test]
    fn rollback_continues_past_failing_step() {
        let c = cfg(CudaMode::Manual);
        let mut ex = FakeExecutor::failing(CriuDump { root_pid: 100 }, "x");
        ex.fail
            .insert(Some(CudaRestore { pid: 101 }), "\"out of memory\"".into());
        let f = run_with_rollback(
            &mut ex,
            &c,
            plan::dump_plan(&c, &dump_target(), &[101, 102]),
            None,
        )
        .unwrap_err();
        assert!(!f.rolled_back_cleanly());
        assert_eq!(f.rollback.len(), 6);
        assert_eq!(
            f.rollback[2].result.as_ref().unwrap_err().cuda_error(),
            Some(CudaError::OutOfMemory)
        );
        assert!(f.rollback[3..].iter().all(|r| r.result.is_ok()));
    }

    #[test]
    fn frozen_job_is_never_touched() {
        let c = cfg(CudaMode::Manual);
        let mut ex = FakeExecutor {
            fail_runnable: true,
            ..Default::default()
        };
        let f = run_with_rollback(
            &mut ex,
            &c,
            plan::dump_plan(&c, &dump_target(), &[101]),
            None,
        )
        .unwrap_err();
        assert!(matches!(f.error, StepError::Unrunnable { pid: 100, .. }));
        assert_eq!(ex.log.len(), 1);
        assert!(f.rollback.is_empty());
        assert!(f.to_string().starts_with("precheck failed"));
    }

    #[test]
    fn rollback_stops_when_job_became_frozen() {
        let c = cfg(CudaMode::Manual);
        struct FreezeAfterDump(FakeExecutor);
        impl Executor for FreezeAfterDump {
            fn execute(&mut self, step: &Step) -> Result<StepOutput, StepError> {
                let dumped = self
                    .0
                    .log
                    .iter()
                    .any(|s| matches!(s.action(), Some(CriuDump { .. })));
                self.0.fail_runnable = dumped;
                self.0.execute(step)
            }
        }
        let mut ex = FreezeAfterDump(FakeExecutor::failing(CriuDump { root_pid: 100 }, "x"));
        let f = run_with_rollback(
            &mut ex,
            &c,
            plan::dump_plan(&c, &dump_target(), &[101]),
            None,
        )
        .unwrap_err();
        // Thawed, then the runnable check failed: nothing more is tried.
        assert_eq!(f.rollback.len(), 2);
        assert!(!f.rolled_back_cleanly());
    }

    #[test]
    fn plugin_dump_failure_has_no_cuda_rollback() {
        let c = cfg(CudaMode::Plugin);
        let mut ex = FakeExecutor::failing(CriuDump { root_pid: 100 }, "x");
        let f = run_with_rollback(
            &mut ex,
            &c,
            plan::dump_plan(&c, &dump_target(), &[101]),
            None,
        )
        .unwrap_err();
        assert_eq!(f.rollback.len(), 1, "only the thaw");
        assert_eq!(f.rollback[0].step, Step::Thaw { root_pid: 100 });
        assert!(f.rolled_back_cleanly());
    }

    #[test]
    fn manual_restore_resolves_pids_then_resumes() {
        let c = cfg(CudaMode::Manual);
        let mut ex = FakeExecutor::default();
        let recs = run_restore(
            &mut ex,
            &c,
            &restore_target(),
            None,
            || Ok(vec![301, 302]),
            None,
        )
        .unwrap();
        assert_eq!(recs.len(), 6);
        assert_eq!(
            ex.actions(),
            [
                Some(CriuRestore),
                None,
                Some(CudaRestore { pid: 301 }),
                Some(CudaRestore { pid: 302 }),
                Some(CudaUnlock { pid: 301 }),
                Some(CudaUnlock { pid: 302 }),
            ]
        );
    }

    #[test]
    fn plugin_restore_does_not_resolve() {
        let c = cfg(CudaMode::Plugin);
        let mut ex = FakeExecutor::default();
        run_restore(
            &mut ex,
            &c,
            &restore_target(),
            None,
            || panic!("must not be called"),
            None,
        )
        .unwrap();
        assert_eq!(ex.actions(), [Some(CriuRestore)]);
    }

    #[test]
    fn restore_failures() {
        let c = cfg(CudaMode::Manual);
        let mut ex = FakeExecutor::failing(CriuRestore, "BUG");
        assert!(matches!(
            run_restore(&mut ex, &c, &restore_target(), None, || Ok(vec![1]), None),
            Err(RestoreError::Criu(_))
        ));

        let mut ex = FakeExecutor::default();
        assert!(matches!(
            run_restore(&mut ex, &c, &restore_target(), None, || Err("gone".into()), None),
            Err(RestoreError::ResolvePids(m)) if m == "gone"
        ));

        let mut ex = FakeExecutor::failing(CudaRestore { pid: 1 }, "\"out of memory\"");
        match run_restore(&mut ex, &c, &restore_target(), None, || Ok(vec![1]), None) {
            Err(RestoreError::Cuda(f)) => {
                assert_eq!(f.error.cuda_error(), Some(CudaError::OutOfMemory));
                assert!(f.rollback.is_empty());
            }
            other => panic!("unexpected {other:?}"),
        }

        let mut t = restore_target();
        t.detached = false;
        assert!(matches!(
            run_restore(
                &mut FakeExecutor::default(),
                &c,
                &t,
                None,
                || Ok(vec![]),
                None
            ),
            Err(RestoreError::Plan(PlanError::ManualRequiresDetached))
        ));
    }

    /// Records the timeout each command was run with; fails the
    /// configured action as if it had hit that timeout.
    #[derive(Default)]
    struct TimingExecutor {
        log: Vec<(Option<Action>, Option<Duration>)>,
        time_out: Option<Action>,
    }

    impl Executor for TimingExecutor {
        fn execute(&mut self, step: &Step) -> Result<StepOutput, StepError> {
            let timeout = match step {
                Step::Run { timeout, .. } => Some(*timeout),
                _ => None,
            };
            self.log.push((step.action(), timeout));
            if step.action().is_some() && step.action() == self.time_out {
                return Err(StepError::TimedOut {
                    after: timeout.unwrap(),
                    output: StepOutput::default(),
                });
            }
            Ok(StepOutput::default())
        }
    }

    #[test]
    fn deadline_cuts_step_timeouts() {
        let c = cfg(CudaMode::Manual);
        let mut ex = TimingExecutor::default();
        let d = Deadline::after(Duration::from_secs(100));
        run_plan_until(
            &mut ex,
            plan::dump_plan(&c, &dump_target(), &[101]),
            Some(&d),
        )
        .unwrap();
        // The lock keeps its 45 s; the dump (no cap) gets what is left.
        assert_eq!(
            ex.log[1],
            (Some(CudaLock { pid: 101 }), Some(Duration::from_secs(45)))
        );
        let (action, Some(t)) = ex.log[3] else {
            panic!()
        };
        assert_eq!(action, Some(CriuDump { root_pid: 100 }));
        assert!(
            t <= Duration::from_secs(100) && t > Duration::from_secs(90),
            "{t:?}"
        );
        // Without a deadline the dump is unbounded.
        let mut ex = TimingExecutor::default();
        run_plan(&mut ex, plan::dump_plan(&c, &dump_target(), &[101])).unwrap();
        assert_eq!(ex.log[3].1, Some(plan::UNBOUNDED));
    }

    #[test]
    fn dump_killed_at_the_deadline_is_thawed_and_rolled_back() {
        let c = cfg(CudaMode::Manual);
        let mut ex = TimingExecutor {
            time_out: Some(CriuDump { root_pid: 100 }),
            ..Default::default()
        };
        let d = Deadline::after(Duration::from_secs(100));
        let f = run_with_rollback(
            &mut ex,
            &c,
            plan::dump_plan(&c, &dump_target(), &[101, 102]),
            Some(&d),
        )
        .unwrap_err();
        assert!(
            matches!(f.error, StepError::DeadlineExceeded { budget, .. } if budget == Duration::from_secs(100)),
            "{:?}",
            f.error
        );
        assert!(f.error.is_killed());
        assert!(
            f.to_string()
                .contains("was killed at the operation's deadline of 100s"),
            "{f}"
        );
        let rb: Vec<Step> = f.rollback.iter().map(|r| r.step.clone()).collect();
        assert_eq!(rb[0], Step::Thaw { root_pid: 100 });
        assert_eq!(
            rb[1],
            Step::EnsureRunnable {
                pids: vec![101, 102]
            }
        );
        assert_eq!(
            rb[2..].iter().map(Step::action).collect::<Vec<_>>(),
            [
                Some(CudaRestore { pid: 101 }),
                Some(CudaRestore { pid: 102 }),
                Some(CudaUnlock { pid: 101 }),
                Some(CudaUnlock { pid: 102 }),
            ]
        );
        // Rollback steps keep their own timeouts.
        assert!(
            ex.log[6..]
                .iter()
                .filter_map(|(_, t)| *t)
                .all(|t| t == Duration::from_secs(300) || t == Duration::from_secs(60))
        );
    }

    #[test]
    fn killed_checkpoint_is_restored_before_unlocking() {
        let c = cfg(CudaMode::Manual);
        let mut ex = TimingExecutor {
            time_out: Some(CudaCheckpoint { pid: 101 }),
            ..Default::default()
        };
        let f = run_with_rollback(
            &mut ex,
            &c,
            plan::dump_plan(&c, &dump_target(), &[101]),
            None,
        )
        .unwrap_err();
        assert!(matches!(f.error, StepError::TimedOut { .. }));
        assert_eq!(
            f.rollback
                .iter()
                .map(|r| r.step.action())
                .collect::<Vec<_>>(),
            [
                None,
                Some(CudaRestore { pid: 101 }),
                Some(CudaUnlock { pid: 101 })
            ]
        );
    }

    #[test]
    fn expired_deadline_runs_nothing_more() {
        let c = cfg(CudaMode::Manual);
        let mut ex = TimingExecutor::default();
        let d = Deadline {
            at: Instant::now(),
            budget: Duration::from_secs(7),
        };
        let f = run_with_rollback(
            &mut ex,
            &c,
            plan::dump_plan(&c, &dump_target(), &[101]),
            Some(&d),
        )
        .unwrap_err();
        // The runnable check ran; the first lock did not.
        assert_eq!(f.failed.action(), Some(CudaLock { pid: 101 }));
        assert!(matches!(
            f.error,
            StepError::DeadlineExceeded { ran: false, .. }
        ));
        assert!(!f.error.is_killed());
        assert!(f.to_string().contains("was not started"), "{f}");
        assert_eq!(ex.log.len(), 1, "{:?}", ex.log);
        // Nothing was locked, so there is nothing to undo.
        assert!(f.rollback.is_empty());
    }

    #[test]
    fn restore_deadline_applies_to_the_cuda_steps() {
        let c = cfg(CudaMode::Manual);
        let mut ex = TimingExecutor {
            time_out: Some(CudaRestore { pid: 301 }),
            ..Default::default()
        };
        let d = Deadline::after(Duration::from_secs(50));
        match run_restore(
            &mut ex,
            &c,
            &restore_target(),
            None,
            || Ok(vec![301]),
            Some(&d),
        ) {
            Err(RestoreError::Cuda(f)) => {
                assert!(matches!(f.error, StepError::DeadlineExceeded { .. }))
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    fn sh(script: &str) -> CommandSpec {
        let mut c = CommandSpec::new("sh");
        c.arg("-c").arg(script);
        c
    }

    const POLL: Duration = Duration::from_millis(5);

    #[test]
    fn system_run_success_captures_output() {
        let out =
            run_with_timeout(&sh("echo out; echo err >&2"), Duration::from_secs(10), POLL).unwrap();
        assert_eq!(out.stdout, "out\n");
        assert_eq!(out.stderr, "err\n");
    }

    #[test]
    fn system_run_failure_has_code_and_stderr() {
        let e = run_with_timeout(
            &sh("echo '\"device not ready\"' >&2; exit 3"),
            Duration::from_secs(10),
            POLL,
        )
        .unwrap_err();
        match &e {
            StepError::Failed { code, output } => {
                assert_eq!(*code, Some(3));
                assert!(output.stderr.contains("device not ready"));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(e.cuda_error(), Some(CudaError::LockTimedOut));
    }

    #[test]
    fn system_run_timeout_kills() {
        let start = Instant::now();
        let e = run_with_timeout(
            &sh("echo started; exec sleep 30"),
            Duration::from_millis(200),
            POLL,
        )
        .unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(10));
        match e {
            StepError::TimedOut { after, output } => {
                assert_eq!(after, Duration::from_millis(200));
                assert_eq!(output.stdout, "started\n");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    fn alive(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| procfs::parse_stat_state(&s))
            .is_some_and(|st| st != 'Z' && st != 'X')
    }

    #[test]
    fn system_run_timeout_kills_the_whole_tree() {
        let tmp = crate::test_util::TempDir::new();
        let pids = tmp.path().join("pids");
        // A grandchild in the same group and one in its own session (as
        // a forked helper that called setsid would be).
        let script = format!(
            "sleep 60 & echo $! > {p}.a; setsid sleep 60 & echo $! > {p}.b; wait",
            p = pids.display()
        );
        let e = run_with_timeout(&sh(&script), Duration::from_millis(500), POLL).unwrap_err();
        assert!(matches!(e, StepError::TimedOut { .. }));
        for suffix in ["a", "b"] {
            let pid: u32 = std::fs::read_to_string(pids.with_extension(suffix))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let mut gone = false;
            for _ in 0..200 {
                if !alive(pid) {
                    gone = true;
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            assert!(gone, "grandchild {suffix} ({pid}) survived");
        }
    }

    #[test]
    fn system_thaw_resumes_a_stopped_tree() {
        let mut ex = SystemExecutor::default();
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 60 & wait"])
            .spawn()
            .unwrap();
        let pid = child.id();
        let mut kids = Vec::new();
        for _ in 0..200 {
            kids = procfs::descendants(Path::new("/proc"), pid).unwrap();
            if !kids.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        for p in std::iter::once(pid).chain(kids.iter().copied()) {
            signal(p, rustix::process::Signal::STOP);
        }
        let mut stopped = false;
        for _ in 0..200 {
            if ex
                .execute(&Step::EnsureRunnable { pids: vec![pid] })
                .is_err()
            {
                stopped = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(stopped);
        let out = ex.execute(&Step::Thaw { root_pid: pid }).unwrap();
        assert!(
            out.stdout.contains(&format!("SIGCONT {pid}")),
            "{}",
            out.stdout
        );
        let ok = ex.execute(&Step::EnsureRunnable {
            pids: std::iter::once(pid).chain(kids.iter().copied()).collect(),
        });
        for p in &kids {
            signal(*p, rustix::process::Signal::KILL);
        }
        let _ = child.kill();
        let _ = child.wait();
        assert!(ok.is_ok(), "{ok:?}");
        // A tree that is gone cannot be thawed.
        assert!(matches!(
            ex.execute(&Step::Thaw { root_pid: pid }),
            Err(StepError::Io(e)) if e.kind() == io::ErrorKind::NotFound
        ));
    }

    #[test]
    fn system_run_does_not_hang_on_inherited_pipes() {
        // A grandchild keeps stdout open after the child exits.
        let start = Instant::now();
        run_with_timeout(&sh("sleep 30 & echo done"), Duration::from_secs(10), POLL).unwrap();
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn system_run_killed_from_outside_is_signaled() {
        let e = run_with_timeout(&sh("kill -9 $$"), Duration::from_secs(10), POLL).unwrap_err();
        assert!(matches!(e, StepError::Signaled { signal: 9, .. }), "{e:?}");
        assert!(e.is_killed());
        assert_eq!(e.to_string(), "was killed by signal 9");
    }

    #[test]
    fn system_run_spawn_error() {
        let e = run_with_timeout(
            &CommandSpec::new("/nonexistent/puzuz-bin"),
            Duration::from_secs(1),
            POLL,
        )
        .unwrap_err();
        assert!(matches!(e, StepError::Spawn { .. }));
    }

    #[test]
    fn system_executor_detects_stopped_process() {
        let mut ex = SystemExecutor::default();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        let ok = ex.execute(&Step::EnsureRunnable {
            pids: vec![std::process::id(), pid],
        });
        assert!(ok.is_ok(), "{ok:?}");

        run_with_timeout(
            &sh(&format!("kill -STOP {pid}")),
            Duration::from_secs(5),
            POLL,
        )
        .unwrap();
        // The state change is asynchronous; poll briefly.
        let mut res = Ok(StepOutput::default());
        for _ in 0..200 {
            res = ex.execute(&Step::EnsureRunnable { pids: vec![pid] });
            if res.is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            matches!(res, Err(StepError::Unrunnable { pid: p, reason: Unrunnable::Stopped(_) }) if p == pid),
            "{res:?}"
        );
    }

    #[test]
    fn system_executor_runs_commands() {
        let mut ex = SystemExecutor::default();
        let step = Step::Run {
            action: CriuRestore,
            command: sh("echo hi"),
            timeout: Duration::from_secs(5),
        };
        assert_eq!(ex.execute(&step).unwrap().stdout, "hi\n");
        let ok = run_plan(&mut ex, vec![step.clone(), step]).unwrap();
        assert_eq!(ok.len(), 2);
    }
}
