//! GitHub release asset selection policy.
//!
//! Network fetching and JSON decoding belong to later GitHub client code. This
//! module takes already extracted asset URLs and applies the compatibility
//! policy: prefer standalone binaries, then tar archives, then zip archives
//! while matching OS, architecture, libc, and command-name conventions.

/// Target machine identity used for release asset matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    os: String,
    arch: String,
    libc: String,
}

impl Target {
    /// Creates a target from `uname`-style OS/architecture values and libc.
    #[must_use]
    pub fn new(os: impl Into<String>, arch: impl Into<String>, libc: impl Into<String>) -> Self {
        Self {
            os: os.into().to_lowercase(),
            arch: arch.into().to_lowercase(),
            libc: libc.into().to_lowercase(),
        }
    }
}

/// Selects the best release asset URL for the command and target.
#[must_use]
pub fn select<'a>(cmd: &str, urls: &'a [&'a str], target: &Target) -> Option<&'a str> {
    let os_patterns = os_patterns(&target.os, &target.arch);
    let arch_patterns = arch_patterns(&target.arch, &target.os);
    let cmd_lower = cmd.to_lowercase();

    // The pass order is compatibility-sensitive. Many projects publish both a
    // raw executable and archives for the same release; the raw binary is the
    // lightest install path and avoids extraction ambiguity. Tarballs beat zip
    // because most Unix-oriented release tooling puts permissions, man pages,
    // and completions there first. Zip remains a fallback for projects that
    // only publish cross-platform archives.
    for pass in [Pass::Plain, Pass::Tar, Pass::Zip] {
        let mut best: Option<(Score, &'a str)> = None;

        for url in urls {
            let filename = filename_lower(url);
            if !matches_any(&filename, &os_patterns) {
                continue;
            }
            let Some(kind) = install_kind_from_filename(&filename) else {
                continue;
            };
            if !pass.matches(kind) {
                continue;
            }

            let exact = exact_cmd_match(&filename, &cmd_lower, &os_patterns, &arch_patterns);
            if !matches_any(&filename, &arch_patterns) {
                continue;
            }

            let score = Score {
                exact: if exact { 0 } else { 1 },
                libc: libc_score(&filename, target),
            };
            if best.is_none_or(|(current, _)| score < current) {
                best = Some((score, *url));
            }
        }

        if let Some((_, result)) = best {
            return Some(result);
        }
    }

    None
}

/// Install behavior for a selected release asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AssetKind {
    Plain,
    Gz,
    Bz2,
    Xz,
    Zst,
    TarGz,
    TarBz2,
    TarZst,
    TarXz,
    Zip,
}

#[derive(Debug, Clone, Copy)]
enum Pass {
    Plain,
    Tar,
    Zip,
}

impl Pass {
    fn matches(self, kind: AssetKind) -> bool {
        match self {
            Self::Plain => matches!(
                kind,
                AssetKind::Plain | AssetKind::Gz | AssetKind::Bz2 | AssetKind::Xz | AssetKind::Zst
            ),
            Self::Tar => matches!(
                kind,
                AssetKind::TarGz | AssetKind::TarBz2 | AssetKind::TarZst | AssetKind::TarXz
            ),
            Self::Zip => kind == AssetKind::Zip,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Score {
    exact: u8,
    libc: u8,
}

const SKIP_SUFFIXES: &[&str] = &[
    ".sha256",
    ".sha256sum",
    ".sha256sums",
    ".sha512",
    ".sha512sum",
    ".sha512sums",
    ".sha",
    ".sha1",
    ".sha1sum",
    ".sha1sums",
    ".md5",
    ".md5sum",
    ".sig",
    ".asc",
    ".minisig",
    ".txt",
    ".json",
    ".jsonl",
    ".zsync",
    ".sigstore",
    ".proof",
    ".sbom",
    ".attestation",
    ".cosign",
    ".intoto",
    ".dsse",
    ".b3",
    ".pem",
    ".cert",
    ".crt",
    ".pkg.tar.gz",
    ".pkg.tar.bz2",
    ".pkg.tar.xz",
    ".pkg.tar.zst",
    ".dmg",
    ".pkg",
    ".apk",
    ".deb",
    ".rpm",
    ".msi",
    ".appimage",
    ".flatpak",
    ".mcpb",
    ".nupkg",
    ".whl",
    ".snap",
];

const SKIP_NAMES: &[&str] = &[
    "sha256sums",
    "sha512sums",
    "sha1sums",
    "md5sums",
    "checksums",
    "checksum",
    "digests",
    "digest",
];

const UNSUPPORTED_ARCHIVE_SUFFIXES: &[&str] = &[
    ".7z",
    ".rar",
    ".tar.lz4",
    ".tar.br",
    ".tar.lzma",
    ".tar.lz",
    ".tlz",
    ".lz4",
    ".br",
    ".lzma",
    ".lz",
    ".tar.z",
    ".z",
];

// Single-file compression formats such as `.gz`, `.bz2`, `.xz`, and `.zst`
// behave like standalone binary downloads that need decompression, while
// `.tar.*` and zip assets go through archive extraction with binary discovery.
#[must_use]
pub(crate) fn install_kind(url: &str) -> Option<AssetKind> {
    install_kind_from_filename(&filename_lower(url))
}

fn install_kind_from_filename(filename: &str) -> Option<AssetKind> {
    if should_skip(filename) || has_suffix(filename, UNSUPPORTED_ARCHIVE_SUFFIXES) {
        return None;
    }
    if filename.ends_with(".tar.gz") || filename.ends_with(".tgz") {
        return Some(AssetKind::TarGz);
    }
    if filename.ends_with(".tar.bz2") || filename.ends_with(".tbz") || filename.ends_with(".tbz2") {
        return Some(AssetKind::TarBz2);
    }
    if filename.ends_with(".tar.zst") || filename.ends_with(".tzst") {
        return Some(AssetKind::TarZst);
    }
    if filename.ends_with(".tar.xz") || filename.ends_with(".txz") {
        return Some(AssetKind::TarXz);
    }
    if filename.ends_with(".gz") {
        return Some(AssetKind::Gz);
    }
    if filename.ends_with(".bz2") {
        return Some(AssetKind::Bz2);
    }
    if filename.ends_with(".xz") {
        return Some(AssetKind::Xz);
    }
    if filename.ends_with(".zst") {
        return Some(AssetKind::Zst);
    }
    if filename.ends_with(".zip") {
        return Some(AssetKind::Zip);
    }
    Some(AssetKind::Plain)
}

fn should_skip(filename: &str) -> bool {
    has_suffix(filename, SKIP_SUFFIXES)
        || SKIP_NAMES.iter().any(|name| {
            filename == *name
                || filename.ends_with(&format!("-{name}"))
                || filename.ends_with(&format!("_{name}"))
                || filename.ends_with(&format!(".{name}"))
        })
}

fn filename_lower(url: &str) -> String {
    let basename = url
        .split_once('?')
        .map_or(url, |(path, _)| path)
        .rsplit('/')
        .next()
        .unwrap_or(url);
    basename.to_lowercase()
}

fn has_suffix(filename: &str, suffixes: &[&str]) -> bool {
    suffixes.iter().any(|suffix| filename.ends_with(suffix))
}

fn matches_any(url_lower: &str, patterns: &[String]) -> bool {
    patterns
        .iter()
        .any(|pattern| contains_token(url_lower, pattern))
}

fn contains_token(value: &str, token: &str) -> bool {
    value.match_indices(token).any(|(start, _)| {
        let before = value[..start].chars().next_back();
        let after = value[start + token.len()..].chars().next();
        before.is_none_or(is_boundary) && after.is_none_or(is_boundary)
    })
}

fn is_boundary(ch: char) -> bool {
    !ch.is_ascii_alphanumeric()
}

fn os_patterns(os: &str, arch: &str) -> Vec<String> {
    let mut patterns = vec![os.to_owned()];
    if os == "darwin" {
        patterns.extend(["macos", "apple", "osx", "mac"].map(str::to_owned));
    }
    if os == "linux" && matches!(arch, "x86_64" | "amd64") {
        patterns.push("linux64".to_owned());
    }
    patterns
}

fn arch_patterns(arch: &str, os: &str) -> Vec<String> {
    let mut patterns = vec![arch.to_owned()];
    match arch {
        "x86_64" => patterns.extend(["amd64", "x64", "x86-64"].map(str::to_owned)),
        "aarch64" => patterns.push("arm64".to_owned()),
        "amd64" => patterns.extend(["x86_64", "x64", "x86-64"].map(str::to_owned)),
        "arm64" => patterns.push("aarch64".to_owned()),
        _ => {}
    }
    if os == "darwin" && matches!(arch, "x86_64" | "amd64" | "aarch64" | "arm64") {
        patterns.extend(["universal", "universal2"].map(str::to_owned));
    }
    if os == "linux" && matches!(arch, "x86_64" | "amd64") {
        patterns.push("linux64".to_owned());
    }
    patterns
}

fn libc_score(filename: &str, target: &Target) -> u8 {
    if target.os != "linux" {
        return 0;
    }
    if libc_patterns(&target.libc)
        .iter()
        .any(|pattern| contains_token(filename, pattern))
    {
        return 0;
    }
    if ["musl", "gnu", "glibc"]
        .iter()
        .any(|pattern| contains_token(filename, pattern))
    {
        return 2;
    }
    1
}

fn libc_patterns(libc: &str) -> Vec<&'static str> {
    match libc {
        "gnu" => vec!["gnu", "glibc"],
        "musl" => vec!["musl"],
        _ => Vec::new(),
    }
}

fn exact_cmd_match(
    url_lower: &str,
    cmd_lower: &str,
    os_patterns: &[String],
    arch_patterns: &[String],
) -> bool {
    let filename = url_lower.rsplit('/').next().unwrap_or(url_lower);
    if filename == cmd_lower {
        // A bare command filename without OS/arch is not sufficient by itself
        // because the outer selector already requires platform and arch tokens
        // elsewhere in the URL. This branch only matters when those tokens are
        // present in a path component outside the basename.
        return true;
    }

    let Some(suffix) = filename.strip_prefix(cmd_lower) else {
        return false;
    };
    let Some(suffix) = suffix.strip_prefix(['-', '_', '.']) else {
        return false;
    };

    // `tool-extra-linux-amd64` should not beat `tool-linux-amd64` in a
    // multi-binary release. After the command prefix, require the suffix to
    // begin with an OS, architecture, or version marker before considering it
    // an exact command asset. Version markers cover common names like
    // `tool-v1.2.3-linux-amd64` and `tool.v1.2.3.linux-amd64`.
    os_patterns
        .iter()
        .chain(arch_patterns)
        .any(|token| suffix.starts_with(token))
        || starts_with_version(suffix)
}

fn starts_with_version(suffix: &str) -> bool {
    let suffix = suffix.strip_prefix('v').unwrap_or(suffix);
    suffix.as_bytes().first().is_some_and(u8::is_ascii_digit)
}

#[cfg(test)]
mod tests {
    use super::{AssetKind, install_kind};

    #[test]
    fn install_kind_accepts_supported_release_formats() {
        assert_eq!(
            install_kind("https://example.com/tool-linux-x86_64"),
            Some(AssetKind::Plain)
        );
        assert_eq!(
            install_kind("https://example.com/tool-linux-x86_64.gz"),
            Some(AssetKind::Gz)
        );
        assert_eq!(
            install_kind("https://example.com/tool-linux-x86_64.bz2"),
            Some(AssetKind::Bz2)
        );
        assert_eq!(
            install_kind("https://example.com/tool-linux-x86_64.xz"),
            Some(AssetKind::Xz)
        );
        assert_eq!(
            install_kind("https://example.com/tool-linux-x86_64.zst"),
            Some(AssetKind::Zst)
        );
        assert_eq!(
            install_kind("https://example.com/tool-linux-x86_64.tar.gz"),
            Some(AssetKind::TarGz)
        );
        assert_eq!(
            install_kind("https://example.com/tool-linux-x86_64.tgz"),
            Some(AssetKind::TarGz)
        );
        assert_eq!(
            install_kind("https://example.com/tool-linux-x86_64.tar.bz2"),
            Some(AssetKind::TarBz2)
        );
        assert_eq!(
            install_kind("https://example.com/tool-linux-x86_64.tbz"),
            Some(AssetKind::TarBz2)
        );
        assert_eq!(
            install_kind("https://example.com/tool-linux-x86_64.tbz2"),
            Some(AssetKind::TarBz2)
        );
        assert_eq!(
            install_kind("https://example.com/tool-linux-x86_64.tar.zst"),
            Some(AssetKind::TarZst)
        );
        assert_eq!(
            install_kind("https://example.com/tool-linux-x86_64.tzst"),
            Some(AssetKind::TarZst)
        );
        assert_eq!(
            install_kind("https://example.com/tool-linux-x86_64.tar.xz"),
            Some(AssetKind::TarXz)
        );
        assert_eq!(
            install_kind("https://example.com/tool-linux-x86_64.txz"),
            Some(AssetKind::TarXz)
        );
        assert_eq!(
            install_kind("https://example.com/tool-linux-x86_64.zip"),
            Some(AssetKind::Zip)
        );
    }

    #[test]
    fn install_kind_skips_metadata_packages_and_unsupported_archives() {
        for url in [
            "https://example.com/tool-linux-x86_64.sha256sum",
            "https://example.com/tool-linux-x86_64.sha512",
            "https://example.com/tool-linux-x86_64.sha",
            "https://example.com/tool-linux-x86_64.sha1",
            "https://example.com/tool-linux-x86_64-checksums",
            "https://example.com/tool-linux-x86_64.intoto.jsonl",
            "https://example.com/tool-linux-x86_64.minisig",
            "https://example.com/tool-linux-x86_64.pkg.tar.zst",
            "https://example.com/tool-linux-x86_64.deb",
            "https://example.com/tool-linux-x86_64.whl",
            "https://example.com/tool-linux-x86_64.appimage",
            "https://example.com/tool-linux-x86_64.7z",
            "https://example.com/tool-linux-x86_64.tar.lz4",
        ] {
            assert_eq!(install_kind(url), None, "{url}");
        }
    }

    #[test]
    fn install_kind_uses_filename_not_repo_path_or_query() {
        assert_eq!(
            install_kind(
                "https://github.com/owner/my.deb.tool/releases/download/v1/tool-linux-x86_64?download=1"
            ),
            Some(AssetKind::Plain)
        );
    }
}
