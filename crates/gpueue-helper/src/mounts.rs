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
//!
//! Also here: automount points in a job's namespace
//! ([`untriggered_automounts`], [`triggered_automounts`]) and what a restore
//! needs from the host's mounts ([`SourceMount`]).

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

/// One line of mountinfo (`proc(5)`), with the fields gpueue uses.
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
    /// Filesystem type (`ext4`, `tmpfs`, `autofs`, `fuse.sshfs`, ...);
    /// empty if the line has none.
    pub fstype: String,
}

/// The fields of a mountinfo line. The kernel separates them with single
/// spaces and escapes space, tab, newline and backslash in paths as
/// `\ooo`, but leaves every other character as it is, so the line is split
/// on ASCII spaces only: a path may contain other (Unicode) whitespace.
fn fields(line: &str) -> impl Iterator<Item = &str> {
    line.split(' ').filter(|f| !f.is_empty())
}

/// Parse mountinfo, skipping malformed lines.
pub fn parse_mountinfo(text: &str) -> Vec<MountEntry> {
    text.lines()
        .filter_map(|l| {
            let mut f = fields(l);
            let mut e = MountEntry {
                id: f.next()?.parse().ok()?,
                parent: f.next()?.parse().ok()?,
                dev: f.next()?.to_owned(),
                root: unescape_octal(f.next()?),
                mount_point: unescape_octal(f.next()?),
                fstype: String::new(),
            };
            // Options, optional fields, then "-" and the filesystem type.
            if f.by_ref().any(|x| x == "-") {
                e.fstype = f.next().map(unescape_octal).unwrap_or_default();
            }
            Some(e)
        })
        .collect()
}

/// The (unescaped) mount point of one mountinfo line.
pub fn mount_point_field(line: &str) -> Option<String> {
    fields(line).nth(4).map(unescape_octal)
}

/// The mount points of the mountinfo `bytes` (as read from
/// `/proc/<pid>/mountinfo`), in order; `Err` if CRIU could not parse a
/// line the way the kernel wrote it: a mount's root or mount point is not
/// UTF-8 or contains a control character.
///
/// CRIU (4.2) reads mountinfo with `sscanf("%ms")`, which stops at any
/// ASCII whitespace (also `\r`, `\v` and `\f`, which the kernel does not
/// escape), and unescapes only `\040`, `\011` and `\134`, not the `\012`
/// of a newline. Such a path would make it dump (and restore) a mount at a
/// different path than the job's. Tabs are refused too, like every other
/// control character, as they are unlikely in a real mount point and
/// other tools (CRIU's `--external mnt[...]` option, logs) treat them
/// differently.
pub fn checked_mount_points(bytes: &[u8]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for line in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        let Ok(line) = std::str::from_utf8(line) else {
            let line = String::from_utf8_lossy(line);
            let path = fields(&line).nth(4).unwrap_or_default().to_owned();
            return Err(format!(
                "the job has a mount whose root or mount point is not UTF-8 ({path:?}), which \
                 CRIU cannot be relied on to handle; unmount it or end the job"
            ));
        };
        let mut f = fields(line).skip(3);
        let (Some(root), Some(mount_point)) = (f.next(), f.next()) else {
            continue;
        };
        let (root, mount_point) = (unescape_octal(root), unescape_octal(mount_point));
        if root
            .chars()
            .chain(mount_point.chars())
            .any(char::is_control)
        {
            return Err(format!(
                "the job has a mount whose root or mount point contains control characters \
                 ({mount_point:?}, root {root:?}), which CRIU cannot parse in mountinfo; \
                 unmount it or end the job"
            ));
        }
        out.push(mount_point);
    }
    Ok(out)
}

/// Undo mountinfo's `\ooo` escapes (space, tab, newline, backslash).
pub fn unescape_octal(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        // Exactly three octal digits (bytes, so that a backslash before
        // a multi-byte character cannot split it).
        if b[i] == b'\\'
            && let Some(d) = b.get(i + 1..i + 4)
            && d.iter().all(|c| (b'0'..=b'7').contains(c))
            && let Ok(v) = u8::try_from(d.iter().fold(0u32, |v, c| v * 8 + u32::from(c - b'0')))
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

    /// Id of the namespace's root mount.
    pub fn root_id(&self) -> Option<u64> {
        self.root.map(|r| self.entries[r].id)
    }

    /// Whether `ancestor` is `id` or one of its ancestors.
    pub fn is_at_or_below(&self, id: u64, ancestor: u64) -> bool {
        self.ancestors(id).contains(&ancestor)
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

/// For each mount of `job` (but its root) whose filesystem `helper` (the
/// helper's namespace, where criu runs) also has, the host paths that CRIU
/// may pick as its external bind source: CRIU takes a mount of its own
/// namespace with the same device whose root contains the job mount's root,
/// and records that mount's mount point plus the rest of the job mount's
/// root (the last such mount, but its superblock options also count, so
/// every candidate is listed). On restore it binds that path by its name,
/// as root, following symlinks on the way (see
/// [`crate::mountfd::bind_source_problem`]).
pub fn bind_source_candidates(job: &MountTree, helper: &MountTree) -> Vec<(u64, Vec<String>)> {
    let job_root = job.root_id();
    let mut out = Vec::new();
    for j in &job.entries {
        if Some(j.id) == job_root {
            continue;
        }
        let mut paths: Vec<String> = Vec::new();
        for h in helper
            .entries
            .iter()
            .filter(|h| h.dev == j.dev && path_under(&j.root, &h.root))
        {
            let rest = j.root[h.root.len()..].trim_start_matches('/');
            let path = match (h.mount_point.as_str(), rest) {
                (mp, "") => mp.to_owned(),
                ("/", rest) => format!("/{rest}"),
                (mp, rest) => format!("{mp}/{rest}"),
            };
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        if !paths.is_empty() {
            out.push((j.id, paths));
        }
    }
    out
}

/// Where a host path that CRIU bind-mounts on restore (an external mount
/// source) lived at dump time: the mount of the helper's namespace that a
/// lookup of the path ended in. A restore needs the same filesystem type
/// mounted at the same place again; the device and the mount id may
/// differ (a reboot mounts it anew).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceMount {
    /// The external source path (as in CRIU's dump log).
    pub path: String,
    /// Mount point of the mount `path` was on.
    pub mount_point: String,
    pub fstype: String,
}

impl MountTree {
    /// The mount a lookup of `path` ends in.
    pub fn top(&self, path: &str) -> Option<&MountEntry> {
        self.resolve(path).last().and_then(|id| self.get(*id))
    }
}

/// [`SourceMount`]s of `paths` in `helper` (the helper's own namespace),
/// without duplicates.
pub fn describe_sources(paths: &[String], helper: &MountTree) -> Vec<SourceMount> {
    let mut out: Vec<SourceMount> = Vec::new();
    for p in paths {
        if out.iter().any(|s| &s.path == p) {
            continue;
        }
        if let Some(m) = helper.top(p) {
            out.push(SourceMount {
                path: p.clone(),
                mount_point: m.mount_point.clone(),
                fstype: m.fstype.clone(),
            });
        }
    }
    out
}

/// Why `src` cannot be restored from `now` (the helper's namespace at
/// restore time), if it cannot: the lookup of its path must pass through
/// a mount of the recorded type at the recorded mount point. Any mount in
/// the stack at that point counts, so an automount point recorded before
/// it was triggered still matches once the filesystem is mounted on top.
pub fn source_problem(src: &SourceMount, now: &MountTree) -> Option<String> {
    let chain = now.resolve(&src.path);
    let found = chain.iter().filter_map(|id| now.get(*id)).any(|m| {
        m.mount_point == src.mount_point && (m.fstype == src.fstype || src.fstype.is_empty())
    });
    if found {
        return None;
    }
    let what = |m: &MountEntry| format!("{} on {}", nonempty(&m.fstype), m.mount_point);
    let current = now
        .top(&src.path)
        .map(what)
        .unwrap_or_else(|| "nothing".into());
    Some(format!(
        "{} needs {} on {} (found {current})",
        src.path,
        nonempty(&src.fstype),
        src.mount_point
    ))
}

/// Whether a lookup of `path` ends on an automount point (an `autofs`
/// mount nothing is mounted on yet).
pub fn ends_on_automount(path: &str, now: &MountTree) -> bool {
    now.top(path).is_some_and(|m| m.fstype == "autofs")
}

/// Automount points (`autofs` mounts) of `tree` that nothing is mounted on
/// yet: the shim looks them up once, so that the automounter mounts their
/// filesystems before the job's namespace is made private.
pub fn untriggered_automounts(tree: &MountTree) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for e in tree.entries() {
        if e.fstype == "autofs"
            && tree.is_visible(e.id)
            && tree.top(&e.mount_point).is_some_and(|t| t.id == e.id)
            && !out.contains(&e.mount_point)
        {
            out.push(e.mount_point.clone());
        }
    }
    out
}

/// Mount points where a filesystem is mounted on top of an automount
/// point (a triggered `x-systemd.automount`): the shim replaces each stack
/// with a copy of its top mount. CRIU would restore both layers as bind
/// mounts of the same host path, and the job only needs the filesystem.
pub fn triggered_automounts(tree: &MountTree) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for e in tree.entries() {
        if e.fstype != "autofs" || !tree.is_visible(e.id) || out.contains(&e.mount_point) {
            continue;
        }
        if tree
            .top(&e.mount_point)
            .is_some_and(|t| t.id != e.id && t.mount_point == e.mount_point && t.fstype != "autofs")
        {
            out.push(e.mount_point.clone());
        }
    }
    out
}

fn nonempty(fstype: &str) -> &str {
    if fstype.is_empty() {
        "a filesystem"
    } else {
        fstype
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CRIU's possible bind sources: the host's mount point plus the rest
    /// of the job mount's root.
    #[test]
    fn bind_source_candidates_follow_criu() {
        let job = MountTree::parse(
            "\
1 0 8:1 / / rw - ext4 /dev/sda1 rw
2 1 8:1 /home/alice/x /data rw - ext4 /dev/sda1 rw
3 1 0:50 / /home/alice/fs rw - fuse.sshfs alice@h: rw
4 1 0:51 /sub /mnt/sub rw - nfs h:/e rw
5 1 0:52 / /tmp rw - tmpfs tmpfs rw
",
        );
        let helper = MountTree::parse(
            "\
10 0 8:1 / / rw - ext4 /dev/sda1 rw
13 10 0:50 / /home/alice/fs rw - fuse.sshfs alice@h: rw
14 10 0:51 / /net rw - nfs h:/e rw
15 10 0:51 /sub /srv rw - nfs h:/e rw
",
        );
        assert_eq!(
            bind_source_candidates(&job, &helper),
            [
                (2, vec!["/home/alice/x".to_owned()]),
                (3, vec!["/home/alice/fs".to_owned()]),
                (4, vec!["/net/sub".to_owned(), "/srv".to_owned()]),
            ]
        );
    }

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

    /// Fields are separated by ASCII spaces only: other whitespace is part
    /// of a path (the kernel escapes only space, tab, newline, backslash).
    #[test]
    fn splits_fields_on_spaces_only() {
        let mi = "1 0 8:1 / / rw - ext4 a rw\n\
                  2 1 0:5 /x\u{2003}y /a\u{a0}b\u{85}c rw shared:1 - tmpfs t rw\n\
                  3 1 0:6 / /v\x0bw rw - tmpfs t rw\n";
        let e = parse_mountinfo(mi);
        assert_eq!(e[1].root, "/x\u{2003}y");
        assert_eq!(e[1].mount_point, "/a\u{a0}b\u{85}c");
        assert_eq!(e[1].fstype, "tmpfs");
        assert_eq!(e[2].mount_point, "/v\x0bw");
        assert_eq!(e[2].fstype, "tmpfs");
        assert_eq!(
            mount_point_field("2 1 0:5 / /a\u{2003}b rw - tmpfs t rw").as_deref(),
            Some("/a\u{2003}b")
        );
        assert_eq!(
            mount_point_field("2 1 0:5 / /a\\040b rw").as_deref(),
            Some("/a b")
        );
    }

    /// Paths that CRIU would split or not unescape are refused.
    #[test]
    fn checked_mount_points_refuse_what_criu_misreads() {
        let tab = b"1 0 8:1 / / rw - ext4 a rw\n2 1 0:5 / /a\\040b\\011c rw - tmpfs t rw\n";
        assert!(checked_mount_points(tab).is_err(), "a tab is refused too");
        let ok = "1 0 8:1 / / rw - ext4 a rw\n2 1 0:5 / /a\\040b\u{2003}\\134 rw - tmpfs t rw\n";
        assert_eq!(
            checked_mount_points(ok.as_bytes()).unwrap(),
            ["/", "/a b\u{2003}\\"]
        );
        for bad in [
            "2 1 0:5 / /a\\012b rw - tmpfs t rw\n",
            "2 1 0:5 / /a\rb rw - tmpfs t rw\n",
            "2 1 0:5 / /a\x0bb rw - tmpfs t rw\n",
            "2 1 0:5 / /a\x0cb rw - tmpfs t rw\n",
            "2 1 0:5 /r\x0b / rw - tmpfs t rw\n",
            "2 1 0:5 / /a\u{85}b rw - tmpfs t rw\n",
        ] {
            let e = checked_mount_points(bad.as_bytes()).unwrap_err();
            assert!(e.contains("control characters"), "{bad:?}: {e}");
        }
        let e = checked_mount_points(b"1 0 8:1 / / rw - ext4 a rw\n2 1 0:5 / /a\xffb rw\n")
            .unwrap_err();
        assert!(e.contains("not UTF-8"), "{e}");
        // A backslash before a multi-byte character, and not three octal
        // digits: left as it is.
        assert_eq!(unescape_octal("/a\\\u{e9}xy"), "/a\\\u{e9}xy");
        assert_eq!(unescape_octal("/a\\+12"), "/a\\+12");
        assert_eq!(unescape_octal("/a\\400"), "/a\\400");
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

    /// The helper's namespace with an untriggered automount on /data, a
    /// triggered one on /nfs, and other filesystems.
    const AUTOFS: &str = "\
1 0 8:1 / / rw shared:1 - ext4 /dev/sda1 rw
2 1 0:50 / /data rw shared:2 - autofs systemd-1 rw,fd=50
3 1 0:51 / /nfs rw shared:3 - autofs systemd-1 rw,fd=51
4 3 0:60 / /nfs rw shared:4 - fuse.sshfs host:/export rw
5 1 7:3 / /srv/ext4 rw shared:5 - ext4 /dev/loop3 rw
6 1 7:3 /home/alice /home/alice rw shared:6 - ext4 /dev/loop3 rw
";

    #[test]
    fn parses_the_filesystem_type() {
        let t = MountTree::parse(AUTOFS);
        assert_eq!(t.get(2).unwrap().fstype, "autofs");
        assert_eq!(t.get(4).unwrap().fstype, "fuse.sshfs");
        assert_eq!(parse_mountinfo("1 0 8:1 / / rw\n")[0].fstype, "");
    }

    #[test]
    fn finds_automount_points() {
        let t = MountTree::parse(AUTOFS);
        assert_eq!(untriggered_automounts(&t), ["/data"]);
        assert_eq!(triggered_automounts(&t), ["/nfs"]);
        assert!(ends_on_automount("/data/x", &t));
        assert!(!ends_on_automount("/nfs/x", &t));
    }

    #[test]
    fn describes_external_sources() {
        let t = MountTree::parse(AUTOFS);
        let s = describe_sources(
            &["/home/alice".into(), "/nfs".into(), "/home/alice".into()],
            &t,
        );
        assert_eq!(
            s,
            [
                SourceMount {
                    path: "/home/alice".into(),
                    mount_point: "/home/alice".into(),
                    fstype: "ext4".into(),
                },
                SourceMount {
                    path: "/nfs".into(),
                    mount_point: "/nfs".into(),
                    fstype: "fuse.sshfs".into(),
                },
            ]
        );
        // The same filesystems mounted anew (other ids and devices).
        let again = MountTree::parse(
            "10 0 8:1 / / rw - ext4 /dev/sda1 rw\n\
             11 10 0:70 / /nfs rw - fuse.sshfs host:/export rw\n\
             12 10 7:9 /home/alice /home/alice rw - ext4 /dev/loop9 rw\n",
        );
        assert!(s.iter().all(|s| source_problem(s, &again).is_none()));
        // /nfs not mounted again: only the root file system's directory.
        let gone = MountTree::parse("10 0 8:1 / / rw - ext4 /dev/sda1 rw\n");
        assert_eq!(
            source_problem(&s[1], &gone).unwrap(),
            "/nfs needs fuse.sshfs on /nfs (found ext4 on /)"
        );
        // An automount point recorded untriggered matches once triggered.
        let auto = SourceMount {
            path: "/data".into(),
            mount_point: "/data".into(),
            fstype: "autofs".into(),
        };
        assert!(source_problem(&auto, &t).is_none());
    }
}
