//! Reading the configuration, and checking the programs and directories
//! used, as root.
//!
//! The helper runs as root, so whoever can change its configuration can
//! make root run any program (`criu_path`) or write anywhere
//! (`checkpoints_dir`). It therefore only reads a file that nobody but root
//! can change: the file and every parent directory must be owned by root,
//! must not be group/world-writable and must not be symlinks. The programs
//! and directories it uses are checked the same way, except that symlinks
//! owned by root are followed (see [`OwnershipCheck::check`]).
//!
//! The one directory that may be group-writable is the Nix store
//! (`/nix/store`, owned by root, group `nixbld`, mode `1775`): the sticky
//! bit keeps the build users from removing, renaming or replacing the
//! entries root owns, and the store paths below it must pass the normal
//! rule. The check only reads metadata; it never changes permissions.

use std::fmt;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use crate::{Config, ConfigError};

/// The Nix store, the only directory that may be group-writable.
pub const NIX_STORE: &str = "/nix/store";
/// The group of the Nix build users, which may add entries to the store.
pub const NIX_BUILD_GROUP: &str = "nixbld";
/// The group file the gid of [`NIX_BUILD_GROUP`] is read from.
pub const GROUP_FILE: &str = "/etc/group";

/// More symlinks than this on the way to a path are refused (as the kernel
/// does with `ELOOP`).
const MAX_SYMLINKS: u32 = 40;

/// Why a path is not trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Untrusted {
    /// The component that failed the check.
    pub path: PathBuf,
    /// The checked path, when `path` was reached through a symlink.
    pub origin: Option<PathBuf>,
    pub message: String,
}

impl Untrusted {
    /// Rewrites the paths in the error, e.g. to show them relative to
    /// another root.
    pub fn map_paths(self, f: impl Fn(&Path) -> PathBuf) -> Self {
        Self {
            path: f(&self.path),
            origin: self.origin.as_deref().map(&f),
            message: self.message,
        }
    }
}

impl fmt::Display for Untrusted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.path.display())?;
        if let Some(origin) = &self.origin {
            write!(f, " (which {} resolves through)", origin.display())?;
        }
        write!(f, " {}", self.message)
    }
}

/// Checks that nobody but `owner` (root) can change what a path refers to:
/// every existing component, from `/` down, must be owned by `owner` and
/// not writable by group or others, with the Nix store as the only
/// exception. Missing components end the check (they are created by root).
#[derive(Debug, Clone)]
pub struct OwnershipCheck {
    owner: u32,
    stop: Option<PathBuf>,
    store: PathBuf,
    store_gid: Option<u32>,
}

impl OwnershipCheck {
    /// The check for root: [`NIX_STORE`] may be writable by its group when
    /// that is [`NIX_BUILD_GROUP`] (as listed in [`GROUP_FILE`]) and the
    /// directory is sticky.
    pub fn root() -> Self {
        Self::new(
            0,
            None,
            PathBuf::from(NIX_STORE),
            group_gid(Path::new(GROUP_FILE), NIX_BUILD_GROUP),
        )
    }

    /// A check for `owner` instead of root that skips the ancestors of
    /// `stop` (`stop` itself is checked), with `store` as the Nix store
    /// whose group must be `store_gid` (`None`: no store is trusted). For
    /// tests, and for checking a system below another root directory.
    pub fn new(owner: u32, stop: Option<PathBuf>, store: PathBuf, store_gid: Option<u32>) -> Self {
        Self {
            owner,
            stop,
            store,
            store_gid,
        }
    }

    /// Checks `path`, following symlinks: a symlink must be owned by
    /// `owner` (replacing it needs write access to its directory, which is
    /// checked), and what it points to is checked in turn.
    pub fn check(&self, path: &Path) -> Result<(), Untrusted> {
        self.walk(path, true)
    }

    /// Checks `path` and refuses any symlink on the way.
    pub fn check_no_symlinks(&self, path: &Path) -> Result<(), Untrusted> {
        self.walk(path, false)
    }

    /// Whether an existing component passes: owned by `owner` and not
    /// writable by group or others, or the Nix store.
    pub fn trusted(&self, path: &Path, meta: &std::fs::Metadata) -> bool {
        meta.uid() == self.owner && (meta.mode() & 0o022 == 0 || self.is_store(path, meta))
    }

    /// The Nix store: exactly `store`, a sticky directory that may be
    /// writable by its group `nixbld` but not by others. With the sticky
    /// bit the build users can add entries but not remove or rename root's.
    /// Unlike the helper's mount checks, other sticky directories (`/tmp`)
    /// are not accepted: entries could be added on the way to a missing
    /// path.
    fn is_store(&self, path: &Path, meta: &std::fs::Metadata) -> bool {
        path == self.store
            && meta.is_dir()
            && meta.mode() & 0o1000 != 0
            && meta.mode() & 0o002 == 0
            && self.store_gid == Some(meta.gid())
    }

    /// Whether `path` is above `stop` (and so not checked).
    fn skipped(&self, path: &Path) -> bool {
        self.stop
            .as_deref()
            .is_some_and(|stop| stop != path && stop.starts_with(path))
    }

    fn walk(&self, path: &Path, follow: bool) -> Result<(), Untrusted> {
        let origin = path;
        let absolute;
        let path = if path.is_absolute() {
            path
        } else {
            let cwd = std::env::current_dir().map_err(|e| Untrusted {
                path: path.to_owned(),
                origin: None,
                message: format!("cannot be resolved: {e}"),
            })?;
            absolute = cwd.join(path);
            &absolute
        };
        let mut via_symlink = false;
        let fail = |p: &Path, via: bool, message: &str| Untrusted {
            path: p.to_owned(),
            origin: via.then(|| origin.to_owned()),
            message: message.to_owned(),
        };
        const NOT_PROTECTED: &str = "must be owned by root and not group/world-writable";

        // `current` never contains a symlink, and each of its components
        // passed the check, so `..` simply drops its last component.
        let mut current = PathBuf::from("/");
        if !self.skipped(&current) {
            let meta =
                std::fs::metadata(&current).map_err(|e| fail(&current, false, &e.to_string()))?;
            if !self.trusted(&current, &meta) {
                return Err(fail(&current, false, NOT_PROTECTED));
            }
        }
        // The components still to walk, the next one last.
        let mut rest = Vec::new();
        push_components(&mut rest, path);
        let mut symlinks = 0;
        while let Some(name) = rest.pop() {
            if name == Path::new("..") {
                current.pop();
                continue;
            }
            let next = current.join(&name);
            let meta = match std::fs::symlink_metadata(&next) {
                Ok(meta) => meta,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(fail(&next, via_symlink, &e.to_string())),
            };
            if !meta.file_type().is_symlink() {
                if !self.skipped(&next) && !self.trusted(&next, &meta) {
                    return Err(fail(&next, via_symlink, NOT_PROTECTED));
                }
                current = next;
                continue;
            }
            if !self.skipped(&next) {
                if !follow {
                    return Err(fail(&next, via_symlink, "is a symlink"));
                }
                if meta.uid() != self.owner {
                    return Err(fail(&next, via_symlink, "is a symlink not owned by root"));
                }
            }
            symlinks += 1;
            if symlinks > MAX_SYMLINKS {
                return Err(fail(&next, via_symlink, "has too many levels of symlinks"));
            }
            let target =
                std::fs::read_link(&next).map_err(|e| fail(&next, via_symlink, &e.to_string()))?;
            via_symlink = true;
            if target.is_absolute() {
                // `/` was checked already.
                current = PathBuf::from("/");
            }
            push_components(&mut rest, &target);
        }
        Ok(())
    }
}

/// Pushes the components of `path` onto the stack `rest` so that its first
/// component is popped first; `/` and `.` are dropped.
fn push_components(rest: &mut Vec<PathBuf>, path: &Path) {
    let names: Vec<PathBuf> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(PathBuf::from(name)),
            Component::ParentDir => Some(PathBuf::from("..")),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => None,
        })
        .collect();
    rest.extend(names.into_iter().rev());
}

/// The gid of the group `name` in the group file at `path` (`/etc/group`
/// format), if it is listed there.
pub fn group_gid(path: &Path, name: &str) -> Option<u32> {
    parse_group_gid(&std::fs::read_to_string(path).ok()?, name)
}

/// The gid of the group `name` in the text of a group file.
pub fn parse_group_gid(text: &str, name: &str) -> Option<u32> {
    text.lines().find_map(|line| {
        let mut fields = line.split(':');
        if fields.next()? != name {
            return None;
        }
        fields.nth(1)?.trim().parse().ok()
    })
}

/// Every existing component of `path` must be owned by root and not
/// writable by group or others (the Nix store excepted, see
/// [`OwnershipCheck`]); otherwise a less privileged user could redirect
/// what root runs, reads or writes there. Symlinks must be owned by root,
/// and what they point to is checked the same way.
pub fn check_root_owned_path(path: &Path) -> Result<(), String> {
    OwnershipCheck::root()
        .check(path)
        .map_err(|e| e.to_string())
}

impl Config {
    /// Reads `path` for a process running as root (the helper): refuses a
    /// file that is not an absolute path, is or is reached through a
    /// symlink, or that anyone but root could have written (see
    /// [`OwnershipCheck::check_no_symlinks`]). A missing file yields the
    /// defaults.
    pub fn load_trusted(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        Self::load_owned_by(path.as_ref(), 0, None)
    }

    /// [`Config::load_trusted`] for files owned by `owner`, checking
    /// ancestors up to `stop`. For tests.
    pub fn load_owned_by(
        path: &Path,
        owner: u32,
        stop: Option<&Path>,
    ) -> Result<Self, ConfigError> {
        let untrusted = |message: String| ConfigError::Untrusted {
            path: path.to_owned(),
            message,
        };
        if !path.is_absolute() {
            return Err(untrusted("the path must be absolute".to_owned()));
        }
        OwnershipCheck::new(
            owner,
            stop.map(Path::to_owned),
            PathBuf::from(NIX_STORE),
            group_gid(Path::new(GROUP_FILE), NIX_BUILD_GROUP),
        )
        .check_no_symlinks(path)
        .map_err(|e| untrusted(e.to_string()))?;
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        // The parents are trusted, so the file cannot have been swapped since
        // the check; look at what was opened anyway.
        let meta = file.metadata().map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        if !meta.is_file() {
            return Err(untrusted("not a regular file".to_owned()));
        }
        if meta.uid() != owner || meta.mode() & 0o022 != 0 {
            return Err(untrusted(
                "the file must be owned by root and not group/world-writable".to_owned(),
            ));
        }
        let text = io::read_to_string(file).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::parse_file(path, &text)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use super::*;

    fn me(dir: &Path) -> u32 {
        std::fs::metadata(dir).unwrap().uid()
    }

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let etc = dir.path().join("etc");
        std::fs::create_dir(&etc).unwrap();
        std::fs::set_permissions(&etc, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = etc.join("config.toml");
        (dir, path)
    }

    fn write(path: &Path, text: &str, mode: u32) {
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn accepts_a_protected_file_and_ignores_unknown_keys() {
        let (dir, path) = setup();
        write(
            &path,
            "criu_path = \"/opt/criu\"\nsome_future_key = 1\n[future_table]\nx = 2\n",
            0o644,
        );
        let config = Config::load_owned_by(&path, me(dir.path()), Some(dir.path())).unwrap();
        assert_eq!(config.criu_path, PathBuf::from("/opt/criu"));
    }

    #[test]
    fn missing_file_gives_defaults() {
        let (dir, path) = setup();
        let config = Config::load_owned_by(&path, me(dir.path()), Some(dir.path())).unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn refuses_writable_or_foreign_files() {
        let (dir, path) = setup();
        let uid = me(dir.path());
        write(&path, "", 0o664);
        let err = Config::load_owned_by(&path, uid, Some(dir.path())).unwrap_err();
        assert!(matches!(err, ConfigError::Untrusted { .. }), "{err}");

        write(&path, "", 0o646);
        assert!(Config::load_owned_by(&path, uid, Some(dir.path())).is_err());

        // Owned by someone else than the expected owner.
        write(&path, "", 0o644);
        let err = Config::load_owned_by(&path, uid + 1, Some(dir.path())).unwrap_err();
        assert!(err.to_string().contains("refusing to use"), "{err}");
    }

    #[test]
    fn refuses_writable_parents_and_symlinks() {
        let (dir, path) = setup();
        let uid = me(dir.path());
        write(&path, "", 0o644);
        let etc = path.parent().unwrap();
        std::fs::set_permissions(etc, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(Config::load_owned_by(&path, uid, Some(dir.path())).is_err());
        // Even when the file itself does not exist yet.
        std::fs::remove_file(&path).unwrap();
        assert!(Config::load_owned_by(&path, uid, Some(dir.path())).is_err());
        std::fs::set_permissions(etc, std::fs::Permissions::from_mode(0o755)).unwrap();

        let real = dir.path().join("real.toml");
        write(&real, "", 0o644);
        std::os::unix::fs::symlink(&real, &path).unwrap();
        let err = Config::load_owned_by(&path, uid, Some(dir.path())).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
    }

    #[test]
    fn refuses_relative_paths_and_reports_parse_errors() {
        assert!(matches!(
            Config::load_trusted("config.toml"),
            Err(ConfigError::Untrusted { .. })
        ));
        let (dir, path) = setup();
        write(&path, "grace_secs = \"x\"\n", 0o644);
        assert!(matches!(
            Config::load_owned_by(&path, me(dir.path()), Some(dir.path())),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn refuses_a_config_file_below_a_symlinked_directory() {
        let (dir, path) = setup();
        let uid = me(dir.path());
        write(&path, "", 0o644);
        let link = dir.path().join("etc-link");
        std::os::unix::fs::symlink(path.parent().unwrap(), &link).unwrap();
        let err =
            Config::load_owned_by(&link.join("config.toml"), uid, Some(dir.path())).unwrap_err();
        assert!(err.to_string().contains("is a symlink"), "{err}");
    }

    fn mkdir(path: &Path, mode: u32) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// `<tmp>/nix/store` (mode 1775) with `abc-criu/bin/criu` in it, and
    /// the check that trusts it as the store when `gid` is its group.
    struct Store {
        dir: tempfile::TempDir,
        store: PathBuf,
        criu: PathBuf,
        uid: u32,
        gid: u32,
    }

    impl Store {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            mkdir(dir.path(), 0o755);
            mkdir(&dir.path().join("nix"), 0o755);
            let store = dir.path().join("nix/store");
            mkdir(&store, 0o1775);
            mkdir(&store.join("abc-criu/bin"), 0o755);
            let criu = store.join("abc-criu/bin/criu");
            write(&criu, "", 0o555);
            let meta = std::fs::metadata(dir.path()).unwrap();
            Self {
                store,
                criu,
                uid: meta.uid(),
                gid: std::fs::metadata(dir.path().join("nix/store"))
                    .unwrap()
                    .gid(),
                dir,
            }
        }

        fn check(&self, gid: Option<u32>) -> OwnershipCheck {
            OwnershipCheck::new(
                self.uid,
                Some(self.dir.path().to_owned()),
                self.store.clone(),
                gid,
            )
        }

        fn trusted(&self) -> OwnershipCheck {
            self.check(Some(self.gid))
        }
    }

    #[test]
    fn accepts_the_sticky_nix_store_of_the_build_group() {
        let s = Store::new();
        s.trusted().check(&s.criu).unwrap();
        s.trusted().check_no_symlinks(&s.criu).unwrap();
        // Also a missing path below it (nothing to replace there).
        s.trusted().check(&s.store.join("missing/bin")).unwrap();
    }

    #[test]
    fn refuses_the_store_without_sticky_bit_or_build_group() {
        let s = Store::new();
        mkdir(&s.store, 0o775);
        let err = s.trusted().check(&s.criu).unwrap_err();
        assert_eq!(err.path, s.store, "{err}");
        assert!(err.to_string().contains("must be owned by root"), "{err}");

        mkdir(&s.store, 0o1777);
        assert!(s.trusted().check(&s.criu).is_err(), "world-writable");

        mkdir(&s.store, 0o1775);
        assert!(s.check(Some(s.gid + 1)).check(&s.criu).is_err(), "group");
        assert!(s.check(None).check(&s.criu).is_err(), "no nixbld group");

        // Owned by someone else.
        let other = OwnershipCheck::new(
            s.uid + 1,
            Some(s.store.clone()),
            s.store.clone(),
            Some(s.gid),
        );
        assert!(other.check(&s.criu).is_err());
    }

    #[test]
    fn only_the_store_itself_may_be_group_writable() {
        let s = Store::new();
        // Another sticky, group-writable directory.
        let other = s.dir.path().join("nix/other");
        mkdir(&other.join("bin"), 0o755);
        mkdir(&other, 0o1775);
        let err = s.trusted().check(&other.join("bin")).unwrap_err();
        assert_eq!(err.path, other, "{err}");

        // A group-writable entry of the store.
        let entry = s.store.join("abc-criu");
        mkdir(&entry, 0o775);
        let err = s.trusted().check(&s.criu).unwrap_err();
        assert_eq!(err.path, entry, "{err}");
        mkdir(&entry, 0o755);

        // A group-writable parent of the store.
        let nix = s.dir.path().join("nix");
        mkdir(&nix, 0o775);
        let err = s.trusted().check(&s.criu).unwrap_err();
        assert_eq!(err.path, nix, "{err}");
        mkdir(&nix, 0o755);
        s.trusted().check(&s.criu).unwrap();
    }

    /// `<tmp>/run/current-system` -> `<store>/xyz-system` (absolute), and
    /// `<store>/xyz-system/sw/bin/criu` -> `../../../abc-criu/bin/criu`.
    fn nixos_links(s: &Store) -> PathBuf {
        mkdir(&s.dir.path().join("run"), 0o755);
        let system = s.store.join("xyz-system");
        mkdir(&system.join("sw/bin"), 0o755);
        std::os::unix::fs::symlink("../../../abc-criu/bin/criu", system.join("sw/bin/criu"))
            .unwrap();
        std::os::unix::fs::symlink(&system, s.dir.path().join("run/current-system")).unwrap();
        s.dir.path().join("run/current-system/sw/bin/criu")
    }

    #[test]
    fn follows_root_owned_symlinks_into_the_store() {
        let s = Store::new();
        let criu = nixos_links(&s);
        s.trusted().check(&criu).unwrap();
        // The strict check (the configuration file) refuses them.
        let err = s.trusted().check_no_symlinks(&criu).unwrap_err();
        assert!(err.to_string().contains("is a symlink"), "{err}");
        // A group-writable directory on the way of the second link.
        let bin = s.store.join("xyz-system/sw/bin");
        mkdir(&bin, 0o775);
        let err = s.trusted().check(&criu).unwrap_err();
        assert_eq!(err.path, bin, "{err}");
        assert_eq!(err.origin.as_deref(), Some(criu.as_path()), "{err}");
    }

    #[test]
    fn refuses_symlinks_to_unprotected_targets() {
        let s = Store::new();
        let bad = s.dir.path().join("bad");
        mkdir(&bad, 0o777);
        write(&bad.join("criu"), "", 0o755);
        let link = s.dir.path().join("criu");
        std::os::unix::fs::symlink(bad.join("criu"), &link).unwrap();
        let err = s.trusted().check(&link).unwrap_err();
        assert_eq!(err.path, bad, "{err}");
        assert!(err.to_string().contains("which"), "{err}");

        // A writable target file.
        mkdir(&bad, 0o755);
        write(&bad.join("criu"), "", 0o757);
        assert!(s.trusted().check(&link).is_err());
    }

    #[test]
    fn refuses_symlinks_owned_by_others() {
        let s = Store::new();
        let link = s.dir.path().join("criu");
        std::os::unix::fs::symlink(&s.criu, &link).unwrap();
        s.trusted().check(&link).unwrap();
        // Checked from the link down, as owned by someone else: the link
        // itself is the first component checked.
        let other = OwnershipCheck::new(s.uid + 1, Some(link.clone()), s.store.clone(), None);
        let err = other.check(&link).unwrap_err();
        assert_eq!(err.path, link, "{err}");
        assert!(
            err.to_string().contains("symlink not owned by root"),
            "{err}"
        );

        if s.uid == 0 {
            std::os::unix::fs::lchown(&link, Some(12345), None).unwrap();
            let err = s.trusted().check(&link).unwrap_err();
            assert!(
                err.to_string().contains("symlink not owned by root"),
                "{err}"
            );
        }
    }

    #[test]
    fn refuses_symlink_loops() {
        let s = Store::new();
        let a = s.dir.path().join("a");
        std::os::unix::fs::symlink("b", &a).unwrap();
        std::os::unix::fs::symlink("a", s.dir.path().join("b")).unwrap();
        let err = s.trusted().check(&a).unwrap_err();
        assert!(err.to_string().contains("too many"), "{err}");
    }

    #[test]
    fn parses_group_files() {
        let text = "root:x:0:\nnixbld:x:30000:nixbld1,nixbld2\nnixbldx:x:1:\n";
        assert_eq!(parse_group_gid(text, "nixbld"), Some(30000));
        assert_eq!(parse_group_gid(text, "root"), Some(0));
        assert_eq!(parse_group_gid(text, "wheel"), None);
        assert_eq!(parse_group_gid("nixbld:x:abc:\n", "nixbld"), None);
    }

    #[test]
    fn root_check_rejects_user_owned_paths() {
        let (_dir, path) = setup();
        write(&path, "", 0o644);
        if me(path.parent().unwrap()) != 0 {
            assert!(check_root_owned_path(&path).is_err());
            assert!(Config::load_trusted(&path).is_err());
        }
    }
}
