//! Wire types shared by the puzuz CLI, daemon and helper.
//!
//! - [`api`]: CLI ⇄ daemon, REST + JSON over HTTP on a Unix socket.
//! - [`helper`]: daemon ⇄ helper, newline-delimited JSON over one persistent
//!   Unix socket connection.
//!
//! This crate only defines types and pure encoding helpers; it performs no
//! I/O. Networking code (the HTTP client) will live behind the `client`
//! feature so that the root helper can depend on the types alone.

pub mod api;
pub mod helper;

/// Version of the puzuz binaries this crate was built with.
pub const BINARY_VERSION: &str = env!("CARGO_PKG_VERSION");
