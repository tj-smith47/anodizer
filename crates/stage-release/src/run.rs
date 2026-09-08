use anodizer_core::artifact::{ArtifactKind, matches_id_filter};
use anodizer_core::context::Context;
use anodizer_core::release_tag::{anchor_crate_tag, resolve_release_tag};
use anodizer_core::scm::ScmTokenType;
use anodizer_core::stage::Stage;
use anyhow::{Context as _, Result};

use crate::dry_run::{DryRunSummary, dry_run_download_base, handle_dry_run};
use crate::flags::{
    ResolvedReleaseFlags, resolve_release_flags, validate_nightly_config, validate_release_flags,
    warn_unsupported_nightly_retention,
};
use crate::release_body::{
    build_release_body, collect_extra_files, render_nondeterministic_exemptions_block,
    resolve_content_source, resolve_header_footer,
};
use crate::{
    compose_release_url, gitea, github, gitlab, populate_artifact_download_urls,
    resolve_release_repo,
};

impl Stage for super::ReleaseStage {
    fn name(&self) -> &str {
        "release"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("release");

        // Operator-selection gate. The GitHub-release publisher
        // (`GithubReleasePublisher::PUBLISHER_NAME == "github-release"`) creates
        // the release and uploads its assets through this stage, which runs as a
        // pipeline stage OUTSIDE the trait-based dispatch chokepoint — so the
        // uniform `--skip` / `--publishers` filter that governs every dispatched
        // publisher does not reach it on its own. Consult
        // `publisher_deselected("github-release")` here, BEFORE any token use or
        // SCM side effect, so an operator who ran `--publishers npm` (or
        // `--skip=github-release`) does NOT create a release or re-upload assets.
        // An EMPTY allowlist deselects nothing, so the main release job (empty
        // allowlist + `--skip=npm`) still runs. Mirrors the docker / blob /
        // snapcraft-publish / docker-sign / announce self-skips; the record-row
        // discipline lives in the publish-loop's github-release dispatch, which
        // already reports the deselected publisher, so this stage need only
        // short-circuit and announce the skip.
        if ctx.publisher_deselected("github-release") {
            log.status(&ctx.deselected_reason("github-release"));
            return Ok(());
        }

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
            .crate_universe()
            .into_iter()
            .filter(|c| c.release.is_some())
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .cloned()
            .collect();

        // Create the tokio runtime once, outside the loop.
        let rt =
            tokio::runtime::Runtime::new().context("release: failed to create tokio runtime")?;

        validate_nightly_config(ctx, &log);

        // Attribute the release stage's asset-upload backoff to a single
        // "release" scope. The upload fan-out spawns tasks on `rt` that may
        // migrate across worker threads, but every one reads the same constant
        // scope value for the stage's duration, so no task-local is needed.
        let _retry_scope = anodizer_core::retry::RetryScope::enter("release");

        for crate_cfg in &crates {
            let Some(release_cfg) = crate_cfg.release.as_ref() else {
                continue;
            };
            if should_skip_release(ctx, release_cfg, &crate_cfg.name, &log)? {
                // No release exists for this crate; record it so the
                // verify-release gate does not hunt for its assets.
                if !ctx
                    .stage_outputs
                    .release_skipped_crates
                    .contains(&crate_cfg.name)
                {
                    ctx.stage_outputs
                        .release_skipped_crates
                        .push(crate_cfg.name.clone());
                }
                continue;
            }
            validate_release_flags(release_cfg, &crate_cfg.name)?;
            // A `token` set on the release repo config overrides the pipeline
            // token for this crate's release API calls (cross-releasing, e.g.
            // code private / release public).
            let crate_token =
                crate::resolve_release_token(ctx, release_cfg).or_else(|| token.clone());
            release_one_crate(
                ctx,
                &log,
                &rt,
                &crate_token,
                crate_cfg,
                release_cfg,
                dry_run,
            )?;
        }

        ctx.stage_outputs.release_stage_ran = true;

        Ok(())
    }
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
    if ctx.is_nightly()
        && ctx
            .config
            .nightly
            .as_ref()
            .and_then(|n| n.skip_if_no_changes)
            == Some(true)
        && let Some(range) = nightly_changelog_range(ctx, crate_name)
        && range.notable_entries == 0
        // A repo with no prior tag in this family has never published
        // anything, so "no changes since the last release" is not a statement
        // it can make: everything in its history is unreleased. Only a range
        // bounded by a real previous tag can be empty in the sense this knob
        // means, so the first nightly always cuts.
        && let Some(previous_tag) = range.previous_tag.as_deref()
    {
        log.status(&format!(
            "skipping nightly release for crate '{crate_name}': no notable changes since {previous_tag}"
        ));
        return Ok(true);
    }
    Ok(false)
}

/// The changelog range this crate's nightly release would describe: the
/// single-track aggregate when the changelog stage produced one, else the
/// crate's own slice. `None` when the changelog stage did not run — an
/// absent signal is never a reason to skip.
fn nightly_changelog_range<'a>(
    ctx: &'a Context,
    crate_name: &str,
) -> Option<&'a anodizer_core::context::ChangelogRangeSummary> {
    ctx.stage_outputs
        .release_body_range
        .as_ref()
        .or_else(|| ctx.stage_outputs.changelog_ranges.get(crate_name))
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

    // Prefer the single-track AGGREGATE body when the changelog stage produced
    // one: for a lockstep / flat-aggregate / single-crate workspace the release
    // spans every crate directory, so the per-crate slice keyed by this release
    // crate's name would collapse to empty whenever the range's commits missed
    // that crate's own path (the recurring "No notable changes" bug when the
    // `release:` block lives on the binary crate at `crates/cli`). A per-crate
    // workspace leaves the aggregate `None` and falls back to this crate's slice.
    let changelog_body = ctx
        .stage_outputs
        .release_body_changelog
        .clone()
        .unwrap_or_else(|| {
            ctx.stage_outputs
                .changelogs
                .get(&crate_name)
                .cloned()
                .unwrap_or_default()
        });

    crate::populate_checksums_var(ctx)?;

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

    let tag = resolve_release_tag(ctx, crate_cfg, release_cfg.tag.as_deref())?;
    // Every `{{ Tag }}` this crate's release renders — header, footer, blob
    // directory, announce body, compare link — must name the tag the release
    // is created on, not the run-wide base tag another family supplied. The
    // body is the biggest consumer of it, so it is composed AFTER the anchor;
    // composing first left each crate's header and footer showing whichever
    // tag the previous crate in the loop anchored.
    anchor_crate_tag(ctx, crate_cfg, &tag, log);

    let release_body = compose_full_release_body(ctx, release_cfg, &crate_name, &changelog_body)?;

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
    let exclude_filter = release_cfg.exclude.as_ref();

    let artifact_entries = assemble_artifact_entries(
        ctx,
        log,
        crate_cfg,
        release_cfg,
        ids_filter,
        exclude_filter,
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
                release_body: &release_body,
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
    // A nightly run names its rolling release from `nightly.name_template`
    // when the user set one; `release.name_template` is the stable-release
    // name and applies to every other run.
    let name_tmpl = nightly_template(ctx, |n| n.name_template.as_deref())
        .unwrap_or_else(|| release_cfg.resolved_name_template().to_string());
    ctx.render_template(&name_tmpl)
        .with_context(|| format!("release: render name_template for crate '{}'", crate_name))
}

/// A non-empty `nightly:` template, on a nightly run only. `None` on any
/// other run, or when the field is unset / blank.
fn nightly_template(
    ctx: &Context,
    field: impl Fn(&anodizer_core::config::NightlyConfig) -> Option<&str>,
) -> Option<String> {
    if !ctx.is_nightly() {
        return None;
    }
    ctx.config
        .nightly
        .as_ref()
        .and_then(field)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
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
/// 5. `include_meta: true` appends `metadata.json` (only the Metadata kind,
///    never anodizer's local `artifacts.json` dist manifest).
// One private helper for the release-entry assembly; the ids/exclude/meta/
// dry-run knobs are each a distinct filter the single caller threads through,
// and a params struct would only relocate the same fields.
#[allow(clippy::too_many_arguments)]
pub(crate) fn assemble_artifact_entries(
    ctx: &mut Context,
    log: &anodizer_core::log::StageLogger,
    crate_cfg: &anodizer_core::config::CrateConfig,
    release_cfg: &anodizer_core::config::ReleaseConfig,
    ids_filter: Option<&Vec<String>>,
    exclude_filter: Option<&Vec<String>>,
    include_meta: bool,
    dry_run: bool,
) -> Result<Vec<(std::path::PathBuf, Option<String>)>> {
    // The id-filtered set BEFORE exclude, so an exclude that drops every
    // remaining asset can be distinguished from an ids filter that already
    // matched nothing.
    let pre_exclude = collect_release_upload_candidates(
        ctx,
        &crate_cfg.name,
        ids_filter.map(Vec::as_slice),
        None,
        include_meta,
    )
    .len();

    let mut artifact_entries: Vec<(std::path::PathBuf, Option<String>)> =
        collect_release_upload_candidates(
            ctx,
            &crate_cfg.name,
            ids_filter.map(Vec::as_slice),
            exclude_filter.map(Vec::as_slice),
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

    if anodizer_core::artifact::exclude_filter_eliminated_all(
        exclude_filter.map(Vec::as_slice),
        pre_exclude,
        artifact_entries.len(),
    ) {
        log.warn(&format!(
            "exclude filter {:?} dropped all {} candidate asset(s) for crate '{}' \
             (the release will be created with no uploaded files; check the globs \
             match asset names, not full paths)",
            exclude_filter.map(Vec::as_slice).unwrap_or_default(),
            pre_exclude,
            crate_cfg.name
        ));
    }

    // Refresh combined checksum files before
    // upload so they include signatures/artifacts added after the checksum
    // stage ran.
    anodizer_stage_checksum::refresh_combined_checksums(ctx, dry_run)?;

    // The refresh above rewrites `checksums.txt`, so the signature the sign
    // stage produced now certifies stale bytes. Re-sign the combined checksums
    // in place before upload so the uploaded `checksums.txt.sig` matches the
    // uploaded `checksums.txt` — otherwise verify-release's crypto gate fails
    // BAD and blocks the one-way-door submitters. No-op when checksum signing
    // is not configured or the signature consumers are deselected.
    anodizer_stage_sign::resign_combined_checksums(ctx, dry_run)?;

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

    // Exactly one file: `artifacts.json` is the dist directory's local
    // manifest, recording absolute dist paths and internal artifact
    // bookkeeping that carry no meaning off the producing machine, so it is
    // never a release asset.
    if include_meta {
        let dist_dir = &ctx.config.dist;
        let meta_name = anodizer_core::dist::METADATA_JSON;
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
/// 5. When `exclude` is supplied, an asset whose name matches any glob is
///    dropped (`passes_exclude_filter`); composes with the `ids` filter.
///
/// Returned entries pair each artifact's path with an optional custom
/// destination name (always `None` here; extra-files appending happens
/// at the call site).
pub fn collect_release_upload_candidates(
    ctx: &Context,
    crate_name: &str,
    ids: Option<&[String]>,
    exclude: Option<&[String]>,
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
                // The raw macOS `.app` directory bundle is wrapped into a
                // `.dmg`/`.pkg` (uploaded instead); a directory cannot be a
                // release asset.
                .filter(|a| !anodizer_core::artifact::is_directory_bundle_artifact(a))
                .filter(|a| matches_id_filter(a, ids))
                .filter(|a| anodizer_core::artifact::passes_exclude_filter(a, exclude))
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

    /// The github-release upload path rewrites `checksums.txt` (via
    /// `refresh_combined_checksums`, to fold in publish-time artifacts) and
    /// then re-signs it (via `resign_combined_checksums`), so the uploaded
    /// `.sig` matches the uploaded `checksums.txt`. Without the re-sign the
    /// signature certifies the pre-refresh bytes and `gpg --verify` fails BAD,
    /// which is exactly what blocked the v0.22.0 release. This drives the real
    /// refresh → re-sign sequence run() performs and asserts the signature is
    /// valid against the rewritten bytes.
    #[test]
    fn release_path_resigns_checksums_after_refresh() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::config::SignConfig;
        use anodizer_core::stage::Stage;
        use std::process::Command;

        // Provisioning spawns gpg + cosign; the sign + re-sign path drives gpg.
        for tool in ["gpg", "cosign"] {
            match anodizer_core::tool_detect::runs(tool) {
                anodizer_core::tool_detect::ToolProbe::Available => {}
                probe => {
                    eprintln!(
                        "skipping release_path_resigns_checksums_after_refresh: {tool}={probe:?}"
                    );
                    return;
                }
            }
        }
        let keys = anodizer_core::harness_signing::provision_ephemeral_keys(1_715_000_000)
            .expect("provision ephemeral gpg keypair");
        let gnupg_home = keys.gnupg_home.to_string_lossy().into_owned();

        let dist = tempfile::tempdir().expect("tempdir for dist");
        let archive = dist.path().join("myapp.tar.gz");
        std::fs::write(&archive, b"archive-one").expect("write archive");
        let checksums = dist.path().join("checksums.txt");
        std::fs::write(&checksums, "0000  myapp.tar.gz\n").expect("write checksums.txt");

        let gpg_checksum = SignConfig {
            id: Some("gpg".to_string()),
            cmd: Some("gpg".to_string()),
            artifacts: Some("checksum".to_string()),
            env: Some(vec![format!("GNUPGHOME={gnupg_home}")]),
            ..Default::default()
        };

        let mut ctx = TestContextBuilder::new()
            .dry_run(false)
            .dist(dist.path().to_path_buf())
            .signs(vec![gpg_checksum])
            .build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: "myapp.tar.gz".to_string(),
            path: archive.clone(),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Checksum,
            name: "checksums.txt".to_string(),
            path: checksums.clone(),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: std::collections::HashMap::from([
                (
                    anodizer_core::artifact::COMBINED_CHECKSUM_META.to_string(),
                    anodizer_core::artifact::COMBINED_CHECKSUM_VALUE.to_string(),
                ),
                // `refresh_combined_checksums` filters on `algorithm`, which the
                // checksum stage always writes on the combined file.
                ("algorithm".to_string(), "sha256".to_string()),
            ]),
            size: None,
        });

        anodizer_stage_sign::SignStage
            .run(&mut ctx)
            .expect("sign stage");
        let sig = dist.path().join("checksums.txt.sig");
        assert!(sig.exists(), "sign stage must produce checksums.txt.sig");

        let verify = || {
            Command::new("gpg")
                .env("GNUPGHOME", &gnupg_home)
                .arg("--verify")
                .arg(&sig)
                .arg(&checksums)
                .output()
                .expect("spawn gpg --verify")
                .status
                .success()
        };
        assert!(verify(), "freshly-signed checksums.txt must verify");

        // A publish-time artifact appears AFTER the checksum + sign stages ran
        // (the docker `.digest` case), so the combined checksums file must be
        // refreshed to include it before upload.
        let extra = dist.path().join("myapp.digest");
        std::fs::write(&extra, b"digest-bytes").expect("write publish-time artifact");
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: "myapp.digest".to_string(),
            path: extra,
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        // The exact sequence run() performs before upload.
        anodizer_stage_checksum::refresh_combined_checksums(&mut ctx, false)
            .expect("refresh combined checksums");
        let refreshed = std::fs::read_to_string(&checksums).expect("read refreshed checksums");
        assert!(
            refreshed.contains("myapp.digest"),
            "refresh must fold the publish-time artifact into checksums.txt: {refreshed:?}"
        );
        assert!(
            !verify(),
            "the sign-stage signature must go stale once refresh rewrites checksums.txt"
        );

        anodizer_stage_sign::resign_combined_checksums(&mut ctx, false)
            .expect("resign combined checksums");
        assert!(
            verify(),
            "the re-signed checksums.txt.sig must verify against the refreshed bytes"
        );
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
                token: None,
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
                token: None,
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

    // ---- nightly.skip_if_no_changes ------------------------------------

    fn skip_if_no_changes_ctx() -> anodizer_core::context::Context {
        let mut ctx = TestContextBuilder::new().tag("v0.0.0-test").build();
        ctx.options.nightly = true;
        ctx.config.nightly = Some(NightlyConfig {
            skip_if_no_changes: Some(true),
            ..Default::default()
        });
        ctx
    }

    fn range(notable: usize, prev: Option<&str>) -> anodizer_core::context::ChangelogRangeSummary {
        anodizer_core::context::ChangelogRangeSummary {
            notable_entries: notable,
            previous_tag: prev.map(str::to_string),
        }
    }

    #[test]
    fn skip_if_no_changes_skips_an_empty_aggregate_range() {
        let mut ctx = skip_if_no_changes_ctx();
        ctx.stage_outputs.release_body_range = Some(range(0, Some("v0.9.0")));
        assert!(
            should_skip_release(&ctx, &ReleaseConfig::default(), "demo", &quiet_log())
                .expect("should_skip_release returns Ok"),
            "an empty single-track range must skip the nightly release"
        );
    }

    #[test]
    fn skip_if_no_changes_cuts_when_the_range_has_entries() {
        let mut ctx = skip_if_no_changes_ctx();
        ctx.stage_outputs.release_body_range = Some(range(3, Some("v0.9.0")));
        assert!(
            !should_skip_release(&ctx, &ReleaseConfig::default(), "demo", &quiet_log())
                .expect("should_skip_release returns Ok"),
            "a range with notable entries must still cut a nightly"
        );
    }

    /// Per-crate workspace: each track answers from its OWN slice, so one
    /// quiet crate does not silence a busy sibling.
    #[test]
    fn skip_if_no_changes_is_decided_per_crate_in_a_per_crate_workspace() {
        let mut ctx = skip_if_no_changes_ctx();
        ctx.stage_outputs
            .changelog_ranges
            .insert("quiet".to_string(), range(0, Some("quiet-v1.0.0")));
        ctx.stage_outputs
            .changelog_ranges
            .insert("busy".to_string(), range(2, Some("busy-v1.0.0")));
        let cfg = ReleaseConfig::default();
        assert!(
            should_skip_release(&ctx, &cfg, "quiet", &quiet_log()).expect("ok"),
            "the crate with an empty range skips"
        );
        assert!(
            !should_skip_release(&ctx, &cfg, "busy", &quiet_log()).expect("ok"),
            "its sibling with entries still cuts"
        );
    }

    /// The default is OFF: an empty range with the knob unset still cuts,
    /// which is the pre-existing rolling-nightly behaviour.
    #[test]
    fn skip_if_no_changes_defaults_to_cutting_an_empty_range() {
        let mut ctx = TestContextBuilder::new().tag("v0.0.0-test").build();
        ctx.options.nightly = true;
        ctx.config.nightly = Some(NightlyConfig::default());
        ctx.stage_outputs.release_body_range = Some(range(0, Some("v0.9.0")));
        assert!(
            !should_skip_release(&ctx, &ReleaseConfig::default(), "demo", &quiet_log())
                .expect("ok"),
            "skip_if_no_changes must be opt-in"
        );
    }

    /// An absent changelog signal (the stage was skipped) is never a reason
    /// to skip — the run has no evidence either way.
    #[test]
    fn skip_if_no_changes_cuts_when_the_changelog_stage_did_not_run() {
        let ctx = skip_if_no_changes_ctx();
        assert!(
            !should_skip_release(&ctx, &ReleaseConfig::default(), "demo", &quiet_log())
                .expect("ok"),
            "no recorded range must not be read as an empty range"
        );
    }

    /// The knob is nightly-only; a stable release with an empty range still
    /// publishes (the tag was cut deliberately).
    #[test]
    fn skip_if_no_changes_ignored_on_a_stable_run() {
        let mut ctx = TestContextBuilder::new().tag("v1.0.0").build();
        ctx.config.nightly = Some(NightlyConfig {
            skip_if_no_changes: Some(true),
            ..Default::default()
        });
        ctx.stage_outputs.release_body_range = Some(range(0, Some("v0.9.0")));
        assert!(
            !should_skip_release(&ctx, &ReleaseConfig::default(), "demo", &quiet_log())
                .expect("ok"),
            "skip_if_no_changes must not reach a stable run"
        );
    }

    /// A repo with no prior tag has published nothing, so an empty range is
    /// not "nothing changed since the last release" — it is "everything is
    /// still unreleased". Skipping there would leave the repo with no
    /// nightly at all, forever.
    #[test]
    fn first_nightly_without_a_prior_tag_is_never_skipped() {
        let mut ctx = skip_if_no_changes_ctx();
        ctx.config.nightly = Some(NightlyConfig {
            skip_if_no_changes: Some(true),
            ..Default::default()
        });
        ctx.stage_outputs.release_body_range = Some(range(0, None));
        assert!(
            !should_skip_release(&ctx, &ReleaseConfig::default(), "demo", &quiet_log())
                .expect("ok"),
            "an empty range with no previous tag must still cut",
        );
    }

    /// Every skip reason must reach `verify-release`, or the gate goes
    /// looking for a release that was never created and 404s.
    #[test]
    fn a_skipped_crate_is_recorded_for_the_verify_gate() {
        use anodizer_core::stage::Stage;

        let mut ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .crates(vec![anodizer_core::config::CrateConfig {
                name: "demo".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ Version }}".to_string()),
                release: Some(ReleaseConfig::default()),
                ..Default::default()
            }])
            .build();
        ctx.options.nightly = true;
        ctx.config.nightly = Some(NightlyConfig {
            skip_if_no_changes: Some(true),
            ..Default::default()
        });
        ctx.stage_outputs.release_body_range = Some(range(0, Some("v0.9.0")));
        crate::ReleaseStage.run(&mut ctx).expect("stage run ok");
        assert_eq!(
            ctx.stage_outputs.release_skipped_crates,
            vec!["demo".to_string()],
        );
        assert!(
            ctx.stage_outputs.release_stage_ran,
            "the stage must mark itself run so the publisher does not repeat it"
        );
    }

    // ---- nightly.name_template / nightly.tag_name ----------------------

    fn nightly_named_ctx(name: Option<&str>, tag: Option<&str>) -> anodizer_core::context::Context {
        let mut ctx = TestContextBuilder::new()
            .project_name("widget")
            .tag("v1.2.3")
            .build();
        ctx.options.nightly = true;
        ctx.config.nightly = Some(NightlyConfig {
            name_template: name.map(str::to_string),
            tag_name: tag.map(str::to_string),
            ..Default::default()
        });
        ctx
    }

    #[test]
    fn nightly_name_template_wins_over_release_name_template() {
        let ctx = nightly_named_ctx(Some("{{ ProjectName }} nightly"), None);
        let release_cfg = ReleaseConfig {
            name_template: Some("{{ ProjectName }} {{ Tag }}".to_string()),
            ..Default::default()
        };
        assert_eq!(
            resolve_release_name(&ctx, &release_cfg, "demo").expect("render ok"),
            "widget nightly"
        );
    }

    #[test]
    fn nightly_name_template_is_ignored_on_a_stable_run() {
        let mut ctx = nightly_named_ctx(Some("{{ ProjectName }} nightly"), None);
        ctx.options.nightly = false;
        let release_cfg = ReleaseConfig {
            name_template: Some("{{ ProjectName }} {{ Tag }}".to_string()),
            ..Default::default()
        };
        assert_eq!(
            resolve_release_name(&ctx, &release_cfg, "demo").expect("render ok"),
            "widget v1.2.3"
        );
    }

    /// Single-family workspace: the rendered value IS the tag.
    #[test]
    fn nightly_tag_name_is_taken_verbatim_in_a_single_family_workspace() {
        let mut ctx = nightly_named_ctx(None, Some("edge"));
        ctx.config.crates = vec![anodizer_core::config::CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ Version }}".to_string()),
            ..Default::default()
        }];
        let crate_cfg = ctx.config.crates[0].clone();
        assert_eq!(
            resolve_release_tag(&ctx, &crate_cfg, None).expect("render ok"),
            "edge"
        );
    }

    /// Per-crate workspace: one literal tag cannot carry three tracks, so
    /// the value is prefixed with each crate's own family.
    #[test]
    fn nightly_tag_name_is_prefixed_per_family_in_a_per_crate_workspace() {
        let mut ctx = nightly_named_ctx(None, Some("edge"));
        let track = |name: &str, tmpl: &str| anodizer_core::config::CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: Some(tmpl.to_string()),
            ..Default::default()
        };
        ctx.config.crates = vec![
            track("app", "v{{ Version }}"),
            track("operator", "operator-v{{ Version }}"),
        ];
        assert_eq!(
            resolve_release_tag(&ctx, &ctx.config.crates[0].clone(), None).expect("render ok"),
            "vedge",
            "the bare `v` track's tag must land inside the `v` family"
        );
        assert_eq!(
            resolve_release_tag(&ctx, &ctx.config.crates[1].clone(), None).expect("render ok"),
            "operator-vedge",
            "the operator track's tag must land inside the operator family"
        );
    }

    /// The tag template is rendered by the release stage, after the nightly
    /// `Version` rewrite, so `{{ Version }}` inside `nightly.tag_name`
    /// resolves to the nightly version.
    #[test]
    fn nightly_tag_name_renders_the_nightly_version() {
        let mut ctx = nightly_named_ctx(None, Some("nightly-{{ Version }}"));
        ctx.template_vars_mut()
            .set("Version", "1.2.4-abc123d-nightly");
        let crate_cfg = anodizer_core::config::CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ Version }}".to_string()),
            ..Default::default()
        };
        assert_eq!(
            resolve_release_tag(&ctx, &crate_cfg, None).expect("render ok"),
            "nightly-1.2.4-abc123d-nightly"
        );
    }

    /// `{{ if .IsNightly }}` branches inside `tag_name` still work: the flag
    /// is set before any stage runs.
    #[test]
    fn nightly_tag_name_can_branch_on_is_nightly() {
        let mut ctx = nightly_named_ctx(
            None,
            Some("{{ if .IsNightly }}edge{{ else }}stable{{ end }}"),
        );
        ctx.template_vars_mut().set_bool("IsNightly", true);
        let crate_cfg = anodizer_core::config::CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ Version }}".to_string()),
            ..Default::default()
        };
        assert_eq!(
            resolve_release_tag(&ctx, &crate_cfg, None).expect("render ok"),
            "edge"
        );
    }

    /// A `monorepo.tag_prefix` namespace is part of the family, so a literal
    /// `nightly.tag_name` must land inside it too — the same scope the
    /// retention matcher tests membership against.
    #[test]
    fn nightly_tag_name_is_prefixed_with_the_monorepo_namespace() {
        let mut ctx = nightly_named_ctx(None, Some("edge"));
        let track = |name: &str, tmpl: &str| anodizer_core::config::CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: Some(tmpl.to_string()),
            ..Default::default()
        };
        ctx.config.crates = vec![
            track("app", "{{ Version }}"),
            track("operator", "operator-v{{ Version }}"),
        ];
        ctx.config.monorepo = Some(anodizer_core::config::MonorepoConfig {
            tag_prefix: Some("sub/".to_string()),
            ..Default::default()
        });
        assert_eq!(
            resolve_release_tag(&ctx, &ctx.config.crates[0].clone(), None).expect("render ok"),
            "sub/edge",
            "a bare-version family under a monorepo namespace IS that namespace"
        );
    }

    #[test]
    fn nightly_tag_name_absent_leaves_the_tag_template_to_mint_the_tag() {
        let ctx = nightly_named_ctx(None, None);
        let crate_cfg = anodizer_core::config::CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ Version }}".to_string()),
            ..Default::default()
        };
        assert_eq!(
            resolve_release_tag(&ctx, &crate_cfg, None).expect("render ok"),
            "v1.2.3"
        );
    }

    #[test]
    fn nightly_tag_name_is_ignored_on_a_stable_run() {
        let mut ctx = nightly_named_ctx(None, Some("edge"));
        ctx.options.nightly = false;
        let crate_cfg = anodizer_core::config::CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ Version }}".to_string()),
            ..Default::default()
        };
        assert_eq!(
            resolve_release_tag(&ctx, &crate_cfg, None).expect("render ok"),
            "v1.2.3"
        );
    }

    /// `nightly.tag_name` is the narrower knob: on a nightly it wins over a
    /// `release.tag` override that applies to every run.
    #[test]
    fn nightly_tag_name_wins_over_a_release_tag_override() {
        let ctx = nightly_named_ctx(None, Some("edge"));
        let crate_cfg = anodizer_core::config::CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ Version }}".to_string()),
            ..Default::default()
        };
        assert_eq!(
            resolve_release_tag(&ctx, &crate_cfg, Some("pinned")).expect("render ok"),
            "edge"
        );
    }

    /// The REST API rejects an empty `tag_name`; a template that renders to
    /// nothing must fail loudly rather than 422 mid-publish.
    #[test]
    fn nightly_tag_name_rendering_empty_is_an_error() {
        let ctx = nightly_named_ctx(
            None,
            Some("{{ Env.ANODIZER_NO_SUCH_VAR | default(value='') }}"),
        );
        let crate_cfg = anodizer_core::config::CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ Version }}".to_string()),
            ..Default::default()
        };
        let err = resolve_release_tag(&ctx, &crate_cfg, None)
            .expect_err("an empty rendered tag must not reach the API");
        assert!(err.to_string().contains("empty tag"), "got: {err}");
        assert!(
            err.to_string().contains("nightly.tag_name"),
            "the bail must name the knob that rendered empty: {err}"
        );
    }

    /// Each crate's release body must render ITS tag. The body is composed
    /// from the same template vars the anchor writes, so composing before the
    /// anchor left every crate after the first titled with its predecessor's
    /// tag.
    #[test]
    fn per_crate_release_body_renders_its_own_tag() {
        use anodizer_core::config::{ContentSource, ReleaseConfig};

        fn track(name: &str, prefix: &str) -> anodizer_core::config::CrateConfig {
            anodizer_core::config::CrateConfig {
                name: name.to_string(),
                path: ".".to_string(),
                tag_template: Some(format!("{prefix}-v{{{{ Version }}}}")),
                release: Some(ReleaseConfig {
                    header: Some(ContentSource::Inline("header {{ Tag }}".to_string())),
                    footer: Some(ContentSource::Inline("footer {{ Tag }}".to_string())),
                    ..Default::default()
                }),
                ..Default::default()
            }
        }

        let capture = anodizer_core::log::LogCapture::new();
        let mut ctx = TestContextBuilder::new()
            .dry_run(true)
            .crates(vec![track("app", "app"), track("operator", "operator")])
            .build();
        ctx.with_log_capture(capture.clone());

        crate::ReleaseStage
            .run(&mut ctx)
            .expect("dry-run release over two tag families");

        let notes: Vec<String> = capture
            .all_messages()
            .into_iter()
            .filter(|(_, m)| m.contains("(dry-run) release notes"))
            .map(|(_, m)| m)
            .collect();
        assert_eq!(notes.len(), 2, "one notes block per crate, got: {notes:?}");
        assert!(
            notes[0].contains("header app-v1.2.3") && notes[0].contains("footer app-v1.2.3"),
            "app's body must render app's tag: {}",
            notes[0]
        );
        assert!(
            notes[1].contains("header operator-v1.2.3")
                && notes[1].contains("footer operator-v1.2.3"),
            "operator's body must render operator's tag: {}",
            notes[1]
        );
        assert!(
            !notes[1].contains("app-v1.2.3"),
            "operator's body must not carry the previous crate's tag: {}",
            notes[1]
        );
    }
}
