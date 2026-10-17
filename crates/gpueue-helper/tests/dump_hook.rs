//! The post-dump hook (`gpueue-helper dump-hook`, see
//! `gpueue_helper::access::run_dump_hook`) scans the job again while CRIU
//! has it stopped, and fails (CRIU then abandons the dump and resumes the
//! job) when the job holds something the helper refuses to dump: e.g. a
//! deleted file opened after the helper's checks before the dump that CRIU
//! could only link back into its directory as root.

use std::fs;
use std::path::Path;
use std::process::Command;

use gpueue_helper::access::{DumpHookRequest, hook_refusal_path, hook_result_path};

/// The `gpueue-helper` binary: `GPUEUE_HELPER_EXE` if set (the test
/// executables run elsewhere than where cargo built them, e.g. in the
/// NixOS VM of packaging/nix/tests/helper-tests.nix), else the one cargo
/// built for the tests.
fn helper_exe() -> std::path::PathBuf {
    let exe = std::env::var_os("GPUEUE_HELPER_EXE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_gpueue-helper").into());
    assert!(
        exe.is_file(),
        "{} is missing; set GPUEUE_HELPER_EXE to the gpueue-helper binary",
        exe.display()
    );
    exe
}

/// Runs the hook at `action` for the job whose processes are `pids` (the
/// first one its init).
///
/// The hook scans every process in the PID namespace of the init (a job
/// has one of its own). Giving a test's job one needs unprivileged user
/// namespaces, which are often not available (GitHub's Ubuntu runners and
/// Docker restrict them), and the host's namespace has processes with
/// hazards of their own (and may have so many that a scan of all of them
/// takes long), so the request names the processes to scan instead.
/// `post_dump_hook_scans_the_whole_pid_namespace_of_the_job` covers the
/// scan of the namespace, where user namespaces are available.
fn run_hook(dir: &Path, action: &str, pids: &[u32]) -> (bool, std::path::PathBuf) {
    run_hook_for(dir, action, pids[0], Some(pids.to_vec()))
}

/// Runs the hook at `action` for the job whose init is `init_pid`, scanning
/// `pids` (or, if `None`, the init's PID namespace, as for a real job).
fn run_hook_for(
    dir: &Path,
    action: &str,
    init_pid: u32,
    pids: Option<Vec<u32>>,
) -> (bool, std::path::PathBuf) {
    let request = dir.join("dump-hook.json");
    let req = DumpHookRequest {
        init_pid,
        // Nothing private: the tempdir stands for a host filesystem.
        private: Vec::new(),
        pids,
    };
    fs::write(&request, serde_json::to_vec(&req).unwrap()).unwrap();
    let status = Command::new(helper_exe())
        .arg("dump-hook")
        .arg(&request)
        .env("CRTOOLS_SCRIPT_ACTION", action)
        .status()
        .unwrap();
    (status.success(), request)
}

#[test]
fn post_dump_hook_vetoes_a_deleted_host_file_with_other_links() {
    use gpueue_helper::testing::{FixtureChild, fixture_dir, fixture_ready};
    if let Some(dir) = fixture_dir() {
        if dir.ends_with("agent") {
            // Another process of the job, with a hazard of its own.
            let _l = std::os::unix::net::UnixListener::bind(dir.join("agent.sock")).unwrap();
            fixture_ready("");
            return;
        }
        // The job: it holds a file deleted while another link remains,
        // which CRIU could only link-remap instead of saving as a ghost.
        let f = dir.join("f");
        let _held = fs::File::open(&f).unwrap();
        fs::remove_file(&f).unwrap();
        fixture_ready("");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let orig = dir.path().join("f");
    fs::write(&orig, b"x").unwrap();
    fs::hard_link(&orig, dir.path().join("keep")).unwrap();
    let name = "post_dump_hook_vetoes_a_deleted_host_file_with_other_links";
    let job = FixtureChild::spawn(name, dir.path());
    let agent_dir = dir.path().join("agent");
    fs::create_dir(&agent_dir).unwrap();
    // The second hazard must not hide the first one's.
    let agent = FixtureChild::spawn(name, &agent_dir);

    let (ok, request) = run_hook(dir.path(), "post-dump", &[job.pid(), agent.pid()]);
    drop((job, agent));
    assert!(
        !ok,
        "the post-dump hook must fail the dump of a job holding a deleted host file \
         that CRIU could only link back"
    );
    let why = fs::read_to_string(hook_refusal_path(&request))
        .expect("the hook records why it refused the dump");
    assert!(
        why.contains("other hard links") && why.contains(orig.to_str().unwrap()),
        "{why}"
    );
    assert!(
        why.contains("agent.sock"),
        "every reason is reported: {why}"
    );
    assert!(!hook_result_path(&request).exists());
}

/// The hook as for a real job: the request names no processes, and the
/// hook scans every process in the PID namespace of the job's init. The
/// job runs in a PID namespace of its own (`unshare --user --pid --fork`,
/// as the helper's own jobs do in theirs): its init holds a deleted file
/// that has another link, and its child (`sleep`) a Unix socket bound on
/// the host's filesystem, which the init closed. This process, outside the
/// namespace, holds a bound socket too, which must not count. The refusal
/// is compared exactly.
///
/// Needs unprivileged user namespaces; where they are not available
/// (GitHub's Ubuntu runners and Docker restrict them) the test prints
/// "skipped: ". The NixOS VM test (packaging/nix/tests/helper-tests.nix)
/// fails on that, so it runs there.
#[test]
fn post_dump_hook_scans_the_whole_pid_namespace_of_the_job() {
    use gpueue_helper::host::{DeletedFile, DumpHazards};
    use gpueue_helper::testing::{FixtureChild, fixture_dir, fixture_ready};
    use std::os::unix::fs::MetadataExt;
    const UNSHARE: [&str; 5] = ["unshare", "--user", "--map-root-user", "--pid", "--fork"];
    if let Some(dir) = fixture_dir() {
        // The init of the job's PID namespace.
        let f = dir.join("f");
        let _held = fs::File::open(&f).unwrap();
        fs::remove_file(&f).unwrap();
        let sock = std::os::unix::net::UnixListener::bind(dir.join("child.sock")).unwrap();
        rustix::io::fcntl_setfd(&sock, rustix::io::FdFlags::empty()).unwrap();
        let mut child = Command::new("sleep")
            .arg("1000")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        drop(sock);
        // /proc is the host's, so /proc/self names this process as the
        // host sees it (here it is 1).
        let host_pid = fs::read_link("/proc/self").unwrap();
        fixture_ready(host_pid.to_str().unwrap());
        let _ = child.kill();
        let _ = child.wait();
        return;
    }
    let probe = Command::new(UNSHARE[0])
        .args(&UNSHARE[1..])
        .arg("true")
        .stderr(std::process::Stdio::piped())
        .output();
    match probe {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            println!(
                "skipped: unprivileged user and PID namespaces are not available: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
            return;
        }
        Err(e) => {
            println!("skipped: running unshare: {e}");
            return;
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let orig = dir.path().join("f");
    fs::write(&orig, b"x").unwrap();
    fs::hard_link(&orig, dir.path().join("keep")).unwrap();
    let job = FixtureChild::spawn_under(
        &UNSHARE,
        "post_dump_hook_scans_the_whole_pid_namespace_of_the_job",
        dir.path(),
    );
    let init: u32 = job.payload.parse().unwrap();
    assert_ne!(init, job.pid(), "the job's init is unshare's child");
    let _outside = std::os::unix::net::UnixListener::bind(dir.path().join("outside.sock")).unwrap();

    let (ok, request) = run_hook_for(dir.path(), "post-dump", init, None);
    let keep = fs::metadata(dir.path().join("keep")).unwrap();
    drop(job);
    assert!(!ok, "the post-dump hook must fail the dump");
    let why = fs::read_to_string(hook_refusal_path(&request))
        .expect("the hook records why it refused the dump");
    let expected = DumpHazards {
        deleted_files: vec![DeletedFile {
            path: orig.to_str().unwrap().to_owned(),
            size: 1,
            uid: keep.uid(),
            mode: keep.mode(),
            nlink: 1,
            private: false,
            file_caps: false,
        }],
        bound_sockets: vec![dir.path().join("child.sock").to_str().unwrap().to_owned()],
        ..Default::default()
    }
    .refusal()
    .unwrap();
    assert_eq!(why, expected);
    assert!(!hook_result_path(&request).exists());
}

#[test]
fn the_dump_hook_does_nothing_at_other_stages() {
    let dir = tempfile::tempdir().unwrap();
    let (ok, request) = run_hook(dir.path(), "pre-dump", &[std::process::id()]);
    assert!(ok);
    assert!(!hook_refusal_path(&request).exists());
    assert!(!hook_result_path(&request).exists());
}

#[test]
fn a_dump_hook_that_cannot_read_its_request_vetoes_the_dump() {
    let dir = tempfile::tempdir().unwrap();
    let request = dir.path().join("missing.json");
    let status = Command::new(helper_exe())
        .arg("dump-hook")
        .arg(&request)
        .env("CRTOOLS_SCRIPT_ACTION", "post-dump")
        .status()
        .unwrap();
    assert!(!status.success());
    assert!(hook_refusal_path(&request).exists());
}
