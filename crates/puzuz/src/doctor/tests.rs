use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;

use puzuz_core::model::GpuIndex;
use puzuz_protocol::api::{CheckpointStorage, DetectedGpuSummary};

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
        "nvidia-smi --query-gpu=index,uuid,name",
        CmdOutput::ok("0, GPU-a, NVIDIA GeForce RTX 4090\n"),
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
    sys.write(
        "/etc/systemd/system/puzuz.service",
        "[Service]\nExecStart=/usr/bin/puzuz daemon\n",
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
fn cuda_checkpoint_that_cannot_start_is_explained() {
    let (sys, _sockets) = healthy();
    sys.respond(
        "cuda-checkpoint --help",
        CmdOutput::failed(
            "",
            "cuda-checkpoint: error while loading shared libraries: libcuda.so.1: cannot open \
             shared object file\n",
        ),
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "cuda-checkpoint"), Level::Fail);
    assert!(message(&report, "cuda-checkpoint").contains("libcuda.so.1"));
}

fn detection(
    mode: GpuDetection,
    source: GpuSource,
    nvml_error: Option<&str>,
    uuids: &[&str],
) -> GpuDetectionStatus {
    GpuDetectionStatus {
        mode,
        source,
        nvml_error: nvml_error.map(str::to_owned),
        gpus: uuids
            .iter()
            .enumerate()
            .map(|(i, uuid)| DetectedGpuSummary {
                index: GpuIndex(i as u32),
                uuid: (*uuid).to_owned(),
                name: "NVIDIA GeForce RTX 4090".to_owned(),
            })
            .collect(),
    }
}

fn with_detection(sys: &mut FakeSystem, detection: GpuDetectionStatus) {
    let mut version = FakeSystem::daemon_ok(true).unwrap();
    version.gpu_detection = Some(detection);
    sys.daemon = Ok(version);
}

#[test]
fn nvml_detection_needs_no_gpus_list() {
    let (mut sys, _sockets) = healthy();
    sys.write("/etc/puzuz/config.toml", "");
    with_detection(
        &mut sys,
        detection(GpuDetection::Auto, GpuSource::Nvml, None, &["GPU-a"]),
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "gpus"), Level::Ok, "{report:#?}");
    assert_eq!(message(&report, "gpus"), "1 GPU(s), detected with NVML");
    assert!(!report.has_failures());

    // A fallback list that matches nvidia-smi is fine too.
    sys.write("/etc/puzuz/config.toml", CONFIG);
    assert_eq!(level(&run(&sys, None), "gpus"), Level::Ok);
}

#[test]
fn nvml_failing_in_auto_mode_warns_with_the_reason() {
    let (mut sys, _sockets) = healthy();
    let error = "cannot initialize NVML: libnvidia-ml.so.1: NVIDIA driver is not loaded \
                 (/dev/nvidiactl does not exist)";
    // Even when the fallback list matches nvidia-smi.
    with_detection(
        &mut sys,
        detection(
            GpuDetection::Auto,
            GpuSource::Config,
            Some(error),
            &["GPU-a"],
        ),
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "gpus"), Level::Warn);
    assert!(message(&report, "gpus").contains(error));
    assert!(message(&report, "gpus").contains("uses the 1 GPU(s) of [[gpus]]"));
    assert!(hint(&report, "gpus").contains("DeviceAllow="));

    sys.write("/etc/puzuz/config.toml", "");
    with_detection(
        &mut sys,
        detection(GpuDetection::Auto, GpuSource::Config, Some(error), &[]),
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "gpus"), Level::Warn);
    assert!(message(&report, "gpus").contains("runs without GPUs"));
}

#[test]
fn no_detected_gpus_while_nvidia_smi_sees_some_warn() {
    let (mut sys, _sockets) = healthy();
    sys.write("/etc/puzuz/config.toml", "gpu_detection = \"static\"\n");
    with_detection(
        &mut sys,
        detection(GpuDetection::Static, GpuSource::Config, None, &[]),
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "gpus"), Level::Warn);
    assert!(message(&report, "gpus").contains("the daemon has none"));
    assert!(hint(&report, "gpus").contains("[[gpus]]"));

    // NVML found none (e.g. a GPU added since the daemon started).
    with_detection(
        &mut sys,
        detection(GpuDetection::Auto, GpuSource::Nvml, None, &[]),
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "gpus"), Level::Warn);
    assert!(hint(&report, "gpus").contains("systemctl restart puzuz.service"));
}

#[test]
fn static_list_disagreeing_with_nvidia_smi_warns() {
    let (mut sys, _sockets) = healthy();
    with_detection(
        &mut sys,
        detection(GpuDetection::Static, GpuSource::Config, None, &["GPU-x"]),
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "gpus"), Level::Warn);
    assert!(
        message(&report, "gpus").contains("GPU 0 is GPU-x but nvidia-smi shows GPU-a"),
        "{}",
        message(&report, "gpus")
    );

    with_detection(
        &mut sys,
        detection(
            GpuDetection::Static,
            GpuSource::Config,
            None,
            &["GPU-a", "GPU-b"],
        ),
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "gpus"), Level::Warn);
    assert!(message(&report, "gpus").contains("GPU 1 is not in nvidia-smi"));

    with_detection(
        &mut sys,
        detection(GpuDetection::Static, GpuSource::Config, None, &["GPU-a"]),
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "gpus"), Level::Ok);
    assert_eq!(
        message(&report, "gpus"),
        "1 GPU(s), from [[gpus]] (gpu_detection = \"static\")"
    );
}

#[test]
fn stale_fallback_list_warns_even_when_nvml_works() {
    let (mut sys, _sockets) = healthy();
    sys.write(
        "/etc/puzuz/config.toml",
        "[[gpus]]\nindex = 0\nuuid = \"GPU-old\"\n",
    );
    with_detection(
        &mut sys,
        detection(GpuDetection::Auto, GpuSource::Nvml, None, &["GPU-a"]),
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "gpus"), Level::Warn);
    assert!(message(&report, "gpus").contains("would be used if NVML failed"));
}

#[test]
fn without_the_daemons_report_only_static_mode_is_checked() {
    let (mut sys, _sockets) = healthy();
    // Auto mode: what the daemon detected is unknown.
    sys.write("/etc/puzuz/config.toml", "");
    let report = run(&sys, None);
    assert_eq!(level(&report, "gpus"), Level::Info);
    sys.daemon = Err("connection refused".to_owned());
    assert_eq!(level(&run(&sys, None), "gpus"), Level::Info);

    // Static mode: the configuration is what the daemon uses.
    sys.write("/etc/puzuz/config.toml", "gpu_detection = \"static\"\n");
    let report = run(&sys, None);
    assert_eq!(level(&report, "gpus"), Level::Warn);
    assert!(hint(&report, "gpus").contains("[[gpus]]"));
    sys.write(
        "/etc/puzuz/config.toml",
        &format!("gpu_detection = \"static\"\n{CONFIG}"),
    );
    assert_eq!(level(&run(&sys, None), "gpus"), Level::Ok);
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
fn config_the_helper_would_refuse_fails() {
    let (sys, _sockets) = healthy();
    let file = sys.path(Path::new("/etc/puzuz/config.toml"));
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o664)).unwrap();
    let report = run(&sys, None);
    assert_eq!(level(&report, "config"), Level::Fail);
    assert!(message(&report, "config").contains("helper refuses"));
    assert!(hint(&report, "config").contains("chmod go-w /etc/puzuz/config.toml"));

    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let dir = sys.path(Path::new("/etc/puzuz"));
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    let report = run(&sys, None);
    assert_eq!(level(&report, "config"), Level::Fail);
    assert!(message(&report, "config").starts_with("/etc/puzuz is"));
}

#[test]
fn helper_settings_come_from_the_config_unless_flags_override_them() {
    let (sys, _sockets) = healthy();
    sys.write(
        "/etc/puzuz/config.toml",
        &format!("cuda_checkpoint_mode = \"plugin\"\ncheckpoints_dir = \"/ckpt\"\n{CONFIG}"),
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "helper settings"), Level::Info);
    let shown = message(&report, "helper settings");
    assert!(shown.contains("cuda mode plugin"), "{shown}");
    assert!(shown.contains("checkpoints /ckpt"), "{shown}");

    sys.write(
        "/etc/systemd/system/puzuz-helper.service",
        "[Service]\nExecStart=/usr/bin/puzuz-helper serve --criu /opt/criu --cuda-mode=manual\n",
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "helper settings"), Level::Warn);
    assert!(message(&report, "helper settings").contains("--criu --cuda-mode"));
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

fn with_storage(sys: &mut FakeSystem, storage: CheckpointStorage) {
    let mut version = FakeSystem::daemon_ok(true).unwrap();
    version.checkpoints = Some(storage);
    sys.daemon = Ok(version);
}

#[test]
fn checkpoint_usage_comes_from_the_daemon() {
    let (mut sys, _sockets) = healthy();
    // Daemons that do not report it: no usage check.
    assert!(run(&sys, None).find("checkpoint usage").is_none());

    // No limit.
    with_storage(
        &mut sys,
        CheckpointStorage {
            images: 2,
            total_bytes: 3 << 30,
            limit_bytes: None,
            available_bytes: Some(400 << 30),
        },
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "checkpoint usage"), Level::Info);
    assert_eq!(
        message(&report, "checkpoint usage"),
        "2 image(s) take 3.0 GiB; no checkpoint_size_limit_bytes set"
    );
    // The daemon's free space wins over what this process sees.
    assert!(message(&report, "checkpoint dir").contains("400.0 GiB free"));
    assert!(!report.has_failures());

    // Within the limit.
    with_storage(
        &mut sys,
        CheckpointStorage {
            images: 2,
            total_bytes: 3 << 30,
            limit_bytes: Some(100 << 30),
            available_bytes: Some(400 << 30),
        },
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "checkpoint usage"), Level::Ok);
    assert!(
        message(&report, "checkpoint usage").contains("of checkpoint_size_limit_bytes (100.0 GiB)")
    );

    // Over the limit: a warning with what to do, not a failure.
    with_storage(
        &mut sys,
        CheckpointStorage {
            images: 3,
            total_bytes: 120 << 30,
            limit_bytes: Some(100 << 30),
            available_bytes: Some(400 << 30),
        },
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "checkpoint usage"), Level::Warn);
    assert!(message(&report, "checkpoint usage").contains("120.0 GiB, over"));
    assert!(hint(&report, "checkpoint usage").contains("puzuz status"));
    assert!(hint(&report, "checkpoint usage").contains("puzuz remove"));
    assert_eq!(level(&report, "checkpoint dir"), Level::Ok);
    assert!(!report.has_failures());
    let mut out = Vec::new();
    report.print(&mut out).unwrap();
    let out = String::from_utf8(out).unwrap();
    assert!(
        out.contains("WARN  checkpoint usage: 3 image(s) take 120.0 GiB"),
        "{out}"
    );
    assert!(out.contains("      fix: find large checkpoints"), "{out}");
}

#[test]
fn low_free_space_reported_by_the_daemon_warns() {
    let (mut sys, _sockets) = healthy();
    // Plenty of space locally, but the daemon sees little.
    sys.fs_usage.available_bytes = 500 << 30;
    with_storage(
        &mut sys,
        CheckpointStorage {
            images: 0,
            total_bytes: 0,
            limit_bytes: None,
            available_bytes: Some(5 << 30),
        },
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "checkpoint dir"), Level::Warn);
    assert!(message(&report, "checkpoint dir").contains("5.0 GiB"));

    // With a limit, only the room still allowed under it is needed.
    with_storage(
        &mut sys,
        CheckpointStorage {
            images: 1,
            total_bytes: 550 << 30,
            limit_bytes: Some(600 << 30),
            available_bytes: Some(100 << 30),
        },
    );
    assert_eq!(level(&run(&sys, None), "checkpoint dir"), Level::Ok);
    with_storage(
        &mut sys,
        CheckpointStorage {
            images: 1,
            total_bytes: 100 << 30,
            limit_bytes: Some(600 << 30),
            available_bytes: Some(100 << 30),
        },
    );
    assert_eq!(level(&run(&sys, None), "checkpoint dir"), Level::Warn);

    // Without the daemon's figure, the local one is used.
    with_storage(
        &mut sys,
        CheckpointStorage {
            images: 0,
            total_bytes: 0,
            limit_bytes: None,
            available_bytes: None,
        },
    );
    assert_eq!(level(&run(&sys, None), "checkpoint dir"), Level::Ok);
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

#[test]
fn plugin_mode_needs_the_cuda_plugin() {
    let (sys, _listeners) = healthy();
    assert!(run(&sys, None).find("cuda plugin").is_none(), "manual mode");
    sys.write(
        "/etc/puzuz/config.toml",
        &format!("cuda_checkpoint_mode = \"plugin\"\n{CONFIG}"),
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "cuda plugin"), Level::Fail, "{report:#?}");
    sys.write("/usr/lib/criu/cuda_plugin.so", "");
    assert_eq!(level(&run(&sys, None), "cuda plugin"), Level::Ok);
}

#[test]
fn effective_exec_start_includes_drop_ins() {
    let (sys, _sockets) = healthy();
    // A drop-in replaced ExecStart=; the unit file in /etc still has the packaged one.
    sys.write_exe("/opt/puzuz/bin/puzuz-helper", "");
    sys.respond(
        "systemctl show --property=ExecStart --value puzuz-helper.service",
        CmdOutput::ok(
            "{ path=/opt/puzuz/bin/puzuz-helper ; argv[]=/opt/puzuz/bin/puzuz-helper serve \
             --criu /opt/criu ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; \
             pid=0 ; code=(null) ; status=0/0 }\n",
        ),
    );
    let report = run(&sys, None);
    assert_eq!(
        message(&report, "helper binary"),
        "/opt/puzuz/bin/puzuz-helper"
    );
    assert_eq!(level(&report, "helper settings"), Level::Warn);
    assert!(message(&report, "helper settings").contains("passes --criu,"));
}

#[test]
fn show_exec_start_parsing() {
    assert_eq!(
        parse_show_exec_start(
            "{ path=/usr/bin/x ; argv[]=/usr/bin/x serve --a=1 ; ignore_errors=no ; ... }\n"
        ),
        Some(ExecStart {
            program: PathBuf::from("/usr/bin/x"),
            argv: vec!["/usr/bin/x".into(), "serve".into(), "--a=1".into()],
        })
    );
    assert_eq!(parse_show_exec_start(""), None);
    assert_eq!(
        flags_of(&["/x".into(), "--a=1".into(), "b".into()]),
        ["--a"]
    );
}

#[test]
fn helper_binary_in_a_writable_directory_fails() {
    let (sys, _sockets) = healthy();
    sys.write_exe("/opt/shared/bin/puzuz-helper", "");
    sys.write(
        "/etc/systemd/system/puzuz-helper.service",
        "[Service]\nExecStart=/opt/shared/bin/puzuz-helper serve\n",
    );
    let shared = sys.path(Path::new("/opt/shared"));
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
    let report = run(&sys, None);
    assert_eq!(level(&report, "helper binary"), Level::Fail);
    assert!(
        message(&report, "helper binary").contains("/opt/shared must be owned by root"),
        "{}",
        message(&report, "helper binary")
    );
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(level(&run(&sys, None), "helper binary"), Level::Ok);
}

#[test]
fn config_the_daemon_cannot_read_warns() {
    let (sys, _sockets) = healthy();
    let file = sys.path(Path::new("/etc/puzuz/config.toml"));
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let report = run(&sys, None);
    assert_eq!(level(&report, "config"), Level::Warn);
    assert!(message(&report, "config").contains("not readable by others"));

    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let dir = sys.path(Path::new("/etc/puzuz"));
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let report = run(&sys, None);
    assert_eq!(level(&report, "config"), Level::Warn);
    assert!(message(&report, "config").contains("/etc/puzuz is not searchable"));
    assert!(hint(&report, "config").contains("chmod 755 /etc/puzuz"));
}

#[test]
fn daemon_paths_outside_read_write_paths_warn() {
    let (sys, _sockets) = healthy();
    // Not reported by systemd: no check.
    assert!(run(&sys, None).find("daemon write paths").is_none());

    sys.respond(
        "systemctl show --property=ProtectSystem --property=ReadWritePaths puzuz.service",
        CmdOutput::ok(
            "ProtectSystem=strict\nReadWritePaths=/var/lib/puzuz/daemon /var/lib/puzuz/logs \
             -/run/puzuz\n",
        ),
    );
    assert_eq!(level(&run(&sys, None), "daemon write paths"), Level::Ok);

    sys.write(
        "/etc/puzuz/config.toml",
        &format!("logs_dir = \"/srv/logs\"\nstate_db = \"/srv/db/state.sqlite\"\n{CONFIG}"),
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "daemon write paths"), Level::Warn);
    let shown = message(&report, "daemon write paths");
    assert!(
        shown.contains("/srv/db (state_db), /srv/logs (logs_dir)"),
        "{shown}"
    );
    assert!(
        hint(&report, "daemon write paths").contains("ReadWritePaths=/srv/db /srv/logs\n"),
        "{}",
        hint(&report, "daemon write paths")
    );

    // A drop-in that disables the sandbox makes every path writable.
    sys.respond(
        "systemctl show --property=ProtectSystem --property=ReadWritePaths puzuz.service",
        CmdOutput::ok("ProtectSystem=no\nReadWritePaths=\n"),
    );
    assert!(run(&sys, None).find("daemon write paths").is_none());
}

fn marker_unit(bin_dir: &str) -> String {
    format!(
        "{}\n[Service]\nExecStart={bin_dir}/puzuz daemon\n",
        crate::service::MARKER
    )
}

fn write_both(sys: &FakeSystem, dir: &str, contents: &str) {
    for unit in ["puzuz.service", "puzuz-helper.service"] {
        sys.write(&format!("{dir}/{unit}"), contents);
    }
}

/// Both units from the dpkg package `puzuz`.
fn install_package(sys: &FakeSystem) {
    for unit in ["puzuz.service", "puzuz-helper.service"] {
        let unit = format!("/usr/lib/systemd/system/{unit}");
        sys.write(&unit, "[Service]\nExecStart=/usr/bin/puzuz daemon\n");
        sys.dpkg_owns(&unit, "puzuz");
    }
}

#[test]
fn install_method_is_reported() {
    let (sys, _sockets) = healthy();
    for unit in ["puzuz.service", "puzuz-helper.service"] {
        std::fs::remove_file(sys.path(&Path::new("/etc/systemd/system").join(unit))).unwrap();
    }
    assert_eq!(message(&run(&sys, None), "install method"), "not installed");

    write_both(
        &sys,
        "/etc/systemd/system",
        "[Service]\nExecStart=/usr/bin/puzuz daemon\n",
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "install method"), Level::Info);
    assert!(message(&report, "install method").starts_with("by hand"));
    assert!(hint(&report, "install method").contains("--force"));

    write_both(&sys, "/etc/systemd/system", &marker_unit("/usr/local/bin"));
    let report = run(&sys, None);
    assert_eq!(level(&report, "install method"), Level::Ok);
    assert_eq!(
        message(&report, "install method"),
        "puzuz service install (binaries in /usr/local/bin)"
    );
}

#[test]
fn a_masked_or_overridden_package_does_not_fail() {
    let (sys, _sockets) = healthy();
    install_package(&sys);
    sys.write(
        "/etc/systemd/system/puzuz.service",
        "[Service]\nExecStart=/usr/bin/puzuz daemon\n",
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "install method"), Level::Ok);
    assert!(
        message(&report, "install method").contains("overridden by"),
        "{}",
        message(&report, "install method")
    );

    let etc = sys.path(Path::new("/etc/systemd/system/puzuz.service"));
    std::fs::remove_file(&etc).unwrap();
    std::os::unix::fs::symlink("/dev/null", &etc).unwrap();
    let report = run(&sys, None);
    assert_eq!(level(&report, "install method"), Level::Info);
    assert!(message(&report, "install method").contains("masked by"));
    assert!(hint(&report, "install method").contains("systemctl unmask"));
}

#[test]
fn mixed_install_methods_fail() {
    let (sys, _sockets) = healthy();
    write_both(&sys, "/etc/systemd/system", &marker_unit("/usr/local/bin"));
    install_package(&sys);
    let report = run(&sys, None);
    assert_eq!(level(&report, "install method"), Level::Fail);
    assert!(message(&report, "install method").starts_with("mixed: "));
    let hint = hint(&report, "install method");
    assert!(hint.contains("sudo puzuz service uninstall"), "{hint}");
    assert!(hint.contains("sudo apt remove puzuz"), "{hint}");
}

fn remove_etc_units(sys: &FakeSystem) {
    for unit in ["puzuz.service", "puzuz-helper.service"] {
        let _ = std::fs::remove_file(sys.path(&Path::new("/etc/systemd/system").join(unit)));
    }
}

fn not_owned(sys: &FakeSystem) {
    sys.respond(
        "dpkg-query -S",
        CmdOutput::failed("", "dpkg-query: no path found matching pattern\n"),
    );
}

#[test]
fn an_unowned_unit_warns() {
    let (sys, _sockets) = healthy();
    remove_etc_units(&sys);
    write_both(
        &sys,
        "/usr/lib/systemd/system",
        "[Service]\nExecStart=/usr/bin/puzuz daemon\n",
    );
    not_owned(&sys);
    let report = run(&sys, None);
    assert_eq!(level(&report, "install method"), Level::Warn);
    assert!(
        message(&report, "install method").starts_with("a unit no package owns"),
        "{}",
        message(&report, "install method")
    );
    assert!(hint(&report, "install method").contains("sudo rm /usr/lib/systemd/system/puzuz"));
}

#[test]
fn a_leftover_next_to_service_install_warns() {
    let (sys, _sockets) = healthy();
    write_both(&sys, "/etc/systemd/system", &marker_unit("/usr/local/bin"));
    write_both(
        &sys,
        "/usr/lib/systemd/system",
        "[Service]\nExecStart=/usr/bin/puzuz daemon\n",
    );
    not_owned(&sys);
    let report = run(&sys, None);
    assert_eq!(level(&report, "install method"), Level::Warn);
    let shown = message(&report, "install method");
    assert!(shown.starts_with("puzuz service install"), "{shown}");
    assert!(shown.contains("no package owns"), "{shown}");
    assert!(hint(&report, "install method").contains("remove the leftover /usr/lib unit"));
}

#[test]
fn units_installed_in_different_ways_warn() {
    let (sys, _sockets) = healthy();
    remove_etc_units(&sys);
    sys.write(
        "/etc/systemd/system/puzuz.service",
        &marker_unit("/usr/local/bin"),
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "install method"), Level::Warn);
    assert!(
        message(&report, "install method").starts_with("puzuz-helper.service is missing"),
        "{}",
        message(&report, "install method")
    );
    assert!(hint(&report, "install method").contains("sudo puzuz service install"));

    // A packaged helper unit next to it conflicts: FAIL, like a mixed setup.
    let unit = "/usr/lib/systemd/system/puzuz-helper.service";
    sys.write(unit, "[Service]\nExecStart=/usr/bin/puzuz-helper serve\n");
    sys.dpkg_owns(unit, "puzuz");
    let report = run(&sys, None);
    assert_eq!(level(&report, "install method"), Level::Fail);
    assert!(message(&report, "install method").contains("conflict"));
    let hint = hint(&report, "install method");
    assert!(hint.contains("sudo puzuz service uninstall"), "{hint}");
    assert!(hint.contains("sudo apt remove puzuz"), "{hint}");
}

#[test]
fn nixos_is_reported() {
    let (sys, _sockets) = healthy();
    remove_etc_units(&sys);
    sys.write("/etc/os-release", "ID=nixos\n");
    let report = run(&sys, None);
    assert_eq!(level(&report, "install method"), Level::Info);
    assert_eq!(
        message(&report, "install method"),
        "not installed (NixOS: use the services.puzuz module)"
    );

    for unit in ["puzuz.service", "puzuz-helper.service"] {
        let link = sys.path(&Path::new("/etc/systemd/system").join(unit));
        std::os::unix::fs::symlink(format!("/nix/store/abc-{unit}"), link).unwrap();
    }
    let report = run(&sys, None);
    assert_eq!(level(&report, "install method"), Level::Ok);
    assert!(message(&report, "install method").starts_with("Nix ("));
}
