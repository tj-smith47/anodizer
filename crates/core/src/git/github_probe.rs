//! Shared GitHub `GET /repos/{owner}/{repo}` reachability + permission probe.
//!
//! Both the publish-stage tap/index preflights and the release-stage
//! github-release preflight need the same network probe: issue the request
//! under the shallow retry policy, read the rate-limit headers (not just the
//! status, so a secondary-rate-limit 403 is separable from an auth 403), and
//! classify the outcome. Only the *severity mapping* of two outcomes differs
//! between callers — a tap that cannot be pushed is a `Warning`, whereas the
//! required github-release target is a `Blocker` — so
//! [`github_repo_push_check`] owns the whole probe→[`PreflightCheck`] mapping
//! (including the `permissions.push` body parse) and each caller supplies
//! only its [`RepoAccessOutcomes`].

use std::ops::ControlFlow;

use crate::PreflightCheck;
use crate::retry::{RetryPolicy, is_retriable, retry_sync};

/// Timeout for a single `GET /repos/{owner}/{repo}` preflight probe request.
/// Shared by every probe caller so the release and publish preflights place
/// the same bound on how long an unreachable GitHub can stall a run.
pub const REPO_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

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

/// The two outcomes whose severity + wording genuinely differ between the
/// preflight callers of [`github_repo_push_check`]: an unwritable repo blocks
/// the required github-release target but only warns for a tap/index repo,
/// and the missing/denied wording names what the caller was probing.
///
/// Every other arm of the probe→check mapping (the `permissions.push` parse
/// ladder, the rate-limited / inconclusive / client-build warnings) is shared
/// policy and lives in the mapper itself, so the two preflights cannot drift
/// apart on how the same token+repo is classified.
pub struct RepoAccessOutcomes {
    /// Returned when the probe proves `permissions.push == false`.
    pub push_denied: PreflightCheck,
    /// Returned when the repo 404s or the token is denied access.
    pub missing_or_denied: PreflightCheck,
}

/// Probe `GET {url}` and map the outcome onto a [`PreflightCheck`].
///
/// Builds the probe client, runs [`github_repo_probe`], and classifies:
///
/// * 200 + `permissions.push == true` ⇒ `Pass`
/// * 200 + `permissions.push == false` ⇒ `outcomes.push_denied`
/// * 200 + `permissions` absent / unparsable body ⇒ `Warning`
/// * 404, or 401 / 403 without a rate-limit signal ⇒ `outcomes.missing_or_denied`
/// * 429, or 401 / 403 carrying a rate-limit header ⇒ `Warning` (a transient
///   GitHub rate limit must not abort a release that would otherwise succeed)
/// * 5xx / transport failure / unexpected status ⇒ `Warning`
pub fn github_repo_push_check(
    url: &str,
    owner: &str,
    repo: &str,
    token: Option<&str>,
    policy: &RetryPolicy,
    outcomes: RepoAccessOutcomes,
) -> PreflightCheck {
    let client = match crate::http::blocking_client(REPO_PROBE_TIMEOUT) {
        Ok(c) => c,
        Err(e) => {
            return PreflightCheck::Warning(format!(
                "could not probe {owner}/{repo} write access ({e}); verify the repo and token manually"
            ));
        }
    };
    probe_to_push_check(
        github_repo_probe(&client, url, token, policy),
        owner,
        repo,
        outcomes,
    )
}

/// Pure probe→check mapper backing [`github_repo_push_check`], split out so
/// the classification arms are unit-testable without an HTTP responder.
pub fn probe_to_push_check(
    probe: RepoProbe,
    owner: &str,
    repo: &str,
    outcomes: RepoAccessOutcomes,
) -> PreflightCheck {
    match probe {
        RepoProbe::Body(body) => match serde_json::from_str::<serde_json::Value>(&body) {
            Ok(v) => match v.pointer("/permissions/push").and_then(|p| p.as_bool()) {
                Some(true) => PreflightCheck::Pass,
                Some(false) => outcomes.push_denied,
                None => PreflightCheck::Warning(format!(
                    "could not determine push access to {owner}/{repo} (no permissions in API \
                     response); verify the token scope manually"
                )),
            },
            Err(_) => PreflightCheck::Warning(format!(
                "could not parse {owner}/{repo} API response; verify the repo and token manually"
            )),
        },
        RepoProbe::Missing | RepoProbe::AuthDenied => outcomes.missing_or_denied,
        // A secondary-rate-limit 403 is indistinguishable from auth denial by
        // status alone; the headers prove it transient, so warn rather than
        // abort a release whose token is actually fine.
        RepoProbe::RateLimited => PreflightCheck::Warning(format!(
            "GitHub API rate-limited while probing {owner}/{repo}; could not verify write access \
             — verify the repo and token manually"
        )),
        RepoProbe::Inconclusive(reason) => PreflightCheck::Warning(format!(
            "could not probe {owner}/{repo} write access ({reason}); verify the repo and token manually"
        )),
    }
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

#[cfg(test)]
mod push_check_tests {
    //! The pure probe→check mapping arms. The HTTP-level probe behavior
    //! (status/header classification) is pinned by the scripted-responder
    //! tests at the two preflight call sites; these cover the shared
    //! severity/parse policy that must not drift between them.
    use super::*;

    fn outcomes() -> RepoAccessOutcomes {
        RepoAccessOutcomes {
            push_denied: PreflightCheck::Blocker("push denied".into()),
            missing_or_denied: PreflightCheck::Blocker("missing or denied".into()),
        }
    }

    #[test]
    fn push_true_passes() {
        let probe = RepoProbe::Body(r#"{"permissions":{"push":true}}"#.into());
        assert_eq!(
            probe_to_push_check(probe, "o", "r", outcomes()),
            PreflightCheck::Pass
        );
    }

    #[test]
    fn push_false_returns_caller_push_denied() {
        let probe = RepoProbe::Body(r#"{"permissions":{"push":false}}"#.into());
        assert_eq!(
            probe_to_push_check(probe, "o", "r", outcomes()),
            PreflightCheck::Blocker("push denied".into())
        );
    }

    #[test]
    fn permissions_absent_warns() {
        let probe = RepoProbe::Body(r#"{"full_name":"o/r"}"#.into());
        match probe_to_push_check(probe, "o", "r", outcomes()) {
            PreflightCheck::Warning(msg) => {
                assert!(msg.contains("could not determine push access"), "{msg}")
            }
            other => panic!("expected Warning, got {other:?}"),
        }
    }

    #[test]
    fn unparsable_body_warns() {
        let probe = RepoProbe::Body("not json".into());
        match probe_to_push_check(probe, "o", "r", outcomes()) {
            PreflightCheck::Warning(msg) => {
                assert!(msg.contains("could not parse o/r"), "{msg}")
            }
            other => panic!("expected Warning, got {other:?}"),
        }
    }

    #[test]
    fn missing_and_auth_denied_return_caller_outcome() {
        for probe in [RepoProbe::Missing, RepoProbe::AuthDenied] {
            assert_eq!(
                probe_to_push_check(probe, "o", "r", outcomes()),
                PreflightCheck::Blocker("missing or denied".into())
            );
        }
    }

    #[test]
    fn rate_limited_warns_never_escalates() {
        match probe_to_push_check(RepoProbe::RateLimited, "o", "r", outcomes()) {
            PreflightCheck::Warning(msg) => assert!(msg.contains("rate-limited"), "{msg}"),
            other => panic!("expected Warning, got {other:?}"),
        }
    }

    #[test]
    fn inconclusive_warns_with_reason() {
        let probe = RepoProbe::Inconclusive("HTTP 500".into());
        match probe_to_push_check(probe, "o", "r", outcomes()) {
            PreflightCheck::Warning(msg) => assert!(msg.contains("HTTP 500"), "{msg}"),
            other => panic!("expected Warning, got {other:?}"),
        }
    }
}
