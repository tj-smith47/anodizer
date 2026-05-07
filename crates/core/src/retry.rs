//! Uniform retry-with-exponential-backoff primitives.
//!
//! Replaces six open-coded retry loops in `stage-docker` (3×) and
//! `stage-release` (3×) that had diverged on backoff formulas —
//! `2^(n-2)`, `2^(n-1)`, and `500 << (attempt-1)` all coexisted.
//!
//! The canonical policy is exponential backoff with multiplier 2 starting at
//! `base_delay` and capped at `max_delay`:
//!
//! ```text
//! attempt 1:  f() executes immediately
//! attempt 2:  sleep base_delay
//! attempt 3:  sleep base_delay * 2
//! attempt N:  sleep min(base_delay * 2^(N-2), max_delay)
//! ```
//!
//! `ControlFlow<Break, Continue>` lets the operation decide retry policy per
//! failure (e.g. 4xx → Break, 5xx → Continue) without the helper encoding
//! protocol-specific predicates.
//!
//! Both a sync (`retry_sync`) and async (`retry_async`) variant are provided so
//! that sites can adopt without crossing a sync/async boundary.

use std::error::Error as StdError;
use std::fmt;
use std::io;
use std::ops::ControlFlow;
use std::time::Duration;

/// Retry policy used by `retry_sync` / `retry_async`.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Total attempts, including the first. Must be ≥ 1.
    pub max_attempts: u32,
    /// Delay before the second attempt (no wait before the first).
    pub base_delay: Duration,
    /// Upper bound on any individual sleep between attempts.
    pub max_delay: Duration,
}

impl RetryPolicy {
    /// Canonical policy matching GoReleaser upload defaults: 10 attempts, 50ms
    /// base, 30s cap.
    pub const UPLOAD: RetryPolicy = RetryPolicy {
        max_attempts: 10,
        base_delay: Duration::from_millis(50),
        max_delay: Duration::from_secs(30),
    };

    pub fn delay_for(&self, next_attempt: u32) -> Duration {
        // `next_attempt` is the attempt we're about to run (≥2). The wait
        // before attempt 2 uses base_delay; before attempt 3 uses base_delay*2;
        // i.e. multiplier = 2^(next_attempt - 2).
        let exp = next_attempt.saturating_sub(2);
        let mult = 1u64.checked_shl(exp).unwrap_or(u64::MAX);
        let ms = (self.base_delay.as_millis() as u64).saturating_mul(mult);
        std::cmp::min(Duration::from_millis(ms), self.max_delay)
    }
}

/// Retry a synchronous operation according to `policy`.
///
/// `op` returns:
/// - `Ok(T)` on success (no retry).
/// - `Err(ControlFlow::Continue(e))` to retry if attempts remain.
/// - `Err(ControlFlow::Break(e))` to stop immediately (4xx-style fast-fail).
///
/// Returns the last error if all attempts are exhausted.
pub fn retry_sync<T, E, F>(policy: &RetryPolicy, mut op: F) -> Result<T, E>
where
    F: FnMut(u32) -> Result<T, ControlFlow<E, E>>,
{
    let max = policy.max_attempts.max(1);
    let mut attempt: u32 = 1;
    loop {
        if attempt > 1 {
            std::thread::sleep(policy.delay_for(attempt));
        }
        match op(attempt) {
            Ok(v) => return Ok(v),
            Err(ControlFlow::Break(e)) => return Err(e),
            Err(ControlFlow::Continue(e)) => {
                if attempt >= max {
                    return Err(e);
                }
            }
        }
        attempt += 1;
    }
}

/// Retry an asynchronous operation according to `policy`.
///
/// Same semantics as `retry_sync` but awaits `op` and uses `tokio::time::sleep`.
pub async fn retry_async<T, E, F, Fut>(policy: &RetryPolicy, mut op: F) -> Result<T, E>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<T, ControlFlow<E, E>>>,
{
    let max = policy.max_attempts.max(1);
    let mut attempt: u32 = 1;
    loop {
        if attempt > 1 {
            tokio::time::sleep(policy.delay_for(attempt)).await;
        }
        match op(attempt).await {
            Ok(v) => return Ok(v),
            Err(ControlFlow::Break(e)) => return Err(e),
            Err(ControlFlow::Continue(e)) => {
                if attempt >= max {
                    return Err(e);
                }
            }
        }
        attempt += 1;
    }
}

/// Classify a `reqwest::Result<reqwest::blocking::Response>` into the
/// `ControlFlow` shape expected by `retry_sync` for a typical HTTP call:
/// 5xx + transport errors retry, 4xx fast-fails, 2xx/3xx returns Ok. The
/// returned response (Ok branch) is the caller's to consume.
///
/// This is the convention shared by every HTTP-uploading publisher; see audit
/// A7 dedup S5.
pub fn classify_http_sync(
    result: reqwest::Result<reqwest::blocking::Response>,
) -> Result<reqwest::blocking::Response, ControlFlow<anyhow::Error, anyhow::Error>> {
    use anyhow::anyhow;
    match result {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() || status.is_redirection() {
                Ok(resp)
            } else if status.is_server_error() {
                Err(ControlFlow::Continue(anyhow!(
                    "HTTP {} {}",
                    status.as_u16(),
                    status.canonical_reason().unwrap_or("server error")
                )))
            } else {
                // 4xx (and any other non-success/redirect/5xx): fast-fail
                Err(ControlFlow::Break(anyhow!(
                    "HTTP {} {}",
                    status.as_u16(),
                    status.canonical_reason().unwrap_or("client error")
                )))
            }
        }
        // Transport-layer failure (DNS, connect, TLS, timeout): retry.
        Err(e) => Err(ControlFlow::Continue(anyhow!(e))),
    }
}

// ---------------------------------------------------------------------------
// Retriable-error classification (mirrors GoReleaser internal/retryx)
// ---------------------------------------------------------------------------

/// Carries an HTTP status code alongside the original error so
/// [`is_retriable`] can route 5xx / 429 to retry and 4xx to fast-fail.
///
/// Mirrors GoReleaser `retryx.HTTPError`. Construct via [`HttpError::new`]
/// (status-only) or wrap an existing `reqwest::Response` via
/// [`HttpError::from_response`].
///
/// A `status` of `0` denotes a network-level failure where no response was
/// ever received (matches GR's `nil resp` branch). Network-level failures
/// are still classified via the inner error's message, so wrapping them in
/// `HttpError { status: 0, .. }` does not lose retriability information.
#[derive(Debug)]
pub struct HttpError {
    /// The wrapped error (transport, decode, or status-derived message).
    pub source: Box<dyn StdError + Send + Sync + 'static>,
    /// HTTP status code; `0` for transport-level failures.
    pub status: u16,
}

impl HttpError {
    /// Wrap an error with a status code. `0` denotes a network-level failure
    /// (no response received).
    pub fn new<E>(source: E, status: u16) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        Self {
            source: Box::new(source),
            status,
        }
    }
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Defer to the inner error so messages stay focused on the cause.
        // Mirrors GR `(e HTTPError) Error() string { return e.Err.Error() }`.
        fmt::Display::fmt(&self.source, f)
    }
}

impl StdError for HttpError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(&*self.source)
    }
}

/// Marker error wrapping any inner error so [`is_retriable`] returns `true`
/// regardless of class. Mirrors GoReleaser `retryx.Retriable` — useful when a
/// caller knows the failure is transient (e.g. an idempotent registry write
/// returning 422 because of a transient race condition) and wants the retry
/// loop to ignore the usual 4xx fast-fail.
#[derive(Debug)]
pub struct Retriable(pub Box<dyn StdError + Send + Sync + 'static>);

impl Retriable {
    /// Construct a `Retriable` wrapper. Returns `None` for a `None` input so
    /// the helper can be threaded through nullable error pipelines without an
    /// extra `if err.is_some()` check at the call site.
    pub fn new<E>(source: E) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        Self(Box::new(source))
    }
}

impl fmt::Display for Retriable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl StdError for Retriable {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(&*self.0)
    }
}

/// Returns `true` if the message looks like a transient network-layer failure.
///
/// Mirrors GoReleaser `retryx.IsNetworkError`. Substring-matches the lowercased
/// `Display` form of the error against the standard set of transient phrases:
/// `connection reset`, `network is unreachable`, `connection closed`,
/// `connection refused`, `tls handshake timeout`, `i/o timeout`,
/// `broken pipe`, `timeout awaiting response headers`,
/// `context deadline exceeded`. Also recognises [`io::ErrorKind::UnexpectedEof`]
/// and bare `io::Error` EOF wrappings via `downcast_ref` traversal of the
/// error chain.
pub fn is_network_error(err: &(dyn StdError + 'static)) -> bool {
    // 1. Walk the source chain for an io::Error that is EOF / UnexpectedEof.
    //    Rust has no equivalent of Go's `io.EOF` sentinel, so we treat
    //    `ErrorKind::UnexpectedEof` and any `io::Error` whose Display form is
    //    `"EOF"` (the convention crates like `rustls` use) as the analog.
    let mut cur: Option<&(dyn StdError + 'static)> = Some(err);
    while let Some(e) = cur {
        if let Some(io_err) = e.downcast_ref::<io::Error>() {
            if io_err.kind() == io::ErrorKind::UnexpectedEof {
                return true;
            }
            let m = io_err.to_string().to_lowercase();
            if m == "eof" || m == "unexpected eof" {
                return true;
            }
        }
        cur = e.source();
    }

    // 2. Substring match against the lowercased Display form.
    let s = err.to_string().to_lowercase();
    NETWORK_ERROR_NEEDLES.iter().any(|n| s.contains(n))
}

/// The set of substrings classified as transient by GoReleaser's
/// `retryx.IsNetworkError` (matching is case-insensitive).
const NETWORK_ERROR_NEEDLES: &[&str] = &[
    "connection reset",
    "network is unreachable",
    "connection closed",
    "connection refused",
    "tls handshake timeout",
    "i/o timeout",
    "broken pipe",
    "timeout awaiting response headers",
    "context deadline exceeded",
];

/// Classify an error as retriable (mirrors GoReleaser `retryx.IsRetriable`).
///
/// Returns `true` for:
/// - any [`is_network_error`] match (substring + EOF / UnexpectedEof in the
///   `source()` chain)
/// - any error whose chain contains a [`Retriable`] wrapper
/// - any error whose chain contains an [`HttpError`] with status `>= 500`
///   or status `429` (Too Many Requests)
///
/// Returns `false` for plain errors and 4xx HTTP errors (other than 429) —
/// those are fast-failed by the retry loop.
pub fn is_retriable(err: &(dyn StdError + 'static)) -> bool {
    // 1. Any link in the chain is an explicit Retriable marker.
    let mut cur: Option<&(dyn StdError + 'static)> = Some(err);
    while let Some(e) = cur {
        if e.is::<Retriable>() {
            return true;
        }
        if let Some(http) = e.downcast_ref::<HttpError>()
            && (http.status >= 500 || http.status == 429)
        {
            return true;
        }
        cur = e.source();
    }

    // 2. Network-error substring / EOF chain match.
    is_network_error(err)
}

/// Convenience: `None` passes through as `false`. Mirrors GoReleaser's
/// `IsRetriable(nil) -> false` semantics.
pub fn is_retriable_opt(err: Option<&(dyn StdError + 'static)>) -> bool {
    err.is_some_and(is_retriable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn fast_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 4,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
        }
    }

    #[test]
    fn delay_progression_caps_at_max() {
        let p = RetryPolicy {
            max_attempts: 10,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_millis(500),
        };
        assert_eq!(p.delay_for(2), Duration::from_millis(100));
        assert_eq!(p.delay_for(3), Duration::from_millis(200));
        assert_eq!(p.delay_for(4), Duration::from_millis(400));
        assert_eq!(p.delay_for(5), Duration::from_millis(500)); // capped
        assert_eq!(p.delay_for(8), Duration::from_millis(500)); // capped
    }

    #[test]
    fn sync_succeeds_on_first_attempt() {
        let calls = AtomicU32::new(0);
        let result: Result<&str, ()> = retry_sync(&fast_policy(), |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok("ok")
        });
        assert_eq!(result, Ok("ok"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn sync_retries_until_success() {
        let calls = AtomicU32::new(0);
        let result: Result<u32, &str> = retry_sync(&fast_policy(), |attempt| {
            calls.fetch_add(1, Ordering::SeqCst);
            if attempt < 3 {
                Err(ControlFlow::Continue("transient"))
            } else {
                Ok(attempt)
            }
        });
        assert_eq!(result, Ok(3));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn sync_break_stops_immediately() {
        let calls = AtomicU32::new(0);
        let result: Result<(), &str> = retry_sync(&fast_policy(), |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(ControlFlow::Break("fatal"))
        });
        assert_eq!(result, Err("fatal"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn sync_returns_last_error_after_exhaustion() {
        let calls = AtomicU32::new(0);
        let result: Result<(), String> = retry_sync(&fast_policy(), |attempt| {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(ControlFlow::Continue(format!("fail {attempt}")))
        });
        assert_eq!(result, Err("fail 4".to_string()));
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn async_retries_until_success() {
        let calls = std::sync::Arc::new(AtomicU32::new(0));
        let calls_inner = calls.clone();
        let result: Result<u32, &str> = retry_async(&fast_policy(), move |attempt| {
            let c = calls_inner.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                if attempt < 2 {
                    Err(ControlFlow::Continue("transient"))
                } else {
                    Ok(attempt)
                }
            }
        })
        .await;
        assert_eq!(result, Ok(2));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    // -----------------------------------------------------------------------
    // is_network_error / is_retriable / HttpError / Retriable
    //
    // Mirrors GoReleaser internal/retryx/retryx_test.go test cases.
    // -----------------------------------------------------------------------

    /// Plain string error wrapper used in classification tests.
    #[derive(Debug)]
    struct StrErr(&'static str);
    impl fmt::Display for StrErr {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.0)
        }
    }
    impl StdError for StrErr {}

    #[derive(Debug)]
    struct OwnedErr(String);
    impl fmt::Display for OwnedErr {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(&self.0)
        }
    }
    impl StdError for OwnedErr {}

    #[test]
    fn network_error_substrings_match() {
        for s in [
            "connection reset by peer",
            "network is unreachable",
            "connection closed unexpectedly",
            "connection refused",
            "tls handshake timeout",
            "i/o timeout",
            "CONNECTION RESET",
            "TLS Handshake Timeout",
            "write: broken pipe",
            "net/http: timeout awaiting response headers",
            "context deadline exceeded",
        ] {
            let e = OwnedErr(s.to_string());
            assert!(is_network_error(&e), "expected network error: {s:?}");
        }
    }

    #[test]
    fn network_error_io_eof_kinds() {
        let e = io::Error::from(io::ErrorKind::UnexpectedEof);
        assert!(is_network_error(&e));

        // A custom-kind io::Error whose Display is "EOF" (rustls / hyper convention).
        let e2 = io::Error::other("EOF");
        assert!(is_network_error(&e2));
    }

    #[test]
    fn network_error_wrapped_unexpected_eof() {
        // Wrap an UnexpectedEof in an outer error so chain-walking is exercised.
        #[derive(Debug)]
        struct Wrap(io::Error);
        impl fmt::Display for Wrap {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "read failed")
            }
        }
        impl StdError for Wrap {
            fn source(&self) -> Option<&(dyn StdError + 'static)> {
                Some(&self.0)
            }
        }
        let inner = io::Error::from(io::ErrorKind::UnexpectedEof);
        let outer = Wrap(inner);
        assert!(is_network_error(&outer));
    }

    #[test]
    fn network_error_non_network_strings_reject() {
        for s in [
            "file not found",
            "permission denied",
            "dial tcp: lookup example.com: no such host",
            "",
        ] {
            let e = OwnedErr(s.to_string());
            assert!(!is_network_error(&e), "expected NOT network error: {s:?}");
        }
    }

    #[test]
    fn retriable_opt_nil_passthrough() {
        assert!(!is_retriable_opt(None));
    }

    #[test]
    fn http_error_500_retriable() {
        let e = HttpError::new(StrErr("internal server error"), 500);
        assert!(is_retriable(&e));
    }

    #[test]
    fn http_error_502_503_retriable() {
        for s in [502u16, 503] {
            let e = HttpError::new(StrErr("bad gateway"), s);
            assert!(is_retriable(&e), "status {s} should be retriable");
        }
    }

    #[test]
    fn http_error_429_retriable() {
        let e = HttpError::new(StrErr("rate limited"), 429);
        assert!(is_retriable(&e));
    }

    #[test]
    fn http_error_4xx_not_retriable() {
        for s in [400u16, 401, 403, 404, 422] {
            let e = HttpError::new(StrErr("client err"), s);
            assert!(!is_retriable(&e), "status {s} should NOT be retriable");
        }
    }

    #[test]
    fn http_error_zero_status_routes_via_message() {
        // Status 0 == network-level failure with no response. Retriability
        // falls back to the network-error substring matcher on the inner.
        let net = HttpError::new(StrErr("connection reset"), 0);
        assert!(is_retriable(&net));

        let non_net = HttpError::new(StrErr("dial failed"), 0);
        assert!(!is_retriable(&non_net));
    }

    #[test]
    fn http_error_unwrap_chain_visible() {
        let inner = StrErr("inner");
        let e = HttpError::new(inner, 503);
        assert!(e.source().is_some());
    }

    #[test]
    fn retriable_wrapper_is_retriable() {
        let e = Retriable::new(StrErr("retry me"));
        assert!(is_retriable(&e));
    }

    #[test]
    fn retriable_wrapper_overrides_4xx() {
        // GR test: a 422 wrapped in Retriable is still retriable.
        let inner = HttpError::new(StrErr("exists"), 422);
        let outer = Retriable::new(inner);
        assert!(is_retriable(&outer));
    }

    #[test]
    fn retriable_wrapper_unwrap_chain_visible() {
        let inner = StrErr("inner");
        let e = Retriable::new(inner);
        assert!(e.source().is_some());
    }

    #[test]
    fn plain_error_not_retriable() {
        let e = StrErr("something");
        assert!(!is_retriable(&e));
    }

    #[test]
    fn anyhow_error_threadable() {
        // Ensure is_retriable works through anyhow::Error's deref-to-dyn path
        // (which is the canonical caller form across the codebase).
        let e: anyhow::Error = anyhow::anyhow!("connection refused");
        assert!(is_retriable(e.as_ref()));

        let e2: anyhow::Error = anyhow::anyhow!("permission denied");
        assert!(!is_retriable(e2.as_ref()));
    }

    #[test]
    fn is_retriable_chain_walks_to_http_error() {
        // An anyhow::Error wrapping a concrete HttpError must be classified
        // by walking source(), not by Display alone — the message "outer"
        // gives no hint, the 503 status does.
        let inner = HttpError::new(StrErr("bad gateway"), 503);
        let wrapped: anyhow::Error = anyhow::Error::new(inner).context("publish failed");
        assert!(is_retriable(wrapped.as_ref()));
    }
}
