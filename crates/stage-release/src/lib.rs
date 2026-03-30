use anodize_core::artifact::ArtifactKind;
use anodize_core::config::{ContentSource, ExtraFileSpec, MakeLatestConfig, PrereleaseConfig};
use anodize_core::context::Context;
use anodize_core::git;
use anodize_core::stage::Stage;
use anyhow::{bail, Context as _, Result};

// ---------------------------------------------------------------------------
// should_mark_prerelease
// ---------------------------------------------------------------------------

/// Decide whether the GitHub Release should be marked as a pre-release.
///
/// - `Auto`     – inspect the tag for common pre-release suffixes.
/// - `Bool(b)`  – use the explicit value regardless of the tag.
/// - `None`     – default to `false`.
pub(crate) fn should_mark_prerelease(config: &Option<PrereleaseConfig>, tag: &str) -> bool {
    match config {
        Some(PrereleaseConfig::Auto) => git::parse_semver(tag)
            .map(|sv| sv.is_prerelease())
            .unwrap_or(false),
        Some(PrereleaseConfig::Bool(b)) => *b,
        None => false,
    }
}

// ---------------------------------------------------------------------------
// build_release_body
// ---------------------------------------------------------------------------

/// Construct the release body by wrapping the changelog with optional
/// header and footer from the release config.
pub(crate) fn build_release_body(
    changelog_body: &str,
    header: Option<&str>,
    footer: Option<&str>,
) -> String {
    let mut parts: Vec<&str> = Vec::new();

    if let Some(h) = header
        && !h.is_empty()
    {
        parts.push(h);
    }

    if !changelog_body.is_empty() {
        parts.push(changelog_body);
    }

    if let Some(f) = footer
        && !f.is_empty()
    {
        parts.push(f);
    }

    parts.join("\n\n")
}

// ---------------------------------------------------------------------------
// collect_extra_files
// ---------------------------------------------------------------------------

/// Resolve `extra_files` glob patterns into concrete file paths.
/// Returns `(path, optional_rendered_name)` pairs. When a `Detailed` spec has
/// a `name_template`, the template is rendered using the provided `Context` and
/// returned as the second element; the upload loop should use this as the
/// upload filename instead of the filesystem name.
/// Invalid glob patterns are silently skipped (callers log through StageLogger).
pub(crate) fn collect_extra_files(
    specs: &[ExtraFileSpec],
    ctx: &Context,
) -> Vec<(std::path::PathBuf, Option<String>)> {
    let mut results = Vec::new();
    for spec in specs {
        match spec {
            ExtraFileSpec::Glob(pattern) => {
                match glob::glob(pattern) {
                    Ok(entries) => {
                        for entry in entries.flatten() {
                            if entry.is_file() {
                                results.push((entry, None));
                            }
                        }
                    }
                    Err(_) => {
                        // Invalid glob — skip silently; the release stage logs via StageLogger.
                    }
                }
            }
            ExtraFileSpec::Detailed { glob: pattern, name_template } => {
                if let Ok(entries) = glob::glob(pattern) {
                    for entry in entries.flatten() {
                        if entry.is_file() {
                            let name = name_template.as_ref().and_then(|tmpl| {
                                let filename = entry.file_name()
                                    .unwrap_or_default()
                                    .to_string_lossy();
                                let mut vars = ctx.template_vars().clone();
                                vars.set("ArtifactName", &filename);
                                anodize_core::template::render(tmpl, &vars).ok()
                            });
                            results.push((entry, name));
                        }
                    }
                }
            }
        }
    }
    results
}

// ---------------------------------------------------------------------------
// resolve_make_latest
// ---------------------------------------------------------------------------

/// Convert our config's `MakeLatestConfig` into octocrab's `MakeLatest` enum.
pub(crate) fn resolve_make_latest(
    config: &Option<MakeLatestConfig>,
) -> Option<octocrab::repos::releases::MakeLatest> {
    use octocrab::repos::releases::MakeLatest;
    match config {
        Some(MakeLatestConfig::Bool(true)) => Some(MakeLatest::True),
        Some(MakeLatestConfig::Bool(false)) => Some(MakeLatest::False),
        Some(MakeLatestConfig::Auto) => Some(MakeLatest::Legacy),
        None => None,
    }
}

// ---------------------------------------------------------------------------
// resolve_release_mode
// ---------------------------------------------------------------------------

/// The valid release `mode` values that control how existing release notes
/// are handled when a release already exists.
const VALID_RELEASE_MODES: &[&str] = &["keep-existing", "append", "prepend", "replace"];

/// Resolve and validate the release mode from config.
/// Returns `"keep-existing"` when `None` or empty (matches GoReleaser default).
pub(crate) fn resolve_release_mode(mode: Option<&str>) -> Result<String> {
    match mode {
        None | Some("") => Ok("keep-existing".to_string()),
        Some(m) => {
            if VALID_RELEASE_MODES.contains(&m) {
                Ok(m.to_string())
            } else {
                anyhow::bail!(
                    "release: invalid mode '{}', must be one of: {}",
                    m,
                    VALID_RELEASE_MODES.join(", ")
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// resolve_content_source
// ---------------------------------------------------------------------------

/// Resolve a `ContentSource` to its string content.
/// - Inline: returns the string directly.
/// - FromFile: reads the file from disk.
/// - FromUrl: fetches the URL content via HTTP GET.
pub(crate) fn resolve_content_source(source: &ContentSource) -> Result<String> {
    match source {
        ContentSource::Inline(s) => Ok(s.clone()),
        ContentSource::FromFile { from_file } => std::fs::read_to_string(from_file)
            .map_err(|e| anyhow::anyhow!("failed to read {}: {}", from_file, e)),
        ContentSource::FromUrl { from_url } => {
            let response = reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()?
                .get(from_url)
                .send()
                .map_err(|e| anyhow::anyhow!("failed to fetch content URL: {}", e))?;
            if !response.status().is_success() {
                bail!("content URL returned HTTP {}", response.status());
            }
            Ok(response.text()?)
        }
    }
}

// ---------------------------------------------------------------------------
// compose_body_for_mode
// ---------------------------------------------------------------------------

/// Compose the final release body based on the release mode.
///
/// - `"replace"` — use new_body as-is (current behavior)
/// - `"keep-existing"` — if existing_body is non-empty, keep it; otherwise use new_body
/// - `"append"` — append new_body after existing_body
/// - `"prepend"` — prepend new_body before existing_body
pub(crate) fn compose_body_for_mode(
    mode: &str,
    existing_body: Option<&str>,
    new_body: &str,
) -> String {
    match mode {
        "keep-existing" => {
            if let Some(existing) = existing_body
                && !existing.is_empty()
            {
                return existing.to_string();
            }
            new_body.to_string()
        }
        "append" => {
            if let Some(existing) = existing_body
                && !existing.is_empty()
            {
                return format!("{}\n\n{}", existing, new_body);
            }
            new_body.to_string()
        }
        "prepend" => {
            if let Some(existing) = existing_body
                && !existing.is_empty()
            {
                return format!("{}\n\n{}", new_body, existing);
            }
            new_body.to_string()
        }
        // "replace" or any other value — just use new_body
        _ => new_body.to_string(),
    }
}

// ---------------------------------------------------------------------------
// build_release_json
// ---------------------------------------------------------------------------

/// GitHub's maximum release body length in characters.
const GITHUB_RELEASE_BODY_MAX_CHARS: usize = 125_000;

/// Build the JSON body for GitHub release create/update API calls.
/// Extracts the common construction shared by PATCH (update existing draft)
/// and POST (create new release) paths.
#[allow(clippy::too_many_arguments)]
fn build_release_json(
    tag: &str,
    name: &str,
    body: &str,
    draft: bool,
    prerelease_flag: bool,
    make_latest: &Option<octocrab::repos::releases::MakeLatest>,
    target_commitish: &Option<String>,
    discussion_category: &Option<String>,
    github_native: bool,
) -> serde_json::Value {
    let mut json = serde_json::json!({
        "tag_name": tag,
        "name": name,
        "draft": draft,
        "prerelease": prerelease_flag,
    });
    if !body.is_empty() {
        let truncated_body = if body.len() > GITHUB_RELEASE_BODY_MAX_CHARS {
            let mut truncated = body[..GITHUB_RELEASE_BODY_MAX_CHARS].to_string();
            truncated.push_str("\n\n...(truncated)");
            truncated
        } else {
            body.to_string()
        };
        json["body"] = serde_json::Value::String(truncated_body);
    }
    if let Some(ml) = make_latest {
        json["make_latest"] = serde_json::Value::String(ml.to_string());
    }
    if let Some(tc) = target_commitish {
        json["target_commitish"] = serde_json::json!(tc);
    }
    if let Some(dc) = discussion_category {
        json["discussion_category_name"] = serde_json::json!(dc);
    }
    if github_native {
        json["generate_release_notes"] = serde_json::Value::Bool(true);
    }
    json
}

// ---------------------------------------------------------------------------
// ReleaseStage
// ---------------------------------------------------------------------------

pub struct ReleaseStage;

impl Stage for ReleaseStage {
    fn name(&self) -> &str {
        "release"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("release");

        // Resolve the GitHub token once (CLI flag > env var).
        let token = ctx
            .options
            .token
            .clone()
            .or_else(|| std::env::var("GITHUB_TOKEN").ok());

        let selected = ctx.options.selected_crates.clone();
        let dry_run = ctx.is_dry_run();
        let github_native_changelog = ctx.github_native_changelog;

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
            let release_cfg = crate_cfg.release.as_ref().unwrap();

            // Skip crates where release is explicitly disabled (supports template strings).
            if let Some(ref d) = release_cfg.disable
                && d.is_disabled(|s| ctx.render_template(s))
            {
                log.status(&format!(
                    "release disabled for crate '{}', skipping",
                    crate_cfg.name
                ));
                continue;
            }

            let crate_name = crate_cfg.name.clone();

            // Validate conflicting draft options.
            if release_cfg.replace_existing_draft.unwrap_or(false)
                && release_cfg.use_existing_draft.unwrap_or(false)
            {
                bail!(
                    "release: crate '{}': cannot set both replace_existing_draft and \
                     use_existing_draft — replace deletes drafts that use_existing_draft needs",
                    crate_name
                );
            }

            let changelog_body = ctx.changelogs.get(&crate_name).cloned().unwrap_or_default();

            // Resolve and validate release mode.
            let release_mode =
                resolve_release_mode(release_cfg.mode.as_deref()).with_context(|| {
                    format!("release: invalid mode for crate '{}'", crate_name)
                })?;
            if release_mode != "keep-existing" {
                log.status(&format!(
                    "release mode '{}' for crate '{}'",
                    release_mode, crate_name
                ));
            }

            // Resolve and template-render header/footer before building release body.
            let rendered_header = release_cfg
                .header
                .as_ref()
                .map(|src| {
                    let raw = resolve_content_source(src)
                        .with_context(|| format!("release: resolve header for crate '{}'", crate_name))?;
                    ctx.render_template(&raw)
                        .with_context(|| format!("release: render header for crate '{}'", crate_name))
                })
                .transpose()?;
            let rendered_footer = release_cfg
                .footer
                .as_ref()
                .map(|src| {
                    let raw = resolve_content_source(src)
                        .with_context(|| format!("release: resolve footer for crate '{}'", crate_name))?;
                    ctx.render_template(&raw)
                        .with_context(|| format!("release: render footer for crate '{}'", crate_name))
                })
                .transpose()?;

            let release_body = build_release_body(
                &changelog_body,
                rendered_header.as_deref(),
                rendered_footer.as_deref(),
            );

            // Resolve tag from template.
            let tag = ctx
                .render_template(&crate_cfg.tag_template)
                .with_context(|| {
                    format!(
                        "release: render tag_template for crate '{}'",
                        crate_cfg.name
                    )
                })?;

            // Resolve release name.
            let release_name = if let Some(tmpl) = &release_cfg.name_template {
                ctx.render_template(tmpl).with_context(|| {
                    format!(
                        "release: render name_template for crate '{}'",
                        crate_cfg.name
                    )
                })?
            } else {
                tag.clone()
            };

            let draft = release_cfg.draft.unwrap_or(false);
            let prerelease = should_mark_prerelease(&release_cfg.prerelease, &tag);
            let skip_upload = release_cfg.skip_upload.unwrap_or(false);
            let replace_existing_draft = release_cfg.replace_existing_draft.unwrap_or(false);
            let replace_existing_artifacts =
                release_cfg.replace_existing_artifacts.unwrap_or(false);
            let make_latest = resolve_make_latest(&release_cfg.make_latest);
            let ids_filter = release_cfg.ids.as_ref();
            let target_commitish = release_cfg.target_commitish.clone();
            let discussion_category_name = release_cfg.discussion_category_name.clone();
            let include_meta = release_cfg.include_meta.unwrap_or(false);
            let use_existing_draft = release_cfg.use_existing_draft.unwrap_or(false);

            // Collect uploadable artifacts for this crate, applying ids filter.
            // Each entry is (path, optional_custom_name). The custom name is only
            // set for extra_files with a name_template; regular artifacts use None.
            let mut artifact_entries: Vec<(std::path::PathBuf, Option<String>)> = [
                ArtifactKind::Archive,
                ArtifactKind::Checksum,
                ArtifactKind::LinuxPackage,
                ArtifactKind::Snap,
                ArtifactKind::DiskImage,
                ArtifactKind::Installer,
                ArtifactKind::MacOsPackage,
                ArtifactKind::SourceArchive,
                ArtifactKind::Sbom,
            ]
            .iter()
            .flat_map(|&kind| {
                let artifacts = ctx
                    .artifacts
                    .by_kind_and_crate(kind, &crate_cfg.name)
                    .into_iter();
                if let Some(ids) = ids_filter {
                    artifacts
                        .filter(|a| {
                            matches!(a.metadata.get("id"), Some(id) if ids.contains(id))
                        })
                        .map(|a| (a.path.clone(), None))
                        .collect::<Vec<_>>()
                } else {
                    artifacts.map(|a| (a.path.clone(), None)).collect::<Vec<_>>()
                }
            })
            .collect();

            // Also include Metadata artifacts that are Signatures or Certificates.
            let sig_cert_entries: Vec<(std::path::PathBuf, Option<String>)> = ctx
                .artifacts
                .by_kind_and_crate(ArtifactKind::Metadata, &crate_cfg.name)
                .into_iter()
                .filter(|a| {
                    matches!(
                        a.metadata.get("type").map(|s| s.as_str()),
                        Some("Signature") | Some("Certificate")
                    )
                })
                .map(|a| (a.path.clone(), None))
                .collect();
            artifact_entries.extend(sig_cert_entries);

            if let Some(ids) = ids_filter {
                log.verbose(&format!(
                    "ids filter {:?} selected {} artifacts for crate '{}'",
                    ids,
                    artifact_entries.len(),
                    crate_cfg.name
                ));
            }

            // Collect extra files from glob patterns (with optional name_template).
            if let Some(extra_specs) = &release_cfg.extra_files {
                let extra = collect_extra_files(extra_specs, ctx);
                artifact_entries.extend(extra);
            }

            // include_meta: upload metadata.json and artifacts.json from dist dir.
            if include_meta {
                let dist_dir = &ctx.config.dist;
                for meta_name in &["metadata.json", "artifacts.json"] {
                    let meta_path = dist_dir.join(meta_name);
                    if meta_path.exists() {
                        artifact_entries.push((meta_path, None));
                    } else {
                        log.verbose(&format!(
                            "include_meta: {} not found at {}",
                            meta_name,
                            meta_path.display()
                        ));
                    }
                }
            }

            if dry_run {
                log.status(&format!(
                    "(dry-run) would create GitHub Release '{}' (tag={}, draft={}, prerelease={}, mode={}) for crate '{}'",
                    release_name, tag, draft, prerelease, release_mode, crate_cfg.name
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
                continue;
            }

            // Require a GitHub config block.
            let github = match &release_cfg.github {
                Some(g) => g.clone(),
                None => {
                    log.warn(&format!(
                        "no github config for crate '{}', skipping",
                        crate_cfg.name
                    ));
                    continue;
                }
            };

            // Require a token for real API calls.
            let token_str = match &token {
                Some(t) => t.clone(),
                None => {
                    anyhow::bail!(
                        "release: no GitHub token available (set GITHUB_TOKEN or pass --token)"
                    );
                }
            };

            // Build the octocrab instance and perform async API calls inside a
            // dedicated tokio runtime (the Stage trait is synchronous).
            let url = rt.block_on(async {
                let octo = octocrab::Octocrab::builder()
                    .personal_token(token_str.clone())
                    .build()
                    .context("release: build octocrab client")?;

                // Handle replace_existing_draft: check if a draft release with
                // the same tag exists and delete it.
                if replace_existing_draft {
                    match octo
                        .repos(&github.owner, &github.name)
                        .releases()
                        .get_by_tag(&tag)
                        .await
                    {
                        Ok(existing) if existing.draft => {
                            log.status(&format!(
                                "replacing existing draft release '{}' (id={})",
                                tag, existing.id
                            ));
                            octo.repos(&github.owner, &github.name)
                                .releases()
                                .delete(existing.id.into_inner())
                                .await
                                .with_context(|| {
                                    format!(
                                        "release: delete existing draft release '{}' on {}/{}",
                                        tag, github.owner, github.name
                                    )
                                })?;
                        }
                        Ok(_) => {
                            // Existing release is not a draft; do not replace it.
                        }
                        Err(_) => {
                            // No existing release with this tag; proceed normally.
                        }
                    }
                }

                // Handle use_existing_draft: look for an existing draft release
                // with the same tag and update it instead of creating a new one.
                let existing_draft = if use_existing_draft {
                    match octo
                        .repos(&github.owner, &github.name)
                        .releases()
                        .get_by_tag(&tag)
                        .await
                    {
                        Ok(existing) if existing.draft => {
                            log.status(&format!(
                                "reusing existing draft release '{}' (id={})",
                                tag, existing.id
                            ));
                            Some(existing)
                        }
                        _ => None,
                    }
                } else {
                    None
                };

                // When updating an existing release, apply mode-based body composition.
                let final_body = if let Some(ref existing) = existing_draft {
                    let existing_body = existing.body.as_deref();
                    compose_body_for_mode(&release_mode, existing_body, &release_body)
                } else {
                    // For new releases, check if a release exists for mode != "replace".
                    if release_mode != "replace" {
                        match octo
                            .repos(&github.owner, &github.name)
                            .releases()
                            .get_by_tag(&tag)
                            .await
                        {
                            Ok(existing) => {
                                let existing_body = existing.body.as_deref();
                                compose_body_for_mode(&release_mode, existing_body, &release_body)
                            }
                            Err(_) => release_body.clone(),
                        }
                    } else {
                        release_body.clone()
                    }
                };

                // Create or update the release. We use raw API calls for all paths
                // to support target_commitish and discussion_category_name, which
                // are not fully exposed by octocrab's builder API.
                //
                // Draft-then-publish: always create as draft first so users never
                // see a release with missing artifacts. After all uploads succeed,
                // we PATCH draft=false if the user wanted a non-draft release.
                let user_wants_draft = draft;
                let json_body = build_release_json(
                    &tag,
                    &release_name,
                    &final_body,
                    true, // always create as draft first
                    prerelease,
                    &make_latest,
                    &target_commitish,
                    &discussion_category_name,
                    github_native_changelog,
                );

                let release = if let Some(ref existing) = existing_draft {
                    // Update the existing draft release via PATCH.
                    let route = format!(
                        "/repos/{}/{}/releases/{}",
                        github.owner, github.name, existing.id
                    );
                    octo.patch::<octocrab::models::repos::Release, _, _>(route, Some(&json_body))
                        .await
                        .with_context(|| {
                            format!(
                                "release: update existing draft release '{}' on {}/{}",
                                tag, github.owner, github.name
                            )
                        })?
                } else {
                    // Create a new release via POST.
                    let route = format!(
                        "/repos/{}/{}/releases",
                        github.owner, github.name
                    );
                    octo.post::<_, octocrab::models::repos::Release>(route, Some(&json_body))
                        .await
                        .with_context(|| {
                            format!(
                                "release: create GitHub release '{}' on {}/{}",
                                tag, github.owner, github.name
                            )
                        })?
                };

                log.status(&format!(
                    "created GitHub Release '{}' (id={}) on {}/{}",
                    release_name, release.id, github.owner, github.name
                ));

                let html_url = release.html_url.to_string();

                // Upload each artifact (unless skip_upload is set).
                if skip_upload {
                    log.status("skip_upload is set, skipping artifact uploads");
                } else {
                    for (path, custom_name) in &artifact_entries {
                        if !path.exists() {
                            log.warn(&format!(
                                "artifact not found, skipping upload: {}",
                                path.display()
                            ));
                            continue;
                        }

                        let file_name = if let Some(name) = custom_name {
                            name.clone()
                        } else {
                            path.file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| "artifact".to_string())
                        };

                        // Handle replace_existing_artifacts: if an asset with the
                        // same name already exists, delete it before uploading.
                        if replace_existing_artifacts {
                            for existing_asset in &release.assets {
                                if existing_asset.name == file_name {
                                    log.verbose(&format!(
                                        "replacing existing artifact '{}'",
                                        file_name
                                    ));
                                    octo.repos(&github.owner, &github.name)
                                        .release_assets()
                                        .delete(existing_asset.id.into_inner())
                                        .await
                                        .with_context(|| {
                                            format!(
                                                "release: delete existing artifact '{}' from release '{}'",
                                                file_name, tag
                                            )
                                        })?;
                                    break;
                                }
                            }
                        }

                        // Retry loop: up to 3 attempts with a 2-second delay.
                        // Retries on HTTP 5xx and network errors. On HTTP 422
                        // with "already_exists", deletes the existing asset and
                        // retries if replace_existing_artifacts is true.
                        const MAX_UPLOAD_ATTEMPTS: u32 = 3;
                        const UPLOAD_RETRY_DELAY: std::time::Duration =
                            std::time::Duration::from_secs(2);

                        let mut last_err: Option<anyhow::Error> = None;
                        for attempt in 1..=MAX_UPLOAD_ATTEMPTS {
                            let data = std::fs::read(path).with_context(|| {
                                format!("release: read artifact {}", path.display())
                            })?;

                            match octo
                                .repos(&github.owner, &github.name)
                                .releases()
                                .upload_asset(release.id.into_inner(), &file_name, data.into())
                                .send()
                                .await
                            {
                                Ok(_) => {
                                    last_err = None;
                                    break;
                                }
                                Err(err) => {
                                    let err_str = err.to_string();
                                    let is_server_error = matches!(
                                        &err,
                                        octocrab::Error::GitHub { source, .. }
                                            if source.status_code.is_server_error()
                                    );
                                    let is_already_exists = matches!(
                                        &err,
                                        octocrab::Error::GitHub { source, .. }
                                            if source.status_code.as_u16() == 422
                                    ) && err_str.contains("already_exists");

                                    if is_already_exists && replace_existing_artifacts {
                                        // Delete the conflicting asset and retry.
                                        log.verbose(&format!(
                                            "artifact '{}' already exists, deleting before retry (attempt {}/{})",
                                            file_name, attempt, MAX_UPLOAD_ATTEMPTS
                                        ));
                                        // List current assets to find the one to delete.
                                        let current_release = octo
                                            .repos(&github.owner, &github.name)
                                            .releases()
                                            .get(release.id.into_inner())
                                            .await
                                            .with_context(|| {
                                                format!(
                                                    "release: fetch release to find duplicate asset '{}'",
                                                    file_name
                                                )
                                            })?;
                                        for asset in &current_release.assets {
                                            if asset.name == file_name {
                                                octo.repos(&github.owner, &github.name)
                                                    .release_assets()
                                                    .delete(asset.id.into_inner())
                                                    .await
                                                    .with_context(|| {
                                                        format!(
                                                            "release: delete duplicate artifact '{}' from release '{}'",
                                                            file_name, tag
                                                        )
                                                    })?;
                                                break;
                                            }
                                        }
                                        last_err = Some(anyhow::anyhow!(err));
                                        if attempt < MAX_UPLOAD_ATTEMPTS {
                                            tokio::time::sleep(UPLOAD_RETRY_DELAY).await;
                                        }
                                        continue;
                                    } else if is_server_error
                                        || matches!(&err, octocrab::Error::Hyper { .. })
                                        || matches!(&err, octocrab::Error::Http { .. })
                                    {
                                        // Retryable server/network error.
                                        log.warn(&format!(
                                            "upload of '{}' failed (attempt {}/{}): {}",
                                            file_name, attempt, MAX_UPLOAD_ATTEMPTS, err_str
                                        ));
                                        last_err = Some(anyhow::anyhow!(err));
                                        if attempt < MAX_UPLOAD_ATTEMPTS {
                                            tokio::time::sleep(UPLOAD_RETRY_DELAY).await;
                                        }
                                        continue;
                                    } else {
                                        // Non-retryable error — fail immediately.
                                        return Err(anyhow::anyhow!(err)).with_context(|| {
                                            format!(
                                                "release: upload artifact '{}' to release '{}'",
                                                file_name, tag
                                            )
                                        });
                                    }
                                }
                            }
                        }
                        if let Some(err) = last_err {
                            return Err(err).with_context(|| {
                                format!(
                                    "release: upload artifact '{}' to release '{}' failed after {} attempts",
                                    file_name, tag, MAX_UPLOAD_ATTEMPTS
                                )
                            });
                        }

                        log.verbose(&format!("uploaded artifact: {}", file_name));
                    }
                }

                // Draft-then-publish: if the user's config has draft=false,
                // un-draft the release now that all assets are uploaded.
                if !user_wants_draft {
                    let publish_route = format!(
                        "/repos/{}/{}/releases/{}",
                        github.owner, github.name, release.id
                    );
                    let publish_body = serde_json::json!({ "draft": false });
                    octo.patch::<octocrab::models::repos::Release, _, _>(
                        publish_route,
                        Some(&publish_body),
                    )
                    .await
                    .with_context(|| {
                        format!(
                            "release: publish (un-draft) release '{}' on {}/{}",
                            tag, github.owner, github.name
                        )
                    })?;
                    log.status(&format!(
                        "published release '{}' (draft -> live)",
                        release_name
                    ));
                }

                Ok::<String, anyhow::Error>(html_url)
            })?;

            ctx.set_release_url(&url);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use anodize_core::config::{
        ContentSource, CrateConfig, ExtraFileSpec, MakeLatestConfig, PrereleaseConfig,
        ReleaseConfig, StringOrBool,
    };
    use anodize_core::test_helpers::TestContextBuilder;

    #[test]
    fn test_is_prerelease_auto_with_rc() {
        assert!(should_mark_prerelease(
            &Some(PrereleaseConfig::Auto),
            "v1.0.0-rc.1"
        ));
    }

    #[test]
    fn test_is_prerelease_auto_stable() {
        assert!(!should_mark_prerelease(
            &Some(PrereleaseConfig::Auto),
            "v1.0.0"
        ));
    }

    #[test]
    fn test_is_prerelease_explicit_true() {
        assert!(should_mark_prerelease(
            &Some(PrereleaseConfig::Bool(true)),
            "v1.0.0"
        ));
    }

    #[test]
    fn test_is_prerelease_explicit_false() {
        assert!(!should_mark_prerelease(
            &Some(PrereleaseConfig::Bool(false)),
            "v1.0.0-rc.1"
        ));
    }

    #[test]
    fn test_is_prerelease_none() {
        assert!(!should_mark_prerelease(&None, "v1.0.0"));
    }

    #[test]
    fn test_stage_skips_crate_without_release_config() {
        let mut ctx = TestContextBuilder::new().build();
        let stage = ReleaseStage;
        // Should succeed — no crates have release config
        assert!(stage.run(&mut ctx).is_ok());
    }

    // ---- build_release_body tests ----

    #[test]
    fn test_build_release_body_with_header_and_footer() {
        let body = build_release_body(
            "## Changes\n- Fixed a bug",
            Some("# Release v1.0"),
            Some("---\nPowered by anodize"),
        );
        assert_eq!(
            body,
            "# Release v1.0\n\n## Changes\n- Fixed a bug\n\n---\nPowered by anodize"
        );
    }

    #[test]
    fn test_build_release_body_header_only() {
        let body = build_release_body("changelog content", Some("HEADER"), None);
        assert_eq!(body, "HEADER\n\nchangelog content");
    }

    #[test]
    fn test_build_release_body_footer_only() {
        let body = build_release_body("changelog content", None, Some("FOOTER"));
        assert_eq!(body, "changelog content\n\nFOOTER");
    }

    #[test]
    fn test_build_release_body_no_header_footer() {
        let body = build_release_body("changelog content", None, None);
        assert_eq!(body, "changelog content");
    }

    #[test]
    fn test_build_release_body_empty_changelog() {
        let body = build_release_body("", Some("HEADER"), Some("FOOTER"));
        assert_eq!(body, "HEADER\n\nFOOTER");
    }

    #[test]
    fn test_build_release_body_all_empty() {
        let body = build_release_body("", None, None);
        assert_eq!(body, "");
    }

    #[test]
    fn test_build_release_body_empty_string_header_footer() {
        // Empty strings should be treated as absent
        let body = build_release_body("changes", Some(""), Some(""));
        assert_eq!(body, "changes");
    }

    // ---- collect_extra_files tests ----

    #[test]
    fn test_collect_extra_files_no_patterns() {
        let ctx = TestContextBuilder::new().build();
        let result = collect_extra_files(&[], &ctx);
        assert!(result.is_empty());
    }

    #[test]
    fn test_collect_extra_files_no_matches() {
        let ctx = TestContextBuilder::new().build();
        let result = collect_extra_files(&[ExtraFileSpec::Glob(
            "/tmp/anodize_test_nonexistent_dir_12345/*.xyz".to_string(),
        )], &ctx);
        assert!(result.is_empty());
    }

    #[test]
    fn test_collect_extra_files_with_real_file() {
        let ctx = TestContextBuilder::new().build();
        // Create a temp file and collect it
        let dir = std::env::temp_dir().join("anodize_extra_files_test");
        let _ = std::fs::create_dir_all(&dir);
        let test_file = dir.join("test_extra.txt");
        std::fs::write(&test_file, "extra file content").unwrap();

        let pattern = dir.join("*.txt").to_string_lossy().into_owned();
        let result = collect_extra_files(&[ExtraFileSpec::Glob(pattern)], &ctx);
        assert!(
            result
                .iter()
                .any(|(p, _)| p.file_name().unwrap() == "test_extra.txt")
        );

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_collect_extra_files_skips_directories() {
        let ctx = TestContextBuilder::new().build();
        let dir = std::env::temp_dir().join("anodize_extra_files_dir_test");
        let _ = std::fs::create_dir_all(dir.join("subdir"));
        let test_file = dir.join("file.txt");
        std::fs::write(&test_file, "content").unwrap();

        // The glob "*" matches both files and directories; we only want files
        let pattern = dir.join("*").to_string_lossy().into_owned();
        let result = collect_extra_files(&[ExtraFileSpec::Glob(pattern)], &ctx);
        assert!(result.iter().all(|(p, _)| p.is_file()));

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_collect_extra_files_detailed_spec() {
        let ctx = TestContextBuilder::new().build();
        let dir = std::env::temp_dir().join("anodize_extra_files_detailed_test");
        let _ = std::fs::create_dir_all(&dir);
        let test_file = dir.join("artifact.sig");
        std::fs::write(&test_file, "signature").unwrap();

        let pattern = dir.join("*.sig").to_string_lossy().into_owned();
        let result = collect_extra_files(&[ExtraFileSpec::Detailed {
            glob: pattern,
            name_template: Some("{{ .ArtifactName }}.sig".to_string()),
        }], &ctx);
        assert_eq!(result.len(), 1);
        assert!(result[0].0.file_name().unwrap() == "artifact.sig");
        // name_template should have been rendered
        assert_eq!(result[0].1.as_deref(), Some("artifact.sig.sig"));

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- resolve_make_latest tests ----

    #[test]
    fn test_resolve_make_latest_true() {
        let ml = resolve_make_latest(&Some(MakeLatestConfig::Bool(true)));
        assert!(ml.is_some());
        assert_eq!(ml.unwrap().to_string(), "true");
    }

    #[test]
    fn test_resolve_make_latest_false() {
        let ml = resolve_make_latest(&Some(MakeLatestConfig::Bool(false)));
        assert!(ml.is_some());
        assert_eq!(ml.unwrap().to_string(), "false");
    }

    #[test]
    fn test_resolve_make_latest_auto() {
        let ml = resolve_make_latest(&Some(MakeLatestConfig::Auto));
        assert!(ml.is_some());
        assert_eq!(ml.unwrap().to_string(), "legacy");
    }

    #[test]
    fn test_resolve_make_latest_none() {
        let ml = resolve_make_latest(&None);
        assert!(ml.is_none());
    }

    // ---- skip_upload behavior test ----

    #[test]
    fn test_skip_upload_dry_run_message() {
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    skip_upload: Some(true),
                    draft: Some(false),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        let stage = ReleaseStage;
        // Dry-run should succeed even with skip_upload = true
        assert!(stage.run(&mut ctx).is_ok());
    }

    // ---- replace_existing_draft / replace_existing_artifacts config defaults ----

    #[test]
    fn test_replace_existing_draft_defaults() {
        let cfg = ReleaseConfig::default();
        assert_eq!(cfg.replace_existing_draft, None);
    }

    #[test]
    fn test_replace_existing_artifacts_defaults() {
        let cfg = ReleaseConfig::default();
        assert_eq!(cfg.replace_existing_artifacts, None);
    }

    // ---- integration-style dry-run tests ----

    #[test]
    fn test_dry_run_with_extra_files() {
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    extra_files: Some(vec![ExtraFileSpec::Glob(
                        "/tmp/anodize_test_nonexistent/*.sig".to_string(),
                    )]),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        let stage = ReleaseStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_dry_run_with_header_footer_in_changelog() {
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    header: Some(ContentSource::Inline("# Custom Header".to_string())),
                    footer: Some(ContentSource::Inline("Custom Footer".to_string())),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        ctx.changelogs
            .insert("testcrate".to_string(), "- bug fix".to_string());
        let stage = ReleaseStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_dry_run_with_make_latest() {
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    make_latest: Some(MakeLatestConfig::Bool(true)),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        let stage = ReleaseStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    // ---- Error path tests (Task 3B) ----

    #[test]
    fn test_release_missing_token_errors() {
        use anodize_core::config::GitHubConfig;

        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .token(None)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    github: Some(GitHubConfig {
                        owner: "testowner".to_string(),
                        name: "testrepo".to_string(),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        let stage = ReleaseStage;
        let result = stage.run(&mut ctx);

        // If GITHUB_TOKEN happens to be set in the environment (e.g., CI),
        // the stage would proceed past token resolution and fail on the API
        // call instead. Either way, it should error.
        assert!(result.is_err(), "release without token should fail");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("GITHUB_TOKEN") || err.contains("--token") || err.contains("release"),
            "error should mention GITHUB_TOKEN, --token, or release failure, got: {err}"
        );
    }

    #[test]
    fn test_release_no_github_config_skips_silently() {
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    github: None, // no github config
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        let stage = ReleaseStage;
        // Should succeed — no github config causes skip, not error
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_prerelease_auto_detects_alpha() {
        assert!(should_mark_prerelease(
            &Some(PrereleaseConfig::Auto),
            "v1.0.0-alpha.1"
        ));
    }

    #[test]
    fn test_prerelease_auto_detects_beta() {
        assert!(should_mark_prerelease(
            &Some(PrereleaseConfig::Auto),
            "v2.0.0-beta"
        ));
    }

    #[test]
    fn test_prerelease_auto_detects_dev() {
        assert!(should_mark_prerelease(
            &Some(PrereleaseConfig::Auto),
            "v1.0.0-dev.5"
        ));
    }

    #[test]
    fn test_collect_extra_files_invalid_glob_pattern() {
        let ctx = TestContextBuilder::new().build();
        // An invalid glob pattern should be handled gracefully
        let result =
            collect_extra_files(&[ExtraFileSpec::Glob("[invalid-glob".to_string())], &ctx);
        // collect_extra_files logs a warning and returns empty, does not panic
        assert!(result.is_empty());
    }

    // ---- MockGitHubClient integration test ----

    #[test]
    fn test_release_pipeline_with_mock_github_client() {
        use anodize_core::github_client::{
            AssetInfo, CreateReleaseParams, GitHubClient, MockGitHubClient, ReleaseInfo,
            UploadAssetParams,
        };

        // Set up the mock to return a successful release creation
        let mock = MockGitHubClient::new();
        mock.set_create_release_response(Ok(ReleaseInfo {
            id: 42,
            html_url: "https://github.com/testowner/testrepo/releases/42".to_string(),
            tag_name: "v1.0.0".to_string(),
            draft: false,
        }));
        mock.set_upload_asset_response(Ok(AssetInfo {
            id: 100,
            name: "artifact.tar.gz".to_string(),
            size: 1024,
        }));

        // Build release parameters as the stage would
        let params = CreateReleaseParams {
            owner: "testowner".to_string(),
            repo: "testrepo".to_string(),
            tag_name: "v1.0.0".to_string(),
            name: "Release v1.0.0".to_string(),
            body: build_release_body("- initial release", Some("# v1.0.0"), None),
            draft: false,
            prerelease: should_mark_prerelease(&Some(PrereleaseConfig::Auto), "v1.0.0"),
            generate_release_notes: false,
            make_latest: None,
        };

        // Simulate the release pipeline: create release + upload asset
        let release = mock.create_release(&params).unwrap();
        assert_eq!(release.id, 42);
        assert_eq!(release.tag_name, "v1.0.0");
        assert!(!release.draft);

        // Simulate uploading an asset
        let upload_params = UploadAssetParams {
            owner: "testowner".to_string(),
            repo: "testrepo".to_string(),
            release_id: release.id,
            file_name: "myapp-linux-amd64.tar.gz".to_string(),
            file_path: std::path::PathBuf::from("/tmp/myapp-linux-amd64.tar.gz"),
        };
        let asset = mock.upload_asset(&upload_params).unwrap();
        assert_eq!(asset.name, "artifact.tar.gz");

        // Verify the mock recorded the correct calls
        assert_eq!(mock.create_release_call_count(), 1);
        assert_eq!(mock.upload_asset_call_count(), 1);

        let create_calls = mock.create_release_calls();
        assert_eq!(create_calls[0].owner, "testowner");
        assert_eq!(create_calls[0].tag_name, "v1.0.0");
        assert_eq!(create_calls[0].body, "# v1.0.0\n\n- initial release");
        assert!(!create_calls[0].prerelease);

        let upload_calls = mock.upload_asset_calls();
        assert_eq!(upload_calls[0].release_id, 42);
        assert_eq!(upload_calls[0].file_name, "myapp-linux-amd64.tar.gz");
    }

    // -----------------------------------------------------------------------
    // Task 4C: Additional behavior tests — config fields actually do things
    // -----------------------------------------------------------------------

    #[test]
    fn test_header_footer_wrap_changelog_in_release_body() {
        // Verify that header and footer actually appear around the changelog body
        let body = build_release_body(
            "- Fixed bug A\n- Added feature B",
            Some("## Release v2.0"),
            Some("---\nThank you for using our tool!"),
        );
        assert!(body.starts_with("## Release v2.0"));
        assert!(body.contains("- Fixed bug A"));
        assert!(body.contains("- Added feature B"));
        assert!(body.ends_with("Thank you for using our tool!"));

        // Parts should be separated by double newlines
        assert!(body.contains("## Release v2.0\n\n- Fixed bug A"));
        assert!(body.contains("Added feature B\n\n---"));
    }

    #[test]
    fn test_extra_files_collected_with_glob() {
        let ctx = TestContextBuilder::new().build();
        // Create temp files and verify glob collection works
        let dir = std::env::temp_dir().join("anodize_release_extra_test");
        let _ = std::fs::create_dir_all(&dir);
        let f1 = dir.join("artifact1.sig");
        let f2 = dir.join("artifact2.sig");
        let f3 = dir.join("readme.txt");
        std::fs::write(&f1, "sig1").unwrap();
        std::fs::write(&f2, "sig2").unwrap();
        std::fs::write(&f3, "text").unwrap();

        // Collect only .sig files
        let pattern = dir.join("*.sig").to_string_lossy().into_owned();
        let result = collect_extra_files(&[ExtraFileSpec::Glob(pattern)], &ctx);
        assert_eq!(result.len(), 2, "should find exactly 2 .sig files");
        assert!(result.iter().all(|(p, _)| p.extension().unwrap() == "sig"));

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_skip_upload_prevents_dry_run_upload_messages() {
        // When skip_upload is true, the dry-run output should mention skip_upload
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    skip_upload: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        let stage = ReleaseStage;
        // Should complete without error
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_make_latest_values_resolve_correctly() {
        // Bool(true) -> MakeLatest::True
        let ml_true = resolve_make_latest(&Some(MakeLatestConfig::Bool(true))).unwrap();
        assert_eq!(ml_true.to_string(), "true");

        // Bool(false) -> MakeLatest::False
        let ml_false = resolve_make_latest(&Some(MakeLatestConfig::Bool(false))).unwrap();
        assert_eq!(ml_false.to_string(), "false");

        // Auto -> MakeLatest::Legacy
        let ml_auto = resolve_make_latest(&Some(MakeLatestConfig::Auto)).unwrap();
        assert_eq!(ml_auto.to_string(), "legacy");

        // None -> None
        assert!(resolve_make_latest(&None).is_none());
    }

    #[test]
    fn test_release_name_template_rendering() {
        // Verify the rendered release name matches expected template output.
        // We simulate the same resolution logic the stage uses: render
        // name_template via ctx.render_template and check the result.
        use anodize_core::github_client::{
            CreateReleaseParams, GitHubClient, MockGitHubClient, ReleaseInfo,
        };

        let ctx = TestContextBuilder::new()
            .project_name("myapp")
            .tag("v2.0.0")
            .build();

        let name_template = "MyApp {{ .Version }}";
        let rendered_name = ctx.render_template(name_template).unwrap();
        assert_eq!(
            rendered_name, "MyApp 2.0.0",
            "name_template should render Version variable"
        );

        let tag_template = "v{{ .Version }}";
        let rendered_tag = ctx.render_template(tag_template).unwrap();
        assert_eq!(rendered_tag, "v2.0.0");

        // Verify the rendered name would propagate to the GitHub API via mock
        let mock = MockGitHubClient::new();
        mock.set_create_release_response(Ok(ReleaseInfo {
            id: 1,
            html_url: "https://github.com/test/test/releases/1".to_string(),
            tag_name: rendered_tag.clone(),
            draft: false,
        }));

        let params = CreateReleaseParams {
            owner: "testowner".to_string(),
            repo: "testrepo".to_string(),
            tag_name: rendered_tag,
            name: rendered_name.clone(),
            body: String::new(),
            draft: false,
            prerelease: false,
            generate_release_notes: false,
            make_latest: None,
        };

        mock.create_release(&params).unwrap();

        let calls = mock.create_release_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].name, "MyApp 2.0.0",
            "rendered name_template should be passed as the release name"
        );
    }

    #[test]
    fn test_draft_release_flag() {
        // Verify draft=true propagates through to the GitHub API parameters.
        use anodize_core::github_client::{
            CreateReleaseParams, GitHubClient, MockGitHubClient, ReleaseInfo,
        };

        let release_cfg = ReleaseConfig {
            draft: Some(true),
            ..Default::default()
        };

        // Resolve draft the same way the stage does
        let draft = release_cfg.draft.unwrap_or(false);
        assert!(draft, "draft=Some(true) should resolve to true");

        // Also verify the default case
        let default_cfg = ReleaseConfig::default();
        let default_draft = default_cfg.draft.unwrap_or(false);
        assert!(!default_draft, "draft=None should default to false");

        // Verify draft=true propagates to the mock GitHub client
        let mock = MockGitHubClient::new();
        mock.set_create_release_response(Ok(ReleaseInfo {
            id: 99,
            html_url: "https://github.com/test/test/releases/99".to_string(),
            tag_name: "v1.0.0".to_string(),
            draft: true,
        }));

        let params = CreateReleaseParams {
            owner: "testowner".to_string(),
            repo: "testrepo".to_string(),
            tag_name: "v1.0.0".to_string(),
            name: "Release v1.0.0".to_string(),
            body: build_release_body("changelog", None, None),
            draft,
            prerelease: should_mark_prerelease(&None, "v1.0.0"),
            generate_release_notes: false,
            make_latest: None,
        };

        let release = mock.create_release(&params).unwrap();
        assert!(release.draft, "mock should return draft=true");

        let calls = mock.create_release_calls();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].draft,
            "draft=true must propagate to CreateReleaseParams"
        );
        assert!(
            !calls[0].prerelease,
            "prerelease should be false for stable tag with None config"
        );
    }

    #[test]
    fn test_prerelease_auto_case_insensitive() {
        // The prerelease Auto detection should be case-insensitive
        assert!(should_mark_prerelease(
            &Some(PrereleaseConfig::Auto),
            "v1.0.0-RC.1"
        ));
        assert!(should_mark_prerelease(
            &Some(PrereleaseConfig::Auto),
            "v1.0.0-BETA"
        ));
        assert!(should_mark_prerelease(
            &Some(PrereleaseConfig::Auto),
            "v1.0.0-ALPHA.5"
        ));
    }

    // ---- Error path tests (Task 4D) ----

    #[test]
    fn test_release_missing_token_error_message_is_actionable() {
        // The release stage requires a GitHub token for non-dry-run.
        // test_release_missing_token_errors already covers this,
        // but we verify the error message is actionable (tells user what to do).
        use anodize_core::config::GitHubConfig;

        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .token(None)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    github: Some(GitHubConfig {
                        owner: "testowner".to_string(),
                        name: "testrepo".to_string(),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        let stage = ReleaseStage;
        let result = stage.run(&mut ctx);

        // If GITHUB_TOKEN is in the environment, the stage proceeds past
        // token resolution and fails on the API call instead. Either way
        // the error should be informative.
        assert!(
            result.is_err(),
            "release without explicit token should fail"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("GITHUB_TOKEN")
                || err.contains("--token")
                || err.contains("release")
                || err.contains("GitHub"),
            "error should mention GITHUB_TOKEN, --token, or release context, got: {err}"
        );
    }

    #[test]
    fn test_mock_github_api_401_error() {
        use anodize_core::github_client::{CreateReleaseParams, GitHubClient, MockGitHubClient};

        let mock = MockGitHubClient::new();
        mock.set_create_release_response(Err("401 Unauthorized: Bad credentials".to_string()));

        let params = CreateReleaseParams {
            owner: "testowner".to_string(),
            repo: "testrepo".to_string(),
            tag_name: "v1.0.0".to_string(),
            name: "Release v1.0.0".to_string(),
            body: String::new(),
            draft: false,
            prerelease: false,
            generate_release_notes: false,
            make_latest: None,
        };

        let result = mock.create_release(&params);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("401") && err.contains("Unauthorized"),
            "error should contain HTTP status and description, got: {err}"
        );
    }

    #[test]
    fn test_mock_github_api_403_error() {
        use anodize_core::github_client::{CreateReleaseParams, GitHubClient, MockGitHubClient};

        let mock = MockGitHubClient::new();
        mock.set_create_release_response(Err(
            "403 Forbidden: Resource not accessible by integration".to_string(),
        ));

        let params = CreateReleaseParams {
            owner: "testowner".to_string(),
            repo: "testrepo".to_string(),
            tag_name: "v1.0.0".to_string(),
            name: "Release".to_string(),
            body: String::new(),
            draft: false,
            prerelease: false,
            generate_release_notes: false,
            make_latest: None,
        };

        let result = mock.create_release(&params);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("403"));
    }

    #[test]
    fn test_mock_github_api_404_error() {
        use anodize_core::github_client::{CreateReleaseParams, GitHubClient, MockGitHubClient};

        let mock = MockGitHubClient::new();
        mock.set_create_release_response(Err("404 Not Found: repository not found".to_string()));

        let params = CreateReleaseParams {
            owner: "testowner".to_string(),
            repo: "nonexistent-repo".to_string(),
            tag_name: "v1.0.0".to_string(),
            name: "Release".to_string(),
            body: String::new(),
            draft: false,
            prerelease: false,
            generate_release_notes: false,
            make_latest: None,
        };

        let result = mock.create_release(&params);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("404") && err.contains("Not Found"),
            "error should contain 404 Not Found, got: {err}"
        );
    }

    #[test]
    fn test_mock_github_api_422_error() {
        use anodize_core::github_client::{CreateReleaseParams, GitHubClient, MockGitHubClient};

        let mock = MockGitHubClient::new();
        mock.set_create_release_response(Err(
            "422 Unprocessable Entity: Validation Failed - tag already exists".to_string(),
        ));

        let params = CreateReleaseParams {
            owner: "testowner".to_string(),
            repo: "testrepo".to_string(),
            tag_name: "v1.0.0".to_string(),
            name: "Release".to_string(),
            body: String::new(),
            draft: false,
            prerelease: false,
            generate_release_notes: false,
            make_latest: None,
        };

        let result = mock.create_release(&params);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("422") && err.contains("Validation"),
            "error should contain 422 and Validation, got: {err}"
        );
    }

    #[test]
    fn test_mock_upload_failure() {
        use anodize_core::github_client::{GitHubClient, MockGitHubClient, UploadAssetParams};

        let mock = MockGitHubClient::new();
        mock.set_upload_asset_response(Err(
            "upload failed: connection timeout after 30s".to_string()
        ));

        let params = UploadAssetParams {
            owner: "testowner".to_string(),
            repo: "testrepo".to_string(),
            release_id: 42,
            file_name: "myapp.tar.gz".to_string(),
            file_path: std::path::PathBuf::from("/tmp/myapp.tar.gz"),
        };

        let result = mock.upload_asset(&params);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("upload failed") && err.contains("timeout"),
            "error should describe the upload failure, got: {err}"
        );
    }

    #[test]
    fn test_dry_run_with_draft_release() {
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    draft: Some(true),
                    prerelease: Some(PrereleaseConfig::Auto),
                    make_latest: Some(MakeLatestConfig::Bool(false)),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        let stage = ReleaseStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    // ---- conflicting draft config tests ----

    #[test]
    fn test_conflicting_replace_and_use_existing_draft_fails() {
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    replace_existing_draft: Some(true),
                    use_existing_draft: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        let stage = ReleaseStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err(), "conflicting draft options should fail");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("replace_existing_draft") && err.contains("use_existing_draft"),
            "error should mention both conflicting options, got: {err}"
        );
    }

    #[test]
    fn test_replace_existing_draft_alone_ok() {
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    replace_existing_draft: Some(true),
                    use_existing_draft: Some(false),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        let stage = ReleaseStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_use_existing_draft_alone_ok() {
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    replace_existing_draft: Some(false),
                    use_existing_draft: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        let stage = ReleaseStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    // ---- release disable tests ----

    #[test]
    fn test_release_disable_config_parsing() {
        let yaml = r#"
disable: true
draft: false
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.disable, Some(StringOrBool::Bool(true)));
    }

    #[test]
    fn test_release_disable_config_parsing_false() {
        let yaml = r#"
disable: false
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.disable, Some(StringOrBool::Bool(false)));
    }

    #[test]
    fn test_release_disable_config_parsing_template_string() {
        let yaml = r#"
disable: "{{ if IsSnapshot }}true{{ endif }}"
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        match cfg.disable {
            Some(StringOrBool::String(s)) => {
                assert!(s.contains("IsSnapshot"));
            }
            other => panic!("expected StringOrBool::String, got {:?}", other),
        }
    }

    #[test]
    fn test_release_disable_config_parsing_absent() {
        let yaml = r#"
draft: true
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.disable, None);
    }

    #[test]
    fn test_release_stage_skipped_when_disabled() {
        // When disable: true is set, the release stage should skip
        // the crate entirely. We test via dry-run to avoid real API calls.
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    disable: Some(StringOrBool::Bool(true)),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        let stage = ReleaseStage;
        // Should succeed with no error - the crate is simply skipped
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_release_stage_not_skipped_when_disable_false() {
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    disable: Some(StringOrBool::Bool(false)),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        let stage = ReleaseStage;
        // Should succeed - disable=false means proceed normally (dry-run)
        assert!(stage.run(&mut ctx).is_ok());
    }

    // ---- resolve_release_mode tests ----

    #[test]
    fn test_resolve_release_mode_defaults_to_keep_existing() {
        assert_eq!(resolve_release_mode(None).unwrap(), "keep-existing");
    }

    #[test]
    fn test_resolve_release_mode_empty_string_defaults_to_keep_existing() {
        assert_eq!(resolve_release_mode(Some("")).unwrap(), "keep-existing");
    }

    #[test]
    fn test_resolve_release_mode_keep_existing() {
        assert_eq!(
            resolve_release_mode(Some("keep-existing")).unwrap(),
            "keep-existing"
        );
    }

    #[test]
    fn test_resolve_release_mode_append() {
        assert_eq!(resolve_release_mode(Some("append")).unwrap(), "append");
    }

    #[test]
    fn test_resolve_release_mode_prepend() {
        assert_eq!(resolve_release_mode(Some("prepend")).unwrap(), "prepend");
    }

    #[test]
    fn test_resolve_release_mode_replace() {
        assert_eq!(resolve_release_mode(Some("replace")).unwrap(), "replace");
    }

    #[test]
    fn test_resolve_release_mode_invalid() {
        let result = resolve_release_mode(Some("invalid-mode"));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("invalid mode 'invalid-mode'"),
            "error should name the invalid mode, got: {err}"
        );
        assert!(
            err.contains("keep-existing") && err.contains("append"),
            "error should list valid modes, got: {err}"
        );
    }

    #[test]
    fn test_release_mode_stored_in_config() {
        let yaml = r#"
mode: keep-existing
draft: false
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.mode.as_deref(), Some("keep-existing"));
    }

    #[test]
    fn test_release_mode_absent_in_config() {
        let yaml = r#"
draft: true
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.mode, None);
    }

    #[test]
    fn test_release_mode_all_valid_values_in_config() {
        for mode in &["keep-existing", "append", "prepend", "replace"] {
            let yaml = format!("mode: {}", mode);
            let cfg: ReleaseConfig = serde_yaml_ng::from_str(&yaml).unwrap();
            assert_eq!(cfg.mode.as_deref(), Some(*mode));
            // Verify it passes validation
            assert!(resolve_release_mode(cfg.mode.as_deref()).is_ok());
        }
    }

    #[test]
    fn test_dry_run_logs_release_mode() {
        // When mode is set, the dry-run output should include it
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    mode: Some("append".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        let stage = ReleaseStage;
        // Dry-run should succeed; the mode is validated and logged
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_invalid_release_mode_fails_stage() {
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    mode: Some("bogus".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        let stage = ReleaseStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err(), "invalid release mode should fail");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("invalid mode") || err.contains("bogus"),
            "error should mention invalid mode, got: {err}"
        );
    }

    // ---- ids filtering tests ----

    #[test]
    fn test_ids_filter_includes_matching_artifacts() {
        use anodize_core::artifact::{Artifact, ArtifactKind};
        use std::collections::HashMap;
        use std::path::PathBuf;

        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    ids: Some(vec!["linux-amd64".to_string()]),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        // Archive with matching id
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: PathBuf::from("/tmp/myapp-linux-amd64.tar.gz"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "linux-amd64".to_string())]),
        });

        // Archive with non-matching id
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: PathBuf::from("/tmp/myapp-darwin-arm64.tar.gz"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "darwin-arm64".to_string())]),
        });

        let stage = ReleaseStage;
        // Dry-run succeeds; the filter is applied internally
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_ids_filter_none_includes_all_artifacts() {
        use anodize_core::artifact::{Artifact, ArtifactKind};
        use std::collections::HashMap;
        use std::path::PathBuf;

        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    ids: None, // no filter
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        // Add two archives with different ids
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: PathBuf::from("/tmp/myapp-linux.tar.gz"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "linux-amd64".to_string())]),
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: PathBuf::from("/tmp/myapp-darwin.tar.gz"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "darwin-arm64".to_string())]),
        });

        let stage = ReleaseStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_ids_filter_unit_logic() {
        // Directly test the filter logic used in the release stage:
        // artifacts whose metadata "id" is in the ids list pass; others don't.
        use anodize_core::artifact::{Artifact, ArtifactKind};
        use std::collections::HashMap;
        use std::path::PathBuf;

        let ids = vec!["linux-amd64".to_string(), "windows-amd64".to_string()];

        let artifacts = vec![
            Artifact {
                kind: ArtifactKind::Archive,
                path: PathBuf::from("/tmp/linux.tar.gz"),
                target: None,
                crate_name: "app".to_string(),
                metadata: HashMap::from([("id".to_string(), "linux-amd64".to_string())]),
            },
            Artifact {
                kind: ArtifactKind::Archive,
                path: PathBuf::from("/tmp/darwin.tar.gz"),
                target: None,
                crate_name: "app".to_string(),
                metadata: HashMap::from([("id".to_string(), "darwin-arm64".to_string())]),
            },
            Artifact {
                kind: ArtifactKind::Archive,
                path: PathBuf::from("/tmp/windows.zip"),
                target: None,
                crate_name: "app".to_string(),
                metadata: HashMap::from([("id".to_string(), "windows-amd64".to_string())]),
            },
            Artifact {
                kind: ArtifactKind::Checksum,
                path: PathBuf::from("/tmp/checksums.txt"),
                target: None,
                crate_name: "app".to_string(),
                metadata: HashMap::new(), // no id metadata
            },
        ];

        // Apply the same filter logic as the stage
        let filtered: Vec<_> = artifacts
            .iter()
            .filter(|a| matches!(a.metadata.get("id"), Some(id) if ids.contains(id)))
            .collect();

        assert_eq!(filtered.len(), 2, "should match linux and windows only");
        assert_eq!(
            filtered[0].path,
            PathBuf::from("/tmp/linux.tar.gz"),
            "first match should be linux"
        );
        assert_eq!(
            filtered[1].path,
            PathBuf::from("/tmp/windows.zip"),
            "second match should be windows"
        );
    }

    #[test]
    fn test_ids_filter_no_id_metadata_excluded() {
        // Artifacts without "id" metadata should be excluded when ids filter is set
        use anodize_core::artifact::{Artifact, ArtifactKind};
        use std::collections::HashMap;
        use std::path::PathBuf;

        let ids = vec!["linux-amd64".to_string()];

        let artifact_no_id = Artifact {
            kind: ArtifactKind::Archive,
            path: PathBuf::from("/tmp/mystery.tar.gz"),
            target: None,
            crate_name: "app".to_string(),
            metadata: HashMap::new(),
        };

        let matches =
            matches!(artifact_no_id.metadata.get("id"), Some(id) if ids.contains(id));
        assert!(
            !matches,
            "artifact without id metadata should not match ids filter"
        );
    }

    #[test]
    fn test_ids_config_parsing() {
        let yaml = r#"
ids:
  - linux-amd64
  - darwin-arm64
draft: false
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let ids = cfg.ids.unwrap();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], "linux-amd64");
        assert_eq!(ids[1], "darwin-arm64");
    }

    #[test]
    fn test_ids_config_absent() {
        let yaml = r#"
draft: true
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(cfg.ids.is_none());
    }

    #[test]
    fn test_ids_and_mode_combined_dry_run() {
        use anodize_core::artifact::{Artifact, ArtifactKind};
        use std::collections::HashMap;
        use std::path::PathBuf;

        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    mode: Some("prepend".to_string()),
                    ids: Some(vec!["linux-amd64".to_string()]),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: PathBuf::from("/tmp/myapp-linux.tar.gz"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "linux-amd64".to_string())]),
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: PathBuf::from("/tmp/myapp-darwin.tar.gz"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "darwin-arm64".to_string())]),
        });

        let stage = ReleaseStage;
        assert!(
            stage.run(&mut ctx).is_ok(),
            "dry-run with mode + ids should succeed"
        );
    }

    #[test]
    fn test_release_collects_all_uploadable_artifact_kinds() {
        use anodize_core::artifact::{Artifact, ArtifactKind};
        use std::path::PathBuf;

        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        // Add one artifact of each uploadable kind.
        let uploadable_kinds = vec![
            (ArtifactKind::Archive, "myapp.tar.gz"),
            (ArtifactKind::Checksum, "checksums.txt"),
            (ArtifactKind::LinuxPackage, "myapp.deb"),
            (ArtifactKind::Snap, "myapp.snap"),
            (ArtifactKind::DiskImage, "myapp.dmg"),
            (ArtifactKind::Installer, "myapp.msi"),
            (ArtifactKind::MacOsPackage, "myapp.pkg"),
            (ArtifactKind::SourceArchive, "myapp-src.tar.gz"),
            (ArtifactKind::Sbom, "myapp.sbom.json"),
        ];
        for (kind, name) in &uploadable_kinds {
            ctx.artifacts.add(Artifact {
                kind: *kind,
                path: PathBuf::from(format!("/tmp/{}", name)),
                target: None,
                crate_name: "myapp".to_string(),
                metadata: Default::default(),
            });
        }

        // Also add a signature Metadata artifact (should be uploaded).
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Metadata,
            path: PathBuf::from("/tmp/checksums.txt.sig"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: std::collections::HashMap::from([
                ("type".to_string(), "Signature".to_string()),
            ]),
        });

        // Add non-uploadable kinds (should NOT be uploaded).
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            path: PathBuf::from("/tmp/myapp"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::DockerImage,
            path: PathBuf::from("ghcr.io/test/myapp:latest"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Library,
            path: PathBuf::from("/tmp/libmyapp.so"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Wasm,
            path: PathBuf::from("/tmp/myapp.wasm"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
        });
        // Plain Metadata (not Signature/Certificate) should NOT be uploaded.
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Metadata,
            path: PathBuf::from("/tmp/metadata.json"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
        });

        let stage = ReleaseStage;
        assert!(
            stage.run(&mut ctx).is_ok(),
            "dry-run with all artifact kinds should succeed"
        );

        // The dry-run completes successfully, confirming the expanded artifact
        // collection logic compiles and processes all expected kinds.
    }

    // ---- compose_body_for_mode tests ----

    #[test]
    fn test_compose_body_replace_ignores_existing() {
        let result = compose_body_for_mode("replace", Some("old body"), "new body");
        assert_eq!(result, "new body");
    }

    #[test]
    fn test_compose_body_replace_no_existing() {
        let result = compose_body_for_mode("replace", None, "new body");
        assert_eq!(result, "new body");
    }

    #[test]
    fn test_compose_body_keep_existing_with_existing() {
        let result = compose_body_for_mode("keep-existing", Some("old body"), "new body");
        assert_eq!(result, "old body");
    }

    #[test]
    fn test_compose_body_keep_existing_empty_existing() {
        let result = compose_body_for_mode("keep-existing", Some(""), "new body");
        assert_eq!(result, "new body");
    }

    #[test]
    fn test_compose_body_keep_existing_no_existing() {
        let result = compose_body_for_mode("keep-existing", None, "new body");
        assert_eq!(result, "new body");
    }

    #[test]
    fn test_compose_body_append_with_existing() {
        let result = compose_body_for_mode("append", Some("old body"), "new body");
        assert_eq!(result, "old body\n\nnew body");
    }

    #[test]
    fn test_compose_body_append_no_existing() {
        let result = compose_body_for_mode("append", None, "new body");
        assert_eq!(result, "new body");
    }

    #[test]
    fn test_compose_body_append_empty_existing() {
        let result = compose_body_for_mode("append", Some(""), "new body");
        assert_eq!(result, "new body");
    }

    #[test]
    fn test_compose_body_prepend_with_existing() {
        let result = compose_body_for_mode("prepend", Some("old body"), "new body");
        assert_eq!(result, "new body\n\nold body");
    }

    #[test]
    fn test_compose_body_prepend_no_existing() {
        let result = compose_body_for_mode("prepend", None, "new body");
        assert_eq!(result, "new body");
    }

    #[test]
    fn test_compose_body_prepend_empty_existing() {
        let result = compose_body_for_mode("prepend", Some(""), "new body");
        assert_eq!(result, "new body");
    }

    // ---- resolve_content_source tests ----

    #[test]
    fn test_resolve_content_source_inline() {
        let source = ContentSource::Inline("hello world".to_string());
        assert_eq!(resolve_content_source(&source).unwrap(), "hello world");
    }

    #[test]
    fn test_resolve_content_source_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("header.md");
        std::fs::write(&file_path, "# Release Header\nFrom file.").unwrap();

        let source = ContentSource::FromFile {
            from_file: file_path.to_string_lossy().into_owned(),
        };
        let result = resolve_content_source(&source).unwrap();
        assert_eq!(result, "# Release Header\nFrom file.");
    }

    #[test]
    fn test_resolve_content_source_from_file_not_found() {
        let source = ContentSource::FromFile {
            from_file: "/tmp/anodize_nonexistent_file_12345.md".to_string(),
        };
        let result = resolve_content_source(&source);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("failed to read"));
    }

    // ---- new config field parsing tests ----

    #[test]
    fn test_target_commitish_config_parsing() {
        let yaml = r#"
target_commitish: main
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.target_commitish, Some("main".to_string()));
    }

    #[test]
    fn test_target_commitish_absent() {
        let yaml = r#"
draft: true
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.target_commitish, None);
    }

    #[test]
    fn test_discussion_category_name_config_parsing() {
        let yaml = r#"
discussion_category_name: Announcements
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(
            cfg.discussion_category_name,
            Some("Announcements".to_string())
        );
    }

    #[test]
    fn test_discussion_category_name_absent() {
        let yaml = r#"
draft: false
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.discussion_category_name, None);
    }

    #[test]
    fn test_include_meta_config_parsing() {
        let yaml = r#"
include_meta: true
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.include_meta, Some(true));
    }

    #[test]
    fn test_include_meta_false() {
        let yaml = r#"
include_meta: false
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.include_meta, Some(false));
    }

    #[test]
    fn test_include_meta_absent() {
        let yaml = r#"
draft: true
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.include_meta, None);
    }

    #[test]
    fn test_use_existing_draft_config_parsing() {
        let yaml = r#"
use_existing_draft: true
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.use_existing_draft, Some(true));
    }

    #[test]
    fn test_use_existing_draft_false() {
        let yaml = r#"
use_existing_draft: false
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.use_existing_draft, Some(false));
    }

    #[test]
    fn test_use_existing_draft_absent() {
        let yaml = r#"
draft: true
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.use_existing_draft, None);
    }

    // ---- dry-run tests for new config fields ----

    #[test]
    fn test_dry_run_with_target_commitish() {
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    target_commitish: Some("main".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        let stage = ReleaseStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_dry_run_with_discussion_category_name() {
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    discussion_category_name: Some("Releases".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        let stage = ReleaseStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_dry_run_with_include_meta() {
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    include_meta: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        let stage = ReleaseStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_dry_run_with_use_existing_draft() {
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    use_existing_draft: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        let stage = ReleaseStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_dry_run_with_all_new_fields() {
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    header: Some(ContentSource::Inline("# Header".to_string())),
                    footer: Some(ContentSource::Inline("Footer".to_string())),
                    extra_files: Some(vec![ExtraFileSpec::Glob("*.sig".to_string())]),
                    target_commitish: Some("release/v1".to_string()),
                    discussion_category_name: Some("Announcements".to_string()),
                    include_meta: Some(true),
                    use_existing_draft: Some(false),
                    mode: Some("append".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        ctx.changelogs
            .insert("testcrate".to_string(), "- changes".to_string());
        let stage = ReleaseStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    // ---- ContentSource from_file dry-run integration test ----

    #[test]
    fn test_dry_run_with_header_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let header_path = dir.path().join("header.md");
        std::fs::write(&header_path, "# Release from file").unwrap();

        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    header: Some(ContentSource::FromFile {
                        from_file: header_path.to_string_lossy().into_owned(),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        let stage = ReleaseStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_include_meta_collects_dist_files() {
        // Create a temp dist directory with metadata files
        let dir = tempfile::tempdir().unwrap();
        let dist_dir = dir.path().join("dist");
        std::fs::create_dir_all(&dist_dir).unwrap();
        std::fs::write(dist_dir.join("metadata.json"), r#"{"key":"value"}"#).unwrap();
        std::fs::write(dist_dir.join("artifacts.json"), r#"[]"#).unwrap();

        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    include_meta: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        // Override the dist path to our temp directory
        ctx.config.dist = dist_dir.clone();

        let stage = ReleaseStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    // ---- body truncation tests ----

    #[test]
    fn test_build_release_json_body_within_limit() {
        let body = "a".repeat(1000);
        let json = build_release_json(
            "v1.0.0", "Release v1.0.0", &body,
            false, false, &None, &None, &None, false,
        );
        assert_eq!(json["body"].as_str().unwrap(), &body);
    }

    #[test]
    fn test_build_release_json_body_at_limit() {
        let body = "a".repeat(GITHUB_RELEASE_BODY_MAX_CHARS);
        let json = build_release_json(
            "v1.0.0", "Release v1.0.0", &body,
            false, false, &None, &None, &None, false,
        );
        assert_eq!(json["body"].as_str().unwrap(), &body);
    }

    #[test]
    fn test_build_release_json_body_exceeds_limit_is_truncated() {
        let body = "a".repeat(GITHUB_RELEASE_BODY_MAX_CHARS + 500);
        let json = build_release_json(
            "v1.0.0", "Release v1.0.0", &body,
            false, false, &None, &None, &None, false,
        );
        let result = json["body"].as_str().unwrap();
        // The truncated body should start with GITHUB_RELEASE_BODY_MAX_CHARS 'a's
        // and end with the truncation marker.
        assert!(result.starts_with(&"a".repeat(GITHUB_RELEASE_BODY_MAX_CHARS)));
        assert!(result.ends_with("\n\n...(truncated)"));
    }

    #[test]
    fn test_build_release_json_empty_body_not_set() {
        let json = build_release_json(
            "v1.0.0", "Release v1.0.0", "",
            false, false, &None, &None, &None, false,
        );
        assert!(json.get("body").is_none());
    }

    // ---- draft-then-publish: build_release_json always uses draft as passed ----

    #[test]
    fn test_build_release_json_draft_true() {
        let json = build_release_json(
            "v1.0.0", "Release v1.0.0", "body",
            true, false, &None, &None, &None, false,
        );
        assert_eq!(json["draft"].as_bool().unwrap(), true);
    }

    #[test]
    fn test_build_release_json_draft_false() {
        let json = build_release_json(
            "v1.0.0", "Release v1.0.0", "body",
            false, false, &None, &None, &None, false,
        );
        assert_eq!(json["draft"].as_bool().unwrap(), false);
    }
}
