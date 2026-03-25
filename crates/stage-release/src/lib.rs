use anodize_core::artifact::ArtifactKind;
use anodize_core::config::PrereleaseConfig;
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
        let changelog_body = ctx.changelog.clone().unwrap_or_default();
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

        for crate_cfg in &crates {
            let release_cfg = crate_cfg.release.as_ref().unwrap();

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

            // Collect uploadable artifacts for this crate.
            let artifact_paths: Vec<std::path::PathBuf> = [
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

            if dry_run {
                eprintln!(
                    "[release] (dry-run) would create GitHub Release '{}' (tag={}, draft={}, prerelease={}) for crate '{}'",
                    release_name, tag, draft, prerelease, crate_cfg.name
                );
                for path in &artifact_paths {
                    eprintln!(
                        "[release] (dry-run)   would upload artifact: {}",
                        path.display()
                    );
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
            let rt = tokio::runtime::Runtime::new()
                .context("release: failed to create tokio runtime")?;

            rt.block_on(async {
                let octo = octocrab::Octocrab::builder()
                    .personal_token(token_str.clone())
                    .build()
                    .context("release: build octocrab client")?;

                // Create the release.
                let release = octo
                    .repos(&github.owner, &github.name)
                    .releases()
                    .create(&tag)
                    .name(&release_name)
                    .body(&changelog_body)
                    .draft(draft)
                    .prerelease(prerelease)
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

                // Upload each artifact.
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

                Ok::<(), anyhow::Error>(())
            })?;
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
    use anodize_core::config::PrereleaseConfig;
    use anodize_core::config::Config;
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
}
