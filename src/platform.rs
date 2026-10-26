//! Platform, host, package-manager, account, and combined filter matching.
//!
//! `shdeps` uses these predicates to decide whether a dependency applies to a
//! machine before any install work starts. The rules intentionally mirror the
//! Bash reference, including asymmetric case handling: platform specs are
//! compared exactly, while host specs are folded to lowercase.

/// Runtime identity used by dependency filters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeEnv {
    platform: String,
    host: String,
    package_manager: String,
    android: bool,
    effective_user: Option<String>,
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
            package_manager: String::new(),
            android: false,
            effective_user: None,
        }
    }

    /// Records the detected package manager used by `mgr:` filters.
    #[must_use]
    pub fn with_package_manager(mut self, package_manager: impl Into<String>) -> Self {
        self.package_manager = package_manager.into().to_lowercase();
        self
    }

    /// Records whether the Linux kernel is hosting an Android/Bionic userspace.
    #[must_use]
    pub fn with_android(mut self, android: bool) -> Self {
        self.android = android;
        self
    }

    /// Records the account resolved from the process's effective UID.
    #[must_use]
    pub fn with_effective_user(mut self, effective_user: Option<String>) -> Self {
        self.effective_user = effective_user;
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

    /// Returns the effective account, or `None` when the OS cannot resolve it.
    #[must_use]
    pub fn effective_user(&self) -> Option<&str> {
        self.effective_user.as_deref()
    }

    /// Returns whether release assets must target Android/Bionic.
    #[must_use]
    pub fn is_android(&self) -> bool {
        self.android
    }

    /// Returns the user-facing package-manager identity.
    ///
    /// Termux uses APT-compatible commands internally, but config already uses
    /// `android:` for its package overrides. Exposing the same durable identity
    /// to filters keeps policy independent of that implementation detail.
    #[must_use]
    pub fn package_manager(&self) -> &str {
        if self.android {
            "android"
        } else {
            &self.package_manager
        }
    }
}

/// Result of evaluating a combined `os:`/`host:`/`mgr:`/`users=` filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterMatch {
    /// The filter applies to the current runtime identity.
    Match,
    /// The `os:` portion rejected the current platform.
    PlatformMismatch,
    /// The `host:` portion rejected the current host.
    HostMismatch,
    /// The `mgr:` portion rejected the active package manager.
    ManagerMismatch,
    /// The `users=` portion rejected or could not verify the effective account.
    UserMismatch,
}

impl FilterMatch {
    /// Returns the Bash-compatible predicate exit code.
    #[must_use]
    pub const fn exit_code(self) -> i32 {
        match self {
            Self::Match => 0,
            Self::PlatformMismatch => 1,
            Self::HostMismatch => 2,
            Self::ManagerMismatch => 3,
            Self::UserMismatch => 4,
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
    let mut identities = vec![env.platform()];
    if env.is_android() {
        identities.push("android");
    }
    match_specs(spec, &identities, CaseMode::Exact)
}

/// Returns whether a comma-separated host spec matches the runtime.
#[must_use]
pub fn host_match(spec: &str, env: &RuntimeEnv) -> bool {
    match_specs(spec, &[env.host()], CaseMode::Lowercase)
}

/// Returns whether a comma-separated package-manager spec matches the runtime.
#[must_use]
pub fn manager_match(spec: &str, env: &RuntimeEnv) -> bool {
    match_specs(spec, &[env.package_manager()], CaseMode::Lowercase)
}

/// Returns whether a combined `os:`/`host:`/`mgr:`/`users=` filter matches the runtime.
#[must_use]
pub fn filter_match(spec: &str, env: &RuntimeEnv) -> FilterMatch {
    let specs = FilterSpecs::parse(spec);
    if specs.platform_rejects(env) {
        return FilterMatch::PlatformMismatch;
    }
    if specs.host_rejects(env) {
        return FilterMatch::HostMismatch;
    }
    if specs.manager_rejects(env) {
        return FilterMatch::ManagerMismatch;
    }
    if specs.user_rejects(env) {
        return FilterMatch::UserMismatch;
    }

    FilterMatch::Match
}

/// Returns whether `spec` rejects this runtime on a stable, known identity.
///
/// Destructive callers (prune, run unattended by cron) must not act on
/// `filter_match` alone, because some identities are probes that can be
/// wrong transiently:
///
/// - `host:` never proves exclusion. `hostname` yields an empty value when
///   the probe fails, and on macOS without a fixed `HostName` it follows the
///   network (DHCP or reverse DNS), so a non-empty value can still be a
///   transient misidentification.
/// - `mgr:` proves exclusion only once a package manager was detected; an
///   empty value means detection failed (for example a cron PATH without
///   brew), not that this machine uses no manager. Detection is otherwise a
///   deterministic PATH lookup.
/// - `os:` always proves exclusion: the platform always resolves (`uname`
///   falls back to the compile-time OS).
/// - `users=` proves exclusion only after the effective UID resolves to an
///   account. Missing identity blocks installation but must not prune data.
///
/// Unlike `filter_match`, every dimension is checked so an unproven host
/// mismatch cannot mask a known package-manager mismatch.
#[must_use]
pub fn provably_excludes(spec: &str, env: &RuntimeEnv) -> bool {
    let specs = FilterSpecs::parse(spec);
    specs.platform_rejects(env)
        || (!env.package_manager().is_empty() && specs.manager_rejects(env))
        || specs.user_mismatch_proven(env)
}

/// Per-dimension specs split out of one combined filter.
#[derive(Default)]
struct FilterSpecs {
    platform: String,
    host: String,
    manager: String,
    users: String,
    users_present: bool,
    users_invalid: bool,
}

impl FilterSpecs {
    fn parse(spec: &str) -> Self {
        let mut specs = Self::default();
        for token in spec.split(',').filter(|token| !token.is_empty()) {
            if let Some(value) = token.strip_prefix("os:") {
                append_spec(&mut specs.platform, value);
            } else if let Some(value) = token.strip_prefix("host:") {
                append_spec(&mut specs.host, value);
            } else if let Some(value) = token.strip_prefix("mgr:") {
                append_spec(&mut specs.manager, value);
            } else if let Some(value) = token.strip_prefix("users=") {
                specs.users_present = true;
                specs.users_invalid |= value.is_empty();
                append_spec(&mut specs.users, value);
            }
        }
        specs
    }

    fn platform_rejects(&self, env: &RuntimeEnv) -> bool {
        !self.platform.is_empty() && !platform_match(&self.platform, env)
    }

    fn host_rejects(&self, env: &RuntimeEnv) -> bool {
        !self.host.is_empty() && !host_match(&self.host, env)
    }

    fn manager_rejects(&self, env: &RuntimeEnv) -> bool {
        !self.manager.is_empty() && !manager_match(&self.manager, env)
    }

    fn user_rejects(&self, env: &RuntimeEnv) -> bool {
        self.users_present
            && (self.users_invalid || !users_match(&self.users, env.effective_user()))
    }

    fn user_mismatch_proven(&self, env: &RuntimeEnv) -> bool {
        self.users_present
            && !self.users_invalid
            && valid_users_spec(&self.users)
            && env.effective_user().is_some()
            && !users_match(&self.users, env.effective_user())
    }
}

/// Refuse malformed names even in negated-only lists; a typo must not turn a
/// root-only declaration into an allow-everyone rule.
fn users_match(spec: &str, effective_user: Option<&str>) -> bool {
    let Some(effective_user) = effective_user else {
        return false;
    };
    valid_users_spec(spec) && match_specs(spec, &[effective_user], CaseMode::Exact)
}

fn valid_users_spec(spec: &str) -> bool {
    spec.split(',').all(|item| {
        let name = item.strip_prefix('!').unwrap_or(item);
        !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    })
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

fn match_specs(spec: &str, current: &[&str], case_mode: CaseMode) -> bool {
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

    if has_exclude
        && current
            .iter()
            .any(|current| items.iter().any(|item| item == &format!("!{current}")))
    {
        return false;
    }

    if has_include {
        current
            .iter()
            .any(|current| items.iter().any(|item| item == current))
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
        provably_excludes,
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
    fn android_runtime_matches_both_linux_and_android_filters() {
        let env = RuntimeEnv::new("linux", "phone").with_android(true);

        assert!(platform_match("linux", &env));
        assert!(platform_match("android", &env));
        assert!(platform_match("!macos", &env));
        assert!(!platform_match("!android", &env));
        assert!(!platform_match("!linux", &env));
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
    fn users_filter_fails_closed_without_a_trusted_account() {
        let env = RuntimeEnv::new("linux", "nas");

        assert_ne!(filter_match("users=root", &env), FilterMatch::Match);
        assert_ne!(filter_match("users=!root", &env), FilterMatch::Match);
        assert!(!provably_excludes("users=root", &env));
        assert!(!provably_excludes("users=!root", &env));
    }

    #[test]
    fn users_filter_matches_effective_account_name() {
        let root = RuntimeEnv::new("linux", "nas").with_effective_user(Some("root".to_owned()));
        let operator =
            RuntimeEnv::new("linux", "nas").with_effective_user(Some("operator".to_owned()));

        assert_eq!(filter_match("users=root", &root), FilterMatch::Match);
        assert_eq!(
            filter_match("users=root", &operator),
            FilterMatch::UserMismatch
        );
        assert_eq!(
            filter_match("users=!root", &root),
            FilterMatch::UserMismatch
        );
        assert_eq!(filter_match("users=!root", &operator), FilterMatch::Match);
        assert_eq!(
            filter_match("users=root,users=operator", &operator),
            FilterMatch::Match
        );
        assert_eq!(
            filter_match("users=root,users=operator,users=!operator", &operator),
            FilterMatch::UserMismatch
        );
    }

    #[test]
    fn users_filter_rejects_empty_or_malformed_names() {
        let root = RuntimeEnv::new("linux", "nas").with_effective_user(Some("root".to_owned()));

        for spec in ["users=", "users=!", "users=root,users=", "users=roo/t"] {
            assert_eq!(
                filter_match(spec, &root),
                FilterMatch::UserMismatch,
                "{spec}"
            );
            assert!(!provably_excludes(spec, &root), "{spec}");
        }
        assert_eq!(
            filter_match("users=,users=root", &root),
            FilterMatch::UserMismatch
        );
        assert!(!provably_excludes("users=,users=root", &root));
    }

    #[test]
    fn users_filter_is_proven_only_for_a_known_account() {
        let root = RuntimeEnv::new("linux", "nas").with_effective_user(Some("root".to_owned()));

        assert!(!provably_excludes("users=root", &root));
        assert!(provably_excludes("users=operator", &root));
        assert!(provably_excludes("users=!root", &root));
        assert!(provably_excludes("host:other,users=operator", &root));
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
    fn manager_filter_supports_includes_excludes_and_unknown_fallbacks() {
        let pacman = RuntimeEnv::new("linux", "workstation").with_package_manager("pacman");
        let unknown = RuntimeEnv::new("linux", "workstation");

        assert_eq!(
            filter_match("mgr:brew,mgr:pacman", &pacman),
            FilterMatch::Match
        );
        assert_eq!(
            filter_match("mgr:apt,mgr:dnf", &pacman),
            FilterMatch::ManagerMismatch
        );
        assert_eq!(
            filter_match("mgr:!brew,mgr:!pacman", &pacman),
            FilterMatch::ManagerMismatch
        );
        assert_eq!(
            filter_match("mgr:!brew,mgr:!pacman", &unknown),
            FilterMatch::Match
        );
        assert_eq!(
            filter_match("mgr:brew,mgr:pacman", &unknown),
            FilterMatch::ManagerMismatch
        );
    }

    #[test]
    fn android_runtime_exposes_android_instead_of_underlying_apt_manager() {
        let env = RuntimeEnv::new("linux", "phone")
            .with_package_manager("apt")
            .with_android(true);

        assert_eq!(filter_match("mgr:android", &env), FilterMatch::Match);
        assert_eq!(filter_match("mgr:apt", &env), FilterMatch::ManagerMismatch);
        assert_eq!(
            filter_match("mgr:!brew,mgr:!pacman", &env),
            FilterMatch::Match
        );
    }

    #[test]
    fn filter_reports_manager_mismatch_after_platform_and_host() {
        let env = RuntimeEnv::new("linux", "nas").with_package_manager("apt");

        assert_eq!(
            filter_match("os:macos,host:missing,mgr:pacman", &env),
            FilterMatch::PlatformMismatch
        );
        assert_eq!(
            filter_match("os:linux,host:missing,mgr:pacman", &env),
            FilterMatch::HostMismatch
        );
        assert_eq!(
            filter_match("os:linux,host:nas,mgr:pacman", &env),
            FilterMatch::ManagerMismatch
        );
    }

    #[test]
    fn filter_result_exposes_bash_exit_codes() {
        assert_eq!(FilterMatch::Match.exit_code(), 0);
        assert_eq!(FilterMatch::PlatformMismatch.exit_code(), 1);
        assert_eq!(FilterMatch::HostMismatch.exit_code(), 2);
        assert_eq!(FilterMatch::ManagerMismatch.exit_code(), 3);
    }

    #[test]
    fn provably_excludes_only_on_a_known_identity_mismatch() {
        let env = RuntimeEnv::new("linux", "desk").with_package_manager("apt");

        assert!(!provably_excludes("", &env));
        assert!(!provably_excludes("os:linux,host:desk,mgr:apt", &env));
        assert!(provably_excludes("os:macos", &env));
        assert!(provably_excludes("mgr:brew", &env));
    }

    #[test]
    fn provably_excludes_never_trusts_the_host() {
        // Hostnames can be empty (failed probe) or transiently wrong (macOS
        // DHCP names), so a host mismatch alone never proves exclusion.
        let known = RuntimeEnv::new("linux", "desk").with_package_manager("apt");
        let unknown = RuntimeEnv::new("linux", "").with_package_manager("apt");

        assert!(!provably_excludes("host:nas", &known));
        assert!(!provably_excludes("host:!desk", &known));
        assert!(!provably_excludes("host:nas", &unknown));
        assert!(provably_excludes("os:macos,host:nas", &unknown));
    }

    #[test]
    fn provably_excludes_ignores_an_unknown_package_manager() {
        let unknown = RuntimeEnv::new("linux", "desk");

        assert!(!provably_excludes("mgr:brew", &unknown));
        assert!(!provably_excludes("mgr:brew,mgr:pacman", &unknown));
    }

    #[test]
    fn provably_excludes_checks_every_dimension() {
        // `filter_match` reports only the first mismatch; an unproven host
        // mismatch must not hide a known package-manager mismatch.
        let env = RuntimeEnv::new("linux", "").with_package_manager("apt");

        assert!(provably_excludes("host:nas,mgr:pacman", &env));
    }
}
