//! Starting and tracking job units.
//!
//! The helper never is the parent of a job: jobs run in units managed by
//! the service manager so they survive helper and daemon restarts and keep
//! their exit status where the helper can find it again. [`Launcher`] hides
//! how that is done; [`systemd::SystemdLauncher`] is the real one and
//! [`fake::FakeLauncher`] is used in tests. Another mechanism (for example
//! systemd's own `PrivatePIDs=` once it can be relied on, or a container
//! runtime) can be added as a further implementation.

pub mod fake;
pub mod systemd;

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use puzuz_checkpoint::CommandSpec;
use puzuz_protocol::helper::{ExitStatus, Gid, HelperErrorCode, JobId, Uid};

use crate::jobs::{JobFiles, UnitKind};

/// Everything needed to start a job unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    pub job: JobId,
    pub uid: Uid,
    pub gid: Gid,
    /// `None`: the service manager looks the groups up for `uid`.
    pub groups: Option<Vec<Gid>>,
    /// The job's files; `spawn.json` is already written.
    pub files: JobFiles,
    /// Only for logging/description; the shim reads the real input from
    /// `spawn.json`.
    pub argv: Vec<String>,
    pub envs: BTreeMap<String, String>,
}

/// A `criu restore` to run in a fresh job unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreSpec {
    pub job: JobId,
    /// `criu restore ... --restore-detached --pidfile <pidfile>`.
    pub command: CommandSpec,
    pub pidfile: PathBuf,
    pub timeout: Duration,
}

/// What the service manager knows about a job's unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitState {
    /// The unit's main process is alive.
    Running,
    /// The main process ended; the status is the service manager's view
    /// (`None` if unknown).
    Exited(Option<ExitStatus>),
    /// No such unit.
    Gone,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct LaunchError {
    pub code: HelperErrorCode,
    pub message: String,
}

impl LaunchError {
    pub fn new(code: HelperErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(HelperErrorCode::Internal, message)
    }
}

pub type LaunchResult<T> = Result<T, LaunchError>;

/// Starts, signals and tracks job units.
pub trait Launcher: Send + Sync + 'static {
    /// Start a job and wait until its shim reports that it runs. Returns
    /// the host PID of the job's PID-namespace init (the dump root).
    fn spawn(&self, spec: &LaunchSpec) -> impl Future<Output = LaunchResult<u32>> + Send;

    /// Run `criu restore` in a new unit for the job and wait until the tree
    /// runs. Returns the host PID of the restored init.
    fn restore(&self, spec: &RestoreSpec) -> impl Future<Output = LaunchResult<u32>> + Send;

    /// Host PID of the job's init, if its unit runs.
    fn init_pid(
        &self,
        job: JobId,
        kind: UnitKind,
    ) -> impl Future<Output = LaunchResult<Option<u32>>> + Send;

    fn unit_state(&self, job: JobId) -> impl Future<Output = LaunchResult<UnitState>> + Send;

    /// Send `signal` to every process of the job.
    fn signal(&self, job: JobId, signal: i32) -> impl Future<Output = LaunchResult<()>> + Send;

    /// Stop the unit (if still running) and forget it.
    fn release(&self, job: JobId) -> impl Future<Output = LaunchResult<()>> + Send;

    /// Jobs that currently have a unit.
    fn list(&self) -> impl Future<Output = LaunchResult<Vec<JobId>>> + Send;
}

impl<T: Launcher> Launcher for std::sync::Arc<T> {
    fn spawn(&self, spec: &LaunchSpec) -> impl Future<Output = LaunchResult<u32>> + Send {
        (**self).spawn(spec)
    }
    fn restore(&self, spec: &RestoreSpec) -> impl Future<Output = LaunchResult<u32>> + Send {
        (**self).restore(spec)
    }
    fn init_pid(
        &self,
        job: JobId,
        kind: UnitKind,
    ) -> impl Future<Output = LaunchResult<Option<u32>>> + Send {
        (**self).init_pid(job, kind)
    }
    fn unit_state(&self, job: JobId) -> impl Future<Output = LaunchResult<UnitState>> + Send {
        (**self).unit_state(job)
    }
    fn signal(&self, job: JobId, signal: i32) -> impl Future<Output = LaunchResult<()>> + Send {
        (**self).signal(job, signal)
    }
    fn release(&self, job: JobId) -> impl Future<Output = LaunchResult<()>> + Send {
        (**self).release(job)
    }
    fn list(&self) -> impl Future<Output = LaunchResult<Vec<JobId>>> + Send {
        (**self).list()
    }
}

/// The unit name of a job.
pub fn unit_name(job: JobId) -> String {
    format!("puzuz-job-{}.service", job.0)
}

/// The job id of a unit name produced by [`unit_name`].
pub fn parse_unit_name(name: &str) -> Option<JobId> {
    name.strip_prefix("puzuz-job-")?
        .strip_suffix(".service")?
        .parse()
        .ok()
        .map(JobId)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_names_round_trip() {
        assert_eq!(unit_name(JobId(42)), "puzuz-job-42.service");
        assert_eq!(parse_unit_name("puzuz-job-42.service"), Some(JobId(42)));
        assert_eq!(parse_unit_name("puzuz-job-x.service"), None);
        assert_eq!(parse_unit_name("puzuz.service"), None);
    }
}
