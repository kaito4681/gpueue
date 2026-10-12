use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use puzuz_protocol::api::{HelperStatus, VersionResponse};

use super::*;
use crate::service::MARKER;
use crate::system::CmdOutput;
use crate::system::fake::FakeSystem;

// A throwaway key pair (its secret key was not kept), another key, and data signed with the first.
const TEST_PUB: &str = include_str!("testdata/test.pub");
const OTHER_PUB: &str = include_str!("testdata/other.pub");
const DATA: &[u8] = b"puzuz release data\n";
const DATA_SIG: &str = "untrusted comment: signature from minisign secret key
RUQJYNONsWvmxH7NcJ5/zM7w2vMx8mrcqlcK6iqgbqU//Tz0Vb4SuiGE7kpDnNz3jlEOJ3GEB3tZLjoGgpGWzaAE/1QqcDNaEAI=
trusted comment: test signature
r2jL3bJ9M/08nEyswJfikSJ/eqvJhLUQRUn67t84/PtT6lMl3qwb1eyL7NFf0x8qFOtFFG7PtQhBx7i0z12lCw==
";

// A release v9.0.0 holding two small scripts, signed with TEST_PUB's key.
const NAME: &str = "puzuz-v9.0.0-x86_64-unknown-linux-gnu";
const TARBALL: &[u8] = include_bytes!("testdata/puzuz-v9.0.0-x86_64-unknown-linux-gnu.tar.gz");
const TARBALL_SIG: &[u8] =
    include_bytes!("testdata/puzuz-v9.0.0-x86_64-unknown-linux-gnu.tar.gz.minisig");

const API: &str = "http://fake-api";
const REPO_API: &str = "http://fake-api/repos/kaito4681/puzuz";

fn sha256_line(data: &[u8], name: &str) -> String {
    let hex: String = Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("{hex}  {name}\n")
}

fn release_json(tag: &str) -> String {
    let name = format!("puzuz-{tag}-{TARGET}.tar.gz");
    serde_json::json!({
        "tag_name": tag,
        "body": "notes, with {braces}",
        "assets": [
            {"id": 11, "name": name, "url": format!("{REPO_API}/releases/assets/11")},
            {"id": 12, "name": format!("{name}.sha256")},
            {"id": 13, "name": format!("{name}.minisig")},
            {"id": 14, "name": "install.sh"},
        ],
    })
    .to_string()
}

/// A root-run system where `service install` installed puzuz into /usr/local/bin, the daemon
/// runs this version, and the fake API's latest release is v9.0.0.
fn system() -> FakeSystem {
    let mut sys = FakeSystem::new();
    sys.exe = PathBuf::from("/usr/local/bin/puzuz");
    sys.write_exe("/usr/local/bin/puzuz", "old puzuz");
    sys.write(
        "/etc/systemd/system/puzuz.service",
        &format!("{MARKER}\n[Service]\nExecStart=/usr/local/bin/puzuz daemon\n"),
    );
    sys.mkdir("/var/lib/puzuz");
    sys.set_env("PUZUZ_RELEASE_API_URL", API);
    sys.set_env("GH_TOKEN", "secret-token");
    sys.write("/test.pub", TEST_PUB);
    sys.set_env(
        "PUZUZ_UPDATE_PUBKEY_FILE",
        &sys.path(Path::new("/test.pub")).to_string_lossy(),
    );
    sys.serve(
        &format!("{REPO_API}/releases/latest"),
        200,
        release_json("v9.0.0"),
    );
    sys.serve(&format!("{REPO_API}/releases/assets/11"), 200, TARBALL);
    sys.serve(
        &format!("{REPO_API}/releases/assets/12"),
        200,
        sha256_line(TARBALL, &format!("{NAME}.tar.gz")),
    );
    sys.serve(&format!("{REPO_API}/releases/assets/13"), 200, TARBALL_SIG);
    sys.respond(
        "puzuz service version-info",
        CmdOutput::ok(
            serde_json::to_string(&VersionInfo {
                version: "9.0.0".to_owned(),
                schema_version: puzuz_core::storage::SCHEMA_VERSION,
            })
            .unwrap(),
        ),
    );
    sys.respond("puzuz service install", CmdOutput::ok("installed it\n"));
    sys.daemon = daemon("9.0.0", Some(puzuz_core::storage::SCHEMA_VERSION));
    sys
}

fn daemon(version: &str, schema: Option<i64>) -> Result<VersionResponse, String> {
    Ok(VersionResponse {
        api_version: 1,
        daemon_version: version.to_owned(),
        helper: HelperStatus {
            connected: true,
            version: Some(version.to_owned()),
        },
        checkpoints: None,
        gpu_detection: None,
        schema_version: schema,
    })
}

fn opts(check: bool, version: Option<&str>) -> UpdateOptions {
    UpdateOptions {
        check,
        version: version.map(str::to_owned),
        yes: true,
    }
}

fn run(sys: &FakeSystem, options: &UpdateOptions) -> Result<String> {
    let mut out = Vec::new();
    update(sys, options, &mut out)?;
    Ok(String::from_utf8(out).unwrap())
}

fn installed(sys: &FakeSystem) -> bool {
    sys.ran("puzuz service install")
}

#[test]
fn signatures_are_checked() {
    verify_signature(TEST_PUB, DATA, DATA_SIG.as_bytes()).unwrap();
    // The bare base64 key works too.
    verify_signature(TEST_PUB.lines().nth(1).unwrap(), DATA, DATA_SIG.as_bytes()).unwrap();
    let err = verify_signature(OTHER_PUB, DATA, DATA_SIG.as_bytes()).unwrap_err();
    assert!(err.contains("bad signature"), "{err}");
    let err = verify_signature(TEST_PUB, b"tampered\n", DATA_SIG.as_bytes()).unwrap_err();
    assert!(err.contains("bad signature"), "{err}");
    let err = verify_signature(TEST_PUB, DATA, b"garbage").unwrap_err();
    assert!(err.contains("invalid signature"), "{err}");
}

#[test]
fn the_built_in_key_is_a_minisign_key() {
    minisign_verify::PublicKey::decode(RELEASE_PUBKEY).unwrap();
}

#[test]
fn checksums_are_checked() {
    let sums = sha256_line(DATA, "a.tar.gz");
    verify_sha256(DATA, sums.as_bytes(), "a.tar.gz").unwrap();
    // `sha256sum -b` marks binary files with `*`.
    verify_sha256(DATA, sums.replace("  ", " *").as_bytes(), "a.tar.gz").unwrap();
    let err = verify_sha256(b"other", sums.as_bytes(), "a.tar.gz").unwrap_err();
    assert!(err.contains("mismatch"), "{err}");
    let err = verify_sha256(DATA, sums.as_bytes(), "b.tar.gz").unwrap_err();
    assert!(err.contains("no line for b.tar.gz"), "{err}");
}

#[test]
fn versions_parse_with_or_without_v() {
    assert_eq!(parse_version("v0.2.0").unwrap(), Version::new(0, 2, 0));
    assert_eq!(parse_version("0.2.0").unwrap(), Version::new(0, 2, 0));
    assert!(parse_version("latest").is_err());
    // Test builds order as semver says.
    assert!(parse_version("0.1.0-test.1").unwrap() < parse_version("0.1.0-test.2").unwrap());
    assert!(parse_version("0.1.0-test.2").unwrap() < parse_version("0.1.0").unwrap());
}

#[test]
fn update_source_must_be_a_github_repository() {
    let mut config = Config::default();
    assert_eq!(source_repo(&config).unwrap(), "kaito4681/puzuz");
    config.update_source = "github:someone/fork".to_owned();
    assert_eq!(source_repo(&config).unwrap(), "someone/fork");
    for bad in [
        "https://example.com",
        "github:",
        "github:a/b/c",
        "github:a b/c",
    ] {
        config.update_source = bad.to_owned();
        assert!(source_repo(&config).is_err(), "{bad}");
    }
}

/// A gzipped tarball with the given entries: (path, type, contents or link target).
fn tarball(entries: &[(&str, tar::EntryType, &str)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    for (path, kind, data) in entries {
        let mut header = tar::Header::new_gnu();
        // Written by hand: set_path refuses `..` and absolute paths.
        let name = &mut header.as_old_mut().name;
        name[..path.len()].copy_from_slice(path.as_bytes());
        header.set_entry_type(*kind);
        header.set_mode(0o4755);
        if kind.is_symlink() || kind.is_hard_link() {
            header.set_link_name(data).unwrap();
            header.set_size(0);
            header.set_cksum();
            builder.append(&header, std::io::empty()).unwrap();
        } else {
            header.set_size(data.len() as u64);
            header.set_cksum();
            builder.append(&header, data.as_bytes()).unwrap();
        }
    }
    builder.into_inner().unwrap().finish().unwrap()
}

#[test]
fn extraction_accepts_files_and_directories_below_the_top_directory() {
    let dir = tempfile::tempdir().unwrap();
    let data = tarball(&[
        ("top/", tar::EntryType::Directory, ""),
        ("top/puzuz", tar::EntryType::Regular, "binary"),
        ("top/docs/a.md", tar::EntryType::Regular, "doc"),
    ]);
    let top = extract(&data, dir.path(), "top").unwrap();
    assert_eq!(std::fs::read(top.join("puzuz")).unwrap(), b"binary");
    assert_eq!(std::fs::read(top.join("docs/a.md")).unwrap(), b"doc");
    // The setuid bit is dropped.
    let mode = std::fs::metadata(top.join("puzuz")).unwrap().mode() & 0o7777;
    assert_eq!(mode, 0o755);

    let dir = tempfile::tempdir().unwrap();
    let top = extract(TARBALL, dir.path(), NAME).unwrap();
    assert!(top.join("puzuz").is_file());
    assert!(top.join("puzuz-helper").is_file());
}

#[test]
fn extraction_refuses_escapes_and_links() {
    let cases: &[(&str, tar::EntryType, &str, &str)] = &[
        (
            "top/../evil",
            tar::EntryType::Regular,
            "x",
            "unexpected path",
        ),
        ("/etc/evil", tar::EntryType::Regular, "x", "unexpected path"),
        (
            "other/puzuz",
            tar::EntryType::Regular,
            "x",
            "unexpected path",
        ),
        (
            "top/link",
            tar::EntryType::Symlink,
            "/etc/shadow",
            "link or special",
        ),
        (
            "top/hard",
            tar::EntryType::Link,
            "top/puzuz",
            "link or special",
        ),
        ("top/fifo", tar::EntryType::Fifo, "", "link or special"),
    ];
    for (path, kind, data, expected) in cases {
        let dir = tempfile::tempdir().unwrap();
        let archive = tarball(&[
            ("top/puzuz", tar::EntryType::Regular, "binary"),
            (path, *kind, data),
        ]);
        let err = extract(&archive, &dir.path().join("x"), "top").unwrap_err();
        assert!(err.contains(expected), "{path}: {err}");
        assert!(!dir.path().join("evil").exists());
    }
    let dir = tempfile::tempdir().unwrap();
    let err = extract(b"not a tarball", dir.path(), "top").unwrap_err();
    assert!(err.contains("refusing the release tarball"), "{err}");
}

#[test]
fn check_reports_a_newer_release() {
    let sys = system();
    let out = run(&sys, &opts(true, None)).unwrap();
    assert!(out.contains("puzuz 9.0.0 is available"), "{out}");
    assert!(out.contains("sudo puzuz update"), "{out}");
    assert!(!installed(&sys));
    // The token is sent as a header, to the API only.
    let log = sys.http_log.borrow();
    let (url, headers) = &log[0];
    assert_eq!(url, &format!("{REPO_API}/releases/latest"));
    assert!(headers.contains(&("Authorization".to_owned(), "Bearer secret-token".to_owned())));
    assert!(!out.contains("secret-token"));
}

#[test]
fn without_root_update_only_checks() {
    let mut sys = system();
    sys.euid = 1000;
    let out = run(&sys, &opts(false, None)).unwrap();
    assert!(out.contains("puzuz 9.0.0 is available"), "{out}");
    assert!(sys.http_log.borrow().len() == 1, "downloaded something");
    assert!(!installed(&sys));
}

#[test]
fn nothing_to_do_when_up_to_date() {
    let sys = system();
    let current = current_version();
    sys.serve(
        &format!("{REPO_API}/releases/latest"),
        200,
        release_json(&format!("v{current}")),
    );
    let out = run(&sys, &opts(false, None)).unwrap();
    assert!(out.contains("is up to date"), "{out}");
    sys.serve(
        &format!("{REPO_API}/releases/latest"),
        200,
        release_json("v0.0.1"),
    );
    let out = run(&sys, &opts(false, None)).unwrap();
    assert!(out.contains("newer than the latest release"), "{out}");
    assert!(!installed(&sys));
}

#[test]
fn missing_or_bad_tokens_are_explained() {
    let sys = system();
    sys.env.borrow_mut().remove("GH_TOKEN");
    sys.serve(&format!("{REPO_API}/releases/latest"), 404, "{}");
    let err = run(&sys, &opts(true, None)).unwrap_err();
    assert!(
        err.contains("A GitHub token that can read kaito4681/puzuz"),
        "{err}"
    );
    assert!(
        sys.http_log.borrow()[0]
            .1
            .iter()
            .all(|(k, _)| k != "Authorization")
    );

    sys.set_env("GITHUB_TOKEN", "expired");
    let err = run(&sys, &opts(true, None)).unwrap_err();
    assert!(
        err.contains("The token from GITHUB_TOKEN cannot read"),
        "{err}"
    );
    assert!(!err.contains("expired\""), "{err}");

    sys.serve(&format!("{REPO_API}/releases/tags/v1.2.3"), 404, "{}");
    let err = run(&sys, &opts(true, Some("1.2.3"))).unwrap_err();
    assert_eq!(err, "release v1.2.3 not found in kaito4681/puzuz");
}

#[test]
fn the_saved_token_must_be_root_only() {
    let sys = system();
    sys.env.borrow_mut().remove("GH_TOKEN");
    sys.write(TOKEN_FILE, "saved-token\n");
    let local = sys.path(Path::new(TOKEN_FILE));
    std::fs::set_permissions(&local, std::fs::Permissions::from_mode(0o644)).unwrap();
    let err = run(&sys, &opts(true, None)).unwrap_err();
    assert!(err.contains("mode 0600"), "{err}");

    std::fs::set_permissions(&local, std::fs::Permissions::from_mode(0o600)).unwrap();
    run(&sys, &opts(true, None)).unwrap();
    let log = sys.http_log.borrow();
    assert!(
        log.last()
            .unwrap()
            .1
            .contains(&("Authorization".to_owned(), "Bearer saved-token".to_owned()))
    );
}

#[test]
fn update_installs_the_verified_release() {
    let sys = system();
    let out = run(&sys, &opts(false, None)).unwrap();
    assert!(out.contains("verified the signature"), "{out}");
    assert!(out.contains("verified the SHA-256 checksum"), "{out}");
    assert!(out.contains("installed it"), "{out}");
    assert!(out.contains("puzuz is now 9.0.0."), "{out}");
    let install = sys
        .commands()
        .into_iter()
        .find(|line| line.starts_with("puzuz service install"))
        .unwrap();
    assert_eq!(install, "puzuz service install --bin-dir /usr/local/bin");
    // The work directory is gone afterwards.
    assert!(!sys.exists(WORK_DIR));
}

#[test]
fn update_asks_first() {
    let sys = system();
    let options = UpdateOptions {
        yes: false,
        ..opts(false, None)
    };
    let err = run(&sys, &options).unwrap_err();
    assert!(err.contains("aborted"), "{err}");
    assert!(sys.prompts.borrow()[0].contains("Update puzuz from"));
    assert!(!installed(&sys));
}

#[test]
fn a_bad_signature_or_checksum_is_refused() {
    let sys = system();
    sys.write("/other.pub", OTHER_PUB);
    sys.set_env(
        "PUZUZ_UPDATE_PUBKEY_FILE",
        &sys.path(Path::new("/other.pub")).to_string_lossy(),
    );
    let err = run(&sys, &opts(false, None)).unwrap_err();
    assert!(err.contains("bad signature"), "{err}");
    assert!(!installed(&sys));
    assert!(!sys.ran("puzuz service version-info"));

    let sys = system();
    sys.serve(
        &format!("{REPO_API}/releases/assets/12"),
        200,
        sha256_line(b"other", &format!("{NAME}.tar.gz")),
    );
    let err = run(&sys, &opts(false, None)).unwrap_err();
    assert!(err.contains("checksum mismatch"), "{err}");
    assert!(!installed(&sys));
}

#[test]
fn a_release_that_cannot_read_the_database_is_refused() {
    let mut sys = system();
    sys.daemon = daemon(
        puzuz_protocol::BINARY_VERSION,
        Some(puzuz_core::storage::SCHEMA_VERSION + 1),
    );
    let err = run(&sys, &opts(false, None)).unwrap_err();
    assert!(err.contains("the database is at schema"), "{err}");
    assert!(!installed(&sys));
}

#[test]
fn a_release_with_another_version_inside_is_refused() {
    let sys = system();
    sys.respond(
        "puzuz service version-info",
        CmdOutput::ok(r#"{"version":"8.0.0","schema_version":1}"#),
    );
    let err = run(&sys, &opts(false, None)).unwrap_err();
    assert!(err.contains("reports version 8.0.0"), "{err}");
    assert!(!installed(&sys));
}

#[test]
fn update_waits_for_the_new_daemon() {
    let mut sys = system();
    sys.daemon = daemon(puzuz_protocol::BINARY_VERSION, Some(1));
    let err = run(&sys, &opts(false, None)).unwrap_err();
    assert!(err.contains("the daemon is not running it"), "{err}");
    assert!(installed(&sys));
}

#[test]
fn only_service_install_is_updated() {
    let sys = system();
    sys.write(
        "/usr/lib/systemd/system/puzuz.service",
        "[Service]\nExecStart=/usr/bin/puzuz daemon\n",
    );
    std::fs::remove_file(sys.path(Path::new("/etc/systemd/system/puzuz.service"))).unwrap();
    let err = run(&sys, &opts(false, None)).unwrap_err();
    assert!(err.contains("installed as distribution package"), "{err}");
    assert!(err.contains("package manager"), "{err}");
    // Checking still works and names the right tool.
    let out = run(&sys, &opts(true, None)).unwrap();
    assert!(out.contains("package manager"), "{out}");

    let sys = system();
    std::fs::remove_file(sys.path(Path::new("/etc/systemd/system/puzuz.service"))).unwrap();
    let err = run(&sys, &opts(false, None)).unwrap_err();
    assert!(err.contains("not installed as a service"), "{err}");
}

#[test]
fn update_must_run_the_installed_binary() {
    let mut sys = system();
    sys.exe = PathBuf::from("/home/u/puzuz");
    let err = run(&sys, &opts(false, None)).unwrap_err();
    assert!(
        err.contains("run `sudo /usr/local/bin/puzuz update`"),
        "{err}"
    );
    assert!(!installed(&sys));
}

#[test]
fn doctor_reports_a_newer_release_as_info() {
    let sys = system();
    let config = Config::default();
    assert_eq!(
        newer_release(&sys, &config).unwrap(),
        Some(Version::new(9, 0, 0))
    );
    assert_eq!(update_hint(&sys), "sudo puzuz update");
    sys.serve(&format!("{REPO_API}/releases/latest"), 500, "");
    assert!(newer_release(&sys, &config).is_err());
}
