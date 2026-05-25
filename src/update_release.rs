//! `github:release` update execution.
//!
//! This module keeps the network/update orchestration separate from the
//! filesystem installer helpers. The flow mirrors Bash's public contract: TTL
//! fast path, release metadata fetch, host asset selection, asset download,
//! install, stamp, and manifest repair.

use std::ffi::OsString;
use std::path::Path;

use crate::config::Entry;
use crate::github;
use crate::github_release;
use crate::github_release_install;
use crate::http::Client;
use crate::manifest::{self, ManifestEntry};
use crate::platform::RuntimeEnv;
use crate::process::{self, Runner};
use crate::runtime::{Env, Roots};
use crate::stamp;
use crate::tool_version;
use crate::update::{Context, Item, Options};
use crate::Result;

pub(crate) fn install(
    entry: &Entry,
    context: &Context<'_, impl Runner>,
    options: Options,
) -> Result<Item> {
    let bin_path = context.roots.bin_dir.join(&entry.cmd);
    let request = ReleaseRequest {
        name: &entry.name,
        cmd: &entry.cmd,
        repo: &entry.name,
        public_bin: &bin_path,
    };
    let outcome = install_request(
        &request,
        context.roots,
        context.env,
        context.env_vars,
        context.runner,
        context.client,
        options,
    )?;
    if !outcome.failed {
        write_manifest(entry, context)?;
    }

    Ok(Item {
        name: entry.name.clone(),
        changed: outcome.changed,
        failed: outcome.failed,
        detail: outcome.detail,
    })
}

pub(crate) struct ReleaseRequest<'a> {
    pub(crate) name: &'a str,
    pub(crate) cmd: &'a str,
    pub(crate) repo: &'a str,
    pub(crate) public_bin: &'a Path,
}

pub(crate) struct ReleaseOutcome {
    pub(crate) changed: bool,
    pub(crate) failed: bool,
    pub(crate) detail: String,
}

pub(crate) fn install_request(
    request: &ReleaseRequest<'_>,
    roots: &Roots,
    runtime_env: &RuntimeEnv,
    env_vars: &std::collections::BTreeMap<String, String>,
    runner: &impl Runner,
    client: &dyn Client,
    options: Options,
) -> Result<ReleaseOutcome> {
    let stamp_path = stamp::remote_path(&roots.state_dir, request.name, "release");
    if process::executable_path(request.public_bin)
        && stamp::remote_fresh(&stamp_path, options.freshness())
    {
        // Bash relinks extras even on the TTL fast path. That idempotent repair
        // matters when a user prunes a completion/manpage symlink by hand while
        // keeping the binary; a fresh stamp should skip the network, not leave
        // related shell integration broken until the next forced reinstall.
        let install_dir = roots.install_dir.join(request.name);
        crate::extras::link(
            &roots.state_dir,
            &roots.install_dir,
            request.name,
            &install_dir,
        )?;
        return Ok(ReleaseOutcome {
            changed: false,
            failed: false,
            detail: "fresh".to_owned(),
        });
    }

    let current_version = process::executable_path(request.public_bin)
        .then(|| process::dep_version(runner, request.cmd))
        .flatten();

    let env = EnvVars {
        vars: env_vars,
        runtime: runtime_env,
    };
    let releases = match github::fetch_releases(request.repo, &env, runner, client) {
        Ok(releases) => releases,
        Err(_) => {
            return Ok(failed("release metadata fetch failed"));
        }
    };
    if let Some(latest) = github_release::latest_stable(&releases) {
        if !options.reinstall
            && current_version
                .as_deref()
                .is_some_and(|current| installed_matches_tag(current, &latest.tag))
        {
            // `--force` deliberately bypasses the TTL so users can ask GitHub
            // "is there anything newer?" immediately. It must not imply a
            // reinstall loop: release downloads are the most expensive shdeps
            // path, and post hooks should only run when the dependency really
            // changed. Bash compared the probed command version to the latest
            // release tag before asset selection, so keep that no-op path even
            // if the release has no asset we would install from scratch today.
            stamp::remote_touch(&stamp_path, options.now)?;
            link_existing_extras(roots, request.name)?;
            return Ok(ReleaseOutcome {
                changed: false,
                failed: false,
                detail: current_version.unwrap_or_else(|| latest.tag.clone()),
            });
        }
    }
    let Some(selection) = github_release::select(request.cmd, &releases, runtime_env, runner)
    else {
        return Ok(failed("no matching release asset"));
    };
    let token = github::token(&env, runner);
    let bytes = match client.get(&selection.url, token.as_deref()) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(failed("release asset download failed")),
    };
    match asset_kind(&selection.url) {
        AssetKind::Plain => {
            github_release_install::install_plain_to(request.public_bin, &bytes)?;
        }
        AssetKind::Gz => {
            github_release_install::install_gz_to(request.public_bin, &bytes)?;
        }
        AssetKind::Bz2 => {
            github_release_install::install_bz2_to(request.public_bin, &bytes)?;
        }
        AssetKind::Zst => {
            github_release_install::install_zst_to(request.public_bin, &bytes)?;
        }
        AssetKind::TarGz => {
            github_release_install::install_tar_gz_to(
                &roots.state_dir,
                &roots.install_dir,
                request.public_bin,
                request.name,
                request.cmd,
                &bytes,
            )?;
        }
        AssetKind::TarBz2 => {
            github_release_install::install_tar_bz2_to(
                &roots.state_dir,
                &roots.install_dir,
                request.public_bin,
                request.name,
                request.cmd,
                &bytes,
            )?;
        }
        AssetKind::TarZst => {
            github_release_install::install_tar_zst_to(
                &roots.state_dir,
                &roots.install_dir,
                request.public_bin,
                request.name,
                request.cmd,
                &bytes,
            )?;
        }
        AssetKind::TarXz => {
            github_release_install::install_tar_xz_to(
                &roots.state_dir,
                &roots.install_dir,
                request.public_bin,
                request.name,
                request.cmd,
                &bytes,
            )?;
        }
        AssetKind::Zip => {
            github_release_install::install_zip_to(
                &roots.state_dir,
                &roots.install_dir,
                request.public_bin,
                request.name,
                request.cmd,
                &bytes,
            )?;
        }
        AssetKind::Unsupported => {
            return Ok(failed("release asset type is not implemented yet"));
        }
    }
    stamp::remote_touch(&stamp_path, options.now)?;

    Ok(ReleaseOutcome {
        changed: true,
        failed: false,
        detail: selection.tag,
    })
}

fn link_existing_extras(roots: &Roots, name: &str) -> Result<()> {
    let install_dir = roots.install_dir.join(name);
    crate::extras::link(&roots.state_dir, &roots.install_dir, name, &install_dir)?;
    Ok(())
}

fn write_manifest(entry: &Entry, context: &Context<'_, impl Runner>) -> Result<()> {
    manifest::upsert(
        context.manifest_path,
        ManifestEntry::new(
            &entry.name,
            "github:release",
            &entry.cmd,
            context.roots.bin_dir.join(&entry.cmd).display().to_string(),
        ),
    )
}

fn failed(detail: &str) -> ReleaseOutcome {
    ReleaseOutcome {
        changed: false,
        failed: true,
        detail: detail.to_owned(),
    }
}

fn installed_matches_tag(installed: &str, tag: &str) -> bool {
    comparable_versions(tag)
        .into_iter()
        .any(|candidate| candidate == installed)
}

fn comparable_versions(tag: &str) -> Vec<String> {
    let tag = tag.trim();
    let mut versions = Vec::new();

    // Keep exact-ish comparisons first for projects that report their tag
    // verbatim. Then add the de-prefixed and dotted forms that cover common
    // GitHub tags such as `v1.2.3`, `rust-v0.133.0`, and `release-2026.5.15`.
    push_unique(&mut versions, tag);
    push_unique(&mut versions, tag.strip_prefix('v').unwrap_or(tag));

    if let Some(dotted) = tool_version::extract(&[tag], "release-tag") {
        push_unique(&mut versions, &dotted);
    }

    versions
}

fn push_unique(values: &mut Vec<String>, value: &str) {
    if value.is_empty() || values.iter().any(|existing| existing == value) {
        return;
    }
    values.push(value.to_owned());
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AssetKind {
    Plain,
    Gz,
    Bz2,
    Zst,
    TarGz,
    TarBz2,
    TarZst,
    TarXz,
    Zip,
    Unsupported,
}

fn asset_kind(url: &str) -> AssetKind {
    let lower = url.to_ascii_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        return AssetKind::TarGz;
    }
    if lower.ends_with(".tar.bz2") {
        return AssetKind::TarBz2;
    }
    if lower.ends_with(".tar.zst") || lower.ends_with(".tzst") {
        return AssetKind::TarZst;
    }
    if lower.ends_with(".tar.xz") {
        return AssetKind::TarXz;
    }
    // The Bash reference supports xz only as a tar archive. Treating a bare
    // `.xz` asset as a plain executable would publish compressed bytes into
    // `SHDEPS_BIN`, so keep this explicitly unsupported until single-file xz
    // installs are added to both implementations.
    if lower.ends_with(".xz") {
        return AssetKind::Unsupported;
    }
    if lower.ends_with(".gz") {
        return AssetKind::Gz;
    }
    if lower.ends_with(".bz2") {
        return AssetKind::Bz2;
    }
    if lower.ends_with(".zst") {
        return AssetKind::Zst;
    }
    if lower.ends_with(".zip") {
        return AssetKind::Zip;
    }
    AssetKind::Plain
}

struct EnvVars<'a> {
    vars: &'a std::collections::BTreeMap<String, String>,
    runtime: &'a RuntimeEnv,
}

impl Env for EnvVars<'_> {
    fn var_os(&self, name: &str) -> Option<OsString> {
        self.vars.get(name).map(OsString::from)
    }

    fn command_output(&self, command: &str, args: &[&str]) -> Option<String> {
        match (command, args) {
            ("uname", ["-s"]) => Some(self.runtime.platform().to_owned()),
            ("hostname", ["-s"] | []) => Some(self.runtime.host().to_owned()),
            _ => None,
        }
    }

    fn read_to_string(&self, _path: &std::path::Path) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{asset_kind, comparable_versions, installed_matches_tag, AssetKind};

    #[test]
    fn asset_kind_accepts_raw_binary_urls() {
        assert_eq!(
            asset_kind("https://example.com/tool-linux-x86_64"),
            AssetKind::Plain
        );
    }

    #[test]
    fn asset_kind_accepts_gzip_tar_archives() {
        assert_eq!(
            asset_kind("https://example.com/tool-linux-x86_64.tar.gz"),
            AssetKind::TarGz
        );
        assert_eq!(
            asset_kind("https://example.com/tool-linux-x86_64.tgz"),
            AssetKind::TarGz
        );
    }

    #[test]
    fn asset_kind_accepts_bzip2_tar_archives() {
        assert_eq!(
            asset_kind("https://example.com/tool-linux-x86_64.tar.bz2"),
            AssetKind::TarBz2
        );
    }

    #[test]
    fn asset_kind_accepts_zstd_tar_archives() {
        assert_eq!(
            asset_kind("https://example.com/tool-linux-x86_64.tar.zst"),
            AssetKind::TarZst
        );
        assert_eq!(
            asset_kind("https://example.com/tool-linux-x86_64.tzst"),
            AssetKind::TarZst
        );
    }

    #[test]
    fn asset_kind_accepts_xz_tar_archives() {
        assert_eq!(
            asset_kind("https://example.com/tool-linux-x86_64.tar.xz"),
            AssetKind::TarXz
        );
    }

    #[test]
    fn asset_kind_rejects_xz_compressed_singles_until_reference_supports_them() {
        assert_eq!(
            asset_kind("https://example.com/tool-linux-x86_64.xz"),
            AssetKind::Unsupported
        );
    }

    #[test]
    fn asset_kind_accepts_gzip_compressed_singles() {
        assert_eq!(
            asset_kind("https://example.com/tool-linux-x86_64.gz"),
            AssetKind::Gz
        );
    }

    #[test]
    fn asset_kind_accepts_bzip2_compressed_singles() {
        assert_eq!(
            asset_kind("https://example.com/tool-linux-x86_64.bz2"),
            AssetKind::Bz2
        );
    }

    #[test]
    fn asset_kind_accepts_zstd_compressed_singles() {
        assert_eq!(
            asset_kind("https://example.com/tool-linux-x86_64.zst"),
            AssetKind::Zst
        );
    }

    #[test]
    fn asset_kind_accepts_zip_archives() {
        assert_eq!(
            asset_kind("https://example.com/tool-linux-x86_64.zip"),
            AssetKind::Zip
        );
    }

    #[test]
    fn release_tag_comparison_accepts_common_github_prefixes() {
        assert!(installed_matches_tag("1.2.3", "v1.2.3"));
        assert!(installed_matches_tag("0.133.0", "rust-v0.133.0"));
        assert!(installed_matches_tag("2026.5.15", "release-2026.5.15"));
        assert!(installed_matches_tag(
            "nightly-20260525",
            "nightly-20260525"
        ));
        assert!(!installed_matches_tag("1.2.2", "v1.2.3"));

        assert_eq!(
            comparable_versions("rust-v0.133.0"),
            vec!["rust-v0.133.0", "0.133.0"]
        );
    }
}
