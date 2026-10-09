//! A scripted [`System`] for tests: files live below a temporary directory and commands answer
//! with canned output.

use std::cell::RefCell;
use std::collections::HashSet;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use puzuz_protocol::api::{HelperStatus, VersionResponse};

use super::{CmdOutput, FsUsage, System};

pub struct FakeSystem {
    pub dir: tempfile::TempDir,
    pub euid: u32,
    pub exe: PathBuf,
    /// Existing groups.
    pub groups: RefCell<HashSet<String>>,
    /// Groups the invoking user is in.
    pub member_of: HashSet<String>,
    /// `(prefix, answer)`: a command line (program base name and arguments joined by spaces)
    /// starting with `prefix` gets `answer`; `None` means "program not found". The last
    /// matching entry wins; unmatched commands succeed with empty output.
    pub responses: RefCell<Vec<(String, Option<CmdOutput>)>>,
    /// Every command line run, in order.
    pub log: RefCell<Vec<String>>,
    pub fs_usage: FsUsage,
    pub daemon: Result<VersionResponse, String>,
    pub confirm_answer: bool,
    pub prompts: RefCell<Vec<String>>,
}

impl FakeSystem {
    pub fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
            euid: 0,
            exe: PathBuf::from("/usr/bin/puzuz"),
            groups: RefCell::default(),
            member_of: HashSet::new(),
            responses: RefCell::default(),
            log: RefCell::default(),
            fs_usage: FsUsage {
                available_bytes: 500 << 30,
                tmpfs: false,
            },
            daemon: Err("cannot connect to the daemon socket: not found".to_owned()),
            confirm_answer: false,
            prompts: RefCell::default(),
        }
    }

    pub fn respond(&self, prefix: &str, output: CmdOutput) {
        self.responses
            .borrow_mut()
            .push((prefix.to_owned(), Some(output)));
    }

    pub fn missing(&self, prefix: &str) {
        self.responses.borrow_mut().push((prefix.to_owned(), None));
    }

    /// Writes a file at a system path, creating parent directories.
    pub fn write(&self, path: &str, contents: &str) {
        let path = self.path(Path::new(path));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    pub fn write_exe(&self, path: &str, contents: &str) {
        self.write(path, contents);
        let path = self.path(Path::new(path));
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    pub fn mkdir(&self, path: &str) {
        std::fs::create_dir_all(self.path(Path::new(path))).unwrap();
    }

    pub fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.path(Path::new(path))).ok()
    }

    pub fn exists(&self, path: &str) -> bool {
        self.path(Path::new(path)).symlink_metadata().is_ok()
    }

    /// Commands run so far.
    pub fn commands(&self) -> Vec<String> {
        self.log.borrow().clone()
    }

    pub fn ran(&self, prefix: &str) -> bool {
        self.log
            .borrow()
            .iter()
            .any(|line| line.starts_with(prefix))
    }

    pub fn daemon_ok(helper_connected: bool) -> Result<VersionResponse, String> {
        Ok(VersionResponse {
            api_version: 1,
            daemon_version: puzuz_protocol::BINARY_VERSION.to_owned(),
            helper: HelperStatus {
                connected: helper_connected,
                version: helper_connected.then(|| puzuz_protocol::BINARY_VERSION.to_owned()),
            },
        })
    }
}

impl System for FakeSystem {
    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn run(&self, program: &str, args: &[&str]) -> io::Result<CmdOutput> {
        let name = Path::new(program)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let line = std::iter::once(name.as_str())
            .chain(args.iter().copied())
            .collect::<Vec<_>>()
            .join(" ");
        self.log.borrow_mut().push(line.clone());
        let answer = self
            .responses
            .borrow()
            .iter()
            .rev()
            .find(|(prefix, _)| line.starts_with(prefix.as_str()))
            .map(|(_, answer)| answer.clone());
        match answer {
            Some(Some(output)) => Ok(output),
            Some(None) => Err(io::Error::new(io::ErrorKind::NotFound, "not found")),
            None => Ok(CmdOutput::ok("")),
        }
    }

    fn euid(&self) -> u32 {
        self.euid
    }

    fn root_uid(&self) -> u32 {
        // Files in the temporary root belong to whoever runs the tests.
        std::fs::metadata(self.dir.path()).unwrap().uid()
    }

    fn current_exe(&self) -> io::Result<PathBuf> {
        Ok(self.exe.clone())
    }

    fn group_exists(&self, name: &str) -> bool {
        self.groups.borrow().contains(name)
    }

    fn user_in_group(&self, name: &str) -> Option<bool> {
        self.group_exists(name)
            .then(|| self.member_of.contains(name))
    }

    fn fs_usage(&self, path: &Path) -> io::Result<FsUsage> {
        std::fs::metadata(self.path(path))?;
        Ok(self.fs_usage)
    }

    fn daemon_version(&self, _socket: &Path) -> Result<VersionResponse, String> {
        self.daemon.clone()
    }

    fn confirm(&self, prompt: &str) -> bool {
        self.prompts.borrow_mut().push(prompt.to_owned());
        self.confirm_answer
    }

    fn sleep(&self, _duration: Duration) {}
}
