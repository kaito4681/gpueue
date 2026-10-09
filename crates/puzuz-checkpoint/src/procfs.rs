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
