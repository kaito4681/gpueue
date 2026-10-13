//! Nix garbage-collector roots for the helper's own store path.
//!
//! A job unit runs `<helper> shim <dir>`, and the job's process tree (and so
//! every checkpoint image of it) contains `<helper> shim-init`. When the
//! helper runs from the Nix store (NixOS), an update switches to another
//! store path, and `nix-collect-garbage` could then delete the binary that
//! a running job's shim, or a checkpoint image waiting to be restored, still
//! needs. So while a job has a record (it runs, or its checkpoint is kept),
//! `/nix/var/nix/gcroots/puzuz/job-<id>` links to the store path that
//! started it. Restores keep the link: the restored tree runs the binary of
//! the dump.
//!
//! Without a Nix store (or with the helper elsewhere) this does nothing.
//! Failing to add or remove a root never fails a job; it is logged.

use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Component, Path, PathBuf};

use puzuz_protocol::helper::JobId;

/// Nix's directory of GC roots; symlinks below it keep their targets.
pub const NIX_GCROOTS: &str = "/nix/var/nix/gcroots";
/// The Nix store.
pub const NIX_STORE: &str = "/nix/store";

#[derive(Debug, Clone, Default)]
pub struct GcRoots {
    /// The directory the links are kept in, and the store path they point
    /// to; `None` when the helper does not run from the Nix store.
    inner: Option<(PathBuf, PathBuf)>,
}

impl GcRoots {
    /// Roots that are never written.
    pub fn disabled() -> Self {
        Self::default()
    }

    /// The roots for the helper at `exe`: enabled when `exe` resolves into
    /// the Nix store and [`NIX_GCROOTS`] exists.
    pub fn detect(exe: &Path) -> Self {
        Self::new(Path::new(NIX_GCROOTS), Path::new(NIX_STORE), exe)
    }

    /// [`Self::detect`] with another roots directory and store (for tests).
    pub fn new(gcroots: &Path, store: &Path, exe: &Path) -> Self {
        let exe = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_owned());
        let Ok(rest) = exe.strip_prefix(store) else {
            return Self::disabled();
        };
        let Some(Component::Normal(top)) = rest.components().next() else {
            return Self::disabled();
        };
        if !gcroots.is_dir() {
            return Self::disabled();
        }
        Self {
            inner: Some((gcroots.join("puzuz"), store.join(top))),
        }
    }

    /// The store path the roots keep, if enabled.
    pub fn target(&self) -> Option<&Path> {
        self.inner.as_ref().map(|(_, target)| target.as_path())
    }

    fn link(dir: &Path, job: JobId) -> PathBuf {
        dir.join(format!("job-{}", job.0))
    }

    /// Keeps the helper's store path while `job` exists. An existing root
    /// (a restored job's) is left as it is.
    pub fn add(&self, job: JobId) {
        let Some((dir, target)) = &self.inner else {
            return;
        };
        let link = Self::link(dir, job);
        let result = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(dir)
            .and_then(|()| match std::os::unix::fs::symlink(target, &link) {
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
                other => other,
            });
        if let Err(e) = result {
            eprintln!(
                "puzuz-helper: warning: cannot add the GC root {} -> {}: {e}; \
                 `nix-collect-garbage` may delete what job {} needs after an update",
                link.display(),
                target.display(),
                job.0
            );
        }
    }

    /// Drops the root of a job that is gone (no unit, no checkpoint).
    pub fn remove(&self, job: JobId) {
        let Some((dir, _)) = &self.inner else {
            return;
        };
        let link = Self::link(dir, job);
        match std::fs::remove_file(&link) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                eprintln!(
                    "puzuz-helper: warning: cannot remove the GC root {}: {e}",
                    link.display()
                );
            }
            _ => {}
        }
    }

    /// Drops the roots of every job for which `keep` is false.
    pub fn retain(&self, keep: impl Fn(JobId) -> bool) {
        let Some((dir, _)) = &self.inner else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let job = name
                .to_str()
                .and_then(|n| n.strip_prefix("job-"))
                .and_then(|n| n.parse().ok())
                .map(JobId);
            if let Some(job) = job.filter(|job| !keep(*job)) {
                self.remove(job);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Nix {
        _tmp: tempfile::TempDir,
        gcroots: PathBuf,
        store: PathBuf,
        exe: PathBuf,
    }

    fn nix() -> Nix {
        let tmp = tempfile::tempdir().unwrap();
        let gcroots = tmp.path().join("gcroots");
        let store = tmp.path().join("store");
        let bin = store.join("abc-puzuz-0.1.0/bin");
        std::fs::create_dir_all(&gcroots).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let exe = bin.join("puzuz-helper");
        std::fs::write(&exe, b"").unwrap();
        Nix {
            _tmp: tmp,
            gcroots,
            store,
            exe,
        }
    }

    fn link(n: &Nix, job: u64) -> PathBuf {
        n.gcroots.join(format!("puzuz/job-{job}"))
    }

    #[test]
    fn roots_point_to_the_top_level_store_path() {
        let n = nix();
        let roots = GcRoots::new(&n.gcroots, &n.store, &n.exe);
        assert_eq!(
            roots.target(),
            Some(n.store.join("abc-puzuz-0.1.0").as_path())
        );
        roots.add(JobId(7));
        assert_eq!(
            std::fs::read_link(link(&n, 7)).unwrap(),
            n.store.join("abc-puzuz-0.1.0")
        );
        roots.remove(JobId(7));
        assert!(!link(&n, 7).exists());
        // Removing again is fine.
        roots.remove(JobId(7));
    }

    #[test]
    fn an_existing_root_is_kept() {
        let n = nix();
        let old = GcRoots::new(&n.gcroots, &n.store, &n.exe);
        old.add(JobId(1));
        let new_bin = n.store.join("def-puzuz-0.2.0/bin");
        std::fs::create_dir_all(&new_bin).unwrap();
        std::fs::write(new_bin.join("puzuz-helper"), b"").unwrap();
        let new = GcRoots::new(&n.gcroots, &n.store, &new_bin.join("puzuz-helper"));
        // A restore by the new helper keeps the root of the dumping one.
        new.add(JobId(1));
        assert_eq!(
            std::fs::read_link(link(&n, 1)).unwrap(),
            n.store.join("abc-puzuz-0.1.0")
        );
    }

    #[test]
    fn retain_drops_roots_of_unknown_jobs() {
        let n = nix();
        let roots = GcRoots::new(&n.gcroots, &n.store, &n.exe);
        for job in [1, 2, 3] {
            roots.add(JobId(job));
        }
        std::fs::write(n.gcroots.join("puzuz/unrelated"), b"").unwrap();
        roots.retain(|job| job.0 == 2);
        assert!(!link(&n, 1).exists() && link(&n, 2).exists() && !link(&n, 3).exists());
        assert!(n.gcroots.join("puzuz/unrelated").exists());
    }

    #[test]
    fn disabled_outside_the_store_or_without_gcroots() {
        let n = nix();
        let elsewhere = n.gcroots.parent().unwrap().join("puzuz-helper");
        std::fs::write(&elsewhere, b"").unwrap();
        let roots = GcRoots::new(&n.gcroots, &n.store, &elsewhere);
        assert_eq!(roots.target(), None);
        roots.add(JobId(1));
        assert!(!n.gcroots.join("puzuz").exists());

        let missing = n.gcroots.join("missing");
        assert_eq!(GcRoots::new(&missing, &n.store, &n.exe).target(), None);
    }
}
