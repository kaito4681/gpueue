//! Daemon configuration (`/etc/puzuz/config.toml`).
//!
//! Only settings that are fixed at start-up live here; groups and GPU slot
//! limits are stored in SQLite and changed with commands. Every key is
//! optional. Unknown keys are ignored because the root helper reads the same
//! file for its own settings.
//!
//! ```toml
//! data_dir = "/var/lib/puzuz"          # daemon/state.sqlite, logs/, checkpoints/
//! # state_db = "/var/lib/puzuz/daemon/state.sqlite"
//! # logs_dir = "/var/lib/puzuz/logs"
//! # checkpoints_dir = "/var/lib/puzuz/checkpoints"
//! api_socket = "/run/puzuz/puzuz.sock"
//! helper_socket = "/run/puzuz/helper.sock"
//! grace_secs = 30
//! shell = ["sh", "-c"]
//! # checkpoint_size_limit_bytes = 500_000_000_000
//! checkpoint_tcp_close = true
//!
//! # Until NVML detection exists, GPUs are listed here.
//! [[gpus]]
//! index = 0
//! uuid = "GPU-..."
//! name = "NVIDIA GeForce RTX 4090"
//! ```

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Default location of the configuration file.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/puzuz/config.toml";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Base directory for the state database, logs and checkpoints.
    pub data_dir: PathBuf,
    /// Overrides `<data_dir>/daemon/state.sqlite`. The database lives in a
    /// subdirectory because `<data_dir>` itself must be owned by root (the
    /// helper refuses to use paths below a directory the daemon can write).
    pub state_db: Option<PathBuf>,
    /// Overrides `<data_dir>/logs`. Logs are `<logs_dir>/<uid>/<id>.log`.
    pub logs_dir: Option<PathBuf>,
    /// Overrides `<data_dir>/checkpoints`. Images are `<dir>/<id>/`.
    pub checkpoints_dir: Option<PathBuf>,
    /// The CLI socket (world-connectable; callers are identified with
    /// `SO_PEERCRED`).
    pub api_socket: PathBuf,
    /// The root helper's socket.
    pub helper_socket: PathBuf,
    /// Seconds between SIGTERM and SIGKILL when stopping a job.
    pub grace_secs: u64,
    /// Command prefix a job's command line is appended to.
    pub shell: Vec<String>,
    /// Warn when all checkpoint images together exceed this size.
    pub checkpoint_size_limit_bytes: Option<u64>,
    /// Close TCP connections when checkpointing (`criu --tcp-close`).
    pub checkpoint_tcp_close: bool,
    /// Path of `criu` (used by the helper).
    pub criu_path: PathBuf,
    /// Path of `cuda-checkpoint` (used by the helper).
    pub cuda_checkpoint_path: PathBuf,
    /// Statically configured GPUs, used until NVML detection is implemented.
    pub gpus: Vec<GpuConfig>,
}

/// One GPU in [`Config::gpus`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuConfig {
    pub index: u32,
    #[serde(default)]
    pub uuid: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub memory_total_bytes: Option<u64>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("/var/lib/puzuz"),
            state_db: None,
            logs_dir: None,
            checkpoints_dir: None,
            api_socket: PathBuf::from(puzuz_protocol::api::DEFAULT_SOCKET_PATH),
            helper_socket: PathBuf::from(puzuz_protocol::helper::DEFAULT_SOCKET_PATH),
            grace_secs: 30,
            shell: vec!["sh".to_owned(), "-c".to_owned()],
            checkpoint_size_limit_bytes: None,
            checkpoint_tcp_close: true,
            criu_path: PathBuf::from("criu"),
            cuda_checkpoint_path: PathBuf::from("cuda-checkpoint"),
            gpus: Vec::new(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid configuration in {}: {source}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: Box<toml::de::Error>,
    },
    #[error("invalid configuration in {}: {message}", path.display())]
    Invalid { path: PathBuf, message: String },
}

impl Config {
    /// Reads `path`. A missing file yields the defaults.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        Self::parse(&text).map_err(|e| match e {
            ParseError::Toml(source) => ConfigError::Parse {
                path: path.to_owned(),
                source: Box::new(source),
            },
            ParseError::Invalid(message) => ConfigError::Invalid {
                path: path.to_owned(),
                message,
            },
        })
    }

    /// Parses and validates a configuration file's contents.
    pub fn parse(text: &str) -> Result<Self, ParseError> {
        let config: Config = toml::from_str(text).map_err(ParseError::Toml)?;
        config.validate().map_err(ParseError::Invalid)?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), String> {
        if self.shell.is_empty() {
            return Err("`shell` must not be empty".to_owned());
        }
        let mut seen = std::collections::HashSet::new();
        for gpu in &self.gpus {
            if !seen.insert(gpu.index) {
                return Err(format!("GPU {} is listed twice", gpu.index));
            }
        }
        Ok(())
    }

    /// A configuration rooted at `data_dir` (for `puzuz daemon --data-dir`
    /// and tests); derived paths follow it unless set explicitly.
    pub fn with_data_dir(mut self, data_dir: impl Into<PathBuf>) -> Self {
        self.data_dir = data_dir.into();
        self
    }

    pub fn state_db_path(&self) -> PathBuf {
        self.state_db
            .clone()
            .unwrap_or_else(|| self.data_dir.join("daemon").join("state.sqlite"))
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.logs_dir
            .clone()
            .unwrap_or_else(|| self.data_dir.join("logs"))
    }

    pub fn checkpoints_dir(&self) -> PathBuf {
        self.checkpoints_dir
            .clone()
            .unwrap_or_else(|| self.data_dir.join("checkpoints"))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error(transparent)]
    Toml(toml::de::Error),
    #[error("{0}")]
    Invalid(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_gives_defaults() {
        let config = Config::parse("").unwrap();
        assert_eq!(config, Config::default());
        assert_eq!(
            config.state_db_path(),
            PathBuf::from("/var/lib/puzuz/daemon/state.sqlite")
        );
        assert_eq!(config.logs_dir(), PathBuf::from("/var/lib/puzuz/logs"));
        assert_eq!(
            config.checkpoints_dir(),
            PathBuf::from("/var/lib/puzuz/checkpoints")
        );
        assert_eq!(config.api_socket, PathBuf::from("/run/puzuz/puzuz.sock"));
        assert_eq!(
            config.helper_socket,
            PathBuf::from("/run/puzuz/helper.sock")
        );
        assert_eq!(config.shell, ["sh", "-c"]);
        assert_eq!(config.grace_secs, 30);
    }

    #[test]
    fn parses_every_key_and_ignores_unknown_ones() {
        let config = Config::parse(
            r#"
            data_dir = "/srv/puzuz"
            checkpoints_dir = "/big/ckpt"
            api_socket = "/tmp/p.sock"
            helper_socket = "/tmp/h.sock"
            grace_secs = 5
            shell = ["bash", "-lc"]
            checkpoint_size_limit_bytes = 1000
            checkpoint_tcp_close = false
            helper_only_setting = "x"

            [[gpus]]
            index = 0
            uuid = "GPU-a"
            name = "A100"

            [[gpus]]
            index = 1
            "#,
        )
        .unwrap();
        assert_eq!(
            config.state_db_path(),
            PathBuf::from("/srv/puzuz/daemon/state.sqlite")
        );
        assert_eq!(config.logs_dir(), PathBuf::from("/srv/puzuz/logs"));
        assert_eq!(config.checkpoints_dir(), PathBuf::from("/big/ckpt"));
        assert_eq!(config.grace_secs, 5);
        assert_eq!(config.shell, ["bash", "-lc"]);
        assert_eq!(config.checkpoint_size_limit_bytes, Some(1000));
        assert!(!config.checkpoint_tcp_close);
        assert_eq!(config.gpus.len(), 2);
        assert_eq!(config.gpus[0].name, "A100");
        assert_eq!(config.gpus[1].uuid, "");
    }

    #[test]
    fn rejects_bad_values() {
        assert!(matches!(
            Config::parse("grace_secs = \"x\""),
            Err(ParseError::Toml(_))
        ));
        assert!(matches!(
            Config::parse("shell = []"),
            Err(ParseError::Invalid(_))
        ));
        assert!(matches!(
            Config::parse("[[gpus]]\nindex = 0\n[[gpus]]\nindex = 0\n"),
            Err(ParseError::Invalid(_))
        ));
    }

    #[test]
    fn load_handles_missing_and_present_files() {
        let dir = tempfile::tempdir().unwrap();
        let missing = Config::load(dir.path().join("nope.toml")).unwrap();
        assert_eq!(missing, Config::default());

        let path = dir.path().join("config.toml");
        std::fs::write(&path, "grace_secs = 7\n").unwrap();
        assert_eq!(Config::load(&path).unwrap().grace_secs, 7);

        std::fs::write(&path, "grace_secs = \n").unwrap();
        let err = Config::load(&path).unwrap_err();
        assert!(err.to_string().contains("config.toml"), "{err}");
    }

    #[test]
    fn data_dir_override_moves_derived_paths() {
        let config = Config::default().with_data_dir("/tmp/x");
        assert_eq!(
            config.state_db_path(),
            PathBuf::from("/tmp/x/daemon/state.sqlite")
        );
        assert_eq!(config.logs_dir(), PathBuf::from("/tmp/x/logs"));
    }
}
