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
//! gpu_device_isolation = true
//! job_system_dbus = false
//! job_systemd_varlink = false
//! checkpoint_deadline_base_secs = 180
//! checkpoint_deadline_secs_per_gib = 30
//! fork_guard = true
//! # Groups dropped from jobs (and refused as a job's group); see
//! # [`job_policy::DEFAULT_JOB_DENIED_GROUPS`].
//! job_denied_groups = ["docker", "lxd", "incus-admin", "libvirt", "libvirtd", "disk", "sudo", "wheel", "admin"]
//! # Directories hidden from jobs behind an empty tmpfs; see
//! # [`job_policy::DEFAULT_JOB_HIDDEN_SOCKET_DIRS`].
//! job_hidden_socket_dirs = ["/run/libvirt", "/run/cups"]
//! job_allowed_users = []             # empty: every user with uid >= job_min_uid
//! update_source = "github:kaito4681/puzuz"
//! update_check = true
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
//! job_min_uid = 1000                 # default: UID_MIN of /etc/login.defs, else 1000
//!
//! # Job priority (Slurm's priority/multifactor); see [`PriorityConfig`].
//! [priority]
//! priority_type = "multifactor"
//! priority_weight_age = 1000
//! priority_weight_fairshare = 10000
//! priority_weight_job_size = 0
//! priority_weight_partition = 0
//! priority_weight_tres = ""
//! priority_max_age_secs = 604800             # 7 days
//! priority_decay_half_life_secs = 604800     # 7 days; 0: no decay
//! priority_calc_period_secs = 300
//! priority_usage_reset_period = "none"
//! priority_favor_small = false
//! tres_billing_weights = "CPU=1.0,GRES/gpu=10.0"
//! enable_user_top = true
//!
//! # The [[gpus]] list is empty by default. See [`GpuConfig`] for the fields.
//! [[gpus]]
//! index = 0                          # required
//! uuid = "GPU-..."                   # default ""
//! name = "NVIDIA GeForce RTX 4090"   # default ""
//! memory_total_bytes = 25_757_220_864  # default unset
//! ```

pub mod job_policy;
pub mod trust;

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Default location of the configuration file.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/puzuz/config.toml";

/// Default of [`Config::update_source`].
pub const DEFAULT_UPDATE_SOURCE: &str = "github:kaito4681/puzuz";

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
    /// Restrict each job unit's device access to the GPUs it was assigned
    /// (helper only): `DevicePolicy=closed` with `DeviceAllow=` of the
    /// assigned GPUs' `/dev/nvidia<minor>` and the shared NVIDIA nodes; CPU
    /// jobs get no NVIDIA device. `false` leaves device access alone (only
    /// `CUDA_VISIBLE_DEVICES` selects the GPUs).
    pub gpu_device_isolation: bool,
    /// Let jobs reach the system D-Bus (helper only). `false` (the
    /// default) hides `/run/dbus` in every job's mount namespace behind an
    /// empty tmpfs (the shim mounts it, creating the directory if needed;
    /// CRIU recreates it on restore), so a job cannot ask systemd,
    /// polkit or other system services for anything (`systemctl`, `run0`,
    /// `pkexec`, `machinectl`, ...). polkit decides group membership from
    /// the user database, not from the job's (reduced) groups, so its
    /// rules would otherwise apply to jobs as to their owners' sessions.
    pub job_system_dbus: bool,
    /// Let jobs reach systemd's Varlink services under `/run/systemd`
    /// (helper only). `false` (the default) hides `/run/systemd` from jobs
    /// except journald's log sockets, resolved's resolution socket and
    /// `resolv.conf` files, and `userdb`: services such as
    /// `io.systemd.Manager` (reload, reexecute), `io.systemd.Credentials`
    /// (decrypting system credentials), mountfsd, nsresourced, machined
    /// or logind authorize callers through polkit without the D-Bus
    /// socket, so [`Config::job_system_dbus`] alone does not keep polkit
    /// rules away from jobs.
    pub job_systemd_varlink: bool,
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
    /// Jobs (spawns and restores) of users with a smaller uid are refused
    /// (helper only). Unset: `UID_MIN` of `/etc/login.defs`, else 1000 (see
    /// [`job_policy::effective_min_uid`]). uid 0 is always refused.
    pub job_min_uid: Option<u32>,
    /// Groups that grant root-equivalent power (helper only; see
    /// [`job_policy::DEFAULT_JOB_DENIED_GROUPS`]). A job whose group is one
    /// of them is refused; they are dropped from a job's supplementary
    /// groups. Names are looked up in `/etc/group`; unknown names are
    /// ignored.
    pub job_denied_groups: Vec<String>,
    /// If not empty, only these users (by name) may run jobs (helper only).
    pub job_allowed_users: Vec<String>,
    /// Directories hidden from every job behind an empty, root-owned tmpfs
    /// (helper only; see [`job_policy::DEFAULT_JOB_HIDDEN_SOCKET_DIRS`]):
    /// those of daemon sockets that authorize callers by looking up their
    /// groups in the user database (directly or through polkit), which
    /// dropping [`Config::job_denied_groups`] from the job does not
    /// defeat. Like `/run/dbus`, a missing directory is created first, so
    /// a socket that appears later is out of reach too. Each entry must be
    /// an absolute path at least two levels deep (see
    /// [`job_policy::check_hidden_socket_dir`]). An empty list hides none.
    pub job_hidden_socket_dirs: Vec<String>,
    /// Where `puzuz update` and `puzuz doctor` look for releases:
    /// `github:<owner>/<repo>` (CLI only).
    pub update_source: String,
    /// Whether `puzuz doctor` checks for a newer release (CLI only).
    pub update_check: bool,
    /// How GPUs are found at start-up.
    pub gpu_detection: GpuDetection,
    /// Path (or name) of the NVML library; by default `libnvidia-ml.so.1`
    /// and then `libnvidia-ml.so` are tried.
    pub nvml_library: Option<PathBuf>,
    /// Statically configured GPUs: used when NVML is unavailable, or always
    /// with `gpu_detection = "static"`.
    pub gpus: Vec<GpuConfig>,
    /// The `[priority]` table: how pending jobs are ordered (daemon only).
    pub priority: PriorityConfig,
}

/// The `[priority]` table: Slurm's multifactor priority settings, named
/// after Slurm's (`PriorityWeightAge` is `priority_weight_age`, ...), with
/// durations in seconds (`*_secs`) like the other keys. See
/// [`puzuz_core::priority`] for the formula.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PriorityConfig {
    /// `"multifactor"`, or `"basic"` for pueue's order (`-o` priority, then
    /// submission order) (`PriorityType`).
    pub priority_type: PriorityType,
    /// `PriorityWeightAge`.
    pub priority_weight_age: u32,
    /// `PriorityWeightFairshare`.
    pub priority_weight_fairshare: u32,
    /// `PriorityWeightJobSize`.
    pub priority_weight_job_size: u32,
    /// `PriorityWeightPartition`; puzuz groups play the role of partitions.
    pub priority_weight_partition: u32,
    /// `PriorityWeightTRES`, e.g. `"GRES/gpu=1000"` or
    /// `"CPU=100,GRES/gpu=1000"`; empty: no TRES factor.
    pub priority_weight_tres: String,
    /// `PriorityMaxAge`: the age factor reaches 1 after this long.
    pub priority_max_age_secs: u64,
    /// `PriorityDecayHalfLife`: usage halves after this long; 0 disables
    /// the decay.
    pub priority_decay_half_life_secs: u64,
    /// `PriorityCalcPeriod`: how often usage is added up and decayed.
    pub priority_calc_period_secs: u64,
    /// `PriorityUsageResetPeriod`.
    pub priority_usage_reset_period: ResetPeriod,
    /// `PriorityFavorSmall`.
    pub priority_favor_small: bool,
    /// `TRESBillingWeights`: what one second of a job counts as for fair
    /// share. Every job counts as one CPU.
    pub tres_billing_weights: String,
    /// Slurm's `SchedulerParameters=enable_user_top`: users may move their
    /// own jobs to the top of their queue (`puzuz top`); administrators
    /// always may.
    pub enable_user_top: bool,
}

pub use puzuz_core::priority::{PriorityType, ResetPeriod, TresWeights};

impl Default for PriorityConfig {
    fn default() -> Self {
        Self {
            priority_type: PriorityType::Multifactor,
            priority_weight_age: 1000,
            priority_weight_fairshare: 10000,
            priority_weight_job_size: 0,
            priority_weight_partition: 0,
            priority_weight_tres: String::new(),
            priority_max_age_secs: 7 * 24 * 60 * 60,
            priority_decay_half_life_secs: 7 * 24 * 60 * 60,
            priority_calc_period_secs: 300,
            priority_usage_reset_period: ResetPeriod::None,
            priority_favor_small: false,
            tres_billing_weights: "CPU=1.0,GRES/gpu=10.0".to_owned(),
            enable_user_top: true,
        }
    }
}

impl PriorityConfig {
    fn validate(&self) -> Result<(), String> {
        TresWeights::parse(&self.priority_weight_tres)
            .map_err(|e| format!("`priority.priority_weight_tres`: {e}"))?;
        TresWeights::parse(&self.tres_billing_weights)
            .map_err(|e| format!("`priority.tres_billing_weights`: {e}"))?;
        if self.priority_calc_period_secs == 0 {
            return Err("`priority.priority_calc_period_secs` must be positive".to_owned());
        }
        for (key, value) in [
            ("priority_max_age_secs", self.priority_max_age_secs),
            (
                "priority_decay_half_life_secs",
                self.priority_decay_half_life_secs,
            ),
            ("priority_calc_period_secs", self.priority_calc_period_secs),
        ] {
            if value > MAX_SECS {
                return Err(format!(
                    "`priority.{key}` = {value} is too large (at most {MAX_SECS}, one year)"
                ));
            }
        }
        Ok(())
    }

    /// The settings the priority calculation needs.
    pub fn params(&self) -> puzuz_core::priority::Params {
        puzuz_core::priority::Params {
            kind: self.priority_type,
            weight_age: self.priority_weight_age,
            weight_fairshare: self.priority_weight_fairshare,
            weight_job_size: self.priority_weight_job_size,
            weight_partition: self.priority_weight_partition,
            // Validated when the file was read.
            weight_tres: TresWeights::parse(&self.priority_weight_tres).unwrap_or_default(),
            max_age_ms: (self.priority_max_age_secs * 1000) as i64,
            favor_small: self.priority_favor_small,
        }
    }

    /// [`PriorityConfig::tres_billing_weights`], parsed.
    pub fn billing_weights(&self) -> TresWeights {
        TresWeights::parse(&self.tres_billing_weights).unwrap_or_default()
    }
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
            gpu_device_isolation: true,
            job_system_dbus: false,
            job_systemd_varlink: false,
            checkpoint_deadline_base_secs: 180,
            checkpoint_deadline_secs_per_gib: 30,
            criu_dump_timeout_secs: None,
            criu_restore_timeout_secs: None,
            fork_guard: true,
            job_min_uid: None,
            job_denied_groups: job_policy::default_denied_groups(),
            job_allowed_users: Vec::new(),
            job_hidden_socket_dirs: job_policy::default_hidden_socket_dirs(),
            update_source: DEFAULT_UPDATE_SOURCE.to_owned(),
            update_check: true,
            gpu_detection: GpuDetection::Auto,
            nvml_library: None,
            gpus: Vec::new(),
            priority: PriorityConfig::default(),
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
        for (key, names) in [
            ("job_denied_groups", &self.job_denied_groups),
            ("job_allowed_users", &self.job_allowed_users),
        ] {
            if let Some(bad) = names
                .iter()
                .find(|n| n.is_empty() || n.contains([':', '\n', '\0']) || n.trim() != *n)
            {
                return Err(format!("`{key}` contains an invalid name {bad:?}"));
            }
        }
        for dir in &self.job_hidden_socket_dirs {
            job_policy::check_hidden_socket_dir(dir)
                .map_err(|e| format!("`job_hidden_socket_dirs`: {e}"))?;
        }
        self.priority.validate()?;
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
        assert!(config.gpu_device_isolation);
        assert!(!config.job_system_dbus);
        assert!(!config.job_systemd_varlink);
        assert_eq!(config.job_min_uid, None);
        assert_eq!(
            config.job_denied_groups,
            job_policy::DEFAULT_JOB_DENIED_GROUPS
        );
        assert!(config.job_allowed_users.is_empty());
        assert_eq!(
            config.job_hidden_socket_dirs,
            job_policy::DEFAULT_JOB_HIDDEN_SOCKET_DIRS
        );
        let p = &config.priority;
        assert_eq!(p.priority_type, PriorityType::Multifactor);
        assert_eq!(p.priority_weight_age, 1000);
        assert_eq!(p.priority_weight_fairshare, 10000);
        assert_eq!(p.priority_weight_job_size, 0);
        assert_eq!(p.priority_weight_partition, 0);
        assert_eq!(p.priority_weight_tres, "");
        assert_eq!(p.priority_max_age_secs, 604_800);
        assert_eq!(p.priority_decay_half_life_secs, 604_800);
        assert_eq!(p.priority_calc_period_secs, 300);
        assert_eq!(p.priority_usage_reset_period, ResetPeriod::None);
        assert!(!p.priority_favor_small);
        assert_eq!(
            p.billing_weights(),
            TresWeights {
                cpu: 1.0,
                gpu: 10.0
            }
        );
        assert!(p.enable_user_top);
        assert_eq!(p.params(), puzuz_core::priority::Params::default());
    }

    #[test]
    fn parses_the_priority_table() {
        let config = Config::parse(
            r#"
            grace_secs = 5

            [priority]
            priority_type = "basic"
            priority_weight_age = 1
            priority_weight_fairshare = 2
            priority_weight_job_size = 3
            priority_weight_partition = 4
            priority_weight_tres = "GRES/gpu=1000"
            priority_max_age_secs = 60
            priority_decay_half_life_secs = 0
            priority_calc_period_secs = 10
            priority_usage_reset_period = "WEEKLY"
            priority_favor_small = true
            tres_billing_weights = "CPU=2"
            enable_user_top = false
            future_key = 1
            "#,
        )
        .unwrap();
        assert_eq!(config.grace_secs, 5);
        let p = &config.priority;
        assert_eq!(p.priority_usage_reset_period, ResetPeriod::Weekly);
        assert!(!p.enable_user_top);
        let params = p.params();
        assert_eq!(params.kind, PriorityType::Basic);
        assert_eq!(
            (
                params.weight_age,
                params.weight_fairshare,
                params.weight_job_size,
                params.weight_partition
            ),
            (1, 2, 3, 4)
        );
        assert_eq!(params.weight_tres.gpu, 1000.0);
        assert_eq!(params.max_age_ms, 60_000);
        assert!(params.favor_small);
        assert_eq!(p.billing_weights(), TresWeights { cpu: 2.0, gpu: 0.0 });
    }

    #[test]
    fn rejects_bad_priority_settings() {
        for (text, needle) in [
            ("priority_weight_tres = \"mem=1\"", "priority_weight_tres"),
            ("tres_billing_weights = \"CPU\"", "tres_billing_weights"),
            ("priority_calc_period_secs = 0", "priority_calc_period_secs"),
            (
                "priority_decay_half_life_secs = 31536001",
                "priority_decay_half_life_secs",
            ),
            ("priority_max_age_secs = 31536001", "priority_max_age_secs"),
        ] {
            let message = invalid(&format!("[priority]\n{text}"));
            assert!(message.contains(needle), "{text}: {message}");
        }
        for text in [
            "priority_type = \"fifo\"",
            "priority_usage_reset_period = \"hourly\"",
            "priority_weight_age = -1",
        ] {
            assert!(
                matches!(
                    Config::parse(&format!("[priority]\n{text}")),
                    Err(ParseError::Toml(_))
                ),
                "{text}"
            );
        }
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
            gpu_device_isolation = false
            job_system_dbus = true
            job_systemd_varlink = true
            criu_dump_timeout_secs = 60
            criu_restore_timeout_secs = 90
            checkpoint_deadline_base_secs = 20
            checkpoint_deadline_secs_per_gib = 0
            job_min_uid = 2000
            job_denied_groups = ["docker"]
            job_allowed_users = ["alice", "bob"]
            job_hidden_socket_dirs = ["/run/libvirt", "/nix/var/nix/daemon-socket"]
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
        assert!(!config.gpu_device_isolation);
        assert!(config.job_system_dbus);
        assert!(config.job_systemd_varlink);
        assert_eq!(config.criu_dump_timeout_secs, Some(60));
        assert_eq!(config.criu_restore_timeout_secs, Some(90));
        assert_eq!(config.checkpoint_deadline_base_secs, 20);
        assert_eq!(config.checkpoint_deadline_secs_per_gib, 0);
        assert_eq!(config.job_min_uid, Some(2000));
        assert_eq!(config.job_denied_groups, ["docker"]);
        assert_eq!(config.job_allowed_users, ["alice", "bob"]);
        assert_eq!(
            config.job_hidden_socket_dirs,
            ["/run/libvirt", "/nix/var/nix/daemon-socket"]
        );
        // An empty list denies no group.
        let config = Config::parse("job_denied_groups = []").unwrap();
        assert!(config.job_denied_groups.is_empty());
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
        assert_eq!(config.job_min_uid, Some(1000));
        // Everything else is the default.
        config.job_min_uid = None;
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
        assert!(invalid("job_denied_groups = [\"\"]").contains("job_denied_groups"));
        assert!(invalid("job_allowed_users = [\"a:b\"]").contains("job_allowed_users"));
        assert!(invalid("job_allowed_users = [\" alice\"]").contains("job_allowed_users"));
        for bad in [
            "run/libvirt",
            "/run",
            "/",
            "/run/libvirt/",
            "/run/../etc",
            "/run/./libvirt",
            "/proc/1",
            "/run/systemd/x",
            "/run/dbus",
            "/tmp/x",
        ] {
            let text = format!("job_hidden_socket_dirs = [{bad:?}]");
            assert!(invalid(&text).contains("job_hidden_socket_dirs"), "{bad}");
        }
        assert!(Config::parse("job_hidden_socket_dirs = []").is_ok());
        assert!(matches!(
            Config::parse("job_min_uid = -1"),
            Err(ParseError::Toml(_))
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
