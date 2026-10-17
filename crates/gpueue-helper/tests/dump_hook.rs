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
/// has one of its own). A test cannot give its job one: unprivileged user
/// namespaces, which that needs, are often not available (GitHub's Ubuntu
/// runners and Docker restrict them). The host's namespace has processes
/// with hazards of their own (and may have so many that a scan of all of
/// them takes long), so the request names the processes to scan instead.
fn run_hook(dir: &Path, action: &str, pids: &[u32]) -> (bool, std::path::PathBuf) {
    let request = dir.join("dump-hook.json");
    let req = DumpHookRequest {
        init_pid: pids[0],
        // Nothing private: the tempdir stands for a host filesystem.
        private: Vec::new(),
        pids: Some(pids.to_vec()),
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
    let job = FixtureChild::spawn(
        "post_dump_hook_vetoes_a_deleted_host_file_with_other_links",
        dir.path(),
    );
    // Another process of the job (this one) with a hazard of its own: it
    // must not hide the first one's.
    let other = std::os::unix::net::UnixListener::bind(dir.path().join("agent.sock")).unwrap();

    let (ok, request) = run_hook(dir.path(), "post-dump", &[job.pid(), std::process::id()]);
    drop((job, other));
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
