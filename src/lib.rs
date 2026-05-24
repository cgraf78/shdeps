#![deny(missing_docs)]

//! Rust implementation core for `shdeps`.
//!
//! The Bash implementation remains the behavioral reference until the Rust
//! port passes the full parity suite. This crate is intentionally split into
//! small modules so config parsing, state mutation, process execution, hooks,
//! install methods, and CLI formatting can each grow behind a single owner.
//! Keeping those boundaries explicit avoids recreating the current Bash script
//! as one large Rust translation unit.
//!
//! Public Rust API stability begins after the Rust port becomes the default.
//! During the port, exported items are documented so internal users understand
//! the intended ownership boundaries, but downstream crates should not treat
//! this API as semver-stable yet.

pub mod api;
pub mod cleanup;
pub mod cli;
pub mod config;
pub mod dep_path;
pub mod errors;
pub mod link_state;
pub mod manifest;
pub mod package_cache;
pub mod platform;
pub mod process;
pub mod release_asset;
pub mod runtime;
pub mod stamp;
pub mod state;
pub mod status;
pub mod tool_version;
pub mod version;

pub use errors::{Error, Result};
