//! How puzuz was installed on this machine: by `puzuz service install`, by a distribution
//! package, by the NixOS module, by hand, or not at all.
//!
//! `service install`/`uninstall`, `doctor` (and later `update`) use this so that a package
//! manager and `puzuz service install` never fight over the same machine.
//!
//! Detection looks only at files:
//! - `/etc/systemd/system/puzuz.service` starting with the `service install` marker line,
//! - a `puzuz.service` in the distribution's unit directory (`/usr/lib/systemd/system`, or
//!   `/lib/systemd/system` where that is not the same directory), which only a package writes,
//! - `/etc/NIXOS`, and an `ExecStart=` below `/nix/store`.
//!
//! Extension point: once puzuz ships .deb/.rpm/AUR packages, [`package_manager`] asks
//! `dpkg-query -S`, `rpm -qf` and `pacman -Qo` who owns the packaged unit, so that messages can
//! name the exact update and removal commands.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::doctor::exec_start_binary;
use crate::service::{DAEMON_UNIT, MARKER, UNIT_DIR};
use crate::system::System;

/// Unit directories a distribution package installs into.
const PACKAGE_UNIT_DIRS: &[&str] = &["/usr/lib/systemd/system", "/lib/systemd/system"];
/// Present on every NixOS system.
const NIXOS_MARKER: &str = "/etc/NIXOS";
const NIX_STORE: &str = "/nix/store";

/// One way puzuz is installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    /// `puzuz service install`; the units run the binaries in `bin_dir` (when it can be read
    /// from the unit).
    ServiceInstall { bin_dir: Option<PathBuf> },
    /// A distribution package: its unit file is at `unit`.
    Package { unit: PathBuf },
    /// NixOS (`services.puzuz` module); its units point into /nix/store.
    NixOs,
    /// `/etc/systemd/system/puzuz.service` written by someone else (an administrator, or
    /// `service install` before the marker existed).
    Manual { unit: PathBuf },
}

impl Method {
    /// Whether puzuz itself may change this installation (`service install`, `update`).
    pub fn is_managed_elsewhere(&self) -> bool {
        matches!(self, Method::Package { .. } | Method::NixOs)
    }

    /// How to update puzuz installed this way.
    pub fn update_hint(&self) -> String {
        match self {
            Method::ServiceInstall { bin_dir } => format!(
                "re-run `sudo <new puzuz> service install --bin-dir {}` with the new binaries",
                bin_dir
                    .as_deref()
                    .unwrap_or(Path::new("/usr/local/bin"))
                    .display()
            ),
            Method::Package { .. } => {
                "update the package with your package manager (e.g. `sudo apt upgrade`)".to_owned()
            }
            Method::NixOs => {
                "update the flake input and run `sudo nixos-rebuild switch`".to_owned()
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
    pub fn removal_hint(&self) -> String {
        match self {
            Method::ServiceInstall { .. } => "sudo puzuz service uninstall".to_owned(),
            Method::Package { .. } => {
                "remove the package with your package manager (e.g. `sudo apt remove puzuz`)"
                    .to_owned()
            }
            Method::NixOs => {
                "set `services.puzuz.enable = false` and run `sudo nixos-rebuild switch`".to_owned()
            }
            Method::Manual { unit } => format!("remove {} by hand", unit.display()),
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
            Method::Package { unit } => {
                write!(f, "distribution package ({})", unit.display())
            }
            Method::NixOs => f.write_str("NixOS module"),
            Method::Manual { unit } => write!(
                f,
                "by hand ({} was not written by `puzuz service install`)",
                unit.display()
            ),
        }
    }
}

/// Every way puzuz is installed on this machine.
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

    /// The installation is (also) owned by a package manager or NixOS.
    pub fn managed_elsewhere(&self) -> Option<&Method> {
        self.methods().iter().find(|m| m.is_managed_elsewhere())
    }

    pub fn has_service_install(&self) -> bool {
        self.methods()
            .iter()
            .any(|m| matches!(m, Method::ServiceInstall { .. }))
    }

    /// The single machine-readable name of the installation: `none`, `service-install`,
    /// `package`, `nixos`, `manual` or `mixed`.
    pub fn keyword(&self) -> &'static str {
        match self {
            InstallMethod::None => "none",
            InstallMethod::Mixed(_) => "mixed",
            InstallMethod::One(method) => match method {
                Method::ServiceInstall { .. } => "service-install",
                Method::Package { .. } => "package",
                Method::NixOs => "nixos",
                Method::Manual { .. } => "manual",
            },
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

/// Whether the unit's `ExecStart=` runs a binary from the Nix store.
fn runs_from_nix_store(text: &str) -> bool {
    exec_start_binary(text).is_some_and(|exe| exe.starts_with(NIX_STORE))
}

/// The packaged daemon unit, if any (on merged-/usr systems both directories are the same).
fn package_unit(sys: &dyn System) -> Option<PathBuf> {
    PACKAGE_UNIT_DIRS
        .iter()
        .map(|dir| Path::new(dir).join(DAEMON_UNIT))
        .find(|unit| sys.path(unit).is_file())
}

/// Who owns the packaged unit. Extension point: query `dpkg-query -S`, `rpm -qf` and
/// `pacman -Qo` here once puzuz ships packages; until then every package is reported alike.
fn package_manager(_sys: &dyn System, unit: &Path) -> Method {
    Method::Package {
        unit: unit.to_owned(),
    }
}

/// Detects how puzuz is installed.
pub fn detect(sys: &dyn System) -> InstallMethod {
    let mut methods = Vec::new();
    let mut add = |method: Method| {
        if !methods.contains(&method) {
            methods.push(method);
        }
    };

    if sys.path(Path::new(NIXOS_MARKER)).exists() {
        add(Method::NixOs);
    }

    if let Some(unit) = package_unit(sys) {
        let text = std::fs::read_to_string(sys.path(&unit)).unwrap_or_default();
        if runs_from_nix_store(&text) {
            add(Method::NixOs);
        } else {
            add(package_manager(sys, &unit));
        }
    }

    let etc_unit = Path::new(UNIT_DIR).join(DAEMON_UNIT);
    // Read through symlinks: NixOS links /etc/systemd/system into the store.
    if let Ok(text) = std::fs::read_to_string(sys.path(&etc_unit)) {
        if text.starts_with(MARKER) {
            add(Method::ServiceInstall {
                bin_dir: exec_start_binary(&text).and_then(|exe| exe.parent().map(Path::to_owned)),
            });
        } else if runs_from_nix_store(&text) {
            add(Method::NixOs);
        } else {
            add(Method::Manual { unit: etc_unit });
        }
    }

    match methods.len() {
        0 => InstallMethod::None,
        1 => InstallMethod::One(methods.remove(0)),
        _ => InstallMethod::Mixed(methods),
    }
}

#[cfg(test)]
mod tests;
