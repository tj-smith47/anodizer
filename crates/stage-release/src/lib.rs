use anodizer_core::config::PrereleaseConfig;
use anodizer_core::context::Context;
use anodizer_core::git;
use anodizer_core::log::{StageLogger, Verbosity};
use anodizer_core::scm::ScmTokenType;
use anyhow::{Context as _, Result};

/// Module-level logger for warnings emitted from helpers (and async upload
/// retry loops) that don't have runtime access to the stage's
/// `ctx.logger("release")`. Routes through StageLogger for consistent `[release]`
/// framing.
pub(crate) fn release_log() -> StageLogger {
    StageLogger::new("release", Verbosity::Normal)
}

mod gitea;
mod github;
mod gitlab;
mod release_body;
mod run;

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// retry_upload — shared exponential-backoff retry for upload operations
// ---------------------------------------------------------------------------

/// Retry an async upload operation with exponential backoff.
/// Matches GoReleaser: 10 attempts, 50ms initial delay, 30s cap.
/// Retries on every failure (GoReleaser wraps all upload errors as
/// `RetriableError`).
pub(crate) async fn retry_upload<F, Fut>(operation_name: &str, mut f: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    use anodizer_core::retry::{RetryPolicy, retry_async};
    use std::ops::ControlFlow;
    retry_async(&RetryPolicy::UPLOAD, |_attempt| {
        let fut = f();
        async move {
            match fut.await {
                Ok(()) => Ok(()),
                Err(e) => Err(ControlFlow::Continue(e)),
            }
        }
    })
    .await
    .with_context(|| format!("{operation_name}: retry exhausted"))
}

// ---------------------------------------------------------------------------
// populate_artifact_download_urls
// ---------------------------------------------------------------------------

/// Set `metadata["url"]` on every artifact for the given crate, constructing
/// the download URL from the SCM backend's download base, owner/repo, tag, and
/// artifact name. This matches GoReleaser's `ReleaseURLTemplate()` pattern and
/// allows publishers to resolve download URLs without explicit `url_template`.
pub(crate) fn populate_artifact_download_urls(
    ctx: &mut Context,
    crate_name: &str,
    token_type: ScmTokenType,
    download_base: &str,
    owner: &str,
    repo: &str,
    tag: &str,
) {
    let dl_base = download_base.trim_end_matches('/');
    let url_tag = anodizer_core::url::percent_encode_path_segment(tag);
    let url_prefix = match token_type {
        ScmTokenType::GitLab => {
            if owner.is_empty() {
                format!("{dl_base}/{repo}/-/releases/{url_tag}/downloads")
            } else {
                format!("{dl_base}/{owner}/{repo}/-/releases/{url_tag}/downloads")
            }
        }
        ScmTokenType::GitHub | ScmTokenType::Gitea => {
            format!("{dl_base}/{owner}/{repo}/releases/download/{url_tag}")
        }
    };
    for artifact in ctx.artifacts.all_mut() {
        if artifact.crate_name == crate_name && !artifact.name.is_empty() {
            let encoded_name = anodizer_core::url::percent_encode_path_segment(&artifact.name);
            artifact
                .metadata
                .insert("url".to_string(), format!("{url_prefix}/{encoded_name}"));
        }
    }
}

// ---------------------------------------------------------------------------
// render_repo_ref
// ---------------------------------------------------------------------------

/// Pick the `ScmRepoConfig` for the active token type (with github
/// fallback) and template-render its `owner` and `name` fields.
///
/// Returns `Ok(None)` when no matching block is configured.
pub(crate) fn resolve_release_repo(
    release_cfg: &anodizer_core::config::ReleaseConfig,
    token_type: ScmTokenType,
    ctx: &anodizer_core::context::Context,
) -> Result<Option<anodizer_core::config::ScmRepoConfig>> {
    let raw = match token_type {
        ScmTokenType::GitLab => release_cfg.gitlab.as_ref().or(release_cfg.github.as_ref()),
        ScmTokenType::Gitea => release_cfg.gitea.as_ref().or(release_cfg.github.as_ref()),
        ScmTokenType::GitHub => release_cfg.github.as_ref(),
    };
    let Some(repo) = raw else {
        return Ok(None);
    };
    let owner = ctx
        .render_template(&repo.owner)
        .with_context(|| format!("release: render repo.owner '{}'", repo.owner))?;
    let name = ctx
        .render_template(&repo.name)
        .with_context(|| format!("release: render repo.name '{}'", repo.name))?;
    Ok(Some(anodizer_core::config::ScmRepoConfig { owner, name }))
}

/// Compose the public release HTML URL for the active SCM provider.
pub(crate) fn compose_release_url(
    token_type: ScmTokenType,
    download_base: &str,
    owner: &str,
    repo: &str,
    tag: &str,
) -> String {
    let base = download_base.trim_end_matches('/');
    match token_type {
        ScmTokenType::GitHub | ScmTokenType::Gitea => {
            format!("{}/{}/{}/releases/tag/{}", base, owner, repo, tag)
        }
        ScmTokenType::GitLab => {
            format!("{}/{}/{}/-/releases/{}", base, owner, repo, tag)
        }
    }
}

// ---------------------------------------------------------------------------
// should_mark_prerelease
// ---------------------------------------------------------------------------

/// Decide whether the GitHub Release should be marked as a pre-release.
///
/// - `Auto`     – inspect the tag for common pre-release suffixes.
/// - `Bool(b)`  – use the explicit value regardless of the tag.
/// - `None`     – default to `false`.
///
/// # Divergence from GoReleaser (BY DESIGN)
///
/// GoReleaser evaluates `prerelease == "auto"` once at `Default()`-time
/// (`internal/pipe/release/release.go:76-85`): it inspects
/// `ctx.Semver.Prerelease` and stores a single `ctx.PreRelease` flag for the
/// whole release run. Every release in the run shares that one decision.
///
/// Anodizer evaluates per-tag at run time. Each crate in a workspace can
/// have an independent tag with its own prerelease suffix, so a single
/// global decision doesn't translate to the workspace model. For example,
/// a workspace release that bumps `core` to `v1.2.3` and `cli` to
/// `v0.4.0-rc.1` should mark only the `cli` release as prerelease — which
/// only works when the decision is per-tag, not per-run.
pub(crate) fn should_mark_prerelease(config: &Option<PrereleaseConfig>, tag: &str) -> bool {
    match config {
        Some(PrereleaseConfig::Auto) => git::parse_semver_tag(tag)
            .map(|sv| sv.is_prerelease())
            .unwrap_or(false),
        Some(PrereleaseConfig::Bool(b)) => *b,
        None => false,
    }
}

// build_release_body, collect_extra_files, resolve_make_latest,
// resolve_content_source, compose_body_for_mode, build_release_json,
// resolve_release_tag live in `release_body.rs`. Mode-resolution is on
// `ReleaseConfig::resolved_mode` (Session C lazy-defaults policy).

// ---------------------------------------------------------------------------
// ReleaseStage
// ---------------------------------------------------------------------------

pub struct ReleaseStage;
