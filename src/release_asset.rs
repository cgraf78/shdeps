//! GitHub release asset selection policy.
//!
//! Network fetching and JSON decoding belong to later GitHub client code. This
//! module takes already extracted asset URLs and applies the compatibility
//! policy from the Bash reference: prefer standalone binaries, then tar
//! archives, then zip archives while matching OS, architecture, libc, and
//! command-name conventions.

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
    let os_patterns = os_patterns(&target.os);
    let arch_patterns = arch_patterns(&target.arch);
    let cmd_lower = cmd.to_lowercase();

    // The pass order is compatibility-sensitive. Many projects publish both a
    // raw executable and archives for the same release; the raw binary is the
    // lightest install path and avoids extraction ambiguity. Tarballs beat zip
    // because most Unix-oriented release tooling puts permissions, man pages,
    // and completions there first. Zip remains a fallback for projects that
    // only publish cross-platform archives.
    for pass in [Pass::Plain, Pass::Tar, Pass::Zip] {
        let mut exact_fallback = None;
        let mut other_match = None;
        let mut other_fallback = None;

        for url in urls {
            let url_lower = url.to_lowercase();
            if !matches_any(&url_lower, &os_patterns) {
                continue;
            }
            // Release pages are crowded with checksums, signatures, packages,
            // and installer formats. Treating those as install assets produces
            // confusing failures later, so filter them before preference
            // scoring instead of letting extension order decide accidentally.
            if should_skip(&url_lower) || !pass.matches(&url_lower) {
                continue;
            }

            let exact = exact_cmd_match(&url_lower, &cmd_lower, &os_patterns, &arch_patterns);
            if !matches_any(&url_lower, &arch_patterns) {
                continue;
            }

            if target.os == "linux" && url_lower.contains(&target.libc) {
                // GNU-vs-musl matters for runtime compatibility. If the asset
                // explicitly matches the host libc and the command name is
                // exact, return immediately. Otherwise remember it as a strong
                // fallback, because multi-binary repositories can still publish
                // a better exact command asset without a libc suffix.
                if exact {
                    return Some(*url);
                }
                if other_match.is_none() {
                    other_match = Some(*url);
                }
            } else if exact {
                if exact_fallback.is_none() {
                    exact_fallback = Some(*url);
                }
            } else if other_fallback.is_none() {
                other_fallback = Some(*url);
            }
        }

        if let Some(result) = exact_fallback.or(other_match).or(other_fallback) {
            return Some(result);
        }
    }

    None
}

#[derive(Debug, Clone, Copy)]
enum Pass {
    Plain,
    Tar,
    Zip,
}

impl Pass {
    fn matches(self, url_lower: &str) -> bool {
        match self {
            Self::Plain => {
                !TAR_EXTS.iter().any(|ext| url_lower.contains(ext)) && !url_lower.contains(".zip")
            }
            Self::Tar => TAR_EXTS.iter().any(|ext| url_lower.contains(ext)),
            Self::Zip => url_lower.ends_with(".zip"),
        }
    }
}

const SKIP_EXTS: &[&str] = &[
    ".sha256",
    ".sha512",
    ".md5",
    ".sig",
    ".asc",
    ".txt",
    ".json",
    ".zsync",
    ".sigstore",
    ".proof",
    ".sbom",
    ".b3",
    ".pem",
    ".dmg",
    ".pkg",
    ".apk",
    ".deb",
    ".rpm",
    ".msi",
    ".appimage",
    ".flatpak",
    ".mcpb",
];

// Single-file compression formats such as `.gz`, `.bz2`, and `.zst` are
// intentionally not listed here. The Bash implementation treats them like
// standalone binary downloads that need decompression, while `.tar.*` and zip
// assets go through archive extraction with binary discovery.
const TAR_EXTS: &[&str] = &[
    ".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst", ".tgz", ".tzst",
];

fn should_skip(url_lower: &str) -> bool {
    SKIP_EXTS.iter().any(|ext| url_lower.contains(ext))
}

fn matches_any(url_lower: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|pattern| url_lower.contains(pattern))
}

fn os_patterns(os: &str) -> Vec<String> {
    let mut patterns = vec![os.to_owned()];
    match os {
        "darwin" => patterns.extend(["macos", "apple", "osx"].map(str::to_owned)),
        "linux" => patterns.push("linux".to_owned()),
        _ => {}
    }
    patterns
}

fn arch_patterns(arch: &str) -> Vec<String> {
    let mut patterns = vec![arch.to_owned()];
    match arch {
        "x86_64" => patterns.extend(["amd64", "x64"].map(str::to_owned)),
        "aarch64" => patterns.push("arm64".to_owned()),
        "amd64" => patterns.extend(["x86_64", "x64"].map(str::to_owned)),
        "arm64" => patterns.push("aarch64".to_owned()),
        _ => {}
    }
    patterns
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
