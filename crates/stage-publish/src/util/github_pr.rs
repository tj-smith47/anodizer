//! GitHub PR close + lookup helpers — used by the close-PR rollback
//! shape (krew today; reusable for any future publisher whose rollback
//! is "close the PR we opened against an upstream").
//!
//! Two public helpers:
//! - [`find_open_pr_numbers_for_head`] — `GET /repos/{owner}/{repo}/pulls`
//!   with `head=<fork_owner>:<branch>` and `state=open`. Returns the
//!   PR numbers of every match, following `Link: rel="next"` pagination
//!   up to a sanity cap of 10 pages (1000 PRs). Distinguishes
//!   auth-failure / repo-not-found from genuine empty results via
//!   [`FindPrError`] so callers can surface actionable warns at
//!   `--rollback-only` time.
//! - [`close_pr_via_api`] — `PATCH /repos/{owner}/{repo}/pulls/{n}` with
//!   `{"state": "closed"}`. Returns a [`CloseOutcome`] enum: `Closed`
//!   on 2xx, `AlreadyClosed` on 404 / 410 / 422 (the PR was already
//!   deleted / gone / in a state GitHub refused the transition for —
//!   the desired end-state "PR closed" is already true), and `Failed`
//!   on anything else. Mirrors the artifactory rollback bucketing in
//!   [`crate::artifactory::DeleteOutcome`].
//!
//! Why a raw http helper instead of extending `anodizer_core::GitHubClient`?
//! `GitHubClient` is a trait bound on release operations (create/list/
//! delete release, upload asset). Adding PR ops would muddy that
//! contract for the one publisher group that needs it. The raw
//! helper keeps the surface narrow and the dependency direction
//! correct (`stage-publish` → `core::http`, not vice versa).
//!
//! Bearer-auth convention: both helpers send `Authorization: Bearer
//! <token>` (modern preferred form). The legacy `Authorization: token
//! <pat>` form in [`crate::util::pr::create_pr_via_api`] is retained
//! for backward-compatibility but new GitHub-API helpers in this
//! crate should consistently use `Bearer`.

use std::time::Duration;

use anodizer_core::{EnvSource, ProcessEnvSource};
use anyhow::Context as _;

use super::branch::github_api_base_from;

// ---------------------------------------------------------------------------
// CloseOutcome — bucket the close-PR PATCH response
// ---------------------------------------------------------------------------

/// Outcome of one `PATCH /pulls/{n} {"state":"closed"}` attempt against
/// a single upstream PR. Returned by [`close_pr_via_api`] so the per-PR
/// response can be aggregated into the summary line.
///
/// Mirrors the artifactory rollback bucketing in
/// [`crate::artifactory::DeleteOutcome`] — `AlreadyClosed` is a
/// **success bucket** because the desired end-state (PR not open) is
/// already true. Re-running `--rollback-only` after a partial success
/// must not surface `AlreadyClosed` PRs as failures; that's how the
/// operator confirms the rollback was complete.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CloseOutcome {
    /// PATCH returned 2xx — the PR transitioned from open to closed
    /// on this attempt.
    Closed,
    /// PATCH returned 404 (PR deleted), 410 (Gone), or 422
    /// (Unprocessable Entity — GitHub's response for "PR already in
    /// the requested state"). All three mean the desired end-state
    /// is already true; the operator does not need to act.
    AlreadyClosed,
    /// Anything else (5xx, 401, 403, 400, transport error).
    /// Carries the upstream status + body for the operator-facing
    /// warn line.
    Failed(String),
}

/// Classify a `PATCH /pulls/{n}` response's status code + body into the
/// rollback summary bucket. Pure helper so the bucket boundary can be
/// unit-tested without firing an HTTP request — production callers go
/// through [`close_pr_via_api`].
///
/// `body` is only consulted for the `Failed(_)` carrier-string;
/// success / already-closed buckets do not surface the body.
pub(crate) fn classify_close_status(status: reqwest::StatusCode, body: &str) -> CloseOutcome {
    if status.is_success() {
        return CloseOutcome::Closed;
    }
    if status == reqwest::StatusCode::NOT_FOUND
        || status == reqwest::StatusCode::GONE
        || status == reqwest::StatusCode::UNPROCESSABLE_ENTITY
    {
        return CloseOutcome::AlreadyClosed;
    }
    CloseOutcome::Failed(format!("HTTP {}: {}", status, body))
}

// ---------------------------------------------------------------------------
// FindPrError — auth-vs-empty-result discrimination
// ---------------------------------------------------------------------------

/// Failure modes for [`find_open_pr_numbers_for_head`].
///
/// Distinguishes "no open PRs match" (the empty `Vec` return) from
/// "we couldn't tell whether any PRs match" (this enum). Critical for
/// incident response: if `KREW_INDEX_TOKEN` is rotated mid-release,
/// rollback must warn "auth failed" rather than the misleading
/// "no PR found; verify manually" that previously fired.
#[derive(Debug)]
pub(crate) enum FindPrError {
    /// Transport / DNS / TLS failure constructing or sending the request.
    Network { url: String, source: anyhow::Error },
    /// HTTP 401 — the supplied token (or its absence) was rejected as
    /// unauthenticated. The operator should verify the env-var is set
    /// in the current shell.
    Auth401 { url: String, env_hint: String },
    /// HTTP 403 — the supplied token authenticated but lacks the
    /// `pull_request:read` scope (or hit a rate-limit on a token that
    /// otherwise works). Operator should verify scopes / fine-grained
    /// permissions.
    Auth403 { url: String, env_hint: String },
    /// HTTP 404 — the upstream `{owner}/{repo}` doesn't exist (or the
    /// token can't see it). Operator may have renamed/deleted the repo
    /// between publish and rollback.
    RepoNotFound { url: String },
    /// Any other non-2xx response. Carries status + (truncated) body
    /// for the operator-facing warn line.
    Other {
        url: String,
        status: reqwest::StatusCode,
        body: String,
    },
}

impl std::fmt::Display for FindPrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network { url, source } => {
                write!(f, "github_pr: network error querying {}: {}", url, source)
            }
            Self::Auth401 { url, env_hint } => write!(
                f,
                "github_pr: 401 Unauthorized querying {}; verify ${} \
                 is set and not expired",
                url, env_hint
            ),
            Self::Auth403 { url, env_hint } => write!(
                f,
                "github_pr: 403 Forbidden querying {}; verify ${} \
                 has pull_request:read scope (or that you're not rate-limited)",
                url, env_hint
            ),
            Self::RepoNotFound { url } => write!(
                f,
                "github_pr: 404 Not Found querying {}; repo may have been renamed or deleted",
                url
            ),
            Self::Other { url, status, body } => {
                write!(
                    f,
                    "github_pr: GET {} returned HTTP {}: {}",
                    url, status, body
                )
            }
        }
    }
}

impl std::error::Error for FindPrError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Network { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

/// Classify the response status of a `GET /repos/{owner}/{repo}/pulls`
/// query into either `Ok(())` (a 2xx — caller should parse the body) or
/// `Err(FindPrError)` for every other status code. Pure helper so the
/// auth-vs-empty-result boundary can be unit-tested without firing an
/// HTTP request — production callers go through
/// [`find_open_pr_numbers_for_head`].
///
/// `url`, `env_hint`, and `body` are only consulted for the error
/// variants' carrier-strings.
pub(crate) fn classify_find_pr_status(
    status: reqwest::StatusCode,
    url: &str,
    env_hint: &str,
    body_supplier: impl FnOnce() -> String,
) -> Result<(), FindPrError> {
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(FindPrError::Auth401 {
            url: url.to_string(),
            env_hint: env_hint.to_string(),
        });
    }
    if status == reqwest::StatusCode::FORBIDDEN {
        return Err(FindPrError::Auth403 {
            url: url.to_string(),
            env_hint: env_hint.to_string(),
        });
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(FindPrError::RepoNotFound {
            url: url.to_string(),
        });
    }
    if !status.is_success() {
        return Err(FindPrError::Other {
            url: url.to_string(),
            status,
            body: body_supplier(),
        });
    }
    Ok(())
}

/// Look up open PR numbers for a given `head=<fork_owner>:<branch>`
/// filter against `<upstream_owner>/<upstream_repo>`.
///
/// Follows `Link: rel="next"` pagination so the helper remains
/// correct as publishers (winget reusing this helper against
/// `microsoft/winget-pkgs`) target repos with thousands of open PRs.
/// The `head=` filter is exact-match so the result set per-fork-per-
/// branch is realistically small (usually 0 or 1); the pagination
/// loop is a safety net.
///
/// **Capped at 10 pages (1000 PRs)** as a sanity bound. If your fork
/// branch could plausibly carry more than 1000 open PRs at rollback
/// time, redesign the rollback strategy — `git push --force` to the
/// fork branch + close-by-head-filter is not the right shape.
///
/// `env_hint` is the env-var name the caller resolved the `token`
/// from (e.g. `"KREW_INDEX_TOKEN"`) — surfaced verbatim in
/// [`FindPrError::Auth401`] / [`FindPrError::Auth403`] so the
/// operator-facing warn line names the specific variable they need
/// to fix.
///
/// Returns `Ok(Vec::new())` for a genuine "no open PRs match" result,
/// and `Err(FindPrError::...)` for every failure mode. Callers must
/// surface the error variant in the warn line — collapsing
/// `Err(...)` to `Vec::new()` reintroduces the auth-blindness bug
/// this signature exists to prevent.
pub(crate) fn find_open_pr_numbers_for_head(
    upstream_owner: &str,
    upstream_repo: &str,
    fork_owner: &str,
    branch: &str,
    token: Option<&str>,
    env_hint: &str,
) -> Result<Vec<u64>, FindPrError> {
    find_open_pr_numbers_for_head_with_env(
        upstream_owner,
        upstream_repo,
        fork_owner,
        branch,
        token,
        env_hint,
        &ProcessEnvSource,
    )
}

/// Env-injectable form of [`find_open_pr_numbers_for_head`] — resolves the
/// GitHub API base through `env` (honoring `ANODIZER_GITHUB_API_BASE`) so a
/// rollback driven by an injected [`MapEnvSource`](anodizer_core::MapEnvSource)
/// can be pointed at an in-process responder without touching the process env.
/// Production passes [`ProcessEnvSource`], where the override is unset and the
/// base resolves to the canonical `https://api.github.com`.
pub(crate) fn find_open_pr_numbers_for_head_with_env<E: EnvSource + ?Sized>(
    upstream_owner: &str,
    upstream_repo: &str,
    fork_owner: &str,
    branch: &str,
    token: Option<&str>,
    env_hint: &str,
    env: &E,
) -> Result<Vec<u64>, FindPrError> {
    const PAGE_CAP: usize = 10;
    let base = github_api_base_from(env);
    let head = format!("{}:{}", fork_owner, branch);
    let first_url = format!(
        "{}/repos/{}/{}/pulls?state=open&head={}&per_page=100",
        base, upstream_owner, upstream_repo, head
    );

    let client = anodizer_core::http::blocking_client(Duration::from_secs(15)).map_err(|e| {
        FindPrError::Network {
            url: first_url.clone(),
            source: e,
        }
    })?;

    let mut next_url: Option<String> = Some(first_url);
    let mut out: Vec<u64> = Vec::new();
    for _page in 0..PAGE_CAP {
        let Some(url) = next_url.take() else {
            break;
        };
        let mut req = client
            .get(&url)
            .header("Accept", "application/vnd.github+json");
        if let Some(tok) = token {
            req = req.bearer_auth(tok);
        }
        let resp = req.send().map_err(|e| FindPrError::Network {
            url: url.clone(),
            source: anyhow::Error::new(e),
        })?;
        let status = resp.status();
        // Capture Link header BEFORE consuming the response body.
        let link_header = resp
            .headers()
            .get(reqwest::header::LINK)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        // The blocking response is consumed at most once. For non-2xx
        // statuses we drain it into a String for the error carrier; for
        // 2xx we hand it to serde-json below. `classify_find_pr_status`
        // takes a body supplier rather than the body up-front so we
        // don't pay the read for success cases.
        if !status.is_success() {
            let body = anodizer_core::http::body_of_blocking(resp);
            return Err(
                classify_find_pr_status(status, &url, env_hint, || body.clone()).unwrap_err(),
            );
        }
        let body: serde_json::Value = resp.json().map_err(|e| FindPrError::Network {
            url: url.clone(),
            source: anyhow::Error::new(e),
        })?;
        if let Some(arr) = body.as_array() {
            out.extend(
                arr.iter()
                    .filter_map(|pr| pr.get("number").and_then(|n| n.as_u64())),
            );
        }
        next_url = link_header.as_deref().and_then(parse_link_header_next);
        if next_url.is_none() {
            break;
        }
    }
    Ok(out)
}

/// Extract the `rel="next"` URL from an RFC-5988 `Link` header, if
/// present. Returns `None` when the header is malformed, missing a
/// `rel="next"` entry, or empty.
///
/// Example input:
/// `<https://api.github.com/repos/o/r/pulls?page=2>; rel="next", <...>; rel="last"`
///
/// Pure helper so pagination can be unit-tested without firing an
/// HTTP request.
pub(crate) fn parse_link_header_next(header: &str) -> Option<String> {
    for part in header.split(',') {
        let part = part.trim();
        // Each part: `<URL>; rel="next"`
        let Some((url_part, params)) = part.split_once(';') else {
            continue;
        };
        let url = url_part
            .trim()
            .trim_start_matches('<')
            .trim_end_matches('>');
        if url.is_empty() {
            continue;
        }
        // The params section may have multiple `; key="value"` pairs.
        let mut is_next = false;
        for kv in params.split(';') {
            let kv = kv.trim();
            // Match `rel="next"` (also tolerate unquoted form just in
            // case — GitHub always quotes but RFC allows either).
            if kv == "rel=\"next\"" || kv == "rel=next" {
                is_next = true;
                break;
            }
        }
        if is_next {
            return Some(url.to_string());
        }
    }
    None
}

/// Close a PR via `PATCH /repos/{owner}/{repo}/pulls/{n}` with
/// `{"state": "closed"}`.
///
/// Returns a [`CloseOutcome`] enum: `Closed` for fresh-close 2xx,
/// `AlreadyClosed` for 404 / 410 / 422 (desired end-state already
/// true), `Failed` for anything else. Callers should bucket
/// outcomes into closed/already-closed/failed counters and surface
/// per-failure warns on `Failed` only.
pub(crate) fn close_pr_via_api(
    upstream_owner: &str,
    upstream_repo: &str,
    pr_number: u64,
    token: &str,
) -> CloseOutcome {
    close_pr_via_api_with_env(
        upstream_owner,
        upstream_repo,
        pr_number,
        token,
        &ProcessEnvSource,
    )
}

/// Env-injectable form of [`close_pr_via_api`] — resolves the GitHub API base
/// through `env` (honoring `ANODIZER_GITHUB_API_BASE`) so a rollback can be
/// driven against an in-process responder without mutating the process env.
/// Production passes [`ProcessEnvSource`] and reaches `https://api.github.com`.
pub(crate) fn close_pr_via_api_with_env<E: EnvSource + ?Sized>(
    upstream_owner: &str,
    upstream_repo: &str,
    pr_number: u64,
    token: &str,
    env: &E,
) -> CloseOutcome {
    let base = github_api_base_from(env);
    let url = format!(
        "{}/repos/{}/{}/pulls/{}",
        base, upstream_owner, upstream_repo, pr_number
    );
    let client = match anodizer_core::http::blocking_client(Duration::from_secs(30))
        .context("github_pr: build blocking HTTP client")
    {
        Ok(c) => c,
        Err(e) => return CloseOutcome::Failed(format!("transport: {}", e)),
    };
    let payload = serde_json::json!({ "state": "closed" });
    let resp = client
        .patch(&url)
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .json(&payload)
        .send();
    match resp {
        Ok(r) => {
            let status = r.status();
            let body = anodizer_core::http::body_of_blocking(r);
            classify_close_status(status, &body)
        }
        Err(e) => CloseOutcome::Failed(format!("transport: {}", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lookup must surface an `Err` variant (not panic, not silently
    /// return `Ok(Vec::new())`) when the request fails — auth-blindness
    /// (the bug `FindPrError` exists to prevent) must never collapse to
    /// `Ok(empty)`. Driven through the `_with_env` seam with the API base
    /// pointed at a guaranteed-dead local URL (`127.0.0.1:1`) so the
    /// transport always errors into `FindPrError::Network`. Firing at the
    /// real `api.github.com` made the variant environment-dependent
    /// (404→`RepoNotFound` with internet, transport-fail offline);
    /// pinning the dead URL makes it deterministic and hermetic.
    #[test]
    fn find_open_pr_numbers_for_head_returns_err_on_failure() {
        let env = anodizer_core::MapEnvSource::new()
            .with("ANODIZER_GITHUB_API_BASE", "http://127.0.0.1:1");
        let result = find_open_pr_numbers_for_head_with_env(
            "this-org-does-not-exist-anodize",
            "neither-does-this-repo-anodize",
            "ghost",
            "branch",
            None,
            "KREW_INDEX_TOKEN",
            &env,
        );
        assert!(
            matches!(result, Err(FindPrError::Network { .. })),
            "expected Network err (dead URL), got {:?}",
            result
        );
    }

    /// `close_pr_via_api` against an unreachable target must bucket into
    /// `Failed(_)` (transport-error variant), not panic. Driven through
    /// the `_with_env` seam with the GitHub API base pointed at a
    /// guaranteed-dead local URL (`127.0.0.1:1`) so the transport always
    /// errors — firing at the real `api.github.com` made this flaky, since
    /// a reachable host returning 404 classifies as `AlreadyClosed`, not
    /// `Failed`. Hermetic: no process-env mutation, no network egress.
    #[test]
    fn close_pr_via_api_failed_when_target_unreachable() {
        let env = anodizer_core::MapEnvSource::new()
            .with("ANODIZER_GITHUB_API_BASE", "http://127.0.0.1:1");
        let result = close_pr_via_api_with_env(
            "this-org-does-not-exist-anodize",
            "neither-does-this-repo-anodize",
            999,
            "ghs_invalidtoken",
            &env,
        );
        assert!(matches!(result, CloseOutcome::Failed(_)));
    }

    // -----------------------------------------------------------------
    // classify_close_status — unit-tested without firing HTTP
    // -----------------------------------------------------------------

    #[test]
    fn close_pr_via_api_treats_2xx_as_closed() {
        for code in [200u16, 201, 204] {
            let status = reqwest::StatusCode::from_u16(code).unwrap();
            let outcome = classify_close_status(status, "{}");
            assert_eq!(outcome, CloseOutcome::Closed, "code {}", code);
        }
    }

    #[test]
    fn close_pr_via_api_treats_404_as_already_closed() {
        let outcome = classify_close_status(reqwest::StatusCode::NOT_FOUND, "{}");
        assert_eq!(outcome, CloseOutcome::AlreadyClosed);
    }

    #[test]
    fn close_pr_via_api_treats_410_as_already_closed() {
        let outcome = classify_close_status(reqwest::StatusCode::GONE, "{}");
        assert_eq!(outcome, CloseOutcome::AlreadyClosed);
    }

    #[test]
    fn close_pr_via_api_treats_422_as_already_closed() {
        // GitHub's actual response code for "PR already in closed
        // state" — observed when a maintainer closed the PR between
        // our `find_open_pr_numbers` query and our PATCH.
        let outcome = classify_close_status(reqwest::StatusCode::UNPROCESSABLE_ENTITY, "{}");
        assert_eq!(outcome, CloseOutcome::AlreadyClosed);
    }

    #[test]
    fn close_pr_via_api_treats_5xx_as_failed() {
        for code in [500u16, 502, 503, 504] {
            let status = reqwest::StatusCode::from_u16(code).unwrap();
            let outcome = classify_close_status(status, "upstream broken");
            assert!(
                matches!(outcome, CloseOutcome::Failed(ref s) if s.contains("upstream broken")),
                "code {} body propagation",
                code
            );
        }
    }

    #[test]
    fn close_pr_via_api_treats_4xx_other_as_failed() {
        // 401 / 403 / 400 / 401 / 429 are real failures — not
        // already-closed. Operator must see them in the failure bucket.
        for code in [400u16, 401, 403, 429] {
            let status = reqwest::StatusCode::from_u16(code).unwrap();
            let outcome = classify_close_status(status, "auth bad");
            assert!(
                matches!(outcome, CloseOutcome::Failed(_)),
                "code {} must be Failed (not AlreadyClosed)",
                code
            );
        }
    }

    // -----------------------------------------------------------------
    // classify_find_pr_status — auth-vs-empty-result discrimination
    // unit-tested without firing HTTP
    // -----------------------------------------------------------------

    #[test]
    fn find_open_pr_numbers_returns_auth401_on_401_response() {
        let result = classify_find_pr_status(
            reqwest::StatusCode::UNAUTHORIZED,
            "https://api.github.com/repos/o/r/pulls",
            "KREW_INDEX_TOKEN",
            || "{\"message\":\"Bad credentials\"}".to_string(),
        );
        match result {
            Err(FindPrError::Auth401 { env_hint, .. }) => {
                assert_eq!(env_hint, "KREW_INDEX_TOKEN");
            }
            other => panic!("expected Auth401, got {:?}", other),
        }
    }

    #[test]
    fn find_open_pr_numbers_returns_auth403_on_403_response() {
        let result = classify_find_pr_status(
            reqwest::StatusCode::FORBIDDEN,
            "https://api.github.com/repos/o/r/pulls",
            "KREW_INDEX_TOKEN",
            || "{\"message\":\"Forbidden\"}".to_string(),
        );
        match result {
            Err(FindPrError::Auth403 { env_hint, .. }) => {
                assert_eq!(env_hint, "KREW_INDEX_TOKEN");
            }
            other => panic!("expected Auth403, got {:?}", other),
        }
    }

    #[test]
    fn find_open_pr_numbers_returns_repo_not_found_on_404_response() {
        let result = classify_find_pr_status(
            reqwest::StatusCode::NOT_FOUND,
            "https://api.github.com/repos/o/r/pulls",
            "KREW_INDEX_TOKEN",
            || "{}".to_string(),
        );
        assert!(matches!(result, Err(FindPrError::RepoNotFound { .. })));
    }

    #[test]
    fn find_open_pr_numbers_returns_other_on_5xx_response() {
        let result = classify_find_pr_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "https://api.github.com/repos/o/r/pulls",
            "KREW_INDEX_TOKEN",
            || "upstream broken".to_string(),
        );
        match result {
            Err(FindPrError::Other { status, body, .. }) => {
                assert_eq!(status, reqwest::StatusCode::INTERNAL_SERVER_ERROR);
                assert!(body.contains("upstream broken"));
            }
            other => panic!("expected Other(5xx), got {:?}", other),
        }
    }

    #[test]
    fn find_open_pr_numbers_returns_ok_on_2xx_response() {
        for code in [200u16, 201, 204] {
            let status = reqwest::StatusCode::from_u16(code).unwrap();
            let result = classify_find_pr_status(status, "https://...", "KREW_INDEX_TOKEN", || {
                panic!("body supplier must not be invoked on 2xx")
            });
            assert!(result.is_ok(), "code {} should classify as Ok", code);
        }
    }

    // -----------------------------------------------------------------
    // parse_link_header_next — RFC-5988 parsing without HTTP
    // -----------------------------------------------------------------

    #[test]
    fn parse_link_header_next_returns_next_url() {
        let h = "<https://api.github.com/repos/o/r/pulls?page=2>; rel=\"next\", \
                 <https://api.github.com/repos/o/r/pulls?page=5>; rel=\"last\"";
        let next = parse_link_header_next(h);
        assert_eq!(
            next.as_deref(),
            Some("https://api.github.com/repos/o/r/pulls?page=2")
        );
    }

    #[test]
    fn parse_link_header_next_returns_none_when_no_next_rel() {
        let h = "<https://api.github.com/repos/o/r/pulls?page=5>; rel=\"last\"";
        assert!(parse_link_header_next(h).is_none());
    }

    #[test]
    fn parse_link_header_next_handles_empty_or_malformed() {
        assert!(parse_link_header_next("").is_none());
        assert!(parse_link_header_next("garbage").is_none());
        assert!(parse_link_header_next("<>; rel=\"next\"").is_none());
    }

    #[test]
    fn parse_link_header_next_accepts_unquoted_rel() {
        // RFC-5988 allows unquoted rel values; GitHub always quotes,
        // but the parser shouldn't choke if a future server doesn't.
        let h = "<https://api.example.com/p?page=2>; rel=next";
        assert_eq!(
            parse_link_header_next(h).as_deref(),
            Some("https://api.example.com/p?page=2")
        );
    }
}
