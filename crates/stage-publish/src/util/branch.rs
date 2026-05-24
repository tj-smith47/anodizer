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

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::{GitRepoConfig, RepositoryConfig};

    /// `resolve_branch` returns `None` when the entire repo config is absent —
    /// callers must fall back to the upstream default-branch path rather than
    /// pushing to a fabricated branch name.
    #[test]
    fn resolve_branch_returns_none_when_repo_missing() {
        assert!(resolve_branch(None).is_none());
    }

    /// A repo config with no explicit `branch:` also returns `None` so the
    /// caller defers to the GitHub default-branch lookup.
    #[test]
    fn resolve_branch_returns_none_when_branch_unset() {
        let repo = RepositoryConfig {
            owner: Some("o".into()),
            name: Some("n".into()),
            branch: None,
            ..Default::default()
        };
        assert!(resolve_branch(Some(&repo)).is_none());
    }

    /// When `branch:` is explicitly set, that exact value is returned —
    /// the function is a pure projection, no normalisation, no defaulting.
    #[test]
    fn resolve_branch_returns_configured_branch_verbatim() {
        let repo = RepositoryConfig {
            branch: Some("release/v2".into()),
            ..Default::default()
        };
        assert_eq!(resolve_branch(Some(&repo)), Some("release/v2"));
    }

    /// Sister fields on the config (e.g. `git.url`) do not interfere — only
    /// `branch:` is consulted. Guards against a future refactor that
    /// accidentally swallows the SSH `git.url` into the branch slot.
    #[test]
    fn resolve_branch_ignores_unrelated_fields() {
        let repo = RepositoryConfig {
            branch: Some("main".into()),
            git: Some(GitRepoConfig {
                url: Some("ssh://git@example.com/x.git".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(resolve_branch(Some(&repo)), Some("main"));
    }

    // `fetch_default_branch` hardcodes the GitHub API base URL, so it
    // can't be redirected to spawn_oneshot_http_responder without a
    // production refactor (accept a base URL or inject the client).
    // Coverage for the 200/404/transport branches is deferred until
    // that refactor lands — a real-network test would add flakiness
    // and slow every developer's `cargo test` for marginal value.
}
