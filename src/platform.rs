//! Platform, host, and combined filter matching.
//!
//! `shdeps` uses these predicates to decide whether a dependency applies to a
//! machine before any install work starts. The rules intentionally mirror the
//! Bash reference, including asymmetric case handling: platform specs are
//! compared exactly, while host specs are folded to lowercase.

/// Runtime identity used by platform and host filters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeEnv {
    platform: String,
    host: String,
    android: bool,
}

impl RuntimeEnv {
    /// Creates a runtime identity from already-normalized values.
    ///
    /// Tests use this constructor to keep filter logic pure. Production code
    /// should construct the same values from `uname`, `/proc/version`, and
    /// `hostname` in one place so cache keys, filters, and diagnostics all
    /// agree on the machine identity.
    #[must_use]
    pub fn new(platform: impl Into<String>, host: impl Into<String>) -> Self {
        Self {
            platform: platform.into(),
            host: host.into().to_lowercase(),
            android: false,
        }
    }

    /// Records whether the Linux kernel is hosting an Android/Bionic userspace.
    #[must_use]
    pub fn with_android(mut self, android: bool) -> Self {
        self.android = android;
        self
    }

    /// Returns the normalized platform name.
    #[must_use]
    pub fn platform(&self) -> &str {
        &self.platform
    }

    /// Returns the normalized short host name.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Returns whether release assets must target Android/Bionic.
    #[must_use]
    pub fn is_android(&self) -> bool {
        self.android
    }
}

/// Result of evaluating a combined `os:`/`host:` filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterMatch {
    /// The filter applies to the current runtime identity.
    Match,
    /// The `os:` portion rejected the current platform.
    PlatformMismatch,
    /// The `host:` portion rejected the current host.
    HostMismatch,
}

impl FilterMatch {
    /// Returns the Bash-compatible predicate exit code.
    #[must_use]
    pub const fn exit_code(self) -> i32 {
        match self {
            Self::Match => 0,
            Self::PlatformMismatch => 1,
            Self::HostMismatch => 2,
        }
    }
}

/// Converts `uname -s` and `/proc/version` evidence into a shdeps platform.
///
/// WSL is detected before `darwin` normalization, matching the Bash reference.
/// The `/proc/version` text is passed in by the caller so unit tests do not
/// need host-specific fixtures.
#[must_use]
pub fn normalize_platform(uname: &str, proc_version: Option<&str>) -> String {
    if proc_version
        .map(|version| version.to_lowercase().contains("microsoft"))
        .unwrap_or(false)
    {
        return "wsl".to_owned();
    }

    let platform = uname.to_lowercase();
    if platform == "darwin" {
        "macos".to_owned()
    } else {
        platform
    }
}

/// Returns whether a comma-separated platform spec matches the runtime.
#[must_use]
pub fn platform_match(spec: &str, env: &RuntimeEnv) -> bool {
    match_spec(spec, env.platform(), CaseMode::Exact)
}

/// Returns whether a comma-separated host spec matches the runtime.
#[must_use]
pub fn host_match(spec: &str, env: &RuntimeEnv) -> bool {
    match_spec(spec, env.host(), CaseMode::Lowercase)
}

/// Returns whether a combined `os:`/`host:` filter matches the runtime.
#[must_use]
pub fn filter_match(spec: &str, env: &RuntimeEnv) -> FilterMatch {
    if spec.is_empty() {
        return FilterMatch::Match;
    }

    let mut platform_spec = String::new();
    let mut host_spec = String::new();

    for token in spec.split(',').filter(|token| !token.is_empty()) {
        if let Some(value) = token.strip_prefix("os:") {
            append_spec(&mut platform_spec, value);
        } else if let Some(value) = token.strip_prefix("host:") {
            append_spec(&mut host_spec, value);
        }
    }

    if !platform_spec.is_empty() && !platform_match(&platform_spec, env) {
        return FilterMatch::PlatformMismatch;
    }
    if !host_spec.is_empty() && !host_match(&host_spec, env) {
        return FilterMatch::HostMismatch;
    }

    FilterMatch::Match
}

fn append_spec(spec: &mut String, value: &str) {
    if !spec.is_empty() {
        spec.push(',');
    }
    spec.push_str(value);
}

#[derive(Debug, Clone, Copy)]
enum CaseMode {
    Exact,
    Lowercase,
}

fn match_spec(spec: &str, current: &str, case_mode: CaseMode) -> bool {
    if spec.is_empty() {
        return true;
    }

    let items = spec
        .split(',')
        .filter(|item| !item.is_empty())
        .map(|item| normalize_item(item, case_mode))
        .collect::<Vec<_>>();

    let has_include = items.iter().any(|item| !item.starts_with('!'));
    let has_exclude = items.iter().any(|item| item.starts_with('!'));

    if has_exclude && items.iter().any(|item| item == &format!("!{current}")) {
        return false;
    }

    if has_include {
        items.iter().any(|item| item == current)
    } else {
        true
    }
}

fn normalize_item(item: &str, case_mode: CaseMode) -> String {
    match case_mode {
        CaseMode::Exact => item.to_owned(),
        CaseMode::Lowercase => item.to_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FilterMatch, RuntimeEnv, filter_match, host_match, normalize_platform, platform_match,
    };

    #[test]
    fn normalizes_platform_names() {
        assert_eq!(normalize_platform("Linux", None), "linux");
        assert_eq!(normalize_platform("Darwin", None), "macos");
        assert_eq!(
            normalize_platform("Linux", Some("Linux version 5.15.0 Microsoft")),
            "wsl"
        );
    }

    #[test]
    fn platform_empty_spec_matches() {
        let env = RuntimeEnv::new("linux", "nas");

        assert!(platform_match("", &env));
    }

    #[test]
    fn platform_include_list_matches_only_listed_platforms() {
        let env = RuntimeEnv::new("linux", "nas");

        assert!(platform_match("linux", &env));
        assert!(platform_match("linux,freebsd", &env));
        assert!(!platform_match("freebsd,openbsd", &env));
    }

    #[test]
    fn platform_exclude_list_rejects_current_platform() {
        let env = RuntimeEnv::new("linux", "nas");

        assert!(!platform_match("!linux", &env));
        assert!(platform_match("!freebsd", &env));
        assert!(platform_match("!freebsd,!openbsd", &env));
    }

    #[test]
    fn platform_mixed_list_checks_excludes_before_includes() {
        let env = RuntimeEnv::new("linux", "nas");

        assert!(!platform_match("linux,!linux", &env));
        assert!(platform_match("linux,!freebsd", &env));
        assert!(!platform_match("freebsd,!openbsd", &env));
    }

    #[test]
    fn platform_match_is_case_sensitive_like_bash() {
        let env = RuntimeEnv::new("linux", "nas");

        assert!(!platform_match("LINUX", &env));
    }

    #[test]
    fn host_empty_spec_matches() {
        let env = RuntimeEnv::new("linux", "nas");

        assert!(host_match("", &env));
    }

    #[test]
    fn host_include_list_matches_only_listed_hosts() {
        let env = RuntimeEnv::new("linux", "nas");

        assert!(host_match("nas", &env));
        assert!(host_match("other,nas", &env));
        assert!(!host_match("host-a,host-b", &env));
    }

    #[test]
    fn host_exclude_list_rejects_current_host() {
        let env = RuntimeEnv::new("linux", "nas");

        assert!(!host_match("!nas", &env));
        assert!(host_match("!host-a", &env));
        assert!(host_match("!host-a,!host-b", &env));
    }

    #[test]
    fn host_exclude_list_preserves_empty_host_semantics() {
        let env = RuntimeEnv::new("linux", "");

        // Legacy Bash compared against the raw empty command-substitution
        // result when hostname probes failed. Specs generated from that same
        // failed probe can therefore be `hosts=!`, and they must still exclude
        // the current host after the Rust port.
        assert!(!host_match("!", &env));
        assert!(host_match("!host-a", &env));
    }

    #[test]
    fn host_mixed_list_checks_excludes_before_includes() {
        let env = RuntimeEnv::new("linux", "nas");

        assert!(!host_match("nas,!nas", &env));
        assert!(host_match("nas,!other", &env));
        assert!(!host_match("other,!host-a", &env));
    }

    #[test]
    fn host_match_is_case_insensitive_like_bash() {
        let env = RuntimeEnv::new("linux", "nas");

        assert!(host_match("NAS", &env));
        assert!(!host_match("!NAS", &env));
    }

    #[test]
    fn filter_empty_or_unknown_tokens_match() {
        let env = RuntimeEnv::new("linux", "nas");

        assert_eq!(filter_match("", &env), FilterMatch::Match);
        assert_eq!(filter_match("arch:x86_64", &env), FilterMatch::Match);
    }

    #[test]
    fn filter_combines_platform_and_host_specs() {
        let env = RuntimeEnv::new("linux", "nas");

        assert_eq!(filter_match("os:linux", &env), FilterMatch::Match);
        assert_eq!(filter_match("host:nas", &env), FilterMatch::Match);
        assert_eq!(filter_match("os:linux,host:nas", &env), FilterMatch::Match);
    }

    #[test]
    fn filter_reports_platform_mismatch_before_host_mismatch() {
        let env = RuntimeEnv::new("linux", "nas");

        assert_eq!(
            filter_match("os:macos,host:missing", &env),
            FilterMatch::PlatformMismatch
        );
        assert_eq!(
            filter_match("os:linux,host:missing", &env),
            FilterMatch::HostMismatch
        );
    }

    #[test]
    fn filter_supports_negated_platform_and_host_tokens() {
        let env = RuntimeEnv::new("linux", "nas");

        assert_eq!(filter_match("os:!macos", &env), FilterMatch::Match);
        assert_eq!(
            filter_match("os:!linux", &env),
            FilterMatch::PlatformMismatch
        );
        assert_eq!(filter_match("host:!other", &env), FilterMatch::Match);
        assert_eq!(filter_match("host:!nas", &env), FilterMatch::HostMismatch);
    }

    #[test]
    fn filter_result_exposes_bash_exit_codes() {
        assert_eq!(FilterMatch::Match.exit_code(), 0);
        assert_eq!(FilterMatch::PlatformMismatch.exit_code(), 1);
        assert_eq!(FilterMatch::HostMismatch.exit_code(), 2);
    }
}
