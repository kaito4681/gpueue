//! Reading `/proc/<pid>/mountinfo`: which mounts are reachable by path and
//! which are hidden under another mount.
//!
//! A mount is *hidden* when its mount point lies at or below the mount
//! point of another mount that was attached later to one of its ancestors.
//! Example (Docker with `--gpus all --tmpfs /run`): the NVIDIA runtime
//! bind-mounts a file on `/run/nvidia-persistenced/socket` of the
//! container's root, then Docker mounts a tmpfs on `/run`. Path lookups end
//! in the tmpfs, so nothing can reach (or unmount by path) the bind mount,
//! but it is still part of the mount namespace and of every namespace
//! copied from it. CRIU dumps it as an external bind of
//! `/run/nvidia-persistenced/socket` and fails to restore it, because that
//! path does not exist (it is the tmpfs's) on the restoring side.
//!
//! The shim removes hidden mounts from each job's namespace (see
//! [`crate::shim`]); the helper refuses a dump that depends on one (see
//! [`unreachable_sources`]).

use std::collections::{HashMap, HashSet};

/// One line of mountinfo (`proc(5)`), with the fields puzuz uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    pub id: u64,
    pub parent: u64,
    /// `major:minor` of the filesystem.
    pub dev: String,
    /// Path of the mount's root within its filesystem.
    pub root: String,
    /// Mount point relative to the reader's root.
    pub mount_point: String,
}

/// Parse mountinfo, skipping malformed lines.
pub fn parse_mountinfo(text: &str) -> Vec<MountEntry> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            Some(MountEntry {
                id: f.next()?.parse().ok()?,
                parent: f.next()?.parse().ok()?,
                dev: f.next()?.to_owned(),
                root: unescape_octal(f.next()?),
                mount_point: unescape_octal(f.next()?),
            })
        })
        .collect()
}

/// Undo mountinfo's `\ooo` escapes (space, tab, newline, backslash).
pub fn unescape_octal(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 4 <= b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 4], 8)
        {
            out.push(v);
            i += 4;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether `path` is `prefix` or below it; a trailing `/` on `prefix` means
/// strictly below.
pub fn path_under(path: &str, prefix: &str) -> bool {
    if prefix.len() > 1 && prefix.ends_with('/') {
        return path.starts_with(prefix);
    }
    if prefix == "/" {
        return path.starts_with('/');
    }
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// The mount tree of one namespace, as seen from mountinfo.
#[derive(Debug)]
pub struct MountTree {
    entries: Vec<MountEntry>,
    by_id: HashMap<u64, usize>,
    /// (parent id, mount point) → index. A later mount on the same
    /// (parent, mount point) is stacked on the earlier one: its parent is
    /// that mount, so the key stays unique.
    children: HashMap<(u64, String), usize>,
    root: Option<usize>,
}

impl MountTree {
    pub fn new(entries: Vec<MountEntry>) -> Self {
        let by_id: HashMap<u64, usize> =
            entries.iter().enumerate().map(|(i, e)| (e.id, i)).collect();
        let mut children = HashMap::new();
        let mut root = None;
        for (i, e) in entries.iter().enumerate() {
            if by_id.contains_key(&e.parent) && e.parent != e.id {
                children.insert((e.parent, e.mount_point.clone()), i);
            } else if root.is_none() || e.mount_point == "/" {
                // The parent is outside our root: the namespace root.
                root = Some(i);
            }
        }
        Self {
            entries,
            by_id,
            children,
            root,
        }
    }

    pub fn parse(mountinfo: &str) -> Self {
        Self::new(parse_mountinfo(mountinfo))
    }

    pub fn entries(&self) -> &[MountEntry] {
        &self.entries
    }

    pub fn get(&self, id: u64) -> Option<&MountEntry> {
        self.by_id.get(&id).map(|&i| &self.entries[i])
    }

    /// The mounts a lookup of `path` passes through, from the root mount to
    /// the mount that `path` ends up in.
    pub fn resolve(&self, path: &str) -> Vec<u64> {
        let Some(root) = self.root else {
            return Vec::new();
        };
        let mut cur = self.entries[root].id;
        let mut chain = vec![cur];
        let descend = |prefix: &str, cur: &mut u64, chain: &mut Vec<u64>| {
            while let Some(&i) = self.children.get(&(*cur, prefix.to_owned())) {
                *cur = self.entries[i].id;
                chain.push(*cur);
            }
        };
        descend("/", &mut cur, &mut chain);
        let mut prefix = String::new();
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            prefix.push('/');
            prefix.push_str(comp);
            descend(&prefix, &mut cur, &mut chain);
        }
        chain
    }

    /// `id` and its ancestors.
    fn ancestors(&self, id: u64) -> HashSet<u64> {
        let mut out = HashSet::new();
        let mut cur = Some(id);
        while let Some(c) = cur {
            if !out.insert(c) {
                break;
            }
            cur = self
                .get(c)
                .map(|e| e.parent)
                .filter(|p| self.by_id.contains_key(p));
        }
        out
    }

    /// Whether a lookup of the mount point of `id` passes through it (a
    /// mount stacked on top of it still counts as reaching it).
    pub fn is_visible(&self, id: u64) -> bool {
        self.get(id)
            .is_some_and(|e| self.resolve(&e.mount_point).contains(&id))
    }

    /// Hidden mounts whose parent is visible (their own submounts are
    /// hidden too and go away with them), with the mount that hides each.
    pub fn hidden(&self) -> Vec<Hidden> {
        let mut out = Vec::new();
        for e in &self.entries {
            if Some(e.id) == self.root.map(|r| self.entries[r].id) || self.is_visible(e.id) {
                continue;
            }
            if !self.is_visible(e.parent) {
                continue;
            }
            let ancestors = self.ancestors(e.id);
            let chain = self.resolve(&e.mount_point);
            if let Some(cover) = chain.iter().find(|c| !ancestors.contains(c)) {
                let cover = self.get(*cover).expect("resolved ids exist");
                out.push(Hidden {
                    id: e.id,
                    mount_point: e.mount_point.clone(),
                    covered_by: cover.id,
                    covered_by_path: cover.mount_point.clone(),
                });
            }
        }
        out
    }
}

/// A hidden mount and the (visible) mount that hides it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hidden {
    pub id: u64,
    pub mount_point: String,
    pub covered_by: u64,
    pub covered_by_path: String,
}

/// Mounts of a job's namespace that CRIU can only dump as an external bind
/// of a path that is hidden in the helper's namespace (where criu runs).
///
/// With `--external mnt[]`, CRIU dumps a mount whose filesystem is also
/// mounted in its own namespace as "external", keyed by the path where it
/// found that filesystem, and on restore bind-mounts that path from under
/// `--root`. If every such mount of the helper's namespace is hidden, the
/// path resolves to something else (or nothing) and the restore fails with
/// `Failed to open_tree <path>`, even on the same host. The job's root
/// mount is skipped (`--root` replaces it).
pub fn unreachable_sources(job: &MountTree, helper: &MountTree) -> Vec<String> {
    let job_root = job.root.map(|r| job.entries[r].id);
    let mut out = Vec::new();
    for j in &job.entries {
        if Some(j.id) == job_root {
            continue;
        }
        let candidates: Vec<&MountEntry> = helper
            .entries
            .iter()
            .filter(|h| h.dev == j.dev && path_under(&j.root, &h.root))
            .collect();
        if !candidates.is_empty() && !candidates.iter().any(|h| helper.is_visible(h.id)) {
            out.push(j.mount_point.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The job namespace of the GPU e2e container before the fix.
    const DOCKER_GPU: &str = "\
9303 9301 0:137 / / rw - overlay overlay rw
9308 9303 0:149 / /dev rw,nosuid - tmpfs tmpfs rw
9309 9308 0:151 / /dev/pts rw - devpts devpts rw
9313 9303 0:150 / /sys rw - sysfs sysfs rw
9319 9303 0:28 /nvidia-persistenced/socket /run/nvidia-persistenced/socket rw - tmpfs tmpfs rw
9320 9319 0:29 / /run/nvidia-persistenced/socket/x rw - tmpfs tmpfs rw
9393 9303 0:153 / /run rw - tmpfs tmpfs rw
9394 9393 0:154 / /run/lock rw - tmpfs tmpfs rw
9396 9303 0:38 /hm\\040stage /e2e ro - tmpfs tmpfs rw
9304 9303 0:182 / /proc rw - proc proc rw
";

    #[test]
    fn parses_and_unescapes() {
        let e = parse_mountinfo(DOCKER_GPU);
        assert_eq!(e.len(), 10);
        assert_eq!(e[8].root, "/hm stage");
        assert_eq!(unescape_octal("a\\134b\\040"), "a\\b ");
        assert_eq!(unescape_octal("trailing\\04"), "trailing\\04");
    }

    #[test]
    fn finds_mounts_hidden_under_a_later_mount() {
        let t = MountTree::parse(DOCKER_GPU);
        assert_eq!(t.resolve("/run/nvidia-persistenced/socket"), [9303, 9393]);
        assert_eq!(t.resolve("/run/lock/x"), [9303, 9393, 9394]);
        assert!(t.is_visible(9394));
        assert!(!t.is_visible(9319));
        assert!(!t.is_visible(9320));
        // Only the topmost hidden mount; its submount goes with it.
        assert_eq!(
            t.hidden(),
            [Hidden {
                id: 9319,
                mount_point: "/run/nvidia-persistenced/socket".into(),
                covered_by: 9393,
                covered_by_path: "/run".into(),
            }]
        );
    }

    #[test]
    fn stacked_mounts_are_not_hidden() {
        // A second tmpfs mounted on /data on top of the first: the first is
        // the parent of the second, so a lookup passes through both.
        let t = MountTree::parse(
            "1 0 0:1 / / rw - ext4 a rw\n2 1 0:2 / /data rw - tmpfs t rw\n3 2 0:3 / /data rw - tmpfs t rw\n",
        );
        assert_eq!(t.resolve("/data/x"), [1, 2, 3]);
        assert!(t.hidden().is_empty());
    }

    #[test]
    fn root_with_an_outside_parent() {
        let t = MountTree::parse("25 1 0:1 / / rw - ext4 a rw\n26 25 0:2 / /tmp rw - tmpfs t rw\n");
        assert_eq!(t.resolve("/tmp/a"), [25, 26]);
        assert!(t.hidden().is_empty());
    }

    #[test]
    fn detects_sources_hidden_in_the_helper_namespace() {
        let helper = MountTree::parse(DOCKER_GPU);
        // After the shim's cleanup the job no longer has the hidden mount.
        let clean = MountTree::parse(
            "\
1 0 0:137 / / rw - overlay overlay rw
2 1 0:153 / /run rw - tmpfs tmpfs rw
3 1 0:150 / /sys rw - sysfs sysfs rw
4 1 0:900 / /tmp rw - tmpfs tmpfs rw
5 1 0:182 / /proc rw - proc proc rw
",
        );
        assert!(unreachable_sources(&clean, &helper).is_empty());
        // A job namespace copied before the cleanup still has it.
        let job = MountTree::parse(DOCKER_GPU);
        assert_eq!(
            unreachable_sources(&job, &helper),
            [
                "/run/nvidia-persistenced/socket",
                "/run/nvidia-persistenced/socket/x"
            ]
        );
        // A bind of a subdirectory of a visible mount is fine.
        let sub = MountTree::parse(
            "1 0 0:137 / / rw - overlay o rw\n2 1 0:153 /a/b /data rw - tmpfs t rw\n",
        );
        assert!(unreachable_sources(&sub, &helper).is_empty());
    }

    #[test]
    fn path_prefixes() {
        assert!(path_under("/run/a", "/run/"));
        assert!(!path_under("/run", "/run/"));
        assert!(path_under("/dev/shm", "/dev/shm"));
        assert!(!path_under("/dev/shmx", "/dev/shm"));
        assert!(path_under("/a", "/"));
        assert!(path_under("/", "/"));
    }
}
