//! An in-memory [`Launcher`] for tests.

use std::collections::BTreeMap;
use std::sync::Mutex;

use puzuz_protocol::helper::{ExitStatus, HelperErrorCode, JobId, signal};

use super::{
    LaunchError, LaunchResult, LaunchSpec, Launcher, RestoreSpec, UnitState, parse_unit_name,
};
use crate::jobs::UnitKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeUnit {
    pub name: String,
    pub state: UnitState,
    pub pid: u32,
    pub signals: Vec<i32>,
    pub kind: UnitKind,
}

#[derive(Debug, Default)]
struct Inner {
    units: BTreeMap<String, FakeUnit>,
    next_pid: u32,
    fail_next_spawn: Option<LaunchError>,
    fail_next_restore: Option<LaunchError>,
    spawned: Vec<LaunchSpec>,
    restored: Vec<RestoreSpec>,
    system_stopping: bool,
    /// How long `spawn` and `restore` take.
    delay: Option<std::time::Duration>,
    /// Set by `stop_waiting`: launches still in their delay are abandoned.
    stopping: bool,
    /// Spawns and restores in progress, and the most seen at once.
    active: usize,
    max_active: usize,
    /// What the check of restored tasks finds.
    restore_check_problems: Vec<String>,
}

/// Units live in a map; tests drive exits with [`FakeLauncher::exit`].
#[derive(Debug, Default)]
pub struct FakeLauncher {
    inner: Mutex<Inner>,
}

impl FakeLauncher {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The most recent unit of `job`.
    pub fn unit(&self, job: JobId) -> Option<FakeUnit> {
        self.lock()
            .units
            .values()
            .filter(|u| parse_unit_name(&u.name) == Some(job))
            .max_by_key(|u| u.pid)
            .cloned()
    }

    /// Make the job's main process exit with `status`.
    pub fn exit(&self, job: JobId, status: ExitStatus) {
        if let Some(u) = self.unit(job) {
            self.lock().units.get_mut(&u.name).unwrap().state = UnitState::Exited(Some(status));
        }
    }

    pub fn fail_next_spawn(&self, err: LaunchError) {
        self.lock().fail_next_spawn = Some(err);
    }

    pub fn fail_next_restore(&self, err: LaunchError) {
        self.lock().fail_next_restore = Some(err);
    }

    /// What the check of restored tasks (the restore's action script)
    /// finds from now on; problems make the restore fail like criu would.
    pub fn set_restore_check_problems(&self, problems: Vec<String>) {
        self.lock().restore_check_problems = problems;
    }

    pub fn set_system_stopping(&self, stopping: bool) {
        self.lock().system_stopping = stopping;
    }

    /// Make `spawn` and `restore` take `delay` (and yield), so that
    /// concurrent requests overlap.
    pub fn set_delay(&self, delay: Option<std::time::Duration>) {
        self.lock().delay = delay;
    }

    /// The launch's delay; fails like the real launcher if the helper
    /// starts stopping meanwhile.
    async fn pause(&self) -> LaunchResult<()> {
        let delay = {
            let mut g = self.lock();
            g.active += 1;
            g.max_active = g.max_active.max(g.active);
            g.delay
        };
        let end = tokio::time::Instant::now() + delay.unwrap_or_default();
        let mut result = Ok(());
        while tokio::time::Instant::now() < end {
            if self.lock().stopping {
                result = Err(LaunchError::new(
                    HelperErrorCode::ShuttingDown,
                    "the helper is stopping",
                ));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        self.lock().active -= 1;
        result
    }

    /// The most spawns and restores that were in progress at once.
    pub fn max_concurrent(&self) -> usize {
        self.lock().max_active
    }

    pub fn spawned(&self) -> Vec<LaunchSpec> {
        self.lock().spawned.clone()
    }

    pub fn restored(&self) -> Vec<RestoreSpec> {
        self.lock().restored.clone()
    }

    fn start(&self, unit: &str, kind: UnitKind) -> LaunchResult<u32> {
        let mut g = self.lock();
        if g.units.contains_key(unit) {
            return Err(LaunchError::new(HelperErrorCode::Busy, "unit exists"));
        }
        g.next_pid = g.next_pid.max(10_000) + 1;
        let pid = g.next_pid;
        g.units.insert(
            unit.to_owned(),
            FakeUnit {
                name: unit.to_owned(),
                state: UnitState::Running,
                pid,
                signals: Vec::new(),
                kind,
            },
        );
        Ok(pid)
    }
}

impl Launcher for FakeLauncher {
    async fn spawn(&self, spec: &LaunchSpec) -> LaunchResult<u32> {
        self.pause().await?;
        if let Some(err) = self.lock().fail_next_spawn.take() {
            return Err(err);
        }
        self.lock().spawned.push(spec.clone());
        self.start(&spec.unit, UnitKind::Spawned)
    }

    async fn restore(&self, spec: &RestoreSpec) -> LaunchResult<u32> {
        self.pause().await?;
        if let Some(err) = self.lock().fail_next_restore.take() {
            return Err(err);
        }
        self.lock().restored.push(spec.clone());
        // Like criu, run the check of the restored tasks before resuming
        // them (`--action-script`, see `crate::access::run_restore_hook`).
        if let Some(script) = spec.command.value_of("--action-script") {
            let script = std::fs::read_to_string(script)
                .map_err(|e| LaunchError::internal(format!("reading the action script: {e}")))?;
            let request = script
                .trim_end()
                .rsplit_once(" verify-restore-hook ")
                .map(|(_, r)| r.trim_matches('\''))
                .ok_or_else(|| LaunchError::internal(format!("unknown action script {script}")))?;
            let problems = self.lock().restore_check_problems.clone();
            let reply: crate::access::VerifyReply = Ok(problems.clone());
            std::fs::write(
                crate::access::hook_result_path(std::path::Path::new(request)),
                serde_json::to_vec(&reply).map_err(|e| LaunchError::internal(e.to_string()))?,
            )
            .map_err(|e| LaunchError::internal(e.to_string()))?;
            if !problems.is_empty() {
                return Err(LaunchError::internal(format!(
                    "criu restore failed (unit failed); see journalctl -u {}",
                    spec.unit
                )));
            }
        }
        self.start(&spec.unit, UnitKind::Restored)
    }

    async fn init_pid(&self, unit: &str, _kind: UnitKind) -> LaunchResult<Option<u32>> {
        Ok(self
            .lock()
            .units
            .get(unit)
            .filter(|u| u.state == UnitState::Running)
            .map(|u| u.pid))
    }

    async fn unit_state(&self, unit: &str) -> LaunchResult<UnitState> {
        Ok(self
            .lock()
            .units
            .get(unit)
            .map_or(UnitState::Gone, |u| u.state))
    }

    async fn signal(&self, unit: &str, sig: i32) -> LaunchResult<()> {
        let mut g = self.lock();
        let u = g
            .units
            .get_mut(unit)
            .ok_or_else(|| LaunchError::new(HelperErrorCode::JobNotFound, "no unit"))?;
        u.signals.push(sig);
        if sig == signal::SIGKILL && u.state == UnitState::Running {
            u.state = UnitState::Exited(Some(ExitStatus::Signaled { signal: sig }));
        }
        Ok(())
    }

    async fn release(&self, unit: &str) -> LaunchResult<()> {
        self.lock().units.remove(unit);
        Ok(())
    }

    async fn list(&self) -> LaunchResult<Vec<String>> {
        Ok(self.lock().units.keys().cloned().collect())
    }

    async fn system_stopping(&self) -> LaunchResult<bool> {
        Ok(self.lock().system_stopping)
    }

    fn stop_waiting(&self) {
        self.lock().stopping = true;
    }
}
