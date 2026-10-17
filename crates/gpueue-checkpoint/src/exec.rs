//! Running plans: the [`Executor`] trait, plan runners with rollback and an
//! operation [`Deadline`], and a [`SystemExecutor`] that runs real processes
//! with outer timeouts.
//!
//! When a step runs out of time, its whole process group and every
//! descendant are killed (`criu` forks helpers). What that leaves behind is
//! undone by the rollback ([`plan::rollback_plan`]): an interrupted
//! `criu dump` is followed by a [`Step::Thaw`] of the job, and CUDA
//! processes that were locked or checkpointed are restored and unlocked.
//!
//! Killing `criu dump` cannot always be undone: if it is killed while its
//! parasite code runs inside the job's processes, the kernel detaches them
//! in the middle of that code, which can make them crash. A rollback that
//! succeeded therefore means that the job was thawed and its CUDA state
//! resumed, not that every process survived.

use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::command::{CommandSpec, DeviceMap, RestoreTarget};
use crate::plan::{self, Action, CheckpointConfig, PlanError, Step};
use crate::probe::{CudaError, CudaState, classify_cuda_error, parse_cuda_state};
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
    /// The step's process is no longer one of the job's (it exited, and
    /// its PID may have been reused): it was not touched (see
    /// [`Executor::confine_to`]).
    #[error("process {pid} is no longer a process of the job; not touched")]
    NotInJob { pid: u32 },
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

    /// The step was started (as opposed to refused before it ran: past the
    /// deadline, or not spawnable), so it may have changed something.
    pub fn ran(&self) -> bool {
        !matches!(
            self,
            StepError::Spawn { .. }
                | StepError::NotInJob { .. }
                | StepError::DeadlineExceeded { ran: false, .. }
        )
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

    /// Confines the steps that act on a process by its PID (CUDA calls,
    /// `criu dump --tree`, SIGCONT) to the processes in the PID namespace
    /// of the job whose namespace init is `init_pid`: the PIDs of a plan
    /// come from a scan of `/proc`, and a process of the job may exit and
    /// its PID be reused before a step runs. A step whose process is not
    /// (any longer) in that namespace is not run ([`StepError::NotInJob`]).
    fn confine_to(&mut self, _init_pid: u32) {}
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
    /// `criu dump` was killed (its deadline, or a signal from elsewhere).
    /// Its rollback cannot undo everything: if it was killed while its
    /// parasite code ran inside the job's processes, they may crash.
    pub fn criu_dump_killed(&self) -> bool {
        matches!(self.failed.action(), Some(Action::CriuDump { .. })) && self.error.is_killed()
    }

    /// True when every attempted rollback step succeeded (a rollback step
    /// of a CUDA process that is gone succeeds without doing anything).
    pub fn rolled_back_cleanly(&self) -> bool {
        self.rollback.iter().all(|r| r.result.is_ok())
    }
}

impl std::fmt::Display for PlanFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fn describe(
            f: &mut std::fmt::Formatter<'_>,
            step: &Step,
            error: &StepError,
        ) -> std::fmt::Result {
            match step {
                Step::EnsureRunnable { .. } => write!(f, "precheck failed: {error}"),
                Step::Thaw { root_pid } => write!(f, "resuming {root_pid} failed: {error}"),
                Step::Run { command, .. } => write!(f, "`{command}` failed: {error}"),
                Step::IfRunnable { then, .. } => describe(f, then, error),
                Step::IfCudaState { pid, then, .. } => match &**then {
                    Step::Run { command, .. } => write!(f, "`{command}` failed: {error}"),
                    _ => write!(f, "resuming CUDA in {pid} failed: {error}"),
                },
            }
        }
        describe(f, &self.failed, &self.error)
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
/// deadline and is best effort: it continues past failing steps. Each CUDA
/// step of the rollback checks its own PID first ([`Step::IfRunnable`]),
/// so a process that is gone, stopped or frozen (a CUDA call would hang)
/// is skipped without keeping the others locked or checkpointed.
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
    // A step that never ran changed nothing.
    let failed = failure.failed.action().filter(|_| failure.error.ran());
    for step in plan::rollback_plan(cfg, &done, failed.as_ref(), killed) {
        let result = exec.execute(&step);
        failure.rollback.push(StepRecord { step, result });
    }
    Err(failure)
}

/// [`run_with_rollback`] for a dump plan ([`plan::dump_plan`]) of a tree
/// whose CUDA processes are `cuda_pids`. In plugin mode, after a
/// `criu dump` that was killed, it also runs [`plan::plugin_dump_rollback`]
/// (CRIU's CUDA plugin may have left them locked or checkpointed), even if
/// the thaw failed: each of its steps checks its own PID first.
pub fn run_dump_with_rollback<E: Executor + ?Sized>(
    exec: &mut E,
    cfg: &CheckpointConfig,
    steps: Vec<Step>,
    cuda_pids: &[u32],
    deadline: Option<&Deadline>,
) -> Result<Vec<StepRecord>, Box<PlanFailure>> {
    let mut failure = match run_with_rollback(exec, cfg, steps, deadline) {
        Ok(done) => return Ok(done),
        Err(f) => f,
    };
    if failure.criu_dump_killed() {
        for step in plan::plugin_dump_rollback(cfg, cuda_pids) {
            let result = exec.execute(&step);
            failure.rollback.push(StepRecord { step, result });
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
    /// The PID namespace (the `/proc/<pid>/ns/pid` link) of the job that
    /// the steps are confined to (see [`Executor::confine_to`]); empty when
    /// it could not be read, which no process matches.
    pub job_pid_ns: Option<PathBuf>,
    /// How long [`Step::Thaw`] waits for the tree to run again.
    pub thaw_settle_timeout: Duration,
}

impl Default for SystemExecutor {
    fn default() -> Self {
        Self {
            proc_root: PathBuf::from("/proc"),
            cgroup_root: PathBuf::from("/sys/fs/cgroup"),
            poll_interval: Duration::from_millis(20),
            job_pid_ns: None,
            thaw_settle_timeout: THAW_SETTLE_TIMEOUT,
        }
    }
}

/// The process a step acts on by its PID.
fn action_pid(action: &Action) -> Option<u32> {
    match *action {
        Action::CudaLock { pid }
        | Action::CudaCheckpoint { pid }
        | Action::CudaRestore { pid }
        | Action::CudaUnlock { pid } => Some(pid),
        Action::CriuDump { root_pid } => Some(root_pid),
        Action::CriuRestore => None,
    }
}

impl Executor for SystemExecutor {
    fn confine_to(&mut self, init_pid: u32) {
        self.job_pid_ns = Some(procfs::pid_ns(&self.proc_root, init_pid).unwrap_or_default());
    }

    fn execute(&mut self, step: &Step) -> Result<StepOutput, StepError> {
        match step {
            Step::EnsureRunnable { pids } => {
                for &pid in pids {
                    self.ensure_in_job(pid)?;
                    if let Some(reason) =
                        procfs::check_runnable(&self.proc_root, &self.cgroup_root, pid)?
                    {
                        return Err(StepError::Unrunnable { pid, reason });
                    }
                }
                Ok(StepOutput::default())
            }
            Step::IfRunnable { pid, then } => {
                // A PID that is no longer the job's: its process is gone.
                let runnable = if self.in_job(*pid) {
                    procfs::check_runnable(&self.proc_root, &self.cgroup_root, *pid)
                } else {
                    Err(io::Error::from(io::ErrorKind::NotFound))
                };
                match runnable {
                    Ok(None) => self.execute(then),
                    Ok(Some(reason)) => Err(StepError::Unrunnable { pid: *pid, reason }),
                    // Gone (or exited): there is nothing left to undo.
                    Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(StepOutput {
                        stdout: format!("process {pid} is gone; skipped"),
                        stderr: String::new(),
                    }),
                    Err(e) => Err(e.into()),
                }
            }
            Step::Thaw { root_pid } => self.thaw(*root_pid),
            Step::Run {
                action,
                command,
                timeout,
            } => {
                if let Some(pid) = action_pid(action) {
                    self.ensure_in_job(pid)?;
                }
                run_with_timeout(command, *timeout, self.poll_interval)
            }
            Step::IfCudaState {
                pid,
                get_state,
                when,
                then,
            } => {
                self.ensure_in_job(*pid)?;
                let out =
                    run_with_timeout(get_state, plan::CUDA_GET_STATE_TIMEOUT, self.poll_interval)?;
                match parse_cuda_state(&out.stdout) {
                    Some(state) if state == *when => self.execute(then),
                    Some(
                        state @ (CudaState::Running | CudaState::Locked | CudaState::Checkpointed),
                    ) => Ok(StepOutput {
                        stdout: format!("process {pid} is {state:?}"),
                        stderr: String::new(),
                    }),
                    state => Err(StepError::Failed {
                        code: None,
                        output: StepOutput {
                            stderr: format!(
                                "unexpected CUDA state of process {pid}: {}",
                                state.map_or_else(
                                    || out.stdout.trim().to_owned(),
                                    |s| format!("{s:?}")
                                )
                            ),
                            stdout: out.stdout,
                        },
                    }),
                }
            }
        }
    }
}

impl SystemExecutor {
    /// Whether `pid` is in the job's PID namespace (always, unconfined).
    fn in_job(&self, pid: u32) -> bool {
        match &self.job_pid_ns {
            None => true,
            Some(ns) => procfs::pid_ns(&self.proc_root, pid).is_ok_and(|p| p == *ns),
        }
    }

    fn ensure_in_job(&self, pid: u32) -> Result<(), StepError> {
        if self.in_job(pid) {
            Ok(())
        } else {
            Err(StepError::NotInJob { pid })
        }
    }

    /// SIGCONT to `pid` if it is still a stopped process in the PID
    /// namespace `ns` (that of the tree's root) and of the job. Through a
    /// pidfd opened before the checks: if the PID was reused meanwhile, the
    /// pidfd names the process that exited, and the signal goes nowhere.
    fn continue_task(&self, pid: u32, ns: &Path) -> bool {
        use rustix::process::{PidfdFlags, Signal, pidfd_open, pidfd_send_signal};
        let Some(raw) = i32::try_from(pid)
            .ok()
            .and_then(rustix::process::Pid::from_raw)
        else {
            return false;
        };
        let Ok(fd) = pidfd_open(raw, PidfdFlags::empty()) else {
            return false;
        };
        let member = procfs::pid_ns(&self.proc_root, pid).is_ok_and(|p| p == ns)
            && self.in_job(pid)
            && procfs::inspect(&self.proc_root, &self.cgroup_root, pid)
                .is_ok_and(|st| st.stopped == Some('T'));
        member && pidfd_send_signal(&fd, Signal::CONT).is_ok()
    }

    /// [`Step::Thaw`]: SIGCONT the stopped tasks and thaw the frozen
    /// cgroups of the tree (up to the cgroup of `root`, see
    /// [`procfs::ThawPlan`]), until (briefly) none is stopped or frozen.
    /// Both take effect asynchronously, and a task may be stopped again
    /// meanwhile (e.g. by a SIGSTOP still queued), so they are re-applied
    /// on every check.
    fn thaw(&self, root: u32) -> Result<StepOutput, StepError> {
        let mut done: Vec<String> = Vec::new();
        let mut note = |line: String| {
            if !done.contains(&line) {
                done.push(line);
            }
        };
        let until = Instant::now() + self.thaw_settle_timeout;
        loop {
            let left = procfs::plan_thaw(&self.proc_root, &self.cgroup_root, root)?;
            if left.is_empty() {
                break;
            }
            if Instant::now() >= until {
                let (pid, reason) = match (left.frozen_tasks.first(), left.stopped.first()) {
                    (Some((pid, dir)), _) => (*pid, Unrunnable::CgroupFrozen(dir.clone())),
                    (None, Some(pid)) => (*pid, Unrunnable::Stopped('T')),
                    (None, None) => (root, Unrunnable::Stopped('T')),
                };
                return Err(StepError::Unrunnable { pid, reason });
            }
            for dir in &left.frozen {
                std::fs::write(dir.join("cgroup.freeze"), "0")?;
                note(format!("thawed {}", dir.display()));
            }
            if !left.stopped.is_empty() {
                // The tree's processes are those in the PID namespace of
                // its root (a namespace init) or, for a root in the host's,
                // processes there.
                let ns = procfs::pid_ns(&self.proc_root, root)?;
                if self.job_pid_ns.as_ref().is_some_and(|j| *j != ns) {
                    return Err(StepError::NotInJob { pid: root });
                }
                for &pid in &left.stopped {
                    if self.continue_task(pid, &ns) {
                        note(format!("SIGCONT {pid}"));
                    }
                }
            }
            thread::sleep(self.poll_interval);
        }
        Ok(StepOutput {
            stdout: done.join("\n"),
            stderr: String::new(),
        })
    }
}

/// How long [`Step::Thaw`] waits for the tree to run again (by default).
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
    run_until(spec, timeout, poll_interval, |deadline| {
        Instant::now() >= deadline
    })
}

/// [`run_with_timeout`], which kills the child once `expired` says so,
/// given the deadline (`timeout` after the spawn). Tests also wait there
/// for the child to be set up, which a loaded machine may take longer for
/// than the timeout under test.
fn run_until(
    spec: &CommandSpec,
    timeout: Duration,
    poll_interval: Duration,
    mut expired: impl FnMut(Instant) -> bool,
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
        if expired(deadline) {
            let status = kill_tree(&mut child)?;
            // It may have exited on its own between `try_wait` and the
            // kill: then it was not killed by us, and how it ended counts
            // (e.g. a `criu dump` that finished has a complete image).
            if status.signal() == Some(rustix::process::Signal::KILL.as_raw()) {
                break None;
            }
            break Some(status);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        // Past the deadline but not expired (a test waits for its child):
        // no busy loop.
        thread::sleep(if left.is_zero() {
            poll_interval
        } else {
            poll_interval.min(left)
        });
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

/// Kill `child`, its process group and all its descendants, and reap
/// `child`. Returns its exit status, which shows a normal exit if it had
/// exited on its own before it could be killed.
///
/// The tree is stopped first so that nothing forks or reaps while it is
/// collected, and the descendants are collected before the child dies
/// (afterwards they would be reparented and could no longer be found).
/// Descendants are signalled through pidfds (see [`open_descendants`]), so
/// a PID that was reused after the `/proc` scan is never signalled. The
/// child itself and its process group are safe to signal by PID: the
/// child is not reaped before the end.
fn kill_tree(child: &mut Child) -> io::Result<ExitStatus> {
    use rustix::process::{Signal, pidfd_send_signal};
    let proc_root = Path::new("/proc");
    let pid = child.id();
    let mut victims = Vec::new();
    open_descendants(proc_root, pid, &mut victims);
    signal(pid, Signal::STOP);
    for (_, fd) in &victims {
        let _ = pidfd_send_signal(fd, Signal::STOP);
    }
    open_descendants(proc_root, pid, &mut victims);
    if let Some(pgid) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    {
        let _ = rustix::process::kill_process_group(pgid, Signal::KILL);
    }
    for (_, fd) in &victims {
        let _ = pidfd_send_signal(fd, Signal::KILL);
    }
    match child.kill() {
        // Already exited between try_wait and kill.
        Err(e) if e.kind() == io::ErrorKind::InvalidInput => {}
        r => r?,
    }
    child.wait()
}

/// Add pidfds of the descendants of `root` (found by a `/proc` scan) that
/// are not in `out` yet. A PID may be reused between the scan and opening
/// its pidfd, so once the pidfd is open, the process's parent is read
/// again: it must still be `root` or a process of the scanned tree (a
/// pidfd opened on a process that exits afterwards just fails to signal).
/// Without pidfd support (Linux < 5.3) descendants outside the process
/// group are not killed.
fn open_descendants(proc_root: &Path, root: u32, out: &mut Vec<(u32, OwnedFd)>) {
    let Ok(tree) = procfs::descendants(proc_root, root) else {
        return;
    };
    for &p in &tree {
        if out.iter().any(|(q, _)| *q == p) {
            continue;
        }
        let Some(raw) = i32::try_from(p)
            .ok()
            .and_then(rustix::process::Pid::from_raw)
        else {
            continue;
        };
        let Ok(fd) = rustix::process::pidfd_open(raw, rustix::process::PidfdFlags::empty()) else {
            continue;
        };
        let parent = std::fs::read_to_string(proc_root.join(p.to_string()).join("stat"))
            .ok()
            .as_deref()
            .and_then(procfs::parse_stat_ppid);
        if parent.is_some_and(|pp| pp == root || tree.contains(&pp)) {
            out.push((p, fd));
        }
    }
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

    /// Records every step and fails the ones it was told to. A
    /// [`Step::IfRunnable`] guard is not recorded itself: it skips its step
    /// for a PID in `gone`, fails for one in `stopped` (or when
    /// `fail_runnable`), and runs it otherwise.
    #[derive(Default)]
    struct FakeExecutor {
        log: Vec<Step>,
        fail: HashMap<Option<Action>, String>,
        fail_runnable: bool,
        gone: Vec<u32>,
        stopped: Vec<u32>,
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
            if let Step::IfRunnable { pid, then } = step {
                if self.gone.contains(pid) {
                    return Ok(StepOutput::default());
                }
                if self.fail_runnable || self.stopped.contains(pid) {
                    return Err(StepError::Unrunnable {
                        pid: *pid,
                        reason: Unrunnable::Stopped('T'),
                    });
                }
                return self.execute(then);
            }
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
        assert_eq!(f.rollback.len(), 5);
        assert_eq!(
            f.rollback[1].result.as_ref().unwrap_err().cuda_error(),
            Some(CudaError::OutOfMemory)
        );
        assert!(f.rollback[2..].iter().all(|r| r.result.is_ok()));
    }

    /// `criu dump` dies of `signal` (or fails on its own when `None`);
    /// everything else succeeds.
    struct KilledCriu(Option<i32>, Vec<Step>);

    impl Executor for KilledCriu {
        fn execute(&mut self, step: &Step) -> Result<StepOutput, StepError> {
            if let Step::IfRunnable { then, .. } = step {
                return self.execute(then);
            }
            self.1.push(step.clone());
            match (step.action(), self.0) {
                (Some(CriuDump { .. }), Some(signal)) => Err(StepError::Signaled {
                    signal,
                    output: StepOutput::default(),
                }),
                (Some(CriuDump { .. }), None) => Err(StepError::Failed {
                    code: Some(1),
                    output: StepOutput::default(),
                }),
                _ => Ok(StepOutput::default()),
            }
        }
    }

    #[test]
    fn killed_plugin_dump_resumes_cuda_by_state() {
        let c = cfg(CudaMode::Plugin);
        let mut ex = KilledCriu(Some(9), Vec::new());
        let f = run_dump_with_rollback(
            &mut ex,
            &c,
            plan::dump_plan(&c, &dump_target(), &[101]),
            &[101],
            None,
        )
        .unwrap_err();
        assert!(f.rolled_back_cleanly());
        assert_eq!(f.rollback[0].step, Step::Thaw { root_pid: 100 });
        let rest: Vec<_> = f.rollback[1..].iter().map(|r| r.step.clone()).collect();
        assert_eq!(rest, plan::plugin_dump_rollback(&c, &[101]));
        assert_eq!(rest.len(), 2);

        // CRIU failing on its own resumes the devices itself; so does a
        // manual-mode dump, whose own CUDA steps are rolled back.
        let mut ex = KilledCriu(None, Vec::new());
        let f = run_dump_with_rollback(
            &mut ex,
            &c,
            plan::dump_plan(&c, &dump_target(), &[101]),
            &[101],
            None,
        )
        .unwrap_err();
        assert_eq!(f.rollback.len(), 1);
        let m = cfg(CudaMode::Manual);
        let mut ex = KilledCriu(Some(9), Vec::new());
        let f = run_dump_with_rollback(
            &mut ex,
            &m,
            plan::dump_plan(&m, &dump_target(), &[101]),
            &[101],
            None,
        )
        .unwrap_err();
        assert!(f.rollback.iter().all(|r| match &r.step {
            Step::IfRunnable { then, .. } => !matches!(**then, Step::IfCudaState { .. }),
            s => !matches!(s, Step::IfCudaState { .. }),
        }));
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
    fn rollback_skips_pids_that_became_frozen() {
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
        // Thawed, then every CUDA step was refused: no CUDA call was made
        // on a frozen process.
        assert_eq!(f.rollback.len(), 3);
        assert!(
            f.rollback[1..]
                .iter()
                .all(|r| matches!(r.result, Err(StepError::Unrunnable { pid: 101, .. })))
        );
        assert!(!f.rolled_back_cleanly());
        assert!(
            !ex.0
                .actions()
                .iter()
                .any(|a| matches!(a, Some(CudaRestore { .. } | CudaUnlock { .. })))
        );
    }

    /// One CUDA process that vanished (e.g. OOM-killed during its
    /// checkpoint) or is stopped must not keep the others checkpointed.
    #[test]
    fn rollback_is_per_pid() {
        let c = cfg(CudaMode::Manual);
        for (gone, stopped) in [(vec![102], vec![]), (vec![], vec![102])] {
            let mut ex = FakeExecutor::failing(CriuDump { root_pid: 100 }, "x");
            ex.gone = gone.clone();
            ex.stopped = stopped.clone();
            let f = run_with_rollback(
                &mut ex,
                &c,
                plan::dump_plan(&c, &dump_target(), &[101, 102, 103]),
                None,
            )
            .unwrap_err();
            assert_eq!(
                ex.actions()[8..],
                [
                    None,
                    Some(CudaRestore { pid: 101 }),
                    Some(CudaRestore { pid: 103 }),
                    Some(CudaUnlock { pid: 101 }),
                    Some(CudaUnlock { pid: 103 }),
                ],
                "gone {gone:?}, stopped {stopped:?}"
            );
            // A gone process has nothing left to undo; a stopped one is
            // reported.
            assert_eq!(f.rolled_back_cleanly(), stopped.is_empty());
        }
    }

    /// The same in plugin mode, and the CUDA processes are resumed even if
    /// the thaw failed.
    #[test]
    fn plugin_rollback_is_per_pid_and_runs_after_a_failed_thaw() {
        struct Ex(Vec<Step>);
        impl Executor for Ex {
            fn execute(&mut self, step: &Step) -> Result<StepOutput, StepError> {
                match step {
                    Step::IfRunnable { pid: 101, .. } => Ok(StepOutput::default()),
                    Step::IfRunnable { then, .. } => self.execute(then),
                    Step::Thaw { .. } => Err(StepError::Unrunnable {
                        pid: 100,
                        reason: Unrunnable::Stopped('T'),
                    }),
                    s if matches!(s.action(), Some(CriuDump { .. })) => Err(StepError::Signaled {
                        signal: 9,
                        output: StepOutput::default(),
                    }),
                    s => {
                        self.0.push(s.clone());
                        Ok(StepOutput::default())
                    }
                }
            }
        }
        let c = cfg(CudaMode::Plugin);
        let mut ex = Ex(Vec::new());
        let f = run_dump_with_rollback(
            &mut ex,
            &c,
            plan::dump_plan(&c, &dump_target(), &[101, 102]),
            &[101, 102],
            None,
        )
        .unwrap_err();
        let queried: Vec<u32> =
            ex.0.iter()
                .filter_map(|s| match s {
                    Step::IfCudaState { pid, .. } => Some(*pid),
                    _ => None,
                })
                .collect();
        assert_eq!(queried, [102, 102]);
        assert_eq!(f.rollback.len(), 5);
    }

    /// A step that never ran (past the deadline) changed nothing: a
    /// resume step that was not started is retried by the rollback rather
    /// than treated as a failed (broken) restore.
    #[test]
    fn rollback_after_a_step_that_never_ran() {
        let c = cfg(CudaMode::Manual);
        struct Ex(Vec<Option<Action>>);
        impl Executor for Ex {
            fn execute(&mut self, step: &Step) -> Result<StepOutput, StepError> {
                if let Step::IfRunnable { then, .. } = step {
                    return self.execute(then);
                }
                self.0.push(step.action());
                match step.action() {
                    Some(CudaRestore { pid: 102 }) if self.0.iter().flatten().count() == 7 => {
                        Err(StepError::DeadlineExceeded {
                            budget: Duration::from_secs(1),
                            ran: false,
                            output: StepOutput::default(),
                        })
                    }
                    _ => Ok(StepOutput::default()),
                }
            }
        }
        let mut t = dump_target();
        t.leave_running = true;
        let mut ex = Ex(Vec::new());
        let f =
            run_with_rollback(&mut ex, &c, plan::dump_plan(&c, &t, &[101, 102]), None).unwrap_err();
        assert!(!f.error.ran());
        assert_eq!(
            f.rollback
                .iter()
                .map(|r| match &r.step {
                    Step::IfRunnable { then, .. } => then.action(),
                    s => s.action(),
                })
                .collect::<Vec<_>>(),
            [
                Some(CudaRestore { pid: 102 }),
                Some(CudaUnlock { pid: 101 }),
                Some(CudaUnlock { pid: 102 }),
            ]
        );
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
            if let Step::IfRunnable { then, .. } = step {
                return self.execute(then);
            }
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
        assert!(f.criu_dump_killed());
        assert!(
            f.to_string()
                .contains("was killed at the operation's deadline of 100s"),
            "{f}"
        );
        let rb: Vec<Step> = f.rollback.iter().map(|r| r.step.clone()).collect();
        assert_eq!(rb[0], Step::Thaw { root_pid: 100 });
        assert_eq!(
            crate::plan::tests::actions(&rb[1..]),
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
            crate::plan::tests::actions(
                &f.rollback
                    .iter()
                    .map(|r| r.step.clone())
                    .collect::<Vec<_>>()
            ),
            [
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

    /// Once confined to a job, steps on a PID outside the job's PID
    /// namespace (a reused PID) are not run.
    #[test]
    fn confined_steps_skip_processes_outside_the_job() {
        let tmp = crate::test_util::TempDir::new();
        let proc_root = tmp.path().join("proc");
        for (pid, ns) in [(10, "pid:[1]"), (11, "pid:[1]"), (20, "pid:[2]")] {
            let dir = proc_root.join(pid.to_string()).join("ns");
            std::fs::create_dir_all(&dir).unwrap();
            std::os::unix::fs::symlink(ns, dir.join("pid")).unwrap();
        }
        let marker = tmp.path().join("ran");
        let run = |pid: u32| Step::Run {
            action: Action::CudaLock { pid },
            command: sh(&format!("echo {pid} >> {}", marker.display())),
            timeout: Duration::from_secs(10),
        };
        let mut ex = SystemExecutor {
            proc_root,
            ..SystemExecutor::default()
        };
        // Unconfined: anything goes.
        ex.execute(&run(20)).unwrap();
        ex.confine_to(10);
        ex.execute(&run(11)).unwrap();
        let e = ex.execute(&run(20)).unwrap_err();
        assert!(matches!(e, StepError::NotInJob { pid: 20 }), "{e:?}");
        assert!(!e.ran());
        let e = ex.execute(&run(30)).unwrap_err();
        assert!(matches!(e, StepError::NotInJob { pid: 30 }), "{e:?}");
        let e = ex
            .execute(&Step::Run {
                action: Action::CriuDump { root_pid: 20 },
                command: sh("true"),
                timeout: Duration::from_secs(10),
            })
            .unwrap_err();
        assert!(matches!(e, StepError::NotInJob { pid: 20 }), "{e:?}");
        // A rollback guard takes it as gone.
        let out = ex
            .execute(&Step::IfRunnable {
                pid: 20,
                then: Box::new(run(20)),
            })
            .unwrap();
        assert!(out.stdout.contains("gone"), "{out:?}");
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "20\n11\n");
    }

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
        let tmp = crate::test_util::TempDir::new();
        let ready = tmp.path().join("ready");
        let start = Instant::now();
        // Killed at the timeout, but not before it has written its output.
        let e = run_until(
            &sh(&format!(
                "echo started; : > {}; exec sleep 30",
                ready.display()
            )),
            Duration::from_millis(200),
            POLL,
            |deadline| ready.exists() && Instant::now() >= deadline,
        )
        .unwrap_err();
        // Not after the 30 seconds of `sleep`.
        assert!(start.elapsed() < Duration::from_secs(25));
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
        let pid = |suffix| -> Option<u32> {
            std::fs::read_to_string(pids.with_extension(suffix))
                .ok()?
                .trim()
                .parse()
                .ok()
        };
        // Killed at the timeout once both grandchildren were started.
        let e = run_until(&sh(&script), Duration::from_millis(500), POLL, |deadline| {
            pid("a").is_some() && pid("b").is_some() && Instant::now() >= deadline
        })
        .unwrap_err();
        assert!(matches!(e, StepError::TimedOut { .. }), "{e:?}");
        for suffix in ["a", "b"] {
            let pid = pid(suffix).unwrap();
            // SIGKILL takes effect when the process is next scheduled: on
            // a loaded machine, that may take a while.
            let deadline = Instant::now() + Duration::from_secs(30);
            while alive(pid) {
                assert!(
                    Instant::now() < deadline,
                    "grandchild {suffix} ({pid}) survived"
                );
                thread::sleep(Duration::from_millis(10));
            }
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

    /// Spawn `script` as `run_with_timeout` does and wait until it has
    /// exited (a zombie, not reaped yet).
    fn exited_child(script: &str) -> Child {
        let mut child = sh(script).to_command().process_group(0).spawn().unwrap();
        for _ in 0..500 {
            if !alive(child.id()) {
                return child;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let _ = child.kill();
        let _ = child.wait();
        panic!("{script} did not exit");
    }

    /// A child that exits between `try_wait` and the kill is reported by
    /// how it exited, not as killed: a `criu dump` that succeeded at its
    /// deadline must not be rolled back.
    #[test]
    fn kill_tree_reports_a_child_that_exited_first() {
        let status = kill_tree(&mut exited_child("exit 0")).unwrap();
        assert!(status.success(), "{status:?}");
        let status = kill_tree(&mut exited_child("exit 3")).unwrap();
        assert_eq!(status.code(), Some(3));
        let mut running = sh("exec sleep 30")
            .to_command()
            .process_group(0)
            .spawn()
            .unwrap();
        let status = kill_tree(&mut running).unwrap();
        assert_eq!(status.signal(), Some(9));
    }

    #[test]
    fn open_descendants_checks_the_parent_after_opening() {
        let tmp = crate::test_util::TempDir::new();
        let proc_root = tmp.path().join("proc");
        let me = std::process::id();
        // A fake /proc in which this test process (which has a real pidfd)
        // is a child of the fake root 4000000.
        let d = proc_root.join(me.to_string());
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("stat"), format!("{me} (x) S 4000000 1 1")).unwrap();
        let mut out = Vec::new();
        open_descendants(&proc_root, 4_000_000, &mut out);
        assert_eq!(out.len(), 1, "parent still matches");
        // The parent changed between the scan and the check.
        std::fs::write(
            proc_root.join(me.to_string()).join("stat"),
            format!("{me} (x) S 1 1 1"),
        )
        .unwrap();
        let mut out = Vec::new();
        open_descendants(&proc_root, 4_000_000, &mut out);
        assert!(out.is_empty());
    }

    /// The settle loop re-applies the thaw: a cgroup that is frozen again
    /// after the first write (or whose thaw did not take) is thawed again.
    #[test]
    fn system_thaw_reapplies_until_settled() {
        let tmp = crate::test_util::TempDir::new();
        let proc_root = tmp.path().join("proc");
        let cg = tmp.path().join("cg");
        let d = proc_root.join("10");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("stat"), "10 (job) S 1 1 1").unwrap();
        std::fs::write(d.join("cgroup"), "0::/job\n").unwrap();
        std::fs::create_dir_all(cg.join("job")).unwrap();
        let freeze = cg.join("job/cgroup.freeze");
        let events = cg.join("job/cgroup.events");
        // As the kernel's files, these are never seen half-written.
        let write = |path: &Path, text: &str| {
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, text).unwrap();
            std::fs::rename(&tmp, path).unwrap();
        };
        write(&freeze, "1\n");
        write(&events, "populated 1\nfrozen 1\n");
        // Far more than either side takes, even on a loaded machine: only
        // a broken thaw runs into it.
        let settle = Duration::from_secs(60);
        let kernel = {
            let (freeze, events) = (freeze.clone(), events.clone());
            thread::spawn(move || {
                let wait_thawed = || {
                    let deadline = Instant::now() + settle;
                    // The thaw's write may be seen half done (empty).
                    while std::fs::read_to_string(&freeze).unwrap().trim() != "0" {
                        assert!(Instant::now() < deadline, "never thawed");
                        thread::sleep(Duration::from_millis(1));
                    }
                };
                // The first thaw is undone (frozen again)...
                wait_thawed();
                write(&freeze, "1\n");
                // ...the second one takes.
                wait_thawed();
                write(&events, "populated 1\nfrozen 0\n");
            })
        };
        let mut ex = SystemExecutor {
            proc_root,
            cgroup_root: cg.clone(),
            poll_interval: POLL,
            thaw_settle_timeout: settle,
            ..SystemExecutor::default()
        };
        let res = ex.execute(&Step::Thaw { root_pid: 10 });
        kernel.join().unwrap();
        let out = res.unwrap();
        assert_eq!(out.stdout, format!("thawed {}", cg.join("job").display()));
    }

    #[test]
    fn system_if_runnable_skips_gone_and_refuses_stopped() {
        let mut ex = SystemExecutor::default();
        let then = || {
            Box::new(Step::Run {
                action: CudaUnlock { pid: 1 },
                command: sh("echo ran"),
                timeout: Duration::from_secs(5),
            })
        };
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        let out = ex.execute(&Step::IfRunnable { pid, then: then() }).unwrap();
        assert_eq!(out.stdout, "ran\n");
        signal(pid, rustix::process::Signal::STOP);
        let mut res = Ok(StepOutput::default());
        for _ in 0..200 {
            res = ex.execute(&Step::IfRunnable { pid, then: then() });
            if res.is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            matches!(res, Err(StepError::Unrunnable { pid: p, reason: Unrunnable::Stopped('T') }) if p == pid),
            "{res:?}"
        );
        let _ = child.kill();
        let _ = child.wait();
        let out = ex.execute(&Step::IfRunnable { pid, then: then() }).unwrap();
        assert_eq!(out.stdout, format!("process {pid} is gone; skipped"));
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
            &CommandSpec::new("/nonexistent/gpueue-bin"),
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

    #[test]
    fn system_executor_acts_on_cuda_state() {
        let mut ex = SystemExecutor::default();
        let cond = |state: &str, when| Step::IfCudaState {
            pid: 7,
            get_state: sh(&format!("echo {state}")),
            when,
            then: Box::new(Step::Run {
                action: CudaUnlock { pid: 7 },
                command: sh("echo unlocked"),
                timeout: Duration::from_secs(5),
            }),
        };
        let out = ex.execute(&cond("locked", CudaState::Locked)).unwrap();
        assert_eq!(out.stdout, "unlocked\n");
        let out = ex.execute(&cond("running", CudaState::Locked)).unwrap();
        assert_eq!(out.stdout, "process 7 is Running");
        let err = ex
            .execute(&cond("restoring", CudaState::Locked))
            .unwrap_err();
        assert!(err.to_string().contains("Restoring"), "{err}");
        assert!(ex.execute(&cond("garbage", CudaState::Locked)).is_err());
    }
}
