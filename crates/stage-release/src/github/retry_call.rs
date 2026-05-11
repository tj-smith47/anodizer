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
use std::ops::ControlFlow;

use anodizer_core::retry::{RetryPolicy, is_retriable, retry_async};

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
pub(crate) async fn retry_octocrab_call<T, F, Fut>(
    policy: &RetryPolicy,
    label: &'static str,
    mut make_call: F,
) -> Result<T, octocrab::Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, octocrab::Error>>,
{
    let max = policy.max_attempts;
    retry_async(policy, |attempt| {
        let fut = make_call();
        async move {
            match fut.await {
                Ok(v) => Ok(v),
                Err(err) => {
                    // Classify retriability without consuming the original
                    // error: build a temporary wrapper from a clone-shaped
                    // probe (status code) so the unmodified `err` can be
                    // returned to the caller. `octocrab::Error` is not
                    // `Clone`, so we extract just the bits the classifier
                    // needs from a borrow.
                    let (status, retriable) = classify_retriability(&err);
                    if retriable {
                        release_log().warn(&format_retry_warn(label, attempt, max, status));
                        Err(ControlFlow::Continue(err))
                    } else {
                        Err(ControlFlow::Break(err))
                    }
                }
            }
        }
    })
    .await
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
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    /// Bind a loopback listener and feed each accepted connection one
    /// scripted HTTP response, in order. Returns the listener address plus
    /// an atomic connection counter so tests can assert the retry count.
    fn spawn_oneshot_http_responder(responses: Vec<&'static str>) -> (SocketAddr, Arc<AtomicU32>) {
        let listener =
            TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port for retry-helper test");
        let addr = listener
            .local_addr()
            .expect("local_addr on freshly bound listener");
        let counter = Arc::new(AtomicU32::new(0));
        let counter_inner = counter.clone();
        std::thread::spawn(move || {
            for (i, resp) in responses.iter().enumerate() {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(_) => return,
                };
                counter_inner.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 8192];
                let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
                let _ = stream.shutdown(std::net::Shutdown::Both);
                if i == responses.len() - 1 {
                    break;
                }
            }
        });
        (addr, counter)
    }

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
            retry_octocrab_call(&policy, "test list", || async {
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
            retry_octocrab_call(&policy, "test list", || async {
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
            retry_octocrab_call(&policy, "test list", || async {
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
}
