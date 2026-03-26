use anodize_core::artifact::ArtifactKind;
use anodize_core::config::{MakeLatestConfig, PrereleaseConfig};
use anodize_core::context::Context;
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
pub fn should_mark_prerelease(config: &Option<PrereleaseConfig>, tag: &str) -> bool {
    match config {
        Some(PrereleaseConfig::Auto) => {
            let t = tag.to_ascii_lowercase();
            t.contains("-rc")
                || t.contains("-beta")
                || t.contains("-alpha")
                || t.contains("-dev")
        }
        Some(PrereleaseConfig::Bool(b)) => *b,
        None => false,
    }
}

// ---------------------------------------------------------------------------
// build_release_body
// ---------------------------------------------------------------------------

/// Construct the release body by wrapping the changelog with optional
/// header and footer from the release config.
pub fn build_release_body(
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
pub fn collect_extra_files(patterns: &[String]) -> Vec<std::path::PathBuf> {
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
            Err(e) => {
                eprintln!(
                    "[release] warning: invalid extra_files glob '{}': {}",
                    pattern, e
                );
            }
        }
    }
    paths
}

// ---------------------------------------------------------------------------
// resolve_make_latest
// ---------------------------------------------------------------------------

/// Convert our config's `MakeLatestConfig` into octocrab's `MakeLatest` enum.
pub fn resolve_make_latest(
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
        // Resolve the GitHub token once (CLI flag > env var).
        let token = ctx
            .options
            .token
            .clone()
            .or_else(|| std::env::var("GITHUB_TOKEN").ok());

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
        let rt = tokio::runtime::Runtime::new()
            .context("release: failed to create tokio runtime")?;

        for crate_cfg in &crates {
            let release_cfg = crate_cfg.release.as_ref().unwrap();
            let crate_name = crate_cfg.name.clone();
            let changelog_body = ctx.changelogs.get(&crate_name).cloned().unwrap_or_default();

            // Template-render header/footer before building release body.
            let rendered_header = release_cfg.header.as_deref()
                .map(|h| ctx.render_template(h))
                .transpose()
                .with_context(|| format!("release: render header for crate '{}'", crate_name))?;
            let rendered_footer = release_cfg.footer.as_deref()
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
                eprintln!(
                    "[release] (dry-run) would create GitHub Release '{}' (tag={}, draft={}, prerelease={}) for crate '{}'",
                    release_name, tag, draft, prerelease, crate_cfg.name
                );
                if skip_upload {
                    eprintln!(
                        "[release] (dry-run)   skip_upload is set, would skip artifact uploads"
                    );
                } else {
                    for path in &artifact_paths {
                        eprintln!(
                            "[release] (dry-run)   would upload artifact: {}",
                            path.display()
                        );
                    }
                }
                continue;
            }

            // Require a GitHub config block.
            let github = match &release_cfg.github {
                Some(g) => g.clone(),
                None => {
                    eprintln!(
                        "[release] no github config for crate '{}', skipping",
                        crate_cfg.name
                    );
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
                            eprintln!(
                                "[release] replacing existing draft release '{}' (id={})",
                                tag, existing.id
                            );
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

                // Create the release, wiring make_latest if configured.
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

                let release = builder
                    .send()
                    .await
                    .with_context(|| {
                        format!(
                            "release: create GitHub release '{}' on {}/{}",
                            tag, github.owner, github.name
                        )
                    })?;

                eprintln!(
                    "[release] created GitHub Release '{}' (id={}) on {}/{}",
                    release_name, release.id, github.owner, github.name
                );

                let html_url = release.html_url.to_string();

                // Upload each artifact (unless skip_upload is set).
                if skip_upload {
                    eprintln!("[release] skip_upload is set, skipping artifact uploads");
                } else {
                    for path in &artifact_paths {
                        if !path.exists() {
                            eprintln!(
                                "[release] warning: artifact not found, skipping upload: {}",
                                path.display()
                            );
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
                                    eprintln!(
                                        "[release] replacing existing artifact '{}'",
                                        file_name
                                    );
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

                        eprintln!("[release] uploaded artifact: {}", file_name);
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
mod tests {
    use super::*;
    use anodize_core::config::{Config, MakeLatestConfig, PrereleaseConfig};
    use anodize_core::context::{Context, ContextOptions};

    #[test]
    fn test_is_prerelease_auto_with_rc() {
        assert!(should_mark_prerelease(&Some(PrereleaseConfig::Auto), "v1.0.0-rc.1"));
    }

    #[test]
    fn test_is_prerelease_auto_stable() {
        assert!(!should_mark_prerelease(&Some(PrereleaseConfig::Auto), "v1.0.0"));
    }

    #[test]
    fn test_is_prerelease_explicit_true() {
        assert!(should_mark_prerelease(&Some(PrereleaseConfig::Bool(true)), "v1.0.0"));
    }

    #[test]
    fn test_is_prerelease_explicit_false() {
        assert!(!should_mark_prerelease(&Some(PrereleaseConfig::Bool(false)), "v1.0.0-rc.1"));
    }

    #[test]
    fn test_is_prerelease_none() {
        assert!(!should_mark_prerelease(&None, "v1.0.0"));
    }

    #[test]
    fn test_stage_skips_crate_without_release_config() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
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
        let result = collect_extra_files(&[
            "/tmp/anodize_test_nonexistent_dir_12345/*.xyz".to_string(),
        ]);
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
        assert!(result.iter().any(|p| p.file_name().unwrap() == "test_extra.txt"));

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
        use anodize_core::config::{CrateConfig, ReleaseConfig};

        let mut config = Config::default();
        config.project_name = "test".to_string();
        config.crates.push(CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                skip_upload: Some(true),
                draft: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        });

        let opts = ContextOptions {
            dry_run: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        let stage = ReleaseStage;
        // Dry-run should succeed even with skip_upload = true
        assert!(stage.run(&mut ctx).is_ok());
    }

    // ---- replace_existing_draft / replace_existing_artifacts config defaults ----

    #[test]
    fn test_replace_existing_draft_defaults() {
        use anodize_core::config::ReleaseConfig;
        let cfg = ReleaseConfig::default();
        assert_eq!(cfg.replace_existing_draft, None);
    }

    #[test]
    fn test_replace_existing_artifacts_defaults() {
        use anodize_core::config::ReleaseConfig;
        let cfg = ReleaseConfig::default();
        assert_eq!(cfg.replace_existing_artifacts, None);
    }

    // ---- integration-style dry-run tests ----

    #[test]
    fn test_dry_run_with_extra_files() {
        use anodize_core::config::{CrateConfig, ReleaseConfig};

        let mut config = Config::default();
        config.project_name = "test".to_string();
        config.crates.push(CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                extra_files: Some(vec![
                    "/tmp/anodize_test_nonexistent/*.sig".to_string(),
                ]),
                ..Default::default()
            }),
            ..Default::default()
        });

        let opts = ContextOptions {
            dry_run: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        let stage = ReleaseStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_dry_run_with_header_footer_in_changelog() {
        use anodize_core::config::{CrateConfig, ReleaseConfig};

        let mut config = Config::default();
        config.project_name = "test".to_string();
        config.crates.push(CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                header: Some("# Custom Header".to_string()),
                footer: Some("Custom Footer".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        });

        let opts = ContextOptions {
            dry_run: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.changelogs
            .insert("testcrate".to_string(), "- bug fix".to_string());
        let stage = ReleaseStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_dry_run_with_make_latest() {
        use anodize_core::config::{CrateConfig, ReleaseConfig};

        let mut config = Config::default();
        config.project_name = "test".to_string();
        config.crates.push(CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                make_latest: Some(MakeLatestConfig::Bool(true)),
                ..Default::default()
            }),
            ..Default::default()
        });

        let opts = ContextOptions {
            dry_run: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        let stage = ReleaseStage;
        assert!(stage.run(&mut ctx).is_ok());
    }
}
