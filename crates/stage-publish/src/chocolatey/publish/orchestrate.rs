use std::path::PathBuf;

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::Result;

use super::super::install::FileType;
use super::super::package::push_nupkg;

use super::*;

/// Per-crate metadata required by both the nuspec generator and the
/// install-script renderer. Values are pre-resolved (template-rendered and
/// fallback-applied) so the orchestrator stays linear.
pub(super) struct ChocoMetadata {
    pub(super) description: String,
    /// Resolved SPDX expression. Not emitted as a nuspec element — Chocolatey
    /// CLI flags any NuGet `<license>` element as CHCU0002 ("use <licenseUrl>
    /// instead") — it gates the `<licenseUrl>` derivation (single identifier
    /// → derivable LICENSE blob; compound → no single canonical file).
    pub(super) license: String,
    /// Resolved `<licenseUrl>` — Chocolatey's only supported license
    /// metadata: the explicit `license_url:` config when set, else a derived
    /// GitHub `…/blob/<ref>/LICENSE` URL when the release repo is known,
    /// else `None` (no `<licenseUrl>` is emitted — never a synthesized
    /// 404ing `opensource.org` URL).
    pub(super) license_url: Option<String>,
    pub(super) authors: String,
    pub(super) project_url: String,
    pub(super) icon_url: String,
    pub(super) tags: Vec<String>,
    /// Resolved `<projectSourceUrl>`: explicit config, else the derived repo
    /// URL (real packages always set it; moderators expect it).
    pub(super) project_source_url: Option<String>,
    /// Resolved `<bugTrackerUrl>`: explicit config, else `{repo}/issues`.
    pub(super) bug_tracker_url: Option<String>,
}

/// Optional, template-rendered text fields that flow into `<title>`,
/// `<copyright>`, `<summary>`, and `<releaseNotes>` of the generated nuspec.
pub(super) struct ChocoTextFields {
    pub(super) title: Option<String>,
    pub(super) copyright: Option<String>,
    pub(super) summary: Option<String>,
    pub(super) release_notes: Option<String>,
    pub(super) name: Option<String>,
    pub(super) package_source_url: Option<String>,
    pub(super) docs_url: Option<String>,
    pub(super) owners: Option<String>,
}

/// Install-script shape. `Dual` carries 32- and 64-bit URL/hash pairs;
/// `Single` carries one URL/hash plus the bitness selector used by the
/// per-architecture template branch.
pub(super) enum InstallMode {
    Dual {
        url32: String,
        hash32: String,
        url64: String,
        hash64: String,
    },
    Single {
        url: String,
        hash: String,
        is_32bit: bool,
    },
}

/// Paths produced by staging the package on disk: the rendered `.nuspec`
/// and the packed `.nupkg` ready for push.
pub(super) struct StagedPackage {
    pub(super) _tmp_dir: tempfile::TempDir,
    pub(super) nupkg_path: PathBuf,
}

/// Returns `Ok(true)` when an actual `push_nupkg` happened against the
/// feed, `Ok(false)` for every skip path (skip=true template, dry-run,
/// missing API key, hash-match already-published, pending-moderation
/// without `republish_in_moderation`). The caller MUST use the bool to
/// gate rollback-evidence recording — recording a target the run never
/// pushed produces a misleading "manual withdrawal required" warning at
/// rollback time.
pub fn publish_to_chocolatey(
    ctx: &mut Context,
    crate_name: &str,
    log: &StageLogger,
) -> Result<bool> {
    let choco_cfg = {
        let (_crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "chocolatey")?;
        publish
            .chocolatey
            .as_ref()
            .ok_or_else(|| {
                anyhow::anyhow!("chocolatey: no chocolatey config for '{}'", crate_name)
            })?
            .clone()
    };
    let choco_cfg = &choco_cfg;

    // Chocolatey is a feed-push publisher: only `api_key` + `source_repo`
    // are required to push. The optional `repository.owner/name` is *only*
    // used as a fallback source for `<projectUrl>` (the gallery link) when
    // `project_url:` is unset. The lookup is optional and falls back to an
    // empty string when both project_url and repository are unset, so
    // internal feeds without a public GitHub release are not blocked.
    //
    let (repo_owner, repo_name) = match choco_cfg.repository.as_ref() {
        Some(r) => (
            r.owner.as_deref().unwrap_or(""),
            r.name.as_deref().unwrap_or(""),
        ),
        None => ("", ""),
    };

    if check_skip_publish(ctx, choco_cfg, crate_name, log)? {
        return Ok(false);
    }

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would push Chocolatey package for '{}'{}",
            crate_name,
            if repo_owner.is_empty() {
                String::new()
            } else {
                format!(" to {}/{}", repo_owner, repo_name)
            }
        ));
        return Ok(false);
    }

    let version = ctx.version();
    let pkg_name = choco_cfg.name.as_deref().unwrap_or(crate_name);

    // The skip gate above already ran (`check_skip_publish`), so render the
    // nuspec via the skip-unaware inner helper — re-evaluating the skip/`if`
    // gate here would double every resolved-with-warning value's log line.
    let nuspec = render_nuspec_inner(ctx, choco_cfg, crate_name, repo_owner, repo_name, log)?;

    let (artifact_32, artifact_64) = select_windows_artifacts(ctx, choco_cfg, crate_name, log);
    let install_mode = build_install_mode(
        ctx,
        choco_cfg,
        pkg_name,
        &version,
        artifact_32,
        artifact_64,
        crate_name,
    )?;

    let file_type = FileType::from_use(choco_cfg.use_artifact.as_deref());
    let install_script = build_install_script(pkg_name, &install_mode, file_type)?;

    let staged = stage_package(pkg_name, &version, &nuspec, &install_script, log)?;

    let api_key = resolve_api_key(ctx, choco_cfg, log)?;
    if api_key.is_empty() {
        log.warn(&format!(
            "skipped push for '{}' — no chocolatey API key",
            crate_name
        ));
        return Ok(false);
    }

    let source = super::super::push_source(choco_cfg);

    // Idempotency with drift detection: Chocolatey package versions are
    // immutable once submitted, so re-pushing returns 403. A
    // version-already-on-feed is treated as a skip ONLY when the feed's
    // recorded package hash matches the local nupkg hash. If they differ,
    // the local nupkg has diverged from what the feed has — typically
    // because the same git tag was re-released with different artifact
    // bytes — and silently skipping would publish an install script that
    // points at an archive whose sha no longer matches (Chocolatey's
    // verifier then rejects the package). Divergence fails loudly with a
    // message instructing the caller to bump the version.
    // Single retry policy resolved from the top-level `retry:` block; reused
    // for the feed-hash GET and the push PUT.
    let policy = ctx.retry_policy();

    if let Some(early_exit) = handle_feed_state(
        ctx,
        choco_cfg,
        source,
        pkg_name,
        &version,
        &staged.nupkg_path,
        &policy,
        log,
    )? {
        return Ok(early_exit);
    }

    // Push via NuGet V2 API — same protocol as `choco push`.
    push_nupkg(
        &staged.nupkg_path,
        source,
        &api_key,
        log,
        &policy,
        ctx.retry_deadline(),
    )?;

    log.status(&format!("Chocolatey package pushed for '{}'", crate_name));
    Ok(true)
}

/// Evaluates `skip:` (literal bool or template) and returns `Ok(true)`
/// when the publisher should be bypassed for this crate.
pub(super) fn check_skip_publish(
    ctx: &mut Context,
    choco_cfg: &anodizer_core::config::ChocolateyConfig,
    crate_name: &str,
    log: &StageLogger,
) -> Result<bool> {
    let label = format!("chocolatey publisher for crate '{}'", crate_name);
    crate::util::should_skip_publisher_with_if(
        ctx,
        choco_cfg.skip.as_ref(),
        None,
        choco_cfg.if_condition.as_deref(),
        &label,
        log,
    )
}

/// Render the `.nuspec` XML a real Chocolatey publish would stage for
/// `crate_name`, in-memory and with no disk/network side effects.
///
/// Returns `Ok(None)` when the publisher would skip this crate (`skip:` true
/// or a falsy `if` condition). Errors when the crate carries no `chocolatey`
/// block, or when `license` is unresolvable (an empty `<licenseUrl>` is what
/// Chocolatey gallery moderators reject). The live publish path and the
/// offline schema validator both produce the nuspec through the same inner
/// render so the validated document is byte-for-byte what a release pushes.
///
/// Unlike the install script, the nuspec does not depend on any Windows
/// archive artifact — it always renders regardless of which platforms built.
pub(crate) fn render_nuspec_for_crate(
    ctx: &Context,
    crate_name: &str,
    log: &StageLogger,
) -> Result<Option<String>> {
    let (_crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "chocolatey")?;
    let choco_cfg = publish
        .chocolatey
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("chocolatey: no chocolatey config for '{}'", crate_name))?;

    let label = format!("chocolatey publisher for crate '{}'", crate_name);
    if crate::util::should_skip_publisher_with_if(
        ctx,
        choco_cfg.skip.as_ref(),
        None,
        choco_cfg.if_condition.as_deref(),
        &label,
        log,
    )? {
        return Ok(None);
    }

    let (repo_owner, repo_name) = match choco_cfg.repository.as_ref() {
        Some(r) => (
            r.owner.as_deref().unwrap_or(""),
            r.name.as_deref().unwrap_or(""),
        ),
        None => ("", ""),
    };
    let nuspec = render_nuspec_inner(ctx, choco_cfg, crate_name, repo_owner, repo_name, log)?;
    Ok(Some(nuspec))
}

/// Reproduce the live chocolatey publish's artifact-dependent bail in the
/// offline schema validator, honoring determinism-shard tolerance.
///
/// The `.nuspec` render ([`render_nuspec_for_crate`]) is metadata-only and never
/// touches a Windows artifact, so schema-checking it alone would let a full
/// build pass while a real publish aborts in [`build_install_mode`] with
/// `chocolatey: no windows artifact found` (or an empty-sha256 bail). This runs
/// the SAME `select_windows_artifacts` + `build_install_mode` the live publish
/// runs and propagates only their `Err`, so that defect surfaces in
/// `check`/`--snapshot`.
///
/// Returns `Ok(false)` when `partial_shard` is set and the crate built no
/// Windows artifact — on a target-restricted shard the artifact is legitimately
/// absent, so there is nothing to check. On a FULL build (`partial_shard` false)
/// an absent artifact is a genuine misconfiguration and reaches the bail.
/// Returns `Ok(true)` when the check ran (clean). The selection is the SAME
/// filter the live publish applies, so the validated set never diverges.
pub(crate) fn validate_install_mode_for_crate(
    ctx: &Context,
    crate_name: &str,
    partial_shard: bool,
    log: &StageLogger,
) -> Result<bool> {
    let (_crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "chocolatey")?;
    let Some(choco_cfg) = publish.chocolatey.as_ref() else {
        return Ok(true);
    };
    let version = ctx.version();
    let pkg_name = choco_cfg.name.as_deref().unwrap_or(crate_name);
    let (artifact_32, artifact_64) = select_windows_artifacts(ctx, choco_cfg, crate_name, log);
    if partial_shard && artifact_32.is_none() && artifact_64.is_none() {
        return Ok(false);
    }
    build_install_mode(
        ctx,
        choco_cfg,
        pkg_name,
        &version,
        artifact_32,
        artifact_64,
        crate_name,
    )?;
    Ok(true)
}

/// Skip-unaware nuspec render: resolve metadata + text fields and build the
/// `.nuspec` XML body. Every resolved-with-warning value is resolved exactly
/// once here, so both the live publish path (which has already evaluated the
/// skip gate) and [`render_nuspec_for_crate`] (which evaluates it itself)
/// share one resolution without double-logging.
fn render_nuspec_inner(
    ctx: &Context,
    choco_cfg: &anodizer_core::config::ChocolateyConfig,
    crate_name: &str,
    repo_owner: &str,
    repo_name: &str,
    log: &StageLogger,
) -> Result<String> {
    let version = ctx.version();
    let metadata = resolve_metadata(ctx, choco_cfg, crate_name, repo_owner, repo_name, log)?;
    let text_fields = render_text_fields(ctx, choco_cfg, crate_name, log)?;
    let nuspec = build_nuspec(choco_cfg, crate_name, &version, &metadata, &text_fields)?;
    crate::util::guard_no_unrendered(ctx, log, "chocolatey nuspec", &nuspec)?;
    Ok(nuspec)
}
