//! Live pre-publish probes shared by the publishers whose
//! [`Publisher::preflight`](anodizer_core::Publisher::preflight) gate needs a
//! real network check rather than the presence-only
//! [`requirements()`](anodizer_core::Publisher::requirements) declaration.
//!
//! Two probe families live here:
//!
//! * [`probe_token_auth`] — a `whoami`-style authenticated GET that proves a
//!   registry token is accepted (not merely present). Consumed by the
//!   irreversible cargo / npm publishers, whose token slot is a one-way door.
//! * [`github_repo_check`] / [`github_repo_config_check`] — a
//!   `GET /repos/{owner}/{repo}` probe that proves the target index/fork repo
//!   exists and the resolved token can push to it. Shared by every
//!   GitHub-repo-backed publisher (homebrew, scoop, nix, krew, winget).
//!
//! [`probe_version_published`] backs the npm duplicate-version warning — npm
//! has no companion state-query checker, so this is its only duplicate guard.
//!
//! All probes degrade to [`PreflightCheck::Warning`] (never a hard block) on a
//! transport failure or an indeterminate status: a transient network blip must
//! surface but must not abort a release that would otherwise succeed.

use std::time::Duration;

use anodizer_core::PreflightCheck;
use anodizer_core::context::Context;
use anodizer_core::http::blocking_client;
use anodizer_core::redact::redact_bearer_tokens;
use std::ops::ControlFlow;

use anodizer_core::retry::{
    RetryPolicy, SuccessClass, http_status, is_retriable, retry_http_blocking, retry_sync,
};

/// Per-probe HTTP timeout. Generous enough to tolerate a cold TLS handshake to
/// crates.io / npm / the GitHub API, short enough that a wedged endpoint cannot
/// stall the pre-publish gate indefinitely.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Combine two outcomes keeping the most severe: `Blocker` > `Warning` >
/// `Pass`. The first-seen message wins within a severity so the operator sees
/// a stable, deterministic line rather than whichever target iterated last.
pub(crate) fn merge(acc: PreflightCheck, next: PreflightCheck) -> PreflightCheck {
    use PreflightCheck::{Blocker, Pass, Warning};
    match (acc, next) {
        (Blocker(m), _) => Blocker(m),
        (_, Blocker(m)) => Blocker(m),
        (Warning(m), _) => Warning(m),
        (_, Warning(m)) => Warning(m),
        (Pass, Pass) => Pass,
    }
}

/// Outcome of an authenticated token probe against a registry `whoami`.
pub(crate) enum TokenAuth {
    /// The registry accepted the credential (2xx).
    Valid,
    /// The registry rejected the credential (401/403) — a hard prerequisite
    /// the publisher cannot satisfy at publish time.
    Invalid,
    /// The probe could not reach a verdict (transport failure, 5xx, or an
    /// unexpected status). Carries a redacted reason for the warn line.
    Indeterminate(String),
}

/// Probe an authenticated `whoami`-style endpoint to prove `authorization` is
/// accepted by the registry.
///
/// * 2xx ⇒ [`TokenAuth::Valid`]
/// * 401 / 403 ⇒ [`TokenAuth::Invalid`]
/// * anything else (transport error, 5xx, unexpected status) ⇒
///   [`TokenAuth::Indeterminate`]
///
/// `authorization` is the full `Authorization` header value (callers supply
/// `Bearer <token>` for npm, the raw token for crates.io) so the probe stays
/// agnostic to each registry's auth scheme. `url` is passed in full so a unit
/// test can point the probe at a local responder without a network round-trip.
pub(crate) fn probe_token_auth(
    url: &str,
    authorization: &str,
    label: &str,
    policy: &RetryPolicy,
) -> TokenAuth {
    let client = match blocking_client(PROBE_TIMEOUT) {
        Ok(c) => c,
        Err(e) => return TokenAuth::Indeterminate(format!("could not build HTTP client: {e}")),
    };
    let auth = authorization.to_string();
    let result = retry_http_blocking(
        label,
        policy,
        SuccessClass::Strict,
        |_| {
            client
                .get(url)
                .header("Authorization", &auth)
                .header("Accept", "application/json")
                .send()
        },
        |status, body| format!("{status}: {}", redact_bearer_tokens(body)),
    );
    match result {
        Ok(_) => TokenAuth::Valid,
        Err(err) => match http_status(&err) {
            401 | 403 => TokenAuth::Invalid,
            0 => TokenAuth::Indeterminate(format!("network failure: {err}")),
            other => TokenAuth::Indeterminate(format!("unexpected HTTP {other}")),
        },
    }
}

/// Whether a registry resource exists (HTTP 200) at `url`.
///
/// Used for the npm duplicate-version warning: an existing `<registry>/<pkg>/
/// <version>` means the publish will be rejected. Any non-2xx (404 = absent,
/// transport error, 5xx) returns `false` — the duplicate warning is
/// best-effort and must never fabricate a false positive from a network blip.
pub(crate) fn probe_version_published(url: &str, label: &str, policy: &RetryPolicy) -> bool {
    let client = match blocking_client(PROBE_TIMEOUT) {
        Ok(c) => c,
        Err(_) => return false,
    };
    retry_http_blocking(
        label,
        policy,
        SuccessClass::Strict,
        |_| client.get(url).send(),
        |status, body| format!("{status}: {}", redact_bearer_tokens(body)),
    )
    .is_ok()
}

/// Probe `GET https://api.github.com/repos/{owner}/{repo}` to prove the target
/// index/fork repo exists and `token` can push to it. See
/// [`github_repo_check_at`] for the outcome mapping.
pub(crate) fn github_repo_check(
    owner: &str,
    repo: &str,
    token: Option<&str>,
    policy: &RetryPolicy,
) -> PreflightCheck {
    let url = format!("https://api.github.com/repos/{owner}/{repo}");
    github_repo_check_at(&url, owner, repo, token, policy)
}

/// Terminal classification of a single `GET /repos/{owner}/{repo}` probe,
/// carrying enough to distinguish a transient rate-limit 403 from an auth 403.
enum RepoProbe {
    /// 2xx — carries the response body for `permissions.push` inspection.
    Body(String),
    /// 404 — repo missing under an otherwise-good token.
    Missing,
    /// 401 / 403 with NO rate-limit signal — the token cannot read the repo.
    AuthDenied,
    /// 429, or a 401 / 403 carrying a rate-limit signal (GitHub returns 403
    /// for both secondary-rate-limit and auth denial, distinguishable only by
    /// the `Retry-After` / `X-RateLimit-Remaining: 0` headers) — transient.
    RateLimited,
    /// 5xx, an unexpected status, or a transport failure — verdict unknown.
    Inconclusive(String),
}

/// Whether a GitHub response's headers mark it as rate-limited: a `Retry-After`
/// header (primary or secondary limit) or `X-RateLimit-Remaining: 0`. Header
/// lookups are case-insensitive ([`reqwest::header::HeaderMap`]).
fn response_is_rate_limited(headers: &reqwest::header::HeaderMap) -> bool {
    if headers.contains_key("retry-after") {
        return true;
    }
    headers
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim() == "0")
        .unwrap_or(false)
}

/// `url`-taking core of [`github_repo_check`] so a unit test can drive the
/// status/permission mapping against a local responder.
///
/// * 404 ⇒ [`PreflightCheck::Blocker`] (repo missing under a good token)
/// * 401 / 403 without a rate-limit signal ⇒ [`PreflightCheck::Blocker`]
///   (the token cannot read the repo)
/// * 429, or 401 / 403 carrying a `Retry-After` / `X-RateLimit-Remaining: 0`
///   header ⇒ [`PreflightCheck::Warning`] (a transient GitHub rate limit must
///   not abort a release that would otherwise succeed)
/// * 200 with `permissions.push == false` ⇒ [`PreflightCheck::Warning`]
/// * 200 with `permissions` absent (unauthenticated read) ⇒
///   [`PreflightCheck::Warning`] (push scope undeterminable)
/// * 200 with `permissions.push == true` ⇒ [`PreflightCheck::Pass`]
/// * transport failure / other status ⇒ [`PreflightCheck::Warning`]
pub(crate) fn github_repo_check_at(
    url: &str,
    owner: &str,
    repo: &str,
    token: Option<&str>,
    policy: &RetryPolicy,
) -> PreflightCheck {
    let client = match blocking_client(PROBE_TIMEOUT) {
        Ok(c) => c,
        Err(e) => {
            return PreflightCheck::Warning(format!(
                "could not probe {owner}/{repo} write access ({e}); verify the repo and token manually"
            ));
        }
    };

    match github_repo_probe(&client, url, token, policy) {
        RepoProbe::Body(body) => match serde_json::from_str::<serde_json::Value>(&body) {
            Ok(v) => match v.pointer("/permissions/push").and_then(|p| p.as_bool()) {
                Some(true) => PreflightCheck::Pass,
                Some(false) => PreflightCheck::Warning(format!(
                    "token cannot push to {owner}/{repo}; the publish PR/commit will fail"
                )),
                None => PreflightCheck::Warning(format!(
                    "could not determine push access to {owner}/{repo} (no permissions in API response); \
                     verify the token scope manually"
                )),
            },
            Err(_) => PreflightCheck::Warning(format!(
                "could not parse {owner}/{repo} API response; verify the repo and token manually"
            )),
        },
        // A missing repo or a token that cannot read it is a hard prerequisite
        // the publish path cannot satisfy — block.
        RepoProbe::Missing | RepoProbe::AuthDenied => PreflightCheck::Blocker(format!(
            "index/fork repo {owner}/{repo} not found or token lacks read access"
        )),
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
fn github_repo_probe(
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

/// Resolve a publisher's repository config to owner/name + token and run
/// [`github_repo_check`].
///
/// Returns [`PreflightCheck::Pass`] (silent) when the repo's owner/name are not
/// both set: an absent target is config-validation territory, and the run path
/// already fails loud on it — the preflight must not manufacture a duplicate
/// blocker for a config error caught elsewhere. owner/name are rendered through
/// the same template engine the publish path uses so `{{ .Env.X }}`-templated
/// coordinates probe their resolved value.
pub(crate) fn github_repo_config_check(
    ctx: &Context,
    repo: Option<&anodizer_core::config::RepositoryConfig>,
    preferred_env: &str,
    policy: &RetryPolicy,
) -> PreflightCheck {
    // A `git.url` override routes the push over SSH / to a self-hosted GHE
    // host, not api.github.com. Probing github.com for those coordinates would
    // false-404 a repo that lives elsewhere, and an SSH-key push is not what a
    // REST-token read probe measures. Defer to the publish path's own checks.
    if repo
        .and_then(|r| r.git.as_ref())
        .and_then(|g| g.url.as_deref())
        .is_some_and(|u| !u.trim().is_empty())
    {
        return PreflightCheck::Pass;
    }
    let Some((owner_raw, name_raw)) = crate::util::resolve_repo_owner_name(repo) else {
        return PreflightCheck::Pass;
    };
    let owner = ctx.render_template(&owner_raw).unwrap_or(owner_raw);
    let name = ctx.render_template(&name_raw).unwrap_or(name_raw);
    if owner.trim().is_empty() || name.trim().is_empty() {
        return PreflightCheck::Pass;
    }
    let token = crate::util::resolve_repo_token(ctx, repo, Some(preferred_env));
    github_repo_check(&owner, &name, token.as_deref(), policy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

    fn fast_retry() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 1,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
        }
    }

    fn http(status_line: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn merge_keeps_most_severe() {
        let b = PreflightCheck::Blocker("b".into());
        let w = PreflightCheck::Warning("w".into());
        assert!(matches!(
            merge(PreflightCheck::Pass, w.clone()),
            PreflightCheck::Warning(_)
        ));
        assert!(matches!(
            merge(w.clone(), b.clone()),
            PreflightCheck::Blocker(_)
        ));
        assert!(matches!(merge(b, w), PreflightCheck::Blocker(_)));
        assert!(matches!(
            merge(PreflightCheck::Pass, PreflightCheck::Pass),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn token_auth_valid_on_200() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![Box::leak(
            http("200 OK", r#"{"username":"me"}"#).into_boxed_str(),
        )]);
        let url = format!("http://{addr}/-/whoami");
        assert!(matches!(
            probe_token_auth(&url, "Bearer t", "test", &fast_retry()),
            TokenAuth::Valid
        ));
    }

    #[test]
    fn token_auth_invalid_on_401() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![Box::leak(
            http("401 Unauthorized", "").into_boxed_str(),
        )]);
        let url = format!("http://{addr}/-/whoami");
        assert!(matches!(
            probe_token_auth(&url, "Bearer bad", "test", &fast_retry()),
            TokenAuth::Invalid
        ));
    }

    #[test]
    fn token_auth_invalid_on_403() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![Box::leak(
            http("403 Forbidden", "").into_boxed_str(),
        )]);
        let url = format!("http://{addr}/me");
        assert!(matches!(
            probe_token_auth(&url, "raw-token", "test", &fast_retry()),
            TokenAuth::Invalid
        ));
    }

    #[test]
    fn token_auth_indeterminate_on_network_error() {
        // Bind then drop to obtain a closed port → connection refused.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        let url = format!("http://{addr}/-/whoami");
        assert!(matches!(
            probe_token_auth(&url, "Bearer t", "test", &fast_retry()),
            TokenAuth::Indeterminate(_)
        ));
    }

    #[test]
    fn version_published_true_on_200() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![Box::leak(
            http("200 OK", r#"{"version":"1.0.0"}"#).into_boxed_str(),
        )]);
        let url = format!("http://{addr}/pkg/1.0.0");
        assert!(probe_version_published(&url, "test", &fast_retry()));
    }

    #[test]
    fn version_published_false_on_404() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![Box::leak(
            http("404 Not Found", "").into_boxed_str(),
        )]);
        let url = format!("http://{addr}/pkg/9.9.9");
        assert!(!probe_version_published(&url, "test", &fast_retry()));
    }

    #[test]
    fn github_repo_pass_when_push_true() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![Box::leak(
            http("200 OK", r#"{"permissions":{"push":true}}"#).into_boxed_str(),
        )]);
        let url = format!("http://{addr}/repos/o/r");
        assert!(matches!(
            github_repo_check_at(&url, "o", "r", Some("tok"), &fast_retry()),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn github_repo_warns_when_push_false() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![Box::leak(
            http("200 OK", r#"{"permissions":{"push":false}}"#).into_boxed_str(),
        )]);
        let url = format!("http://{addr}/repos/o/r");
        match github_repo_check_at(&url, "o", "r", Some("tok"), &fast_retry()) {
            PreflightCheck::Warning(m) => assert!(m.contains("cannot push"), "{m}"),
            other => panic!("expected Warning, got {other:?}"),
        }
    }

    #[test]
    fn github_repo_warns_when_permissions_absent() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![Box::leak(
            http("200 OK", r#"{"full_name":"o/r"}"#).into_boxed_str(),
        )]);
        let url = format!("http://{addr}/repos/o/r");
        assert!(matches!(
            github_repo_check_at(&url, "o", "r", None, &fast_retry()),
            PreflightCheck::Warning(_)
        ));
    }

    #[test]
    fn github_repo_blocks_on_404() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![Box::leak(
            http("404 Not Found", "").into_boxed_str(),
        )]);
        let url = format!("http://{addr}/repos/o/missing");
        match github_repo_check_at(&url, "o", "missing", Some("tok"), &fast_retry()) {
            PreflightCheck::Blocker(m) => assert!(m.contains("not found"), "{m}"),
            other => panic!("expected Blocker, got {other:?}"),
        }
    }

    #[test]
    fn github_repo_blocks_on_403() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![Box::leak(
            http("403 Forbidden", "").into_boxed_str(),
        )]);
        let url = format!("http://{addr}/repos/o/r");
        assert!(matches!(
            github_repo_check_at(&url, "o", "r", Some("tok"), &fast_retry()),
            PreflightCheck::Blocker(_)
        ));
    }

    /// A raw HTTP response with one extra header line beyond `Content-Length`,
    /// for exercising the rate-limit header inspection.
    fn http_with_header(status_line: &str, header: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\n{header}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn github_repo_warns_on_rate_limited_403() {
        // A secondary-rate-limit 403 carries `X-RateLimit-Remaining: 0`; it is
        // transient and must NOT block a release whose token is actually valid.
        let (addr, _c) = spawn_oneshot_http_responder(vec![Box::leak(
            http_with_header("403 Forbidden", "X-RateLimit-Remaining: 0", "").into_boxed_str(),
        )]);
        let url = format!("http://{addr}/repos/o/r");
        assert!(
            matches!(
                github_repo_check_at(&url, "o", "r", Some("tok"), &fast_retry()),
                PreflightCheck::Warning(_)
            ),
            "rate-limited 403 must degrade to Warning, not Blocker"
        );
    }

    #[test]
    fn github_repo_warns_on_retry_after_403() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![Box::leak(
            http_with_header("403 Forbidden", "Retry-After: 60", "").into_boxed_str(),
        )]);
        let url = format!("http://{addr}/repos/o/r");
        assert!(matches!(
            github_repo_check_at(&url, "o", "r", Some("tok"), &fast_retry()),
            PreflightCheck::Warning(_)
        ));
    }

    #[test]
    fn github_repo_warns_on_429() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![Box::leak(
            http("429 Too Many Requests", "").into_boxed_str(),
        )]);
        let url = format!("http://{addr}/repos/o/r");
        assert!(matches!(
            github_repo_check_at(&url, "o", "r", Some("tok"), &fast_retry()),
            PreflightCheck::Warning(_)
        ));
    }

    #[test]
    fn github_repo_config_check_skips_probe_for_ssh_git_url() {
        // A `git.url` SSH/GHE override pushes elsewhere than api.github.com;
        // the probe must short-circuit to Pass WITHOUT a network round-trip
        // (a bound-then-dropped port would surface as a Warning, not Pass, if
        // the probe ran).
        use anodizer_core::config::{GitRepoConfig, RepositoryConfig};
        let repo = RepositoryConfig {
            owner: Some("o".into()),
            name: Some("r".into()),
            git: Some(GitRepoConfig {
                url: Some("ssh://git@ghe.corp.example/o/r.git".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ctx = anodizer_core::context::Context::test_fixture();
        assert!(matches!(
            github_repo_config_check(&ctx, Some(&repo), "GITHUB_TOKEN", &fast_retry()),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn github_repo_warns_on_network_error() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        let url = format!("http://{addr}/repos/o/r");
        assert!(matches!(
            github_repo_check_at(&url, "o", "r", Some("tok"), &fast_retry()),
            PreflightCheck::Warning(_)
        ));
    }
}
