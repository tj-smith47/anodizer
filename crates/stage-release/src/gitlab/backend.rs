use super::*;

// ---------------------------------------------------------------------------
// Backend orchestration
// ---------------------------------------------------------------------------

/// Runtime / context infrastructure for [`run_gitlab_backend`].
///
/// Bundles the four "ambient" handles every backend call needs (matches the
/// shape of `github::BackendEnv`) so the function signature stays under
/// clippy's 7-argument threshold.
pub(crate) struct GitlabBackendEnv<'a> {
    pub rt: &'a tokio::runtime::Runtime,
    pub ctx: &'a anodizer_core::context::Context,
    pub log: &'a anodizer_core::log::StageLogger,
    pub token: &'a Option<String>,
}

/// Per-release inputs the orchestrator forwards from `ReleaseStage::run` to
/// [`run_gitlab_backend`]. Bundled so the function signature stays under
/// clippy's 7-argument threshold without an attribute suppression.
#[derive(Clone, Copy)]
pub(crate) struct GitlabBackendSpec<'a> {
    pub tag: &'a str,
    pub release_name: &'a str,
    pub release_body: &'a str,
    pub release_mode: &'a str,
    pub skip_upload: bool,
    pub replace_existing_draft: bool,
    pub use_existing_draft: bool,
    pub replace_existing_artifacts: bool,
}

/// Run the GitLab release backend for one crate.
///
/// Returns `(release_html_url, download_base, owner, repo_name)` on success,
/// or `Ok(None)` when the crate has no `release.gitlab` (or fallback
/// `release.github`) configuration — callers should `continue` the outer
/// loop after this helper logs the "no gitlab config" warning.
pub(crate) fn run_gitlab_backend(
    env: &GitlabBackendEnv<'_>,
    crate_cfg: &anodizer_core::config::CrateConfig,
    release_cfg: &anodizer_core::config::ReleaseConfig,
    spec: &GitlabBackendSpec<'_>,
    artifact_entries: &[(std::path::PathBuf, Option<String>)],
) -> Result<Option<(String, String, String, String)>> {
    use std::sync::Arc;

    let GitlabBackendEnv {
        rt,
        ctx,
        log,
        token,
    } = env;
    let ctx = *ctx;
    let log = *log;
    let token = *token;

    let repo_cfg = match crate::resolve_release_repo(release_cfg, ctx.token_type, ctx)? {
        Some(r) => r,
        None => {
            log.warn(&format!(
                "skipped release for crate '{}' — no gitlab config",
                crate_cfg.name
            ));
            return Ok(None);
        }
    };

    let token_str = match token {
        Some(t) => t.clone(),
        None => {
            bail!("release: no GitLab token available (set GITLAB_TOKEN, or pass --token)");
        }
    };

    let gitlab_urls = ctx.config.gitlab_urls.clone().unwrap_or_default();
    let api_url = gitlab_urls
        .api
        .unwrap_or_else(|| "https://gitlab.com/api/v4".to_string());
    let download_url = gitlab_urls
        .download
        .unwrap_or_else(|| "https://gitlab.com".to_string());
    let skip_tls = gitlab_urls.skip_tls_verify.unwrap_or(false);
    // Only send JOB-TOKEN when
    // CI_JOB_TOKEN is set, the flag is on, and the token equals CI_JOB_TOKEN.
    // Otherwise fall back to PRIVATE-TOKEN.
    let use_job_token = resolve_use_job_token_with_env(
        gitlab_urls.use_job_token.unwrap_or(false),
        &token_str,
        ctx.env_source(),
    );
    let use_pkg_registry = gitlab_urls.use_package_registry.unwrap_or(false) || use_job_token;

    let project_id = gitlab_project_id(&repo_cfg.owner, &repo_cfg.name);
    let commit_sha = ctx
        .git_info
        .as_ref()
        .map(|g| g.commit.clone())
        .unwrap_or_default();

    let project_name_for_pkg = ctx.config.project_name.clone();
    let version_for_pkg = ctx
        .git_info
        .as_ref()
        .map(|g| {
            // Strip leading 'v' for package version (e.g. "v1.2.3" -> "1.2.3").
            g.tag.strip_prefix('v').unwrap_or(&g.tag).to_string()
        })
        .unwrap_or_else(|| "0.0.0".to_string());

    // GitLab does not support draft releases — warn if draft options are set.
    if spec.replace_existing_draft {
        log.warn(
            "replace_existing_draft has no effect on GitLab (draft releases are not supported)",
        );
    }
    if spec.use_existing_draft {
        log.warn("use_existing_draft has no effect on GitLab (draft releases are not supported)");
    }

    // Per-publisher retry policy. 5xx / 429 / network errors retry with
    // exponential backoff through `retry_http_async` inside every gitlab_*
    // function. Default: 10 attempts × 10s base × 5m cap (the
    // `pkg/config.Retry` defaults).
    let policy = ctx.retry_policy();
    let deadline = ctx.retry_deadline();
    let tag = spec.tag;
    let release_name = spec.release_name;
    let release_body = spec.release_body;
    let release_mode = spec.release_mode;
    let skip_upload = spec.skip_upload;
    let replace_existing_artifacts = spec.replace_existing_artifacts;

    let url = rt.block_on(async {
        let client = build_gitlab_client(&token_str, skip_tls, use_job_token)?;

        let gitlab_ctx = GitlabCtx {
            client: &client,
            api_url: &api_url,
            project_id: &project_id,
            policy: &policy,
            deadline,
            log,
        };

        // Create or update the release.
        gitlab_create_release(
            &gitlab_ctx,
            &GitlabReleaseSpec {
                tag,
                name: release_name,
                body: release_body,
                commit: &commit_sha,
                release_mode,
            },
        )
        .await?;

        log.status(&format!(
            "created GitLab Release '{}' (tag={}) on {}",
            release_name, tag, project_id
        ));

        // Upload artifacts through the shared forge upload loop (probe /
        // idempotent-skip / delete-then-upload policy lives in
        // `forge::run_upload_loop`; this backend contributes only the
        // GitLab API calls).
        if skip_upload {
            log.status("skipped artifact uploads — skip_upload is set");
        } else {
            let plan = crate::forge::UploadPlan::resolve(
                release_cfg,
                ctx.env_source(),
                replace_existing_artifacts,
            );
            let forge_client = Arc::new(GitlabAssetClient {
                probe_client: build_gitlab_probe_client(&token_str, skip_tls, use_job_token)?,
                client,
                api_url: api_url.clone(),
                project_id: project_id.clone(),
                policy,
                deadline,
                tag: tag.to_string(),
                download_url: download_url.clone(),
                pkg: use_pkg_registry
                    .then(|| (project_name_for_pkg.clone(), version_for_pkg.clone())),
                replace_existing_artifacts,
                log: log.clone(),
            });
            crate::forge::run_upload_loop(forge_client, &plan, artifact_entries, log).await?;
        }

        // GitLab does not support draft releases — publish is a no-op.

        let html_url = gitlab_release_url(&download_url, &repo_cfg.owner, &repo_cfg.name, tag);
        Ok::<String, anyhow::Error>(html_url)
    })?;

    Ok(Some((
        url,
        download_url,
        repo_cfg.owner.clone(),
        repo_cfg.name.clone(),
    )))
}
