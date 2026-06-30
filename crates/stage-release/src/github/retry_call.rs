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
//! The bespoke `upload_asset` retry loop in `upload.rs` cannot route through
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
/// (the helper here AND the bespoke upload-asset loop in `upload.rs`).
///
/// Extracted so the two retry pathways can't drift on label format. The
/// `format_retry_warn_shape_pins_shared_format` test below pins the exact
/// format string both pathways emit.
///
/// A `status` of `0` denotes a transport-layer failure where no HTTP response
/// was received. Rendering a bare `status=0` reads as a success code, so that
/// case is spelled out as `transport error (no HTTP response)` instead; a real
/// HTTP status (`>0`) is shown as `status=<code>`. Either way the line ends in
/// `; will retry` so the operator reads it unambiguously as "this attempt
/// failed, retrying".
pub(crate) fn format_retry_warn(label: &str, attempt: u32, max: u32, status: u16) -> String {
    let cause = if status == 0 {
        "transport error (no HTTP response)".to_string()
    } else {
        format!("status={status}")
    };
    format!("{label} failed (attempt {attempt}/{max}, {cause}); will retry")
}

/// Closing line after a retry loop resolves to SUCCESS on attempt `attempts`
/// (only emitted when `attempts > 1`, i.e. at least one retry was needed — a
/// first-try success stays silent). Closes the gap where the operator saw the
/// penultimate attempt's warning and then nothing.
pub(crate) fn format_retry_succeeded(label: &str, attempts: u32) -> String {
    format!("{label} succeeded after {attempts} attempt(s)")
}

/// Closing line after a retry loop EXHAUSTS every attempt and gives up,
/// emitted before the error propagates so the operator sees a definite
/// terminal outcome rather than silence after the last per-attempt warning.
pub(crate) fn format_retry_giving_up(label: &str, attempts: u32) -> String {
    format!("{label} failed after {attempts} attempt(s), giving up")
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
            Ok(v) => {
                // Close the loop: a success that needed >1 attempt gets a
                // single confirming line so the operator who saw the prior
                // attempts' warnings sees the resolution rather than silence.
                // A first-try success stays silent (no retry happened).
                if attempt > 1 {
                    release_log().status(&format_retry_succeeded(label, attempt));
                }
                return Ok(v);
            }
            Err(err) => {
                let secondary_rl = is_secondary_rate_limit(&err);
                let (status, retriable) = classify_retriability(&err);
                // A secondary rate-limit 403 is not retriable by the default
                // classifier (which only retries 5xx/429), but it IS a
                // transient condition that must be retried after a delay. A
                // non-retriable error fast-fails WITHOUT a "giving up" line:
                // that closing line marks retry EXHAUSTION, not a clean
                // fast-fail (which surfaces its own error directly).
                if !retriable && !secondary_rl {
                    return Err(err);
                }
                release_log().warn(&format_retry_warn(label, attempt, max, status));
                if attempt >= max {
                    // Exhausted every retry: emit a definite terminal line
                    // before the error propagates.
                    release_log().warn(&format_retry_giving_up(label, attempt));
                    return Err(err);
                }
                // Secondary rate-limit: sleep the dedicated RL delay (with
                // jitter) instead of the policy's exp-backoff delay.
                if secondary_rl {
                    let delay = jitter_duration(secondary_rl_delay(retry_after));
                    release_log().warn(&format!(
                        "{label} hit GitHub secondary rate limit; \
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
    if let octocrab::Error::GitHub { source, .. } = err {
        let status = source.status_code.as_u16();
        let probe = HttpError::new(std::io::Error::other("status probe"), status);
        (status, is_retriable(&probe))
    } else if is_octocrab_transport_error(err) {
        let probe = Retriable::new(std::io::Error::other("transport probe"));
        (0, is_retriable(&probe))
    } else {
        // Conservative default: unfamiliar future variants fast-fail
        // rather than spin. Matches `classify_octocrab_error`'s fallback.
        (0, false)
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

/// Single classifier for retriable octocrab transport/transient errors.
///
/// Returns `true` for the network-layer / proxy / decode failure variants
/// that carry no HTTP status and are always safe to retry against a healthy
/// GitHub origin: `Hyper`, `Http`, `Service`, `Other`, `Serde`, `Json`.
/// (`Serde` / `Json` count because GitHub occasionally serves an HTML
/// 502/503 interstitial that breaks octocrab's JSON decode — a transient
/// proxy failure, not a malformed contract.) Status-bearing `GitHub`
/// errors and unfamiliar future variants return `false`: their
/// retriability is decided by HTTP status, not transport class.
///
/// This is the one predicate every GitHub-backend retry site shares (the
/// borrow-based probe in [`classify_retriability`], the upload-attempt
/// classifier in [`super::upload_outcome`], and the test oracle in
/// [`super::retry_classify`]) so a new upstream octocrab variant is
/// classified identically everywhere instead of drifting per copy.
pub(crate) fn is_octocrab_transport_error(err: &octocrab::Error) -> bool {
    matches!(
        err,
        octocrab::Error::Hyper { .. }
            | octocrab::Error::Http { .. }
            | octocrab::Error::Service { .. }
            | octocrab::Error::Other { .. }
            | octocrab::Error::Serde { .. }
            | octocrab::Error::Json { .. }
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
    use crate::test_support::build_test_octocrab;
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

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

    #[tokio::test]
    async fn is_octocrab_transport_error_true_for_transport_and_decode_false_for_404() {
        // Direct coverage for the predicate the upload path now shares: a
        // real transport-class error and a JSON-decode failure must classify
        // as transport (retriable), while a status-bearing GitHub 404 must
        // not (its retriability is the 404 router's job, not the transport
        // bucket's). Without this, the upload-path copy of the variant set
        // had zero tests and a new octocrab variant could go unnoticed.

        // Transport-class error: an unresolvable RFC 2606 `.invalid` host
        // fails fast at the connector on every platform, yielding a
        // `Service`/`Hyper` variant with no HTTP status attached.
        anodizer_core::tls::install_default_crypto_provider();
        let octo = octocrab::OctocrabBuilder::new()
            .base_uri("http://nonexistent.invalid/")
            .expect("base_uri must accept RFC 2606 .invalid URL")
            .build()
            .expect("OctocrabBuilder::build");
        let transport_err = octo
            .get::<serde_json::Value, _, ()>("/", None::<&()>)
            .await
            .expect_err("request against .invalid host must fail at the connector");
        assert!(
            is_octocrab_transport_error(&transport_err),
            "real transport-class octocrab error must classify as transport: {transport_err:?}"
        );
        assert!(
            !is_octocrab_404(&transport_err),
            "a transport error carries no HTTP status and is not a 404"
        );

        // Decode-class error: a 200 with a non-JSON body makes octocrab fail
        // to deserialize into the requested type, producing a `Serde`/`Json`
        // variant — also part of the retriable transport set (GitHub serves
        // HTML interstitials from upstream proxies under load).
        let (addr, _calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 8\r\n\r\nnot-json",
        ]);
        let octo = build_test_octocrab(addr);
        let decode_err = octo
            .get::<serde_json::Value, _, ()>("/test", None::<&()>)
            .await
            .expect_err("invalid JSON body must surface a decode Err");
        assert!(
            is_octocrab_transport_error(&decode_err),
            "a JSON-decode failure (Serde/Json) is transport-class: {decode_err:?}"
        );

        // Status-bearing GitHub 404: NOT a transport error. The 404 predicate
        // owns it; the transport predicate must reject it so the upload path
        // routes it to its dedicated read-after-write retry arm.
        let (addr, _calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: 27\r\n\r\n{\"message\":\"Not Found\"}    ",
        ]);
        let octo = build_test_octocrab(addr);
        let github_404 = octo
            .get::<serde_json::Value, _, ()>("/test", None::<&()>)
            .await
            .expect_err("404 must surface as Err");
        assert!(
            !is_octocrab_transport_error(&github_404),
            "a status-bearing GitHub 404 is not a transport error: {github_404:?}"
        );
        assert!(
            is_octocrab_404(&github_404),
            "the 404 predicate must still recognise the GitHub 404"
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
            "delete release failed (attempt 3/10, status=503); will retry"
        );
    }

    #[test]
    fn format_retry_warn_status_zero_reads_as_transport_error() {
        // A transport-layer failure (no HTTP response) carries status 0. A
        // bare `status=0` reads as an HTTP success code; the warning must
        // instead name the transport error explicitly and never contain a
        // misleading `status=0`.
        let s = format_retry_warn("create release", 1, 10, 0);
        assert_eq!(
            s,
            "create release failed (attempt 1/10, transport error (no HTTP response)); will retry"
        );
        assert!(
            !s.contains("status=0"),
            "transport-error warning must not contain a misleading `status=0`: {s}"
        );
        assert!(
            s.contains("will retry"),
            "per-attempt warning must read as a retry, not a terminal failure: {s}"
        );
    }

    #[test]
    fn format_retry_succeeded_shape() {
        // The closing success line emitted only when >1 attempt was needed.
        assert_eq!(
            format_retry_succeeded("create release", 3),
            "create release succeeded after 3 attempt(s)"
        );
    }

    #[test]
    fn format_retry_giving_up_shape() {
        // The closing exhaustion line emitted before the error propagates.
        assert_eq!(
            format_retry_giving_up("create release", 10),
            "create release failed after 10 attempt(s), giving up"
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
    #[serial_test::serial(secondary_rl_env)]
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
        // The delay is read process-globally deep inside the async retry loop
        // (`retry_octocrab_call` → `secondary_rl_delay`), which does not thread
        // an `EnvSource`; the `serial(secondary_rl_env)` attribute serializes
        // this mutation against any other test touching the same var.
        // SAFETY: test-only env mutation; unique key, serialized window.
        unsafe {
            // env-ok: #[serial(secondary_rl_env)]; sole mutator of this var
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
            // env-ok: #[serial(secondary_rl_env)]; sole mutator of this var
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
