//! Checkpoint/restore expressed as explicit, ordered plans of steps.
//!
//! Ordering rules (from the PoCs):
//! - Never call `cuda-checkpoint` on a stopped or frozen process: it hangs
//!   forever. Every plan that touches CUDA starts with
//!   [`Step::EnsureRunnable`], and every command carries an outer timeout.
//! - Manual dump: lock *all* CUDA PIDs, then checkpoint all, then
//!   `criu dump`. Manual restore: `criu restore`, then restore all, then
//!   unlock all.
//! - Plugin mode: CRIU's CUDA plugin does the CUDA part itself.
//! - On failure the CUDA steps already taken are undone in reverse
//!   ([`rollback_plan`]): checkpointed PIDs are restored, then every locked
//!   PID is unlocked. After a killed plugin-mode `criu dump`, the CUDA
//!   processes are resumed according to their queried state
//!   ([`plugin_dump_rollback`]).

use std::time::Duration;

use crate::command::{
    self, CommandSpec, CriuOptions, CudaAction, CudaCheckpointOptions, CudaMode, DeviceMap,
    DumpTarget, RestoreTarget,
};
use crate::probe::CudaState;

/// Stand-in for "no timeout of its own" on a step that is still bounded by
/// its operation's deadline (see [`Timeouts::operation_deadline`]).
pub const UNBOUNDED: Duration = Duration::from_secs(7 * 24 * 3600);

const GIB: u64 = 1 << 30;

/// Outer timeouts applied by the executor (the child is killed when hit).
///
/// Every step has its own timeout (a guard against a hung tool), and the
/// whole checkpoint or restore has a deadline that scales with the job's
/// size: [`Timeouts::operation_deadline`]. A step gets whichever ends first.
///
/// Defaults are based on measurements on an NVMe disk (see
/// `docs/install.md`, "Checkpoint timeouts"): `criu dump` wrote 14 GiB of
/// RSS in 18-25 s (up to ~1.8 s/GiB under memory pressure), `criu restore`
/// read it back in ~9 s from a cold page cache, `cuda-checkpoint` moved
/// GPU memory at 0.6-1.0 s/GiB (11.8 GiB: 8-10 s; restore 6-8 s), and a
/// lock or unlock took a few milliseconds: about 1.5-2.5 s per GiB end to
/// end, against the default deadline's 30 s/GiB + 180 s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timeouts {
    /// Added to the lock `--timeout` to get the outer timeout of a lock.
    pub cuda_lock_grace: Duration,
    /// Outer timeout of a lock when no `--timeout` is configured.
    pub cuda_lock_without_timeout: Duration,
    pub cuda_checkpoint: Duration,
    pub cuda_restore: Duration,
    pub cuda_unlock: Duration,
    /// Optional cap on `criu dump` alone; `None`: only the operation's
    /// deadline applies.
    pub criu_dump: Option<Duration>,
    /// Optional cap on `criu restore` alone; `None`: only the operation's
    /// deadline applies.
    pub criu_restore: Option<Duration>,
    /// Fixed part of [`Timeouts::operation_deadline`].
    pub deadline_base: Duration,
    /// Part of [`Timeouts::operation_deadline`] per started GiB of job
    /// memory (RSS + VRAM for a dump, image size for a restore).
    pub deadline_per_gib: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            cuda_lock_grace: Duration::from_secs(15),
            cuda_lock_without_timeout: Duration::from_secs(120),
            cuda_checkpoint: Duration::from_secs(300),
            cuda_restore: Duration::from_secs(300),
            cuda_unlock: Duration::from_secs(60),
            criu_dump: None,
            criu_restore: None,
            deadline_base: Duration::from_secs(180),
            deadline_per_gib: Duration::from_secs(30),
        }
    }
}

impl Timeouts {
    /// Deadline of a whole checkpoint (locks, CUDA checkpoints and
    /// `criu dump`) or restore (`criu restore`, CUDA restores and unlocks)
    /// of a job with `bytes` of memory: `deadline_base + deadline_per_gib *
    /// ceil(bytes / GiB)`. The rollback after a failure is not counted; see
    /// [`Timeouts::rollback_budget`].
    pub fn operation_deadline(&self, bytes: u64) -> Duration {
        let gib = u32::try_from(bytes.div_ceil(GIB)).unwrap_or(u32::MAX);
        self.deadline_base
            .saturating_add(self.deadline_per_gib.saturating_mul(gib))
            .min(UNBOUNDED)
    }

    /// Upper bound of the rollback after a failed dump with `cuda_pids`
    /// CUDA processes: each one restored and unlocked, plus a few seconds
    /// for thawing and the runnable checks.
    pub fn rollback_budget(&self, cuda_pids: usize) -> Duration {
        // Plus two state queries in plugin mode ([`plugin_dump_rollback`]).
        let per_pid = self.cuda_restore + self.cuda_unlock + 2 * CUDA_GET_STATE_TIMEOUT;
        Duration::from_secs(5)
            + per_pid.saturating_mul(u32::try_from(cuda_pids).unwrap_or(u32::MAX))
    }
}

/// Everything needed to build checkpoint/restore plans.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CheckpointConfig {
    pub mode: CudaMode,
    pub criu: CriuOptions,
    pub cuda: CudaCheckpointOptions,
    pub timeouts: Timeouts,
}

/// What a [`Step::Run`] does, for ordering assertions and rollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    CudaLock { pid: u32 },
    CudaCheckpoint { pid: u32 },
    CudaRestore { pid: u32 },
    CudaUnlock { pid: u32 },
    CriuDump { root_pid: u32 },
    CriuRestore,
}

/// One step of a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Fail unless every PID is neither stopped nor in a frozen cgroup.
    EnsureRunnable { pids: Vec<u32> },
    /// Resume the tree under `root_pid` after an interrupted `criu dump`:
    /// SIGCONT its stopped tasks and thaw its frozen cgroups. Fails if
    /// `root_pid` is gone.
    Thaw { root_pid: u32 },
    /// Run `command`, killing it after `timeout`.
    Run {
        action: Action,
        command: CommandSpec,
        timeout: Duration,
    },
    /// Rollback of a CUDA process whose state is not known (CRIU's CUDA
    /// plugin was killed during a dump): run `get_state` (killed after
    /// [`CUDA_GET_STATE_TIMEOUT`]) and run `then` (a [`Step::Run`]) if the
    /// process is in state `when`. Running, locked and checkpointed
    /// processes in another state are left alone; any other state fails.
    IfCudaState {
        pid: u32,
        get_state: CommandSpec,
        when: CudaState,
        then: Box<Step>,
    },
}

/// Outer timeout of the `cuda-checkpoint --get-state` of a
/// [`Step::IfCudaState`].
pub const CUDA_GET_STATE_TIMEOUT: Duration = Duration::from_secs(30);

impl Step {
    pub fn action(&self) -> Option<Action> {
        match self {
            Step::Run { action, .. } => Some(*action),
            Step::EnsureRunnable { .. } | Step::Thaw { .. } | Step::IfCudaState { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    /// Manual mode must run CUDA restore after `criu restore` returns, which
    /// an attached `criu restore` only does when the job exits.
    #[error("manual CUDA mode requires a detached criu restore")]
    ManualRequiresDetached,
}

fn cuda_step(
    cfg: &CheckpointConfig,
    action: CudaAction,
    pid: u32,
    map: Option<&DeviceMap>,
) -> Step {
    let t = &cfg.timeouts;
    let (action_kind, timeout) = match action {
        CudaAction::Lock => (
            Action::CudaLock { pid },
            cfg.cuda
                .lock_timeout
                .map_or(t.cuda_lock_without_timeout, |l| l + t.cuda_lock_grace),
        ),
        CudaAction::Checkpoint => (Action::CudaCheckpoint { pid }, t.cuda_checkpoint),
        CudaAction::Restore => (Action::CudaRestore { pid }, t.cuda_restore),
        CudaAction::Unlock => (Action::CudaUnlock { pid }, t.cuda_unlock),
    };
    Step::Run {
        action: action_kind,
        command: command::cuda_action(&cfg.cuda, action, pid, map),
        timeout,
    }
}

fn dedup(pids: &[u32]) -> Vec<u32> {
    let mut out: Vec<u32> = Vec::with_capacity(pids.len());
    for &p in pids {
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Plan for dumping a job.
///
/// `cuda_pids` are the host PIDs in the tree that use CUDA (ignored for the
/// CUDA steps in plugin mode, but still checked for being runnable because
/// the plugin calls `cuda-checkpoint` on them too).
pub fn dump_plan(cfg: &CheckpointConfig, target: &DumpTarget, cuda_pids: &[u32]) -> Vec<Step> {
    let cuda_pids = dedup(cuda_pids);
    let mut runnable = vec![target.tree_root_pid];
    runnable.extend(cuda_pids.iter().filter(|p| **p != target.tree_root_pid));

    let mut steps = vec![Step::EnsureRunnable { pids: runnable }];
    let manual = cfg.mode == CudaMode::Manual;
    if manual {
        steps.extend(
            cuda_pids
                .iter()
                .map(|&p| cuda_step(cfg, CudaAction::Lock, p, None)),
        );
        steps.extend(
            cuda_pids
                .iter()
                .map(|&p| cuda_step(cfg, CudaAction::Checkpoint, p, None)),
        );
    }
    steps.push(Step::Run {
        action: Action::CriuDump {
            root_pid: target.tree_root_pid,
        },
        command: command::criu_dump(&cfg.criu, cfg.mode, target),
        timeout: cfg.timeouts.criu_dump.unwrap_or(UNBOUNDED),
    });
    if manual && target.leave_running && !cuda_pids.is_empty() {
        steps.extend(resume_steps(cfg, &cuda_pids, None));
    }
    steps
}

/// The `criu restore` part of a restore. In manual mode, follow it with
/// [`restore_cuda_plan`] once the restored host PIDs are known (they differ
/// from the dumped ones when the job has its own PID namespace).
pub fn restore_criu_plan(
    cfg: &CheckpointConfig,
    target: &RestoreTarget,
) -> Result<Vec<Step>, PlanError> {
    if cfg.mode == CudaMode::Manual && !target.detached {
        return Err(PlanError::ManualRequiresDetached);
    }
    Ok(vec![Step::Run {
        action: Action::CriuRestore,
        command: command::criu_restore(&cfg.criu, cfg.mode, target),
        timeout: cfg.timeouts.criu_restore.unwrap_or(UNBOUNDED),
    }])
}

/// The CUDA part of a restore: restore all, then unlock all. Empty in plugin
/// mode or without CUDA PIDs.
pub fn restore_cuda_plan(
    cfg: &CheckpointConfig,
    host_pids: &[u32],
    device_map: Option<&DeviceMap>,
) -> Vec<Step> {
    if cfg.mode != CudaMode::Manual || host_pids.is_empty() {
        return Vec::new();
    }
    resume_steps(cfg, &dedup(host_pids), device_map)
}

fn resume_steps(cfg: &CheckpointConfig, pids: &[u32], map: Option<&DeviceMap>) -> Vec<Step> {
    let mut steps = vec![Step::EnsureRunnable {
        pids: pids.to_vec(),
    }];
    steps.extend(
        pids.iter()
            .map(|&p| cuda_step(cfg, CudaAction::Restore, p, map)),
    );
    steps.extend(
        pids.iter()
            .map(|&p| cuda_step(cfg, CudaAction::Unlock, p, None)),
    );
    steps
}

/// CUDA state of a PID as implied by the actions run so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Implied {
    Running,
    Locked,
    Checkpointed,
    /// A restore failed; the process cannot be trusted any more.
    Broken,
}

/// Steps that undo the actions in `completed` (successful actions, in
/// order) given that `failed` did not succeed. `killed` says that `failed`
/// was killed (its own timeout, the operation's deadline or a signal from
/// elsewhere) rather than failing on its own, so it may have stopped
/// half-way.
///
/// - A failed `criu dump` is followed by a [`Step::Thaw`] of the tree: CRIU
///   resumes the tasks itself when it fails, but not when it is killed
///   (the kernel then detaches its ptrace, which resumes them, except tasks
///   that were group-stopped or a cgroup that was frozen).
/// - A failed lock leaves the PID running, but a killed one may have
///   locked it, so it is unlocked.
/// - A failed checkpoint may leave the PID locked, so it is unlocked; a
///   killed one may have checkpointed it, so it is restored first.
/// - A failed restore is not retried (an out-of-memory restore is not
///   recoverable); the PID is left alone.
/// - After a failed `criu dump` the checkpointed PIDs are restored and
///   unlocked.
pub fn rollback_plan(
    cfg: &CheckpointConfig,
    completed: &[Action],
    failed: Option<&Action>,
    killed: bool,
) -> Vec<Step> {
    let mut order: Vec<u32> = Vec::new();
    let mut state: Vec<Implied> = Vec::new();
    let mut set = |pid: u32, s: Implied| match order.iter().position(|p| *p == pid) {
        Some(i) => state[i] = s,
        None => {
            order.push(pid);
            state.push(s);
        }
    };
    for a in completed {
        match *a {
            Action::CudaLock { pid } => set(pid, Implied::Locked),
            Action::CudaCheckpoint { pid } => set(pid, Implied::Checkpointed),
            Action::CudaRestore { pid } => set(pid, Implied::Locked),
            Action::CudaUnlock { pid } => set(pid, Implied::Running),
            Action::CriuDump { .. } | Action::CriuRestore => {}
        }
    }
    match failed {
        Some(Action::CudaLock { pid }) if killed => set(*pid, Implied::Locked),
        Some(Action::CudaCheckpoint { pid }) if killed => set(*pid, Implied::Checkpointed),
        Some(Action::CudaCheckpoint { pid }) => set(*pid, Implied::Locked),
        Some(Action::CudaRestore { pid }) => set(*pid, Implied::Broken),
        _ => {}
    }

    let mut steps = Vec::new();
    if let Some(Action::CriuDump { root_pid }) = failed {
        steps.push(Step::Thaw {
            root_pid: *root_pid,
        });
    }
    let pick = |want: &[Implied]| -> Vec<u32> {
        order
            .iter()
            .zip(&state)
            .filter(|(_, s)| want.contains(s))
            .map(|(p, _)| *p)
            .collect()
    };
    let to_restore = pick(&[Implied::Checkpointed]);
    let to_unlock = pick(&[Implied::Checkpointed, Implied::Locked]);
    if to_unlock.is_empty() {
        return steps;
    }
    steps.push(Step::EnsureRunnable {
        pids: to_unlock.clone(),
    });
    steps.extend(
        to_restore
            .iter()
            .map(|&p| cuda_step(cfg, CudaAction::Restore, p, None)),
    );
    steps.extend(
        to_unlock
            .iter()
            .map(|&p| cuda_step(cfg, CudaAction::Unlock, p, None)),
    );
    steps
}

/// The CUDA part of the rollback after a plugin-mode `criu dump` that was
/// killed (its deadline, or a signal from elsewhere), to run after the
/// [`Step::Thaw`] of [`rollback_plan`]. CRIU's CUDA plugin locks and
/// checkpoints `cuda_pids` inside `criu dump` and resumes them itself when
/// the dump fails, but not when CRIU is killed: they may then be left
/// locked or checkpointed, and the job would hang in its next CUDA call.
/// Their state is queried: checkpointed PIDs are restored, then locked
/// ones unlocked. Empty in manual mode (the helper's own CUDA steps are
/// undone by [`rollback_plan`]) or without CUDA PIDs.
pub fn plugin_dump_rollback(cfg: &CheckpointConfig, cuda_pids: &[u32]) -> Vec<Step> {
    let pids = dedup(cuda_pids);
    if cfg.mode != CudaMode::Plugin || pids.is_empty() {
        return Vec::new();
    }
    let when = |state: CudaState, action: CudaAction| {
        pids.iter().map(move |&pid| Step::IfCudaState {
            pid,
            get_state: command::cuda_get_state(&cfg.cuda, pid),
            when: state,
            then: Box::new(cuda_step(cfg, action, pid, None)),
        })
    };
    let mut steps = vec![Step::EnsureRunnable { pids: pids.clone() }];
    steps.extend(when(CudaState::Checkpointed, CudaAction::Restore));
    steps.extend(when(CudaState::Locked, CudaAction::Unlock));
    steps
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::PathBuf;

    pub(crate) fn cfg(mode: CudaMode) -> CheckpointConfig {
        CheckpointConfig {
            mode,
            ..Default::default()
        }
    }

    pub(crate) fn dump_target() -> DumpTarget {
        DumpTarget {
            tree_root_pid: 100,
            images_dir: PathBuf::from("/ckpt/1/images"),
            log_file: PathBuf::from("/ckpt/1/dump.log"),
            leave_running: false,
        }
    }

    pub(crate) fn restore_target() -> RestoreTarget {
        RestoreTarget {
            images_dir: PathBuf::from("/ckpt/1/images"),
            log_file: PathBuf::from("/ckpt/1/restore.log"),
            detached: true,
            root: Some(PathBuf::from("/run/puzuz/root")),
            pidfile: Some(PathBuf::from("/ckpt/1/restore.pid")),
            inherit_fds: vec![],
        }
    }

    fn actions(steps: &[Step]) -> Vec<Option<Action>> {
        steps.iter().map(Step::action).collect()
    }

    use Action::*;

    #[test]
    fn manual_dump_locks_all_then_checkpoints_all_then_dumps() {
        let steps = dump_plan(&cfg(CudaMode::Manual), &dump_target(), &[101, 102, 101]);
        assert_eq!(
            steps[0],
            Step::EnsureRunnable {
                pids: vec![100, 101, 102]
            }
        );
        assert_eq!(
            actions(&steps),
            [
                None,
                Some(CudaLock { pid: 101 }),
                Some(CudaLock { pid: 102 }),
                Some(CudaCheckpoint { pid: 101 }),
                Some(CudaCheckpoint { pid: 102 }),
                Some(CriuDump { root_pid: 100 }),
            ]
        );
        let Step::Run {
            command, timeout, ..
        } = &steps[1]
        else {
            panic!()
        };
        assert_eq!(
            command.args_lossy(),
            ["--action", "lock", "--pid", "101", "--timeout", "30000"]
        );
        assert_eq!(*timeout, Duration::from_secs(45));
        let Step::Run {
            command, timeout, ..
        } = steps.last().unwrap()
        else {
            panic!()
        };
        assert!(
            command.has_arg("-L"),
            "manual mode disables the CUDA plugin"
        );
        assert_eq!(*timeout, UNBOUNDED);
    }

    #[test]
    fn manual_dump_of_cpu_only_job() {
        let steps = dump_plan(&cfg(CudaMode::Manual), &dump_target(), &[]);
        assert_eq!(actions(&steps), [None, Some(CriuDump { root_pid: 100 })]);
    }

    #[test]
    fn plugin_dump_is_just_criu() {
        let steps = dump_plan(&cfg(CudaMode::Plugin), &dump_target(), &[100, 101]);
        assert_eq!(
            steps[0],
            Step::EnsureRunnable {
                pids: vec![100, 101]
            }
        );
        assert_eq!(actions(&steps), [None, Some(CriuDump { root_pid: 100 })]);
    }

    #[test]
    fn manual_leave_running_resumes_after_dump() {
        let mut t = dump_target();
        t.leave_running = true;
        let steps = dump_plan(&cfg(CudaMode::Manual), &t, &[101, 102]);
        assert_eq!(
            actions(&steps)[5..],
            [
                Some(CriuDump { root_pid: 100 }),
                None,
                Some(CudaRestore { pid: 101 }),
                Some(CudaRestore { pid: 102 }),
                Some(CudaUnlock { pid: 101 }),
                Some(CudaUnlock { pid: 102 }),
            ]
        );
    }

    #[test]
    fn lock_without_timeout_uses_fallback_outer_timeout() {
        let mut c = cfg(CudaMode::Manual);
        c.cuda.lock_timeout = None;
        let steps = dump_plan(&c, &dump_target(), &[101]);
        let Step::Run {
            timeout, command, ..
        } = &steps[1]
        else {
            panic!()
        };
        assert_eq!(*timeout, Duration::from_secs(120));
        assert!(!command.has_arg("--timeout"));
    }

    #[test]
    fn restore_plans() {
        let c = cfg(CudaMode::Manual);
        let criu = restore_criu_plan(&c, &restore_target()).unwrap();
        assert_eq!(actions(&criu), [Some(CriuRestore)]);

        let map = DeviceMap::new([("GPU-a", "GPU-b")]).unwrap();
        let cuda = restore_cuda_plan(&c, &[201, 202], Some(&map));
        assert_eq!(
            cuda[0],
            Step::EnsureRunnable {
                pids: vec![201, 202]
            }
        );
        assert_eq!(
            actions(&cuda),
            [
                None,
                Some(CudaRestore { pid: 201 }),
                Some(CudaRestore { pid: 202 }),
                Some(CudaUnlock { pid: 201 }),
                Some(CudaUnlock { pid: 202 }),
            ]
        );
        let Step::Run { command, .. } = &cuda[1] else {
            panic!()
        };
        assert!(command.has_arg("--device-map"));
        assert!(restore_cuda_plan(&c, &[], None).is_empty());
    }

    #[test]
    fn plugin_restore_has_no_cuda_steps_and_may_be_attached() {
        let c = cfg(CudaMode::Plugin);
        let mut t = restore_target();
        t.detached = false;
        assert_eq!(
            actions(&restore_criu_plan(&c, &t).unwrap()),
            [Some(CriuRestore)]
        );
        assert!(restore_cuda_plan(&c, &[201], None).is_empty());
    }

    #[test]
    fn manual_restore_requires_detached() {
        let mut t = restore_target();
        t.detached = false;
        assert_eq!(
            restore_criu_plan(&cfg(CudaMode::Manual), &t),
            Err(PlanError::ManualRequiresDetached)
        );
    }

    #[test]
    fn rollback_after_criu_dump_failure() {
        let c = cfg(CudaMode::Manual);
        let done = [
            CudaLock { pid: 1 },
            CudaLock { pid: 2 },
            CudaCheckpoint { pid: 1 },
            CudaCheckpoint { pid: 2 },
        ];
        let rb = rollback_plan(&c, &done, Some(&CriuDump { root_pid: 9 }), false);
        assert_eq!(rb[0], Step::Thaw { root_pid: 9 });
        assert_eq!(rb[1], Step::EnsureRunnable { pids: vec![1, 2] });
        assert_eq!(
            actions(&rb),
            [
                None,
                None,
                Some(CudaRestore { pid: 1 }),
                Some(CudaRestore { pid: 2 }),
                Some(CudaUnlock { pid: 1 }),
                Some(CudaUnlock { pid: 2 }),
            ]
        );
    }

    #[test]
    fn rollback_after_lock_failure_unlocks_earlier_locks() {
        let c = cfg(CudaMode::Manual);
        let rb = rollback_plan(
            &c,
            &[CudaLock { pid: 1 }],
            Some(&CudaLock { pid: 2 }),
            false,
        );
        assert_eq!(actions(&rb), [None, Some(CudaUnlock { pid: 1 })]);
        assert!(rollback_plan(&c, &[], Some(&CudaLock { pid: 1 }), false).is_empty());
    }

    #[test]
    fn rollback_after_checkpoint_failure() {
        let c = cfg(CudaMode::Manual);
        let done = [
            CudaLock { pid: 1 },
            CudaLock { pid: 2 },
            CudaCheckpoint { pid: 1 },
        ];
        let rb = rollback_plan(&c, &done, Some(&CudaCheckpoint { pid: 2 }), false);
        assert_eq!(
            actions(&rb),
            [
                None,
                Some(CudaRestore { pid: 1 }),
                Some(CudaUnlock { pid: 1 }),
                Some(CudaUnlock { pid: 2 }),
            ]
        );
    }

    #[test]
    fn rollback_after_resume_failure_skips_broken_pid() {
        let c = cfg(CudaMode::Manual);
        let done = [
            CudaLock { pid: 1 },
            CudaLock { pid: 2 },
            CudaCheckpoint { pid: 1 },
            CudaCheckpoint { pid: 2 },
            CriuDump { root_pid: 9 },
            CudaRestore { pid: 1 },
        ];
        let rb = rollback_plan(&c, &done, Some(&CudaRestore { pid: 2 }), true);
        assert_eq!(actions(&rb), [None, Some(CudaUnlock { pid: 1 })]);
    }

    #[test]
    fn rollback_nothing_to_do() {
        let c = cfg(CudaMode::Plugin);
        assert_eq!(
            rollback_plan(&c, &[], Some(&CriuDump { root_pid: 1 }), true),
            [Step::Thaw { root_pid: 1 }],
            "a killed dump is followed by a thaw even without CUDA"
        );
        let done = [CudaLock { pid: 1 }, CudaUnlock { pid: 1 }];
        assert!(rollback_plan(&c, &done, None, false).is_empty());
    }

    #[test]
    fn rollback_after_killed_cuda_steps_assumes_the_worst() {
        let c = cfg(CudaMode::Manual);
        // A killed lock may have locked: unlock it too.
        let rb = rollback_plan(&c, &[CudaLock { pid: 1 }], Some(&CudaLock { pid: 2 }), true);
        assert_eq!(
            actions(&rb),
            [
                None,
                Some(CudaUnlock { pid: 1 }),
                Some(CudaUnlock { pid: 2 })
            ]
        );
        // A killed checkpoint may have checkpointed: restore, then unlock.
        let done = [CudaLock { pid: 1 }, CudaLock { pid: 2 }];
        let rb = rollback_plan(&c, &done, Some(&CudaCheckpoint { pid: 1 }), true);
        assert_eq!(
            actions(&rb),
            [
                None,
                Some(CudaRestore { pid: 1 }),
                Some(CudaUnlock { pid: 1 }),
                Some(CudaUnlock { pid: 2 }),
            ]
        );
    }

    #[test]
    fn operation_deadline_scales_with_size() {
        let t = Timeouts::default();
        assert_eq!(t.operation_deadline(0), Duration::from_secs(180));
        assert_eq!(t.operation_deadline(1), Duration::from_secs(210));
        assert_eq!(t.operation_deadline(1 << 30), Duration::from_secs(210));
        assert_eq!(
            t.operation_deadline((1 << 30) + 1),
            Duration::from_secs(240)
        );
        assert_eq!(t.operation_deadline(24 << 30), Duration::from_secs(900));
        assert_eq!(t.operation_deadline(u64::MAX), UNBOUNDED);
        assert_eq!(t.rollback_budget(0), Duration::from_secs(5));
        assert_eq!(t.rollback_budget(2), Duration::from_secs(5 + 2 * 420));
    }

    #[test]
    fn plugin_dump_rollback_restores_then_unlocks_by_state() {
        assert!(plugin_dump_rollback(&cfg(CudaMode::Manual), &[1]).is_empty());
        let c = cfg(CudaMode::Plugin);
        assert!(plugin_dump_rollback(&c, &[]).is_empty());
        let rb = plugin_dump_rollback(&c, &[1, 2, 1]);
        assert_eq!(rb[0], Step::EnsureRunnable { pids: vec![1, 2] });
        let conds: Vec<_> = rb[1..]
            .iter()
            .map(|s| match s {
                Step::IfCudaState {
                    pid,
                    get_state,
                    when,
                    then,
                } => {
                    let p = pid.to_string();
                    assert_eq!(get_state.args_lossy(), ["--get-state", "--pid", p.as_str()]);
                    (*pid, *when, then.action().unwrap())
                }
                s => panic!("unexpected {s:?}"),
            })
            .collect();
        assert_eq!(
            conds,
            [
                (1, CudaState::Checkpointed, CudaRestore { pid: 1 }),
                (2, CudaState::Checkpointed, CudaRestore { pid: 2 }),
                (1, CudaState::Locked, CudaUnlock { pid: 1 }),
                (2, CudaState::Locked, CudaUnlock { pid: 2 }),
            ]
        );
    }

    #[test]
    fn criu_steps_are_bounded_by_the_deadline_unless_capped() {
        let mut c = cfg(CudaMode::Manual);
        let timeout = |steps: &[Step]| match steps.last() {
            Some(Step::Run { timeout, .. }) => *timeout,
            other => panic!("{other:?}"),
        };
        assert_eq!(timeout(&dump_plan(&c, &dump_target(), &[])), UNBOUNDED);
        assert_eq!(
            timeout(&restore_criu_plan(&c, &restore_target()).unwrap()),
            UNBOUNDED
        );
        c.timeouts.criu_dump = Some(Duration::from_secs(5));
        c.timeouts.criu_restore = Some(Duration::from_secs(6));
        assert_eq!(
            timeout(&dump_plan(&c, &dump_target(), &[])),
            Duration::from_secs(5)
        );
        assert_eq!(
            timeout(&restore_criu_plan(&c, &restore_target()).unwrap()),
            Duration::from_secs(6)
        );
    }
}
