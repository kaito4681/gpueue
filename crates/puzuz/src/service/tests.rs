use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use puzuz_daemon::config::Config;

use super::*;
use crate::system::CmdOutput;
use crate::system::fake::FakeSystem;

fn opts() -> InstallOptions {
    InstallOptions {
        no_start: false,
        bin_dir: None,
    }
}

/// A root-run system with both binaries in /usr/bin.
fn system() -> FakeSystem {
    let sys = FakeSystem::new();
    sys.write_exe("/usr/bin/puzuz", "puzuz binary");
    sys.write_exe("/usr/bin/puzuz-helper", "helper binary");
    sys
}

fn run_install(sys: &FakeSystem, options: &InstallOptions) -> Result<String> {
    let mut out = Vec::new();
    install(sys, options, &mut out)?;
    Ok(String::from_utf8(out).unwrap())
}

fn run_uninstall(sys: &FakeSystem, purge: bool, yes: bool) -> Result<String> {
    let mut out = Vec::new();
    uninstall(sys, &UninstallOptions { purge, yes }, &mut out)?;
    Ok(String::from_utf8(out).unwrap())
}

fn mode(sys: &FakeSystem, path: &str) -> u32 {
    std::fs::metadata(sys.path(Path::new(path)))
        .unwrap()
        .permissions()
        .mode()
        & 0o7777
}

#[test]
fn install_and_uninstall_require_root() {
    let mut sys = system();
    sys.euid = 1000;
    let err = run_install(&sys, &opts()).unwrap_err();
    assert!(err.contains("must run as root"), "{err}");
    assert!(err.contains("sudo puzuz service install"), "{err}");
    let err = run_uninstall(&sys, false, false).unwrap_err();
    assert!(err.contains("sudo puzuz service uninstall"), "{err}");
    assert!(sys.commands().is_empty());
    assert!(!sys.exists("/etc/systemd/system/puzuz.service"));
}

#[test]
fn install_writes_files_and_starts_services() {
    let sys = system();
    let out = run_install(&sys, &opts()).unwrap();

    let helper_unit = sys
        .read("/etc/systemd/system/puzuz-helper.service")
        .unwrap();
    assert!(helper_unit.starts_with(MARKER));
    assert!(helper_unit.contains("\nExecStart=/usr/bin/puzuz-helper serve\n"));
    let daemon_unit = sys.read("/etc/systemd/system/puzuz.service").unwrap();
    assert!(daemon_unit.contains("\nExecStart=/usr/bin/puzuz daemon\n"));
    assert!(daemon_unit.contains("User=puzuz"));
    let sysusers = sys.read("/etc/sysusers.d/puzuz.conf").unwrap();
    assert!(sysusers.contains("g puzuz-admin -"));
    let tmpfiles = sys.read("/etc/tmpfiles.d/puzuz.conf").unwrap();
    assert!(tmpfiles.contains("/var/lib/puzuz/daemon"));
    assert_eq!(mode(&sys, "/etc/systemd/system/puzuz.service"), 0o644);

    let config = sys.read("/etc/puzuz/config.toml").unwrap();
    assert_eq!(config, CONFIG_TEMPLATE);
    assert_eq!(Config::parse(&config).unwrap(), Config::default());

    let root = sys.root().display().to_string();
    assert_eq!(
        sys.commands()[..6],
        [
            format!("systemd-sysusers {root}/etc/sysusers.d/puzuz.conf"),
            "groupadd --system puzuz-admin".to_owned(),
            format!("systemd-tmpfiles --create {root}/etc/tmpfiles.d/puzuz.conf"),
            "systemctl daemon-reload".to_owned(),
            "systemctl try-restart puzuz-helper.service puzuz.service".to_owned(),
            "systemctl enable --now puzuz-helper.service puzuz.service".to_owned(),
        ]
    );
    // The doctor report follows.
    assert!(
        out.contains("installed /etc/systemd/system/puzuz.service"),
        "{out}"
    );
    assert!(out.contains("created /etc/puzuz/config.toml"), "{out}");
    assert!(out.contains("Checking the installation"), "{out}");
    assert!(out.contains("FAIL  criu:"), "{out}");
}

#[test]
fn install_is_idempotent_and_keeps_the_configuration() {
    let sys = system();
    sys.groups.borrow_mut().insert(ADMIN_GROUP.to_owned());
    run_install(&sys, &opts()).unwrap();
    sys.write("/etc/puzuz/config.toml", "grace_secs = 5\n");
    sys.log.borrow_mut().clear();

    let out = run_install(&sys, &opts()).unwrap();
    assert!(
        out.contains("unchanged /etc/systemd/system/puzuz.service"),
        "{out}"
    );
    assert!(
        out.contains("kept the existing /etc/puzuz/config.toml"),
        "{out}"
    );
    assert_eq!(
        sys.read("/etc/puzuz/config.toml").unwrap(),
        "grace_secs = 5\n"
    );
    // Nothing changed, so nothing is restarted, and the group already exists.
    assert!(!sys.ran("systemctl try-restart"), "{:?}", sys.commands());
    assert!(!sys.ran("groupadd"));
    assert!(sys.ran("systemctl enable --now puzuz-helper.service puzuz.service"));
}

#[test]
fn install_restarts_after_a_unit_changed() {
    let sys = system();
    run_install(&sys, &opts()).unwrap();
    sys.write("/etc/systemd/system/puzuz.service", "edited");
    sys.log.borrow_mut().clear();
    run_install(&sys, &opts()).unwrap();
    assert!(sys.ran("systemctl try-restart puzuz-helper.service puzuz.service"));
}

#[test]
fn install_without_start_only_enables() {
    let sys = system();
    let out = run_install(
        &sys,
        &InstallOptions {
            no_start: true,
            bin_dir: None,
        },
    )
    .unwrap();
    assert!(sys.ran("systemctl enable puzuz-helper.service puzuz.service"));
    assert!(!sys.ran("systemctl enable --now"));
    assert!(!sys.ran("systemctl try-restart"));
    assert!(out.contains("--no-start"), "{out}");
}

#[test]
fn install_waits_for_the_daemon() {
    let mut sys = system();
    sys.daemon = FakeSystem::daemon_ok(true);
    let out = run_install(&sys, &opts()).unwrap();
    assert!(out.contains("OK    helper: connected"), "{out}");
}

#[test]
fn binaries_outside_system_dirs_need_bin_dir() {
    let mut sys = system();
    sys.exe = PathBuf::from("/home/u/.cargo/bin/puzuz");
    sys.write_exe("/home/u/.cargo/bin/puzuz", "new puzuz");
    sys.write_exe("/home/u/.cargo/bin/puzuz-helper", "new helper");
    let err = run_install(&sys, &opts()).unwrap_err();
    assert!(err.contains("--bin-dir /usr/local/bin"), "{err}");
    assert!(!sys.exists("/etc/systemd/system/puzuz.service"));
}

#[test]
fn bin_dir_installs_copies() {
    let mut sys = FakeSystem::new();
    sys.exe = PathBuf::from("/home/u/target/release/puzuz");
    sys.write_exe("/home/u/target/release/puzuz", "new puzuz");
    sys.write_exe("/home/u/target/release/puzuz-helper", "new helper");
    sys.write_exe("/usr/local/bin/puzuz-helper", "old helper");
    let options = InstallOptions {
        no_start: false,
        bin_dir: Some(PathBuf::from("/usr/local/bin")),
    };
    let out = run_install(&sys, &options).unwrap();
    assert_eq!(sys.read("/usr/local/bin/puzuz").unwrap(), "new puzuz");
    assert_eq!(
        sys.read("/usr/local/bin/puzuz-helper").unwrap(),
        "new helper"
    );
    assert_eq!(mode(&sys, "/usr/local/bin/puzuz-helper"), 0o755);
    assert!(
        out.contains("installed /usr/local/bin/puzuz-helper"),
        "{out}"
    );
    let unit = sys
        .read("/etc/systemd/system/puzuz-helper.service")
        .unwrap();
    assert!(unit.contains("\nExecStart=/usr/local/bin/puzuz-helper serve\n"));
    let unit = sys.read("/etc/systemd/system/puzuz.service").unwrap();
    assert!(unit.contains("\nExecStart=/usr/local/bin/puzuz daemon\n"));

    // Re-running with the same binaries copies nothing and restarts nothing.
    sys.log.borrow_mut().clear();
    let out = run_install(&sys, &options).unwrap();
    assert!(!out.contains("installed /usr/local/bin"), "{out}");
    assert!(!sys.ran("systemctl try-restart"));

    // A new build is copied and the services restarted.
    sys.write_exe("/home/u/target/release/puzuz-helper", "newer helper");
    sys.log.borrow_mut().clear();
    run_install(&sys, &options).unwrap();
    assert_eq!(
        sys.read("/usr/local/bin/puzuz-helper").unwrap(),
        "newer helper"
    );
    assert!(sys.ran("systemctl try-restart"));
}

#[test]
fn bin_dir_equal_to_the_binary_location_copies_nothing() {
    let sys = system();
    let options = InstallOptions {
        no_start: true,
        bin_dir: Some(PathBuf::from("/usr/bin")),
    };
    let out = run_install(&sys, &options).unwrap();
    assert!(!out.contains("installed /usr/bin"), "{out}");
}

#[test]
fn bad_bin_dirs_and_missing_helper_are_rejected() {
    let sys = system();
    for dir in ["relative/bin", "/opt/my bin", "/opt/%h"] {
        let options = InstallOptions {
            no_start: false,
            bin_dir: Some(PathBuf::from(dir)),
        };
        let err = run_install(&sys, &options).unwrap_err();
        assert!(err.contains("--bin-dir"), "{dir}: {err}");
    }
    std::fs::remove_file(sys.path(Path::new("/usr/bin/puzuz-helper"))).unwrap();
    let err = run_install(&sys, &opts()).unwrap_err();
    assert!(err.contains("/usr/bin/puzuz-helper not found"), "{err}");
}

#[test]
fn failing_commands_abort_the_install() {
    let sys = system();
    sys.respond(
        "systemd-sysusers",
        CmdOutput::failed("", "Failed to parse puzuz.conf"),
    );
    let err = run_install(&sys, &opts()).unwrap_err();
    assert!(err.contains("Failed to parse puzuz.conf"), "{err}");
    assert!(!sys.ran("systemctl"));

    let sys = system();
    sys.missing("systemctl");
    let err = run_install(&sys, &opts()).unwrap_err();
    assert!(
        err.contains("cannot run `systemctl daemon-reload`"),
        "{err}"
    );
}

#[test]
fn new_configuration_lists_detected_gpus() {
    let sys = system();
    sys.write_exe("/usr/bin/nvidia-smi", "");
    sys.respond(
        "nvidia-smi --query-gpu=index,uuid,name,memory.total",
        CmdOutput::ok(
            "0, GPU-aaaa, NVIDIA GeForce RTX 4090, 24564\n1, GPU-bbbb, NVIDIA \"Quoted\", 81920\n",
        ),
    );
    run_install(&sys, &opts()).unwrap();
    let config = Config::parse(&sys.read("/etc/puzuz/config.toml").unwrap()).unwrap();
    assert_eq!(config.gpus.len(), 2);
    assert_eq!(config.gpus[0].uuid, "GPU-aaaa");
    assert_eq!(config.gpus[0].name, "NVIDIA GeForce RTX 4090");
    assert_eq!(config.gpus[0].memory_total_bytes, Some(24564 << 20));
    assert_eq!(config.gpus[1].index, 1);
    assert_eq!(config.gpus[1].name, "NVIDIA \"Quoted\"");
}

#[test]
fn uninstall_removes_units_but_keeps_data() {
    let sys = system();
    run_install(&sys, &opts()).unwrap();
    sys.mkdir("/var/lib/puzuz/daemon");
    sys.log.borrow_mut().clear();

    let out = run_uninstall(&sys, false, false).unwrap();
    assert_eq!(
        sys.commands(),
        [
            "systemctl list-units --plain --no-legend --state=active,activating,deactivating,reloading puzuz-job-*",
            "systemctl disable --now puzuz.service puzuz-helper.service",
            "systemctl daemon-reload",
        ]
    );
    for path in [
        "/etc/systemd/system/puzuz.service",
        "/etc/systemd/system/puzuz-helper.service",
        "/etc/sysusers.d/puzuz.conf",
        "/etc/tmpfiles.d/puzuz.conf",
    ] {
        assert!(!sys.exists(path), "{path}");
    }
    assert!(sys.exists("/etc/puzuz/config.toml"));
    assert!(sys.exists("/var/lib/puzuz/daemon"));
    assert!(sys.exists("/usr/bin/puzuz-helper"));
    assert!(out.contains("--purge"), "{out}");

    // Again: nothing left to do.
    sys.log.borrow_mut().clear();
    run_uninstall(&sys, false, false).unwrap();
    assert!(!sys.ran("systemctl disable"));
}

#[test]
fn uninstall_keeps_files_it_did_not_install() {
    let sys = system();
    sys.write(
        "/etc/tmpfiles.d/puzuz.conf",
        "d /var/lib/puzuz 0755 root root -\n",
    );
    let out = run_uninstall(&sys, false, false).unwrap();
    assert!(sys.exists("/etc/tmpfiles.d/puzuz.conf"));
    assert!(out.contains("kept /etc/tmpfiles.d/puzuz.conf"), "{out}");
}

#[test]
fn uninstall_warns_about_running_jobs() {
    let sys = system();
    sys.respond(
        "systemctl list-units",
        CmdOutput::ok("puzuz-job-3.service loaded active running puzuz job 3\n"),
    );
    let out = run_uninstall(&sys, false, false).unwrap();
    assert!(out.contains("puzuz-job-3.service"), "{out}");

    // Purging would delete their logs and images.
    sys.mkdir("/var/lib/puzuz/logs");
    let err = run_uninstall(&sys, true, true).unwrap_err();
    assert!(err.contains("puzuz-job-3.service"), "{err}");
    assert!(sys.exists("/var/lib/puzuz/logs"));
}

#[test]
fn purge_asks_for_confirmation() {
    let sys = system();
    run_install(&sys, &opts()).unwrap();
    sys.write("/var/lib/puzuz/daemon/state.sqlite", "db");
    sys.log.borrow_mut().clear();

    let err = run_uninstall(&sys, true, false).unwrap_err();
    assert!(err.contains("aborted"), "{err}");
    assert_eq!(sys.prompts.borrow().len(), 1);
    assert!(sys.prompts.borrow()[0].contains("/var/lib/puzuz"));
    assert!(sys.exists("/etc/systemd/system/puzuz.service"));
    assert!(sys.exists("/var/lib/puzuz/daemon/state.sqlite"));
    assert!(!sys.ran("systemctl disable"));
}

#[test]
fn purge_deletes_data_but_not_mount_points() {
    let mut sys = system();
    run_install(&sys, &opts()).unwrap();
    sys.write("/var/lib/puzuz/daemon/state.sqlite", "db");
    sys.write("/var/lib/puzuz/checkpoints/7/pages-1.img", "image");
    sys.write("/run/puzuz-helper/criu-root/etc/passwd", "the host's root!");
    sys.write("/run/puzuz/puzuz.sock", "");
    sys.write("/srv/keep/file", "kept");
    std::os::unix::fs::symlink(
        sys.path(Path::new("/srv/keep")),
        sys.path(Path::new("/var/lib/puzuz/link")),
    )
    .unwrap();
    sys.write(
        "/proc/self/mountinfo",
        "22 1 8:1 / / rw - ext4 /dev/sda1 rw\n\
         90 22 8:1 / /run/puzuz-helper/criu-root rw - ext4 /dev/sda1 rw\n",
    );
    sys.confirm_answer = true;

    let out = run_uninstall(&sys, true, false).unwrap();
    assert!(!sys.exists("/var/lib/puzuz"));
    assert!(!sys.exists("/etc/puzuz"));
    assert!(!sys.exists("/run/puzuz"));
    // The symlink was removed, not followed.
    assert_eq!(sys.read("/srv/keep/file").unwrap(), "kept");
    // The bind mount was not descended into.
    assert_eq!(
        sys.read("/run/puzuz-helper/criu-root/etc/passwd").unwrap(),
        "the host's root!"
    );
    assert!(
        out.contains("kept /run/puzuz-helper/criu-root (a mount point"),
        "{out}"
    );
    assert!(out.contains("removed /var/lib/puzuz\n"), "{out}");
    assert!(!out.contains("removed /run/puzuz-helper"), "{out}");
    // `--yes` skips the question.
    sys.confirm_answer = false;
    run_uninstall(&sys, true, true).unwrap();
    assert_eq!(sys.prompts.borrow().len(), 1);
}

#[test]
fn status_reports_units_and_connections() {
    let mut sys = system();
    sys.respond("systemctl is-enabled", CmdOutput::ok("enabled\n"));
    sys.respond("systemctl is-active", CmdOutput::ok("active\n"));
    sys.daemon = FakeSystem::daemon_ok(true);
    let mut out = Vec::new();
    assert!(status(&sys, None, &mut out).unwrap());
    let out = String::from_utf8(out).unwrap();
    assert!(
        out.contains("puzuz-helper.service   enabled, active"),
        "{out}"
    );
    assert!(
        out.contains("puzuz.service          enabled, active"),
        "{out}"
    );
    assert!(out.contains("reachable at /run/puzuz/puzuz.sock"), "{out}");
    assert!(out.contains("helper                 connected"), "{out}");

    sys.daemon = FakeSystem::daemon_ok(false);
    let mut out = Vec::new();
    assert!(!status(&sys, Some(Path::new("/tmp/other.sock")), &mut out).unwrap());
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains("reachable at /tmp/other.sock"), "{out}");
    assert!(out.contains("not connected"), "{out}");

    sys.respond(
        "systemctl is-active puzuz.service",
        CmdOutput::failed("inactive\n", ""),
    );
    sys.daemon = Err("connection refused".to_owned());
    let mut out = Vec::new();
    assert!(!status(&sys, None, &mut out).unwrap());
    let out = String::from_utf8(out).unwrap();
    assert!(
        out.contains("puzuz.service          enabled, inactive"),
        "{out}"
    );
    assert!(out.contains("not reachable: connection refused"), "{out}");
}

#[test]
fn render_unit_rewrites_only_exec_start() {
    let unit = render_unit(
        "[Service]\nExecStart=/usr/bin/puzuz daemon\n# ExecStart=/usr/bin/x\n",
        Path::new("/opt/bin"),
    );
    assert_eq!(
        unit,
        "[Service]\nExecStart=/opt/bin/puzuz daemon\n# ExecStart=/usr/bin/x\n"
    );
}

#[test]
fn packaged_units_start_the_expected_binaries() {
    assert!(HELPER_UNIT_TEMPLATE.contains("\nExecStart=/usr/bin/puzuz-helper serve\n"));
    assert!(DAEMON_UNIT_TEMPLATE.contains("\nExecStart=/usr/bin/puzuz daemon\n"));
}

#[test]
fn config_template_documents_every_key_and_parses_to_the_defaults() {
    assert_eq!(Config::parse(CONFIG_TEMPLATE).unwrap(), Config::default());
    let defaults = serde_json::to_value(Config::default()).unwrap();
    for key in defaults.as_object().unwrap().keys() {
        let documented = if key == "gpus" {
            "# [[gpus]]".to_owned()
        } else {
            format!("# {key} = ")
        };
        assert!(
            CONFIG_TEMPLATE.contains(&documented),
            "{key} is not documented"
        );
    }
    // The commented values are the defaults.
    let uncommented: String = CONFIG_TEMPLATE
        .lines()
        .filter_map(|line| line.strip_prefix("# "))
        .filter(|line| {
            line.split_once(" = ")
                .is_some_and(|(key, _)| defaults.get(key).is_some())
                // Optional keys whose commented values are examples, not defaults.
                && !line.starts_with("checkpoint_size_limit_bytes")
                && !line.starts_with("nvml_library")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut parsed = Config::parse(&uncommented).unwrap();
    let default = Config::default();
    assert_eq!(parsed.state_db_path(), default.state_db_path());
    assert_eq!(parsed.helper_state_dir(), default.helper_state_dir());
    assert_eq!(parsed.logs_dir(), default.logs_dir());
    assert_eq!(parsed.checkpoints_dir(), default.checkpoints_dir());
    parsed.state_db = None;
    parsed.helper_state_dir = None;
    parsed.logs_dir = None;
    parsed.checkpoints_dir = None;
    assert_eq!(parsed, default);
}

#[test]
fn mountinfo_escapes() {
    assert_eq!(unescape_mountinfo("/mnt/a\\040b"), "/mnt/a b");
    assert_eq!(unescape_mountinfo("/plain"), "/plain");
    assert_eq!(unescape_mountinfo("/odd\\"), "/odd\\");
}
