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
        let dry_run = ctx.is_dry_run();

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

/// Validate release flag combinations that are mutually exclusive and would
/// produce undefined behavior if both are set.
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

    let release_name = resolve_release_name(ctx, release_cfg, &crate_cfg.name)?;

    let flags = resolve_release_flags(ctx, release_cfg, &crate_name, &tag, log)?;
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
                keep_single_release: flags.keep_single_release,
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
            "release: release.tag override '{}' differs from pushed git tag '{}' (crate '{}') — GitHub will create a new tag at the target commit",
            tag, pushed_tag, crate_name
        ));
    }
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
/// Composition order (matches GoReleaser `internal/pipe/release/release.go`):
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
/// 5. `include_meta: true` appends `metadata.json` (GoReleaser
///    `release.go:170-172` — only the Metadata kind, not anodizer's
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
                "release: ids filter {:?} matched zero artifacts for crate '{}' \
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

    // GoReleaser release.go:121 — refresh combined checksum files before
    // upload so they include signatures/artifacts added after the checksum
    // stage ran. Mirrors GoReleaser's ExtraRefresh hook.
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
                "include_meta: {} not found at {}",
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
/// the changelog stage in `ctx.stage_outputs.changelog_header`). GoReleaser
/// only has the `release.*` source (loaded via `loadContent(ReleaseHeader…)`
/// in `internal/pipe/changelog/changelog.go`); anodizer extends that to a
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
/// `true` / `false` / `auto` / `1` / `0` / "". `auto` mirrors GoReleaser:
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
    /// Nightly: delete the existing release that points at the same tag
    /// before creating the new one (GoReleaser `nightly.keep_single_release`).
    /// Only honored on `--nightly` runs; ignored otherwise.
    keep_single_release: bool,
}

/// Resolve all release flags from config + CLI overrides for one crate.
fn resolve_release_flags(
    ctx: &Context,
    release_cfg: &anodizer_core::config::ReleaseConfig,
    crate_name: &str,
    tag: &str,
    log: &anodizer_core::log::StageLogger,
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
    // and `nightly.keep_single_release` (only meaningful when `is_nightly()`).
    let nightly_cfg = ctx.config.nightly.as_ref();
    let draft = if ctx.is_nightly()
        && let Some(d) = nightly_cfg.and_then(|n| n.draft)
    {
        d
    } else {
        release_cfg.resolved_draft()
    };
    let keep_single_release = ctx.is_nightly()
        && nightly_cfg
            .and_then(|n| n.keep_single_release)
            .unwrap_or(false);
    if ctx.is_nightly() && draft && keep_single_release {
        log.warn(
            "release: nightly with both draft=true and keep_single_release=true \
             — no published nightly release will exist (each run replaces a prior draft)",
        );
    }
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
        keep_single_release,
    })
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
                keep_single_release: flags.keep_single_release,
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
    keep_single_release: bool,
    artifact_entries: &'a [(std::path::PathBuf, Option<String>)],
}

/// Resolve the dry-run download-base URL for the active SCM provider.
///
/// Falls back to the public default for each provider when no override is
/// configured. For Gitea, the download base is additionally derived from the
/// API URL by stripping the `/api/v1` suffix to mirror GoReleaser's
/// `defaults.go:29-36`.
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
    if s.keep_single_release {
        log.status(&format!(
            "(dry-run)   would delete existing release at tag '{}' before recreating (nightly.keep_single_release)",
            s.tag,
        ));
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
/// release. Mirrors GoReleaser's `release.go` upload set:
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
pub(crate) fn collect_release_upload_candidates(
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
        let flags = resolve_release_flags(&ctx, &release_cfg, "demo", "nightly", &quiet_log())
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
        let flags = resolve_release_flags(&ctx, &release_cfg, "demo", "nightly", &quiet_log())
            .expect("resolve_release_flags returns Ok");
        assert!(
            flags.draft,
            "nightly.draft=None must fall through to release.draft=true"
        );
    }

    #[test]
    fn resolve_release_flags_keep_single_release_ignored_when_not_nightly() {
        // nightly.keep_single_release = Some(true) must be ignored outside nightly runs.
        let mut ctx = TestContextBuilder::new().tag("v0.0.0-test").build();
        // options.nightly defaults to false
        ctx.config.nightly = Some(NightlyConfig {
            keep_single_release: Some(true),
            ..Default::default()
        });
        let release_cfg = ReleaseConfig::default();
        let flags = resolve_release_flags(&ctx, &release_cfg, "demo", "v1.0.0", &quiet_log())
            .expect("resolve_release_flags returns Ok");
        assert!(
            !flags.keep_single_release,
            "keep_single_release must be false outside nightly runs"
        );
    }

    #[test]
    fn resolve_release_flags_keep_single_release_honored_when_nightly() {
        // nightly.keep_single_release = Some(true) must propagate to flags when is_nightly().
        let mut ctx = TestContextBuilder::new().tag("v0.0.0-test").build();
        ctx.options.nightly = true;
        ctx.config.nightly = Some(NightlyConfig {
            keep_single_release: Some(true),
            ..Default::default()
        });
        let release_cfg = ReleaseConfig::default();
        let flags = resolve_release_flags(&ctx, &release_cfg, "demo", "nightly", &quiet_log())
            .expect("resolve_release_flags returns Ok");
        assert!(
            flags.keep_single_release,
            "keep_single_release must be true when nightly and nightly.keep_single_release=Some(true)"
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
    fn resolve_release_flags_warns_when_nightly_draft_and_keep_single_release() {
        // draft=true + keep_single_release=true on a nightly run is a GoReleaser-
        // documented gotcha: no published release ever exists because each run
        // replaces a prior draft. Must succeed (warn, not error).
        let mut ctx = TestContextBuilder::new().tag("v0.0.0-test").build();
        ctx.options.nightly = true;
        ctx.config.nightly = Some(NightlyConfig {
            draft: Some(true),
            keep_single_release: Some(true),
            ..Default::default()
        });
        let release_cfg = ReleaseConfig::default();
        // Must succeed (warn, not error).
        let flags = resolve_release_flags(&ctx, &release_cfg, "demo", "nightly", &quiet_log())
            .expect("nightly + draft + keep_single_release must not error");
        assert!(flags.draft, "draft must be true");
        assert!(
            flags.keep_single_release,
            "keep_single_release must be true"
        );
    }
}
