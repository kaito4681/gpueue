//! `puzuz doctor`: checks the installation and prints how to fix what is missing.

use std::fmt;
use std::io::{self, Write};
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

use puzuz_checkpoint::probe::{Version, parse_criu_version, parse_cuda_checkpoint_help};
use puzuz_daemon::config::{Config, DEFAULT_CONFIG_PATH};
use puzuz_protocol::api::{CheckpointStorage, VersionResponse};

use crate::service::{ADMIN_GROUP, DAEMON_UNIT, HELPER_UNIT, TMPFILES_PATH, UNIT_DIR};
use crate::system::{System, find_program, is_root_owned_and_protected};

/// Oldest CRIU that works with the helper (restoring into systemd units, CUDA support).
const MIN_CRIU: (u32, u32, u32) = (4, 2, 0);
/// `Type=exec` in the units.
const MIN_SYSTEMD: u32 = 240;
/// clone3() with set_tid, which CRIU uses to restore PIDs.
const CLONE3_SET_TID_KERNEL: (u32, u32, u32) = (5, 5, 0);
/// Below this much free space for checkpoint images, warn.
const LOW_SPACE_BYTES: u64 = 20 << 30;

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
}

pub fn load_config(sys: &dyn System) -> LoadedConfig {
    let path = sys.path(Path::new(DEFAULT_CONFIG_PATH));
    if !path.exists() {
        return LoadedConfig {
            config: Config::default(),
            check: Check::warn(
                "config",
                format!("{DEFAULT_CONFIG_PATH} does not exist; the defaults apply"),
            )
            .hint("sudo puzuz service install   (creates a commented default configuration)"),
        };
    }
    match Config::load(&path) {
        Ok(config) => LoadedConfig {
            config,
            check: check_config_trust(sys),
        },
        Err(err) => LoadedConfig {
            config: Config::default(),
            check: Check::fail("config", err.to_string()).hint(format!(
                "fix {DEFAULT_CONFIG_PATH}; the daemon does not start with it"
            )),
        },
    }
}

/// The helper (root) reads the same file and refuses it unless only root can change it.
fn check_config_trust(sys: &dyn System) -> Check {
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
                .hint(format!(
                    "sudo chown root:root {0}; sudo chmod go-w {0}",
                    path.display()
                ));
            }
            Err(err) => return Check::fail("config", format!("{}: {err}", path.display())),
        }
    }
    Check::ok("config", DEFAULT_CONFIG_PATH)
}

/// What the helper uses: `config.toml`, unless its unit passes flags that override it.
fn check_helper_settings(sys: &dyn System, config: &Config) -> Check {
    const NAME: &str = "helper settings";
    let summary = format!(
        "cuda mode {}, criu {}, cuda-checkpoint {}, checkpoints {}, logs {}",
        config.cuda_checkpoint_mode.as_str(),
        config.criu_path.display(),
        config.cuda_checkpoint_path.display(),
        config.checkpoints_dir().display(),
        config.logs_dir().display(),
    );
    let unit = unit_file(sys, HELPER_UNIT);
    let flags = std::fs::read_to_string(sys.path(&unit))
        .ok()
        .map(|text| exec_start_flags(&text))
        .unwrap_or_default();
    if flags.is_empty() {
        return Check::info(NAME, format!("from {DEFAULT_CONFIG_PATH}: {summary}"));
    }
    Check::warn(
        NAME,
        format!(
            "ExecStart= of {} passes {}, which override {DEFAULT_CONFIG_PATH} \
             (this report shows the file's values: {summary})",
            unit.display(),
            flags.join(" ")
        ),
    )
    .hint(format!(
        "set the keys in {DEFAULT_CONFIG_PATH} and remove the flags \
         (`sudo systemctl edit {HELPER_UNIT}` or `sudo puzuz service install`)"
    ))
}

/// The `--flags` of the first `ExecStart=` line of a unit file.
pub fn exec_start_flags(unit: &str) -> Vec<String> {
    unit.lines()
        .find_map(|line| line.trim().strip_prefix("ExecStart="))
        .map(|cmd| {
            cmd.split_whitespace()
                .filter(|arg| arg.starts_with("--"))
                .map(|arg| arg.split('=').next().unwrap_or(arg).to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Runs every check. `socket` overrides the daemon socket from the configuration.
pub fn run(sys: &dyn System, socket: Option<&Path>) -> Report {
    let LoadedConfig { config, check } = load_config(sys);
    let mut checks = vec![check];

    checks.push(check_criu(sys, &config.criu_path));
    let driver = driver_version(sys);
    checks.push(check_driver(&driver));
    checks.push(check_cuda_checkpoint(
        sys,
        &config.cuda_checkpoint_path,
        driver.as_ref().ok(),
    ));
    if driver.is_ok() {
        checks.push(check_gpus(sys, &config));
    }
    checks.push(check_kernel(sys));
    checks.push(check_systemd(sys));
    for unit in [HELPER_UNIT, DAEMON_UNIT] {
        checks.push(check_unit(sys, unit));
    }
    checks.push(check_helper_binary(sys));
    checks.push(check_helper_settings(sys, &config));
    let api_socket = socket.map_or_else(|| config.api_socket.clone(), Path::to_owned);
    checks.push(check_socket(sys, "daemon socket", &api_socket, DAEMON_UNIT));
    checks.push(check_socket(
        sys,
        "helper socket",
        &config.helper_socket,
        HELPER_UNIT,
    ));
    let daemon = sys.daemon_version(&api_socket);
    checks.extend(check_daemon(&daemon));
    let storage = daemon.as_ref().ok().and_then(|v| v.checkpoints.as_ref());
    checks.push(check_checkpoint_dir(sys, &config, storage));
    if let Some(storage) = storage {
        checks.push(check_checkpoint_usage(storage));
    }
    checks.push(check_admin(sys));
    Report { checks }
}

fn check_criu(sys: &dyn System, configured: &Path) -> Check {
    const NAME: &str = "criu";
    let install = format!(
        "install CRIU {}.{} or newer (https://criu.org/Installation), or set `criu_path` in {}",
        MIN_CRIU.0, MIN_CRIU.1, DEFAULT_CONFIG_PATH
    );
    let Some(path) = find_program(sys, configured) else {
        return Check::fail(NAME, format!("{} not found", configured.display())).hint(install);
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
    let Some(smi) = find_program(sys, Path::new("nvidia-smi")) else {
        return Err(
            "no NVIDIA driver found (no /proc/driver/nvidia/version, no nvidia-smi)".into(),
        );
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

fn check_driver(driver: &Result<Version, String>) -> Check {
    match driver {
        Ok(version) => Check::ok("nvidia driver", version.to_string()),
        Err(err) => Check::warn("nvidia driver", format!("{err}; only CPU jobs can run")).hint(
            "install the NVIDIA driver (r570 or newer for cuda-checkpoint) if this host has GPUs",
        ),
    }
}

fn check_cuda_checkpoint(sys: &dyn System, configured: &Path, driver: Option<&Version>) -> Check {
    const NAME: &str = "cuda-checkpoint";
    // Without a driver there is nothing to checkpoint on a GPU.
    let missing = if driver.is_some() {
        Level::Fail
    } else {
        Level::Warn
    };
    let install = format!(
        "install cuda-checkpoint matching the driver (https://github.com/NVIDIA/cuda-checkpoint) \
         into /usr/local/bin, or set `cuda_checkpoint_path` in {DEFAULT_CONFIG_PATH}"
    );
    let Some(path) = find_program(sys, configured) else {
        return Check::new(
            missing,
            NAME,
            format!(
                "{} not found; GPU jobs cannot be checkpointed",
                configured.display()
            ),
        )
        .hint(install);
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
        return Check::new(missing, NAME, format!("`{shown} --help` failed: {reason}"))
            .hint("check that the NVIDIA driver (libcuda.so.1) is installed and matches it");
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
        .hint("install the cuda-checkpoint of driver r570 or newer");
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

fn check_gpus(sys: &dyn System, config: &Config) -> Check {
    const NAME: &str = "gpus";
    let Some(smi) = find_program(sys, &config.nvidia_smi_path) else {
        return Check::warn(
            NAME,
            "nvidia-smi not found; the helper needs it to find CUDA processes",
        )
        .hint("install nvidia-smi (part of the NVIDIA driver utilities)");
    };
    let listed = match sys.run(
        &smi.to_string_lossy(),
        &["--query-gpu=index,name", "--format=csv,noheader"],
    ) {
        Ok(output) if output.success => output
            .stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
        Ok(output) => {
            return Check::warn(NAME, format!("nvidia-smi failed: {}", output.stdout.trim()));
        }
        Err(err) => return Check::warn(NAME, format!("cannot run nvidia-smi: {err}")),
    };
    let configured = config.gpus.len();
    if configured == 0 && listed > 0 {
        return Check::warn(
            NAME,
            format!("nvidia-smi lists {listed} GPU(s) but none is configured; GPU jobs cannot run"),
        )
        .hint(format!(
            "add [[gpus]] entries to {DEFAULT_CONFIG_PATH} \
             (see `nvidia-smi --query-gpu=index,uuid,name --format=csv`)"
        ));
    }
    if configured != listed {
        return Check::warn(
            NAME,
            format!("nvidia-smi lists {listed} GPU(s), the configuration {configured}"),
        )
        .hint(format!("update [[gpus]] in {DEFAULT_CONFIG_PATH}"));
    }
    Check::ok(NAME, format!("{listed} GPU(s)"))
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

fn check_unit(sys: &dyn System, unit: &'static str) -> Check {
    let (enabled, active) = unit_state(sys, unit);
    let message = format!("{enabled}, {active}");
    if enabled == "not-found" || enabled == "unknown" {
        return Check::fail(unit, format!("not installed ({message})"))
            .hint("sudo puzuz service install");
    }
    if enabled != "enabled" {
        return Check::fail(unit, message).hint(format!("sudo systemctl enable --now {unit}"));
    }
    if active != "active" {
        return Check::fail(unit, message).hint(format!(
            "sudo systemctl start {unit}; see `journalctl -u {unit}` for why it stopped"
        ));
    }
    Check::ok(unit, message)
}

/// The helper runs as root: its binary must not be replaceable by anyone else.
fn check_helper_binary(sys: &dyn System) -> Check {
    const NAME: &str = "helper binary";
    let unit = unit_file(sys, HELPER_UNIT);
    let Ok(text) = std::fs::read_to_string(sys.path(&unit)) else {
        return Check::fail(NAME, format!("{} not installed", unit.display()))
            .hint("sudo puzuz service install");
    };
    let Some(exe) = exec_start_binary(&text) else {
        return Check::fail(NAME, format!("no ExecStart= in {}", unit.display()))
            .hint("sudo puzuz service install");
    };
    match is_root_owned_and_protected(sys, &exe) {
        Ok(true) => Check::ok(NAME, exe.display().to_string()),
        Ok(false) => Check::fail(
            NAME,
            format!(
                "{} is not owned by root or is writable by others",
                exe.display()
            ),
        )
        .hint(format!(
            "sudo chown root:root {0}; sudo chmod 755 {0}",
            exe.display()
        )),
        Err(err) => Check::fail(NAME, format!("{}: {err}", exe.display()))
            .hint("sudo puzuz service install --bin-dir /usr/local/bin"),
    }
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
) -> Check {
    const NAME: &str = "checkpoint dir";
    let dir = config.checkpoints_dir();
    let shown = dir.display();
    if !sys.path(&dir).is_dir() {
        return Check::fail(NAME, format!("{shown} does not exist"))
            .hint(format!("sudo systemd-tmpfiles --create {TMPFILES_PATH}"));
    }
    let local = sys.fs_usage(&dir);
    if let Ok(usage) = &local
        && usage.tmpfs
    {
        return Check::fail(
            NAME,
            format!("{shown} is on tmpfs; images would compete with jobs for RAM"),
        )
        .hint(format!(
            "mount a disk at {shown} or set `checkpoints_dir` in {DEFAULT_CONFIG_PATH}"
        ));
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
fn check_checkpoint_usage(storage: &CheckpointStorage) -> Check {
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
             `checkpoint_size_limit_bytes` in {DEFAULT_CONFIG_PATH}"
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

fn check_admin(sys: &dyn System) -> Check {
    const NAME: &str = "admin";
    if sys.euid() == 0 {
        if !sys.group_exists(ADMIN_GROUP) {
            return Check::warn(NAME, format!("group {ADMIN_GROUP} does not exist"))
                .hint("sudo puzuz service install");
        }
        return Check::info(NAME, "running as root (administrator)");
    }
    match sys.user_in_group(ADMIN_GROUP) {
        None => Check::warn(NAME, format!("group {ADMIN_GROUP} does not exist"))
            .hint("sudo puzuz service install"),
        Some(true) => Check::info(NAME, format!("you are in {ADMIN_GROUP} (administrator)")),
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
