//! Shared GitHub `GET /repos/{owner}/{repo}` reachability + permission probe.
//!
//! Both the publish-stage tap/index preflights and the release-stage
//! github-release preflight need the same network probe: issue the request
//! under the shallow retry policy, read the rate-limit headers (not just the
//! status, so a secondary-rate-limit 403 is separable from an auth 403), and
//! classify the outcome. Only the *severity mapping* of each outcome differs
//! between callers — a tap that cannot be pushed is a `Warning`, whereas the
//! required github-release target is a `Blocker` — so this module returns a
//! neutral [`RepoProbe`] classification and leaves the `PreflightCheck` mapping
//! to each caller.

use std::ops::ControlFlow;

use crate::retry::{RetryPolicy, is_retriable, retry_sync};

/// Terminal classification of a single `GET /repos/{owner}/{repo}` probe,
/// carrying enough to distinguish a transient rate-limit 403 from an auth 403.
pub enum RepoProbe {
    /// 2xx — carries the response body for `permissions.push` inspection.
    Body(String),
    /// 404 — repo missing under an otherwise-good token.
    Missing,
    /// 401 / 403 with NO rate-limit signal — the token cannot access the repo.
    AuthDenied,
    /// 429, or a 401 / 403 carrying a rate-limit signal (GitHub returns 403 for
    /// both secondary-rate-limit and auth denial, distinguishable only by the
    /// `Retry-After` / `X-RateLimit-Remaining: 0` headers) — transient.
    RateLimited,
    /// 5xx, an unexpected status, or a transport failure — verdict unknown.
    Inconclusive(String),
}

/// Whether a GitHub response's headers mark it as rate-limited: a `Retry-After`
/// header (primary or secondary limit) or `X-RateLimit-Remaining: 0`. Header
/// lookups are case-insensitive ([`reqwest::header::HeaderMap`]).
pub fn response_is_rate_limited(headers: &reqwest::header::HeaderMap) -> bool {
    if headers.contains_key("retry-after") {
        return true;
    }
    headers
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim() == "0")
        .unwrap_or(false)
}

/// Run the `GET /repos/{owner}/{repo}` request under the shallow probe policy,
/// reading response headers (not just the status) so a secondary-rate-limit 403
/// is separable from an auth 403. 5xx and retriable transport errors retry
/// within `policy`; everything else resolves on the first response.
///
/// `token` is optional: a `Some(non-empty)` value adds the `Authorization`
/// bearer header (an empty string is treated as no token — the unauthenticated
/// read path), so the required-token callers pass `Some(token)` and the
/// best-effort callers can pass `None`.
pub fn github_repo_probe(
    client: &reqwest::blocking::Client,
    url: &str,
    token: Option<&str>,
    policy: &RetryPolicy,
) -> RepoProbe {
    let token = token.map(str::to_string);
    let outcome = retry_sync(policy, |_attempt| {
        let mut b = client
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(ref tok) = token
            && !tok.is_empty()
        {
            b = b.header("Authorization", format!("Bearer {tok}"));
        }
        match b.send() {
            Ok(resp) => {
                let code = resp.status().as_u16();
                // Capture the rate-limit verdict from headers BEFORE `text()`
                // consumes the response.
                let rate_limited = response_is_rate_limited(resp.headers());
                if resp.status().is_success() {
                    Ok(RepoProbe::Body(resp.text().unwrap_or_default()))
                } else if resp.status().is_server_error() {
                    Err(ControlFlow::Continue(RepoProbe::Inconclusive(format!(
                        "HTTP {code}"
                    ))))
                } else if code == 429 || ((code == 403 || code == 401) && rate_limited) {
                    Ok(RepoProbe::RateLimited)
                } else if code == 404 {
                    Ok(RepoProbe::Missing)
                } else if code == 403 || code == 401 {
                    Ok(RepoProbe::AuthDenied)
                } else {
                    Ok(RepoProbe::Inconclusive(format!("unexpected HTTP {code}")))
                }
            }
            Err(e) => {
                let msg = format!("network failure: {e}");
                if is_retriable(&e) {
                    Err(ControlFlow::Continue(RepoProbe::Inconclusive(msg)))
                } else {
                    Err(ControlFlow::Break(RepoProbe::Inconclusive(msg)))
                }
            }
        }
    });
    // Both the success and the retries-exhausted arm collapse to the same
    // terminal `RepoProbe`.
    match outcome {
        Ok(p) | Err(p) => p,
    }
}
