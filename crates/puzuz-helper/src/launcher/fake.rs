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

    pub fn set_system_stopping(&self, stopping: bool) {
        self.lock().system_stopping = stopping;
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
        if let Some(err) = self.lock().fail_next_spawn.take() {
            return Err(err);
        }
        self.lock().spawned.push(spec.clone());
        self.start(&spec.unit, UnitKind::Spawned)
    }

    async fn restore(&self, spec: &RestoreSpec) -> LaunchResult<u32> {
        if let Some(err) = self.lock().fail_next_restore.take() {
            return Err(err);
        }
        self.lock().restored.push(spec.clone());
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
}
