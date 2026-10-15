//! The root helper of gpueue.
//!
//! It accepts one kind of client — the daemon, identified by its uid — on
//! a Unix socket and runs, signals, checkpoints and restores jobs on its
//! behalf, validating every request itself (see [`validate`]).
//!
//! - [`server`]: socket, Hello, request dispatch, event delivery.
//! - [`engine`]: the job table and every operation, including recovery
//!   after a restart and the checkpoint-everything shutdown.
//! - [`events`]: the persistent, acknowledged event queue.
//! - [`launcher`]: how job units are started (systemd over D-Bus, or a fake).
//! - [`shim`]: the job unit's main process (PID/mount namespace init).
//! - [`host`]: `/proc`, `nvidia-smi`, users and mounts.
//! - [`devices`]: GPU isolation (the device nodes a job unit may open).
//! - [`mountfd`]: walking to mount points and bind sources without
//!   following symlinks, through directories only root controls.
//! - [`mounts`]: mountinfo analysis (hidden mounts, CRIU's external binds).
//! - [`deadmounts`]: FUSE and network mounts that are gone or do not
//!   answer, and how dumps and restores get along without them.
//! - [`forkguard`]: the preload library that makes forked children of CUDA
//!   processes checkpointable.
//! - [`helperbin`]: the content-addressed copies of the helper that job
//!   units run, so that upgrades do not break restores.
//! - [`gcroots`]: Nix GC roots that keep the helper's store path while jobs
//!   need it.
//! - [`traced`]: lets the CUDA restore threads of a stopped job run while
//!   the rest of it stays stopped.

pub mod access;
pub mod config;
pub mod deadmounts;
pub mod devices;
pub mod engine;
pub mod events;
pub mod forkguard;
pub mod gcroots;
pub mod helperbin;
pub mod host;
pub mod images;
pub mod jobs;
pub mod launcher;
pub mod mountfd;
pub mod mounts;
pub mod notify;
pub mod server;
pub mod shim;
pub mod testing;
pub mod traced;
pub mod validate;

pub use config::HelperConfig;
pub use engine::{ExecutorFactory, Helper, ShutdownManifest};
