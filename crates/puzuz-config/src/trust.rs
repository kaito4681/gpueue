//! Reading the configuration as root.
//!
//! The helper runs as root, so whoever can change its configuration can
//! make root run any program (`criu_path`) or write anywhere
//! (`checkpoints_dir`). It therefore only reads a file that nobody but root
//! can change: the file and every parent directory must be owned by root,
//! must not be group/world-writable and must not be symlinks.

use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::{Config, ConfigError};

/// Every existing ancestor of `path` (and `path` itself, if it exists) must
/// be owned by root, not writable by group or others, and not a symlink;
/// otherwise a less privileged user could redirect what root reads or
/// writes there.
pub fn check_root_owned_path(path: &Path) -> Result<(), String> {
    check_owned_path(path, 0, None)
}

/// [`check_root_owned_path`] for `owner` instead of root, checking the
/// ancestors only up to (and including) `stop` when given. For tests.
pub fn check_owned_path(path: &Path, owner: u32, stop: Option<&Path>) -> Result<(), String> {
    for p in path.ancestors() {
        if p.as_os_str().is_empty() {
            break;
        }
        let meta = match std::fs::symlink_metadata(p) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("{}: {e}", p.display())),
        };
        if meta.file_type().is_symlink() {
            return Err(format!("{} is a symlink", p.display()));
        }
        if meta.uid() != owner || meta.mode() & 0o022 != 0 {
            return Err(format!(
                "{} must be owned by {} and not group/world-writable",
                p.display(),
                if owner == 0 { "root" } else { "its owner" }
            ));
        }
        if Some(p) == stop {
            break;
        }
    }
    Ok(())
}

impl Config {
    /// Reads `path` for a process running as root (the helper): refuses a
    /// file that is not an absolute path, is a symlink, or that anyone but
    /// root could have written (see [`check_root_owned_path`]). A missing
    /// file yields the defaults.
    pub fn load_trusted(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        Self::load_owned_by(path.as_ref(), 0, None)
    }

    /// [`Config::load_trusted`] for files owned by `owner`, checking
    /// ancestors up to `stop`. For tests.
    pub fn load_owned_by(
        path: &Path,
        owner: u32,
        stop: Option<&Path>,
    ) -> Result<Self, ConfigError> {
        let untrusted = |message: String| ConfigError::Untrusted {
            path: path.to_owned(),
            message,
        };
        if !path.is_absolute() {
            return Err(untrusted("the path must be absolute".to_owned()));
        }
        check_owned_path(path, owner, stop).map_err(untrusted)?;
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        // The parents are trusted, so the file cannot have been swapped since
        // the check; look at what was opened anyway.
        let meta = file.metadata().map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        if !meta.is_file() {
            return Err(untrusted("not a regular file".to_owned()));
        }
        if meta.uid() != owner || meta.mode() & 0o022 != 0 {
            return Err(untrusted(
                "the file must be owned by root and not group/world-writable".to_owned(),
            ));
        }
        let text = io::read_to_string(file).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::parse_file(path, &text)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use super::*;

    fn me(dir: &Path) -> u32 {
        std::fs::metadata(dir).unwrap().uid()
    }

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let etc = dir.path().join("etc");
        std::fs::create_dir(&etc).unwrap();
        std::fs::set_permissions(&etc, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = etc.join("config.toml");
        (dir, path)
    }

    fn write(path: &Path, text: &str, mode: u32) {
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn accepts_a_protected_file_and_ignores_unknown_keys() {
        let (dir, path) = setup();
        write(
            &path,
            "criu_path = \"/opt/criu\"\nsome_future_key = 1\n[future_table]\nx = 2\n",
            0o644,
        );
        let config = Config::load_owned_by(&path, me(dir.path()), Some(dir.path())).unwrap();
        assert_eq!(config.criu_path, PathBuf::from("/opt/criu"));
    }

    #[test]
    fn missing_file_gives_defaults() {
        let (dir, path) = setup();
        let config = Config::load_owned_by(&path, me(dir.path()), Some(dir.path())).unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn refuses_writable_or_foreign_files() {
        let (dir, path) = setup();
        let uid = me(dir.path());
        write(&path, "", 0o664);
        let err = Config::load_owned_by(&path, uid, Some(dir.path())).unwrap_err();
        assert!(matches!(err, ConfigError::Untrusted { .. }), "{err}");

        write(&path, "", 0o646);
        assert!(Config::load_owned_by(&path, uid, Some(dir.path())).is_err());

        // Owned by someone else than the expected owner.
        write(&path, "", 0o644);
        let err = Config::load_owned_by(&path, uid + 1, Some(dir.path())).unwrap_err();
        assert!(err.to_string().contains("refusing to use"), "{err}");
    }

    #[test]
    fn refuses_writable_parents_and_symlinks() {
        let (dir, path) = setup();
        let uid = me(dir.path());
        write(&path, "", 0o644);
        let etc = path.parent().unwrap();
        std::fs::set_permissions(etc, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(Config::load_owned_by(&path, uid, Some(dir.path())).is_err());
        // Even when the file itself does not exist yet.
        std::fs::remove_file(&path).unwrap();
        assert!(Config::load_owned_by(&path, uid, Some(dir.path())).is_err());
        std::fs::set_permissions(etc, std::fs::Permissions::from_mode(0o755)).unwrap();

        let real = dir.path().join("real.toml");
        write(&real, "", 0o644);
        std::os::unix::fs::symlink(&real, &path).unwrap();
        let err = Config::load_owned_by(&path, uid, Some(dir.path())).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
    }

    #[test]
    fn refuses_relative_paths_and_reports_parse_errors() {
        assert!(matches!(
            Config::load_trusted("config.toml"),
            Err(ConfigError::Untrusted { .. })
        ));
        let (dir, path) = setup();
        write(&path, "grace_secs = \"x\"\n", 0o644);
        assert!(matches!(
            Config::load_owned_by(&path, me(dir.path()), Some(dir.path())),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn root_check_rejects_user_owned_paths() {
        let (_dir, path) = setup();
        write(&path, "", 0o644);
        if me(path.parent().unwrap()) != 0 {
            assert!(check_root_owned_path(&path).is_err());
            assert!(Config::load_trusted(&path).is_err());
        }
    }
}
