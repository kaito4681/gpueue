//! Opening job logs without trusting their directory's contents.
//!
//! A job's log is `<logs_dir>/<uid>/<id>.log`. The helper creates
//! `<logs_dir>/<uid>/` owned by the job's owner (group `gpueue`, mode 2770),
//! so the owner can put anything there under the log's name: a symlink to
//! the daemon's database or to another user's log, a hard link to a file the
//! `gpueue` group may read, a FIFO that blocks the opener. Following it would
//! let the owner read (`gpueue log`) or truncate (`restart --in-place`) files
//! only the daemon may access.
//!
//! [`open`] therefore opens the user directory and then the log relative to
//! it, neither following symlinks nor blocking, and accepts only a regular
//! file owned by the job's owner. Truncation goes through that same handle.

use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use gpueue_core::model::{JobId, Uid};
use nix::fcntl::{OFlag, open as open_at_cwd, openat};
use nix::sys::stat::Mode;

/// How the log is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

/// The file name of job `id`'s log inside its owner's log directory.
pub fn file_name(id: JobId) -> String {
    format!("{id}.log")
}

fn refuse(path: &Path, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("refusing to open {}: {why}", path.display()),
    )
}

/// Opens job `id`'s log in `logs_dir` (see the module documentation).
/// Fails with [`io::ErrorKind::NotFound`] when there is no log (yet), and
/// with [`io::ErrorKind::PermissionDenied`] when the log or the user
/// directory is not what the helper creates.
pub fn open(logs_dir: &Path, owner: Uid, id: JobId, access: Access) -> io::Result<File> {
    let dir_path = logs_dir.join(owner.0.to_string());
    let path = dir_path.join(file_name(id));
    let nofollow = OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let dir: OwnedFd = open_at_cwd(
        &dir_path,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | nofollow,
        Mode::empty(),
    )
    .map_err(|e| match e {
        nix::Error::ELOOP | nix::Error::ENOTDIR => refuse(&dir_path, "not a directory"),
        e => io::Error::from(e),
    })?;
    let mode = match access {
        Access::Read => OFlag::O_RDONLY,
        // No O_TRUNC: nothing may change before the checks below.
        Access::Write => OFlag::O_WRONLY,
    };
    // O_NONBLOCK: opening a FIFO must not hang (it is rejected below).
    let fd = openat(
        &dir,
        file_name(id).as_str(),
        mode | nofollow | OFlag::O_NONBLOCK | OFlag::O_NOCTTY,
        Mode::empty(),
    )
    .map_err(|e| match e {
        nix::Error::ELOOP => refuse(&path, "it is a symbolic link"),
        e => io::Error::from(e),
    })?;
    let file = File::from(fd);
    let meta = file.metadata()?;
    if !meta.file_type().is_file() {
        return Err(refuse(&path, "not a regular file"));
    }
    if meta.uid() != owner.0 {
        return Err(refuse(&path, "not owned by the job's owner"));
    }
    // Regular files ignore O_NONBLOCK; clear it all the same.
    nix::fcntl::fcntl(&file, nix::fcntl::FcntlArg::F_SETFL(OFlag::empty()))
        .map_err(io::Error::from)?;
    Ok(file)
}

/// Empties job `id`'s log (`restart --in-place`). A missing log is fine.
pub fn truncate(logs_dir: &Path, owner: Uid, id: JobId) -> io::Result<()> {
    match open(logs_dir, owner, id, Access::Write) {
        Ok(file) => file.set_len(0),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    fn me() -> Uid {
        Uid(nix::unistd::getuid().as_raw())
    }

    /// `<tmp>/logs/<me>/` and the path of job 1's log in it.
    fn setup() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let logs = tmp.path().join("logs");
        let dir = logs.join(me().0.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join(file_name(JobId(1)));
        (tmp, logs, log)
    }

    fn read(logs: &Path, owner: Uid) -> io::Result<String> {
        let mut file = open(logs, owner, JobId(1), Access::Read)?;
        let mut text = String::new();
        io::Read::read_to_string(&mut file, &mut text)?;
        Ok(text)
    }

    #[test]
    fn opens_and_truncates_a_regular_log_of_the_owner() {
        let (_tmp, logs, log) = setup();
        assert_eq!(
            read(&logs, me()).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        truncate(&logs, me(), JobId(1)).unwrap();
        assert!(!log.exists());
        std::fs::write(&log, "hello\n").unwrap();
        assert_eq!(read(&logs, me()).unwrap(), "hello\n");
        truncate(&logs, me(), JobId(1)).unwrap();
        assert_eq!(read(&logs, me()).unwrap(), "");
    }

    #[test]
    fn refuses_symlinks_and_leaves_their_target_alone() {
        let (tmp, logs, log) = setup();
        let secret = tmp.path().join("state.sqlite");
        std::fs::write(&secret, "secret").unwrap();
        symlink(&secret, &log).unwrap();
        let err = read(&logs, me()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        let err = truncate(&logs, me(), JobId(1)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "secret");
        // A dangling symlink is no "missing log" either.
        std::fs::remove_file(&secret).unwrap();
        assert_eq!(
            truncate(&logs, me(), JobId(1)).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn refuses_a_symlinked_user_directory() {
        let (tmp, logs, _log) = setup();
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::fs::write(elsewhere.join(file_name(JobId(1))), "x").unwrap();
        let other = Uid(me().0.wrapping_add(1));
        symlink(&elsewhere, logs.join(other.0.to_string())).unwrap();
        let err = read(&logs, other).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
    }

    #[test]
    fn refuses_fifos_without_blocking_and_files_of_other_owners() {
        let (_tmp, logs, log) = setup();
        nix::unistd::mkfifo(&log, Mode::from_bits_truncate(0o600)).unwrap();
        let err = read(&logs, me()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        std::fs::remove_file(&log).unwrap();

        // A file someone else owns (here: the log of a job of `me` read as
        // another owner's job, through a directory of that name).
        std::fs::write(&log, "x").unwrap();
        let other = Uid(me().0.wrapping_add(1));
        let other_dir = logs.join(other.0.to_string());
        std::fs::create_dir(&other_dir).unwrap();
        std::fs::hard_link(&log, other_dir.join(file_name(JobId(1)))).unwrap();
        let err = read(&logs, other).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        assert_eq!(
            truncate(&logs, other, JobId(1)).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "x");
    }
}
