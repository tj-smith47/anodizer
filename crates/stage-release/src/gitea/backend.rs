use super::*;

/// Runtime / context infrastructure for [`run_gitea_backend`].
///
/// Bundles the four "ambient" handles every backend call needs (matches the
/// shape of `github::BackendEnv`) so the function signature stays under
/// clippy's 7-argument threshold.
pub(crate) struct GiteaBackendEnv<'a> {
    pub rt: &'a tokio::runtime::Runtime,
    pub ctx: &'a anodizer_core::context::Context,
    pub log: &'a anodizer_core::log::StageLogger,
    pub token: &'a Option<String>,
}

/// Per-release inputs the orchestrator forwards from `ReleaseStage::run` to
/// [`run_gitea_backend`]. Bundled so the function signature stays under
/// clippy's 7-argument threshold without an attribute suppression.
#[derive(Clone, Copy)]
pub(crate) struct GiteaBackendSpec<'a> {
    pub tag: &'a str,
    pub release_name: &'a str,
    pub release_body: &'a str,
    pub release_mode: &'a str,
    pub draft: bool,
    pub prerelease: bool,
    pub skip_upload: bool,
    pub replace_existing_draft: bool,
    pub use_existing_draft: bool,
    pub replace_existing_artifacts: bool,
}

/// Run the Gitea release backend for one crate.
///
/// Returns `(release_html_url, download_base, owner, repo_name)` on success,
/// or `Ok(None)` when the crate has no `release.gitea` (or fallback
/// `release.github`) configuration — callers should `continue` the outer
/// loop after this helper logs the "no gitea config" warning.
pub(crate) fn run_gitea_backend(
    env: &GiteaBackendEnv<'_>,
    crate_cfg: &anodizer_core::config::CrateConfig,
    release_cfg: &anodizer_core::config::ReleaseConfig,
    spec: &GiteaBackendSpec<'_>,
    artifact_entries: &[(std::path::PathBuf, Option<String>)],
) -> Result<Option<(String, String, String, String)>> {
    use std::sync::Arc;

    let GiteaBackendEnv {
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
                "skipped release for crate '{}' — no gitea config",
                crate_cfg.name
            ));
            return Ok(None);
        }
    };

    let token_str = match token {
        Some(t) => t.clone(),
        None => {
            bail!("release: no Gitea token available (set GITEA_TOKEN, or pass --token)");
        }
    };

    let gitea_urls = ctx.config.gitea_urls.clone().unwrap_or_default();
    let api_url = gitea_urls
        .api
        .unwrap_or_else(|| "https://gitea.com/api/v1".to_string());
    let download_url = gitea_urls
        .download
        .unwrap_or_else(|| "https://gitea.com".to_string());
    let skip_tls = gitea_urls.skip_tls_verify.unwrap_or(false);

    let commit_sha = ctx
        .git_info
        .as_ref()
        .map(|g| g.commit.clone())
        .unwrap_or_default();

    // Gitea does not support draft releases robustly — warn if draft options are set.
    if spec.replace_existing_draft {
        log.warn("replace_existing_draft has no effect on Gitea (draft support is limited)");
    }
    if spec.use_existing_draft {
        log.warn("use_existing_draft has no effect on Gitea (draft support is limited)");
    }

    // Per-publisher retry policy. Same shape and rationale as GitLab.
    let policy = ctx.retry_policy();
    let deadline = ctx.retry_deadline();
    let tag = spec.tag;
    let release_name = spec.release_name;
    let release_body = spec.release_body;
    let release_mode = spec.release_mode;
    let skip_upload = spec.skip_upload;
    let replace_existing_artifacts = spec.replace_existing_artifacts;
    let draft = spec.draft;
    let prerelease = spec.prerelease;

    let url = rt.block_on(async {
        let client = build_gitea_client(&token_str, skip_tls)?;

        let gitea_ctx = GiteaCtx {
            client: &client,
            api_url: &api_url,
            owner: &repo_cfg.owner,
            repo: &repo_cfg.name,
            policy: &policy,
            deadline,
            log,
        };

        // Create or update the release.
        let release_id = gitea_create_release(
            &gitea_ctx,
            &GiteaReleaseSpec {
                tag,
                commit: &commit_sha,
                name: release_name,
                body: release_body,
                draft,
                prerelease,
                release_mode,
            },
        )
        .await?;

        log.status(&format!(
            "created Gitea Release '{}' (id={}, tag={}) on {}/{}",
            release_name, release_id, tag, repo_cfg.owner, repo_cfg.name
        ));

        // Upload artifacts through the shared forge upload loop (probe /
        // idempotent-skip / delete-then-upload policy lives in
        // `forge::run_upload_loop`; this backend contributes only the
        // Gitea API calls).
        if skip_upload {
            log.status("skipped artifact uploads — skip_upload is set");
        } else {
            let plan = crate::forge::UploadPlan::resolve(
                release_cfg,
                ctx.env_source(),
                replace_existing_artifacts,
            );
            let forge_client = Arc::new(GiteaAssetClient {
                client,
                api_url: api_url.clone(),
                owner: repo_cfg.owner.clone(),
                repo: repo_cfg.name.clone(),
                policy,
                deadline,
                release_id,
                tag: tag.to_string(),
                log: log.clone(),
            });
            crate::forge::run_upload_loop(forge_client, &plan, artifact_entries, log).await?;
        }

        // Gitea PublishRelease is a no-op.

        let html_url = gitea_release_url(&download_url, &repo_cfg.owner, &repo_cfg.name, tag);
        Ok::<String, anyhow::Error>(html_url)
    })?;

    Ok(Some((
        url,
        download_url,
        repo_cfg.owner.clone(),
        repo_cfg.name.clone(),
    )))
}
