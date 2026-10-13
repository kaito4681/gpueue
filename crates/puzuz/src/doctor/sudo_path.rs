//! RHEL, CentOS, Rocky, Alma and Fedora reset `PATH` under sudo to a `secure_path` without
//! /usr/local/bin, where `service install` puts copied binaries: `sudo puzuz ...` then fails
//! with "command not found".

use std::path::{Path, PathBuf};

use super::Check;
use crate::install_method::{Detection, Family, Method};
use crate::service::DEFAULT_BIN_DIR;
use crate::system::System;

const NAME: &str = "sudo PATH";
const SUDOERS: &str = "/etc/sudoers";
const SUDOERS_DIR: &str = "/etc/sudoers.d";

/// The value of a `secure_path` setting in one `Defaults` line: `Some(None)` for
/// `!secure_path`, `None` if the line does not set it. Only plain `Defaults` lines count (not
/// `Defaults:user` and the like).
fn secure_path_setting(line: &str) -> Option<Option<String>> {
    let rest = line.trim().strip_prefix("Defaults")?;
    if !rest.starts_with([' ', '\t']) {
        return None;
    }
    let mut found = None;
    for item in rest.split(',').map(str::trim) {
        if item == "!secure_path" {
            found = Some(None);
        } else if let Some(value) = item.strip_prefix("secure_path")
            && let Some(value) = value.trim_start().strip_prefix('=')
        {
            found = Some(Some(value.trim().trim_matches('"').to_owned()));
        }
    }
    found
}

/// The effective `secure_path` and the file setting it: `/etc/sudoers`, then the files in
/// `/etc/sudoers.d` in sudo's order (names with a `.` or ending in `~` are skipped). `None`
/// when `/etc/sudoers` cannot be read (doctor run without root) or nothing sets it.
fn secure_path(sys: &dyn System) -> Option<(String, PathBuf)> {
    let main = std::fs::read_to_string(sys.path(Path::new(SUDOERS))).ok()?;
    let mut files = vec![(PathBuf::from(SUDOERS), main)];
    if let Ok(entries) = std::fs::read_dir(sys.path(Path::new(SUDOERS_DIR))) {
        let mut names: Vec<String> = entries
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter(|name| !name.contains('.') && !name.ends_with('~'))
            .collect();
        names.sort();
        for name in names {
            let path = Path::new(SUDOERS_DIR).join(name);
            if let Ok(text) = std::fs::read_to_string(sys.path(&path)) {
                files.push((path, text));
            }
        }
    }
    let mut effective = None;
    for (path, text) in &files {
        for line in text.lines() {
            if let Some(setting) = secure_path_setting(line) {
                effective = setting.map(|value| (value, path.clone()));
            }
        }
    }
    effective
}

/// INFO when the binaries are in /usr/local/bin on a RHEL-family system whose sudo
/// `secure_path` lacks it; `None` otherwise (also when the sudoers files cannot be read).
pub(super) fn check(sys: &dyn System, detection: &Detection) -> Option<Check> {
    let in_usr_local_bin = detection.methods().any(|m| {
        matches!(m, Method::ServiceInstall { bin_dir: Some(dir) } if dir == Path::new(DEFAULT_BIN_DIR))
    });
    if !in_usr_local_bin || detection.os.family() != Some(Family::Rhel) {
        return None;
    }
    let (value, file) = secure_path(sys)?;
    if value.split(':').any(|dir| dir == DEFAULT_BIN_DIR) {
        return None;
    }
    Some(
        Check::info(
            NAME,
            format!(
                "sudo's secure_path (in {}) does not include {DEFAULT_BIN_DIR}, so `sudo puzuz \
                 ...` does not find puzuz",
                file.display()
            ),
        )
        .hint(format!(
            "run `sudo {DEFAULT_BIN_DIR}/puzuz ...`, or add {DEFAULT_BIN_DIR} to secure_path \
             with `sudo visudo`"
        )),
    )
}
