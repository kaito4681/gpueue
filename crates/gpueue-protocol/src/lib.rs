//! Wire types shared by the gpueue CLI, daemon and helper.
//!
//! - [`api`]: CLI ⇄ daemon, REST + JSON over HTTP on a Unix socket.
//! - [`helper`]: daemon ⇄ helper, newline-delimited JSON over one persistent
//!   Unix socket connection.
//!
//! - [`client`] (feature `client`): async HTTP client of the daemon API, used
//!   by the CLI.
//!
//! Without the `client` feature this crate only defines types and pure
//! encoding helpers and performs no I/O, so the root helper can depend on the
//! types alone.

pub mod api;
#[cfg(feature = "client")]
pub mod client;
pub mod helper;

/// Version of the gpueue binaries this crate was built with.
pub const BINARY_VERSION: &str = env!("CARGO_PKG_VERSION");
