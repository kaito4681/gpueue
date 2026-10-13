//! `puzuz service install | uninstall | status`.
//!
//! The packaging files (systemd units, sysusers.d and tmpfiles.d snippets and the default
//! configuration) are embedded in the binary, so installing needs nothing but the `puzuz` and
//! `puzuz-helper` binaries. Installed files go to /etc, the place for locally installed system
//! configuration; files owned by a distribution package (/usr/lib/...) are never touched.

use std::collections::BTreeSet;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use puzuz_daemon::config::DEFAULT_CONFIG_PATH;

use crate::doctor::{self, Report};
use crate::install_method::{self, Detection, Method};
use crate::system::{System, check_root_owned_chain, run_checked};

mod receipt;

pub use receipt::{RECEIPT_PATH, Receipt};

pub const DAEMON_UNIT: &str = "puzuz.service";
pub const HELPER_UNIT: &str = "puzuz-helper.service";
pub const ADMIN_GROUP: &str = "puzuz-admin";

pub const UNIT_DIR: &str = "/etc/systemd/system";
pub const SYSUSERS_PATH: &str = "/etc/sysusers.d/puzuz.conf";
pub const TMPFILES_PATH: &str = "/etc/tmpfiles.d/puzuz.conf";
const CONFIG_DIR: &str = "/etc/puzuz";

/// Directories the units may run the binaries from without copying them.
const SYSTEM_BIN_DIRS: &[&str] = &["/usr/bin", "/usr/local/bin"];

/// Data removed by `uninstall --purge`.
const PURGE_DIRS: &[&str] = &[
    "/var/lib/puzuz",
    CONFIG_DIR,
    "/run/puzuz",
    "/run/puzuz-helper",
];

const DAEMON_UNIT_TEMPLATE: &str = include_str!("../../../packaging/systemd/puzuz.service");
const HELPER_UNIT_TEMPLATE: &str = include_str!("../../../packaging/systemd/puzuz-helper.service");
const SYSUSERS: &str = include_str!("../../../packaging/sysusers.d/puzuz.conf");
const TMPFILES: &str = include_str!("../../../packaging/tmpfiles.d/puzuz.conf");
pub const CONFIG_TEMPLATE: &str = include_str!("../../../packaging/config.toml");

/// First line of every file `install` writes; `uninstall` only removes files starting with it.
pub const MARKER: &str =
    "# Installed by `puzuz service install`; re-running it overwrites this file.";

/// How long `install` waits for the daemon to connect to the helper after starting them.
const START_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// A failed `service` operation; the message is meant for the user.
pub type Result<T, E = String> = std::result::Result<T, E>;

fn io_err(context: impl std::fmt::Display) -> impl FnOnce(io::Error) -> String {
    move |err| format!("{context}: {err}")
}

fn require_root(sys: &dyn System, what: &str) -> Result<()> {
    if sys.euid() == 0 {
        Ok(())
    } else {
        Err(format!(
            "`puzuz {what}` must run as root; try `sudo puzuz {what}`"
        ))
    }
}

/// A file to install: system path, contents and mode.
struct InstallFile {
    path: PathBuf,
    contents: String,
    mode: u32,
}

/// The unit and snippet files for binaries in `bin_dir`.
fn packaged_files(bin_dir: &Path) -> Vec<InstallFile> {
    let file = |path: PathBuf, body: String| InstallFile {
        path,
        contents: format!("{MARKER}\n{body}"),
        mode: 0o644,
    };
    vec![
        file(
            Path::new(UNIT_DIR).join(HELPER_UNIT),
            render_unit(HELPER_UNIT_TEMPLATE, bin_dir),
        ),
        file(
            Path::new(UNIT_DIR).join(DAEMON_UNIT),
            render_unit(DAEMON_UNIT_TEMPLATE, bin_dir),
        ),
        file(PathBuf::from(SYSUSERS_PATH), SYSUSERS.to_owned()),
        file(PathBuf::from(TMPFILES_PATH), TMPFILES.to_owned()),
    ]
}

/// Points the unit's `ExecStart=` at the binaries in `bin_dir`.
pub fn render_unit(template: &str, bin_dir: &Path) -> String {
    let prefix = format!("ExecStart={}/", bin_dir.display());
    template
        .lines()
        .map(|line| match line.strip_prefix("ExecStart=/usr/bin/") {
            Some(rest) => format!("{prefix}{rest}\n"),
            None => format!("{line}\n"),
        })
        .collect()
}

/// Creates the system directory `dir` and its missing parents with mode 0755, whatever the
/// umask (`sudo` may run with umask 077, which would leave e.g. /etc/puzuz unreadable for the
/// daemon).
fn create_dirs(sys: &dyn System, dir: &Path) -> Result<()> {
    let local = sys.path(dir);
    let missing: Vec<&Path> = local
        .ancestors()
        .take_while(|p| p.symlink_metadata().is_err())
        .collect();
    for p in missing.into_iter().rev() {
        let create = || -> io::Result<()> {
            match std::fs::DirBuilder::new().mode(0o755).create(p) {
                Err(err) if err.kind() != io::ErrorKind::AlreadyExists => return Err(err),
                _ => {}
            }
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755))
        };
        create().map_err(io_err(format!("creating {}", p.display())))?;
    }
    Ok(())
}

/// Writes `contents` to the system path atomically unless it is already there.
/// Returns whether anything changed.
fn write_file(sys: &dyn System, path: &Path, contents: &[u8], mode: u32) -> Result<bool> {
    let local = sys.path(path);
    if let (Ok(existing), Ok(meta)) = (std::fs::read(&local), std::fs::metadata(&local))
        && existing == contents
        && meta.mode() & 0o7777 == mode
    {
        return Ok(false);
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    create_dirs(sys, parent)?;
    let file_name = local.file_name().unwrap_or_default().to_string_lossy();
    let tmp = local.with_file_name(format!(".{file_name}.puzuz-install"));
    let write = || -> io::Result<()> {
        // A leftover (or planted) temporary file or symlink is removed, not followed; the new
        // one is created exclusively and never through a symlink.
        match std::fs::remove_file(&tmp) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
            _ => {}
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
        // Renaming replaces even a running binary (writing into it fails with ETXTBSY).
        std::fs::rename(&tmp, &local)
    };
    write().map_err(|err| {
        let _ = std::fs::remove_file(&tmp);
        format!("writing {}: {err}", path.display())
    })?;
    Ok(true)
}

fn valid_bin_dir(dir: &Path) -> Result<()> {
    let text = dir.to_str().unwrap_or("");
    let ok = dir.is_absolute()
        && !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._+-".contains(c));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "--bin-dir {} must be an absolute path of letters, digits and / . _ + -",
            dir.display()
        ))
    }
}

/// Where the binaries are installed when neither `--bin-dir` nor an existing installation
/// says otherwise: the standard place for software the administrator installs (FHS), which
/// package managers leave alone and which is in systemd's and every user's default `PATH`.
pub const DEFAULT_BIN_DIR: &str = "/usr/local/bin";

/// The binaries the units run, as installed by [`install_binaries`].
struct Binaries {
    bin_dir: PathBuf,
    /// Whether a binary in `bin_dir` changed.
    changed: bool,
    /// The binaries copied into `bin_dir` (empty: used in place).
    copied: Vec<PathBuf>,
    /// The Nix store path the running binary belongs to, if it runs from the store.
    store_path: Option<PathBuf>,
}

pub const NIX_STORE: &str = "/nix/store";
/// The garbage-collector root `service install` registers for binaries from the Nix store.
pub const GC_ROOT: &str = "/nix/var/nix/gcroots/puzuz";
/// Where `nix-store` is when it is not in systemd's default `PATH` (sudo's `secure_path` hides
/// the profile of a multi-user Nix installation).
const NIX_STORE_BINARIES: &[&str] = &[
    "/nix/var/nix/profiles/default/bin/nix-store",
    "/run/current-system/sw/bin/nix-store",
];

/// The store path (`/nix/store/<hash>-<name>`) the system path `exe` resolves into, if any.
fn nix_store_path(sys: &dyn System, exe: &Path) -> Option<PathBuf> {
    let root = std::fs::canonicalize(sys.root()).ok()?;
    let resolved = std::fs::canonicalize(sys.path(exe)).ok()?;
    let system = Path::new("/").join(resolved.strip_prefix(&root).ok()?);
    let entry = system.strip_prefix(NIX_STORE).ok()?.components().next()?;
    Some(Path::new(NIX_STORE).join(entry))
}

/// Registers [`GC_ROOT`] for `store_path` with `nix-store`, so that `nix-collect-garbage` keeps
/// what the copied binaries use (their libraries and loader). Returns whether it worked; a
/// failure is only a warning.
fn add_gc_root(sys: &dyn System, store_path: &Path, out: &mut dyn Write) -> bool {
    let program = crate::system::find_program(sys, Path::new("nix-store")).or_else(|| {
        NIX_STORE_BINARIES
            .iter()
            .map(PathBuf::from)
            .find(|path| sys.path(path).is_file())
    });
    let manual = format!(
        "register it yourself so that garbage collection keeps what the binaries use: \
         `sudo nix-store --add-root {GC_ROOT} --realise {}`",
        store_path.display()
    );
    let Some(program) = program else {
        writeln!(
            out,
            "warning: nix-store not found; the binaries come from {} but no GC root was \
             registered; {manual}",
            store_path.display()
        )
        .ok();
        return false;
    };
    let store_path_text = store_path.to_string_lossy();
    match run_checked(
        sys,
        &program.to_string_lossy(),
        &["--add-root", GC_ROOT, "--realise", &store_path_text],
    ) {
        Ok(_) => {
            writeln!(
                out,
                "registered the Nix GC root {GC_ROOT} for {}",
                store_path.display()
            )
            .ok();
            true
        }
        Err(err) => {
            writeln!(out, "warning: {err}; {manual}").ok();
            false
        }
    }
}

/// Removes the GC root `root` (only ever [`GC_ROOT`], and only a symlink).
fn remove_gc_root(sys: &dyn System, root: &Path, out: &mut dyn Write) {
    if root != Path::new(GC_ROOT) {
        writeln!(
            out,
            "warning: ignoring the GC root {} in {RECEIPT_PATH} (expected {GC_ROOT})",
            root.display()
        )
        .ok();
        return;
    }
    let local = sys.path(root);
    match std::fs::symlink_metadata(&local) {
        Ok(meta) if meta.file_type().is_symlink() => match std::fs::remove_file(&local) {
            Ok(()) => {
                writeln!(out, "removed the Nix GC root {GC_ROOT}").ok();
            }
            Err(err) => {
                writeln!(out, "warning: removing {GC_ROOT}: {err}").ok();
            }
        },
        Ok(_) => {
            writeln!(out, "warning: kept {GC_ROOT} (not a symlink)").ok();
        }
        Err(_) => {}
    }
}

/// A GC root and the store path it keeps, as recorded in the receipt.
type GcRoot = (Option<PathBuf>, Option<PathBuf>);

/// The GC root of the new installation: registered when the binaries come from the Nix store;
/// otherwise the previous one stays while the installed binaries are unchanged and is removed
/// once they are replaced (by binaries that do not need the store).
fn update_gc_root(
    sys: &dyn System,
    previous: Option<&Receipt>,
    binaries: &Binaries,
    out: &mut dyn Write,
) -> GcRoot {
    if let Some(store_path) = &binaries.store_path {
        return if add_gc_root(sys, store_path, out) {
            (Some(PathBuf::from(GC_ROOT)), Some(store_path.clone()))
        } else {
            (None, None)
        };
    }
    let Some((previous, root)) = previous.and_then(|p| Some((p, p.gc_root.clone()?))) else {
        return (None, None);
    };
    if previous.bin_dir == binaries.bin_dir && !binaries.changed {
        return (Some(root), previous.gc_root_target.clone());
    }
    remove_gc_root(sys, &root, out);
    (None, None)
}

/// The directory of the binaries an existing `service install` installation runs.
fn installed_bin_dir(detection: &Detection) -> Option<&Path> {
    detection.methods().find_map(|m| match m {
        Method::ServiceInstall { bin_dir: Some(dir) } => Some(dir.as_path()),
        _ => None,
    })
}

/// Decides where the units run the binaries from and copies them there unless they already
/// are: `--bin-dir` if given; else the directory of an existing `service install`
/// installation; else the directory of this binary if it is /usr/bin or /usr/local/bin (used
/// in place); else [`DEFAULT_BIN_DIR`].
fn install_binaries(
    sys: &dyn System,
    explicit: Option<&Path>,
    detection: &Detection,
    out: &mut dyn Write,
) -> Result<Binaries> {
    let exe = sys
        .current_exe()
        .map_err(io_err("cannot locate the running puzuz binary"))?;
    let exe_dir = exe
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", exe.display()))?
        .to_owned();
    let helper = exe_dir.join("puzuz-helper");
    if !sys.path(&helper).is_file() {
        return Err(format!(
            "{} not found; puzuz-helper must be installed next to puzuz",
            helper.display()
        ));
    }
    let bin_dir = if let Some(dir) = explicit {
        dir.to_owned()
    } else if let Some(dir) = installed_bin_dir(detection) {
        dir.to_owned()
    } else if SYSTEM_BIN_DIRS.iter().any(|dir| exe_dir == Path::new(dir)) {
        exe_dir.clone()
    } else {
        writeln!(
            out,
            "installing into {DEFAULT_BIN_DIR} (use --bin-dir to choose another directory)"
        )
        .ok();
        PathBuf::from(DEFAULT_BIN_DIR)
    };
    valid_bin_dir(&bin_dir)?;
    // puzuz-helper runs as root from here: whoever can replace it, or rename a parent
    // directory, gets root.
    check_root_owned_chain(sys, &bin_dir).map_err(|err| {
        if explicit.is_some() {
            format!(
                "--bin-dir {}: {err}; choose a directory only root can write to, such as \
                 {DEFAULT_BIN_DIR}",
                bin_dir.display()
            )
        } else {
            format!(
                "the helper would run as root from {}, but {err}; fix that, or choose a \
                 directory only root can write to with --bin-dir",
                bin_dir.display()
            )
        }
    })?;
    let mut changed = false;
    let mut copied = Vec::new();
    for (source, name) in [(&exe, "puzuz"), (&helper, "puzuz-helper")] {
        let target = bin_dir.join(name);
        if sys.path(source) == sys.path(&target) {
            continue;
        }
        let bytes = std::fs::read(sys.path(source))
            .map_err(io_err(format!("reading {}", source.display())))?;
        if write_file(sys, &target, &bytes, 0o755)? {
            writeln!(out, "installed {}", target.display()).ok();
            changed = true;
        }
        copied.push(target);
    }
    Ok(Binaries {
        bin_dir,
        changed,
        copied,
        store_path: nix_store_path(sys, &exe),
    })
}

/// Writes the install receipt for `binaries`. A re-install into the same directory keeps the
/// copies recorded before (re-running the installed copy uses it in place); copies the
/// previous installation left in another directory are returned for removal.
fn write_receipt(
    sys: &dyn System,
    previous: Option<&Receipt>,
    binaries: &Binaries,
    (gc_root, gc_root_target): GcRoot,
) -> Result<Vec<PathBuf>> {
    let mut copied = binaries.copied.clone();
    let mut stale = Vec::new();
    if let Some(previous) = previous {
        for path in previous.removable_copies() {
            if previous.bin_dir == binaries.bin_dir {
                if !copied.contains(&path) {
                    copied.push(path);
                }
            } else {
                stale.push(path);
            }
        }
    }
    copied.sort();
    let new = Receipt {
        version: puzuz_protocol::BINARY_VERSION.to_owned(),
        bin_dir: binaries.bin_dir.clone(),
        copied,
        gc_root,
        gc_root_target,
    };
    write_file(
        sys,
        Path::new(RECEIPT_PATH),
        new.to_toml().as_bytes(),
        0o644,
    )?;
    Ok(stale)
}

/// Removes copies a previous installation left in a directory the units no longer use.
fn remove_stale_copies(sys: &dyn System, stale: &[PathBuf], out: &mut dyn Write) {
    for path in stale {
        match std::fs::remove_file(sys.path(path)) {
            Ok(()) => {
                writeln!(
                    out,
                    "removed {} (copied by the previous install)",
                    path.display()
                )
                .ok();
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                writeln!(out, "warning: removing {}: {err}", path.display()).ok();
            }
        }
    }
}

/// GPUs reported by nvidia-smi as `[[gpus]]` entries for a new configuration file.
fn detected_gpus_toml(sys: &dyn System) -> String {
    let Some(smi) = crate::system::find_program(sys, Path::new("nvidia-smi")) else {
        return String::new();
    };
    let Ok(output) = sys.run(
        &smi.to_string_lossy(),
        &[
            "--query-gpu=index,uuid,name,memory.total",
            "--format=csv,noheader,nounits",
        ],
    ) else {
        return String::new();
    };
    if !output.success {
        return String::new();
    }
    let mut toml = String::new();
    for line in output.stdout.lines() {
        let fields: Vec<&str> = line.split(',').map(str::trim).collect();
        let [index, uuid, name, memory_mib] = fields[..] else {
            continue;
        };
        let Ok(index) = index.parse::<u32>() else {
            continue;
        };
        // JSON string escapes are valid in TOML basic strings.
        let quote = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_owned());
        toml.push_str(&format!(
            "\n[[gpus]]\nindex = {index}\nuuid = {}\nname = {}\n",
            quote(uuid),
            quote(name)
        ));
        if let Ok(mib) = memory_mib.parse::<u64>() {
            toml.push_str(&format!("memory_total_bytes = {}\n", mib << 20));
        }
    }
    if toml.is_empty() {
        toml
    } else {
        format!("\n# Detected with nvidia-smi by `puzuz service install`.{toml}")
    }
}

pub struct InstallOptions {
    pub no_start: bool,
    pub bin_dir: Option<PathBuf>,
    /// Overwrite unit and snippet files that `install` did not write (no [`MARKER`]).
    pub force: bool,
}

/// Checks of the final doctor report that fail as expected after `install --no-start`: the
/// services are not running yet.
const NOT_STARTED_CHECKS: &[&str] = &[
    HELPER_UNIT,
    DAEMON_UNIT,
    "daemon socket",
    "helper socket",
    "daemon",
    "helper",
];

/// Whether `install` succeeded, judged by its final doctor report: no failures, except those
/// expected because `--no-start` left the services stopped.
pub fn install_succeeded(report: &Report, options: &InstallOptions) -> bool {
    !report.checks.iter().any(|check| {
        check.level == crate::doctor::Level::Fail
            && !(options.no_start && NOT_STARTED_CHECKS.contains(&check.name))
    })
}

/// Refuses to overwrite files an administrator or a package wrote (they lack [`MARKER`]),
/// unless forced. Checked before anything is changed.
fn check_foreign_files(sys: &dyn System, files: &[InstallFile], force: bool) -> Result<()> {
    let foreign: Vec<String> = files
        .iter()
        .filter(|file| {
            std::fs::read(sys.path(&file.path))
                .is_ok_and(|text| !text.starts_with(MARKER.as_bytes()))
        })
        .map(|file| file.path.display().to_string())
        .collect();
    if foreign.is_empty() || force {
        return Ok(());
    }
    Err(format!(
        "{} {} not written by `puzuz service install`; refusing to overwrite. Customize the \
         services with drop-ins (`sudo systemctl edit {DAEMON_UNIT}`) instead, or re-run with \
         --force to replace them",
        foreign.join(", "),
        if foreign.len() == 1 { "was" } else { "were" }
    ))
}

/// Refuses to touch a machine where a package manager or Nix manages puzuz: the units in /etc
/// would shadow the packaged ones, and both would share /etc/puzuz and /var/lib/puzuz.
/// `--force` does not override this. A unit in /usr/lib that no package owns is refused
/// without `--force`. Returns warnings to print before installing.
fn check_install_method(detection: &Detection, force: bool) -> Result<Vec<String>> {
    if let Some(managed) = detection.managed_elsewhere() {
        let mut message = format!(
            "puzuz is installed as {managed}; `puzuz service install` would conflict with it \
             (--force does not change this). To update it, {}.",
            detection.update_hint(managed)
        );
        if detection.has_service_install() {
            message.push_str(
                " It is also installed by `puzuz service install`; remove that copy with \
                 `sudo puzuz service uninstall`.",
            );
        } else {
            message.push_str(&format!(
                " To switch to `puzuz service install`, first {}.",
                detection.removal_hint(managed)
            ));
        }
        return Err(message);
    }
    if detection.os.is_nixos() {
        return Err(
            "this is NixOS: install puzuz with the `services.puzuz` NixOS module instead of \
             `puzuz service install` (--force does not change this)"
                .to_owned(),
        );
    }
    if detection.unit_dir_in_store {
        return Err(format!(
            "{UNIT_DIR} resolves into /nix/store: Nix manages it, and it cannot be written. \
             Install puzuz through the Nix configuration that manages it (--force does not \
             change this)"
        ));
    }
    let mut warnings = Vec::new();
    for method in detection.methods() {
        if let Method::Unowned { unit, .. } = method {
            let unit = unit.display();
            let note = format!(
                "no package owns {unit} (left over from a removed package, or copied there by \
                 hand); delete it (`sudo rm {unit}`)"
            );
            if !force && detection.unowned_needs_force() {
                return Err(format!(
                    "{note} and re-run, or re-run with --force to install next to it"
                ));
            }
            warnings.push(format!("{note}: the units in {UNIT_DIR} shadow it"));
        }
    }
    for leftover in &detection.leftovers {
        let leftover = leftover.display();
        warnings.push(format!(
            "no package owns {leftover}, which the units in {UNIT_DIR} shadow; delete it \
             (`sudo rm {leftover}`)"
        ));
    }
    Ok(warnings)
}

/// `puzuz service install`. Prints progress and finally the doctor report to `out`.
pub fn install(sys: &dyn System, options: &InstallOptions, out: &mut dyn Write) -> Result<Report> {
    require_root(sys, "service install")?;
    let detection = install_method::detect(sys);
    let warnings = check_install_method(&detection, options.force)?;
    if let Some(bin_dir) = &options.bin_dir {
        // Validated before anything is copied; the files below then name it.
        valid_bin_dir(bin_dir)?;
    }
    check_foreign_files(
        sys,
        &packaged_files(options.bin_dir.as_deref().unwrap_or(Path::new("/usr/bin"))),
        options.force,
    )?;
    for warning in warnings {
        writeln!(out, "warning: {warning}").ok();
    }
    // A receipt that cannot be trusted is not built upon (nor are the files it lists).
    let previous = receipt::read(sys).unwrap_or_else(|err| {
        writeln!(out, "warning: {err}").ok();
        None
    });
    let binaries = install_binaries(sys, options.bin_dir.as_deref(), &detection, out)?;
    let gc_root = update_gc_root(sys, previous.as_ref(), &binaries, out);
    let stale = write_receipt(sys, previous.as_ref(), &binaries, gc_root)?;
    let bin_dir = binaries.bin_dir.clone();
    let binaries_changed = binaries.changed;

    let mut units_changed = false;
    for file in packaged_files(&bin_dir) {
        let changed = write_file(sys, &file.path, file.contents.as_bytes(), file.mode)?;
        let verb = if changed { "installed" } else { "unchanged" };
        writeln!(out, "{verb} {}", file.path.display()).ok();
        units_changed |= changed && file.path.starts_with(UNIT_DIR);
    }
    remove_stale_copies(sys, &stale, out);

    // The user first: tmpfiles.d hands directories to it.
    let sysusers = sys.path(Path::new(SYSUSERS_PATH));
    run_checked(sys, "systemd-sysusers", &[&sysusers.to_string_lossy()])?;
    if !sys.group_exists(ADMIN_GROUP) {
        run_checked(sys, "groupadd", &["--system", ADMIN_GROUP])?;
    }
    writeln!(out, "system user puzuz and group {ADMIN_GROUP} are present").ok();
    let tmpfiles = sys.path(Path::new(TMPFILES_PATH));
    run_checked(
        sys,
        "systemd-tmpfiles",
        &["--create", &tmpfiles.to_string_lossy()],
    )?;
    writeln!(out, "created the directories in {TMPFILES_PATH}").ok();

    let config = Path::new(DEFAULT_CONFIG_PATH);
    if sys.path(config).exists() {
        writeln!(out, "kept the existing {DEFAULT_CONFIG_PATH}").ok();
    } else {
        let contents = format!("{CONFIG_TEMPLATE}{}", detected_gpus_toml(sys));
        write_file(sys, config, contents.as_bytes(), 0o644)?;
        writeln!(out, "created {DEFAULT_CONFIG_PATH}").ok();
    }

    run_checked(sys, "systemctl", &["daemon-reload"])?;
    if options.no_start {
        run_checked(sys, "systemctl", &["enable", HELPER_UNIT, DAEMON_UNIT])?;
        writeln!(
            out,
            "enabled {HELPER_UNIT} and {DAEMON_UNIT} (not started: --no-start); start them with \
             `sudo systemctl start {HELPER_UNIT} {DAEMON_UNIT}`"
        )
        .ok();
    } else {
        if units_changed || binaries_changed {
            // Picks up new units and binaries if they are already running. Jobs run in their
            // own units and are not affected.
            run_checked(sys, "systemctl", &["try-restart", HELPER_UNIT, DAEMON_UNIT])?;
        }
        run_checked(
            sys,
            "systemctl",
            &["enable", "--now", HELPER_UNIT, DAEMON_UNIT],
        )?;
        writeln!(out, "enabled and started {HELPER_UNIT} and {DAEMON_UNIT}").ok();
        wait_for_daemon(sys);
    }

    writeln!(out, "\nChecking the installation (puzuz doctor):").ok();
    let report = doctor::run(sys, None);
    report.print(out).ok();
    Ok(report)
}

/// Waits until the daemon answers and is connected to the helper (or the timeout passes).
fn wait_for_daemon(sys: &dyn System) {
    let socket = doctor::load_config(sys).config.api_socket;
    let mut waited = Duration::ZERO;
    while waited < START_TIMEOUT {
        if matches!(sys.daemon_version(&socket), Ok(v) if v.helper.connected) {
            return;
        }
        sys.sleep(POLL_INTERVAL);
        waited += POLL_INTERVAL;
    }
}

pub struct UninstallOptions {
    pub purge: bool,
    pub yes: bool,
}

/// Loaded job units (`puzuz-job-<id>[-r<n>].service`) that are not inactive.
fn active_job_units(sys: &dyn System) -> Vec<String> {
    let Ok(output) = sys.run(
        "systemctl",
        &[
            "list-units",
            "--plain",
            "--no-legend",
            "--state=active,activating,deactivating,reloading",
            "puzuz-job-*",
        ],
    ) else {
        return Vec::new();
    };
    output
        .stdout
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|unit| unit.starts_with("puzuz-job-"))
        .map(str::to_owned)
        .collect()
}

/// `puzuz service uninstall`.
pub fn uninstall(sys: &dyn System, options: &UninstallOptions, out: &mut dyn Write) -> Result<()> {
    require_root(sys, "service uninstall")?;
    let method = install_method::detect(sys);
    if !method.has_service_install() {
        match method.methods().next() {
            // Removes what may be left of an earlier installation (and the data with --purge).
            None => {
                writeln!(out, "puzuz is not installed (no units found)").ok();
            }
            Some(other) => {
                return Err(format!(
                    "puzuz is installed as {other}, not by `puzuz service install`; {}",
                    method.removal_hint(other)
                ));
            }
        }
    }
    if let Some(managed) = method.managed_elsewhere()
        && options.purge
    {
        return Err(format!(
            "puzuz is also installed as {managed}, which uses {CONFIG_DIR} and \
             /var/lib/puzuz too; refusing to purge them. `sudo puzuz service uninstall` \
             (without --purge) removes only what `puzuz service install` wrote"
        ));
    }
    let jobs = active_job_units(sys);
    if !jobs.is_empty() {
        let list = jobs.join(" ");
        if options.purge {
            return Err(format!(
                "jobs are still running ({list}); purging would delete their logs and \
                 checkpoints. Stop them first (`sudo systemctl stop 'puzuz-job-*'`)."
            ));
        }
        writeln!(
            out,
            "warning: jobs keep running in their own units ({list}); stop them with \
             `sudo systemctl stop 'puzuz-job-*'` if they should not."
        )
        .ok();
    }
    if options.purge && !options.yes {
        let prompt = format!(
            "Delete {} (configuration, state, logs and checkpoint images)?",
            PURGE_DIRS.join(", ")
        );
        if !sys.confirm(&prompt) {
            return Err("aborted; nothing was changed".to_owned());
        }
    }

    // Daemon first, then the helper (it would otherwise be restarted by Wants=).
    let units_present = [DAEMON_UNIT, HELPER_UNIT]
        .iter()
        .any(|unit| sys.path(&Path::new(UNIT_DIR).join(unit)).exists());
    if units_present {
        match run_checked(
            sys,
            "systemctl",
            &["disable", "--now", DAEMON_UNIT, HELPER_UNIT],
        ) {
            Ok(_) => writeln!(out, "stopped and disabled {DAEMON_UNIT} and {HELPER_UNIT}").ok(),
            Err(err) => writeln!(out, "warning: {err}").ok(),
        };
    }

    let mut removed_units = false;
    for file in packaged_files(Path::new("/usr/bin")) {
        let local = sys.path(&file.path);
        let Ok(text) = std::fs::read_to_string(&local) else {
            continue;
        };
        if !text.starts_with(MARKER) {
            writeln!(
                out,
                "kept {} (not installed by `puzuz service install`)",
                file.path.display()
            )
            .ok();
            continue;
        }
        std::fs::remove_file(&local)
            .map_err(io_err(format!("removing {}", file.path.display())))?;
        writeln!(out, "removed {}", file.path.display()).ok();
        removed_units |= file.path.starts_with(UNIT_DIR);
    }
    if (removed_units || units_present)
        && let Err(err) = run_checked(sys, "systemctl", &["daemon-reload"])
    {
        writeln!(out, "warning: {err}").ok();
    }

    if let Some(managed) = method.managed_elsewhere() {
        writeln!(
            out,
            "puzuz is still installed as {managed}; start its services with \
             `sudo systemctl enable --now {HELPER_UNIT} {DAEMON_UNIT}`."
        )
        .ok();
    }

    if options.purge {
        let mounts = mount_points(sys);
        for dir in PURGE_DIRS {
            let path = Path::new(dir);
            if sys.path(path).symlink_metadata().is_err() {
                continue;
            }
            let skipped = remove_tree(sys, path, &mounts)?;
            for skipped in &skipped {
                writeln!(
                    out,
                    "kept {} (a mount point; unmount it and delete it by hand)",
                    skipped.display()
                )
                .ok();
            }
            if skipped.is_empty() {
                writeln!(out, "removed {dir}").ok();
            }
        }
        writeln!(
            out,
            "The system user puzuz, the group {ADMIN_GROUP} and the binaries are kept."
        )
        .ok();
    } else {
        writeln!(
            out,
            "Kept {CONFIG_DIR} and /var/lib/puzuz (state, logs, checkpoints); \
             `sudo puzuz service uninstall --purge` deletes them."
        )
        .ok();
    }
    Ok(())
}

/// Mount points (system paths) from /proc/self/mountinfo.
fn mount_points(sys: &dyn System) -> BTreeSet<PathBuf> {
    std::fs::read_to_string(sys.path(Path::new("/proc/self/mountinfo")))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split(' ').nth(4))
        .map(|field| PathBuf::from(unescape_mountinfo(field)))
        .collect()
}

/// Undoes the octal escapes (`\040` for a space) of mountinfo fields.
fn unescape_mountinfo(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = bytes
            .get(i + 1..i + 4)
            .and_then(|digits| std::str::from_utf8(digits).ok())
            .and_then(|digits| u8::from_str_radix(digits, 8).ok());
        if bytes[i] == b'\\'
            && let Some(value) = octal
        {
            out.push(value);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Deletes the system path `path` recursively without following symlinks and without entering
/// mount points (a bind mount of `/` below /run/puzuz-helper must never be descended into).
/// Returns the mount points that were left in place.
fn remove_tree(sys: &dyn System, path: &Path, mounts: &BTreeSet<PathBuf>) -> Result<Vec<PathBuf>> {
    let mut skipped = Vec::new();
    remove_tree_inner(sys, path, mounts, None, &mut skipped)?;
    Ok(skipped)
}

fn remove_tree_inner(
    sys: &dyn System,
    path: &Path,
    mounts: &BTreeSet<PathBuf>,
    parent_dev: Option<u64>,
    skipped: &mut Vec<PathBuf>,
) -> Result<bool> {
    let local = sys.path(path);
    let meta = std::fs::symlink_metadata(&local).map_err(io_err(path.display()))?;
    if !meta.is_dir() {
        std::fs::remove_file(&local).map_err(io_err(format!("removing {}", path.display())))?;
        return Ok(true);
    }
    let is_mount = mounts.contains(path) || parent_dev.is_some_and(|dev| dev != meta.dev());
    if is_mount && parent_dev.is_some() {
        skipped.push(path.to_owned());
        return Ok(false);
    }
    let mut empty = true;
    let entries =
        std::fs::read_dir(&local).map_err(io_err(format!("reading {}", path.display())))?;
    for entry in entries {
        let entry = entry.map_err(io_err(format!("reading {}", path.display())))?;
        let child = path.join(entry.file_name());
        empty &= remove_tree_inner(sys, &child, mounts, Some(meta.dev()), skipped)?;
    }
    if empty && !is_mount {
        std::fs::remove_dir(&local).map_err(io_err(format!("removing {}", path.display())))?;
        return Ok(true);
    }
    Ok(false)
}

/// `puzuz service status`. Returns whether both services run and are connected.
pub fn status(sys: &dyn System, socket: Option<&Path>, out: &mut dyn Write) -> io::Result<bool> {
    let mut healthy = true;
    let detection = install_method::detect(sys);
    healthy &= !detection.is_conflicting();
    writeln!(out, "{:<22} {}", "installed", detection.install)?;
    if let Some(partial) = &detection.partial {
        writeln!(out, "{:<22} {partial}", "")?;
    }
    for unit in [HELPER_UNIT, DAEMON_UNIT] {
        let (enabled, active) = doctor::unit_state(sys, unit);
        healthy &= active == "active";
        writeln!(out, "{unit:<22} {enabled}, {active}")?;
    }
    let socket = socket.map_or_else(
        || doctor::load_config(sys).config.api_socket,
        Path::to_owned,
    );
    match sys.daemon_version(&socket) {
        Ok(version) => {
            writeln!(
                out,
                "{:<22} reachable at {} (version {}, API v{})",
                "daemon",
                socket.display(),
                version.daemon_version,
                version.api_version
            )?;
            if version.helper.connected {
                writeln!(
                    out,
                    "{:<22} connected (version {})",
                    "helper",
                    version.helper.version.as_deref().unwrap_or("unknown")
                )?;
            } else {
                healthy = false;
                writeln!(out, "{:<22} not connected", "helper")?;
            }
        }
        Err(err) => {
            healthy = false;
            writeln!(out, "{:<22} not reachable: {err}", "daemon")?;
            writeln!(out, "{:<22} unknown", "helper")?;
        }
    }
    Ok(healthy)
}

#[cfg(test)]
mod tests;
