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
        force: false,
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
    sys.write(
        "/etc/systemd/system/puzuz.service",
        &format!("{MARKER}\nedited"),
    );
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
            force: false,
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

/// A root-run system with both binaries in a build directory.
fn build_dir_system() -> FakeSystem {
    let mut sys = FakeSystem::new();
    sys.exe = PathBuf::from("/home/u/target/release/puzuz");
    sys.write_exe("/home/u/target/release/puzuz", "new puzuz");
    sys.write_exe("/home/u/target/release/puzuz-helper", "new helper");
    sys
}

fn receipt(sys: &FakeSystem) -> Receipt {
    toml::from_str(&sys.read(RECEIPT_PATH).expect("no receipt")).unwrap()
}

fn paths(list: &[&str]) -> Vec<PathBuf> {
    list.iter().map(PathBuf::from).collect()
}

#[test]
fn binaries_outside_system_dirs_are_copied_to_usr_local_bin() {
    let sys = build_dir_system();
    let out = run_install(&sys, &opts()).unwrap();
    assert!(
        out.contains("installing into /usr/local/bin (use --bin-dir to choose another directory)"),
        "{out}"
    );
    assert_eq!(sys.read("/usr/local/bin/puzuz").unwrap(), "new puzuz");
    assert_eq!(
        sys.read("/usr/local/bin/puzuz-helper").unwrap(),
        "new helper"
    );
    let unit = sys.read("/etc/systemd/system/puzuz.service").unwrap();
    assert!(unit.contains("\nExecStart=/usr/local/bin/puzuz daemon\n"));
    assert_eq!(
        receipt(&sys),
        Receipt {
            version: puzuz_protocol::BINARY_VERSION.to_owned(),
            bin_dir: PathBuf::from("/usr/local/bin"),
            copied: paths(&["/usr/local/bin/puzuz", "/usr/local/bin/puzuz-helper"]),
            ..Receipt::default()
        }
    );
    assert_eq!(mode(&sys, RECEIPT_PATH), 0o644);

    // Re-running the installed copy uses it in place and keeps it recorded as copied.
    let mut sys = sys;
    sys.exe = PathBuf::from("/usr/local/bin/puzuz");
    let out = run_install(&sys, &opts()).unwrap();
    assert!(!out.contains("installing into"), "{out}");
    assert_eq!(
        receipt(&sys).copied,
        paths(&["/usr/local/bin/puzuz", "/usr/local/bin/puzuz-helper"])
    );
}

#[test]
fn binaries_in_a_system_dir_are_used_in_place() {
    let sys = system();
    let out = run_install(&sys, &opts()).unwrap();
    assert!(!out.contains("installing into"), "{out}");
    assert!(!sys.exists("/usr/local/bin/puzuz"));
    let unit = sys.read("/etc/systemd/system/puzuz.service").unwrap();
    assert!(unit.contains("\nExecStart=/usr/bin/puzuz daemon\n"));
    let receipt = receipt(&sys);
    assert_eq!(receipt.bin_dir, PathBuf::from("/usr/bin"));
    assert!(receipt.copied.is_empty());
}

#[test]
fn a_reinstall_stays_in_the_installed_directory() {
    let mut sys = build_dir_system();
    sys.mkdir("/opt/tools/bin");
    let options = InstallOptions {
        bin_dir: Some(PathBuf::from("/opt/tools/bin")),
        ..opts()
    };
    run_install(&sys, &options).unwrap();
    sys.write_exe("/home/u/target/release/puzuz", "newer puzuz");
    let out = run_install(&sys, &opts()).unwrap();
    assert!(!out.contains("installing into"), "{out}");
    assert_eq!(sys.read("/opt/tools/bin/puzuz").unwrap(), "newer puzuz");
    assert!(!sys.exists("/usr/local/bin/puzuz"));
    let unit = sys.read("/etc/systemd/system/puzuz.service").unwrap();
    assert!(unit.contains("\nExecStart=/opt/tools/bin/puzuz daemon\n"));

    // Even a binary in /usr/bin installs there (copied), not in place.
    sys.exe = PathBuf::from("/usr/bin/puzuz");
    sys.write_exe("/usr/bin/puzuz", "other puzuz");
    sys.write_exe("/usr/bin/puzuz-helper", "other helper");
    run_install(&sys, &opts()).unwrap();
    assert_eq!(sys.read("/opt/tools/bin/puzuz").unwrap(), "other puzuz");
}

#[test]
fn moving_the_installation_removes_the_previous_copies() {
    let sys = build_dir_system();
    run_install(&sys, &opts()).unwrap();
    sys.mkdir("/opt/tools/bin");
    let options = InstallOptions {
        bin_dir: Some(PathBuf::from("/opt/tools/bin")),
        ..opts()
    };
    let out = run_install(&sys, &options).unwrap();
    assert!(
        out.contains("removed /usr/local/bin/puzuz-helper (copied by the previous install)"),
        "{out}"
    );
    assert!(!sys.exists("/usr/local/bin/puzuz"));
    assert_eq!(
        receipt(&sys).copied,
        paths(&["/opt/tools/bin/puzuz", "/opt/tools/bin/puzuz-helper"])
    );

    // An in-place installation's binaries were never copied and are never removed.
    let sys = system();
    run_install(&sys, &opts()).unwrap();
    sys.mkdir("/opt/tools/bin");
    run_install(&sys, &options).unwrap();
    assert!(sys.exists("/usr/bin/puzuz"));
    assert!(sys.exists("/usr/bin/puzuz-helper"));
}

#[test]
fn an_untrusted_receipt_is_ignored() {
    let sys = build_dir_system();
    sys.write(
        RECEIPT_PATH,
        "version = \"0.0.1\"\nbin_dir = \"/usr/sbin\"\ncopied = [\"/usr/sbin/puzuz\"]\n",
    );
    sys.write_exe("/usr/sbin/puzuz", "someone else's");
    std::fs::set_permissions(
        sys.path(Path::new(RECEIPT_PATH)),
        std::fs::Permissions::from_mode(0o666),
    )
    .unwrap();
    let out = run_install(&sys, &opts()).unwrap();
    assert!(out.contains("writable by others; ignoring it"), "{out}");
    assert!(sys.exists("/usr/sbin/puzuz"));
    assert_eq!(receipt(&sys).bin_dir, PathBuf::from("/usr/local/bin"));
    assert_eq!(mode(&sys, RECEIPT_PATH), 0o644);
}

#[test]
fn receipt_lists_only_binaries_in_its_directory_as_removable() {
    let receipt = Receipt {
        version: "0.1.0".to_owned(),
        bin_dir: PathBuf::from("/usr/local/bin"),
        copied: paths(&[
            "/usr/local/bin/puzuz",
            "/usr/local/bin/../sbin/puzuz",
            "/etc/passwd",
            "/usr/local/bin/puzuz-helper",
        ]),
        gc_root: Some(PathBuf::from(GC_ROOT)),
        gc_root_target: Some(PathBuf::from("/nix/store/abc-puzuz-0.1.0")),
    };
    assert_eq!(
        receipt.removable_copies(),
        paths(&["/usr/local/bin/puzuz", "/usr/local/bin/puzuz-helper"])
    );
    let parsed: Receipt = toml::from_str(&receipt.to_toml()).unwrap();
    assert_eq!(parsed, receipt);
}

const STORE_PATH: &str = "/nix/store/abc-puzuz-0.1.0";

/// A root-run system running puzuz from the Nix store through `./result` (as after
/// `nix build`).
fn nix_build_system() -> FakeSystem {
    let mut sys = FakeSystem::new();
    sys.write_exe(&format!("{STORE_PATH}/bin/puzuz"), "nix puzuz");
    sys.write_exe(&format!("{STORE_PATH}/bin/puzuz-helper"), "nix helper");
    sys.mkdir("/home/u");
    std::os::unix::fs::symlink(
        sys.path(Path::new(STORE_PATH)),
        sys.path(Path::new("/home/u/result")),
    )
    .unwrap();
    sys.exe = PathBuf::from("/home/u/result/bin/puzuz");
    sys
}

#[test]
fn binaries_from_the_nix_store_are_copied_and_kept_by_a_gc_root() {
    let sys = nix_build_system();
    sys.write_exe("/nix/var/nix/profiles/default/bin/nix-store", "");
    let out = run_install(&sys, &opts()).unwrap();
    assert!(out.contains("installing into /usr/local/bin"), "{out}");
    assert_eq!(sys.read("/usr/local/bin/puzuz").unwrap(), "nix puzuz");
    assert!(
        sys.ran(&format!(
            "nix-store --add-root /nix/var/nix/gcroots/puzuz --realise {STORE_PATH}"
        )),
        "{:?}",
        sys.commands()
    );
    assert!(
        out.contains(&format!(
            "registered the Nix GC root /nix/var/nix/gcroots/puzuz for {STORE_PATH}"
        )),
        "{out}"
    );
    let receipt = receipt(&sys);
    assert_eq!(receipt.gc_root, Some(PathBuf::from(GC_ROOT)));
    assert_eq!(receipt.gc_root_target, Some(PathBuf::from(STORE_PATH)));
    assert!(
        sys.read(RECEIPT_PATH)
            .unwrap()
            .contains(&format!("gc_root_target = \"{STORE_PATH}\"\n"))
    );
}

#[test]
fn without_nix_store_the_gc_root_is_only_a_warning() {
    let sys = nix_build_system();
    let out = run_install(&sys, &opts()).unwrap();
    assert!(out.contains("warning: nix-store not found"), "{out}");
    assert!(
        out.contains(&format!(
            "sudo nix-store --add-root /nix/var/nix/gcroots/puzuz --realise {STORE_PATH}"
        )),
        "{out}"
    );
    assert_eq!(sys.read("/usr/local/bin/puzuz").unwrap(), "nix puzuz");
    assert_eq!(receipt(&sys).gc_root, None);

    // A failing nix-store, too.
    let sys = nix_build_system();
    sys.write_exe("/usr/bin/nix-store", "");
    sys.respond("nix-store", CmdOutput::failed("", "permission denied"));
    let out = run_install(&sys, &opts()).unwrap();
    assert!(out.contains("warning: `"), "{out}");
    assert!(out.contains("permission denied"), "{out}");
    assert_eq!(receipt(&sys).gc_root, None);
}

/// Pretends `nix-store --add-root` ran: the GC root symlink exists.
fn make_gc_root(sys: &FakeSystem) {
    sys.mkdir("/nix/var/nix/gcroots");
    let root = sys.path(Path::new(GC_ROOT));
    let _ = std::fs::remove_file(&root);
    std::os::unix::fs::symlink(STORE_PATH, root).unwrap();
}

#[test]
fn the_gc_root_goes_once_the_binaries_no_longer_come_from_the_store() {
    let mut sys = nix_build_system();
    sys.write_exe("/usr/bin/nix-store", "");
    run_install(&sys, &opts()).unwrap();
    make_gc_root(&sys);

    // Re-running the installed copy changes nothing: the copies still need the store.
    sys.exe = PathBuf::from("/usr/local/bin/puzuz");
    sys.log.borrow_mut().clear();
    run_install(&sys, &opts()).unwrap();
    assert!(!sys.ran("nix-store"));
    assert!(sys.exists(GC_ROOT));
    assert_eq!(receipt(&sys).gc_root, Some(PathBuf::from(GC_ROOT)));
    assert_eq!(
        receipt(&sys).gc_root_target,
        Some(PathBuf::from(STORE_PATH))
    );

    // Binaries from elsewhere replace them; the store path is no longer needed.
    sys.exe = PathBuf::from("/home/u/target/release/puzuz");
    sys.write_exe("/home/u/target/release/puzuz", "new puzuz");
    sys.write_exe("/home/u/target/release/puzuz-helper", "new helper");
    let out = run_install(&sys, &opts()).unwrap();
    assert!(
        out.contains("removed the Nix GC root /nix/var/nix/gcroots/puzuz"),
        "{out}"
    );
    assert!(!sys.exists(GC_ROOT));
    assert_eq!(receipt(&sys).gc_root, None);
    assert_eq!(receipt(&sys).gc_root_target, None);
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
        force: false,
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
        force: false,
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
            force: false,
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

/// The key (or table name) of a commented TOML line of the configuration
/// template: `key = value` or a table header such as `[[gpus]]`.
fn template_toml_key(text: &str) -> Option<&str> {
    let is_name = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_lowercase() || c == '_');
    if let Some((key, _)) = text.split_once(" = ")
        && is_name(key)
    {
        return Some(key);
    }
    let table = text.trim_start_matches('[').trim_end_matches(']');
    (text.starts_with('[') && is_name(table)).then_some(table)
}

/// Splits the commented TOML of the configuration template by its
/// convention: a `# key = value` line shows a default, and lines indented by
/// `#   ` right below a `# Example:` line form an example. Returns the
/// default lines and the example blocks. Panics on commented TOML that
/// follows neither form (such as a table header outside an example).
fn template_defaults_and_examples(template: &str) -> (String, Vec<String>) {
    let mut defaults = String::new();
    let mut examples: Vec<String> = Vec::new();
    let mut in_example = false;
    for line in template.lines() {
        if line == "# Example:" {
            examples.push(String::new());
            in_example = true;
            continue;
        }
        if in_example && let Some(example) = line.strip_prefix("#   ") {
            let block = examples.last_mut().unwrap();
            block.push_str(example);
            block.push('\n');
            continue;
        }
        in_example = false;
        let Some(comment) = line.strip_prefix('#') else {
            continue;
        };
        if let Some(text) = comment.strip_prefix(' ')
            && !text.starts_with([' ', '['])
            && template_toml_key(text).is_some()
        {
            defaults.push_str(text);
            defaults.push('\n');
            continue;
        }
        assert!(
            template_toml_key(comment.trim_start()).is_none(),
            "commented TOML that is neither `# key = default` nor below `# Example:`: {line}"
        );
    }
    (defaults, examples)
}

#[test]
fn config_template_convention() {
    let (defaults, examples) = template_defaults_and_examples(
        "# a = 1\n# Example:\n#   b = 2\n#   [[c]]\n# prose, then\n#   indented prose\n",
    );
    assert_eq!(defaults, "a = 1\n");
    assert_eq!(examples, ["b = 2\n[[c]]\n"]);
    for unmarked in [
        "#   b = 2\n",
        "# [[gpus]]\n",
        "#  index = 0\n",
        "# Example:\n\n#   x = 1\n",
    ] {
        assert!(
            std::panic::catch_unwind(|| template_defaults_and_examples(unmarked)).is_err(),
            "{unmarked:?} was accepted"
        );
    }
}

#[test]
fn config_template_documents_every_key_and_parses_to_the_defaults() {
    use puzuz_daemon::config::GpuConfig;

    assert_eq!(Config::parse(CONFIG_TEMPLATE).unwrap(), Config::default());
    let (defaults_toml, examples) = template_defaults_and_examples(CONFIG_TEMPLATE);
    let default = Config::default();
    let defaults = serde_json::to_value(&default).unwrap();
    let default_keys: Vec<&str> = defaults_toml
        .lines()
        .filter_map(template_toml_key)
        .collect();
    let example_keys: Vec<&str> = examples
        .iter()
        .flat_map(|example| example.lines().filter_map(template_toml_key))
        .collect();

    // Every key is shown, with its default or in an example.
    for key in defaults.as_object().unwrap().keys() {
        assert!(
            default_keys.contains(&key.as_str()) || example_keys.contains(&key.as_str()),
            "{key} is not documented"
        );
    }
    // Every field of a [[gpus]] entry is described and in the example.
    let gpu = serde_json::to_value(GpuConfig {
        index: 0,
        uuid: String::new(),
        name: String::new(),
        memory_total_bytes: None,
    })
    .unwrap();
    for field in gpu.as_object().unwrap().keys() {
        assert!(
            example_keys.contains(&field.as_str()),
            "[[gpus]] field {field} is not in the example"
        );
        assert!(
            CONFIG_TEMPLATE.contains(&format!("\n# - {field} (")),
            "[[gpus]] field {field} is not described"
        );
    }

    // The lines shown as defaults are the defaults. The paths below data_dir
    // are unset by default and resolve to the paths shown.
    let mut parsed = Config::parse(&defaults_toml).unwrap();
    assert_eq!(parsed.state_db_path(), default.state_db_path());
    assert_eq!(parsed.helper_state_dir(), default.helper_state_dir());
    assert_eq!(parsed.logs_dir(), default.logs_dir());
    assert_eq!(parsed.checkpoints_dir(), default.checkpoints_dir());
    parsed.state_db = None;
    parsed.helper_state_dir = None;
    parsed.logs_dir = None;
    parsed.checkpoints_dir = None;
    assert_eq!(parsed, default);

    // Examples are valid and are not defaults.
    for example in &examples {
        let parsed =
            Config::parse(example).unwrap_or_else(|e| panic!("invalid example {example:?}: {e}"));
        let parsed = serde_json::to_value(parsed).unwrap();
        for key in example.lines().filter_map(template_toml_key) {
            let Some(default_value) = defaults.get(key) else {
                continue; // a field of a [[gpus]] entry
            };
            assert!(
                !default_keys.contains(&key),
                "{key} is shown both as a default and in an example"
            );
            assert_ne!(
                parsed.get(key),
                Some(default_value),
                "the example of {key} is its default; show it as `# {key} = ...`"
            );
        }
    }
}

#[test]
fn mountinfo_escapes() {
    assert_eq!(unescape_mountinfo("/mnt/a\\040b"), "/mnt/a b");
    assert_eq!(unescape_mountinfo("/plain"), "/plain");
    assert_eq!(unescape_mountinfo("/odd\\"), "/odd\\");
}

#[test]
fn install_refuses_to_overwrite_foreign_files_without_force() {
    let sys = system();
    let admin_unit = "[Service]\nExecStart=/usr/bin/puzuz daemon --custom\n";
    sys.write("/etc/systemd/system/puzuz.service", admin_unit);
    let err = run_install(&sys, &opts()).unwrap_err();
    assert!(
        err.contains("/etc/systemd/system/puzuz.service was not written"),
        "{err}"
    );
    assert!(err.contains("--force"), "{err}");
    // Nothing was changed or run.
    assert_eq!(
        sys.read("/etc/systemd/system/puzuz.service").unwrap(),
        admin_unit
    );
    assert!(!sys.exists("/etc/systemd/system/puzuz-helper.service"));
    assert!(sys.commands().is_empty());

    let forced = InstallOptions {
        force: true,
        ..opts()
    };
    run_install(&sys, &forced).unwrap();
    assert!(
        sys.read("/etc/systemd/system/puzuz.service")
            .unwrap()
            .starts_with(MARKER)
    );
}

#[test]
fn bin_dir_must_be_root_owned_and_protected() {
    let mut sys = FakeSystem::new();
    sys.exe = PathBuf::from("/home/u/target/release/puzuz");
    sys.write_exe("/home/u/target/release/puzuz", "new puzuz");
    sys.write_exe("/home/u/target/release/puzuz-helper", "new helper");
    let options = InstallOptions {
        bin_dir: Some(PathBuf::from("/opt/tools/bin")),
        ..opts()
    };
    for writable in ["/opt/tools/bin", "/opt/tools"] {
        sys.mkdir("/opt/tools/bin");
        let dir = sys.path(Path::new(writable));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o775)).unwrap();
        let err = run_install(&sys, &options).unwrap_err();
        assert!(
            err.contains(&format!("{writable} must be owned by root")),
            "{writable}: {err}"
        );
        assert!(!sys.exists("/opt/tools/bin/puzuz-helper"));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    run_install(&sys, &options).unwrap();
    assert_eq!(
        sys.read("/opt/tools/bin/puzuz-helper").unwrap(),
        "new helper"
    );
}

#[test]
fn install_does_not_follow_a_planted_temporary_file() {
    let mut sys = FakeSystem::new();
    sys.exe = PathBuf::from("/home/u/target/release/puzuz");
    sys.write_exe("/home/u/target/release/puzuz", "new puzuz");
    sys.write_exe("/home/u/target/release/puzuz-helper", "new helper");
    sys.write("/etc/shadow", "secret");
    sys.mkdir("/usr/local/bin");
    std::os::unix::fs::symlink(
        sys.path(Path::new("/etc/shadow")),
        sys.path(Path::new("/usr/local/bin/.puzuz-helper.puzuz-install")),
    )
    .unwrap();
    let options = InstallOptions {
        bin_dir: Some(PathBuf::from("/usr/local/bin")),
        ..opts()
    };
    run_install(&sys, &options).unwrap();
    assert_eq!(sys.read("/etc/shadow").unwrap(), "secret");
    assert_eq!(
        sys.read("/usr/local/bin/puzuz-helper").unwrap(),
        "new helper"
    );
    assert!(!sys.exists("/usr/local/bin/.puzuz-helper.puzuz-install"));
}

/// Runs the install in a child process with umask 077 (the umask is process-wide, so it must
/// not change under the other tests).
#[test]
fn created_directories_are_world_readable_whatever_the_umask() {
    const CHILD: &str = "PUZUZ_TEST_UMASK_CHILD";
    if let Some(out) = std::env::var_os(CHILD) {
        nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o077));
        let mut sys = FakeSystem::new();
        sys.exe = PathBuf::from("/home/u/target/release/puzuz");
        sys.write_exe("/home/u/target/release/puzuz", "new puzuz");
        sys.write_exe("/home/u/target/release/puzuz-helper", "new helper");
        let options = InstallOptions {
            bin_dir: Some(PathBuf::from("/usr/local/bin")),
            ..opts()
        };
        run_install(&sys, &options).unwrap();
        let modes: Vec<String> = [
            "/etc/puzuz",
            "/etc/puzuz/config.toml",
            "/etc/systemd/system",
            "/usr/local/bin",
            "/usr/local/bin/puzuz-helper",
        ]
        .iter()
        .map(|path| format!("{path} {:o}", mode(&sys, path)))
        .collect();
        std::fs::write(out, modes.join("\n")).unwrap();
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("modes");
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "service::tests::created_directories_are_world_readable_whatever_the_umask",
        ])
        .env(CHILD, &out)
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "{}{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        "/etc/puzuz 755\n/etc/puzuz/config.toml 644\n/etc/systemd/system 755\n\
         /usr/local/bin 755\n/usr/local/bin/puzuz-helper 755"
    );
}

#[test]
fn install_success_ignores_only_expected_no_start_failures() {
    let check = |name: &'static str, level| crate::doctor::Check {
        level,
        name,
        message: String::new(),
        hint: None,
    };
    let report = Report {
        checks: vec![
            check(DAEMON_UNIT, crate::doctor::Level::Fail),
            check("daemon", crate::doctor::Level::Fail),
            check("criu", crate::doctor::Level::Warn),
        ],
    };
    let no_start = InstallOptions {
        no_start: true,
        ..opts()
    };
    assert!(!install_succeeded(&report, &opts()));
    assert!(install_succeeded(&report, &no_start));
    let mut broken = report.clone();
    broken
        .checks
        .push(check("criu", crate::doctor::Level::Fail));
    assert!(!install_succeeded(&broken, &no_start));
}

const PACKAGED_UNIT: &str = "/usr/lib/systemd/system/puzuz.service";
const PACKAGED_HELPER_UNIT: &str = "/usr/lib/systemd/system/puzuz-helper.service";

/// Both units from the dpkg package `puzuz`.
fn install_package(sys: &FakeSystem) {
    for unit in [PACKAGED_UNIT, PACKAGED_HELPER_UNIT] {
        sys.write(unit, "[Service]\nExecStart=/usr/bin/puzuz daemon\n");
        sys.dpkg_owns(unit, "puzuz");
    }
}

#[test]
fn install_refuses_a_package_managed_machine_even_with_force() {
    let sys = system();
    install_package(&sys);
    for force in [false, true] {
        let options = InstallOptions { force, ..opts() };
        let err = run_install(&sys, &options).unwrap_err();
        assert!(err.contains("distribution package (puzuz, dpkg"), "{err}");
        assert!(err.contains("sudo apt update && sudo apt upgrade"), "{err}");
        assert!(err.contains("first run `sudo apt remove puzuz`"), "{err}");
        assert!(err.contains("--force does not change this"), "{err}");
    }
    assert!(!sys.exists("/etc/systemd/system/puzuz.service"));
    assert!(sys.commands_without_queries().is_empty());
}

#[test]
fn install_refuses_a_masked_or_overridden_package_even_with_force() {
    for masked in [true, false] {
        let sys = system();
        install_package(&sys);
        if masked {
            let local = sys.path(Path::new("/etc/systemd/system/puzuz.service"));
            std::fs::create_dir_all(local.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink("/dev/null", local).unwrap();
        } else {
            sys.write(
                "/etc/systemd/system/puzuz.service",
                "[Service]\nExecStart=/usr/bin/puzuz daemon\n",
            );
        }
        for force in [false, true] {
            let options = InstallOptions { force, ..opts() };
            let err = run_install(&sys, &options).unwrap_err();
            assert!(err.contains("distribution package"), "{err}");
            assert!(err.contains("--force does not change this"), "{err}");
            assert!(!err.contains("mixed"), "{err}");
        }
        let err = run_uninstall(&sys, false, true).unwrap_err();
        assert!(err.contains("not by `puzuz service install`"), "{err}");
        assert!(sys.commands_without_queries().is_empty());
    }
}

#[test]
fn install_refuses_units_from_the_nix_store() {
    let sys = system();
    for unit in ["puzuz.service", "puzuz-helper.service"] {
        let local = sys.path(Path::new(UNIT_DIR).join(unit).as_path());
        std::fs::create_dir_all(local.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(format!("/nix/store/abc-{unit}"), local).unwrap();
    }
    for force in [false, true] {
        let options = InstallOptions { force, ..opts() };
        let err = run_install(&sys, &options).unwrap_err();
        assert!(err.contains("installed as Nix"), "{err}");
        assert!(err.contains("Nix configuration"), "{err}");
    }
    assert!(sys.commands().is_empty());
}

#[test]
fn uninstall_refuses_a_package_managed_machine() {
    let sys = system();
    install_package(&sys);
    let err = run_uninstall(&sys, false, true).unwrap_err();
    assert!(err.contains("not by `puzuz service install`"), "{err}");
    assert!(err.contains("sudo apt remove puzuz"), "{err}");
    assert!(sys.exists(PACKAGED_UNIT));
    assert!(sys.commands_without_queries().is_empty());
}

#[test]
fn uninstall_of_a_mixed_setup_removes_only_its_own_files() {
    let sys = system();
    run_install(&sys, &opts()).unwrap();
    install_package(&sys);
    sys.mkdir("/var/lib/puzuz/daemon");

    let err = run_uninstall(&sys, true, true).unwrap_err();
    assert!(err.contains("refusing to purge"), "{err}");
    assert!(sys.exists("/etc/systemd/system/puzuz.service"));

    let out = run_uninstall(&sys, false, false).unwrap();
    assert!(!sys.exists("/etc/systemd/system/puzuz.service"));
    assert!(sys.exists(PACKAGED_UNIT));
    assert!(sys.exists("/var/lib/puzuz/daemon"));
    assert!(
        out.contains("still installed as distribution package"),
        "{out}"
    );
}

#[test]
fn status_reports_the_install_method() {
    let sys = system();
    let mut out = Vec::new();
    status(&sys, None, &mut out).unwrap();
    let out = String::from_utf8(out).unwrap();
    assert!(
        out.contains("installed              not installed"),
        "{out}"
    );

    run_install(&sys, &opts()).unwrap();
    install_package(&sys);
    let mut out = Vec::new();
    assert!(!status(&sys, None, &mut out).unwrap());
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains("installed              mixed: "), "{out}");
}

fn symlink(sys: &FakeSystem, target: &str, link: &str) {
    let local = sys.path(Path::new(link));
    std::fs::create_dir_all(local.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(target, local).unwrap();
}

/// Both units in /usr/lib, owned by no package.
fn unowned_units(sys: &FakeSystem) {
    for unit in [PACKAGED_UNIT, PACKAGED_HELPER_UNIT] {
        sys.write(unit, "[Service]\nExecStart=/usr/bin/puzuz daemon\n");
    }
    sys.respond(
        "dpkg-query -S",
        CmdOutput::failed("", "dpkg-query: no path found matching pattern\n"),
    );
    sys.missing("rpm");
    sys.missing("pacman");
}

#[test]
fn install_refuses_a_package_of_unknown_origin_even_with_force() {
    let sys = system();
    for unit in [PACKAGED_UNIT, PACKAGED_HELPER_UNIT] {
        sys.write(unit, "[Service]\nExecStart=/usr/bin/puzuz daemon\n");
    }
    for tool in ["dpkg-query", "rpm", "pacman"] {
        sys.missing(tool);
    }
    sys.write(
        "/etc/os-release",
        "ID=rocky\nID_LIKE=\"rhel centos fedora\"\n",
    );
    for force in [false, true] {
        let options = InstallOptions { force, ..opts() };
        let err = run_install(&sys, &options).unwrap_err();
        assert!(err.contains("owner unknown"), "{err}");
        assert!(err.contains("`sudo dnf upgrade`"), "{err}");
    }
    assert!(sys.commands_without_queries().is_empty());
}

#[test]
fn install_refuses_nixos_without_puzuz() {
    let sys = system();
    sys.write("/etc/os-release", "NAME=NixOS\nID=nixos\n");
    for force in [false, true] {
        let options = InstallOptions { force, ..opts() };
        let err = run_install(&sys, &options).unwrap_err();
        assert!(err.contains("this is NixOS"), "{err}");
        assert!(err.contains("`services.puzuz` NixOS module"), "{err}");
    }
    assert!(sys.commands().is_empty());

    // /etc/NIXOS alone is not NixOS.
    let sys = system();
    sys.write("/etc/NIXOS", "");
    run_install(&sys, &opts()).unwrap();
}

#[test]
fn install_refuses_the_nixos_module() {
    let sys = system();
    sys.write("/etc/os-release", "ID=nixos\n");
    symlink(&sys, "/nix/store/abc-system-units", UNIT_DIR);
    for unit in [DAEMON_UNIT, HELPER_UNIT] {
        sys.write(
            &format!("/nix/store/abc-system-units/{unit}"),
            "[Service]\nExecStart=/nix/store/def-puzuz/bin/puzuz daemon\n",
        );
    }
    let err = run_install(&sys, &opts()).unwrap_err();
    assert!(err.contains("installed as Nix"), "{err}");
    assert!(err.contains("nixos-rebuild switch"), "{err}");
    assert!(err.contains("services.puzuz.enable = false"), "{err}");
    let err = run_uninstall(&sys, false, true).unwrap_err();
    assert!(err.contains("services.puzuz.enable = false"), "{err}");
}

#[test]
fn install_refuses_a_unit_directory_in_the_nix_store() {
    let sys = system();
    sys.write("/etc/os-release", "ID=ubuntu\nID_LIKE=debian\n");
    symlink(&sys, "/nix/store/abc-units", UNIT_DIR);
    let forced = InstallOptions {
        force: true,
        ..opts()
    };
    let err = run_install(&sys, &forced).unwrap_err();
    assert!(err.contains("resolves into /nix/store"), "{err}");
    assert!(sys.commands().is_empty());
}

#[test]
fn install_over_an_unowned_unit_needs_force() {
    let sys = system();
    unowned_units(&sys);
    let err = run_install(&sys, &opts()).unwrap_err();
    assert!(
        err.contains("no package owns /usr/lib/systemd/system/puzuz.service"),
        "{err}"
    );
    assert!(
        err.contains("`sudo rm /usr/lib/systemd/system/puzuz.service`"),
        "{err}"
    );
    assert!(err.contains("--force"), "{err}");
    assert!(sys.commands_without_queries().is_empty());
    // Nothing of `service install` to uninstall.
    let err = run_uninstall(&sys, false, true).unwrap_err();
    assert!(err.contains("not by `puzuz service install`"), "{err}");
    assert!(
        err.contains("delete /usr/lib/systemd/system/puzuz.service"),
        "{err}"
    );

    let forced = InstallOptions {
        force: true,
        ..opts()
    };
    let out = run_install(&sys, &forced).unwrap();
    assert!(out.contains("warning: no package owns"), "{out}");
    assert!(sys.exists("/etc/systemd/system/puzuz.service"));

    // Now a `service install` with leftovers: re-running needs no --force, but warns.
    let detection = install_method::detect(&sys);
    assert_eq!(detection.keyword(), "service-install");
    assert_eq!(detection.leftovers.len(), 2);
    let out = run_install(&sys, &opts()).unwrap();
    assert!(
        out.contains("which the units in /etc/systemd/system shadow"),
        "{out}"
    );
}

#[test]
fn uninstall_refuses_a_hand_written_unit() {
    let sys = system();
    for unit in [DAEMON_UNIT, HELPER_UNIT] {
        sys.write(
            &format!("/etc/systemd/system/{unit}"),
            "[Service]\nExecStart=/opt/puzuz daemon\n",
        );
    }
    let err = run_uninstall(&sys, false, true).unwrap_err();
    assert!(err.contains("installed as by hand"), "{err}");
    assert!(
        err.contains("remove /etc/systemd/system/puzuz.service by hand"),
        "{err}"
    );
    assert!(sys.exists("/etc/systemd/system/puzuz.service"));
    assert!(sys.commands().is_empty());
}

#[test]
fn uninstall_of_nothing_says_so() {
    let sys = system();
    let out = run_uninstall(&sys, false, true).unwrap();
    assert!(out.contains("puzuz is not installed"), "{out}");
}

#[test]
fn a_packaged_helper_next_to_installed_units_is_refused() {
    let sys = system();
    run_install(&sys, &opts()).unwrap();
    std::fs::remove_file(sys.path(Path::new("/etc/systemd/system/puzuz-helper.service"))).unwrap();
    sys.write(
        PACKAGED_HELPER_UNIT,
        "[Service]\nExecStart=/usr/bin/puzuz-helper serve\n",
    );
    sys.dpkg_owns(PACKAGED_HELPER_UNIT, "puzuz");
    assert!(install_method::detect(&sys).partial.is_some());
    // Unhealthy, like a mixed setup.
    let mut out = Vec::new();
    assert!(!status(&sys, None, &mut out).unwrap());
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("but puzuz-helper.service as distribution package")
    );

    let forced = InstallOptions {
        force: true,
        ..opts()
    };
    let err = run_install(&sys, &forced).unwrap_err();
    assert!(err.contains("distribution package (puzuz, dpkg"), "{err}");
    assert!(err.contains("sudo puzuz service uninstall"), "{err}");
    let err = run_uninstall(&sys, true, true).unwrap_err();
    assert!(err.contains("refusing to purge"), "{err}");
    run_uninstall(&sys, false, true).unwrap();
    assert!(!sys.exists("/etc/systemd/system/puzuz.service"));
    assert!(sys.exists(PACKAGED_HELPER_UNIT));
}

#[test]
fn an_unowned_unit_next_to_installed_units_needs_no_force() {
    let sys = system();
    run_install(&sys, &opts()).unwrap();
    // The daemon's /etc unit is gone; an unowned /usr/lib one is left.
    std::fs::remove_file(sys.path(Path::new("/etc/systemd/system/puzuz.service"))).unwrap();
    sys.write(
        PACKAGED_UNIT,
        "[Service]\nExecStart=/usr/bin/puzuz daemon\n",
    );
    sys.respond(
        "dpkg-query -S",
        CmdOutput::failed("", "dpkg-query: no path found matching pattern\n"),
    );
    assert_eq!(install_method::detect(&sys).keyword(), "service-install");
    let out = run_install(&sys, &opts()).unwrap();
    assert!(out.contains("warning: no package owns"), "{out}");
    assert!(sys.exists("/etc/systemd/system/puzuz.service"));
}
