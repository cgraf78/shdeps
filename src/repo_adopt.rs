//! Fail-closed inspection for an unrecorded `github:repo` checkout.
//!
//! A preexisting directory has not yet been claimed by Shdeps' manifest, so
//! ordinary update code must not trust its Git configuration or repository
//! shape. This module reads the small identity/control surface directly as
//! inert data. It deliberately does not run Git against the candidate; remote
//! quarantine and complete tree verification build on this preflight.

use std::collections::BTreeMap;
use std::fs;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::repo;

const MAX_CONTROL_BYTES: u64 = 1024 * 1024;
const MAX_REF_ENTRIES: usize = 10_000;
const MAX_HOOK_ENTRIES: usize = 256;

/// Validated shape of an unrecorded managed destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Destination {
    /// No object exists, so a fresh install may publish the root.
    Absent,
    /// A real ordinary checkout passed inert metadata inspection.
    OrdinaryCheckout,
    /// The root has the exact configured development-link shape.
    ///
    /// Repository identity and tracked-command checks belong to the separate
    /// development-source gate; this variant grants no exact-revision claim.
    DevelopmentLink,
}

/// Classifies and validates any unrecorded object at the managed destination.
///
/// Absence is the only state that authorizes a fresh install. A real directory
/// must be a valid ordinary checkout, while a symlink must target the exact
/// configured development path. Every other existing object is preserved and
/// rejected before source selection can delete or replace it.
pub(crate) fn inspect_destination(
    root: &Path,
    development_root: &Path,
    expected_repo: &str,
) -> io::Result<Destination> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Destination::Absent),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_dir() {
        inspect(root, expected_repo)?;
        return Ok(Destination::OrdinaryCheckout);
    }
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(root)?;
        if target != development_root {
            return Err(invalid(
                "managed destination is a foreign or malformed symlink",
            ));
        }
        // Development-source discovery has historically followed a symlink at
        // the configured source path. Validate with the same contract here so
        // an interrupted first publication never creates a link that the next
        // Shdeps run is unable to adopt.
        require_followed_directory(development_root, "development checkout")?;
        return Ok(Destination::DevelopmentLink);
    }
    Err(invalid(
        "managed destination is neither a checkout directory nor the supported development link",
    ))
}

// Inspect only inert identity and control metadata; complete content and remote
// equivalence are deliberately delegated to the quarantine verification layer.
fn inspect(root: &Path, expected_repo: &str) -> io::Result<()> {
    require_directory(root, "checkout root")?;
    let git_dir = root.join(".git");
    require_directory(&git_dir, "checkout .git directory")?;
    validate_object_store_boundaries(&git_dir)?;
    reject_linked_or_in_progress_state(&git_dir)?;

    let head = read_required_text(&git_dir.join("HEAD"), "HEAD")?;
    let branch = parse_attached_branch(&head)?;
    let oid = read_branch_oid(&git_dir, &branch)?;
    if !valid_oid(&oid) {
        return Err(invalid("checkout branch contains an invalid object id"));
    }

    let config = read_required_text(&git_dir.join("config"), "config")?;
    validate_config(&config, &branch, expected_repo)?;
    validate_hooks(&git_dir.join("hooks"))?;
    Ok(())
}

// Require a non-symlink directory where adoption must own the directory inode.
fn require_directory(path: &Path, label: &str) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            invalid(format!("{label} is missing"))
        } else {
            error
        }
    })?;
    if !metadata.file_type().is_dir() {
        return Err(invalid(format!("{label} is not a real directory")));
    }
    Ok(())
}

// Preserve the historical development-source contract, which follows a
// configured symlink but still requires its final target to be a directory.
fn require_followed_directory(path: &Path, label: &str) -> io::Result<()> {
    let metadata = fs::metadata(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            invalid(format!("{label} is missing"))
        } else {
            error
        }
    })?;
    if !metadata.is_dir() {
        return Err(invalid(format!("{label} is not a directory")));
    }
    Ok(())
}

// Reject top-level object-store indirection before any legacy Git path can read
// or write through a candidate-controlled external directory.
fn validate_object_store_boundaries(git_dir: &Path) -> io::Result<()> {
    let objects = git_dir.join("objects");
    require_directory(&objects, "checkout object store")?;
    for (relative, label) in [
        ("info", "checkout object-store info directory"),
        ("pack", "checkout object-store pack directory"),
    ] {
        let path = objects.join(relative);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => return Err(invalid(format!("{label} is not a real directory"))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

// Reject Git states whose semantics depend on recovery metadata, linked
// worktrees, or object substitution outside this ordinary checkout.
fn reject_linked_or_in_progress_state(git_dir: &Path) -> io::Result<()> {
    for relative in [
        "commondir",
        "config.worktree",
        "worktrees",
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "REBASE_HEAD",
        "BISECT_LOG",
        "BISECT_START",
        "AUTO_MERGE",
        "rebase-apply",
        "rebase-merge",
        "sequencer",
        "index.lock",
        "HEAD.lock",
        "config.lock",
        "packed-refs.lock",
        "shallow.lock",
        "objects/info/alternates",
        "objects/info/http-alternates",
        "info/grafts",
        "refs/replace",
    ] {
        let path = git_dir.join(relative);
        if exists_no_follow(&path)? {
            return Err(invalid(format!(
                "checkout contains unsupported Git state `{relative}`"
            )));
        }
    }

    let mut visited = 0;
    reject_ref_locks(&git_dir.join("refs"), "refs", &mut visited)
}

// Walk the bounded loose-ref namespace without following links, rejecting lock
// debris and names that Git would parse differently from this inert reader.
fn reject_ref_locks(path: &Path, relative: &str, visited: &mut usize) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_dir() {
        return Err(invalid("checkout refs path is not a real directory"));
    }

    for entry in fs::read_dir(path)? {
        *visited += 1;
        if *visited > MAX_REF_ENTRIES {
            return Err(invalid("checkout refs namespace is unexpectedly large"));
        }
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid("checkout refs contain a non-UTF-8 name"))?;
        if file_type.is_symlink() {
            return Err(invalid("checkout refs contain a symlink"));
        }
        if !valid_ref_component(&name) {
            return Err(invalid(format!(
                "checkout refs contain an invalid component `{name}`"
            )));
        }
        let ref_name = format!("{relative}/{name}");
        if file_type.is_dir() {
            reject_ref_locks(&entry.path(), &ref_name, visited)?;
        } else if !file_type.is_file() {
            return Err(invalid("checkout refs contain a special file"));
        } else if !valid_ref_name(&ref_name) {
            return Err(invalid(format!(
                "checkout refs contain an invalid name `{ref_name}`"
            )));
        } else {
            reject_multiple_links(&entry.metadata()?, "ref")?;
        }
    }
    Ok(())
}

// Distinguish true absence from permission or I/O failures at a no-follow
// ownership boundary.
fn exists_no_follow(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

// Read one bounded regular control file without accepting symlink, hardlink,
// NUL, or non-UTF-8 aliases that could escape the candidate boundary.
fn read_required_text(path: &Path, label: &str) -> io::Result<String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            invalid(format!("checkout {label} is missing"))
        } else {
            error
        }
    })?;
    if !metadata.file_type().is_file() {
        return Err(invalid(format!("checkout {label} is not a regular file")));
    }
    if metadata.len() > MAX_CONTROL_BYTES {
        return Err(invalid(format!("checkout {label} is unexpectedly large")));
    }
    reject_multiple_links(&metadata, label)?;
    let bytes = fs::read(path)?;
    if bytes.contains(&0) {
        return Err(invalid(format!("checkout {label} contains a NUL byte")));
    }
    String::from_utf8(bytes).map_err(|_| invalid(format!("checkout {label} is not valid UTF-8")))
}

// Extract an attached local branch from HEAD; detached identities are verified
// later by quarantine and are intentionally not adoptable.
fn parse_attached_branch(head: &str) -> io::Result<String> {
    let head = one_line(head, "HEAD")?;
    let branch = head
        .strip_prefix("ref: refs/heads/")
        .ok_or_else(|| invalid("checkout HEAD is detached or malformed"))?;
    if !valid_branch(branch) {
        return Err(invalid("checkout HEAD names an unsafe branch"));
    }
    Ok(branch.to_owned())
}

// Keep control records to one logical line so hidden trailing state cannot be
// interpreted differently by Git and this preflight.
fn one_line<'a>(content: &'a str, label: &str) -> io::Result<&'a str> {
    let line = content.strip_suffix('\n').unwrap_or(content);
    if line.is_empty() || line.contains(['\n', '\r']) {
        return Err(invalid(format!("checkout {label} is not one clean line")));
    }
    Ok(line)
}

// Apply the shared Git ref grammar to the branch suffix selected by HEAD.
fn valid_branch(branch: &str) -> bool {
    valid_ref_suffix(branch)
}

// Validate a complete refs/ name while keeping the reusable grammar separate
// from the required namespace prefix.
fn valid_ref_name(name: &str) -> bool {
    name.strip_prefix("refs/").is_some_and(valid_ref_suffix)
}

// Encode Git's forbidden ref forms rather than embedding a narrower policy
// alphabet that would reject otherwise ordinary repositories.
fn valid_ref_suffix(name: &str) -> bool {
    !name.is_empty()
        && name != "@"
        && !name.ends_with(['.', '/'])
        && !name.contains("..")
        && !name.contains("@{")
        && !name.contains("//")
        && name.bytes().all(|byte| {
            !byte.is_ascii_control()
                && byte != b' '
                && !matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
        && name.split('/').all(valid_ref_component)
}

// Validate each path component because dot-prefix and .lock restrictions apply
// independently at every slash-delimited level.
fn valid_ref_component(component: &str) -> bool {
    !component.is_empty()
        && !component.starts_with('.')
        && !component.ends_with(".lock")
        && !component.contains("..")
        && !component.contains("@{")
        && component.bytes().all(|byte| {
            !byte.is_ascii_control()
                && byte != b' '
                && !matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
}

// Resolve the attached branch from loose or packed refs without invoking Git
// or accepting a missing identity.
fn read_branch_oid(git_dir: &Path, branch: &str) -> io::Result<String> {
    let ref_name = format!("refs/heads/{branch}");
    let loose = git_dir.join(&ref_name);
    let loose_oid = match fs::symlink_metadata(&loose) {
        Ok(_) => {
            Some(one_line(&read_required_text(&loose, "branch ref")?, "branch ref")?.to_owned())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };

    let packed_oid = read_packed_branch_oid(git_dir, &ref_name)?;
    loose_oid
        .or(packed_oid)
        .ok_or_else(|| invalid("checkout branch ref is missing"))
}

// Parse the bounded packed-ref control file and return the unique active branch
// while validating every unrelated record that Git would also consume.
fn read_packed_branch_oid(git_dir: &Path, ref_name: &str) -> io::Result<Option<String>> {
    let packed_path = git_dir.join("packed-refs");
    let packed = match fs::symlink_metadata(&packed_path) {
        Ok(_) => read_required_text(&packed_path, "packed-refs")?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut found = None;
    let mut peel_allowed = false;
    for line in packed.lines() {
        if line.is_empty() || line.starts_with('#') {
            peel_allowed = false;
            continue;
        }
        if let Some(peeled) = line.strip_prefix('^') {
            if !peel_allowed || !valid_oid(peeled) {
                return Err(invalid(
                    "checkout packed-refs contains an orphan or invalid peeled id",
                ));
            }
            peel_allowed = false;
            continue;
        }
        let (oid, name) = line
            .split_once(' ')
            .ok_or_else(|| invalid("checkout packed-refs contains a malformed record"))?;
        if !valid_oid(oid) || !valid_ref_name(name) {
            return Err(invalid("checkout packed-refs contains a malformed ref"));
        }
        if name.starts_with("refs/replace/") {
            return Err(invalid("checkout contains a packed replacement ref"));
        }
        peel_allowed = name.starts_with("refs/tags/");
        if name == ref_name && found.replace(oid.to_owned()).is_some() {
            return Err(invalid("checkout packed-refs repeats the active branch"));
        }
    }
    Ok(found)
}

// This v1 preflight accepts only SHA-1 repositories; SHA-256 requires matching
// repository-format extension handling rather than a wider length check alone.
fn valid_oid(oid: &str) -> bool {
    oid.len() == 40 && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[derive(Debug, Clone, Copy)]
enum Section {
    Core,
    Origin,
    Branch,
}

// Parse the small ordinary-clone config grammar as data, excluding every key
// that could redirect execution, credentials, transport, or object behavior.
fn validate_config(config: &str, branch: &str, expected_repo: &str) -> io::Result<()> {
    let mut section = None;
    let mut core = BTreeMap::new();
    let mut origin_url = None;
    let mut origin_fetch = None;
    let mut origin_push_url = None;
    let mut origin_tag_opt = None;
    let mut branch_remote = None;
    let mut branch_merge = None;

    for (index, raw_line) in config.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if line.starts_with('[') {
            section = Some(parse_section(line, branch).map_err(|error| {
                invalid(format!("checkout config line {}: {error}", index + 1))
            })?);
            continue;
        }

        let active = section.ok_or_else(|| {
            invalid(format!(
                "checkout config line {} appears before a section",
                index + 1
            ))
        })?;
        let (key, value) = line.split_once('=').ok_or_else(|| {
            invalid(format!(
                "checkout config line {} is not a simple assignment",
                index + 1
            ))
        })?;
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        if key.is_empty()
            || value.is_empty()
            || value.bytes().any(|byte| byte.is_ascii_control())
            || value.contains(['"', '\'', '\\'])
        {
            return Err(invalid(format!(
                "checkout config line {} uses unsupported quoting or bytes",
                index + 1
            )));
        }

        match active {
            Section::Core => {
                validate_core_value(&key, value)?;
                insert_once(&mut core, &key, value, "core config")?;
            }
            Section::Origin => match key.as_str() {
                "url" => set_once(&mut origin_url, value, "origin URL")?,
                "fetch" => set_once(&mut origin_fetch, value, "origin fetch refspec")?,
                "pushurl" => set_once(&mut origin_push_url, value, "origin push URL")?,
                "tagopt" => set_once(&mut origin_tag_opt, value, "origin tag option")?,
                _ => {
                    return Err(invalid(format!(
                        "checkout origin config contains unsupported key `{key}`"
                    )));
                }
            },
            Section::Branch => match key.as_str() {
                "remote" => set_once(&mut branch_remote, value, "branch remote")?,
                "merge" => set_once(&mut branch_merge, value, "branch merge ref")?,
                _ => {
                    return Err(invalid(format!(
                        "checkout branch config contains unsupported key `{key}`"
                    )));
                }
            },
        }
    }

    if core.get("repositoryformatversion").map(String::as_str) != Some("0")
        || !core
            .get("bare")
            .is_some_and(|value| value.eq_ignore_ascii_case("false"))
    {
        return Err(invalid(
            "checkout core config is not an ordinary non-bare repository",
        ));
    }
    let origin_url = origin_url.ok_or_else(|| invalid("checkout origin URL is missing"))?;
    validate_repo_url(&origin_url, expected_repo, "origin")?;
    if let Some(push_url) = origin_push_url {
        validate_repo_url(&push_url, expected_repo, "origin push URL")?;
    }
    // Ordinary clones use the wildcard refspec, while the shared Actions
    // bootstrap deliberately uses a shallow, no-tags, single-branch clone.
    // Accept exactly those two Git-produced forms so Shdeps can adopt the
    // bootstrap checkout without broadening this inert config grammar.
    let single_branch_fetch = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");
    if !matches!(
        origin_fetch.as_deref(),
        Some("+refs/heads/*:refs/remotes/origin/*")
    ) && origin_fetch.as_deref() != Some(single_branch_fetch.as_str())
    {
        return Err(invalid("checkout origin fetch refspec is not canonical"));
    }
    if origin_tag_opt
        .as_deref()
        .is_some_and(|value| value != "--no-tags")
    {
        return Err(invalid("checkout origin tag option is not canonical"));
    }
    if branch_remote.as_deref() != Some("origin") {
        return Err(invalid("checkout branch does not track origin"));
    }
    if branch_merge.as_deref() != Some(&format!("refs/heads/{branch}")) {
        return Err(invalid("checkout branch merge ref does not match HEAD"));
    }
    Ok(())
}

// Admit only core, origin, and the exact active branch sections; unknown
// sections are unsafe because their semantics have not been inertly modeled.
fn parse_section(line: &str, branch: &str) -> io::Result<Section> {
    let inner = line
        .strip_prefix('[')
        .and_then(|line| line.strip_suffix(']'))
        .ok_or_else(|| invalid("malformed section header"))?;
    if inner.eq_ignore_ascii_case("core") {
        return Ok(Section::Core);
    }
    if let Some(subsection) = quoted_subsection(inner, "remote") {
        return (subsection == "origin")
            .then_some(Section::Origin)
            .ok_or_else(|| invalid("only the origin remote is allowed"));
    }
    if let Some(subsection) = quoted_subsection(inner, "branch") {
        return (subsection == branch)
            .then_some(Section::Branch)
            .ok_or_else(|| invalid("config branch does not match HEAD"));
    }
    Err(invalid("unsupported config section"))
}

// Extract Git's quoted subsection spelling without supporting escapes that
// would require a second parser and risk interpretation drift.
fn quoted_subsection<'a>(inner: &'a str, expected_section: &str) -> Option<&'a str> {
    let whitespace = inner.find(char::is_whitespace)?;
    let (section, rest) = inner.split_at(whitespace);
    if !section.eq_ignore_ascii_case(expected_section) {
        return None;
    }
    let quoted = rest.trim();
    quoted
        .strip_prefix('"')?
        .strip_suffix('"')
        .filter(|value| !value.is_empty() && !value.contains(['"', '\\']))
}

// Restrict core configuration to passive filesystem compatibility booleans and
// the ordinary non-bare repository format.
fn validate_core_value(key: &str, value: &str) -> io::Result<()> {
    let valid = match key {
        "repositoryformatversion" => value == "0",
        "bare" => value.eq_ignore_ascii_case("false"),
        "filemode" | "ignorecase" | "precomposeunicode" | "symlinks" => {
            value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("false")
        }
        "logallrefupdates" => value.eq_ignore_ascii_case("true"),
        _ => false,
    };
    valid.then_some(()).ok_or_else(|| {
        invalid(format!(
            "checkout core config contains unsupported key or value `{key}`"
        ))
    })
}

// Record one keyed setting and reject duplicates whose last-wins behavior could
// diverge from this validator's ownership decision.
fn insert_once(
    values: &mut BTreeMap<String, String>,
    key: &str,
    value: &str,
    label: &str,
) -> io::Result<()> {
    if values.insert(key.to_owned(), value.to_owned()).is_some() {
        return Err(invalid(format!("checkout repeats {label} key `{key}`")));
    }
    Ok(())
}

// Record one singleton setting while rejecting repeated remote/branch values.
fn set_once(slot: &mut Option<String>, value: &str, label: &str) -> io::Result<()> {
    if slot.replace(value.to_owned()).is_some() {
        return Err(invalid(format!("checkout repeats {label}")));
    }
    Ok(())
}

// Compare every accepted URL through the shared strict GitHub canonicalizer so
// installer and Shdeps identities cannot drift by transport spelling.
fn validate_repo_url(url: &str, expected_repo: &str, label: &str) -> io::Result<()> {
    if repo::canonical_github_repo(url).as_deref() != Some(expected_repo) {
        return Err(invalid(format!(
            "checkout {label} does not identify configured repository `{expected_repo}`"
        )));
    }
    Ok(())
}

// Allow only bounded, unaliased template samples; an active or special hook is
// executable candidate policy and therefore cannot cross the adoption boundary.
fn validate_hooks(hooks_dir: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(hooks_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_dir() {
        return Err(invalid("checkout hooks path is not a real directory"));
    }
    for (index, entry) in fs::read_dir(hooks_dir)?.enumerate() {
        if index >= MAX_HOOK_ENTRIES {
            return Err(invalid("checkout hooks directory is unexpectedly large"));
        }
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid("checkout hooks contain a non-UTF-8 name"))?;
        if !file_type.is_file() || !name.ends_with(".sample") {
            return Err(invalid(format!(
                "checkout contains active or unsupported hook `{name}`"
            )));
        }
        reject_multiple_links(&entry.metadata()?, "hook sample")?;
    }
    Ok(())
}

// Reject control inodes with external hardlink aliases before later permission
// normalization or Git mutation could affect an outside path.
fn reject_multiple_links(metadata: &fs::Metadata, label: &str) -> io::Result<()> {
    #[cfg(unix)]
    if metadata.nlink() != 1 {
        return Err(invalid(format!(
            "checkout {label} has aliases outside its ownership boundary"
        )));
    }
    #[cfg(not(unix))]
    let _ = (metadata, label);
    Ok(())
}

// Keep all fail-closed preflight diagnostics in one stable InvalidData class.
fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;

    use super::inspect;

    const OID: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn accepts_a_minimal_standard_checkout_identity() {
        let root = candidate("valid", "https://github.com/owner/tool");
        fs::write(
            root.join(".git/packed-refs"),
            format!("{OID} refs/tags/v1.0+portable\n^{OID}\n"),
        )
        .unwrap();

        inspect(&root, "owner/tool").unwrap();
    }

    #[test]
    fn accepts_actions_single_branch_checkout_identity() {
        let root = candidate("actions-single-branch", "https://github.com/owner/tool");
        let config = fs::read_to_string(root.join(".git/config")).unwrap();
        fs::write(
            root.join(".git/config"),
            config.replace(
                "\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
                "\ttagOpt = --no-tags\n\tfetch = +refs/heads/main:refs/remotes/origin/main\n",
            ),
        )
        .unwrap();

        inspect(&root, "owner/tool").unwrap();
    }

    #[test]
    fn rejects_foreign_origins_and_dangerous_config_surfaces() {
        let foreign = candidate("foreign", "https://github.com/other/tool");
        assert!(
            inspect(&foreign, "owner/tool")
                .unwrap_err()
                .to_string()
                .contains("origin")
        );

        for (name, section) in [
            ("include", "[include]\n\tpath = /tmp/hostile\n"),
            ("ssh-command", "[core]\n\tsshCommand = /tmp/hostile\n"),
            ("fsmonitor", "[core]\n\tfsmonitor = /tmp/hostile\n"),
            (
                "url-rewrite",
                "[url \"ssh://hostile/\"]\n\tinsteadOf = https://github.com/\n",
            ),
            (
                "upload-pack",
                "[remote \"origin\"]\n\tuploadpack = /tmp/hostile\n",
            ),
            ("tag-option", "[remote \"origin\"]\n\ttagOpt = --tags\n"),
            (
                "credential-helper",
                "[credential]\n\thelper = /tmp/hostile\n",
            ),
        ] {
            let root = candidate(name, "https://github.com/owner/tool");
            fs::OpenOptions::new()
                .append(true)
                .open(root.join(".git/config"))
                .unwrap()
                .write_all(section.as_bytes())
                .unwrap();
            assert!(inspect(&root, "owner/tool").is_err(), "{name}");
        }
    }

    #[test]
    fn rejects_recovery_and_object_substitution_state() {
        for (name, relative) in [
            ("merge", "MERGE_HEAD"),
            ("rebase", "rebase-merge"),
            ("lock", "refs/heads/main.lock"),
            ("alternates", "objects/info/alternates"),
            ("grafts", "info/grafts"),
            ("replace", "refs/replace/one"),
        ] {
            let root = candidate(name, "https://github.com/owner/tool");
            let path = root.join(".git").join(relative);
            if relative == "rebase-merge" {
                fs::create_dir_all(&path).unwrap();
            } else {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(&path, OID).unwrap();
            }
            assert!(inspect(&root, "owner/tool").is_err(), "{name}");
        }

        let packed_replace = candidate("packed-replace", "https://github.com/owner/tool");
        fs::write(
            packed_replace.join(".git/packed-refs"),
            format!("# pack-refs with: peeled fully-peeled\n{OID} refs/replace/one\n"),
        )
        .unwrap();
        assert!(inspect(&packed_replace, "owner/tool").is_err());

        let malformed_packed = candidate("malformed-packed", "https://github.com/owner/tool");
        fs::write(
            malformed_packed.join(".git/packed-refs"),
            format!("{OID} refs/heads/feature/.hidden\n"),
        )
        .unwrap();
        assert!(inspect(&malformed_packed, "owner/tool").is_err());

        let malformed_loose = candidate("malformed-loose", "https://github.com/owner/tool");
        fs::create_dir_all(malformed_loose.join(".git/refs/tags")).unwrap();
        fs::write(malformed_loose.join(".git/refs/tags/a..b"), OID).unwrap();
        assert!(inspect(&malformed_loose, "owner/tool").is_err());

        let orphan_peeled = candidate("orphan-peeled", "https://github.com/owner/tool");
        fs::write(orphan_peeled.join(".git/packed-refs"), format!("^{OID}\n")).unwrap();
        assert!(inspect(&orphan_peeled, "owner/tool").is_err());

        let sample_directory = candidate("hook-directory", "https://github.com/owner/tool");
        fs::create_dir_all(sample_directory.join(".git/hooks/hostile.sample")).unwrap();
        assert!(inspect(&sample_directory, "owner/tool").is_err());

        let excessive_hooks = candidate("excessive-hooks", "https://github.com/owner/tool");
        fs::create_dir_all(excessive_hooks.join(".git/hooks")).unwrap();
        for index in 0..257 {
            fs::write(
                excessive_hooks.join(format!(".git/hooks/hook-{index}.sample")),
                "sample\n",
            )
            .unwrap();
        }
        assert!(inspect(&excessive_hooks, "owner/tool").is_err());

        let hardlinked_config = candidate("hardlinked-config", "https://github.com/owner/tool");
        let config_path = hardlinked_config.join(".git/config");
        let outside = hardlinked_config.with_extension("outside-config");
        fs::rename(&config_path, &outside).unwrap();
        fs::hard_link(&outside, &config_path).unwrap();
        assert!(inspect(&hardlinked_config, "owner/tool").is_err());

        let hardlinked_ref = candidate("hardlinked-ref", "https://github.com/owner/tool");
        let outside_ref = hardlinked_ref.with_extension("outside-ref");
        fs::write(&outside_ref, OID).unwrap();
        fs::create_dir_all(hardlinked_ref.join(".git/refs/tags")).unwrap();
        fs::hard_link(&outside_ref, hardlinked_ref.join(".git/refs/tags/one")).unwrap();
        assert!(inspect(&hardlinked_ref, "owner/tool").is_err());
    }

    #[test]
    #[cfg(unix)]
    fn rejects_symlinked_object_store_boundaries() {
        use std::os::unix::fs::symlink;

        for relative in ["objects", "objects/info", "objects/pack"] {
            let root = candidate(
                &format!("symlinked-{}", relative.replace('/', "-")),
                "https://github.com/owner/tool",
            );
            let path = root.join(".git").join(relative);
            if relative == "objects" {
                fs::remove_dir_all(&path).unwrap();
            } else {
                fs::remove_dir(&path).unwrap();
            }
            let external = root.with_extension(relative.replace('/', "-"));
            fs::create_dir_all(&external).unwrap();
            symlink(&external, &path).unwrap();

            assert!(inspect(&root, "owner/tool").is_err(), "{relative}");
        }
    }

    #[test]
    fn rejects_detached_or_mismatched_branch_state() {
        let detached = candidate("detached", "https://github.com/owner/tool");
        fs::write(detached.join(".git/HEAD"), format!("{OID}\n")).unwrap();
        assert!(inspect(&detached, "owner/tool").is_err());

        let mismatch = candidate("branch-mismatch", "https://github.com/owner/tool");
        let config = fs::read_to_string(mismatch.join(".git/config")).unwrap();
        fs::write(
            mismatch.join(".git/config"),
            config.replace("refs/heads/main", "refs/heads/other"),
        )
        .unwrap();
        assert!(inspect(&mismatch, "owner/tool").is_err());

        for branch in ["feature/.hidden", "topic.lock/sub"] {
            let unsafe_branch = candidate(
                &format!("unsafe-branch-{}", branch.replace('/', "-")),
                "https://github.com/owner/tool",
            );
            let ref_path = unsafe_branch.join(".git/refs/heads").join(branch);
            fs::create_dir_all(ref_path.parent().unwrap()).unwrap();
            fs::write(&ref_path, format!("{OID}\n")).unwrap();
            fs::write(
                unsafe_branch.join(".git/HEAD"),
                format!("ref: refs/heads/{branch}\n"),
            )
            .unwrap();
            let config = fs::read_to_string(unsafe_branch.join(".git/config")).unwrap();
            fs::write(
                unsafe_branch.join(".git/config"),
                config.replace("main", branch),
            )
            .unwrap();
            assert!(inspect(&unsafe_branch, "owner/tool").is_err(), "{branch}");
        }

        let sha256_without_extension = candidate("sha256-oid", "https://github.com/owner/tool");
        fs::write(
            sha256_without_extension.join(".git/refs/heads/main"),
            format!("{}\n", "a".repeat(64)),
        )
        .unwrap();
        assert!(inspect(&sha256_without_extension, "owner/tool").is_err());
    }

    fn candidate(name: &str, origin: &str) -> std::path::PathBuf {
        let root = crate::test_support::temp_dir(&format!("shdeps-repo-adopt-{name}"));
        fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
        fs::create_dir_all(root.join(".git/objects/info")).unwrap();
        fs::create_dir_all(root.join(".git/objects/pack")).unwrap();
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(root.join(".git/refs/heads/main"), format!("{OID}\n")).unwrap();
        fs::write(
            root.join(".git/config"),
            format!(
                "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n\tlogallrefupdates = true\n[remote \"origin\"]\n\turl = {origin}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n"
            ),
        )
        .unwrap();
        root
    }
}
