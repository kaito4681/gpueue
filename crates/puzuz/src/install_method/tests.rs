use std::path::PathBuf;

use super::*;
use crate::system::CmdOutput;
use crate::system::fake::FakeSystem;

const ETC: &str = "/etc/systemd/system";
const LIB: &str = "/usr/lib/systemd/system";
const UNITS: [&str; 2] = [DAEMON_UNIT, HELPER_UNIT];

fn path(dir: &str, unit: &str) -> String {
    format!("{dir}/{unit}")
}

fn marker_unit(bin_dir: &str, unit: &str) -> String {
    let exe = if unit == HELPER_UNIT {
        "puzuz-helper serve"
    } else {
        "puzuz daemon"
    };
    format!("{MARKER}\n[Service]\nExecStart={bin_dir}/{exe}\n")
}

fn plain_unit(exe: &str) -> String {
    format!("[Service]\nExecStart={exe} daemon\n")
}

/// Both units, `service install`ed into `bin_dir`.
fn write_service_install(sys: &FakeSystem, bin_dir: &str) {
    for unit in UNITS {
        sys.write(&path(ETC, unit), &marker_unit(bin_dir, unit));
    }
}

/// Both units in `dir`, without the marker.
fn write_plain(sys: &FakeSystem, dir: &str) {
    for unit in UNITS {
        sys.write(&path(dir, unit), &plain_unit("/usr/bin/puzuz"));
    }
}

/// Both packaged units, owned by the dpkg package `puzuz`.
fn write_dpkg_package(sys: &FakeSystem) {
    write_plain(sys, LIB);
    for unit in UNITS {
        sys.dpkg_owns(&path(LIB, unit), "puzuz");
    }
}

fn symlink(sys: &FakeSystem, target: &str, link: &str) {
    let local = sys.path(Path::new(link));
    std::fs::create_dir_all(local.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(target, local).unwrap();
}

fn not_owned(sys: &FakeSystem) {
    sys.respond(
        "dpkg-query -S",
        CmdOutput::failed("", "dpkg-query: no path found matching pattern\n"),
    );
}

fn service_install(bin_dir: &str) -> Method {
    Method::ServiceInstall {
        bin_dir: Some(PathBuf::from(bin_dir)),
    }
}

fn package(unit: &str, owner: Owner, local: Option<LocalUnit>) -> Method {
    Method::Package {
        unit: PathBuf::from(unit),
        owner,
        local,
    }
}

fn dpkg(unit: &str) -> Method {
    package(unit, Owner::Dpkg("puzuz".to_owned()), None)
}

fn os(id: &str, id_like: &[&str]) -> OsRelease {
    OsRelease {
        id: id.to_owned(),
        id_like: id_like.iter().map(|s| (*s).to_owned()).collect(),
    }
}

#[test]
fn nothing_installed() {
    let sys = FakeSystem::new();
    let detection = detect(&sys);
    assert_eq!(detection.install, InstallMethod::None);
    assert_eq!(detection.keyword(), "none");
    assert_eq!(detection.install.to_string(), "not installed");
    assert!(detection.managed_elsewhere().is_none());
    assert_eq!(detection.partial, None);
    assert!(!detection.unit_dir_in_store);
    // No package manager was asked: there is no packaged unit.
    assert!(sys.commands().is_empty());
}

#[test]
fn service_install_is_recognized_by_its_marker() {
    let sys = FakeSystem::new();
    write_service_install(&sys, "/usr/local/bin");
    let detection = detect(&sys);
    assert_eq!(
        detection.install,
        InstallMethod::One(service_install("/usr/local/bin"))
    );
    assert_eq!(detection.helper, detection.install);
    assert_eq!(detection.keyword(), "service-install");
    assert_eq!(
        detection.install.to_string(),
        "puzuz service install (binaries in /usr/local/bin)"
    );
    assert_eq!(detection.partial, None);
    assert!(detection.has_service_install());
    assert!(detection.managed_elsewhere().is_none());
}

#[test]
fn a_dpkg_package_is_named() {
    let sys = FakeSystem::new();
    write_dpkg_package(&sys);
    let detection = detect(&sys);
    let unit = path(LIB, DAEMON_UNIT);
    assert_eq!(detection.install, InstallMethod::One(dpkg(&unit)));
    assert_eq!(detection.keyword(), "package");
    assert_eq!(detection.partial, None);
    assert_eq!(detection.managed_elsewhere(), Some(&dpkg(&unit)));
    assert_eq!(
        detection.install.to_string(),
        "distribution package (puzuz, dpkg: /usr/lib/systemd/system/puzuz.service)"
    );
    assert!(sys.ran("dpkg-query -S /usr/lib/systemd/system/puzuz.service"));
}

#[test]
fn an_rpm_package_is_named() {
    let sys = FakeSystem::new();
    write_plain(&sys, LIB);
    sys.missing("dpkg-query");
    sys.respond("rpm -qf", CmdOutput::ok("puzuz\n"));
    let detection = detect(&sys);
    let unit = path(LIB, DAEMON_UNIT);
    assert_eq!(
        detection.install,
        InstallMethod::One(package(&unit, Owner::Rpm("puzuz".to_owned()), None))
    );
    assert!(sys.ran("rpm -qf --queryformat %{NAME}\\n /usr/lib/systemd/system/puzuz.service"));
    let method = &detection.install.methods()[0];
    assert_eq!(detection.update_hint(method), "run `sudo dnf upgrade`");
    assert_eq!(
        detection.removal_hint(method),
        "run `sudo dnf remove puzuz`"
    );
}

#[test]
fn a_pacman_package_is_named() {
    let sys = FakeSystem::new();
    write_plain(&sys, LIB);
    sys.missing("dpkg-query");
    sys.missing("rpm");
    sys.respond("pacman -Qqo", CmdOutput::ok("puzuz-bin\n"));
    let detection = detect(&sys);
    let method = &detection.install.methods()[0];
    assert_eq!(
        method,
        &package(
            &path(LIB, DAEMON_UNIT),
            Owner::Pacman("puzuz-bin".to_owned()),
            None
        )
    );
    assert_eq!(
        detection.update_hint(method),
        "run `yay -Syu` (AUR) or `sudo pacman -Syu`"
    );
    assert_eq!(
        detection.removal_hint(method),
        "run `sudo pacman -R puzuz-bin`"
    );
}

#[test]
fn a_unit_nobody_owns_is_unowned() {
    let sys = FakeSystem::new();
    write_plain(&sys, LIB);
    not_owned(&sys);
    sys.respond(
        "rpm",
        CmdOutput::failed("file is not owned by any package\n", ""),
    );
    sys.missing("pacman");
    let detection = detect(&sys);
    let unit = PathBuf::from(path(LIB, DAEMON_UNIT));
    assert_eq!(
        detection.install,
        InstallMethod::One(Method::Unowned {
            unit: unit.clone(),
            local: None
        })
    );
    assert_eq!(detection.keyword(), "unowned");
    assert!(detection.managed_elsewhere().is_none());
    assert_eq!(
        detection.install.to_string(),
        "a unit no package owns (/usr/lib/systemd/system/puzuz.service)"
    );
    // Both paths were asked, of every package manager that exists.
    assert!(sys.ran("dpkg-query -S /lib/systemd/system/puzuz.service"));
    assert!(sys.ran("rpm -qf"));
}

#[test]
fn without_any_package_manager_the_owner_is_unknown() {
    let sys = FakeSystem::new();
    write_plain(&sys, LIB);
    for tool in ["dpkg-query", "rpm", "pacman"] {
        sys.missing(tool);
    }
    let detection = detect(&sys);
    let method = &detection.install.methods()[0];
    assert_eq!(
        method,
        &package(&path(LIB, DAEMON_UNIT), Owner::Unknown, None)
    );
    assert!(detection.managed_elsewhere().is_some());
    assert_eq!(detection.keyword(), "package");
    assert_eq!(
        detection.update_hint(method),
        "update the package with your package manager"
    );

    // The distribution names the package manager.
    sys.write("/etc/os-release", "ID=fedora\n");
    let detection = detect(&sys);
    assert_eq!(
        detection.update_hint(method),
        "update the package with your package manager (e.g. `sudo dnf upgrade`)"
    );
    assert!(detection.removal_hint(method).contains("sudo dnf remove"));
}

#[test]
fn merged_usr_asks_for_the_lib_path_too() {
    let sys = FakeSystem::new();
    symlink(&sys, "usr/lib", "/lib");
    write_plain(&sys, LIB);
    not_owned(&sys);
    for unit in UNITS {
        // dpkg registered the unit under /lib.
        sys.dpkg_owns(&path("/lib/systemd/system", unit), "puzuz");
    }
    let detection = detect(&sys);
    assert_eq!(
        detection.install,
        InstallMethod::One(dpkg(&path(LIB, DAEMON_UNIT)))
    );
}

#[test]
fn a_unit_in_lib_is_a_package_too() {
    let sys = FakeSystem::new();
    for unit in UNITS {
        let unit = path("/lib/systemd/system", unit);
        sys.write(&unit, &plain_unit("/usr/bin/puzuz"));
        sys.dpkg_owns(&unit, "puzuz");
    }
    assert_eq!(
        detect(&sys).install,
        InstallMethod::One(dpkg(&path("/lib/systemd/system", DAEMON_UNIT)))
    );
}

#[test]
fn a_dpkg_owner_with_an_architecture_is_its_name() {
    assert_eq!(
        parse_dpkg_owner("puzuz:amd64: /usr/lib/systemd/system/puzuz.service\n"),
        Some("puzuz".to_owned())
    );
    assert_eq!(
        parse_dpkg_owner(
            "diversion by other from: /usr/lib/x\ndiversion by other to: /usr/lib/x.real\n\
             a, b: /usr/lib/x\n"
        ),
        Some("a".to_owned())
    );
    assert_eq!(parse_dpkg_owner(""), None);
}

#[test]
fn a_masked_package_unit_is_still_the_package() {
    let sys = FakeSystem::new();
    write_dpkg_package(&sys);
    symlink(&sys, "/dev/null", &path(ETC, DAEMON_UNIT));
    let detection = detect(&sys);
    let etc = PathBuf::from(path(ETC, DAEMON_UNIT));
    assert_eq!(
        detection.install,
        InstallMethod::One(package(
            &path(LIB, DAEMON_UNIT),
            Owner::Dpkg("puzuz".to_owned()),
            Some(LocalUnit::Masked(etc))
        ))
    );
    assert_eq!(detection.keyword(), "package");
    assert_eq!(detection.partial, None);
    assert!(detection.managed_elsewhere().is_some());
    assert_eq!(
        detection.install.to_string(),
        "distribution package (puzuz, dpkg: /usr/lib/systemd/system/puzuz.service; masked by \
         /etc/systemd/system/puzuz.service)"
    );
}

#[test]
fn an_overridden_package_unit_is_still_the_package() {
    let sys = FakeSystem::new();
    write_dpkg_package(&sys);
    // `systemctl edit --full`: a marker-less copy in /etc.
    sys.write(
        &path(ETC, DAEMON_UNIT),
        "[Service]\nExecStart=/usr/bin/puzuz daemon --verbose\n",
    );
    let detection = detect(&sys);
    assert_eq!(
        detection.install,
        InstallMethod::One(package(
            &path(LIB, DAEMON_UNIT),
            Owner::Dpkg("puzuz".to_owned()),
            Some(LocalUnit::Overridden(PathBuf::from(path(ETC, DAEMON_UNIT))))
        ))
    );
    assert_eq!(
        detection.install.to_string(),
        "distribution package (puzuz, dpkg: /usr/lib/systemd/system/puzuz.service; overridden \
         by /etc/systemd/system/puzuz.service)"
    );
}

#[test]
fn an_overridden_unowned_unit_is_still_unowned() {
    let sys = FakeSystem::new();
    write_plain(&sys, LIB);
    not_owned(&sys);
    sys.write(&path(ETC, DAEMON_UNIT), &plain_unit("/opt/puzuz"));
    let detection = detect(&sys);
    assert_eq!(
        detection.install,
        InstallMethod::One(Method::Unowned {
            unit: PathBuf::from(path(LIB, DAEMON_UNIT)),
            local: Some(LocalUnit::Overridden(PathBuf::from(path(ETC, DAEMON_UNIT)))),
        })
    );
}

#[test]
fn a_unit_without_the_marker_is_manual() {
    let sys = FakeSystem::new();
    write_plain(&sys, ETC);
    let detection = detect(&sys);
    assert_eq!(
        detection.install,
        InstallMethod::One(Method::Manual {
            unit: PathBuf::from(path(ETC, DAEMON_UNIT))
        })
    );
    assert_eq!(detection.keyword(), "manual");
    assert_eq!(detection.partial, None);
    assert!(detection.managed_elsewhere().is_none());
}

#[test]
fn a_hand_written_unit_running_from_the_store_is_manual() {
    let sys = FakeSystem::new();
    for unit in UNITS {
        sys.write(
            &path(ETC, unit),
            &plain_unit("/nix/store/abc-puzuz-0.1.0/bin/puzuz"),
        );
    }
    let detection = detect(&sys);
    assert_eq!(detection.keyword(), "manual");
    assert!(detection.managed_elsewhere().is_none());
}

#[test]
fn nixos_without_puzuz_is_not_installed() {
    let sys = FakeSystem::new();
    sys.write(
        "/etc/os-release",
        "NAME=NixOS\nID=nixos\nVERSION_ID=\"26.05\"\n",
    );
    sys.write("/etc/NIXOS", "");
    let detection = detect(&sys);
    assert_eq!(detection.install, InstallMethod::None);
    assert!(detection.os.is_nixos());
    assert_eq!(detection.keyword(), "none");
}

#[test]
fn etc_nixos_alone_is_not_nixos() {
    let sys = FakeSystem::new();
    sys.write("/etc/NIXOS", "");
    sys.write("/etc/os-release", "ID=ubuntu\nID_LIKE=debian\n");
    assert!(!detect(&sys).os.is_nixos());
}

/// NixOS: /etc/systemd/system -> /etc/static/systemd/system -> /nix/store/...-system-units.
fn nixos_module(sys: &FakeSystem) {
    sys.write("/etc/os-release", "ID=nixos\n");
    symlink(sys, "/etc/static/systemd/system", ETC);
    symlink(
        sys,
        "/nix/store/abc-system-units",
        "/etc/static/systemd/system",
    );
}

#[test]
fn the_nixos_module_is_nix() {
    let sys = FakeSystem::new();
    nixos_module(&sys);
    for unit in UNITS {
        sys.write(
            &path("/nix/store/abc-system-units", unit),
            &plain_unit("/nix/store/def-puzuz/bin/puzuz"),
        );
    }
    let detection = detect(&sys);
    assert!(detection.unit_dir_in_store);
    assert_eq!(
        detection.install,
        InstallMethod::One(Method::Nix {
            unit: PathBuf::from(path(ETC, DAEMON_UNIT))
        })
    );
    assert_eq!(detection.keyword(), "nix");
    assert_eq!(detection.partial, None);
    let method = detection.managed_elsewhere().unwrap();
    assert_eq!(
        detection.update_hint(method),
        "run `nix flake update && sudo nixos-rebuild switch`"
    );
    assert!(detection.removal_hint(method).contains("enable = false"));
}

#[test]
fn a_unit_linked_into_the_store_is_nix_on_any_os() {
    let sys = FakeSystem::new();
    sys.write("/etc/os-release", "ID=ubuntu\nID_LIKE=debian\n");
    for unit in UNITS {
        symlink(&sys, &format!("/nix/store/abc-{unit}"), &path(ETC, unit));
    }
    let detection = detect(&sys);
    assert!(!detection.unit_dir_in_store);
    assert_eq!(detection.keyword(), "nix");
    let method = detection.managed_elsewhere().unwrap();
    assert_eq!(
        detection.update_hint(method),
        "change the Nix configuration that created /etc/systemd/system/puzuz.service"
    );
}

#[test]
fn a_packaged_unit_running_from_the_store_is_nix() {
    let sys = FakeSystem::new();
    for unit in UNITS {
        sys.write(
            &path(LIB, unit),
            &plain_unit("/nix/store/abc-puzuz-0.1.0/bin/puzuz"),
        );
    }
    let detection = detect(&sys);
    assert_eq!(
        detection.install,
        InstallMethod::One(Method::Nix {
            unit: PathBuf::from(path(LIB, DAEMON_UNIT))
        })
    );
    // Nobody is asked who owns it.
    assert!(sys.commands().is_empty());

    // A hand-written override next to it is no second installation.
    sys.write(&path(ETC, DAEMON_UNIT), &plain_unit("/opt/puzuz"));
    assert_eq!(detect(&sys).keyword(), "nix");
}

#[test]
fn only_the_helper_unit_is_partial() {
    let sys = FakeSystem::new();
    sys.write(
        &path(ETC, HELPER_UNIT),
        &marker_unit("/usr/local/bin", HELPER_UNIT),
    );
    let detection = detect(&sys);
    assert_eq!(
        detection.install,
        InstallMethod::One(service_install("/usr/local/bin"))
    );
    assert_eq!(
        detection.partial.as_deref(),
        Some(
            "only puzuz-helper.service is installed (puzuz service install (binaries in \
             /usr/local/bin))"
        )
    );
}

#[test]
fn a_missing_helper_unit_is_partial() {
    let sys = FakeSystem::new();
    sys.write(
        &path(ETC, DAEMON_UNIT),
        &marker_unit("/usr/local/bin", DAEMON_UNIT),
    );
    let detection = detect(&sys);
    assert_eq!(detection.keyword(), "service-install");
    assert!(
        detection
            .partial
            .as_deref()
            .unwrap()
            .starts_with("puzuz-helper.service is missing")
    );
}

#[test]
fn units_installed_in_different_ways_are_partial() {
    let sys = FakeSystem::new();
    sys.write(
        &path(ETC, DAEMON_UNIT),
        &marker_unit("/usr/local/bin", DAEMON_UNIT),
    );
    let helper = path(LIB, HELPER_UNIT);
    sys.write(&helper, &plain_unit("/usr/bin/puzuz-helper"));
    sys.dpkg_owns(&helper, "puzuz");
    let detection = detect(&sys);
    assert_eq!(
        detection.install,
        InstallMethod::One(service_install("/usr/local/bin"))
    );
    // For install.sh this is a conflict like `mixed`.
    assert_eq!(detection.keyword(), "mixed");
    assert_eq!(detection.helper, InstallMethod::One(dpkg(&helper)));
    assert!(detection.has_service_install());
    // The packaged helper unit counts too.
    assert_eq!(detection.managed_elsewhere(), Some(&dpkg(&helper)));
    let partial = detection.partial.unwrap();
    assert!(
        partial.starts_with("puzuz.service is installed as puzuz service install"),
        "{partial}"
    );
    assert!(
        partial.contains("but puzuz-helper.service as distribution package"),
        "{partial}"
    );

    // Different bin dirs do not belong together either.
    let sys = FakeSystem::new();
    sys.write(
        &path(ETC, DAEMON_UNIT),
        &marker_unit("/usr/local/bin", DAEMON_UNIT),
    );
    sys.write(
        &path(ETC, HELPER_UNIT),
        &marker_unit("/opt/bin", HELPER_UNIT),
    );
    assert!(detect(&sys).partial.is_some());
}

#[test]
fn service_install_next_to_a_package_is_mixed() {
    let sys = FakeSystem::new();
    write_dpkg_package(&sys);
    write_service_install(&sys, "/usr/local/bin");
    let detection = detect(&sys);
    let unit = path(LIB, DAEMON_UNIT);
    assert_eq!(
        detection.install,
        InstallMethod::Mixed(vec![dpkg(&unit), service_install("/usr/local/bin")])
    );
    assert_eq!(detection.keyword(), "mixed");
    assert_eq!(detection.partial, None);
    assert!(detection.has_service_install());
    assert_eq!(detection.managed_elsewhere(), Some(&dpkg(&unit)));
    assert_eq!(
        detection.install.to_string(),
        "mixed: distribution package (puzuz, dpkg: /usr/lib/systemd/system/puzuz.service) and \
         puzuz service install (binaries in /usr/local/bin)"
    );
}

#[test]
fn service_install_next_to_an_unowned_unit_is_a_leftover() {
    let sys = FakeSystem::new();
    write_plain(&sys, LIB);
    not_owned(&sys);
    write_service_install(&sys, "/usr/local/bin");
    let detection = detect(&sys);
    assert_eq!(
        detection.install,
        InstallMethod::One(service_install("/usr/local/bin"))
    );
    assert_eq!(detection.keyword(), "service-install");
    assert_eq!(
        detection.leftovers,
        UNITS.map(|unit| PathBuf::from(path(LIB, unit)))
    );
}

#[test]
fn service_install_next_to_nix_is_mixed() {
    let sys = FakeSystem::new();
    for unit in UNITS {
        sys.write(&path(LIB, unit), &plain_unit("/nix/store/abc/bin/puzuz"));
    }
    write_service_install(&sys, "/usr/local/bin");
    let detection = detect(&sys);
    assert_eq!(detection.keyword(), "mixed");
    assert!(matches!(
        detection.managed_elsewhere(),
        Some(Method::Nix { .. })
    ));
}

#[test]
fn a_mask_alone_is_not_an_installation() {
    let sys = FakeSystem::new();
    for unit in UNITS {
        symlink(&sys, "/dev/null", &path(ETC, unit));
    }
    let detection = detect(&sys);
    assert_eq!(detection.install, InstallMethod::None);
    assert_eq!(detection.partial, None);
}

#[test]
fn os_release_is_parsed() {
    let release = OsRelease::parse(
        "NAME=\"Rocky Linux\"\nID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\n# comment\n",
    );
    assert_eq!(release, os("rocky", &["rhel", "centos", "fedora"]));
    assert_eq!(release.family(), Some(Family::Rhel));
    assert_eq!(os("ubuntu", &["debian"]).family(), Some(Family::Debian));
    assert_eq!(os("debian", &[]).family(), Some(Family::Debian));
    assert_eq!(os("arch", &[]).family(), Some(Family::Arch));
    assert_eq!(
        os("opensuse-tumbleweed", &["opensuse", "suse"]).family(),
        Some(Family::Suse)
    );
    assert_eq!(os("nixos", &[]).family(), None);
    assert_eq!(OsRelease::parse("").family(), None);

    // /usr/lib/os-release when /etc has none.
    let sys = FakeSystem::new();
    sys.write("/usr/lib/os-release", "ID='arch'\n");
    assert_eq!(OsRelease::read(&sys), os("arch", &[]));
}

#[test]
fn hints_name_the_right_tool() {
    let ubuntu = os("ubuntu", &["debian"]);
    let unit = path(LIB, DAEMON_UNIT);
    assert_eq!(
        service_install("/opt/bin").update_hint(&ubuntu),
        "run `sudo puzuz update`"
    );
    assert_eq!(
        service_install("/opt/bin").removal_hint(&ubuntu),
        "run `sudo puzuz service uninstall`"
    );
    assert_eq!(
        dpkg(&unit).update_hint(&ubuntu),
        "run `sudo apt update && sudo apt upgrade`"
    );
    assert_eq!(
        dpkg(&unit).removal_hint(&ubuntu),
        "run `sudo apt remove puzuz`"
    );
    let unowned = Method::Unowned {
        unit: PathBuf::from(&unit),
        local: None,
    };
    assert!(unowned.update_hint(&ubuntu).contains("no package owns"));
    assert!(unowned.removal_hint(&ubuntu).starts_with("delete /usr/lib"));
    let nix = Method::Nix {
        unit: PathBuf::from(path(ETC, DAEMON_UNIT)),
    };
    assert!(nix.update_hint(&os("nixos", &[])).contains("nixos-rebuild"));
    assert!(!nix.update_hint(&ubuntu).contains("nixos-rebuild"));
    assert!(nix.removal_hint(&ubuntu).contains("Nix configuration"));
}

#[test]
fn resolve_follows_relative_and_absolute_links() {
    let sys = FakeSystem::new();
    sys.mkdir("/a/b");
    symlink(&sys, "b", "/a/link");
    symlink(&sys, "/a/link/../b", "/c");
    assert_eq!(resolve(&sys, Path::new("/c/x")), PathBuf::from("/a/b/x"));
    // A loop ends.
    symlink(&sys, "/loop", "/loop");
    let _ = resolve(&sys, Path::new("/loop/x"));
}
