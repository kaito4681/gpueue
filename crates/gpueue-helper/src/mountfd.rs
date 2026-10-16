//! `O_PATH` descriptors for mount surgery and for paths that root binds,
//! creates or mounts on.
//!
//! Paths come from mountinfo, CRIU's images or the configuration, but the
//! directories on the way may belong to a job's owner, who can rename them
//! or replace them with symlinks at any time (e.g. on a FUSE mount of their
//! own). Root never resolves such a path twice and never follows a
//! symlink: it walks to the parent once, one component at a time,
//! accepting only directories that nobody but root can change (for the
//! host paths CRIU binds, also those of system accounts that no job runs
//! as, see [`OwnerTrust`]), and then works relative to descriptors.
//!
//! Used by the shim (in the job's namespace, see [`crate::shim`]), by the
//! helper for the mount points it creates before a restore (see
//! [`crate::jobs::ensure_restore_mount_points`]), and to decide whether a
//! host path that `criu restore` binds by its name can be redirected (see
//! [`bind_source_problem`]).

use std::os::fd::{AsFd, AsRawFd, OwnedFd};

use rustix::fs::{AtFlags, Mode, OFlags, ResolveFlags, StatxFlags};
use serde::{Deserialize, Serialize};

const PATH_FLAGS: OFlags = OFlags::PATH
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
/// For a mount point itself: without `O_DIRECTORY`, which would make
/// the lookup trigger an automount point (in the job's namespace, where
/// the automounter refuses with `EPERM`).
const LEAF_FLAGS: OFlags = OFlags::PATH.union(OFlags::NOFOLLOW).union(OFlags::CLOEXEC);
const RESOLVE: ResolveFlags = ResolveFlags::NO_SYMLINKS.union(ResolveFlags::NO_MAGICLINKS);

/// `f_type` of a FUSE filesystem (`statfs(2)`).
const FUSE_SUPER_MAGIC: i128 = 0x6573_5546;

/// Whose directories a walk accepts on the way to a path: root's, and
/// with [`OwnerTrust::system`] also those of system accounts that no job
/// may run as.
///
/// A job's owner can redirect a path through a directory they own (rename
/// an entry, leave a symlink). A system account's directory, such as
/// `/run/systemd/resolve` (systemd-resolve's on Ubuntu), is out of their
/// reach: no job runs as such an account (see `job_min_uid`), and the
/// daemon's user, the one system account that acts for users, is never
/// trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerTrust {
    /// Owners with a uid below this one count like root (`0`: only root).
    #[serde(default)]
    pub system_below: u32,
    /// An owner that never counts, even below `system_below`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub except: Option<u32>,
}

impl Default for OwnerTrust {
    fn default() -> Self {
        Self::ROOT_ONLY
    }
}

impl OwnerTrust {
    /// Only root's directories.
    pub const ROOT_ONLY: Self = Self {
        system_below: 0,
        except: None,
    };

    /// Root's directories and those of the accounts below `job_min_uid`
    /// (the job policy's minimum uid: no job runs as them, whatever
    /// `job_allowed_users` says), but the daemon's user `daemon_uid`.
    pub fn system(job_min_uid: u32, daemon_uid: u32) -> Self {
        Self {
            system_below: job_min_uid,
            except: Some(daemon_uid),
        }
    }

    /// Whether a directory owned by `uid` is out of the reach of jobs'
    /// owners.
    pub fn trusts(&self, uid: u32) -> bool {
        uid == 0 || (uid < self.system_below && self.except != Some(uid))
    }

    /// Who may change the directories that a walk accepts, for messages.
    fn who(&self) -> &'static str {
        if self.system_below > 1 {
            "only root or a system account"
        } else {
            "only root"
        }
    }
}

/// Whether a directory with this mode, owned by one that the walk
/// trusts (`trusted_owner`: root, a system account, or a test's own
/// user), can only be changed by its owner: not group/world-writable, or
/// sticky (then others cannot rename the owner's entries, and the next
/// component is checked in turn).
pub fn controlled_by(trusted_owner: bool, mode: u32) -> bool {
    trusted_owner && (mode & 0o022 == 0 || mode & 0o1000 != 0)
}

/// The parent directory of the absolute `path` and its last
/// component; `Err` if a component is a symlink or a directory on the
/// way is not controlled by root ([`controlled_by`]) or is on a FUSE
/// filesystem (its daemon decides what owner and mode it reports and
/// what a lookup in it returns, a symlink included).
pub fn open_parent(path: &str) -> Result<(OwnedFd, String), String> {
    open_parent_trusting(path, OwnerTrust::ROOT_ONLY)
}

/// [`open_parent`], accepting the directories of the owners that `trust`
/// trusts.
pub fn open_parent_trusting(path: &str, trust: OwnerTrust) -> Result<(OwnedFd, String), String> {
    open_parent_in("/", path, |uid| trust.trusts(uid), trust.who())
}

/// [`open_parent`] with the walk starting at the directory `top`
/// (instead of `/`) and every directory on the way required to be owned
/// by one that `trusted` accepts (`who` says who that is, for messages)
/// and controlled by them: `path`'s components are looked up from `top`.
/// Tests use it with their own user, as they cannot rely on `/` and the
/// system directories being root's (e.g. in a Nix build sandbox).
pub(crate) fn open_parent_in(
    top: &str,
    path: &str,
    trusted: impl Fn(u32) -> bool,
    who: &str,
) -> Result<(OwnedFd, String), String> {
    let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    let Some((last, dirs)) = comps.split_last() else {
        return Err(format!("{path:?} has no parent"));
    };
    if !path.starts_with('/') {
        return Err(format!("{path:?} is not an absolute path"));
    }
    if comps.iter().any(|c| *c == "." || *c == "..") {
        return Err(format!("{path:?} is not a canonical path"));
    }
    let mut fd = rustix::fs::open(top, PATH_FLAGS, Mode::empty())
        .map_err(|e| format!("opening {top}: {e}"))?;
    check(&fd, top, &trusted, who)?;
    let mut walked = String::new();
    for c in dirs {
        walked.push('/');
        walked.push_str(c);
        fd = rustix::fs::openat2(&fd, *c, PATH_FLAGS, Mode::empty(), RESOLVE)
            .map_err(|e| format!("opening {walked} without following symlinks: {e}"))?;
        check(&fd, &walked, &trusted, who)?;
    }
    Ok((fd, (*last).to_owned()))
}

fn check(
    fd: &OwnedFd,
    what: &str,
    trusted: &impl Fn(u32) -> bool,
    who: &str,
) -> Result<(), String> {
    let st = rustix::fs::fstat(fd).map_err(|e| format!("{what}: {e}"))?;
    if !controlled_by(trusted(st.st_uid), st.st_mode) {
        return Err(format!("{what} is not a directory that {who} can change"));
    }
    let fs = rustix::fs::fstatfs(fd).map_err(|e| format!("{what}: {e}"))?;
    #[allow(clippy::useless_conversion, clippy::unnecessary_fallible_conversions)]
    let ty = i128::from(fs.f_type);
    if ty == FUSE_SUPER_MAGIC {
        return Err(format!(
            "{what} is on a FUSE filesystem, whose daemon decides what is there"
        ));
    }
    Ok(())
}

/// `name` in `dir`, without following symlinks: the root of whatever is
/// mounted there, or the directory itself.
pub fn open_at(dir: &OwnedFd, name: &str) -> Result<OwnedFd, String> {
    let fd = rustix::fs::openat2(dir, name, LEAF_FLAGS, Mode::empty(), RESOLVE)
        .map_err(|e| format!("opening {name}: {e}"))?;
    // `O_NOFOLLOW` opens a symlink itself.
    let st = rustix::fs::fstat(&fd).map_err(|e| format!("{name}: {e}"))?;
    if st.st_mode & 0o170000 == 0o120000 {
        return Err(format!("{name} is a symlink"));
    }
    Ok(fd)
}

/// `path` without following symlinks (no ownership requirements).
pub fn open_nofollow(path: &str) -> Result<OwnedFd, String> {
    rustix::fs::openat2(rustix::fs::CWD, path, LEAF_FLAGS, Mode::empty(), RESOLVE)
        .map_err(|e| format!("opening {path}: {e}"))
}

/// The id of the mount `fd` is on (as in mountinfo). `statx` asks the
/// filesystem, and a FUSE mount without `allow_other` refuses root with
/// `EACCES`; then the `mnt_id` of `/proc/self/fdinfo`, which does not.
pub fn mnt_id(fd: &OwnedFd) -> Result<u64, String> {
    rustix::fs::statx(fd, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)
        .map(|s| s.stx_mnt_id)
        .or_else(|e| {
            let info =
                std::fs::read_to_string(format!("/proc/self/fdinfo/{}", fd.as_fd().as_raw_fd()))
                    .map_err(|_| format!("statx: {e}"))?;
            info.lines()
                .find_map(|l| l.strip_prefix("mnt_id:"))
                .and_then(|v| v.trim().parse().ok())
                .ok_or_else(|| format!("statx: {e}"))
        })
}

/// The `/proc/self/fd` link of `fd`, which mount calls follow to the very
/// object (so a path is not resolved again).
pub fn fd_path(fd: &OwnedFd) -> String {
    format!("/proc/self/fd/{}", fd.as_fd().as_raw_fd())
}

/// Detach the mount whose root `fd` is (`umount2` through its
/// `/proc/self/fd` link, so the path is not resolved again).
pub fn detach(fd: &OwnedFd) -> Result<(), String> {
    nix::mount::umount2(fd_path(fd).as_str(), nix::mount::MntFlags::MNT_DETACH)
        .map_err(|e| format!("detaching: {e}"))
}

/// The directory `dir` (an absolute path, which may name a link such as
/// `/var/run` on the way) as a descriptor, created as root (`0755`) if
/// only it is missing, and its canonical path. `Ok(None)` if its parent
/// does not exist.
///
/// The parent is canonicalized once and then walked with
/// [`open_parent`]: every directory on the way must be one that only root
/// can change, so nobody else can redirect the directory that is created
/// or mounted on. The helper checks at start-up that the configured
/// directories have no other ancestors (see
/// [`crate::config::HelperConfig::paths_to_check`]).
pub fn ensure_root_dir(dir: &str) -> Result<Option<(OwnedFd, String)>, String> {
    let path = std::path::Path::new(dir);
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(format!("{dir:?} has no parent"));
    };
    let name = name
        .to_str()
        .ok_or_else(|| format!("{dir:?} is not UTF-8"))?;
    let parent = match std::fs::canonicalize(parent) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", parent.display())),
    };
    let real = parent.join(name);
    let real = real
        .to_str()
        .ok_or_else(|| format!("{} is not UTF-8", real.display()))?
        .to_owned();
    let (pfd, name) = open_parent(&real)?;
    let fd = match open_at(&pfd, &name) {
        Ok(fd) => fd,
        Err(_) => {
            match rustix::fs::mkdirat(&pfd, name.as_str(), Mode::from_raw_mode(0o755)) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                Err(e) => return Err(format!("creating {real}: {e}")),
            }
            open_at(&pfd, &name)?
        }
    };
    let st = rustix::fs::fstat(&fd).map_err(|e| format!("{real}: {e}"))?;
    if st.st_mode & 0o170000 != 0o040000 {
        return Err(format!("{real}: not a directory"));
    }
    Ok(Some((fd, real)))
}

/// Why `criu restore` must not bind the host path `path` (an external
/// mount source, which CRIU looks up by its name, as root, following
/// symlinks on the way), if it must not: a directory on the way is one
/// that someone `trust` does not trust can change (see [`OwnerTrust`]),
/// or is on a FUSE filesystem, so whoever controls it could make the
/// lookup end somewhere else, such as in another user's directory, between
/// the checks and the restore. `/` itself is always fine.
pub fn bind_source_problem(path: &str, trust: OwnerTrust) -> Option<String> {
    if path.split('/').all(|c| c.is_empty()) {
        return None;
    }
    open_parent_trusting(path, trust).err()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_surgery_only_walks_root_controlled_directories() {
        let root = OwnerTrust::ROOT_ONLY;
        assert!(controlled_by(root.trusts(0), 0o40755));
        assert!(controlled_by(root.trusts(0), 0o41777), "sticky like /tmp");
        assert!(!controlled_by(root.trusts(0), 0o40777));
        assert!(!controlled_by(root.trusts(0), 0o40775));
        assert!(!controlled_by(root.trusts(1000), 0o40755));
        assert!(!controlled_by(root.trusts(991), 0o40755));

        // The walk starts at a directory tree of our own, with ourselves
        // as the owner that must control it: `/` and the system
        // directories are not root's everywhere (e.g. in a Nix build
        // sandbox).
        use std::os::unix::fs::PermissionsExt;
        let me = nix::unistd::geteuid().as_raw();
        let tmp = tempfile::tempdir().unwrap();
        let top = tmp.path().to_str().unwrap();
        let mkdir = |rel: &str, mode: u32| {
            let p = tmp.path().join(rel);
            std::fs::create_dir(&p).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        mkdir("usr", 0o755);
        mkdir("usr/bin", 0o755);
        std::fs::write(tmp.path().join("usr/bin/env"), b"").unwrap();
        mkdir("proc", 0o755);
        mkdir("proc/1", 0o555);
        std::os::unix::fs::symlink("1", tmp.path().join("proc/self")).unwrap();
        mkdir("tmp", 0o1777);
        mkdir("tmp/d", 0o755);
        mkdir("home", 0o777);
        mkdir("srv", 0o775);

        let mine = |uid: u32| uid == me;
        let who = "only root";
        let (_, name) = open_parent_in(top, "/usr/bin/env", mine, who).unwrap();
        assert_eq!(name, "env");
        // Sticky (like /tmp): the next component is checked in turn.
        open_parent_in(top, "/tmp/d/x", mine, who).unwrap();
        // A symlink on the way (like `/proc/self`) is not followed.
        let e = open_parent_in(top, "/proc/self/fd/0", mine, who).unwrap_err();
        assert!(e.contains("/proc/self"), "{e}");
        // A directory that others than its owner can change: entries
        // could be renamed or replaced by a symlink.
        let e = open_parent_in(top, "/home/mnt", mine, who).unwrap_err();
        assert!(
            e.contains("/home is not") && e.contains("only root can change"),
            "{e}"
        );
        let e = open_parent_in(top, "/srv/mnt", mine, who).unwrap_err();
        assert!(e.contains("/srv is not"), "{e}");
        // A directory owned by someone else (e.g. a user's directory when
        // the walk wants root's): already the top is refused.
        let e = open_parent_in(top, "/usr/bin/env", |uid| uid == me ^ 1, who).unwrap_err();
        assert!(e.contains("only root can change"), "{e}");
        assert!(open_parent("/").is_err());
        assert!(open_parent("/usr/../etc/x").is_err());
        assert!(open_parent("usr/bin").is_err());
        assert_eq!(bind_source_problem("/", OwnerTrust::ROOT_ONLY), None);
    }

    /// Bind sources below a directory of a system account (such as
    /// `/run/systemd/resolve`, systemd-resolve's on Ubuntu) are out of
    /// the reach of jobs' owners; a user's own directory is not.
    #[test]
    fn bind_sources_may_lie_below_system_accounts_directories() {
        // UID_MIN 1000; the daemon runs as 997.
        let trust = OwnerTrust::system(1000, 997);
        assert!(trust.trusts(0));
        assert!(trust.trusts(991), "systemd-resolve");
        assert!(trust.trusts(1), "daemon");
        assert!(!trust.trusts(997), "the daemon's user acts for users");
        assert!(!trust.trusts(1000), "a user who may run jobs");
        assert!(!trust.trusts(65534));
        assert!(controlled_by(trust.trusts(991), 0o40755));
        assert!(controlled_by(trust.trusts(991), 0o41777));
        assert!(!controlled_by(trust.trusts(991), 0o40775), "group-writable");
        assert!(!controlled_by(trust.trusts(991), 0o40757), "world-writable");
        assert!(!controlled_by(trust.trusts(1000), 0o40755));
        assert!(!controlled_by(trust.trusts(997), 0o40755));
        // A lower job_min_uid trusts fewer accounts.
        assert!(!OwnerTrust::system(500, 997).trusts(991));
        // The test policy (job_min_uid 1) and older requests: root only.
        assert!(!OwnerTrust::system(1, 997).trusts(991));
        assert_eq!(OwnerTrust::default(), OwnerTrust::ROOT_ONLY);
        let old: OwnerTrust = serde_json::from_str("{}").unwrap();
        assert_eq!(old, OwnerTrust::ROOT_ONLY);

        // A walk through directories of a trusted owner (ourselves, as
        // for a system account) passes; a user's (anyone else) does not.
        use std::os::unix::fs::PermissionsExt;
        let me = nix::unistd::geteuid().as_raw();
        let tmp = tempfile::tempdir().unwrap();
        let top = tmp.path().to_str().unwrap();
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::create_dir_all(tmp.path().join("run/systemd/resolve")).unwrap();
        for d in ["run", "run/systemd", "run/systemd/resolve"] {
            std::fs::set_permissions(tmp.path().join(d), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        let system = OwnerTrust::system(me.saturating_add(1), u32::MAX);
        let who = system.who();
        assert_eq!(who, "only root or a system account");
        let (_, name) = open_parent_in(
            top,
            "/run/systemd/resolve/io.systemd.Resolve",
            |uid| system.trusts(uid),
            who,
        )
        .unwrap();
        assert_eq!(name, "io.systemd.Resolve");
        let e = open_parent_in(
            top,
            "/run/systemd/resolve/io.systemd.Resolve",
            |uid| uid != me,
            who,
        )
        .unwrap_err();
        assert!(
            e.contains("only root or a system account can change"),
            "{e}"
        );
        // Group-writable: a member of the group could rename entries.
        std::fs::set_permissions(
            tmp.path().join("run/systemd/resolve"),
            std::fs::Permissions::from_mode(0o775),
        )
        .unwrap();
        let e = open_parent_in(
            top,
            "/run/systemd/resolve/io.systemd.Resolve",
            |uid| system.trusts(uid),
            who,
        )
        .unwrap_err();
        assert!(e.contains("/run/systemd/resolve is not"), "{e}");
    }
}
