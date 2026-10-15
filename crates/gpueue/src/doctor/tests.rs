use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;

use gpueue_core::model::GpuIndex;
use gpueue_protocol::api::{CheckpointStorage, DetectedGpuSummary};

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
    sys.write("/etc/gpueue/config.toml", CONFIG);
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
        "/etc/systemd/system/gpueue-helper.service",
        "[Service]\nExecStart=/usr/bin/gpueue-helper serve\n",
    );
    sys.write(
        "/etc/systemd/system/gpueue.service",
        "[Service]\nExecStart=/usr/bin/gpueue daemon\n",
    );
    sys.write_exe("/usr/bin/gpueue-helper", "");
    sys.mkdir("/run/gpueue");
    let listeners = ["/run/gpueue/gpueue.sock", "/run/gpueue/helper.sock"]
        .iter()
        .map(|path| UnixListener::bind(sys.path(Path::new(path))).unwrap())
        .collect();
    sys.daemon = FakeSystem::daemon_ok(true);
    sys.mkdir("/var/lib/gpueue/checkpoints");
    sys.groups.borrow_mut().insert("gpueue-admin".to_owned());
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
    assert_eq!(message(&report, "gpueue.service"), "enabled, active");
    assert!(message(&report, "admin").contains("not in gpueue-admin"));

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
        "/etc/gpueue/config.toml",
        &format!("criu_path = \"/opt/criu/sbin/criu\"\n{CONFIG}"),
    );
    assert_eq!(level(&run(&sys, None), "criu"), Level::Fail);
    sys.write_exe("/opt/criu/sbin/criu", "");
    let report = run(&sys, None);
    assert_eq!(message(&report, "criu"), "4.2.1 (/opt/criu/sbin/criu)");
}

fn chmod(sys: &FakeSystem, path: &str, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        sys.path(Path::new(path)),
        std::fs::Permissions::from_mode(mode),
    )
    .unwrap();
}

#[test]
fn as_root_programs_others_can_replace_are_not_run() {
    let (mut sys, _sockets) = healthy();
    sys.euid = 0;
    let report = run(&sys, None);
    assert_eq!(message(&report, "criu"), "4.2.1 (/usr/sbin/criu)");
    assert_eq!(level(&report, "gpus"), Level::Info, "{report:#?}");

    // A configured path in a directory anyone can write to.
    sys.write(
        "/etc/gpueue/config.toml",
        &format!("criu_path = \"/opt/criu/sbin/criu\"\n{CONFIG}"),
    );
    sys.write_exe("/opt/criu/sbin/criu", "");
    chmod(&sys, "/opt/criu/sbin", 0o777);
    sys.log.borrow_mut().clear();
    let report = run(&sys, None);
    assert_eq!(level(&report, "criu"), Level::Fail);
    assert!(
        message(&report, "criu").starts_with("/opt/criu/sbin/criu was not run: "),
        "{}",
        message(&report, "criu")
    );
    assert!(hint(&report, "criu").contains("`criu_path`"), "{report:#?}");
    assert!(!sys.ran("criu"), "{:?}", sys.commands());

    // Unprivileged, it is not run either: the check reads metadata only, and the helper
    // refuses it whoever runs doctor.
    sys.euid = 1000;
    sys.log.borrow_mut().clear();
    let report = run(&sys, None);
    assert_eq!(level(&report, "criu"), Level::Fail);
    assert!(
        message(&report, "criu").starts_with("/opt/criu/sbin/criu was not run: "),
        "{}",
        message(&report, "criu")
    );
    assert!(!sys.ran("criu"), "{:?}", sys.commands());
    chmod(&sys, "/opt/criu/sbin", 0o755);
    let report = run(&sys, None);
    assert_eq!(message(&report, "criu"), "4.2.1 (/opt/criu/sbin/criu)");

    // A bare name resolves through systemd's PATH; the resolved file is checked.
    sys.euid = 0;
    chmod(&sys, "/usr/bin/nvidia-smi", 0o777);
    sys.log.borrow_mut().clear();
    let report = run(&sys, None);
    assert_eq!(level(&report, "gpus"), Level::Fail);
    assert!(
        message(&report, "gpus").starts_with("/usr/bin/nvidia-smi was not run"),
        "{report:#?}"
    );
    assert!(!sys.ran("nvidia-smi"), "{:?}", sys.commands());
}

#[test]
fn as_root_nothing_an_untrusted_config_names_is_run() {
    let (mut sys, _sockets) = healthy();
    sys.euid = 0;
    chmod(&sys, "/etc/gpueue/config.toml", 0o666);
    let report = run(&sys, None);
    assert_eq!(level(&report, "config"), Level::Fail);
    for name in ["criu", "cuda-checkpoint", "gpus"] {
        assert_eq!(level(&report, name), Level::Fail, "{name}");
        assert!(
            message(&report, name).contains("failed the trust check"),
            "{name}: {}",
            message(&report, name)
        );
    }
    for program in ["criu", "cuda-checkpoint", "nvidia-smi --query-gpu=index"] {
        assert!(!sys.ran(program), "{program}: {:?}", sys.commands());
    }

    // Unprivileged, doctor reports the config but still checks the programs.
    sys.euid = 1000;
    let report = run(&sys, None);
    assert_eq!(level(&report, "config"), Level::Fail);
    assert_eq!(level(&report, "criu"), Level::Ok);
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
    sys.write("/etc/gpueue/config.toml", "");
    with_detection(
        &mut sys,
        detection(GpuDetection::Auto, GpuSource::Nvml, None, &["GPU-a"]),
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "gpus"), Level::Ok, "{report:#?}");
    assert_eq!(message(&report, "gpus"), "1 GPU(s), detected with NVML");
    assert!(!report.has_failures());

    // A fallback list that matches nvidia-smi is fine too.
    sys.write("/etc/gpueue/config.toml", CONFIG);
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

    sys.write("/etc/gpueue/config.toml", "");
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
    sys.write("/etc/gpueue/config.toml", "gpu_detection = \"static\"\n");
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
    assert!(hint(&report, "gpus").contains("systemctl restart gpueue.service"));
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
        "/etc/gpueue/config.toml",
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
    sys.write("/etc/gpueue/config.toml", "");
    let report = run(&sys, None);
    assert_eq!(level(&report, "gpus"), Level::Info);
    sys.daemon = Err("connection refused".to_owned());
    assert_eq!(level(&run(&sys, None), "gpus"), Level::Info);

    // Static mode: the configuration is what the daemon uses.
    sys.write("/etc/gpueue/config.toml", "gpu_detection = \"static\"\n");
    let report = run(&sys, None);
    assert_eq!(level(&report, "gpus"), Level::Warn);
    assert!(hint(&report, "gpus").contains("[[gpus]]"));
    sys.write(
        "/etc/gpueue/config.toml",
        &format!("gpu_detection = \"static\"\n{CONFIG}"),
    );
    assert_eq!(level(&run(&sys, None), "gpus"), Level::Ok);
}

#[test]
fn config_problems() {
    let (sys, _sockets) = healthy();
    sys.write("/etc/gpueue/config.toml", "grace_secs = \"x\"\n");
    let report = run(&sys, None);
    assert_eq!(level(&report, "config"), Level::Fail);

    // Bad priority settings are reported too.
    sys.write(
        "/etc/gpueue/config.toml",
        "[priority]\ntres_billing_weights = \"mem=1\"\n",
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "config"), Level::Fail);
    let message = report.find("config").unwrap().message.clone();
    assert!(message.contains("tres_billing_weights"), "{message}");

    std::fs::remove_file(sys.path(Path::new("/etc/gpueue/config.toml"))).unwrap();
    let report = run(&sys, None);
    assert_eq!(level(&report, "config"), Level::Warn);
    assert!(hint(&report, "config").contains("sudo gpueue service install"));
}

#[test]
fn config_the_helper_would_refuse_fails() {
    let (sys, _sockets) = healthy();
    let file = sys.path(Path::new("/etc/gpueue/config.toml"));
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o664)).unwrap();
    let report = run(&sys, None);
    assert_eq!(level(&report, "config"), Level::Fail);
    assert!(message(&report, "config").contains("helper refuses"));
    assert!(hint(&report, "config").contains("chmod go-w /etc/gpueue/config.toml"));

    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let dir = sys.path(Path::new("/etc/gpueue"));
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    let report = run(&sys, None);
    assert_eq!(level(&report, "config"), Level::Fail);
    assert!(
        message(&report, "config").starts_with("/etc/gpueue must be owned by root"),
        "{}",
        message(&report, "config")
    );
    assert!(hint(&report, "config").contains("chmod go-w /etc/gpueue"));
}

/// Creates a symlink at `link` to `target` (system paths).
fn symlink(sys: &FakeSystem, target: &str, link: &str) {
    std::os::unix::fs::symlink(sys.path(Path::new(target)), sys.path(Path::new(link))).unwrap();
}

#[test]
fn config_reached_through_a_symlink_fails_like_in_the_helper() {
    let (sys, _sockets) = healthy();
    // The file the symlink points to names another criu: it must not be read.
    let config = format!("criu_path = \"/opt/criu/sbin/criu\"\n{CONFIG}");
    sys.write("/etc/gpueue-real/config.toml", &config);
    std::fs::remove_file(sys.path(Path::new("/etc/gpueue/config.toml"))).unwrap();
    symlink(
        &sys,
        "/etc/gpueue-real/config.toml",
        "/etc/gpueue/config.toml",
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "config"), Level::Fail);
    assert!(
        message(&report, "config")
            .starts_with("/etc/gpueue/config.toml is a symlink; the helper refuses to start"),
        "{}",
        message(&report, "config")
    );
    assert!(hint(&report, "config").contains("replace the symlink"));
    assert_eq!(message(&report, "criu"), "4.2.1 (/usr/sbin/criu)");
    assert_eq!(load_config(&sys).config.criu_path, Path::new("criu"));
    assert!(!load_config(&sys).trusted);

    // A symlinked directory.
    std::fs::remove_dir_all(sys.path(Path::new("/etc/gpueue"))).unwrap();
    symlink(&sys, "/etc/gpueue-real", "/etc/gpueue");
    let report = run(&sys, None);
    assert_eq!(level(&report, "config"), Level::Fail);
    assert!(
        message(&report, "config").starts_with("/etc/gpueue is a symlink"),
        "{}",
        message(&report, "config")
    );

    // A dangling symlink is no missing file: the helper refuses it too.
    std::fs::remove_file(sys.path(Path::new("/etc/gpueue"))).unwrap();
    sys.mkdir("/etc/gpueue");
    symlink(&sys, "/nowhere/config.toml", "/etc/gpueue/config.toml");
    let report = run(&sys, None);
    assert_eq!(level(&report, "config"), Level::Fail);
    assert!(
        !message(&report, "config").contains("does not exist"),
        "{}",
        message(&report, "config")
    );
    assert!(!load_config(&sys).trusted);
}

#[test]
fn an_untrusted_config_is_not_read() {
    let (sys, _sockets) = healthy();
    // Writable by others, and naming a criu that is there: doctor goes by the defaults.
    sys.write(
        "/etc/gpueue/config.toml",
        &format!("criu_path = \"/opt/criu/sbin/criu\"\n{CONFIG}"),
    );
    sys.write_exe("/opt/criu/sbin/criu", "");
    chmod(&sys, "/etc/gpueue/config.toml", 0o666);
    let loaded = load_config(&sys);
    assert_eq!(loaded.check.level, Level::Fail);
    assert!(!loaded.trusted);
    assert_eq!(loaded.config.criu_path, Path::new("criu"));
    let report = run(&sys, None);
    assert_eq!(message(&report, "criu"), "4.2.1 (/usr/sbin/criu)");

    // An unparsable file is reported as such once it is trusted.
    sys.write("/etc/gpueue/config.toml", "grace_secs = \"x\"\n");
    chmod(&sys, "/etc/gpueue/config.toml", 0o666);
    let report = run(&sys, None);
    assert!(message(&report, "config").contains("must be owned by root"));
    chmod(&sys, "/etc/gpueue/config.toml", 0o644);
    let report = run(&sys, None);
    assert!(message(&report, "config").contains("grace_secs"));
}

#[test]
fn programs_are_found_as_the_helper_finds_them() {
    let (sys, _sockets) = healthy();
    // Not executable: skipped for the next directory of the service PATH, as the helper does.
    sys.write("/usr/local/sbin/criu", "");
    let report = run(&sys, None);
    assert_eq!(message(&report, "criu"), "4.2.1 (/usr/sbin/criu)");
    assert_eq!(
        crate::system::find_program(&sys, Path::new("criu")),
        Some(PathBuf::from("/usr/sbin/criu"))
    );
    // Relative paths with a directory are refused by the helper.
    sys.write_exe("/usr/sbin/criu", "");
    assert_eq!(
        crate::system::find_program(&sys, Path::new("usr/sbin/criu")),
        None
    );
    assert_eq!(
        crate::system::find_runnable_program(&sys, Path::new("usr/sbin/criu")),
        Program::Missing
    );
}

#[test]
fn helper_settings_come_from_the_config_unless_flags_override_them() {
    let (sys, _sockets) = healthy();
    sys.write(
        "/etc/gpueue/config.toml",
        &format!("cuda_checkpoint_mode = \"plugin\"\ncheckpoints_dir = \"/ckpt\"\n{CONFIG}"),
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "helper settings"), Level::Info);
    let shown = message(&report, "helper settings");
    assert!(shown.contains("cuda mode plugin"), "{shown}");
    assert!(shown.contains("checkpoints /ckpt"), "{shown}");

    sys.write(
        "/etc/systemd/system/gpueue-helper.service",
        "[Service]\nExecStart=/usr/bin/gpueue-helper serve --criu /opt/criu --cuda-mode=manual\n",
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "helper settings"), Level::Warn);
    assert!(message(&report, "helper settings").contains("--criu --cuda-mode"));
}

#[test]
fn job_system_dbus_is_shown() {
    let (sys, _sockets) = healthy();
    let report = run(&sys, None);
    assert_eq!(level(&report, "job d-bus"), Level::Ok);
    assert!(message(&report, "job d-bus").starts_with("blocked: "));

    sys.write(
        "/etc/gpueue/config.toml",
        &format!("job_system_dbus = true\n{CONFIG}"),
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "job d-bus"), Level::Warn);
    assert!(hint(&report, "job d-bus").contains("job_system_dbus = true"));

    sys.write(
        "/etc/gpueue/config.toml",
        &format!("job_systemd_varlink = true\n{CONFIG}"),
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "job d-bus"), Level::Warn);
    assert!(message(&report, "job d-bus").contains("job_systemd_varlink = true"));
}

#[test]
fn hidden_daemon_sockets_are_shown() {
    let (sys, _sockets) = healthy();
    let report = run(&sys, None);
    assert_eq!(level(&report, "job sockets"), Level::Ok);
    assert!(message(&report, "job sockets").contains("/run/libvirt, /run/cups"));

    // A default left out.
    sys.write(
        "/etc/gpueue/config.toml",
        &format!("job_hidden_socket_dirs = [\"/run/cups\"]\n{CONFIG}"),
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "job sockets"), Level::Warn);
    assert!(message(&report, "job sockets").starts_with("/run/libvirt not hidden"));
    assert!(hint(&report, "job sockets").contains(r#"["/run/cups", "/run/libvirt"]"#));

    // An entry that is a socket, not a directory.
    sys.write(
        "/etc/gpueue/config.toml",
        &format!(
            "job_hidden_socket_dirs = [\"/run/libvirt\", \"/run/cups\", \"/run/x/sock\"]\n{CONFIG}"
        ),
    );
    sys.write("/run/x/sock", "");
    let report = run(&sys, None);
    assert_eq!(level(&report, "job sockets"), Level::Warn);
    assert!(message(&report, "job sockets").contains("/run/x/sock not a directory"));

    // nix-daemon trusting a group.
    sys.write("/etc/gpueue/config.toml", CONFIG);
    sys.write(
        "/etc/nix/nix.conf",
        "trusted-users = root\nextra-trusted-users = @wheel # admins\n",
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "job sockets"), Level::Warn);
    assert!(message(&report, "job sockets").contains("nix-daemon trusts @wheel"));
    assert!(hint(&report, "job sockets").contains("/nix/var/nix/daemon-socket"));
    sys.write(
        "/etc/gpueue/config.toml",
        &format!(
            "job_hidden_socket_dirs = [\"/run/libvirt\", \"/run/cups\", \"/nix/var/nix/daemon-socket\"]\n{CONFIG}"
        ),
    );
    assert_eq!(level(&run(&sys, None), "job sockets"), Level::Ok);
    sys.write("/etc/gpueue/config.toml", CONFIG);
    sys.write("/etc/nix/nix.conf", "trusted-users = root\n");
    assert_eq!(level(&run(&sys, None), "job sockets"), Level::Ok);
}

#[test]
fn nix_trusted_users_are_parsed() {
    use super::nix_trusted_non_root;
    assert!(nix_trusted_non_root("").is_empty());
    assert!(nix_trusted_non_root("trusted-users = root\n").is_empty());
    assert_eq!(
        nix_trusted_non_root("trusted-users = root @wheel alice\n"),
        ["@wheel", "alice"]
    );
    // The last setting wins; extra- adds.
    assert_eq!(
        nix_trusted_non_root(
            "trusted-users = @wheel\ntrusted-users = root\nextra-trusted-users = bob\n"
        ),
        ["bob"]
    );
    assert!(nix_trusted_non_root("# trusted-users = @wheel\n").is_empty());
}

#[test]
fn gpu_isolation_is_shown() {
    let (sys, _sockets) = healthy();
    let report = run(&sys, None);
    assert_eq!(level(&report, "gpu isolation"), Level::Ok);
    assert!(message(&report, "gpu isolation").starts_with("on: "));

    sys.write(
        "/etc/gpueue/config.toml",
        &format!("gpu_device_isolation = false\n{CONFIG}"),
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "gpu isolation"), Level::Info);
    assert!(message(&report, "gpu isolation").starts_with("off "));
    assert!(hint(&report, "gpu isolation").contains("gpu_device_isolation = true"));
}

#[test]
fn job_policy_shows_the_minimum_uid_source_and_denied_groups() {
    let (sys, _sockets) = healthy();
    let report = run(&sys, None);
    assert_eq!(level(&report, "job policy"), Level::Info);
    let shown = message(&report, "job policy");
    assert!(shown.contains("uid >= 1000 (built-in fallback)"), "{shown}");
    assert!(shown.contains("docker (absent)"), "{shown}");
    assert!(shown.contains("allowed users: every user"), "{shown}");

    sys.write("/etc/login.defs", "# UID_MIN 1\nUID_MIN 2000\n");
    sys.write("/etc/group", "docker:x:999:alice\nsudo:x:27:\n");
    let report = run(&sys, None);
    let shown = message(&report, "job policy");
    assert!(
        shown.contains("uid >= 2000 (UID_MIN in /etc/login.defs)"),
        "{shown}"
    );
    assert!(shown.contains("docker (gid 999)"), "{shown}");
    assert!(shown.contains("sudo (gid 27)"), "{shown}");
    assert!(shown.contains("wheel (absent)"), "{shown}");

    sys.write(
        "/etc/gpueue/config.toml",
        &format!(
            "job_min_uid = 1500\njob_denied_groups = []\njob_allowed_users = [\"alice\"]\n{CONFIG}"
        ),
    );
    let shown = message(&run(&sys, None), "job policy").to_owned();
    assert_eq!(
        shown,
        "jobs run as uid >= 1500 (job_min_uid in the configuration); denied groups: none; \
         allowed users: alice"
    );
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
    assert_eq!(level(&report, "gpueue.service"), Level::Fail);
}

#[test]
fn unit_states() {
    let (sys, _sockets) = healthy();
    sys.respond(
        "systemctl is-enabled gpueue.service",
        CmdOutput::failed("not-found\n", ""),
    );
    sys.respond(
        "systemctl is-enabled gpueue-helper.service",
        CmdOutput::failed("disabled\n", ""),
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "gpueue.service"), Level::Fail);
    assert_eq!(
        hint(&report, "gpueue.service"),
        "sudo gpueue service install"
    );
    assert_eq!(
        hint(&report, "gpueue-helper.service"),
        "sudo systemctl enable --now gpueue-helper.service"
    );

    let (sys, _sockets) = healthy();
    sys.respond(
        "systemctl is-active gpueue-helper.service",
        CmdOutput::failed("failed\n", ""),
    );
    let report = run(&sys, None);
    assert_eq!(message(&report, "gpueue-helper.service"), "enabled, failed");
    assert!(hint(&report, "gpueue-helper.service").contains("journalctl -u gpueue-helper.service"));
}

#[test]
fn helper_binary_must_be_protected() {
    let (sys, _sockets) = healthy();
    let exe = sys.path(Path::new("/usr/bin/gpueue-helper"));
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o775)).unwrap();
    let report = run(&sys, None);
    assert_eq!(level(&report, "helper binary"), Level::Fail);
    assert!(hint(&report, "helper binary").contains("chmod 755 /usr/bin/gpueue-helper"));

    // The unit file systemd reports wins over /etc/systemd/system.
    sys.write(
        "/usr/lib/systemd/system/gpueue-helper.service",
        "[Service]\nExecStart=/usr/local/bin/gpueue-helper serve\n",
    );
    sys.write_exe("/usr/local/bin/gpueue-helper", "");
    sys.respond(
        "systemctl show --property=FragmentPath --value gpueue-helper.service",
        CmdOutput::ok("/usr/lib/systemd/system/gpueue-helper.service\n"),
    );
    let report = run(&sys, None);
    assert_eq!(
        message(&report, "helper binary"),
        "/usr/local/bin/gpueue-helper"
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
    std::fs::remove_file(sys.path(Path::new("/run/gpueue/helper.sock"))).unwrap();
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
fn helper_missing_programs() {
    let (mut sys, _sockets) = healthy();
    assert!(run(&sys, None).find("helper programs").is_none());

    // cuda-checkpoint was installed after the helper started; nvidia-smi
    // is still missing, which its own check reports.
    let mut version = FakeSystem::daemon_ok(true).unwrap();
    version.helper.missing_programs = vec!["cuda-checkpoint".to_owned(), "nvidia-smi".to_owned()];
    sys.daemon = Ok(version);
    std::fs::remove_file(sys.path(Path::new("/usr/bin/nvidia-smi"))).ok();
    let report = run(&sys, None);
    // GPU jobs cannot be checkpointed on a host with a driver: as for a missing cuda-checkpoint.
    assert_eq!(level(&report, "helper programs"), Level::Fail);
    assert_eq!(
        message(&report, "helper programs"),
        "cuda-checkpoint (/usr/local/bin/cuda-checkpoint) was installed after the helper \
         started; the helper cannot use it until it is restarted"
    );
    assert_eq!(
        hint(&report, "helper programs"),
        "sudo systemctl restart gpueue-helper.service"
    );
    assert_eq!(level(&report, "cuda-checkpoint"), Level::Ok);

    // Without a driver, only a warning.
    std::fs::remove_file(sys.path(Path::new("/proc/driver/nvidia/version"))).unwrap();
    assert_eq!(level(&run(&sys, None), "helper programs"), Level::Warn);

    // Still missing: only its own check.
    std::fs::remove_file(sys.path(Path::new("/usr/local/bin/cuda-checkpoint"))).unwrap();
    assert!(run(&sys, None).find("helper programs").is_none());
}

#[test]
fn helper_missing_programs_are_matched_by_their_configured_name() {
    let (mut sys, _sockets) = healthy();
    sys.write(
        "/etc/gpueue/config.toml",
        &format!("cuda_checkpoint_path = \"cuda-checkpoint-r570\"\n{CONFIG}"),
    );
    sys.write_exe("/usr/local/bin/cuda-checkpoint-r570", "");
    let mut version = FakeSystem::daemon_ok(true).unwrap();
    version.helper.missing_programs = vec!["cuda-checkpoint-r570".to_owned()];
    sys.daemon = Ok(version);
    let report = run(&sys, None);
    assert_eq!(level(&report, "helper programs"), Level::Fail);
    assert!(
        message(&report, "helper programs").starts_with(
            "cuda-checkpoint-r570 (/usr/local/bin/cuda-checkpoint-r570) was installed after"
        ),
        "{}",
        message(&report, "helper programs")
    );
}

#[test]
fn a_helper_program_installed_later_that_others_can_replace_is_not_a_restart() {
    let (mut sys, _sockets) = healthy();
    let mut version = FakeSystem::daemon_ok(true).unwrap();
    version.helper.missing_programs = vec!["criu".to_owned()];
    sys.daemon = Ok(version);
    // A user-owned criu: a restarted helper would refuse it and exit.
    chmod(&sys, "/usr/sbin", 0o777);
    let report = run(&sys, None);
    assert_eq!(level(&report, "helper programs"), Level::Fail);
    let message = message(&report, "helper programs");
    assert!(
        message.starts_with("/usr/sbin/criu was installed after the helper started, but /usr/sbin"),
        "{message}"
    );
    assert!(message.contains("would refuse it and exit"), "{message}");
    let hint = hint(&report, "helper programs");
    assert!(hint.contains("sudo chmod go-w /usr/sbin/criu"), "{hint}");
    assert!(!hint.contains("systemctl restart"), "{hint}");
    chmod(&sys, "/usr/sbin", 0o755);
}

/// The helper's status with `programs` reported as resolved at its start.
fn helper_uses(sys: &mut FakeSystem, programs: &[(&str, &str)]) {
    let mut version = FakeSystem::daemon_ok(true).unwrap();
    version.helper.programs = programs
        .iter()
        .map(|(name, path)| gpueue_protocol::helper::HelperProgram {
            name: (*name).to_owned(),
            path: PathBuf::from(path),
        })
        .collect();
    sys.daemon = Ok(version);
}

#[test]
fn helper_programs_removed_since_it_started() {
    let (mut sys, _sockets) = healthy();
    // The same paths as the checks above: nothing to add.
    helper_uses(
        &mut sys,
        &[
            ("criu", "/usr/sbin/criu"),
            ("cuda-checkpoint", "/usr/local/bin/cuda-checkpoint"),
            ("nvidia-smi", "/usr/bin/nvidia-smi"),
        ],
    );
    assert!(run(&sys, None).find("helper programs").is_none());

    // criu moved from /usr/local/sbin to /usr/sbin after the helper started.
    helper_uses(
        &mut sys,
        &[
            ("criu", "/usr/local/sbin/criu"),
            ("nvidia-smi", "/opt/old/nvidia-smi"),
        ],
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "criu"), Level::Ok);
    assert_eq!(level(&report, "helper programs"), Level::Fail);
    assert_eq!(
        message(&report, "helper programs"),
        "the helper runs criu from /usr/local/sbin/criu, which is gone; it finds \
         /usr/sbin/criu only once it is restarted; the helper runs nvidia-smi from \
         /opt/old/nvidia-smi, which is gone; it finds /usr/bin/nvidia-smi only once it is \
         restarted"
    );
    assert_eq!(
        hint(&report, "helper programs"),
        "sudo systemctl restart gpueue-helper.service"
    );

    // nvidia-smi alone is a warning.
    helper_uses(&mut sys, &[("nvidia-smi", "/opt/old/nvidia-smi")]);
    assert_eq!(level(&run(&sys, None), "helper programs"), Level::Warn);

    // Gone with no other copy: the program's own check reports it.
    helper_uses(&mut sys, &[("criu", "/usr/local/sbin/criu")]);
    std::fs::remove_file(sys.path(Path::new("/usr/sbin/criu"))).unwrap();
    let report = run(&sys, None);
    assert_eq!(level(&report, "criu"), Level::Fail);
    assert!(report.find("helper programs").is_none());
}

#[test]
fn a_helper_program_others_can_now_replace_fails() {
    let (mut sys, _sockets) = healthy();
    // The helper found criu in /usr/local/sbin, which is now writable by others; doctor itself
    // finds the protected /usr/sbin/criu first only because that one is executable.
    sys.write("/usr/local/sbin/criu", "");
    chmod(&sys, "/usr/local/sbin/criu", 0o644);
    chmod(&sys, "/usr/local/sbin", 0o777);
    helper_uses(&mut sys, &[("criu", "/usr/local/sbin/criu")]);
    let report = run(&sys, None);
    assert_eq!(level(&report, "criu"), Level::Ok);
    assert_eq!(level(&report, "helper programs"), Level::Fail);
    let message = message(&report, "helper programs");
    assert!(
        message.starts_with("the helper runs criu from /usr/local/sbin/criu, but /usr/local/sbin"),
        "{message}"
    );
    assert!(
        hint(&report, "helper programs").contains("sudo chmod go-w /usr/local/sbin/criu"),
        "{report:#?}"
    );
    chmod(&sys, "/usr/local/sbin", 0o755);
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

    std::fs::remove_dir(sys.path(Path::new("/var/lib/gpueue/checkpoints"))).unwrap();
    let report = run(&sys, None);
    assert_eq!(level(&report, "checkpoint dir"), Level::Fail);
    assert!(hint(&report, "checkpoint dir").contains("systemd-tmpfiles --create"));
}

#[test]
fn checkpoint_size_limit_raises_the_space_threshold() {
    let (mut sys, _sockets) = healthy();
    sys.write(
        "/etc/gpueue/config.toml",
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
    assert!(hint(&report, "checkpoint usage").contains("gpueue status"));
    assert!(hint(&report, "checkpoint usage").contains("gpueue remove"));
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
    sys.member_of.insert("gpueue-admin".to_owned());
    let report = run(&sys, None);
    assert_eq!(level(&report, "admin"), Level::Info);
    assert!(message(&report, "admin").contains("administrator"));

    sys.groups.borrow_mut().clear();
    assert_eq!(level(&run(&sys, None), "admin"), Level::Warn);

    sys.euid = 0;
    assert_eq!(level(&run(&sys, None), "admin"), Level::Warn);
    sys.groups.borrow_mut().insert("gpueue-admin".to_owned());
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
        "/etc/gpueue/config.toml",
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
    sys.write_exe("/opt/gpueue/bin/gpueue-helper", "");
    sys.respond(
        "systemctl show --property=ExecStart --value gpueue-helper.service",
        CmdOutput::ok(
            "{ path=/opt/gpueue/bin/gpueue-helper ; argv[]=/opt/gpueue/bin/gpueue-helper serve \
             --criu /opt/criu ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; \
             pid=0 ; code=(null) ; status=0/0 }\n",
        ),
    );
    let report = run(&sys, None);
    assert_eq!(
        message(&report, "helper binary"),
        "/opt/gpueue/bin/gpueue-helper"
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
    sys.write_exe("/opt/shared/bin/gpueue-helper", "");
    sys.write(
        "/etc/systemd/system/gpueue-helper.service",
        "[Service]\nExecStart=/opt/shared/bin/gpueue-helper serve\n",
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
    let file = sys.path(Path::new("/etc/gpueue/config.toml"));
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let report = run(&sys, None);
    assert_eq!(level(&report, "config"), Level::Warn);
    assert!(message(&report, "config").contains("not readable by others"));

    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let dir = sys.path(Path::new("/etc/gpueue"));
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let report = run(&sys, None);
    assert_eq!(level(&report, "config"), Level::Warn);
    assert!(message(&report, "config").contains("/etc/gpueue is not searchable"));
    assert!(hint(&report, "config").contains("chmod 755 /etc/gpueue"));
}

#[test]
fn daemon_paths_outside_read_write_paths_warn() {
    let (sys, _sockets) = healthy();
    // Not reported by systemd: no check.
    assert!(run(&sys, None).find("daemon write paths").is_none());

    sys.respond(
        "systemctl show --property=ProtectSystem --property=ReadWritePaths gpueue.service",
        CmdOutput::ok(
            "ProtectSystem=strict\nReadWritePaths=/var/lib/gpueue/daemon /var/lib/gpueue/logs \
             -/run/gpueue\n",
        ),
    );
    assert_eq!(level(&run(&sys, None), "daemon write paths"), Level::Ok);

    sys.write(
        "/etc/gpueue/config.toml",
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
        "systemctl show --property=ProtectSystem --property=ReadWritePaths gpueue.service",
        CmdOutput::ok("ProtectSystem=no\nReadWritePaths=\n"),
    );
    assert!(run(&sys, None).find("daemon write paths").is_none());
}

fn marker_unit(bin_dir: &str) -> String {
    format!(
        "{}\n[Service]\nExecStart={bin_dir}/gpueue daemon\n",
        crate::service::MARKER
    )
}

fn write_both(sys: &FakeSystem, dir: &str, contents: &str) {
    for unit in ["gpueue.service", "gpueue-helper.service"] {
        sys.write(&format!("{dir}/{unit}"), contents);
    }
}

/// Both units from the dpkg package `gpueue`.
fn install_package(sys: &FakeSystem) {
    for unit in ["gpueue.service", "gpueue-helper.service"] {
        let unit = format!("/usr/lib/systemd/system/{unit}");
        sys.write(&unit, "[Service]\nExecStart=/usr/bin/gpueue daemon\n");
        sys.dpkg_owns(&unit, "gpueue");
    }
}

#[test]
fn install_method_is_reported() {
    let (sys, _sockets) = healthy();
    for unit in ["gpueue.service", "gpueue-helper.service"] {
        std::fs::remove_file(sys.path(&Path::new("/etc/systemd/system").join(unit))).unwrap();
    }
    assert_eq!(message(&run(&sys, None), "install method"), "not installed");

    write_both(
        &sys,
        "/etc/systemd/system",
        "[Service]\nExecStart=/usr/bin/gpueue daemon\n",
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
        "gpueue service install (binaries in /usr/local/bin)"
    );
}

#[test]
fn rhel_sudo_secure_path_without_usr_local_bin_is_explained() {
    let (sys, _sockets) = healthy();
    sys.write(
        "/etc/systemd/system/gpueue.service",
        &marker_unit("/usr/local/bin"),
    );
    sys.write(
        "/etc/os-release",
        "NAME=\"Rocky Linux\"\nID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\n",
    );
    // Not readable (doctor without root): nothing to report.
    assert!(run(&sys, None).find("sudo PATH").is_none());

    sys.write(
        "/etc/sudoers",
        "Defaults   !visiblepw\n\
         Defaults    secure_path = /sbin:/bin:/usr/sbin:/usr/bin\n\
         Defaults:alice secure_path=/usr/local/bin\n\
         #includedir /etc/sudoers.d\n",
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "sudo PATH"), Level::Info);
    assert!(
        message(&report, "sudo PATH").contains("(in /etc/sudoers) does not include /usr/local/bin"),
        "{}",
        message(&report, "sudo PATH")
    );
    assert!(hint(&report, "sudo PATH").contains("sudo /usr/local/bin/gpueue"));

    // A drop-in that adds it (sudoers.d files with a dot are ignored, as by sudo).
    sys.write(
        "/etc/sudoers.d/ignored.conf",
        "Defaults secure_path=\"/usr/local/bin:/usr/bin\"\n",
    );
    assert!(run(&sys, None).find("sudo PATH").is_some());
    sys.write(
        "/etc/sudoers.d/path",
        "Defaults env_reset, secure_path=\"/usr/local/sbin:/usr/local/bin:/usr/bin\"\n",
    );
    assert!(run(&sys, None).find("sudo PATH").is_none());

    // Not RHEL, or not in /usr/local/bin: nothing to report.
    std::fs::remove_file(sys.path(Path::new("/etc/sudoers.d/path"))).unwrap();
    sys.write("/etc/os-release", "ID=ubuntu\nID_LIKE=debian\n");
    assert!(run(&sys, None).find("sudo PATH").is_none());
    sys.write("/etc/os-release", "ID=fedora\n");
    assert!(run(&sys, None).find("sudo PATH").is_some());
    sys.write(
        "/etc/systemd/system/gpueue.service",
        &marker_unit("/usr/bin"),
    );
    assert!(run(&sys, None).find("sudo PATH").is_none());
}

/// A Rocky Linux machine with the binaries in /usr/local/bin and `sudoers` as /etc/sudoers.
fn rocky_with_sudoers(sudoers: &str) -> (FakeSystem, Vec<UnixListener>) {
    let (sys, sockets) = healthy();
    sys.write(
        "/etc/systemd/system/gpueue.service",
        &marker_unit("/usr/local/bin"),
    );
    sys.write(
        "/etc/os-release",
        "ID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\n",
    );
    sys.write("/etc/sudoers", sudoers);
    (sys, sockets)
}

fn sudo_path_reported(sys: &FakeSystem) -> bool {
    run(sys, None).find("sudo PATH").is_some()
}

#[test]
fn sudoers_are_read_in_sudos_order() {
    // An include directory in the middle: the main file's later line wins.
    let (sys, _sockets) = rocky_with_sudoers(
        "@includedir /etc/sudoers.d\nDefaults secure_path=/sbin:/bin:/usr/sbin:/usr/bin\n",
    );
    sys.write(
        "/etc/sudoers.d/path",
        "Defaults secure_path=/usr/local/bin:/usr/bin\n",
    );
    assert!(sudo_path_reported(&sys));
    let report = run(&sys, None);
    assert!(message(&report, "sudo PATH").contains("(in /etc/sudoers)"));

    // At the end, the drop-in wins.
    sys.write(
        "/etc/sudoers",
        "Defaults secure_path=/sbin:/bin:/usr/sbin:/usr/bin\n#includedir /etc/sudoers.d\n",
    );
    assert!(!sudo_path_reported(&sys));

    // `@include` of a file (relative to the including file), at its position.
    sys.write(
        "/etc/sudoers",
        "Defaults secure_path=/usr/local/bin:/usr/bin\n@include sudoers.local\n",
    );
    sys.write("/etc/sudoers.local", "Defaults secure_path=/usr/bin\n");
    let report = run(&sys, None);
    assert!(
        message(&report, "sudo PATH").contains("(in /etc/sudoers.local)"),
        "{}",
        message(&report, "sudo PATH")
    );

    // `!secure_path` unsets it: sudo keeps the caller's PATH.
    sys.write(
        "/etc/sudoers",
        "Defaults secure_path=/usr/bin\nDefaults !secure_path\n",
    );
    assert!(!sudo_path_reported(&sys));
}

#[test]
fn sudoers_continuations_and_comments() {
    let (sys, _sockets) = rocky_with_sudoers(
        "Defaults env_reset, \\\n    secure_path = /sbin:/bin:\\\n/usr/local/bin:/usr/bin\n",
    );
    assert!(!sudo_path_reported(&sys));

    sys.write(
        "/etc/sudoers",
        "Defaults secure_path=\"/usr/local/bin:/usr/bin\" # the usual\n",
    );
    assert!(!sudo_path_reported(&sys));

    // The comment does not count.
    sys.write(
        "/etc/sudoers",
        "Defaults secure_path=/usr/bin # /usr/local/bin\n",
    );
    assert!(sudo_path_reported(&sys));
}

#[test]
fn sudoers_that_cannot_be_read_with_confidence_skip_the_check() {
    for sudoers in [
        "Defaults secure_path=/usr/bin\n@include /etc/sudoers.%h\n",
        "Defaults secure_path=/usr/bin\n@include /etc/missing\n",
        "Defaults secure_path+=/usr/bin\n",
        "Defaults secure_path=\"/usr/bin\n",
        "Defaults secure_path\n",
    ] {
        let (sys, _sockets) = rocky_with_sudoers(sudoers);
        assert!(!sudo_path_reported(&sys), "{sudoers:?}");
    }
    // An include loop.
    let (sys, _sockets) =
        rocky_with_sudoers("Defaults secure_path=/usr/bin\n@include /etc/sudoers\n");
    assert!(!sudo_path_reported(&sys));
}

#[test]
fn a_masked_or_overridden_package_does_not_fail() {
    let (sys, _sockets) = healthy();
    install_package(&sys);
    sys.write(
        "/etc/systemd/system/gpueue.service",
        "[Service]\nExecStart=/usr/bin/gpueue daemon\n",
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "install method"), Level::Ok);
    assert!(
        message(&report, "install method").contains("overridden by"),
        "{}",
        message(&report, "install method")
    );

    let etc = sys.path(Path::new("/etc/systemd/system/gpueue.service"));
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
    assert!(hint.contains("sudo gpueue uninstall"), "{hint}");
    assert!(hint.contains("sudo apt remove gpueue"), "{hint}");
}

fn remove_etc_units(sys: &FakeSystem) {
    for unit in ["gpueue.service", "gpueue-helper.service"] {
        let _ = std::fs::remove_file(sys.path(&Path::new("/etc/systemd/system").join(unit)));
    }
}

fn not_owned(sys: &FakeSystem) {
    sys.respond(
        "dpkg-query -S",
        CmdOutput::failed("", "dpkg-query: no path found matching pattern\n"),
    );
    // A Debian system: no other package manager could own it.
    sys.missing("rpm");
    sys.missing("pacman");
}

#[test]
fn an_unowned_unit_warns() {
    let (sys, _sockets) = healthy();
    remove_etc_units(&sys);
    write_both(
        &sys,
        "/usr/lib/systemd/system",
        "[Service]\nExecStart=/usr/bin/gpueue daemon\n",
    );
    not_owned(&sys);
    let report = run(&sys, None);
    assert_eq!(level(&report, "install method"), Level::Warn);
    assert!(
        message(&report, "install method").starts_with("a unit no package owns"),
        "{}",
        message(&report, "install method")
    );
    assert!(hint(&report, "install method").contains("sudo rm /usr/lib/systemd/system/gpueue"));
}

#[test]
fn a_leftover_next_to_service_install_warns() {
    let (sys, _sockets) = healthy();
    write_both(&sys, "/etc/systemd/system", &marker_unit("/usr/local/bin"));
    write_both(
        &sys,
        "/usr/lib/systemd/system",
        "[Service]\nExecStart=/usr/bin/gpueue daemon\n",
    );
    not_owned(&sys);
    let report = run(&sys, None);
    assert_eq!(level(&report, "install method"), Level::Warn);
    let shown = message(&report, "install method");
    assert!(shown.starts_with("gpueue service install"), "{shown}");
    assert!(shown.contains("no package owns"), "{shown}");
    assert!(hint(&report, "install method").contains("remove the leftover /usr/lib unit"));
}

#[test]
fn units_installed_in_different_ways_warn() {
    let (sys, _sockets) = healthy();
    remove_etc_units(&sys);
    sys.write(
        "/etc/systemd/system/gpueue.service",
        &marker_unit("/usr/local/bin"),
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "install method"), Level::Warn);
    assert!(
        message(&report, "install method").starts_with("gpueue-helper.service is missing"),
        "{}",
        message(&report, "install method")
    );
    assert!(hint(&report, "install method").contains("sudo gpueue service install"));

    // A packaged helper unit next to it conflicts: FAIL, like a mixed setup.
    let unit = "/usr/lib/systemd/system/gpueue-helper.service";
    sys.write(unit, "[Service]\nExecStart=/usr/bin/gpueue-helper serve\n");
    sys.dpkg_owns(unit, "gpueue");
    let report = run(&sys, None);
    assert_eq!(level(&report, "install method"), Level::Fail);
    assert!(message(&report, "install method").contains("conflict"));
    let hint = hint(&report, "install method");
    assert!(hint.contains("sudo gpueue uninstall"), "{hint}");
    assert!(hint.contains("sudo apt remove gpueue"), "{hint}");
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
        "not installed (NixOS: use the services.gpueue module)"
    );

    for unit in ["gpueue.service", "gpueue-helper.service"] {
        let link = sys.path(&Path::new("/etc/systemd/system").join(unit));
        std::os::unix::fs::symlink(format!("/nix/store/abc-{unit}"), link).unwrap();
    }
    let report = run(&sys, None);
    assert_eq!(level(&report, "install method"), Level::Ok);
    assert!(message(&report, "install method").starts_with("Nix ("));
}

/// A healthy installation with `os_release`, and problems in checks whose hint differs on
/// NixOS: CRIU, cuda-checkpoint and nvidia-smi missing, the daemon unit not installed and the
/// helper's disabled, a configured directory outside `ReadWritePaths=`, no checkpoint directory.
fn broken_on(os_release: &str) -> (FakeSystem, Vec<UnixListener>) {
    let (sys, sockets) = healthy();
    sys.write("/etc/os-release", os_release);
    sys.write(
        "/etc/gpueue/config.toml",
        &format!(
            "criu_path = \"/missing/criu\"\n\
             cuda_checkpoint_path = \"/missing/cuda-checkpoint\"\n\
             nvidia_smi_path = \"/missing/nvidia-smi\"\n\
             logs_dir = \"/srv/logs\"\n{CONFIG}"
        ),
    );
    sys.respond(
        "systemctl is-enabled gpueue.service",
        CmdOutput::failed("not-found\n", ""),
    );
    sys.respond(
        "systemctl is-enabled gpueue-helper.service",
        CmdOutput::failed("disabled\n", ""),
    );
    sys.respond(
        "systemctl show --property=ProtectSystem --property=ReadWritePaths gpueue.service",
        CmdOutput::ok(
            "ProtectSystem=strict\nReadWritePaths=/var/lib/gpueue/daemon /var/lib/gpueue/logs \
             -/run/gpueue\n",
        ),
    );
    std::fs::remove_dir(sys.path(Path::new("/var/lib/gpueue/checkpoints"))).unwrap();
    (sys, sockets)
}

#[test]
fn hints_outside_nixos_are_unchanged() {
    for os_release in ["ID=ubuntu\nID_LIKE=debian\n", "ID=fedora\n", ""] {
        let (sys, _sockets) = broken_on(os_release);
        let report = run(&sys, None);
        assert_eq!(
            hint(&report, "criu"),
            "install CRIU 4.2 or newer (https://criu.org/Installation), or set `criu_path` in \
             /etc/gpueue/config.toml"
        );
        assert_eq!(
            hint(&report, "cuda-checkpoint"),
            "install cuda-checkpoint matching the driver \
             (https://github.com/NVIDIA/cuda-checkpoint) into /usr/local/bin, or set \
             `cuda_checkpoint_path` in /etc/gpueue/config.toml"
        );
        assert_eq!(
            hint(&report, "gpus"),
            "install nvidia-smi (part of the NVIDIA driver utilities)"
        );
        assert_eq!(
            hint(&report, "gpueue.service"),
            "sudo gpueue service install"
        );
        assert_eq!(
            hint(&report, "gpueue-helper.service"),
            "sudo systemctl enable --now gpueue-helper.service"
        );
        assert_eq!(
            hint(&report, "daemon write paths"),
            "add a drop-in with `sudo systemctl edit gpueue.service`:\n[Service]\n\
             ReadWritePaths=/srv/logs\nthen `sudo systemctl restart gpueue.service`"
        );
        assert_eq!(
            hint(&report, "checkpoint dir"),
            "sudo systemd-tmpfiles --create /etc/tmpfiles.d/gpueue.conf"
        );
        assert!(message(&report, "admin").contains("sudo usermod -aG gpueue-admin"));
        assert!(hint(&report, "install method").contains("--force"));
    }
}

#[test]
fn nixos_hints_name_the_module_options() {
    let (sys, _sockets) = broken_on("NAME=NixOS\nID=nixos\n");
    let report = run(&sys, None);
    for name in [
        "criu",
        "cuda-checkpoint",
        "gpus",
        "gpueue.service",
        "gpueue-helper.service",
        "daemon write paths",
        "checkpoint dir",
        "install method",
    ] {
        let hint = hint(&report, name);
        assert!(!hint.is_empty(), "{name}: no hint");
        for wrong in [
            "/usr/local/bin",
            "service install",
            "systemctl edit",
            "systemctl enable",
            "/etc/tmpfiles.d",
            "in /etc/gpueue/config.toml",
        ] {
            assert!(!hint.contains(wrong), "{name}: {hint}");
        }
    }
    let has = |name: &str, text: &str| {
        let hint = hint(&report, name);
        assert!(hint.contains(text), "{name}: {hint}");
    };
    has("criu", "services.gpueue.criuPackage");
    has("criu", "sudo nixos-rebuild switch");
    has("cuda-checkpoint", "services.gpueue.cudaCheckpointPackage");
    has("gpus", "services.gpueue.nvidiaSmiPath");
    has("gpueue.service", "services.gpueue.enable = true");
    has("gpueue-helper.service", "services.gpueue.enable = true");
    has(
        "daemon write paths",
        "systemd.services.gpueue.serviceConfig.ReadWritePaths = [ \"/srv/logs\" ];",
    );
    has("checkpoint dir", "sudo systemd-tmpfiles --create ");
    // The units in /etc/systemd/system run binaries outside the store: by hand.
    has("install method", "services.gpueue.enable = true");
    assert!(message(&report, "admin").contains("services.gpueue.admins"));
}

#[test]
fn nixos_config_hints_name_the_settings() {
    let nixos = "ID=nixos\n";
    // Missing.
    let (sys, _sockets) = healthy();
    sys.write("/etc/os-release", nixos);
    std::fs::remove_file(sys.path(Path::new("/etc/gpueue/config.toml"))).unwrap();
    let report = run(&sys, None);
    assert!(hint(&report, "config").contains("services.gpueue.enable = true"));
    assert!(!hint(&report, "config").contains("service install"));
    sys.write("/etc/os-release", "ID=debian\n");
    assert_eq!(
        hint(&run(&sys, None), "config"),
        "sudo gpueue service install   (creates a commented default configuration)"
    );

    // Invalid.
    sys.write("/etc/gpueue/config.toml", "grace_secs = \"x\"\n");
    assert_eq!(
        hint(&run(&sys, None), "config"),
        "fix /etc/gpueue/config.toml; the daemon does not start with it"
    );
    sys.write("/etc/os-release", nixos);
    let report = run(&sys, None);
    assert_eq!(level(&report, "config"), Level::Fail);
    assert!(
        hint(&report, "config").starts_with("fix `services.gpueue.settings`"),
        "{}",
        hint(&report, "config")
    );
    // `load_config` (used by other commands) reads os-release itself.
    let loaded = load_config(&sys).check.hint.unwrap_or_default();
    assert!(loaded.contains("services.gpueue.settings"), "{loaded}");

    // Writable by others: rebuilding rewrites it.
    sys.write("/etc/gpueue/config.toml", CONFIG);
    let file = sys.path(Path::new("/etc/gpueue/config.toml"));
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o664)).unwrap();
    let report = run(&sys, None);
    assert_eq!(level(&report, "config"), Level::Fail);
    assert!(
        hint(&report, "config").starts_with("`sudo nixos-rebuild switch` rewrites"),
        "{}",
        hint(&report, "config")
    );
    sys.write("/etc/os-release", "ID=debian\n");
    assert_eq!(
        hint(&run(&sys, None), "config"),
        "sudo chown root:root /etc/gpueue/config.toml; sudo chmod go-w /etc/gpueue/config.toml"
    );
}

#[test]
fn nixos_helper_flags_hint_names_the_settings() {
    let (sys, _sockets) = healthy();
    sys.write("/etc/os-release", "ID=nixos\n");
    sys.write(
        "/etc/systemd/system/gpueue-helper.service",
        "[Service]\nExecStart=/usr/bin/gpueue-helper serve --criu /opt/criu\n",
    );
    let report = run(&sys, None);
    assert_eq!(level(&report, "helper settings"), Level::Warn);
    let shown = hint(&report, "helper settings");
    assert!(shown.contains("services.gpueue.settings"), "{shown}");
    assert!(shown.contains("systemd.services.gpueue-helper"), "{shown}");
    assert!(!shown.contains("systemctl edit"), "{shown}");
}
