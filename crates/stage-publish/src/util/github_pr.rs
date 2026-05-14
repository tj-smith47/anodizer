//! GitHub PR close + lookup helpers — used by Bundle C close-PR
//! rollback (krew today; reusable for any future publisher whose
//! rollback shape is "close the PR we opened against an upstream").
//!
//! Two helpers:
//! - [`find_open_pr_numbers_for_head`] — `GET /repos/{owner}/{repo}/pulls`
//!   with `head=<fork_owner>:<branch>` and `state=open`. Returns the
//!   PR numbers of every match. Bounded by the upstream's open-PR
//!   list (usually small); no pagination needed for the rollback path.
//! - [`close_pr_via_api`] — `PATCH /repos/{owner}/{repo}/pulls/{n}` with
//!   `{"state": "closed"}`. Best-effort: returns the underlying error
//!   on transport / non-2xx so callers can warn and continue.
//!
//! Why a raw http helper instead of extending `anodizer_core::GitHubClient`?
//! `GitHubClient` is a trait bound on release operations (create/list/
//! delete release, upload asset). Adding PR ops would muddy that
//! contract for the one publisher group that needs it. The raw
//! helper keeps the surface narrow and the dependency direction
//! correct (`stage-publish` → `core::http`, not vice versa).

use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};

/// Look up open PR numbers for a given `head=<fork_owner>:<branch>`
/// filter against `<upstream_owner>/<upstream_repo>`.
///
/// Returns an empty Vec on any of: missing token, non-success status,
/// malformed JSON. Rollback treats empty-result identically to
/// no-evidence and emits a warn — operator picks up the manual
/// cleanup.
///
/// Token resolution is the caller's responsibility: pass `None` for
/// public-repo queries (GitHub will rate-limit anonymously but it'll
/// work), or the resolved env-var value when available.
pub(crate) fn find_open_pr_numbers_for_head(
    upstream_owner: &str,
    upstream_repo: &str,
    fork_owner: &str,
    branch: &str,
    token: Option<&str>,
) -> Vec<u64> {
    let head = format!("{}:{}", fork_owner, branch);
    let url = format!(
        "https://api.github.com/repos/{}/{}/pulls?state=open&head={}&per_page=100",
        upstream_owner, upstream_repo, head
    );
    let Ok(client) = anodizer_core::http::blocking_client(Duration::from_secs(15)) else {
        return Vec::new();
    };
    let mut req = client
        .get(&url)
        .header("Accept", "application/vnd.github+json");
    if let Some(tok) = token {
        req = req.bearer_auth(tok);
    }
    let resp = match req.send() {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let body: serde_json::Value = match resp.json() {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    body.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|pr| pr.get("number").and_then(|n| n.as_u64()))
                .collect()
        })
        .unwrap_or_default()
}

/// Close a PR via `PATCH /repos/{owner}/{repo}/pulls/{n}` with
/// `{"state": "closed"}`.
///
/// Returns `Ok(())` on a 2xx response, `Err(...)` with the upstream
/// status + body otherwise. Callers should warn-and-continue on
/// error (rollback is best-effort).
pub(crate) fn close_pr_via_api(
    upstream_owner: &str,
    upstream_repo: &str,
    pr_number: u64,
    token: &str,
) -> Result<()> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/pulls/{}",
        upstream_owner, upstream_repo, pr_number
    );
    let client = anodizer_core::http::blocking_client(Duration::from_secs(30))
        .context("github_pr: build blocking HTTP client")?;
    let payload = serde_json::json!({ "state": "closed" });
    let resp = client
        .patch(&url)
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .json(&payload)
        .send()
        .with_context(|| format!("github_pr: PATCH {}", url))?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let body = anodizer_core::http::body_of_blocking(resp);
    Err(anyhow!(
        "github_pr: close PR {}/{}#{} returned HTTP {}: {}",
        upstream_owner,
        upstream_repo,
        pr_number,
        status,
        body
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With no network reachable / no auth, the lookup should return
    /// an empty Vec instead of panicking. Smoke test for the
    /// "graceful degradation on failure" contract.
    #[test]
    fn find_open_pr_numbers_for_head_returns_empty_on_failure() {
        // Point at a clearly bogus host so the request fails fast.
        // The wrapper resolves the host before timing out.
        let result = find_open_pr_numbers_for_head(
            "this-org-does-not-exist-anodize",
            "neither-does-this-repo-anodize",
            "ghost",
            "branch",
            None,
        );
        assert!(result.is_empty());
    }

    /// `close_pr_via_api` against a non-existent repo + bogus token
    /// should bubble an `Err`, not panic. Lets callers safely
    /// warn-and-continue.
    #[test]
    fn close_pr_via_api_errors_when_target_unreachable() {
        let result = close_pr_via_api(
            "this-org-does-not-exist-anodize",
            "neither-does-this-repo-anodize",
            999,
            "ghs_invalidtoken",
        );
        assert!(result.is_err());
    }
}
