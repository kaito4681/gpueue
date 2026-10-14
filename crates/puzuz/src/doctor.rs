//! `puzuz doctor`: checks the installation and prints how to fix what is missing.

use std::collections::BTreeSet;
use std::fmt;
use std::io::{self, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use puzuz_checkpoint::command::{CUDA_PLUGIN_FILE, DEFAULT_CRIU_PLUGIN_DIR};
use puzuz_checkpoint::probe::{Version, parse_criu_version, parse_cuda_checkpoint_help};
use puzuz_daemon::config::{Config, CudaCheckpointMode, DEFAULT_CONFIG_PATH, GpuDetection};
use puzuz_protocol::api::{CheckpointStorage, GpuDetectionStatus, GpuSource, VersionResponse};

use crate::install_method::{self, Detection, InstallMethod, LocalUnit, Method, OsRelease};
use crate::service::{ADMIN_GROUP, DAEMON_UNIT, HELPER_UNIT, TMPFILES_PATH, UNIT_DIR};
use crate::system::{
    Program, System, check_root_owned_chain, find_runnable_program, is_root_owned_and_protected,
};

mod sudo_path;

/// Oldest CRIU that works with the helper (restoring into systemd units, CUDA support).
const MIN_CRIU: (u32, u32, u32) = (4, 2, 0);
/// `Type=exec` in the units.
const MIN_SYSTEMD: u32 = 240;
/// clone3() with set_tid, which CRIU uses to restore PIDs.
const CLONE3_SET_TID_KERNEL: (u32, u32, u32) = (5, 5, 0);
/// Below this much free space for checkpoint images, warn.
const LOW_SPACE_BYTES: u64 = 20 << 30;
/// Applies a change to the NixOS configuration (`services.puzuz`).
const REBUILD: &str = "sudo nixos-rebuild switch";
/// What enables the NixOS module; it installs the units, the configuration file and the groups.
const ENABLE_MODULE: &str = "set `services.puzuz.enable = true` in your NixOS configuration and run `sudo nixos-rebuild \
     switch`";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Info,
    Ok,
    Warn,
    Fail,
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Level::Info => "INFO",
            Level::Ok => "OK",
            Level::Warn => "WARN",
            Level::Fail => "FAIL",
        })
    }
}

/// The result of one check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub level: Level,
    pub name: &'static str,
    pub message: String,
    /// What to run or change to fix it.
    pub hint: Option<String>,
}

impl Check {
    fn new(level: Level, name: &'static str, message: impl Into<String>) -> Self {
        Self {
            level,
            name,
            message: message.into(),
            hint: None,
        }
    }

    fn ok(name: &'static str, message: impl Into<String>) -> Self {
        Self::new(Level::Ok, name, message)
    }

    fn info(name: &'static str, message: impl Into<String>) -> Self {
        Self::new(Level::Info, name, message)
    }

    fn warn(name: &'static str, message: impl Into<String>) -> Self {
        Self::new(Level::Warn, name, message)
    }

    fn fail(name: &'static str, message: impl Into<String>) -> Self {
        Self::new(Level::Fail, name, message)
    }

    fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// All checks of one run.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    pub fn has_failures(&self) -> bool {
        self.checks.iter().any(|check| check.level == Level::Fail)
    }

    pub fn count(&self, level: Level) -> usize {
        self.checks.iter().filter(|c| c.level == level).count()
    }

    #[cfg(test)]
    pub fn find(&self, name: &str) -> Option<&Check> {
        self.checks.iter().find(|check| check.name == name)
    }

    pub fn print(&self, out: &mut dyn Write) -> io::Result<()> {
        for check in &self.checks {
            writeln!(
                out,
                "{:<4}  {}: {}",
                check.level.to_string(),
                check.name,
                check.message
            )?;
            if let Some(hint) = &check.hint {
                for (i, line) in hint.lines().enumerate() {
                    let label = if i == 0 { "fix:" } else { "    " };
                    writeln!(out, "      {label} {line}")?;
                }
            }
        }
        let (fails, warns) = (self.count(Level::Fail), self.count(Level::Warn));
        if fails + warns == 0 {
            writeln!(out, "\nEverything looks good.")
        } else {
            writeln!(out, "\n{fails} failed, {warns} warning(s).")
        }
    }
}

/// The configuration the daemon would use, and how it was read.
pub struct LoadedConfig {
    pub config: Config,
    pub check: Check,
    /// The file passed the helper's trust check (only root can change it), or does not exist
    /// or cannot be parsed (the defaults apply). The programs it names are run only if so.
    pub trusted: bool,
}

pub fn load_config(sys: &dyn System) -> LoadedConfig {
    load_config_on(sys, OsRelease::read(sys).is_nixos())
}

/// [`load_config`], with hints for NixOS (where the module writes the file) if `nixos`.
fn load_config_on(sys: &dyn System, nixos: bool) -> LoadedConfig {
    let path = sys.path(Path::new(DEFAULT_CONFIG_PATH));
    if !path.exists() {
        let hint = if nixos {
            format!(
                "{ENABLE_MODULE}; the module writes {DEFAULT_CONFIG_PATH} from \
                 `services.puzuz.settings`"
            )
        } else {
            "sudo puzuz service install   (creates a commented default configuration)".to_owned()
        };
        return LoadedConfig {
            config: Config::default(),
            check: Check::warn(
                "config",
                format!("{DEFAULT_CONFIG_PATH} does not exist; the defaults apply"),
            )
            .hint(hint),
            trusted: true,
        };
    }
    match Config::load(&path) {
        Ok(config) => {
            let mut check = check_config_trust(sys, nixos);
            let trusted = check.level == Level::Ok;
            if trusted {
                check = check_config_readable(sys, nixos);
            }
            LoadedConfig {
                config,
                check,
                trusted,
            }
        }
        Err(err) => {
            let hint = if nixos {
                format!(
                    "fix `services.puzuz.settings` (the module writes {DEFAULT_CONFIG_PATH} from \
                     it) and run `{REBUILD}`; the daemon does not start with it"
                )
            } else {
                format!("fix {DEFAULT_CONFIG_PATH}; the daemon does not start with it")
            };
            LoadedConfig {
                config: Config::default(),
                check: Check::fail("config", err.to_string()).hint(hint),
                trusted: true,
            }
        }
    }
}

/// On NixOS, `fix` comes after what rewrites the configuration file owned by root, mode 0644.
fn rewrite_config_first(nixos: bool, fix: String) -> String {
    if nixos {
        format!(
            "`{REBUILD}` rewrites {DEFAULT_CONFIG_PATH} from `services.puzuz.settings` (owned by \
             root, mode 0644); if that does not help: {fix}"
        )
    } else {
        fix
    }
}

/// The helper (root) reads the same file and refuses it unless only root can change it.
fn check_config_trust(sys: &dyn System, nixos: bool) -> Check {
    let file = Path::new(DEFAULT_CONFIG_PATH);
    for path in [file, file.parent().unwrap_or(file)] {
        match is_root_owned_and_protected(sys, path) {
            Ok(true) => {}
            Ok(false) => {
                return Check::fail(
                    "config",
                    format!(
                        "{} is not owned by root or is group/world-writable; \
                         the helper refuses to start with it",
                        path.display()
                    ),
                )
                .hint(rewrite_config_first(
                    nixos,
                    format!(
                        "sudo chown root:root {0}; sudo chmod go-w {0}",
                        path.display()
                    ),
                ));
            }
            Err(err) => return Check::fail("config", format!("{}: {err}", path.display())),
        }
    }
    Check::ok("config", DEFAULT_CONFIG_PATH)
}

/// The daemon runs as an unprivileged user, so it needs the file to be readable by others and
/// the directories above it to be searchable (`doctor` itself may run as root and read it
/// anyway). A file readable through its group only is reported too, as that cannot be verified
/// here.
fn check_config_readable(sys: &dyn System, nixos: bool) -> Check {
    let file = Path::new(DEFAULT_CONFIG_PATH);
    let mut problems = Vec::new();
    match std::fs::metadata(sys.path(file)) {
        Ok(meta) if meta.mode() & 0o004 == 0 => {
            problems.push(format!("{} is not readable by others", file.display()));
        }
        Ok(_) => {}
        Err(err) => return Check::fail("config", format!("{}: {err}", file.display())),
    }
    for dir in file.ancestors().skip(1) {
        if let Ok(meta) = std::fs::metadata(sys.path(dir))
            && meta.mode() & 0o001 == 0
            && sys.path(dir) != sys.root()
        {
            problems.push(format!("{} is not searchable by others", dir.display()));
        }
    }
    if problems.is_empty() {
        return Check::ok("config", DEFAULT_CONFIG_PATH);
    }
    Check::warn(
        "config",
        format!(
            "{}; the daemon (user puzuz) may be unable to read its configuration",
            problems.join(", ")
        ),
    )
    .hint(rewrite_config_first(
        nixos,
        format!(
            "sudo chmod 755 {}; sudo chmod 644 {DEFAULT_CONFIG_PATH}",
            file.parent().unwrap_or(file).display()
        ),
    ))
}

/// What the helper uses: `config.toml`, unless its unit passes flags that override it.
fn check_helper_settings(sys: &dyn System, config: &Config, nixos: bool) -> Check {
    const NAME: &str = "helper settings";
    let summary = format!(
        "cuda mode {}, criu {}, cuda-checkpoint {}, checkpoints {}, logs {}",
        config.cuda_checkpoint_mode.as_str(),
        config.criu_path.display(),
        config.cuda_checkpoint_path.display(),
        config.checkpoints_dir().display(),
        config.logs_dir().display(),
    );
    let flags = exec_start(sys, HELPER_UNIT)
        .map(|exec| flags_of(&exec.argv))
        .unwrap_or_default();
    if flags.is_empty() {
        return Check::info(NAME, format!("from {DEFAULT_CONFIG_PATH}: {summary}"));
    }
    Check::warn(
        NAME,
        format!(
            "ExecStart= of {HELPER_UNIT} passes {}, which override {DEFAULT_CONFIG_PATH} \
             (this report shows the file's values: {summary})",
            flags.join(" ")
        ),
    )
    .hint(if nixos {
        format!(
            "set the keys in `services.puzuz.settings` and remove the flags from \
             `systemd.services.puzuz-helper` in your NixOS configuration, then `{REBUILD}`"
        )
    } else {
        format!(
            "set the keys in {DEFAULT_CONFIG_PATH} and remove the flags \
             (`sudo systemctl edit {HELPER_UNIT}` or `sudo puzuz service install`)"
        )
    })
}

/// `gpu_device_isolation`: whether job units may open only their GPUs' device nodes.
fn check_gpu_isolation(config: &Config, nixos: bool) -> Check {
    const NAME: &str = "gpu isolation";
    if config.gpu_device_isolation {
        return Check::ok(
            NAME,
            "on: job units get DevicePolicy=closed and may open only their assigned GPUs' \
             /dev/nvidia* nodes (none for CPU jobs)",
        );
    }
    let how = if nixos {
        format!("set `services.puzuz.settings.gpu_device_isolation = true`, then `{REBUILD}`")
    } else {
        format!(
            "set `gpu_device_isolation = true` in {DEFAULT_CONFIG_PATH} and \
             `sudo systemctl restart {HELPER_UNIT}`"
        )
    };
    Check::info(
        NAME,
        "off (gpu_device_isolation = false): every job can open every GPU; only \
         CUDA_VISIBLE_DEVICES selects them",
    )
    .hint(format!("unless you need it off (e.g. for MIG), {how}"))
}

/// `job_system_dbus`: whether jobs may reach the system D-Bus (systemd, polkit).
fn check_job_system_dbus(config: &Config, nixos: bool) -> Check {
    const NAME: &str = "job d-bus";
    let open: Vec<&str> = [
        ("job_system_dbus", config.job_system_dbus),
        ("job_systemd_varlink", config.job_systemd_varlink),
    ]
    .into_iter()
    .filter(|(_, on)| *on)
    .map(|(key, _)| key)
    .collect();
    if open.is_empty() {
        return Check::ok(
            NAME,
            "blocked: jobs cannot reach the system D-Bus or systemd's Varlink services (no \
             systemctl, run0, pkexec or other polkit-authorized actions from jobs)",
        );
    }
    let keys = open
        .iter()
        .map(|k| format!("`{k} = true`"))
        .collect::<Vec<_>>()
        .join(" and ");
    let how = if nixos {
        format!("remove {keys} from `services.puzuz.settings`, then `{REBUILD}`")
    } else {
        format!(
            "remove {keys} from {DEFAULT_CONFIG_PATH} and \
             `sudo systemctl restart {HELPER_UNIT}`"
        )
    };
    Check::warn(
        NAME,
        format!(
            "allowed ({}): jobs can ask systemd and polkit for actions; polkit rules that \
             trust a group apply to the jobs of its members",
            open.iter()
                .map(|k| format!("{k} = true"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    )
    .hint(format!("unless jobs need it, {how}"))
}

/// The helper's job credential policy: the minimum uid and where it comes from, the denied
/// groups (and which of them exist here), the allowed users.
fn check_job_policy(sys: &dyn System, config: &Config) -> Check {
    use puzuz_config::job_policy;
    const NAME: &str = "job policy";
    let (min_uid, source) = job_policy::effective_min_uid(
        config.job_min_uid,
        &sys.path(Path::new(job_policy::LOGIN_DEFS_PATH)),
    );
    let groups = std::fs::read_to_string(sys.path(Path::new(job_policy::GROUP_FILE_PATH)))
        .unwrap_or_default();
    let present = job_policy::resolve_group_names(&groups, &config.job_denied_groups);
    let denied = if config.job_denied_groups.is_empty() {
        "none".to_owned()
    } else {
        config
            .job_denied_groups
            .iter()
            .map(|name| match present.iter().find(|(n, _)| n == name) {
                Some((_, gid)) => format!("{name} (gid {gid})"),
                None => format!("{name} (absent)"),
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let allowed = if config.job_allowed_users.is_empty() {
        format!("every user with uid >= {min_uid}")
    } else {
        config.job_allowed_users.join(", ")
    };
    Check::info(
        NAME,
        format!(
            "jobs run as uid >= {min_uid} ({}); denied groups: {denied}; allowed users: {allowed}",
            source.describe()
        ),
    )
}

/// The `--flags` (without values) of a command line.
fn flags_of(argv: &[String]) -> Vec<String> {
    argv.iter()
        .skip(1)
        .filter(|arg| arg.starts_with("--"))
        .map(|arg| arg.split('=').next().unwrap_or(arg).to_owned())
        .collect()
}

/// The command a unit starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecStart {
    /// The program, as systemd resolved it.
    pub program: PathBuf,
    /// The whole command line, `argv[0]` included.
    pub argv: Vec<String>,
}

/// The unit's effective `ExecStart=`, drop-ins included, as `systemctl show` reports it; when
/// systemd cannot tell (e.g. the unit is not loaded), the first `ExecStart=` of its unit file.
fn exec_start(sys: &dyn System, unit: &str) -> Option<ExecStart> {
    let shown = sys
        .run(
            "systemctl",
            &["show", "--property=ExecStart", "--value", unit],
        )
        .ok()
        .filter(|output| output.success)
        .and_then(|output| parse_show_exec_start(&output.stdout));
    if shown.is_some() {
        return shown;
    }
    let text = std::fs::read_to_string(sys.path(&unit_file(sys, unit))).ok()?;
    let program = exec_start_binary(&text)?;
    let argv = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("ExecStart="))
        .map(|cmd| cmd.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default();
    Some(ExecStart { program, argv })
}

/// Parses `systemctl show --property=ExecStart --value`:
/// `{ path=/usr/bin/x ; argv[]=/usr/bin/x serve --flag ; ignore_errors=no ; ... }`. With several
/// commands, the first one.
pub fn parse_show_exec_start(text: &str) -> Option<ExecStart> {
    let field = |name: &str| -> Option<&str> {
        let start = text.find(name)? + name.len();
        let rest = &text[start..];
        Some(rest[..rest.find(" ;").unwrap_or(rest.len())].trim())
    };
    let program = field("path=").filter(|path| path.starts_with('/'))?;
    let argv = field("argv[]=")
        .map(|argv| argv.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_else(|| vec![program.to_owned()]);
    Some(ExecStart {
        program: PathBuf::from(program),
        argv,
    })
}

/// Runs every check. `socket` overrides the daemon socket from the configuration.
pub fn run(sys: &dyn System, socket: Option<&Path>) -> Report {
    // Detected once, for the install method, sudo's PATH, the update hint and whether the hints
    // name the NixOS module.
    let detection = install_method::detect(sys);
    let nixos = detection.os.is_nixos();
    let LoadedConfig {
        config,
        check,
        trusted,
    } = load_config_on(sys, nixos);
    // As root, the programs an untrusted configuration names are never run.
    let programs = Programs {
        config_trusted: trusted || sys.euid() != 0,
        nixos,
    };
    let mut checks = vec![check, check_install_method(&detection)];
    checks.extend(sudo_path::check(sys, &detection));
    let api_socket = socket.map_or_else(|| config.api_socket.clone(), Path::to_owned);
    let daemon = sys.daemon_version(&api_socket);

    checks.push(check_criu(sys, &config.criu_path, programs));
    let driver = driver_version(sys);
    checks.push(check_driver(&driver, nixos));
    checks.push(check_cuda_checkpoint(
        sys,
        &config.cuda_checkpoint_path,
        driver.as_ref().ok(),
        programs,
    ));
    if config.cuda_checkpoint_mode == CudaCheckpointMode::Plugin {
        checks.push(check_cuda_plugin(sys, driver.is_ok(), nixos));
    }
    if driver.is_ok() {
        let detection = daemon.as_ref().ok().and_then(|v| v.gpu_detection.as_ref());
        checks.push(check_gpus(sys, &config, detection, programs));
    }
    checks.push(check_kernel(sys));
    checks.push(check_systemd(sys));
    for unit in [HELPER_UNIT, DAEMON_UNIT] {
        checks.push(check_unit(sys, unit, nixos));
    }
    checks.push(check_helper_binary(sys, nixos));
    checks.push(check_helper_settings(sys, &config, nixos));
    checks.push(check_gpu_isolation(&config, nixos));
    checks.push(check_job_system_dbus(&config, nixos));
    checks.push(check_job_policy(sys, &config));
    if let Some(check) = check_daemon_write_paths(sys, &config, nixos) {
        checks.push(check);
    }
    checks.push(check_socket(sys, "daemon socket", &api_socket, DAEMON_UNIT));
    checks.push(check_socket(
        sys,
        "helper socket",
        &config.helper_socket,
        HELPER_UNIT,
    ));
    checks.extend(check_daemon(&daemon));
    let storage = daemon.as_ref().ok().and_then(|v| v.checkpoints.as_ref());
    checks.push(check_checkpoint_dir(sys, &config, storage, nixos));
    if let Some(storage) = storage {
        checks.push(check_checkpoint_usage(storage, nixos));
    }
    checks.push(check_admin(sys, nixos));
    if let Some(check) = check_update(sys, &config, &detection) {
        checks.push(check);
    }
    Report { checks }
}

/// Whether a newer release exists (`update_check = false` turns this off). Never more than
/// INFO: being offline is not a problem of the installation.
fn check_update(sys: &dyn System, config: &Config, detection: &Detection) -> Option<Check> {
    const NAME: &str = "update";
    if !config.update_check {
        return None;
    }
    let current = crate::update::current_version();
    Some(match crate::update::newer_release(sys, config) {
        Ok(Some(newer)) => Check::info(
            NAME,
            format!("puzuz {newer} is available (this is {current})"),
        )
        .hint(crate::update::update_hint(detection, config, Some(&newer))),
        Ok(None) => Check::ok(NAME, format!("{current} is the latest release")),
        Err(err) => Check::info(NAME, format!("cannot check for a newer release: {err}")),
    })
}

/// How puzuz is installed; two ways at once shadow each other's units.
fn check_install_method(detection: &Detection) -> Check {
    const NAME: &str = "install method";
    let method = &detection.install;
    let check = match method {
        InstallMethod::None if detection.os.is_nixos() => {
            Check::info(NAME, "not installed (NixOS: use the services.puzuz module)")
        }
        InstallMethod::None => Check::info(NAME, "not installed"),
        InstallMethod::One(Method::Manual { unit }) if detection.os.is_nixos() => {
            Check::info(NAME, method.to_string()).hint(format!(
                "on NixOS, use the services.puzuz module instead: remove {} and {ENABLE_MODULE}",
                unit.display()
            ))
        }
        InstallMethod::One(Method::Manual { unit }) => {
            Check::info(NAME, method.to_string()).hint(format!(
                "`sudo puzuz service install --force` replaces {} with the packaged units",
                unit.display()
            ))
        }
        InstallMethod::One(Method::Package {
            local: Some(LocalUnit::Masked(_)),
            ..
        }) => Check::info(NAME, method.to_string()).hint(format!(
            "`sudo systemctl unmask {}` lets the packaged unit run again",
            crate::service::DAEMON_UNIT
        )),
        InstallMethod::One(Method::Unowned { unit, .. }) if detection.os.is_nixos() => {
            Check::warn(NAME, method.to_string()).hint(format!(
                "if it is left over, delete it (`sudo rm {}`); on NixOS, install puzuz with the \
                 services.puzuz module: {ENABLE_MODULE}",
                unit.display()
            ))
        }
        InstallMethod::One(Method::Unowned { unit, .. }) => {
            let unit = unit.display();
            Check::warn(NAME, method.to_string()).hint(format!(
                "if it is left over from a removed package, delete it (`sudo rm {unit}`); to \
                 switch to `puzuz service install`, delete it first or run `sudo puzuz service \
                 install --force`"
            ))
        }
        InstallMethod::One(_) => Check::ok(NAME, method.to_string()),
        InstallMethod::Mixed(methods) => {
            let fixes: Vec<String> = methods
                .iter()
                .map(|m| {
                    let kept = methods
                        .iter()
                        .filter(|other| *other != m)
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(" and ");
                    format!("to keep {kept}: {}", detection.removal_hint(m))
                })
                .collect();
            return Check::fail(
                NAME,
                format!("{method}; their units and data directories conflict"),
            )
            .hint(format!("remove all but one of them:\n{}", fixes.join("\n")));
        }
    };
    if check.level >= Level::Warn {
        return check;
    }
    if let Some(partial) = &detection.partial {
        if detection.is_conflicting() {
            // `service install` on one unit, a package or Nix on the other: like Mixed.
            let managed = detection
                .managed_elsewhere()
                .expect("a conflict involves a package or Nix");
            return Check::fail(
                NAME,
                format!("{partial}; their units and data directories conflict"),
            )
            .hint(format!(
                "remove all but one of them:\nto keep {managed}: run `sudo puzuz uninstall`\n\
                 to keep puzuz service install: {}",
                detection.removal_hint(managed)
            ));
        }
        let hint = match detection.managed_elsewhere() {
            Some(managed) => format!(
                "keep one installation: to keep {managed}, delete the other unit; otherwise \
                 {}",
                detection.removal_hint(managed)
            ),
            None if detection.os.is_nixos() => format!(
                "both units must come from the same installation: delete the stray unit; the \
                 services.puzuz module installs both ({ENABLE_MODULE})"
            ),
            None => "both units must come from the same installation: re-run it (e.g. `sudo \
                     puzuz service install`) or delete the stray unit"
                .to_owned(),
        };
        return Check::warn(NAME, partial.clone()).hint(hint);
    }
    if !detection.leftovers.is_empty() {
        let leftovers: Vec<String> = detection
            .leftovers
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        return Check::warn(
            NAME,
            format!(
                "{method}; no package owns {} (shadowed by the units in {UNIT_DIR})",
                leftovers.join(" and ")
            ),
        )
        .hint(format!(
            "remove the leftover /usr/lib unit: `sudo rm {}`",
            leftovers.join(" ")
        ));
    }
    check
}

/// How the checks that run a configured program may do so.
#[derive(Debug, Clone, Copy)]
struct Programs {
    /// The configuration file may name programs to run (see [`LoadedConfig::trusted`]).
    config_trusted: bool,
    nixos: bool,
}

impl Programs {
    /// Resolves the program the configuration key `key` names, or the FAIL to report instead of
    /// running it: as root, neither a program from an untrusted configuration nor one that
    /// someone else could replace is run.
    fn resolve(
        self,
        sys: &dyn System,
        name: &'static str,
        key: &str,
        configured: &Path,
    ) -> Result<Option<PathBuf>, Check> {
        if !self.config_trusted {
            return Err(Check::fail(
                name,
                format!(
                    "{} was not run: {DEFAULT_CONFIG_PATH} failed the trust check (see \"config\"), \
                     so the programs it names are not run as root",
                    configured.display()
                ),
            )
            .hint(rewrite_config_first(
                self.nixos,
                format!(
                    "make {DEFAULT_CONFIG_PATH} and {} owned by root and not group- or \
                     world-writable, then re-run `sudo puzuz doctor`",
                    Path::new(DEFAULT_CONFIG_PATH)
                        .parent()
                        .unwrap_or(Path::new("/"))
                        .display()
                ),
            )));
        }
        match find_runnable_program(sys, configured) {
            Program::Found(path) => Ok(Some(path)),
            Program::Missing => Ok(None),
            Program::Untrusted { path, reason } => Err(Check::fail(
                name,
                format!(
                    "{} was not run as root: {reason}; the helper would refuse it too",
                    path.display()
                ),
            )
            .hint(if self.nixos {
                format!(
                    "point `{key}` in `services.puzuz.settings` at a path in /nix/store (or \
                     remove it), then `{REBUILD}`"
                )
            } else {
                format!(
                    "make {} and its directories owned by root and not group- or \
                     world-writable (`sudo chown root: {0}; sudo chmod go-w {0}`), or point \
                     `{key}` in {DEFAULT_CONFIG_PATH} at a copy that is",
                    path.display()
                )
            })),
        }
    }
}

fn check_criu(sys: &dyn System, configured: &Path, programs: Programs) -> Check {
    const NAME: &str = "criu";
    let nixos = programs.nixos;
    let install = if nixos {
        format!(
            "set `services.puzuz.criuPackage` to CRIU {}.{} or newer (its default, the puzuz \
             flake's CRIU 4.2, is) and remove any `criu_path` from `services.puzuz.settings`, \
             then `{REBUILD}`",
            MIN_CRIU.0, MIN_CRIU.1
        )
    } else {
        format!(
            "install CRIU {}.{} or newer (https://criu.org/Installation), or set `criu_path` in {}",
            MIN_CRIU.0, MIN_CRIU.1, DEFAULT_CONFIG_PATH
        )
    };
    let path = match programs.resolve(sys, NAME, "criu_path", configured) {
        Ok(Some(path)) => path,
        Ok(None) => {
            return Check::fail(NAME, format!("{} not found", configured.display())).hint(install);
        }
        Err(check) => return check,
    };
    let shown = path.display();
    let output = match sys.run(&path.to_string_lossy(), &["--version"]) {
        Ok(output) => output,
        Err(err) => return Check::fail(NAME, format!("cannot run {shown}: {err}")).hint(install),
    };
    let Some(version) = parse_criu_version(&output.stdout) else {
        return Check::fail(NAME, format!("cannot parse `{shown} --version`")).hint(install);
    };
    let version = version.version;
    if !version.at_least(MIN_CRIU.0, MIN_CRIU.1, MIN_CRIU.2) {
        return Check::fail(NAME, format!("{shown} is version {version}, too old")).hint(install);
    }
    Check::ok(NAME, format!("{version} ({shown})"))
}

/// The NVIDIA kernel module's version: from /proc/driver/nvidia/version, or nvidia-smi.
fn driver_version(sys: &dyn System) -> Result<Version, String> {
    if let Ok(text) = std::fs::read_to_string(sys.path(Path::new("/proc/driver/nvidia/version"))) {
        // "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  595.58.03  Release Build ..."
        let version = text
            .lines()
            .find(|line| line.starts_with("NVRM"))
            .into_iter()
            .flat_map(str::split_whitespace)
            .filter(|token| token.contains('.'))
            .find_map(Version::parse);
        if let Some(version) = version {
            return Ok(version);
        }
    }
    let smi = match find_runnable_program(sys, Path::new("nvidia-smi")) {
        Program::Found(smi) => smi,
        Program::Missing => {
            return Err(
                "no NVIDIA driver found (no /proc/driver/nvidia/version, no nvidia-smi)".into(),
            );
        }
        Program::Untrusted { path, reason } => {
            return Err(format!(
                "no /proc/driver/nvidia/version, and {} was not run as root: {reason}",
                path.display()
            ));
        }
    };
    let output = sys
        .run(
            &smi.to_string_lossy(),
            &["--query-gpu=driver_version", "--format=csv,noheader"],
        )
        .map_err(|e| format!("cannot run nvidia-smi: {e}"))?;
    if !output.success {
        return Err(format!("nvidia-smi failed: {}", output.stdout.trim()));
    }
    output
        .stdout
        .lines()
        .find_map(Version::parse)
        .ok_or_else(|| "cannot parse the driver version from nvidia-smi".to_owned())
}

fn check_driver(driver: &Result<Version, String>, nixos: bool) -> Check {
    match driver {
        Ok(version) => Check::ok("nvidia driver", version.to_string()),
        Err(err) => Check::warn("nvidia driver", format!("{err}; only CPU jobs can run")).hint(
            if nixos {
                "enable the NVIDIA driver (r570 or newer for cuda-checkpoint) if this host has \
                 GPUs: `services.xserver.videoDrivers = [ \"nvidia\" ]` (also on headless \
                 machines) or `hardware.nvidia.datacenter.enable = true`, then `sudo \
                 nixos-rebuild switch`"
            } else {
                "install the NVIDIA driver (r570 or newer for cuda-checkpoint) if this host has GPUs"
            },
        ),
    }
}

/// `cuda_checkpoint_mode = "plugin"`: CRIU needs its CUDA plugin in its
/// built-in plugin directory (the helper passes no `-L` then).
fn check_cuda_plugin(sys: &dyn System, have_driver: bool, nixos: bool) -> Check {
    const NAME: &str = "cuda plugin";
    let path = Path::new(DEFAULT_CRIU_PLUGIN_DIR).join(CUDA_PLUGIN_FILE);
    if sys.path(&path).is_file() {
        return Check::ok(NAME, path.display().to_string());
    }
    let level = if have_driver {
        Level::Fail
    } else {
        Level::Warn
    };
    Check::new(
        level,
        NAME,
        format!(
            "cuda_checkpoint_mode is \"plugin\" but {} is missing; GPU jobs cannot be \
             checkpointed",
            path.display()
        ),
    )
    .hint(if nixos {
        format!(
            "set cuda_checkpoint_mode = \"manual\" in `services.puzuz.settings` (with \
             `services.puzuz.cudaCheckpointPackage`), then `{REBUILD}`: NixOS has no \
             {DEFAULT_CRIU_PLUGIN_DIR}"
        )
    } else {
        format!(
            "build CRIU with `make criu cuda_plugin` and install plugins/cuda/{CUDA_PLUGIN_FILE} \
             into {DEFAULT_CRIU_PLUGIN_DIR}, or set cuda_checkpoint_mode = \"manual\" in \
             {DEFAULT_CONFIG_PATH}"
        )
    })
}

fn check_cuda_checkpoint(
    sys: &dyn System,
    configured: &Path,
    driver: Option<&Version>,
    programs: Programs,
) -> Check {
    const NAME: &str = "cuda-checkpoint";
    let nixos = programs.nixos;
    // Without a driver there is nothing to checkpoint on a GPU.
    let missing = if driver.is_some() {
        Level::Fail
    } else {
        Level::Warn
    };
    let install = if nixos {
        format!(
            "set `services.puzuz.cudaCheckpointPackage` to a package with bin/cuda-checkpoint \
             matching the driver (https://github.com/NVIDIA/cuda-checkpoint; docs/install.md, \
             \"cuda-checkpoint on NixOS\", has a derivation), then `{REBUILD}`"
        )
    } else {
        format!(
            "install cuda-checkpoint matching the driver (https://github.com/NVIDIA/cuda-checkpoint) \
             into /usr/local/bin, or set `cuda_checkpoint_path` in {DEFAULT_CONFIG_PATH}"
        )
    };
    let path = match programs.resolve(sys, NAME, "cuda_checkpoint_path", configured) {
        Ok(Some(path)) => path,
        Ok(None) => {
            return Check::new(
                missing,
                NAME,
                format!(
                    "{} not found; GPU jobs cannot be checkpointed",
                    configured.display()
                ),
            )
            .hint(install);
        }
        Err(check) => return check,
    };
    let shown = path.display();
    let output = match sys.run(&path.to_string_lossy(), &["--help"]) {
        Ok(output) => output,
        Err(err) => {
            return Check::new(missing, NAME, format!("cannot run {shown}: {err}")).hint(install);
        }
    };
    // Older binaries print the help on stderr.
    let features = parse_cuda_checkpoint_help(&format!("{}\n{}", output.stdout, output.stderr));
    if !output.success && features.version.is_none() && !features.actions {
        // E.g. "error while loading shared libraries: libcuda.so.1".
        let reason = output.stderr.lines().next().unwrap_or("no output").trim();
        return Check::new(missing, NAME, format!("`{shown} --help` failed: {reason}")).hint(
            if nixos {
                "check that the NVIDIA driver is enabled (`hardware.nvidia`) and matches it, and \
                 that the package of `services.puzuz.cudaCheckpointPackage` finds libcuda.so.1 \
                 in /run/opengl-driver/lib (autoAddDriverRunpath)"
            } else {
                "check that the NVIDIA driver (libcuda.so.1) is installed and matches it"
            },
        );
    }
    let version = features
        .version
        .as_ref()
        .map_or_else(|| "unknown version".to_owned(), ToString::to_string);
    if !features.supports_manual_mode() {
        return Check::new(
            missing,
            NAME,
            format!("{shown} ({version}) lacks --action lock/checkpoint/restore/unlock"),
        )
        .hint(if nixos {
            "set `services.puzuz.cudaCheckpointPackage` to the cuda-checkpoint of driver r570 or \
             newer, then `sudo nixos-rebuild switch`"
        } else {
            "install the cuda-checkpoint of driver r570 or newer"
        });
    }
    if let (Some(tool), Some(driver)) = (&features.version, driver)
        && tool.major() != driver.major()
    {
        return Check::warn(
            NAME,
            format!("{shown} is version {tool} but the driver is {driver}"),
        )
        .hint(install);
    }
    Check::ok(NAME, format!("{version} ({shown})"))
}

/// A GPU's index and UUID, as nvidia-smi, the daemon or `[[gpus]]` have it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GpuId {
    index: u32,
    uuid: String,
}

/// How `ours` (what the daemon uses) differs from what nvidia-smi lists. An
/// empty UUID (allowed in `[[gpus]]`) only has to exist at that index.
fn gpu_differences(ours: &[GpuId], listed: &[GpuId]) -> Vec<String> {
    let indices: BTreeSet<u32> = ours.iter().chain(listed).map(|gpu| gpu.index).collect();
    let find = |gpus: &[GpuId], index: u32| gpus.iter().find(|gpu| gpu.index == index).cloned();
    indices
        .into_iter()
        .filter_map(|index| match (find(ours, index), find(listed, index)) {
            (Some(a), Some(b)) if a.uuid.is_empty() || a.uuid == b.uuid => None,
            (Some(a), Some(b)) => Some(format!(
                "GPU {index} is {} but nvidia-smi shows {}",
                a.uuid, b.uuid
            )),
            (Some(_), None) => Some(format!("GPU {index} is not in nvidia-smi")),
            (None, Some(b)) => Some(format!("GPU {index} ({}) is missing", b.uuid)),
            (None, None) => None,
        })
        .collect()
}

/// Compares the GPUs the daemon detected (`detection`, from `GET
/// /v1/version`) with nvidia-smi, and only warns about real problems: NVML
/// failing in auto mode, no GPUs while nvidia-smi sees some, or a list that
/// disagrees with nvidia-smi. Without the daemon's report, `[[gpus]]` stands
/// in for it in static mode.
fn check_gpus(
    sys: &dyn System,
    config: &Config,
    detection: Option<&GpuDetectionStatus>,
    programs: Programs,
) -> Check {
    const NAME: &str = "gpus";
    let nixos = programs.nixos;
    let smi = match programs.resolve(sys, NAME, "nvidia_smi_path", &config.nvidia_smi_path) {
        Ok(Some(smi)) => smi,
        Err(check) => return check,
        Ok(None) => {
            return Check::warn(
                NAME,
                "nvidia-smi not found; the helper needs it to find CUDA processes",
            )
            .hint(if nixos {
                "set `services.puzuz.nvidiaSmiPath` (the default, with the NVIDIA driver enabled \
             through `hardware.nvidia`, is its nvidia-smi), then `sudo nixos-rebuild switch`"
            } else {
                "install nvidia-smi (part of the NVIDIA driver utilities)"
            });
        }
    };
    let listed: Vec<GpuId> = match sys.run(
        &smi.to_string_lossy(),
        &["--query-gpu=index,uuid,name", "--format=csv,noheader"],
    ) {
        Ok(output) if output.success => output
            .stdout
            .lines()
            .filter_map(|line| {
                let mut fields = line.splitn(3, ',').map(str::trim);
                let index = fields.next()?.parse().ok()?;
                let uuid = fields.next().unwrap_or_default().to_owned();
                Some(GpuId { index, uuid })
            })
            .collect(),
        Ok(output) => {
            return Check::warn(NAME, format!("nvidia-smi failed: {}", output.stdout.trim()));
        }
        Err(err) => return Check::warn(NAME, format!("cannot run nvidia-smi: {err}")),
    };
    let configured: Vec<GpuId> = config
        .gpus
        .iter()
        .map(|gpu| GpuId {
            index: gpu.index,
            uuid: gpu.uuid.clone(),
        })
        .collect();
    let update_list = if nixos {
        format!(
            "update `gpus` in `services.puzuz.settings` \
             (`nvidia-smi --query-gpu=index,uuid,name,memory.total --format=csv`), then \
             `{REBUILD}` (it restarts {DAEMON_UNIT})"
        )
    } else {
        format!(
            "update [[gpus]] in {DEFAULT_CONFIG_PATH} \
             (`nvidia-smi --query-gpu=index,uuid,name,memory.total --format=csv`), then \
             `sudo systemctl restart {DAEMON_UNIT}`"
        )
    };

    let Some(detection) = detection else {
        // The daemon is not reachable (reported below) or predates the report.
        if config.gpu_detection == GpuDetection::Auto {
            return Check::info(
                NAME,
                format!(
                    "nvidia-smi lists {} GPU(s); the daemon did not report which GPUs it \
                     detected",
                    listed.len()
                ),
            );
        }
        if configured.is_empty() && !listed.is_empty() {
            return Check::warn(
                NAME,
                format!(
                    "nvidia-smi lists {} GPU(s) but gpu_detection = \"static\" and [[gpus]] is \
                     empty; GPU jobs cannot run",
                    listed.len()
                ),
            )
            .hint(update_list);
        }
        let differences = gpu_differences(&configured, &listed);
        if !differences.is_empty() {
            return Check::warn(
                NAME,
                format!(
                    "[[gpus]] differs from nvidia-smi: {}",
                    differences.join("; ")
                ),
            )
            .hint(update_list);
        }
        return Check::ok(NAME, format!("{} GPU(s) in [[gpus]]", configured.len()));
    };

    let detected: Vec<GpuId> = detection
        .gpus
        .iter()
        .map(|gpu| GpuId {
            index: gpu.index.0,
            uuid: gpu.uuid.clone(),
        })
        .collect();
    if let Some(error) = &detection.nvml_error {
        let fallback = if detected.is_empty() {
            "it runs without GPUs".to_owned()
        } else {
            format!("it uses the {} GPU(s) of [[gpus]] instead", detected.len())
        };
        return Check::warn(
            NAME,
            format!("the daemon could not detect GPUs with NVML ({error}); {fallback}"),
        )
        .hint(if nixos {
            format!(
                "the daemon loads NVML from `services.puzuz.nvmlLibrary` (the default, with the \
                 NVIDIA driver enabled through `hardware.nvidia`, is its libnvidia-ml.so.1) and \
                 must be able to open /dev/nvidiactl and /dev/nvidia<N>: do not override \
                 DevicePolicy=/DeviceAllow= or add PrivateDevices=yes in \
                 `systemd.services.puzuz`; then `{REBUILD}` and see `journalctl -u {DAEMON_UNIT}`"
            )
        } else {
            format!(
                "the daemon (user puzuz) must be able to open /dev/nvidiactl and /dev/nvidia<N>: \
                 keep the DevicePolicy=/DeviceAllow= lines of the packaged {DAEMON_UNIT} \
                 (`sudo puzuz service install` rewrites it) and do not add PrivateDevices=yes; \
                 then `sudo systemctl restart {DAEMON_UNIT}` and see `journalctl -u {DAEMON_UNIT}`"
            )
        });
    }
    let from_nvml = detection.source == GpuSource::Nvml;
    let fix = if from_nvml {
        format!("GPUs are detected when the daemon starts: `sudo systemctl restart {DAEMON_UNIT}`")
    } else {
        update_list
    };
    if detected.is_empty() && !listed.is_empty() {
        return Check::warn(
            NAME,
            format!(
                "nvidia-smi lists {} GPU(s) but the daemon has none; GPU jobs cannot run",
                listed.len()
            ),
        )
        .hint(fix);
    }
    let differences = gpu_differences(&detected, &listed);
    if !differences.is_empty() {
        let what = if from_nvml {
            "the GPUs the daemon detected with NVML differ from nvidia-smi"
        } else {
            "the daemon's [[gpus]] list differs from nvidia-smi"
        };
        return Check::warn(NAME, format!("{what}: {}", differences.join("; "))).hint(fix);
    }
    if from_nvml && !configured.is_empty() {
        let stale = gpu_differences(&configured, &listed);
        if !stale.is_empty() {
            return Check::warn(
                NAME,
                format!(
                    "{} GPU(s) detected with NVML, but the fallback [[gpus]] list differs from \
                     nvidia-smi ({}); it would be used if NVML failed",
                    detected.len(),
                    stale.join("; ")
                ),
            )
            .hint(if nixos {
                format!("update or remove `gpus` in `services.puzuz.settings`, then `{REBUILD}`")
            } else {
                format!("update or remove [[gpus]] in {DEFAULT_CONFIG_PATH}")
            });
        }
    }
    let how = if from_nvml {
        "detected with NVML".to_owned()
    } else if detection.source == GpuSource::Unknown {
        "found by a method this puzuz version does not know (newer daemon?)".to_owned()
    } else {
        format!(
            "from [[gpus]] (gpu_detection = \"{}\")",
            detection.mode.as_str()
        )
    };
    Check::ok(NAME, format!("{} GPU(s), {how}", detected.len()))
}

fn check_kernel(sys: &dyn System) -> Check {
    const NAME: &str = "kernel";
    let release = std::fs::read_to_string(sys.path(Path::new("/proc/sys/kernel/osrelease")))
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|_| "unknown".to_owned());
    if !sys.path(Path::new("/proc/sys/kernel/ns_last_pid")).exists() {
        return Check::fail(
            NAME,
            format!("{release} lacks /proc/sys/kernel/ns_last_pid (CONFIG_CHECKPOINT_RESTORE)"),
        )
        .hint("use a kernel built with CONFIG_CHECKPOINT_RESTORE=y; `sudo criu check` lists what is missing");
    }
    let (major, minor, patch) = CLONE3_SET_TID_KERNEL;
    match Version::parse(&release) {
        Some(version) if !version.at_least(major, minor, patch) => Check::warn(
            NAME,
            format!("{release} is older than {major}.{minor} (no clone3 set_tid)"),
        )
        .hint("upgrade the kernel; `sudo criu check` shows what CRIU can do"),
        _ => Check::ok(NAME, release),
    }
}

fn check_systemd(sys: &dyn System) -> Check {
    const NAME: &str = "systemd";
    match sys.run("systemctl", &["--version"]) {
        Ok(output) => {
            // "systemd 259 (259.1-1)"
            let version = output
                .stdout
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|v| v.parse::<u32>().ok());
            match version {
                Some(v) if v >= MIN_SYSTEMD => Check::ok(NAME, v.to_string()),
                Some(v) => Check::fail(
                    NAME,
                    format!("{v} is too old; {MIN_SYSTEMD} or newer is required"),
                ),
                None => Check::warn(NAME, "cannot parse `systemctl --version`"),
            }
        }
        Err(err) => Check::fail(NAME, format!("systemctl not available: {err}"))
            .hint("puzuz needs systemd as the init system"),
    }
}

/// Output of `systemctl <verb> <unit>`, e.g. "enabled" or "inactive".
fn unit_property(sys: &dyn System, verb: &str, unit: &str) -> String {
    match sys.run("systemctl", &[verb, unit]) {
        Ok(output) => {
            let text = output.stdout.trim();
            if text.is_empty() {
                "unknown".to_owned()
            } else {
                text.lines().next().unwrap_or_default().to_owned()
            }
        }
        Err(_) => "unknown".to_owned(),
    }
}

/// `(is-enabled, is-active)` of a unit.
pub fn unit_state(sys: &dyn System, unit: &str) -> (String, String) {
    (
        unit_property(sys, "is-enabled", unit),
        unit_property(sys, "is-active", unit),
    )
}

/// How to install the units: `service install`, or on NixOS the module.
fn install_hint(nixos: bool) -> String {
    if nixos {
        ENABLE_MODULE.to_owned()
    } else {
        "sudo puzuz service install".to_owned()
    }
}

fn check_unit(sys: &dyn System, unit: &'static str, nixos: bool) -> Check {
    let (enabled, active) = unit_state(sys, unit);
    let message = format!("{enabled}, {active}");
    if enabled == "not-found" || enabled == "unknown" {
        return Check::fail(unit, format!("not installed ({message})")).hint(install_hint(nixos));
    }
    if enabled != "enabled" {
        // NixOS links the units from the store; `systemctl enable` cannot write there.
        let hint = if nixos {
            format!(
                "the services.puzuz module enables {unit} (`services.puzuz.enable = true`, then \
                 `{REBUILD}`); do not disable it with `systemd.services.{}.enable = false`",
                unit.trim_end_matches(".service")
            )
        } else {
            format!("sudo systemctl enable --now {unit}")
        };
        return Check::fail(unit, message).hint(hint);
    }
    if active != "active" {
        return Check::fail(unit, message).hint(format!(
            "sudo systemctl start {unit}; see `journalctl -u {unit}` for why it stopped"
        ));
    }
    Check::ok(unit, message)
}

/// The helper runs as root: its binary, and every directory above it, must not be replaceable
/// by anyone else.
fn check_helper_binary(sys: &dyn System, nixos: bool) -> Check {
    const NAME: &str = "helper binary";
    let Some(exec) = exec_start(sys, HELPER_UNIT) else {
        return Check::fail(
            NAME,
            format!("no ExecStart= for {HELPER_UNIT} (not installed?)"),
        )
        .hint(install_hint(nixos));
    };
    let exe = exec.program;
    if let Err(err) = std::fs::metadata(sys.path(&exe)) {
        return Check::fail(NAME, format!("{}: {err}", exe.display())).hint(install_hint(nixos));
    }
    match check_root_owned_chain(sys, &exe) {
        Ok(()) => Check::ok(NAME, exe.display().to_string()),
        Err(err) => Check::fail(
            NAME,
            format!(
                "{} runs as root, but {err}; whoever can write there can become root",
                exe.display()
            ),
        )
        .hint(if nixos {
            format!(
                "let {HELPER_UNIT} run the helper of `services.puzuz.package` from /nix/store: \
                 remove any ExecStart= override of `systemd.services.puzuz-helper`, then \
                 `{REBUILD}`"
            )
        } else {
            format!(
                "sudo chown root:root {0}; sudo chmod 755 {0}, and the same for each directory \
                 above it; or re-run `sudo puzuz service install` with a directory only root can \
                 write to",
                exe.display()
            )
        }),
    }
}

/// With `ProtectSystem=strict` (as packaged), the daemon can only write below the unit's
/// effective `ReadWritePaths=` (drop-ins included); configured locations elsewhere fail with
/// EROFS. `None` when systemd does not report the unit's sandboxing.
fn check_daemon_write_paths(sys: &dyn System, config: &Config, nixos: bool) -> Option<Check> {
    const NAME: &str = "daemon write paths";
    let output = sys
        .run(
            "systemctl",
            &[
                "show",
                "--property=ProtectSystem",
                "--property=ReadWritePaths",
                DAEMON_UNIT,
            ],
        )
        .ok()
        .filter(|output| output.success)?;
    let mut protect = None;
    let mut writable = Vec::new();
    for line in output.stdout.lines() {
        if let Some(value) = line.strip_prefix("ProtectSystem=") {
            protect = Some(value.trim().to_owned());
        } else if let Some(value) = line.strip_prefix("ReadWritePaths=") {
            // `-` (ignore if missing) and `+` (outside RootDirectory=) prefixes.
            writable.extend(
                value
                    .split_whitespace()
                    .map(|path| PathBuf::from(path.trim_start_matches(['-', '+']))),
            );
        }
    }
    if protect.as_deref() != Some("strict") {
        return None;
    }
    let parent = |path: &Path| path.parent().unwrap_or(path).to_owned();
    // SQLite writes its journal next to the database.
    let needed = [
        ("state_db", parent(&config.state_db_path())),
        ("logs_dir", config.logs_dir()),
        ("api_socket", parent(&config.api_socket)),
    ];
    let outside: Vec<(&str, &PathBuf)> = needed
        .iter()
        .filter(|(_, dir)| !writable.iter().any(|w| dir.starts_with(w)))
        .map(|(key, dir)| (*key, dir))
        .collect();
    if outside.is_empty() {
        return Some(Check::ok(
            NAME,
            "the configured state, log and socket directories are writable",
        ));
    }
    let listed = |f: &dyn Fn(&str, &Path) -> String| {
        outside
            .iter()
            .map(|(key, dir)| f(key, dir))
            .collect::<Vec<_>>()
            .join(", ")
    };
    Some(
        Check::warn(
            NAME,
            format!(
                "{DAEMON_UNIT} runs with ProtectSystem=strict and cannot write to {}: not below \
                 its ReadWritePaths=",
                listed(&|key, dir| format!("{} ({key})", dir.display()))
            ),
        )
        .hint(if nixos {
            format!(
                "add to your NixOS configuration:\n\
                 systemd.services.puzuz.serviceConfig.ReadWritePaths = [ {} ];\n\
                 then `{REBUILD}`",
                listed(&|_, dir| format!("\"{}\"", dir.display())).replace(", ", " ")
            )
        } else {
            format!(
                "add a drop-in with `sudo systemctl edit {DAEMON_UNIT}`:\n[Service]\n\
                 ReadWritePaths={}\nthen `sudo systemctl restart {DAEMON_UNIT}`",
                listed(&|_, dir| dir.display().to_string()).replace(", ", " ")
            )
        }),
    )
}

/// Where the unit's file is: as systemd reports it, else where `service install` puts it.
fn unit_file(sys: &dyn System, unit: &str) -> PathBuf {
    sys.run(
        "systemctl",
        &["show", "--property=FragmentPath", "--value", unit],
    )
    .ok()
    .filter(|output| output.success)
    .map(|output| output.stdout.trim().to_owned())
    .filter(|path| path.starts_with('/'))
    .map_or_else(|| Path::new(UNIT_DIR).join(unit), PathBuf::from)
}

/// The program of the first `ExecStart=` line of a unit file.
pub fn exec_start_binary(unit: &str) -> Option<PathBuf> {
    unit.lines()
        .find_map(|line| line.trim().strip_prefix("ExecStart="))
        .and_then(|cmd| cmd.split_whitespace().next())
        .map(|exe| PathBuf::from(exe.trim_start_matches(['-', '@', ':', '+', '!'])))
}

fn check_socket(sys: &dyn System, name: &'static str, path: &Path, unit: &str) -> Check {
    match std::fs::symlink_metadata(sys.path(path)) {
        Ok(meta) if meta.file_type().is_socket() => Check::ok(name, path.display().to_string()),
        Ok(_) => Check::fail(name, format!("{} is not a socket", path.display()))
            .hint(format!("remove it and run `sudo systemctl restart {unit}`")),
        Err(err) => Check::fail(name, format!("{}: {err}", path.display()))
            .hint(format!("sudo systemctl start {unit}")),
    }
}

fn check_daemon(daemon: &Result<VersionResponse, String>) -> Vec<Check> {
    match daemon {
        Ok(version) => {
            let mut checks = Vec::new();
            let ours = puzuz_protocol::BINARY_VERSION;
            checks.push(if version.daemon_version == ours {
                Check::ok(
                    "daemon",
                    format!("reachable, version {}", version.daemon_version),
                )
            } else {
                Check::warn(
                    "daemon",
                    format!(
                        "reachable, but runs version {} while this puzuz is {ours}",
                        version.daemon_version
                    ),
                )
                .hint(format!(
                    "sudo systemctl restart {HELPER_UNIT} {DAEMON_UNIT}"
                ))
            });
            checks.push(if version.helper.connected {
                let helper_version = version.helper.version.as_deref().unwrap_or("unknown");
                Check::ok("helper", format!("connected, version {helper_version}"))
            } else {
                Check::fail("helper", "the daemon is not connected to the helper").hint(format!(
                    "sudo systemctl start {HELPER_UNIT}; see `journalctl -u {HELPER_UNIT}`"
                ))
            });
            if let Some(orphans) = version.orphan_jobs.as_deref().filter(|o| !o.is_empty()) {
                let ids: Vec<String> = orphans.iter().map(ToString::to_string).collect();
                checks.push(
                    Check::warn(
                        "orphaned jobs",
                        format!(
                            "the helper runs jobs the daemon does not account for: {} (their \
                             GPUs stay taken)",
                            ids.join(", ")
                        ),
                    )
                    .hint(
                        "`puzuz status` shows them to administrators; an administrator can stop \
                         one with `puzuz kill <id>`",
                    ),
                );
            }
            checks
        }
        Err(err) => vec![
            Check::fail("daemon", format!("not reachable: {err}")).hint(format!(
                "sudo systemctl start {DAEMON_UNIT}; see `journalctl -u {DAEMON_UNIT}`"
            )),
            Check::warn("helper", "unknown (the daemon is not reachable)"),
        ],
    }
}

/// Existence, filesystem type and free space of the image directory. The
/// daemon's figures (`storage`) are preferred: it knows how much the images
/// already take, and sees the directory it actually uses.
fn check_checkpoint_dir(
    sys: &dyn System,
    config: &Config,
    storage: Option<&CheckpointStorage>,
    nixos: bool,
) -> Check {
    const NAME: &str = "checkpoint dir";
    let dir = config.checkpoints_dir();
    let shown = dir.display();
    if !sys.path(&dir).is_dir() {
        // NixOS has the module's tmpfiles.d entries elsewhere than TMPFILES_PATH.
        let hint = if nixos {
            "sudo systemd-tmpfiles --create   (the services.puzuz module installs puzuz's \
             tmpfiles.d entries)"
                .to_owned()
        } else {
            format!("sudo systemd-tmpfiles --create {TMPFILES_PATH}")
        };
        return Check::fail(NAME, format!("{shown} does not exist")).hint(hint);
    }
    let local = sys.fs_usage(&dir);
    if let Ok(usage) = &local
        && usage.tmpfs
    {
        return Check::fail(
            NAME,
            format!("{shown} is on tmpfs; images would compete with jobs for RAM"),
        )
        .hint(if nixos {
            format!(
                "mount a disk at {shown} (`fileSystems`) or set `checkpoints_dir` in \
                 `services.puzuz.settings`, then `{REBUILD}`"
            )
        } else {
            format!("mount a disk at {shown} or set `checkpoints_dir` in {DEFAULT_CONFIG_PATH}")
        });
    }
    let available = match (storage.and_then(|s| s.available_bytes), &local) {
        (Some(bytes), _) => bytes,
        (None, Ok(usage)) => usage.available_bytes,
        (None, Err(err)) => return Check::warn(NAME, format!("cannot inspect {shown}: {err}")),
    };
    let free = format_bytes(available);
    // Room for the images still allowed under the limit, and at least for a
    // large dump. The daemon does not know jobs' RSS and VRAM use, so a
    // fixed threshold stands in for "RSS + VRAM x 1.2 of the largest job".
    let budget = match storage {
        Some(storage) => storage
            .limit_bytes
            .map(|limit| limit.saturating_sub(storage.total_bytes)),
        None => config.checkpoint_size_limit_bytes,
    };
    let needed = budget.unwrap_or(0).max(LOW_SPACE_BYTES);
    if available < needed {
        return Check::warn(
            NAME,
            format!("{shown}: only {free} free; a dump needs about RAM + VRAM of the job"),
        )
        .hint("free space there or move `checkpoints_dir` to a larger disk");
    }
    Check::ok(NAME, format!("{shown}: {free} free"))
}

/// The total size of the images versus `checkpoint_size_limit_bytes`, as
/// the daemon accounts for it (the image directory is root-only).
fn check_checkpoint_usage(storage: &CheckpointStorage, nixos: bool) -> Check {
    const NAME: &str = "checkpoint usage";
    let used = format!(
        "{} image(s) take {}",
        storage.images,
        format_bytes(storage.total_bytes)
    );
    match storage.limit_bytes {
        None => Check::info(NAME, format!("{used}; no checkpoint_size_limit_bytes set")),
        Some(limit) if storage.total_bytes > limit => Check::warn(
            NAME,
            format!(
                "{used}, over checkpoint_size_limit_bytes ({}); images are never deleted \
                 automatically",
                format_bytes(limit)
            ),
        )
        .hint(format!(
            "find large checkpoints with `puzuz status` (administrators see every job), then \
             restore them or `puzuz remove <id>` jobs that are no longer needed; or raise \
             `checkpoint_size_limit_bytes` in {}",
            if nixos {
                "`services.puzuz.settings`"
            } else {
                DEFAULT_CONFIG_PATH
            }
        )),
        Some(limit) => Check::ok(
            NAME,
            format!(
                "{used} of checkpoint_size_limit_bytes ({})",
                format_bytes(limit)
            ),
        ),
    }
}

fn check_admin(sys: &dyn System, nixos: bool) -> Check {
    const NAME: &str = "admin";
    if sys.euid() == 0 {
        if !sys.group_exists(ADMIN_GROUP) {
            return Check::warn(NAME, format!("group {ADMIN_GROUP} does not exist"))
                .hint(install_hint(nixos));
        }
        return Check::info(NAME, "running as root (administrator)");
    }
    match sys.user_in_group(ADMIN_GROUP) {
        None => Check::warn(NAME, format!("group {ADMIN_GROUP} does not exist"))
            .hint(install_hint(nixos)),
        Some(true) => Check::info(NAME, format!("you are in {ADMIN_GROUP} (administrator)")),
        Some(false) if nixos => Check::info(
            NAME,
            format!(
                "you are not in {ADMIN_GROUP}; administrators are listed in \
                 `services.puzuz.admins` (then `{REBUILD}`)"
            ),
        ),
        Some(false) => Check::info(
            NAME,
            format!(
                "you are not in {ADMIN_GROUP}; administrators are added with \
                 `sudo usermod -aG {ADMIN_GROUP} <user>`"
            ),
        ),
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests;
