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
    let url = "https://api.github.com/rate_limit";
    let resp = match client
        .get(url)
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
mod tests {
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
}
