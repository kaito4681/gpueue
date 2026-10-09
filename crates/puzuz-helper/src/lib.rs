//! The root helper of puzuz.
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

pub mod config;
pub mod engine;
pub mod events;
pub mod host;
pub mod jobs;
pub mod launcher;
pub mod notify;
pub mod server;
pub mod shim;
pub mod testing;
pub mod validate;

pub use config::HelperConfig;
pub use engine::{ExecutorFactory, Helper, ShutdownManifest};
