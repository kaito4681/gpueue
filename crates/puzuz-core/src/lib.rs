//! Core domain model and scheduling logic for puzuz.
//!
//! This crate is pure logic: it performs no I/O except in the `storage`
//! module, and has no knowledge of HTTP, systemd or CRIU.

pub mod error;
pub mod model;
pub mod storage;

pub use error::Error;
