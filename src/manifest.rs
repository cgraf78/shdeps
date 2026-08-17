//! Manifest state file support.
//!
//! The manifest is the handoff between installs, `list`/`check`, orphan
//! detection, prune, and method-transition cleanup. It intentionally stays
//! plain text so a broken machine can be debugged over SSH with `cat`, but all
//! Rust writes go through a temp file and atomic rename so partial rows are not
//! mistaken for valid install state.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::Result;
use crate::config::Entry;
use crate::state;

// Built-in install methods can complete in parallel, but manifest updates are
// read-modify-write operations against one plain-text file.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// One manifest row: `name|method|cmd|install_path`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ManifestEntry {
    /// Dependency name and manifest key.
    pub name: String,
    /// Install method that created the row.
    pub method: String,
    /// Command name associated with the dependency.
    pub cmd: String,
    /// Installed artifact path. Package-manager and custom rows may leave this empty.
    pub install_path: String,
}

impl ManifestEntry {
    /// Creates a manifest entry from its four row fields.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        method: impl Into<String>,
        cmd: impl Into<String>,
        install_path: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            method: method.into(),
            cmd: cmd.into(),
            install_path: install_path.into(),
        }
    }

    /// Parses a manifest row, ignoring blank lines.
    ///
    /// Tolerates a stray trailing `\r` on each field (`str::lines` already
    /// strips a `\r` that precedes the line-feed, but a `\r` at the end
    /// of the final field's content — e.g., the last line of a CRLF file
    /// without a final newline — would otherwise survive into the parsed
    /// `install_path` value and silently break path comparisons later).
    /// Field-level trim is the simplest place to enforce this without
    /// changing `parse(content)` callers.
    #[must_use]
    pub fn parse(line: &str) -> Option<Self> {
        if line.is_empty() {
            return None;
        }

        let mut fields = line.splitn(4, '|');
        let name = fields.next().unwrap_or_default().trim_end_matches('\r');
        if name.is_empty() {
            return None;
        }

        Some(Self::new(
            name,
            fields.next().unwrap_or_default().trim_end_matches('\r'),
            fields.next().unwrap_or_default().trim_end_matches('\r'),
            fields.next().unwrap_or_default().trim_end_matches('\r'),
        ))
    }

    /// Serializes the entry in the Bash-compatible line format.
    #[must_use]
    pub fn line(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.name, self.method, self.cmd, self.install_path
        )
    }

    fn same_payload(&self, method: &str, cmd: &str, install_path: &str) -> bool {
        self.method == method && self.cmd == cmd && self.install_path == install_path
    }
}

/// In-memory manifest preserving on-disk row order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    entries: Vec<ManifestEntry>,
}

impl Manifest {
    /// Parses manifest file content.
    ///
    /// Strips a leading UTF-8 BOM (`\u{FEFF}`) so a manifest produced by
    /// a BOM-emitting editor (some Windows tools default to this) parses
    /// the first record correctly. Without the strip, the BOM would
    /// remain glued to the first `name` field and silently fail every
    /// later `manifest.get(name)` lookup with a confusing "dep is
    /// untracked" symptom on Windows-edited or backup-restored manifests.
    #[must_use]
    pub fn parse(content: &str) -> Self {
        let content = content.strip_prefix('\u{FEFF}').unwrap_or(content);
        Self {
            entries: content.lines().filter_map(ManifestEntry::parse).collect(),
        }
    }

    /// Returns manifest rows in file order.
    #[must_use]
    pub fn entries(&self) -> &[ManifestEntry] {
        &self.entries
    }

    /// Returns one effective row per dependency using last-row-wins semantics.
    #[must_use]
    pub fn effective_entries(&self) -> Vec<&ManifestEntry> {
        let mut seen = BTreeSet::new();
        let mut effective = self
            .entries
            .iter()
            .rev()
            .filter(|entry| seen.insert(entry.name.as_str()))
            .collect::<Vec<_>>();
        effective.reverse();
        effective
    }

    /// Returns the effective entry for a dependency.
    ///
    /// Bash loads the manifest into an associative array, so later duplicate
    /// rows override earlier rows in memory. Keep that behavior for reads while
    /// still letting upsert repair duplicate rows back to one canonical entry.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ManifestEntry> {
        self.entries.iter().rev().find(|entry| entry.name == name)
    }

    /// Counts rows for a dependency name.
    #[must_use]
    pub fn count(&self, name: &str) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.name == name)
            .count()
    }

    /// Returns true if upserting this row would not change the file.
    #[must_use]
    pub fn is_current_unique(
        &self,
        name: &str,
        method: &str,
        cmd: &str,
        install_path: &str,
    ) -> bool {
        self.count(name) == 1
            && self
                .get(name)
                .is_some_and(|entry| entry.same_payload(method, cmd, install_path))
    }

    /// Replaces all rows for `entry.name` with one canonical row appended last.
    pub fn upsert(&mut self, entry: ManifestEntry) {
        self.entries.retain(|existing| existing.name != entry.name);
        self.entries.push(entry);
    }

    /// Removes all rows for `name`.
    pub fn remove(&mut self, name: &str) {
        self.entries.retain(|entry| entry.name != name);
    }

    /// Returns entries present in the manifest but absent from loaded config.
    #[must_use]
    pub fn orphans(&self, config: &[Entry]) -> Vec<ManifestEntry> {
        let configured = config
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<BTreeSet<_>>();

        self.effective_entries()
            .into_iter()
            .filter(|entry| !configured.contains(entry.name.as_str()))
            .cloned()
            .collect()
    }

    fn content(&self) -> String {
        if self.entries.is_empty() {
            String::new()
        } else {
            let mut content = self
                .entries
                .iter()
                .map(ManifestEntry::line)
                .collect::<Vec<_>>()
                .join("\n");
            content.push('\n');
            content
        }
    }
}

/// Returns the manifest path for a state directory.
#[must_use]
pub fn path(state_dir: &Path) -> PathBuf {
    state_dir.join("manifest")
}

/// Reads a manifest file. Missing files are empty manifests.
pub fn read(path: &Path) -> Result<Manifest> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(Manifest::parse(&content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Manifest::default()),
        Err(error) => Err(error.into()),
    }
}

/// Upserts one manifest row, preserving the file when the row is already unique.
pub fn upsert(path: &Path, entry: ManifestEntry) -> Result<()> {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut manifest = read(path)?;
    if manifest.is_current_unique(&entry.name, &entry.method, &entry.cmd, &entry.install_path) {
        // This is a performance invariant, not only an optimization. `pkg`
        // updates can touch every dependency on warm no-op runs, so preserving
        // the file avoids needless inode churn, filesystem writes, and cache
        // invalidation when nothing actually changed.
        return Ok(());
    }

    manifest.upsert(entry);
    state::write_atomic(path, &manifest.content())
}

/// Removes all rows for a manifest key. Missing files are a successful no-op.
pub fn remove(path: &Path, name: &str) -> Result<()> {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut manifest = read(path)?;
    if manifest.entries.is_empty() {
        return Ok(());
    }

    manifest.remove(name);
    state::write_atomic(path, &manifest.content())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::{Manifest, ManifestEntry, path, read, remove, upsert};
    use crate::config::parse_entry;

    #[test]
    fn parse_strips_utf8_bom_at_file_start() {
        // A manifest produced by a BOM-emitting editor (some Windows
        // tools default to this) would otherwise glue the BOM to the
        // first dep's `name` field, breaking every later
        // `manifest.get(name)` lookup with a confusing "dep is
        // untracked" symptom on Windows-edited / backup-restored files.
        let manifest = Manifest::parse("\u{FEFF}tool-a|pkg|tool-a|\n");

        assert_eq!(manifest.entries().len(), 1);
        assert_eq!(
            manifest.get("tool-a"),
            Some(&ManifestEntry::new("tool-a", "pkg", "tool-a", ""))
        );
    }

    #[test]
    fn parse_tolerates_trailing_carriage_returns_on_each_field() {
        // `str::lines` already strips `\r` that precedes a `\n`, but a
        // file with CRLF endings whose final line lacks a trailing
        // newline would leave a `\r` glued to the last field's value.
        // Field-level trim catches that case and keeps `manifest.get()`
        // lookups working on Windows-edited manifests.
        let crlf = Manifest::parse("tool-a|pkg|tool-a|.local/share\r\ntool-b|pkg|tool-b|\r");
        assert_eq!(crlf.entries().len(), 2);
        assert_eq!(
            crlf.get("tool-a"),
            Some(&ManifestEntry::new(
                "tool-a",
                "pkg",
                "tool-a",
                ".local/share"
            ))
        );
        assert_eq!(
            crlf.get("tool-b"),
            Some(&ManifestEntry::new("tool-b", "pkg", "tool-b", ""))
        );
    }

    #[test]
    fn parse_ignores_blank_lines_and_preserves_last_duplicate_as_effective() {
        let manifest = Manifest::parse(
            "tool-a|pkg|tool-a|\n\
             \n\
             tool-a|github:repo|tool-a|.local/share/tool-a\n",
        );

        assert_eq!(manifest.count("tool-a"), 2);
        assert_eq!(
            manifest.effective_entries(),
            vec![manifest.get("tool-a").unwrap()]
        );
        assert_eq!(
            manifest.get("tool-a"),
            Some(&ManifestEntry::new(
                "tool-a",
                "github:repo",
                "tool-a",
                ".local/share/tool-a"
            ))
        );
    }

    #[test]
    fn path_lives_in_state_dir() {
        assert_eq!(
            path(PathBuf::from("/tmp/state").as_path()),
            PathBuf::from("/tmp/state/manifest")
        );
    }

    #[test]
    fn upsert_creates_manifest_and_appends_rows() {
        let dir = temp_dir("upsert-creates");
        let manifest = path(&dir);

        upsert(&manifest, ManifestEntry::new("tool-a", "pkg", "tool-a", "")).unwrap();
        upsert(
            &manifest,
            ManifestEntry::new("tool-b", "github:release", "tb", ".local/bin/tb"),
        )
        .unwrap();

        let content = fs::read_to_string(&manifest).unwrap();
        assert!(content.contains("tool-a|pkg|tool-a|"));
        assert!(content.contains("tool-b|github:release|tb|.local/bin/tb"));
    }

    #[test]
    #[cfg(unix)]
    fn upsert_noop_preserves_manifest_inode() {
        use std::os::unix::fs::MetadataExt;

        let dir = temp_dir("upsert-noop");
        let manifest = path(&dir);
        let entry = ManifestEntry::new("tool-b", "github:release", "tb", ".local/bin/tb");

        upsert(&manifest, entry.clone()).unwrap();
        let before = fs::metadata(&manifest).unwrap().ino();
        upsert(&manifest, entry).unwrap();
        let after = fs::metadata(&manifest).unwrap().ino();

        assert_eq!(before, after);
    }

    #[test]
    fn upsert_repairs_duplicate_current_entries() {
        let dir = temp_dir("upsert-duplicates");
        let manifest = path(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            &manifest,
            "tool-b|github:release|tb|.local/bin/tb\n\
             tool-b|github:release|tb|.local/bin/tb\n",
        )
        .unwrap();

        upsert(
            &manifest,
            ManifestEntry::new("tool-b", "github:release", "tb", ".local/bin/tb"),
        )
        .unwrap();

        let parsed = read(&manifest).unwrap();
        assert_eq!(parsed.count("tool-b"), 1);
    }

    #[test]
    fn upsert_replaces_only_exact_slash_name() {
        let dir = temp_dir("slash-name");
        let manifest = path(&dir);

        upsert(
            &manifest,
            ManifestEntry::new("neovim/neovim", "github:release", "nvim", ".local/bin/nvim"),
        )
        .unwrap();
        upsert(
            &manifest,
            ManifestEntry::new("cgraf78/ds", "github:repo", "ds", ".local/share/cgraf78/ds"),
        )
        .unwrap();
        upsert(
            &manifest,
            ManifestEntry::new(
                "neovim/neovim",
                "github:release",
                "nvim",
                ".local/bin/nvim-updated",
            ),
        )
        .unwrap();

        let parsed = read(&manifest).unwrap();
        assert_eq!(parsed.count("neovim/neovim"), 1);
        assert_eq!(
            parsed
                .get("cgraf78/ds")
                .map(|entry| entry.install_path.as_str()),
            Some(".local/share/cgraf78/ds")
        );
        assert_eq!(
            parsed
                .get("neovim/neovim")
                .map(|entry| entry.install_path.as_str()),
            Some(".local/bin/nvim-updated")
        );
    }

    #[test]
    fn remove_deletes_entry_and_keeps_others() {
        let dir = temp_dir("remove");
        let manifest = path(&dir);

        upsert(&manifest, ManifestEntry::new("tool-a", "pkg", "tool-a", "")).unwrap();
        upsert(
            &manifest,
            ManifestEntry::new("tool-b", "github:release", "tb", ".local/bin/tb"),
        )
        .unwrap();
        remove(&manifest, "tool-a").unwrap();
        remove(&dir.join("missing-manifest"), "nothing").unwrap();

        let parsed = read(&manifest).unwrap();
        assert!(parsed.get("tool-a").is_none());
        assert!(parsed.get("tool-b").is_some());
    }

    #[test]
    fn orphan_detection_uses_config_names_regardless_of_platform_filter() {
        let manifest = Manifest::parse(
            "dep-a|pkg|dep-a|\n\
             owner/dep-b|github:release|dep-b|.local/bin/dep-b\n\
             owner/dep-c|github:repo|dep-c|.local/share/dep-c\n",
        );
        let config = [
            parse_entry("dep-a|pkg", None),
            parse_entry("owner/dep-b|github:release|dep-b|-|os:never", None),
        ];

        let orphans = manifest.orphans(&config);

        assert_eq!(
            orphans,
            vec![ManifestEntry::new(
                "owner/dep-c",
                "github:repo",
                "dep-c",
                ".local/share/dep-c"
            )]
        );
    }

    fn temp_dir(name: &str) -> PathBuf {
        crate::test_support::temp_dir(&format!("shdeps-manifest-{name}"))
    }
}
