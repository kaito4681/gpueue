//! Jobs as transient systemd services, started over D-Bus.
//!
//! Spawned job unit `gpueue-job-<id>.service`:
//! - `ExecStart=<helper> shim <job dir>` as root; the shim creates the PID
//!   and mount namespaces, cleans the mounts and drops to the owner itself
//!   (no `User=`; `PrivatePIDs=` is not used because it cannot clean the
//!   mount namespace). `CapabilityBoundingSet=` is limited to what the shim
//!   needs; `NoNewPrivileges=yes`.
//! - `Type=exec`, `RemainAfterExit=yes` (the exit status stays queryable
//!   until the helper releases the unit), `KillMode=mixed` (a stop sends
//!   SIGTERM to the shim, which forwards it to the job), and
//!   `Before=gpueue-helper.service` (at OS shutdown the helper stops — and
//!   dumps — first; the job units are stopped after it).
//!   `After=remote-fs.target network.target` (ordering only): the job
//!   units left at shutdown (`--no-checkpoint` jobs, failed or refused
//!   dumps) are stopped while NFS and the network are still there.
//! - `ExecStart=` runs the helper's content-addressed copy (see
//!   [`crate::helperbin`]), not the installed binary, which upgrades
//!   replace.
//! - `StandardInput=null`; the shim's own messages go to the journal. The
//!   job's stdout/stderr are *not* connected with `StandardOutput=append:`
//!   because systemd opens that file as root, following symlinks, in a
//!   directory the job's owner controls; the shim opens the log itself
//!   after dropping privileges.
//! - With `gpu_device_isolation` (the default), `DevicePolicy=closed` and a
//!   `DeviceAllow=` list of the assigned GPUs' NVIDIA device nodes (none
//!   for a CPU job; see [`crate::devices`]). Restored units get the same
//!   for the GPUs of the restore.
//!
//! Restored job unit `gpueue-job-<id>-r<n>.service`: `Type=forking`,
//! `PIDFile=`, `ExecStart=<helper> criu-wrap <root> criu restore -d
//! --pidfile ... --root <root> ...`. The wrapper bind-mounts `/` on `<root>`
//! in a private mount namespace and execs criu; once criu exits, systemd
//! adopts the restored init as the unit's main process. The CUDA part of a
//! manual-mode restore is run by the helper afterwards (not in
//! `ExecStartPost=`) so it shares the executor, timeouts and error
//! reporting of every other checkpoint step.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use gpueue_protocol::helper::{ExitStatus, HelperErrorCode, signal};
use zbus::Connection;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use super::{
    LaunchError, LaunchResult, LaunchSpec, Launcher, RestoreSpec, UnitState, parse_unit_name,
};
use crate::devices::DeviceRule;
use crate::host::children_of;
use crate::jobs::{StartFailure, StartReport, UnitKind};

const DEST: &str = "org.freedesktop.systemd1";
const MANAGER_PATH: &str = "/org/freedesktop/systemd1";
const MANAGER: &str = "org.freedesktop.systemd1.Manager";
const UNIT_IFACE: &str = "org.freedesktop.systemd1.Unit";
const SERVICE_IFACE: &str = "org.freedesktop.systemd1.Service";
const PROPS_IFACE: &str = "org.freedesktop.DBus.Properties";
const JOB_IFACE: &str = "org.freedesktop.systemd1.Job";
const NO_SUCH_UNIT: &str = "org.freedesktop.systemd1.NoSuchUnit";
/// Errors of a `Get` on a job object that no longer exists.
const JOB_GONE_ERRORS: &[&str] = &[
    "org.freedesktop.systemd1.NoSuchJob",
    "org.freedesktop.DBus.Error.UnknownObject",
    "org.freedesktop.DBus.Error.UnknownInterface",
    "org.freedesktop.DBus.Error.UnknownMethod",
    "org.freedesktop.DBus.Error.UnknownProperty",
];

/// The unit every job unit is ordered before.
pub const HELPER_UNIT: &str = "gpueue-helper.service";
/// The units every job unit is ordered after (see [`common_props`]).
pub const JOB_UNIT_AFTER: [&str; 2] = ["remote-fs.target", "network.target"];

const CAP_CHOWN: u64 = 0;
const CAP_DAC_OVERRIDE: u64 = 1;
const CAP_DAC_READ_SEARCH: u64 = 2;
const CAP_FOWNER: u64 = 3;
const CAP_KILL: u64 = 5;
const CAP_SETGID: u64 = 6;
const CAP_SETUID: u64 = 7;
const CAP_SETPCAP: u64 = 8;
const CAP_SYS_ADMIN: u64 = 21;
/// What the root shim needs before it drops everything: namespaces and
/// mounts, changing credentials, dropping the bounding set, and reading its
/// root-only job files.
pub const SHIM_CAPS: u64 = (1 << CAP_SYS_ADMIN)
    | (1 << CAP_SETUID)
    | (1 << CAP_SETGID)
    | (1 << CAP_SETPCAP)
    | (1 << CAP_DAC_OVERRIDE)
    | (1 << CAP_DAC_READ_SEARCH)
    | (1 << CAP_FOWNER)
    | (1 << CAP_CHOWN)
    | (1 << CAP_KILL);

const POLL: Duration = Duration::from_millis(50);

/// `CLD_EXITED`, `CLD_KILLED`, `CLD_DUMPED` as reported in `ExecMainCode`.
const CLD_EXITED: i32 = 1;
const CLD_KILLED: i32 = 2;
const CLD_DUMPED: i32 = 3;

#[derive(Debug, Clone)]
pub struct SystemdLauncher {
    conn: Connection,
    helper_exe: PathBuf,
    spawn_timeout: Duration,
    job_stop_timeout: Duration,
    proc_root: PathBuf,
    /// Set by [`Launcher::stop_waiting`].
    stopping: Arc<AtomicBool>,
}

type Props<'a> = Vec<(&'a str, Value<'a>)>;

impl SystemdLauncher {
    /// Connect to the system bus.
    pub async fn connect(
        helper_exe: PathBuf,
        spawn_timeout: Duration,
        job_stop_timeout: Duration,
    ) -> zbus::Result<Self> {
        Ok(Self {
            conn: Connection::system().await?,
            helper_exe,
            spawn_timeout,
            job_stop_timeout,
            proc_root: PathBuf::from("/proc"),
            stopping: Arc::new(AtomicBool::new(false)),
        })
    }

    async fn manager_call<B, R>(&self, method: &str, body: &B) -> zbus::Result<R>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
        R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
    {
        let reply = self
            .conn
            .call_method(Some(DEST), MANAGER_PATH, Some(MANAGER), method, body)
            .await?;
        reply.body().deserialize()
    }

    async fn property(&self, path: &str, iface: &str, name: &str) -> zbus::Result<OwnedValue> {
        let reply = self
            .conn
            .call_method(Some(DEST), path, Some(PROPS_IFACE), "Get", &(iface, name))
            .await?;
        reply.body().deserialize()
    }

    async fn unit_path(&self, unit: &str) -> zbus::Result<Option<OwnedObjectPath>> {
        match self
            .manager_call::<_, OwnedObjectPath>("GetUnit", &(unit,))
            .await
        {
            Ok(p) => Ok(Some(p)),
            Err(e) if is_no_such_unit(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn string_prop(
        &self,
        path: &OwnedObjectPath,
        iface: &str,
        name: &str,
    ) -> LaunchResult<String> {
        let v = self
            .property(path.as_str(), iface, name)
            .await
            .map_err(dbus)?;
        String::try_from(v).map_err(|e| dbus(e.into()))
    }

    async fn int_prop(&self, path: &OwnedObjectPath, name: &str) -> LaunchResult<i32> {
        let v = self
            .property(path.as_str(), SERVICE_IFACE, name)
            .await
            .map_err(dbus)?;
        i32::try_from(v).map_err(|e| dbus(e.into()))
    }

    async fn main_pid(&self, unit: &str) -> LaunchResult<Option<u32>> {
        let Some(path) = self.unit_path(unit).await.map_err(dbus)? else {
            return Ok(None);
        };
        let v = self
            .property(path.as_str(), SERVICE_IFACE, "MainPID")
            .await
            .map_err(dbus)?;
        let pid = u32::try_from(v).map_err(|e| dbus(e.into()))?;
        Ok((pid != 0).then_some(pid))
    }

    async fn start_transient(&self, unit: &str, props: Props<'_>) -> LaunchResult<()> {
        // A failed unit of the same name (e.g. left by a crash) blocks it.
        let _ = self
            .manager_call::<_, ()>("ResetFailedUnit", &(unit,))
            .await;
        let aux: Vec<(&str, Props<'_>)> = Vec::new();
        let job = self
            .manager_call::<_, OwnedObjectPath>("StartTransientUnit", &(unit, "fail", props, aux))
            .await
            .map_err(|e| {
                LaunchError::new(
                    HelperErrorCode::SpawnFailed,
                    format!("starting {unit}: {e}"),
                )
            })?;
        self.wait_job_dispatched(unit, &job).await
    }

    /// Waits until systemd has begun running the start job `job` of `unit`
    /// (or has already finished it).
    ///
    /// `StartTransientUnit` returns as soon as the job is queued; systemd
    /// dispatches it in a later iteration of its event loop. Until then the
    /// unit is still `inactive`, which the callers' polling would take for
    /// "already ended" (and they would then stop the unit just as it
    /// starts). Once the job is `running`, the unit is `activating` or past
    /// it.
    ///
    /// While the helper is being stopped or restarted, the start job of a
    /// job unit stays `waiting` until the helper's stop job is done (job
    /// units are ordered `Before=` the helper, and systemd runs a stop
    /// before a start whatever the direction of the ordering). Waiting for
    /// it would stall the stop until the spawn timeout, so once
    /// [`Launcher::stop_waiting`] was called a start job that is still
    /// queued is cancelled and the launch refused with `shutting_down`
    /// (nothing ran; the daemon offers the job again).
    async fn wait_job_dispatched(&self, unit: &str, job: &OwnedObjectPath) -> LaunchResult<()> {
        const JOB_POLL: Duration = Duration::from_millis(5);
        let deadline = Instant::now() + self.spawn_timeout;
        loop {
            match self.property(job.as_str(), JOB_IFACE, "State").await {
                Ok(v) => {
                    if String::try_from(v).map_err(|e| dbus(e.into()))? == "running" {
                        return Ok(());
                    }
                }
                Err(e) if is_job_gone(&e) => return Ok(()),
                Err(e) => return Err(dbus(e)),
            }
            if self.stopping.load(Ordering::SeqCst) {
                return self.cancel_start(unit, job).await;
            }
            if Instant::now() >= deadline {
                let _ = self.release(unit).await;
                return Err(LaunchError::new(
                    HelperErrorCode::SpawnFailed,
                    format!(
                        "systemd did not start {unit} within {:?}",
                        self.spawn_timeout
                    ),
                ));
            }
            tokio::time::sleep(JOB_POLL).await;
        }
    }

    /// Cancels the queued start job `job` of `unit` (see
    /// [`Self::wait_job_dispatched`]).
    async fn cancel_start(&self, unit: &str, job: &OwnedObjectPath) -> LaunchResult<()> {
        if let Err(e) = self
            .conn
            .call_method(Some(DEST), job.as_str(), Some(JOB_IFACE), "Cancel", &())
            .await
            && !is_job_gone(&e)
        {
            return Err(dbus(e));
        }
        // It may have been dispatched just before the cancel.
        if self.unit_state(unit).await? == UnitState::Running {
            return Ok(());
        }
        Err(LaunchError::new(
            HelperErrorCode::ShuttingDown,
            format!("the helper is stopping; {unit} was not started"),
        ))
    }

    fn common_props(&self, description: String) -> Props<'static> {
        common_props(description, self.job_stop_timeout)
    }

    async fn wait_for_start(&self, spec: &LaunchSpec) -> LaunchResult<u32> {
        let deadline = Instant::now() + self.spawn_timeout;
        let unit = spec.unit.as_str();
        loop {
            match spec.files.read_start() {
                Some(StartReport::Started { init_host_pid, .. }) => {
                    // The init is the shim's only child. A very short job
                    // may already be over; its exit status is in the files
                    // and the monitor reports it.
                    if let Some(pid) = self.init_pid(unit, UnitKind::Spawned).await? {
                        return Ok(pid);
                    }
                    if init_host_pid != 0 {
                        return Ok(init_host_pid);
                    }
                    return Err(LaunchError::new(
                        HelperErrorCode::SpawnFailed,
                        format!("{unit}: the job started but its init is gone"),
                    ));
                }
                Some(StartReport::Failed { kind, message }) => {
                    let code = match kind {
                        StartFailure::PermissionDenied => HelperErrorCode::PermissionDenied,
                        StartFailure::Other => HelperErrorCode::SpawnFailed,
                    };
                    return Err(LaunchError::new(code, message));
                }
                None => {}
            }
            match self.unit_state(unit).await? {
                UnitState::Running => {}
                state => {
                    // The shim may have reported just before exiting.
                    if spec.files.read_start().is_some() {
                        continue;
                    }
                    return Err(LaunchError::new(
                        HelperErrorCode::SpawnFailed,
                        format!(
                            "{unit} ended before the job started ({state:?}); see journalctl -u {unit}"
                        ),
                    ));
                }
            }
            if Instant::now() >= deadline {
                return Err(LaunchError::new(
                    HelperErrorCode::SpawnFailed,
                    format!(
                        "{unit}: the job did not start within {:?}",
                        self.spawn_timeout
                    ),
                ));
            }
            tokio::time::sleep(POLL).await;
        }
    }
}

impl Launcher for SystemdLauncher {
    async fn spawn(&self, spec: &LaunchSpec) -> LaunchResult<u32> {
        let exe = spec.exe.to_string_lossy().into_owned();
        let dir = spec.files.dir().to_string_lossy().into_owned();
        let mut props = self.common_props(format!(
            "gpueue job {}: {}",
            spec.job.0,
            spec.argv.join(" ")
        ));
        props.extend(device_props(spec.devices.as_deref()));
        props.extend([
            ("Type", Value::from("exec")),
            (
                "ExecStart",
                Value::from(vec![(
                    exe.clone(),
                    vec![exe, "shim".to_owned(), dir],
                    false,
                )]),
            ),
            ("CapabilityBoundingSet", Value::from(SHIM_CAPS)),
        ]);
        self.start_transient(&spec.unit, props).await?;
        match self.wait_for_start(spec).await {
            Ok(pid) => Ok(pid),
            Err(e) => {
                let _ = self.release(&spec.unit).await;
                Err(e)
            }
        }
    }

    async fn restore(&self, spec: &RestoreSpec) -> LaunchResult<u32> {
        let criu = resolve_program(&spec.command.program).ok_or_else(|| {
            LaunchError::internal(format!(
                "{} not found in PATH",
                spec.command.program.display()
            ))
        })?;
        let mut argv = vec![
            self.helper_exe.to_string_lossy().into_owned(),
            "criu-wrap".to_owned(),
        ];
        argv.push(
            spec.criu_root
                .as_ref()
                .map_or_else(String::new, |r| r.to_string_lossy().into_owned()),
        );
        argv.push(criu.to_string_lossy().into_owned());
        argv.extend(spec.command.args_lossy());
        let mut props = self.common_props(format!("gpueue job {} (restored)", spec.job.0));
        props.extend(device_props(spec.devices.as_deref()));
        props.extend([
            ("Type", Value::from("forking")),
            (
                "ExecStart",
                Value::from(vec![(argv[0].clone(), argv, false)]),
            ),
            (
                "PIDFile",
                Value::from(spec.pidfile.to_string_lossy().into_owned()),
            ),
            ("TimeoutStartUSec", Value::from(duration_usec(spec.timeout))),
        ]);
        self.start_transient(&spec.unit, props).await?;

        let deadline = Instant::now() + spec.timeout + Duration::from_secs(10);
        loop {
            let path = self.unit_path(&spec.unit).await.map_err(dbus)?;
            let active = match &path {
                Some(p) => self.string_prop(p, UNIT_IFACE, "ActiveState").await?,
                None => "gone".to_owned(),
            };
            match active.as_str() {
                "active" => {
                    if let Some(pid) = self.main_pid(&spec.unit).await? {
                        return Ok(pid);
                    }
                }
                "activating" | "reloading" => {}
                other => {
                    let _ = self.release(&spec.unit).await;
                    return Err(LaunchError::internal(format!(
                        "criu restore failed (unit {other}); see journalctl -u {}",
                        spec.unit
                    )));
                }
            }
            if Instant::now() >= deadline {
                let _ = self.release(&spec.unit).await;
                return Err(LaunchError::internal("criu restore timed out"));
            }
            tokio::time::sleep(POLL).await;
        }
    }

    async fn init_pid(&self, unit: &str, kind: UnitKind) -> LaunchResult<Option<u32>> {
        if self.unit_state(unit).await? != UnitState::Running {
            return Ok(None);
        }
        let Some(main) = self.main_pid(unit).await? else {
            return Ok(None);
        };
        match kind {
            UnitKind::Restored => Ok(Some(main)),
            UnitKind::Spawned => Ok(children_of(&self.proc_root, main)
                .map_err(|e| LaunchError::internal(format!("scanning /proc: {e}")))?
                .first()
                .copied()),
        }
    }

    async fn unit_state(&self, unit: &str) -> LaunchResult<UnitState> {
        let Some(path) = self.unit_path(unit).await.map_err(dbus)? else {
            return Ok(UnitState::Gone);
        };
        let active = self.string_prop(&path, UNIT_IFACE, "ActiveState").await?;
        let sub = self.string_prop(&path, UNIT_IFACE, "SubState").await?;
        let running = match active.as_str() {
            "activating" | "reloading" | "deactivating" | "refreshing" => true,
            "active" => sub != "exited",
            _ => false,
        };
        if running {
            return Ok(UnitState::Running);
        }
        let code = self.int_prop(&path, "ExecMainCode").await?;
        let status = self.int_prop(&path, "ExecMainStatus").await?;
        Ok(UnitState::Exited(exit_status_from_systemd(code, status)))
    }

    async fn signal(&self, unit: &str, sig: i32) -> LaunchResult<()> {
        let whom = signal_target(sig);
        self.manager_call::<_, ()>("KillUnit", &(unit, whom, sig))
            .await
            .map_err(|e| {
                if is_no_such_unit(&e) {
                    LaunchError::new(HelperErrorCode::JobNotFound, format!("{e}"))
                } else {
                    dbus(e)
                }
            })
    }

    async fn release(&self, unit: &str) -> LaunchResult<()> {
        match self
            .manager_call::<_, OwnedObjectPath>("StopUnit", &(unit, "replace"))
            .await
        {
            Ok(_) => {}
            Err(e) if is_no_such_unit(&e) => return Ok(()),
            Err(e) => return Err(dbus(e)),
        }
        let deadline = Instant::now() + self.job_stop_timeout + Duration::from_secs(10);
        loop {
            match self.unit_state(unit).await? {
                UnitState::Gone | UnitState::Exited(_) => break,
                UnitState::Running if Instant::now() >= deadline => {
                    return Err(LaunchError::internal(format!(
                        "{unit} did not stop within {:?}",
                        self.job_stop_timeout
                    )));
                }
                UnitState::Running => tokio::time::sleep(POLL).await,
            }
        }
        // A dumped unit ends `failed` (137); forget it.
        let _ = self
            .manager_call::<_, ()>("ResetFailedUnit", &(unit,))
            .await;
        Ok(())
    }

    async fn list(&self) -> LaunchResult<Vec<String>> {
        type UnitRow = (
            String,
            String,
            String,
            String,
            String,
            String,
            OwnedObjectPath,
            u32,
            String,
            OwnedObjectPath,
        );
        let states: Vec<String> = Vec::new();
        let rows: Vec<UnitRow> = self
            .manager_call(
                "ListUnitsByPatterns",
                &(states, vec!["gpueue-job-*.service".to_owned()]),
            )
            .await
            .map_err(dbus)?;
        let mut units: Vec<String> = rows
            .into_iter()
            .map(|r| r.0)
            .filter(|n| parse_unit_name(n).is_some())
            .collect();
        units.sort();
        units.dedup();
        Ok(units)
    }

    async fn system_stopping(&self) -> LaunchResult<bool> {
        let v = self
            .property(MANAGER_PATH, MANAGER, "SystemState")
            .await
            .map_err(dbus)?;
        let state = String::try_from(v).map_err(|e| dbus(e.into()))?;
        Ok(state == "stopping")
    }

    fn stop_waiting(&self) {
        self.stopping.store(true, Ordering::SeqCst);
    }
}

/// What every job unit (spawned or restored) gets. `Before=` the helper:
/// at OS shutdown the helper stops (and dumps) first. `After=` the remote
/// filesystems and the network (ordering only, nothing is pulled in): the
/// job units still running then (`--no-checkpoint` jobs, and jobs whose
/// dump was refused or failed) are stopped before NFS is unmounted and the
/// network goes down, so a job that saves its state there on SIGTERM can.
pub fn common_props(description: String, job_stop_timeout: Duration) -> Props<'static> {
    vec![
        ("Description", Value::from(description)),
        ("Before", Value::from(vec![HELPER_UNIT.to_owned()])),
        (
            "After",
            Value::from(
                JOB_UNIT_AFTER
                    .iter()
                    .map(|u| (*u).to_owned())
                    .collect::<Vec<_>>(),
            ),
        ),
        ("RemainAfterExit", Value::from(true)),
        ("KillMode", Value::from("mixed")),
        ("NoNewPrivileges", Value::from(true)),
        ("StandardInput", Value::from("null")),
        ("StandardOutput", Value::from("journal")),
        ("StandardError", Value::from("journal")),
        (
            "TimeoutStopUSec",
            Value::from(duration_usec(job_stop_timeout)),
        ),
    ]
}

/// `DevicePolicy=closed` with `DeviceAllow=` of `devices` (an array of
/// `(path, access)` on D-Bus); nothing without isolation.
pub fn device_props(devices: Option<&[DeviceRule]>) -> Props<'static> {
    let Some(devices) = devices else {
        return Vec::new();
    };
    vec![
        ("DevicePolicy", Value::from("closed")),
        ("DeviceAllow", Value::from(devices.to_vec())),
    ]
}

/// `KillUnit`'s `whom`: signals the shim forwards (see
/// [`crate::shim::FORWARDED`]) go to the main process only; those it cannot
/// (STOP, CONT, KILL) go to every process. The helper refuses all others
/// before they get here.
pub fn signal_target(sig: i32) -> &'static str {
    match sig {
        signal::SIGSTOP | signal::SIGCONT | signal::SIGKILL => "all",
        _ => "main",
    }
}

fn is_no_such_unit(e: &zbus::Error) -> bool {
    matches!(e, zbus::Error::MethodError(name, _, _) if name.as_str() == NO_SUCH_UNIT)
}

fn is_job_gone(e: &zbus::Error) -> bool {
    matches!(e, zbus::Error::MethodError(name, _, _) if JOB_GONE_ERRORS.contains(&name.as_str()))
}

fn dbus(e: zbus::Error) -> LaunchError {
    LaunchError::internal(format!("systemd D-Bus call failed: {e}"))
}

fn duration_usec(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

/// `ExecMainCode`/`ExecMainStatus` to an exit status.
pub fn exit_status_from_systemd(code: i32, status: i32) -> Option<ExitStatus> {
    match code {
        CLD_EXITED => Some(ExitStatus::Exited { code: status }),
        CLD_KILLED | CLD_DUMPED => Some(ExitStatus::Signaled { signal: status }),
        _ => None,
    }
}

/// Absolute path of `program`, searching `PATH` for bare names (transient
/// units need an absolute `ExecStart`).
pub fn resolve_program(program: &Path) -> Option<PathBuf> {
    if program.is_absolute() {
        return Some(program.to_owned());
    }
    if program.components().count() > 1 {
        return std::env::current_dir().ok().map(|d| d.join(program));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(program))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_systemd_exit_codes() {
        assert_eq!(
            exit_status_from_systemd(1, 3),
            Some(ExitStatus::Exited { code: 3 })
        );
        assert_eq!(
            exit_status_from_systemd(2, 9),
            Some(ExitStatus::Signaled { signal: 9 })
        );
        assert_eq!(exit_status_from_systemd(0, 0), None);
    }

    #[test]
    fn routes_signals() {
        assert_eq!(signal_target(signal::SIGTERM), "main");
        assert_eq!(signal_target(signal::SIGINT), "main");
        assert_eq!(signal_target(signal::SIGSTOP), "all");
        assert_eq!(signal_target(signal::SIGKILL), "all");
        // TSTP and ALRM are forwarded by the shim, which must not get them
        // itself: TSTP would stop it, ALRM end it.
        assert_eq!(signal_target(20), "main");
        assert!(crate::shim::is_forwardable(20));
        assert!(crate::shim::is_forwardable(14));
        assert!(crate::shim::is_forwardable(28)); // WINCH
        assert!(!crate::shim::is_forwardable(signal::SIGKILL));
        assert!(!crate::shim::is_forwardable(signal::SIGSTOP));
        assert!(!crate::shim::is_forwardable(17)); // CHLD
        assert!(!crate::shim::is_forwardable(11)); // SEGV
        assert!(!crate::shim::is_forwardable(34)); // SIGRTMIN
    }

    #[test]
    fn job_units_are_ordered_after_the_network_and_remote_filesystems() {
        let props = common_props("job".into(), Duration::from_secs(5));
        let get = |name: &str| {
            props
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| panic!("no {name}"))
        };
        assert_eq!(
            get("After"),
            Value::from(vec![
                "remote-fs.target".to_owned(),
                "network.target".to_owned()
            ])
        );
        assert_eq!(get("After").value_signature().to_string(), "as");
        assert_eq!(
            get("Before"),
            Value::from(vec!["gpueue-helper.service".to_owned()])
        );
        // Ordering only: nothing is pulled in.
        for dep in ["Requires", "Wants", "BindsTo", "Requisite"] {
            assert!(props.iter().all(|(n, _)| *n != dep), "{dep} is set");
        }
        assert_eq!(get("TimeoutStopUSec"), Value::from(5_000_000u64));
    }

    #[test]
    fn device_properties() {
        assert!(device_props(None).is_empty());
        let rules = vec![
            ("/dev/nvidia1".to_owned(), "rw".to_owned()),
            ("/dev/nvidiactl".to_owned(), "rw".to_owned()),
        ];
        let props = device_props(Some(&rules));
        assert_eq!(props.len(), 2);
        assert_eq!(props[0], ("DevicePolicy", Value::from("closed")));
        assert_eq!(props[1].0, "DeviceAllow");
        assert_eq!(props[1].1.value_signature().to_string(), "a(ss)");
        assert_eq!(props[1].1, Value::from(rules));
        // A CPU job: closed, nothing allowed beyond systemd's pseudo devices.
        let props = device_props(Some(&[]));
        assert_eq!(props[0], ("DevicePolicy", Value::from("closed")));
        assert_eq!(props[1].1.value_signature().to_string(), "a(ss)");
    }

    #[test]
    fn resolves_programs() {
        assert_eq!(
            resolve_program(Path::new("/usr/sbin/criu")),
            Some(PathBuf::from("/usr/sbin/criu"))
        );
        assert!(resolve_program(Path::new("sh")).is_some_and(|p| p.is_absolute()));
    }
}
