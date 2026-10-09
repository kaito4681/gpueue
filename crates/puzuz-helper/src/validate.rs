//! Validation of daemon requests. The helper does not trust the daemon:
//! a compromised daemon must not be able to run anything as root, join a
//! group the owner is not in, or make root touch files on a user's behalf.

use std::path::{Component, Path};

use puzuz_protocol::helper::{Gid, HelperErrorCode, JobId, SpawnRequest, Uid};

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
}

/// The groups a validated spawn will run with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub uid: Uid,
    pub gid: Gid,
    /// `None`: let systemd call `initgroups` for the user.
    pub groups: Option<Vec<Gid>>,
    pub user: UserInfo,
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

/// Check the owner and groups of a job against NSS.
pub fn check_credentials<H: HostProbe + ?Sized>(
    host: &H,
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
    Ok(Credentials {
        uid,
        gid,
        groups: groups.map(<[Gid]>::to_vec),
        user,
    })
}

/// Validate a spawn request.
pub fn check_spawn<H: HostProbe + ?Sized>(
    host: &H,
    req: &SpawnRequest,
) -> Result<Credentials, Rejection> {
    let creds = check_credentials(host, req.uid, req.gid, req.groups.as_deref())?;
    check_path("cwd", &req.cwd)?;
    check_path("log path", &req.log_path)?;
    if req.shell.is_empty() || req.shell[0].is_empty() {
        return Err(Rejection::invalid("shell must not be empty"));
    }
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

/// Every existing ancestor of `path` (and `path` itself, if it exists) must
/// be owned by root and not writable by group or others, otherwise a less
/// privileged user could redirect what root writes there.
pub fn check_root_owned_path(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    for p in path.ancestors() {
        let meta = match std::fs::symlink_metadata(p) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("{}: {e}", p.display())),
        };
        if meta.file_type().is_symlink() {
            return Err(format!("{} is a symlink", p.display()));
        }
        if meta.uid() != 0 || meta.mode() & 0o022 != 0 {
            return Err(format!(
                "{} must be owned by root and not group/world-writable",
                p.display()
            ));
        }
    }
    Ok(())
}

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
            log_path: "/var/lib/puzuz/logs/1000/1.log".into(),
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
        let c = check_spawn(&host(), &req()).unwrap();
        assert_eq!(c.groups, None);
        let mut r = req();
        r.groups = Some(vec![Gid(27)]);
        assert!(check_spawn(&host(), &r).is_ok());
    }

    #[test]
    fn rejects_root_and_foreign_groups() {
        let mut r = req();
        r.uid = Uid(0);
        assert_eq!(
            check_spawn(&host(), &r).unwrap_err().code,
            HelperErrorCode::InvalidRequest
        );
        let mut r = req();
        r.gid = Gid(999); // e.g. docker
        assert_eq!(
            check_spawn(&host(), &r).unwrap_err().code,
            HelperErrorCode::PermissionDenied
        );
        let mut r = req();
        r.groups = Some(vec![Gid(1000), Gid(0)]);
        assert!(check_spawn(&host(), &r).is_err());
        let mut r = req();
        r.uid = Uid(4242);
        assert!(check_spawn(&host(), &r).is_err());
    }

    #[test]
    fn rejects_bad_paths_and_env() {
        let mut r = req();
        r.cwd = "relative".into();
        assert!(check_spawn(&host(), &r).is_err());
        let mut r = req();
        r.log_path = "/var/lib/puzuz/logs/../../../etc/shadow".into();
        assert!(check_spawn(&host(), &r).is_err());
        let mut r = req();
        r.envs.insert("A=B".into(), "x".into());
        assert!(check_spawn(&host(), &r).is_err());
        let mut r = req();
        r.shell = vec![];
        assert!(check_spawn(&host(), &r).is_err());
    }

    #[test]
    fn image_dir_must_be_the_jobs_own() {
        let base = Path::new("/var/lib/puzuz/checkpoints");
        assert!(check_image_dir(base, JobId(3), &base.join("3")).is_ok());
        assert!(check_image_dir(base, JobId(3), &base.join("4")).is_err());
        assert!(check_image_dir(base, JobId(3), Path::new("/etc")).is_err());
    }
}
