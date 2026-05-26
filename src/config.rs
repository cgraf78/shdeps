//! Config parsing and dependency identity rules.
//!
//! This module owns the grammar for `*.conf` lines and the canonical
//! dependency name used by manifests, state paths, hooks, and install roots.
//! Keeping those rules here avoids subtle drift where one caller strips
//! `.git`, another uses the raw config spelling, and cleanup logic later looks
//! in the wrong path.

use std::fs;
use std::path::{Path, PathBuf};

use crate::Result;

/// Parsed dependency entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Canonical dependency name and manifest key.
    pub name: String,
    /// Install method, such as `pkg` or `github:release`.
    pub method: String,
    /// Command name used for command lookup and public bin links.
    pub cmd: String,
    /// Package-manager override list for `pkg` dependencies.
    pub aliases: String,
    /// Combined `os:`/`host:` filter string.
    pub filter: String,
}

/// Canonicalizes a dependency name for the install method.
#[must_use]
pub fn canonical_name(name: &str, method: &str) -> String {
    match method {
        "github" | "github:repo" | "github:release" => name
            .strip_suffix(".git")
            .filter(|_| name.contains('/'))
            .unwrap_or(name)
            .to_owned(),
        _ => name.to_owned(),
    }
}

/// Returns the short dependency name after the final `/`.
#[must_use]
pub fn short_name(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

/// Resolves a package-manager-qualified override list.
///
/// The Bash reference uses this both for package aliases and for command names
/// such as `apt:batcat`. A missing or non-matching package manager returns the
/// default value unchanged; the `NONE` sentinel is intentionally just another
/// resolved value because higher-level package logic owns the skip behavior.
#[must_use]
pub fn resolve_override(default: &str, overrides: &str, pkg_mgr: Option<&str>) -> String {
    let Some(pkg_mgr) = pkg_mgr.filter(|value| !value.is_empty()) else {
        return default.to_owned();
    };

    for pair in overrides.split(',').filter(|pair| !pair.is_empty()) {
        let (manager, value) = pair.split_once(':').unwrap_or((pair, ""));
        if manager == pkg_mgr {
            return value.to_owned();
        }
    }

    default.to_owned()
}

/// Parses one pipe-delimited registry entry.
///
/// The shell loader stores whitespace-split config lines as `|`-delimited
/// strings before a later parse step expands defaults. Keeping this parser
/// separate lets Rust tests compare both the raw load shape and the expanded
/// operational entry.
#[must_use]
pub fn parse_entry(raw: &str, pkg_mgr: Option<&str>) -> Entry {
    let mut fields = raw.split('|');
    let raw_name = fields.next().unwrap_or_default();
    let method = fields.next().unwrap_or_default();
    let mut cmd = dash_to_empty(fields.next().unwrap_or_default()).to_owned();
    let aliases = dash_to_empty(fields.next().unwrap_or_default()).to_owned();
    let filter = dash_to_empty(fields.next().unwrap_or_default()).to_owned();

    let name = canonical_name(raw_name, method);
    if cmd.is_empty() {
        cmd = short_name(&name).to_owned();
    }
    if cmd.contains(':') {
        cmd = resolve_override(short_name(&name), &cmd, pkg_mgr);
    }

    Entry {
        name,
        method: method.to_owned(),
        cmd,
        aliases,
        filter,
    }
}

/// Parses a single config-file line into the raw loaded-entry representation.
///
/// Lines that are blank or begin with `#` after leading whitespace are ignored.
/// Inline comments are recognized as any whitespace-separated token starting
/// with `#`: `tool  pkg  # this is a comment` parses as the two fields
/// `tool|pkg`. A `#` inside an unquoted field (e.g., `tool#extra pkg`) is
/// preserved because that is a single token and there is no field whose
/// real value starts with `#` in any current dep method.
#[must_use]
pub fn parse_config_line(line: &str) -> Option<String> {
    let trimmed_start = line.trim_start();
    if trimmed_start.is_empty() || trimmed_start.starts_with('#') {
        return None;
    }

    let mut fields = line
        .split_whitespace()
        // `take_while` stops at the first token that begins a comment so we
        // do not silently materialize `#` as the `cmd` field (or any other
        // field) when a user appends a trailing remark to a dep line.
        .take_while(|field| !field.starts_with('#'))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if fields.is_empty() {
        // The entire line was a comment after leading non-#: e.g.,
        // `   # spaced comment` is already handled above, but a defensive
        // empty check keeps later index accesses safe and avoids emitting an
        // empty `||` entry.
        return None;
    }
    if fields.len() >= 2 {
        fields[0] = canonical_name(&fields[0], &fields[1]);
    }

    Some(fields.join("|"))
}

/// Sorts raw loaded entries using the Bash reference's case-insensitive name key.
pub fn sort_entries(entries: &mut [String]) {
    entries.sort_by(|left, right| {
        entry_name(left)
            .to_lowercase()
            .cmp(&entry_name(right).to_lowercase())
            .then_with(|| entry_name(left).cmp(entry_name(right)))
            .then_with(|| left.cmp(right))
    });
}

/// Parses config text from already ordered files into sorted raw entries.
///
/// When the same dependency name appears more than once across the input
/// files (or within one file), only the *last* occurrence is kept. This
/// matches the `manifest::get` semantics — which already returns the last
/// matching record for a given name — and prevents the updater from
/// processing the same dep twice (and consequently running install / post
/// hooks twice). Inputs are iterated in caller-supplied order, so the
/// directory loader's lexical file ordering is preserved end-to-end.
#[must_use]
pub fn parse_config_texts<'a>(texts: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let entries = texts
        .into_iter()
        .flat_map(|text| text.lines().filter_map(parse_config_line))
        .collect::<Vec<_>>();
    let mut entries = dedupe_last_wins(entries);
    sort_entries(&mut entries);
    entries
}

/// Deduplicates entries by dependency name, keeping the LAST occurrence.
///
/// Two-pass: first walk records the index of each name's last
/// occurrence, then a second walk keeps only entries whose index
/// matches. The dedupe DECISION is stable with respect to load order
/// (the latest occurrence per name is the one that survives), and the
/// SURVIVING entries are returned in their original load-order
/// positions — `parse_config_texts` and `load_dir` then pass that
/// result through `sort_entries`, so the user-visible final order is
/// sort order, NOT load order. Callers that need load-order output
/// must call this helper directly and skip the sort.
///
/// Runs in O(n) time on top of one `HashMap` — suitable for the
/// few-hundred-deps scale we expect in practice. The `HashMap<String,
/// usize>` does allocate one owned `String` per unique name; a
/// `&str`-keyed map would avoid that but require borrow gymnastics
/// that are not worth the noise at this scale.
fn dedupe_last_wins(entries: Vec<String>) -> Vec<String> {
    use std::collections::HashMap;
    let mut last_seen: HashMap<String, usize> = HashMap::new();
    for (index, entry) in entries.iter().enumerate() {
        last_seen.insert(entry_name(entry).to_owned(), index);
    }
    entries
        .into_iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            (last_seen.get(entry_name(&entry)) == Some(&index)).then_some(entry)
        })
        .collect()
}

/// Loads all direct `*.conf` files from a config directory into sorted entries.
///
/// This mirrors the Bash loader's durable behavior: missing config dirs are an
/// empty dependency set, files are read in lexical path order, and the final
/// registry is sorted by dependency name. Keeping directory loading here means
/// `update`, `list`, `dep-file`, and bridge APIs cannot accidentally disagree
/// about `.git` canonicalization or comment handling.
pub fn load_dir(conf_dir: &Path) -> Result<Vec<String>> {
    let mut files = conf_files(conf_dir)?;
    files.sort();

    let mut entries = Vec::new();
    for file in files {
        let content = fs::read_to_string(file)?;
        entries.extend(content.lines().filter_map(parse_config_line));
    }
    // Match `parse_config_texts`: dedupe by dep name before sorting so a
    // user who declares the same dep in two `*.conf` files gets the last
    // (lex-order) definition rather than two adjacent entries that would
    // both get processed by `update`.
    let mut entries = dedupe_last_wins(entries);
    sort_entries(&mut entries);
    Ok(entries)
}

/// Returns whether a dependency name is safe to use as a managed path suffix.
#[must_use]
pub fn valid_dep_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && !name.split('/').any(|part| part == "..")
        && !name.chars().any(char::is_whitespace)
        && !name.contains('|')
}

fn dash_to_empty(value: &str) -> &str {
    if value == "-" { "" } else { value }
}

fn entry_name(entry: &str) -> &str {
    entry.split('|').next().unwrap_or(entry)
}

/// Returns direct `*.conf` files in a config directory.
///
/// Loading and cache validation both need the same file set. Keeping the glob
/// rule here prevents the package warm-path cache from accidentally proving a
/// different config surface than `load_dir()` actually reads.
pub fn conf_files(conf_dir: &Path) -> Result<Vec<PathBuf>> {
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::{
        canonical_name, load_dir, parse_config_line, parse_config_texts, parse_entry,
        resolve_override, short_name, sort_entries, valid_dep_name,
    };

    #[test]
    fn canonicalizes_git_suffix_only_for_github_methods() {
        assert_eq!(
            canonical_name("cgraf78/ds.git", "github:repo"),
            "cgraf78/ds"
        );
        assert_eq!(
            canonical_name("owner/tool.git", "github:release"),
            "owner/tool"
        );
        assert_eq!(canonical_name("tool.git", "pkg"), "tool.git");
    }

    #[test]
    fn short_name_uses_suffix_after_final_slash() {
        assert_eq!(short_name("neovim/neovim"), "neovim");
        assert_eq!(short_name("bat"), "bat");
        assert_eq!(
            short_name("zsh-users/zsh-autosuggestions"),
            "zsh-autosuggestions"
        );
    }

    #[test]
    fn resolve_override_returns_matching_manager_value() {
        assert_eq!(resolve_override("fd", "", Some("apt")), "fd");
        assert_eq!(
            resolve_override("fd", "apt:fd-find,dnf:fd-find", Some("apt")),
            "fd-find"
        );
        assert_eq!(
            resolve_override("fd", "apt:fd-find,dnf:fd-find", Some("pacman")),
            "fd"
        );
        assert_eq!(
            resolve_override(
                "nerd-fonts",
                "apt:NONE,brew:font-hack-nerd-font",
                Some("apt")
            ),
            "NONE"
        );
        assert_eq!(
            resolve_override(
                "nerd-fonts",
                "apt:NONE,brew:font-hack-nerd-font",
                Some("brew")
            ),
            "font-hack-nerd-font"
        );
    }

    #[test]
    fn parse_entry_expands_defaults_and_dash_fields() {
        let entry = parse_entry("pkg-a|pkg|-|-|-", Some("apt"));

        assert_eq!(entry.name, "pkg-a");
        assert_eq!(entry.method, "pkg");
        assert_eq!(entry.cmd, "pkg-a");
        assert_eq!(entry.aliases, "");
        assert_eq!(entry.filter, "");
    }

    #[test]
    fn parse_entry_resolves_platform_qualified_command() {
        let apt = parse_entry("bat|pkg|apt:batcat", Some("apt"));
        let brew = parse_entry("bat|pkg|apt:batcat", Some("brew"));

        assert_eq!(apt.cmd, "batcat");
        assert_eq!(brew.cmd, "bat");
    }

    #[test]
    fn parse_entry_preserves_github_release_command_and_filter() {
        let entry = parse_entry(
            "neovim/neovim|github:release|nvim|-|os:linux,os:macos",
            None,
        );

        assert_eq!(entry.name, "neovim/neovim");
        assert_eq!(entry.cmd, "nvim");
        assert_eq!(entry.filter, "os:linux,os:macos");
    }

    #[test]
    fn parse_entry_canonicalizes_github_git_suffix() {
        let repo = parse_entry("cgraf78/ds.git|github:repo", None);
        let auto = parse_entry("owner/auto.git|github|auto", None);
        let release = parse_entry("owner/tool.git|github:release|tool", None);

        assert_eq!(repo.name, "cgraf78/ds");
        assert_eq!(repo.cmd, "ds");
        assert_eq!(auto.name, "owner/auto");
        assert_eq!(auto.cmd, "auto");
        assert_eq!(release.name, "owner/tool");
        assert_eq!(release.cmd, "tool");
    }

    #[test]
    fn parse_config_line_ignores_comments_and_blanks() {
        assert_eq!(parse_config_line("# comment"), None);
        assert_eq!(parse_config_line("   # comment"), None);
        assert_eq!(parse_config_line("   "), None);
    }

    #[test]
    fn parse_config_line_strips_trailing_inline_hash_comments() {
        // The shell-style convention of a `#` token introducing a trailing
        // comment must produce the same parse as the comment-free form.
        // Otherwise the comment would be silently materialized as the `cmd`
        // field (or, worse, as the `method` field), producing a wrong entry
        // that no `valid_dep_name` style check would later catch.
        assert_eq!(
            parse_config_line("tool  pkg  # this is a comment").as_deref(),
            Some("tool|pkg")
        );
        assert_eq!(
            parse_config_line("tool  pkg  apt:tool-pkg  # alias hint").as_deref(),
            Some("tool|pkg|apt:tool-pkg")
        );
        // A comment whose `#` is immediately followed by content still wins:
        // it is a whitespace-separated `#`-prefixed token, no spaces required
        // between the `#` and the text.
        assert_eq!(
            parse_config_line("tool  pkg  #no-space-after-hash").as_deref(),
            Some("tool|pkg")
        );
    }

    #[test]
    fn parse_config_line_keeps_hash_inside_unquoted_token() {
        // A `#` that is part of a non-whitespace-separated token is not a
        // comment — there is no current method whose field value legitimately
        // starts with `#`, so the only safe interpretation of a mid-token
        // `#` is "do not split here". This guards against accidentally
        // truncating an alias value such as `apt:foo#bar` (hypothetical).
        assert_eq!(
            parse_config_line("tool#extra  pkg").as_deref(),
            Some("tool#extra|pkg")
        );
        assert_eq!(
            parse_config_line("tool  pkg  apt:foo#bar").as_deref(),
            Some("tool|pkg|apt:foo#bar")
        );
    }

    #[test]
    fn parse_config_line_splits_whitespace_into_raw_entry() {
        assert_eq!(
            parse_config_line("fd  pkg  apt:fdfind  apt:fd-find,dnf:fd-find  os:linux").as_deref(),
            Some("fd|pkg|apt:fdfind|apt:fd-find,dnf:fd-find|os:linux")
        );
    }

    #[test]
    fn parse_config_line_canonicalizes_github_names_before_storage() {
        assert_eq!(
            parse_config_line("cgraf78/ds.git github:repo").as_deref(),
            Some("cgraf78/ds|github:repo")
        );
        assert_eq!(
            parse_config_line("owner/auto.git github").as_deref(),
            Some("owner/auto|github")
        );
        assert_eq!(
            parse_config_line("tool.git pkg").as_deref(),
            Some("tool.git|pkg")
        );
    }

    #[test]
    fn parse_config_texts_merges_and_sorts_entries_by_name() {
        let entries = parse_config_texts([
            "tool-a  pkg\ntool-b  pkg\n",
            "owner/tool-c  github:release\n",
            "tool-d  custom\n",
        ]);

        assert_eq!(
            entries,
            vec![
                "owner/tool-c|github:release",
                "tool-a|pkg",
                "tool-b|pkg",
                "tool-d|custom"
            ]
        );
    }

    #[test]
    fn parse_config_texts_deduplicates_repeats_with_last_occurrence_winning() {
        // `manifest::get` is last-wins, so loading the same dep twice from
        // two `.conf` files must collapse to one entry — the later one.
        // Otherwise `update` would process the dep twice and run install
        // and post-hook side effects twice, with the user seeing two rows
        // in `shdeps list`.
        let entries = parse_config_texts([
            "tool-a  pkg\ntool-b  pkg\n",
            "tool-a  cargo\n", // same name, different method: later wins
        ]);
        assert_eq!(entries, vec!["tool-a|cargo", "tool-b|pkg"]);
    }

    #[test]
    fn parse_config_texts_deduplicates_repeats_within_one_file() {
        // Duplicates inside one file also collapse — the last definition
        // wins, mirroring the cross-file behavior so a user can override a
        // shared template by re-declaring later in the same file.
        let entries = parse_config_texts(["tool-a  pkg\ntool-b  pkg\ntool-a  cargo\n"]);
        assert_eq!(entries, vec!["tool-a|cargo", "tool-b|pkg"]);
    }

    #[test]
    fn load_dir_deduplicates_across_lexically_ordered_conf_files() {
        // Files are loaded in lex order, and the dedupe is last-wins, so a
        // later-named `*.conf` file is the authoritative source for any
        // dep name it redeclares. This lets a user drop in an overrides
        // file with a high prefix (e.g., `99-overrides.conf`) and have it
        // win without touching the upstream templates.
        let dir = temp_dir("dedupe-multi");
        write(&dir.join("00-core.conf"), "tool-a  pkg\ntool-b  pkg\n");
        write(&dir.join("99-overrides.conf"), "tool-a  cargo\n");

        let entries = load_dir(&dir).unwrap();

        assert_eq!(entries, vec!["tool-a|cargo", "tool-b|pkg"]);
    }

    #[test]
    fn load_dir_reads_direct_conf_files_and_sorts_loaded_entries() {
        let dir = temp_dir("load-dir");
        write(
            &dir.join("b.conf"),
            "tool-b  pkg\nowner/tool.git github:repo\n",
        );
        write(&dir.join("a.conf"), "# comment\ntool-a  cargo\n");
        write(&dir.join("ignored.txt"), "ignored pkg\n");

        let entries = load_dir(&dir).unwrap();

        assert_eq!(
            entries,
            vec!["owner/tool|github:repo", "tool-a|cargo", "tool-b|pkg"]
        );
    }

    #[test]
    fn load_dir_treats_missing_directory_as_empty_config() {
        let dir = temp_dir("missing-dir");

        assert!(load_dir(&dir.join("missing")).unwrap().is_empty());
    }

    #[test]
    fn sort_entries_uses_case_insensitive_name_key() {
        let mut entries = vec![
            "tool-b|pkg".to_owned(),
            "Owner/tool-a|pkg".to_owned(),
            "tool-c|pkg".to_owned(),
        ];

        sort_entries(&mut entries);

        assert_eq!(entries[0], "Owner/tool-a|pkg");
    }

    #[test]
    fn sort_entries_has_deterministic_case_tie_breaker() {
        let mut entries = vec![
            "tool|pkg|b".to_owned(),
            "Tool|pkg".to_owned(),
            "tool|pkg|a".to_owned(),
        ];

        sort_entries(&mut entries);

        assert_eq!(entries, vec!["Tool|pkg", "tool|pkg|a", "tool|pkg|b"]);
    }

    #[test]
    fn valid_dep_name_rejects_path_traversal_and_unsafe_names() {
        assert!(valid_dep_name("cgraf78/ds"));
        assert!(valid_dep_name("jq"));
        assert!(!valid_dep_name(""));
        assert!(!valid_dep_name("/tmp/tool"));
        assert!(!valid_dep_name(".."));
        assert!(!valid_dep_name("../tool"));
        assert!(!valid_dep_name("tool/.."));
        assert!(!valid_dep_name("tool/../other"));
        assert!(!valid_dep_name("bad name"));
        assert!(!valid_dep_name("bad|name"));
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "shdeps-config-{name}-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, content: &str) {
        fs::write(path, content).unwrap();
    }
}
