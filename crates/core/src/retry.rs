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
}
