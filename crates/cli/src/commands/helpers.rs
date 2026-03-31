use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::config::{Config, GitHubConfig, WorkspaceConfig};
use anodize_core::context::Context;
use anodize_core::git;
use anodize_core::log::StageLogger;
use anyhow::{Context as _, Result};
use std::collections::HashMap;
use std::path::Path;

/// Set a process-level environment variable.
///
/// # Safety contract
///
/// `std::env::set_var` is unsafe because it mutates global process state that
/// other threads may be reading concurrently.  This function must ONLY be
/// called during single-threaded pipeline setup (i.e., inside `setup_env`)
/// before any worker threads are spawned.  All later stages that need env
/// values should read from the `Context` template vars or pass them
/// explicitly via `Command::envs()`.
fn set_env_var_single_threaded(key: &str, value: &str) {
    // SAFETY: Caller guarantees no other threads exist yet.
    unsafe { std::env::set_var(key, value) };
}

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
pub fn resolve_git_context(
    ctx: &mut Context,
    config: &Config,
    log: &StageLogger,
) -> anyhow::Result<()> {
    // Warn on shallow clones where tag discovery may be incomplete.
    if git::is_shallow_clone() {
        eprintln!(
            "WARNING: shallow clone detected; tag discovery may be incomplete. Use `git fetch --unshallow` in CI."
        );
    }

    // Allow env var overrides for tag discovery (like GoReleaser's
    // GORELEASER_CURRENT_TAG / GORELEASER_PREVIOUS_TAG).
    let tag_override = std::env::var("ANODIZE_CURRENT_TAG")
        .ok()
        .filter(|s| !s.is_empty());

    let first_crate = ctx
        .options
        .selected_crates
        .first()
        .and_then(|name| config.crates.iter().find(|c| &c.name == name))
        .or_else(|| config.crates.first());

    if let Some(crate_cfg) = first_crate {
        let tag = if let Some(ref override_tag) = tag_override {
            log.verbose(&format!(
                "using ANODIZE_CURRENT_TAG override: {}",
                override_tag
            ));
            override_tag.clone()
        } else {
            let latest_tag = match git::find_latest_tag_matching(
                &crate_cfg.tag_template,
                config.git.as_ref(),
                Some(ctx.template_vars()),
            ) {
                Ok(found) => found,
                Err(e) => {
                    log.warn(&format!("error finding tags matching template: {e}"));
                    None
                }
            };
            match latest_tag {
                Some(t) => t,
                None => {
                    if ctx.options.snapshot {
                        log.warn("no git tags found, defaulting to v0.0.0 (snapshot mode).");
                        "v0.0.0".to_string()
                    } else if ctx.options.dry_run {
                        log.warn("no git tags found, defaulting to v0.0.0 (dry-run mode).");
                        "v0.0.0".to_string()
                    } else {
                        anyhow::bail!("no git tag found; create a tag or use --snapshot");
                    }
                }
            }
        };

        // Validate HEAD points at the tag (like GoReleaser's ErrWrongRef).
        // Skip this check for the synthetic v0.0.0 tag since it doesn't exist in git.
        let is_synthetic_tag = tag == "v0.0.0" && tag_override.is_none();
        if !is_synthetic_tag {
            if let Ok(false) = git::tag_points_at_head(&tag) {
                if !ctx.options.snapshot {
                    let head = git::get_short_commit().unwrap_or_else(|_| "unknown".to_string());
                    anyhow::bail!(
                        "tag {} does not point at HEAD ({}). Check out the tag or use --snapshot to skip this check.",
                        tag,
                        head
                    );
                }
            }
        }

        match git::detect_git_info(&tag) {
            Ok(mut git_info) => {
                // Validate dirty working tree: error in non-snapshot/non-dry-run mode,
                // matching GoReleaser's CheckDirty behavior.
                if git_info.dirty && !ctx.options.snapshot {
                    if ctx.options.dry_run {
                        log.warn(
                            "git is in a dirty state; run `git status` to see what changed."
                        );
                    } else {
                        anyhow::bail!(
                            "git is in a dirty state; run `git status` to see what changed. \
                             Use --snapshot to force."
                        );
                    }
                }

                // Allow ANODIZE_PREVIOUS_TAG env override for the previous tag.
                if let Ok(prev_override) = std::env::var("ANODIZE_PREVIOUS_TAG") {
                    log.verbose(&format!(
                        "using ANODIZE_PREVIOUS_TAG override: {}",
                        prev_override
                    ));
                    git_info.previous_tag = Some(prev_override);
                } else {
                    git_info.previous_tag = git::find_previous_tag(
                        &tag,
                        config.git.as_ref(),
                        Some(ctx.template_vars()),
                    )
                    .ok()
                    .flatten();
                }
                ctx.git_info = Some(git_info);
                ctx.populate_git_vars();
            }
            Err(e) => {
                if ctx.options.snapshot {
                    log.warn(&format!(
                        "could not detect git info in snapshot mode, using defaults: {e}"
                    ));
                    ctx.git_info = Some(git::GitInfo {
                        tag: tag.clone(),
                        commit: "none".to_string(),
                        short_commit: "none".to_string(),
                        branch: "none".to_string(),
                        dirty: true,
                        semver: git::SemVer {
                            major: 0,
                            minor: 0,
                            patch: 0,
                            prerelease: None,
                            build_metadata: None,
                        },
                        commit_date: String::new(),
                        commit_timestamp: String::new(),
                        previous_tag: None,
                        remote_url: String::new(),
                        summary: "snapshot".to_string(),
                        tag_subject: String::new(),
                        tag_contents: String::new(),
                        tag_body: String::new(),
                        first_commit: None,
                    });
                    ctx.populate_git_vars();
                } else {
                    return Err(anyhow::anyhow!("could not detect git info: {e}"));
                }
            }
        }
    } else {
        ctx.populate_git_vars();
    }
    Ok(())
}

/// Load process environment variables, `.env` files, and user-defined env vars
/// into the context's template variables.
///
/// Loading order (later wins):
/// 1. All process environment variables (`std::env::vars()`)
/// 2. Variables from `.env` files specified in config
/// 3. Explicit `env:` map entries from config
///
/// This ensures config-defined env vars always take precedence over process
/// environment, matching GoReleaser's behavior where all process env vars are
/// accessible in templates via `{{ .Env.VAR }}`.
pub fn setup_env(
    ctx: &mut Context,
    config: &Config,
    log: &anodize_core::log::StageLogger,
) -> anyhow::Result<()> {
    // Load ALL process environment variables first (lowest priority)
    for (key, value) in std::env::vars() {
        ctx.template_vars_mut().set_env(&key, &value);
    }

    // Load .env files into template context (overrides process env)
    if let Some(ref env_files) = config.env_files {
        let env_vars = anodize_core::config::load_env_files(env_files, log)
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        for (key, value) in &env_vars {
            ctx.template_vars_mut().set_env(key, value);
        }
    }

    // Populate user-defined env vars into template context (highest priority).
    // GoReleaser renders env values through the template engine.
    if let Some(ref env_map) = config.env {
        for (key, value) in env_map {
            let rendered = ctx.render_template(value).unwrap_or_else(|_| value.clone());
            ctx.template_vars_mut().set_env(key, &rendered);
            // Also set in the process environment so that child processes which
            // inherit env (docker, lipo, rustup, git, hook scripts) see these
            // values. Some commands use explicit `.envs()`, but many rely on
            // process-level inheritance.
            //
            // SAFETY: This is called during single-threaded pipeline setup in
            // `setup_env`, before any worker threads are spawned. No concurrent
            // readers of the process environment exist at this point.
            set_env_var_single_threaded(key, &rendered);
        }
    }

    // Early token presence check (like GoReleaser's ErrMissingToken in env.go).
    // Warn if no GitHub token is available and the pipeline will need one.
    // Don't hard-error — snapshot mode and dry-run can proceed without a token.
    let has_token = ctx.options.token.is_some()
        || std::env::var("ANODIZE_GITHUB_TOKEN").is_ok()
        || std::env::var("GITHUB_TOKEN").is_ok();
    if !has_token && !ctx.is_snapshot() {
        let needs_token = config.crates.iter().any(|c| c.release.is_some())
            && !ctx.should_skip("release");
        if needs_token {
            log.warn(
                "no GitHub token found; release/publish stages may fail. \
                 Set GITHUB_TOKEN or ANODIZE_GITHUB_TOKEN.",
            );
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
            name: String::new(),
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
