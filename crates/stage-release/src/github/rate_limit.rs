//! Proactive GitHub API rate-limit checking.
//!
//! Before every PATCH/POST/PUT we hit `/rate_limit`; if the remaining quota
//! sits at or below `threshold` the loop sleeps until reset. Uses the
//! the secondary-rate-limit reset header (commit
//! `60028b19eb6845164ed7bac541032efe1b07fe14`, which made the wait
//! iterative + ctx-cancellable). The Go version uses `time.After(sleep)`
//! inside `select { case <-ctx.Done() ... }`; the Rust analog races the
//! timer against both SIGINT (`ctrl_c()`) and SIGTERM (`SignalKind::terminate()`,
//! Unix only) so the wait aborts promptly under either signal — important
//! for containerised CI runs that receive SIGTERM on cancellation.
//!
//! Note: `check_github_search_rate_limit` was deleted alongside the Search
//! API author-lookup removal (commit
//! `17315a556ef69444cf54ad27f623abf728472bc6` / parity item P3 — full sha
//! preserved so future rebases can't drift the reference). Re-introduce
//! it only if a future feature actually queries `/search/users`.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::release_log;
use anodizer_core::EnvSource;
#[cfg(test)]
use anodizer_core::ProcessEnvSource;

/// Async sleep callback signature used by [`check_github_rate_limit_with_sleep`].
/// Production callers pass [`tokio_sleep`]; tests inject a recorder that
/// captures the requested duration without blocking.
pub(crate) type SleepFn = Box<dyn Fn(Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send>;

/// Production sleep callback — delegates to [`tokio::time::sleep`].
pub(crate) fn tokio_sleep() -> SleepFn {
    Box::new(|d| Box::pin(tokio::time::sleep(d)))
}

/// Compute the seconds to sleep waiting for the GitHub rate-limit window
/// to reset. Returns `None` when `remaining > threshold` (no sleep needed).
/// Returns `Some(reset_epoch - now + 1)` when the reset is in the future,
/// and `Some(5)` (the past-reset floor) when `reset_epoch <= now`.
///
/// Pure — no IO, no clock reads. Callers pass `now` as the current epoch
/// seconds; the wrapper reads `SystemTime::now()`.
pub(crate) fn compute_rate_limit_sleep_secs(
    remaining: u64,
    threshold: u64,
    reset_epoch: u64,
    now: u64,
) -> Option<u64> {
    if remaining > threshold {
        return None;
    }
    let secs = if reset_epoch > now {
        reset_epoch - now + 1
    } else {
        5
    };
    Some(secs)
}

/// Resolve the GitHub REST API base URL through an injected env
/// source. Honors the undocumented `ANODIZER_GITHUB_API_BASE` override
/// so unit tests can redirect `/rate_limit` polls to an in-process
/// responder via a [`MapEnvSource`](anodizer_core::MapEnvSource);
/// defaults to the canonical `https://api.github.com` in production
/// where production callers pass [`ProcessEnvSource`] and the var is
/// unset. Trailing `/` is stripped so the caller can append a
/// `/`-prefixed suffix without producing a double slash. Mirrors the
/// sibling helper in `stage-publish/src/util/branch.rs`.
fn github_api_base_from<E: EnvSource + ?Sized>(env: &E) -> String {
    let raw = env
        .var("ANODIZER_GITHUB_API_BASE")
        .unwrap_or_else(|| "https://api.github.com".to_string());
    raw.trim_end_matches('/').to_string()
}

/// Proactively check the GitHub core rate limit before issuing a request.
///
/// If `remaining > threshold` returns immediately. Otherwise sleeps until the
/// reset epoch (plus a 1-second buffer), or until SIGINT (Ctrl-C) or SIGTERM
/// (Unix only) interrupts the wait — whichever is sooner.
///
/// Failures (transport, non-success response, malformed JSON) silently
/// degrade to "continue and hope for the best", matching the upstream
/// behaviour where `rateLimitChecker` logs and returns without aborting the
/// outer release flow.
///
/// Process-env-fed shim retained for the transport-failure test (which
/// pins the `silently degrade on connect refused` contract through the
/// default `ProcessEnvSource`).
#[cfg(test)]
pub(crate) async fn check_github_rate_limit(client: &reqwest::Client, token: &str, threshold: u64) {
    check_github_rate_limit_with_env(client, token, threshold, &ProcessEnvSource).await;
}

/// Env-injectable form of [`check_github_rate_limit`]. The production
/// entry point delegates to this via [`ProcessEnvSource`]; tests inject
/// a [`MapEnvSource`](anodizer_core::MapEnvSource) so the responder
/// address is read from the map instead of the process env.
///
/// Delegates to [`check_github_rate_limit_with_sleep`] with
/// [`tokio_sleep`] as the sleep callback.
pub(crate) async fn check_github_rate_limit_with_env<E: EnvSource + ?Sized>(
    client: &reqwest::Client,
    token: &str,
    threshold: u64,
    env: &E,
) {
    check_github_rate_limit_with_sleep(client, token, threshold, env, tokio_sleep()).await;
}

/// Fully-injectable form: env source **and** sleep callback are caller-
/// supplied. Production callers reach this through
/// [`check_github_rate_limit_with_env`] (which passes [`tokio_sleep`]);
/// tests inject a no-op recorder to verify sleep duration without
/// wall-clock delay.
pub(crate) async fn check_github_rate_limit_with_sleep<E: EnvSource + ?Sized>(
    client: &reqwest::Client,
    token: &str,
    threshold: u64,
    env: &E,
    sleep_fn: SleepFn,
) {
    let url = format!("{}/rate_limit", github_api_base_from(env));
    let resp = match client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", anodizer_core::http::USER_AGENT)
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return, // Can't check — continue and hope for the best
    };

    if !resp.status().is_success() {
        return;
    }

    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return,
    };

    let remaining = body
        .pointer("/resources/core/remaining")
        .and_then(|v| v.as_u64())
        .unwrap_or(u64::MAX);
    let reset_epoch = body
        .pointer("/resources/core/reset")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let sleep_secs = match compute_rate_limit_sleep_secs(remaining, threshold, reset_epoch, now) {
        None => return,
        Some(s) => s,
    };
    release_log().status(&format!(
        "rate limit almost reached ({remaining} remaining), sleeping for {sleep_secs}s..."
    ));

    let duration = Duration::from_secs(sleep_secs);

    // Reads the secondary-rate-limit reset header (commit
    // `60028b19eb6845164ed7bac541032efe1b07fe14`) — use a single `select`-
    // based wait so a cancellation signal aborts the sleep instead of
    // stalling the whole release for up to an hour. Race the timer against
    // both SIGINT (`ctrl_c()`) and SIGTERM (Unix only). On Windows there is
    // no SIGTERM equivalent reachable from `tokio::signal`; ctrl_c covers
    // the only console-cancel signal there.
    let sleep = sleep_fn(duration);
    tokio::pin!(sleep);
    #[cfg(unix)]
    {
        // `signal()` returns Result; on the rare init failure, fall back to
        // an always-pending future so the select still waits the timer +
        // SIGINT — never block the release on a missing SIGTERM listener.
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
        tokio::select! {
            _ = &mut sleep => {}
            _ = tokio::signal::ctrl_c() => {
                release_log().warn(
                    "rate-limit wait interrupted by SIGINT; release will likely fail \
                     on the next API call",
                );
            }
            _ = async {
                match sigterm.as_mut() {
                    Some(s) => { s.recv().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {
                release_log().warn(
                    "rate-limit wait interrupted by SIGTERM; release will likely fail \
                     on the next API call",
                );
            }
        }
    }
    #[cfg(not(unix))]
    {
        tokio::select! {
            _ = &mut sleep => {}
            _ = tokio::signal::ctrl_c() => {
                release_log().warn(
                    "rate-limit wait interrupted by Ctrl-C; release will likely fail \
                     on the next API call",
                );
            }
        }
    }
}

#[cfg(all(test, unix))]
mod sigterm_tests {
    //! Verify the SIGTERM-aware select arm actually fires when the process
    //! receives SIGTERM. We can't easily call `check_github_rate_limit`
    //! end-to-end without a fake GitHub server, but the load-bearing piece
    //! — `signal(SignalKind::terminate())` returning a stream that yields
    //! on a delivered SIGTERM — is testable in isolation.
    //!
    //! Sending the signal to our own PID is safe in test context: the
    //! tokio signal driver registers a handler that swallows the default
    //! "terminate the process" disposition. The race is bounded by a
    //! 2-second timeout so a regression (handler not installed, signal
    //! lost) fails loudly instead of hanging the test runner.
    use super::*;
    use tokio::signal::unix::{SignalKind, signal};

    #[tokio::test(flavor = "current_thread")]
    async fn sigterm_listener_observes_self_signal() {
        let mut sigterm = signal(SignalKind::terminate())
            .map_err(|e| format!("could not install SIGTERM handler: {e}"))
            .ok()
            .unwrap_or_else(|| panic!("SIGTERM handler install failed"));

        // Spawn a task that delivers SIGTERM to our own PID after a short
        // delay (via /usr/bin/kill, which avoids needing libc as a dev-dep
        // — the module-boundaries rule allow-lists `Command::new` in any
        // file under `crates/stage-*`). Tokio's signal driver has already
        // swapped in a handler that suppresses the default termination
        // disposition (proven by `signal(SignalKind::terminate())`
        // succeeding above), so the SIGTERM is observed, not fatal.
        let pid = std::process::id();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let _ = std::process::Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status();
        });

        let recv = sigterm.recv();
        let timeout = tokio::time::sleep(std::time::Duration::from_secs(2));
        tokio::select! {
            v = recv => {
                assert!(v.is_some(), "signal stream closed before SIGTERM delivered");
            }
            _ = timeout => {
                panic!("SIGTERM listener did not observe self-delivered signal within 2s");
            }
        }
    }

    #[test]
    fn sigterm_signal_kind_is_constructible() {
        // Cheap smoke test: prove the cfg-gated path's call site is
        // syntactically valid + the `terminate` SignalKind is a real
        // constant on this target. Catches accidental rename / removal in
        // future tokio releases without needing a tokio runtime.
        let _ = SignalKind::terminate();
    }

    /// Pin the silent-degrade contract: when the rate-limit HTTP request
    /// fails at the transport layer (connection refused), the function
    /// must return promptly instead of propagating or panicking.
    ///
    /// We point `api.github.com` at a TCP address that has just been
    /// closed (bind + drop the listener) so the `client.get(...).send()`
    /// future resolves to `Err(_)`, exercising the first `Err(_) => return`
    /// arm at line 42. The 5 s timeout bounds a regression: if the
    /// silent-degrade arm is removed or replaced with a `.unwrap()`, the
    /// task panics and the timeout fires.
    #[tokio::test]
    async fn transport_failure_silently_degrades() {
        // Acquire an ephemeral port then drop the listener — subsequent
        // connects to this address yield `Connection refused`.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local_addr");
        drop(listener);

        // Build a reqwest client whose DNS resolution maps
        // `api.github.com` to the now-closed loopback port. The default
        // base URL (`https://api.github.com`) the function builds when
        // `ANODIZER_GITHUB_API_BASE` is unset then resolves to a TCP
        // connect that fails immediately.
        let client = reqwest::Client::builder()
            .resolve("api.github.com", addr)
            .build()
            .expect("reqwest client builds");

        let fut = check_github_rate_limit(&client, "fake-token", 100);
        // 5 s upper bound — Linux returns `ECONNREFUSED` synchronously,
        // so the silent-degrade arm should fire in <1 s. If we hang, the
        // function violated its no-panic / no-bubble contract.
        let res = tokio::time::timeout(std::time::Duration::from_secs(5), fut).await;
        assert!(
            res.is_ok(),
            "check_github_rate_limit must silently degrade on transport failure, not hang"
        );
    }
}

#[cfg(test)]
mod test_helpers {
    use anodizer_core::MapEnvSource;

    pub(super) fn env_with_base(base: &str) -> MapEnvSource {
        MapEnvSource::new().with("ANODIZER_GITHUB_API_BASE", base)
    }

    pub(super) fn canned_json_200(body: &str) -> &'static str {
        let raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body,
        );
        Box::leak(raw.into_boxed_str())
    }
}

#[cfg(test)]
mod http_tests {
    //! End-to-end coverage of the `/rate_limit` HTTP path.
    //!
    //! Each test injects `ANODIZER_GITHUB_API_BASE` through a
    //! [`MapEnvSource`] passed to
    //! [`check_github_rate_limit_with_env`] so the production code's
    //! call to `https://api.github.com/rate_limit` is intercepted
    //! without touching the real GitHub API and without mutating the
    //! process env. Tests run in full isolation — no env mutex
    //! acquisition, no shared state between sibling tests.
    //!
    //! Sleep-path coverage is split: the pure sleep-secs computation is
    //! exercised exhaustively via `compute_rate_limit_sleep_secs` (see
    //! `compute_tests` below) so the branch logic doesn't entangle with
    //! tokio's runtime; the wiring that actually sleeps + races against
    //! signals is covered by `e2e_sleeps_briefly_when_reset_is_one_second_away`
    //! using a sub-second reset window so the wall-clock cost is bounded.
    use super::test_helpers::{canned_json_200, env_with_base};
    use super::*;
    use anodizer_core::MapEnvSource;
    use anodizer_core::test_helpers::https_responder::{
        https_test_client, spawn_oneshot_https_responder,
    };
    use std::sync::atomic::Ordering;

    /// `remaining > threshold` means quota is available, so the
    /// function returns without sleeping after exactly one HTTP
    /// round-trip. A regression that flipped the comparison operator
    /// (e.g. `>=` to `<=`) would sleep ~3600 s on every release; the
    /// bounded `timeout` here fires fast instead.
    #[tokio::test]
    async fn remaining_above_threshold_returns_without_sleep() {
        let body = r#"{"resources":{"core":{"remaining":5000,"reset":9999999999,"limit":5000}}}"#;
        let (addr, calls) = spawn_oneshot_https_responder(vec![canned_json_200(body)]);
        let env = env_with_base(&format!("https://{addr}"));

        let client = https_test_client();
        let fut = check_github_rate_limit_with_env(&client, "fake-token", 100, &env);
        // 5 s upper bound — happy path is sub-second. Hitting this
        // means the function sleeps when it shouldn't.
        let res = tokio::time::timeout(std::time::Duration::from_secs(5), fut).await;
        assert!(
            res.is_ok(),
            "must return promptly when remaining > threshold"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "exactly one /rate_limit request should be issued"
        );
    }

    /// Non-2xx response (401 Unauthorized) must silently degrade — the
    /// production contract is "can't check, continue and hope for the
    /// best" since aborting a release on a transient `/rate_limit` 401
    /// would be worse than the rare overrun. A regression that
    /// propagated the error or panicked on `.json()` parsing of an
    /// error-page body would fail the bounded timeout.
    #[tokio::test]
    async fn non_success_status_silently_degrades() {
        let resp = "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n";
        let (addr, calls) = spawn_oneshot_https_responder(vec![resp]);
        let env = env_with_base(&format!("https://{addr}"));

        let client = https_test_client();
        let fut = check_github_rate_limit_with_env(&client, "fake-token", 100, &env);
        let res = tokio::time::timeout(std::time::Duration::from_secs(5), fut).await;
        assert!(res.is_ok(), "401 must silently degrade, not hang or panic");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// 500 walks the same `!is_success()` branch as 401 — pinned
    /// separately because GitHub can return either on a transient
    /// outage. A regression that added retry logic to this function
    /// would inflate the call count above 1.
    #[tokio::test]
    async fn server_error_status_silently_degrades() {
        let resp = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n";
        let (addr, calls) = spawn_oneshot_https_responder(vec![resp]);
        let env = env_with_base(&format!("https://{addr}"));

        let client = https_test_client();
        let fut = check_github_rate_limit_with_env(&client, "fake-token", 100, &env);
        let res = tokio::time::timeout(std::time::Duration::from_secs(5), fut).await;
        assert!(res.is_ok(), "500 must silently degrade");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Malformed JSON in a 200 response must silently degrade —
    /// common in the wild when an auth proxy intercepts the request
    /// and returns an HTML error page with `200`. A regression that
    /// unwrapped the parse Result would panic the calling task and
    /// abort the release. A single `{` ensures `serde_json` rejects
    /// at the first token.
    #[tokio::test]
    async fn malformed_json_body_silently_degrades() {
        let body = "{";
        let raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body,
        );
        let resp: &'static str = Box::leak(raw.into_boxed_str());
        let (addr, calls) = spawn_oneshot_https_responder(vec![resp]);
        let env = env_with_base(&format!("https://{addr}"));

        let client = https_test_client();
        let fut = check_github_rate_limit_with_env(&client, "fake-token", 100, &env);
        let res = tokio::time::timeout(std::time::Duration::from_secs(5), fut).await;
        assert!(
            res.is_ok(),
            "malformed JSON must silently degrade, not panic"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Missing JSON pointers (`/resources/core/remaining`) must
    /// `unwrap_or(u64::MAX)` — i.e. treat the response as "infinite
    /// quota" and skip the sleep. This pins the defensive default; a
    /// regression to `unwrap_or(0)` would force every release to sleep
    /// on a schema drift.
    #[tokio::test]
    async fn missing_pointer_fields_skip_sleep() {
        let body = r#"{"resources":{"other":{"remaining":1}}}"#;
        let (addr, calls) = spawn_oneshot_https_responder(vec![canned_json_200(body)]);
        let env = env_with_base(&format!("https://{addr}"));

        let client = https_test_client();
        let fut = check_github_rate_limit_with_env(&client, "fake-token", 100, &env);
        let res = tokio::time::timeout(std::time::Duration::from_secs(5), fut).await;
        assert!(
            res.is_ok(),
            "missing JSON pointer must fall back to u64::MAX (no sleep)"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// `github_api_base_from` strips a trailing slash so callers can
    /// unconditionally append `/rate_limit` without producing
    /// `//rate_limit` (which 404s on GitHub). Mirror of the contract
    /// pinned in `stage-publish/src/util/branch.rs`.
    #[test]
    fn base_url_strips_trailing_slash() {
        let env = env_with_base("https://example.com/api/");
        assert_eq!(github_api_base_from(&env), "https://example.com/api");
    }

    /// Default base URL when the env source has no override is the
    /// canonical `https://api.github.com` — pins the production default
    /// so a regression to a typo'd host doesn't ship silently. The empty
    /// [`MapEnvSource`] mimics a production process where the env var
    /// has never been exported.
    #[test]
    fn base_url_defaults_to_api_github_com() {
        let env = MapEnvSource::new();
        assert_eq!(github_api_base_from(&env), "https://api.github.com");
    }

    /// Drive `check_github_rate_limit` through the real sleep + select
    /// path with a ~1 s reset window. The pure helper covers the
    /// branch logic; this test pins the WIRING — that the function
    /// actually awaits `sleep_secs`, races it against the signal arms,
    /// and resumes cleanly. A regression that dropped the `.await` (or
    /// swapped `select!` for a bare `sleep.await` then forgot to wake)
    /// would fail the 8 s bound. Sleep duration is bounded above by
    /// roughly 2 s (1 s reset window + 1 s buffer added by
    /// `compute_rate_limit_sleep_secs`); the 8 s timeout leaves ample
    /// headroom for CI scheduler jitter.
    #[tokio::test]
    async fn e2e_sleeps_briefly_when_reset_is_one_second_away() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock is post-epoch")
            .as_secs();
        let reset = now + 1;
        let body = format!(
            r#"{{"resources":{{"core":{{"remaining":0,"reset":{reset},"limit":5000}}}}}}"#,
        );
        let (addr, calls) =
            spawn_oneshot_https_responder(vec![canned_json_200(Box::leak(body.into_boxed_str()))]);
        let env = env_with_base(&format!("https://{addr}"));

        let client = https_test_client();
        let fut = check_github_rate_limit_with_env(&client, "fake-token", 100, &env);
        let res = tokio::time::timeout(std::time::Duration::from_secs(8), fut).await;
        assert!(
            res.is_ok(),
            "must complete the sleep + select wait within the bounded window"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "exactly one /rate_limit request should be issued"
        );
    }
}

#[cfg(test)]
mod compute_tests {
    //! Exhaustive coverage of the pure sleep-secs computation. No IO,
    //! no clock reads — every branch is reachable by varying the four
    //! inputs. The end-to-end wiring (HTTP poll + tokio sleep + signal
    //! select) is covered by `http_tests::e2e_sleeps_briefly_when_reset_is_one_second_away`.
    use super::*;

    /// `remaining > threshold` short-circuits with `None` — the
    /// happy-path that bypasses sleep entirely. A regression flipping
    /// `>` to `<` would have every release sleep through the reset
    /// window even with full quota.
    #[test]
    fn compute_returns_none_when_remaining_above_threshold() {
        // `reset_epoch` and `now` are deliberately implausible (max +
        // min) to prove they're unused on this branch.
        assert_eq!(compute_rate_limit_sleep_secs(101, 100, u64::MAX, 0), None,);
    }

    /// Future reset returns `(reset - now) + 1` — the +1 buffer
    /// so a release doesn't retry at
    /// the exact reset instant and race the upstream window flip.
    #[test]
    fn compute_returns_future_reset_diff_plus_one_when_reset_in_future() {
        assert_eq!(compute_rate_limit_sleep_secs(0, 100, 1000, 500), Some(501),);
    }

    /// Reset strictly in the past AND reset == now both fall to the
    /// 5 s floor. The boundary matters: a regression to `>=` would
    /// underflow `reset_epoch - now` when they're equal (or yield 1 s
    /// instead of 5 s on the equal case), shrinking the retry budget
    /// below the upstream window's grace.
    #[test]
    fn compute_returns_past_reset_floor_when_reset_in_past_or_equal_to_now() {
        // Reset strictly in the past.
        assert_eq!(compute_rate_limit_sleep_secs(0, 100, 500, 1000), Some(5),);
        // Reset exactly equal to now — pinned because the production
        // code branches on `reset_epoch > now`, not `>=`.
        assert_eq!(compute_rate_limit_sleep_secs(0, 100, 1000, 1000), Some(5),);
    }

    /// `remaining == threshold` is the inclusive boundary — equal
    /// counts as "depleted" and triggers the sleep. A regression to
    /// `>=` would skip the sleep at the exact threshold and overrun
    /// the next call.
    #[test]
    fn compute_returns_some_when_remaining_equals_threshold() {
        assert_eq!(
            compute_rate_limit_sleep_secs(100, 100, 2000, 1000),
            Some(1001),
        );
    }

    /// Zero remaining is the canonical depleted case; pinned
    /// separately from the threshold boundary because real GitHub
    /// responses often hit zero (not threshold-exact) before the
    /// pre-flight check fires.
    #[test]
    fn compute_returns_some_when_remaining_is_zero() {
        assert_eq!(
            compute_rate_limit_sleep_secs(0, 100, 2000, 1000),
            Some(1001),
        );
    }
}

#[cfg(test)]
mod sleep_injection_tests {
    //! Verify the sleep callback injection wiring.
    //!
    //! These tests use [`check_github_rate_limit_with_sleep`] with a
    //! no-op sleep recorder that captures the requested [`Duration`]
    //! without blocking. The pure branch logic is already covered by
    //! `compute_tests`; these tests pin that the wiring between the
    //! HTTP-response parser and the sleep callback is correct — i.e.
    //! the computed duration actually reaches the injected callback.
    use super::test_helpers::{canned_json_200, env_with_base};
    use super::*;
    use anodizer_core::test_helpers::https_responder::{
        https_test_client, spawn_oneshot_https_responder,
    };
    use std::sync::{Arc, Mutex};

    /// Build a [`SleepFn`] that records every requested duration into
    /// the returned `Arc<Mutex<Vec<Duration>>>` and resolves immediately.
    fn recording_sleep() -> (SleepFn, Arc<Mutex<Vec<Duration>>>) {
        let log: Arc<Mutex<Vec<Duration>>> = Arc::new(Mutex::new(Vec::new()));
        let log_clone = Arc::clone(&log);
        let f: SleepFn = Box::new(move |d| {
            log_clone.lock().unwrap().push(d);
            Box::pin(async {})
        });
        (f, log)
    }

    /// When `remaining <= threshold` and `reset_epoch > now`, the
    /// function must sleep for `(reset_epoch - now + 1)` seconds. The
    /// injected recorder captures the duration so we can assert the
    /// exact value without wall-clock delay.
    #[tokio::test]
    async fn sleep_until_future_reset_records_correct_duration() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock is post-epoch")
            .as_secs();
        let reset = now + 600; // 10 minutes from now
        let body = format!(
            r#"{{"resources":{{"core":{{"remaining":0,"reset":{reset},"limit":5000}}}}}}"#,
        );
        let (addr, _calls) =
            spawn_oneshot_https_responder(vec![canned_json_200(Box::leak(body.into_boxed_str()))]);
        let env = env_with_base(&format!("https://{addr}"));
        let client = https_test_client();

        let (sleep_fn, log) = recording_sleep();
        check_github_rate_limit_with_sleep(&client, "fake-token", 100, &env, sleep_fn).await;

        let recorded = log.lock().unwrap();
        assert_eq!(
            recorded.len(),
            1,
            "sleep callback must be invoked exactly once"
        );
        // `compute_rate_limit_sleep_secs` adds +1 to the diff.
        // The exact value depends on wall-clock `now` read inside the
        // function; bound it within a 2-second tolerance window.
        let secs = recorded[0].as_secs();
        assert!(
            (600..=602).contains(&secs),
            "expected ~601s sleep, got {secs}s",
        );
    }

    /// When `reset_epoch <= now` (reset already passed), the 5-second
    /// floor applies. Pins the past-reset branch through the full
    /// HTTP + sleep-injection path.
    #[tokio::test]
    async fn past_reset_records_five_second_floor() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock is post-epoch")
            .as_secs();
        let reset = now.saturating_sub(100); // 100 seconds in the past
        let body = format!(
            r#"{{"resources":{{"core":{{"remaining":0,"reset":{reset},"limit":5000}}}}}}"#,
        );
        let (addr, _calls) =
            spawn_oneshot_https_responder(vec![canned_json_200(Box::leak(body.into_boxed_str()))]);
        let env = env_with_base(&format!("https://{addr}"));
        let client = https_test_client();

        let (sleep_fn, log) = recording_sleep();
        check_github_rate_limit_with_sleep(&client, "fake-token", 100, &env, sleep_fn).await;

        let recorded = log.lock().unwrap();
        assert_eq!(
            recorded.len(),
            1,
            "sleep callback must be invoked exactly once"
        );
        assert_eq!(
            recorded[0],
            Duration::from_secs(5),
            "past-reset branch must sleep exactly 5s",
        );
    }

    /// When `remaining > threshold`, no sleep occurs at all. The
    /// recorder must remain empty.
    #[tokio::test]
    async fn no_sleep_when_remaining_above_threshold() {
        let body = r#"{"resources":{"core":{"remaining":5000,"reset":9999999999,"limit":5000}}}"#;
        let (addr, _calls) = spawn_oneshot_https_responder(vec![canned_json_200(body)]);
        let env = env_with_base(&format!("https://{addr}"));
        let client = https_test_client();

        let (sleep_fn, log) = recording_sleep();
        check_github_rate_limit_with_sleep(&client, "fake-token", 100, &env, sleep_fn).await;

        let recorded = log.lock().unwrap();
        assert!(
            recorded.is_empty(),
            "sleep callback must not be invoked when remaining > threshold",
        );
    }
}
