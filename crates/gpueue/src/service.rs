//! `gpueue service install | uninstall | status`.
//!
//! The packaging files (systemd units, sysusers.d and tmpfiles.d snippets and the default
//! configuration) are embedded in the binary, so installing needs nothing but the `gpueue` and
//! `gpueue-helper` binaries. Installed files go to /etc, the place for locally installed system
//! configuration; files owned by a distribution package (/usr/lib/...) are never touched.

use std::collections::BTreeSet;
use std::io::{self, Write};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpueue_daemon::config::DEFAULT_CONFIG_PATH;

use crate::doctor::{self, Report};
use crate::install_method::{self, Detection, Method};
use crate::system::{System, check_root_owned_chain, run_checked};

mod gc_root;
mod receipt;

pub use gc_root::GC_ROOT;
use receipt::{BINARIES as BINARY_NAMES, ReceiptState};
pub use receipt::{RECEIPT_PATH, Receipt};

pub const DAEMON_UNIT: &str = "gpueue.service";
pub const HELPER_UNIT: &str = "gpueue-helper.service";
pub const ADMIN_GROUP: &str = "gpueue-admin";

pub const UNIT_DIR: &str = "/etc/systemd/system";
pub const SYSUSERS_PATH: &str = "/etc/sysusers.d/gpueue.conf";
pub const TMPFILES_PATH: &str = "/etc/tmpfiles.d/gpueue.conf";
const CONFIG_DIR: &str = "/etc/gpueue";

/// Directories the units may run the binaries from without copying them.
const SYSTEM_BIN_DIRS: &[&str] = &["/usr/bin", "/usr/local/bin"];

/// Data removed by `uninstall --purge`.
const PURGE_DIRS: &[&str] = &[
    "/var/lib/gpueue",
    CONFIG_DIR,
    "/run/gpueue",
    "/run/gpueue-helper",
];

const DAEMON_UNIT_TEMPLATE: &str = include_str!("../../../packaging/systemd/gpueue.service");
const HELPER_UNIT_TEMPLATE: &str = include_str!("../../../packaging/systemd/gpueue-helper.service");
const SYSUSERS: &str = include_str!("../../../packaging/sysusers.d/gpueue.conf");
const TMPFILES: &str = include_str!("../../../packaging/tmpfiles.d/gpueue.conf");
const SHUTDOWN_TIMEOUT_DROP_IN: &str =
    include_str!("../../../packaging/systemd/shutdown-target-timeout.conf");

/// The targets whose `JobTimeoutSec=` (30 min, then a forced reboot or poweroff) caps the whole
/// shutdown, the helper's dumps included; [`SHUTDOWN_TIMEOUT_DROP_IN`] raises it.
const SHUTDOWN_TARGETS: &[&str] = &[
    "reboot.target",
    "poweroff.target",
    "halt.target",
    "kexec.target",
    "soft-reboot.target",
];
/// File name of the drop-in in each `<target>.d` directory.
const SHUTDOWN_DROP_IN_NAME: &str = "gpueue.conf";
pub const CONFIG_TEMPLATE: &str = include_str!("../../../packaging/config.toml");

/// First line of every file `install` writes; `uninstall` only removes files starting with it.
pub const MARKER: &str =
    "# Installed by `gpueue service install`; re-running it overwrites this file.";

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
            "`gpueue {what}` must run as root; try `sudo gpueue {what}`"
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
    let mut files = vec![
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
    ];
    files.extend(
        shutdown_drop_ins()
            .into_iter()
            .map(|path| file(path, SHUTDOWN_TIMEOUT_DROP_IN.to_owned())),
    );
    files
}

/// The drop-ins raising the shutdown targets' `JobTimeoutSec=`, one per [`SHUTDOWN_TARGETS`].
fn shutdown_drop_ins() -> Vec<PathBuf> {
    SHUTDOWN_TARGETS
        .iter()
        .map(|target| {
            Path::new(UNIT_DIR)
                .join(format!("{target}.d"))
                .join(SHUTDOWN_DROP_IN_NAME)
        })
        .collect()
}

/// Whether the system path is one of the two service units (not a drop-in or a snippet): a
/// changed unit needs the services restarted, a changed drop-in of a target only a reload.
fn is_service_unit(path: &Path) -> bool {
    path.parent() == Some(Path::new(UNIT_DIR))
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
/// umask (`sudo` may run with umask 077, which would leave e.g. /etc/gpueue unreadable for the
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

/// Whether the system path is a regular file (not a symlink) owned by root and not writable by
/// group or others.
fn root_owned_regular_file(sys: &dyn System, path: &Path) -> bool {
    std::fs::symlink_metadata(sys.path(path)).is_ok_and(|meta| {
        meta.file_type().is_file() && meta.uid() == sys.root_uid() && meta.mode() & 0o022 == 0
    })
}

/// Whether the system path already is what [`write_file`] would write: a regular file (not a
/// symlink, which may point at a file its owner can change), owned by root, with exactly
/// `mode` (never group- or world-writable) and `contents`.
fn already_written(sys: &dyn System, path: &Path, contents: &[u8], mode: u32) -> bool {
    let local = sys.path(path);
    let Ok(meta) = std::fs::symlink_metadata(&local) else {
        return false;
    };
    root_owned_regular_file(sys, path)
        && meta.mode() & 0o7777 == mode
        && meta.len() == contents.len() as u64
        // Opened without following a symlink that took its place since.
        && std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&local)
            .and_then(|mut file| {
                let mut existing = Vec::with_capacity(contents.len());
                io::Read::read_to_end(&mut file, &mut existing)?;
                Ok(existing)
            })
            .is_ok_and(|existing| existing == contents)
}

/// A file [`stage_file`] wrote next to its target (synced, with its final mode) but did not
/// put in place yet.
struct Staged {
    /// The system path it replaces.
    path: PathBuf,
    /// The temporary file (a local path) in the target's directory.
    tmp: PathBuf,
}

/// Writes `contents` to a temporary file next to the system path `path` unless the path
/// already holds them. Returns `None` if nothing needs to change.
fn stage_file(sys: &dyn System, path: &Path, contents: &[u8], mode: u32) -> Result<Option<Staged>> {
    if already_written(sys, path, contents, mode) {
        return Ok(None);
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    create_dirs(sys, parent)?;
    let tmp = sibling(&sys.path(path), "gpueue-install");
    let write = || -> io::Result<()> {
        // A leftover (or planted) temporary file or symlink is removed, not followed; the new
        // one is created exclusively and never through a symlink.
        remove_if_present(&tmp)?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
    };
    write().map_err(|err| {
        let _ = std::fs::remove_file(&tmp);
        format!("writing {}: {err}", path.display())
    })?;
    Ok(Some(Staged {
        path: path.to_owned(),
        tmp,
    }))
}

/// `.<name>.<suffix>` next to the local path `local`.
fn sibling(local: &Path, suffix: &str) -> PathBuf {
    let file_name = local.file_name().unwrap_or_default().to_string_lossy();
    local.with_file_name(format!(".{file_name}.{suffix}"))
}

/// Removes the file (or symlink) at the local path unless it is missing.
fn remove_if_present(local: &Path) -> io::Result<()> {
    match std::fs::remove_file(local) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

/// Puts the staged files in place, in order, each atomically (renaming replaces even a running
/// binary; writing into it would fail with ETXTBSY). All or none: if one cannot be put in
/// place, those already renamed are rolled back to the files they replaced (kept as hard links
/// until all are in place). Every temporary file is removed.
fn commit_staged(sys: &dyn System, staged: &[Staged]) -> Result<()> {
    commit_staged_with(sys, staged, &|from, to| std::fs::rename(from, to))
}

/// [`commit_staged`] with the rename it uses (tests make it fail).
fn commit_staged_with(
    sys: &dyn System,
    staged: &[Staged],
    rename: &dyn Fn(&Path, &Path) -> io::Result<()>,
) -> Result<()> {
    // The files put in place, each with a hard link to the file it replaced, if any.
    let mut done: Vec<(&Staged, Option<PathBuf>)> = Vec::new();
    let mut failure = None;
    for file in staged {
        let local = sys.path(&file.path);
        let commit = || -> io::Result<Option<PathBuf>> {
            let previous = match std::fs::symlink_metadata(&local) {
                Ok(_) => {
                    let backup = sibling(&local, "gpueue-previous");
                    remove_if_present(&backup)?;
                    // Links the entry itself: a symlink is not followed.
                    std::fs::hard_link(&local, &backup)?;
                    Some(backup)
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => None,
                Err(err) => return Err(err),
            };
            if let Err(err) = rename(&file.tmp, &local) {
                if let Some(backup) = &previous {
                    let _ = std::fs::remove_file(backup);
                }
                return Err(err);
            }
            Ok(previous)
        };
        match commit() {
            Ok(previous) => done.push((file, previous)),
            Err(err) => {
                failure = Some(format!("writing {}: {err}", file.path.display()));
                break;
            }
        }
    }
    let result = match failure {
        None => {
            for backup in done.iter().filter_map(|(_, previous)| previous.as_ref()) {
                let _ = std::fs::remove_file(backup);
            }
            Ok(())
        }
        Some(failure) if done.is_empty() && staged.len() > 1 => {
            Err(format!("{failure}; nothing was replaced"))
        }
        Some(failure) if done.is_empty() => Err(failure),
        Some(failure) => {
            let mut not_restored = Vec::new();
            for (file, previous) in done.iter().rev() {
                let local = sys.path(&file.path);
                let restored = match previous {
                    Some(backup) => std::fs::rename(backup, &local),
                    None => std::fs::remove_file(&local),
                };
                if let Err(err) = restored {
                    not_restored.push(format!("{}: {err}", file.path.display()));
                }
            }
            let names = done
                .iter()
                .map(|(file, _)| file.path.display().to_string())
                .collect::<Vec<_>>()
                .join(" and ");
            Err(if not_restored.is_empty() {
                format!("{failure}; restored the previous {names}")
            } else {
                format!(
                    "{failure}; could not restore the previous {names} ({})",
                    not_restored.join(", ")
                )
            })
        }
    };
    for file in staged {
        let _ = remove_if_present(&file.tmp);
    }
    result
}

/// Writes `contents` to the system path atomically unless it is already there.
/// Returns whether anything changed.
fn write_file(sys: &dyn System, path: &Path, contents: &[u8], mode: u32) -> Result<bool> {
    let Some(staged) = stage_file(sys, path, contents, mode)? else {
        return Ok(false);
    };
    commit_staged(sys, std::slice::from_ref(&staged))?;
    Ok(true)
}

fn valid_bin_dir(dir: &Path) -> Result<()> {
    let text = dir.to_str().unwrap_or("");
    let ok = dir.is_absolute()
        && !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._+-".contains(c));
    if !ok {
        return Err(format!(
            "--bin-dir {} must be an absolute path of letters, digits and / . _ + -",
            dir.display()
        ));
    }
    if text.split('/').any(|part| part == "." || part == "..") {
        return Err(format!(
            "--bin-dir {} must not contain . or .. components",
            dir.display()
        ));
    }
    Ok(())
}

/// Whether two system paths are the same file (device and inode, symlinks followed): e.g.
/// /bin/gpueue and /usr/bin/gpueue on a merged-/usr system.
fn same_file(sys: &dyn System, a: &Path, b: &Path) -> bool {
    match (
        std::fs::metadata(sys.path(a)),
        std::fs::metadata(sys.path(b)),
    ) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
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
        .map_err(io_err("cannot locate the running gpueue binary"))?;
    let exe_dir = exe
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", exe.display()))?
        .to_owned();
    let helper = exe_dir.join("gpueue-helper");
    if !sys.path(&helper).is_file() {
        return Err(format!(
            "{} not found; gpueue-helper must be installed next to gpueue",
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
    // gpueue-helper runs as root from here: whoever can replace it, or rename a parent
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
    // Nix-built binaries keep loading their loader and libraries from the store.
    gc_root::check_store_trusted(sys, &[&helper, &exe])?;
    let mut copied = Vec::new();
    let mut staged = Vec::new();
    // Both are written before either is put in place, the helper first: a failure never leaves
    // a new gpueue next to an old helper (or the other way round).
    let mut stage = || -> Result<()> {
        for (source, name) in [(&helper, "gpueue-helper"), (&exe, "gpueue")] {
            let target = bin_dir.join(name);
            // Used in place, also under another name of the same directory (/bin and /usr/bin
            // on merged /usr) — but not through a symlink or a file root does not own.
            if sys.path(source) == sys.path(&target) {
                // Run as root from where it is: nobody but root may be able to replace it (nor
                // what a symlink there points to).
                check_root_owned_chain(sys, &target).map_err(|err| {
                    format!(
                        "{} would be used in place and run as root, but {err}; make it a file \
                         only root can change",
                        target.display()
                    )
                })?;
                continue;
            }
            if same_file(sys, source, &target) && root_owned_regular_file(sys, &target) {
                continue;
            }
            let bytes = std::fs::read(sys.path(source))
                .map_err(io_err(format!("reading {}", source.display())))?;
            if let Some(file) = stage_file(sys, &target, &bytes, 0o755)? {
                staged.push(file);
            }
            copied.push(target);
        }
        Ok(())
    };
    if let Err(err) = stage() {
        for file in &staged {
            let _ = remove_if_present(&file.tmp);
        }
        return Err(format!(
            "{err}; the binaries in {} were not changed",
            bin_dir.display()
        ));
    }
    commit_staged(sys, &staged)?;
    for file in &staged {
        writeln!(out, "installed {}", file.path.display()).ok();
    }
    let changed = !staged.is_empty();
    copied.sort();
    Ok(Binaries {
        bin_dir,
        changed,
        copied,
        store_path: gc_root::store_path_of(sys, &exe),
    })
}

/// Writes the install receipt for `binaries`. A re-install into the same directory keeps the
/// copies recorded before (re-running the installed copy uses it in place); copies the
/// previous installation left in another directory are returned for removal.
fn write_receipt(
    sys: &dyn System,
    previous: Option<&Receipt>,
    binaries: &Binaries,
    (gc_root, gc_root_target): gc_root::GcRoot,
) -> Result<Vec<PathBuf>> {
    let mut copied = binaries.copied.clone();
    let mut stale = Vec::new();
    if let Some(previous) = previous {
        // The same directory under another name (/bin and /usr/bin on merged /usr) is the same
        // installation: its copies are the binaries just installed, never stale ones.
        let same_dir = previous.bin_dir == binaries.bin_dir
            || same_file(sys, &previous.bin_dir, &binaries.bin_dir);
        for path in previous.removable_copies() {
            let current = path
                .file_name()
                .map(|name| binaries.bin_dir.join(name))
                .unwrap_or_else(|| path.clone());
            if same_dir {
                if !copied.contains(&current) {
                    copied.push(current);
                }
            } else if !BINARY_NAMES
                .iter()
                .any(|name| same_file(sys, &path, &binaries.bin_dir.join(name)))
            {
                stale.push(path);
            }
        }
    }
    copied.sort();
    receipt::write(
        sys,
        &Receipt {
            version: gpueue_protocol::BINARY_VERSION.to_owned(),
            bin_dir: binaries.bin_dir.clone(),
            copied,
            gc_root,
            gc_root_target,
        },
    )?;
    Ok(stale)
}

/// After `gpueue update` installed the binaries of a release tarball over an installation from
/// the Nix store: once the installed binaries no longer use the store, the GC root recorded in
/// the receipt is removed and the receipt updated. (The new `service install` does this
/// itself; this also covers a release that predates the receipt.)
pub fn release_gc_root(sys: &dyn System, version: &str, out: &mut dyn Write) -> Result<()> {
    let ReceiptState::Valid(mut receipt) = receipt::read_or_warn(sys, out) else {
        return Ok(());
    };
    let Some(root) = receipt.gc_root.clone() else {
        return Ok(());
    };
    if gc_root::binaries_need_store(sys, &receipt.bin_dir) {
        return Ok(());
    }
    gc_root::remove_gc_root(sys, &root, out);
    receipt.gc_root = None;
    receipt.gc_root_target = None;
    receipt.version = version.to_owned();
    receipt::write(sys, &receipt)
}

/// GPUs reported by nvidia-smi as `[[gpus]]` entries for a new configuration file.
fn detected_gpus_toml(sys: &dyn System) -> String {
    // Run as root: only an nvidia-smi nobody else can replace.
    let crate::system::Program::Found(smi) =
        crate::system::find_runnable_program(sys, Path::new("nvidia-smi"))
    else {
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
        format!("\n# Detected with nvidia-smi by `gpueue service install`.{toml}")
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
        "{} {} not written by `gpueue service install`; refusing to overwrite. Customize the \
         services with drop-ins (`sudo systemctl edit {DAEMON_UNIT}`) instead, or re-run with \
         --force to replace them",
        foreign.join(", "),
        if foreign.len() == 1 { "was" } else { "were" }
    ))
}

/// Refuses to touch a machine where a package manager or Nix manages gpueue: the units in /etc
/// would shadow the packaged ones, and both would share /etc/gpueue and /var/lib/gpueue.
/// `--force` does not override this. A unit in /usr/lib that no package owns is refused
/// without `--force`. Returns warnings to print before installing.
fn check_install_method(detection: &Detection, force: bool) -> Result<Vec<String>> {
    if let Some(managed) = detection.managed_elsewhere() {
        let mut message = format!(
            "gpueue is installed as {managed}; `gpueue service install` would conflict with it \
             (--force does not change this). To update it, {}.",
            detection.update_hint(managed)
        );
        if detection.has_service_install() {
            message.push_str(
                " It is also installed by `gpueue service install`; remove that copy with \
                 `sudo gpueue uninstall`.",
            );
        } else {
            message.push_str(&format!(
                " To switch to `gpueue service install`, first {}.",
                detection.removal_hint(managed)
            ));
        }
        return Err(message);
    }
    if detection.os.is_nixos() {
        return Err(
            "this is NixOS: install gpueue with the `services.gpueue` NixOS module instead of \
             `gpueue service install` (--force does not change this)"
                .to_owned(),
        );
    }
    if detection.unit_dir_in_store {
        return Err(format!(
            "{UNIT_DIR} resolves into /nix/store: Nix manages it, and it cannot be written. \
             Install gpueue through the Nix configuration that manages it (--force does not \
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

/// `gpueue service install`. Prints progress and finally the doctor report to `out`.
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
    // A receipt that cannot be used is neither built upon nor overwritten: it may be the only
    // record of copies and a GC root that would otherwise be forgotten.
    let previous = match receipt::read_or_warn(sys, out) {
        ReceiptState::Valid(receipt) => Some(receipt),
        ReceiptState::Missing => None,
        ReceiptState::Invalid(_) => {
            return Err(format!(
                "refusing to replace {RECEIPT_PATH} (see the warning above): it records which \
                 binaries gpueue copied and its Nix GC root. Fix it, or delete it (`sudo rm \
                 {RECEIPT_PATH}`) after checking what it lists, and re-run"
            ));
        }
    };
    let binaries = install_binaries(sys, options.bin_dir.as_deref(), &detection, out)?;
    let previous_root = previous.as_ref().map_or((None, None), |p| {
        (p.gc_root.clone(), p.gc_root_target.clone())
    });
    let gc_root = gc_root::decide_gc_root(
        sys,
        &binaries.bin_dir,
        binaries.store_path.as_deref(),
        previous_root,
        out,
    );
    let stale = write_receipt(sys, previous.as_ref(), &binaries, gc_root)?;
    let bin_dir = binaries.bin_dir.clone();
    let binaries_changed = binaries.changed;

    let mut units_changed = false;
    for file in packaged_files(&bin_dir) {
        let changed = write_file(sys, &file.path, file.contents.as_bytes(), file.mode)?;
        let verb = if changed { "installed" } else { "unchanged" };
        writeln!(out, "{verb} {}", file.path.display()).ok();
        units_changed |= changed && is_service_unit(&file.path);
    }
    let left = receipt::remove_copies(sys, &stale, " (copied by the previous install)", out);
    if !left.is_empty() {
        writeln!(
            out,
            "warning: remove {} by hand",
            left.iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(" ")
        )
        .ok();
    }

    // The user first: tmpfiles.d hands directories to it.
    let sysusers = sys.path(Path::new(SYSUSERS_PATH));
    run_checked(sys, "systemd-sysusers", &[&sysusers.to_string_lossy()])?;
    if !sys.group_exists(ADMIN_GROUP) {
        run_checked(sys, "groupadd", &["--system", ADMIN_GROUP])?;
    }
    writeln!(
        out,
        "system user gpueue and group {ADMIN_GROUP} are present"
    )
    .ok();
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

    writeln!(out, "\nChecking the installation (gpueue doctor):").ok();
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

/// Loaded job units (`gpueue-job-<id>[-r<n>].service`) that are not inactive. An error if
/// systemd cannot be asked: that is not the same as "no jobs".
fn active_job_units(sys: &dyn System) -> Result<Vec<String>> {
    let output = run_checked(
        sys,
        "systemctl",
        &[
            "list-units",
            "--plain",
            "--no-legend",
            "--state=active,activating,deactivating,reloading",
            "gpueue-job-*",
        ],
    )?;
    Ok(output
        .stdout
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|unit| unit.starts_with("gpueue-job-"))
        .map(str::to_owned)
        .collect())
}

/// Removes what the install receipt lists: the binaries `service install` copied (never
/// binaries it used in place, nor files a package owns, nor anything else), the Nix GC root and
/// the receipt itself. Without a usable receipt the binaries are kept. A receipt is kept (with
/// what is left) when a binary could not be removed, unless `purge` deletes it anyway.
fn remove_binaries(
    sys: &dyn System,
    state: &ReceiptState,
    detection: &Detection,
    purge: bool,
    out: &mut dyn Write,
) {
    let kept = || {
        installed_bin_dir(detection).map_or_else(
            || "gpueue and gpueue-helper".to_owned(),
            Receipt::binaries_in,
        )
    };
    let receipt =
        match state {
            ReceiptState::Valid(receipt) => receipt,
            ReceiptState::Missing => {
                if detection.has_service_install() {
                    writeln!(
                    out,
                    "kept the binaries: {RECEIPT_PATH} is missing (installed by an older gpueue), \
                     so what was copied is unknown; remove {} by hand unless something else \
                     uses them",
                    kept()
                )
                .ok();
                }
                return;
            }
            ReceiptState::Invalid(_) => {
                writeln!(
                out,
                "kept the binaries and any Nix GC root, since {RECEIPT_PATH} cannot be used (see \
                 the warning above); remove {} (and {GC_ROOT}, if present) by hand unless \
                 something else uses them{}",
                kept(),
                if purge { "" } else { ", then delete the receipt" }
            )
            .ok();
                return;
            }
        };
    let removable = receipt.removable_copies();
    for path in receipt.copied.iter().filter(|p| !removable.contains(p)) {
        writeln!(
            out,
            "warning: ignoring {} in {RECEIPT_PATH} (not a gpueue binary in {})",
            path.display(),
            receipt.bin_dir.display()
        )
        .ok();
    }
    let left = receipt::remove_copies(sys, &removable, "", out);
    if receipt.copied.is_empty() {
        writeln!(
            out,
            "kept {} (used in place, not copied by `gpueue service install`)",
            Receipt::binaries_in(&receipt.bin_dir).replace(' ', " and ")
        )
        .ok();
    }
    if let Some(root) = &receipt.gc_root {
        gc_root::remove_gc_root(sys, root, out);
    }
    if left.is_empty() {
        receipt::remove(sys, out);
        return;
    }
    let list = left
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(" ");
    if purge {
        writeln!(out, "warning: could not remove {list}; remove them by hand").ok();
        return;
    }
    let rest = Receipt {
        copied: left,
        gc_root: None,
        gc_root_target: None,
        ..receipt.clone()
    };
    match receipt::write(sys, &rest) {
        Ok(()) => writeln!(
            out,
            "warning: could not remove {list}; kept {RECEIPT_PATH} listing them (re-run \
             `sudo gpueue uninstall`, or remove them by hand)"
        ),
        Err(err) => writeln!(out, "warning: could not remove {list}; {err}"),
    }
    .ok();
}

/// `gpueue uninstall` (also `gpueue service uninstall`).
pub fn uninstall(sys: &dyn System, options: &UninstallOptions, out: &mut dyn Write) -> Result<()> {
    require_root(sys, "uninstall")?;
    let method = install_method::detect(sys);
    if !method.has_service_install() {
        match method.methods().next() {
            // Removes what may be left of an earlier installation (and the data with --purge).
            None => {
                writeln!(out, "gpueue is not installed (no units found)").ok();
            }
            Some(other) => {
                return Err(format!(
                    "gpueue is installed as {other}, not by `gpueue service install`; {}",
                    method.removal_hint(other)
                ));
            }
        }
    }
    if let Some(managed) = method.managed_elsewhere()
        && options.purge
    {
        return Err(format!(
            "gpueue is also installed as {managed}, which uses {CONFIG_DIR} and \
             /var/lib/gpueue too; refusing to purge them. `sudo gpueue uninstall` \
             (without --purge) removes only what `gpueue service install` wrote"
        ));
    }
    let jobs = match active_job_units(sys) {
        Ok(jobs) => jobs,
        Err(err) if options.purge => {
            return Err(format!(
                "cannot tell whether jobs are still running ({err}); refusing to purge, which \
                 would delete their logs and checkpoints"
            ));
        }
        Err(err) => {
            writeln!(
                out,
                "warning: cannot tell whether jobs are still running ({err}); jobs keep \
                 running in their own units"
            )
            .ok();
            Vec::new()
        }
    };
    if !jobs.is_empty() {
        let list = jobs.join(" ");
        if options.purge {
            return Err(format!(
                "jobs are still running ({list}); purging would delete their logs and \
                 checkpoints. Stop them first (`sudo systemctl stop 'gpueue-job-*'`)."
            ));
        }
        writeln!(
            out,
            "warning: jobs keep running in their own units ({list}); stop them with \
             `sudo systemctl stop 'gpueue-job-*'` if they should not."
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

    // Read before anything is removed (and before --purge deletes it).
    let receipt = receipt::read_or_warn(sys, out);

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
                "kept {} (not installed by `gpueue service install`)",
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
    // The `<target>.d` directories `install` created, unless something else is in them.
    for drop_in in shutdown_drop_ins() {
        if let Some(dir) = drop_in.parent() {
            let _ = std::fs::remove_dir(sys.path(dir));
        }
    }
    if (removed_units || units_present)
        && let Err(err) = run_checked(sys, "systemctl", &["daemon-reload"])
    {
        writeln!(out, "warning: {err}").ok();
    }

    remove_binaries(sys, &receipt, &method, options.purge, out);

    if let Some(managed) = method.managed_elsewhere() {
        writeln!(
            out,
            "gpueue is still installed as {managed}; start its services with \
             `sudo systemctl enable --now {HELPER_UNIT} {DAEMON_UNIT}`."
        )
        .ok();
    }

    if options.purge {
        purge(sys, &remove_dir, out)?;
        writeln!(
            out,
            "The system user gpueue and the group {ADMIN_GROUP} are kept."
        )
        .ok();
    } else {
        writeln!(
            out,
            "Kept {CONFIG_DIR} and /var/lib/gpueue (state, logs, checkpoints); \
             `sudo gpueue uninstall --purge` deletes them."
        )
        .ok();
    }
    Ok(())
}

/// Removes an empty directory: `name` in the directory `parent`.
type RemoveDir<'a> = dyn Fn(&std::os::fd::OwnedFd, &std::ffi::OsStr) -> nix::Result<()> + 'a;

/// [`RemoveDir`] with `unlinkat(AT_REMOVEDIR)`.
fn remove_dir(parent: &std::os::fd::OwnedFd, name: &std::ffi::OsStr) -> nix::Result<()> {
    nix::unistd::unlinkat(parent, name, nix::unistd::UnlinkatFlags::RemoveDir)
}

/// Deletes [`PURGE_DIRS`] with [`remove_tree`], reporting what is kept. What cannot be deleted
/// below a purge directory (a mount point, or a directory a user keeps adding files to) is
/// reported and the rest is still deleted.
fn purge(sys: &dyn System, remove_dir: &RemoveDir<'_>, out: &mut dyn Write) -> Result<()> {
    let mounts = mount_points(sys);
    for dir in PURGE_DIRS {
        let path = Path::new(dir);
        if sys.path(path).symlink_metadata().is_err() {
            continue;
        }
        let purged = remove_tree(sys, path, &mounts, remove_dir)?;
        for skipped in &purged.kept_mounts {
            writeln!(
                out,
                "kept {} (a mount point; unmount it and delete it by hand)",
                skipped.display()
            )
            .ok();
        }
        for skipped in &purged.kept_not_empty {
            writeln!(
                out,
                "kept {} (files were added to it while it was deleted; delete it by hand)",
                skipped.display()
            )
            .ok();
        }
        if purged.removed {
            writeln!(out, "removed {dir}").ok();
        } else if purged.is_mount {
            writeln!(out, "kept {dir} (a mount point; deleted what was in it)").ok();
        } else {
            writeln!(out, "kept {dir} (deleted what could be deleted; see above)").ok();
        }
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

/// What [`remove_tree`] did with a purge directory.
#[derive(Debug, Default, PartialEq, Eq)]
struct Purged {
    /// The directory itself is gone.
    removed: bool,
    /// Mount points below it that were left in place (system paths).
    kept_mounts: Vec<PathBuf>,
    /// The directory itself is a mount point: its contents were deleted, the directory kept.
    is_mount: bool,
    /// Directories below it that were not empty any more when they were to be removed (system
    /// paths).
    kept_not_empty: Vec<PathBuf>,
}

/// Deletes the system path `path` recursively without following symlinks and without entering
/// mount points (a bind mount of `/` below /run/gpueue-helper must never be descended into).
///
/// Users own directories inside the purged trees (`/var/lib/gpueue/logs/<uid>/`), so the walk
/// works on directory file descriptors (`openat(O_NOFOLLOW)`, `unlinkat`), never on path names
/// that could be swapped for a symlink between a check and its use (CVE-2022-21658).
/// `remove_dir` removes the emptied directories.
fn remove_tree(
    sys: &dyn System,
    path: &Path,
    mounts: &BTreeSet<PathBuf>,
    remove_dir: &RemoveDir<'_>,
) -> Result<Purged> {
    use nix::fcntl::{OFlag, open};
    use nix::sys::stat::Mode;

    let local = sys.path(path);
    let (Some(parent), Some(name)) = (local.parent(), local.file_name()) else {
        return Err(format!("refusing to remove {}", path.display()));
    };
    // The parents (/var/lib, /etc, /run) are root's; only what is below them is walked by fd.
    let parent_fd = open(
        parent,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|err| format!("opening {}: {err}", parent.display()))?;
    let parent_dev = nix::sys::stat::fstat(&parent_fd)
        .map_err(|err| format!("reading {}: {err}", parent.display()))?
        .st_dev;
    let mut purged = Purged::default();
    let at = Location {
        parent: &parent_fd,
        name,
        path,
        parent_dev,
        remove_dir,
    };
    purged.removed = remove_entry(&at, mounts, true, &mut purged)?;
    Ok(purged)
}

/// An entry [`remove_entry`] removes: `name` in the directory `parent` (on the device
/// `parent_dev`), the system path `path`; directories are removed with `remove_dir`.
struct Location<'a> {
    parent: &'a std::os::fd::OwnedFd,
    name: &'a std::ffi::OsStr,
    path: &'a Path,
    parent_dev: u64,
    remove_dir: &'a RemoveDir<'a>,
}

/// Removes the entry; returns whether it is gone. `top` is the purge directory itself, which
/// is emptied even if it is a mount point.
fn remove_entry(
    at: &Location<'_>,
    mounts: &BTreeSet<PathBuf>,
    top: bool,
    purged: &mut Purged,
) -> Result<bool> {
    use nix::errno::Errno;
    use nix::fcntl::{OFlag, openat};
    use nix::sys::stat::Mode;
    use nix::unistd::{UnlinkatFlags, unlinkat};

    let path = at.path;
    let removing = |err: Errno| format!("removing {}: {err}", path.display());
    let reading = |err: Errno| format!("reading {}: {err}", path.display());
    let dir = match openat(
        at.parent,
        at.name,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    ) {
        Ok(dir) => dir,
        // Not a directory (a symlink is never followed): unlink the entry itself. unlinkat
        // does not follow symlinks either, and fails if a directory took its place.
        Err(Errno::ENOTDIR | Errno::ELOOP) => {
            unlinkat(at.parent, at.name, UnlinkatFlags::NoRemoveDir).map_err(removing)?;
            return Ok(true);
        }
        Err(Errno::ENOENT) => return Ok(true),
        Err(err) => return Err(format!("opening {}: {err}", path.display())),
    };
    let dev = nix::sys::stat::fstat(&dir).map_err(reading)?.st_dev;
    let is_mount = mounts.contains(path) || dev != at.parent_dev;
    if is_mount {
        if !top {
            purged.kept_mounts.push(path.to_owned());
            return Ok(false);
        }
        purged.is_mount = true;
    }
    let mut names = Vec::new();
    let listing = dir
        .try_clone()
        .map_err(|err| format!("reading {}: {err}", path.display()))?;
    for entry in nix::dir::Dir::from_fd(listing)
        .map_err(reading)?
        .into_iter()
    {
        let entry = entry.map_err(reading)?;
        let bytes = entry.file_name().to_bytes();
        if bytes != b"." && bytes != b".." {
            names.push(std::ffi::OsString::from_vec(bytes.to_vec()));
        }
    }
    let mut empty = true;
    for name in &names {
        let child = Location {
            parent: &dir,
            name,
            path: &path.join(name),
            parent_dev: dev,
            remove_dir: at.remove_dir,
        };
        empty &= remove_entry(&child, mounts, false, purged)?;
    }
    drop(dir);
    if empty && !is_mount {
        return match (at.remove_dir)(at.parent, at.name) {
            Ok(()) => Ok(true),
            // A user added a file to their directory (`/var/lib/gpueue/logs/<uid>/`) since it
            // was listed: it is kept, like a mount point, and the purge goes on.
            Err(Errno::ENOTEMPTY | Errno::EEXIST) if !top => {
                purged.kept_not_empty.push(path.to_owned());
                Ok(false)
            }
            Err(err) => Err(removing(err)),
        };
    }
    Ok(false)
}

/// `gpueue service status`. Returns whether both services run and are connected.
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
