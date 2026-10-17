//! Validation of daemon requests. The helper does not trust the daemon:
//! a compromised daemon must not be able to run anything as root, join a
//! group the owner is not in, or make root touch files on a user's behalf.

use std::path::{Component, Path, PathBuf};

use gpueue_config::job_policy::{self, MinUidSource};
use gpueue_protocol::helper::{Gid, HelperErrorCode, JobId, SpawnRequest, Uid};

use crate::host::{HostProbe, UserInfo};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub code: HelperErrorCode,
    pub message: String,
}

impl Rejection {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: HelperErrorCode::InvalidRequest,
            message: message.into(),
        }
    }

    pub fn denied(message: impl Into<String>) -> Self {
        Self {
            code: HelperErrorCode::PermissionDenied,
            message: message.into(),
        }
    }

    /// Refused by the [`JobPolicy`].
    pub fn policy(user: &str, reason: impl std::fmt::Display) -> Self {
        Self {
            code: HelperErrorCode::PolicyDenied,
            message: format!("gpueue refuses to run jobs as {user}: {reason}"),
        }
    }
}

/// The groups a validated spawn will run with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub uid: Uid,
    pub gid: Gid,
    /// The supplementary groups; always set once checked (the requested
    /// ones, or what `initgroups` would set, without denied groups).
    pub groups: Option<Vec<Gid>>,
    pub user: UserInfo,
    /// The groups of `job_denied_groups` that exist on this host, and gid 0,
    /// as `(name, gid)`.
    pub denied: Vec<(String, u32)>,
    /// Supplementary groups the job would have had but that are left out
    /// because they are denied (names).
    pub dropped: Vec<String>,
}

impl Credentials {
    /// Every group the owner is in (the primary one included) that a job
    /// may have: denied groups are left out.
    pub fn allowed_gids(&self) -> Vec<u32> {
        let mut gids: Vec<u32> = self
            .user
            .groups
            .iter()
            .copied()
            .chain([self.user.gid])
            .filter(|g| !self.is_denied(*g))
            .collect();
        gids.sort_unstable();
        gids.dedup();
        gids
    }

    pub fn is_denied(&self, gid: u32) -> bool {
        self.denied.iter().any(|(_, g)| *g == gid)
    }

    /// The name of a denied group (its number if it has none).
    pub fn denied_name(&self, gid: u32) -> String {
        self.denied
            .iter()
            .find(|(_, g)| *g == gid)
            .map_or_else(|| gid.to_string(), |(n, _)| n.clone())
    }
}

/// Who jobs may run as (`job_min_uid`, `job_denied_groups` and
/// `job_allowed_users` of the configuration file); see
/// [`gpueue_config::job_policy`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobPolicy {
    pub min_uid: u32,
    pub min_uid_source: MinUidSource,
    /// `job_max_uid`: jobs of larger uids are refused. `None`: no bound.
    /// [`job_policy::is_reserved_uid`] uids are refused either way.
    pub max_uid: Option<u32>,
    /// Group names; looked up in `group_file` at each check, so groups
    /// created later are covered.
    pub denied_groups: Vec<String>,
    /// Empty: every user.
    pub allowed_users: Vec<String>,
    pub group_file: PathBuf,
}

impl JobPolicy {
    /// The policy of the configuration `file`; the minimum uid comes from
    /// the `login.defs` file at `login_defs` when `file` has none.
    pub fn from_config(file: &gpueue_config::Config, login_defs: &Path, group_file: &Path) -> Self {
        let (min_uid, min_uid_source) = job_policy::effective_min_uid(file.job_min_uid, login_defs);
        Self {
            min_uid,
            min_uid_source,
            max_uid: file.job_max_uid,
            denied_groups: file.job_denied_groups.clone(),
            allowed_users: file.job_allowed_users.clone(),
            group_file: group_file.to_owned(),
        }
    }

    /// No restriction beyond the built-in ones (no root, only the owner's
    /// groups). For tests; the helper uses [`Self::from_config`].
    pub fn permissive() -> Self {
        Self {
            min_uid: 1,
            min_uid_source: MinUidSource::Config,
            max_uid: None,
            denied_groups: Vec::new(),
            allowed_users: Vec::new(),
            group_file: PathBuf::from(job_policy::GROUP_FILE_PATH),
        }
    }

    /// The denied groups that exist (read from the group file), and gid 0.
    pub fn denied_gids(&self) -> std::io::Result<Vec<(String, u32)>> {
        let mut denied = if self.denied_groups.is_empty() {
            Vec::new()
        } else {
            let text = std::fs::read_to_string(&self.group_file)?;
            job_policy::resolve_group_names(&text, &self.denied_groups)
        };
        if !denied.iter().any(|(_, g)| *g == 0) {
            denied.push(("root".to_owned(), 0));
        }
        Ok(denied)
    }

    /// One line for the helper's log.
    pub fn describe(&self) -> String {
        format!(
            "job_min_uid {} ({}), job_max_uid {}, job_denied_groups {:?}, job_allowed_users {}",
            self.min_uid,
            self.min_uid_source.describe(),
            self.max_uid
                .map_or_else(|| "(none)".to_owned(), |m| m.to_string()),
            self.denied_groups,
            if self.allowed_users.is_empty() {
                "(everyone)".to_owned()
            } else {
                format!("{:?}", self.allowed_users)
            }
        )
    }
}

/// An absolute path without `..` and without NUL bytes.
pub fn check_path(what: &str, path: &Path) -> Result<(), Rejection> {
    if !path.is_absolute() {
        return Err(Rejection::invalid(format!(
            "{what} must be absolute: {}",
            path.display()
        )));
    }
    if path.components().any(|c| c == Component::ParentDir) {
        return Err(Rejection::invalid(format!(
            "{what} must not contain '..': {}",
            path.display()
        )));
    }
    if path.as_os_str().as_encoded_bytes().contains(&0) {
        return Err(Rejection::invalid(format!("{what} contains a NUL byte")));
    }
    Ok(())
}

/// Check the owner and groups of a job against NSS and the [`JobPolicy`].
/// Denied supplementary groups are left out of the returned groups (see
/// [`Credentials::dropped`]); a denied job group is refused.
pub fn check_credentials<H: HostProbe + ?Sized>(
    host: &H,
    policy: &JobPolicy,
    uid: Uid,
    gid: Gid,
    groups: Option<&[Gid]>,
) -> Result<Credentials, Rejection> {
    if uid.0 == 0 {
        return Err(Rejection::invalid("refusing to run a job as root"));
    }
    let user = host
        .lookup_user(uid.0)
        .map_err(|e| Rejection::invalid(format!("looking up uid {}: {e}", uid.0)))?
        .ok_or_else(|| Rejection::invalid(format!("uid {} has no user entry", uid.0)))?;
    if uid.0 < policy.min_uid {
        return Err(Rejection::policy(
            &user.name,
            format!(
                "uid {} is below job_min_uid {} ({})",
                uid.0,
                policy.min_uid,
                policy.min_uid_source.describe()
            ),
        ));
    }
    if job_policy::is_reserved_uid(uid.0) {
        return Err(Rejection::policy(
            &user.name,
            format!(
                "uid {} is reserved (nobody, 65535, 4294967295 and systemd's dynamic users \
                 61184-65519 never run jobs)",
                uid.0
            ),
        ));
    }
    if let Some(max) = policy.max_uid
        && uid.0 > max
    {
        return Err(Rejection::policy(
            &user.name,
            format!("uid {} is above job_max_uid {max}", uid.0),
        ));
    }
    if !policy.allowed_users.is_empty() && !policy.allowed_users.contains(&user.name) {
        return Err(Rejection::policy(&user.name, "not in job_allowed_users"));
    }
    let member = |g: u32| g == user.gid || user.groups.contains(&g);
    if gid.0 == 0 || !member(gid.0) {
        return Err(Rejection::denied(format!(
            "user {} is not a member of gid {}",
            user.name, gid.0
        )));
    }
    if let Some(groups) = groups
        && let Some(bad) = groups.iter().find(|g| g.0 == 0 || !member(g.0))
    {
        return Err(Rejection::denied(format!(
            "user {} is not a member of gid {}",
            user.name, bad.0
        )));
    }
    let denied = policy.denied_gids().map_err(|e| Rejection {
        code: HelperErrorCode::Internal,
        message: format!(
            "reading {} for job_denied_groups: {e}",
            policy.group_file.display()
        ),
    })?;
    let mut creds = Credentials {
        uid,
        gid,
        groups: groups.map(<[Gid]>::to_vec),
        user,
        denied,
        dropped: Vec::new(),
    };
    if creds.is_denied(gid.0) {
        return Err(Rejection::policy(
            &creds.user.name,
            format!(
                "primary group {} is in job_denied_groups",
                creds.denied_name(gid.0)
            ),
        ));
    }
    // What the job would get: the requested groups, or what `initgroups`
    // sets (every group of the user, and the job's group). The result is
    // always passed explicitly, so that the shim does not look the groups
    // up again (and find a denied group added in between).
    let wanted: Vec<u32> = match &creds.groups {
        Some(groups) => groups.iter().map(|g| g.0).collect(),
        None => creds.user.groups.iter().copied().chain([gid.0]).collect(),
    };
    let mut dropped: Vec<u32> = wanted
        .iter()
        .copied()
        .filter(|g| creds.is_denied(*g))
        .collect();
    dropped.sort_unstable();
    dropped.dedup();
    let mut kept: Vec<u32> = wanted
        .into_iter()
        .filter(|g| !dropped.contains(g))
        .collect();
    kept.sort_unstable();
    kept.dedup();
    creds.groups = Some(kept.into_iter().map(Gid).collect());
    creds.dropped = dropped.iter().map(|g| creds.denied_name(*g)).collect();
    Ok(creds)
}

/// Longest accepted command line (the `command` string and each shell
/// word), cwd or log path, in bytes. Linux caps one argument at 128 KiB
/// (`MAX_ARG_STRLEN`) and a path at 4 KiB.
pub const MAX_COMMAND_LEN: usize = 128 * 1024;
pub const MAX_PATH_LEN: usize = 4096;
/// Most environment variables and most bytes in all of them (names and
/// values) of one job. The kernel limits `execve` to roughly 2 MiB of
/// arguments and environment together; the daemon's request body is capped
/// at 2 MiB.
pub const MAX_ENV_VARS: usize = 16 * 1024;
pub const MAX_ENV_BYTES: usize = 2 * 1024 * 1024;
/// Most words of the shell prefix.
pub const MAX_SHELL_WORDS: usize = 64;

fn check_spawn_sizes(req: &SpawnRequest) -> Result<(), Rejection> {
    let too_big = |what: &str| Err(Rejection::invalid(format!("{what} is too large")));
    if req.command.len() > MAX_COMMAND_LEN {
        return too_big("command");
    }
    if req.shell.len() > MAX_SHELL_WORDS || req.shell.iter().any(|w| w.len() > MAX_COMMAND_LEN) {
        return too_big("shell");
    }
    if req.cwd.as_os_str().len() > MAX_PATH_LEN || req.log_path.as_os_str().len() > MAX_PATH_LEN {
        return too_big("cwd or log path");
    }
    if req.envs.len() > MAX_ENV_VARS
        || req
            .envs
            .iter()
            .map(|(k, v)| k.len() + v.len() + 2)
            .sum::<usize>()
            > MAX_ENV_BYTES
    {
        return too_big("environment");
    }
    Ok(())
}

/// Validate a spawn request.
pub fn check_spawn<H: HostProbe + ?Sized>(
    host: &H,
    policy: &JobPolicy,
    req: &SpawnRequest,
) -> Result<Credentials, Rejection> {
    let creds = check_credentials(host, policy, req.uid, req.gid, req.groups.as_deref())?;
    check_path("cwd", &req.cwd)?;
    check_path("log path", &req.log_path)?;
    if req.shell.is_empty() || req.shell[0].is_empty() {
        return Err(Rejection::invalid("shell must not be empty"));
    }
    check_spawn_sizes(req)?;
    let nul = |s: &str| s.contains('\0');
    if req.shell.iter().any(|s| nul(s)) || nul(&req.command) {
        return Err(Rejection::invalid("command contains a NUL byte"));
    }
    for (k, v) in &req.envs {
        if k.is_empty() || k.contains('=') || nul(k) || nul(v) {
            return Err(Rejection::invalid(format!(
                "invalid environment variable name {k:?}"
            )));
        }
    }
    Ok(creds)
}

/// Every existing component of a path must be owned by root and not
/// writable by group or others (the sticky Nix store excepted); root-owned
/// symlinks are followed (the rule of the configuration loader, which
/// refuses symlinks).
pub use gpueue_config::trust::check_root_owned_path;

/// The image directory must be exactly `<base>/<job id>`; the daemon cannot
/// point the helper anywhere else.
pub fn check_image_dir(base: &Path, job: JobId, image_dir: &Path) -> Result<(), Rejection> {
    check_path("image dir", image_dir)?;
    let expected = base.join(job.0.to_string());
    if image_dir != expected {
        return Err(Rejection::invalid(format!(
            "image dir must be {}, got {}",
            expected.display(),
            image_dir.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeHost;
    use std::collections::BTreeMap;

    fn req() -> SpawnRequest {
        SpawnRequest {
            job: JobId(1),
            uid: Uid(1000),
            gid: Gid(1000),
            groups: None,
            cwd: "/home/alice".into(),
            command: "true".into(),
            shell: vec!["sh".into(), "-c".into()],
            envs: BTreeMap::from([("PATH".into(), "/usr/bin".into())]),
            gpus: vec![],
            log_path: "/var/lib/gpueue/logs/1000/1.log".into(),
            no_checkpoint: false,
        }
    }

    fn host() -> FakeHost {
        let h = FakeHost::default();
        h.add_user(1000, "alice", 1000, &[1000, 27]);
        h
    }

    #[test]
    fn accepts_valid_spawn() {
        let c = check_spawn(&host(), &JobPolicy::permissive(), &req()).unwrap();
        // What `initgroups` would set, passed explicitly.
        assert_eq!(c.groups, Some(vec![Gid(27), Gid(1000)]));
        assert!(c.dropped.is_empty());
        let mut r = req();
        r.groups = Some(vec![Gid(27)]);
        assert!(check_spawn(&host(), &JobPolicy::permissive(), &r).is_ok());
    }

    #[test]
    fn refuses_oversized_spawn_requests() {
        let mut r = req();
        r.command = "x".repeat(MAX_COMMAND_LEN + 1);
        assert!(check_spawn(&host(), &JobPolicy::permissive(), &r).is_err());
        let mut r = req();
        r.envs.insert("BIG".into(), "x".repeat(MAX_ENV_BYTES));
        assert!(check_spawn(&host(), &JobPolicy::permissive(), &r).is_err());
        let mut r = req();
        r.cwd = format!("/{}", "a".repeat(MAX_PATH_LEN)).into();
        assert!(check_spawn(&host(), &JobPolicy::permissive(), &r).is_err());
    }

    #[test]
    fn refuses_reserved_uids_and_honors_job_max_uid() {
        let (p, _dir) = policy("");
        assert_eq!(p.max_uid, None);
        let h = FakeHost::default();
        let uids = [
            60000u32,
            60513,
            61183,
            61184,
            65000,
            65519,
            65520,
            65534,
            65535,
            100_000,
            u32::MAX,
        ];
        for uid in uids {
            h.add_user(uid, "u", uid, &[uid]);
        }
        let go = |p: &JobPolicy, uid: u32| check_credentials(&h, p, Uid(uid), Gid(uid), None);
        // No upper bound: sssd-sized and systemd-homed uids are fine.
        for uid in [60000u32, 60513, 61183, 65520, 100_000] {
            assert!(go(&p, uid).is_ok(), "{uid}");
        }
        for uid in [61184u32, 65000, 65519, 65534, 65535, u32::MAX] {
            assert_eq!(
                go(&p, uid).unwrap_err().code,
                HelperErrorCode::PolicyDenied,
                "{uid}"
            );
        }
        let (p, _dir) = policy("job_max_uid = 60513");
        assert_eq!(p.max_uid, Some(60513));
        assert!(go(&p, 60513).is_ok());
        let e = go(&p, 100_000).unwrap_err();
        assert_eq!(e.code, HelperErrorCode::PolicyDenied);
        assert!(
            e.message.contains("above job_max_uid 60513"),
            "{}",
            e.message
        );
        // Reserved uids stay refused even when the bound is raised above them.
        let (p, _dir) = policy("job_max_uid = 4294967295");
        assert!(go(&p, 65534).is_err());
    }

    const LOGIN_DEFS: &str = "# UID_MIN 10\nUID_MIN\t\t 1000\nUID_MAX 60000\n";
    const GROUP: &str = "\
root:x:0:
sudo:x:27:alice
docker:x:999:alice
lxd:x:998:
users:x:100:alice
";

    /// The policy of the configuration `text`, with fixture files.
    fn policy(text: &str) -> (JobPolicy, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let defs = dir.path().join("login.defs");
        let group = dir.path().join("group");
        std::fs::write(&defs, LOGIN_DEFS).unwrap();
        std::fs::write(&group, GROUP).unwrap();
        let file = gpueue_config::Config::parse(text).unwrap();
        (JobPolicy::from_config(&file, &defs, &group), dir)
    }

    fn policy_host() -> FakeHost {
        let h = FakeHost::default();
        h.add_user(1000, "alice", 1000, &[1000, 27, 999, 100]);
        h.add_user(1001, "bob", 1001, &[1001]);
        h.add_user(999, "svc", 999, &[999]);
        h.add_user(1002, "dock", 999, &[999, 100]);
        h.add_user(1003, "rooty", 1003, &[1003, 0]);
        h
    }

    fn check(p: &JobPolicy, uid: u32, gid: u32) -> Result<Credentials, Rejection> {
        check_credentials(&policy_host(), p, Uid(uid), Gid(gid), None)
    }

    #[test]
    fn min_uid_defaults_to_login_defs_and_refuses_below_it() {
        let (p, _dir) = policy("");
        assert_eq!(
            (p.min_uid, p.min_uid_source),
            (1000, MinUidSource::LoginDefs)
        );
        let e = check(&p, 999, 999).unwrap_err();
        assert_eq!(e.code, HelperErrorCode::PolicyDenied);
        assert_eq!(
            e.message,
            "gpueue refuses to run jobs as svc: uid 999 is below job_min_uid 1000 \
             (UID_MIN in /etc/login.defs)"
        );
        assert!(check(&p, 1001, 1001).is_ok());
        // The configuration wins; without either, the fallback applies.
        let (p, _dir) = policy("job_min_uid = 1002\njob_denied_groups = []");
        assert_eq!(p.min_uid_source, MinUidSource::Config);
        assert!(check(&p, 1001, 1001).is_err());
        assert!(check(&p, 1002, 999).is_ok());
        let file = gpueue_config::Config::default();
        let p = JobPolicy::from_config(&file, Path::new("/nonexistent"), Path::new("/x"));
        assert_eq!(
            (p.min_uid, p.min_uid_source),
            (1000, MinUidSource::Fallback)
        );
    }

    #[test]
    fn refuses_a_denied_primary_group() {
        let (p, _dir) = policy("");
        let e = check(&p, 1002, 999).unwrap_err();
        assert_eq!(e.code, HelperErrorCode::PolicyDenied);
        assert_eq!(
            e.message,
            "gpueue refuses to run jobs as dock: primary group docker is in job_denied_groups"
        );
        // The user's other group is fine.
        let c = check(&p, 1002, 100).unwrap();
        assert_eq!(c.groups, Some(vec![Gid(100)]));
        assert_eq!(c.dropped, ["docker"]);
    }

    #[test]
    fn drops_denied_supplementary_groups() {
        let (p, _dir) = policy("");
        let c = check(&p, 1000, 1000).unwrap();
        assert_eq!(c.groups, Some(vec![Gid(100), Gid(1000)]));
        assert_eq!(c.dropped, ["sudo", "docker"]);
        assert_eq!(c.allowed_gids(), [100, 1000]);
        // Requested groups are filtered the same way.
        let c = check_credentials(
            &policy_host(),
            &p,
            Uid(1000),
            Gid(1000),
            Some(&[Gid(999), Gid(100)]),
        )
        .unwrap();
        assert_eq!(c.groups, Some(vec![Gid(100)]));
        assert_eq!(c.dropped, ["docker"]);
        // gid 0 is always dropped, even with no denied groups.
        let (p, _dir) = policy("job_denied_groups = []");
        let c = check(&p, 1003, 1003).unwrap();
        assert_eq!(c.groups, Some(vec![Gid(1003)]));
        assert_eq!(c.dropped, ["root"]);
        let c = check(&p, 1000, 1000).unwrap();
        assert_eq!(c.groups, Some(vec![Gid(27), Gid(100), Gid(999), Gid(1000)]));
        assert!(c.dropped.is_empty());
    }

    #[test]
    fn unknown_denied_group_names_are_ignored() {
        let (p, _dir) = policy("job_denied_groups = [\"nosuchgroup\", \"lxd\"]");
        let c = check(&p, 1000, 1000).unwrap();
        assert!(c.dropped.is_empty());
        assert_eq!(c.denied, [("lxd".to_owned(), 998), ("root".to_owned(), 0)]);
        // An unreadable group file refuses rather than letting groups through.
        let mut p = p;
        p.group_file = "/nonexistent/group".into();
        assert_eq!(
            check(&p, 1000, 1000).unwrap_err().code,
            HelperErrorCode::Internal
        );
    }

    #[test]
    fn allowed_users_filter() {
        let (p, _dir) = policy("job_allowed_users = [\"bob\"]");
        assert!(check(&p, 1001, 1001).is_ok());
        let e = check(&p, 1000, 1000).unwrap_err();
        assert_eq!(e.code, HelperErrorCode::PolicyDenied);
        assert_eq!(
            e.message,
            "gpueue refuses to run jobs as alice: not in job_allowed_users"
        );
    }

    #[test]
    fn rejects_root_and_foreign_groups() {
        let mut r = req();
        r.uid = Uid(0);
        assert_eq!(
            check_spawn(&host(), &JobPolicy::permissive(), &r)
                .unwrap_err()
                .code,
            HelperErrorCode::InvalidRequest
        );
        let mut r = req();
        r.gid = Gid(999); // e.g. docker
        assert_eq!(
            check_spawn(&host(), &JobPolicy::permissive(), &r)
                .unwrap_err()
                .code,
            HelperErrorCode::PermissionDenied
        );
        let mut r = req();
        r.groups = Some(vec![Gid(1000), Gid(0)]);
        assert!(check_spawn(&host(), &JobPolicy::permissive(), &r).is_err());
        let mut r = req();
        r.uid = Uid(4242);
        assert!(check_spawn(&host(), &JobPolicy::permissive(), &r).is_err());
    }

    #[test]
    fn rejects_bad_paths_and_env() {
        let mut r = req();
        r.cwd = "relative".into();
        assert!(check_spawn(&host(), &JobPolicy::permissive(), &r).is_err());
        let mut r = req();
        r.log_path = "/var/lib/gpueue/logs/../../../etc/shadow".into();
        assert!(check_spawn(&host(), &JobPolicy::permissive(), &r).is_err());
        let mut r = req();
        r.envs.insert("A=B".into(), "x".into());
        assert!(check_spawn(&host(), &JobPolicy::permissive(), &r).is_err());
        let mut r = req();
        r.shell = vec![];
        assert!(check_spawn(&host(), &JobPolicy::permissive(), &r).is_err());
    }

    #[test]
    fn image_dir_must_be_the_jobs_own() {
        let base = Path::new("/var/lib/gpueue/checkpoints");
        assert!(check_image_dir(base, JobId(3), &base.join("3")).is_ok());
        assert!(check_image_dir(base, JobId(3), &base.join("4")).is_err());
        assert!(check_image_dir(base, JobId(3), Path::new("/etc")).is_err());
    }
}
