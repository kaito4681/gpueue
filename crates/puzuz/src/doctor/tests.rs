use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;

use super::*;
use crate::system::fake::FakeSystem;
use crate::system::{CmdOutput, FsUsage};

const CUDA_HELP: &str = "\
CUDA checkpoint and restore utility.
Version 595.91.07. Copyright (C) 2025 NVIDIA Corporation. All rights reserved.
    --get-state                       Prints the current state of the process
    --action <ACTION>                 Action to perform: lock, checkpoint, restore, unlock
    --timeout <TIMEOUT>               Timeout in milliseconds for lock
    --device-map <MAP>                Device map for restore
";

const CONFIG: &str = "\
[[gpus]]
index = 0
uuid = \"GPU-a\"
name = \"RTX\"
";

/// A healthy installation. Sockets are bound for as long as the returned listeners live.
fn healthy() -> (FakeSystem, Vec<UnixListener>) {
    let mut sys = FakeSystem::new();
    sys.euid = 1000;
    sys.write("/etc/puzuz/config.toml", CONFIG);
    sys.write_exe("/usr/sbin/criu", "");
    sys.respond(
        "criu --version",
        CmdOutput::ok("Version: 4.2.1\nGitID: v4.2.1\n"),
    );
    sys.write(
        "/proc/driver/nvidia/version",
        "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  595.91.07  Release Build  \
         (dvs-builder@U16)  Mon Jan 1 00:00:00 UTC 2026\nGCC version:  gcc version 13.3.0\n",
    );
    sys.write_exe("/usr/local/bin/cuda-checkpoint", "");
    sys.respond("cuda-checkpoint --help", CmdOutput::ok(CUDA_HELP));
    sys.write_exe("/usr/bin/nvidia-smi", "");
    sys.respond(
        "nvidia-smi --query-gpu=index,name",
        CmdOutput::ok("0, NVIDIA GeForce RTX 4090\n"),
    );
    sys.write("/proc/sys/kernel/osrelease", "6.8.0-generic\n");
    sys.write("/proc/sys/kernel/ns_last_pid", "1234\n");
    sys.respond(
        "systemctl --version",
        CmdOutput::ok("systemd 259 (259.1-1)\n+PAM\n"),
    );
    sys.respond("systemctl is-enabled", CmdOutput::ok("enabled\n"));
    sys.respond("systemctl is-active", CmdOutput::ok("active\n"));
    sys.write(
        "/etc/systemd/system/puzuz-helper.service",
        "[Service]\nExecStart=/usr/bin/puzuz-helper serve\n",
    );
    sys.write_exe("/usr/bin/puzuz-helper", "");
    sys.mkdir("/run/puzuz");
    let listeners = ["/run/puzuz/puzuz.sock", "/run/puzuz/helper.sock"]
        .iter()
        .map(|path| UnixListener::bind(sys.path(Path::new(path))).unwrap())
        .collect();
    sys.daemon = FakeSystem::daemon_ok(true);
    sys.mkdir("/var/lib/puzuz/checkpoints");
    sys.groups.borrow_mut().insert("puzuz-admin".to_owned());
    (sys, listeners)
}

fn level(report: &Report, name: &str) -> Level {
    report
        .find(name)
        .unwrap_or_else(|| panic!("no check {name}: {report:#?}"))
        .level
}

fn message<'a>(report: &'a Report, name: &str) -> &'a str {
    &report.find(name).unwrap().message
}

fn hint<'a>(report: &'a Report, name: &str) -> &'a str {
    report.find(name).unwrap().hint.as_deref().unwrap_or("")
}

#[test]
fn healthy_installation_passes() {
    let (sys, _sockets) = healthy();
    let report = run(&sys, None);
    for check in &report.checks {
        assert!(matches!(check.level, Level::Ok | Level::Info), "{check:?}");
    }
    assert!(!report.has_failures());
    assert_eq!(message(&report, "criu"), "4.2.1 (/usr/sbin/criu)");
    assert_eq!(
        message(&report, "cuda-checkpoint"),
        "595.91.07 (/usr/local/bin/cuda-checkpoint)"
    );
    assert_eq!(message(&report, "nvidia driver"), "595.91.07");
    assert_eq!(message(&report, "kernel"), "6.8.0-generic");
    assert_eq!(message(&report, "systemd"), "259");
    assert_eq!(message(&report, "puzuz.service"), "enabled, active");
    assert!(message(&report, "admin").contains("not in puzuz-admin"));

    let mut out = Vec::new();
    report.print(&mut out).unwrap();
    let out = String::from_utf8(out).unwrap();
    assert!(
        out.contains("OK    criu: 4.2.1 (/usr/sbin/criu)\n"),
        "{out}"
    );
    assert!(out.contains("INFO  admin: "), "{out}");
    assert!(out.ends_with("Everything looks good.\n"), "{out}");
}

#[test]
fn printed_failures_carry_fix_hints() {
    let (sys, _sockets) = healthy();
    sys.missing("criu");
    std::fs::remove_file(sys.path(Path::new("/usr/sbin/criu"))).unwrap();
    let report = run(&sys, None);
    assert!(report.has_failures());
    let mut out = Vec::new();
    report.print(&mut out).unwrap();
    let out = String::from_utf8(out).unwrap();
    assert!(
        out.contains("FAIL  criu: criu not found\n      fix: install CRIU 4.2"),
        "{out}"
    );
    assert!(out.ends_with("1 failed, 0 warning(s).\n"), "{out}");
}

#[test]
fn old_criu_fails() {
    let (sys, _sockets) = healthy();
    sys.respond("criu --version", CmdOutput::ok("Version: 3.19\n"));
    let report = run(&sys, None);
    assert_eq!(level(&report, "criu"), Level::Fail);
    assert!(message(&report, "criu").contains("3.19, too old"));

    sys.respond("criu --version", CmdOutput::ok("garbage"));
    assert_eq!(level(&run(&sys, None), "criu"), Level::Fail);
}

#[test]
fn configured_criu_path_is_used() {
    let (sys, _sockets) = healthy();
    sys.write(
        "/etc/puzuz/config.toml",
        &format!("criu_path = \"/opt/criu/sbin/criu\"\n{CONFIG}"),
    );
    assert_eq!(level(&run(&sys, None), "criu"), Level::Fail);
    sys.write_exe("/opt/criu/sbin/criu", "");
    let report = run(&sys, None);
    assert_eq!(message(&report, "criu"), "4.2.1 (/opt/criu/sbin/criu)");
}

#[test]
fn without_a_driver_gpu_checks_only_warn() {
    let (sys, _sockets) = healthy();
    std::fs::remove_file(sys.path(Path::new("/proc/driver/nvidia/version"))).unwrap();
    std::fs::remove_file(sys.path(Path::new("/usr/bin/nvidia-smi"))).unwrap();
    std::fs::remove_file(sys.path(Path::new("/usr/local/bin/cuda-checkpoint"))).unwrap();
    let report = run(&sys, None);
    assert_eq!(level(&report, "nvidia driver"), Level::Warn);
    assert_eq!(level(&report, "cuda-checkpoint"), Level::Warn);
    assert!(report.find("gpus").is_none());
    assert!(!report.has_failures(), "{report:#?}");
}

#[test]
fn driver_version_falls_back_to_nvidia_smi() {
    let (sys, _sockets) = healthy();
    std::fs::remove_file(sys.path(Path::new("/proc/driver/nvidia/version"))).unwrap();
    sys.respond(
        "nvidia-smi --query-gpu=driver_version",
        CmdOutput::ok("580.65.06\n"),
    );
    let report = run(&sys, None);
    assert_eq!(message(&report, "nvidia driver"), "580.65.06");
    // cuda-checkpoint 595 does not match driver 580.
    assert_eq!(level(&report, "cuda-checkpoint"), Level::Warn);
}

#[test]
fn cuda_checkpoint_problems_fail_with_a_driver() {
    let (sys, _sockets) = healthy();
    std::fs::remove_file(sys.path(Path::new("/usr/local/bin/cuda-checkpoint"))).unwrap();
    let report = run(&sys, None);
    assert_eq!(level(&report, "cuda-checkpoint"), Level::Fail);
    assert!(hint(&report, "cuda-checkpoint").contains("NVIDIA/cuda-checkpoint"));

    sys.write_exe("/usr/local/bin/cuda-checkpoint", "");
    sys.respond(
        "cuda-checkpoint --help",
        CmdOutput::ok("Version 550.54.14.\n    --toggle    Toggle\n"),
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "cuda-checkpoint"), Level::Fail);
    assert!(message(&report, "cuda-checkpoint").contains("lacks --action"));
}

#[test]
fn unconfigured_gpus_warn() {
    let (sys, _sockets) = healthy();
    sys.write("/etc/puzuz/config.toml", "");
    let report = run(&sys, None);
    assert_eq!(level(&report, "gpus"), Level::Warn);
    assert!(hint(&report, "gpus").contains("[[gpus]]"));
}

#[test]
fn config_problems() {
    let (sys, _sockets) = healthy();
    sys.write("/etc/puzuz/config.toml", "grace_secs = \"x\"\n");
    let report = run(&sys, None);
    assert_eq!(level(&report, "config"), Level::Fail);

    std::fs::remove_file(sys.path(Path::new("/etc/puzuz/config.toml"))).unwrap();
    let report = run(&sys, None);
    assert_eq!(level(&report, "config"), Level::Warn);
    assert!(hint(&report, "config").contains("sudo puzuz service install"));
}

#[test]
fn kernel_checks() {
    let (sys, _sockets) = healthy();
    sys.write("/proc/sys/kernel/osrelease", "5.4.0\n");
    assert_eq!(level(&run(&sys, None), "kernel"), Level::Warn);
    std::fs::remove_file(sys.path(Path::new("/proc/sys/kernel/ns_last_pid"))).unwrap();
    let report = run(&sys, None);
    assert_eq!(level(&report, "kernel"), Level::Fail);
    assert!(message(&report, "kernel").contains("CONFIG_CHECKPOINT_RESTORE"));
}

#[test]
fn systemd_checks() {
    let (sys, _sockets) = healthy();
    sys.respond("systemctl --version", CmdOutput::ok("systemd 237\n"));
    assert_eq!(level(&run(&sys, None), "systemd"), Level::Fail);
    sys.missing("systemctl");
    let report = run(&sys, None);
    assert_eq!(level(&report, "systemd"), Level::Fail);
    assert_eq!(level(&report, "puzuz.service"), Level::Fail);
}

#[test]
fn unit_states() {
    let (sys, _sockets) = healthy();
    sys.respond(
        "systemctl is-enabled puzuz.service",
        CmdOutput::failed("not-found\n", ""),
    );
    sys.respond(
        "systemctl is-enabled puzuz-helper.service",
        CmdOutput::failed("disabled\n", ""),
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "puzuz.service"), Level::Fail);
    assert_eq!(hint(&report, "puzuz.service"), "sudo puzuz service install");
    assert_eq!(
        hint(&report, "puzuz-helper.service"),
        "sudo systemctl enable --now puzuz-helper.service"
    );

    let (sys, _sockets) = healthy();
    sys.respond(
        "systemctl is-active puzuz-helper.service",
        CmdOutput::failed("failed\n", ""),
    );
    let report = run(&sys, None);
    assert_eq!(message(&report, "puzuz-helper.service"), "enabled, failed");
    assert!(hint(&report, "puzuz-helper.service").contains("journalctl -u puzuz-helper.service"));
}

#[test]
fn helper_binary_must_be_protected() {
    let (sys, _sockets) = healthy();
    let exe = sys.path(Path::new("/usr/bin/puzuz-helper"));
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o775)).unwrap();
    let report = run(&sys, None);
    assert_eq!(level(&report, "helper binary"), Level::Fail);
    assert!(hint(&report, "helper binary").contains("chmod 755 /usr/bin/puzuz-helper"));

    // The unit file systemd reports wins over /etc/systemd/system.
    sys.write(
        "/usr/lib/systemd/system/puzuz-helper.service",
        "[Service]\nExecStart=/usr/local/bin/puzuz-helper serve\n",
    );
    sys.write_exe("/usr/local/bin/puzuz-helper", "");
    sys.respond(
        "systemctl show --property=FragmentPath --value puzuz-helper.service",
        CmdOutput::ok("/usr/lib/systemd/system/puzuz-helper.service\n"),
    );
    let report = run(&sys, None);
    assert_eq!(
        message(&report, "helper binary"),
        "/usr/local/bin/puzuz-helper"
    );
}

#[test]
fn exec_start_parsing() {
    assert_eq!(
        exec_start_binary("[Service]\nExecStart=-/usr/bin/x serve\n"),
        Some(PathBuf::from("/usr/bin/x"))
    );
    assert_eq!(exec_start_binary("[Service]\n"), None);
}

#[test]
fn sockets_and_connections() {
    let (mut sys, sockets) = healthy();
    drop(sockets);
    std::fs::remove_file(sys.path(Path::new("/run/puzuz/helper.sock"))).unwrap();
    sys.daemon = Err("connection refused".to_owned());
    let report = run(&sys, None);
    // A leftover socket file still counts as present.
    assert_eq!(level(&report, "daemon socket"), Level::Ok);
    assert_eq!(level(&report, "helper socket"), Level::Fail);
    assert_eq!(level(&report, "daemon"), Level::Fail);
    assert!(message(&report, "daemon").contains("connection refused"));
    assert_eq!(level(&report, "helper"), Level::Warn);

    sys.daemon = FakeSystem::daemon_ok(false);
    let report = run(&sys, None);
    assert_eq!(level(&report, "daemon"), Level::Ok);
    assert_eq!(level(&report, "helper"), Level::Fail);

    let mut version = FakeSystem::daemon_ok(true).unwrap();
    version.daemon_version = "0.0.1".to_owned();
    sys.daemon = Ok(version);
    let report = run(&sys, None);
    assert_eq!(level(&report, "daemon"), Level::Warn);
}

#[test]
fn socket_override() {
    let (sys, _sockets) = healthy();
    let report = run(&sys, Some(Path::new("/run/other.sock")));
    assert_eq!(level(&report, "daemon socket"), Level::Fail);
    assert!(message(&report, "daemon socket").starts_with("/run/other.sock"));
}

#[test]
fn checkpoint_storage() {
    let (mut sys, _sockets) = healthy();
    sys.fs_usage = FsUsage {
        available_bytes: 5 << 30,
        tmpfs: false,
    };
    let report = run(&sys, None);
    assert_eq!(level(&report, "checkpoint dir"), Level::Warn);
    assert!(message(&report, "checkpoint dir").contains("5.0 GiB"));

    sys.fs_usage.tmpfs = true;
    assert_eq!(level(&run(&sys, None), "checkpoint dir"), Level::Fail);

    std::fs::remove_dir(sys.path(Path::new("/var/lib/puzuz/checkpoints"))).unwrap();
    let report = run(&sys, None);
    assert_eq!(level(&report, "checkpoint dir"), Level::Fail);
    assert!(hint(&report, "checkpoint dir").contains("systemd-tmpfiles --create"));
}

#[test]
fn checkpoint_size_limit_raises_the_space_threshold() {
    let (mut sys, _sockets) = healthy();
    sys.write(
        "/etc/puzuz/config.toml",
        &format!("checkpoint_size_limit_bytes = {}\n{CONFIG}", 600u64 << 30),
    );
    sys.fs_usage.available_bytes = 500 << 30;
    assert_eq!(level(&run(&sys, None), "checkpoint dir"), Level::Warn);
}

#[test]
fn admin_membership() {
    let (mut sys, _sockets) = healthy();
    sys.member_of.insert("puzuz-admin".to_owned());
    let report = run(&sys, None);
    assert_eq!(level(&report, "admin"), Level::Info);
    assert!(message(&report, "admin").contains("administrator"));

    sys.groups.borrow_mut().clear();
    assert_eq!(level(&run(&sys, None), "admin"), Level::Warn);

    sys.euid = 0;
    assert_eq!(level(&run(&sys, None), "admin"), Level::Warn);
    sys.groups.borrow_mut().insert("puzuz-admin".to_owned());
    assert!(message(&run(&sys, None), "admin").contains("root"));
}

#[test]
fn byte_formatting() {
    assert_eq!(format_bytes(512), "512 B");
    assert_eq!(format_bytes(1536), "1.5 KiB");
    assert_eq!(format_bytes(20 << 30), "20.0 GiB");
}
