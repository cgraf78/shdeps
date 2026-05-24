//! `github:release` update execution.
//!
//! This module starts with the raw standalone binary path. Archives and
//! compressed singles intentionally stay outside this first slice, but the
//! flow already matches the full update shape: TTL fast path, release metadata
//! fetch, host asset selection, asset download, install, stamp, and manifest.

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
    if !plain_asset(&selection.url) {
        return Ok(failed(entry, "release asset type is not implemented yet"));
    }

    let token = github::token(&env, context.runner);
    let bytes = match context.client.get(&selection.url, token.as_deref()) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(failed(entry, "release asset download failed")),
    };
    github_release_install::install_plain(&context.roots.bin_dir, &entry.cmd, &bytes)?;
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

fn plain_asset(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    ![
        ".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst", ".tgz", ".tzst", ".zip", ".gz", ".bz2",
        ".zst",
    ]
    .iter()
    .any(|extension| lower.ends_with(extension))
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
