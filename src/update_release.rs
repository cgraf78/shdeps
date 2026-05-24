//! `github:release` update execution.
//!
//! This module keeps the network/update orchestration separate from the
//! filesystem installer helpers. The flow mirrors Bash's public contract: TTL
//! fast path, release metadata fetch, host asset selection, asset download,
//! install, stamp, and manifest repair.

use std::ffi::OsString;

use crate::config::Entry;
use crate::github;
use crate::github_release;
use crate::github_release_install;
use crate::manifest::{self, ManifestEntry};
use crate::platform::RuntimeEnv;
use crate::process::{self, Runner};
use crate::runtime::Env;
use crate::stamp;
use crate::update::{Context, Item, Options};
use crate::Result;

pub(crate) fn install(
    entry: &Entry,
    context: &Context<'_, impl Runner>,
    options: Options,
) -> Result<Item> {
    let bin_path = context.roots.bin_dir.join(&entry.cmd);
    let stamp_path = stamp::remote_path(&context.roots.state_dir, &entry.name, "release");
    if process::executable_path(&bin_path) && stamp::remote_fresh(&stamp_path, options.freshness())
    {
        write_manifest(entry, context)?;
        return Ok(Item {
            name: entry.name.clone(),
            changed: false,
            failed: false,
            detail: "fresh".to_owned(),
        });
    }

    let env = EnvVars {
        vars: context.env_vars,
        runtime: context.env,
    };
    let releases = match github::fetch_releases(&entry.name, &env, context.runner, context.client) {
        Ok(releases) => releases,
        Err(_) => {
            return Ok(failed(entry, "release metadata fetch failed"));
        }
    };
    let Some(selection) =
        github_release::select(&entry.cmd, &releases, context.env, context.runner)
    else {
        return Ok(failed(entry, "no matching release asset"));
    };
    let token = github::token(&env, context.runner);
    let bytes = match context.client.get(&selection.url, token.as_deref()) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(failed(entry, "release asset download failed")),
    };
    match asset_kind(&selection.url) {
        AssetKind::Plain => {
            github_release_install::install_plain(&context.roots.bin_dir, &entry.cmd, &bytes)?;
        }
        AssetKind::Gz => {
            github_release_install::install_gz(&context.roots.bin_dir, &entry.cmd, &bytes)?;
        }
        AssetKind::TarGz => {
            github_release_install::install_tar_gz(
                &context.roots.state_dir,
                &context.roots.install_dir,
                &context.roots.bin_dir,
                &entry.name,
                &entry.cmd,
                &bytes,
            )?;
        }
        AssetKind::Unsupported => {
            return Ok(failed(entry, "release asset type is not implemented yet"))
        }
    }
    stamp::remote_touch(&stamp_path, options.now)?;
    write_manifest(entry, context)?;

    Ok(Item {
        name: entry.name.clone(),
        changed: true,
        failed: false,
        detail: selection.tag,
    })
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

fn failed(entry: &Entry, detail: &str) -> Item {
    Item {
        name: entry.name.clone(),
        changed: false,
        failed: true,
        detail: detail.to_owned(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AssetKind {
    Plain,
    Gz,
    TarGz,
    Unsupported,
}

fn asset_kind(url: &str) -> AssetKind {
    let lower = url.to_ascii_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        return AssetKind::TarGz;
    }
    if lower.ends_with(".gz") {
        return AssetKind::Gz;
    }
    // Plain `.bz2`/`.zst` single-file compression and other archive formats
    // need format-specific handling before they are safe drop-in replacements
    // for Bash. Treat them as explicit unsupported matches instead of
    // accidentally writing compressed bytes into SHDEPS_BIN_DIR as a "plain"
    // executable.
    if [
        ".tar.xz", ".tar.bz2", ".tar.zst", ".tzst", ".zip", ".bz2", ".zst",
    ]
    .iter()
    .any(|extension| lower.ends_with(extension))
    {
        return AssetKind::Unsupported;
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
    use super::{asset_kind, AssetKind};

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
    fn asset_kind_rejects_known_archives_and_compressed_singles_until_supported() {
        for url in [
            "https://example.com/tool.zip",
            "https://example.com/tool.tar.xz",
            "https://example.com/tool.tar.bz2",
            "https://example.com/tool.tar.zst",
            "https://example.com/tool.bz2",
            "https://example.com/tool.zst",
        ] {
            assert_eq!(asset_kind(url), AssetKind::Unsupported, "{url}");
        }
    }

    #[test]
    fn asset_kind_accepts_gzip_compressed_singles() {
        assert_eq!(
            asset_kind("https://example.com/tool-linux-x86_64.gz"),
            AssetKind::Gz
        );
    }
}
