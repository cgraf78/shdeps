//! Public command-link inspection for configured dependencies.
//!
//! This module backs `shdeps dep-links`. The command is intentionally cheap:
//! it reads local config, local manifest state, and local dependency roots only.
//! It must not run hooks, package-manager probes, GitHub API calls, or version
//! commands because health checks such as `dot doctor` call it interactively.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::Result;
use crate::config;
use crate::dep_path::{self, ResolveError};
use crate::link_state::{self, Kind};
use crate::manifest;
use crate::method;
use crate::platform::RuntimeEnv;
use crate::process;
use crate::runtime::Roots;

/// One public command link owned by a configured dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyLink {
    /// Command name exposed under `SHDEPS_BIN_DIR`.
    pub command: String,
    /// Public command path, normally `<SHDEPS_BIN_DIR>/<command>`.
    pub public_path: PathBuf,
    /// Expected source/target path for the public command.
    pub target_path: PathBuf,
}

/// Writes dependency links in the stable machine format used by the CLI/API.
///
/// Keep formatting here so every caller emits identical rows. The fields are
/// path-like but intentionally unescaped because shdeps-managed command names
/// and roots are newline-free local filesystem values.
pub fn write_tsv<W>(links: &[DependencyLink], writer: &mut W) -> Result<()>
where
    W: Write,
{
    for link in links {
        writeln!(
            writer,
            "{}\t{}\t{}",
            link.command,
            link.public_path.display(),
            link.target_path.display()
        )?;
    }
    Ok(())
}

/// Resolves the public command links shdeps owns for `target`.
///
/// Repo-style installs resolve command links from the dependency `bin/`
/// directory. Binary-root install methods use tracked `.binlinks` state when
/// available so release archives can expose every executable under their
/// packaged `bin/` directory; otherwise they fall back to the configured
/// command, with the install manifest providing the target when available.
pub fn links(target: &str, roots: &Roots, env: &RuntimeEnv) -> Result<Vec<DependencyLink>> {
    if !config::valid_dep_name(target) {
        return Err(ResolveError::InvalidInput.into());
    }

    let Some(entry) = dep_path::find_entry(target, &roots.conf_dir, env)? else {
        return Err(ResolveError::NotFound.into());
    };

    let manifest = manifest::read(&manifest::path(&roots.state_dir))?;
    let concrete_method = concrete_method(&entry.method, &entry.name, &manifest);

    if concrete_method == method::GITHUB_REPO {
        return repo_links(target, roots, env);
    }

    if method::is_binary_install_root(&concrete_method) {
        let tracked = tracked_bin_links(&entry, roots)?;
        if !tracked.is_empty() {
            return Ok(tracked);
        }

        let Some(target_path) = binary_target(&entry, roots, &manifest) else {
            return Err(ResolveError::NotFound.into());
        };
        return Ok(vec![DependencyLink {
            command: entry.cmd.clone(),
            public_path: roots.bin_dir.join(&entry.cmd),
            target_path,
        }]);
    }

    Ok(Vec::new())
}

fn concrete_method(configured_method: &str, name: &str, manifest: &manifest::Manifest) -> String {
    if configured_method == method::GITHUB {
        manifest
            .get(name)
            .map(|entry| entry.method.clone())
            .unwrap_or_else(|| method::GITHUB_REPO.to_owned())
    } else {
        configured_method.to_owned()
    }
}

fn repo_links(target: &str, roots: &Roots, env: &RuntimeEnv) -> Result<Vec<DependencyLink>> {
    let root = dep_path::root(target, &roots.dep_path_roots(), env)?;
    let source_dir = root.join("bin");
    let Ok(entries) = fs::read_dir(&source_dir) else {
        return Ok(Vec::new());
    };

    let mut links = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if !process::executable_path(&path) {
            continue;
        }
        let Some(command) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        links.push(DependencyLink {
            command: command.to_owned(),
            public_path: roots.bin_dir.join(command),
            target_path: path,
        });
    }

    links.sort_by(|left, right| left.command.cmp(&right.command));
    Ok(links)
}

fn tracked_bin_links(entry: &config::Entry, roots: &Roots) -> Result<Vec<DependencyLink>> {
    let state_path = link_state::path(&roots.state_dir, &entry.name, Kind::Bin);
    let mut links = Vec::new();
    for public_path in link_state::read(&state_path)? {
        let Some(command) = public_path
            .file_name()
            .and_then(|name| name.to_str())
            .map(ToOwned::to_owned)
        else {
            continue;
        };
        let target_path = fs::read_link(&public_path).unwrap_or_else(|_| public_path.clone());
        links.push(DependencyLink {
            command,
            public_path,
            target_path,
        });
    }

    links.sort_by(|left, right| left.command.cmp(&right.command));
    Ok(links)
}

fn binary_target(
    entry: &config::Entry,
    roots: &Roots,
    manifest: &manifest::Manifest,
) -> Option<PathBuf> {
    manifest
        .get(&entry.name)
        .and_then(|row| non_empty_path(&row.install_path))
        .or_else(|| Some(roots.bin_dir.join(&entry.cmd)))
}

fn non_empty_path(path: &str) -> Option<PathBuf> {
    (!path.is_empty()).then(|| Path::new(path).to_path_buf())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use super::links;
    use crate::platform::RuntimeEnv;
    use crate::runtime::Roots;

    #[test]
    fn repo_links_include_every_direct_executable_sorted_by_command() {
        let fixture = Fixture::new("repo-links");
        fixture.write_conf("owner/tool  github:repo\n");
        fixture.write_executable("share/owner/tool/bin/b-tool");
        fixture.write_executable("share/owner/tool/bin/a-tool");
        fixture.write("share/owner/tool/bin/not-executable", "#!/bin/sh\n");

        let links = links("owner/tool", &fixture.roots(), &fixture.env()).unwrap();

        assert_eq!(links.len(), 2);
        assert_eq!(links[0].command, "a-tool");
        assert_eq!(links[0].public_path, fixture.dir.join("bin/a-tool"));
        assert_eq!(
            links[0].target_path,
            fixture
                .dir
                .join("share/owner/tool")
                .canonicalize()
                .unwrap()
                .join("bin/a-tool")
        );
        assert_eq!(links[1].command, "b-tool");
    }

    #[test]
    fn bare_github_manifest_release_reports_single_binary_target() {
        let fixture = Fixture::new("release-link");
        let target = fixture.dir.join("bin/tool-real");
        fixture.write_conf("owner/tool  github  tool\n");
        fixture.write(
            "state/manifest",
            &format!("owner/tool|github:release|tool|{}\n", target.display()),
        );

        let links = links("owner/tool", &fixture.roots(), &fixture.env()).unwrap();

        assert_eq!(links.len(), 1);
        assert_eq!(links[0].command, "tool");
        assert_eq!(links[0].public_path, fixture.dir.join("bin/tool"));
        assert_eq!(links[0].target_path, target);
    }

    #[test]
    fn android_release_fallback_uses_runtime_command_override() {
        let fixture = Fixture::new("android-release-link");
        fixture.write_conf("owner/tool  github:release  android:tool-android,apt:tool\n");
        fixture.write(
            "state/manifest",
            "owner/tool|github:release|tool-android|\n",
        );
        let env = RuntimeEnv::new("linux", "phone").with_android(true);

        let links = links("owner/tool", &fixture.roots(), &env).unwrap();

        assert_eq!(links.len(), 1);
        assert_eq!(links[0].command, "tool-android");
        assert_eq!(links[0].public_path, fixture.dir.join("bin/tool-android"));
        assert_eq!(links[0].target_path, fixture.dir.join("bin/tool-android"));
    }

    #[test]
    #[cfg(unix)]
    fn release_links_use_tracked_archive_binlinks_when_present() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new("release-binlinks");
        let root = fixture.dir.join("share/owner/tool");
        let tool = root.join("bin/tool");
        let helper = root.join("bin/tool-helper");
        let public_tool = fixture.dir.join("bin/tool");
        let public_helper = fixture.dir.join("bin/tool-helper");
        fixture.write_conf("owner/tool  github:release  tool\n");
        fixture.write(
            "state/manifest",
            &format!("owner/tool|github:release|tool|{}\n", public_tool.display()),
        );
        fixture.write_executable("share/owner/tool/bin/tool");
        fixture.write_executable("share/owner/tool/bin/tool-helper");
        fs::create_dir_all(fixture.dir.join("bin")).unwrap();
        symlink(&tool, &public_tool).unwrap();
        symlink(&helper, &public_helper).unwrap();
        fixture.write(
            "state/owner/tool.binlinks",
            &format!("{}\n{}\n", public_helper.display(), public_tool.display()),
        );

        let links = links("owner/tool", &fixture.roots(), &fixture.env()).unwrap();

        assert_eq!(links.len(), 2);
        assert_eq!(links[0].command, "tool");
        assert_eq!(links[0].public_path, public_tool);
        assert_eq!(links[0].target_path, tool);
        assert_eq!(links[1].command, "tool-helper");
        assert_eq!(links[1].public_path, public_helper);
        assert_eq!(links[1].target_path, helper);
    }

    #[test]
    fn filtered_dependency_is_not_found() {
        let fixture = Fixture::new("filtered");
        fixture.write_conf("owner/tool  github:repo  -  -  os:macos\n");
        fixture.write_executable("share/owner/tool/bin/tool");

        let error = links("owner/tool", &fixture.roots(), &fixture.env()).unwrap_err();

        assert!(matches!(
            error,
            crate::Error::Resolve(crate::dep_path::ResolveError::NotFound)
        ));
    }

    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir = crate::test_support::temp_dir(&format!("shdeps-dep-links-{name}"));
            Self { dir }
        }

        fn roots(&self) -> Roots {
            Roots {
                conf_dir: self.dir.join("conf"),
                hooks_dir: self.dir.join("conf/hooks.d"),
                state_dir: self.dir.join("state"),
                git_dev_dir: self.dir.join("git"),
                install_dir: self.dir.join("share"),
                bin_dir: self.dir.join("bin"),
                home: self.dir.join("home"),
            }
        }

        fn env(&self) -> RuntimeEnv {
            RuntimeEnv::new("linux", "test-host")
        }

        fn write_conf(&self, content: &str) {
            self.write("conf/deps.conf", content);
        }

        fn write(&self, rel: impl AsRef<Path>, content: &str) {
            let path = self.dir.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }

        fn write_executable(&self, rel: impl AsRef<Path>) {
            let path = self.dir.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "#!/bin/sh\n").unwrap();
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions).unwrap();
        }
    }
}
