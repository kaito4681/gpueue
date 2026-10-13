//! How puzuz was installed on this machine: by `puzuz service install`, by a distribution
//! package, by Nix (the NixOS module, or another Nix tool), by hand, or not at all.
//!
//! `service install`/`uninstall`, `update` and `doctor` use this so that a package manager and
//! `puzuz service install` never fight over the same machine. `install.sh` asks the same
//! question through `puzuz service install-method`.
//!
//! Both units (`puzuz.service` and `puzuz-helper.service`) are classified the same way:
//! 1. `/etc/systemd/system/<unit>`: missing; a symlink to /dev/null (`systemctl mask`); a
//!    symlink that resolves into /nix/store (Nix); a file starting with the `service install`
//!    marker; or any other file (written by hand).
//! 2. `<unit>` in the distribution's unit directory (`/usr/lib/systemd/system`, or
//!    `/lib/systemd/system`): one that resolves into /nix/store or runs a binary from there is
//!    Nix; otherwise `dpkg-query -S`, `rpm -qf` and `pacman -Qo` are asked who owns it (under
//!    both paths, since on merged-/usr systems dpkg may have registered the /lib one). Owned
//!    by a package: [`Method::Package`]; nobody owns it: [`Method::Unowned`]; none of the tools
//!    exists: a package of unknown origin (refused like any package).
//! 3. Next to a packaged (or unowned) unit, a mask or a hand-written /etc unit is the
//!    administrator's change of it ([`LocalUnit`]), not a second installation. A hand-written
//!    /etc unit alone is [`Method::Manual`], even when it runs a binary from /nix/store. A
//!    `service install` unit next to a packaged or Nix unit is [`InstallMethod::Mixed`]; next
//!    to an unowned one it stays a `service install` (its /etc unit wins), with the unowned
//!    file reported as a leftover.
//!
//! If the two units disagree (or only one exists) the installation is [`Detection::partial`].
//!
//! The operating system comes from `/etc/os-release` (or `/usr/lib/os-release`). It only
//! decides wording (and that NixOS must use its module); it never decides the install method,
//! since the same distribution can have puzuz from a package or from `install.sh`.

use std::ffi::OsString;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use crate::doctor::exec_start_binary;
use crate::service::{DAEMON_UNIT, HELPER_UNIT, MARKER, UNIT_DIR};
use crate::system::System;

/// Unit directories a distribution package installs into.
const PACKAGE_UNIT_DIRS: &[&str] = &["/usr/lib/systemd/system", "/lib/systemd/system"];
const NIX_STORE: &str = "/nix/store";
const OS_RELEASE_FILES: &[&str] = &["/etc/os-release", "/usr/lib/os-release"];
/// Symlinks followed while resolving a path, like the kernel's limit.
const MAX_SYMLINKS: usize = 40;

/// `ID` and `ID_LIKE` of os-release(5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OsRelease {
    pub id: String,
    pub id_like: Vec<String>,
}

/// Distribution families, for naming the package manager in messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Debian,
    Rhel,
    Arch,
    Suse,
}

impl OsRelease {
    /// Reads `/etc/os-release`, else `/usr/lib/os-release`; empty if neither can be read.
    pub fn read(sys: &dyn System) -> Self {
        OS_RELEASE_FILES
            .iter()
            .find_map(|file| std::fs::read_to_string(sys.path(Path::new(file))).ok())
            .map(|text| Self::parse(&text))
            .unwrap_or_default()
    }

    pub fn parse(text: &str) -> Self {
        let mut release = Self::default();
        for line in text.lines() {
            let Some((key, value)) = line.trim().split_once('=') else {
                continue;
            };
            let value = value.trim().trim_matches(['"', '\'']);
            match key.trim() {
                "ID" => release.id = value.to_owned(),
                "ID_LIKE" => {
                    release.id_like = value.split_whitespace().map(str::to_owned).collect();
                }
                _ => {}
            }
        }
        release
    }

    pub fn is_nixos(&self) -> bool {
        self.id == "nixos"
    }

    /// The family of `ID`, else the first of `ID_LIKE` that is known.
    pub fn family(&self) -> Option<Family> {
        std::iter::once(&self.id)
            .chain(&self.id_like)
            .find_map(|id| match id.as_str() {
                "debian" | "ubuntu" => Some(Family::Debian),
                "fedora" | "rhel" | "centos" => Some(Family::Rhel),
                "arch" => Some(Family::Arch),
                "suse" => Some(Family::Suse),
                other if other.starts_with("opensuse") => Some(Family::Suse),
                _ => None,
            })
    }
}

/// The package that owns a packaged unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    Dpkg(String),
    Rpm(String),
    Pacman(String),
    /// No package query tool exists, so the owner cannot be asked.
    Unknown,
}

/// One way puzuz is installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    /// `puzuz service install`; the units run the binaries in `bin_dir` (when it can be read
    /// from the unit).
    ServiceInstall { bin_dir: Option<PathBuf> },
    /// A distribution package: its unit file is at `unit`, possibly masked or replaced by the
    /// administrator in /etc/systemd/system (`local`).
    Package {
        unit: PathBuf,
        owner: Owner,
        local: Option<LocalUnit>,
    },
    /// A unit in the distribution's unit directory that no package owns: left over from a
    /// removed package, or copied there by hand (`make install`).
    Unowned {
        unit: PathBuf,
        local: Option<LocalUnit>,
    },
    /// Units managed by Nix (the NixOS `services.puzuz` module, or e.g. system-manager): `unit`
    /// resolves into, or runs a binary from, /nix/store.
    Nix { unit: PathBuf },
    /// `/etc/systemd/system/<unit>` written by someone else (an administrator, or `service
    /// install` before the marker existed).
    Manual { unit: PathBuf },
}

/// What the administrator did to a packaged unit in /etc/systemd/system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalUnit {
    /// `systemctl mask`: the unit is a symlink to /dev/null.
    Masked(PathBuf),
    /// `systemctl edit --full`, or a copy: a unit without the `service install` marker that
    /// replaces the packaged one.
    Overridden(PathBuf),
}

/// `(update, removal)` command examples for a package of unknown origin.
fn family_examples(os: &OsRelease) -> Option<(&'static str, &'static str)> {
    Some(match os.family()? {
        Family::Debian => (
            "sudo apt update && sudo apt upgrade",
            "sudo apt remove <package>",
        ),
        Family::Rhel => ("sudo dnf upgrade", "sudo dnf remove <package>"),
        Family::Arch => ("sudo pacman -Syu", "sudo pacman -R <package>"),
        Family::Suse => ("sudo zypper update", "sudo zypper remove <package>"),
    })
}

impl Method {
    /// Whether something else than puzuz owns this installation, so that `service install`
    /// and `update` must not change it (not even with `--force`).
    pub fn is_managed_elsewhere(&self) -> bool {
        matches!(self, Method::Package { .. } | Method::Nix { .. })
    }

    /// How to update puzuz installed this way.
    pub fn update_hint(&self, os: &OsRelease) -> String {
        match self {
            Method::ServiceInstall { .. } => "run `sudo puzuz update`".to_owned(),
            Method::Package { owner, .. } => match owner {
                Owner::Dpkg(_) => "run `sudo apt update && sudo apt upgrade`".to_owned(),
                Owner::Rpm(_) => "run `sudo dnf upgrade`".to_owned(),
                Owner::Pacman(_) => "run `yay -Syu` (AUR) or `sudo pacman -Syu`".to_owned(),
                Owner::Unknown => match family_examples(os) {
                    Some((update, _)) => {
                        format!("update the package with your package manager (e.g. `{update}`)")
                    }
                    None => "update the package with your package manager".to_owned(),
                },
            },
            Method::Unowned { unit, .. } => format!(
                "no package owns {}; delete it and install puzuz with install.sh or `sudo \
                 puzuz service install`",
                unit.display()
            ),
            Method::Nix { unit } => {
                if os.is_nixos() {
                    "run `nix flake update && sudo nixos-rebuild switch`".to_owned()
                } else {
                    format!(
                        "change the Nix configuration that created {}",
                        unit.display()
                    )
                }
            }
            Method::Manual { unit } => format!(
                "{} was not written by `puzuz service install`; replace the binaries it runs \
                 by hand, or remove it and run `sudo puzuz service install --bin-dir \
                 /usr/local/bin`",
                unit.display()
            ),
        }
    }

    /// How to remove puzuz installed this way.
    pub fn removal_hint(&self, os: &OsRelease) -> String {
        match self {
            Method::ServiceInstall { .. } => "run `sudo puzuz service uninstall`".to_owned(),
            Method::Package { owner, .. } => match owner {
                Owner::Dpkg(name) => format!("run `sudo apt remove {name}`"),
                Owner::Rpm(name) => format!("run `sudo dnf remove {name}`"),
                Owner::Pacman(name) => format!("run `sudo pacman -R {name}`"),
                Owner::Unknown => match family_examples(os) {
                    Some((_, remove)) => {
                        format!("remove the package with your package manager (e.g. `{remove}`)")
                    }
                    None => "remove the package with your package manager".to_owned(),
                },
            },
            Method::Unowned { unit, .. } => format!(
                "delete {} by hand (no package owns it) and run `sudo systemctl daemon-reload`",
                unit.display()
            ),
            Method::Nix { unit } => {
                if os.is_nixos() {
                    "set `services.puzuz.enable = false` and run `sudo nixos-rebuild switch`"
                        .to_owned()
                } else {
                    format!(
                        "remove puzuz from the Nix configuration that created {}",
                        unit.display()
                    )
                }
            }
            Method::Manual { unit } => format!("remove {} by hand", unit.display()),
        }
    }

    /// The same way of installing, ignoring which unit it was seen on (for comparing the
    /// daemon's and the helper's units).
    fn same_kind(&self, other: &Method) -> bool {
        match (self, other) {
            (Method::ServiceInstall { bin_dir: a }, Method::ServiceInstall { bin_dir: b }) => {
                a == b
            }
            (Method::Package { owner: a, .. }, Method::Package { owner: b, .. }) => a == b,
            (Method::Unowned { .. }, Method::Unowned { .. })
            | (Method::Nix { .. }, Method::Nix { .. })
            | (Method::Manual { .. }, Method::Manual { .. }) => true,
            _ => false,
        }
    }
}

fn fmt_local(f: &mut fmt::Formatter<'_>, local: &Option<LocalUnit>) -> fmt::Result {
    match local {
        None => Ok(()),
        Some(LocalUnit::Masked(etc)) => write!(f, "; masked by {}", etc.display()),
        Some(LocalUnit::Overridden(etc)) => write!(f, "; overridden by {}", etc.display()),
    }
}

impl fmt::Display for Owner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Owner::Dpkg(name) => write!(f, "{name}, dpkg"),
            Owner::Rpm(name) => write!(f, "{name}, rpm"),
            Owner::Pacman(name) => write!(f, "{name}, pacman"),
            Owner::Unknown => f.write_str("owner unknown"),
        }
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Method::ServiceInstall { bin_dir: Some(dir) } => {
                write!(f, "puzuz service install (binaries in {})", dir.display())
            }
            Method::ServiceInstall { bin_dir: None } => f.write_str("puzuz service install"),
            Method::Package { unit, owner, local } => {
                write!(f, "distribution package ({owner}: {}", unit.display())?;
                fmt_local(f, local)?;
                f.write_str(")")
            }
            Method::Unowned { unit, local } => {
                write!(f, "a unit no package owns ({}", unit.display())?;
                fmt_local(f, local)?;
                f.write_str(")")
            }
            Method::Nix { unit } => write!(f, "Nix ({} is in /nix/store)", unit.display()),
            Method::Manual { unit } => write!(
                f,
                "by hand ({} was not written by `puzuz service install`)",
                unit.display()
            ),
        }
    }
}

/// Every way puzuz is installed, as seen on one unit (or both, see [`Detection::install`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallMethod {
    /// No puzuz units anywhere.
    None,
    One(Method),
    /// More than one: the units of one shadow the other's (a unit in /etc overrides one in
    /// /usr/lib), and they share /etc/puzuz and /var/lib/puzuz.
    Mixed(Vec<Method>),
}

impl InstallMethod {
    pub fn methods(&self) -> &[Method] {
        match self {
            InstallMethod::None => &[],
            InstallMethod::One(method) => std::slice::from_ref(method),
            InstallMethod::Mixed(methods) => methods,
        }
    }

    /// The single machine-readable name of the installation: `none`, `service-install`,
    /// `package`, `unowned`, `nix`, `manual` or `mixed`.
    pub fn keyword(&self) -> &'static str {
        match self {
            InstallMethod::None => "none",
            InstallMethod::Mixed(_) => "mixed",
            InstallMethod::One(method) => match method {
                Method::ServiceInstall { .. } => "service-install",
                Method::Package { .. } => "package",
                Method::Unowned { .. } => "unowned",
                Method::Nix { .. } => "nix",
                Method::Manual { .. } => "manual",
            },
        }
    }

    fn same_kind(&self, other: &InstallMethod) -> bool {
        let (a, b) = (self.methods(), other.methods());
        a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.same_kind(b))
    }

    fn from_methods(mut methods: Vec<Method>) -> Self {
        match methods.len() {
            0 => InstallMethod::None,
            1 => InstallMethod::One(methods.remove(0)),
            _ => InstallMethod::Mixed(methods),
        }
    }
}

impl fmt::Display for InstallMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InstallMethod::None => f.write_str("not installed"),
            InstallMethod::One(method) => method.fmt(f),
            InstallMethod::Mixed(methods) => {
                f.write_str("mixed: ")?;
                for (i, method) in methods.iter().enumerate() {
                    if i > 0 {
                        f.write_str(" and ")?;
                    }
                    method.fmt(f)?;
                }
                Ok(())
            }
        }
    }
}

/// Everything [`detect`] found out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    pub os: OsRelease,
    /// How `puzuz.service` is installed, or `puzuz-helper.service` if there is no daemon unit.
    pub install: InstallMethod,
    /// How `puzuz-helper.service` is installed.
    pub helper: InstallMethod,
    /// Why the daemon's and the helper's units do not belong together, if they do not.
    pub partial: Option<String>,
    /// Units in the distribution's unit directory that no package owns, next to a `service
    /// install` unit that shadows them.
    pub leftovers: Vec<PathBuf>,
    /// /etc/systemd/system resolves into /nix/store (and cannot be written).
    pub unit_dir_in_store: bool,
}

impl Detection {
    pub fn keyword(&self) -> &'static str {
        self.install.keyword()
    }

    /// The methods of both units (each once).
    pub fn methods(&self) -> impl Iterator<Item = &Method> {
        let daemon = self.install.methods();
        daemon.iter().chain(
            self.helper
                .methods()
                .iter()
                .filter(move |method| !daemon.contains(method)),
        )
    }

    /// A unit of puzuz is owned by a package manager or Nix.
    pub fn managed_elsewhere(&self) -> Option<&Method> {
        self.methods().find(|m| m.is_managed_elsewhere())
    }

    /// A unit of puzuz was written by `puzuz service install`.
    pub fn has_service_install(&self) -> bool {
        self.methods()
            .any(|m| matches!(m, Method::ServiceInstall { .. }))
    }

    pub fn update_hint(&self, method: &Method) -> String {
        method.update_hint(&self.os)
    }

    pub fn removal_hint(&self, method: &Method) -> String {
        method.removal_hint(&self.os)
    }
}

/// Resolves every symlink in the system path `path` (below [`System::root`]), like
/// `realpath -m`: components that do not exist are kept as they are.
fn resolve(sys: &dyn System, path: &Path) -> PathBuf {
    fn push_components(pending: &mut Vec<OsString>, path: &Path) {
        // Reversed, so that `pop` yields them in order.
        for component in path.components().rev() {
            match component {
                Component::Normal(name) => pending.push(name.to_owned()),
                Component::ParentDir => pending.push("..".into()),
                Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
            }
        }
    }
    let mut resolved = PathBuf::from("/");
    let mut pending = Vec::new();
    push_components(&mut pending, path);
    let mut followed = 0;
    while let Some(name) = pending.pop() {
        if name == ".." {
            resolved.pop();
            continue;
        }
        let next = resolved.join(&name);
        match std::fs::read_link(sys.path(&next)) {
            Ok(target) if followed < MAX_SYMLINKS => {
                followed += 1;
                if target.is_absolute() {
                    resolved = PathBuf::from("/");
                }
                push_components(&mut pending, &target);
            }
            _ => resolved = next,
        }
    }
    resolved
}

fn in_nix_store(path: &Path) -> bool {
    path.starts_with(NIX_STORE)
}

/// Whether the unit's `ExecStart=` runs a binary from the Nix store.
fn runs_from_nix_store(text: &str) -> bool {
    exec_start_binary(text).is_some_and(|exe| in_nix_store(&exe))
}

/// Whether the system path exists (as a file, directory or symlink, even a dangling one).
/// Its parents are resolved below [`System::root`].
fn exists(sys: &dyn System, path: &Path) -> bool {
    let parent = path
        .parent()
        .map_or_else(|| PathBuf::from("/"), |p| resolve(sys, p));
    let local = match path.file_name() {
        Some(name) => parent.join(name),
        None => parent,
    };
    std::fs::symlink_metadata(sys.path(&local)).is_ok()
}

/// What `/etc/systemd/system/<unit>` is.
enum EtcUnit {
    Missing,
    Masked,
    Nix,
    ServiceInstall(Option<PathBuf>),
    HandWritten,
}

fn classify_etc_unit(sys: &dyn System, unit: &Path) -> EtcUnit {
    if !exists(sys, unit) {
        return EtcUnit::Missing;
    }
    let resolved = resolve(sys, unit);
    if resolved == Path::new("/dev/null") {
        return EtcUnit::Masked;
    }
    if in_nix_store(&resolved) {
        return EtcUnit::Nix;
    }
    match std::fs::read_to_string(sys.path(&resolved)) {
        Ok(text) if text.starts_with(MARKER) => EtcUnit::ServiceInstall(
            exec_start_binary(&text).and_then(|exe| exe.parent().map(Path::to_owned)),
        ),
        // Including a unit that cannot be read: it is not ours.
        _ => EtcUnit::HandWritten,
    }
}

/// The same unit file under the other package unit directory (/usr/lib <-> /lib).
fn other_package_path(unit: &Path) -> Option<PathBuf> {
    if let Ok(rest) = unit.strip_prefix("/usr/lib") {
        Some(Path::new("/lib").join(rest))
    } else {
        unit.strip_prefix("/lib")
            .ok()
            .map(|rest| Path::new("/usr/lib").join(rest))
    }
}

/// The package name in `dpkg-query -S` output (`pkg[:arch][, other]: /path`).
fn parse_dpkg_owner(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .filter(|line| !line.starts_with("diversion "))
        .find_map(|line| line.split_once(": "))
        .and_then(|(packages, _)| packages.split(", ").next())
        .map(|package| {
            package
                .split(':')
                .next()
                .unwrap_or(package)
                .trim()
                .to_owned()
        })
        .filter(|package| !package.is_empty())
}

/// The first line of the output, if it is a package name.
fn parse_name(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .next()
        .map(str::trim)
        .filter(|name| !name.is_empty() && !name.contains(char::is_whitespace))
        .map(str::to_owned)
}

/// Who owns the packaged unit: `Some(owner)`, or `None` if the package managers that exist
/// say nobody does. Without any of them the owner is [`Owner::Unknown`].
fn query_owner(sys: &dyn System, unit: &Path) -> Option<Owner> {
    type Query = (
        &'static str,
        &'static [&'static str],
        fn(&str) -> Option<String>,
    );
    const QUERIES: &[Query] = &[
        ("dpkg-query", &["-S"], parse_dpkg_owner),
        ("rpm", &["-qf", "--queryformat", "%{NAME}\\n"], parse_name),
        ("pacman", &["-Qqo"], parse_name),
    ];
    let paths: Vec<PathBuf> = std::iter::once(unit.to_owned())
        .chain(other_package_path(unit))
        .collect();
    let mut asked = false;
    for (index, (program, args, parse)) in QUERIES.iter().enumerate() {
        for path in &paths {
            let path = path.to_string_lossy();
            let mut argv = args.to_vec();
            argv.push(&path);
            match sys.run(program, &argv) {
                Ok(output) => {
                    asked = true;
                    if !output.success {
                        continue;
                    }
                    if let Some(name) = parse(&output.stdout) {
                        return Some(match index {
                            0 => Owner::Dpkg(name),
                            1 => Owner::Rpm(name),
                            _ => Owner::Pacman(name),
                        });
                    }
                }
                // Not installed (`NotFound`) or not runnable: try the next package manager.
                Err(_) => break,
            }
        }
    }
    if asked { None } else { Some(Owner::Unknown) }
}

/// The result for one unit.
struct UnitDetection {
    install: InstallMethod,
    leftover: Option<PathBuf>,
}

fn detect_unit(sys: &dyn System, name: &str) -> UnitDetection {
    let etc_unit = Path::new(UNIT_DIR).join(name);
    let etc = classify_etc_unit(sys, &etc_unit);
    let local = match etc {
        EtcUnit::Masked => Some(LocalUnit::Masked(etc_unit.clone())),
        EtcUnit::HandWritten => Some(LocalUnit::Overridden(etc_unit.clone())),
        _ => None,
    };

    // The packaged unit (on merged-/usr systems both directories are the same).
    let packaged = PACKAGE_UNIT_DIRS
        .iter()
        .map(|dir| Path::new(dir).join(name))
        .find(|unit| exists(sys, unit))
        .map(|unit| {
            let resolved = resolve(sys, &unit);
            let text = std::fs::read_to_string(sys.path(&resolved)).unwrap_or_default();
            if in_nix_store(&resolved) || runs_from_nix_store(&text) {
                Method::Nix { unit }
            } else {
                match query_owner(sys, &unit) {
                    Some(owner) => Method::Package {
                        unit,
                        owner,
                        local: local.clone(),
                    },
                    None => Method::Unowned {
                        unit,
                        local: local.clone(),
                    },
                }
            }
        });

    let mut methods = Vec::new();
    let mut leftover = None;
    let etc_method = match etc {
        EtcUnit::Missing | EtcUnit::Masked => None,
        EtcUnit::Nix => Some(Method::Nix { unit: etc_unit }),
        EtcUnit::ServiceInstall(bin_dir) => Some(Method::ServiceInstall { bin_dir }),
        // Next to a packaged unit, its override (kept in `local`, except for Nix).
        EtcUnit::HandWritten => match &packaged {
            Some(Method::Package { .. } | Method::Unowned { .. } | Method::Nix { .. }) => None,
            _ => Some(Method::Manual { unit: etc_unit }),
        },
    };
    let service_install = matches!(etc_method, Some(Method::ServiceInstall { .. }));
    match packaged {
        // Shadowed by the `service install` unit: a leftover, not a second installation.
        Some(Method::Unowned { unit, .. }) if service_install => leftover = Some(unit),
        Some(method) => methods.push(method),
        None => {}
    }
    if let Some(method) = etc_method
        && !methods.contains(&method)
    {
        methods.push(method);
    }
    UnitDetection {
        install: InstallMethod::from_methods(methods),
        leftover,
    }
}

/// Detects how puzuz is installed.
pub fn detect(sys: &dyn System) -> Detection {
    let os = OsRelease::read(sys);
    let daemon = detect_unit(sys, DAEMON_UNIT);
    let helper = detect_unit(sys, HELPER_UNIT);
    let partial = match (&daemon.install, &helper.install) {
        (InstallMethod::None, InstallMethod::None) => None,
        (InstallMethod::None, helper) => {
            Some(format!("only {HELPER_UNIT} is installed ({helper})"))
        }
        (daemon, InstallMethod::None) => Some(format!(
            "{HELPER_UNIT} is missing ({DAEMON_UNIT}: {daemon})"
        )),
        (daemon, helper) if !daemon.same_kind(helper) => Some(format!(
            "{DAEMON_UNIT} is installed as {daemon}, but {HELPER_UNIT} as {helper}"
        )),
        _ => None,
    };
    let install = if daemon.install == InstallMethod::None {
        helper.install.clone()
    } else {
        daemon.install
    };
    Detection {
        os,
        install,
        helper: helper.install,
        partial,
        leftovers: daemon.leftover.into_iter().chain(helper.leftover).collect(),
        unit_dir_in_store: in_nix_store(&resolve(sys, Path::new(UNIT_DIR))),
    }
}

#[cfg(test)]
mod tests;
