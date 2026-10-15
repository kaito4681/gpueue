//! FUSE and network mounts of a job's namespace whose filesystem is gone,
//! does not answer, or cannot be accessed by root ("dead" mounts), and how
//! a dump and a restore get along without them.
//!
//! Every job's mount namespace is a private copy of the host's, so it holds
//! every FUSE or network filesystem that was mounted when the job started,
//! whether the job uses it or not. CRIU cannot save such a filesystem
//! itself: with `--external mnt[]` it dumps it as an external bind of the
//! path where the helper's (= the host's) namespace has it, and fails the
//! whole dump otherwise (`failed to dump fs ./nfs (fuse): always fail`).
//! When the FUSE daemon of a mount exits (sshfs whose connection is lost
//! unmounts it on the host) or its server stops answering, every job would
//! therefore fail to checkpoint, including jobs that never touched it.
//! A FUSE mount without `allow_other` (an AppImage's, a user's own sshfs,
//! the document portal) is alive but refuses root, so CRIU cannot restore
//! a bind of it, and is treated the same way.
//!
//! Before a dump the helper looks for such mounts ([`remote_mounts`]: FUSE
//! or network filesystems that the host no longer has, whose `statfs`
//! fails with a connection error or does not answer within
//! [`PROBE_TIMEOUT`], or whose root root cannot look at) and for anything
//! of the job that depends on them ([`dead_mount_users`]: open
//! descriptors, working and root directories, executables, file mappings
//! and bound Unix sockets).
//!
//! - A job that depends on a dead mount is not dumped: the dump is refused
//!   (the job is left alone) with a reason that names the mount.
//! - Otherwise the dead mounts are *detached*: each is passed to CRIU as an
//!   external mount with a key of ours (`--external mnt[<mount point>]:
//!   <key>`), so CRIU records it without looking at it, and mounts below
//!   one are left out (`--skip-mnt`). The restore binds, for each key
//!   ([`restore_args`]), the host's mount at the same place if the same
//!   type of filesystem is mounted there again and answers, and an empty,
//!   inaccessible directory (mode 0, root's) otherwise: the job gets the
//!   filesystem back once it is back, and never sees what the host has
//!   underneath its mount point.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::mounts::MountTree;

/// How long a dead-mount probe waits for a filesystem to answer.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The directory, below the checkpoint base, that a restore binds in place
/// of a detached mount whose filesystem is not back.
pub const PLACEHOLDER_DIR: &str = "detached-mount";

/// Whether CRIU can only dump a mount of this filesystem type as an
/// external bind (it cannot save or recreate the filesystem itself), and
/// the filesystem may disappear or stop answering on its own: FUSE and
/// network filesystems.
pub fn is_remote_fstype(fstype: &str) -> bool {
    let base = fstype.split('.').next().unwrap_or(fstype);
    matches!(
        base,
        "fuse"
            | "fuseblk"
            | "nfs"
            | "nfs4"
            | "cifs"
            | "smb3"
            | "smbfs"
            | "9p"
            | "ceph"
            | "glusterfs"
            | "lustre"
            | "afs"
            | "gpfs"
            | "beegfs"
    )
}

/// A FUSE or network mount of a job's namespace (see [`is_remote_fstype`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteMount {
    pub id: u64,
    pub mount_point: String,
    pub fstype: String,
    /// `major:minor` of the filesystem.
    pub dev: String,
    /// Whether the helper's namespace still has a mount of the filesystem
    /// (the same device). Without one, CRIU cannot dump it as an external
    /// bind.
    pub on_host: bool,
}

/// The FUSE and network mounts of `job` (but its root), and whether
/// `helper` (the helper's own namespace) still has each filesystem.
pub fn remote_mounts(job: &MountTree, helper: &MountTree) -> Vec<RemoteMount> {
    let root = job.root_id();
    job.entries()
        .iter()
        .filter(|e| Some(e.id) != root && is_remote_fstype(&e.fstype))
        .map(|e| RemoteMount {
            id: e.id,
            mount_point: e.mount_point.clone(),
            fstype: e.fstype.clone(),
            dev: e.dev.clone(),
            on_host: helper.entries().iter().any(|h| h.dev == e.dev),
        })
        .collect()
}

/// A mount of a job's namespace that CRIU cannot dump as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeadMount {
    pub id: u64,
    pub mount_point: String,
    pub fstype: String,
    pub dev: String,
    /// Why, for messages: "is no longer mounted on the host", "does not
    /// answer (...)" or [`NO_ROOT_ACCESS`].
    pub why: String,
}

impl DeadMount {
    fn describe(&self) -> String {
        format!("{} ({}) {}", self.mount_point, self.fstype, self.why)
    }
}

/// What [`crate::host::HostProbe::dead_mounts`] found in a job's namespace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeadMounts {
    /// Dead mounts that are not below another dead mount.
    pub dead: Vec<DeadMount>,
    /// Mount points of the mounts below a dead mount (of any type).
    pub below: Vec<String>,
    /// What of the job depends on a dead mount: (mount point of the dead
    /// mount, what), e.g. `("/nfs", "the working directory /nfs/run")`.
    pub used: Vec<(String, String)>,
}

/// The dead ones among `remote` (see [`DeadMount::why`]), given the
/// answers of their probes (one per mount, `None` if it answered), and the
/// mounts of `job` below them.
pub fn dead_mounts(
    job: &MountTree,
    remote: &[RemoteMount],
    answers: &[Option<String>],
) -> DeadMounts {
    let mut all: Vec<DeadMount> = Vec::new();
    for (i, m) in remote.iter().enumerate() {
        let why = if !m.on_host {
            "is no longer mounted on the host".to_owned()
        } else if let Some(Some(why)) = answers.get(i) {
            why.clone()
        } else {
            continue;
        };
        all.push(DeadMount {
            id: m.id,
            mount_point: m.mount_point.clone(),
            fstype: m.fstype.clone(),
            dev: m.dev.clone(),
            why,
        });
    }
    let below_dead = |id: u64| {
        all.iter()
            .any(|d| d.id != id && job.is_at_or_below(id, d.id))
    };
    let mut out = DeadMounts::default();
    for e in job.entries() {
        if below_dead(e.id) && !out.below.contains(&e.mount_point) {
            out.below.push(e.mount_point.clone());
        }
    }
    out.dead = all.iter().filter(|d| !below_dead(d.id)).cloned().collect();
    out
}

/// What one process of a job holds, as read from `/proc/<pid>` (paths as
/// the process sees them, in the job's namespace).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessFiles {
    /// Open descriptors: (number, `mnt_id` from `fdinfo`, link target).
    pub fds: Vec<(i32, Option<u64>, String)>,
    pub cwd: Option<String>,
    pub root: Option<String>,
    pub exe: Option<String>,
    /// File mappings: (`major:minor` in decimal, path).
    pub maps: Vec<(String, String)>,
    /// Paths that Unix sockets of the process are bound to.
    pub sockets: Vec<String>,
}

/// What in `procs` depends on the dead mounts of `dead` (or the mounts
/// below them): see [`DeadMounts::used`]. Descriptors are matched by their
/// mount id, mappings by device or path, everything else by path; nothing
/// here touches the dead filesystem.
pub fn dead_mount_users(
    job: &MountTree,
    dead: &DeadMounts,
    procs: &[ProcessFiles],
) -> Vec<(String, String)> {
    // The dead mount that `id` is or is below.
    let owner =
        |id: u64| -> Option<&DeadMount> { dead.dead.iter().find(|d| job.is_at_or_below(id, d.id)) };
    let by_path = |path: &str| -> Option<&DeadMount> {
        let path = path.strip_suffix(" (deleted)").unwrap_or(path);
        if !path.starts_with('/') {
            return None;
        }
        job.resolve(path).last().and_then(|id| owner(*id))
    };
    let mut out: Vec<(String, String)> = Vec::new();
    let mut add = |d: Option<&DeadMount>, what: String| {
        if let Some(d) = d {
            let entry = (d.mount_point.clone(), what);
            if !out.contains(&entry) {
                out.push(entry);
            }
        }
    };
    for p in procs {
        for (fd, mnt_id, target) in &p.fds {
            if let Some(d) = mnt_id.and_then(owner) {
                add(Some(d), format!("the open file {target} (fd {fd})"));
            }
        }
        for (link, what) in [
            (&p.cwd, "the working directory"),
            (&p.root, "the root directory"),
            (&p.exe, "the executable"),
        ] {
            if let Some(path) = link {
                add(by_path(path), format!("{what} {path}"));
            }
        }
        for (dev, path) in &p.maps {
            let d = dead
                .dead
                .iter()
                .find(|d| &d.dev == dev)
                .or_else(|| by_path(path));
            add(d, format!("the mapped file {path}"));
        }
        for path in &p.sockets {
            add(by_path(path), format!("a Unix socket bound to {path}"));
        }
    }
    out
}

/// Why a job with `dead` mounts cannot be dumped, if it cannot: it depends
/// on one of them, or one of them cannot be detached (see
/// [`DetachedMount::for_dump`]).
pub fn refusal(dead: &DeadMounts) -> Option<String> {
    const SHOWN: usize = 5;
    if !dead.used.is_empty() {
        let mut parts: Vec<String> = Vec::new();
        for d in &dead.dead {
            let what: Vec<&str> = dead
                .used
                .iter()
                .filter(|(mp, _)| mp == &d.mount_point)
                .map(|(_, w)| w.as_str())
                .collect();
            if what.is_empty() {
                continue;
            }
            let more = what.len().saturating_sub(SHOWN);
            let mut list = what[..what.len().min(SHOWN)].join(", ");
            if more > 0 {
                list.push_str(&format!(" and {more} more"));
            }
            parts.push(format!("{}; the job uses it: {list}", d.describe()));
        }
        return Some(format!(
            "the job depends on a filesystem that is gone, does not answer or root cannot \
             access: {}. CRIU cannot save what the job has open there, so it is not \
             checkpointed; mount the filesystem again (with `allow_other` if root cannot \
             access it), or end the job",
            parts.join("; ")
        ));
    }
    let bad: Vec<String> = dead
        .dead
        .iter()
        .filter(|d| !detachable(&d.mount_point))
        .map(DeadMount::describe)
        .collect();
    if !bad.is_empty() {
        return Some(format!(
            "the job's namespace has filesystems that are gone, do not answer or root cannot \
             access, whose mount points CRIU cannot be told to leave out (they contain ']' or \
             a line break): {}",
            bad.join("; ")
        ));
    }
    None
}

/// Whether CRIU's `--external mnt[<path>]:<key>` can name `path`.
fn detachable(path: &str) -> bool {
    !path.contains([']', '\n']) && path.starts_with('/')
}

/// A dead mount that a dump left to the restore (see the module docs);
/// recorded in the checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetachedMount {
    /// Mount point in the job's namespace.
    pub mount_point: String,
    pub fstype: String,
    /// CRIU's key for it in the image.
    pub key: String,
    /// Why it was detached, for messages.
    #[serde(default)]
    pub why: String,
}

impl DetachedMount {
    /// The mounts to detach for a dump of a job with `dead` mounts that
    /// [`refusal`] accepted.
    pub fn for_dump(dead: &DeadMounts) -> Vec<DetachedMount> {
        dead.dead
            .iter()
            .enumerate()
            .map(|(i, d)| DetachedMount {
                mount_point: d.mount_point.clone(),
                fstype: d.fstype.clone(),
                key: format!("gpueue-detached-{i}"),
                why: d.why.clone(),
            })
            .collect()
    }
}

/// The `criu dump` arguments that detach `mounts` and leave out the mounts
/// `below` them.
pub fn dump_args(mounts: &[DetachedMount], below: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for m in mounts {
        out.push("--external".to_owned());
        out.push(format!("mnt[{}]:{}", m.mount_point, m.key));
    }
    for b in below {
        out.push("--skip-mnt".to_owned());
        out.push(b.clone());
    }
    out
}

/// The `criu restore` arguments that bind `sources[i]` (a path in the
/// restoring CRIU's namespace) for `mounts[i]`.
pub fn restore_args(mounts: &[DetachedMount], sources: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for (m, src) in mounts.iter().zip(sources) {
        out.push("--external".to_owned());
        out.push(format!("mnt[{}]:{src}", m.key));
    }
    out
}

/// Whether `now` (the helper's namespace at restore time) has the
/// filesystem of `m` mounted at its mount point again: a lookup of the
/// mount point ends in a mount of that type there.
pub fn is_back(m: &DetachedMount, now: &MountTree) -> bool {
    now.top(&m.mount_point)
        .is_some_and(|t| t.mount_point == m.mount_point && t.fstype == m.fstype)
}

/// Create (if needed) the empty directory that a restore binds in place of
/// a detached mount whose filesystem is not back: root's, mode 0, so the
/// restored job can neither read nor write it. `dir` is in the checkpoint
/// base, which only root can change.
pub fn ensure_placeholder(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    match std::fs::create_dir(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => {
            return Err(std::io::Error::other(format!(
                "creating {}: {e}",
                dir.display()
            )));
        }
    }
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.is_dir() || meta.uid() != rustix::process::geteuid().as_raw() {
        return Err(std::io::Error::other(format!(
            "{} is not a directory of the helper's own",
            dir.display()
        )));
    }
    if meta.mode() & 0o7777 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o000))?;
    }
    Ok(())
}

/// Whether the mount point `path` of a detached mount is missing on this
/// host, so that a restore could not mount anything there (CRIU restores
/// the job's root from a non-recursive bind of `/`, so it needs the
/// directory itself, not what is mounted on it). A path that cannot be
/// looked at because a filesystem that does not answer is mounted on it
/// is there: one can only mount on an existing directory.
pub fn mount_point_missing(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(m) => !m.is_dir(),
        Err(e) => {
            e.kind() == std::io::ErrorKind::NotFound
                || e.raw_os_error() == Some(rustix::io::Errno::NOTDIR.raw_os_error())
        }
    }
}

/// [`DeadMount::why`] of a FUSE mount that refuses root.
pub const NO_ROOT_ACCESS: &str = "cannot be accessed by root (a FUSE mount without `allow_other`)";

/// Why CRIU cannot use the filesystem at `path`, if it cannot (see
/// [`DeadMount::why`]):
/// - "does not answer (...)": `statfs` (which FUSE and network filesystems
///   pass on to their daemon or server) fails with a connection error;
/// - [`NO_ROOT_ACCESS`]: looking at it fails with `EACCES`, as a FUSE
///   mount without `allow_other` does for everyone but its owner (its
///   `statfs` succeeds regardless).
///
/// Other errors mean that it answered. May block; see [`probe_paths`].
pub fn probe(path: &Path) -> Option<String> {
    use rustix::io::Errno;
    match rustix::fs::statfs(path) {
        Ok(_) => match rustix::fs::lstat(path) {
            Err(Errno::ACCESS | Errno::PERM) => Some(NO_ROOT_ACCESS.to_owned()),
            _ => None,
        },
        Err(e) => {
            let dead = [
                Errno::NOTCONN,
                Errno::CONNABORTED,
                Errno::CONNREFUSED,
                Errno::CONNRESET,
                Errno::TIMEDOUT,
                Errno::HOSTDOWN,
                Errno::HOSTUNREACH,
                Errno::NETDOWN,
                Errno::NETUNREACH,
                Errno::STALE,
                Errno::IO,
            ];
            dead.contains(&e).then(|| {
                // "Transport endpoint is not connected (os error 107)"
                let msg = std::io::Error::from(e).to_string();
                let msg = match msg.rfind(" (os error ") {
                    Some(i) => &msg[..i],
                    None => &msg,
                };
                format!("does not answer ({msg})")
            })
        }
    }
}

/// [`probe`] for each of `paths`, each in a thread of its own; a path that
/// does not answer within `timeout` counts as dead. One entry per path,
/// `None` if it answered and can be used. The threads that are still
/// stuck are left behind, so run this in a process that exits afterwards
/// (the `verify-access` child).
pub fn probe_paths(paths: &[String], timeout: Duration) -> Vec<Option<String>> {
    each_with_deadline(paths.to_vec(), timeout, |p| probe(Path::new(&p)))
        .into_iter()
        .map(|r| {
            r.unwrap_or_else(|| {
                Some(format!(
                    "does not answer (no answer within {}s)",
                    timeout.as_secs()
                ))
            })
        })
        .collect()
}

/// `f` for each of `items`, each in a thread of its own; one entry per
/// item, `None` for those that did not finish within `timeout` (all
/// together). The threads that are still stuck (on a filesystem that does
/// not answer) are left behind, so run this in a process that exits
/// afterwards (the `verify-access` child).
pub fn each_with_deadline<I, T>(
    items: Vec<I>,
    timeout: Duration,
    f: impl Fn(I) -> T + Send + Sync + 'static,
) -> Vec<Option<T>>
where
    I: Send + 'static,
    T: Send + 'static,
{
    let f = std::sync::Arc::new(f);
    let (tx, rx) = std::sync::mpsc::channel();
    let n = items.len();
    for (i, item) in items.into_iter().enumerate() {
        let tx = tx.clone();
        let f = f.clone();
        std::thread::spawn(move || {
            let _ = tx.send((i, f(item)));
        });
    }
    drop(tx);
    let mut out: Vec<Option<T>> = (0..n).map(|_| None).collect();
    let deadline = std::time::Instant::now() + timeout;
    let mut left = n;
    while left > 0 {
        let Some(wait) = deadline.checked_duration_since(std::time::Instant::now()) else {
            break;
        };
        match rx.recv_timeout(wait) {
            Ok((i, r)) => {
                out[i] = Some(r);
                left -= 1;
            }
            Err(_) => break,
        }
    }
    out
}

/// `major:minor` of a `/proc/<pid>/maps` device field (hex, `fd:01`) in
/// mountinfo's decimal form (`253:1`).
pub fn maps_dev(field: &str) -> Option<String> {
    let (maj, min) = field.split_once(':')?;
    Some(format!(
        "{}:{}",
        u64::from_str_radix(maj, 16).ok()?,
        u64::from_str_radix(min, 16).ok()?
    ))
}

/// The file mappings in `/proc/<pid>/maps`: (`major:minor` in decimal,
/// path), without duplicates. Anonymous and special mappings (`[heap]`)
/// are left out.
pub fn parse_maps(text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        // address perms offset dev inode path (the path may contain spaces)
        let mut f = line.splitn(6, ' ');
        let (Some(_), Some(_), Some(_), Some(dev), Some(ino), Some(path)) =
            (f.next(), f.next(), f.next(), f.next(), f.next(), f.next())
        else {
            continue;
        };
        let path = path.trim_start();
        if ino == "0" || !path.starts_with('/') {
            continue;
        }
        if let Some(dev) = maps_dev(dev) {
            let entry = (dev, path.to_owned());
            if !out.contains(&entry) {
                out.push(entry);
            }
        }
    }
    out
}

/// `mnt_id:` of a `/proc/<pid>/fdinfo/<fd>` file.
pub fn parse_fdinfo_mnt_id(text: &str) -> Option<u64> {
    text.lines()
        .find_map(|l| l.strip_prefix("mnt_id:"))
        .and_then(|v| v.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A job's namespace: sshfs on /nfs (with a bind below it), NFS on
    /// /home, and its own /tmp and /proc.
    const JOB: &str = "\
100 99 8:1 / / rw - ext4 /dev/sda1 rw
101 100 0:60 / /nfs rw - fuse.sshfs host:/export rw
102 101 8:1 /srv/x /nfs/x rw - ext4 /dev/sda1 rw
103 100 0:61 / /home rw - nfs4 srv:/home rw
104 100 0:62 / /tmp rw - tmpfs tmpfs rw
105 100 0:63 / /proc rw - proc proc rw
";

    /// The helper's namespace after sshfs exited and unmounted /nfs.
    const HELPER: &str = "\
1 0 8:1 / / rw - ext4 /dev/sda1 rw
3 1 0:61 / /home rw - nfs4 srv:/home rw
";

    #[test]
    fn filesystem_types() {
        for t in [
            "fuse",
            "fuse.sshfs",
            "fuseblk",
            "nfs",
            "nfs4",
            "cifs",
            "9p",
            "ceph",
        ] {
            assert!(is_remote_fstype(t), "{t}");
        }
        for t in ["ext4", "tmpfs", "overlay", "proc", "autofs", "fusectl", ""] {
            assert!(!is_remote_fstype(t), "{t}");
        }
    }

    #[test]
    fn finds_dead_mounts_and_what_lies_below() {
        let job = MountTree::parse(JOB);
        let helper = MountTree::parse(HELPER);
        let remote = remote_mounts(&job, &helper);
        assert_eq!(
            remote
                .iter()
                .map(|m| (m.mount_point.as_str(), m.on_host))
                .collect::<Vec<_>>(),
            [("/nfs", false), ("/home", true)]
        );
        // /home answers: only /nfs is dead.
        let dead = dead_mounts(&job, &remote, &[None, None]);
        assert_eq!(dead.dead.len(), 1);
        assert_eq!(dead.dead[0].mount_point, "/nfs");
        assert_eq!(dead.dead[0].why, "is no longer mounted on the host");
        assert_eq!(dead.below, ["/nfs/x"]);
        // /home does not answer either.
        let dead = dead_mounts(
            &job,
            &remote,
            &[
                None,
                Some("does not answer (Transport endpoint is not connected)".into()),
            ],
        );
        assert_eq!(
            dead.dead
                .iter()
                .map(DeadMount::describe)
                .collect::<Vec<_>>(),
            [
                "/nfs (fuse.sshfs) is no longer mounted on the host",
                "/home (nfs4) does not answer (Transport endpoint is not connected)"
            ]
        );
        // Everything alive and mounted: nothing.
        let alive = MountTree::parse(
            "1 0 8:1 / / rw - ext4 a rw\n2 1 0:60 / /nfs rw - fuse.sshfs h rw\n3 1 0:61 / /home rw - nfs4 s rw\n",
        );
        let remote = remote_mounts(&job, &alive);
        assert_eq!(
            dead_mounts(&job, &remote, &[None, None]),
            DeadMounts::default()
        );
    }

    /// The shim's masks (empty tmpfs on /run/dbus and /run/systemd, with
    /// single files and directories bound from the host's /run below the
    /// latter) are the job's own and not FUSE or network mounts: never
    /// dead, even though the helper's namespace lacks the tmpfs.
    #[test]
    fn ignores_the_shims_masks() {
        let job = MountTree::parse(
            "\
100 99 8:1 / / rw - ext4 /dev/sda1 rw
101 100 0:25 / /run rw - tmpfs tmpfs rw
102 101 0:90 / /run/dbus rw - tmpfs tmpfs rw
103 101 0:91 / /run/systemd rw - tmpfs tmpfs rw
104 103 0:25 /systemd/journal/socket /run/systemd/journal/socket rw - tmpfs tmpfs rw
105 103 0:25 /systemd/userdb /run/systemd/userdb rw - tmpfs tmpfs rw
106 100 0:60 / /nfs rw - fuse.sshfs host:/export rw
",
        );
        let helper = MountTree::parse(
            "1 0 8:1 / / rw - ext4 /dev/sda1 rw\n2 1 0:25 / /run rw - tmpfs tmpfs rw\n",
        );
        let remote = remote_mounts(&job, &helper);
        assert_eq!(
            remote
                .iter()
                .map(|m| m.mount_point.as_str())
                .collect::<Vec<_>>(),
            ["/nfs"]
        );
        let dead = dead_mounts(&job, &remote, &[None]);
        assert_eq!(dead.dead.len(), 1);
        assert!(dead.below.is_empty(), "{:?}", dead.below);
    }

    #[test]
    fn finds_what_depends_on_a_dead_mount() {
        let job = MountTree::parse(JOB);
        let helper = MountTree::parse(HELPER);
        let mut dead = dead_mounts(&job, &remote_mounts(&job, &helper), &[None, None]);
        // A job that uses /home and /tmp only.
        let unrelated = ProcessFiles {
            fds: vec![
                (1, Some(103), "/home/a/out.txt".into()),
                (2, Some(104), "/tmp/x".into()),
                (3, None, "pipe:[12]".into()),
            ],
            cwd: Some("/home/a".into()),
            root: Some("/".into()),
            exe: Some("/usr/bin/python3".into()),
            maps: vec![("8:1".into(), "/usr/lib/libc.so.6".into())],
            sockets: vec!["/tmp/s".into()],
        };
        assert!(dead_mount_users(&job, &dead, std::slice::from_ref(&unrelated)).is_empty());
        dead.used = dead_mount_users(&job, &dead, std::slice::from_ref(&unrelated));
        assert_eq!(refusal(&dead), None);
        // One that uses /nfs in every way (and a mount below it).
        let user = ProcessFiles {
            fds: vec![
                (3, Some(101), "/nfs/out.txt".into()),
                (4, Some(102), "/nfs/x/in".into()),
            ],
            cwd: Some("/nfs/run (deleted)".into()),
            root: Some("/".into()),
            exe: Some("/nfs/bin/tool".into()),
            maps: vec![
                ("0:60".into(), "/nfs/lib.so".into()),
                ("8:1".into(), "/nfs/x/data".into()),
            ],
            sockets: vec!["/nfs/ctl.sock".into()],
        };
        let used = dead_mount_users(&job, &dead, &[unrelated, user]);
        assert_eq!(
            used.iter().map(|(_, w)| w.as_str()).collect::<Vec<_>>(),
            [
                "the open file /nfs/out.txt (fd 3)",
                "the open file /nfs/x/in (fd 4)",
                "the working directory /nfs/run (deleted)",
                "the executable /nfs/bin/tool",
                "the mapped file /nfs/lib.so",
                "the mapped file /nfs/x/data",
                "a Unix socket bound to /nfs/ctl.sock",
            ]
        );
        assert!(used.iter().all(|(mp, _)| mp == "/nfs"));
        dead.used = used;
        let reason = refusal(&dead).unwrap();
        assert!(
            reason.starts_with(
                "the job depends on a filesystem that is gone, does not answer or root cannot \
                 access: /nfs (fuse.sshfs) is no longer mounted on the host; the job uses it: \
                 the open file /nfs/out.txt (fd 3), "
            ),
            "{reason}"
        );
        assert!(reason.contains(" and 2 more."), "{reason}");
    }

    #[test]
    fn criu_arguments() {
        let job = MountTree::parse(JOB);
        let helper = MountTree::parse(HELPER);
        let dead = dead_mounts(&job, &remote_mounts(&job, &helper), &[None, None]);
        let detached = DetachedMount::for_dump(&dead);
        assert_eq!(
            detached,
            [DetachedMount {
                mount_point: "/nfs".into(),
                fstype: "fuse.sshfs".into(),
                key: "gpueue-detached-0".into(),
                why: "is no longer mounted on the host".into(),
            }]
        );
        assert_eq!(
            dump_args(&detached, &dead.below),
            [
                "--external",
                "mnt[/nfs]:gpueue-detached-0",
                "--skip-mnt",
                "/nfs/x"
            ]
        );
        assert_eq!(
            restore_args(
                &detached,
                &["/var/lib/gpueue/checkpoints/detached-mount".into()]
            ),
            [
                "--external",
                "mnt[gpueue-detached-0]:/var/lib/gpueue/checkpoints/detached-mount"
            ]
        );
        // Recorded in the checkpoint; older records lack `why`.
        let json = serde_json::to_string(&detached[0]).unwrap();
        assert_eq!(
            serde_json::from_str::<DetachedMount>(&json).unwrap(),
            detached[0]
        );
        let old: DetachedMount =
            serde_json::from_str(r#"{"mount_point":"/a","fstype":"fuse","key":"k"}"#).unwrap();
        assert_eq!(old.why, "");
        // A mount point CRIU's syntax cannot carry.
        let odd = DeadMounts {
            dead: vec![DeadMount {
                id: 1,
                mount_point: "/a]b".into(),
                fstype: "fuse".into(),
                dev: "0:1".into(),
                why: "is no longer mounted on the host".into(),
            }],
            ..DeadMounts::default()
        };
        assert!(refusal(&odd).unwrap().contains("/a]b (fuse)"));
    }

    #[test]
    fn whether_a_filesystem_is_back() {
        let m = DetachedMount {
            mount_point: "/nfs".into(),
            fstype: "fuse.sshfs".into(),
            key: "k".into(),
            why: String::new(),
        };
        assert!(!is_back(&m, &MountTree::parse(HELPER)));
        let back =
            MountTree::parse("1 0 8:1 / / rw - ext4 a rw\n7 1 0:90 / /nfs rw - fuse.sshfs h rw\n");
        assert!(is_back(&m, &back));
        // Something else mounted there.
        let other =
            MountTree::parse("1 0 8:1 / / rw - ext4 a rw\n7 1 0:90 / /nfs rw - tmpfs t rw\n");
        assert!(!is_back(&m, &other));
    }

    #[test]
    fn parses_proc_files() {
        let maps = "\
55d0c0a00000-55d0c0a28000 r--p 00000000 fd:01 1234                       /usr/bin/python3.12
7f0000000000-7f0000001000 rw-p 00000000 00:3c 99                         /nfs/data file.bin
7f0000002000-7f0000003000 rw-p 00000000 00:00 0                          [heap]
7f0000004000-7f0000005000 rw-p 00000000 00:00 0
7f0000006000-7f0000007000 r--p 00000000 00:3c 99                         /nfs/data file.bin
";
        assert_eq!(
            parse_maps(maps),
            [
                ("253:1".to_owned(), "/usr/bin/python3.12".to_owned()),
                ("0:60".to_owned(), "/nfs/data file.bin".to_owned()),
            ]
        );
        assert_eq!(
            parse_fdinfo_mnt_id("pos:\t0\nflags:\t0100002\nmnt_id:\t101\nino:\t5\n"),
            Some(101)
        );
        assert_eq!(parse_fdinfo_mnt_id("pos:\t0\n"), None);
    }

    /// A path that answers, one that does not exist (an answer too), and
    /// the timeout.
    #[test]
    fn probes_paths() {
        let dir = tempfile::tempdir().unwrap();
        let paths = vec![
            dir.path().to_str().unwrap().to_owned(),
            dir.path().join("missing").to_str().unwrap().to_owned(),
        ];
        assert_eq!(probe_paths(&paths, Duration::from_secs(5)), [None, None]);
        assert_eq!(
            probe_paths(&[], Duration::from_secs(5)),
            Vec::<Option<String>>::new()
        );
    }

    /// What does not finish in time (as a lookup on a filesystem that does
    /// not answer) is left behind, without holding up the others.
    #[test]
    fn each_with_deadline_leaves_stuck_items_behind() {
        let start = std::time::Instant::now();
        let out = each_with_deadline(vec![1u64, 60, 2], Duration::from_millis(300), |s| {
            std::thread::sleep(Duration::from_millis(s * 10));
            s
        });
        assert_eq!(out, [Some(1), None, Some(2)]);
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn mount_points() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, "x").unwrap();
        assert!(!mount_point_missing(dir.path()));
        assert!(mount_point_missing(&dir.path().join("missing")));
        assert!(mount_point_missing(&file));
        assert!(mount_point_missing(&file.join("below")));
    }

    #[test]
    fn creates_the_placeholder() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(PLACEHOLDER_DIR);
        ensure_placeholder(&p).unwrap();
        ensure_placeholder(&p).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o7777, 0);
        // Not a symlink to somewhere else.
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(dir.path(), &link).unwrap();
        assert!(ensure_placeholder(&link).is_err());
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}
