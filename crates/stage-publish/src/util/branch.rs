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

/// Resolve the GitHub REST API base URL. Honors the undocumented
/// `ANODIZER_GITHUB_API_BASE` env override so unit tests can redirect
/// requests to an in-process responder; defaults to the canonical
/// `https://api.github.com` in production where the var is unset. Any
/// trailing `/` is stripped so callers can unconditionally `format!`
/// with a `/`-prefixed suffix without producing a double slash.
pub(super) fn github_api_base() -> String {
    let raw = std::env::var("ANODIZER_GITHUB_API_BASE")
        .unwrap_or_else(|_| "https://api.github.com".to_string());
    raw.trim_end_matches('/').to_string()
}

/// Look up a GitHub repo's `default_branch` via the REST API. Returns `None`
/// on any failure (token missing, network error, repo not found, parse
/// failure) so the caller can fall back to a sensible default.
pub(super) fn fetch_default_branch(owner: &str, name: &str, token: Option<&str>) -> Option<String> {
    let base = github_api_base();
    let url = format!("{base}/repos/{owner}/{name}");
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

    // -----------------------------------------------------------------
    // `fetch_default_branch` HTTP coverage. Each test redirects requests
    // to an in-process responder by setting `ANODIZER_GITHUB_API_BASE`
    // under the workspace env mutex (`cargo test` parallelises within a
    // single binary, and the var is read by sibling tests in this file).
    // -----------------------------------------------------------------

    use anodizer_core::test_helpers::env::env_mutex;
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

    /// Acquire the env mutex and point `ANODIZER_GITHUB_API_BASE` at the
    /// given in-process responder. The returned guard restores the
    /// pre-test env on drop so a panicking test body cannot leak the
    /// override into sibling tests.
    struct BaseOverride {
        _guard: std::sync::MutexGuard<'static, ()>,
        previous: Option<String>,
    }

    impl BaseOverride {
        fn set(base: &str) -> Self {
            let guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
            let previous = std::env::var("ANODIZER_GITHUB_API_BASE").ok();
            // SAFETY: serialised by the workspace env mutex; pair set / restore.
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

    /// 200 with `{"default_branch":"master"}` is the upstream path used
    /// by `submit_pr_via_gh_with_opts` when discovering the base ref of
    /// repos whose default is `master` (e.g. `microsoft/winget-pkgs`).
    /// Returning `Some("master")` is what stops the caller from
    /// defaulting to `"main"` and producing the tangled "Base ref must
    /// be a branch" GraphQL error documented in the caller.
    #[test]
    fn fetch_default_branch_returns_master_on_200() {
        let (addr, _calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 27\r\n\r\n{\"default_branch\":\"master\"}",
        ]);
        let _ov = BaseOverride::set(&format!("http://{addr}"));
        let got = fetch_default_branch("o", "n", None);
        assert_eq!(got.as_deref(), Some("master"));
    }

    /// Sanity for the parse path — the function must surface whatever
    /// branch name the API returns, not pin to a hardcoded value.
    #[test]
    fn fetch_default_branch_returns_main_on_200() {
        let (addr, _calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 25\r\n\r\n{\"default_branch\":\"main\"}",
        ]);
        let _ov = BaseOverride::set(&format!("http://{addr}"));
        let got = fetch_default_branch("o", "n", None);
        assert_eq!(got.as_deref(), Some("main"));
    }

    /// 404 returns `None` so the caller falls back to `"main"` — pins
    /// the documented "non-existent repo silently degrades" contract.
    /// A regression that propagated the error instead would break
    /// `submit_pr_via_gh_with_opts` on any typo'd `repository:` slug.
    #[test]
    fn fetch_default_branch_returns_none_on_404() {
        let (addr, _calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
        ]);
        let _ov = BaseOverride::set(&format!("http://{addr}"));
        assert!(fetch_default_branch("o", "n", None).is_none());
    }

    /// 500 returns `None` — the function silently degrades on server
    /// error too, not just 404. A regression that surfaced the 5xx as
    /// an `Err` would gate PR creation on transient upstream outages.
    #[test]
    fn fetch_default_branch_returns_none_on_500() {
        let (addr, _calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
        ]);
        let _ov = BaseOverride::set(&format!("http://{addr}"));
        assert!(fetch_default_branch("o", "n", None).is_none());
    }

    /// Malformed JSON returns `None`. The body parses with `serde_json`
    /// so an HTML error page (common when an auth proxy intercepts the
    /// request) must NOT panic or propagate; the silent-fallback
    /// contract is the whole point of returning `Option`.
    #[test]
    fn fetch_default_branch_returns_none_on_malformed_json() {
        let (addr, _calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 17\r\n\r\n<html>oops</html>",
        ]);
        let _ov = BaseOverride::set(&format!("http://{addr}"));
        assert!(fetch_default_branch("o", "n", None).is_none());
    }
}
