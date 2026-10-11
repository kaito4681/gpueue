//! Small `/proc` and cgroup v2 readers.
//!
//! `cuda-checkpoint` hangs forever (even with `--timeout`) when the target is
//! stopped (SIGSTOP) or frozen (cgroup freezer), because the request is
//! served by a thread inside the target. These helpers let the executor
//! refuse such calls. Parsing is kept pure for testing.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The state letter from `/proc/<pid>/stat` (field 3).
pub fn parse_stat_state(stat: &str) -> Option<char> {
    // comm (field 2) may contain spaces and ')' so split at the last ')'.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().next()?.chars().next()
}

/// The parent PID from `/proc/<pid>/stat` (field 4).
pub fn parse_stat_ppid(stat: &str) -> Option<u32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// `T` (stopped) or `t` (tracing stop).
pub fn is_stopped_state(state: char) -> bool {
    matches!(state, 'T' | 't')
}

/// The cgroup v2 path (`0::<path>`) from `/proc/<pid>/cgroup`.
pub fn parse_cgroup_v2_path(cgroup: &str) -> Option<&str> {
    cgroup.lines().find_map(|l| l.strip_prefix("0::"))
}

/// Whether a cgroup v2 `cgroup.events` file reports `frozen 1`.
pub fn parse_cgroup_frozen(events: &str) -> bool {
    events.lines().any(|l| {
        let mut it = l.split_whitespace();
        it.next() == Some("frozen") && it.next() == Some("1")
    })
}

/// The `NSpid:` line of `/proc/<pid>/status`: host PID first, innermost last.
pub fn parse_nspid(status: &str) -> Option<Vec<u32>> {
    let line = status.lines().find_map(|l| l.strip_prefix("NSpid:"))?;
    let pids = line
        .split_whitespace()
        .map(str::parse)
        .collect::<Result<Vec<u32>, _>>()
        .ok()?;
    (!pids.is_empty()).then_some(pids)
}

/// Why a process must not be handed to `cuda-checkpoint` right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unrunnable {
    Stopped(char),
    CgroupFrozen(PathBuf),
}

/// Check `/proc/<pid>` (under `proc_root`) and its cgroup (under
/// `cgroup_root`). Returns `Ok(None)` when the process can be signalled.
pub fn check_runnable(
    proc_root: &Path,
    cgroup_root: &Path,
    pid: u32,
) -> io::Result<Option<Unrunnable>> {
    let dir = proc_root.join(pid.to_string());
    let stat = fs::read_to_string(dir.join("stat"))?;
    let state = parse_stat_state(&stat)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unparsable stat"))?;
    if is_stopped_state(state) {
        return Ok(Some(Unrunnable::Stopped(state)));
    }
    let cgroup = fs::read_to_string(dir.join("cgroup"))?;
    if let Some(cg) = parse_cgroup_v2_path(&cgroup) {
        let cg_dir = cgroup_root.join(cg.trim_start_matches('/'));
        match fs::read_to_string(cg_dir.join("cgroup.events")) {
            Ok(events) if parse_cgroup_frozen(&events) => {
                return Ok(Some(Unrunnable::CgroupFrozen(cg_dir)));
            }
            Ok(_) => {}
            // No cgroup v2 mounted there: nothing to check.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

/// Numeric entries of `proc_root` with their parent PIDs. Processes that
/// exit while scanning are skipped.
fn parents(proc_root: &Path) -> io::Result<Vec<(u32, u32)>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(proc_root)? {
        let Ok(entry) = entry else { continue };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if let Some(ppid) = fs::read_to_string(entry.path().join("stat"))
            .ok()
            .as_deref()
            .and_then(parse_stat_ppid)
        {
            out.push((pid, ppid));
        }
    }
    Ok(out)
}

/// All descendants of `pid` (children first, then their children, ...),
/// by scanning `proc_root`. A snapshot: processes may fork or exit
/// meanwhile.
pub fn descendants(proc_root: &Path, pid: u32) -> io::Result<Vec<u32>> {
    let all = parents(proc_root)?;
    let mut out: Vec<u32> = Vec::new();
    let mut i = 0;
    let mut frontier = vec![pid];
    while i < frontier.len() {
        let parent = frontier[i];
        i += 1;
        for &(p, pp) in &all {
            if pp == parent && p != pid && !out.contains(&p) {
                out.push(p);
                frontier.push(p);
            }
        }
    }
    Ok(out)
}

/// What [`Step::Thaw`](crate::plan::Step::Thaw) has to do for a tree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThawPlan {
    /// Tasks in the stopped state (`T`), to be sent SIGCONT.
    pub stopped: Vec<u32>,
    /// Frozen cgroup v2 directories of the tree, to be thawed by writing
    /// `0` to their `cgroup.freeze`.
    pub frozen: Vec<PathBuf>,
}

impl ThawPlan {
    pub fn is_empty(&self) -> bool {
        self.stopped.is_empty() && self.frozen.is_empty()
    }
}

/// Find the stopped tasks and frozen cgroups of the tree under `root`.
/// Fails if `root` itself is gone (e.g. CRIU had already killed the
/// dumped tree when it was interrupted).
pub fn plan_thaw(proc_root: &Path, cgroup_root: &Path, root: u32) -> io::Result<ThawPlan> {
    // Fails with NotFound when the root is gone.
    fs::read_to_string(proc_root.join(root.to_string()).join("stat"))?;
    let mut plan = ThawPlan::default();
    let mut pids = vec![root];
    pids.extend(descendants(proc_root, root)?);
    for pid in pids {
        match check_runnable(proc_root, cgroup_root, pid) {
            // A tracing stop (`t`) has a live tracer, which is not ours
            // to interfere with.
            Ok(Some(Unrunnable::Stopped('T'))) => plan.stopped.push(pid),
            Ok(Some(Unrunnable::CgroupFrozen(dir))) => {
                if !plan.frozen.contains(&dir) {
                    plan.frozen.push(dir);
                }
            }
            Ok(_) => {}
            // Exited meanwhile.
            Err(e) if pid != root && e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(plan)
}

/// Map namespace-inner PIDs to host PIDs. `candidates` holds
/// `NSpid` vectors (host first, innermost last) of processes already known to
/// be in the job's PID namespace (a single-entry vector means the process is
/// in the host namespace, where inner and host PIDs coincide). Returns the host PIDs in `wanted` order, or
/// the inner PIDs that were not found.
pub fn match_ns_pids(candidates: &[Vec<u32>], wanted: &[u32]) -> Result<Vec<u32>, Vec<u32>> {
    let mut found = Vec::with_capacity(wanted.len());
    let mut missing = Vec::new();
    for &inner in wanted {
        match candidates.iter().find(|ns| ns.last() == Some(&inner)) {
            Some(ns) => found.push(ns[0]),
            None => missing.push(inner),
        }
    }
    if missing.is_empty() {
        Ok(found)
    } else {
        Err(missing)
    }
}

/// Resolve inner PIDs of the PID namespace that `ns_member_host_pid` belongs
/// to into host PIDs, by scanning `proc_root`.
pub fn resolve_ns_pids(
    proc_root: &Path,
    ns_member_host_pid: u32,
    wanted: &[u32],
) -> io::Result<Result<Vec<u32>, Vec<u32>>> {
    let ns_link = |pid: &str| fs::read_link(proc_root.join(pid).join("ns/pid"));
    let target_ns = ns_link(&ns_member_host_pid.to_string())?;
    let mut candidates = Vec::new();
    for entry in fs::read_dir(proc_root)? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(name) = name
            .to_str()
            .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))
        else {
            continue;
        };
        // Processes may exit while scanning; skip any read error.
        if ns_link(name).ok().as_ref() != Some(&target_ns) {
            continue;
        }
        if let Some(ns) = fs::read_to_string(entry.path().join("status"))
            .ok()
            .as_deref()
            .and_then(parse_nspid)
        {
            candidates.push(ns);
        }
    }
    Ok(match_ns_pids(&candidates, wanted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TempDir;

    #[test]
    fn stat_state() {
        assert_eq!(parse_stat_state("123 (python3) S 1 123 123 0"), Some('S'));
        assert_eq!(parse_stat_state("123 (a) b) T 1"), Some('T'));
        assert_eq!(parse_stat_state("123 (x y) t 1"), Some('t'));
        assert_eq!(parse_stat_state("garbage"), None);
        assert_eq!(parse_stat_state("1 (x)"), None);
        assert!(is_stopped_state('T') && is_stopped_state('t'));
        assert!(!is_stopped_state('S') && !is_stopped_state('R') && !is_stopped_state('D'));
    }

    #[test]
    fn cgroup_parsing() {
        assert_eq!(
            parse_cgroup_v2_path("0::/system.slice/puzuz-job-3.scope\n"),
            Some("/system.slice/puzuz-job-3.scope")
        );
        assert_eq!(
            parse_cgroup_v2_path("12:pids:/x\n0::/user.slice\n"),
            Some("/user.slice")
        );
        assert_eq!(parse_cgroup_v2_path("1:name=systemd:/x\n"), None);
        assert!(parse_cgroup_frozen("populated 1\nfrozen 1\n"));
        assert!(!parse_cgroup_frozen("populated 1\nfrozen 0\n"));
        assert!(!parse_cgroup_frozen(""));
    }

    #[test]
    fn nspid() {
        assert_eq!(
            parse_nspid("Name:\tpython3\nNSpid:\t57\t2\nPPid:\t56\n"),
            Some(vec![57, 2])
        );
        assert_eq!(parse_nspid("NSpid:\t57\n"), Some(vec![57]));
        assert_eq!(parse_nspid("Name:\tx\n"), None);
        assert_eq!(parse_nspid("NSpid:\n"), None);
        assert_eq!(parse_nspid("NSpid:\tx\n"), None);
    }

    #[test]
    fn match_pids() {
        let c = vec![vec![56, 1], vec![57, 2], vec![900, 3], vec![1000]];
        assert_eq!(match_ns_pids(&c, &[2, 1]), Ok(vec![57, 56]));
        assert_eq!(match_ns_pids(&c, &[]), Ok(vec![]));
        assert_eq!(match_ns_pids(&c, &[2, 9, 1000]), Err(vec![9]));
        assert_eq!(match_ns_pids(&c, &[1000]), Ok(vec![1000]));
    }

    fn fake_proc(tmp: &TempDir, pid: u32, state: char, cgroup: &str) {
        let d = tmp.path().join("proc").join(pid.to_string());
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("stat"), format!("{pid} (job) {state} 1 1 1")).unwrap();
        fs::write(d.join("cgroup"), format!("0::{cgroup}\n")).unwrap();
    }

    #[test]
    fn runnable_checks_on_fake_tree() {
        let tmp = TempDir::new();
        let proc_root = tmp.path().join("proc");
        let cg_root = tmp.path().join("cg");
        fake_proc(&tmp, 10, 'S', "/jobs/a");
        fake_proc(&tmp, 11, 'T', "/jobs/a");
        fake_proc(&tmp, 12, 'R', "/jobs/b");
        fake_proc(&tmp, 13, 'R', "/jobs/none");
        fs::create_dir_all(cg_root.join("jobs/a")).unwrap();
        fs::create_dir_all(cg_root.join("jobs/b")).unwrap();
        fs::write(
            cg_root.join("jobs/a/cgroup.events"),
            "populated 1\nfrozen 0\n",
        )
        .unwrap();
        fs::write(
            cg_root.join("jobs/b/cgroup.events"),
            "populated 1\nfrozen 1\n",
        )
        .unwrap();

        assert_eq!(check_runnable(&proc_root, &cg_root, 10).unwrap(), None);
        assert_eq!(
            check_runnable(&proc_root, &cg_root, 11).unwrap(),
            Some(Unrunnable::Stopped('T'))
        );
        assert_eq!(
            check_runnable(&proc_root, &cg_root, 12).unwrap(),
            Some(Unrunnable::CgroupFrozen(cg_root.join("jobs/b")))
        );
        assert_eq!(check_runnable(&proc_root, &cg_root, 13).unwrap(), None);
        assert!(check_runnable(&proc_root, &cg_root, 99).is_err());
    }

    #[test]
    fn stat_ppid() {
        assert_eq!(parse_stat_ppid("123 (python3) S 45 123 123 0"), Some(45));
        assert_eq!(parse_stat_ppid("123 (a) b) T 7 1"), Some(7));
        assert_eq!(parse_stat_ppid("123 (x) S"), None);
        assert_eq!(parse_stat_ppid("garbage"), None);
    }

    fn fake_task(tmp: &TempDir, pid: u32, ppid: u32, state: char, cgroup: &str) {
        let d = tmp.path().join("proc").join(pid.to_string());
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("stat"), format!("{pid} (job x) {state} {ppid} 1 1")).unwrap();
        fs::write(d.join("cgroup"), format!("0::{cgroup}\n")).unwrap();
    }

    #[test]
    fn descendants_and_thaw_plan_on_fake_tree() {
        let tmp = TempDir::new();
        let proc_root = tmp.path().join("proc");
        let cg_root = tmp.path().join("cg");
        // 10 -> 11 -> 13, 10 -> 12; 20 is unrelated (stopped, frozen).
        fake_task(&tmp, 10, 1, 'S', "/jobs/a");
        fake_task(&tmp, 11, 10, 'T', "/jobs/a");
        fake_task(&tmp, 12, 10, 't', "/jobs/a");
        fake_task(&tmp, 13, 11, 'S', "/jobs/a/sub");
        fake_task(&tmp, 20, 1, 'T', "/jobs/b");
        fs::create_dir_all(proc_root.join("self")).unwrap();
        for (dir, frozen) in [("jobs/a", 0), ("jobs/a/sub", 1), ("jobs/b", 1)] {
            fs::create_dir_all(cg_root.join(dir)).unwrap();
            fs::write(
                cg_root.join(dir).join("cgroup.events"),
                format!("populated 1\nfrozen {frozen}\n"),
            )
            .unwrap();
        }
        let mut d = descendants(&proc_root, 10).unwrap();
        d.sort();
        assert_eq!(d, [11, 12, 13]);
        assert!(descendants(&proc_root, 13).unwrap().is_empty());

        let plan = plan_thaw(&proc_root, &cg_root, 10).unwrap();
        assert_eq!(plan.stopped, [11]);
        assert_eq!(plan.frozen, [cg_root.join("jobs/a/sub")]);
        assert!(!plan.is_empty());
        assert!(plan_thaw(&proc_root, &cg_root, 12).unwrap().is_empty());
        let gone = plan_thaw(&proc_root, &cg_root, 99).unwrap_err();
        assert_eq!(gone.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn descendants_on_real_proc() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 30 & wait"])
            .spawn()
            .unwrap();
        let pid = child.id();
        let mut found = Vec::new();
        for _ in 0..200 {
            found = descendants(Path::new("/proc"), pid).unwrap();
            if !found.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = child.kill();
        let _ = child.wait();
        for p in &found {
            let _ = std::process::Command::new("kill")
                .args(["-9", &p.to_string()])
                .status();
        }
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(descendants(Path::new("/proc"), std::process::id()).is_ok());
    }

    #[test]
    fn runnable_self_on_real_proc() {
        let r = check_runnable(
            Path::new("/proc"),
            Path::new("/sys/fs/cgroup"),
            std::process::id(),
        )
        .unwrap();
        assert_eq!(r, None);
    }

    #[test]
    fn resolve_self_in_own_namespace() {
        let me = std::process::id();
        // In our own namespace NSpid has a single entry unless we run inside
        // a nested PID namespace (e.g. a container); either way the lookup
        // must not fail with an I/O error.
        let r = resolve_ns_pids(Path::new("/proc"), me, &[]).unwrap();
        assert_eq!(r, Ok(vec![]));
    }
}
