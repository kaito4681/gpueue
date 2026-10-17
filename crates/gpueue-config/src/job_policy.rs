//! Who jobs may run as and what they may reach: the defaults and host
//! lookups behind [`Config::job_min_uid`](crate::Config::job_min_uid),
//! [`Config::job_denied_groups`](crate::Config::job_denied_groups),
//! [`Config::job_allowed_users`](crate::Config::job_allowed_users) and
//! [`Config::job_hidden_socket_dirs`](crate::Config::job_hidden_socket_dirs).
//!
//! The helper enforces the policy (it is the trust boundary); `gpueue doctor`
//! shows the effective one. The file paths are parameters so that tests can
//! use fixtures.

use std::path::Path;

/// Where `UID_MIN` is read from when `job_min_uid` is not set.
pub const LOGIN_DEFS_PATH: &str = "/etc/login.defs";

/// Where the names of `job_denied_groups` are looked up.
pub use crate::trust::GROUP_FILE as GROUP_FILE_PATH;
use crate::trust::parse_group_gid;

/// `job_min_uid` when it is not set and `/etc/login.defs` has no usable
/// `UID_MIN` (the value most distributions use).
pub const FALLBACK_JOB_MIN_UID: u32 = 1000;

/// Default of `job_denied_groups`: groups whose members can become root (or
/// do anything root can) without a password, so a job, which runs
/// unattended, must not carry them.
///
/// - `docker`: the Docker socket starts containers that bind-mount `/` and
///   run as root.
/// - `lxd`, `incus-admin`: the same through LXD/Incus (privileged
///   containers, host mounts). Incus's unprivileged `incus` group is not
///   included.
/// - `libvirt`, `libvirtd` (NixOS's name): manage `qemu:///system`, whose
///   VMs can be given host block devices and directories.
/// - `disk`: raw read/write access to block devices, i.e. to every file on
///   them.
/// - `sudo`, `wheel`, `admin`: sudo rights. sudo normally asks for the
///   password, which an unattended job does not have, but `NOPASSWD` rules
///   or cached credentials would let the job become root; jobs do not
///   need them.
///
/// Not included because they do not amount to root: `shadow` (password
/// hashes, offline cracking at most), `adm` (logs), `kvm`, `video` and
/// `render` (devices; GPU jobs need the latter two).
pub const DEFAULT_JOB_DENIED_GROUPS: &[&str] = &[
    "docker",
    "lxd",
    "incus-admin",
    "libvirt",
    "libvirtd",
    "disk",
    "sudo",
    "wheel",
    "admin",
];

/// The default of [`Config::job_denied_groups`](crate::Config::job_denied_groups).
pub fn default_denied_groups() -> Vec<String> {
    DEFAULT_JOB_DENIED_GROUPS
        .iter()
        .map(|g| (*g).to_owned())
        .collect()
}

/// Default of `job_hidden_socket_dirs`: directories of daemon sockets that
/// authorize clients by looking up the caller's groups in the user
/// database (directly or through polkit), so dropping groups from a job
/// (see [`DEFAULT_JOB_DENIED_GROUPS`]) does not take their rights away.
///
/// - `/run/libvirt`: libvirt's sockets (`libvirt-sock`, `virtqemud-sock`,
///   `virtstoraged-sock`, ... and their `-ro`/`-admin` variants). On
///   Fedora, RHEL, Debian, Arch and NixOS the read-write sockets are
///   world-connectable and authorized through polkit, whose shipped rule
///   grants `org.libvirt.unix.manage` to group `libvirt` (`libvirtd` on
///   NixOS): a member's job could define a VM or storage pool on the
///   host's disks.
/// - `/run/cups`: CUPS's local socket, where cupsd identifies the caller
///   from its peer credentials and checks `SystemGroup` (`lpadmin`,
///   `wheel`, `sys`) in the group database, so a member's job could
///   administer printers. Printing still works over `localhost:631`
///   (libcups falls back to it when the socket is missing), where
///   administration needs a password.
pub const DEFAULT_JOB_HIDDEN_SOCKET_DIRS: &[&str] = &["/run/libvirt", "/run/cups"];

/// The default of [`Config::job_hidden_socket_dirs`](crate::Config::job_hidden_socket_dirs).
pub fn default_hidden_socket_dirs() -> Vec<String> {
    DEFAULT_JOB_HIDDEN_SOCKET_DIRS
        .iter()
        .map(|d| (*d).to_owned())
        .collect()
}

/// Top-level directories (and what lies below them) that
/// `job_hidden_socket_dirs` may not name: the job needs them, the shim
/// replaces them anyway (`/tmp`, `/proc`), or another key governs them
/// (`/run/dbus`, `/run/systemd`).
const NOT_HIDEABLE: &[&str] = &[
    "/proc",
    "/sys",
    "/dev",
    "/tmp",
    "/run/dbus",
    "/var/run/dbus",
    "/run/systemd",
    "/var/run/systemd",
    "/run/user",
    "/var/run/user",
];

/// Why `dir` cannot be an entry of `job_hidden_socket_dirs`, if it cannot:
/// it must be an absolute, normalized path at least two levels deep (not
/// `/`, `/run` or `/home`), outside [`NOT_HIDEABLE`].
pub fn check_hidden_socket_dir(dir: &str) -> Result<(), String> {
    let path = Path::new(dir);
    let normal = dir.strip_prefix('/').is_some_and(|rest| {
        rest.split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..")
    });
    if !normal || dir.contains(['\n', '\0']) {
        return Err(format!("{dir:?} is not an absolute, normalized path"));
    }
    if path.components().count() < 3 {
        return Err(format!(
            "{dir:?} is a top-level directory; name the directory that holds the sockets"
        ));
    }
    if let Some(reserved) = NOT_HIDEABLE.iter().find(|r| path.starts_with(Path::new(r))) {
        return Err(format!(
            "{dir:?} is below {reserved}, which cannot be hidden this way"
        ));
    }
    Ok(())
}

/// Where the effective minimum uid came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MinUidSource {
    /// `job_min_uid` in the configuration file.
    Config,
    /// `UID_MIN` in `/etc/login.defs`.
    LoginDefs,
    /// [`FALLBACK_JOB_MIN_UID`].
    Fallback,
}

impl MinUidSource {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Config => "job_min_uid in the configuration",
            Self::LoginDefs => "UID_MIN in /etc/login.defs",
            Self::Fallback => "built-in fallback",
        }
    }
}

/// The value of `UID_MIN` in the contents of a `login.defs` file. Comments
/// (`#`) and malformed lines are ignored; the last valid setting wins.
pub fn parse_uid_min(login_defs: &str) -> Option<u32> {
    parse_login_defs_number(login_defs, "UID_MIN")
}

fn parse_login_defs_number(login_defs: &str, key: &str) -> Option<u32> {
    let mut found = None;
    for line in login_defs.lines() {
        let line = line.split('#').next().unwrap_or("");
        let mut words = line.split_whitespace();
        if words.next() != Some(key) {
            continue;
        }
        if let (Some(value), None) = (words.next(), words.next())
            && let Ok(uid) = value.parse::<u32>()
        {
            found = Some(uid);
        }
    }
    found
}

/// The minimum uid of jobs: `configured` (from `job_min_uid`), else
/// `UID_MIN` of the `login.defs` file at `login_defs`, else
/// [`FALLBACK_JOB_MIN_UID`] (also when the file cannot be read).
pub fn effective_min_uid(configured: Option<u32>, login_defs: &Path) -> (u32, MinUidSource) {
    if let Some(uid) = configured {
        return (uid, MinUidSource::Config);
    }
    match std::fs::read_to_string(login_defs)
        .ok()
        .as_deref()
        .and_then(parse_uid_min)
    {
        Some(uid) => (uid, MinUidSource::LoginDefs),
        None => (FALLBACK_JOB_MIN_UID, MinUidSource::Fallback),
    }
}

/// Uids that never run jobs, whatever `job_max_uid` says: `nobody` and
/// `nogroup` (65534), the 16-bit "invalid" uid (65535) and the 32-bit one
/// (`(uid_t)-1`, 4294967295).
pub const RESERVED_UIDS: &[u32] = &[65534, 65535, u32::MAX];

/// systemd's `DynamicUser=` range (`61184..=65519`): transient service
/// accounts, never users.
pub const DYNAMIC_USER_UIDS: std::ops::RangeInclusive<u32> = 61184..=65519;

/// Whether no job may ever run as `uid` ([`RESERVED_UIDS`] and
/// [`DYNAMIC_USER_UIDS`]).
pub fn is_reserved_uid(uid: u32) -> bool {
    RESERVED_UIDS.contains(&uid) || DYNAMIC_USER_UIDS.contains(&uid)
}

/// The `(name, gid)` of each of `names` that the group file contents
/// `group_file` define; names it does not list are left out.
pub fn resolve_group_names(group_file: &str, names: &[String]) -> Vec<(String, u32)> {
    names
        .iter()
        .filter_map(|name| Some((name.clone(), parse_group_gid(group_file, name)?)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOGIN_DEFS: &str = "\
# UID_MIN 5
MAIL_DIR        /var/mail
UID_MIN                  1000   # regular users
UID_MAX                 60000
SYS_UID_MIN               100
";

    #[test]
    fn parses_uid_min() {
        assert_eq!(parse_uid_min(LOGIN_DEFS), Some(1000));
        assert_eq!(parse_uid_min("UID_MIN 500\nUID_MIN 2000\n"), Some(2000));
        assert_eq!(parse_uid_min("UID_MIN\tabc\n"), None);
        assert_eq!(parse_uid_min("UID_MIN 1 2\n"), None);
        assert_eq!(parse_uid_min("#UID_MIN 500\nSYS_UID_MIN 100\n"), None);
        assert_eq!(parse_uid_min(""), None);
    }

    #[test]
    fn reserved_uids() {
        for uid in [65534, 65535, u32::MAX, 61184, 65000, 65519] {
            assert!(is_reserved_uid(uid), "{uid}");
        }
        // systemd-homed users and LDAP/sssd ranges are regular users.
        for uid in [1000, 60513, 61183, 65520, 100_000, 200_000_000] {
            assert!(!is_reserved_uid(uid), "{uid}");
        }
    }

    #[test]
    fn min_uid_prefers_config_then_login_defs_then_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let defs = dir.path().join("login.defs");
        std::fs::write(&defs, "UID_MIN 2000\n").unwrap();
        assert_eq!(
            effective_min_uid(Some(500), &defs),
            (500, MinUidSource::Config)
        );
        assert_eq!(
            effective_min_uid(None, &defs),
            (2000, MinUidSource::LoginDefs)
        );
        std::fs::write(&defs, "UID_MIN lots\n").unwrap();
        assert_eq!(
            effective_min_uid(None, &defs),
            (FALLBACK_JOB_MIN_UID, MinUidSource::Fallback)
        );
        assert_eq!(
            effective_min_uid(None, &dir.path().join("missing")),
            (FALLBACK_JOB_MIN_UID, MinUidSource::Fallback)
        );
    }

    #[test]
    fn resolves_group_names_and_ignores_unknown_ones() {
        let group = "\
root:x:0:
sudo:x:27:alice
docker:x:999:alice,bob
+nisgroup:::
broken line
lxd:x:notanumber:
";
        let names: Vec<String> = ["docker", "sudo", "wheel", "lxd", "nisgroup"]
            .map(String::from)
            .into();
        assert_eq!(
            resolve_group_names(group, &names),
            [("docker".to_owned(), 999), ("sudo".to_owned(), 27)]
        );
    }

    #[test]
    fn default_denied_groups_match_the_constant() {
        assert_eq!(
            default_denied_groups().len(),
            DEFAULT_JOB_DENIED_GROUPS.len()
        );
        assert!(default_denied_groups().contains(&"docker".to_owned()));
    }
}
