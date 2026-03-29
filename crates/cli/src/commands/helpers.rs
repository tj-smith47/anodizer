use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::config::{Config, GitHubConfig, WorkspaceConfig};
use anodize_core::context::Context;
use anodize_core::git;
use anodize_core::log::StageLogger;
use anyhow::{Context as _, Result};
use std::collections::HashMap;
use std::path::Path;

/// Apply a workspace's configuration overlay onto the top-level config.
///
/// - `crates` is always replaced.
/// - `changelog`, `signs`, `before`, and `after` replace when present.
/// - `env` is merged additively (workspace values override same-key top-level values).
pub fn apply_workspace_overlay(config: &mut Config, ws: &WorkspaceConfig) {
    config.crates = ws.crates.clone();
    if ws.changelog.is_some() {
        config.changelog = ws.changelog.clone();
    }
    if !ws.signs.is_empty() {
        config.signs = ws.signs.clone();
    }
    if ws.before.is_some() {
        config.before = ws.before.clone();
    }
    if ws.after.is_some() {
        config.after = ws.after.clone();
    }
    if let Some(ref env_map) = ws.env {
        let merged = config.env.get_or_insert_with(HashMap::new);
        for (k, v) in env_map {
            merged.insert(k.clone(), v.clone());
        }
    }
}

/// Resolve tag and populate git variables on the context.
///
/// Finds the first selected crate (or the first crate in config), looks up
/// the latest tag matching its `tag_template`, detects git info, and
/// populates the context's template variables.
pub fn resolve_git_context(ctx: &mut Context, config: &Config, log: &StageLogger) {
    let first_crate = ctx
        .options
        .selected_crates
        .first()
        .and_then(|name| config.crates.iter().find(|c| &c.name == name))
        .or_else(|| config.crates.first());

    if let Some(crate_cfg) = first_crate {
        let latest_tag = git::find_latest_tag_matching(&crate_cfg.tag_template)
            .ok()
            .flatten();
        let tag = latest_tag.clone().unwrap_or_else(|| "v0.0.0".to_string());

        match git::detect_git_info(&tag) {
            Ok(mut git_info) => {
                git_info.previous_tag = latest_tag;
                ctx.git_info = Some(git_info);
                ctx.populate_git_vars();
            }
            Err(e) => {
                log.warn(&format!("could not detect git info: {e}"));
                ctx.populate_git_vars();
            }
        }
    } else {
        ctx.populate_git_vars();
    }
}

/// Load `.env` files and populate user-defined env vars into the context's
/// template variables.
pub fn setup_env(
    ctx: &mut Context,
    config: &Config,
    log: &anodize_core::log::StageLogger,
) -> anyhow::Result<()> {
    // Load .env files into template context (not the process environment)
    if let Some(ref env_files) = config.env_files {
        let env_vars = anodize_core::config::load_env_files(env_files, log)
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        for (key, value) in &env_vars {
            ctx.template_vars_mut().set_env(key, value);
        }
    }

    // Populate user-defined env vars into template context
    if let Some(ref env_map) = config.env {
        for (key, value) in env_map {
            ctx.template_vars_mut().set_env(key, value);
        }
    }

    Ok(())
}

/// Auto-detect the GitHub owner/name from the git remote and fill in any crate
/// release configs that are missing the `github` section.
pub fn auto_detect_github(config: &mut Config, log: &StageLogger) {
    let detected_github = git::detect_github_repo().ok();
    for crate_cfg in &mut config.crates {
        if let Some(ref mut release) = crate_cfg.release
            && release.github.is_none()
        {
            if let Some((ref owner, ref name)) = detected_github {
                release.github = Some(GitHubConfig {
                    owner: owner.clone(),
                    name: name.clone(),
                });
            } else {
                log.warn("could not auto-detect GitHub repo from git remote");
            }
        }
    }
}

/// Load artifacts from dist/metadata.json into the context's artifact registry.
/// Used by `publish` and `announce` commands that run from a completed dist/.
pub fn load_artifacts_from_dist(ctx: &mut Context, dist: &Path) -> Result<()> {
    let metadata_path = dist.join("metadata.json");
    if !metadata_path.exists() {
        anyhow::bail!(
            "no metadata.json found in {}. Run a full release or merge first.",
            dist.display()
        );
    }

    let content = std::fs::read_to_string(&metadata_path)
        .with_context(|| format!("read {}", metadata_path.display()))?;

    #[derive(serde::Deserialize)]
    struct MetadataArtifact {
        kind: String,
        path: String,
        target: Option<String>,
        crate_name: String,
        #[serde(default)]
        metadata: HashMap<String, String>,
    }

    let artifacts: Vec<MetadataArtifact> = serde_json::from_str(&content)
        .with_context(|| format!("parse {}", metadata_path.display()))?;

    for a in artifacts {
        let kind = ArtifactKind::parse(&a.kind)
            .ok_or_else(|| anyhow::anyhow!("unknown artifact kind: {}", a.kind))?;
        ctx.artifacts.add(Artifact {
            kind,
            path: std::path::PathBuf::from(&a.path),
            target: a.target,
            crate_name: a.crate_name,
            metadata: a.metadata,
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodize_core::config::{ChangelogConfig, CrateConfig, SignConfig};

    fn make_crate(name: &str) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: format!("{}-v{{{{ .Version }}}}", name),
            ..Default::default()
        }
    }

    #[test]
    fn test_apply_workspace_overlay_replaces_crates() {
        let mut config = Config {
            project_name: "test".to_string(),
            crates: vec![make_crate("original")],
            ..Default::default()
        };
        let ws = WorkspaceConfig {
            name: "ws".to_string(),
            crates: vec![make_crate("ws-crate")],
            ..Default::default()
        };

        apply_workspace_overlay(&mut config, &ws);
        assert_eq!(config.crates.len(), 1);
        assert_eq!(config.crates[0].name, "ws-crate");
    }

    #[test]
    fn test_apply_workspace_overlay_merges_env() {
        let mut config = Config {
            project_name: "test".to_string(),
            env: Some(HashMap::from([
                ("SHARED".to_string(), "from-top".to_string()),
                ("TOP_ONLY".to_string(), "top-value".to_string()),
            ])),
            ..Default::default()
        };
        let ws = WorkspaceConfig {
            name: "ws".to_string(),
            crates: vec![],
            env: Some(HashMap::from([
                ("SHARED".to_string(), "from-ws".to_string()),
                ("WS_ONLY".to_string(), "ws-value".to_string()),
            ])),
            ..Default::default()
        };

        apply_workspace_overlay(&mut config, &ws);
        let env = config.env.as_ref().unwrap();
        assert_eq!(env.get("TOP_ONLY").unwrap(), "top-value");
        assert_eq!(env.get("SHARED").unwrap(), "from-ws");
        assert_eq!(env.get("WS_ONLY").unwrap(), "ws-value");
    }

    #[test]
    fn test_apply_workspace_overlay_replaces_signs() {
        let mut config = Config {
            project_name: "test".to_string(),
            signs: vec![SignConfig {
                cmd: Some("gpg".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let ws = WorkspaceConfig {
            name: "ws".to_string(),
            crates: vec![],
            signs: vec![SignConfig {
                cmd: Some("cosign".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };

        apply_workspace_overlay(&mut config, &ws);
        assert_eq!(config.signs.len(), 1);
        assert_eq!(config.signs[0].cmd.as_deref(), Some("cosign"));
    }

    #[test]
    fn test_apply_workspace_overlay_replaces_changelog() {
        let mut config = Config {
            project_name: "test".to_string(),
            changelog: Some(ChangelogConfig {
                sort: Some("asc".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ws = WorkspaceConfig {
            name: "ws".to_string(),
            crates: vec![],
            changelog: Some(ChangelogConfig {
                sort: Some("desc".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };

        apply_workspace_overlay(&mut config, &ws);
        assert_eq!(
            config.changelog.as_ref().unwrap().sort.as_deref(),
            Some("desc")
        );
    }

    #[test]
    fn test_apply_workspace_overlay_skips_none_fields() {
        let mut config = Config {
            project_name: "test".to_string(),
            changelog: Some(ChangelogConfig {
                sort: Some("asc".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ws = WorkspaceConfig {
            name: "ws".to_string(),
            crates: vec![],
            // changelog is None, should not overwrite
            ..Default::default()
        };

        apply_workspace_overlay(&mut config, &ws);
        // Original changelog preserved
        assert_eq!(
            config.changelog.as_ref().unwrap().sort.as_deref(),
            Some("asc")
        );
    }
}
