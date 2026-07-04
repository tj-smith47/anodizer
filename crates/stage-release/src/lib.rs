use anodizer_core::config::PrereleaseConfig;
use anodizer_core::context::Context;
use anodizer_core::git;
use anodizer_core::log::{StageLogger, Verbosity};
use anodizer_core::scm::ScmTokenType;
use anyhow::{Context as _, Result};

/// Module-level logger for warnings emitted from helpers (and async upload
/// retry loops) that don't have runtime access to the stage's
/// `ctx.logger("release")`. Carries the `release` stage context so these
/// lines render under the release section header (the per-line tag is gone —
/// format B), keeping them consistent with the rest of the stage's output.
pub(crate) fn release_log() -> StageLogger {
    StageLogger::new("release", Verbosity::Normal)
}

mod forge;
mod gitea;
mod github;
pub use github::fetch_published_asset_names;
mod gitlab;
pub mod publisher;
mod release_body;
mod run;
pub use run::collect_release_upload_candidates;

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// classify_asset_conflict — shared release-asset overwrite decision
// ---------------------------------------------------------------------------

/// The decision for a release asset whose name already exists (or may exist)
/// on the remote, derived from a byte-size probe plus the user's
/// `replace_existing_artifacts` setting.
///
/// This is the single source of truth for the immutable-releases invariant
/// shared by every SCM backend: a **byte-identical** remote asset is a no-op,
/// not an overwrite, so it is skipped REGARDLESS of `replace_existing_artifacts`
/// — the user's flag guards against replacing *different* bytes, never against
/// re-uploading the same bytes. The shared upload loop
/// ([`forge::run_upload_loop`]) acts on these variants directly for the
/// proactive-probe forges (Gitea, GitLab); GitHub applies the same rule
/// reactively via its post-422 `AlreadyExistsAction` projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AssetConflict {
    /// Remote asset is present and byte-identical to the local file: skip the
    /// upload (idempotent no-op), independent of the replace flag.
    IdenticalSkip,
    /// Remote asset is present, differs from the local file, and the user
    /// opted into overwrites (`replace_existing_artifacts: true`): delete the
    /// stale asset, then upload.
    ReplaceDiffering,
    /// Remote asset is present, differs from the local file, and overwrites are
    /// forbidden (`replace_existing_artifacts: false`): surface the conflict
    /// instead of mutating published bytes.
    ConflictForbidden,
    /// No conflicting remote asset to reconcile: upload as-is.
    NoConflict,
}

/// Classify a release-asset upload against any same-named remote asset.
///
/// `remote_present` is whether a remote asset with the target name exists at
/// all; `remote_size` is its byte size when known (`None` = present but size
/// unreadable). `local_size` is the local file's byte count.
///
/// Pure (no I/O) so the overwrite decision is unit-testable without a live
/// API client. The same-size idempotent skip fires regardless of
/// `replace_existing_artifacts`; a differing remote routes to overwrite when
/// the flag is set and to a forbidden-conflict otherwise. An unknown remote
/// size on a present asset is treated as a mismatch (better to bail/replace
/// than silently keep possibly-wrong bytes).
pub(crate) fn classify_asset_conflict(
    replace_existing_artifacts: bool,
    remote_present: bool,
    remote_size: Option<u64>,
    local_size: u64,
) -> AssetConflict {
    if !remote_present {
        return AssetConflict::NoConflict;
    }
    if remote_size == Some(local_size) {
        return AssetConflict::IdenticalSkip;
    }
    if replace_existing_artifacts {
        AssetConflict::ReplaceDiffering
    } else {
        AssetConflict::ConflictForbidden
    }
}

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
/// artifact name. This lets publishers resolve download URLs without an
/// explicit `url_template`.
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
///
/// GitLab omits the `/{owner}` segment when `owner` is empty (a top-level
/// project with no namespace), matching the authoritative
/// [`gitlab::gitlab_release_url`] path. Without this, an empty owner would
/// emit a double-slash `{base}//{repo}/-/releases/{tag}` that diverges from
/// the URL the live create returns. GitHub / Gitea always include the owner
/// segment, mirroring their authoritative composers.
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
            if owner.is_empty() {
                format!("{}/{}/-/releases/{}", base, repo, tag)
            } else {
                format!("{}/{}/{}/-/releases/{}", base, owner, repo, tag)
            }
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
/// A prerelease decision could be made once at config-load time by inspecting
/// the parsed semver's prerelease segment and storing a single flag for the
/// whole release run, so every release in the run shares that one decision.
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
/// Mixed mode (some combined + some split sidecars) falls back to a map keyed
/// by `ChecksumOf` for every artifact, with the
/// combined files keyed by their artifact `name` since they have no
/// `ChecksumOf`. Mixed mode is unusual but the map shape stays consistent
/// for templates that already iterate with `{% for k, v in Checksums %}`.
/// Read a checksum artifact's contents for the `{{ .Checksums }}` release-body
/// variable. Returns `Ok(None)` ONLY in `--dry-run` when the file is absent:
/// the checksum stage registers the artifact but skips writing it in dry-run
/// (the `(dry-run) combined checksums → …` line), so a missing file there is
/// expected and simply contributes nothing to the preview. In a real run the
/// file must exist and be readable — a `NotFound` (the stage didn't produce
/// what it registered) OR any other read error is a defect that must fail the
/// release, never silently blank the checksums body.
fn read_checksum_artifact(dry_run: bool, path: &std::path::Path) -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(e) if dry_run && e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::new(e)).with_context(|| {
            format!(
                "release: reading checksum artifact {} for the {{{{ .Checksums }}}} release-body variable",
                path.display()
            )
        }),
    }
}

pub(crate) fn populate_checksums_var(ctx: &mut Context) -> anyhow::Result<()> {
    use anodizer_core::artifact::ArtifactKind;

    let dry_run = ctx.is_dry_run();
    let checksum_artifacts = ctx.artifacts.by_kind(ArtifactKind::Checksum);
    if checksum_artifacts.is_empty() {
        ctx.template_vars_mut().set("Checksums", "");
        return Ok(());
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
            let Some(content) = read_checksum_artifact(dry_run, &artifact.path)? else {
                continue;
            };
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
        return Ok(());
    }

    let mut map = serde_json::Map::new();
    for artifact in &checksum_artifacts {
        let key = artifact
            .metadata
            .get("ChecksumOf")
            .cloned()
            .unwrap_or_else(|| artifact.name.clone());
        let Some(content) = read_checksum_artifact(dry_run, &artifact.path)? else {
            continue;
        };
        map.insert(key, serde_json::Value::String(content));
    }
    ctx.template_vars_mut()
        .set_structured("Checksums", serde_json::Value::Object(map));
    Ok(())
}

// ---------------------------------------------------------------------------
// ReleaseStage
// ---------------------------------------------------------------------------

pub struct ReleaseStage;

#[cfg(test)]
mod asset_conflict_tests {
    //! The shared overwrite classifier consumed by both the GitHub and Gitea
    //! backends. The byte-identical-skip invariant lives here once; the
    //! per-backend projection tests (`spec.rs` / `gitea.rs`) pin the mapping
    //! onto their own action enums.
    use super::{AssetConflict, classify_asset_conflict};

    #[test]
    fn absent_remote_is_no_conflict_regardless_of_flag() {
        assert_eq!(
            classify_asset_conflict(false, false, None, 100),
            AssetConflict::NoConflict
        );
        assert_eq!(
            classify_asset_conflict(true, false, None, 100),
            AssetConflict::NoConflict
        );
    }

    #[test]
    fn identical_bytes_skip_regardless_of_flag() {
        // The cardinal invariant: same size = idempotent no-op even when
        // `replace_existing_artifacts: false`.
        assert_eq!(
            classify_asset_conflict(false, true, Some(100), 100),
            AssetConflict::IdenticalSkip
        );
        assert_eq!(
            classify_asset_conflict(true, true, Some(100), 100),
            AssetConflict::IdenticalSkip
        );
    }

    #[test]
    fn differing_bytes_with_replace_allowed_overwrites() {
        assert_eq!(
            classify_asset_conflict(true, true, Some(100), 200),
            AssetConflict::ReplaceDiffering
        );
        // Unknown remote size on a present asset is treated as a mismatch.
        assert_eq!(
            classify_asset_conflict(true, true, None, 200),
            AssetConflict::ReplaceDiffering
        );
    }

    #[test]
    fn differing_bytes_with_replace_forbidden_is_conflict() {
        assert_eq!(
            classify_asset_conflict(false, true, Some(100), 200),
            AssetConflict::ConflictForbidden
        );
        // Present-but-unreadable size + no opt-in: bail rather than mutate.
        assert_eq!(
            classify_asset_conflict(false, true, None, 200),
            AssetConflict::ConflictForbidden
        );
    }
}
