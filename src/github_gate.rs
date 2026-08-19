//! Per-run GitHub API circuit breaker.
//!
//! One rate-limited response means every further call from the same
//! credentials in the same run is guaranteed to fail until the quota window
//! resets. Without a breaker, a forced update spends dozens of doomed calls
//! rediscovering that fact - and on shared home/office egress IPs those
//! calls drain the *other* hosts' unauthenticated quota too. Wrapping the
//! transport keeps the policy in one place: resolver, prefetch, and per-dep
//! paths all inherit it without plumbing.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::http::{Client, HttpStatusError, is_github_api_url};

/// `Client` decorator that short-circuits GitHub API calls after a rate
/// limit response has been observed.
pub struct GatedClient<'a> {
    inner: &'a dyn Client,
    tripped: AtomicBool,
    saw_token: AtomicBool,
}

impl<'a> GatedClient<'a> {
    /// Wraps `inner` with the gate open.
    pub fn new(inner: &'a dyn Client) -> Self {
        Self {
            inner,
            tripped: AtomicBool::new(false),
            saw_token: AtomicBool::new(false),
        }
    }

    /// True once any API call observed a rate-limit status this run.
    pub fn tripped(&self) -> bool {
        self.tripped.load(Ordering::SeqCst)
    }

    /// True when at least one gated call carried a bearer token.
    pub fn saw_token(&self) -> bool {
        self.saw_token.load(Ordering::SeqCst)
    }

    /// One-line operator guidance when the gate tripped, `None` otherwise.
    ///
    /// The message is selected from the run's observed auth state, not by
    /// parsing any error text (machine semantics stay separate from prose).
    pub fn trip_warning(&self) -> Option<&'static str> {
        if !self.tripped() {
            return None;
        }
        Some(if self.saw_token() {
            "GitHub API rate limit exceeded; remaining GitHub checks used cached data. Retry after the quota window resets."
        } else {
            "GitHub API rate limit exceeded (unauthenticated calls share 60/hour per IP); remaining GitHub checks used cached data. Set GH_TOKEN, or SHDEPS_ALLOW_GH_AUTH_TOKEN=1 to allow gh CLI credentials."
        })
    }

    fn short_circuit() -> io::Error {
        io::Error::other(HttpStatusError::new(
            429,
            "skipped: GitHub API rate limit already observed this run",
        ))
    }

    fn observe(&self, url: &str, token: Option<&str>, result: &io::Result<Vec<u8>>) {
        if !is_github_api_url(url) {
            return;
        }
        let Err(error) = result else { return };
        let status = error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<HttpStatusError>())
            .map(HttpStatusError::status);
        if status.is_some_and(|status| rate_limited_status(status, token)) {
            self.tripped.store(true, Ordering::SeqCst);
        }
    }

    fn call(
        &self,
        url: &str,
        token: Option<&str>,
        send: impl FnOnce() -> io::Result<Vec<u8>>,
    ) -> io::Result<Vec<u8>> {
        if is_github_api_url(url) {
            if self.tripped() {
                return Err(Self::short_circuit());
            }
            if token.is_some() {
                self.saw_token.store(true, Ordering::SeqCst);
            }
        }
        let result = send();
        self.observe(url, token, &result);
        result
    }
}

fn rate_limited_status(status: u16, token: Option<&str>) -> bool {
    status == 429 || (status == 403 && token.is_none())
}

impl Client for GatedClient<'_> {
    fn get(&self, url: &str, token: Option<&str>) -> io::Result<Vec<u8>> {
        self.call(url, token, || self.inner.get(url, token))
    }

    fn get_metadata(&self, url: &str, token: Option<&str>) -> io::Result<Vec<u8>> {
        self.call(url, token, || self.inner.get_metadata(url, token))
    }

    fn get_github_asset(&self, url: &str, token: Option<&str>) -> io::Result<Vec<u8>> {
        // API asset 403s can also mean token scope/SSO/resource policy failures.
        // Do not let one download failure suppress unrelated metadata checks.
        let result = self.inner.get_github_asset(url, token);
        self.observe_asset(url, token, &result);
        result
    }

    fn redirect_location(&self, url: &str) -> io::Result<Option<String>> {
        // The latest-release probe is a public github.com route, not a REST
        // API call. It neither spends the gated quota nor carries a token, so
        // it must remain available even after the API circuit breaker trips.
        self.inner.redirect_location(url)
    }
}

impl GatedClient<'_> {
    fn observe_asset(&self, url: &str, token: Option<&str>, result: &io::Result<Vec<u8>>) {
        if !is_github_api_url(url) {
            return;
        }
        let Err(error) = result else { return };
        let status = error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<HttpStatusError>())
            .map(HttpStatusError::status);
        if status == Some(429) {
            if token.is_some() {
                self.saw_token.store(true, Ordering::SeqCst);
            }
            self.tripped.store(true, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::GatedClient;
    use crate::http::{Client, HttpStatusError};

    /// Inner fake that always rate-limits and counts real calls.
    struct RateLimitedInner {
        calls: AtomicUsize,
    }

    impl Client for RateLimitedInner {
        fn get(&self, _url: &str, _token: Option<&str>) -> io::Result<Vec<u8>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::other(HttpStatusError::new(403, "limited")))
        }
    }

    struct TooManyRequestsInner {
        calls: AtomicUsize,
    }

    struct RedirectInner {
        calls: AtomicUsize,
    }

    impl Client for RedirectInner {
        fn get(&self, _url: &str, _token: Option<&str>) -> io::Result<Vec<u8>> {
            panic!("public redirect forwarding must not issue a body GET");
        }

        fn redirect_location(&self, _url: &str) -> io::Result<Option<String>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Some(
                "https://github.com/owner/tool/releases/tag/v1.2.3".to_owned(),
            ))
        }
    }

    impl Client for TooManyRequestsInner {
        fn get(&self, _url: &str, _token: Option<&str>) -> io::Result<Vec<u8>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::other(HttpStatusError::new(429, "limited")))
        }

        fn get_github_asset(&self, _url: &str, _token: Option<&str>) -> io::Result<Vec<u8>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::other(HttpStatusError::new(429, "limited")))
        }
    }

    /// Inner fake for authenticated 403s that are not quota exhaustion.
    struct ForbiddenInner {
        calls: AtomicUsize,
    }

    impl Client for ForbiddenInner {
        fn get(&self, _url: &str, _token: Option<&str>) -> io::Result<Vec<u8>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::other(HttpStatusError::new(403, "forbidden")))
        }
    }

    struct OkInner {
        calls: AtomicUsize,
    }

    struct MetadataInner {
        calls: AtomicUsize,
    }

    impl Client for MetadataInner {
        fn get(&self, url: &str, _token: Option<&str>) -> io::Result<Vec<u8>> {
            panic!("metadata forwarding must not use the unbounded GET path for {url}");
        }

        fn get_metadata(&self, _url: &str, _token: Option<&str>) -> io::Result<Vec<u8>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(b"[]".to_vec())
        }
    }

    impl Client for OkInner {
        fn get(&self, _url: &str, _token: Option<&str>) -> io::Result<Vec<u8>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(b"[]".to_vec())
        }
    }

    const API_URL: &str = "https://api.github.com/repos/owner/tool/releases";
    const BROWSER_URL: &str = "https://github.com/owner/tool/releases/download/v1/t.tar.gz";

    #[test]
    fn gate_preserves_the_inner_metadata_transport() {
        let inner = MetadataInner {
            calls: AtomicUsize::new(0),
        };
        let gate = GatedClient::new(&inner);

        assert_eq!(gate.get_metadata(API_URL, Some("token")).unwrap(), b"[]");
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
        assert!(gate.saw_token());
    }

    #[test]
    fn gate_short_circuits_api_calls_after_first_rate_limit() {
        let inner = RateLimitedInner {
            calls: AtomicUsize::new(0),
        };
        let gate = GatedClient::new(&inner);

        assert!(gate.get(API_URL, None).is_err());
        assert!(gate.get(API_URL, None).is_err());
        assert!(gate.get(API_URL, None).is_err());

        assert_eq!(
            inner.calls.load(Ordering::SeqCst),
            1,
            "only the first call may hit the network"
        );
        assert!(gate.tripped());
    }

    #[test]
    fn short_circuit_errors_still_classify_as_rate_limited() {
        let inner = RateLimitedInner {
            calls: AtomicUsize::new(0),
        };
        let gate = GatedClient::new(&inner);
        let _ = gate.get(API_URL, None);

        let error = gate.get(API_URL, None).unwrap_err();
        let status = error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<HttpStatusError>())
            .map(HttpStatusError::status);
        assert_eq!(status, Some(429));
    }

    #[test]
    fn gate_ignores_non_api_hosts() {
        let inner = RateLimitedInner {
            calls: AtomicUsize::new(0),
        };
        let gate = GatedClient::new(&inner);
        let _ = gate.get(API_URL, None); // trip it

        let _ = gate.get(BROWSER_URL, None);
        assert_eq!(
            inner.calls.load(Ordering::SeqCst),
            2,
            "browser asset downloads must never be short-circuited"
        );
    }

    #[test]
    fn gate_forwards_public_redirect_probes_without_api_policy() {
        let inner = RedirectInner {
            calls: AtomicUsize::new(0),
        };
        let gate = GatedClient::new(&inner);

        assert_eq!(
            gate.redirect_location("https://github.com/owner/tool/releases/latest")
                .unwrap()
                .as_deref(),
            Some("https://github.com/owner/tool/releases/tag/v1.2.3")
        );
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
        assert!(!gate.tripped());
        assert!(!gate.saw_token());
    }

    #[test]
    fn gate_ignores_api_asset_download_failures() {
        let inner = RateLimitedInner {
            calls: AtomicUsize::new(0),
        };
        let gate = GatedClient::new(&inner);

        assert!(gate.get_github_asset(API_URL, Some("tok")).is_err());

        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
        assert!(!gate.tripped());
        assert!(gate.trip_warning().is_none());
    }

    #[test]
    fn api_asset_rate_limits_trip_gate() {
        let inner = TooManyRequestsInner {
            calls: AtomicUsize::new(0),
        };
        let gate = GatedClient::new(&inner);

        assert!(gate.get_github_asset(API_URL, Some("tok")).is_err());
        assert!(gate.get(API_URL, Some("tok")).is_err());

        assert_eq!(
            inner.calls.load(Ordering::SeqCst),
            1,
            "asset 429 should trip the gate before later metadata calls"
        );
        assert!(gate.tripped());
        assert!(!gate.trip_warning().unwrap().contains("unauthenticated"));
    }

    #[test]
    fn authenticated_forbidden_does_not_trip_gate() {
        let inner = ForbiddenInner {
            calls: AtomicUsize::new(0),
        };
        let gate = GatedClient::new(&inner);

        assert!(gate.get(API_URL, Some("tok")).is_err());
        assert!(gate.get(API_URL, Some("tok")).is_err());

        assert_eq!(inner.calls.load(Ordering::SeqCst), 2);
        assert!(!gate.tripped());
        assert!(gate.trip_warning().is_none());
    }

    #[test]
    fn short_circuited_token_does_not_change_warning_auth_state() {
        let inner = RateLimitedInner {
            calls: AtomicUsize::new(0),
        };
        let gate = GatedClient::new(&inner);

        let _ = gate.get(API_URL, None);
        let _ = gate.get(API_URL, Some("tok"));

        assert!(!gate.saw_token());
        assert!(gate.trip_warning().unwrap().contains("unauthenticated"));
    }

    #[test]
    fn gate_stays_open_for_successful_calls() {
        let inner = OkInner {
            calls: AtomicUsize::new(0),
        };
        let gate = GatedClient::new(&inner);
        assert!(gate.get(API_URL, Some("tok")).is_ok());
        assert!(gate.get(API_URL, Some("tok")).is_ok());
        assert_eq!(inner.calls.load(Ordering::SeqCst), 2);
        assert!(!gate.tripped());
        assert!(gate.trip_warning().is_none());
    }

    #[test]
    fn trip_warning_distinguishes_authenticated_runs() {
        let inner = RateLimitedInner {
            calls: AtomicUsize::new(0),
        };
        let unauth = GatedClient::new(&inner);
        let _ = unauth.get(API_URL, None);
        assert!(unauth.trip_warning().unwrap().contains("unauthenticated"));

        let inner = TooManyRequestsInner {
            calls: AtomicUsize::new(0),
        };
        let auth = GatedClient::new(&inner);
        let _ = auth.get(API_URL, Some("tok"));
        assert!(!auth.trip_warning().unwrap().contains("unauthenticated"));
    }
}
