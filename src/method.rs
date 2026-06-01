//! Shared dependency method identifiers and classification helpers.
//!
//! Method strings are persisted in manifests and config files, so the string
//! values themselves are part of the on-disk format. Keep every caller that
//! needs method knowledge routed through this module instead of retyping the
//! same match arms across update, cleanup, status, and CLI layers.

use crate::config::Entry;

pub(crate) const PKG: &str = "pkg";
pub(crate) const GITHUB: &str = "github";
pub(crate) const GITHUB_REPO: &str = "github:repo";
pub(crate) const GITHUB_RELEASE: &str = "github:release";
pub(crate) const CARGO: &str = "cargo";
pub(crate) const GO: &str = "go";
pub(crate) const UV: &str = "uv";
pub(crate) const NPM: &str = "npm";
pub(crate) const CUSTOM: &str = "custom";

pub(crate) const EXTERNAL: [&str; 4] = [CARGO, GO, UV, NPM];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ToolRequirement {
    pub(crate) command: &'static str,
    pub(crate) reason: &'static str,
}

pub(crate) fn canonicalizes_github_name(method: &str) -> bool {
    matches!(method, GITHUB | GITHUB_REPO | GITHUB_RELEASE)
}

pub(crate) fn is_external(method: &str) -> bool {
    EXTERNAL.contains(&method)
}

pub(crate) fn is_concrete_github(method: &str) -> bool {
    matches!(method, GITHUB_REPO | GITHUB_RELEASE)
}

pub(crate) fn is_github_config(method: &str) -> bool {
    method == GITHUB || is_concrete_github(method)
}

pub(crate) fn is_binary_install_root(method: &str) -> bool {
    method == GITHUB_RELEASE || is_external(method)
}

pub(crate) fn is_symlink_install_root(method: &str) -> bool {
    method == GITHUB_REPO || is_external(method)
}

pub(crate) fn requires_external_plan(method: &str) -> bool {
    is_external(method)
}

pub(crate) fn prerequisite(method: &str, include_bare_github: bool) -> Option<ToolRequirement> {
    match method {
        GITHUB if include_bare_github => Some(ToolRequirement {
            command: "curl",
            reason: "GitHub release metadata and downloads",
        }),
        GITHUB => None,
        GITHUB_RELEASE => Some(ToolRequirement {
            command: "curl",
            reason: "GitHub release metadata and downloads",
        }),
        GITHUB_REPO => Some(ToolRequirement {
            command: "git",
            reason: "GitHub repo installs",
        }),
        _ => external_tool(method).map(|command| ToolRequirement {
            command,
            reason: external_reason(method),
        }),
    }
}

pub(crate) fn external_tool(method: &str) -> Option<&'static str> {
    match method {
        CARGO => Some("cargo"),
        GO => Some("go"),
        UV => Some("uv"),
        NPM => Some("npm"),
        _ => None,
    }
}

fn external_reason(method: &str) -> &'static str {
    match method {
        CARGO => "cargo installs",
        GO => "go installs",
        UV => "uv installs",
        NPM => "npm installs",
        _ => "external installs",
    }
}

pub(crate) fn active_non_package_builtin(entry: &Entry) -> bool {
    entry.method != PKG && entry.method != CUSTOM
}
