//! The install receipt: what `puzuz service install` put where, so that `puzuz uninstall`
//! removes exactly the binaries it copied and nothing else.
//!
//! `/var/lib/puzuz/install-receipt` (root, 0644) is a small TOML file:
//!
//! ```toml
//! version = "0.2.0"
//! bin_dir = "/usr/local/bin"
//! copied = ["/usr/local/bin/puzuz", "/usr/local/bin/puzuz-helper"]
//! gc_root = "/nix/var/nix/gcroots/puzuz"
//! gc_root_target = "/nix/store/...-puzuz-0.2.0"
//! ```
//!
//! An empty `copied` means the units run binaries that were already in `bin_dir` (installed in
//! place, e.g. by a package or by hand); those are never removed. `gc_root` is present while the
//! installed binaries load libraries from the Nix store: the store path they came from is then
//! registered as a garbage-collector root. Unknown keys are ignored (a newer puzuz may add some).

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::install_method::{self, FileOwner};
use crate::system::{System, is_root_owned_and_protected};

pub const RECEIPT_PATH: &str = "/var/lib/puzuz/install-receipt";

/// The binaries `service install` may copy; anything else in a receipt is ignored.
pub const BINARIES: [&str; 2] = ["puzuz", "puzuz-helper"];

const HEADER: &str = "# Written by `puzuz service install`; `puzuz uninstall` removes the files \
                      in `copied`.\n# An empty `copied` means the units run binaries that were \
                      already in bin_dir.\n";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    /// The version of the `puzuz` that wrote it.
    pub version: String,
    /// Where the units run the binaries from.
    pub bin_dir: PathBuf,
    /// The binaries `service install` copied into `bin_dir`.
    #[serde(default)]
    pub copied: Vec<PathBuf>,
    /// The Nix GC root registered for `gc_root_target`, the store path the binaries came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gc_root: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gc_root_target: Option<PathBuf>,
}

impl Receipt {
    pub fn to_toml(&self) -> String {
        let body = toml::to_string(self).expect("a receipt is always representable in TOML");
        format!("{HEADER}{body}")
    }

    /// The copied binaries that are safe to delete: `puzuz` and `puzuz-helper` in `bin_dir`.
    pub fn removable_copies(&self) -> Vec<PathBuf> {
        self.copied
            .iter()
            .filter(|path| {
                BINARIES
                    .iter()
                    .any(|name| **path == self.bin_dir.join(name))
            })
            .cloned()
            .collect()
    }

    /// The binaries in `bin_dir`, for messages.
    pub fn binaries_in(dir: &Path) -> String {
        BINARIES
            .iter()
            .map(|name| dir.join(name).display().to_string())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// What [`read`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiptState {
    /// No receipt: nothing installed, or an installation by a puzuz older than the receipt.
    Missing,
    /// A receipt that cannot be used: unreadable, not protected (anyone but root could have
    /// written what `uninstall` deletes), or not valid TOML of the expected shape.
    Invalid(String),
    Valid(Receipt),
}

pub fn read(sys: &dyn System) -> ReceiptState {
    let path = Path::new(RECEIPT_PATH);
    let text = match std::fs::read_to_string(sys.path(path)) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return ReceiptState::Missing,
        Err(err) => return ReceiptState::Invalid(format!("{RECEIPT_PATH} cannot be read: {err}")),
    };
    match is_root_owned_and_protected(sys, path) {
        Ok(true) => {}
        Ok(false) => {
            return ReceiptState::Invalid(format!(
                "{RECEIPT_PATH} is not owned by root or is writable by others, so it cannot be \
                 trusted"
            ));
        }
        Err(err) => return ReceiptState::Invalid(format!("{RECEIPT_PATH} cannot be read: {err}")),
    }
    match toml::from_str(&text) {
        Ok(receipt) => ReceiptState::Valid(receipt),
        Err(err) => ReceiptState::Invalid(format!("{RECEIPT_PATH} is not a valid receipt: {err}")),
    }
}

/// [`read`], printing a warning for an invalid receipt.
pub fn read_or_warn(sys: &dyn System, out: &mut dyn Write) -> ReceiptState {
    let state = read(sys);
    if let ReceiptState::Invalid(reason) = &state {
        writeln!(out, "warning: {}", reason.trim_end()).ok();
    }
    state
}

pub fn write(sys: &dyn System, receipt: &Receipt) -> Result<(), String> {
    super::write_file(
        sys,
        Path::new(RECEIPT_PATH),
        receipt.to_toml().as_bytes(),
        0o644,
    )
    .map(|_| ())
}

pub fn remove(sys: &dyn System, out: &mut dyn Write) {
    match std::fs::remove_file(sys.path(Path::new(RECEIPT_PATH))) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => {
            writeln!(out, "warning: removing {RECEIPT_PATH}: {err}").ok();
        }
        _ => {}
    }
}

/// Deletes copies `service install` made, one by one: never a file a package owns (a package
/// may have been installed over them since), nor one whose owner cannot be asked. Failures
/// are reported and the rest goes on. Returns the files that are still there and still ours.
pub fn remove_copies(
    sys: &dyn System,
    paths: &[PathBuf],
    note: &str,
    out: &mut dyn Write,
) -> Vec<PathBuf> {
    let mut left = Vec::new();
    for path in paths {
        let local = sys.path(path);
        match std::fs::symlink_metadata(&local) {
            Ok(meta) if !meta.is_dir() => {}
            _ => continue,
        }
        match install_method::file_owner(sys, path) {
            FileOwner::Package(owner) => {
                writeln!(
                    out,
                    "warning: kept {} (it belongs to the package {owner} now)",
                    path.display()
                )
                .ok();
                continue;
            }
            FileOwner::Unknown => {
                writeln!(
                    out,
                    "warning: kept {} (the package manager could not tell whether a package \
                     owns it)",
                    path.display()
                )
                .ok();
                left.push(path.clone());
                continue;
            }
            FileOwner::NotOwned => {}
        }
        // The running binary may be one of them; unlinking it is fine.
        match std::fs::remove_file(&local) {
            Ok(()) => {
                writeln!(out, "removed {}{note}", path.display()).ok();
            }
            Err(err) => {
                writeln!(out, "warning: removing {}: {err}", path.display()).ok();
                left.push(path.clone());
            }
        }
    }
    left
}
