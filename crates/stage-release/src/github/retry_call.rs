//! Thin wrapper around `retry_async` + `classify_octocrab_error` for the
//! GitHub backend's octocrab call sites.
//!
//! Each retriable octocrab call (find-draft list, replace-draft delete,
//! create-release POST, update-release PATCH, un-draft publish PATCH) shares
//! the same boilerplate:
//!
//! ```text
//! retry_async(&policy, |attempt| async move {
//!     match <octocrab_call>.await {
//!         Ok(v) => Ok(v),
//!         Err(err) => {
//!             let (wrapped, status) = classify_octocrab_error(err);
//!             if is_retriable(&*wrapped) {
//!                 warn!("... attempt {attempt} status={status}");
//!                 Err(ControlFlow::Continue(...))
//!             } else {
//!                 Err(ControlFlow::Break(...))
//!             }
//!         }
//!     }
//! }).await
//! ```
//!
//! Lifted here so the four octocrab call sites in `github::mod` (and the
//! un-draft PATCH that already used the inline form) all share one
//! classification + logging pathway. Drift between the loops is the failure
//! mode we are avoiding: prior to this helper, the upload retry, the publish
//! PATCH retry, and any new wiring each had their own copy of the same five
//! `matches!` arms, and the upload loop drifted to use bespoke logging while
//! the publish PATCH used `release_log().warn`.
//!
//! ## Return type
//!
//! `retry_octocrab_call` returns `Result<T, octocrab::Error>` so callers can
//! match on the underlying variant (notably `Error::GitHub { source }` to
//! route a 404 to "no existing release" vs. propagating every other status
//! code). The classification used to drive retriability stays internal to
//! the helper; the original `octocrab::Error` is handed back unchanged on
//! retry exhaustion or fast-fail.
//!
//! ## Divergence with the upload-asset loop
//!
//! The bespoke `upload_asset` retry loop in `mod.rs` cannot route through
//! `retry_octocrab_call` because it carries upload-specific state (the
//! resume-stream re-read of the artifact, the 422-`already_exists`
//! delete+retry dance, the one-shot overwrite guard). It re-uses
//! [`format_retry_warn`] for per-attempt logging so the warn format stays
//! consistent across both pathways; the format is pinned by a unit test in
//! this module.

use std::future::Future;

use anodizer_core::retry::{RetryPolicy, is_retriable, jitter_duration};

use super::secondary_rate_limit::{RetryAfterCapture, is_secondary_rate_limit, secondary_rl_delay};
use crate::release_log;

/// Per-attempt warning line shared by every retry-wrapped octocrab call site
/// (the helper here AND the bespoke upload-asset loop in `mod.rs`).
///
/// Extracted so the two retry pathways can't drift on label format. A test
/// in `mod.rs` pins the exact format string against the upload loop's call.
pub(crate) fn format_retry_warn(label: &str, attempt: u32, max: u32, status: u16) -> String {
    format!("release: {label} failed (retriable, attempt {attempt}/{max}, status={status})")
}

/// Run an octocrab call through the shared retry policy.
///
/// `label` is the short operation name shown in the per-attempt warning
/// (e.g. `"find draft release"`, `"delete release"`, `"create release"`).
/// `make_call` is invoked once per attempt and must rebuild the future from
/// scratch (octocrab's response futures are not `Clone`).
///
/// Returns the inner octocrab result on success. On retry exhaustion or
/// fast-fail, the original [`octocrab::Error`] is returned unchanged so the
/// caller can match on `Error::GitHub { source }` for status-code routing
/// (e.g. mapping a 404 to "no existing release" while propagating every
/// other status).
///
/// ## Secondary rate-limit handling
///
/// When a secondary rate-limit response (403/429 with GitHub's secondary-RL
/// body text) is detected, the helper logs a dedicated warning and sleeps for
/// `secondary_rl_delay()` — which honours the server's `Retry-After` header
/// (captured by [`RetryAfterCapture`] middleware), clamped to [60, 600] s,
/// overridable via `ANODIZER_GITHUB_SECONDARY_RL_DELAY_SECS` — with ±20 %
/// jitter before retrying. The policy's normal exp-backoff delay is skipped
/// for secondary-RL attempts to avoid doubling the sleep.
pub(crate) async fn retry_octocrab_call<T, F, Fut>(
    policy: &RetryPolicy,
    label: &'static str,
    retry_after: Option<&RetryAfterCapture>,
    mut make_call: F,
) -> Result<T, octocrab::Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, octocrab::Error>>,
{
    let max = policy.max_attempts.max(1);
    let mut attempt: u32 = 1;
    let mut last_err: Option<octocrab::Error> = None;
    loop {
        // Normal exp-backoff sleep (skipped on first attempt, and skipped
        // when the previous attempt was a secondary-RL response — in that
        // case we already slept the secondary-RL duration below).
        let skip_policy_sleep = last_err.as_ref().is_some_and(is_secondary_rate_limit);
        if attempt > 1 && !skip_policy_sleep {
            tokio::time::sleep(policy.delay_for(attempt)).await;
        }

        match make_call().await {
            Ok(v) => return Ok(v),
            Err(err) => {
                let secondary_rl = is_secondary_rate_limit(&err);
                let (status, retriable) = classify_retriability(&err);
                // A secondary rate-limit 403 is not retriable by the default
                // classifier (which only retries 5xx/429), but it IS a
                // transient condition that must be retried after a delay.
                if !retriable && !secondary_rl {
                    return Err(err);
                }
                release_log().warn(&format_retry_warn(label, attempt, max, status));
                if attempt >= max {
                    return Err(err);
                }
                // Secondary rate-limit: sleep the dedicated RL delay (with
                // jitter) instead of the policy's exp-backoff delay.
                if secondary_rl {
                    let delay = jitter_duration(secondary_rl_delay(retry_after));
                    release_log().warn(&format!(
                        "release: {label} hit GitHub secondary rate limit; \
                         sleeping {:.1}s before retry (attempt {attempt}/{max})",
                        delay.as_secs_f64(),
                    ));
                    tokio::time::sleep(delay).await;
                }
                last_err = Some(err);
            }
        }
        attempt += 1;
    }
}

/// Borrow-based retriability probe for [`octocrab::Error`].
///
/// Mirrors [`classify_octocrab_error`]'s rules but consumes only a reference
/// so the original error can be returned to the caller unchanged. Returns
/// `(status_code, retriable)` where `status_code` is `0` for transport-layer
/// failures with no HTTP response attached.
fn classify_retriability(err: &octocrab::Error) -> (u16, bool) {
    // Build a throwaway wrapper from a synthetic inner so we can reuse the
    // existing `is_retriable` predicate without taking ownership of `err`.
    // The wrapper's job is just to set the right "retriable / not" bit for
    // the shared classifier; the actual error returned to the caller is the
    // borrowed original.
    use anodizer_core::retry::{HttpError, Retriable};
    match err {
        octocrab::Error::GitHub { source, .. } => {
            let status = source.status_code.as_u16();
            let probe = HttpError::new(std::io::Error::other("status probe"), status);
            (status, is_retriable(&probe))
        }
        octocrab::Error::Hyper { .. }
        | octocrab::Error::Http { .. }
        | octocrab::Error::Service { .. }
        | octocrab::Error::Other { .. }
        | octocrab::Error::Serde { .. }
        | octocrab::Error::Json { .. } => {
            let probe = Retriable::new(std::io::Error::other("transport probe"));
            (0, is_retriable(&probe))
        }
        _ => {
            // Conservative default: unfamiliar future variants fast-fail
            // rather than spin. Matches `classify_octocrab_error`'s fallback.
            (0, false)
        }
    }
}

/// Detect a 404 status in an [`octocrab::Error`].
///
/// Used by `run_github_backend` to map the `get_by_tag` lookup's only
/// non-error fall-through (real 404 -> "no existing release") while
/// propagating every other status (auth, validation, exhausted retries on
/// 5xx). The match is on the typed variant so transport-layer failures
/// (which carry no status) cannot accidentally fall through.
pub(crate) fn is_octocrab_404(err: &octocrab::Error) -> bool {
    matches!(
        err,
        octocrab::Error::GitHub { source, .. } if source.status_code.as_u16() == 404
    )
}

#[cfg(test)]
mod tests {
    //! Drive the helper through an in-process TCP listener that scripts HTTP
    //! responses. Matches the test convention used by `gitea.rs` /
    //! `gitlab.rs` (see `spawn_oneshot_http_responder`).
    //!
    //! We point `OctocrabBuilder::base_uri` at the listener and exercise a
    //! single raw `get` call so the helper's retry + classifier behaviour is
    //! verified end-to-end with a real `octocrab::Error` instead of a mock.
    use super::*;
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
    use std::net::SocketAddr;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    fn build_test_octocrab(addr: SocketAddr) -> octocrab::Octocrab {
        let builder = octocrab::OctocrabBuilder::new()
            .base_uri(format!("http://{addr}/"))
            .expect("OctocrabBuilder::base_uri accepts loopback URL");
        builder
            .build()
            .expect("OctocrabBuilder::build succeeds on loopback URL")
    }

    #[tokio::test]
    async fn retries_5xx_then_succeeds() {
        // Two 503s and then a 200 with an empty JSON array. The helper must
        // retry past both 503s and return Ok on the third attempt.
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]",
        ]);
        let octo = build_test_octocrab(addr);
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };
        let result: Result<Vec<serde_json::Value>, octocrab::Error> =
            retry_octocrab_call(&policy, "test list", None, || async {
                octo.get("/test", None::<&()>).await
            })
            .await;
        assert!(
            result.is_ok(),
            "5xx must retry to success: {:?}",
            result.err()
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "expected 2 retries past 503 + 1 success"
        );
    }

    #[tokio::test]
    async fn fast_fails_4xx_without_retry() {
        // A single 404 must fast-fail; the helper must NOT retry 4xx.
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: 27\r\n\r\n{\"message\":\"Not Found\"}    ",
        ]);
        let octo = build_test_octocrab(addr);
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };
        let result: Result<Vec<serde_json::Value>, octocrab::Error> =
            retry_octocrab_call(&policy, "test list", None, || async {
                octo.get("/test", None::<&()>).await
            })
            .await;
        assert!(result.is_err(), "4xx must surface as Err, got Ok");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "4xx must NOT retry (fast-fail honors classifier)"
        );
    }

    #[tokio::test]
    async fn respects_max_attempts_one() {
        // `RetryConfig { attempts: 1 }` must produce exactly one octocrab
        // call even on a retriable 503. This pins the
        // `RetryConfig::to_policy` -> `retry_async` wiring contract.
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
        ]);
        let octo = build_test_octocrab(addr);
        let policy = RetryPolicy {
            max_attempts: 1,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };
        let result: Result<Vec<serde_json::Value>, octocrab::Error> =
            retry_octocrab_call(&policy, "test list", None, || async {
                octo.get("/test", None::<&()>).await
            })
            .await;
        assert!(result.is_err(), "attempts=1 + 503 must surface Err");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "attempts=1 must produce exactly one octocrab call"
        );
    }

    #[test]
    fn format_retry_warn_shape_pins_shared_format() {
        // Pin the format string used by BOTH the helper's per-attempt warn
        // and the upload loop's per-attempt warn (mod.rs). Drift between
        // the two formats is the failure mode the helper exists to prevent.
        let s = format_retry_warn("delete release", 3, 10, 503);
        assert_eq!(
            s,
            "release: delete release failed (retriable, attempt 3/10, status=503)"
        );
    }

    /// Drive the secondary-rate-limit backoff path end-to-end.
    ///
    /// Uses a 403 (not 429) secondary-RL response. Rationale: octocrab's
    /// default `RetryConfig::Simple(3)` tower middleware intercepts 429s at
    /// the transport layer and retries them internally before `map_github_error`
    /// ever runs. A 403 secondary-RL response is not intercepted by that
    /// middleware and reaches `map_github_error` unchanged, giving us a typed
    /// `octocrab::Error::GitHub { status_code: 403 }` that `is_secondary_rate_limit`
    /// can inspect. GitHub sends both 403 and 429 for secondary limits; 403 is
    /// the more common form for content-creation bursts.
    ///
    /// The architectural-reality assertion is: the helper detects a
    /// secondary-RL response, sleeps the configured delay (with jitter), and
    /// retries to success. Multi-second wall-clock is incidental, so the test
    /// configures `ANODIZER_GITHUB_SECONDARY_RL_DELAY_SECS=1` and asserts
    /// elapsed >= 800 ms (1 s * 0.8 jitter floor).
    ///
    /// `tokio::time::pause()` is NOT used here: the oneshot HTTP responder
    /// runs on a real socket served by a `std::thread`, which won't observe
    /// virtual time. The 1 s real delay is the minimum that still
    /// distinguishes "delay applied" from "no delay" given normal scheduler
    /// jitter.
    #[tokio::test]
    async fn secondary_rate_limit_403_retries_with_delay() {
        use std::time::Instant;

        // Secondary-RL body: 403 with the secondary-rate-limit message.
        // NOTE: the `Retry-After: 2` header is present in the wire format
        // for realism (this is what GitHub sends), but it is NOT parsed by
        // our code. octocrab's typed error layer strips response headers
        // when it converts a non-2xx response into `GitHubError`, so the
        // header is architecturally inaccessible — see the module header
        // in `secondary_rate_limit.rs` for the full explanation. The retry
        // delay is driven by `ANODIZER_GITHUB_SECONDARY_RL_DELAY_SECS`
        // instead, set below.
        let body_403 = r#"{"message":"You have exceeded a secondary rate limit and have been temporarily blocked from content creation. Please retry your request again later.","documentation_url":"https://docs.github.com/rest/overview/resources-in-the-rest-api#secondary-rate-limits"}"#;
        let body_len = body_403.len();
        let resp_403 = Box::leak(
            format!(
                "HTTP/1.1 403 Forbidden\r\n\
                 Content-Type: application/json\r\n\
                 Retry-After: 2\r\n\
                 Content-Length: {body_len}\r\n\
                 \r\n\
                 {body_403}"
            )
            .into_boxed_str(),
        );
        let resp_200 =
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]";

        let (addr, calls) = spawn_oneshot_http_responder(vec![resp_403, resp_200]);
        let octo = build_test_octocrab(addr);

        // Tiny exp-backoff in policy; secondary-RL sleep is controlled by
        // the env var set below.
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };

        // Set secondary-RL delay to 1 s. With ±20 % jitter the actual sleep
        // is in [800 ms, 1.2 s); we assert >= 800 ms to prove the delay was
        // honored without paying a multi-second wall-clock cost per run.
        // SAFETY: test-only env mutation; unique key, brief window.
        unsafe {
            std::env::set_var("ANODIZER_GITHUB_SECONDARY_RL_DELAY_SECS", "1");
        }

        let t0 = Instant::now();
        let result: Result<Vec<serde_json::Value>, octocrab::Error> =
            retry_octocrab_call(&policy, "test upload", None, || async {
                octo.get("/test", None::<&()>).await
            })
            .await;
        let elapsed = t0.elapsed();

        unsafe {
            std::env::remove_var("ANODIZER_GITHUB_SECONDARY_RL_DELAY_SECS");
        }

        assert!(
            result.is_ok(),
            "403 secondary-RL must retry to success: {:?}",
            result.err()
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "expected exactly 2 calls: 1 secondary-RL 403 + 1 success 200"
        );
        // With 1 s base and ±20 % jitter, worst-case is 1 s * 0.8 = 800 ms.
        assert!(
            elapsed >= Duration::from_millis(800),
            "secondary-RL delay must hold for at least 800 ms (jitter floor is 80 % of 1 s; \
             elapsed: {elapsed:?})"
        );
    }
}
