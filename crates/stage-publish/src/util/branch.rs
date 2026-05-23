//! Branch helpers — resolve the configured push branch and look up
//! the upstream default-branch via the GitHub REST API.
//!
//! `fetch_default_branch` is `pub(super)` because the only caller is
//! `super::pr::submit_pr_via_gh_with_opts`; keeping it out of
//! `pub(crate)` keeps the surface tight.

use anodizer_core::config::RepositoryConfig;

/// Resolve the branch to push to from RepositoryConfig.
pub(crate) fn resolve_branch(repo: Option<&RepositoryConfig>) -> Option<&str> {
    repo.and_then(|r| r.branch.as_deref())
}

/// Look up a GitHub repo's `default_branch` via the REST API. Returns `None`
/// on any failure (token missing, network error, repo not found, parse
/// failure) so the caller can fall back to a sensible default.
pub(super) fn fetch_default_branch(owner: &str, name: &str, token: Option<&str>) -> Option<String> {
    let url = format!("https://api.github.com/repos/{}/{}", owner, name);
    let mut req = anodizer_core::http::blocking_client(std::time::Duration::from_secs(10))
        .ok()?
        .get(&url)
        .header("Accept", "application/vnd.github+json");
    if let Some(tok) = token {
        req = req.bearer_auth(tok);
    }
    let resp = req.send().ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = resp.json().ok()?;
    body.get("default_branch")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}
