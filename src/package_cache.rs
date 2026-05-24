//! Package-check cache state.
//!
//! Package-manager availability checks dominate warm `shdeps update` time on
//! machines with larger configs. The cache is therefore proof-based rather than
//! timestamp-based: a hit means the same package manager, config, manifest,
//! package database, command paths, hook content, and dynamic repo gates are
//! still in effect. Keeping that proof centralized prevents future install
//! code from adding a new invalidation input in one path and forgetting it in
//! another.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use crate::state;
use crate::Result;

const CACHE_VERSION: &str = "shdeps-pkg-check-cache-v3";
const CACHE_FILE: &str = "pkg-check-cache-v3";

/// Inputs that decide whether a package-check cache entry is still valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inputs {
    /// Directory containing the cache file.
    pub state_dir: PathBuf,
    /// Number of package dependencies proven up to date by the full scan.
    pub count: usize,
    /// Active package manager identity, such as `apt`, `brew`, or `dnf`.
    pub pkg_mgr: String,
    /// Current shdeps platform name after Bash-compatible normalization.
    pub platform: String,
    /// Current host name used by `host:` filters.
    pub host: String,
    /// Config directory itself; directory metadata catches file add/remove.
    pub conf_dir: PathBuf,
    /// Config files whose contents affect active package dependencies.
    pub conf_files: Vec<PathBuf>,
    /// Manifest path. Missing manifests are a valid cache input.
    pub manifest_path: PathBuf,
    /// Dynamic repo override environment values that affect install decisions.
    pub env_overrides: Vec<(String, String)>,
    /// Package database signatures keyed by a manager-specific cache key.
    pub db_signatures: Vec<(String, String)>,
    /// Command lookup results for active package deps.
    pub commands: Vec<(String, String)>,
    /// Package hook paths keyed by dependency name.
    pub hooks: Vec<(String, PathBuf)>,
    /// `SHDEPS_FORCE`; true bypasses the cache.
    pub force: bool,
    /// `SHDEPS_REINSTALL`; true bypasses the cache.
    pub reinstall: bool,
    /// `SHDEPS_LOG_LEVEL`; verbose mode bypasses so diagnostics stay complete.
    pub log_level: u8,
}

/// Cache validation result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// The cache is valid and can account for `count` package deps.
    Hit {
        /// Number of package dependencies covered by the cached full scan.
        count: usize,
    },
    /// The cache is absent, disabled, incomplete, or stale.
    Miss {
        /// Human-readable invalidation reason for verbose diagnostics.
        reason: String,
    },
}

impl Status {
    /// Returns true when validation produced a cache hit.
    #[must_use]
    pub const fn is_hit(&self) -> bool {
        matches!(self, Self::Hit { .. })
    }
}

/// Returns the package-check cache path for a state directory.
#[must_use]
pub fn path(state_dir: &Path) -> PathBuf {
    state_dir.join(CACHE_FILE)
}

/// Validates the current package-check cache against `inputs`.
///
/// When a content hash still matches after only path metadata changed, this
/// refreshes the cache file with the new metadata. That mirrors the spec's
/// performance contract: warm checks should avoid hashing on the next run after
/// harmless metadata churn such as a checkout rewriting unchanged config files.
pub fn current(inputs: &Inputs) -> Result<Status> {
    if let Some(reason) = disabled_reason(inputs) {
        return Ok(Status::Miss {
            reason: reason.to_owned(),
        });
    }

    let cache_path = path(&inputs.state_dir);
    let content = match fs::read_to_string(&cache_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Status::Miss {
                reason: "missing cache".to_owned(),
            });
        }
        Err(error) => return Err(error.into()),
    };

    let cache = Cache::parse(&content);
    let (status, refresh) = validate(&cache, inputs)?;
    if matches!(status, Status::Hit { .. }) && refresh {
        let fresh = build_cache(inputs)?;
        if fresh.content() != content {
            state::write_atomic(&cache_path, &fresh.content())?;
        }
    }
    Ok(status)
}

/// Writes a fresh package-check cache.
pub fn write(inputs: &Inputs) -> Result<()> {
    if disabled_reason(inputs).is_some() {
        return Ok(());
    }

    let cache = build_cache(inputs)?;
    state::write_atomic(&path(&inputs.state_dir), &cache.content())
}

fn disabled_reason(inputs: &Inputs) -> Option<&'static str> {
    if inputs.force {
        Some("force enabled")
    } else if inputs.reinstall {
        Some("reinstall enabled")
    } else if inputs.log_level >= 2 {
        Some("verbose logging enabled")
    } else {
        None
    }
}

fn validate(cache: &Cache, inputs: &Inputs) -> Result<(Status, bool)> {
    if let Err(reason) = require_identity(cache, "version", CACHE_VERSION) {
        return Ok((miss(reason), false));
    }
    if let Err(reason) = require_identity(cache, "pkg_mgr", &inputs.pkg_mgr) {
        return Ok((miss(reason), false));
    }
    if let Err(reason) = require_identity(cache, "platform", &inputs.platform) {
        return Ok((miss(reason), false));
    }
    if let Err(reason) = require_identity(cache, "host", &inputs.host) {
        return Ok((miss(reason), false));
    }
    if let Err(reason) = require_identity(cache, "env", &env_fingerprint(&inputs.env_overrides)) {
        return Ok((miss(reason), false));
    }

    let count = match cache.one("count") {
        Some(value) => value.parse::<usize>().ok(),
        None => None,
    };
    let Some(count) = count else {
        return Ok((miss("missing or invalid count"), false));
    };

    let mut refresh = false;
    match require_path(cache, "confdir", "confdir", &inputs.conf_dir)? {
        Ok(check) => refresh |= check.needs_refresh(),
        Err(reason) => return Ok((miss(reason), false)),
    }
    match require_path(cache, "manifest", "manifest", &inputs.manifest_path)? {
        Ok(check) => refresh |= check.needs_refresh(),
        Err(reason) => return Ok((miss(reason), false)),
    }

    let conf_paths = inputs
        .conf_files
        .iter()
        .map(PathBuf::as_path)
        .collect::<BTreeSet<_>>();
    match require_path_set(cache, "conf", &conf_paths)? {
        Ok(check) => refresh |= check.needs_refresh(),
        Err(reason) => return Ok((miss(reason), false)),
    }

    let hook_paths = inputs
        .hooks
        .iter()
        .map(|(name, path)| (name.as_str(), path.as_path()))
        .collect::<BTreeMap<_, _>>();
    match require_named_path_set(cache, "hook", &hook_paths)? {
        Ok(check) => refresh |= check.needs_refresh(),
        Err(reason) => return Ok((miss(reason), false)),
    }

    if let Err(reason) = require_pair_set(cache, "db", &inputs.db_signatures) {
        return Ok((miss(reason), false));
    }
    if let Err(reason) = require_pair_set(cache, "cmd", &inputs.commands) {
        return Ok((miss(reason), false));
    }

    Ok((Status::Hit { count }, refresh))
}

fn require_identity(cache: &Cache, key: &str, expected: &str) -> std::result::Result<(), String> {
    match cache.one(key) {
        Some(actual) if actual == expected => Ok(()),
        Some(_) => Err(format!("{key} changed")),
        None => Err(format!("missing {key}")),
    }
}

fn require_path(
    cache: &Cache,
    key: &str,
    name: &str,
    path: &Path,
) -> Result<std::result::Result<PathCheck, String>> {
    let Some(record) = cache.records.iter().find(|record| {
        record.key == key && record.fields.first().is_some_and(|field| field == name)
    }) else {
        return Ok(Err(format!("missing {key}")));
    };

    match PathRecord::parse(record) {
        Some(cached) => cached.validate(path),
        None => Ok(Err(format!("invalid {key}"))),
    }
}

fn require_path_set(
    cache: &Cache,
    key: &str,
    expected: &BTreeSet<&Path>,
) -> Result<std::result::Result<PathCheck, String>> {
    let mut cached = BTreeSet::new();
    for record in cache.records.iter().filter(|record| record.key == key) {
        let Some(path) = record.fields.first() else {
            return Ok(Err(format!("invalid {key}")));
        };
        cached.insert(PathBuf::from(path));
    }

    let expected_bufs = expected
        .iter()
        .map(|path| path.to_path_buf())
        .collect::<BTreeSet<_>>();
    if cached != expected_bufs {
        return Ok(Err(format!("{key} set changed")));
    }

    let mut combined = PathCheck::Valid;
    for path in expected {
        match require_path(cache, key, &path.to_string_lossy(), path)? {
            Ok(check) => combined = combined.combine(check),
            Err(reason) => return Ok(Err(reason)),
        }
    }
    Ok(Ok(combined))
}

fn require_named_path_set(
    cache: &Cache,
    key: &str,
    expected: &BTreeMap<&str, &Path>,
) -> Result<std::result::Result<PathCheck, String>> {
    let mut cached = BTreeMap::new();
    for record in cache.records.iter().filter(|record| record.key == key) {
        let Some(name) = record.fields.first() else {
            return Ok(Err(format!("invalid {key}")));
        };
        let Some(path) = record.fields.get(1) else {
            return Ok(Err(format!("invalid {key}")));
        };
        cached.insert(name.as_str(), PathBuf::from(path));
    }

    let expected_bufs = expected
        .iter()
        .map(|(name, path)| (*name, (*path).to_path_buf()))
        .collect::<BTreeMap<_, _>>();
    if cached != expected_bufs {
        return Ok(Err(format!("{key} set changed")));
    }

    let mut combined = PathCheck::Valid;
    for (name, path) in expected {
        match require_path(cache, key, name, path)? {
            Ok(check) => combined = combined.combine(check),
            Err(reason) => return Ok(Err(reason)),
        }
    }
    Ok(Ok(combined))
}

fn require_pair_set(
    cache: &Cache,
    key: &str,
    expected: &[(String, String)],
) -> std::result::Result<(), String> {
    let cached = cache.pairs(key);
    let expected = sorted_pairs(expected);
    if cached == expected {
        Ok(())
    } else {
        Err(format!("{key} changed"))
    }
}

fn build_cache(inputs: &Inputs) -> Result<Cache> {
    let mut records = vec![
        Record::new("version", [CACHE_VERSION]),
        Record::new("count", [inputs.count.to_string()]),
        Record::new("pkg_mgr", [&inputs.pkg_mgr]),
        Record::new("platform", [&inputs.platform]),
        Record::new("host", [&inputs.host]),
        Record::new("env", [env_fingerprint(&inputs.env_overrides)]),
        PathRecord::snapshot("confdir", "confdir", &inputs.conf_dir)?.into_record(),
        PathRecord::snapshot("manifest", "manifest", &inputs.manifest_path)?.into_record(),
    ];

    let mut conf_files = inputs.conf_files.clone();
    conf_files.sort();
    for path in conf_files {
        records.push(PathRecord::snapshot("conf", &path.to_string_lossy(), &path)?.into_record());
    }

    for (key, value) in sorted_pairs(&inputs.db_signatures) {
        records.push(Record::new("db", [key, value]));
    }
    for (key, value) in sorted_pairs(&inputs.commands) {
        records.push(Record::new("cmd", [key, value]));
    }

    let mut hooks = inputs.hooks.clone();
    hooks.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    for (name, path) in hooks {
        records.push(
            PathRecord::snapshot("hook", &name, &path)?
                .with_named_path(path)
                .into_record(),
        );
    }

    Ok(Cache { records })
}

fn sorted_pairs(pairs: &[(String, String)]) -> Vec<(String, String)> {
    let mut pairs = pairs.to_vec();
    pairs.sort();
    pairs
}

fn env_fingerprint(overrides: &[(String, String)]) -> String {
    let mut pairs = sorted_pairs(overrides);
    pairs.retain(|(key, _)| {
        key == "SHDEPS_AUTO_EPEL" || key.starts_with("SHDEPS_") && key.ends_with("_REPO")
    });

    let mut hasher = StableHasher::default();
    for (key, value) in pairs {
        key.hash(&mut hasher);
        value.hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

fn miss(reason: impl Into<String>) -> Status {
    Status::Miss {
        reason: reason.into(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathCheck {
    Valid,
    Refresh,
}

impl PathCheck {
    fn needs_refresh(self) -> bool {
        matches!(self, Self::Refresh)
    }

    fn combine(self, other: Self) -> Self {
        if self.needs_refresh() || other.needs_refresh() {
            Self::Refresh
        } else {
            Self::Valid
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Cache {
    records: Vec<Record>,
}

impl Cache {
    fn parse(content: &str) -> Self {
        Self {
            records: content
                .lines()
                .filter_map(|line| {
                    let mut fields = line.split('\t');
                    let key = fields.next()?;
                    if key.is_empty() {
                        return None;
                    }
                    Some(Record {
                        key: key.to_owned(),
                        fields: fields.map(str::to_owned).collect(),
                    })
                })
                .collect(),
        }
    }

    fn content(&self) -> String {
        let mut content = self
            .records
            .iter()
            .map(Record::line)
            .collect::<Vec<_>>()
            .join("\n");
        content.push('\n');
        content
    }

    fn one(&self, key: &str) -> Option<&str> {
        self.records
            .iter()
            .find(|record| record.key == key)
            .and_then(|record| record.fields.first())
            .map(String::as_str)
    }

    fn pairs(&self, key: &str) -> Vec<(String, String)> {
        let mut pairs = self
            .records
            .iter()
            .filter(|record| record.key == key)
            .filter_map(|record| {
                Some((
                    record.fields.first()?.clone(),
                    record.fields.get(1)?.clone(),
                ))
            })
            .collect::<Vec<_>>();
        pairs.sort();
        pairs
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Record {
    key: String,
    fields: Vec<String>,
}

impl Record {
    fn new<const N: usize>(key: &str, fields: [impl ToString; N]) -> Self {
        Self {
            key: key.to_owned(),
            fields: fields.into_iter().map(|field| field.to_string()).collect(),
        }
    }

    fn line(&self) -> String {
        let mut fields = Vec::with_capacity(self.fields.len() + 1);
        fields.push(self.key.clone());
        fields.extend(self.fields.iter().cloned());
        fields.join("\t")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PathRecord {
    key: String,
    name: String,
    path: PathBuf,
    kind: String,
    mtime_ns: u128,
    size: u64,
    hash: String,
}

impl PathRecord {
    fn snapshot(key: &str, name: &str, path: &Path) -> Result<Self> {
        let probe = PathProbe::probe(path)?;
        Ok(Self {
            key: key.to_owned(),
            name: name.to_owned(),
            path: path.to_path_buf(),
            kind: probe.kind,
            mtime_ns: probe.mtime_ns,
            size: probe.size,
            hash: probe.hash,
        })
    }

    fn with_named_path(mut self, path: PathBuf) -> Self {
        self.path = path;
        self
    }

    fn parse(record: &Record) -> Option<Self> {
        let name = record.fields.first()?.to_owned();
        let path = PathBuf::from(record.fields.get(1)?);
        let kind = record.fields.get(2)?.to_owned();
        let mtime_ns = record.fields.get(3)?.parse().ok()?;
        let size = record.fields.get(4)?.parse().ok()?;
        let hash = record.fields.get(5)?.to_owned();
        Some(Self {
            key: record.key.clone(),
            name,
            path,
            kind,
            mtime_ns,
            size,
            hash,
        })
    }

    fn into_record(self) -> Record {
        Record::new(
            &self.key,
            [
                self.name,
                self.path.to_string_lossy().into_owned(),
                self.kind,
                self.mtime_ns.to_string(),
                self.size.to_string(),
                self.hash,
            ],
        )
    }

    fn validate(&self, expected_path: &Path) -> Result<std::result::Result<PathCheck, String>> {
        if self.path != expected_path {
            return Ok(Err("path changed".to_owned()));
        }

        let current = PathProbe::probe(expected_path)?;
        if self.kind != current.kind {
            return Ok(Err("path kind changed".to_owned()));
        }

        // The hot path stops at mtime+size. Hashing many config and package DB
        // files on every shell startup would make the cache more expensive than
        // the work it avoids. Only hash after metadata says something changed.
        if self.mtime_ns == current.mtime_ns && self.size == current.size {
            return Ok(Ok(PathCheck::Valid));
        }

        if self.hash == current.hash {
            // Metadata-only churn should not make the next warm run pay this
            // hash cost again. The caller refreshes cached metadata after all
            // proof obligations have passed.
            Ok(Ok(PathCheck::Refresh))
        } else {
            Ok(Err("path content changed".to_owned()))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PathProbe {
    kind: String,
    mtime_ns: u128,
    size: u64,
    hash: String,
}

impl PathProbe {
    fn probe(path: &Path) -> Result<Self> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                let kind = kind(&metadata);
                let mtime_ns = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|duration| duration.as_nanos())
                    .unwrap_or_default();
                let size = metadata.len();
                let hash = hash_path(path, &metadata)?;
                Ok(Self {
                    kind,
                    mtime_ns,
                    size,
                    hash,
                })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                kind: "missing".to_owned(),
                mtime_ns: 0,
                size: 0,
                hash: "missing".to_owned(),
            }),
            Err(error) => Err(error.into()),
        }
    }
}

fn kind(metadata: &fs::Metadata) -> String {
    let file_type = metadata.file_type();
    if file_type.is_file() {
        "file"
    } else if file_type.is_dir() {
        "dir"
    } else if file_type.is_symlink() {
        "symlink"
    } else {
        "other"
    }
    .to_owned()
}

fn hash_path(path: &Path, metadata: &fs::Metadata) -> Result<String> {
    let mut hasher = StableHasher::default();
    kind(metadata).hash(&mut hasher);

    if metadata.is_file() {
        let mut file = File::open(path)?;
        let mut buffer = [0; 8192];
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hasher.write(&buffer[..n]);
        }
    } else if metadata.is_dir() {
        // Directory mtimes are generally enough, but hashing direct child names
        // after metadata changes lets a harmless touch refresh the cache without
        // forcing a full package scan.
        let mut names = fs::read_dir(path)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        for name in names {
            name.hash(&mut hasher);
        }
    } else {
        metadata.len().hash(&mut hasher);
        #[cfg(unix)]
        metadata.ino().hash(&mut hasher);
    }

    Ok(format!("{:016x}", hasher.finish()))
}

#[derive(Default)]
struct StableHasher {
    state: u64,
}

impl Hasher for StableHasher {
    fn finish(&self) -> u64 {
        self.state
    }

    fn write(&mut self, bytes: &[u8]) {
        if self.state == 0 {
            self.state = 0xcbf29ce484222325;
        }
        for byte in bytes {
            self.state ^= u64::from(*byte);
            self.state = self.state.wrapping_mul(0x100000001b3);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::thread;
    use std::time::Duration;

    use super::{current, path, write, Inputs, Status};

    #[test]
    fn write_and_validate_cache_hit() {
        let fixture = Fixture::new("hit");
        fixture.write("conf/deps.conf", "cache-tool  pkg\n");
        fixture.write("manifest", "cache-tool|pkg|cache-tool|\n");
        fixture.write("hooks/cache-tool.sh", "post() { :; }\n");
        let inputs = fixture.inputs();

        write(&inputs).unwrap();

        assert_eq!(current(&inputs).unwrap(), Status::Hit { count: 1 });
    }

    #[test]
    fn force_reinstall_and_verbose_logging_bypass_cache() {
        let fixture = Fixture::new("gates");
        let mut inputs = fixture.inputs();

        inputs.force = true;
        assert!(!current(&inputs).unwrap().is_hit());
        inputs.force = false;
        inputs.reinstall = true;
        assert!(!current(&inputs).unwrap().is_hit());
        inputs.reinstall = false;
        inputs.log_level = 2;
        assert!(!current(&inputs).unwrap().is_hit());
    }

    #[test]
    fn incomplete_cache_misses() {
        let fixture = Fixture::new("incomplete");
        fs::create_dir_all(fixture.dir.join("state")).unwrap();
        fs::write(
            path(&fixture.inputs().state_dir),
            "version\tshdeps-pkg-check-cache-v3\n",
        )
        .unwrap();

        assert!(!current(&fixture.inputs()).unwrap().is_hit());
    }

    #[test]
    fn content_changes_invalidate_path_records() {
        let fixture = Fixture::new("content-change");
        fixture.write("conf/deps.conf", "cache-tool  pkg\n");
        let inputs = fixture.inputs();
        write(&inputs).unwrap();

        thread::sleep(Duration::from_millis(5));
        fixture.write("conf/deps.conf", "cache-tool  pkg\nother  pkg\n");

        assert!(!current(&inputs).unwrap().is_hit());
    }

    #[test]
    fn unchanged_content_with_metadata_churn_still_hits() {
        let fixture = Fixture::new("metadata-churn");
        fixture.write("conf/deps.conf", "cache-tool  pkg\n");
        let inputs = fixture.inputs();
        write(&inputs).unwrap();

        thread::sleep(Duration::from_millis(5));
        fixture.write("conf/deps.conf", "cache-tool  pkg\n");

        assert_eq!(current(&inputs).unwrap(), Status::Hit { count: 1 });
    }

    #[test]
    fn command_and_database_changes_invalidate_cache() {
        let fixture = Fixture::new("commands");
        let inputs = fixture.inputs();
        write(&inputs).unwrap();

        let mut changed_cmd = inputs.clone();
        changed_cmd.commands = vec![("cache-tool".to_owned(), "/new/path".to_owned())];
        assert!(!current(&changed_cmd).unwrap().is_hit());

        let mut changed_db = inputs;
        changed_db.db_signatures = vec![("apt".to_owned(), "new-db".to_owned())];
        assert!(!current(&changed_db).unwrap().is_hit());
    }

    #[test]
    fn env_override_changes_invalidate_cache() {
        let fixture = Fixture::new("env");
        let mut inputs = fixture.inputs();
        inputs.env_overrides = vec![("SHDEPS_AUTO_EPEL".to_owned(), "0".to_owned())];
        write(&inputs).unwrap();

        inputs.env_overrides = vec![("SHDEPS_AUTO_EPEL".to_owned(), "1".to_owned())];

        assert!(!current(&inputs).unwrap().is_hit());
    }

    #[test]
    fn hook_set_and_hook_content_changes_invalidate_cache() {
        let fixture = Fixture::new("hooks");
        fixture.write("hooks/cache-tool.sh", "post() { :; }\n");
        let inputs = fixture.inputs();
        write(&inputs).unwrap();

        let mut missing_hook = inputs.clone();
        missing_hook.hooks.clear();
        assert!(!current(&missing_hook).unwrap().is_hit());

        thread::sleep(Duration::from_millis(5));
        fixture.write("hooks/cache-tool.sh", "post() { printf changed; }\n");
        assert!(!current(&inputs).unwrap().is_hit());
    }

    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "shdeps-package-cache-{name}-{}-{}",
                std::process::id(),
                std::thread::current().name().unwrap_or("test")
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        fn inputs(&self) -> Inputs {
            Inputs {
                state_dir: self.dir.join("state"),
                count: 1,
                pkg_mgr: "apt".to_owned(),
                platform: "linux".to_owned(),
                host: "test-host".to_owned(),
                conf_dir: self.dir.join("conf"),
                conf_files: vec![self.dir.join("conf/deps.conf")],
                manifest_path: self.dir.join("manifest"),
                env_overrides: Vec::new(),
                db_signatures: vec![("apt".to_owned(), "db-v1".to_owned())],
                commands: vec![("cache-tool".to_owned(), "/mock/bin/cache-tool".to_owned())],
                hooks: vec![(
                    "cache-tool".to_owned(),
                    self.dir.join("hooks/cache-tool.sh"),
                )],
                force: false,
                reinstall: false,
                log_level: 1,
            }
        }

        fn write(&self, rel: impl AsRef<Path>, content: &str) {
            let path = self.dir.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
    }
}
