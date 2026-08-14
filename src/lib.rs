#![deny(missing_docs)]

//! Rust implementation core for `shdeps`.
//!
//! This crate owns the implementation behind the CLI and sourceable Bash
//! wrapper. It is intentionally split into small modules so config parsing,
//! state mutation, process execution, hooks, install methods, and CLI
//! formatting each grow behind a single owner.
//!
//! Exported items are documented so internal users understand the intended
//! ownership boundaries, but downstream crates should not treat this API as
//! semver-stable yet.

pub mod api;
pub mod archive;
pub mod bin_link;
#[cfg(all(test, unix))]
mod checkout_lock;
pub mod checksum;
pub mod cleanup;
pub mod cli;
pub mod config;
pub mod dep_links;
pub mod dep_path;
pub mod errors;
pub mod external;
pub mod extras;
pub mod github;
pub mod github_gate;
pub mod github_method;
pub mod github_release;
pub mod github_release_install;
pub mod hook_toolkit;
pub mod hooks;
pub mod http;
pub mod install_metadata;
pub mod jobs;
pub mod link_state;
pub mod manifest;
mod method;
pub mod package_cache;
pub mod pkg;
pub mod platform;
pub mod process;
pub mod prune;
pub mod release_activate;
pub mod release_artifact;
pub mod release_asset;
pub mod release_stage;
pub mod repo;
pub mod runtime;
pub mod self_update;
pub mod stamp;
pub mod state;
pub mod status;
#[cfg(test)]
mod test_support;
pub mod tool_version;
pub mod update;
mod update_external;
mod update_pkg;
mod update_release;
mod update_repo;
mod update_transition;
pub mod version;

pub use errors::{Error, Result};
