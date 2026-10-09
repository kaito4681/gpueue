//! Jobs as transient systemd services (`puzuz-job-<id>.service`), started
//! over D-Bus.
//!
//! Spawned job unit:
//! - `ExecStart=<helper> shim <job dir>`, `Type=exec`, `RemainAfterExit=yes`
//!   (so the exit status stays queryable until the helper releases the
//!   unit), `Before=puzuz-helper.service` (at shutdown the helper stops —
//!   and dumps — first; the job units are stopped after it).
//! - `User=`/`Group=`/`SupplementaryGroups=`: systemd drops to the owner.
//!   The shim keeps only `CAP_SYS_ADMIN` (to create the PID and mount
//!   namespaces and mount `/proc`) and `CAP_SETPCAP` (to drop its bounding
//!   set), granted as ambient capabilities; it drops them before running
//!   the job. `NoNewPrivileges=yes`, `CapabilityBoundingSet=` limited to
//!   those two.
//! - `StandardInput=null`; the shim's own output goes to the journal. The
//!   job's stdout/stderr are *not* connected with `StandardOutput=append:`
//!   because systemd opens that file as root; the shim opens the log itself
//!   after dropping its capabilities, i.e. with the owner's permissions.
//!
//! Restored job unit: `ExecStart=criu restore ... --restore-detached
//! --pidfile <file>`, `Type=forking`, `PIDFile=<file>`: once `criu` exits,
//! systemd adopts the restored init as the unit's main process.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use puzuz_protocol::helper::{ExitStatus, HelperErrorCode, JobId};
use zbus::Connection;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use super::{
    LaunchError, LaunchResult, LaunchSpec, Launcher, RestoreSpec, UnitState, parse_unit_name,
    unit_name,
};
use crate::host::children_of;
use crate::jobs::{StartFailure, StartReport, UnitKind};

const DEST: &str = "org.freedesktop.systemd1";
const MANAGER_PATH: &str = "/org/freedesktop/systemd1";
const MANAGER: &str = "org.freedesktop.systemd1.Manager";
const UNIT_IFACE: &str = "org.freedesktop.systemd1.Unit";
const SERVICE_IFACE: &str = "org.freedesktop.systemd1.Service";
const PROPS_IFACE: &str = "org.freedesktop.DBus.Properties";
const NO_SUCH_UNIT: &str = "org.freedesktop.systemd1.NoSuchUnit";

/// The unit every job unit is ordered before.
pub const HELPER_UNIT: &str = "puzuz-helper.service";

const CAP_SETPCAP: u64 = 8;
const CAP_SYS_ADMIN: u64 = 21;
/// Capabilities the shim needs, as a systemd capability mask.
pub const SHIM_CAPS: u64 = (1 << CAP_SYS_ADMIN) | (1 << CAP_SETPCAP);

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

    async fn property(
        &self,
        path: &OwnedObjectPath,
        iface: &str,
        name: &str,
    ) -> zbus::Result<OwnedValue> {
        let reply = self
            .conn
            .call_method(
                Some(DEST),
                path.as_ref(),
                Some(PROPS_IFACE),
                "Get",
                &(iface, name),
            )
            .await?;
        reply.body().deserialize()
    }

    async fn unit_path(&self, job: JobId) -> zbus::Result<Option<OwnedObjectPath>> {
        match self
            .manager_call::<_, OwnedObjectPath>("GetUnit", &(unit_name(job),))
            .await
        {
            Ok(p) => Ok(Some(p)),
            Err(e) if is_no_such_unit(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn main_pid(&self, job: JobId) -> zbus::Result<Option<u32>> {
        let Some(path) = self.unit_path(job).await? else {
            return Ok(None);
        };
        let pid: u32 = self
            .property(&path, SERVICE_IFACE, "MainPID")
            .await?
            .try_into()?;
        Ok((pid != 0).then_some(pid))
    }

    async fn start_transient(&self, job: JobId, props: Props<'_>) -> LaunchResult<()> {
        let aux: Vec<(&str, Props<'_>)> = Vec::new();
        self.manager_call::<_, OwnedObjectPath>(
            "StartTransientUnit",
            &(unit_name(job), "fail", props, aux),
        )
        .await
        .map(|_| ())
        .map_err(|e| {
            LaunchError::new(
                HelperErrorCode::SpawnFailed,
                format!("starting {}: {e}", unit_name(job)),
            )
        })
    }

    fn common_props(&self, job: JobId, description: String) -> Props<'static> {
        vec![
            ("Description", Value::from(description)),
            ("Before", Value::from(vec![HELPER_UNIT.to_owned()])),
            ("RemainAfterExit", Value::from(true)),
            ("KillMode", Value::from("control-group")),
            ("StandardInput", Value::from("null")),
            ("StandardOutput", Value::from("journal")),
            ("StandardError", Value::from("journal")),
            (
                "TimeoutStopUSec",
                Value::from(duration_usec(self.job_stop_timeout)),
            ),
            (
                "SyslogIdentifier",
                Value::from(format!("puzuz-job-{}", job.0)),
            ),
        ]
    }

    async fn wait_for_start(&self, spec: &LaunchSpec) -> LaunchResult<u32> {
        let deadline = Instant::now() + self.spawn_timeout;
        let unit = unit_name(spec.job);
        loop {
            match spec.files.read_start() {
                Some(StartReport::Started { .. }) => {
                    // The init is the only child of the shim. It exists
                    // before the shim reports success.
                    if let Some(pid) = self.init_pid(spec.job, UnitKind::Spawned).await? {
                        return Ok(pid);
                    }
                    return Err(LaunchError::new(
                        HelperErrorCode::SpawnFailed,
                        format!("{unit}: job started but its init is gone"),
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
            match self.unit_state(spec.job).await? {
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

    async fn wait_until_gone(&self, job: JobId, timeout: Duration) -> LaunchResult<()> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.unit_state(job).await? {
                UnitState::Gone | UnitState::Exited(_) => return Ok(()),
                UnitState::Running if Instant::now() >= deadline => {
                    return Err(LaunchError::internal(format!(
                        "{} did not stop within {timeout:?}",
                        unit_name(job)
                    )));
                }
                UnitState::Running => tokio::time::sleep(POLL).await,
            }
        }
    }
}

impl Launcher for SystemdLauncher {
    async fn spawn(&self, spec: &LaunchSpec) -> LaunchResult<u32> {
        let exe = self.helper_exe.to_string_lossy().into_owned();
        let dir = spec.files.dir().to_string_lossy().into_owned();
        let mut props = self.common_props(
            spec.job,
            format!("puzuz job {}: {}", spec.job.0, spec.argv.join(" ")),
        );
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
            ("User", Value::from(spec.uid.0.to_string())),
            ("Group", Value::from(spec.gid.0.to_string())),
            ("NoNewPrivileges", Value::from(true)),
            ("CapabilityBoundingSet", Value::from(SHIM_CAPS)),
            ("AmbientCapabilities", Value::from(SHIM_CAPS)),
        ]);
        if let Some(groups) = &spec.groups {
            props.push((
                "SupplementaryGroups",
                Value::from(groups.iter().map(|g| g.0.to_string()).collect::<Vec<_>>()),
            ));
        }
        self.start_transient(spec.job, props).await?;
        match self.wait_for_start(spec).await {
            Ok(pid) => Ok(pid),
            Err(e) => {
                let _ = self.release(spec.job).await;
                Err(e)
            }
        }
    }

    async fn restore(&self, spec: &RestoreSpec) -> LaunchResult<u32> {
        let program = resolve_program(&spec.command.program).ok_or_else(|| {
            LaunchError::internal(format!(
                "{} not found in PATH",
                spec.command.program.display()
            ))
        })?;
        let program = program.to_string_lossy().into_owned();
        let mut argv = vec![program.clone()];
        argv.extend(spec.command.args_lossy());
        let mut props = self.common_props(spec.job, format!("puzuz job {} (restored)", spec.job.0));
        props.extend([
            ("Type", Value::from("forking")),
            ("ExecStart", Value::from(vec![(program, argv, false)])),
            (
                "PIDFile",
                Value::from(spec.pidfile.to_string_lossy().into_owned()),
            ),
            ("TimeoutStartUSec", Value::from(duration_usec(spec.timeout))),
        ]);
        self.start_transient(spec.job, props).await?;

        let deadline = Instant::now() + spec.timeout + Duration::from_secs(10);
        loop {
            match self.unit_state(spec.job).await? {
                UnitState::Running => {
                    let path = self.unit_path(spec.job).await.map_err(dbus)?;
                    let active = match &path {
                        Some(p) => String::try_from(
                            self.property(p, UNIT_IFACE, "ActiveState")
                                .await
                                .map_err(dbus)?,
                        )
                        .map_err(|e| dbus(e.into()))?,
                        None => String::new(),
                    };
                    if active == "active"
                        && let Some(pid) = self.main_pid(spec.job).await.map_err(dbus)?
                    {
                        return Ok(pid);
                    }
                }
                state => {
                    let _ = self.release(spec.job).await;
                    return Err(LaunchError::new(
                        HelperErrorCode::Internal,
                        format!("criu restore failed ({state:?})"),
                    ));
                }
            }
            if Instant::now() >= deadline {
                let _ = self.release(spec.job).await;
                return Err(LaunchError::internal("criu restore timed out"));
            }
            tokio::time::sleep(POLL).await;
        }
    }

    async fn init_pid(&self, job: JobId, kind: UnitKind) -> LaunchResult<Option<u32>> {
        if self.unit_state(job).await? != UnitState::Running {
            return Ok(None);
        }
        let Some(main) = self.main_pid(job).await.map_err(dbus)? else {
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

    async fn unit_state(&self, job: JobId) -> LaunchResult<UnitState> {
        let Some(path) = self.unit_path(job).await.map_err(dbus)? else {
            return Ok(UnitState::Gone);
        };
        let get_str = |iface: &'static str, name: &'static str| {
            let path = path.clone();
            async move {
                let v = self.property(&path, iface, name).await?;
                Ok::<String, zbus::Error>(String::try_from(v)?)
            }
        };
        let active = get_str(UNIT_IFACE, "ActiveState").await.map_err(dbus)?;
        let sub = get_str(UNIT_IFACE, "SubState").await.map_err(dbus)?;
        let running = match active.as_str() {
            "activating" | "reloading" | "deactivating" | "refreshing" => true,
            "active" => sub != "exited",
            _ => false,
        };
        if running {
            return Ok(UnitState::Running);
        }
        let code: i32 = self
            .property(&path, SERVICE_IFACE, "ExecMainCode")
            .await
            .map_err(dbus)?
            .try_into()
            .map_err(|e: zbus::zvariant::Error| dbus(e.into()))?;
        let status: i32 = self
            .property(&path, SERVICE_IFACE, "ExecMainStatus")
            .await
            .map_err(dbus)?
            .try_into()
            .map_err(|e: zbus::zvariant::Error| dbus(e.into()))?;
        Ok(UnitState::Exited(exit_status_from_systemd(code, status)))
    }

    async fn signal(&self, job: JobId, signal: i32) -> LaunchResult<()> {
        self.manager_call::<_, ()>("KillUnit", &(unit_name(job), "all", signal))
            .await
            .map_err(|e| {
                if is_no_such_unit(&e) {
                    LaunchError::new(HelperErrorCode::JobNotFound, format!("{e}"))
                } else {
                    dbus(e)
                }
            })
    }

    async fn release(&self, job: JobId) -> LaunchResult<()> {
        match self
            .manager_call::<_, OwnedObjectPath>("StopUnit", &(unit_name(job), "replace"))
            .await
        {
            Ok(_) => {}
            Err(e) if is_no_such_unit(&e) => return Ok(()),
            Err(e) => return Err(dbus(e)),
        }
        self.wait_until_gone(job, self.job_stop_timeout + Duration::from_secs(10))
            .await?;
        // A failed unit stays loaded until reset; ignore "not loaded".
        let _ = self
            .manager_call::<_, ()>("ResetFailedUnit", &(unit_name(job),))
            .await;
        Ok(())
    }

    async fn list(&self) -> LaunchResult<Vec<JobId>> {
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
                &(states, vec!["puzuz-job-*.service".to_owned()]),
            )
            .await
            .map_err(dbus)?;
        let mut jobs: Vec<JobId> = rows.iter().filter_map(|r| parse_unit_name(&r.0)).collect();
        jobs.sort();
        jobs.dedup();
        Ok(jobs)
    }
}

fn is_no_such_unit(e: &zbus::Error) -> bool {
    matches!(e, zbus::Error::MethodError(name, _, _) if name.as_str() == NO_SUCH_UNIT)
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
    fn resolves_programs() {
        assert_eq!(
            resolve_program(Path::new("/usr/sbin/criu")),
            Some(PathBuf::from("/usr/sbin/criu"))
        );
        assert!(resolve_program(Path::new("sh")).is_some_and(|p| p.is_absolute()));
    }
}
