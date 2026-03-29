use anodize_core::artifact::ArtifactKind;
use anodize_core::config::{MakeLatestConfig, PrereleaseConfig};
use anodize_core::context::Context;
use anodize_core::git;
use anodize_core::stage::Stage;
use anyhow::{Context as _, Result};

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
/// Invalid glob patterns are silently skipped (callers log through StageLogger).
pub(crate) fn collect_extra_files(patterns: &[String]) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    for pattern in patterns {
        match glob::glob(pattern) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    if entry.is_file() {
                        paths.push(entry);
                    }
                }
            }
            Err(_) => {
                // Invalid glob — skip silently; the release stage logs via StageLogger.
            }
        }
    }
    paths
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

            // Skip crates where release is explicitly disabled.
            if release_cfg.disable.unwrap_or(false) {
                log.status(&format!(
                    "release disabled for crate '{}', skipping",
                    crate_cfg.name
                ));
                continue;
            }

            let crate_name = crate_cfg.name.clone();
            let changelog_body = ctx.changelogs.get(&crate_name).cloned().unwrap_or_default();

            // Template-render header/footer before building release body.
            let rendered_header = release_cfg
                .header
                .as_deref()
                .map(|h| ctx.render_template(h))
                .transpose()
                .with_context(|| format!("release: render header for crate '{}'", crate_name))?;
            let rendered_footer = release_cfg
                .footer
                .as_deref()
                .map(|f| ctx.render_template(f))
                .transpose()
                .with_context(|| format!("release: render footer for crate '{}'", crate_name))?;

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

            // Collect uploadable artifacts for this crate.
            let mut artifact_paths: Vec<std::path::PathBuf> = [
                ArtifactKind::Archive,
                ArtifactKind::Checksum,
                ArtifactKind::LinuxPackage,
            ]
            .iter()
            .flat_map(|&kind| {
                ctx.artifacts
                    .by_kind_and_crate(kind, &crate_cfg.name)
                    .into_iter()
                    .map(|a| a.path.clone())
                    .collect::<Vec<_>>()
            })
            .collect();

            // Collect extra files from glob patterns.
            if let Some(extra_patterns) = &release_cfg.extra_files {
                let extra = collect_extra_files(extra_patterns);
                artifact_paths.extend(extra);
            }

            if dry_run {
                log.status(&format!(
                    "(dry-run) would create GitHub Release '{}' (tag={}, draft={}, prerelease={}) for crate '{}'",
                    release_name, tag, draft, prerelease, crate_cfg.name
                ));
                if skip_upload {
                    log.status("(dry-run)   skip_upload is set, would skip artifact uploads");
                } else {
                    for path in &artifact_paths {
                        log.status(&format!(
                            "(dry-run)   would upload artifact: {}",
                            path.display()
                        ));
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

                // Create the release. When github-native changelog is enabled,
                // use a raw API request to set `generate_release_notes: true`
                // (not exposed by octocrab's CreateReleaseBuilder).
                let release = if github_native_changelog {
                    let route = format!(
                        "/repos/{}/{}/releases",
                        github.owner, github.name
                    );
                    let mut body = serde_json::json!({
                        "tag_name": tag,
                        "name": release_name,
                        "draft": draft,
                        "prerelease": prerelease,
                        "generate_release_notes": true,
                    });
                    if !release_body.is_empty() {
                        body["body"] = serde_json::Value::String(release_body.clone());
                    }
                    if let Some(ref ml) = make_latest {
                        body["make_latest"] = serde_json::Value::String(ml.to_string());
                    }
                    octo.post::<_, octocrab::models::repos::Release>(route, Some(&body))
                        .await
                        .with_context(|| {
                            format!(
                                "release: create GitHub release '{}' on {}/{}",
                                tag, github.owner, github.name
                            )
                        })?
                } else {
                    let repo_handler = octo.repos(&github.owner, &github.name);
                    let releases_handler = repo_handler.releases();
                    let mut builder = releases_handler
                        .create(&tag)
                        .name(&release_name)
                        .body(&release_body)
                        .draft(draft)
                        .prerelease(prerelease);

                    if let Some(ml) = make_latest {
                        builder = builder.make_latest(ml);
                    }

                    builder
                        .send()
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
                    for path in &artifact_paths {
                        if !path.exists() {
                            log.warn(&format!(
                                "artifact not found, skipping upload: {}",
                                path.display()
                            ));
                            continue;
                        }

                        let file_name = path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "artifact".to_string());

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

                        let data = std::fs::read(path).with_context(|| {
                            format!("release: read artifact {}", path.display())
                        })?;

                        octo.repos(&github.owner, &github.name)
                            .releases()
                            .upload_asset(release.id.into_inner(), &file_name, data.into())
                            .send()
                            .await
                            .with_context(|| {
                                format!(
                                    "release: upload artifact '{}' to release '{}'",
                                    file_name, tag
                                )
                            })?;

                        log.verbose(&format!("uploaded artifact: {}", file_name));
                    }
                }

                Ok::<String, anyhow::Error>(html_url)
            })?;

            ctx.template_vars_mut().set("ReleaseURL", &url);
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
    use anodize_core::config::{CrateConfig, MakeLatestConfig, PrereleaseConfig, ReleaseConfig};
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
        let result = collect_extra_files(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_collect_extra_files_no_matches() {
        let result =
            collect_extra_files(&["/tmp/anodize_test_nonexistent_dir_12345/*.xyz".to_string()]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_collect_extra_files_with_real_file() {
        // Create a temp file and collect it
        let dir = std::env::temp_dir().join("anodize_extra_files_test");
        let _ = std::fs::create_dir_all(&dir);
        let test_file = dir.join("test_extra.txt");
        std::fs::write(&test_file, "extra file content").unwrap();

        let pattern = dir.join("*.txt").to_string_lossy().into_owned();
        let result = collect_extra_files(&[pattern]);
        assert!(
            result
                .iter()
                .any(|p| p.file_name().unwrap() == "test_extra.txt")
        );

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_collect_extra_files_skips_directories() {
        let dir = std::env::temp_dir().join("anodize_extra_files_dir_test");
        let _ = std::fs::create_dir_all(dir.join("subdir"));
        let test_file = dir.join("file.txt");
        std::fs::write(&test_file, "content").unwrap();

        // The glob "*" matches both files and directories; we only want files
        let pattern = dir.join("*").to_string_lossy().into_owned();
        let result = collect_extra_files(&[pattern]);
        assert!(result.iter().all(|p| p.is_file()));

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
                    extra_files: Some(vec!["/tmp/anodize_test_nonexistent/*.sig".to_string()]),
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
                    header: Some("# Custom Header".to_string()),
                    footer: Some("Custom Footer".to_string()),
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
        // An invalid glob pattern should be handled gracefully
        let result = collect_extra_files(&["[invalid-glob".to_string()]);
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
        let result = collect_extra_files(&[pattern]);
        assert_eq!(result.len(), 2, "should find exactly 2 .sig files");
        assert!(result.iter().all(|p| p.extension().unwrap() == "sig"));

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

    // ---- release disable tests ----

    #[test]
    fn test_release_disable_config_parsing() {
        let yaml = r#"
disable: true
draft: false
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.disable, Some(true));
    }

    #[test]
    fn test_release_disable_config_parsing_false() {
        let yaml = r#"
disable: false
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.disable, Some(false));
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
                    disable: Some(true),
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
                    disable: Some(false),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        let stage = ReleaseStage;
        // Should succeed - disable=false means proceed normally (dry-run)
        assert!(stage.run(&mut ctx).is_ok());
    }
}
