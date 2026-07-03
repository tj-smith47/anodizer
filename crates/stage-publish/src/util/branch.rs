//! Branch helpers — resolve the configured push branch and look up
//! the upstream default-branch via the GitHub REST API.
//!
//! `fetch_default_branch_with_env` is `pub(super)` because the only caller
//! is `super::pr::submit_pr_via_gh_with_opts_with_env`, which threads the
//! Context's `EnvSource` so an in-process responder can intercept the
//! lookup without mutating the process env; keeping it out of `pub(crate)`
//! keeps the surface tight.

use anodizer_core::EnvSource;
use anodizer_core::config::RepositoryConfig;
use anodizer_core::context::Context;

/// Resolve the push branch from a RepositoryConfig, rendering its template.
///
/// Returns an owned, rendered branch name so a templated
/// `branch: "{{ .Env.RELEASE_BRANCH }}"` resolves before it reaches the
/// `git checkout -B` / `git push` argv; a malformed template falls back to
/// the raw value (matching the lenient render path).
pub(crate) fn resolve_branch(ctx: &Context, repo: Option<&RepositoryConfig>) -> Option<String> {
    repo.and_then(|r| r.branch.as_deref())
        .map(|b| ctx.render_template(b).unwrap_or_else(|_| b.to_string()))
}

/// Look up a GitHub repo's `default_branch` via the REST API, resolving
/// the API base through the injected `env` (honoring
/// `ANODIZER_GITHUB_API_BASE`) so an in-process responder can intercept
/// the request without mutating the process env. Returns `None` on any
/// failure (token missing, network error, repo not found, parse failure)
/// so the caller can fall back to a sensible default.
pub(super) fn fetch_default_branch_with_env<E: EnvSource + ?Sized>(
    owner: &str,
    name: &str,
    token: Option<&str>,
    env: &E,
) -> Option<String> {
    let base = anodizer_core::http::github_api_base(env);
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
    use anodizer_core::test_helpers::TestContextBuilder;

    /// `resolve_branch` returns `None` when the entire repo config is absent —
    /// callers must fall back to the upstream default-branch path rather than
    /// pushing to a fabricated branch name.
    #[test]
    fn resolve_branch_returns_none_when_repo_missing() {
        let ctx = TestContextBuilder::new().build();
        assert!(resolve_branch(&ctx, None).is_none());
    }

    /// A repo config with no explicit `branch:` also returns `None` so the
    /// caller defers to the GitHub default-branch lookup.
    #[test]
    fn resolve_branch_returns_none_when_branch_unset() {
        let ctx = TestContextBuilder::new().build();
        let repo = RepositoryConfig {
            owner: Some("o".into()),
            name: Some("n".into()),
            branch: None,
            ..Default::default()
        };
        assert!(resolve_branch(&ctx, Some(&repo)).is_none());
    }

    /// When `branch:` is explicitly set, that exact value is returned —
    /// no normalisation, no defaulting (plain string, no template).
    #[test]
    fn resolve_branch_returns_configured_branch_verbatim() {
        let ctx = TestContextBuilder::new().build();
        let repo = RepositoryConfig {
            branch: Some("release/v2".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_branch(&ctx, Some(&repo)).as_deref(),
            Some("release/v2")
        );
    }

    /// A templated `branch:` (`{{ .Env.X }}`) renders to the env value;
    /// the literal template text must never reach the git argv.
    #[test]
    fn resolve_branch_renders_template() {
        let mut ctx = TestContextBuilder::new().build();
        ctx.template_vars_mut()
            .set_env("RELEASE_BRANCH", "release/v9");
        let repo = RepositoryConfig {
            branch: Some("{{ .Env.RELEASE_BRANCH }}".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_branch(&ctx, Some(&repo)).as_deref(),
            Some("release/v9"),
            "templated branch must render to the env value, not the literal"
        );
    }

    /// Sister fields on the config (e.g. `git.url`) do not interfere — only
    /// `branch:` is consulted. Guards against a future refactor that
    /// accidentally swallows the SSH `git.url` into the branch slot.
    #[test]
    fn resolve_branch_ignores_unrelated_fields() {
        let ctx = TestContextBuilder::new().build();
        let repo = RepositoryConfig {
            branch: Some("main".into()),
            git: Some(GitRepoConfig {
                url: Some("ssh://git@example.com/x.git".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(resolve_branch(&ctx, Some(&repo)).as_deref(), Some("main"));
    }

    // -----------------------------------------------------------------
    // `fetch_default_branch_with_env` HTTP coverage. Each test redirects
    // requests to an in-process responder by injecting `ANODIZER_GITHUB_API_BASE`
    // through a [`MapEnvSource`] passed to `fetch_default_branch_with_env`
    // — no process env mutation, no env mutex acquisition, no shared
    // state with sibling tests.
    // -----------------------------------------------------------------

    use anodizer_core::MapEnvSource;
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

    fn env_with_base(base: &str) -> MapEnvSource {
        MapEnvSource::new().with("ANODIZER_GITHUB_API_BASE", base)
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
        let env = env_with_base(&format!("http://{addr}"));
        let got = fetch_default_branch_with_env("o", "n", None, &env);
        assert_eq!(got.as_deref(), Some("master"));
    }

    /// Sanity for the parse path — the function must surface whatever
    /// branch name the API returns, not pin to a hardcoded value.
    #[test]
    fn fetch_default_branch_returns_main_on_200() {
        let (addr, _calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 25\r\n\r\n{\"default_branch\":\"main\"}",
        ]);
        let env = env_with_base(&format!("http://{addr}"));
        let got = fetch_default_branch_with_env("o", "n", None, &env);
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
        let env = env_with_base(&format!("http://{addr}"));
        assert!(fetch_default_branch_with_env("o", "n", None, &env).is_none());
    }

    /// 500 returns `None` — the function silently degrades on server
    /// error too, not just 404. A regression that surfaced the 5xx as
    /// an `Err` would gate PR creation on transient upstream outages.
    #[test]
    fn fetch_default_branch_returns_none_on_500() {
        let (addr, _calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
        ]);
        let env = env_with_base(&format!("http://{addr}"));
        assert!(fetch_default_branch_with_env("o", "n", None, &env).is_none());
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
        let env = env_with_base(&format!("http://{addr}"));
        assert!(fetch_default_branch_with_env("o", "n", None, &env).is_none());
    }
}
