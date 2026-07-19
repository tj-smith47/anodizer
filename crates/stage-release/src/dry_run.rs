use anodizer_core::context::Context;
use anodizer_core::scm::ScmTokenType;
use anyhow::Result;

use crate::{compose_release_url, populate_artifact_download_urls, resolve_release_repo};

/// Per-release summary fields surfaced in dry-run output.
///
/// Bundles the long argument list for [`handle_dry_run`] so the signature
/// stays under clippy's threshold and the call site reads like a struct
/// literal rather than a positional dump.
pub(crate) struct DryRunSummary<'a> {
    pub(crate) crate_name: &'a str,
    pub(crate) release_name: &'a str,
    pub(crate) tag: &'a str,
    pub(crate) draft: bool,
    pub(crate) prerelease: bool,
    pub(crate) release_mode: &'a str,
    pub(crate) skip_upload: bool,
    pub(crate) retention_keep_last: Option<usize>,
    pub(crate) publish_repo_override: Option<(String, String)>,
    pub(crate) artifact_entries: &'a [(std::path::PathBuf, Option<String>)],
}

/// Resolve the dry-run download-base URL for the active SCM provider.
///
/// Falls back to the public default for each provider when no override is
/// configured. For Gitea, the download base is additionally derived from the
/// API URL by stripping the `/api/v1` suffix.
pub(crate) fn dry_run_download_base(ctx: &Context) -> String {
    anodizer_core::download_url::default_download_base(ctx)
}

/// Log every configured `<provider>_urls.*` value in dry-run output so the
/// user can see which override is active without re-running with a live
/// token.
fn log_dry_run_provider_urls(ctx: &Context, log: &anodizer_core::log::StageLogger) {
    match ctx.token_type {
        ScmTokenType::GitHub => {
            if let Some(urls) = &ctx.config.github_urls {
                if let Some(api) = &urls.api {
                    log.status(&format!("(dry-run) github_urls.api = {}", api));
                }
                if let Some(upload) = &urls.upload {
                    log.status(&format!("(dry-run) github_urls.upload = {}", upload));
                }
                if let Some(download) = &urls.download {
                    log.status(&format!("(dry-run) github_urls.download = {}", download));
                }
                if urls.skip_tls_verify.unwrap_or(false) {
                    log.status("(dry-run) github_urls.skip_tls_verify = true");
                }
            }
        }
        ScmTokenType::GitLab => {
            if let Some(urls) = &ctx.config.gitlab_urls {
                if let Some(api) = &urls.api {
                    log.status(&format!("(dry-run) gitlab_urls.api = {}", api));
                }
                if let Some(download) = &urls.download {
                    log.status(&format!("(dry-run) gitlab_urls.download = {}", download));
                }
                if urls.skip_tls_verify.unwrap_or(false) {
                    log.status("(dry-run) gitlab_urls.skip_tls_verify = true");
                }
                if urls.use_package_registry.unwrap_or(false) {
                    log.status("(dry-run) gitlab_urls.use_package_registry = true");
                }
                if urls.use_job_token.unwrap_or(false) {
                    log.status("(dry-run) gitlab_urls.use_job_token = true");
                }
            }
        }
        ScmTokenType::Gitea => {
            if let Some(urls) = &ctx.config.gitea_urls {
                if let Some(api) = &urls.api {
                    log.status(&format!("(dry-run) gitea_urls.api = {}", api));
                }
                if let Some(download) = &urls.download {
                    log.status(&format!("(dry-run) gitea_urls.download = {}", download));
                }
                if urls.skip_tls_verify.unwrap_or(false) {
                    log.status("(dry-run) gitea_urls.skip_tls_verify = true");
                }
            }
        }
    }
}

/// Emit dry-run telemetry for one crate's release and populate artifact
/// download URLs so publishers can render manifests with correct URLs even
/// when no real release was created.
pub(crate) fn handle_dry_run(
    ctx: &mut Context,
    log: &anodizer_core::log::StageLogger,
    release_cfg: &anodizer_core::config::ReleaseConfig,
    s: DryRunSummary<'_>,
) -> Result<()> {
    let backend_label = match ctx.token_type {
        ScmTokenType::GitLab => "GitLab",
        ScmTokenType::Gitea => "Gitea",
        ScmTokenType::GitHub => "GitHub",
    };

    log_dry_run_provider_urls(ctx, log);

    log.status(&format!(
        "(dry-run) would create {} Release '{}' (tag={}, draft={}, prerelease={}, mode={}) for crate '{}'",
        backend_label,
        s.release_name,
        s.tag,
        s.draft,
        s.prerelease,
        s.release_mode,
        s.crate_name,
    ));
    if let Some((owner, repo)) = &s.publish_repo_override {
        log.status(&format!(
            "(dry-run) would publish to override repo '{owner}/{repo}' (nightly.publish_repo)",
        ));
    }
    // retention_keep_last folds in the keep_single_release alias (=> Some(1)).
    if let Some(keep_last) = s.retention_keep_last {
        if keep_last == 1 {
            log.status(
                "(dry-run) would delete prior nightly release(s) before recreating (nightly retention keep_last=1 / keep_single_release)",
            );
        } else {
            log.status(&format!(
                "(dry-run) would keep the {keep_last} newest nightly release(s) and delete the rest, incl. their tags (nightly retention)",
            ));
        }
    }
    if s.skip_upload {
        log.status("(dry-run) skip_upload is set, would skip artifact uploads");
    } else {
        for (path, custom_name) in s.artifact_entries {
            if let Some(name) = custom_name {
                log.status(&format!(
                    "(dry-run) would upload artifact {} (as '{}')",
                    path.display(),
                    name,
                ));
            } else {
                log.status(&format!(
                    "(dry-run) would upload artifact {}",
                    path.display()
                ));
            }
        }
    }

    let dry_dl_base = dry_run_download_base(ctx);
    let dry_repo_cfg = resolve_release_repo(release_cfg, ctx.token_type, ctx)?;
    let (dry_owner, dry_repo) = dry_repo_cfg
        .as_ref()
        .map(|r| (r.owner.as_str(), r.name.as_str()))
        .unwrap_or(("", ""));
    populate_artifact_download_urls(
        ctx,
        s.crate_name,
        ctx.token_type,
        &dry_dl_base,
        dry_owner,
        dry_repo,
        s.tag,
    );
    if !dry_owner.is_empty() && !dry_repo.is_empty() {
        let dry_release_url =
            compose_release_url(ctx.token_type, &dry_dl_base, dry_owner, dry_repo, s.tag);
        ctx.set_release_url(&dry_release_url);
    }

    Ok(())
}
