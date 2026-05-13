use std::sync::Arc;

use anodizer_core::artifact::{ArtifactKind, matches_id_filter};
use anodizer_core::context::Context;
use anodizer_core::scm::ScmTokenType;
use anodizer_core::stage::Stage;
use anyhow::{Context as _, Result, bail};

use crate::release_body::{
    build_release_body, collect_extra_files, resolve_content_source, resolve_header_footer,
    resolve_make_latest, resolve_release_tag,
};
use crate::{
    compose_release_url, gitea, github, gitlab, populate_artifact_download_urls,
    resolve_release_repo, retry_upload, should_mark_prerelease,
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

            // Skip crates where release is explicitly disabled (supports template strings).
            if let Some(ref d) = release_cfg.skip {
                let off = d
                    .try_evaluates_to_true(|s| ctx.render_template(s))
                    .with_context(|| {
                        format!(
                            "release: render skip template for crate '{}'",
                            crate_cfg.name
                        )
                    })?;
                if off {
                    log.status(&format!("release skipped for crate '{}'", crate_cfg.name));
                    continue;
                }
            }

            let crate_name = crate_cfg.name.clone();

            // Validate conflicting draft options.
            if release_cfg.resolved_replace_existing_draft()
                && release_cfg.resolved_use_existing_draft()
            {
                bail!(
                    "release: crate '{}': cannot set both replace_existing_draft and \
                     use_existing_draft — replace deletes drafts that use_existing_draft needs",
                    crate_name
                );
            }

            let changelog_body = ctx
                .stage_outputs
                .changelogs
                .get(&crate_name)
                .cloned()
                .unwrap_or_default();

            // Populate the {{ Checksums }} template variable from checksum
            // artifacts. See `populate_checksums_var` for the workspace
            // aggregation rules (combined-mode union vs split-mode map).
            crate::populate_checksums_var(ctx);

            // Resolve and validate release mode.
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

            // Refresh Artifacts template var so release body templates can iterate artifacts.
            ctx.refresh_artifacts_var();

            // Resolve and template-render header/footer before building release body.
            // resolve_content_source now template-renders the from_file path and the
            // from_url URL + header values internally; body still rendered here.
            //
            // Anodizer-local precedence: `release.header` is the more
            // specific override and wins; `changelog.header` (rendered and
            // stashed by the changelog stage in `ctx.stage_outputs.changelog_header`) is the
            // fallback so a YAML-configured changelog wrapper still reaches
            // the release body. Same for the footer. GoReleaser only has the
            // `release.*` source (loaded via `loadContent(ReleaseHeader…)` in
            // `internal/pipe/changelog/changelog.go`); we extend that to a
            // second source as a Rust-first ergonomic. See
            // `release_body::resolve_header_footer` for the precedence helper.
            let release_header = release_cfg
                .header
                .as_ref()
                .map(|src| {
                    let raw = resolve_content_source(src, ctx).with_context(|| {
                        format!("release: resolve header for crate '{}'", crate_name)
                    })?;
                    ctx.render_template(&raw).with_context(|| {
                        format!("release: render header for crate '{}'", crate_name)
                    })
                })
                .transpose()?;
            let release_footer = release_cfg
                .footer
                .as_ref()
                .map(|src| {
                    let raw = resolve_content_source(src, ctx).with_context(|| {
                        format!("release: resolve footer for crate '{}'", crate_name)
                    })?;
                    ctx.render_template(&raw).with_context(|| {
                        format!("release: render footer for crate '{}'", crate_name)
                    })
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

            let release_body = build_release_body(
                &changelog_body,
                rendered_header.as_deref(),
                rendered_footer.as_deref(),
            );

            // Resolve tag: use release.tag override if set, otherwise tag_template.
            let tag = resolve_release_tag(
                ctx,
                &crate_cfg.tag_template,
                release_cfg.tag.as_deref(),
                &crate_cfg.name,
            )?;

            // Warn loudly when `release.tag` resolves to something other than
            // the pushed git tag: GitHub will auto-create the override tag at
            // target_commitish, diverging from the source-of-truth tag that
            // triggered the release.
            if release_cfg.tag.is_some()
                && let Some(pushed_tag) = ctx.template_vars().get("Tag")
                && !pushed_tag.is_empty()
                && pushed_tag != tag.as_str()
            {
                log.warn(&format!(
                    "release: release.tag override '{}' differs from pushed git tag '{}' (crate '{}') — GitHub will create a new tag at the target commit",
                    tag, pushed_tag, crate_cfg.name
                ));
            }

            // Resolve release name. GoReleaser defaults to `"{{.Tag}}"` (Go
            // template); anodizer's renderer expects Tera syntax so the default
            // is the equivalent `"{{ Tag }}"`. The two render to the same
            // string for any user-supplied template; this only affects the
            // surface form (no leading dot) when introspecting the default.
            let name_tmpl = release_cfg.resolved_name_template();
            let release_name = ctx.render_template(name_tmpl).with_context(|| {
                format!(
                    "release: render name_template for crate '{}'",
                    crate_cfg.name
                )
            })?;

            let draft = release_cfg.resolved_draft();
            let prerelease = should_mark_prerelease(&release_cfg.prerelease, &tag);
            let skip_upload = match release_cfg.skip_upload.as_ref() {
                Some(s) => {
                    // Template-render the value first (supports {{ .IsSnapshot }}, etc.)
                    let rendered = if s.is_template() {
                        ctx.render_template(s.as_str()).with_context(|| {
                            format!(
                                "release: render skip_upload template '{}' for crate '{}'",
                                s.as_str(),
                                crate_cfg.name
                            )
                        })?
                    } else {
                        s.as_str().to_string()
                    };
                    match rendered.trim() {
                        "auto" => ctx.is_snapshot(),
                        "true" | "1" => true,
                        "false" | "0" | "" => false,
                        other => bail!(
                            "release: invalid skip_upload value '{}' for crate '{}' \
                             (expected one of: true/false/auto/1/0, or a template that renders to one of those)",
                            other,
                            crate_cfg.name
                        ),
                    }
                }
                None => false,
            };
            let replace_existing_draft = release_cfg.resolved_replace_existing_draft();
            let replace_existing_artifacts = release_cfg.resolved_replace_existing_artifacts();
            let make_latest =
                resolve_make_latest(&release_cfg.make_latest, |s| ctx.render_template(s))?;
            let ids_filter = release_cfg.ids.as_ref();
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
            let discussion_category_name = release_cfg.discussion_category_name.clone();
            let include_meta = release_cfg.resolved_include_meta();
            let use_existing_draft = release_cfg.resolved_use_existing_draft();

            // Collect uploadable artifacts for this crate, applying ids filter.
            // Each entry is (path, optional_custom_name). The custom name is
            // only set for extra_files with a name_template; regular artifacts
            // use None.
            //
            // Source-of-truth is `release_uploadable_kinds()`. It mirrors
            // GoReleaser's `artifact.ReleaseUploadableTypes()` (called from
            // `internal/pipe/release/release.go`) and additionally includes
            // the four GR-Pro installer kinds anodizer ships as OSS:
            // Installer (MSI/NSIS), DiskImage (DMG), and MacOsPackage (PKG).
            // Snap remains excluded — snaps publish to the snap store.
            //
            // When `IncludeMeta` is true the Metadata kind is appended, per
            // `release.go:160-167`.
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

            // GoReleaser release.go:121 — refresh combined checksum files
            // before upload so they include signatures/artifacts added after
            // the checksum stage ran. Mirrors GoReleaser's ExtraRefresh hook.
            anodizer_stage_checksum::refresh_combined_checksums(ctx, dry_run)?;

            // Collect extra files from glob patterns (with optional name_template).
            if let Some(extra_specs) = &release_cfg.extra_files {
                let extra = collect_extra_files(extra_specs, ctx)?;
                artifact_entries.extend(extra);
            }

            // Process templated_extra_files: render template contents and write to dist dir.
            // NOTE: Rendered files are written to the shared dist directory. If multiple
            // release configs use the same dst name, later writes will overwrite earlier
            // ones. Users should ensure dst names are unique across configs.
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

            // include_meta: upload metadata.json from dist dir.
            //
            // Matches GoReleaser `internal/pipe/release/release.go:170-172`:
            // `IncludeMeta` appends ONLY `artifact.Metadata` (the
            // `metadata.json` file). `artifacts.json` is anodizer's local
            // dist manifest and is not part of the GR uploadable surface;
            // uploading it as a release asset diverges from GR and surprises
            // downstream tooling that expects exactly one extra file under
            // `include_meta: true`.
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

            if dry_run {
                let backend_label = match ctx.token_type {
                    ScmTokenType::GitLab => "GitLab",
                    ScmTokenType::Gitea => "Gitea",
                    ScmTokenType::GitHub => "GitHub",
                };

                // Log platform-specific URLs when configured.
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
                                log.status(&format!(
                                    "(dry-run)   github_urls.download = {}",
                                    download
                                ));
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
                                log.status(&format!(
                                    "(dry-run)   gitlab_urls.download = {}",
                                    download
                                ));
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
                                log.status(&format!(
                                    "(dry-run)   gitea_urls.download = {}",
                                    download
                                ));
                            }
                            if urls.skip_tls_verify.unwrap_or(false) {
                                log.status("(dry-run)   gitea_urls.skip_tls_verify = true");
                            }
                        }
                    }
                }

                log.status(&format!(
                    "(dry-run) would create {} Release '{}' (tag={}, draft={}, prerelease={}, mode={}) for crate '{}'",
                    backend_label, release_name, tag, draft, prerelease, release_mode, crate_cfg.name
                ));
                if skip_upload {
                    log.status("(dry-run)   skip_upload is set, would skip artifact uploads");
                } else {
                    for (path, custom_name) in &artifact_entries {
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

                // Even in dry-run, populate artifact download URLs so publishers
                // can generate manifests with correct URLs.
                let dry_dl_base = match ctx.token_type {
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
                    ScmTokenType::Gitea => {
                        ctx.config
                            .gitea_urls
                            .as_ref()
                            .and_then(|u| u.download.clone())
                            .unwrap_or_else(|| {
                                // Derive download URL from API URL by stripping
                                // /api/v1 suffix (GoReleaser defaults.go:29-36).
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
                            })
                    }
                };
                let dry_repo_cfg = resolve_release_repo(release_cfg, ctx.token_type, ctx)?;
                let (dry_owner, dry_repo) = dry_repo_cfg
                    .as_ref()
                    .map(|r| (r.owner.as_str(), r.name.as_str()))
                    .unwrap_or(("", ""));
                populate_artifact_download_urls(
                    ctx,
                    &crate_name,
                    ctx.token_type,
                    &dry_dl_base,
                    dry_owner,
                    dry_repo,
                    &tag,
                );
                if !dry_owner.is_empty() && !dry_repo.is_empty() {
                    let dry_release_url = compose_release_url(
                        ctx.token_type,
                        &dry_dl_base,
                        dry_owner,
                        dry_repo,
                        &tag,
                    );
                    ctx.set_release_url(&dry_release_url);
                }

                continue;
            }

            // ---------------------------------------------------------------
            // Backend dispatch: GitHub, GitLab, or Gitea
            // ---------------------------------------------------------------
            // Each backend arm returns (release_html_url, download_base, owner, repo)
            // so we can populate artifact metadata["url"] after the match.
            let (release_url, download_base, repo_owner, repo_name) = match ctx.token_type {
                // ===============================================================
                // GitLab backend
                // ===============================================================
                ScmTokenType::GitLab => {
                    let repo_cfg = match resolve_release_repo(release_cfg, ctx.token_type, ctx)? {
                        Some(r) => r,
                        None => {
                            log.warn(&format!(
                                "no gitlab config for crate '{}', skipping",
                                crate_cfg.name
                            ));
                            continue;
                        }
                    };

                    let token_str = match &token {
                        Some(t) => t.clone(),
                        None => {
                            bail!(
                                "release: no GitLab token available (set GITLAB_TOKEN, or pass --token)"
                            );
                        }
                    };

                    let gitlab_urls = ctx.config.gitlab_urls.clone().unwrap_or_default();
                    let api_url = gitlab_urls
                        .api
                        .unwrap_or_else(|| "https://gitlab.com/api/v4".to_string());
                    let download_url = gitlab_urls
                        .download
                        .unwrap_or_else(|| "https://gitlab.com".to_string());
                    let skip_tls = gitlab_urls.skip_tls_verify.unwrap_or(false);
                    // Match GoReleaser's `checkUseJobToken`: only send JOB-TOKEN
                    // when CI_JOB_TOKEN is set, the flag is on, and the token
                    // equals CI_JOB_TOKEN. Otherwise fall back to PRIVATE-TOKEN.
                    let use_job_token = gitlab::resolve_use_job_token(
                        gitlab_urls.use_job_token.unwrap_or(false),
                        &token_str,
                    );
                    let use_pkg_registry =
                        gitlab_urls.use_package_registry.unwrap_or(false) || use_job_token;

                    let project_id = gitlab::gitlab_project_id(&repo_cfg.owner, &repo_cfg.name);
                    let commit_sha = ctx
                        .git_info
                        .as_ref()
                        .map(|g| g.commit.clone())
                        .unwrap_or_default();

                    let project_name_for_pkg = ctx.config.project_name.clone();
                    let version_for_pkg = ctx
                        .git_info
                        .as_ref()
                        .map(|g| {
                            // Strip leading 'v' for package version (e.g. "v1.2.3" -> "1.2.3").
                            g.tag.strip_prefix('v').unwrap_or(&g.tag).to_string()
                        })
                        .unwrap_or_else(|| "0.0.0".to_string());

                    // GitLab does not support draft releases — warn if draft options are set.
                    if replace_existing_draft {
                        log.warn("replace_existing_draft has no effect on GitLab (draft releases are not supported)");
                    }
                    if use_existing_draft {
                        log.warn("use_existing_draft has no effect on GitLab (draft releases are not supported)");
                    }

                    // Per-publisher retry policy (Wave-1 RetryConfig). 5xx /
                    // 429 / network errors retry with exponential backoff
                    // through `retry_http_async` inside every gitlab_*
                    // function. Default: 10 attempts × 10s base × 5m cap
                    // (matches GoReleaser `pkg/config.Retry` defaults).
                    let policy = ctx.retry_policy();

                    let url = rt.block_on(async {
                        let client =
                            gitlab::build_gitlab_client(&token_str, skip_tls, use_job_token)?;

                        let gitlab_ctx = gitlab::GitlabCtx {
                            client: &client,
                            api_url: &api_url,
                            project_id: &project_id,
                            policy: &policy,
                        };

                        // Create or update the release.
                        gitlab::gitlab_create_release(
                            &gitlab_ctx,
                            &gitlab::GitlabReleaseSpec {
                                tag: &tag,
                                name: &release_name,
                                body: &release_body,
                                commit: &commit_sha,
                                release_mode: &release_mode,
                            },
                        )
                        .await?;

                        log.status(&format!(
                            "created GitLab Release '{}' (tag={}) on {}",
                            release_name, tag, project_id
                        ));

                        // Upload artifacts with bounded parallelism (matching GitHub path).
                        if skip_upload {
                            log.status("skip_upload is set, skipping artifact uploads");
                        } else {
                            let upload_parallelism = std::cmp::max(ctx.options.parallelism, 1);
                            let semaphore =
                                Arc::new(tokio::sync::Semaphore::new(upload_parallelism));

                            // Prepare the list of uploadable entries (error on missing files).
                            let mut missing_files = Vec::new();
                            let prepared_entries: Vec<(std::path::PathBuf, String)> =
                                artifact_entries
                                    .iter()
                                    .filter_map(|(path, custom_name)| {
                                        if !path.exists() {
                                            missing_files.push(path.display().to_string());
                                            return None;
                                        }
                                        let file_name = if let Some(name) = custom_name {
                                            name.clone()
                                        } else {
                                            path.file_name()
                                                .map(|n| n.to_string_lossy().into_owned())
                                                .unwrap_or_else(|| "artifact".to_string())
                                        };
                                        Some((path.clone(), file_name))
                                    })
                                    .collect();

                            if !missing_files.is_empty() {
                                anyhow::bail!(
                                    "the following artifact files are missing:\n  {}",
                                    missing_files.join("\n  ")
                                );
                            }

                            let client = Arc::new(client);
                            let mut join_set = tokio::task::JoinSet::new();

                            for (path, file_name) in prepared_entries {
                                let sem = semaphore.clone();
                                let client = client.clone();
                                let api_url = api_url.clone();
                                let project_id = project_id.clone();
                                let tag = tag.clone();
                                let project_name_for_pkg = project_name_for_pkg.clone();
                                let version_for_pkg = version_for_pkg.clone();
                                let download_url = download_url.clone();
                                let policy_inner = policy;

                                join_set.spawn(async move {
                                    let _permit = sem
                                        .acquire()
                                        .await
                                        .map_err(|e| anyhow::anyhow!("semaphore closed: {}", e))?;

                                    let op_name = format!("gitlab: upload '{}'", file_name);
                                    let ctx = gitlab::GitlabCtx {
                                        client: &client,
                                        api_url: &api_url,
                                        project_id: &project_id,
                                        policy: &policy_inner,
                                    };
                                    let asset = gitlab::GitlabAssetSpec {
                                        file_path: &path,
                                        file_name: &file_name,
                                    };
                                    let pkg_spec = gitlab::GitlabPackageRegistrySpec {
                                        project_name: &project_name_for_pkg,
                                        version: &version_for_pkg,
                                    };
                                    let pkg = use_pkg_registry.then_some(&pkg_spec);
                                    retry_upload(&op_name, || {
                                        gitlab::gitlab_upload_asset(
                                            &ctx,
                                            &tag,
                                            &asset,
                                            pkg,
                                            &download_url,
                                            replace_existing_artifacts,
                                        )
                                    })
                                    .await
                                    .with_context(|| {
                                        format!(
                                            "release: upload artifact '{}' to GitLab release '{}'",
                                            file_name, tag
                                        )
                                    })?;

                                    Ok::<String, anyhow::Error>(file_name)
                                });
                            }

                            while let Some(result) = join_set.join_next().await {
                                let file_name = result
                                    .context("gitlab: upload task panicked")?
                                    .context("gitlab: upload task failed")?;
                                log.verbose(&format!("uploaded artifact: {}", file_name));
                            }
                        }

                        // GitLab does not support draft releases — publish is a no-op.

                        let html_url = gitlab::gitlab_release_url(
                            &download_url,
                            &repo_cfg.owner,
                            &repo_cfg.name,
                            &tag,
                        );
                        Ok::<String, anyhow::Error>(html_url)
                    })?;

                    (
                        url,
                        download_url,
                        repo_cfg.owner.clone(),
                        repo_cfg.name.clone(),
                    )
                }

                // ===============================================================
                // Gitea backend
                // ===============================================================
                ScmTokenType::Gitea => {
                    let repo_cfg = match resolve_release_repo(release_cfg, ctx.token_type, ctx)? {
                        Some(r) => r,
                        None => {
                            log.warn(&format!(
                                "no gitea config for crate '{}', skipping",
                                crate_cfg.name
                            ));
                            continue;
                        }
                    };

                    let token_str = match &token {
                        Some(t) => t.clone(),
                        None => {
                            bail!(
                                "release: no Gitea token available (set GITEA_TOKEN, or pass --token)"
                            );
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
                    if replace_existing_draft {
                        log.warn("replace_existing_draft has no effect on Gitea (draft support is limited)");
                    }
                    if use_existing_draft {
                        log.warn(
                            "use_existing_draft has no effect on Gitea (draft support is limited)",
                        );
                    }

                    // Per-publisher retry policy (Wave-1 RetryConfig). Same
                    // shape and rationale as the GitLab branch above.
                    let policy = ctx.retry_policy();

                    let url = rt.block_on(async {
                        let client = gitea::build_gitea_client(&token_str, skip_tls)?;

                        let gitea_ctx = gitea::GiteaCtx {
                            client: &client,
                            api_url: &api_url,
                            owner: &repo_cfg.owner,
                            repo: &repo_cfg.name,
                            policy: &policy,
                        };

                        // Create or update the release.
                        let release_id = gitea::gitea_create_release(
                            &gitea_ctx,
                            &gitea::GiteaReleaseSpec {
                                tag: &tag,
                                commit: &commit_sha,
                                name: &release_name,
                                body: &release_body,
                                draft,
                                prerelease,
                                release_mode: &release_mode,
                            },
                        )
                        .await?;

                        log.status(&format!(
                            "created Gitea Release '{}' (id={}, tag={}) on {}/{}",
                            release_name, release_id, tag, repo_cfg.owner, repo_cfg.name
                        ));

                        // Upload artifacts with bounded parallelism (matching GitLab pattern).
                        if skip_upload {
                            log.status("skip_upload is set, skipping artifact uploads");
                        } else {
                            let upload_parallelism = std::cmp::max(ctx.options.parallelism, 1);
                            let semaphore =
                                Arc::new(tokio::sync::Semaphore::new(upload_parallelism));

                            // Prepare the list of uploadable entries (error on missing files).
                            let mut missing_files = Vec::new();
                            let prepared_entries: Vec<(std::path::PathBuf, String)> =
                                artifact_entries
                                    .iter()
                                    .filter_map(|(path, custom_name)| {
                                        if !path.exists() {
                                            missing_files.push(path.display().to_string());
                                            return None;
                                        }
                                        let file_name = if let Some(name) = custom_name {
                                            name.clone()
                                        } else {
                                            path.file_name()
                                                .map(|n| n.to_string_lossy().into_owned())
                                                .unwrap_or_else(|| "artifact".to_string())
                                        };
                                        Some((path.clone(), file_name))
                                    })
                                    .collect();

                            if !missing_files.is_empty() {
                                anyhow::bail!(
                                    "the following artifact files are missing:\n  {}",
                                    missing_files.join("\n  ")
                                );
                            }

                            let client = Arc::new(client);
                            let mut join_set = tokio::task::JoinSet::new();

                            for (path, file_name) in prepared_entries {
                                let sem = semaphore.clone();
                                let client = client.clone();
                                let api_url = api_url.clone();
                                let owner = repo_cfg.owner.clone();
                                let repo = repo_cfg.name.clone();
                                let tag = tag.clone();
                                let policy_inner = policy;

                                join_set.spawn(async move {
                                    let _permit = sem
                                        .acquire()
                                        .await
                                        .map_err(|e| anyhow::anyhow!("semaphore closed: {}", e))?;

                                    let ctx = gitea::GiteaCtx {
                                        client: &client,
                                        api_url: &api_url,
                                        owner: &owner,
                                        repo: &repo,
                                        policy: &policy_inner,
                                    };

                                    // Handle replace_existing_artifacts: if an asset with the
                                    // same name exists, delete it before uploading.
                                    if replace_existing_artifacts {
                                        gitea::gitea_delete_asset_by_name(
                                            &ctx,
                                            release_id,
                                            &file_name,
                                        )
                                        .await
                                        .with_context(|| {
                                            format!(
                                                "gitea: delete existing asset '{}' from release {}",
                                                file_name, release_id
                                            )
                                        })?;
                                    }

                                    let op_name = format!("gitea: upload '{}'", file_name);
                                    let asset = gitea::GiteaAssetSpec {
                                        file_path: &path,
                                        file_name: &file_name,
                                    };
                                    retry_upload(&op_name, || {
                                        gitea::gitea_upload_asset(&ctx, release_id, &asset)
                                    })
                                    .await
                                    .with_context(|| {
                                        format!(
                                            "release: upload artifact '{}' to Gitea release '{}'",
                                            file_name, tag
                                        )
                                    })?;

                                    Ok::<String, anyhow::Error>(file_name)
                                });
                            }

                            while let Some(result) = join_set.join_next().await {
                                let file_name = result
                                    .context("gitea: upload task panicked")?
                                    .context("gitea: upload task failed")?;
                                log.verbose(&format!("uploaded artifact: {}", file_name));
                            }
                        }

                        // Gitea PublishRelease is a no-op (matching GoReleaser).

                        let html_url = gitea::gitea_release_url(
                            &download_url,
                            &repo_cfg.owner,
                            &repo_cfg.name,
                            &tag,
                        );
                        Ok::<String, anyhow::Error>(html_url)
                    })?;

                    (
                        url,
                        download_url,
                        repo_cfg.owner.clone(),
                        repo_cfg.name.clone(),
                    )
                }

                // ===============================================================
                // GitHub backend (existing octocrab implementation)
                // ===============================================================
                // ===============================================================
                // GitHub backend (extracted to github::run_github_backend)
                // ===============================================================
                ScmTokenType::GitHub => {
                    let env = github::BackendEnv {
                        rt: &rt,
                        ctx,
                        log: &log,
                        token: &token,
                    };
                    let spec = github::GithubReleaseSpec {
                        tag: &tag,
                        name: &release_name,
                        body: &release_body,
                        mode: &release_mode,
                        draft,
                        prerelease,
                        make_latest: &make_latest,
                        target_commitish: &target_commitish,
                        discussion_category: &discussion_category_name,
                    };
                    let upload_opts = github::UploadOpts {
                        skip_upload,
                        replace_existing_draft,
                        replace_existing_artifacts,
                        use_existing_draft,
                    };
                    match github::run_github_backend(
                        &env,
                        crate_cfg,
                        release_cfg,
                        &spec,
                        &upload_opts,
                        &artifact_entries,
                    )? {
                        Some(t) => t,
                        None => continue,
                    }
                }
            }; // end match ctx.token_type

            // Populate artifact metadata["url"] for all uploadable artifacts
            // so publishers (homebrew, scoop, chocolatey, winget, krew, nix, cask)
            // can construct download links without requiring explicit url_template.
            // Matches GoReleaser's ReleaseURLTemplate() pattern.
            if !skip_upload {
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
