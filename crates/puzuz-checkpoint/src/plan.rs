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
//!   PID is unlocked.

use std::time::Duration;

use crate::command::{
    self, CommandSpec, CriuOptions, CudaAction, CudaCheckpointOptions, CudaMode, DeviceMap,
    DumpTarget, RestoreTarget,
};

/// Outer timeouts applied by the executor (the child is killed when hit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timeouts {
    /// Added to the lock `--timeout` to get the outer timeout of a lock.
    pub cuda_lock_grace: Duration,
    /// Outer timeout of a lock when no `--timeout` is configured.
    pub cuda_lock_without_timeout: Duration,
    pub cuda_checkpoint: Duration,
    pub cuda_restore: Duration,
    pub cuda_unlock: Duration,
    pub criu_dump: Duration,
    pub criu_restore: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        // PoC: 7 GB of VRAM checkpoints in ~3.4 s and dumps in ~11.4 s; these
        // leave ample room for 16+ GB jobs on slower disks.
        Self {
            cuda_lock_grace: Duration::from_secs(15),
            cuda_lock_without_timeout: Duration::from_secs(120),
            cuda_checkpoint: Duration::from_secs(300),
            cuda_restore: Duration::from_secs(300),
            cuda_unlock: Duration::from_secs(60),
            criu_dump: Duration::from_secs(1800),
            criu_restore: Duration::from_secs(1800),
        }
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
    /// Run `command`, killing it after `timeout`.
    Run {
        action: Action,
        command: CommandSpec,
        timeout: Duration,
    },
}

impl Step {
    pub fn action(&self) -> Option<Action> {
        match self {
            Step::Run { action, .. } => Some(*action),
            Step::EnsureRunnable { .. } => None,
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
        timeout: cfg.timeouts.criu_dump,
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
        timeout: cfg.timeouts.criu_restore,
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

/// Steps that undo the CUDA actions in `completed` (successful actions, in
/// order) given that `failed` did not succeed.
///
/// - A failed lock leaves the PID running.
/// - A failed checkpoint may leave the PID locked, so it is unlocked.
/// - A failed restore is not retried (an out-of-memory restore is not
///   recoverable); the PID is left alone.
/// - A failed `criu dump` has already been rolled back by CRIU itself, so the
///   checkpointed PIDs are simply restored and unlocked.
pub fn rollback_plan(
    cfg: &CheckpointConfig,
    completed: &[Action],
    failed: Option<&Action>,
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
        Some(Action::CudaCheckpoint { pid }) => set(*pid, Implied::Locked),
        Some(Action::CudaRestore { pid }) => set(*pid, Implied::Broken),
        _ => {}
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
        return Vec::new();
    }
    let mut steps = vec![Step::EnsureRunnable {
        pids: to_unlock.clone(),
    }];
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
        assert_eq!(*timeout, Duration::from_secs(1800));
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
        let rb = rollback_plan(&c, &done, Some(&CriuDump { root_pid: 9 }));
        assert_eq!(rb[0], Step::EnsureRunnable { pids: vec![1, 2] });
        assert_eq!(
            actions(&rb),
            [
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
        let rb = rollback_plan(&c, &[CudaLock { pid: 1 }], Some(&CudaLock { pid: 2 }));
        assert_eq!(actions(&rb), [None, Some(CudaUnlock { pid: 1 })]);
        assert!(rollback_plan(&c, &[], Some(&CudaLock { pid: 1 })).is_empty());
    }

    #[test]
    fn rollback_after_checkpoint_failure() {
        let c = cfg(CudaMode::Manual);
        let done = [
            CudaLock { pid: 1 },
            CudaLock { pid: 2 },
            CudaCheckpoint { pid: 1 },
        ];
        let rb = rollback_plan(&c, &done, Some(&CudaCheckpoint { pid: 2 }));
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
        let rb = rollback_plan(&c, &done, Some(&CudaRestore { pid: 2 }));
        assert_eq!(actions(&rb), [None, Some(CudaUnlock { pid: 1 })]);
    }

    #[test]
    fn rollback_nothing_to_do() {
        let c = cfg(CudaMode::Plugin);
        assert!(rollback_plan(&c, &[], Some(&CriuDump { root_pid: 1 })).is_empty());
        let done = [CudaLock { pid: 1 }, CudaUnlock { pid: 1 }];
        assert!(rollback_plan(&c, &done, None).is_empty());
    }
}
