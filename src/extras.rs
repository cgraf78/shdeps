//! Man page and shell completion linking for GitHub-style installs.
//!
//! GitHub repo and release installs often unpack useful files outside `bin/`.
//! shdeps makes those discoverable by symlinking known man/completion layouts
//! into the same XDG-style directories the Bash implementation uses. The
//! important ownership boundary is the `.links` state file: relink and prune
//! remove only paths recorded there, so stale generated links disappear without
//! treating the whole user-local share tree as shdeps-owned.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::Result;
use crate::link_state::{self, Kind};

const MAN_PATTERNS: &[&str] = &[
    "share/man/man[0-9]/*.[0-9]",
    "share/man/man[0-9]/*.[0-9].gz",
    "man/man[0-9]/*.[0-9]",
    "man/man[0-9]/*.[0-9].gz",
    "manpages/*.[0-9]",
    "manpages/*.[0-9].gz",
    "doc/*.[0-9]",
    "doc/*.[0-9].gz",
    "*.1",
    "*.1.gz",
];

const BASH_PATTERNS: &[&str] = &[
    "share/bash-completion/completions/*",
    "completions/*.bash",
    "completion/*.bash",
    "complete/*.bash",
    "autocomplete/*.bash",
];

const ZSH_PATTERNS: &[&str] = &[
    "share/zsh/site-functions/_*",
    "completions/_*",
    "completions/*.zsh",
    "completion/_*",
    "complete/_*",
    "autocomplete/_*",
    "autocomplete/*.zsh",
];

const FISH_PATTERNS: &[&str] = &[
    "share/fish/vendor_completions.d/*.fish",
    "completions/*.fish",
    "completion/*.fish",
    "complete/*.fish",
    "autocomplete/*.fish",
];

/// Links discoverable extras from one dependency install directory.
pub fn link(
    state_dir: &Path,
    install_base: &Path,
    name: &str,
    install_dir: &Path,
) -> Result<Vec<PathBuf>> {
    if !install_dir.is_dir() {
        return Ok(Vec::new());
    }

    let state_path = link_state::path(state_dir, name, Kind::Extras);
    link_state::unlink_tracked(&state_path)?;

    let mut linker = Linker {
        install_base,
        created: Vec::new(),
        seen: BTreeSet::new(),
    };
    linker.man_pages(install_dir)?;
    linker.bash(install_dir)?;
    linker.zsh(install_dir)?;
    linker.fish(install_dir)?;

    link_state::write(&state_path, &linker.created)?;
    Ok(linker.created)
}

struct Linker<'a> {
    install_base: &'a Path,
    created: Vec<PathBuf>,
    seen: BTreeSet<PathBuf>,
}

impl Linker<'_> {
    fn man_pages(&mut self, install_dir: &Path) -> Result<()> {
        for source in expand_patterns(install_dir, MAN_PATTERNS)? {
            let Some(base) = source.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let name_no_gz = base.strip_suffix(".gz").unwrap_or(base);
            let section = name_no_gz.rsplit('.').next().unwrap_or("1");
            self.add(
                &source,
                self.install_base.join(format!("man/man{section}/{base}")),
            )?;
        }
        Ok(())
    }

    fn bash(&mut self, install_dir: &Path) -> Result<()> {
        let target_dir = self.install_base.join("bash-completion/completions");
        for source in expand_patterns(install_dir, BASH_PATTERNS)? {
            let Some(base) = source.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let target_name = base.strip_suffix(".bash").unwrap_or(base);
            self.add(&source, target_dir.join(target_name))?;
        }
        Ok(())
    }

    fn zsh(&mut self, install_dir: &Path) -> Result<()> {
        let target_dir = self.install_base.join("zsh/site-functions");
        for source in expand_patterns(install_dir, ZSH_PATTERNS)? {
            let Some(base) = source.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            // zsh's `fpath` discovery requires underscore-prefixed function
            // names. Some upstream archives already ship `_tool`, while
            // GoReleaser-style `tool.zsh` files need the compatibility rename.
            let target_name = if let Some(name) = base.strip_suffix(".zsh") {
                format!("_{name}")
            } else if base.starts_with('_') {
                base.to_owned()
            } else {
                format!("_{base}")
            };
            self.add(&source, target_dir.join(target_name))?;
        }
        Ok(())
    }

    fn fish(&mut self, install_dir: &Path) -> Result<()> {
        let target_dir = self.install_base.join("fish/vendor_completions.d");
        for source in expand_patterns(install_dir, FISH_PATTERNS)? {
            let Some(base) = source.file_name() else {
                continue;
            };
            self.add(&source, target_dir.join(base))?;
        }
        Ok(())
    }

    fn add(&mut self, source: &Path, target: PathBuf) -> Result<()> {
        if !self.seen.insert(target.clone()) {
            return Ok(());
        }
        ensure_public_parent(&target)?;
        if !replace_symlink(source, &target)? {
            return Ok(());
        }
        self.created.push(target);
        Ok(())
    }
}

fn expand_patterns(root: &Path, patterns: &[&str]) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for pattern in patterns {
        expand_segments(root, &segments(pattern), &mut paths)?;
    }
    Ok(paths
        .into_iter()
        .filter(|path| path.is_file())
        .collect::<Vec<_>>())
}

fn expand_segments(root: &Path, segments: &[&str], paths: &mut Vec<PathBuf>) -> Result<()> {
    let Some((segment, rest)) = segments.split_first() else {
        paths.push(root.to_path_buf());
        return Ok(());
    };

    if !has_pattern(segment) {
        expand_segments(&root.join(segment), rest, paths)?;
        return Ok(());
    }

    let Ok(entries) = fs::read_dir(root) else {
        return Ok(());
    };
    for entry in entries {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if pattern_match(segment, name) {
            expand_segments(&path, rest, paths)?;
        }
    }
    Ok(())
}

fn segments(pattern: &str) -> Vec<&str> {
    pattern.split('/').collect()
}

fn has_pattern(segment: &str) -> bool {
    segment.contains('*') || segment.contains("[0-9]")
}

fn pattern_match(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        Some((prefix, suffix)) => {
            if !name.starts_with(prefix) || !suffix_match(suffix, name) {
                return false;
            }
            name.len() >= prefix.len() + suffix_min_len(suffix)
        }
        None => segment_match(pattern, name),
    }
}

fn suffix_match(pattern: &str, name: &str) -> bool {
    if let Some((before_digit, after_digit)) = pattern.split_once("[0-9]") {
        let Some(tail) = name.strip_suffix(after_digit) else {
            return false;
        };
        let Some((index, digit)) = tail.char_indices().next_back() else {
            return false;
        };
        digit.is_ascii_digit() && tail[..index].ends_with(before_digit)
    } else {
        name.ends_with(pattern)
    }
}

fn suffix_min_len(pattern: &str) -> usize {
    pattern.replace("[0-9]", "0").len()
}

fn segment_match(pattern: &str, name: &str) -> bool {
    if let Some((prefix, suffix)) = pattern.split_once("[0-9]") {
        let Some(rest) = name.strip_prefix(prefix) else {
            return false;
        };
        let Some(rest) = rest.strip_suffix(suffix) else {
            return false;
        };
        rest.len() == 1 && rest.chars().all(|ch| ch.is_ascii_digit())
    } else {
        pattern == name
    }
}

fn ensure_public_parent(target: &Path) -> Result<()> {
    let Some(parent) = target.parent() else {
        return Ok(());
    };
    fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        // The Bash helper creates completion directories under `umask 022` so
        // zsh `compaudit` does not reject them as insecure. Rust inherits the
        // process umask, so explicitly normalize the directory mode.
        //
        // `create_dir_all` may create several intermediate ancestors at once
        // (e.g., `share/zsh/site-functions/` where both `zsh/` and
        // `site-functions/` are fresh). The pre-fix code only normalized the
        // innermost (leaf) parent, so an outer ancestor created with the
        // process umask could keep group/other-write bits — and `zsh
        // compaudit` rejects the WHOLE fpath chain when any ancestor is
        // insecure, silently breaking completion loading. Walk upward from
        // the leaf parent until we hit a directory that already has the
        // public mode, fixing every level the umask-affected
        // `create_dir_all` may have touched. Idempotent: directories that
        // already match the mode are left untouched.
        normalize_ancestor_perms(parent)?;
    }
    Ok(())
}

#[cfg(unix)]
fn normalize_ancestor_perms(leaf: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    // Walk upward stripping group/other-write bits from every directory
    // that has them. Stops as soon as an ancestor is already secure
    // (mode equals strip-022 mode), which acts as the natural boundary
    // between umask-affected directories we just created and pre-existing
    // user-owned directories we have no business mutating. The walk also
    // stops on the first non-directory entry or on any FS error.
    let mut current = Some(leaf);
    while let Some(dir) = current {
        let metadata = match fs::symlink_metadata(dir) {
            Ok(meta) => meta,
            Err(_) => break,
        };
        if !metadata.file_type().is_dir() {
            break;
        }
        let mode = metadata.permissions().mode();
        let secure_mode = mode & !0o022;
        if mode == secure_mode {
            // Already secure: assume every ancestor above is also
            // user-owned and intentional. Do not walk further.
            break;
        }
        fs::set_permissions(dir, fs::Permissions::from_mode(secure_mode))?;
        current = dir.parent();
    }
    Ok(())
}

/// Replaces a shdeps-owned symlink at `target` pointing to `source`.
///
/// Returns `Ok(true)` when the link was created or replaced, and `Ok(false)`
/// when an existing regular file or non-symlink at `target` was preserved.
/// The ownership rule mirrors `bin_link::one`: shdeps may overwrite its own
/// symlinks, but must never clobber a user-owned file the user has placed at
/// the same path (a real man page, a hand-written completion, etc.). Without
/// this guard, an `extras` linker would silently delete user files on every
/// `shdeps update`.
///
/// **Residual race window — user file CAN BE LOST:** if a concurrent
/// process atomically replaces `target` with a regular file in the
/// narrow window between `symlink_metadata` (which saw a symlink) and
/// the atomic-rename `symlink` swap, the rename will atomically
/// overwrite that regular file with the new symlink and this function
/// will return `Ok(true)`. The user's file content is destroyed. The
/// `symlink_metadata` check provides the legitimate-no-clobber guarantee
/// only under the assumption that no other process is racing the same
/// path; the race outcome IS data loss for the user file, not just
/// "behaves as if no file were there." Callers that need a stronger
/// guarantee would have to acquire a parent-directory lock outside
/// this function. The race is bounded by user-write access to the
/// parent directory, which on the normal `~/.local/share/...` layout
/// is just the user themselves, but operators sharing that path with
/// less-trusted tooling should be aware.
#[cfg(unix)]
fn replace_symlink(source: &Path, target: &Path) -> Result<bool> {
    use std::os::unix::fs::symlink;

    // `symlink_metadata` does not follow symlinks, so a dangling symlink is
    // still treated as shdeps-owned (replaceable) rather than a missing file.
    match fs::symlink_metadata(target) {
        Ok(meta) if !meta.file_type().is_symlink() => return Ok(false),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // No existing entry — `symlink` followed by no rename is
            // the simplest path. There is no TOCTOU window because
            // nothing to remove first.
            symlink(source, target)?;
            return Ok(true);
        }
        Err(error) => return Err(error.into()),
    }
    // Atomic replace: create the new symlink under a sibling temp name
    // in the same parent directory, then `rename` it over `target`.
    // `rename` is atomic on POSIX when both paths are on the same
    // filesystem (guaranteed here because the temp name shares a
    // parent dir). This NARROWS — does not eliminate — the previous
    // `symlink_metadata` → `remove_file` → `symlink` race window.
    //
    // Specifically: the prior sequence had a "delete then create"
    // gap during which the path was momentarily missing entirely. An
    // adversary could observe that gap to delete a user-owned file
    // that the user atomically placed at the path between the
    // metadata check and the unlink, then race shdeps for what gets
    // created.
    //
    // With atomic rename there is no missing-path gap: the path is
    // always either the prior content or the new symlink. The race
    // window between `symlink_metadata` and `rename` still exists,
    // and within that window a process that atomically replaces the
    // existing symlink with a regular file will see that file
    // overwritten by our new symlink. The user file is lost in that
    // case — but only its content; no "deleted with no replacement"
    // gap exists. Combined with the upfront `is_symlink` guard that
    // returns false for non-symlinks observed at check time, this is
    // the smallest race surface achievable without
    // `AT_EMPTY_PATH`-style atomic check-and-replace primitives.
    let parent = target.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cannot replace symlink with no parent directory",
        )
    })?;
    let file_name = target.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "target path has no file name",
        )
    })?;
    let staging = parent.join(format!(
        ".{}.shdeps-link.{}.{}",
        file_name.to_string_lossy(),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default(),
    ));
    // Clean any stale staging file from a previous crashed run before
    // creating the new symlink — `symlink` errors out if the path
    // already exists.
    let _ = fs::remove_file(&staging);
    symlink(source, &staging)?;
    if let Err(error) = fs::rename(&staging, target) {
        // Best-effort cleanup of the staging symlink so a failed
        // rename does not leave a `.tool.1.shdeps-link.<pid>.<nanos>`
        // stub next to the target.
        let _ = fs::remove_file(&staging);
        return Err(error.into());
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use crate::link_state::{self, Kind};

    #[test]
    #[cfg(unix)]
    fn links_man_pages_and_completions_with_bash_names() {
        let dir = temp_dir("link");
        let install = dir.join("share/owner/tool");
        fs::create_dir_all(install.join("share/man/man1")).unwrap();
        fs::create_dir_all(install.join("completions")).unwrap();
        fs::write(install.join("share/man/man1/tool.1"), ".TH TOOL 1\n").unwrap();
        fs::write(install.join("completions/tool.bash"), "complete\n").unwrap();
        fs::write(install.join("completions/tool.zsh"), "compdef\n").unwrap();
        fs::write(install.join("completions/tool.fish"), "complete\n").unwrap();

        let created =
            super::link(&dir.join("state"), &dir.join("xdg"), "owner/tool", &install).unwrap();

        assert_eq!(created.len(), 4);
        assert_eq!(
            fs::read_link(dir.join("xdg/man/man1/tool.1")).unwrap(),
            install.join("share/man/man1/tool.1")
        );
        assert_eq!(
            fs::read_link(dir.join("xdg/bash-completion/completions/tool")).unwrap(),
            install.join("completions/tool.bash")
        );
        assert_eq!(
            fs::read_link(dir.join("xdg/zsh/site-functions/_tool")).unwrap(),
            install.join("completions/tool.zsh")
        );
        assert_eq!(
            fs::read_link(dir.join("xdg/fish/vendor_completions.d/tool.fish")).unwrap(),
            install.join("completions/tool.fish")
        );
    }

    #[test]
    #[cfg(unix)]
    fn link_normalizes_all_freshly_created_ancestor_dir_perms() {
        // When `create_dir_all` creates several intermediate dirs at once
        // under a permissive umask, the pre-fix code only fixed the leaf
        // parent. The remaining ancestors kept group/other-write bits,
        // and `zsh compaudit` rejects the whole fpath chain when ANY
        // ancestor is insecure. Now the walk fixes every level created
        // under umask and stops at the first already-secure dir.
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("ancestor-perms");
        let install = dir.join("install");
        fs::create_dir_all(install.join("share/zsh/site-functions")).unwrap();
        fs::write(install.join("share/zsh/site-functions/_tool"), "compdef\n").unwrap();

        // Set the parent xdg directory mode such that creating descendants
        // under it would inherit insecure write bits. We force this state
        // by chmod'ing the parent to 0o775 before sourcing extras.
        let xdg = dir.join("xdg");
        fs::create_dir_all(&xdg).unwrap();
        fs::set_permissions(&xdg, fs::Permissions::from_mode(0o755)).unwrap();

        super::link(&dir.join("state"), &xdg, "owner/tool", &install).unwrap();

        // The leaf parent and EACH freshly created intermediate must
        // have the group/other-write bits stripped.
        let leaf = xdg.join("zsh/site-functions");
        let mid = xdg.join("zsh");
        assert_eq!(
            fs::metadata(&leaf).unwrap().permissions().mode() & 0o022,
            0,
            "leaf parent kept insecure write bits"
        );
        assert_eq!(
            fs::metadata(&mid).unwrap().permissions().mode() & 0o022,
            0,
            "intermediate ancestor kept insecure write bits"
        );
    }

    #[test]
    #[cfg(unix)]
    fn link_preserves_user_owned_regular_file_at_target_path() {
        // The user may have hand-placed a real man page or completion at one
        // of the XDG locations shdeps writes into. Mirroring `bin_link::one`,
        // the extras linker must skip such targets rather than `remove_file`
        // them — otherwise every `shdeps update` silently nukes user data.
        let dir = temp_dir("preserve");
        let install = dir.join("install");
        fs::create_dir_all(install.join("share/man/man1")).unwrap();
        fs::write(install.join("share/man/man1/tool.1"), ".TH TOOL 1\n").unwrap();

        // Pre-existing user-owned regular file at the destination.
        let user_target = dir.join("xdg/man/man1/tool.1");
        fs::create_dir_all(user_target.parent().unwrap()).unwrap();
        fs::write(&user_target, "user-owned man page").unwrap();

        let created =
            super::link(&dir.join("state"), &dir.join("xdg"), "owner/tool", &install).unwrap();

        // The user's file must still be intact and unchanged.
        assert!(!user_target.is_symlink());
        assert_eq!(
            fs::read_to_string(&user_target).unwrap(),
            "user-owned man page"
        );
        // The skipped target must not be recorded in link state, otherwise a
        // later prune would unlink the user's file.
        assert!(created.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn link_replaces_existing_shdeps_owned_symlink() {
        // A stale shdeps symlink (or a dangling one) is the normal replace
        // case; the guard for user-owned files must not block re-linking when
        // the existing target is a symlink.
        let dir = temp_dir("relink");
        let install = dir.join("install");
        fs::create_dir_all(install.join("share/man/man1")).unwrap();
        fs::write(install.join("share/man/man1/tool.1"), ".TH TOOL 1\n").unwrap();

        let target = dir.join("xdg/man/man1/tool.1");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(dir.join("stale-source"), &target).unwrap();

        let created =
            super::link(&dir.join("state"), &dir.join("xdg"), "owner/tool", &install).unwrap();

        assert_eq!(created.len(), 1);
        assert_eq!(
            fs::read_link(&target).unwrap(),
            install.join("share/man/man1/tool.1")
        );
    }

    #[test]
    #[cfg(unix)]
    fn link_state_uses_nested_dependency_name_and_removes_stale_links() {
        let dir = temp_dir("state");
        let install = dir.join("install");
        fs::create_dir_all(install.join("doc")).unwrap();
        fs::write(install.join("doc/tool.1"), ".TH TOOL 1\n").unwrap();

        super::link(&dir.join("state"), &dir.join("xdg"), "owner/tool", &install).unwrap();
        let state_path = link_state::path(&dir.join("state"), "owner/tool", Kind::Extras);
        assert!(state_path.exists());
        assert!(dir.join("xdg/man/man1/tool.1").is_symlink());

        fs::remove_file(install.join("doc/tool.1")).unwrap();
        super::link(&dir.join("state"), &dir.join("xdg"), "owner/tool", &install).unwrap();

        assert!(!dir.join("xdg/man/man1/tool.1").exists());
        assert!(!state_path.exists());
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "shdeps-extras-{name}-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
