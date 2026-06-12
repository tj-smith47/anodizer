use anodizer_core::artifact::{ArtifactKind, matches_id_filter};
use anodizer_core::context::Context;
use anodizer_core::scm::ScmTokenType;
use anodizer_core::stage::Stage;
use anyhow::{Context as _, Result, bail};

use crate::release_body::{
    build_release_body, collect_extra_files, render_nondeterministic_exemptions_block,
    resolve_content_source, resolve_header_footer, resolve_make_latest, resolve_release_tag,
};
use crate::{
    compose_release_url, gitea, github, gitlab, populate_artifact_download_urls,
    resolve_release_repo, should_mark_prerelease,
};

impl Stage for super::ReleaseStage {
    fn name(&self) -> &str {
        "release"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("release");

        // The SCM token is already resolved into ctx.options.token by the CLI
        // pipeline init (resolve_scm_token_type). Trust it directly.
        let token = ctx.options.token.clone();

        let selected = ctx.options.selected_crates.clone();
        // `--snapshot` means "build without publishing", so it must take the same
        // no-live-API path as `--dry-run`: emit the "would create …" telemetry and
        // return before `dispatch_to_scm_backend`. Without this, snapshot fell
        // through to the live SCM backend, which bails on a missing token (and
        // would create a real release if one were present). Mirrors the
        // `!is_dry_run() && !is_snapshot()` guard the GitHub backend already uses
        // for release-ID capture.
        let dry_run = ctx.is_dry_run() || ctx.is_snapshot();

        // Collect crates that have a `release` block.
        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| c.release.is_some())
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .cloned()
            .collect();

        // Create the tokio runtime once, outside the loop.
        let rt =
            tokio::runtime::Runtime::new().context("release: failed to create tokio runtime")?;

        validate_nightly_config(ctx, &log);

        for crate_cfg in &crates {
            let Some(release_cfg) = crate_cfg.release.as_ref() else {
                continue;
            };
            if should_skip_release(ctx, release_cfg, &crate_cfg.name, &log)? {
                continue;
            }
            validate_release_flags(release_cfg, &crate_cfg.name)?;
            release_one_crate(ctx, &log, &rt, &token, crate_cfg, release_cfg, dry_run)?;
        }

        Ok(())
    }
}

/// Emit once-per-run warnings about workspace-level nightly configuration
/// combinations that are technically valid but operationally surprising.
///
/// Surfaces the gotcha that `nightly.draft = true`
/// combined with `nightly.keep_single_release = true` leaves no published
/// nightly release in a non-draft state, because each run replaces the prior
/// draft before it can be promoted.
fn validate_nightly_config(ctx: &Context, log: &anodizer_core::log::StageLogger) {
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
fn validate_release_flags(
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

/// Check whether a crate's release should be skipped: evaluates the `skip`
/// template and honours `nightly.publish_release: false` on nightly runs.
fn should_skip_release(
    ctx: &Context,
    release_cfg: &anodizer_core::config::ReleaseConfig,
    crate_name: &str,
    log: &anodizer_core::log::StageLogger,
) -> Result<bool> {
    if let Some(ref d) = release_cfg.skip {
        let off = d
            .try_evaluates_to_true(|s| ctx.render_template(s))
            .with_context(|| format!("release: render skip template for crate '{}'", crate_name))?;
        if off {
            log.status(&format!("release skipped for crate '{}'", crate_name));
            return Ok(true);
        }
    }
    if ctx.is_nightly()
        && ctx.config.nightly.as_ref().and_then(|n| n.publish_release) == Some(false)
    {
        log.status(&format!(
            "release skipped for crate '{}' (nightly.publish_release: false)",
            crate_name
        ));
        return Ok(true);
    }
    Ok(false)
}

/// Execute the full release pipeline for a single crate: resolve tag, build
/// release body, collect artifacts, and either emit dry-run telemetry or
/// dispatch to the live SCM backend.
fn release_one_crate(
    ctx: &mut Context,
    log: &anodizer_core::log::StageLogger,
    rt: &tokio::runtime::Runtime,
    token: &Option<String>,
    crate_cfg: &anodizer_core::config::CrateConfig,
    release_cfg: &anodizer_core::config::ReleaseConfig,
    dry_run: bool,
) -> Result<()> {
    let crate_name = crate_cfg.name.clone();

    let changelog_body = ctx
        .stage_outputs
        .changelogs
        .get(&crate_name)
        .cloned()
        .unwrap_or_default();

    crate::populate_checksums_var(ctx);

    let release_mode = release_cfg
        .resolved_mode()
        .map(|m| m.to_string())
        .with_context(|| format!("release: invalid mode for crate '{}'", crate_name))?;
    if release_mode != anodizer_core::config::ReleaseConfig::DEFAULT_MODE {
        log.status(&format!(
            "release mode '{}' for crate '{}'",
            release_mode, crate_name
        ));
    }

    ctx.refresh_artifacts_var();

    let release_body = compose_full_release_body(ctx, release_cfg, &crate_name, &changelog_body)?;

    let tag = resolve_release_tag(
        ctx,
        &crate_cfg.tag_template,
        release_cfg.tag.as_deref(),
        &crate_cfg.name,
    )?;

    warn_tag_override_divergence(ctx, release_cfg, &tag, &crate_cfg.name, log);

    // Derive a default `ReleaseURL` from the SCM repo + tag BEFORE the
    // dry-run / backend branches. Without it, any path that never reaches
    // the authoritative `html_url` (dry-run, snapshot, `--publish-only`
    // consuming an already-published release, a backend that returns
    // `None`) leaves `ReleaseURL` unset, and the announce / webhook / email
    // stages then fail to render `{{ ReleaseURL }}` (`Variable 'ReleaseURL'
    // not found in context`). The authoritative URL from the create path
    // still overwrites this default at the end of `release_one_crate`.
    ensure_release_url(ctx, release_cfg, &tag, &crate_cfg.name)?;

    let release_name = resolve_release_name(ctx, release_cfg, &crate_cfg.name)?;

    let flags = resolve_release_flags(ctx, release_cfg, &crate_name, &tag)?;
    let ids_filter = release_cfg.ids.as_ref();

    let artifact_entries = assemble_artifact_entries(
        ctx,
        log,
        crate_cfg,
        release_cfg,
        ids_filter,
        flags.include_meta,
        dry_run,
    )?;

    if dry_run {
        handle_dry_run(
            ctx,
            log,
            release_cfg,
            DryRunSummary {
                crate_name: &crate_name,
                release_name: &release_name,
                tag: &tag,
                draft: flags.draft,
                prerelease: flags.prerelease,
                release_mode: &release_mode,
                skip_upload: flags.skip_upload,
                retention_keep_last: flags.retention_keep_last,
                publish_repo_override: flags.publish_repo_override.clone(),
                artifact_entries: &artifact_entries,
            },
        )?;
        return Ok(());
    }

    let backend_result = dispatch_to_scm_backend(
        ctx,
        log,
        rt,
        token,
        crate_cfg,
        release_cfg,
        &tag,
        &release_name,
        &release_body,
        &release_mode,
        &flags,
        &artifact_entries,
    )?;

    if let Some((release_url, download_base, repo_owner, repo_name)) = backend_result {
        if !flags.skip_upload {
            populate_artifact_download_urls(
                ctx,
                &crate_name,
                ctx.token_type,
                &download_base,
                &repo_owner,
                &repo_name,
                &tag,
            );
        }
        ctx.set_release_url(&release_url);
    }

    Ok(())
}

/// Warn when `release.tag` resolves to a value different from the pushed
/// git tag.
fn warn_tag_override_divergence(
    ctx: &Context,
    release_cfg: &anodizer_core::config::ReleaseConfig,
    tag: &str,
    crate_name: &str,
    log: &anodizer_core::log::StageLogger,
) {
    if release_cfg.tag.is_some()
        && let Some(pushed_tag) = ctx.template_vars().get("Tag")
        && !pushed_tag.is_empty()
        && pushed_tag != tag
    {
        log.warn(&format!(
            "release.tag override '{}' differs from pushed git tag '{}' (crate '{}') — GitHub will create a new tag at the target commit",
            tag, pushed_tag, crate_name
        ));
    }
}

/// Set a default `ReleaseURL` template var derived from the active SCM
/// repo + tag when one is not already present.
///
/// `ReleaseURL` is fully derivable from `(provider, download_base, owner,
/// repo, tag)` — the same inputs [`compose_release_url`] uses for the live
/// create path. Deriving it up front guarantees announce / webhook / email
/// templates can always render `{{ ReleaseURL }}`, even on paths that never
/// hit the create backend (dry-run, snapshot, `--publish-only` against an
/// already-published release, or a backend returning `None`).
///
/// No-op when:
/// - `ReleaseURL` is already set to a non-empty value (the authoritative
///   `html_url` from a prior crate's create, or a re-entry), or
/// - the crate has no resolvable `<provider>` repo block (nothing to derive
///   an owner/repo from) — the live path would also produce no URL here, so
///   leaving it unset matches existing behavior rather than inventing a URL
///   against an unconfigured repo.
fn ensure_release_url(
    ctx: &mut Context,
    release_cfg: &anodizer_core::config::ReleaseConfig,
    tag: &str,
    crate_name: &str,
) -> Result<()> {
    if ctx
        .template_vars()
        .get("ReleaseURL")
        .is_some_and(|u| !u.is_empty())
    {
        return Ok(());
    }
    let Some(repo) = resolve_release_repo(release_cfg, ctx.token_type, ctx)? else {
        return Ok(());
    };
    if repo.owner.is_empty() && repo.name.is_empty() {
        return Ok(());
    }
    let download_base = dry_run_download_base(ctx);
    let url = compose_release_url(ctx.token_type, &download_base, &repo.owner, &repo.name, tag);
    ctx.set_release_url(&url);
    ctx.logger("release").verbose(&format!(
        "derived default ReleaseURL '{url}' for crate '{crate_name}'"
    ));
    Ok(())
}

/// Render the release name from the configured name_template.
fn resolve_release_name(
    ctx: &Context,
    release_cfg: &anodizer_core::config::ReleaseConfig,
    crate_name: &str,
) -> Result<String> {
    let name_tmpl = release_cfg.resolved_name_template();
    ctx.render_template(name_tmpl)
        .with_context(|| format!("release: render name_template for crate '{}'", crate_name))
}

/// Collect the full set of `(path, Option<custom_name>)` entries to upload as
/// release assets for one crate.
///
/// Composition order:
///
/// 1. Uploadable artifacts produced by upstream stages, filtered by
///    `ids_filter` and the crate name (`release_uploadable_kinds()` +
///    optional `Metadata` when `include_meta`).
/// 2. `refresh_combined_checksums` updates combined sidecars in-place so
///    they include signatures/artifacts added after the checksum stage ran.
/// 3. `release.extra_files` glob patterns expand and append (with their
///    optional `name_template` honored).
/// 4. `release.templated_extra_files` are rendered into the dist dir and
///    appended with their `dst` name as the custom_name.
/// 5. `include_meta: true` appends `metadata.json` (only the Metadata
///    kind, not anodizer's
///    private `artifacts.json` manifest).
fn assemble_artifact_entries(
    ctx: &mut Context,
    log: &anodizer_core::log::StageLogger,
    crate_cfg: &anodizer_core::config::CrateConfig,
    release_cfg: &anodizer_core::config::ReleaseConfig,
    ids_filter: Option<&Vec<String>>,
    include_meta: bool,
    dry_run: bool,
) -> Result<Vec<(std::path::PathBuf, Option<String>)>> {
    let mut artifact_entries: Vec<(std::path::PathBuf, Option<String>)> =
        collect_release_upload_candidates(
            ctx,
            &crate_cfg.name,
            ids_filter.map(Vec::as_slice),
            include_meta,
        );

    if let Some(ids) = ids_filter {
        if artifact_entries.is_empty() {
            log.warn(&format!(
                "ids filter {:?} matched zero artifacts for crate '{}' \
                 (the release will be created with no uploaded files; check \
                 the ids match a configured build/archive id)",
                ids, crate_cfg.name
            ));
        } else {
            log.verbose(&format!(
                "ids filter {:?} selected {} artifacts for crate '{}'",
                ids,
                artifact_entries.len(),
                crate_cfg.name
            ));
        }
    }

    // Refresh combined checksum files before
    // upload so they include signatures/artifacts added after the checksum
    // stage ran.
    anodizer_stage_checksum::refresh_combined_checksums(ctx, dry_run)?;

    if let Some(extra_specs) = &release_cfg.extra_files {
        let extra = collect_extra_files(extra_specs, ctx)?;
        artifact_entries.extend(extra);
    }

    // Rendered templated_extra_files are written to the shared dist
    // directory. If multiple release configs use the same dst name, later
    // writes will overwrite earlier ones — callers should ensure dst names
    // are unique across configs.
    if let Some(ref tpl_specs) = release_cfg.templated_extra_files
        && !tpl_specs.is_empty()
    {
        let dist_dir = &ctx.config.dist;
        let rendered = anodizer_core::templated_files::process_templated_extra_files(
            tpl_specs, ctx, dist_dir, "release",
        )?;
        for (path, dst_name) in rendered {
            artifact_entries.push((path, Some(dst_name)));
        }
    }

    if include_meta {
        let dist_dir = &ctx.config.dist;
        let meta_name = "metadata.json";
        let meta_path = dist_dir.join(meta_name);
        if meta_path.exists() {
            artifact_entries.push((meta_path, None));
        } else if ctx.is_strict() {
            anyhow::bail!(
                "include_meta: {} not found at {} (strict mode)",
                meta_name,
                meta_path.display()
            );
        } else {
            log.warn(&format!(
                "include_meta file {} not found at {}",
                meta_name,
                meta_path.display()
            ));
        }
    }

    Ok(artifact_entries)
}

/// Render the release body for one crate: header + footer + non-determinism
/// exemption block + changelog (with `{{ .Checksums }}` already substituted).
///
/// # Header / footer precedence (anodizer-local)
///
/// `release.header` wins over `changelog.header` (the latter is stashed by
/// the changelog stage in `ctx.stage_outputs.changelog_header`). The
/// only has the `release.*` source (loaded via `loadContent(ReleaseHeader…)`
/// from the changelog stage); anodizer extends that to a
/// second source as a Rust-first ergonomic so a YAML-configured changelog
/// wrapper still reaches the release body. Same for the footer.
///
/// # Non-determinism exemptions
///
/// When `ctx.determinism.runtime_allowlist` is non-empty, an exemption
/// notice is prepended to the changelog body (which is where
/// `{{ .Checksums }}` lives by convention) so the notice unambiguously
/// precedes any checksums the user templated into the body. Blank-line
/// separator so markdown consumers treat it as a distinct paragraph.
fn compose_full_release_body(
    ctx: &Context,
    release_cfg: &anodizer_core::config::ReleaseConfig,
    crate_name: &str,
    changelog_body: &str,
) -> Result<String> {
    let release_header = release_cfg
        .header
        .as_ref()
        .map(|src| {
            let raw = resolve_content_source(src, ctx)
                .with_context(|| format!("release: resolve header for crate '{}'", crate_name))?;
            ctx.render_template(&raw)
                .with_context(|| format!("release: render header for crate '{}'", crate_name))
        })
        .transpose()?;
    let release_footer = release_cfg
        .footer
        .as_ref()
        .map(|src| {
            let raw = resolve_content_source(src, ctx)
                .with_context(|| format!("release: resolve footer for crate '{}'", crate_name))?;
            ctx.render_template(&raw)
                .with_context(|| format!("release: render footer for crate '{}'", crate_name))
        })
        .transpose()?;
    let rendered_header = resolve_header_footer(
        release_header.as_deref(),
        ctx.stage_outputs.changelog_header.as_deref(),
    )
    .map(str::to_owned);
    let rendered_footer = resolve_header_footer(
        release_footer.as_deref(),
        ctx.stage_outputs.changelog_footer.as_deref(),
    )
    .map(str::to_owned);

    let exemptions = ctx
        .determinism
        .as_ref()
        .map(|s| render_nondeterministic_exemptions_block(&s.runtime_allowlist))
        .unwrap_or_default();
    let changelog_with_exemptions = if exemptions.is_empty() {
        changelog_body.to_string()
    } else if changelog_body.is_empty() {
        exemptions
    } else {
        format!("{}\n{}", exemptions, changelog_body)
    };
    Ok(build_release_body(
        &changelog_with_exemptions,
        rendered_header.as_deref(),
        rendered_footer.as_deref(),
    ))
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
struct ResolvedReleaseFlags {
    draft: bool,
    prerelease: bool,
    skip_upload: bool,
    replace_existing_draft: bool,
    replace_existing_artifacts: bool,
    make_latest: Option<octocrab::repos::releases::MakeLatest>,
    target_commitish: Option<String>,
    discussion_category_name: Option<String>,
    include_meta: bool,
    use_existing_draft: bool,
    /// Nightly retention: keep the N newest nightly releases and delete the
    /// rest (+ their tags) AFTER the new release is created and published.
    /// `Some(1)` is the rolling-single-release case (the `keep_single_release`
    /// alias). Resolved from `NightlyConfig::resolved_keep_last` (which folds in
    /// the legacy alias and its precedence). Only honored on `--nightly` runs,
    /// and only acted on by the GitHub backend.
    retention_keep_last: Option<usize>,
    /// Nightly `publish_repo`: `(owner, repo)` to redirect the release to a
    /// repo other than the source. Only honored on `--nightly` runs.
    publish_repo_override: Option<(String, String)>,
}

/// Resolve all release flags from config + CLI overrides for one crate.
fn resolve_release_flags(
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
fn warn_unsupported_nightly_retention(
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

/// Dispatch a single crate's release to the appropriate SCM backend
/// (GitHub, GitLab, or Gitea) based on `ctx.token_type`.
///
/// Returns `Some((release_url, download_base, owner, repo))` on success,
/// or `None` when the backend signals "skip this crate" (e.g. `keep_existing`
/// mode with an existing release).
#[allow(clippy::too_many_arguments)]
fn dispatch_to_scm_backend(
    ctx: &mut Context,
    log: &anodizer_core::log::StageLogger,
    rt: &tokio::runtime::Runtime,
    token: &Option<String>,
    crate_cfg: &anodizer_core::config::CrateConfig,
    release_cfg: &anodizer_core::config::ReleaseConfig,
    tag: &str,
    release_name: &str,
    release_body: &str,
    release_mode: &str,
    flags: &ResolvedReleaseFlags,
    artifact_entries: &[(std::path::PathBuf, Option<String>)],
) -> Result<Option<(String, String, String, String)>> {
    match ctx.token_type {
        ScmTokenType::GitLab => {
            warn_unsupported_nightly_retention(log, "GitLab", flags);
            let gitlab_env = gitlab::GitlabBackendEnv {
                rt,
                ctx,
                log,
                token,
            };
            let gitlab_spec = gitlab::GitlabBackendSpec {
                tag,
                release_name,
                release_body,
                release_mode,
                skip_upload: flags.skip_upload,
                replace_existing_draft: flags.replace_existing_draft,
                use_existing_draft: flags.use_existing_draft,
                replace_existing_artifacts: flags.replace_existing_artifacts,
            };
            Ok(gitlab::run_gitlab_backend(
                &gitlab_env,
                crate_cfg,
                release_cfg,
                &gitlab_spec,
                artifact_entries,
            )?)
        }

        ScmTokenType::Gitea => {
            warn_unsupported_nightly_retention(log, "Gitea", flags);
            let gitea_env = gitea::GiteaBackendEnv {
                rt,
                ctx,
                log,
                token,
            };
            let gitea_spec = gitea::GiteaBackendSpec {
                tag,
                release_name,
                release_body,
                release_mode,
                draft: flags.draft,
                prerelease: flags.prerelease,
                skip_upload: flags.skip_upload,
                replace_existing_draft: flags.replace_existing_draft,
                use_existing_draft: flags.use_existing_draft,
                replace_existing_artifacts: flags.replace_existing_artifacts,
            };
            Ok(gitea::run_gitea_backend(
                &gitea_env,
                crate_cfg,
                release_cfg,
                &gitea_spec,
                artifact_entries,
            )?)
        }

        ScmTokenType::GitHub => {
            let env = github::BackendEnv {
                rt,
                ctx,
                log,
                token,
            };
            let spec = github::GithubReleaseSpec {
                tag,
                name: release_name,
                body: release_body,
                mode: release_mode,
                draft: flags.draft,
                prerelease: flags.prerelease,
                make_latest: &flags.make_latest,
                target_commitish: &flags.target_commitish,
                discussion_category: &flags.discussion_category_name,
            };
            let upload_opts = github::UploadOpts {
                skip_upload: flags.skip_upload,
                replace_existing_draft: flags.replace_existing_draft,
                replace_existing_artifacts: flags.replace_existing_artifacts,
                use_existing_draft: flags.use_existing_draft,
                resume_release: ctx.options.resume_release,
                retention_keep_last: flags.retention_keep_last,
                publish_repo_override: flags.publish_repo_override.clone(),
            };
            Ok(github::run_github_backend(
                &env,
                crate_cfg,
                release_cfg,
                &spec,
                &upload_opts,
                artifact_entries,
            )?)
        }
    }
}

/// Per-release summary fields surfaced in dry-run output.
///
/// Bundles the long argument list for [`handle_dry_run`] so the signature
/// stays under clippy's threshold and the call site reads like a struct
/// literal rather than a positional dump.
struct DryRunSummary<'a> {
    crate_name: &'a str,
    release_name: &'a str,
    tag: &'a str,
    draft: bool,
    prerelease: bool,
    release_mode: &'a str,
    skip_upload: bool,
    retention_keep_last: Option<usize>,
    publish_repo_override: Option<(String, String)>,
    artifact_entries: &'a [(std::path::PathBuf, Option<String>)],
}

/// Resolve the dry-run download-base URL for the active SCM provider.
///
/// Falls back to the public default for each provider when no override is
/// configured. For Gitea, the download base is additionally derived from the
/// API URL by stripping the `/api/v1` suffix.
fn dry_run_download_base(ctx: &Context) -> String {
    match ctx.token_type {
        ScmTokenType::GitHub => ctx
            .config
            .github_urls
            .as_ref()
            .and_then(|u| u.download.clone())
            .unwrap_or_else(|| "https://github.com".to_string()),
        ScmTokenType::GitLab => ctx
            .config
            .gitlab_urls
            .as_ref()
            .and_then(|u| u.download.clone())
            .unwrap_or_else(|| "https://gitlab.com".to_string()),
        ScmTokenType::Gitea => ctx
            .config
            .gitea_urls
            .as_ref()
            .and_then(|u| u.download.clone())
            .unwrap_or_else(|| {
                ctx.config
                    .gitea_urls
                    .as_ref()
                    .and_then(|u| u.api.as_deref())
                    .map(|api| {
                        api.trim_end_matches('/')
                            .trim_end_matches("/api/v1")
                            .to_string()
                    })
                    .unwrap_or_else(|| "https://gitea.com".to_string())
            }),
    }
}

/// Log every configured `<provider>_urls.*` value in dry-run output so the
/// user can see which override is active without re-running with a live
/// token.
fn log_dry_run_provider_urls(ctx: &Context, log: &anodizer_core::log::StageLogger) {
    match ctx.token_type {
        ScmTokenType::GitHub => {
            if let Some(urls) = &ctx.config.github_urls {
                if let Some(api) = &urls.api {
                    log.status(&format!("(dry-run)   github_urls.api = {}", api));
                }
                if let Some(upload) = &urls.upload {
                    log.status(&format!("(dry-run)   github_urls.upload = {}", upload));
                }
                if let Some(download) = &urls.download {
                    log.status(&format!("(dry-run)   github_urls.download = {}", download));
                }
                if urls.skip_tls_verify.unwrap_or(false) {
                    log.status("(dry-run)   github_urls.skip_tls_verify = true");
                }
            }
        }
        ScmTokenType::GitLab => {
            if let Some(urls) = &ctx.config.gitlab_urls {
                if let Some(api) = &urls.api {
                    log.status(&format!("(dry-run)   gitlab_urls.api = {}", api));
                }
                if let Some(download) = &urls.download {
                    log.status(&format!("(dry-run)   gitlab_urls.download = {}", download));
                }
                if urls.skip_tls_verify.unwrap_or(false) {
                    log.status("(dry-run)   gitlab_urls.skip_tls_verify = true");
                }
                if urls.use_package_registry.unwrap_or(false) {
                    log.status("(dry-run)   gitlab_urls.use_package_registry = true");
                }
                if urls.use_job_token.unwrap_or(false) {
                    log.status("(dry-run)   gitlab_urls.use_job_token = true");
                }
            }
        }
        ScmTokenType::Gitea => {
            if let Some(urls) = &ctx.config.gitea_urls {
                if let Some(api) = &urls.api {
                    log.status(&format!("(dry-run)   gitea_urls.api = {}", api));
                }
                if let Some(download) = &urls.download {
                    log.status(&format!("(dry-run)   gitea_urls.download = {}", download));
                }
                if urls.skip_tls_verify.unwrap_or(false) {
                    log.status("(dry-run)   gitea_urls.skip_tls_verify = true");
                }
            }
        }
    }
}

/// Emit dry-run telemetry for one crate's release and populate artifact
/// download URLs so publishers can render manifests with correct URLs even
/// when no real release was created.
fn handle_dry_run(
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
            "(dry-run)   would publish to override repo '{owner}/{repo}' (nightly.publish_repo)",
        ));
    }
    // retention_keep_last folds in the keep_single_release alias (=> Some(1)).
    if let Some(keep_last) = s.retention_keep_last {
        if keep_last == 1 {
            log.status(
                "(dry-run)   would delete prior nightly release(s) before recreating (nightly retention keep_last=1 / keep_single_release)",
            );
        } else {
            log.status(&format!(
                "(dry-run)   would keep the {keep_last} newest nightly release(s) and delete the rest, incl. their tags (nightly retention)",
            ));
        }
    }
    if s.skip_upload {
        log.status("(dry-run)   skip_upload is set, would skip artifact uploads");
    } else {
        for (path, custom_name) in s.artifact_entries {
            if let Some(name) = custom_name {
                log.status(&format!(
                    "(dry-run)   would upload artifact: {} (as '{}')",
                    path.display(),
                    name,
                ));
            } else {
                log.status(&format!(
                    "(dry-run)   would upload artifact: {}",
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

/// Enumerate the release-upload candidate set for a single crate.
///
/// Source of truth for which artifacts get uploaded to a GitHub/GitLab/Gitea
/// release. The upload set:
/// `release_uploadable_kinds()` plus `Metadata` when `include_meta` is true.
///
/// Filters applied (in order):
/// 1. Kind must be in the release-uploadable set.
/// 2. Crate must match `crate_name`.
/// 3. Binary-sign intermediates are excluded (see `is_binary_sign_output`).
/// 4. When `ids` is supplied, `matches_id_filter` is applied.
///
/// Returned entries pair each artifact's path with an optional custom
/// destination name (always `None` here; extra-files appending happens
/// at the call site).
pub fn collect_release_upload_candidates(
    ctx: &Context,
    crate_name: &str,
    ids: Option<&[String]>,
    include_meta: bool,
) -> Vec<(std::path::PathBuf, Option<String>)> {
    let mut upload_kinds: Vec<ArtifactKind> =
        anodizer_core::artifact::release_uploadable_kinds().to_vec();
    if include_meta {
        upload_kinds.push(ArtifactKind::Metadata);
    }
    upload_kinds
        .iter()
        .flat_map(|&kind| {
            ctx.artifacts
                .by_kind_and_crate(kind, crate_name)
                .into_iter()
                .filter(|a| !anodizer_core::artifact::is_binary_sign_output(a))
                .filter(|a| matches_id_filter(a, ids))
                .map(|a| (a.path.clone(), None))
                .collect::<Vec<_>>()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::{NightlyConfig, ReleaseConfig};
    use anodizer_core::log::{StageLogger, Verbosity};
    use anodizer_core::test_helpers::TestContextBuilder;

    fn quiet_log() -> StageLogger {
        StageLogger::new("test", Verbosity::Quiet)
    }

    #[test]
    fn ensure_release_url_derives_default_from_repo_and_tag_when_unset() {
        // The announce/webhook/email failure mode: a path that never reaches
        // the create backend (dry-run / snapshot / publish-only against an
        // already-published release) must still leave a renderable
        // `{{ ReleaseURL }}` in context, derived from the GitHub repo + tag.
        let mut ctx = TestContextBuilder::new().tag("v1.0.0").build();
        assert!(
            ctx.template_vars().get("ReleaseURL").is_none(),
            "precondition: ReleaseURL starts unset"
        );
        let release_cfg = ReleaseConfig {
            github: Some(anodizer_core::config::ScmRepoConfig {
                owner: "tj-smith47".to_string(),
                name: "anodizer".to_string(),
            }),
            ..Default::default()
        };
        ensure_release_url(&mut ctx, &release_cfg, "v1.0.0", "anodizer")
            .expect("ensure_release_url returns Ok");
        assert_eq!(
            ctx.template_vars().get("ReleaseURL").map(String::as_str),
            Some("https://github.com/tj-smith47/anodizer/releases/tag/v1.0.0"),
            "ReleaseURL must be derived from owner/repo/tag"
        );
    }

    #[test]
    fn ensure_release_url_preserves_authoritative_url_already_set() {
        // The create path's authoritative `html_url` must NOT be clobbered by
        // the derived default when both run (the derive guard fires first, the
        // create overwrite fires last — but a re-entry must respect the set value).
        let mut ctx = TestContextBuilder::new().tag("v1.0.0").build();
        ctx.set_release_url(
            "https://github.com/tj-smith47/anodizer/releases/tag/v1.0.0-authoritative",
        );
        let release_cfg = ReleaseConfig {
            github: Some(anodizer_core::config::ScmRepoConfig {
                owner: "tj-smith47".to_string(),
                name: "anodizer".to_string(),
            }),
            ..Default::default()
        };
        ensure_release_url(&mut ctx, &release_cfg, "v1.0.0", "anodizer")
            .expect("ensure_release_url returns Ok");
        assert_eq!(
            ctx.template_vars().get("ReleaseURL").map(String::as_str),
            Some("https://github.com/tj-smith47/anodizer/releases/tag/v1.0.0-authoritative"),
            "an already-set ReleaseURL must be preserved"
        );
    }

    #[test]
    fn ensure_release_url_noop_when_no_repo_block_configured() {
        // No `release.github` block → nothing to derive an owner/repo from;
        // leave ReleaseURL unset rather than invent a URL against no repo.
        let mut ctx = TestContextBuilder::new().tag("v1.0.0").build();
        let release_cfg = ReleaseConfig::default();
        ensure_release_url(&mut ctx, &release_cfg, "v1.0.0", "demo")
            .expect("ensure_release_url returns Ok");
        assert!(
            ctx.template_vars().get("ReleaseURL").is_none(),
            "no repo block → ReleaseURL stays unset"
        );
    }

    #[test]
    fn should_skip_release_returns_true_when_nightly_and_publish_release_false() {
        let mut ctx = TestContextBuilder::new().tag("v0.0.0-test").build();
        ctx.options.nightly = true;
        ctx.config.nightly = Some(NightlyConfig {
            publish_release: Some(false),
            ..Default::default()
        });
        let release_cfg = ReleaseConfig::default();
        let log = quiet_log();
        let result = should_skip_release(&ctx, &release_cfg, "demo", &log)
            .expect("should_skip_release returns Ok");
        assert!(
            result,
            "publish_release: false must cause skip on nightly run"
        );
    }

    #[test]
    fn should_skip_release_returns_false_when_not_nightly_even_with_publish_release_false() {
        let mut ctx = TestContextBuilder::new().tag("v0.0.0-test").build();
        // options.nightly defaults to false
        ctx.config.nightly = Some(NightlyConfig {
            publish_release: Some(false),
            ..Default::default()
        });
        let release_cfg = ReleaseConfig::default();
        let log = quiet_log();
        let result = should_skip_release(&ctx, &release_cfg, "demo", &log)
            .expect("should_skip_release returns Ok");
        assert!(
            !result,
            "publish_release: false must only skip on nightly; non-nightly must run"
        );
    }

    #[test]
    fn should_skip_release_returns_false_when_nightly_and_publish_release_default() {
        let mut ctx = TestContextBuilder::new().tag("v0.0.0-test").build();
        ctx.options.nightly = true;
        // config.nightly absent — default is publish_release: true
        ctx.config.nightly = None;
        let release_cfg = ReleaseConfig::default();
        let log = quiet_log();
        let result = should_skip_release(&ctx, &release_cfg, "demo", &log)
            .expect("should_skip_release returns Ok");
        assert!(
            !result,
            "absent nightly.publish_release must default to run (not skip)"
        );
    }

    #[test]
    fn resolve_release_flags_nightly_draft_some_overrides_release_draft() {
        // nightly.draft = Some(true) wins over release.draft = false when is_nightly().
        let mut ctx = TestContextBuilder::new().tag("v0.0.0-test").build();
        ctx.options.nightly = true;
        ctx.config.nightly = Some(NightlyConfig {
            draft: Some(true),
            ..Default::default()
        });
        let release_cfg = ReleaseConfig {
            draft: Some(false),
            ..Default::default()
        };
        let flags = resolve_release_flags(&ctx, &release_cfg, "demo", "nightly")
            .expect("resolve_release_flags returns Ok");
        assert!(
            flags.draft,
            "nightly.draft=Some(true) must override release.draft=false"
        );
    }

    #[test]
    fn resolve_release_flags_nightly_draft_none_preserves_release_draft() {
        // nightly.draft = None falls through to release.draft when is_nightly().
        let mut ctx = TestContextBuilder::new().tag("v0.0.0-test").build();
        ctx.options.nightly = true;
        ctx.config.nightly = Some(NightlyConfig {
            draft: None,
            ..Default::default()
        });
        let release_cfg = ReleaseConfig {
            draft: Some(true),
            ..Default::default()
        };
        let flags = resolve_release_flags(&ctx, &release_cfg, "demo", "nightly")
            .expect("resolve_release_flags returns Ok");
        assert!(
            flags.draft,
            "nightly.draft=None must fall through to release.draft=true"
        );
    }

    #[test]
    fn resolve_release_flags_keep_single_release_ignored_when_not_nightly() {
        // nightly.keep_single_release = Some(true) must be ignored outside
        // nightly runs (retention_keep_last stays None).
        let mut ctx = TestContextBuilder::new().tag("v0.0.0-test").build();
        // options.nightly defaults to false
        ctx.config.nightly = Some(NightlyConfig {
            keep_single_release: Some(true),
            ..Default::default()
        });
        let release_cfg = ReleaseConfig::default();
        let flags = resolve_release_flags(&ctx, &release_cfg, "demo", "v1.0.0")
            .expect("resolve_release_flags returns Ok");
        assert_eq!(
            flags.retention_keep_last, None,
            "keep_single_release must not enable retention outside nightly runs"
        );
    }

    #[test]
    fn resolve_release_flags_keep_single_release_honored_when_nightly() {
        // nightly.keep_single_release = Some(true) must resolve to
        // retention_keep_last == Some(1) when is_nightly().
        let mut ctx = TestContextBuilder::new().tag("v0.0.0-test").build();
        ctx.options.nightly = true;
        ctx.config.nightly = Some(NightlyConfig {
            keep_single_release: Some(true),
            ..Default::default()
        });
        let release_cfg = ReleaseConfig::default();
        let flags = resolve_release_flags(&ctx, &release_cfg, "demo", "nightly")
            .expect("resolve_release_flags returns Ok");
        assert_eq!(
            flags.retention_keep_last,
            Some(1),
            "nightly keep_single_release must resolve to retention_keep_last == Some(1)"
        );
    }

    #[test]
    fn resolve_release_flags_retention_keep_last_honored_when_nightly() {
        // nightly.retention.keep_last = N propagates directly to flags.
        let mut ctx = TestContextBuilder::new().tag("v0.0.0-test").build();
        ctx.options.nightly = true;
        ctx.config.nightly = Some(NightlyConfig {
            retention: Some(anodizer_core::config::RetentionConfig { keep_last: 10 }),
            ..Default::default()
        });
        let release_cfg = ReleaseConfig::default();
        let flags = resolve_release_flags(&ctx, &release_cfg, "demo", "nightly")
            .expect("resolve_release_flags returns Ok");
        assert_eq!(flags.retention_keep_last, Some(10));
    }

    /// Build a `ResolvedReleaseFlags` with only the retention fields set, for
    /// the unsupported-backend warning tests.
    fn flags_with_retention(
        keep_last: Option<usize>,
        publish_repo: Option<(String, String)>,
    ) -> ResolvedReleaseFlags {
        ResolvedReleaseFlags {
            draft: false,
            prerelease: false,
            skip_upload: false,
            replace_existing_draft: false,
            replace_existing_artifacts: false,
            make_latest: None,
            target_commitish: None,
            discussion_category_name: None,
            include_meta: false,
            use_existing_draft: false,
            retention_keep_last: keep_last,
            publish_repo_override: publish_repo,
        }
    }

    #[test]
    fn warn_unsupported_nightly_retention_warns_for_keep_last() {
        let (log, capture) = StageLogger::with_capture("release", Verbosity::Normal);
        warn_unsupported_nightly_retention(&log, "GitLab", &flags_with_retention(Some(3), None));
        let warns = capture.warn_messages();
        assert_eq!(warns.len(), 1, "exactly one warning expected: {warns:?}");
        assert!(
            warns[0].contains("GitLab") && warns[0].contains("retention"),
            "warning must name the backend + retention: {warns:?}"
        );
    }

    #[test]
    fn warn_unsupported_nightly_retention_warns_for_publish_repo() {
        let (log, capture) = StageLogger::with_capture("release", Verbosity::Normal);
        warn_unsupported_nightly_retention(
            &log,
            "Gitea",
            &flags_with_retention(None, Some(("nushell".into(), "nightly".into()))),
        );
        let warns = capture.warn_messages();
        assert_eq!(warns.len(), 1, "exactly one warning expected: {warns:?}");
        assert!(
            warns[0].contains("Gitea") && warns[0].contains("publish_repo"),
            "warning must name the backend + publish_repo: {warns:?}"
        );
    }

    #[test]
    fn warn_unsupported_nightly_retention_silent_when_unset() {
        let (log, capture) = StageLogger::with_capture("release", Verbosity::Normal);
        warn_unsupported_nightly_retention(&log, "GitLab", &flags_with_retention(None, None));
        assert_eq!(
            capture.warn_count(),
            0,
            "no warning when neither retention nor publish_repo is set"
        );
    }

    #[test]
    fn validate_release_flags_rejects_replace_and_use_existing_draft_together() {
        // Both flags set simultaneously is always an error.
        let release_cfg = ReleaseConfig {
            replace_existing_draft: Some(true),
            use_existing_draft: Some(true),
            ..Default::default()
        };
        let result = validate_release_flags(&release_cfg, "demo");
        assert!(
            result.is_err(),
            "replace_existing_draft + use_existing_draft must be rejected"
        );
        let msg = format!("{:?}", result.unwrap_err());
        assert!(
            msg.contains("replace_existing_draft") && msg.contains("use_existing_draft"),
            "error must name both conflicting flags; got: {msg}"
        );
    }

    #[test]
    fn validate_release_flags_accepts_replace_existing_draft_alone() {
        let release_cfg = ReleaseConfig {
            replace_existing_draft: Some(true),
            use_existing_draft: Some(false),
            ..Default::default()
        };
        assert!(
            validate_release_flags(&release_cfg, "demo").is_ok(),
            "replace_existing_draft=true alone must not error"
        );
    }

    #[test]
    fn validate_release_flags_accepts_use_existing_draft_alone() {
        let release_cfg = ReleaseConfig {
            replace_existing_draft: Some(false),
            use_existing_draft: Some(true),
            ..Default::default()
        };
        assert!(
            validate_release_flags(&release_cfg, "demo").is_ok(),
            "use_existing_draft=true alone must not error"
        );
    }

    #[test]
    fn validate_nightly_config_is_noop_when_not_nightly() {
        let ctx = TestContextBuilder::new().tag("v0.0.0-test").build();
        validate_nightly_config(&ctx, &quiet_log());
    }

    #[test]
    fn validate_nightly_config_is_noop_when_nightly_block_absent() {
        let mut ctx = TestContextBuilder::new().tag("v0.0.0-test").build();
        ctx.options.nightly = true;
        validate_nightly_config(&ctx, &quiet_log());
    }

    #[test]
    fn validate_nightly_config_noop_when_only_draft_set() {
        let mut ctx = TestContextBuilder::new().tag("v0.0.0-test").build();
        ctx.options.nightly = true;
        ctx.config.nightly = Some(NightlyConfig {
            draft: Some(true),
            keep_single_release: None,
            ..Default::default()
        });
        validate_nightly_config(&ctx, &quiet_log());
    }

    #[test]
    fn validate_nightly_config_noop_when_only_keep_single_release_set() {
        let mut ctx = TestContextBuilder::new().tag("v0.0.0-test").build();
        ctx.options.nightly = true;
        ctx.config.nightly = Some(NightlyConfig {
            draft: None,
            keep_single_release: Some(true),
            ..Default::default()
        });
        validate_nightly_config(&ctx, &quiet_log());
    }

    #[test]
    fn validate_nightly_config_warns_when_nightly_draft_and_keep_single_release_both_true() {
        // draft=true + keep_single_release=true on a nightly run is the
        // Documented gotcha. The function must run without
        // panicking (the warn-emission path is exercised end-to-end).
        let mut ctx = TestContextBuilder::new().tag("v0.0.0-test").build();
        ctx.options.nightly = true;
        ctx.config.nightly = Some(NightlyConfig {
            draft: Some(true),
            keep_single_release: Some(true),
            ..Default::default()
        });
        validate_nightly_config(&ctx, &quiet_log());
    }
}
