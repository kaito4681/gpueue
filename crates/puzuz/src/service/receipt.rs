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
//! place, e.g. by a package or by hand); those are never removed. `gc_root` is present when the
//! binaries were installed from the Nix store: they still use libraries from it, so the store
//! path is registered as a garbage-collector root.

use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::system::{System, is_root_owned_and_protected};

pub const RECEIPT_PATH: &str = "/var/lib/puzuz/install-receipt";

/// The binaries `service install` may copy; anything else in a receipt is ignored.
pub const BINARIES: [&str; 2] = ["puzuz", "puzuz-helper"];

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    /// The version of the `puzuz` that wrote it.
    pub version: String,
    /// Where the units run the binaries from.
    pub bin_dir: PathBuf,
    /// The binaries `service install` copied into `bin_dir`.
    #[serde(default)]
    pub copied: Vec<PathBuf>,
    /// The Nix GC root registered for `gc_root_target`, the store path the binaries came from.
    #[serde(default)]
    pub gc_root: Option<PathBuf>,
    #[serde(default)]
    pub gc_root_target: Option<PathBuf>,
}

/// A TOML basic string (JSON string escapes are valid in TOML basic strings).
fn quote(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_owned())
}

fn quote_path(path: &Path) -> String {
    quote(&path.to_string_lossy())
}

impl Receipt {
    pub fn to_toml(&self) -> String {
        let copied: Vec<String> = self.copied.iter().map(|p| quote_path(p)).collect();
        let mut text = format!(
            "# Written by `puzuz service install`; `puzuz uninstall` removes the files in `copied`.\n\
             # An empty `copied` means the units run binaries that were already in bin_dir.\n\
             version = {}\n\
             bin_dir = {}\n\
             copied = [{}]\n",
            quote(&self.version),
            quote_path(&self.bin_dir),
            copied.join(", "),
        );
        for (key, value) in [
            ("gc_root", &self.gc_root),
            ("gc_root_target", &self.gc_root_target),
        ] {
            if let Some(value) = value {
                text.push_str(&format!("{key} = {}\n", quote_path(value)));
            }
        }
        text
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
}

/// Reads the receipt; `Ok(None)` if there is none (an installation by an older puzuz). A
/// receipt that anyone but root could have written is refused: `uninstall` deletes what it
/// lists.
pub fn read(sys: &dyn System) -> Result<Option<Receipt>, String> {
    let path = Path::new(RECEIPT_PATH);
    let text = match std::fs::read_to_string(sys.path(path)) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("{RECEIPT_PATH}: {err}")),
    };
    match is_root_owned_and_protected(sys, path) {
        Ok(true) => {}
        Ok(false) => {
            return Err(format!(
                "{RECEIPT_PATH} is not owned by root or is writable by others; ignoring it"
            ));
        }
        Err(err) => return Err(format!("{RECEIPT_PATH}: {err}")),
    }
    toml::from_str(&text)
        .map(Some)
        .map_err(|err| format!("{RECEIPT_PATH}: {err}"))
}
