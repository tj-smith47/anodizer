//! Secondary rate-limit detection and backoff for GitHub uploads.
//!
//! GitHub's *secondary* rate limit is distinct from the proactive primary-limit
//! check in `rate_limit.rs`. It is triggered by burst patterns and surfaces as
//! an HTTP 403 or 429 whose response body contains
//! `"You have exceeded a secondary rate limit"`. GitHub may also include a
//! `Retry-After` header (integer seconds) as a hint.
//!
//! ## `Retry-After` header and octocrab
//!
//! octocrab's `map_github_error` discards response headers when it converts a
//! non-2xx body into `GitHubError` (the `GitHubError` struct holds only
//! `message`, `documentation_url`, `errors`, and `status_code`). The
//! `Retry-After` header is therefore not accessible via the typed error.
//!
//! As a practical equivalent: when a secondary rate-limit response is detected,
//! the upload loop applies a minimum backoff of `SECONDARY_RL_MIN_SECS` seconds
//! (plus ±20 % jitter) before the next attempt. This is conservative and
//! honours the spirit of `Retry-After: N ≥ 60` that GitHub's secondary-limit
//! responses typically carry. A configurable override is available via
//! `ANODIZER_GITHUB_SECONDARY_RL_DELAY_SECS`.
//!
//! GoReleaser reference: `internal/client/github.go` does not explicitly handle
//! secondary rate limits. Anodizer adds this as a Rust-first improvement.

use std::time::Duration;

/// Minimum sleep when a secondary rate-limit response is detected, absent a
/// more specific `Retry-After` hint accessible through the API.
///
/// GitHub's documentation states that secondary rate-limit waits typically
/// range from 30–90 seconds. 60 s is the conservative midpoint.
/// Override via `ANODIZER_GITHUB_SECONDARY_RL_DELAY_SECS`.
pub(crate) const SECONDARY_RL_MIN_SECS: u64 = 60;

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
/// Reads `ANODIZER_GITHUB_SECONDARY_RL_DELAY_SECS` first; falls back to
/// [`SECONDARY_RL_MIN_SECS`]. Callers should apply `jitter_duration` on top of
/// the returned value.
pub(crate) fn secondary_rl_delay() -> Duration {
    let secs = std::env::var("ANODIZER_GITHUB_SECONDARY_RL_DELAY_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(SECONDARY_RL_MIN_SECS);
    Duration::from_secs(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::{Read, Write};
    use std::net::TcpListener;
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
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let addr = listener.local_addr().expect("local_addr");
            std::thread::spawn(move || {
                for _ in 0..serve_count {
                    if let Ok((mut s, _)) = listener.accept() {
                        let mut buf = [0u8; 4096];
                        let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
                        let _ = s.read(&mut buf);
                        let _ = s.write_all(raw.as_bytes());
                        let _ = s.flush();
                    } else {
                        break;
                    }
                }
            });
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

    #[test]
    fn secondary_rl_delay_env_override() {
        // When ANODIZER_GITHUB_SECONDARY_RL_DELAY_SECS is set to a positive
        // integer, secondary_rl_delay() must return that duration.
        // SAFETY: test-only; no other thread reads this var concurrently
        // during this test (cargo runs each #[test] fn in its own thread but
        // we use a unique key so the window is short and isolated).
        unsafe {
            std::env::set_var("ANODIZER_GITHUB_SECONDARY_RL_DELAY_SECS", "3");
        }
        let d = secondary_rl_delay();
        unsafe {
            std::env::remove_var("ANODIZER_GITHUB_SECONDARY_RL_DELAY_SECS");
        }
        assert_eq!(d, Duration::from_secs(3));
    }

    #[test]
    fn secondary_rl_delay_default_when_unset() {
        unsafe {
            std::env::remove_var("ANODIZER_GITHUB_SECONDARY_RL_DELAY_SECS");
        }
        assert_eq!(
            secondary_rl_delay(),
            Duration::from_secs(SECONDARY_RL_MIN_SECS)
        );
    }
}
