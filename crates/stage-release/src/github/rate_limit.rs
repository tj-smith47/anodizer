//! Proactive GitHub API rate-limit checking.
//!
//! Before every PATCH/POST/PUT we hit `/rate_limit`; if the remaining quota
//! sits at or below `threshold` we sleep until reset. Mirrors GoReleaser's
//! `internal/client/github.go::checkRateLimit` (PR #6540, commit
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

use crate::release_log;

/// Resolve the GitHub REST API base URL. Honors the undocumented
/// `ANODIZER_GITHUB_API_BASE` env override so unit tests can redirect
/// `/rate_limit` polls to an in-process responder; defaults to the
/// canonical `https://api.github.com` in production where the var is
/// unset. Trailing `/` is stripped so the caller can append a
/// `/`-prefixed suffix without producing a double slash. Mirrors the
/// sibling helper in `stage-publish/src/util/branch.rs`.
fn github_api_base() -> String {
    let raw = std::env::var("ANODIZER_GITHUB_API_BASE")
        .unwrap_or_else(|_| "https://api.github.com".to_string());
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
pub(crate) async fn check_github_rate_limit(client: &reqwest::Client, token: &str, threshold: u64) {
    let url = format!("{}/rate_limit", github_api_base());
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

    if remaining > threshold {
        return;
    }

    // Sleep until reset + small buffer.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let sleep_secs = if reset_epoch > now {
        reset_epoch - now + 1
    } else {
        5 // Minimum 5 seconds if reset is in the past
    };
    release_log().status(&format!(
        "rate limit almost reached ({remaining} remaining), sleeping for {sleep_secs}s..."
    ));

    // Mirrors GoReleaser PR #6540 (commit
    // `60028b19eb6845164ed7bac541032efe1b07fe14`) — use a single `select`-
    // based wait so a cancellation signal aborts the sleep instead of
    // stalling the whole release for up to an hour. Race the timer against
    // both SIGINT (`ctrl_c()`) and SIGTERM (Unix only). On Windows there is
    // no SIGTERM equivalent reachable from `tokio::signal`; ctrl_c covers
    // the only console-cancel signal there.
    let sleep = tokio::time::sleep(std::time::Duration::from_secs(sleep_secs));
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
mod http_tests {
    //! End-to-end coverage of the `/rate_limit` HTTP path.
    //!
    //! Each test points `ANODIZER_GITHUB_API_BASE` at an in-process
    //! HTTPS responder so the production code's call to
    //! `https://api.github.com/rate_limit` is intercepted without
    //! touching the real GitHub API. The env var is serialised under
    //! the workspace `env_mutex()` because `cargo test` parallelises
    //! within a single binary and sibling tests in this module read
    //! the same key.
    //!
    //! The `start_paused` virtual-time runtime is only used for the
    //! sleep-until-reset test, which asserts the function sleeps
    //! through the configured reset window without paying a real
    //! wall-clock minute per run. All other tests use the default
    //! runtime since they exercise return-fast paths.
    use super::*;
    use anodizer_core::test_helpers::env::env_mutex;
    use anodizer_core::test_helpers::https_responder::{
        https_test_client, spawn_oneshot_https_responder,
    };
    use std::sync::atomic::Ordering;

    /// RAII guard: acquire the workspace env mutex, set
    /// `ANODIZER_GITHUB_API_BASE` to `base`, restore the prior value on
    /// drop. The mutex prevents a sibling test in this binary from
    /// observing a half-swapped env. Unwinding through a panicking test
    /// body restores the env (Drop runs on stack unwind) so a regression
    /// doesn't leak the override into the next test.
    struct BaseOverride {
        _guard: std::sync::MutexGuard<'static, ()>,
        previous: Option<String>,
    }

    impl BaseOverride {
        fn set(base: &str) -> Self {
            let guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
            let previous = std::env::var("ANODIZER_GITHUB_API_BASE").ok();
            // SAFETY: serialised by the env mutex held in `_guard`.
            unsafe { std::env::set_var("ANODIZER_GITHUB_API_BASE", base) };
            Self {
                _guard: guard,
                previous,
            }
        }
    }

    impl Drop for BaseOverride {
        fn drop(&mut self) {
            // SAFETY: still under the env mutex (held by `_guard`).
            unsafe {
                match &self.previous {
                    Some(prev) => std::env::set_var("ANODIZER_GITHUB_API_BASE", prev),
                    None => std::env::remove_var("ANODIZER_GITHUB_API_BASE"),
                }
            }
        }
    }

    /// Build a canned `200 OK` HTTPS response carrying the given JSON
    /// body. `Content-Length` is auto-derived so callers can't drift
    /// header and body out of sync (an early flake in retry-call tests
    /// before the helper landed).
    ///
    /// Returns `&'static str` because `spawn_oneshot_https_responder`
    /// requires `'static` lifetimes; `Box::leak` is the established
    /// idiom across this codebase (see
    /// `crates/stage-release/src/github/retry_call.rs::secondary_rate_limit_403_retries_with_delay`)
    /// and the per-test leak is bounded by test-binary lifetime.
    fn canned_json_200(body: &str) -> &'static str {
        let raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body,
        );
        Box::leak(raw.into_boxed_str())
    }

    /// `remaining > threshold` means quota is available, so the
    /// function returns without sleeping after exactly one HTTP
    /// round-trip. A regression that flipped the comparison operator
    /// (e.g. `>=` to `<=`) would sleep ~3600 s on every release; the
    /// bounded `timeout` here fires fast instead.
    #[tokio::test]
    async fn remaining_above_threshold_returns_without_sleep() {
        let body = r#"{"resources":{"core":{"remaining":5000,"reset":9999999999,"limit":5000}}}"#;
        let (addr, calls) = spawn_oneshot_https_responder(vec![canned_json_200(body)]);
        let _ov = BaseOverride::set(&format!("https://{addr}"));

        let client = https_test_client();
        let fut = check_github_rate_limit(&client, "fake-token", 100);
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
        let _ov = BaseOverride::set(&format!("https://{addr}"));

        let client = https_test_client();
        let fut = check_github_rate_limit(&client, "fake-token", 100);
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
        let _ov = BaseOverride::set(&format!("https://{addr}"));

        let client = https_test_client();
        let fut = check_github_rate_limit(&client, "fake-token", 100);
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
        let _ov = BaseOverride::set(&format!("https://{addr}"));

        let client = https_test_client();
        let fut = check_github_rate_limit(&client, "fake-token", 100);
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
        let _ov = BaseOverride::set(&format!("https://{addr}"));

        let client = https_test_client();
        let fut = check_github_rate_limit(&client, "fake-token", 100);
        let res = tokio::time::timeout(std::time::Duration::from_secs(5), fut).await;
        assert!(
            res.is_ok(),
            "missing JSON pointer must fall back to u64::MAX (no sleep)"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// `github_api_base()` strips a trailing slash so callers can
    /// unconditionally append `/rate_limit` without producing
    /// `//rate_limit` (which 404s on GitHub). Mirror of the contract
    /// pinned in `stage-publish/src/util/branch.rs`; tested via the
    /// public override path because the helper itself is module-private.
    #[test]
    fn base_url_strips_trailing_slash() {
        let _ov = BaseOverride::set("https://example.com/api/");
        assert_eq!(github_api_base(), "https://example.com/api");
    }

    /// Default base URL when the env var is unset is the canonical
    /// `https://api.github.com` — pins the production default so a
    /// regression to a typo'd host doesn't ship silently (it would
    /// fail to find the responder under the override too, but the
    /// blast radius matters: prod calls would be misdirected for every
    /// user who doesn't set the override).
    #[test]
    fn base_url_defaults_to_api_github_com() {
        // Acquire the env mutex and clear the var to assert the
        // unset-default branch.
        let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var("ANODIZER_GITHUB_API_BASE").ok();
        // SAFETY: serialised by the env mutex above; restore below.
        unsafe { std::env::remove_var("ANODIZER_GITHUB_API_BASE") };
        let got = github_api_base();
        // SAFETY: still under the env mutex.
        unsafe {
            match previous {
                Some(prev) => std::env::set_var("ANODIZER_GITHUB_API_BASE", prev),
                None => std::env::remove_var("ANODIZER_GITHUB_API_BASE"),
            }
        }
        assert_eq!(got, "https://api.github.com");
    }
}
