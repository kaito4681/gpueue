//! Pure parsers for the output of `criu` and `cuda-checkpoint`.

use std::fmt;

/// A dotted version such as `4.2.1` or `595.91.07`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    /// Numeric components; missing trailing components are not padded.
    pub parts: Vec<u32>,
    /// The version exactly as printed (keeps leading zeros like `07`).
    pub raw: String,
}

impl Version {
    /// Parse a version token like `4.2`, `4.2.1` or `v4.2.1-rc1`.
    /// Non-numeric suffixes after the dotted digits are ignored.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().trim_start_matches(['v', 'V']);
        let end = s
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(s.len());
        let raw = s[..end].trim_end_matches('.');
        if raw.is_empty() {
            return None;
        }
        let parts = raw
            .split('.')
            .map(str::parse)
            .collect::<Result<Vec<u32>, _>>()
            .ok()?;
        Some(Self {
            parts,
            raw: raw.to_owned(),
        })
    }

    pub fn major(&self) -> u32 {
        self.parts.first().copied().unwrap_or(0)
    }

    /// Compare against `(major, minor, patch)`, treating missing parts as 0.
    pub fn at_least(&self, major: u32, minor: u32, patch: u32) -> bool {
        let p = |i: usize| self.parts.get(i).copied().unwrap_or(0);
        (p(0), p(1), p(2)) >= (major, minor, patch)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

/// Parsed `criu --version` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CriuVersion {
    pub version: Version,
    pub git_id: Option<String>,
}

impl CriuVersion {
    /// CRIU 4.0 introduced `cuda_plugin.so`.
    pub fn supports_cuda_plugin(&self) -> bool {
        self.version.at_least(4, 0, 0)
    }
}

/// Parse `criu --version`, e.g.
///
/// ```text
/// Version: 4.2.1
/// GitID: v4.2.1
/// ```
pub fn parse_criu_version(output: &str) -> Option<CriuVersion> {
    let mut version = None;
    let mut git_id = None;
    for line in output.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("Version:") {
            version = Version::parse(v);
        } else if let Some(g) = line.strip_prefix("GitID:") {
            let g = g.trim();
            if !g.is_empty() {
                git_id = Some(g.to_owned());
            }
        }
    }
    Some(CriuVersion {
        version: version?,
        git_id,
    })
}

/// What a `cuda-checkpoint` binary supports, from its `--help` text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CudaCheckpointFeatures {
    /// The version line follows the driver (`Version 595.91.07.`).
    pub version: Option<Version>,
    pub toggle: bool,
    pub get_state: bool,
    /// `--action lock|checkpoint|restore|unlock` (r570+).
    pub actions: bool,
    /// `--timeout` for lock (r570+).
    pub lock_timeout: bool,
    /// `--device-map` for restore (r575/580+).
    pub device_map: bool,
    pub get_restore_tid: bool,
    pub launch_job: bool,
}

impl CudaCheckpointFeatures {
    /// Whether the binary supports everything manual mode needs.
    pub fn supports_manual_mode(&self) -> bool {
        self.actions && self.lock_timeout && self.get_state
    }
}

/// Parse `cuda-checkpoint --help` (the help text includes the version line).
pub fn parse_cuda_checkpoint_help(output: &str) -> CudaCheckpointFeatures {
    let mut f = CudaCheckpointFeatures::default();
    for line in output.lines() {
        let line = line.trim();
        if f.version.is_none()
            && let Some(rest) = line.strip_prefix("Version ")
        {
            f.version = rest.split_whitespace().next().and_then(Version::parse);
        }
        // Only option lines (starting with `--`) count, not prose.
        if !line.starts_with("--") {
            continue;
        }
        f.toggle |= line.starts_with("--toggle");
        f.get_state |= line.starts_with("--get-state");
        f.get_restore_tid |= line.starts_with("--get-restore-tid");
        f.launch_job |= line.starts_with("--launch-job");
        if line.starts_with("--action") {
            f.actions |= ["lock", "checkpoint", "restore", "unlock"]
                .iter()
                .all(|a| line.contains(a));
        }
        f.lock_timeout |= line.contains("--timeout");
        f.device_map |= line.contains("--device-map");
    }
    f
}

/// CUDA checkpoint state of a process (`CUprocessState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CudaState {
    Running,
    Locked,
    Checkpointed,
    Failed,
    /// CUDA 13.4+.
    Checkpointing,
    /// CUDA 13.4+.
    Restoring,
}

/// Parse `cuda-checkpoint --get-state` output. Tolerates surrounding text by
/// taking the last recognised word.
pub fn parse_cuda_state(output: &str) -> Option<CudaState> {
    output
        .split(|c: char| !c.is_ascii_alphabetic())
        .filter_map(|w| match w.to_ascii_lowercase().as_str() {
            "running" => Some(CudaState::Running),
            "locked" => Some(CudaState::Locked),
            "checkpointed" => Some(CudaState::Checkpointed),
            "failed" => Some(CudaState::Failed),
            "checkpointing" => Some(CudaState::Checkpointing),
            "restoring" => Some(CudaState::Restoring),
            _ => None,
        })
        .next_back()
}

/// Classified `cuda-checkpoint` error (from its stderr/stdout).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CudaError {
    /// "initialization error": the process does not use CUDA.
    NotCudaProcess,
    /// "device not ready": lock timed out; the process is still running.
    LockTimedOut,
    /// "out of memory": restore found too little free GPU memory. The process
    /// can no longer be resumed.
    OutOfMemory,
    /// "the operation cannot be performed in the present state".
    InvalidState,
    /// "operation not supported" (e.g. UVM / managed memory).
    NotSupported,
    /// "invalid argument" (e.g. bad device map); state is unchanged.
    InvalidArgument,
    /// "OS call failed or operation not supported on this OS": wrong uid,
    /// non-dumpable, missing PID, or a broken process.
    OsCallFailed,
    Unknown,
}

/// Classify the error text printed by a failed `cuda-checkpoint` call.
pub fn classify_cuda_error(output: &str) -> CudaError {
    let s = output.to_ascii_lowercase();
    // Order matters: "OS call failed or operation not supported on this OS"
    // contains "operation not supported".
    if s.contains("os call failed") {
        CudaError::OsCallFailed
    } else if s.contains("initialization error") {
        CudaError::NotCudaProcess
    } else if s.contains("device not ready") {
        CudaError::LockTimedOut
    } else if s.contains("out of memory") {
        CudaError::OutOfMemory
    } else if s.contains("cannot be performed in the present state") {
        CudaError::InvalidState
    } else if s.contains("operation not supported") {
        CudaError::NotSupported
    } else if s.contains("invalid argument") || s.contains("invalid device remap") {
        CudaError::InvalidArgument
    } else {
        CudaError::Unknown
    }
}

/// Last `Error (...)` lines from a CRIU log, for a short failure summary.
pub fn summarize_criu_log(log: &str, max_lines: usize) -> Vec<String> {
    let errors: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("Error (") || l.contains("BUG at"))
        .collect();
    let start = errors.len().saturating_sub(max_lines);
    errors[start..]
        .iter()
        .map(|l| {
            // Strip the "(01.107020) " timestamp prefix.
            let l = l.trim();
            match l.strip_prefix('(').and_then(|r| r.split_once(") ")) {
                Some((_, rest)) => rest.to_owned(),
                None => l.to_owned(),
            }
        })
        .collect()
}

/// Paths that CRIU's dump log (`-v4`) names as auto-detected external
/// mounts (`--external mnt[]`): `mnt: autodetected external mount <path>
/// for ./<mount point>(<id>)`. A restore bind-mounts each `<path>` from
/// under `--root`, so all of them must exist on the restoring host.
pub fn criu_external_mounts(log: &str) -> Vec<String> {
    const MARK: &str = "autodetected external mount ";
    let mut out: Vec<String> = Vec::new();
    for l in log.lines() {
        let Some((_, rest)) = l.split_once(MARK) else {
            continue;
        };
        let Some((path, _)) = rest.rsplit_once(" for ./") else {
            continue;
        };
        if !out.iter().any(|p| p == path) {
            out.push(path.to_owned());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_mounts_from_a_dump_log() {
        let log = "\
(00.069149) mnt: autodetected external mount /run for ./run(9393)
(00.069297) mnt: autodetected external mount /run/nvidia-persistenced/socket for ./run/nvidia-persistenced/socket(9319)
(00.069300) mnt: autodetected external mount /data a for b for ./data a for b(9318)
(00.069301) mnt: autodetected external mount /run for ./x(9300)
(00.069302) mnt: something else
";
        assert_eq!(
            criu_external_mounts(log),
            ["/run", "/run/nvidia-persistenced/socket", "/data a for b"]
        );
    }

    const HELP_595: &str = "\
CUDA checkpoint and restore utility.
Version 595.91.07. Copyright (C) 2026 NVIDIA Corporation. All rights reserved.

Operations:
--get-state --pid <pid>
\tPrints the current checkpoint state of the process specified by <pid>

--action lock | checkpoint | restore | unlock --pid <pid> [--timeout <ms>] [--device-map <uuids>]
\tPerforms the specified action on <pid>.
\tFor the lock action a timeout can be provided, the lock operation will wait up to <ms> milliseconds for the operation to succeed.
\tFor the restore action a device map can be provided in the format oldUuid=newUuid,oldUuid=newUuid,... which will be used to remap old devices to new devices.

--toggle --pid <pid>
\tToggles the CUDA state in the specified process between the running and checkpointed states

--get-restore-tid --pid <pid>
\tRetrieves the CUDA restore thread ID of the process specified by <pid>

Options:
--pid|-p <pid>
\tThe pid upon which to perform the operation

--timeout|-t <timeout>
\tOptional timeout that can be specified for the lock action in milliseconds

--device-map|-d <deviceUuids>
\tOptional device map used during the restore action to manually remap old devices onto new ones.

--launch-job|-j <program> [<arg1> ... [<argn>]]
\tInitialize a memfd to become a job file

--help|-h
\tPrint this help message
";

    const HELP_550: &str = "\
CUDA checkpoint and restore utility.
Version 550.54.14. Copyright (C) 2024 NVIDIA Corporation. All rights reserved.

--toggle --pid <pid>
\tToggles the CUDA state in the specified process between the running and checkpointed states
--get-restore-tid --pid <pid>
--help|-h
";

    #[test]
    fn version_parse() {
        let v = Version::parse("4.2.1").unwrap();
        assert_eq!(v.parts, [4, 2, 1]);
        assert_eq!(v.to_string(), "4.2.1");
        assert_eq!(Version::parse("v4.0-rc2").unwrap().parts, [4, 0]);
        assert_eq!(Version::parse(" 595.91.07. ").unwrap().raw, "595.91.07");
        assert_eq!(Version::parse("595.91.07").unwrap().parts, [595, 91, 7]);
        assert!(Version::parse("").is_none());
        assert!(Version::parse("abc").is_none());
        assert!(Version::parse("1..2").is_none());
        let v = Version::parse("4.2").unwrap();
        assert!(v.at_least(4, 2, 0));
        assert!(!v.at_least(4, 2, 1));
        assert!(v.at_least(3, 99, 99));
        assert_eq!(v.major(), 4);
    }

    #[test]
    fn criu_version() {
        let v = parse_criu_version("Version: 4.2.1\nGitID: v4.2.1\n").unwrap();
        assert_eq!(v.version.parts, [4, 2, 1]);
        assert_eq!(v.git_id.as_deref(), Some("v4.2.1"));
        assert!(v.supports_cuda_plugin());

        let v = parse_criu_version("Version: 3.19\n").unwrap();
        assert_eq!(v.git_id, None);
        assert!(!v.supports_cuda_plugin());

        // Ubuntu package style.
        let v = parse_criu_version("Version: 4.2\n").unwrap();
        assert!(v.supports_cuda_plugin());

        assert!(parse_criu_version("criu: command not found").is_none());
        assert!(parse_criu_version("").is_none());
    }

    #[test]
    fn cuda_help_595() {
        let f = parse_cuda_checkpoint_help(HELP_595);
        assert_eq!(f.version.as_ref().unwrap().raw, "595.91.07");
        assert!(f.toggle && f.get_state && f.actions && f.lock_timeout);
        assert!(f.device_map && f.get_restore_tid && f.launch_job);
        assert!(f.supports_manual_mode());
    }

    #[test]
    fn cuda_help_550_is_toggle_only() {
        let f = parse_cuda_checkpoint_help(HELP_550);
        assert_eq!(f.version.unwrap().parts, [550, 54, 14]);
        assert!(f.toggle && f.get_restore_tid);
        assert!(!f.get_state && !f.actions && !f.lock_timeout && !f.device_map && !f.launch_job);
        assert!(!CudaCheckpointFeatures::default().supports_manual_mode());
    }

    #[test]
    fn cuda_help_garbage() {
        assert_eq!(
            parse_cuda_checkpoint_help("No such file"),
            CudaCheckpointFeatures::default()
        );
    }

    #[test]
    fn cuda_state() {
        assert_eq!(parse_cuda_state("running\n"), Some(CudaState::Running));
        assert_eq!(parse_cuda_state("Locked"), Some(CudaState::Locked));
        assert_eq!(
            parse_cuda_state("checkpointed"),
            Some(CudaState::Checkpointed)
        );
        assert_eq!(parse_cuda_state("failed"), Some(CudaState::Failed));
        assert_eq!(
            parse_cuda_state("state=restoring"),
            Some(CudaState::Restoring)
        );
        assert_eq!(
            parse_cuda_state("checkpointing"),
            Some(CudaState::Checkpointing)
        );
        assert_eq!(parse_cuda_state(""), None);
        assert_eq!(parse_cuda_state("Could not get state"), None);
    }

    #[test]
    fn cuda_errors() {
        let cases = [
            (
                "Could not restore on process ID 12: \"out of memory\"",
                CudaError::OutOfMemory,
            ),
            (
                "Error toggling CUDA in process ID 5: \"OS call failed or operation not supported on this OS\"",
                CudaError::OsCallFailed,
            ),
            (
                "Could not lock 5: \"device not ready\"",
                CudaError::LockTimedOut,
            ),
            ("\"initialization error\"", CudaError::NotCudaProcess),
            (
                "\"the operation cannot be performed in the present state\"",
                CudaError::InvalidState,
            ),
            ("\"operation not supported\"", CudaError::NotSupported),
            ("\"invalid argument\"", CudaError::InvalidArgument),
            ("Invalid device remap", CudaError::InvalidArgument),
            ("segfault", CudaError::Unknown),
        ];
        for (text, want) in cases {
            assert_eq!(classify_cuda_error(text), want, "{text}");
        }
    }

    #[test]
    fn criu_log_summary() {
        let log = "\
(00.000100) Version: 4.2.1
(01.107020) Error (criu/sk-inet.c:200): inet: Connected TCP socket, consider using --tcp-established option.
(01.107100) Warn  something
(01.200000) Error (criu/cr-dump.c:2100): Dumping FAILED.
";
        assert_eq!(
            summarize_criu_log(log, 5),
            [
                "Error (criu/sk-inet.c:200): inet: Connected TCP socket, consider using --tcp-established option.",
                "Error (criu/cr-dump.c:2100): Dumping FAILED.",
            ]
        );
        assert_eq!(
            summarize_criu_log(log, 1),
            ["Error (criu/cr-dump.c:2100): Dumping FAILED."]
        );
        assert_eq!(
            summarize_criu_log("mnt: BUG at criu/mount.c:48", 3),
            ["mnt: BUG at criu/mount.c:48"]
        );
        assert!(summarize_criu_log("all good", 3).is_empty());
    }
}
