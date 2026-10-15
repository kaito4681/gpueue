//! CRIU and cuda-checkpoint integration for gpueue.
//!
//! This crate is mostly pure so it can be tested without root:
//!
//! - [`layout`]: the per-job checkpoint directory (`images/`, `meta.json`,
//!   `COMPLETE` marker written last) and its validation.
//! - [`command`]: builders for `criu dump`/`restore` and `cuda-checkpoint`
//!   invocations, with defaults matching the PoC.
//! - [`plan`]: checkpoint and restore as ordered [`plan::Step`]s, including
//!   the rollback plan for a failed dump.
//! - [`exec`]: the [`exec::Executor`] trait, plan runners with rollback, and
//!   [`exec::SystemExecutor`] which runs real processes with outer timeouts.
//! - [`probe`]: parsers for `criu --version`, `cuda-checkpoint --help`,
//!   `--get-state` and error output.
//! - [`space`]: the `(rss + vram) * 1.2` free-space precheck.
//! - [`procfs`]: stopped/frozen detection and PID-namespace PID mapping.

pub mod command;
pub mod exec;
pub mod layout;
pub mod plan;
pub mod probe;
pub mod procfs;
pub mod space;

pub use command::{CommandSpec, CudaMode, DeviceMap, DumpTarget, RestoreTarget};
pub use exec::{Executor, PlanFailure, RestoreError, StepError, SystemExecutor};
pub use layout::{CheckpointDir, CheckpointMeta, DirStatus};
pub use plan::{Action, CheckpointConfig, Step};

#[cfg(test)]
pub(crate) mod test_util {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A temporary directory removed on drop.
    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            let p = std::env::temp_dir().join(format!(
                "gpueue-checkpoint-test-{}-{}-{nanos}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
