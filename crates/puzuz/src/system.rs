//! Everything `service` and `doctor` need from the host, behind [`System`] so that both can be
//! tested against a temporary root directory and scripted command results.
//!
//! Paths handed to a [`System`] are absolute paths *as seen on the managed system* (e.g.
//! `/etc/puzuz/config.toml`); [`System::path`] maps them below [`System::root`] for file access.

use std::io::{self, BufRead, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use puzuz_protocol::api::VersionResponse;
use puzuz_protocol::client::Client;

#[cfg(test)]
pub mod fake;

/// Result of running a command to completion.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CmdOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

#[cfg(test)]
impl CmdOutput {
    pub fn ok(stdout: impl Into<String>) -> Self {
        Self {
            success: true,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    pub fn failed(stdout: impl Into<String>, stderr: impl Into<String>) -> Self {
        Self {
            success: false,
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

    /// Path of the running `puzuz` binary (a system path).
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
    ) -> Result<HttpResponse, String>;
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
            available_bytes: puzuz_checkpoint::space::available_bytes(path)?,
            tmpfs: puzuz_checkpoint::space::is_tmpfs(path)?,
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
    ) -> Result<HttpResponse, String> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            // Never send the token to where GitHub redirects downloads (object storage).
            .redirect_auth_headers(ureq::config::RedirectAuthHeaders::Never)
            .user_agent(format!("puzuz/{}", puzuz_protocol::BINARY_VERSION))
            .build()
            .into();
        let mut request = agent.get(url);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let mut response = request.call().map_err(|err| err.to_string())?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_HTTP_BODY)
            .read_to_vec()
            .map_err(|err| err.to_string())?;
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

/// Whether the system path is owned by root and not writable by group or others.
pub fn is_root_owned_and_protected(sys: &dyn System, path: &Path) -> io::Result<bool> {
    let meta = std::fs::metadata(sys.path(path))?;
    Ok(meta.uid() == sys.root_uid() && meta.mode() & 0o022 == 0)
}

/// Checks that nobody but root can replace what is at the system path `path`: the path itself
/// (if it exists) and every existing parent directory must be owned by root and not writable by
/// group or others. Missing components are skipped (they are created by root). Symlinks are
/// allowed when root owns them, since replacing one needs write access to its (checked)
/// directory; the path they resolve to is then checked the same way.
pub fn check_root_owned_chain(sys: &dyn System, path: &Path) -> Result<(), String> {
    let local = sys.path(path);
    let root = sys.root();
    let to_system = |p: &Path| -> PathBuf {
        p.strip_prefix(root)
            .map_or_else(|_| p.to_owned(), |rest| Path::new("/").join(rest))
    };
    let mut saw_symlink = false;
    for p in local.ancestors() {
        let meta = match std::fs::symlink_metadata(p) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(format!("{}: {err}", to_system(p).display())),
        };
        let symlink = meta.file_type().is_symlink();
        saw_symlink |= symlink;
        let writable = !symlink && meta.mode() & 0o022 != 0;
        if meta.uid() != sys.root_uid() || writable {
            return Err(format!(
                "{} must be owned by root and not group/world-writable",
                to_system(p).display()
            ));
        }
        if p == root {
            break;
        }
    }
    if saw_symlink {
        // The parents of a missing path cannot be resolved; check the longest existing prefix.
        let existing = local.ancestors().find(|p| p.exists()).unwrap_or(root);
        let resolved = std::fs::canonicalize(existing)
            .map_err(|err| format!("{}: {err}", to_system(existing).display()))?;
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_owned());
        for p in resolved.ancestors() {
            let meta = std::fs::metadata(p).map_err(|err| format!("{}: {err}", p.display()))?;
            if meta.uid() != sys.root_uid() || meta.mode() & 0o022 != 0 {
                return Err(format!(
                    "{} (which {} resolves to) must be owned by root and not \
                     group/world-writable",
                    p.display(),
                    path.display()
                ));
            }
            if p == root {
                break;
            }
        }
    }
    Ok(())
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
