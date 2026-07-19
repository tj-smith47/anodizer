use anyhow::Result;

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;

use super::{
    fetch_git_commits, fetch_gitea_commits, fetch_github_commits, fetch_gitlab_commits,
    should_preempt_scm_to_git,
};
use crate::group::CommitInfo;

/// Build the GitHub login enricher for this run, or `None` when enrichment
/// cannot apply: a non-GitHub changelog source (`gitlab`/`gitea` logins live
/// in a different namespace), no token through the standard chain
/// (`--token` → `ANODIZER_GITHUB_TOKEN` → `GITHUB_TOKEN`; no ambient-auth
/// lookups, by contract — unauthenticated runs keep name-based rendering),
/// or no derivable GitHub target (`release.github` config, falling back to
/// the `origin` remote).
pub(crate) fn build_login_enricher(
    ctx: &Context,
    use_source: &str,
) -> Option<crate::enrich::LoginEnricher<'static>> {
    if !crate::enrich::use_source_supports_github_logins(use_source) {
        return None;
    }
    let token =
        anodizer_core::git::resolve_github_token_with_env(ctx.options.token.as_deref(), &|key| {
            ctx.env_var(key)
        })?;
    let configured = ctx
        .config
        .crate_universe()
        .into_iter()
        .filter_map(|c| c.release.as_ref().and_then(|r| r.github.as_ref()))
        .map(|g| (g.owner.clone(), g.name.clone()))
        .find(|(o, n)| !o.is_empty() && !n.is_empty());
    let root = ctx
        .options
        .project_root
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let (owner, repo) = crate::enrich::derive_github_target(
        configured.as_ref().map(|(o, n)| (o.as_str(), n.as_str())),
        &root,
    )?;
    Some(crate::enrich::LoginEnricher::for_github_repo(
        owner, repo, token, &root,
    ))
}

/// Fetch commits for a crate via the configured SCM backend, with
/// fallback-to-git on transient SCM API failures (strict mode escalates
/// to an error). Returns `(commits, logins_str)`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn fetch_crate_commits(
    ctx: &mut Context,
    log: &StageLogger,
    use_source: &str,
    prev_tag: &Option<String>,
    to: Option<&str>,
    scope: &anodizer_core::changelog_scope::ChangelogScope,
    crate_name: &str,
    workspace_root: &std::path::Path,
) -> Result<(Vec<CommitInfo>, String)> {
    let paths = scope.pathspecs();
    let use_github = use_source == "github";
    let use_gitlab = use_source == "gitlab";
    let use_gitea = use_source == "gitea";

    // Pre-empt the SCM API call when there is no previous tag (first
    // release on a branch). The changelog backend does the
    // same: it warns and returns the git changeloger directly.
    let scm_no_prev_tag = should_preempt_scm_to_git(use_github, use_gitlab, use_gitea, prev_tag);
    if scm_no_prev_tag {
        let scm_label = if use_github {
            "github"
        } else if use_gitlab {
            "gitlab"
        } else {
            "gitea"
        };
        log.status(&format!(
            "no previous tag found — using 'git' instead of '{}' for crate '{}'",
            scm_label, crate_name
        ));
    }

    if scm_no_prev_tag {
        return Ok((
            fetch_git_commits_scoped(workspace_root, prev_tag, to, scope, crate_name, log)?,
            String::new(),
        ));
    }
    if use_github {
        match fetch_github_commits(ctx, prev_tag, paths, log) {
            Ok((infos, logins)) => return Ok((infos, logins)),
            Err(e) => {
                ctx.strict_guard(
                    log,
                    &format!(
                        "changelog GitHub API fetch failed, falling back to git: {}",
                        e
                    ),
                )?;
                return Ok((
                    fetch_git_commits_scoped(workspace_root, prev_tag, to, scope, crate_name, log)?,
                    String::new(),
                ));
            }
        }
    }
    if use_gitlab {
        match fetch_gitlab_commits(ctx, prev_tag, log) {
            Ok((infos, logins)) => return Ok((infos, logins)),
            Err(e) => {
                ctx.strict_guard(
                    log,
                    &format!(
                        "changelog GitLab API fetch failed, falling back to git: {}",
                        e
                    ),
                )?;
                return Ok((
                    fetch_git_commits_scoped(workspace_root, prev_tag, to, scope, crate_name, log)?,
                    String::new(),
                ));
            }
        }
    }
    if use_gitea {
        match fetch_gitea_commits(ctx, prev_tag, log) {
            Ok((infos, logins)) => return Ok((infos, logins)),
            Err(e) => {
                ctx.strict_guard(
                    log,
                    &format!(
                        "changelog Gitea API fetch failed, falling back to git: {}",
                        e
                    ),
                )?;
                return Ok((
                    fetch_git_commits_scoped(workspace_root, prev_tag, to, scope, crate_name, log)?,
                    String::new(),
                ));
            }
        }
    }
    Ok((
        fetch_git_commits_scoped(workspace_root, prev_tag, to, scope, crate_name, log)?,
        String::new(),
    ))
}

/// Fetch git commits scoped by `scope.dirs`, then apply the precise
/// `changelog.paths` glob intersect ([`scope.narrow`]) when one is required.
///
/// When no narrowing is needed the git pathspec already bounds the result
/// exactly, so this delegates to the metadata-only [`fetch_git_commits`].
/// Otherwise it fetches touched-file lists and drops commits whose files all
/// fall outside `changelog.paths` — the exact intersection of the derived
/// directory scope with the configured globs.
fn fetch_git_commits_scoped(
    workspace_root: &std::path::Path,
    prev_tag: &Option<String>,
    to: Option<&str>,
    scope: &anodizer_core::changelog_scope::ChangelogScope,
    crate_name: &str,
    log: &StageLogger,
) -> Result<Vec<CommitInfo>> {
    let paths = scope.pathspecs();
    if scope.narrow.is_none() {
        return fetch_git_commits(workspace_root, prev_tag, to, paths, crate_name, log);
    }
    crate::fetch::fetch_git_commits_narrowed(workspace_root, prev_tag, to, scope, crate_name, log)
}

#[cfg(test)]
mod login_enricher_gating_tests {
    use super::build_login_enricher;
    use anodizer_core::config::{CrateConfig, ReleaseConfig, ScmRepoConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    fn ctx_with(
        token: Option<&str>,
        github: Option<(&str, &str)>,
        root: std::path::PathBuf,
    ) -> anodizer_core::context::Context {
        ctx_with_env(token, github, root, &[])
    }

    /// Builds a context whose env source is map-backed (hermetic: the
    /// developer's or CI's real `GITHUB_TOKEN` can never leak into these
    /// gates). The empty-string defaults double as coverage that GHA's
    /// missing-secret materialization (`""`) counts as absent; `env` pairs
    /// override them.
    fn ctx_with_env(
        token: Option<&str>,
        github: Option<(&str, &str)>,
        root: std::path::PathBuf,
        env: &[(&str, &str)],
    ) -> anodizer_core::context::Context {
        let mut builder = TestContextBuilder::new()
            .project_name("test")
            .token(token.map(str::to_string))
            .project_root(root)
            .crates(vec![CrateConfig {
                name: "mylib".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                release: github.map(|(owner, name)| ReleaseConfig {
                    github: Some(ScmRepoConfig {
                        owner: owner.to_string(),
                        name: name.to_string(),
                        token: None,
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .env("ANODIZER_GITHUB_TOKEN", "")
            .env("GITHUB_TOKEN", "");
        for (k, v) in env {
            builder = builder.env(*k, *v);
        }
        builder.build()
    }

    /// No token anywhere in the chain (`--token` → `ANODIZER_GITHUB_TOKEN`
    /// → `GITHUB_TOKEN`) → no enricher, regardless of a configured GitHub
    /// target: unauthenticated runs keep name-based rendering and never
    /// attempt ambient-auth lookups.
    #[test]
    fn no_token_disables_enrichment() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with(None, Some(("octo", "repo")), tmp.path().to_path_buf());
        assert!(build_login_enricher(&ctx, "git").is_none());
        let ctx = ctx_with(Some(""), Some(("octo", "repo")), tmp.path().to_path_buf());
        assert!(
            build_login_enricher(&ctx, "git").is_none(),
            "empty token counts as absent"
        );
    }

    /// A run with only the `GITHUB_TOKEN` env var set (no `--token` flag —
    /// the standard GitHub Actions shape) must get pipeline enrichment,
    /// and `ANODIZER_GITHUB_TOKEN` must work the same way.
    #[test]
    fn env_token_alone_enables_enrichment() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with_env(
            None,
            Some(("octo", "repo")),
            tmp.path().to_path_buf(),
            &[("GITHUB_TOKEN", "gh-tok")],
        );
        assert!(
            build_login_enricher(&ctx, "git").is_some(),
            "GITHUB_TOKEN env alone must enable enrichment"
        );
        let ctx = ctx_with_env(
            None,
            Some(("octo", "repo")),
            tmp.path().to_path_buf(),
            &[("ANODIZER_GITHUB_TOKEN", "anod-tok")],
        );
        assert!(
            build_login_enricher(&ctx, "git").is_some(),
            "ANODIZER_GITHUB_TOKEN env alone must enable enrichment"
        );
    }

    /// GitLab/Gitea changelog sources never enrich from GitHub (different
    /// login namespaces), token or not.
    #[test]
    fn non_github_sources_disable_enrichment() {
        let tmp = tempfile::tempdir().unwrap();
        for src in ["gitlab", "gitea", "github-native"] {
            let ctx = ctx_with(
                Some("tok"),
                Some(("octo", "repo")),
                tmp.path().to_path_buf(),
            );
            assert!(
                build_login_enricher(&ctx, src).is_none(),
                "{src} must not enrich"
            );
        }
    }

    /// Token + configured `release.github` → enricher, for both the git and
    /// github sources (the latter backfills authors the compare API returned
    /// without a login).
    #[test]
    fn token_and_configured_target_enable_enrichment() {
        let tmp = tempfile::tempdir().unwrap();
        for src in ["git", "github"] {
            let ctx = ctx_with(
                Some("tok"),
                Some(("octo", "repo")),
                tmp.path().to_path_buf(),
            );
            assert!(
                build_login_enricher(&ctx, src).is_some(),
                "{src} must enrich"
            );
        }
    }

    /// Token but no configured target AND no GitHub remote (the project root
    /// is not even a git repo) → no enricher.
    #[test]
    fn no_derivable_target_disables_enrichment() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with(Some("tok"), None, tmp.path().to_path_buf());
        assert!(build_login_enricher(&ctx, "git").is_none());
    }
}
