//! Starting and tracking job units.
//!
//! The helper never is the parent of a job: jobs run in units managed by
//! the service manager so they survive helper and daemon restarts and keep
//! their exit status where the helper can find it again. [`Launcher`] hides
//! how that is done; [`systemd::SystemdLauncher`] is the real one and
//! [`fake::FakeLauncher`] is used in tests.
//!
//! Units are addressed by name: a job starts in `gpueue-job-<id>.service`
//! and its n-th restore runs in `gpueue-job-<id>-r<n>.service` (a dumped
//! unit ends `failed` and is reset, but a distinct name keeps the history
//! unambiguous).

pub mod fake;
pub mod systemd;

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use gpueue_checkpoint::CommandSpec;
use gpueue_protocol::helper::{ExitStatus, HelperErrorCode, JobId};

use crate::devices::DeviceRule;
use crate::jobs::{JobFiles, UnitKind};

/// Everything needed to start a job unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    pub job: JobId,
    pub unit: String,
    /// The helper binary the unit runs as `<exe> shim <job dir>` (see
    /// [`crate::helperbin`]).
    pub exe: PathBuf,
    /// The job's files; `spawn.json` (credentials, command, environment)
    /// is already written.
    pub files: JobFiles,
    /// The job's owner, for the unit description.
    pub uid: u32,
    /// For tests; the shim reads `spawn.json`.
    pub argv: Vec<String>,
    pub envs: BTreeMap<String, String>,
    /// See [`RestoreSpec::devices`].
    pub devices: Option<Vec<DeviceRule>>,
}

/// A `criu restore` to run in a fresh job unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreSpec {
    pub job: JobId,
    pub unit: String,
    /// `criu restore ... --restore-detached --pidfile <pidfile>`.
    pub command: CommandSpec,
    pub pidfile: PathBuf,
    pub timeout: Duration,
    /// Where `/` is bind-mounted (in a private mount namespace) for
    /// `criu restore --root`.
    pub criu_root: Option<PathBuf>,
    /// `Some`: the unit gets `DevicePolicy=closed` and these `DeviceAllow=`
    /// entries (see [`crate::devices`]); `None`: no device restrictions
    /// (`gpu_device_isolation = false`).
    pub devices: Option<Vec<DeviceRule>>,
}

/// What the service manager knows about a unit.
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

    /// Run `criu restore` in a new unit and wait until the tree runs.
    /// Returns the host PID of the restored init.
    fn restore(&self, spec: &RestoreSpec) -> impl Future<Output = LaunchResult<u32>> + Send;

    /// Host PID of the job's init, if the unit runs.
    fn init_pid(
        &self,
        unit: &str,
        kind: UnitKind,
    ) -> impl Future<Output = LaunchResult<Option<u32>>> + Send;

    fn unit_state(&self, unit: &str) -> impl Future<Output = LaunchResult<UnitState>> + Send;

    /// Deliver `signal` to the job. Termination-style signals go to the
    /// unit's main process, which forwards them to the job's process group;
    /// SIGSTOP/SIGCONT/SIGKILL (which cannot be forwarded) go to every
    /// process of the unit.
    fn signal(&self, unit: &str, signal: i32) -> impl Future<Output = LaunchResult<()>> + Send;

    /// Stop the unit (if still running) and forget it (`reset-failed`).
    fn release(&self, unit: &str) -> impl Future<Output = LaunchResult<()>> + Send;

    /// Names of all existing job units.
    fn list(&self) -> impl Future<Output = LaunchResult<Vec<String>>> + Send;

    /// Whether the whole system is shutting down (`systemctl
    /// is-system-running` says `stopping`), as opposed to only the helper
    /// being stopped or restarted.
    fn system_stopping(&self) -> impl Future<Output = LaunchResult<bool>> + Send;

    /// The helper is being stopped: from now on, a spawn or restore whose
    /// unit systemd has not begun to start is abandoned (its start job is
    /// cancelled) and fails with `shutting_down`. Job units are ordered
    /// before the helper, so their start jobs wait for the helper's stop
    /// job, which in turn waits for the operations in flight.
    fn stop_waiting(&self) {}
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
        unit: &str,
        kind: UnitKind,
    ) -> impl Future<Output = LaunchResult<Option<u32>>> + Send {
        (**self).init_pid(unit, kind)
    }
    fn unit_state(&self, unit: &str) -> impl Future<Output = LaunchResult<UnitState>> + Send {
        (**self).unit_state(unit)
    }
    fn signal(&self, unit: &str, signal: i32) -> impl Future<Output = LaunchResult<()>> + Send {
        (**self).signal(unit, signal)
    }
    fn release(&self, unit: &str) -> impl Future<Output = LaunchResult<()>> + Send {
        (**self).release(unit)
    }
    fn list(&self) -> impl Future<Output = LaunchResult<Vec<String>>> + Send {
        (**self).list()
    }
    fn system_stopping(&self) -> impl Future<Output = LaunchResult<bool>> + Send {
        (**self).system_stopping()
    }
    fn stop_waiting(&self) {
        (**self).stop_waiting();
    }
}

/// The unit a job is spawned in.
pub fn unit_name(job: JobId) -> String {
    format!("gpueue-job-{}.service", job.0)
}

/// The unit of the n-th restore of a job.
pub fn restore_unit_name(job: JobId, n: u32) -> String {
    format!("gpueue-job-{}-r{n}.service", job.0)
}

/// The job id of a name produced by [`unit_name`] or [`restore_unit_name`].
pub fn parse_unit_name(name: &str) -> Option<JobId> {
    let rest = name.strip_prefix("gpueue-job-")?.strip_suffix(".service")?;
    let id = match rest.split_once("-r") {
        Some((id, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => id,
        Some(_) => return None,
        None => rest,
    };
    id.parse().ok().map(JobId)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_names_round_trip() {
        assert_eq!(unit_name(JobId(42)), "gpueue-job-42.service");
        assert_eq!(restore_unit_name(JobId(42), 3), "gpueue-job-42-r3.service");
        assert_eq!(parse_unit_name("gpueue-job-42.service"), Some(JobId(42)));
        assert_eq!(parse_unit_name("gpueue-job-42-r3.service"), Some(JobId(42)));
        assert_eq!(parse_unit_name("gpueue-job-42-rx.service"), None);
        assert_eq!(parse_unit_name("gpueue-job-x.service"), None);
        assert_eq!(parse_unit_name("gpueue.service"), None);
    }
}
