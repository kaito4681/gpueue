//! Everything `service` and `doctor` need from the host, behind [`System`] so that both can be
//! tested against a temporary root directory and scripted command results.
//!
//! Paths handed to a [`System`] are absolute paths *as seen on the managed system* (e.g.
//! `/etc/gpueue/config.toml`); [`System::path`] maps them below [`System::root`] for file access.

use std::io::{self, BufRead, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use gpueue_protocol::api::VersionResponse;
use gpueue_protocol::client::Client;

#[cfg(test)]
pub mod fake;

/// Result of running a command to completion.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CmdOutput {
    pub success: bool,
    /// Exit code; `None` if a signal ended the command.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

#[cfg(test)]
impl CmdOutput {
    pub fn ok(stdout: impl Into<String>) -> Self {
        Self {
            success: true,
            code: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    /// Exit code 1.
    pub fn failed(stdout: impl Into<String>, stderr: impl Into<String>) -> Self {
        Self::exited(1, stdout, stderr)
    }

    pub fn exited(code: i32, stdout: impl Into<String>, stderr: impl Into<String>) -> Self {
        Self {
            success: code == 0,
            code: Some(code),
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }
}

/// Space and type of the filesystem holding a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsUsage {
    pub available_bytes: u64,
    pub tmpfs: bool,
}

/// The host as seen by `service` and `doctor`.
pub trait System {
    /// Directory the managed system's `/` is at (`/` on a real system).
    fn root(&self) -> &Path;

    /// Where the absolute system path `path` is in the local filesystem.
    fn path(&self, path: &Path) -> PathBuf {
        let relative = path.strip_prefix("/").unwrap_or(path);
        self.root().join(relative)
    }

    /// Runs `program` with `args` to completion (without a shell, in the C locale).
    /// A missing program is an error of kind [`io::ErrorKind::NotFound`].
    fn run(&self, program: &str, args: &[&str]) -> io::Result<CmdOutput>;

    /// Effective uid of this process.
    fn euid(&self) -> u32;

    /// The uid files created by root have (0 on a real system).
    fn root_uid(&self) -> u32 {
        0
    }

    /// Path of the running `gpueue` binary (a system path).
    fn current_exe(&self) -> io::Result<PathBuf>;

    /// Whether the group exists.
    fn group_exists(&self, name: &str) -> bool;

    /// Whether the invoking user is in the group; `None` if the group does not exist.
    fn user_in_group(&self, name: &str) -> Option<bool>;

    /// Free space and type of the filesystem holding the system path `path`.
    fn fs_usage(&self, path: &Path) -> io::Result<FsUsage>;

    /// `GET /v1/version` on the daemon socket at the system path `socket`.
    fn daemon_version(&self, socket: &Path) -> Result<VersionResponse, String>;

    /// Asks a yes/no question on the terminal; anything but "y"/"yes" is no.
    fn confirm(&self, prompt: &str) -> bool;

    fn sleep(&self, duration: Duration);

    /// An environment variable of this process.
    fn env_var(&self, name: &str) -> Option<String>;

    /// `GET url` with the given headers, following redirects (without passing `Authorization`
    /// on). Any HTTP status is a response; only network failures are errors.
    fn http_get(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        timeout: Duration,
    ) -> Result<HttpResponse, HttpError>;
}

/// A network failure of [`System::http_get`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpError {
    pub message: String,
    /// The server closed or reset the connection (e.g. a pooled keep-alive connection it had
    /// already dropped): sending the request again on a new connection may well succeed.
    pub connection_lost: bool,
}

impl HttpError {
    #[cfg(test)]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            connection_lost: false,
        }
    }

    fn from_ureq(err: ureq::Error) -> Self {
        let connection_lost = matches!(
            &err,
            ureq::Error::Io(io) if matches!(
                io.kind(),
                io::ErrorKind::UnexpectedEof
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::BrokenPipe
            )
        );
        Self {
            message: err.to_string(),
            connection_lost,
        }
    }
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// The answer to [`System::http_get`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// The largest response body [`RealSystem::http_get`] reads (a release tarball is far smaller).
const MAX_HTTP_BODY: u64 = 1 << 30;

/// The real host.
#[derive(Debug, Default)]
pub struct RealSystem;

impl System for RealSystem {
    fn root(&self) -> &Path {
        Path::new("/")
    }

    fn run(&self, program: &str, args: &[&str]) -> io::Result<CmdOutput> {
        let output = Command::new(program)
            .args(args)
            .env("LC_ALL", "C")
            .stdin(std::process::Stdio::null())
            .output()?;
        Ok(CmdOutput {
            success: output.status.success(),
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn euid(&self) -> u32 {
        nix::unistd::geteuid().as_raw()
    }

    fn current_exe(&self) -> io::Result<PathBuf> {
        std::env::current_exe()
    }

    fn group_exists(&self, name: &str) -> bool {
        matches!(nix::unistd::Group::from_name(name), Ok(Some(_)))
    }

    fn user_in_group(&self, name: &str) -> Option<bool> {
        let group = nix::unistd::Group::from_name(name).ok()??;
        if nix::unistd::getgroups()
            .map(|groups| groups.contains(&group.gid))
            .unwrap_or(false)
            || nix::unistd::getegid() == group.gid
        {
            return Some(true);
        }
        // Added to the group but not logged in again since.
        let user = nix::unistd::User::from_uid(nix::unistd::getuid()).ok()??;
        Some(group.mem.contains(&user.name))
    }

    fn fs_usage(&self, path: &Path) -> io::Result<FsUsage> {
        Ok(FsUsage {
            available_bytes: gpueue_checkpoint::space::available_bytes(path)?,
            tmpfs: gpueue_checkpoint::space::is_tmpfs(path)?,
        })
    }

    fn daemon_version(&self, socket: &Path) -> Result<VersionResponse, String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("failed to start the async runtime: {e}"))?;
        let client = Client::new(socket);
        runtime.block_on(async {
            match tokio::time::timeout(Duration::from_secs(5), client.daemon_version()).await {
                Ok(Ok(version)) => Ok(version),
                Ok(Err(err)) => Err(err.to_string()),
                Err(_) => Err("the daemon did not answer within 5 seconds".to_owned()),
            }
        })
    }

    fn confirm(&self, prompt: &str) -> bool {
        print!("{prompt} [y/N] ");
        if io::stdout().flush().is_err() {
            return false;
        }
        let mut answer = String::new();
        if io::stdin().lock().read_line(&mut answer).is_err() {
            return false;
        }
        matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }

    fn env_var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn http_get(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        timeout: Duration,
    ) -> Result<HttpResponse, HttpError> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            // Never send the token to where GitHub redirects downloads (object storage).
            .redirect_auth_headers(ureq::config::RedirectAuthHeaders::Never)
            .user_agent(format!("gpueue/{}", gpueue_protocol::BINARY_VERSION))
            .build()
            .into();
        let mut request = agent.get(url);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let mut response = request.call().map_err(HttpError::from_ureq)?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_HTTP_BODY)
            .read_to_vec()
            .map_err(HttpError::from_ureq)?;
        Ok(HttpResponse { status, body })
    }
}

/// Directories searched for system tools: systemd's default `PATH` for services, which is what
/// the helper sees (so that e.g. `criu` in /usr/sbin is found when `doctor` runs unprivileged).
pub const TOOL_DIRS: &[&str] = &[
    "/usr/local/sbin",
    "/usr/local/bin",
    "/usr/sbin",
    "/usr/bin",
    "/sbin",
    "/bin",
];

/// Resolves a configured program (a bare name or a path) to a system path; `None` if a bare
/// name is not in [`TOOL_DIRS`] or a path does not exist.
pub fn find_program(sys: &dyn System, program: &Path) -> Option<PathBuf> {
    if program.components().count() > 1 {
        return sys.path(program).is_file().then(|| program.to_owned());
    }
    TOOL_DIRS
        .iter()
        .map(|dir| Path::new(dir).join(program))
        .find(|candidate| sys.path(candidate).is_file())
}

/// What [`find_runnable_program`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Program {
    /// May be run (a system path).
    Found(PathBuf),
    /// A bare name not in [`TOOL_DIRS`], or a path that does not exist.
    Missing,
    /// Running as root, but someone else could replace it: it is not run.
    Untrusted { path: PathBuf, reason: String },
}

/// [`find_program`] for a program this process runs. As root (`gpueue doctor` and `service
/// install` run as root), only one that nobody but root can replace is accepted: the helper's
/// rule, [`check_root_owned_chain`], applied to the resolved path.
pub fn find_runnable_program(sys: &dyn System, program: &Path) -> Program {
    let Some(path) = find_program(sys, program) else {
        return Program::Missing;
    };
    if sys.euid() != 0 {
        return Program::Found(path);
    }
    match check_root_owned_chain(sys, &path) {
        Ok(()) => Program::Found(path),
        Err(reason) => Program::Untrusted { path, reason },
    }
}

/// Whether the system path is owned by root and not writable by group or others.
pub fn is_root_owned_and_protected(sys: &dyn System, path: &Path) -> io::Result<bool> {
    let meta = std::fs::metadata(sys.path(path))?;
    Ok(meta.uid() == sys.root_uid() && meta.mode() & 0o022 == 0)
}

/// Checks that nobody but root can replace what is at the system path `path`: the path itself
/// (if it exists) and every existing parent directory must be owned by root and not writable by
/// group or others. Missing components are skipped (they are created by root). Symlinks are
/// allowed when root owns them, since replacing one needs write access to its (checked)
/// directory; the path they resolve to is then checked the same way. `/nix/store` may be
/// group-writable when it is sticky and its group is `nixbld`. This is the helper's rule
/// ([`gpueue_config::trust::OwnershipCheck`]), applied below [`System::root`].
pub fn check_root_owned_chain(sys: &dyn System, path: &Path) -> Result<(), String> {
    use gpueue_config::trust::{GROUP_FILE, NIX_BUILD_GROUP, NIX_STORE, OwnershipCheck, group_gid};

    let root = sys.root();
    // The check walks resolved paths.
    let real_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_owned());
    let store = sys.path(Path::new(NIX_STORE));
    let store = std::fs::canonicalize(&store).unwrap_or(store);
    let nixbld = group_gid(&sys.path(Path::new(GROUP_FILE)), NIX_BUILD_GROUP);
    let to_system = |p: &Path| -> PathBuf {
        [root, real_root.as_path()]
            .iter()
            .find_map(|r| p.strip_prefix(r).ok())
            .map_or_else(|| p.to_owned(), |rest| Path::new("/").join(rest))
    };
    OwnershipCheck::new(sys.root_uid(), Some(real_root.clone()), store, nixbld)
        .check(&sys.path(path))
        .map_err(|err| err.map_paths(to_system).to_string())
}

/// Runs a command and turns a failure into an error message.
pub fn run_checked(sys: &dyn System, program: &str, args: &[&str]) -> Result<CmdOutput, String> {
    let shown = std::iter::once(program)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ");
    let output = sys
        .run(program, args)
        .map_err(|e| format!("cannot run `{shown}`: {e}"))?;
    if output.success {
        Ok(output)
    } else {
        let detail = if output.stderr.trim().is_empty() {
            output.stdout.trim()
        } else {
            output.stderr.trim()
        };
        Err(format!("`{shown}` failed: {detail}"))
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::fake::FakeSystem;
    use super::*;

    fn chmod(sys: &FakeSystem, path: &str, mode: u32) {
        let local = sys.path(Path::new(path));
        std::fs::set_permissions(local, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// A NixOS-like system: `/nix/store` (1775) with criu in it, reached through
    /// `/run/current-system`, and `/etc/group` with the line `group_line(gid of the store)`.
    fn nixos(group_line: impl FnOnce(u32) -> String) -> FakeSystem {
        let sys = FakeSystem::new();
        for dir in ["/nix", "/run", "/etc"] {
            sys.mkdir(dir);
            chmod(&sys, dir, 0o755);
        }
        sys.mkdir("/nix/store/abc-criu/bin");
        sys.write_exe("/nix/store/abc-criu/bin/criu", "");
        sys.mkdir("/nix/store/xyz-system/sw/bin");
        for dir in [
            "/nix/store/abc-criu",
            "/nix/store/abc-criu/bin",
            "/nix/store/xyz-system",
            "/nix/store/xyz-system/sw",
            "/nix/store/xyz-system/sw/bin",
        ] {
            chmod(&sys, dir, 0o755);
        }
        chmod(&sys, "/nix/store", 0o1775);
        std::os::unix::fs::symlink(
            "../../../abc-criu/bin/criu",
            sys.path(Path::new("/nix/store/xyz-system/sw/bin/criu")),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            sys.path(Path::new("/nix/store/xyz-system")),
            sys.path(Path::new("/run/current-system")),
        )
        .unwrap();
        let gid = std::fs::metadata(sys.path(Path::new("/nix/store")))
            .unwrap()
            .gid();
        sys.write("/etc/group", &format!("root:x:0:\n{}\n", group_line(gid)));
        sys
    }

    #[test]
    fn dropped_connections_are_told_apart_from_other_http_failures() {
        let io_error = |kind, message: &str| {
            HttpError::from_ureq(ureq::Error::Io(io::Error::new(kind, message)))
        };
        // What ureq reports when a pooled connection was closed by the server.
        let err = io_error(io::ErrorKind::UnexpectedEof, "Peer disconnected");
        assert!(err.connection_lost);
        assert_eq!(err.to_string(), "io: Peer disconnected");
        assert!(io_error(io::ErrorKind::ConnectionReset, "reset").connection_lost);
        assert!(io_error(io::ErrorKind::BrokenPipe, "broken pipe").connection_lost);

        assert!(!io_error(io::ErrorKind::ConnectionRefused, "refused").connection_lost);
        assert!(!HttpError::from_ureq(ureq::Error::Timeout(ureq::Timeout::Global)).connection_lost);
        assert!(!HttpError::from_ureq(ureq::Error::HostNotFound).connection_lost);
    }

    #[test]
    fn root_owned_chain_trusts_the_nix_store_and_follows_symlinks() {
        let sys = nixos(|gid| format!("nixbld:x:{gid}:nixbld1"));
        check_root_owned_chain(&sys, Path::new("/run/current-system/sw/bin/criu")).unwrap();
        check_root_owned_chain(&sys, Path::new("/nix/store/abc-criu/bin")).unwrap();

        chmod(&sys, "/nix/store/abc-criu", 0o775);
        let err =
            check_root_owned_chain(&sys, Path::new("/run/current-system/sw/bin/criu")).unwrap_err();
        assert!(
            err.starts_with("/nix/store/abc-criu (which /run/current-system/sw/bin/criu"),
            "{err}"
        );
    }

    #[test]
    fn root_owned_chain_refuses_a_store_without_sticky_bit_or_nixbld() {
        let sys = nixos(|gid| format!("nixbld:x:{}:", gid + 1));
        let err = check_root_owned_chain(&sys, Path::new("/nix/store/abc-criu/bin")).unwrap_err();
        assert!(err.starts_with("/nix/store must be owned by root"), "{err}");

        let sys = nixos(|gid| format!("nixbld:x:{gid}:"));
        chmod(&sys, "/nix/store", 0o775);
        assert!(check_root_owned_chain(&sys, Path::new("/nix/store/abc-criu/bin")).is_err());

        // Other sticky, group-writable directories are refused.
        let sys = nixos(|gid| format!("nixbld:x:{gid}:"));
        sys.mkdir("/opt/bin");
        chmod(&sys, "/opt/bin", 0o755);
        chmod(&sys, "/opt", 0o1775);
        let err = check_root_owned_chain(&sys, Path::new("/opt/bin")).unwrap_err();
        assert!(err.starts_with("/opt must be owned by root"), "{err}");
    }
}
