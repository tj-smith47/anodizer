use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::config::{Config, ForceTokenKind, GitHubConfig, WorkspaceConfig};
use anodize_core::context::Context;
use anodize_core::git;
use anodize_core::log::StageLogger;
use anodize_core::scm::{self, ScmTokenType};
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

/// Collect all configured build targets from a config, in declaration order.
///
/// Iterates `config.crates` plus every `config.workspaces[].crates` so monorepos
/// with multi-root workspaces are covered. Per-crate `builds[].targets` entries
/// are collected first, then `defaults.targets`. Duplicates are filtered, and
/// `defaults.ignore` (os/arch pairs) removes matching targets.
///
/// `selected_crates` filters the iteration: when empty, all crates are used;
/// otherwise only crates whose `name` is in the slice contribute.
pub fn collect_build_targets(config: &Config, selected_crates: &[String]) -> Vec<String> {
    let mut targets: Vec<String> = Vec::new();

    let all_crates = config.crates.iter().chain(
        config
            .workspaces
            .as_deref()
            .unwrap_or_default()
            .iter()
            .flat_map(|w| w.crates.iter()),
    );

    for krate in all_crates {
        if !selected_crates.is_empty() && !selected_crates.contains(&krate.name) {
            continue;
        }

        if let Some(ref builds) = krate.builds {
            for build in builds {
                if let Some(ref build_targets) = build.targets {
                    for t in build_targets {
                        if !targets.contains(t) {
                            targets.push(t.clone());
                        }
                    }
                }
            }
        }

        if let Some(ref defaults) = config.defaults
            && let Some(ref default_targets) = defaults.targets
        {
            for t in default_targets {
                if !targets.contains(t) {
                    targets.push(t.clone());
                }
            }
        }
    }

    if let Some(ref defaults) = config.defaults
        && let Some(ref ignores) = defaults.ignore
    {
        targets.retain(|t| {
            let (os, arch) = anodize_core::target::map_target(t);
            !ignores.iter().any(|ig| ig.os == os && ig.arch == arch)
        });
    }

    targets
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
    if !ws.binary_signs.is_empty() {
        config.binary_signs = ws.binary_signs.clone();
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

    // Resolve a crate to derive the tag from. Selection order:
    //   1. The first explicitly selected crate (--crate or --all selection)
    //   2. The first top-level crate in config
    //   3. The first crate of the first workspace (workspace-only configs)
    //
    // The workspace fallback is critical for snapshot/dry-run mode in
    // workspace-only configs (e.g. cfgd) — without it, `Version` is never
    // populated in the template context, breaking any template that
    // references it.
    let first_crate = ctx
        .options
        .selected_crates
        .first()
        .and_then(|name| {
            config.crates.iter().find(|c| &c.name == name).or_else(|| {
                config.workspaces.as_ref().and_then(|ws_list| {
                    ws_list
                        .iter()
                        .flat_map(|w| w.crates.iter())
                        .find(|c| &c.name == name)
                })
            })
        })
        .or_else(|| config.crates.first())
        .or_else(|| {
            config
                .workspaces
                .as_ref()
                .and_then(|ws_list| ws_list.iter().flat_map(|w| w.crates.iter()).next())
        });

    if let Some(crate_cfg) = first_crate {
        let tag = if let Some(ref override_tag) = tag_override {
            log.verbose(&format!(
                "using ANODIZE_CURRENT_TAG override: {}",
                override_tag
            ));
            override_tag.clone()
        } else {
            let monorepo_prefix = config.monorepo_tag_prefix();
            let latest_tag = match git::find_latest_tag_matching_with_prefix(
                &crate_cfg.tag_template,
                config.git.as_ref(),
                Some(ctx.template_vars()),
                monorepo_prefix,
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
        if !is_synthetic_tag
            && let Ok(false) = git::tag_points_at_head(&tag)
            && !ctx.options.snapshot
        {
            let head = git::get_short_commit().unwrap_or_else(|_| "unknown".to_string());
            anyhow::bail!(
                "tag {} does not point at HEAD ({}). Check out the tag or use --snapshot to skip this check.",
                tag,
                head
            );
        }

        match git::detect_git_info(&tag, ctx.skip_validate()) {
            Ok(mut git_info) => {
                // Validate dirty working tree: error in non-snapshot/non-dry-run mode,
                // matching GoReleaser's CheckDirty behavior.
                if git_info.dirty && !ctx.options.snapshot {
                    if ctx.options.dry_run {
                        log.warn("git is in a dirty state; run `git status` to see what changed.");
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
                    // Derive the tag-prefix filter from the current crate's
                    // tag_template (e.g. `v` for cfgd, `csi-v` for cfgd-csi)
                    // so monorepo-style workspaces don't bleed prior tags
                    // across crates. Without this, `git describe --tags`
                    // returns the most recent tag of ANY crate — e.g.
                    // `cfgd: csi-v0.3.4 -> 0.3.5` ends up in the nix/
                    // homebrew commit message because csi was the most
                    // recently tagged sibling. Falls back to the global
                    // monorepo prefix when the template has no extractable
                    // prefix.
                    let crate_prefix = git::extract_tag_prefix(&crate_cfg.tag_template);
                    let prefix = crate_prefix
                        .as_deref()
                        .or_else(|| config.monorepo_tag_prefix());
                    git_info.previous_tag = git::find_previous_tag_with_prefix(
                        &tag,
                        config.git.as_ref(),
                        Some(ctx.template_vars()),
                        prefix,
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

    // Load env files into template context (overrides process env).
    // Supports both list form (array of .env files) and struct form (token file paths).
    // These are user-configured, so use set_config_env (safe for cross-platform
    // serialization and subprocess injection).
    if let Some(ref env_files_config) = config.env_files {
        match env_files_config {
            anodize_core::config::EnvFilesConfig::List(files) => {
                let env_vars = anodize_core::config::load_env_files(files, log, ctx.is_strict())
                    .map_err(|e| anyhow::anyhow!("{}", e))?;
                for (key, value) in &env_vars {
                    ctx.template_vars_mut().set_config_env(key, value);
                }
            }
            anodize_core::config::EnvFilesConfig::TokenFiles(token_config) => {
                let token_vars = anodize_core::config::load_token_files(token_config, log)
                    .map_err(|e| anyhow::anyhow!("{}", e))?;
                for (key, value) in &token_vars {
                    ctx.template_vars_mut().set_config_env(key, value);
                    set_env_var_single_threaded(key, value);
                }
            }
        }
    } else {
        // GoReleaser parity (env.go:setDefaultTokenFiles): always check default
        // token file paths even when env_files is not configured.
        let default_config = anodize_core::config::EnvFilesTokenConfig::default();
        let token_vars = anodize_core::config::load_token_files(&default_config, log)
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        for (key, value) in &token_vars {
            ctx.template_vars_mut().set_config_env(key, value);
            set_env_var_single_threaded(key, value);
        }
    }

    // Populate user-defined env vars into template context (highest priority).
    // GoReleaser renders env values through the template engine.
    if let Some(ref env_map) = config.env {
        for (key, value) in env_map {
            let rendered = ctx.render_template(value).unwrap_or_else(|_| value.clone());
            ctx.template_vars_mut().set_config_env(key, &rendered);
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

    // Populate user-defined custom variables into template context.
    if let Some(ref vars_map) = config.variables {
        for (key, value) in vars_map {
            // Render variable values through templates (they may reference env vars or other template vars)
            let rendered = ctx.render_template(value).unwrap_or_else(|_| value.clone());
            ctx.template_vars_mut().set_custom_var(key, &rendered);
        }
    }

    // GoReleaser env.go:75-86: when force_token is active, clear non-forced
    // token env vars BEFORE the multi-token check so it cannot fire.
    let resolved_force = config.force_token.as_ref().cloned().or_else(|| {
        let env_val = std::env::var("ANODIZE_FORCE_TOKEN")
            .ok()
            .or_else(|| std::env::var("GORELEASER_FORCE_TOKEN").ok())?;
        match env_val.to_lowercase().as_str() {
            "github" => Some(ForceTokenKind::GitHub),
            "gitlab" => Some(ForceTokenKind::GitLab),
            "gitea" => Some(ForceTokenKind::Gitea),
            _ => None,
        }
    });
    if let Some(ref forced) = resolved_force {
        // Remove env vars for non-forced token types so downstream code
        // only sees the forced provider's token.
        let keep_github = matches!(forced, ForceTokenKind::GitHub);
        let keep_gitlab = matches!(forced, ForceTokenKind::GitLab);
        let keep_gitea = matches!(forced, ForceTokenKind::Gitea);
        if !keep_github {
            // SAFETY: single-threaded pipeline setup, see set_env_var_single_threaded.
            unsafe {
                std::env::remove_var("GITHUB_TOKEN");
                std::env::remove_var("ANODIZE_GITHUB_TOKEN");
            }
        }
        if !keep_gitlab {
            unsafe { std::env::remove_var("GITLAB_TOKEN") };
        }
        if !keep_gitea {
            unsafe { std::env::remove_var("GITEA_TOKEN") };
        }
    }

    // Multiple-token detection (GoReleaser env.go:88-101 ErrMultipleTokens).
    // When multiple SCM tokens are set without force_token, error early.
    if resolved_force.is_none() {
        let has_github =
            std::env::var("GITHUB_TOKEN").is_ok() || std::env::var("ANODIZE_GITHUB_TOKEN").is_ok();
        let has_gitlab = std::env::var("GITLAB_TOKEN").is_ok();
        let has_gitea = std::env::var("GITEA_TOKEN").is_ok();
        let count = [has_github, has_gitlab, has_gitea]
            .iter()
            .filter(|&&b| b)
            .count();
        if count > 1 {
            anyhow::bail!(
                "multiple SCM tokens set simultaneously ({}). Set force_token in config \
                 or ANODIZE_FORCE_TOKEN env var to specify which to use.",
                [
                    if has_github {
                        Some("GITHUB_TOKEN")
                    } else {
                        None
                    },
                    if has_gitlab {
                        Some("GITLAB_TOKEN")
                    } else {
                        None
                    },
                    if has_gitea { Some("GITEA_TOKEN") } else { None },
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(", ")
            );
        }
    }

    // Missing token hard error (GoReleaser env.go:138-142 ErrMissingToken).
    // Error early if no SCM token and the pipeline needs one.
    // Snapshot mode, dry-run, and release.disable can proceed without a token.
    if ctx.options.token.is_none() && !ctx.is_snapshot() && !ctx.is_dry_run() {
        let release_disabled = config
            .crates
            .first()
            .and_then(|c| c.release.as_ref()?.disable.as_ref())
            .is_some_and(|d| d.is_disabled(|t| ctx.render_template(t)));
        let needs_token = config.crates.iter().any(|c| c.release.is_some())
            && !ctx.should_skip("release")
            && !release_disabled;
        if needs_token {
            let hint = match ctx.token_type {
                anodize_core::scm::ScmTokenType::GitLab => {
                    "no GitLab token found. Set GITLAB_TOKEN."
                }
                anodize_core::scm::ScmTokenType::Gitea => "no Gitea token found. Set GITEA_TOKEN.",
                anodize_core::scm::ScmTokenType::GitHub => {
                    "no GitHub token found. Set GITHUB_TOKEN or ANODIZE_GITHUB_TOKEN."
                }
            };
            anyhow::bail!("{}", hint);
        }
    }

    Ok(())
}

/// Write `dist/config.yaml` with the fully-resolved (effective) config.
///
/// GoReleaser always writes this, including in dry-run mode (effectiveconfig.go).
/// Shared by `release` and `build` pipelines so both surface the same artifact.
pub fn write_effective_config(config: &Config, log: &StageLogger) -> Result<()> {
    let dist = &config.dist;
    std::fs::create_dir_all(dist)
        .with_context(|| format!("failed to create dist directory: {}", dist.display()))?;
    let effective_path = dist.join("config.yaml");
    let yaml = serde_yaml_ng::to_string(config).context("failed to serialize effective config")?;
    std::fs::write(&effective_path, &yaml)
        .with_context(|| format!("failed to write {}", effective_path.display()))?;
    log.verbose(&format!(
        "wrote effective config to {}",
        effective_path.display()
    ));
    Ok(())
}

/// Print the artifact size report if `report_sizes` is enabled in config.
pub fn run_report_sizes(ctx: &mut Context, config: &Config, log: &StageLogger) {
    if config.report_sizes.unwrap_or(false) {
        anodize_core::artifact::print_size_report(&mut ctx.artifacts, log);
    }
}

/// Write `dist/metadata.json` and `dist/artifacts.json` and apply the
/// configured `metadata.mod_timestamp` to both files.
///
/// Mirrors GoReleaser's metadata.Pipe + artifacts.Pipe. Registers
/// `metadata.json` as an artifact so downstream stages can pick it up.
pub fn write_metadata_and_artifacts(
    ctx: &mut Context,
    config: &Config,
    log: &StageLogger,
) -> Result<()> {
    let dist = &config.dist;
    std::fs::create_dir_all(dist)
        .with_context(|| format!("failed to create dist directory: {}", dist.display()))?;

    let metadata_path = dist.join("metadata.json");
    let goos = anodize_core::context::map_os_to_goos(std::env::consts::OS);
    let goarch = anodize_core::context::map_arch_to_goarch(std::env::consts::ARCH);

    let tag = ctx.template_vars().get("Tag").cloned().unwrap_or_default();
    let previous_tag = ctx
        .template_vars()
        .get("PreviousTag")
        .cloned()
        .unwrap_or_default();
    let version = ctx.version();
    let commit = ctx
        .template_vars()
        .get("FullCommit")
        .cloned()
        .unwrap_or_default();
    let date = ctx.template_vars().get("Date").cloned().unwrap_or_default();

    let project_metadata = serde_json::json!({
        "project_name": config.project_name,
        "tag": tag,
        "previous_tag": previous_tag,
        "version": version,
        "commit": commit,
        "date": date,
        "runtime": {
            "goos": goos,
            "goarch": goarch,
        }
    });

    let json_str = serde_json::to_string_pretty(&project_metadata)
        .context("failed to serialize project metadata JSON")?;
    std::fs::write(&metadata_path, &json_str)
        .with_context(|| format!("failed to write {}", metadata_path.display()))?;
    log.status(&format!("wrote {}", metadata_path.display()));

    ctx.artifacts.add(anodize_core::artifact::Artifact {
        kind: ArtifactKind::Metadata,
        name: "metadata.json".to_string(),
        path: metadata_path.clone(),
        target: None,
        crate_name: config.project_name.clone(),
        metadata: Default::default(),
        size: None,
    });

    let artifacts_path = dist.join("artifacts.json");
    let artifacts_json = ctx
        .artifacts
        .to_artifacts_json()
        .context("failed to serialize artifact list")?;
    let json_str = serde_json::to_string_pretty(&artifacts_json)
        .context("failed to serialize artifacts JSON")?;
    std::fs::write(&artifacts_path, &json_str)
        .with_context(|| format!("failed to write {}", artifacts_path.display()))?;
    log.status(&format!("wrote {}", artifacts_path.display()));

    if let Some(ref meta) = config.metadata
        && let Some(ref ts_tmpl) = meta.mod_timestamp
    {
        let rendered = ctx
            .render_template(ts_tmpl)
            .context("failed to render metadata.mod_timestamp template")?;
        if !rendered.is_empty() {
            let mtime = anodize_core::util::parse_mod_timestamp(&rendered)
                .with_context(|| format!("invalid metadata.mod_timestamp value: {:?}", rendered))?;
            anodize_core::util::set_file_mtime(&metadata_path, mtime)?;
            anodize_core::util::set_file_mtime(&artifacts_path, mtime)?;
            log.status(&format!(
                "set mtime on metadata.json and artifacts.json to {}",
                rendered
            ));
        }
    }

    Ok(())
}

/// Auto-infer `project_name` from Cargo.toml when not set in config.
///
/// GoReleaser's project.go:22-43 infers the project name from Cargo.toml,
/// go.mod, or the git remote. We mirror the Cargo.toml branch here so
/// every pipeline command (release, build, check, continue) resolves the
/// project name consistently.
pub fn infer_project_name(config: &mut Config, log: &StageLogger) {
    if !config.project_name.is_empty() {
        return;
    }
    if let Ok(cargo_toml) = std::fs::read_to_string("Cargo.toml")
        && let Ok(doc) = cargo_toml.parse::<toml_edit::DocumentMut>()
        && let Some(name) = doc
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
    {
        config.project_name = name.to_string();
        log.verbose(&format!("inferred project_name '{}' from Cargo.toml", name));
    }
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

/// Perform the standard context setup sequence shared by all pipeline commands.
///
/// This encapsulates the boilerplate that every pipeline entry point
/// (release, publish, announce, continue) must run after constructing a
/// `Context`:
///   1. Resolve SCM token type from config/environment
///   2. Populate time template variables
///   3. Populate runtime template variables
///   4. Load environment variables and `.env` files
///   5. Resolve git context (tag discovery, git info)
pub fn setup_context(ctx: &mut Context, config: &Config, log: &StageLogger) -> Result<()> {
    resolve_scm_token_type(ctx, config);
    ctx.populate_time_vars();
    ctx.populate_runtime_vars();
    setup_env(ctx, config, log)?;
    resolve_git_context(ctx, config, log)?;
    Ok(())
}

/// Resolve the SCM token type and token value from config and environment.
///
/// This sets `ctx.token_type` based on priority (highest first):
/// 1. `config.force_token` — explicit user config (`force_token: gitlab`)
/// 2. `ANODIZE_FORCE_TOKEN` env var — e.g. `github`, `gitlab`, `gitea`
/// 3. `GORELEASER_FORCE_TOKEN` env var — GoReleaser compat fallback
/// 4. Environment variable presence — `GITLAB_TOKEN` → GitLab, `GITEA_TOKEN` → Gitea
/// 5. Default — GitHub
///
/// It also resolves the token value into `ctx.options.token` (if not already
/// set by a CLI flag) from the appropriate environment variable:
/// - GitLab: `GITLAB_TOKEN`
/// - Gitea: `GITEA_TOKEN`
/// - GitHub: `ANODIZE_GITHUB_TOKEN` or `GITHUB_TOKEN`
pub fn resolve_scm_token_type(ctx: &mut Context, config: &Config) {
    // Detect which SCM backend to use from environment variables.
    let env_hint = if std::env::var("GITLAB_TOKEN").is_ok() {
        Some("gitlab")
    } else if std::env::var("GITEA_TOKEN").is_ok() {
        Some("gitea")
    } else {
        None
    };

    // GoReleaser supports GORELEASER_FORCE_TOKEN env var as a fallback for the
    // config-level force_token field.  We support ANODIZE_FORCE_TOKEN (primary)
    // and GORELEASER_FORCE_TOKEN (compat).
    let force_token = config.force_token.as_ref().cloned().or_else(|| {
        let env_val = std::env::var("ANODIZE_FORCE_TOKEN")
            .ok()
            .or_else(|| std::env::var("GORELEASER_FORCE_TOKEN").ok())?;
        match env_val.to_lowercase().as_str() {
            "github" => Some(ForceTokenKind::GitHub),
            "gitlab" => Some(ForceTokenKind::GitLab),
            "gitea" => Some(ForceTokenKind::Gitea),
            _ => None,
        }
    });

    ctx.token_type = scm::resolve_token_type(force_token.as_ref(), env_hint);

    // Resolve the token value if not already provided via CLI flag.
    if ctx.options.token.is_none() {
        ctx.options.token = match ctx.token_type {
            ScmTokenType::GitLab => std::env::var("GITLAB_TOKEN").ok(),
            ScmTokenType::Gitea => std::env::var("GITEA_TOKEN").ok(),
            ScmTokenType::GitHub => std::env::var("ANODIZE_GITHUB_TOKEN")
                .ok()
                .or_else(|| std::env::var("GITHUB_TOKEN").ok()),
        };
    }
}

/// Load artifacts from dist/artifacts.json into the context's artifact registry.
/// Used by `publish` and `announce` commands that run from a completed dist/.
pub fn load_artifacts_from_dist(ctx: &mut Context, dist: &Path) -> Result<()> {
    let artifacts_path = dist.join("artifacts.json");
    if !artifacts_path.exists() {
        anyhow::bail!(
            "no artifacts.json found in {}. Run a full release or merge first.",
            dist.display()
        );
    }

    let content = std::fs::read_to_string(&artifacts_path)
        .with_context(|| format!("read {}", artifacts_path.display()))?;

    #[derive(serde::Deserialize)]
    struct MetadataArtifact {
        kind: String,
        #[serde(default)]
        name: Option<String>,
        path: String,
        target: Option<String>,
        crate_name: String,
        #[serde(default)]
        metadata: HashMap<String, String>,
        #[serde(default)]
        size: Option<u64>,
    }

    let artifacts: Vec<MetadataArtifact> = serde_json::from_str(&content)
        .with_context(|| format!("parse {}", artifacts_path.display()))?;

    for a in artifacts {
        let kind = ArtifactKind::parse(&a.kind)
            .ok_or_else(|| anyhow::anyhow!("unknown artifact kind: {}", a.kind))?;
        ctx.artifacts.add(Artifact {
            kind,
            name: a.name.unwrap_or_default(),
            path: std::path::PathBuf::from(&a.path),
            target: a.target,
            crate_name: a.crate_name,
            metadata: a.metadata,
            size: a.size,
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodize_core::config::{ChangelogConfig, CrateConfig, SignConfig};
    use anodize_core::context::ContextOptions;
    use anodize_core::scm::ScmTokenType;

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

    // -----------------------------------------------------------------------
    // load_artifacts_from_dist tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_load_artifacts_from_dist_valid() {
        use anodize_core::artifact::ArtifactKind;
        use anodize_core::context::{Context, ContextOptions};

        let dir = tempfile::TempDir::new().unwrap();
        let artifacts_json = serde_json::json!([
            {
                "kind": "binary",
                "name": "myapp",
                "path": "dist/myapp",
                "target": "x86_64-unknown-linux-gnu",
                "crate_name": "myapp",
                "metadata": {},
                "size": 4096
            },
            {
                "kind": "archive",
                "name": "myapp.tar.gz",
                "path": "dist/myapp.tar.gz",
                "target": null,
                "crate_name": "myapp",
                "metadata": {"format": "tar.gz"}
            }
        ]);
        std::fs::write(
            dir.path().join("artifacts.json"),
            serde_json::to_string_pretty(&artifacts_json).unwrap(),
        )
        .unwrap();

        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        load_artifacts_from_dist(&mut ctx, dir.path()).unwrap();

        let all = ctx.artifacts.all();
        assert_eq!(all.len(), 2);

        assert_eq!(all[0].kind, ArtifactKind::Binary);
        assert_eq!(all[0].name, "myapp");
        assert_eq!(
            all[0].size,
            Some(4096),
            "size should be preserved from JSON"
        );

        assert_eq!(all[1].kind, ArtifactKind::Archive);
        assert_eq!(all[1].name, "myapp.tar.gz");
        assert_eq!(
            all[1].metadata.get("format").map(|s| s.as_str()),
            Some("tar.gz")
        );
        assert_eq!(
            all[1].size, None,
            "size should be None when absent from JSON"
        );
    }

    #[test]
    fn test_load_artifacts_from_dist_missing_file() {
        use anodize_core::context::{Context, ContextOptions};

        let dir = tempfile::TempDir::new().unwrap();
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let result = load_artifacts_from_dist(&mut ctx, dir.path());
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("no artifacts.json found"),
            "error should mention missing file: {msg}"
        );
    }

    #[test]
    fn test_load_artifacts_from_dist_invalid_json() {
        use anodize_core::context::{Context, ContextOptions};

        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("artifacts.json"), "not valid json").unwrap();

        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let result = load_artifacts_from_dist(&mut ctx, dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn test_load_artifacts_from_dist_unknown_kind() {
        use anodize_core::context::{Context, ContextOptions};

        let dir = tempfile::TempDir::new().unwrap();
        let artifacts_json = serde_json::json!([
            {
                "kind": "unknown_kind",
                "name": "thing",
                "path": "dist/thing",
                "target": null,
                "crate_name": "myapp",
                "metadata": {}
            }
        ]);
        std::fs::write(
            dir.path().join("artifacts.json"),
            serde_json::to_string_pretty(&artifacts_json).unwrap(),
        )
        .unwrap();

        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let result = load_artifacts_from_dist(&mut ctx, dir.path());
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("unknown artifact kind"),
            "error should mention unknown kind: {msg}"
        );
    }

    #[test]
    fn test_load_artifacts_from_dist_roundtrip() {
        use anodize_core::artifact::{Artifact, ArtifactKind, ArtifactRegistry};
        use anodize_core::context::{Context, ContextOptions};

        // Build an artifact registry, serialize, write, then load back
        let mut registry = ArtifactRegistry::new();
        registry.add(Artifact {
            kind: ArtifactKind::Checksum,
            name: String::new(),
            path: std::path::PathBuf::from("dist/checksums.txt"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: Some(256),
        });
        registry.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let json_val = registry.to_artifacts_json().unwrap();
        let json_str = serde_json::to_string_pretty(&json_val).unwrap();

        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("artifacts.json"), &json_str).unwrap();

        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        load_artifacts_from_dist(&mut ctx, dir.path()).unwrap();

        let loaded = ctx.artifacts.all();
        assert_eq!(loaded.len(), 2);

        assert_eq!(loaded[0].kind, ArtifactKind::Checksum);
        assert_eq!(loaded[0].name, "checksums.txt");
        assert_eq!(loaded[0].size, Some(256));

        assert_eq!(loaded[1].kind, ArtifactKind::Binary);
        assert_eq!(loaded[1].name, "myapp");
        assert_eq!(loaded[1].target.as_deref(), Some("aarch64-apple-darwin"));
        assert_eq!(loaded[1].size, None);
    }

    // -----------------------------------------------------------------------
    // resolve_scm_token_type tests
    // -----------------------------------------------------------------------

    /// Mutex to serialize tests that mutate process environment variables.
    /// cargo test runs tests in parallel within a single process, so
    /// concurrent env mutations cause flaky failures without serialization.
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Helper to run resolve_scm_token_type tests with controlled env state.
    /// Acquires ENV_MUTEX, removes all SCM token env vars, runs the closure,
    /// then restores original state.
    fn with_clean_token_env<F: FnOnce()>(f: F) {
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        // Save and remove all token env vars to isolate the test.
        let saved: Vec<(&str, Option<String>)> = [
            "GITLAB_TOKEN",
            "GITEA_TOKEN",
            "ANODIZE_GITHUB_TOKEN",
            "GITHUB_TOKEN",
            "ANODIZE_FORCE_TOKEN",
            "GORELEASER_FORCE_TOKEN",
        ]
        .iter()
        .map(|&k| (k, std::env::var(k).ok()))
        .collect();

        for &(k, _) in &saved {
            // SAFETY: ENV_MUTEX ensures no concurrent env access from our tests.
            unsafe { std::env::remove_var(k) };
        }

        f();

        // Restore original env state.
        for (k, v) in saved {
            match v {
                Some(val) => unsafe { std::env::set_var(k, val) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
    }

    #[test]
    fn test_resolve_scm_token_type_default_is_github() {
        with_clean_token_env(|| {
            let config = Config::default();
            let mut ctx = Context::new(config.clone(), ContextOptions::default());
            resolve_scm_token_type(&mut ctx, &config);

            assert_eq!(ctx.token_type, ScmTokenType::GitHub);
            // No token env vars set, token should remain None.
            assert!(ctx.options.token.is_none());
        });
    }

    #[test]
    fn test_resolve_scm_token_type_force_gitlab() {
        with_clean_token_env(|| {
            let config = Config {
                force_token: Some(ForceTokenKind::GitLab),
                ..Default::default()
            };
            let mut ctx = Context::new(config.clone(), ContextOptions::default());

            // Set GITLAB_TOKEN so token value resolution picks it up.
            unsafe { std::env::set_var("GITLAB_TOKEN", "glpat-test123") };
            resolve_scm_token_type(&mut ctx, &config);

            assert_eq!(ctx.token_type, ScmTokenType::GitLab);
            assert_eq!(ctx.options.token.as_deref(), Some("glpat-test123"));
        });
    }

    #[test]
    fn test_resolve_scm_token_type_force_gitea() {
        with_clean_token_env(|| {
            let config = Config {
                force_token: Some(ForceTokenKind::Gitea),
                ..Default::default()
            };
            let mut ctx = Context::new(config.clone(), ContextOptions::default());

            unsafe { std::env::set_var("GITEA_TOKEN", "gitea-tok") };
            resolve_scm_token_type(&mut ctx, &config);

            assert_eq!(ctx.token_type, ScmTokenType::Gitea);
            assert_eq!(ctx.options.token.as_deref(), Some("gitea-tok"));
        });
    }

    #[test]
    fn test_resolve_scm_token_type_env_gitlab_detected() {
        with_clean_token_env(|| {
            unsafe { std::env::set_var("GITLAB_TOKEN", "glpat-env") };

            let config = Config::default();
            let mut ctx = Context::new(config.clone(), ContextOptions::default());
            resolve_scm_token_type(&mut ctx, &config);

            assert_eq!(ctx.token_type, ScmTokenType::GitLab);
            assert_eq!(ctx.options.token.as_deref(), Some("glpat-env"));
        });
    }

    #[test]
    fn test_resolve_scm_token_type_env_gitea_detected() {
        with_clean_token_env(|| {
            unsafe { std::env::set_var("GITEA_TOKEN", "gitea-env") };

            let config = Config::default();
            let mut ctx = Context::new(config.clone(), ContextOptions::default());
            resolve_scm_token_type(&mut ctx, &config);

            assert_eq!(ctx.token_type, ScmTokenType::Gitea);
            assert_eq!(ctx.options.token.as_deref(), Some("gitea-env"));
        });
    }

    #[test]
    fn test_resolve_scm_token_type_github_token_from_env() {
        with_clean_token_env(|| {
            unsafe { std::env::set_var("GITHUB_TOKEN", "ghp-from-env") };

            let config = Config::default();
            let mut ctx = Context::new(config.clone(), ContextOptions::default());
            resolve_scm_token_type(&mut ctx, &config);

            assert_eq!(ctx.token_type, ScmTokenType::GitHub);
            assert_eq!(ctx.options.token.as_deref(), Some("ghp-from-env"));
        });
    }

    #[test]
    fn test_resolve_scm_token_type_anodize_github_token_takes_precedence() {
        with_clean_token_env(|| {
            unsafe { std::env::set_var("ANODIZE_GITHUB_TOKEN", "anodize-tok") };
            unsafe { std::env::set_var("GITHUB_TOKEN", "gh-tok") };

            let config = Config::default();
            let mut ctx = Context::new(config.clone(), ContextOptions::default());
            resolve_scm_token_type(&mut ctx, &config);

            assert_eq!(ctx.token_type, ScmTokenType::GitHub);
            assert_eq!(
                ctx.options.token.as_deref(),
                Some("anodize-tok"),
                "ANODIZE_GITHUB_TOKEN should take precedence over GITHUB_TOKEN"
            );
        });
    }

    #[test]
    fn test_resolve_scm_token_type_cli_token_preserved() {
        with_clean_token_env(|| {
            unsafe { std::env::set_var("GITHUB_TOKEN", "from-env") };

            let config = Config::default();
            let opts = ContextOptions {
                token: Some("from-cli".to_string()),
                ..Default::default()
            };
            let mut ctx = Context::new(config.clone(), opts);
            resolve_scm_token_type(&mut ctx, &config);

            assert_eq!(ctx.token_type, ScmTokenType::GitHub);
            assert_eq!(
                ctx.options.token.as_deref(),
                Some("from-cli"),
                "CLI --token flag should not be overwritten by env var"
            );
        });
    }

    #[test]
    fn test_resolve_scm_token_type_force_overrides_env_detection() {
        with_clean_token_env(|| {
            // GITLAB_TOKEN is set, but force_token says GitHub.
            unsafe { std::env::set_var("GITLAB_TOKEN", "glpat-ignored") };

            let config = Config {
                force_token: Some(ForceTokenKind::GitHub),
                ..Default::default()
            };
            let mut ctx = Context::new(config.clone(), ContextOptions::default());
            resolve_scm_token_type(&mut ctx, &config);

            assert_eq!(
                ctx.token_type,
                ScmTokenType::GitHub,
                "force_token should override env-based detection"
            );
            // Token value should be None since no GitHub token env var is set.
            assert!(
                ctx.options.token.is_none(),
                "no GitHub token env var set, so token should remain None"
            );
        });
    }

    #[test]
    fn test_resolve_scm_token_type_gitlab_priority_over_gitea() {
        with_clean_token_env(|| {
            // Both GITLAB_TOKEN and GITEA_TOKEN are set; GITLAB should win.
            unsafe { std::env::set_var("GITLAB_TOKEN", "gl-tok") };
            unsafe { std::env::set_var("GITEA_TOKEN", "gt-tok") };

            let config = Config::default();
            let mut ctx = Context::new(config.clone(), ContextOptions::default());
            resolve_scm_token_type(&mut ctx, &config);

            assert_eq!(
                ctx.token_type,
                ScmTokenType::GitLab,
                "GITLAB_TOKEN should be checked before GITEA_TOKEN"
            );
            assert_eq!(ctx.options.token.as_deref(), Some("gl-tok"));
        });
    }

    #[test]
    fn test_resolve_scm_token_type_anodize_force_token_env_gitlab() {
        with_clean_token_env(|| {
            // ANODIZE_FORCE_TOKEN env var should override env-based detection.
            unsafe { std::env::set_var("ANODIZE_FORCE_TOKEN", "gitlab") };
            unsafe { std::env::set_var("GITLAB_TOKEN", "glpat-env") };

            let config = Config::default();
            let mut ctx = Context::new(config.clone(), ContextOptions::default());
            resolve_scm_token_type(&mut ctx, &config);

            assert_eq!(
                ctx.token_type,
                ScmTokenType::GitLab,
                "ANODIZE_FORCE_TOKEN=gitlab should force GitLab"
            );
            assert_eq!(ctx.options.token.as_deref(), Some("glpat-env"));
        });
    }

    #[test]
    fn test_resolve_scm_token_type_anodize_force_token_env_github() {
        with_clean_token_env(|| {
            // Force GitHub even though GITLAB_TOKEN is present.
            unsafe { std::env::set_var("ANODIZE_FORCE_TOKEN", "github") };
            unsafe { std::env::set_var("GITLAB_TOKEN", "glpat-ignored") };
            unsafe { std::env::set_var("GITHUB_TOKEN", "ghp-forced") };

            let config = Config::default();
            let mut ctx = Context::new(config.clone(), ContextOptions::default());
            resolve_scm_token_type(&mut ctx, &config);

            assert_eq!(
                ctx.token_type,
                ScmTokenType::GitHub,
                "ANODIZE_FORCE_TOKEN=github should override GITLAB_TOKEN detection"
            );
            assert_eq!(ctx.options.token.as_deref(), Some("ghp-forced"));
        });
    }

    #[test]
    fn test_resolve_scm_token_type_goreleaser_force_token_compat() {
        with_clean_token_env(|| {
            // GORELEASER_FORCE_TOKEN is the compat fallback when ANODIZE_ is not set.
            unsafe { std::env::set_var("GORELEASER_FORCE_TOKEN", "gitea") };
            unsafe { std::env::set_var("GITEA_TOKEN", "gitea-compat") };

            let config = Config::default();
            let mut ctx = Context::new(config.clone(), ContextOptions::default());
            resolve_scm_token_type(&mut ctx, &config);

            assert_eq!(
                ctx.token_type,
                ScmTokenType::Gitea,
                "GORELEASER_FORCE_TOKEN should work as compat fallback"
            );
            assert_eq!(ctx.options.token.as_deref(), Some("gitea-compat"));
        });
    }

    #[test]
    fn test_resolve_scm_token_type_anodize_force_token_overrides_goreleaser() {
        with_clean_token_env(|| {
            // When both env vars are set, ANODIZE_FORCE_TOKEN takes precedence.
            unsafe { std::env::set_var("ANODIZE_FORCE_TOKEN", "github") };
            unsafe { std::env::set_var("GORELEASER_FORCE_TOKEN", "gitlab") };
            unsafe { std::env::set_var("GITHUB_TOKEN", "ghp-wins") };
            unsafe { std::env::set_var("GITLAB_TOKEN", "glpat-loses") };

            let config = Config::default();
            let mut ctx = Context::new(config.clone(), ContextOptions::default());
            resolve_scm_token_type(&mut ctx, &config);

            assert_eq!(
                ctx.token_type,
                ScmTokenType::GitHub,
                "ANODIZE_FORCE_TOKEN should take precedence over GORELEASER_FORCE_TOKEN"
            );
            assert_eq!(ctx.options.token.as_deref(), Some("ghp-wins"));
        });
    }

    #[test]
    fn test_resolve_scm_token_type_config_force_token_overrides_env() {
        with_clean_token_env(|| {
            // Config-level force_token should override env var.
            unsafe { std::env::set_var("ANODIZE_FORCE_TOKEN", "gitlab") };
            unsafe { std::env::set_var("GITHUB_TOKEN", "ghp-config") };

            let config = Config {
                force_token: Some(ForceTokenKind::GitHub),
                ..Default::default()
            };
            let mut ctx = Context::new(config.clone(), ContextOptions::default());
            resolve_scm_token_type(&mut ctx, &config);

            assert_eq!(
                ctx.token_type,
                ScmTokenType::GitHub,
                "config.force_token should override ANODIZE_FORCE_TOKEN env var"
            );
            assert_eq!(ctx.options.token.as_deref(), Some("ghp-config"));
        });
    }

    #[test]
    fn test_resolve_scm_token_type_invalid_force_token_env_ignored() {
        with_clean_token_env(|| {
            // Invalid value should be ignored, falling back to env-based detection.
            unsafe { std::env::set_var("ANODIZE_FORCE_TOKEN", "invalid") };
            unsafe { std::env::set_var("GITLAB_TOKEN", "glpat-detected") };

            let config = Config::default();
            let mut ctx = Context::new(config.clone(), ContextOptions::default());
            resolve_scm_token_type(&mut ctx, &config);

            assert_eq!(
                ctx.token_type,
                ScmTokenType::GitLab,
                "invalid ANODIZE_FORCE_TOKEN should fall back to env detection"
            );
            assert_eq!(ctx.options.token.as_deref(), Some("glpat-detected"));
        });
    }
}
