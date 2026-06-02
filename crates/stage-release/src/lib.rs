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
pub use github::fetch_published_asset_names;
mod gitlab;
pub mod publisher;
mod release_body;
mod run;

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// retry_upload — shared exponential-backoff retry for upload operations
// ---------------------------------------------------------------------------

/// Retry an async upload operation with exponential backoff.
/// 10 attempts, 50ms initial delay, 30s cap.
///
/// # Layering note
///
/// As of P1.4, gitlab/gitea publishers themselves call `retry_http_async`
/// internally with the user's `Config.retry` policy. Wrapping those
/// already-retrying calls in `retry_upload` (here) produces nested-retry
/// behavior: the inner helper exhausts its policy first, then this outer
/// loop retries up to its own 10 attempts. The total worst-case latency
/// grows accordingly. This is intentional — the per-publisher inner
/// policy gives the user a configurable surface that didn't exist before,
/// and the outer loop stays as the safety net.
///
/// # Classifier alignment with the inner helpers
///
/// The inner `retry_http_async` already classifies via [`is_retriable`]
/// (5xx / 429 / network-substring → retry, 4xx → fast-fail). The outer
/// loop here MUST honor the same classification: blindly retrying every
/// `Err` would amplify a 4xx fast-fail by 10×, defeating the inner's
/// decision. We re-run [`is_retriable`] on the bubbled-up error and
/// `Break` on non-retriable failures, matching the inner's policy and
/// the intended retry envelope.
pub(crate) async fn retry_upload<F, Fut>(operation_name: &str, mut f: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    use anodizer_core::retry::{RetryPolicy, is_retriable, retry_async};
    use std::ops::ControlFlow;
    retry_async(&RetryPolicy::UPLOAD, |_attempt| {
        let fut = f();
        async move {
            match fut.await {
                Ok(()) => Ok(()),
                Err(e) if is_retriable(e.as_ref()) => Err(ControlFlow::Continue(e)),
                Err(e) => Err(ControlFlow::Break(e)),
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
/// artifact name. This is the release-URL-template pattern and
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

/// Pick the `ScmRepoConfig` for the active publish target and template-render
/// its `owner` and `name` fields.
///
/// Resolution order:
/// 1. Explicit `release.provider:`.
/// 2. Active SCM token type with provider-side fallback (the historical
///    behaviour — preserved so existing configs don't change shape).
///
/// Returns `Ok(None)` when no matching block is configured.
pub(crate) fn resolve_release_repo(
    release_cfg: &anodizer_core::config::ReleaseConfig,
    token_type: ScmTokenType,
    ctx: &anodizer_core::context::Context,
) -> Result<Option<anodizer_core::config::ScmRepoConfig>> {
    // Explicit `release.provider:` wins over token-type inference. This
    // is the cross-platform publishing seam: a project hosted on GitLab
    // (so `GITLAB_TOKEN` is the active token) can declare
    // `provider: github` to redirect publish output to GitHub.
    use anodizer_core::config::ForceTokenKind;
    let raw = match release_cfg.provider {
        Some(ForceTokenKind::GitHub) => release_cfg.github.as_ref(),
        Some(ForceTokenKind::GitLab) => release_cfg.gitlab.as_ref(),
        Some(ForceTokenKind::Gitea) => release_cfg.gitea.as_ref(),
        None => match token_type {
            ScmTokenType::GitLab => release_cfg.gitlab.as_ref().or(release_cfg.github.as_ref()),
            ScmTokenType::Gitea => release_cfg.gitea.as_ref().or(release_cfg.github.as_ref()),
            ScmTokenType::GitHub => release_cfg.github.as_ref(),
        },
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
/// # Design note
///
/// The `prerelease == "auto"` check could be evaluated once at config-load
/// time: it inspects
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
// `ReleaseConfig::resolved_mode` (lazy-defaults policy).

// ---------------------------------------------------------------------------
// populate_checksums_var
// ---------------------------------------------------------------------------

/// Populate the `{{ .Checksums }}` template variable from the registered
/// `ArtifactKind::Checksum` artifacts.
///
/// # Mode selection
///
/// The release-body description emits two shapes:
///
/// - 0 artifacts → unset / empty string
/// - 1 artifact  → string with the combined file's contents
/// - ≥2 artifacts (split-mode sidecars) → `map[ChecksumOf]contents` so a
///   Tera template can do `{% for k, v in Checksums %}…{% endfor %}`
///
/// Anodizer's workspace model adds a third case:
/// **multiple combined-mode sidecars**, one per crate. The checksum stage
/// marks those with `metadata["combined"] = "true"` (and leaves
/// `ChecksumOf` unset). Without aggregation, the ≥2-artifact branch above
/// would collide every combined file on an empty `ChecksumOf` key, leaking
/// the build host's filesystem layout into release notes and dropping
/// every crate's content except the last. Instead, when every checksum
/// artifact is a combined-mode sidecar, this helper UNIONS all per-crate
/// content lines into a single SHA256SUMS-style block, deduplicated and
/// sorted alphabetically by filename (matching the per-crate sort the
/// checksum stage already applies, and following the convention so a
/// release body templated with `{{ .Checksums }}` renders the full
/// workspace inventory).
///
/// Mixed mode (some combined + some split sidecars) falls back to the
/// a map keyed by `ChecksumOf` for every artifact, with the
/// combined files keyed by their artifact `name` since they have no
/// `ChecksumOf`. Mixed mode is unusual but the map shape stays consistent
/// for templates that already iterate with `{% for k, v in Checksums %}`.
pub(crate) fn populate_checksums_var(ctx: &mut Context) {
    use anodizer_core::artifact::ArtifactKind;

    let checksum_artifacts = ctx.artifacts.by_kind(ArtifactKind::Checksum);
    if checksum_artifacts.is_empty() {
        ctx.template_vars_mut().set("Checksums", "");
        return;
    }

    let is_combined = |a: &&anodizer_core::artifact::Artifact| {
        a.metadata.get("combined").map(|s| s.as_str()) == Some("true")
    };
    let all_combined = checksum_artifacts.iter().all(is_combined);
    let any_split = checksum_artifacts
        .iter()
        .any(|a| a.metadata.contains_key("ChecksumOf"));

    if all_combined && !any_split {
        let mut lines: Vec<String> = Vec::new();
        for artifact in &checksum_artifacts {
            let content = std::fs::read_to_string(&artifact.path).unwrap_or_default();
            for line in content.lines() {
                if !line.is_empty() {
                    lines.push(line.to_string());
                }
            }
        }
        lines.sort_by(|a, b| {
            let name_a = a.split_once("  ").map(|(_, n)| n).unwrap_or(a);
            let name_b = b.split_once("  ").map(|(_, n)| n).unwrap_or(b);
            name_a.cmp(name_b)
        });
        lines.dedup();
        ctx.template_vars_mut().set("Checksums", &lines.join("\n"));
        return;
    }

    let mut map = serde_json::Map::new();
    for artifact in &checksum_artifacts {
        let key = artifact
            .metadata
            .get("ChecksumOf")
            .cloned()
            .unwrap_or_else(|| artifact.name.clone());
        let content = std::fs::read_to_string(&artifact.path).unwrap_or_default();
        map.insert(key, serde_json::Value::String(content));
    }
    ctx.template_vars_mut()
        .set_structured("Checksums", serde_json::Value::Object(map));
}

// ---------------------------------------------------------------------------
// ReleaseStage
// ---------------------------------------------------------------------------

pub struct ReleaseStage;
