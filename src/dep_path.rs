//! Dependency root and asset path resolution.
//!
//! These helpers back `dep-root`, `dep-path`, and `dep-file`. They are cheap
//! commands: they resolve local config and filesystem state only, without
//! package-manager detection, hooks, GitHub API calls, or network probes.

use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{self, Entry};
use crate::platform::{self, RuntimeEnv};
use crate::Result;

/// Filesystem/config roots used to resolve dependency paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Roots {
    /// Config directory containing `*.conf` files.
    pub conf_dir: PathBuf,
    /// Local development clone directory, normally `~/git`.
    pub git_dev_dir: PathBuf,
    /// Managed install directory, normally `~/.local/share`.
    pub install_dir: PathBuf,
}

/// Dependency path resolution failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveError {
    /// The requested dependency name or relative path is invalid.
    InvalidInput,
    /// The dependency is unknown, inactive, has no managed root, or is missing.
    NotFound,
}

impl ResolveError {
    /// Returns the Bash-compatible exit code for this failure.
    #[must_use]
    pub const fn exit_code(self) -> i32 {
        match self {
            Self::InvalidInput => 2,
            Self::NotFound => 1,
        }
    }
}

/// Resolves a configured dependency root directory.
pub fn root(target: &str, roots: &Roots, env: &RuntimeEnv) -> Result<PathBuf> {
    if !config::valid_dep_name(target) {
        return Err(ResolveError::InvalidInput.into());
    }

    let Some(entry) = find_entry(target, &roots.conf_dir, env)? else {
        return Err(ResolveError::NotFound.into());
    };

    root_for_entry(&entry, roots).ok_or_else(|| ResolveError::NotFound.into())
}

/// Resolves a path below a configured dependency root. The final path may not exist.
pub fn path(target: &str, rel: &str, roots: &Roots, env: &RuntimeEnv) -> Result<PathBuf> {
    if !valid_relative(rel) {
        return Err(ResolveError::InvalidInput.into());
    }

    let root = root(target, roots, env)?;
    Ok(root.join(rel))
}

/// Resolves a readable regular file below a configured dependency root.
pub fn file(target: &str, rel: &str, roots: &Roots, env: &RuntimeEnv) -> Result<PathBuf> {
    let path = path(target, rel, roots, env)?;
    let metadata = fs::metadata(&path).map_err(|_| ResolveError::NotFound)?;
    if !metadata.is_file() {
        return Err(ResolveError::NotFound.into());
    }

    // Rust cannot check readability portably without opening the file, and
    // `metadata` permissions are insufficient once ACLs or platform-specific
    // access rules are involved. Opening mirrors the user's actual next step:
    // `dep-file` should only print a path that can be read now.
    fs::File::open(&path).map_err(|_| ResolveError::NotFound)?;
    Ok(path)
}

/// Loads the first active config entry for `target`.
///
/// This deliberately parses only the static config data needed for one
/// dependency, rather than building package-manager state or probing installed
/// tools. It still preserves the Bash loader's global entry sort before
/// matching. That sort matters for rare duplicate-name configs: downstream
/// scripts may rely on the exact entry Bash would visit first, and `dep-file`
/// is often used during shell/editor startup where compatibility bugs are more
/// painful than the small cost of sorting a few config lines.
pub fn find_entry(target: &str, conf_dir: &Path, env: &RuntimeEnv) -> Result<Option<Entry>> {
    let mut files = conf_files(conf_dir)?;
    files.sort();

    let mut entries = Vec::new();
    for file in files {
        let content = fs::read_to_string(file)?;
        for line in content.lines() {
            let Some(raw) = config::parse_config_line(line) else {
                continue;
            };
            entries.push(raw);
        }
    }
    config::sort_entries(&mut entries);

    for raw in entries {
        let entry = config::parse_entry(&raw, None);
        if entry.name != target {
            continue;
        }
        if platform::filter_match(&entry.filter, env) == platform::FilterMatch::Match {
            return Ok(Some(entry));
        }
        return Ok(None);
    }

    Ok(None)
}

fn root_for_entry(entry: &Entry, roots: &Roots) -> Option<PathBuf> {
    match entry.method.as_str() {
        "github:repo" => {
            // Local development clones intentionally win over managed clones.
            // Client projects use this to source assets from the live checkout
            // while iterating, and the Bash implementation has treated
            // `~/git/<short-name>` as the highest-priority root for years.
            let dev_root = roots.git_dev_dir.join(config::short_name(&entry.name));
            existing_root(&dev_root).or_else(|| existing_root(&roots.install_dir.join(&entry.name)))
        }
        "github:release" | "cargo" | "go" | "uv" | "npm" => {
            existing_root(&roots.install_dir.join(&entry.name))
        }
        _ => None,
    }
}

fn existing_root(path: &Path) -> Option<PathBuf> {
    if path.is_dir() {
        fs::canonicalize(path).ok()
    } else {
        None
    }
}

fn conf_files(conf_dir: &Path) -> Result<Vec<PathBuf>> {
    let Ok(entries) = fs::read_dir(conf_dir) else {
        return Ok(Vec::new());
    };

    let mut files = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "conf")
        {
            files.push(path);
        }
    }
    Ok(files)
}

fn valid_relative(rel: &str) -> bool {
    if rel.is_empty() || Path::new(rel).is_absolute() {
        return false;
    }

    // This validation is intentionally lexical. It prevents callers from
    // spelling absolute paths or explicit parent traversal, but it does not
    // promise a symlink sandbox inside the dependency root. The spec keeps that
    // boundary because dependency assets are trusted local tool data, and a
    // full canonicalization sandbox would break legitimate symlinked assets.
    !rel.split('/').any(|part| part == "..")
}

impl From<ResolveError> for crate::Error {
    fn from(error: ResolveError) -> Self {
        crate::Error::Resolve(error)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::{file, find_entry, path, root, ResolveError, Roots};
    use crate::platform::RuntimeEnv;

    #[test]
    fn github_repo_prefers_dev_clone_over_installed_clone() {
        let fixture = Fixture::new("github-dev");
        fixture.write_conf("cgraf78/sley  github:repo\n");
        fixture.mkdir("share/cgraf78/sley/share/sley");
        fixture.mkdir("git/sley/share/sley");

        let root = root("cgraf78/sley", &fixture.roots(), &fixture.env()).unwrap();

        assert_eq!(root, fixture.dir.join("git/sley").canonicalize().unwrap());
    }

    #[test]
    fn github_repo_falls_back_to_installed_clone() {
        let fixture = Fixture::new("github-installed");
        fixture.write_conf("cgraf78/sley  github:repo\n");
        fixture.mkdir("share/cgraf78/sley/share/sley");

        let root = root("cgraf78/sley", &fixture.roots(), &fixture.env()).unwrap();

        assert_eq!(
            root,
            fixture
                .dir
                .join("share/cgraf78/sley")
                .canonicalize()
                .unwrap()
        );
    }

    #[test]
    fn non_repo_methods_use_install_root() {
        let fixture = Fixture::new("install-root");
        fixture.write_conf("mytool  cargo\n");
        fixture.mkdir("share/mytool/share/mytool");

        let root = root("mytool", &fixture.roots(), &fixture.env()).unwrap();

        assert_eq!(
            root,
            fixture.dir.join("share/mytool").canonicalize().unwrap()
        );
    }

    #[test]
    fn package_deps_have_no_managed_root() {
        let fixture = Fixture::new("pkg-root");
        fixture.write_conf("jq  pkg\n");

        let error = root("jq", &fixture.roots(), &fixture.env()).unwrap_err();

        assert!(matches!(
            error,
            crate::Error::Resolve(ResolveError::NotFound)
        ));
    }

    #[test]
    fn filters_can_make_a_configured_dep_inactive() {
        let fixture = Fixture::new("filtered");
        fixture.write_conf("tool  cargo  -  -  os:macos\n");
        fixture.mkdir("share/tool");

        assert!(
            find_entry("tool", &fixture.roots().conf_dir, &fixture.env())
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            root("tool", &fixture.roots(), &fixture.env()).unwrap_err(),
            crate::Error::Resolve(ResolveError::NotFound)
        ));
    }

    #[test]
    fn duplicate_entries_follow_bash_sorted_order_before_filtering() {
        let fixture = Fixture::new("duplicate-sort");
        fixture.write_conf("tool  cargo  -  -  os:macos\ntool  cargo\n");
        fixture.mkdir("share/tool");

        let root = root("tool", &fixture.roots(), &fixture.env()).unwrap();

        assert_eq!(root, fixture.dir.join("share/tool").canonicalize().unwrap());
    }

    #[test]
    fn dep_path_appends_relative_path_without_requiring_existence() {
        let fixture = Fixture::new("dep-path");
        fixture.write_conf("cgraf78/sley  github:repo\n");
        fixture.mkdir("git/sley");

        let path = path(
            "cgraf78/sley",
            "share/sley/shell.sh",
            &fixture.roots(),
            &fixture.env(),
        )
        .unwrap();

        assert_eq!(
            path,
            fixture
                .dir
                .join("git/sley")
                .canonicalize()
                .unwrap()
                .join("share/sley/shell.sh")
        );
    }

    #[test]
    fn dep_file_requires_readable_regular_file() {
        let fixture = Fixture::new("dep-file");
        fixture.write_conf("cgraf78/sley  github:repo\n");
        fixture.mkdir("git/sley/share/sley/dir-asset");
        fixture.write("git/sley/share/sley/shell.sh", "SLEY_SHELL_LOADED=dev\n");

        assert!(file(
            "cgraf78/sley",
            "share/sley/shell.sh",
            &fixture.roots(),
            &fixture.env()
        )
        .is_ok());
        assert!(matches!(
            file(
                "cgraf78/sley",
                "share/sley/missing.sh",
                &fixture.roots(),
                &fixture.env()
            )
            .unwrap_err(),
            crate::Error::Resolve(ResolveError::NotFound)
        ));
        assert!(matches!(
            file(
                "cgraf78/sley",
                "share/sley/dir-asset",
                &fixture.roots(),
                &fixture.env()
            )
            .unwrap_err(),
            crate::Error::Resolve(ResolveError::NotFound)
        ));
    }

    #[test]
    fn rejects_invalid_dependency_names_and_relative_paths() {
        let fixture = Fixture::new("invalid");
        fixture.write_conf("cgraf78/sley  github:repo\n");
        fixture.mkdir("git/sley");

        assert!(matches!(
            root("../sley", &fixture.roots(), &fixture.env()).unwrap_err(),
            crate::Error::Resolve(ResolveError::InvalidInput)
        ));
        assert!(matches!(
            path(
                "cgraf78/sley",
                "../shell.sh",
                &fixture.roots(),
                &fixture.env()
            )
            .unwrap_err(),
            crate::Error::Resolve(ResolveError::InvalidInput)
        ));
        assert!(matches!(
            path(
                "cgraf78/sley",
                "/tmp/shell.sh",
                &fixture.roots(),
                &fixture.env()
            )
            .unwrap_err(),
            crate::Error::Resolve(ResolveError::InvalidInput)
        ));
    }

    #[test]
    fn missing_or_non_conf_dirs_are_empty() {
        let fixture = Fixture::new("missing-conf");
        fixture.write("conf/ignored.txt", "tool  cargo\n");
        fixture.mkdir("share/tool");

        assert!(
            find_entry("tool", &fixture.roots().conf_dir, &fixture.env())
                .unwrap()
                .is_none()
        );
        assert!(
            find_entry("tool", &fixture.dir.join("no-such-conf"), &fixture.env())
                .unwrap()
                .is_none()
        );
    }

    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "shdeps-dep-path-{name}-{}-{}",
                std::process::id(),
                std::thread::current().name().unwrap_or("test")
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        fn roots(&self) -> Roots {
            Roots {
                conf_dir: self.dir.join("conf"),
                git_dev_dir: self.dir.join("git"),
                install_dir: self.dir.join("share"),
            }
        }

        fn env(&self) -> RuntimeEnv {
            RuntimeEnv::new("linux", "test-host")
        }

        fn write_conf(&self, content: &str) {
            self.write("conf/deps.conf", content);
        }

        fn mkdir(&self, rel: impl AsRef<Path>) {
            fs::create_dir_all(self.dir.join(rel)).unwrap();
        }

        fn write(&self, rel: impl AsRef<Path>, content: &str) {
            let path = self.dir.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
    }
}
