//! The configuration file `/etc/puzuz/config.toml`, shared by the daemon,
//! the root helper and `puzuz doctor`.
//!
//! Only settings that are fixed at start-up live here; groups and GPU slot
//! limits are stored in the daemon's SQLite database and changed with
//! commands. Every key is optional. Unknown keys are ignored, so a newer
//! file still works with an older binary.
//!
//! The daemon reads the file with [`Config::load`]. The helper, which runs
//! as root, reads it with [`Config::load_trusted`]: the file and every
//! parent directory must be owned by root and not group/world-writable
//! (see [`trust`]). Command-line flags of the helper override the file.
//!
//! A file that spells out every default, followed by examples of the keys
//! that are unset by default (`packaging/config.toml` documents each key):
//!
//! ```toml
//! data_dir = "/var/lib/puzuz"          # daemon/, helper/, logs/, checkpoints/
//! # Unset by default; they follow data_dir.
//! # state_db = "/var/lib/puzuz/daemon/state.sqlite"
//! # helper_state_dir = "/var/lib/puzuz/helper"
//! # logs_dir = "/var/lib/puzuz/logs"
//! # checkpoints_dir = "/var/lib/puzuz/checkpoints"
//! api_socket = "/run/puzuz/puzuz.sock"
//! helper_socket = "/run/puzuz/helper.sock"
//! daemon_user = "puzuz"
//! grace_secs = 30
//! shell = ["sh", "-c"]
//! checkpoint_size_warning_interval_secs = 3600
//! checkpoint_tcp_close = true
//! criu_path = "criu"
//! cuda_checkpoint_path = "cuda-checkpoint"
//! nvidia_smi_path = "nvidia-smi"
//! cuda_checkpoint_mode = "manual"
//! checkpoint_deadline_base_secs = 180
//! checkpoint_deadline_secs_per_gib = 30
//! fork_guard = true
//! # GPUs are detected with NVML ("auto"). Without NVML (no driver, no
//! # libnvidia-ml.so.1) the [[gpus]] list is used instead; "static" always
//! # uses the list.
//! gpu_detection = "auto"
//!
//! # Examples; unset by default.
//! checkpoint_size_limit_bytes = 500_000_000_000   # default: no limit
//! criu_dump_timeout_secs = 3600      # default: only the deadline applies
//! criu_restore_timeout_secs = 3600   # default: only the deadline applies
//! nvml_library = "/usr/lib/x86_64-linux-gnu/libnvidia-ml.so.1"
//!
//! # The [[gpus]] list is empty by default. See [`GpuConfig`] for the fields.
//! [[gpus]]
//! index = 0                          # required
//! uuid = "GPU-..."                   # default ""
//! name = "NVIDIA GeForce RTX 4090"   # default ""
//! memory_total_bytes = 25_757_220_864  # default unset
//! ```

pub mod trust;

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Default location of the configuration file.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/puzuz/config.toml";

/// Upper bound of every `*_secs` key (one year). Larger values are surely
/// mistakes, and would overflow deadline arithmetic in the daemon and helper.
pub const MAX_SECS: u64 = 365 * 24 * 60 * 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Base directory for the state database, logs and checkpoints.
    pub data_dir: PathBuf,
    /// Overrides `<data_dir>/daemon/state.sqlite`. The database lives in a
    /// subdirectory because `<data_dir>` itself must be owned by root (the
    /// helper refuses to use paths below a directory the daemon can write).
    pub state_db: Option<PathBuf>,
    /// Overrides `<data_dir>/helper`: the helper's event queue and job
    /// records. Must be owned by root (used by the helper only).
    pub helper_state_dir: Option<PathBuf>,
    /// Overrides `<data_dir>/logs`. Logs are `<logs_dir>/<uid>/<id>.log`.
    pub logs_dir: Option<PathBuf>,
    /// Overrides `<data_dir>/checkpoints`. Images are `<dir>/<id>/`.
    pub checkpoints_dir: Option<PathBuf>,
    /// The CLI socket (world-connectable; callers are identified with
    /// `SO_PEERCRED`).
    pub api_socket: PathBuf,
    /// The root helper's socket.
    pub helper_socket: PathBuf,
    /// The system user the daemon runs as (`User=` of `puzuz.service`).
    /// The helper accepts connections only from this user.
    pub daemon_user: String,
    /// Seconds between SIGTERM and SIGKILL when stopping a job (the daemon's
    /// `kill`/`stop`, and `TimeoutStopSec=` of the helper's job units).
    pub grace_secs: u64,
    /// Command prefix a job's command line is appended to.
    pub shell: Vec<String>,
    /// Warn when all checkpoint images together exceed this size (images
    /// are never deleted automatically).
    pub checkpoint_size_limit_bytes: Option<u64>,
    /// Remind each administrator of an exceeded
    /// `checkpoint_size_limit_bytes` at most this often (daemon only).
    pub checkpoint_size_warning_interval_secs: u64,
    /// Close TCP connections when checkpointing (`criu --tcp-close`).
    pub checkpoint_tcp_close: bool,
    /// Path of `criu` (used by the helper).
    pub criu_path: PathBuf,
    /// Path of `cuda-checkpoint` (used by the helper).
    pub cuda_checkpoint_path: PathBuf,
    /// Path of `nvidia-smi` (used by the helper to find CUDA processes).
    pub nvidia_smi_path: PathBuf,
    /// Who drives `cuda-checkpoint` during a dump/restore (helper only).
    pub cuda_checkpoint_mode: CudaCheckpointMode,
    /// Fixed part of the deadline of a whole checkpoint or restore of a
    /// job, in seconds (helper only). The deadline is
    /// `checkpoint_deadline_base_secs + checkpoint_deadline_secs_per_gib *
    /// GiB`, where GiB is the job's RSS + VRAM (rounded up) for a
    /// checkpoint and the image size for a restore. When it passes, the
    /// running tool is killed and the job is resumed (checkpoint) or
    /// killed (restore; the image is kept).
    pub checkpoint_deadline_base_secs: u64,
    /// Seconds per GiB of the deadline above (helper only).
    pub checkpoint_deadline_secs_per_gib: u64,
    /// Optional cap, in seconds, on `criu dump` alone (helper only). Unset:
    /// only the deadline applies.
    pub criu_dump_timeout_secs: Option<u64>,
    /// Optional cap, in seconds, on `criu restore` alone (helper only).
    /// Unset: only the deadline applies.
    pub criu_restore_timeout_secs: Option<u64>,
    /// Preload the fork guard into jobs (helper only): children that a
    /// CUDA process forks without exec, such as PyTorch DataLoader workers
    /// with the default `fork` start method, drop the inherited NVIDIA
    /// device mappings, which CRIU cannot dump. Installed into
    /// `<data_dir>/lib`.
    pub fork_guard: bool,
    /// How GPUs are found at start-up.
    pub gpu_detection: GpuDetection,
    /// Path (or name) of the NVML library; by default `libnvidia-ml.so.1`
    /// and then `libnvidia-ml.so` are tried.
    pub nvml_library: Option<PathBuf>,
    /// Statically configured GPUs: used when NVML is unavailable, or always
    /// with `gpu_detection = "static"`.
    pub gpus: Vec<GpuConfig>,
}

/// [`Config::gpu_detection`]: NVML falling back to [`Config::gpus`]
/// (`auto`), or only [`Config::gpus`] (`static`). Defined in
/// `puzuz-protocol` because `GET /v1/version` reports it.
pub use puzuz_protocol::api::GpuDetection;

/// [`Config::cuda_checkpoint_mode`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CudaCheckpointMode {
    /// The helper calls `cuda-checkpoint` around `criu` itself; works with
    /// the distribution's criu (no CUDA plugin needed).
    #[default]
    Manual,
    /// CRIU's CUDA plugin calls `cuda-checkpoint`.
    Plugin,
}

impl CudaCheckpointMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Plugin => "plugin",
        }
    }
}

/// One `[[gpus]]` entry of [`Config::gpus`]. `puzuz service install` writes
/// one per GPU that `nvidia-smi --query-gpu=index,uuid,name,memory.total`
/// lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuConfig {
    /// Required and unique: the GPU's index in PCI bus order, as nvidia-smi
    /// numbers it. `puzuz add --gpu` takes it, and jobs see it in
    /// `CUDA_VISIBLE_DEVICES` (with `CUDA_DEVICE_ORDER=PCI_BUS_ID`).
    pub index: u32,
    /// The GPU's UUID (`GPU-...`; default empty). The daemon warns at
    /// start-up when a GPU with a different UUID than before is found at an
    /// index.
    #[serde(default)]
    pub uuid: String,
    /// The model (default empty), shown by `puzuz gpu` and recorded with
    /// each checkpoint; restoring requires the same model.
    #[serde(default)]
    pub name: String,
    /// The GPU's memory in bytes (default unset), shown by `puzuz gpu`.
    #[serde(default)]
    pub memory_total_bytes: Option<u64>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("/var/lib/puzuz"),
            state_db: None,
            helper_state_dir: None,
            logs_dir: None,
            checkpoints_dir: None,
            api_socket: PathBuf::from(puzuz_protocol::api::DEFAULT_SOCKET_PATH),
            helper_socket: PathBuf::from(puzuz_protocol::helper::DEFAULT_SOCKET_PATH),
            daemon_user: "puzuz".to_owned(),
            grace_secs: 30,
            shell: vec!["sh".to_owned(), "-c".to_owned()],
            checkpoint_size_limit_bytes: None,
            checkpoint_size_warning_interval_secs: 3600,
            checkpoint_tcp_close: true,
            criu_path: PathBuf::from("criu"),
            cuda_checkpoint_path: PathBuf::from("cuda-checkpoint"),
            nvidia_smi_path: PathBuf::from("nvidia-smi"),
            cuda_checkpoint_mode: CudaCheckpointMode::Manual,
            checkpoint_deadline_base_secs: 180,
            checkpoint_deadline_secs_per_gib: 30,
            criu_dump_timeout_secs: None,
            criu_restore_timeout_secs: None,
            fork_guard: true,
            gpu_detection: GpuDetection::Auto,
            nvml_library: None,
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
    #[error("refusing to use {}: {message}", path.display())]
    Untrusted { path: PathBuf, message: String },
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
        Self::parse_file(path, &text)
    }

    /// [`Config::parse`] with errors that name `path`.
    pub(crate) fn parse_file(path: &Path, text: &str) -> Result<Self, ConfigError> {
        Self::parse(text).map_err(|e| match e {
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
        if self.daemon_user.is_empty() {
            return Err("`daemon_user` must not be empty".to_owned());
        }
        if self.criu_dump_timeout_secs == Some(0) || self.criu_restore_timeout_secs == Some(0) {
            return Err("criu timeouts must be positive".to_owned());
        }
        if self.checkpoint_deadline_base_secs == 0 {
            return Err("`checkpoint_deadline_base_secs` must be positive".to_owned());
        }
        let secs = [
            ("grace_secs", Some(self.grace_secs)),
            (
                "checkpoint_size_warning_interval_secs",
                Some(self.checkpoint_size_warning_interval_secs),
            ),
            (
                "checkpoint_deadline_base_secs",
                Some(self.checkpoint_deadline_base_secs),
            ),
            (
                "checkpoint_deadline_secs_per_gib",
                Some(self.checkpoint_deadline_secs_per_gib),
            ),
            ("criu_dump_timeout_secs", self.criu_dump_timeout_secs),
            ("criu_restore_timeout_secs", self.criu_restore_timeout_secs),
        ];
        for (key, value) in secs {
            if let Some(value) = value
                && value > MAX_SECS
            {
                return Err(format!(
                    "`{key}` = {value} is too large (at most {MAX_SECS}, one year)"
                ));
            }
        }
        // Relative paths would depend on the working directory, and the
        // helper (root) only checks absolute paths for root ownership.
        let dirs = [
            ("data_dir", Some(&self.data_dir)),
            ("state_db", self.state_db.as_ref()),
            ("helper_state_dir", self.helper_state_dir.as_ref()),
            ("logs_dir", self.logs_dir.as_ref()),
            ("checkpoints_dir", self.checkpoints_dir.as_ref()),
            ("api_socket", Some(&self.api_socket)),
            ("helper_socket", Some(&self.helper_socket)),
        ];
        for (key, path) in dirs {
            if let Some(path) = path
                && !path.is_absolute()
            {
                return Err(format!(
                    "`{key}` = {:?} must be an absolute path",
                    path.display().to_string()
                ));
            }
        }
        // Programs are looked up in PATH by bare name; anything with a slash
        // must be absolute.
        let shell = self.shell.first().map(Path::new);
        let programs = [
            ("criu_path", Some(self.criu_path.as_path())),
            (
                "cuda_checkpoint_path",
                Some(self.cuda_checkpoint_path.as_path()),
            ),
            ("nvidia_smi_path", Some(self.nvidia_smi_path.as_path())),
            ("nvml_library", self.nvml_library.as_deref()),
            ("shell", shell),
        ];
        for (key, path) in programs {
            if let Some(path) = path
                && !is_bare_name_or_absolute(path)
            {
                return Err(format!(
                    "`{key}` = {:?} must be a bare program name (looked up in PATH) or an \
                     absolute path",
                    path.display().to_string()
                ));
            }
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

    pub fn helper_state_dir(&self) -> PathBuf {
        self.helper_state_dir
            .clone()
            .unwrap_or_else(|| self.data_dir.join("helper"))
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

/// Whether `path` is a plain file name (no `/`) or an absolute path.
fn is_bare_name_or_absolute(path: &Path) -> bool {
    let bare = !path.as_os_str().is_empty() && !path.as_os_str().as_encoded_bytes().contains(&b'/');
    bare || path.is_absolute()
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
        assert_eq!(
            config.helper_state_dir(),
            PathBuf::from("/var/lib/puzuz/helper")
        );
        assert_eq!(config.daemon_user, "puzuz");
        assert_eq!(config.cuda_checkpoint_mode, CudaCheckpointMode::Manual);
        assert_eq!(config.criu_dump_timeout_secs, None);
        assert_eq!(config.checkpoint_deadline_base_secs, 180);
        assert_eq!(config.checkpoint_deadline_secs_per_gib, 30);
        assert!(config.fork_guard);
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
            checkpoint_size_warning_interval_secs = 60
            checkpoint_tcp_close = false
            gpu_detection = "static"
            nvml_library = "/opt/nvml.so"
            daemon_user = "svc-puzuz"
            nvidia_smi_path = "/opt/nvidia-smi"
            cuda_checkpoint_mode = "plugin"
            criu_dump_timeout_secs = 60
            criu_restore_timeout_secs = 90
            checkpoint_deadline_base_secs = 20
            checkpoint_deadline_secs_per_gib = 0
            some_future_setting = "x"

            [some_future_table]
            key = 1

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
        assert_eq!(config.checkpoint_size_warning_interval_secs, 60);
        assert!(!config.checkpoint_tcp_close);
        assert_eq!(config.gpu_detection, GpuDetection::Static);
        assert_eq!(config.nvml_library, Some(PathBuf::from("/opt/nvml.so")));
        assert_eq!(config.gpus.len(), 2);
        assert_eq!(config.gpus[0].name, "A100");
        assert_eq!(config.gpus[1].uuid, "");
        assert_eq!(
            config.helper_state_dir(),
            PathBuf::from("/srv/puzuz/helper")
        );
        assert_eq!(config.daemon_user, "svc-puzuz");
        assert_eq!(config.nvidia_smi_path, PathBuf::from("/opt/nvidia-smi"));
        assert_eq!(config.cuda_checkpoint_mode, CudaCheckpointMode::Plugin);
        assert_eq!(config.criu_dump_timeout_secs, Some(60));
        assert_eq!(config.criu_restore_timeout_secs, Some(90));
        assert_eq!(config.checkpoint_deadline_base_secs, 20);
        assert_eq!(config.checkpoint_deadline_secs_per_gib, 0);
    }

    #[test]
    fn crate_doc_example_shows_the_defaults_and_examples() {
        let doc: String = include_str!("lib.rs")
            .lines()
            .filter_map(|line| line.strip_prefix("//!"))
            .map(|line| line.strip_prefix(' ').unwrap_or(line))
            .skip_while(|line| *line != "```toml")
            .skip(1)
            .take_while(|line| *line != "```")
            .collect::<Vec<_>>()
            .join("\n");
        let mut config = Config::parse(&doc).unwrap();
        assert_eq!(config.checkpoint_size_limit_bytes, Some(500_000_000_000));
        assert!(config.nvml_library.is_some());
        assert_eq!(
            config.gpus,
            [GpuConfig {
                index: 0,
                uuid: "GPU-...".into(),
                name: "NVIDIA GeForce RTX 4090".into(),
                memory_total_bytes: Some(25_757_220_864),
            }]
        );
        assert_eq!(config.criu_dump_timeout_secs, Some(3600));
        assert_eq!(config.criu_restore_timeout_secs, Some(3600));
        // Everything else is the default.
        config.checkpoint_size_limit_bytes = None;
        config.nvml_library = None;
        config.criu_dump_timeout_secs = None;
        config.criu_restore_timeout_secs = None;
        config.gpus.clear();
        assert_eq!(config, Config::default());
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
            Config::parse("cuda_checkpoint_mode = \"auto\""),
            Err(ParseError::Toml(_))
        ));
        assert!(matches!(
            Config::parse("daemon_user = \"\""),
            Err(ParseError::Invalid(_))
        ));
        assert!(matches!(
            Config::parse("criu_dump_timeout_secs = 0"),
            Err(ParseError::Invalid(_))
        ));
        assert!(matches!(
            Config::parse("checkpoint_deadline_base_secs = 0"),
            Err(ParseError::Invalid(_))
        ));
        assert!(matches!(
            Config::parse("[[gpus]]\nindex = 0\n[[gpus]]\nindex = 0\n"),
            Err(ParseError::Invalid(_))
        ));
    }

    fn invalid(text: &str) -> String {
        match Config::parse(text) {
            Err(ParseError::Invalid(message)) => message,
            other => panic!("{text}: expected a validation error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_huge_durations() {
        for key in [
            "grace_secs",
            "checkpoint_size_warning_interval_secs",
            "checkpoint_deadline_base_secs",
            "checkpoint_deadline_secs_per_gib",
            "criu_dump_timeout_secs",
            "criu_restore_timeout_secs",
        ] {
            let message = invalid(&format!("{key} = {}", MAX_SECS + 1));
            assert!(
                message.contains(key) && message.contains("too large"),
                "{message}"
            );
            let ok = Config::parse(&format!("{key} = {MAX_SECS}"));
            assert!(ok.is_ok(), "{key}: {ok:?}");
        }
        // The value that made `Instant::now() + grace` panic in the helper.
        invalid(&format!("grace_secs = {}", u64::MAX / 2));
    }

    #[test]
    fn rejects_relative_paths() {
        for key in [
            "data_dir",
            "state_db",
            "helper_state_dir",
            "logs_dir",
            "checkpoints_dir",
            "api_socket",
            "helper_socket",
        ] {
            let message = invalid(&format!("{key} = \"var/x\""));
            assert!(message.contains("absolute"), "{message}");
            invalid(&format!("{key} = \"x\""));
        }
        for key in [
            "criu_path",
            "cuda_checkpoint_path",
            "nvidia_smi_path",
            "nvml_library",
        ] {
            let message = invalid(&format!("{key} = \"bin/tool\""));
            assert!(message.contains("bare program name"), "{message}");
            invalid(&format!("{key} = \"./tool\""));
            invalid(&format!("{key} = \"\""));
            Config::parse(&format!("{key} = \"tool\"")).unwrap();
            Config::parse(&format!("{key} = \"/opt/bin/tool\"")).unwrap();
        }
        invalid("shell = [\"bin/sh\", \"-c\"]");
        Config::parse("shell = [\"/bin/bash\", \"-lc\"]").unwrap();
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
