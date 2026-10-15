//! RHEL, CentOS, Rocky, Alma and Fedora reset `PATH` under sudo to a `secure_path` without
//! /usr/local/bin, where `service install` puts copied binaries: `sudo gpueue ...` then fails
//! with "command not found".

use std::path::{Path, PathBuf};

use super::Check;
use crate::install_method::{Detection, Family, Method};
use crate::service::DEFAULT_BIN_DIR;
use crate::system::System;

const NAME: &str = "sudo PATH";
const SUDOERS: &str = "/etc/sudoers";

/// Files nested deeper than this are not followed (sudo's own limit is 128); the check is
/// skipped instead.
const MAX_DEPTH: usize = 8;

/// The sudoers files could not be evaluated with confidence; the check is skipped.
#[derive(Debug, PartialEq, Eq)]
struct Unsure;

/// The effective `secure_path` while reading sudoers files in sudo's order: `None` while unset
/// (or after `!secure_path`), else the value and the file that set it.
type SecurePath = Option<(String, PathBuf)>;

/// Splits on `sep` outside double quotes.
fn split_unquoted(text: &str, sep: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut start, mut quoted, mut escaped) = (0, false, false);
    for (i, c) in text.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => quoted = !quoted,
            c if c == sep && !quoted => {
                parts.push(&text[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}

/// The line without a trailing `# comment` (a `#` outside double quotes).
fn strip_comment(line: &str) -> &str {
    let (mut quoted, mut escaped) = (false, false);
    for (i, c) in line.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => quoted = !quoted,
            '#' if !quoted => return &line[..i],
            _ => {}
        }
    }
    line
}

/// Lines with backslash continuations joined.
fn logical_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        match line.strip_suffix('\\') {
            Some(start) => current.push_str(start),
            None => {
                current.push_str(line);
                lines.push(std::mem::take(&mut current));
            }
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// The `secure_path` settings of a plain `Defaults` line (not `Defaults:user`, `@host`, `!cmd`
/// or `>runas`), applied to `effective` in order.
fn apply_defaults(line: &str, file: &Path, effective: &mut SecurePath) -> Result<(), Unsure> {
    let Some(rest) = line.strip_prefix("Defaults") else {
        return Ok(());
    };
    if !rest.starts_with([' ', '\t']) {
        return Ok(());
    }
    for item in split_unquoted(rest, ',').into_iter().map(str::trim) {
        if item == "!secure_path" {
            *effective = None;
            continue;
        }
        let Some(value) = item.strip_prefix("secure_path") else {
            continue;
        };
        let value = value.trim_start();
        let Some(value) = value.strip_prefix('=').filter(|_| !value.starts_with("+=")) else {
            // `secure_path+=`, `-=`, or no value: not something to guess about.
            return Err(Unsure);
        };
        let value = value.trim();
        let value = match value.strip_prefix('"') {
            Some(quoted) => quoted.strip_suffix('"').ok_or(Unsure)?,
            None => value,
        };
        if value.contains(['\\', '"', ' ', '\t']) {
            return Err(Unsure);
        }
        *effective = Some((value.to_owned(), file.to_owned()));
    }
    Ok(())
}

/// The path of an include directive, relative paths taken from the including file's directory.
fn include_path(arg: &str, file: &Path) -> Result<PathBuf, Unsure> {
    let arg = arg.trim();
    let arg = match arg.strip_prefix('"') {
        Some(quoted) => quoted.strip_suffix('"').ok_or(Unsure)?,
        None => arg,
    };
    // `%h` (the host name) and escapes are not expanded here.
    if arg.is_empty() || arg.contains(['%', '\\', ' ']) {
        return Err(Unsure);
    }
    let path = Path::new(arg);
    Ok(if path.is_absolute() {
        path.to_owned()
    } else {
        file.parent().unwrap_or(Path::new("/")).join(path)
    })
}

/// Reads one sudoers file in order, following `@include`/`#include` and
/// `@includedir`/`#includedir` where they appear, as sudo does: later settings win.
fn read_sudoers(
    sys: &dyn System,
    file: &Path,
    depth: usize,
    effective: &mut SecurePath,
) -> Result<(), Unsure> {
    if depth > MAX_DEPTH {
        return Err(Unsure);
    }
    let text = std::fs::read_to_string(sys.path(file)).map_err(|_| Unsure)?;
    for line in logical_lines(&text) {
        let line = line.trim();
        let directive = ["@includedir", "#includedir", "@include", "#include"]
            .iter()
            .find_map(|name| {
                line.strip_prefix(name)
                    .filter(|rest| rest.starts_with([' ', '\t']))
                    .map(|rest| (*name, rest))
            });
        match directive {
            Some((name, arg)) if name.ends_with("dir") => {
                let dir = include_path(arg, file)?;
                // A missing directory is skipped by sudo, too.
                let Ok(entries) = std::fs::read_dir(sys.path(&dir)) else {
                    continue;
                };
                let mut names: Vec<String> = entries
                    .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
                    .filter(|name| !name.contains('.') && !name.ends_with('~'))
                    .collect();
                names.sort();
                for name in names {
                    let path = dir.join(name);
                    if sys.path(&path).is_file() {
                        read_sudoers(sys, &path, depth + 1, effective)?;
                    }
                }
            }
            Some((_, arg)) => read_sudoers(sys, &include_path(arg, file)?, depth + 1, effective)?,
            None => apply_defaults(strip_comment(line).trim(), file, effective)?,
        }
    }
    Ok(())
}

/// The effective `secure_path` and the file setting it, starting at `/etc/sudoers`. `Err` when
/// it cannot be told with confidence (including when `/etc/sudoers` cannot be read, as when
/// doctor runs without root).
fn secure_path(sys: &dyn System) -> Result<SecurePath, Unsure> {
    let mut effective = None;
    read_sudoers(sys, Path::new(SUDOERS), 0, &mut effective)?;
    Ok(effective)
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
    let (value, file) = secure_path(sys).ok()??;
    if value.split(':').any(|dir| dir == DEFAULT_BIN_DIR) {
        return None;
    }
    Some(
        Check::info(
            NAME,
            format!(
                "sudo's secure_path (in {}) does not include {DEFAULT_BIN_DIR}, so `sudo gpueue \
                 ...` does not find gpueue",
                file.display()
            ),
        )
        .hint(format!(
            "run `sudo {DEFAULT_BIN_DIR}/gpueue ...`, or add {DEFAULT_BIN_DIR} to secure_path \
             with `sudo visudo`"
        )),
    )
}
