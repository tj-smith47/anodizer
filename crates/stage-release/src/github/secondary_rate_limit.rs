//! Secondary rate-limit detection and backoff for GitHub uploads.
//!
//! GitHub's *secondary* rate limit is distinct from the proactive primary-limit
//! check in `rate_limit.rs`. It is triggered by burst patterns and surfaces as
//! an HTTP 403 or 429 whose response body contains
//! `"You have exceeded a secondary rate limit"`. GitHub may also include a
//! `Retry-After` header (integer seconds) as a hint.
//!
//! ## `Retry-After` header capture
//!
//! octocrab's `map_github_error` discards response headers when it converts a
//! non-2xx body into `GitHubError` (the `GitHubError` struct holds only
//! `message`, `documentation_url`, `errors`, and `status_code`). To recover
//! the server's `Retry-After` hint, a tower middleware layer
//! ([`RetryAfterService`]) intercepts every HTTP response *before* octocrab
//! processes it and stores the header's integer value in a shared
//! [`RetryAfterCapture`]. The retry loops then read that captured value via
//! [`secondary_rl_delay`] and honour it (clamped to [60, 600] seconds) instead
//! of always falling back to a fixed constant.
//!
//! GoReleaser parity: `internal/client/github.go` reads the exact
//! `Retry-After` from go-github's `*AbuseRateLimitError`, clamped with a
//! 1-minute floor and 10-minute cap.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use tower::{Layer, Service};

/// Minimum sleep when a secondary rate-limit response is detected, absent a
/// more specific `Retry-After` hint accessible through the API.
///
/// GitHub's documentation states that secondary rate-limit waits typically
/// range from 30–90 seconds. 60 s is the conservative midpoint.
/// Override via `ANODIZER_GITHUB_SECONDARY_RL_DELAY_SECS`.
pub(crate) const SECONDARY_RL_MIN_SECS: u64 = 60;

/// Maximum `Retry-After` value the retry loop will honour (10 minutes).
/// Values above this are clamped to avoid indefinite stalls from a
/// misbehaving proxy or pathological server response.
const RETRY_AFTER_MAX_SECS: u64 = 600;

// ---------------------------------------------------------------------------
// RetryAfterCapture — shared state read by the retry loops
// ---------------------------------------------------------------------------

/// Shared capture of the most recent `Retry-After` header value (seconds).
///
/// Written by [`RetryAfterService`] on every HTTP response that carries the
/// header; read by [`secondary_rl_delay`] when deciding how long to sleep
/// after a secondary rate-limit response.
#[derive(Clone, Debug)]
pub(crate) struct RetryAfterCapture(Arc<AtomicU64>);

impl RetryAfterCapture {
    pub(crate) fn new() -> Self {
        Self(Arc::new(AtomicU64::new(0)))
    }

    /// Return the last captured `Retry-After` value, or `None` if no header
    /// has been seen (or the stored value is 0).
    pub(crate) fn get(&self) -> Option<Duration> {
        let secs = self.0.load(Ordering::Relaxed);
        if secs == 0 {
            None
        } else {
            Some(Duration::from_secs(secs))
        }
    }

    /// Store a captured `Retry-After` integer-seconds value.
    fn set(&self, secs: u64) {
        self.0.store(secs, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// RetryAfterLayer / RetryAfterService — tower middleware
// ---------------------------------------------------------------------------

/// Tower [`Layer`] that wraps an inner HTTP service with
/// [`RetryAfterService`], capturing `Retry-After` headers before octocrab
/// processes the response.
#[derive(Clone)]
pub(crate) struct RetryAfterLayer {
    capture: RetryAfterCapture,
}

impl RetryAfterLayer {
    pub(crate) fn new(capture: RetryAfterCapture) -> Self {
        Self { capture }
    }
}

impl<S> Layer<S> for RetryAfterLayer {
    type Service = RetryAfterService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RetryAfterService {
            inner,
            capture: self.capture.clone(),
        }
    }
}

/// Tower [`Service`] that intercepts HTTP responses and captures the
/// `retry-after` header value (integer seconds) before passing the response
/// through unchanged.
#[derive(Clone)]
pub(crate) struct RetryAfterService<S> {
    inner: S,
    capture: RetryAfterCapture,
}

impl<S, ReqBody, ResBody> Service<http::Request<ReqBody>> for RetryAfterService<S>
where
    S: Service<http::Request<ReqBody>, Response = http::Response<ResBody>>,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<ReqBody>) -> Self::Future {
        let capture = self.capture.clone();
        let fut = self.inner.call(req);
        Box::pin(async move {
            let resp = fut.await?;
            if let Some(val) = resp.headers().get(http::header::RETRY_AFTER)
                && let Ok(s) = val.to_str()
                && let Ok(secs) = s.trim().parse::<u64>()
            {
                capture.set(secs);
            }
            Ok(resp)
        })
    }
}

/// Returns `true` when `err` looks like a GitHub secondary rate-limit response.
///
/// Detection criteria (any one is sufficient):
/// 1. HTTP status is 403 or 429 AND the `GitHubError.message` field contains
///    `"secondary rate limit"` (case-insensitive).
/// 2. HTTP status is 403 or 429 AND the `GitHubError.documentation_url` field
///    contains `"secondary-rate-limits"` (GitHub includes this in rate-limit
///    error bodies).
///
/// A plain 403 (auth failure) or 429 (primary rate-limit) without these
/// indicators returns `false`.
pub(crate) fn is_secondary_rate_limit(err: &octocrab::Error) -> bool {
    let octocrab::Error::GitHub { source, .. } = err else {
        return false;
    };
    let status = source.status_code.as_u16();
    if status != 403 && status != 429 {
        return false;
    }
    let msg = source.message.to_lowercase();
    if msg.contains("secondary rate limit") {
        return true;
    }
    if let Some(doc_url) = &source.documentation_url
        && doc_url.contains("secondary-rate-limits")
    {
        return true;
    }
    false
}

/// Return the delay to apply when a secondary rate-limit response is detected.
///
/// Precedence (first match wins):
/// 1. `ANODIZER_GITHUB_SECONDARY_RL_DELAY_SECS` env var — hard override.
/// 2. `capture` — the server's `Retry-After` header value, clamped to
///    \[60, 600\] seconds.
/// 3. [`SECONDARY_RL_MIN_SECS`] (60 s) constant fallback.
///
/// Callers should apply `jitter_duration` on top of the returned value.
pub(crate) fn secondary_rl_delay(capture: Option<&RetryAfterCapture>) -> Duration {
    // Hard override via env var takes absolute precedence.
    if let Some(secs) = std::env::var("ANODIZER_GITHUB_SECONDARY_RL_DELAY_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
    {
        return Duration::from_secs(secs);
    }

    // Honour the server's Retry-After header, clamped to [60, 600].
    if let Some(captured) = capture.and_then(RetryAfterCapture::get) {
        let secs = captured
            .as_secs()
            .clamp(SECONDARY_RL_MIN_SECS, RETRY_AFTER_MAX_SECS);
        return Duration::from_secs(secs);
    }

    Duration::from_secs(SECONDARY_RL_MIN_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
    use std::time::Duration;

    /// Synthesise an `octocrab::Error::GitHub` for the given status and body.
    ///
    /// For 429 and 5xx responses octocrab's built-in tower retry middleware
    /// (`RetryConfig::Simple(3)`) makes up to 3 additional attempts, so the TCP
    /// listener must serve the response `4` times (1 initial + 3 retries) before
    /// tower gives up and lets `map_github_error` see the status. For 4xx
    /// responses other than 429 (e.g. 403), tower does NOT retry so `1` is
    /// sufficient.
    fn make_github_error_sync(status: u16, body: &'static str) -> octocrab::Error {
        let body_len = body.len();
        let raw = Box::leak(
            format!(
                "HTTP/1.1 {status} STATUS\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: {body_len}\r\n\
                 \r\n\
                 {body}"
            )
            .into_boxed_str(),
        );
        // 429 (and 5xx) are retried by octocrab's tower middleware up to 3
        // times; serve the response 4 times so the final attempt reaches
        // `map_github_error` and produces a typed GitHub error.
        let serve_count: usize = if status == 429 || status >= 500 { 4 } else { 1 };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio rt for test helper");
        rt.block_on(async move {
            // Serve `serve_count` identical responses. octocrab's tower
            // retry middleware makes up to 4 attempts on 429 / 5xx; for
            // 4xx other than 429 a single response is enough.
            let (addr, _calls) = spawn_oneshot_http_responder(vec![raw; serve_count]);
            let octo = octocrab::OctocrabBuilder::new()
                .base_uri(format!("http://{addr}/"))
                .expect("base_uri")
                .build()
                .expect("build");
            octo.get::<serde_json::Value, _, _>("/test", None::<&()>)
                .await
                .expect_err("must error on non-2xx")
        })
    }

    #[test]
    fn detects_secondary_rate_limit_403() {
        let body = r#"{"message":"You have exceeded a secondary rate limit and have been temporarily blocked from content creation. Please retry your request again later.","documentation_url":"https://docs.github.com/rest/overview/resources-in-the-rest-api#secondary-rate-limits"}"#;
        let err = make_github_error_sync(403, body);
        assert!(
            is_secondary_rate_limit(&err),
            "403 with secondary-rate-limit message must classify as secondary"
        );
    }

    #[test]
    fn detects_secondary_rate_limit_429() {
        let body = r#"{"message":"You have exceeded a secondary rate limit","documentation_url":"https://docs.github.com/rest/overview/resources-in-the-rest-api#secondary-rate-limits"}"#;
        let err = make_github_error_sync(429, body);
        assert!(
            is_secondary_rate_limit(&err),
            "429 with secondary-rate-limit message must classify as secondary"
        );
    }

    #[test]
    fn detects_secondary_rate_limit_via_doc_url_only() {
        // Body has a generic message but the doc URL contains the secondary
        // rate-limit indicator — detection must fire on either signal.
        // Uses 403 (not 429) to avoid octocrab's internal tower retry
        // consuming multiple TCP connections before the error surfaces.
        let body = r#"{"message":"Too many requests","documentation_url":"https://docs.github.com/rest/overview/resources-in-the-rest-api#secondary-rate-limits"}"#;
        let err = make_github_error_sync(403, body);
        assert!(
            is_secondary_rate_limit(&err),
            "403 with secondary-rate-limits in doc URL must classify as secondary"
        );
    }

    #[test]
    fn plain_403_auth_failure_is_not_secondary() {
        let body =
            r#"{"message":"Bad credentials","documentation_url":"https://docs.github.com/rest"}"#;
        let err = make_github_error_sync(403, body);
        assert!(
            !is_secondary_rate_limit(&err),
            "plain 403 auth failure must NOT classify as secondary rate limit"
        );
    }

    #[test]
    fn plain_429_without_secondary_indicator_is_not_secondary() {
        let body = r#"{"message":"API rate limit exceeded","documentation_url":"https://docs.github.com/rest/overview/resources-in-the-rest-api#rate-limiting"}"#;
        let err = make_github_error_sync(429, body);
        assert!(
            !is_secondary_rate_limit(&err),
            "429 without 'secondary rate limit' in message must NOT classify as secondary"
        );
    }

    // Both env-var tests below mutate ANODIZER_GITHUB_SECONDARY_RL_DELAY_SECS,
    // which is a process-wide side effect. Without `#[serial]` they race
    // on parallel cargo-test threads and the "default_when_unset" test can
    // observe the "3" value set by "env_override". The race was unmasked
    // by the test-responder centralization (faster test runs => tighter
    // interleaving window); the underlying bug pre-dates that change.
    #[test]
    #[serial_test::serial]
    fn secondary_rl_delay_env_override() {
        // Env var takes absolute precedence — even over a captured value.
        unsafe {
            std::env::set_var("ANODIZER_GITHUB_SECONDARY_RL_DELAY_SECS", "3");
        }
        let cap = RetryAfterCapture::new();
        cap.set(120);
        let d = secondary_rl_delay(Some(&cap));
        unsafe {
            std::env::remove_var("ANODIZER_GITHUB_SECONDARY_RL_DELAY_SECS");
        }
        assert_eq!(d, Duration::from_secs(3));
    }

    #[test]
    #[serial_test::serial]
    fn secondary_rl_delay_default_when_unset() {
        unsafe {
            std::env::remove_var("ANODIZER_GITHUB_SECONDARY_RL_DELAY_SECS");
        }
        assert_eq!(
            secondary_rl_delay(None),
            Duration::from_secs(SECONDARY_RL_MIN_SECS)
        );
    }

    #[test]
    fn retry_after_capture_get_set() {
        let cap = RetryAfterCapture::new();
        assert!(cap.get().is_none(), "fresh capture must be None");
        cap.set(90);
        assert_eq!(cap.get(), Some(Duration::from_secs(90)));
        cap.set(0);
        assert!(cap.get().is_none(), "storing 0 resets to None");
    }

    #[test]
    #[serial_test::serial]
    fn secondary_rl_delay_prefers_captured_over_constant() {
        // No env override; captured value of 120 s should be honoured.
        unsafe {
            std::env::remove_var("ANODIZER_GITHUB_SECONDARY_RL_DELAY_SECS");
        }
        let cap = RetryAfterCapture::new();
        cap.set(120);
        assert_eq!(
            secondary_rl_delay(Some(&cap)),
            Duration::from_secs(120),
            "captured Retry-After should override the 60 s constant"
        );
    }

    #[test]
    #[serial_test::serial]
    fn secondary_rl_delay_clamps_low_captured_value() {
        // A server-sent Retry-After: 5 is below the 60 s floor.
        unsafe {
            std::env::remove_var("ANODIZER_GITHUB_SECONDARY_RL_DELAY_SECS");
        }
        let cap = RetryAfterCapture::new();
        cap.set(5);
        assert_eq!(
            secondary_rl_delay(Some(&cap)),
            Duration::from_secs(SECONDARY_RL_MIN_SECS),
            "values below 60 s must be clamped to the floor"
        );
    }

    #[test]
    #[serial_test::serial]
    fn secondary_rl_delay_clamps_high_captured_value() {
        // A server-sent Retry-After: 9999 is above the 600 s cap.
        unsafe {
            std::env::remove_var("ANODIZER_GITHUB_SECONDARY_RL_DELAY_SECS");
        }
        let cap = RetryAfterCapture::new();
        cap.set(9999);
        assert_eq!(
            secondary_rl_delay(Some(&cap)),
            Duration::from_secs(RETRY_AFTER_MAX_SECS),
            "values above 600 s must be clamped to the cap"
        );
    }
}
