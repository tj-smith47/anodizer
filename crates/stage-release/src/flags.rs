use anodizer_core::context::Context;
use anyhow::{Context as _, Result, bail};

use crate::release_body::resolve_make_latest;
use crate::should_mark_prerelease;

/// Emit once-per-run warnings about workspace-level nightly configuration
/// combinations that are technically valid but operationally surprising.
///
/// Surfaces the gotcha that `nightly.draft = true`
/// combined with `nightly.keep_single_release = true` leaves no published
/// nightly release in a non-draft state, because each run replaces the prior
/// draft before it can be promoted.
pub(crate) fn validate_nightly_config(ctx: &Context, log: &anodizer_core::log::StageLogger) {
    if !ctx.is_nightly() {
        return;
    }
    let Some(nightly_cfg) = ctx.config.nightly.as_ref() else {
        return;
    };
    // keep_single_release (or retention.keep_last:1) + draft leaves no
    // promoted nightly: each run replaces the prior draft before it publishes.
    if nightly_cfg.draft == Some(true) && nightly_cfg.resolved_keep_last() == Some(1) {
        log.warn(
            "nightly with both draft=true and a keep_last:1 retention \
             (keep_single_release) — no published nightly release will exist \
             (each run replaces a prior draft)",
        );
    }
}

/// Validate release flag combinations that are mutually exclusive and would
/// produce conflicting behavior if both are set.
///
/// Returns `Err` when the combination is invalid; `Ok(())` otherwise.
pub(crate) fn validate_release_flags(
    release_cfg: &anodizer_core::config::ReleaseConfig,
    crate_name: &str,
) -> Result<()> {
    if release_cfg.resolved_replace_existing_draft() && release_cfg.resolved_use_existing_draft() {
        bail!(
            "release: crate '{}': cannot set both replace_existing_draft and \
             use_existing_draft — replace deletes drafts that use_existing_draft needs",
            crate_name
        );
    }
    Ok(())
}

/// Resolve the `skip_upload` decision for one crate's release.
///
/// Accepts a template (`{{ .IsSnapshot }}`, etc.) that renders to one of
/// `true` / `false` / `auto` / `1` / `0` / "". `auto` resolves as:
/// skip when the run is a snapshot. Any other rendered value bails with
/// the actionable error message.
fn resolve_skip_upload(
    ctx: &Context,
    release_cfg: &anodizer_core::config::ReleaseConfig,
    crate_name: &str,
) -> Result<bool> {
    let Some(s) = release_cfg.skip_upload.as_ref() else {
        return Ok(false);
    };
    let rendered = if s.is_template() {
        ctx.render_template(s.as_str()).with_context(|| {
            format!(
                "release: render skip_upload template '{}' for crate '{}'",
                s.as_str(),
                crate_name
            )
        })?
    } else {
        s.as_str().to_string()
    };
    Ok(match rendered.trim() {
        "auto" => ctx.is_snapshot(),
        "true" | "1" => true,
        "false" | "0" | "" => false,
        other => bail!(
            "release: invalid skip_upload value '{}' for crate '{}' \
             (expected one of: true/false/auto/1/0, or a template that renders to one of those)",
            other,
            crate_name
        ),
    })
}

/// Resolved boolean/enum flags for one crate's release, computed once and
/// threaded through the dry-run path and the live SCM backend dispatch.
pub(crate) struct ResolvedReleaseFlags {
    pub(crate) draft: bool,
    pub(crate) prerelease: bool,
    pub(crate) skip_upload: bool,
    pub(crate) replace_existing_draft: bool,
    pub(crate) replace_existing_artifacts: bool,
    pub(crate) make_latest: Option<octocrab::repos::releases::MakeLatest>,
    pub(crate) target_commitish: Option<String>,
    pub(crate) discussion_category_name: Option<String>,
    pub(crate) include_meta: bool,
    pub(crate) use_existing_draft: bool,
    /// Nightly retention: keep the N newest nightly releases and delete the
    /// rest (+ their tags) AFTER the new release is created and published.
    /// `Some(1)` is the rolling-single-release case (the `keep_single_release`
    /// alias). Resolved from `NightlyConfig::resolved_keep_last` (which folds in
    /// the legacy alias and its precedence). Only honored on `--nightly` runs,
    /// and only acted on by the GitHub backend.
    pub(crate) retention_keep_last: Option<usize>,
    /// Nightly `publish_repo`: `(owner, repo)` to redirect the release to a
    /// repo other than the source. Only honored on `--nightly` runs.
    pub(crate) publish_repo_override: Option<(String, String)>,
}

/// Resolve all release flags from config + CLI overrides for one crate.
pub(crate) fn resolve_release_flags(
    ctx: &Context,
    release_cfg: &anodizer_core::config::ReleaseConfig,
    crate_name: &str,
    tag: &str,
) -> Result<ResolvedReleaseFlags> {
    let skip_upload = resolve_skip_upload(ctx, release_cfg, crate_name)?;
    let target_commitish = release_cfg
        .target_commitish
        .as_ref()
        .map(|tc| ctx.render_template(tc))
        .transpose()
        .with_context(|| {
            format!(
                "release: render target_commitish for crate '{}'",
                crate_name
            )
        })?;
    // Nightly overrides: `nightly.draft` (Some(v) wins over `release.draft`)
    // — only meaningful when `is_nightly()`.
    let nightly_cfg = ctx.config.nightly.as_ref();
    let draft = if ctx.is_nightly()
        && let Some(d) = nightly_cfg.and_then(|n| n.draft)
    {
        d
    } else {
        release_cfg.resolved_draft()
    };
    // Retention (keep_last:N) and publish_repo are nightly-only. The
    // resolved_keep_last() helper applies the back-compat precedence
    // (retention block wins over the keep_single_release alias, which maps
    // to keep_last:1) — the single source of truth for the backend sweep.
    let retention_keep_last = if ctx.is_nightly() {
        nightly_cfg.and_then(|n| n.resolved_keep_last())
    } else {
        None
    };
    let publish_repo_override = if ctx.is_nightly() {
        nightly_cfg
            .and_then(|n| n.publish_repo.as_deref())
            .and_then(|s| s.split_once('/'))
            .map(|(o, r)| (o.to_string(), r.to_string()))
    } else {
        None
    };
    Ok(ResolvedReleaseFlags {
        draft,
        prerelease: should_mark_prerelease(&release_cfg.prerelease, tag),
        skip_upload,
        replace_existing_draft: release_cfg.resolved_replace_existing_draft(),
        replace_existing_artifacts: release_cfg.resolved_replace_existing_artifacts()
            || ctx.options.replace_existing_artifacts,
        make_latest: resolve_make_latest(&release_cfg.make_latest, |s| ctx.render_template(s))?,
        target_commitish,
        discussion_category_name: release_cfg.discussion_category_name.clone(),
        include_meta: release_cfg.resolved_include_meta(),
        use_existing_draft: release_cfg.resolved_use_existing_draft(),
        retention_keep_last,
        publish_repo_override,
    })
}

/// Warn when nightly retention / `publish_repo` is configured for an SCM
/// backend that does not act on it.
///
/// `nightly.retention` / `nightly.keep_single_release` and
/// `nightly.publish_repo` are only wired into the GitHub backend's release
/// sweep. On GitLab / Gitea they would silently no-op, so surface a clear
/// reporter warning (not `eprintln!`) rather than let the user assume the old
/// releases are being pruned.
pub(crate) fn warn_unsupported_nightly_retention(
    log: &anodizer_core::log::StageLogger,
    backend_label: &str,
    flags: &ResolvedReleaseFlags,
) {
    if flags.retention_keep_last.is_some() {
        log.warn(&format!(
            "nightly retention (keep_last / keep_single_release) is only \
             applied on GitHub releases; it has no effect on {backend_label} \
             and prior nightly releases will NOT be pruned"
        ));
    }
    if let Some((owner, repo)) = &flags.publish_repo_override {
        log.warn(&format!(
            "nightly.publish_repo '{owner}/{repo}' is only honored on GitHub \
             releases; it has no effect on {backend_label} (the release targets \
             the configured {backend_label} repo)"
        ));
    }
}
