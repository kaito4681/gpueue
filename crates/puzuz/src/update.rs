//! `puzuz update`: replaces an installation made by `puzuz service install` with a release from
//! GitHub, and the "update" check of `puzuz doctor`.
//!
//! Releases are read through the GitHub API, anonymously or, when one is given, with a token
//! (`GH_TOKEN`/`GITHUB_TOKEN`, or `/etc/puzuz/github-token` saved by `install.sh --save-token`).
//! The repository is public, so the token is optional; it raises the API rate limit.
//!
//! An update downloads the release tarball, its `.sha256` and its `.minisig` into
//! `/var/lib/puzuz/update` (root, 0700), checks the signature with the key built into this
//! binary ([`crate::release_key`]) and the checksum, extracts the tarball (regular files and
//! directories only), asks the new binary which database schema it supports, refuses a release
//! that would not open the database, and runs the new binary's `service install --bin-dir <the
//! installed directory>`. Running jobs are not affected (see `service install`).
//!
//! Test-only overrides, not shown in `--help`: `PUZUZ_RELEASE_API_URL` (a fake API) and
//! `PUZUZ_UPDATE_PUBKEY_FILE` (another public key).

use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use puzuz_daemon::config::Config;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::doctor;
use crate::install_method::{self, Detection, InstallMethod, Method, Owner};
use crate::release_key::RELEASE_PUBKEY;
use crate::system::System;

/// The release target of this build: releases are built for x86_64 and aarch64. Unit tests use
/// their x86_64 test release on every architecture.
pub const TARGET: &str = if cfg!(all(target_arch = "aarch64", not(test))) {
    "aarch64-unknown-linux-gnu"
} else if cfg!(any(target_arch = "x86_64", test)) {
    "x86_64-unknown-linux-gnu"
} else {
    // No release exists; `puzuz update` then reports the missing asset.
    "unsupported-architecture"
};
/// Where `install.sh --save-token` saves the token (root, 0600).
pub const TOKEN_FILE: &str = "/etc/puzuz/github-token";
/// Where releases are downloaded and extracted; removed afterwards.
pub const WORK_DIR: &str = "/var/lib/puzuz/update";
const DEFAULT_API_URL: &str = "https://api.github.com";
const API_URL_ENV: &str = "PUZUZ_RELEASE_API_URL";
const PUBKEY_FILE_ENV: &str = "PUZUZ_UPDATE_PUBKEY_FILE";

const API_TIMEOUT: Duration = Duration::from_secs(20);
/// `puzuz doctor` must stay quick when GitHub is unreachable.
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
/// How long to wait for the new daemon after installing it.
const RESTART_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// A failed update; the message is meant for the user.
pub type Result<T, E = String> = std::result::Result<T, E>;

/// This binary's version.
pub fn current_version() -> Version {
    Version::parse(puzuz_protocol::BINARY_VERSION).expect("the crate version is semver")
}

/// Parses a version given by the user (`0.2.0` or `v0.2.0`).
pub fn parse_version(text: &str) -> Result<Version> {
    let text = text.trim();
    Version::parse(text.strip_prefix('v').unwrap_or(text))
        .map_err(|err| format!("{text:?} is not a version like 0.2.0: {err}"))
}

/// What `puzuz service version-info` prints: what a binary is and supports. `puzuz update` asks
/// the new binary before installing it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionInfo {
    pub version: String,
    /// The database schema version it reads and writes (`PRAGMA user_version`).
    pub schema_version: i64,
}

impl VersionInfo {
    pub fn this_binary() -> Self {
        Self {
            version: puzuz_protocol::BINARY_VERSION.to_owned(),
            schema_version: puzuz_core::storage::SCHEMA_VERSION,
        }
    }
}

/// [`source_repo`] for messages: the default repository when `update_source` is invalid
/// (fetching a release reports that).
fn release_repo(config: &Config) -> String {
    source_repo(config).unwrap_or_else(|_| {
        source_repo(&Config::default()).expect("the default update_source is valid")
    })
}

/// The GitHub repository releases come from (`update_source = "github:<owner>/<repo>"`).
fn source_repo(config: &Config) -> Result<String> {
    let source = config.update_source.trim();
    let repo = source.strip_prefix("github:").ok_or_else(|| {
        format!("update_source = {source:?} is not supported; use \"github:<owner>/<repo>\"")
    })?;
    let valid = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    };
    match repo.split_once('/') {
        Some((owner, name)) if valid(owner) && valid(name) => Ok(repo.to_owned()),
        _ => Err(format!(
            "update_source = {source:?} is not \"github:<owner>/<repo>\""
        )),
    }
}

/// A GitHub token and where it came from (the token itself is never shown).
struct Token {
    value: String,
    source: &'static str,
}

/// The token to use: `GH_TOKEN`, `GITHUB_TOKEN`, or the root-only [`TOKEN_FILE`].
fn find_token(sys: &dyn System) -> Result<Option<Token>> {
    for var in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Some(value) = sys.env_var(var).filter(|v| !v.trim().is_empty()) {
            return Ok(Some(Token {
                value: value.trim().to_owned(),
                source: var,
            }));
        }
    }
    let local = sys.path(Path::new(TOKEN_FILE));
    let meta = match std::fs::symlink_metadata(&local) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("{TOKEN_FILE}: {err}")),
    };
    if !meta.is_file() || meta.uid() != sys.root_uid() || meta.mode() & 0o077 != 0 {
        return Err(format!(
            "{TOKEN_FILE} must be a file owned by root with mode 0600 (it holds a token); \
             fix it with `sudo chown root:root {TOKEN_FILE}; sudo chmod 600 {TOKEN_FILE}`"
        ));
    }
    match std::fs::read_to_string(&local) {
        Ok(text) => Ok(text
            .lines()
            .next()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(|value| Token {
                value: value.to_owned(),
                source: TOKEN_FILE,
            })),
        // Not root: the file is root's.
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => Ok(None),
        Err(err) => Err(format!("{TOKEN_FILE}: {err}")),
    }
}

/// A release, as the GitHub API returns it.
#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub id: u64,
    pub name: String,
}

impl Release {
    pub fn version(&self) -> Result<Version> {
        parse_version(&self.tag_name).map_err(|err| format!("release {}: {err}", self.tag_name))
    }

    /// Base name of the release tarball (and of the directory inside it).
    pub fn archive_name(&self) -> String {
        format!("puzuz-{}-{TARGET}", self.tag_name)
    }

    fn asset(&self, name: &str) -> Result<&Asset> {
        self.assets
            .iter()
            .find(|asset| asset.name == name)
            .ok_or_else(|| format!("release {} has no {name}", self.tag_name))
    }
}

/// Reads releases of one repository through the GitHub API.
pub struct Client<'a> {
    sys: &'a dyn System,
    api: String,
    repo: String,
    token: Option<Token>,
}

impl<'a> Client<'a> {
    pub fn new(sys: &'a dyn System, config: &Config) -> Result<Self> {
        let api = sys
            .env_var(API_URL_ENV)
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| DEFAULT_API_URL.to_owned());
        Ok(Self {
            sys,
            api: api.trim_end_matches('/').to_owned(),
            repo: source_repo(config)?,
            token: find_token(sys)?,
        })
    }

    fn get(&self, path: &str, accept: &str, timeout: Duration) -> Result<Vec<u8>, (u16, String)> {
        let url = format!("{}{path}", self.api);
        let auth = self
            .token
            .as_ref()
            .map(|token| format!("Bearer {}", token.value));
        let mut headers = vec![("Accept", accept), ("X-GitHub-Api-Version", "2022-11-28")];
        if let Some(auth) = &auth {
            headers.push(("Authorization", auth));
        }
        let response = self
            .sys
            .http_get(&url, &headers, timeout)
            .map_err(|err| (0, format!("cannot reach {}: {err}", self.api)))?;
        match response.status {
            200 => Ok(response.body),
            401 | 403 | 404 => Err((response.status, self.no_access(path, response.status))),
            status => Err((status, format!("GET {path}: HTTP {status}"))),
        }
    }

    /// Explains a 401/403/404. GitHub answers 401 for a bad token, 403 when the API rate limit is
    /// used up, and 404 for a release or repository that does not exist (or that a token cannot
    /// read).
    fn no_access(&self, path: &str, status: u16) -> String {
        match (&self.token, status) {
            (Some(token), _) => format!(
                "GET {path}: HTTP {status} with the token from {}: it may be expired or revoked, \
                 or {} has no published release. A token is optional; without it the release is \
                 downloaded anonymously",
                token.source, self.repo
            ),
            (None, 404) => format!(
                "GET {path}: HTTP 404. {} has no published release (or does not exist)",
                self.repo
            ),
            (None, _) => format!(
                "GET {path}: HTTP {status}. GitHub refused the anonymous request, usually because \
                 of its API rate limit; try again later, or use a GitHub token: set GH_TOKEN \
                 (`sudo GH_TOKEN=... puzuz update`), or save one in {TOKEN_FILE} with \
                 `install.sh --save-token`"
            ),
        }
    }

    /// The latest release, or the one for `version`.
    pub fn release(&self, version: Option<&Version>, timeout: Duration) -> Result<Release> {
        let path = match version {
            Some(version) => format!("/repos/{}/releases/tags/v{version}", self.repo),
            None => format!("/repos/{}/releases/latest", self.repo),
        };
        let body = self
            .get(&path, "application/vnd.github+json", timeout)
            .map_err(|(status, err)| match version {
                Some(version) if status == 404 => {
                    format!("release v{version} not found in {}", self.repo)
                }
                _ => err,
            })?;
        serde_json::from_slice(&body).map_err(|err| format!("GET {path}: unexpected answer: {err}"))
    }

    /// Downloads the asset `name` of `release`.
    pub fn download(&self, release: &Release, name: &str) -> Result<Vec<u8>> {
        let asset = release.asset(name)?;
        let path = format!("/repos/{}/releases/assets/{}", self.repo, asset.id);
        self.get(&path, "application/octet-stream", DOWNLOAD_TIMEOUT)
            .map_err(|(_, err)| format!("downloading {name}: {err}"))
    }
}

/// The public key releases must be signed with.
fn public_key(sys: &dyn System) -> Result<String> {
    match sys.env_var(PUBKEY_FILE_ENV).filter(|p| !p.is_empty()) {
        Some(path) => {
            std::fs::read_to_string(&path).map_err(|err| format!("{PUBKEY_FILE_ENV}={path}: {err}"))
        }
        None => Ok(RELEASE_PUBKEY.to_owned()),
    }
}

/// Checks the minisign signature `signature` (a `.minisig` file) of `data`.
pub fn verify_signature(public_key: &str, data: &[u8], signature: &[u8]) -> Result<()> {
    let key = if public_key.trim().lines().count() == 1 {
        minisign_verify::PublicKey::from_base64(public_key.trim())
    } else {
        minisign_verify::PublicKey::decode(public_key)
    }
    .map_err(|err| format!("invalid release public key: {err}"))?;
    let signature = std::str::from_utf8(signature)
        .map_err(|_| "the signature file is not text".to_owned())
        .and_then(|text| {
            minisign_verify::Signature::decode(text)
                .map_err(|err| format!("invalid signature file: {err}"))
        })?;
    key.verify(data, &signature, false)
        .map_err(|err| format!("bad signature: {err}"))
}

/// Checks `data` against a `sha256sum` line for `name` in `sums`.
pub fn verify_sha256(data: &[u8], sums: &[u8], name: &str) -> Result<()> {
    let sums = String::from_utf8_lossy(sums);
    let expected = sums
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            let hash = fields.next()?;
            let file = fields.next()?.trim_start_matches('*');
            (file == name).then(|| hash.to_ascii_lowercase())
        })
        .ok_or_else(|| format!("the checksum file has no line for {name}"))?;
    let actual: String = Sha256::digest(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if actual == expected {
        Ok(())
    } else {
        Err(format!("SHA-256 checksum mismatch for {name}"))
    }
}

/// Extracts the gzipped tarball `data` into the local directory `dest`. Only regular files and
/// directories below the single top directory `top` are accepted: no absolute paths, no `..`, no
/// links or devices. File modes keep only the permission bits up to 0755. Returns `dest/top`.
pub fn extract(data: &[u8], dest: &Path, top: &str) -> Result<PathBuf> {
    let bad = |what: String| format!("refusing the release tarball: {what}");
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(data));
    let entries = archive
        .entries()
        .map_err(|err| bad(format!("cannot read it: {err}")))?;
    for entry in entries {
        let mut entry = entry.map_err(|err| bad(format!("cannot read it: {err}")))?;
        let path = entry
            .path()
            .map_err(|err| bad(format!("bad path: {err}")))?
            .into_owned();
        let mut components = path.components();
        let normal = path
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
        if !normal || components.next() != Some(Component::Normal(top.as_ref())) {
            return Err(bad(format!("unexpected path {}", path.display())));
        }
        let kind = entry.header().entry_type();
        let target = dest.join(&path);
        if kind.is_dir() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o755)
                .create(&target)
                .map_err(|err| format!("creating {}: {err}", target.display()))?;
        } else if kind.is_file() {
            if let Some(parent) = target.parent() {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o755)
                    .create(parent)
                    .map_err(|err| format!("creating {}: {err}", parent.display()))?;
            }
            let mode = entry.header().mode().unwrap_or(0o644) & 0o755;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .custom_flags(nix::libc::O_NOFOLLOW)
                .open(&target)
                .map_err(|err| format!("creating {}: {err}", target.display()))?;
            io::copy(&mut entry, &mut file)
                .map_err(|err| format!("writing {}: {err}", target.display()))?;
        } else {
            return Err(bad(format!("{} is a link or special file", path.display())));
        }
    }
    let top_dir = dest.join(top);
    if !top_dir.is_dir() {
        return Err(bad(format!("no directory {top}")));
    }
    Ok(top_dir)
}

pub struct UpdateOptions {
    pub check: bool,
    pub version: Option<String>,
    pub yes: bool,
}

/// How to update puzuz installed as `detection` says, for messages. `repo` is where releases
/// come from (`update_source`); `version` the release to update to, when known.
fn how_to_update(detection: &Detection, version: Option<&Version>, repo: &str) -> String {
    match &detection.install {
        InstallMethod::One(Method::ServiceInstall { .. }) if !detection.is_conflicting() => {
            match version {
                Some(version) => format!("sudo puzuz update --version {version}"),
                None => "sudo puzuz update".to_owned(),
            }
        }
        InstallMethod::One(Method::ServiceInstall { .. }) | InstallMethod::Mixed(_) => {
            "puzuz is installed in more than one way; run `puzuz doctor` first".to_owned()
        }
        // Our own .rpm, attached to the releases: there is no dnf repository yet (issue #23),
        // so a plain `dnf upgrade` would not find a newer version.
        InstallMethod::One(Method::Package {
            owner: Owner::Rpm(name),
            ..
        }) if name == "puzuz" => rpm_update_hint(repo, version),
        InstallMethod::One(method) => detection.update_hint(method),
        InstallMethod::None if detection.os.is_nixos() => {
            "install it with the `services.puzuz` NixOS module (see docs/install.md)".to_owned()
        }
        InstallMethod::None => "install it with install.sh (see docs/install.md)".to_owned(),
    }
}

/// How to update the puzuz .rpm of the GitHub releases of `repo` to `version` (or to the
/// latest release), and how to check its signature first.
fn rpm_update_hint(repo: &str, version: Option<&Version>) -> String {
    let arch = std::env::consts::ARCH;
    let (version, latest) = match version {
        Some(version) => (version.to_string(), String::new()),
        None => (
            "<version>".to_owned(),
            format!(" with the version of https://github.com/{repo}/releases/latest"),
        ),
    };
    let base = format!("https://github.com/{repo}/releases/download/v{version}");
    format!(
        "run `sudo dnf upgrade {base}/puzuz-{version}-1.{arch}.rpm`{latest} (there is no dnf \
         repository yet). To check the package signature first, run `sudo rpm --import \
         {base}/RPM-GPG-KEY-puzuz` and add `--setopt=localpkg_gpgcheck=1`"
    )
}

/// Where the binaries of a `service install` installation are, or why `puzuz update` must not
/// install over this one.
fn service_install_bin_dir(detection: &Detection, repo: &str) -> Result<PathBuf> {
    let method = &detection.install;
    if let (InstallMethod::One(Method::ServiceInstall { .. }), Some(managed)) =
        (method, detection.managed_elsewhere())
    {
        return Err(format!(
            "puzuz is installed in more than one way ({}; one of them is {managed}); run \
             `puzuz doctor` and remove all but one first",
            detection.partial.as_deref().unwrap_or("its units disagree")
        ));
    }
    match method {
        InstallMethod::One(Method::ServiceInstall { bin_dir: Some(dir) }) => Ok(dir.clone()),
        InstallMethod::One(Method::ServiceInstall { bin_dir: None }) => Err(
            "cannot tell from /etc/systemd/system/puzuz.service where the binaries are; \
                 re-run `sudo puzuz service install --bin-dir /usr/local/bin` first"
                .to_owned(),
        ),
        InstallMethod::None => Err(format!(
            "puzuz is not installed as a service; {}",
            how_to_update(detection, None, repo)
        )),
        InstallMethod::One(other) => Err(format!(
            "puzuz is installed as {other}; `puzuz update` only updates what \
                 `puzuz service install` installed. To update it, {}.",
            how_to_update(detection, None, repo)
        )),
        InstallMethod::Mixed(_) => Err(format!(
            "puzuz is installed in more than one way ({method}); run `puzuz doctor` and \
                 remove all but one first"
        )),
    }
}

/// `puzuz update`. Without root it only checks (like `--check`).
pub fn update(sys: &dyn System, options: &UpdateOptions, out: &mut dyn Write) -> Result<()> {
    let config = doctor::load_config(sys).config;
    let wanted = options.version.as_deref().map(parse_version).transpose()?;
    let current = current_version();
    let method = install_method::detect(sys);
    let repo = release_repo(&config);
    // Installing needs an installation this command may change, run from its binary: refuse
    // anything else before contacting GitHub, so that the error names the right way to update.
    // Checking (`--check`, or without root) only reports.
    let bin_dir = if options.check || sys.euid() != 0 {
        None
    } else {
        let bin_dir = service_install_bin_dir(&method, &repo)?;
        check_running_installed_binary(sys, &bin_dir)?;
        Some(bin_dir)
    };
    let client = Client::new(sys, &config)?;
    let release = client.release(wanted.as_ref(), API_TIMEOUT)?;
    let target = release.version()?;

    if target == current {
        writeln!(out, "puzuz {current} is up to date.").ok();
        return Ok(());
    }
    if wanted.is_none() && target < current {
        writeln!(
            out,
            "puzuz {current} is newer than the latest release ({target}); nothing to do."
        )
        .ok();
        return Ok(());
    }
    let downgrade = target < current;
    let Some(bin_dir) = bin_dir else {
        writeln!(
            out,
            "puzuz {target} is available (installed: {current}{}).",
            if downgrade { "; it is older" } else { "" }
        )
        .ok();
        writeln!(
            out,
            "To install it: {}",
            how_to_update(&method, wanted.as_ref().or(Some(&target)), &repo)
        )
        .ok();
        return Ok(());
    };

    if !options.yes {
        let verb = if downgrade { "Downgrade" } else { "Update" };
        let prompt = format!(
            "{verb} puzuz from {current} to {target}? Running jobs keep running; the services \
             restart."
        );
        if !sys.confirm(&prompt) {
            return Err("aborted; nothing was changed".to_owned());
        }
    }

    let work = Path::new(WORK_DIR);
    prepare_work_dir(sys, work)?;
    let result = install_release(
        sys,
        &InstallPlan {
            client: &client,
            config: &config,
            release: &release,
            target: &target,
            downgrade,
            bin_dir: &bin_dir,
            work,
        },
        out,
    );
    let _ = std::fs::remove_dir_all(sys.path(work));
    result
}

/// Refuses to run from anywhere but the installed binary: its version is what is compared, and
/// a copy elsewhere would be left behind.
fn check_running_installed_binary(sys: &dyn System, bin_dir: &Path) -> Result<()> {
    let exe = sys
        .current_exe()
        .map_err(|err| format!("cannot locate the running puzuz binary: {err}"))?;
    let installed = bin_dir.join("puzuz");
    let same = exe == installed
        || matches!(
            (
                std::fs::canonicalize(sys.path(&exe)),
                std::fs::canonicalize(sys.path(&installed)),
            ),
            (Ok(a), Ok(b)) if a == b
        );
    if same {
        Ok(())
    } else {
        Err(format!(
            "this is {}, but the services run {}; run `sudo {} update`",
            exe.display(),
            installed.display(),
            installed.display()
        ))
    }
}

/// Creates an empty [`WORK_DIR`] owned by root with mode 0700.
fn prepare_work_dir(sys: &dyn System, work: &Path) -> Result<()> {
    let local = sys.path(work);
    match std::fs::remove_dir_all(&local) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => {
            return Err(format!("removing {}: {err}", work.display()));
        }
        _ => {}
    }
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&local)
        .map_err(|err| format!("creating {}: {err}", work.display()))
}

struct InstallPlan<'a> {
    client: &'a Client<'a>,
    config: &'a Config,
    release: &'a Release,
    target: &'a Version,
    downgrade: bool,
    bin_dir: &'a Path,
    work: &'a Path,
}

fn write_work_file(sys: &dyn System, path: &Path, data: &[u8]) -> Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(sys.path(path))
        .and_then(|mut file| file.write_all(data))
        .map_err(|err| format!("writing {}: {err}", path.display()))
}

fn install_release(sys: &dyn System, plan: &InstallPlan<'_>, out: &mut dyn Write) -> Result<()> {
    let name = plan.release.archive_name();
    let tarball_name = format!("{name}.tar.gz");
    writeln!(out, "downloading {tarball_name}").ok();
    let tarball = plan.client.download(plan.release, &tarball_name)?;
    let sums = plan
        .client
        .download(plan.release, &format!("{tarball_name}.sha256"))?;
    let signature = plan
        .client
        .download(plan.release, &format!("{tarball_name}.minisig"))?;
    for (file, data) in [
        (tarball_name.clone(), &tarball),
        (format!("{tarball_name}.sha256"), &sums),
        (format!("{tarball_name}.minisig"), &signature),
    ] {
        write_work_file(sys, &plan.work.join(file), data)?;
    }

    verify_signature(&public_key(sys)?, &tarball, &signature)
        .map_err(|err| format!("{tarball_name}: {err}; not installing it"))?;
    writeln!(out, "verified the signature").ok();
    verify_sha256(&tarball, &sums, &tarball_name)?;
    writeln!(out, "verified the SHA-256 checksum").ok();
    extract(&tarball, &sys.path(plan.work), &name)?;
    let new_puzuz = plan.work.join(&name).join("puzuz");

    check_new_binary(sys, plan, &new_puzuz)?;

    writeln!(
        out,
        "installing puzuz {} into {}",
        plan.target,
        plan.bin_dir.display()
    )
    .ok();
    let bin_dir = plan.bin_dir.to_string_lossy();
    let output = sys
        .run(
            &new_puzuz.to_string_lossy(),
            &["service", "install", "--bin-dir", &bin_dir],
        )
        .map_err(|err| format!("cannot run {}: {err}", new_puzuz.display()))?;
    out.write_all(output.stdout.as_bytes()).ok();
    out.write_all(output.stderr.as_bytes()).ok();
    if !output.success {
        let running = crate::service::status(sys, None, &mut io::sink()).unwrap_or(false);
        if !running {
            return Err(format!(
                "`{} service install` failed (see above)",
                new_puzuz.display()
            ));
        }
        writeln!(
            out,
            "warning: the services run, but some checks above failed; fix them and re-run \
             `puzuz doctor`"
        )
        .ok();
    }

    wait_for_daemon(sys, plan)?;
    writeln!(out, "\npuzuz is now {}.", plan.target).ok();
    Ok(())
}

/// Asks the new binary what it is and which database schema it supports, and refuses a release
/// that would not open the daemon's database.
fn check_new_binary(sys: &dyn System, plan: &InstallPlan<'_>, new_puzuz: &Path) -> Result<()> {
    let info = sys
        .run(&new_puzuz.to_string_lossy(), &["service", "version-info"])
        .ok()
        .filter(|output| output.success)
        .and_then(|output| serde_json::from_str::<VersionInfo>(output.stdout.trim()).ok());
    let Some(info) = info else {
        if plan.downgrade {
            return Err(format!(
                "puzuz {} cannot tell which database schema it supports; refusing to downgrade \
                 to it",
                plan.target
            ));
        }
        return Ok(());
    };
    if info.version != plan.target.to_string() {
        return Err(format!(
            "the binary in release {} reports version {}; not installing it",
            plan.release.tag_name, info.version
        ));
    }
    match sys.daemon_version(&plan.config.api_socket) {
        Ok(version) => match version.schema_version {
            Some(database) if info.schema_version < database => Err(format!(
                "puzuz {} supports database schema {} but the database is at schema {database} \
                 (written by a newer puzuz); the daemon would refuse to start. Refusing to \
                 install it",
                plan.target, info.schema_version
            )),
            Some(_) => Ok(()),
            None if plan.downgrade => Err(
                "the daemon does not report its database schema; refusing to downgrade".to_owned(),
            ),
            None => Ok(()),
        },
        Err(err) if plan.downgrade => Err(format!(
            "cannot ask the daemon for its database schema ({err}); start it before \
             downgrading"
        )),
        Err(_) => Ok(()),
    }
}

/// Waits until the daemon runs the new version and is connected to the helper.
fn wait_for_daemon(sys: &dyn System, plan: &InstallPlan<'_>) -> Result<()> {
    let target = plan.target.to_string();
    let mut waited = Duration::ZERO;
    let mut last: String;
    loop {
        match sys.daemon_version(&plan.config.api_socket) {
            Ok(version) if version.daemon_version == target && version.helper.connected => {
                return Ok(());
            }
            Ok(version) => {
                last = format!(
                    "version {}, helper {}",
                    version.daemon_version,
                    if version.helper.connected {
                        "connected"
                    } else {
                        "not connected"
                    }
                );
            }
            Err(err) => last = err,
        }
        if waited >= RESTART_TIMEOUT {
            return Err(format!(
                "installed puzuz {target}, but the daemon is not running it ({last}); see \
                 `journalctl -u puzuz -u puzuz-helper`"
            ));
        }
        sys.sleep(POLL_INTERVAL);
        waited += POLL_INTERVAL;
    }
}

/// For `puzuz doctor`: a release newer than this binary, if any.
pub fn newer_release(sys: &dyn System, config: &Config) -> Result<Option<Version>> {
    let client = Client::new(sys, config)?;
    let latest = client.release(None, CHECK_TIMEOUT)?.version()?;
    Ok((latest > current_version()).then_some(latest))
}

/// For `puzuz doctor`: how to update to the newer release `version`.
pub fn update_hint(detection: &Detection, config: &Config, version: Option<&Version>) -> String {
    how_to_update(detection, version, &release_repo(config))
}

#[cfg(test)]
mod tests;
