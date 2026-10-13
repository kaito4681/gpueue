use std::path::PathBuf;

use super::*;
use crate::system::fake::FakeSystem;

const ETC_UNIT: &str = "/etc/systemd/system/puzuz.service";
const LIB_UNIT: &str = "/usr/lib/systemd/system/puzuz.service";

fn marker_unit(bin_dir: &str) -> String {
    format!("{MARKER}\n[Service]\nExecStart={bin_dir}/puzuz daemon\n")
}

fn service_install(bin_dir: &str) -> Method {
    Method::ServiceInstall {
        bin_dir: Some(PathBuf::from(bin_dir)),
    }
}

fn package(unit: &str) -> Method {
    Method::Package {
        unit: PathBuf::from(unit),
        local: None,
    }
}

fn package_with(unit: &str, local: LocalUnit) -> Method {
    Method::Package {
        unit: PathBuf::from(unit),
        local: Some(local),
    }
}

fn mask(sys: &FakeSystem, unit: &str) {
    let local = sys.path(Path::new(unit));
    std::fs::create_dir_all(local.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("/dev/null", local).unwrap();
}

#[test]
fn nothing_installed() {
    let sys = FakeSystem::new();
    let method = detect(&sys);
    assert_eq!(method, InstallMethod::None);
    assert_eq!(method.keyword(), "none");
    assert_eq!(method.to_string(), "not installed");
    assert!(method.managed_elsewhere().is_none());
}

#[test]
fn service_install_is_recognized_by_its_marker() {
    let sys = FakeSystem::new();
    sys.write(ETC_UNIT, &marker_unit("/usr/local/bin"));
    let method = detect(&sys);
    assert_eq!(
        method,
        InstallMethod::One(service_install("/usr/local/bin"))
    );
    assert_eq!(method.keyword(), "service-install");
    assert_eq!(
        method.to_string(),
        "puzuz service install (binaries in /usr/local/bin)"
    );
    assert!(method.has_service_install());
    assert!(method.managed_elsewhere().is_none());
}

#[test]
fn a_unit_in_usr_lib_is_a_package() {
    let sys = FakeSystem::new();
    sys.write(LIB_UNIT, "[Service]\nExecStart=/usr/bin/puzuz daemon\n");
    let method = detect(&sys);
    assert_eq!(method, InstallMethod::One(package(LIB_UNIT)));
    assert_eq!(method.keyword(), "package");
    assert_eq!(method.managed_elsewhere(), Some(&package(LIB_UNIT)));
}

#[test]
fn a_unit_in_lib_is_a_package_too() {
    let sys = FakeSystem::new();
    let unit = "/lib/systemd/system/puzuz.service";
    sys.write(unit, "[Service]\nExecStart=/usr/bin/puzuz daemon\n");
    assert_eq!(detect(&sys), InstallMethod::One(package(unit)));
}

#[test]
fn merged_usr_reports_the_package_once() {
    let sys = FakeSystem::new();
    sys.write(LIB_UNIT, "[Service]\nExecStart=/usr/bin/puzuz daemon\n");
    std::os::unix::fs::symlink("usr/lib", sys.path(Path::new("/lib"))).unwrap();
    assert_eq!(detect(&sys), InstallMethod::One(package(LIB_UNIT)));
}

#[test]
fn nixos_is_recognized_by_etc_nixos_and_the_store() {
    let sys = FakeSystem::new();
    sys.write("/etc/NIXOS", "");
    assert_eq!(detect(&sys), InstallMethod::One(Method::NixOs));

    // The module's units run binaries from /nix/store, without the marker.
    sys.write(
        ETC_UNIT,
        "[Service]\nExecStart=/nix/store/abc-puzuz-0.1.0/bin/puzuz daemon\n",
    );
    let method = detect(&sys);
    assert_eq!(method, InstallMethod::One(Method::NixOs));
    assert_eq!(method.keyword(), "nixos");
    assert!(method.managed_elsewhere().is_some());

    // Even without /etc/NIXOS.
    let sys = FakeSystem::new();
    sys.write(
        LIB_UNIT,
        "[Service]\nExecStart=/nix/store/abc-puzuz-0.1.0/bin/puzuz daemon\n",
    );
    assert_eq!(detect(&sys), InstallMethod::One(Method::NixOs));
}

#[test]
fn a_unit_without_the_marker_is_manual() {
    let sys = FakeSystem::new();
    sys.write(ETC_UNIT, "[Service]\nExecStart=/opt/puzuz/puzuz daemon\n");
    let method = detect(&sys);
    assert_eq!(
        method,
        InstallMethod::One(Method::Manual {
            unit: PathBuf::from(ETC_UNIT)
        })
    );
    assert_eq!(method.keyword(), "manual");
    assert!(method.managed_elsewhere().is_none());
}

#[test]
fn service_install_next_to_a_package_is_mixed() {
    let sys = FakeSystem::new();
    sys.write(LIB_UNIT, "[Service]\nExecStart=/usr/bin/puzuz daemon\n");
    sys.write(ETC_UNIT, &marker_unit("/usr/local/bin"));
    let method = detect(&sys);
    assert_eq!(
        method,
        InstallMethod::Mixed(vec![package(LIB_UNIT), service_install("/usr/local/bin"),])
    );
    assert_eq!(method.keyword(), "mixed");
    assert!(method.has_service_install());
    assert_eq!(method.managed_elsewhere(), Some(&package(LIB_UNIT)));
    assert_eq!(
        method.to_string(),
        "mixed: distribution package (/usr/lib/systemd/system/puzuz.service) and puzuz service \
         install (binaries in /usr/local/bin)"
    );
}

#[test]
fn service_install_on_nixos_is_mixed() {
    let sys = FakeSystem::new();
    sys.write("/etc/NIXOS", "");
    sys.write(ETC_UNIT, &marker_unit("/usr/local/bin"));
    assert_eq!(
        detect(&sys),
        InstallMethod::Mixed(vec![Method::NixOs, service_install("/usr/local/bin")])
    );
}

#[test]
fn a_masked_package_unit_is_still_the_package() {
    let sys = FakeSystem::new();
    sys.write(LIB_UNIT, "[Service]\nExecStart=/usr/bin/puzuz daemon\n");
    mask(&sys, ETC_UNIT);
    let method = detect(&sys);
    assert_eq!(
        method,
        InstallMethod::One(package_with(
            LIB_UNIT,
            LocalUnit::Masked(PathBuf::from(ETC_UNIT))
        ))
    );
    assert_eq!(method.keyword(), "package");
    assert!(method.managed_elsewhere().is_some());
    assert_eq!(
        method.to_string(),
        "distribution package (/usr/lib/systemd/system/puzuz.service; masked by \
         /etc/systemd/system/puzuz.service)"
    );
}

#[test]
fn an_overridden_package_unit_is_still_the_package() {
    let sys = FakeSystem::new();
    sys.write(LIB_UNIT, "[Service]\nExecStart=/usr/bin/puzuz daemon\n");
    // `systemctl edit --full`: a marker-less copy in /etc.
    sys.write(
        ETC_UNIT,
        "[Service]\nExecStart=/usr/bin/puzuz daemon --verbose\n",
    );
    let method = detect(&sys);
    assert_eq!(
        method,
        InstallMethod::One(package_with(
            LIB_UNIT,
            LocalUnit::Overridden(PathBuf::from(ETC_UNIT))
        ))
    );
    assert_eq!(method.keyword(), "package");
    assert_eq!(
        method.to_string(),
        "distribution package (/usr/lib/systemd/system/puzuz.service; overridden by \
         /etc/systemd/system/puzuz.service)"
    );
}

#[test]
fn a_mask_alone_is_not_an_installation() {
    let sys = FakeSystem::new();
    mask(&sys, ETC_UNIT);
    assert_eq!(detect(&sys), InstallMethod::None);

    // Nor next to NixOS.
    sys.write("/etc/NIXOS", "");
    assert_eq!(detect(&sys), InstallMethod::One(Method::NixOs));
}

#[test]
fn hints_name_the_right_tool() {
    assert!(
        service_install("/opt/bin")
            .update_hint()
            .contains("service install --bin-dir /opt/bin")
    );
    assert!(package(LIB_UNIT).update_hint().contains("package manager"));
    assert!(package(LIB_UNIT).removal_hint().contains("apt remove"));
    assert!(Method::NixOs.update_hint().contains("nixos-rebuild"));
    assert!(Method::NixOs.removal_hint().contains("enable = false"));
    assert_eq!(
        service_install("/usr/local/bin").removal_hint(),
        "sudo puzuz service uninstall"
    );
}
