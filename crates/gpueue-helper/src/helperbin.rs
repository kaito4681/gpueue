//! Content-addressed copies of the helper binary that job units run.
//!
//! A job unit runs `<helper> shim <job dir>`, and the job's process tree
//! (and so every checkpoint image of it) contains `<helper> shim-init`.
//! CRIU restores a mapped executable only if the file at its dump-time
//! path is the same one (size, build id). Upgrades (deb, rpm, `install.sh`
//! / `gpueue update`, the AUR package) replace `/usr/bin/gpueue-helper` (or
//! `/usr/local/bin/gpueue-helper`) in place, so a job dumped before an
//! upgrade could not be restored after it.
//!
//! So at start-up the helper copies its own binary into a root-owned
//! directory (`<data_dir>/lib`, next to the fork guard) as
//! `gpueue-helper-<hash of the contents>`, and job units run that copy.
//! The job record and the record kept with the image name it
//! ([`crate::jobs::JobRecord::shim_exe`]); a restore puts the tree back on
//! the copy it was dumped with, which an upgrade does not touch. Copies are
//! removed at the helper's start once no job record and no checkpoint
//! needs them ([`collect_garbage`]).
//!
//! On NixOS the helper runs from the Nix store, whose paths never change;
//! [`crate::gcroots`] keeps them, and no copy is made.
//!
//! With SELinux, a file created under `/var/lib` gets the type `var_lib_t`,
//! which systemd (`init_t`) may not execute: job units would fail with
//! status 203/EXEC. So the copies get the SELinux label of the running
//! helper binary (`bin_t` for `/usr/bin/gpueue-helper`), which systemd
//! evidently may execute, since it started the helper. [`install`] labels
//! a new copy before it is renamed into place and relabels the copies that
//! are already there (a relabel such as `restorecon -R /var/lib` gives them
//! `var_lib_t` again), and [`ensure_label`] checks a copy before a job unit
//! runs it. Without SELinux the binary has no `security.selinux` attribute
//! and nothing is labelled.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rustix::fs::XattrFlags;

/// File names of the copies start with this.
pub const NAME_PREFIX: &str = "gpueue-helper-";

/// `gpueue-helper-<hash>` for a binary with these contents.
pub fn file_name(contents: &[u8]) -> String {
    format!("{NAME_PREFIX}{:016x}", content_hash(contents))
}

/// FNV-1a over little-endian 64-bit words (and the length): a stable
/// content hash for the file name, not a security measure (the directory is
/// root-owned and an existing copy is compared byte for byte). Word-wise
/// because the helper hashes itself at every start, and a debug build is
/// over 100 MiB.
fn content_hash(bytes: &[u8]) -> u64 {
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let (words, rest) = bytes.as_chunks::<8>();
    for w in words {
        h = (h ^ u64::from_le_bytes(*w)).wrapping_mul(PRIME);
    }
    for b in rest {
        h = (h ^ u64::from(*b)).wrapping_mul(PRIME);
    }
    (h ^ bytes.len() as u64).wrapping_mul(PRIME)
}

/// Whether `path` is a copy in `dir` (by name).
pub fn is_copy(dir: &Path, path: &Path) -> bool {
    path.parent() == Some(dir)
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(NAME_PREFIX))
}

fn read_all(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// The extended attribute that holds a file's SELinux label.
pub const SELINUX_XATTR: &str = "security.selinux";

/// The value of the extended attribute `name` of `path` (following
/// symlinks), or `None` when the file has none or its filesystem does not
/// support extended attributes.
pub fn read_label(path: &Path, name: &str) -> io::Result<Option<Vec<u8>>> {
    // SELinux contexts are far shorter.
    let mut buf = vec![0u8; 4096];
    match rustix::fs::getxattr(path, name, &mut buf[..]) {
        Ok(len) => {
            buf.truncate(len);
            Ok(Some(buf))
        }
        Err(rustix::io::Errno::NODATA | rustix::io::Errno::NOTSUP) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Gives `path` the label `label` (attribute `name`) unless it has it
/// already. Returns whether it was changed.
fn set_label(path: &Path, name: &str, label: &[u8]) -> io::Result<bool> {
    if read_label(path, name)?.as_deref() == Some(label) {
        return Ok(false);
    }
    rustix::fs::setxattr(path, name, label, XattrFlags::empty())?;
    Ok(true)
}

/// The SELinux label for the copies of the binary at `exe`: its own, or
/// `None` without SELinux. An error reading it is logged and taken as none.
pub fn label_of(exe: &Path) -> Option<Vec<u8>> {
    label_by(exe, SELINUX_XATTR)
}

fn label_by(exe: &Path, name: &str) -> Option<Vec<u8>> {
    read_label(exe, name).unwrap_or_else(|e| {
        eprintln!(
            "gpueue-helper: warning: cannot read the SELinux label of {}: {e}",
            exe.display()
        );
        None
    })
}

/// Gives the copy at `path` the SELinux label `label` (see [`label_of`])
/// if it lost it, e.g. to a `restorecon -R /var/lib`. Failures are logged:
/// the job unit then fails to start with 203/EXEC, which reports itself.
pub fn ensure_label(path: &Path, label: Option<&[u8]>) {
    ensure_label_by(path, SELINUX_XATTR, label);
}

fn ensure_label_by(path: &Path, name: &str, label: Option<&[u8]>) {
    let Some(label) = label else { return };
    match set_label(path, name, label) {
        Ok(false) => {}
        Ok(true) => eprintln!(
            "gpueue-helper: relabelled {} as {}",
            path.display(),
            String::from_utf8_lossy(label.strip_suffix(b"\0").unwrap_or(label))
        ),
        Err(e) => eprintln!(
            "gpueue-helper: warning: cannot give {} the SELinux label {} of the helper: {e}; \
             systemd may refuse to run it (status 203/EXEC)",
            path.display(),
            String::from_utf8_lossy(label.strip_suffix(b"\0").unwrap_or(label))
        ),
    }
}

/// Copies the binary at `exe` into `dir` (created with mode 0755) as
/// `gpueue-helper-<hash>`, mode 0755, unless it is already there with the
/// same contents, and returns the copy's path. A new file is renamed into
/// place, so processes that run an existing copy keep it. Every copy in
/// `dir` gets the SELinux label of `exe`, if it has one.
pub fn install(dir: &Path, exe: &Path) -> io::Result<PathBuf> {
    install_labelled(dir, exe, SELINUX_XATTR)
}

fn install_labelled(dir: &Path, exe: &Path, xattr: &str) -> io::Result<PathBuf> {
    let contents = read_all(exe)?;
    let label = label_by(exe, xattr);
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o755))?;
    let name = file_name(&contents);
    let path = dir.join(&name);
    let same_size = fs::metadata(&path).is_ok_and(|m| m.len() == contents.len() as u64);
    if same_size && read_all(&path).is_ok_and(|b| b == contents) {
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        relabel_copies(dir, xattr, label.as_deref());
        return Ok(path);
    }
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let res = (|| {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o755)
            .open(&tmp)?;
        f.write_all(&contents)?;
        f.set_permissions(fs::Permissions::from_mode(0o755))?;
        if let Some(label) = &label
            && let Err(e) = rustix::fs::fsetxattr(f.as_fd(), xattr, label, XattrFlags::empty())
        {
            eprintln!(
                "gpueue-helper: warning: cannot give {} the SELinux label {} of {}: {e}; \
                 systemd may refuse to run it (status 203/EXEC)",
                path.display(),
                String::from_utf8_lossy(label.strip_suffix(b"\0").unwrap_or(label)),
                exe.display()
            );
        }
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, &path)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res?;
    relabel_copies(dir, xattr, label.as_deref());
    Ok(path)
}

/// Gives every copy in `dir` the label `label` (the older ones are run
/// again by restores of their checkpoints).
fn relabel_copies(dir: &Path, xattr: &str, label: Option<&[u8]>) {
    if label.is_none() {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if is_copy(dir, &path) {
            ensure_label_by(&path, xattr, label);
        }
    }
}

/// Removes the copies in `dir` that are not in `keep`, and temporary files
/// of interrupted installs. Returns what was removed. Other files (the
/// fork guard) are left alone.
pub fn collect_garbage(dir: &Path, keep: &BTreeSet<PathBuf>) -> io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let stale_tmp = name.starts_with(&format!(".{NAME_PREFIX}")) && name.contains(".tmp-");
        let path = entry.path();
        if (name.starts_with(NAME_PREFIX) && !keep.contains(&path)) || stale_tmp {
            match fs::remove_file(&path) {
                Ok(()) => removed.push(path),
                Err(e) => eprintln!(
                    "gpueue-helper: warning: cannot remove the old helper copy {}: {e}",
                    path.display()
                ),
            }
        }
    }
    removed.sort();
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exe(dir: &Path, contents: &[u8]) -> PathBuf {
        let p = dir.join("gpueue-helper");
        // A new file, as an upgrade puts there (without the old one's
        // extended attributes).
        let _ = fs::remove_file(&p);
        fs::write(&p, contents).unwrap();
        p
    }

    #[test]
    fn install_is_content_addressed_and_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");
        let v1 = exe(tmp.path(), b"\x7fELF version 1");
        let a = install(&lib, &v1).unwrap();
        assert!(is_copy(&lib, &a));
        assert_eq!(fs::read(&a).unwrap(), b"\x7fELF version 1");
        assert_eq!(
            fs::metadata(&a).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(&lib).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let ino = std::os::unix::fs::MetadataExt::ino(&fs::metadata(&a).unwrap());
        assert_eq!(install(&lib, &v1).unwrap(), a);
        assert_eq!(
            std::os::unix::fs::MetadataExt::ino(&fs::metadata(&a).unwrap()),
            ino,
            "an identical copy is not rewritten"
        );

        // An upgrade replaces the binary: a second copy, the first stays.
        let v2 = exe(tmp.path(), b"\x7fELF version 2");
        let b = install(&lib, &v2).unwrap();
        assert_ne!(a, b);
        assert_eq!(fs::read(&a).unwrap(), b"\x7fELF version 1");
        assert_eq!(fs::read(&b).unwrap(), b"\x7fELF version 2");

        // A damaged copy is replaced.
        fs::write(&b, b"junk").unwrap();
        assert_eq!(install(&lib, &v2).unwrap(), b);
        assert_eq!(fs::read(&b).unwrap(), b"\x7fELF version 2");
        assert_eq!(fs::read_dir(&lib).unwrap().count(), 2, "no temporary files");
    }

    /// The SELinux label is copied like any extended attribute; a `user.`
    /// one stands in for `security.selinux`, which only root may set.
    #[test]
    fn copies_get_the_label_of_the_binary() {
        const XATTR: &str = "user.gpueue-test-label";
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");
        let v1 = exe(tmp.path(), b"\x7fELF version 1");
        let label = b"system_u:object_r:bin_t:s0\0";
        if let Err(e) = rustix::fs::setxattr(&v1, XATTR, label, XattrFlags::empty()) {
            eprintln!("skipped: no user extended attributes here ({e})");
            return;
        }
        let a = install_labelled(&lib, &v1, XATTR).unwrap();
        assert_eq!(read_label(&a, XATTR).unwrap().as_deref(), Some(&label[..]));

        // A relabel took it away: the next start (and `ensure_label`) puts
        // it back, also on older copies.
        let other = b"system_u:object_r:var_lib_t:s0\0";
        rustix::fs::setxattr(&a, XATTR, other, XattrFlags::empty()).unwrap();
        let v2 = exe(tmp.path(), b"\x7fELF version 2");
        rustix::fs::setxattr(&v2, XATTR, label, XattrFlags::empty()).unwrap();
        let b = install_labelled(&lib, &v2, XATTR).unwrap();
        assert_eq!(read_label(&a, XATTR).unwrap().as_deref(), Some(&label[..]));
        assert_eq!(read_label(&b, XATTR).unwrap().as_deref(), Some(&label[..]));
        rustix::fs::setxattr(&b, XATTR, other, XattrFlags::empty()).unwrap();
        assert_eq!(install_labelled(&lib, &v2, XATTR).unwrap(), b);
        assert_eq!(read_label(&b, XATTR).unwrap().as_deref(), Some(&label[..]));
        rustix::fs::setxattr(&b, XATTR, other, XattrFlags::empty()).unwrap();
        ensure_label_by(&b, XATTR, Some(label));
        assert_eq!(read_label(&b, XATTR).unwrap().as_deref(), Some(&label[..]));

        // Without a label (no SELinux) nothing is set.
        let v3 = exe(tmp.path(), b"\x7fELF version 3");
        let c = install_labelled(&lib, &v3, XATTR).unwrap();
        assert_eq!(read_label(&c, XATTR).unwrap(), None);
        ensure_label_by(&c, XATTR, None);
        assert_eq!(read_label(&c, XATTR).unwrap(), None);
        assert_eq!(fs::read_dir(&lib).unwrap().count(), 3, "no temporary files");
    }

    #[test]
    fn a_missing_label_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let f = exe(tmp.path(), b"x");
        assert_eq!(read_label(&f, "user.gpueue-test-none").unwrap(), None);
        assert!(read_label(&tmp.path().join("missing"), SELINUX_XATTR).is_err());
    }

    #[test]
    fn garbage_collection_keeps_what_is_referenced() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");
        let a = install(&lib, &exe(tmp.path(), b"a")).unwrap();
        let b = install(&lib, &exe(tmp.path(), b"b")).unwrap();
        let c = install(&lib, &exe(tmp.path(), b"c")).unwrap();
        let guard = lib.join("gpueue-forkguard-0123.so");
        fs::write(&guard, b"").unwrap();
        let tmp_file = lib.join(format!(".{NAME_PREFIX}abc.tmp-42"));
        fs::write(&tmp_file, b"").unwrap();

        let keep: BTreeSet<PathBuf> = [a.clone(), c.clone()].into();
        let removed = collect_garbage(&lib, &keep).unwrap();
        let mut expected = vec![b.clone(), tmp_file.clone()];
        expected.sort();
        assert_eq!(removed, expected);
        assert!(a.exists() && !b.exists() && c.exists() && guard.exists());
        // A missing directory is nothing to collect.
        assert!(
            collect_garbage(&tmp.path().join("missing"), &keep)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn the_hash_depends_on_every_byte_and_the_length() {
        let base = content_hash(b"0123456789abcdef");
        assert_ne!(base, content_hash(b"0123456789abcdeF"));
        assert_ne!(base, content_hash(b"0123456789abcdef\0"));
        assert_ne!(content_hash(b""), content_hash(b"\0"));
        assert_eq!(base, content_hash(b"0123456789abcdef"));
    }

    #[test]
    fn copies_are_recognized_by_directory_and_name() {
        let lib = Path::new("/var/lib/gpueue/lib");
        assert!(is_copy(lib, &lib.join("gpueue-helper-0123456789abcdef")));
        assert!(!is_copy(lib, &lib.join("gpueue-forkguard-1.so")));
        assert!(!is_copy(lib, Path::new("/usr/bin/gpueue-helper")));
        assert!(!is_copy(
            lib,
            Path::new("/elsewhere/gpueue-helper-0123456789abcdef")
        ));
    }
}
