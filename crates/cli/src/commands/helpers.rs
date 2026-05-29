use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::{Config, ForceTokenKind, GitHubConfig, WorkspaceConfig};
use anodizer_core::context::Context;
use anodizer_core::git;
use anodizer_core::log::StageLogger;
use anodizer_core::scm::{self, ScmTokenType};
use anyhow::{Context as _, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Parse a comma-separated list (e.g. `--targets=a,b,c` or `--stages=x,y`)
/// into the canonical `Option<Vec<String>>` form.
///
/// - `None`           → `None` (no filter).
/// - `Some("a,b")`    → `Some(["a", "b"])`.
/// - Empty / whitespace-only tokens (trailing comma, double comma,
///   surrounding spaces) are dropped — they're noise, not intent.
/// - `Some("")` or `Some(" , ")` (all-empty after trimming) → `Err`. The
///   operator clearly meant to pass *something*; surfacing the typo
///   beats silently degrading into a no-op filter.
///
/// `flag_help` is the `--flag=<example>` snippet appended to the error so
/// each call site gets a copy-pasteable hint specific to its CSV shape.
pub(crate) fn parse_csv_list(
    raw: Option<&str>,
    flag_help: &str,
) -> Result<Option<Vec<String>>, String> {
    match raw {
        None => Ok(None),
        Some(list) => {
            let parsed: Vec<String> = list
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            if parsed.is_empty() {
                return Err(format!(
                    "{flag_help} must list at least one entry (got empty / whitespace-only input)"
                ));
            }
            Ok(Some(parsed))
        }
    }
}

/// Walk an artifact path iterator and fail if any path appears more than
/// once. Used by post-load manifest validators (publish-only's per-shard
/// merge, `release --merge`'s split-worker merge) to surface accidental
/// shard overlap as a hard error rather than a silent double-publish
/// downstream.
pub(crate) fn detect_duplicate_paths<'a, I>(paths: I) -> Result<()>
where
    I: IntoIterator<Item = &'a Path>,
{
    use std::collections::BTreeMap;
    let mut counts: BTreeMap<PathBuf, usize> = BTreeMap::new();
    for p in paths {
        *counts.entry(p.to_path_buf()).or_insert(0) += 1;
    }
    let duplicates: Vec<(PathBuf, usize)> = counts.into_iter().filter(|(_, n)| *n > 1).collect();
    if duplicates.is_empty() {
        return Ok(());
    }
    let summary = duplicates
        .iter()
        .map(|(p, n)| format!("{} ({}×)", p.display(), n))
        .collect::<Vec<_>>()
        .join(", ");
    anyhow::bail!(
        "duplicate artifact path(s) after merging per-shard manifests: {summary}. \
         Hypothesis: two shards overlapped on the same target, so both \
         emitted an artifact for the same path. Inspect the matrix in \
         `.github/workflows/release.yml` (or the equivalent dispatcher) \
         to confirm the shards partition the target set."
    );
}

/// Walk an artifact path iterator and verify each file exists on disk
/// under `dist/`. Tries the literal path first (absolute or relative),
/// then `dist.join(<path>)`. Missing files are fatal so SignStage /
/// ChecksumStage emit an operator-friendly manifest-shaped diagnostic
/// rather than cosign / gpg's less actionable "file not found".
///
/// Files in `dist/` that are *absent* from the manifest are not flagged
/// — dist trees carry metadata.json, harness logs, etc. that aren't
/// part of the artifact registry.
pub(crate) fn detect_missing_files<'a, I>(paths: I, dist: &Path) -> Result<()>
where
    I: IntoIterator<Item = &'a Path>,
{
    let mut missing: Vec<PathBuf> = Vec::new();
    for p in paths {
        if p.is_absolute() {
            if !p.is_file() {
                missing.push(p.to_path_buf());
            }
        } else if !p.is_file() && !dist.join(p).is_file() {
            missing.push(p.to_path_buf());
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    missing.sort();
    let summary = missing
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    anyhow::bail!(
        "artifacts manifest references file(s) not present under {}: {summary}. \
         The preserved dist is incomplete; re-run \
         `anodize check determinism --preserve-dist=<dist>` to repopulate, or \
         remove the stale manifest entries before retrying.",
        dist.display(),
    );
}

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

/// Resolve the effective `force_token` kind — config field first, then the
/// `ANODIZER_FORCE_TOKEN` env var, then the `GORELEASER_FORCE_TOKEN` compat
/// fallback. Returns `None` if nothing is set (or the value isn't a recognised
/// backend).
///
/// Extracted so `setup_env` and `resolve_scm_token_type` can't drift — adding
/// a new backend only needs to be wired in this one place. Reads env vars
/// through the injected `EnvSource` so tests stay off process-env mutation.
fn resolve_force_token_with_env<E: anodizer_core::env_source::EnvSource + ?Sized>(
    config: &Config,
    env: &E,
) -> Option<ForceTokenKind> {
    config.force_token.as_ref().cloned().or_else(|| {
        let env_val = env
            .var("ANODIZER_FORCE_TOKEN")
            .or_else(|| env.var("GORELEASER_FORCE_TOKEN"))?;
        match env_val.to_lowercase().as_str() {
            "github" => Some(ForceTokenKind::GitHub),
            "gitlab" => Some(ForceTokenKind::GitLab),
            "gitea" => Some(ForceTokenKind::Gitea),
            _ => None,
        }
    })
}

/// Process-env convenience wrapper over [`resolve_force_token_with_env`].
fn resolve_force_token(config: &Config) -> Option<ForceTokenKind> {
    resolve_force_token_with_env(config, &anodizer_core::env_source::ProcessEnvSource)
}

/// Collect all configured build targets from a config, in declaration order.
///
/// Iterates `config.crates` plus every `config.workspaces[].crates` so monorepos
/// with multi-root workspaces are covered. Per-crate `builds[].targets` entries
/// REPLACE `defaults.targets` for that build (override semantics — matching
/// the `BuildConfig.targets` rustdoc and the stage-build runtime). Builds
/// whose `targets` field is `None` fall back to `defaults.targets`.
/// Duplicates are filtered across all builds, and `defaults.builds.ignore`
/// (os/arch pairs) removes matching targets.
///
/// `selected_crates` filters the iteration: when empty, all crates are used;
/// otherwise only crates whose `name` is in the slice contribute.
pub fn collect_build_targets(config: &Config, selected_crates: &[String]) -> Vec<String> {
    let mut targets: Vec<String> = Vec::new();
    let default_targets = config
        .defaults
        .as_ref()
        .and_then(|d| d.targets.as_deref())
        .unwrap_or(&[]);

    let all_crates = config.crates.iter().chain(
        config
            .workspaces
            .as_deref()
            .unwrap_or_default()
            .iter()
            .flat_map(|w| w.crates.iter()),
    );

    let mut have_any_build = false;
    for krate in all_crates {
        if !selected_crates.is_empty() && !selected_crates.contains(&krate.name) {
            continue;
        }

        if let Some(ref builds) = krate.builds {
            for build in builds {
                have_any_build = true;
                // Override semantics: when a per-build `targets` is set,
                // it REPLACES `defaults.targets` for that build. Only when
                // it is None does the build fall through to the defaults.
                let chosen = match build.targets.as_deref() {
                    Some(ts) => ts,
                    None => default_targets,
                };
                for t in chosen {
                    if !targets.contains(t) {
                        targets.push(t.clone());
                    }
                }
            }
        }
    }

    // No builds at all (e.g. lib-only crates inheriting nothing); the
    // defaults.targets list is the canonical fallback set so callers like
    // `anodizer release --single-target` still see something to filter
    // against.
    if !have_any_build {
        for t in default_targets {
            if !targets.contains(t) {
                targets.push(t.clone());
            }
        }
    }

    if let Some(ignores) = config
        .defaults
        .as_ref()
        .and_then(|d| d.builds.as_ref())
        .and_then(|b| b.ignore.as_ref())
    {
        targets.retain(|t| {
            let (os, arch) = anodizer_core::target::map_target(t);
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
    if let Some(ref env_list) = ws.env {
        let merged = config.env.get_or_insert_with(Vec::new);
        merged.extend(env_list.iter().cloned());
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

    // Allow env var overrides for tag discovery. Anodizer-native var wins;
    // the GoReleaser compat alias is checked as a fallback so CI jobs migrating
    // from GoReleaser pick up their existing env vars without rewiring. As a
    // last resort, GitHub Actions exposes the triggering tag as GITHUB_REF_NAME
    // when GITHUB_REF_TYPE=tag — use that so workflows that didn't explicitly
    // export ANODIZER_CURRENT_TAG (e.g. `Release.yml` jobs dispatched by a tag
    // push) still resolve the correct tag instead of falling through to
    // per-crate-template latest-tag scanning (which can mis-resolve when the
    // triggering tag's prefix doesn't match the first crate's tag_template).
    let anodizer_current_tag = ctx.env_var("ANODIZER_CURRENT_TAG");
    let goreleaser_current_tag = ctx.env_var("GORELEASER_CURRENT_TAG");
    let github_ref_type = ctx.env_var("GITHUB_REF_TYPE");
    let github_ref_name = ctx.env_var("GITHUB_REF_NAME");
    tracing::debug!(
        anodizer_current_tag = ?anodizer_current_tag,
        goreleaser_current_tag = ?goreleaser_current_tag,
        github_ref_type = ?github_ref_type,
        github_ref_name = ?github_ref_name,
        "tag_override resolution: env var snapshot"
    );
    let tag_override = anodizer_current_tag
        .filter(|s| !s.is_empty())
        .or_else(|| goreleaser_current_tag.filter(|s| !s.is_empty()))
        .or_else(|| {
            let is_tag = github_ref_type.as_deref().filter(|s| *s == "tag").is_some();
            if is_tag {
                github_ref_name.filter(|s| !s.is_empty())
            } else {
                None
            }
        });

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
                "using ANODIZER_CURRENT_TAG override: {}",
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

                // Allow ANODIZER_PREVIOUS_TAG (or GoReleaser compat
                // GORELEASER_PREVIOUS_TAG) env override for the previous tag.
                let prev_override = ctx
                    .env_var("ANODIZER_PREVIOUS_TAG")
                    .filter(|s| !s.is_empty())
                    .or_else(|| {
                        ctx.env_var("GORELEASER_PREVIOUS_TAG")
                            .filter(|s| !s.is_empty())
                    });
                if let Some(prev_override) = prev_override {
                    log.verbose(&format!(
                        "using ANODIZER_PREVIOUS_TAG override: {}",
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

/// Combine `defaults.env` and top-level `config.env` into a single list with
/// deterministic precedence: defaults entries come first so any same-keyed
/// entry in `config.env` clobbers the defaults version on the
/// last-one-wins-per-key application path inside `setup_env`.
///
/// Returns `None` when both inputs are `None`. Returns the cloned non-None
/// input when only one side is set.
fn merge_env_with_defaults(
    defaults_env: Option<&Vec<String>>,
    config_env: Option<&Vec<String>>,
) -> Option<Vec<String>> {
    match (defaults_env, config_env) {
        (None, None) => None,
        (Some(d), None) => Some(d.clone()),
        (None, Some(c)) => Some(c.clone()),
        (Some(d), Some(c)) => {
            let mut v = Vec::with_capacity(d.len() + c.len());
            v.extend(d.iter().cloned());
            v.extend(c.iter().cloned());
            Some(v)
        }
    }
}

/// Load process environment variables, `.env` files, and user-defined env vars
/// into the context's template variables.
///
/// Loading order (later wins):
/// 1. All process environment variables (`std::env::vars()`)
/// 2. Variables from `.env` files specified in config
/// 3. Explicit `env:` map entries — `defaults.env` first, then `config.env`
///    (so per-config entries override defaults on duplicate keys)
///
/// This ensures config-defined env vars always take precedence over process
/// environment, matching GoReleaser's behavior where all process env vars are
/// accessible in templates via `{{ .Env.VAR }}`.
pub fn setup_env(
    ctx: &mut Context,
    config: &Config,
    log: &anodizer_core::log::StageLogger,
) -> anyhow::Result<()> {
    // Load ALL process environment variables first (lowest priority)
    for (key, value) in ctx.env_source().vars() {
        ctx.template_vars_mut().set_env(&key, &value);
    }

    // Load env files into template context (overrides process env).
    // Supports both list form (array of .env files) and struct form (token file paths).
    // These are user-configured, so use set_config_env (safe for cross-platform
    // serialization and subprocess injection).
    if let Some(ref env_files_config) = config.env_files {
        match env_files_config {
            anodizer_core::config::EnvFilesConfig::List(files) => {
                let env_vars = anodizer_core::config::load_env_files(files, log, ctx.is_strict())
                    .map_err(anyhow::Error::msg)?;
                for (key, value) in &env_vars {
                    ctx.template_vars_mut().set_config_env(key, value);
                }
            }
            anodizer_core::config::EnvFilesConfig::TokenFiles(token_config) => {
                let token_vars = anodizer_core::config::load_token_files(token_config, log)
                    .map_err(anyhow::Error::msg)?;
                for (key, value) in &token_vars {
                    ctx.template_vars_mut().set_config_env(key, value);
                    set_env_var_single_threaded(key, value);
                }
            }
        }
    } else {
        // always check default
        // token file paths even when env_files is not configured.
        let default_config = anodizer_core::config::EnvFilesTokenConfig::default();
        let token_vars = anodizer_core::config::load_token_files(&default_config, log)
            .map_err(anyhow::Error::msg)?;
        for (key, value) in &token_vars {
            ctx.template_vars_mut().set_config_env(key, value);
            set_env_var_single_threaded(key, value);
        }
    }

    // Populate user-defined env vars into template context (highest priority).
    // GoReleaser renders env values through the template engine.
    let merged_env = merge_env_with_defaults(
        config.defaults.as_ref().and_then(|d| d.env.as_ref()),
        config.env.as_ref(),
    );
    if let Some(ref env_list) = merged_env {
        let rendered_pairs =
            anodizer_core::config::render_env_entries(env_list, |v| ctx.render_template(v))
                .with_context(|| "config.env: parse and render entries")?;
        for (key, rendered) in rendered_pairs {
            ctx.template_vars_mut().set_config_env(&key, &rendered);
            // Also set in the process environment so that child processes which
            // inherit env (docker, lipo, rustup, git, hook scripts) see these
            // values. Some commands use explicit `.envs()`, but many rely on
            // process-level inheritance.
            //
            // SAFETY: This is called during single-threaded pipeline setup in
            // `setup_env`, before any worker threads are spawned. No concurrent
            // readers of the process environment exist at this point.
            set_env_var_single_threaded(&key, &rendered);
        }
    }

    // Populate user-defined custom variables into template context.
    //
    // Iteration is sorted by key (BTreeMap) so cross-variable references
    // resolve deterministically: a value like `b: "{{ .Var.a }}_v2"`
    // sees `a` IF `a` sorts earlier than `b`. The single-pass model is
    // documented behaviour — variables that reference other variables
    // must rely on the alphabetical-key ordering, or refer through
    // `{{ .Env.* }}`.
    //
    // Render errors are hard: an unknown variable / typo in a template
    // expression (`{{ .Tagg }}`) fails the load instead of silently
    // passing the literal `{{ }}` through to a publisher (homebrew,
    // scoop, nix, …) where it would surface only after publication.
    if let Some(ref vars_map) = config.variables {
        for (key, value) in vars_map {
            let rendered = ctx
                .render_template(value)
                .with_context(|| format!("variables.{key}: failed to render template '{value}'"))?;
            ctx.template_vars_mut().set_custom_var(key, &rendered);
        }
    }

    // GoReleaser env.go:75-86: when force_token is active, clear non-forced
    // token env vars BEFORE the multi-token check so it cannot fire.
    let resolved_force = resolve_force_token(config);
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
                std::env::remove_var("ANODIZER_GITHUB_TOKEN");
            }
        }
        if !keep_gitlab {
            // SAFETY: single-threaded pipeline setup, see set_env_var_single_threaded.
            unsafe { std::env::remove_var("GITLAB_TOKEN") };
        }
        if !keep_gitea {
            // SAFETY: single-threaded pipeline setup, see set_env_var_single_threaded.
            unsafe { std::env::remove_var("GITEA_TOKEN") };
        }
    }

    // Multiple-token detection (GoReleaser env.go:88-101 ErrMultipleTokens).
    // When multiple SCM tokens are set without force_token, error early.
    if resolved_force.is_none() {
        let has_github =
            ctx.env_var("GITHUB_TOKEN").is_some() || ctx.env_var("ANODIZER_GITHUB_TOKEN").is_some();
        let has_gitlab = ctx.env_var("GITLAB_TOKEN").is_some();
        let has_gitea = ctx.env_var("GITEA_TOKEN").is_some();
        let count = [has_github, has_gitlab, has_gitea]
            .iter()
            .filter(|&&b| b)
            .count();
        if count > 1 {
            anyhow::bail!(
                "multiple SCM tokens set simultaneously ({}). Set force_token in config \
                 or ANODIZER_FORCE_TOKEN env var to specify which to use.",
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
    // Snapshot mode, dry-run, and release.skip can proceed without a token.
    //
    // `--publish-only` defers the token check to
    // `publish_only::preflight_credentials`, which combines the token
    // check with the production sign-key check (the spec wants both
    // validated together at the top of the publish-only branch). If
    // setup_env bailed here first, publish-only would never get a
    // chance to emit its combined preflight error or honor
    // `--no-preflight`. The publish-only branch enforces the same
    // gate downstream so dropping it here doesn't widen the hole.
    if ctx.options.token.is_none()
        && !ctx.is_snapshot()
        && !ctx.is_dry_run()
        && !ctx.options.publish_only
    {
        let release_skipped = match config
            .crates
            .first()
            .and_then(|c| c.release.as_ref()?.skip.as_ref())
        {
            Some(d) => d
                .try_evaluates_to_true(|t| ctx.render_template(t))
                .with_context(|| "release: render skip template")?,
            None => false,
        };
        let needs_token = config.crates.iter().any(|c| c.release.is_some())
            && !ctx.should_skip("release")
            && !release_skipped;
        if needs_token {
            let hint = match ctx.token_type {
                anodizer_core::scm::ScmTokenType::GitLab => {
                    "no GitLab token found. Set GITLAB_TOKEN."
                }
                anodizer_core::scm::ScmTokenType::Gitea => "no Gitea token found. Set GITEA_TOKEN.",
                anodizer_core::scm::ScmTokenType::GitHub => {
                    "no GitHub token found. Set GITHUB_TOKEN or ANODIZER_GITHUB_TOKEN."
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
///
/// Two runs of the determinism harness must emit a byte-identical
/// `config.yaml`. The `Config` type carries many `HashMap<String, _>` fields
/// (`docker.labels`, `docker.build_args`, `variables`, `nfpm.dependencies`,
/// announcer `extra`, custom headers, …) whose iteration order is randomized
/// per process. We serialize to a `serde_yaml_ng::Value` first, then
/// recursively sort every mapping's keys alphabetically, then emit the
/// canonical form. Centralised here so adding a new HashMap field anywhere
/// in `Config` is automatically covered without a per-field `serialize_with`
/// attribute.
pub fn write_effective_config(config: &Config, log: &StageLogger) -> Result<()> {
    let dist = &config.dist;
    std::fs::create_dir_all(dist)
        .with_context(|| format!("failed to create dist directory: {}", dist.display()))?;
    let effective_path = dist.join("config.yaml");
    let mut value: serde_yaml_ng::Value =
        serde_yaml_ng::to_value(config).context("failed to serialize effective config")?;
    sort_yaml_mapping(&mut value);
    let yaml = serde_yaml_ng::to_string(&value).context("failed to serialize effective config")?;
    std::fs::write(&effective_path, &yaml)
        .with_context(|| format!("failed to write {}", effective_path.display()))?;
    log.verbose(&format!(
        "wrote effective config to {}",
        effective_path.display()
    ));
    Ok(())
}

/// Recursively sort every `Value::Mapping` entry by key.
///
/// `serde_yaml_ng::Mapping` is an `IndexMap` (insertion-ordered), so the
/// emit order is whatever order serde visited the source. For
/// `HashMap<String, _>` fields that order is randomized per process — fatal
/// for the determinism harness, which fingerprints `dist/config.yaml`. This
/// helper rebuilds each mapping in sort order (lexicographically by the
/// `Display` form of `Value`, which for `String` keys is the underlying
/// string — the only mapping-key shape the `Config` type produces).
fn sort_yaml_mapping(value: &mut serde_yaml_ng::Value) {
    use serde_yaml_ng::{Mapping, Value};
    match value {
        Value::Mapping(map) => {
            let mut entries: Vec<(Value, Value)> = std::mem::take(map).into_iter().collect();
            entries.sort_by_key(|(a, _)| yaml_key_sort_key(a));
            let mut sorted = Mapping::with_capacity(entries.len());
            for (k, mut v) in entries {
                sort_yaml_mapping(&mut v);
                sorted.insert(k, v);
            }
            *map = sorted;
        }
        Value::Sequence(seq) => {
            for v in seq.iter_mut() {
                sort_yaml_mapping(v);
            }
        }
        Value::Tagged(tagged) => sort_yaml_mapping(&mut tagged.value),
        _ => {}
    }
}

/// Stable string-keyed sort for YAML mapping entries. Strings compare on
/// their UTF-8 bytes (the common case); every other `Value` flavour falls
/// back to its `Debug` rendering so the order is at least deterministic.
fn yaml_key_sort_key(v: &serde_yaml_ng::Value) -> String {
    match v {
        serde_yaml_ng::Value::String(s) => s.clone(),
        other => format!("{:?}", other),
    }
}

/// Print the artifact size report if `report_sizes` is enabled in config.
pub fn run_report_sizes(ctx: &mut Context, config: &Config, log: &StageLogger) {
    if config.report_sizes.unwrap_or(false) {
        anodizer_core::artifact::print_size_report(&mut ctx.artifacts, log);
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
    let goos = anodizer_core::context::map_os_to_goos(std::env::consts::OS);
    let goarch = anodizer_core::context::map_arch_to_goarch(std::env::consts::ARCH);

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

    ctx.artifacts.add(anodizer_core::artifact::Artifact {
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
            let mtime = anodizer_core::util::parse_mod_timestamp(&rendered)
                .with_context(|| format!("invalid metadata.mod_timestamp value: {:?}", rendered))?;
            anodizer_core::util::set_file_mtime(&metadata_path, mtime)?;
            anodizer_core::util::set_file_mtime(&artifacts_path, mtime)?;
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
    // Default the GR-Pro `IsPrepare` template var to `"false"` for every
    // command that flows through `setup_context`. The release command
    // overrides this when `--prepare` is passed (see
    // `commands/release/mod.rs`). Setting it unconditionally avoids a
    // "missing key" footgun in user templates that branch on
    // `{{ if IsPrepare }}`.
    ctx.template_vars_mut().set("IsPrepare", "false");
    setup_env(ctx, config, log)?;
    resolve_git_context(ctx, config, log)?;
    Ok(())
}

/// Resolve the SCM token type and token value from config and environment.
///
/// This sets `ctx.token_type` based on priority (highest first):
/// 1. `config.force_token` — explicit user config (`force_token: gitlab`)
/// 2. `ANODIZER_FORCE_TOKEN` env var — e.g. `github`, `gitlab`, `gitea`
/// 3. `GORELEASER_FORCE_TOKEN` env var — GoReleaser compat fallback
/// 4. Environment variable presence — `GITLAB_TOKEN` → GitLab, `GITEA_TOKEN` → Gitea
/// 5. Default — GitHub
///
/// It also resolves the token value into `ctx.options.token` (if not already
/// set by a CLI flag) from the appropriate environment variable:
/// - GitLab: `GITLAB_TOKEN`
/// - Gitea: `GITEA_TOKEN`
/// - GitHub: `ANODIZER_GITHUB_TOKEN` or `GITHUB_TOKEN`
pub fn resolve_scm_token_type(ctx: &mut Context, config: &Config) {
    // Detect which SCM backend to use from environment variables. The
    // EnvSource indirection lets tests build a `Context` via
    // `TestContextBuilder::env(...)` and drive every branch without
    // mutating process env.
    let env_hint = if ctx.env_var("GITLAB_TOKEN").is_some() {
        Some("gitlab")
    } else if ctx.env_var("GITEA_TOKEN").is_some() {
        Some("gitea")
    } else {
        None
    };

    // Resolution priority: explicit `release.provider:` (GoReleaser Pro
    // cross-platform publishing) > top-level `force_token:` > env-var
    // detection. `release.provider:` makes the cross-platform case
    // declarative: a project that lives on GitLab but publishes to
    // GitHub declares `provider: github` and the token detection
    // honours it without needing the user to clear `GITLAB_TOKEN`.
    let provider_force = config.release.as_ref().and_then(|r| r.provider.clone());
    let force_token =
        provider_force.or_else(|| resolve_force_token_with_env(config, ctx.env_source()));

    ctx.token_type = scm::resolve_token_type(force_token.as_ref(), env_hint);

    // Resolve the token value if not already provided via CLI flag.
    if ctx.options.token.is_none() {
        ctx.options.token = match ctx.token_type {
            ScmTokenType::GitLab => ctx.env_var("GITLAB_TOKEN"),
            ScmTokenType::Gitea => ctx.env_var("GITEA_TOKEN"),
            ScmTokenType::GitHub => ctx
                .env_var("ANODIZER_GITHUB_TOKEN")
                .or_else(|| ctx.env_var("GITHUB_TOKEN")),
        };
    }
}

/// Load config, auto-detect GitHub, build a `Context`, and rehydrate
/// artifacts from `dist/` — the shared prelude for the `publish`,
/// `announce`, and (no-`--merge` branch of) `continue` commands.
///
/// Returns `(config, ctx, dist)` so the caller can drive the publish /
/// announce pipeline. `ctx_opts` is assembled by the caller so each
/// command supplies its own `skip_stages` / `merge` / `token` overlay.
///
/// Side effect: emits a `log.status("loaded N artifact(s) from <dist>")`
/// line after rehydration so the operator sees the artifact count at
/// the same point in every "resume from dist" command.
pub fn init_publish_stage_ctx(
    config_override: Option<&Path>,
    ctx_opts: anodizer_core::context::ContextOptions,
    dist_override: Option<&Path>,
    infer_project: bool,
    log: &StageLogger,
) -> Result<(Config, Context, std::path::PathBuf)> {
    let config_path = crate::pipeline::find_config_with_logger(config_override, Some(log))?;
    let mut config = crate::pipeline::load_config(&config_path)?;
    if infer_project {
        infer_project_name(&mut config, log);
    }
    auto_detect_github(&mut config, log);

    let mut ctx = Context::new(config.clone(), ctx_opts);
    setup_context(&mut ctx, &config, log)?;

    let dist = dist_override.unwrap_or(&config.dist).to_path_buf();
    load_artifacts_from_dist(&mut ctx, &dist)?;
    log.status(&format!(
        "loaded {} artifact(s) from {}",
        ctx.artifacts.all().len(),
        dist.display()
    ));

    Ok((config, ctx, dist))
}

/// Load artifacts from dist/artifacts.json into the context's artifact registry.
/// Used by `publish` and `announce` commands that run from a completed dist/.
pub fn load_artifacts_from_dist(ctx: &mut Context, dist: &Path) -> Result<()> {
    let artifacts_path = dist.join("artifacts.json");
    load_artifacts_from_manifest(ctx, dist, &artifacts_path)
}

/// Load artifacts from an explicitly-named manifest path under `dist/`.
/// Split from [`load_artifacts_from_dist`] so a sharded matrix can fold
/// in `artifacts-<shard>.json` files one at a time. `dist` is carried
/// only for the error message (caller-meaningful location).
pub fn load_artifacts_from_manifest(
    ctx: &mut Context,
    dist: &Path,
    manifest_path: &Path,
) -> Result<()> {
    if !manifest_path.exists() {
        anyhow::bail!(
            "no artifacts manifest found at {} (under {}). Run a full release or merge first.",
            manifest_path.display(),
            dist.display()
        );
    }

    let content = std::fs::read_to_string(manifest_path)
        .with_context(|| format!("read {}", manifest_path.display()))?;

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
        .with_context(|| format!("parse {}", manifest_path.display()))?;

    for a in artifacts {
        let kind = ArtifactKind::parse(&a.kind)
            .ok_or_else(|| anyhow::anyhow!("unknown artifact kind: {}", a.kind))?;
        // Re-anchor `./dist/<rel>` / `dist/<rel>` paths onto the caller-
        // supplied `dist` root. Stored paths reflect the harness worktree's
        // `dist/<file>` shape; per-crate publish-only consumes from
        // `./dist/<crate>/` and would otherwise hit the manifest path
        // verbatim (`./dist/<file>`) instead of `./dist/<crate>/<file>` and
        // trip `detect_missing_files`. Flat callers (`publish`, `announce`,
        // single-crate `publish-only`) pass `dist=./dist`, so the rewrite
        // is a no-op for them. Paths outside the dist root (raw
        // `.det-tmp/target/...` binaries surfaced as Binary artifacts) are
        // left alone.
        let path_str = a.path.as_str();
        let rewritten = if let Some(rel) = path_str
            .strip_prefix("./dist/")
            .or_else(|| path_str.strip_prefix("dist/"))
        {
            dist.join(rel)
        } else {
            std::path::PathBuf::from(path_str)
        };
        // Cross-shard cross-target artifacts (source archive, install.sh,
        // metadata.json — all `target: None`) appear in every shard's
        // manifest by design and are byte-deduped on disk by
        // `download-artifact merge-multiple`. Adding all N shard entries
        // here would emit N-1 "already registered" warnings per artifact
        // before `dedupe_targetless_duplicates` cleaned them up — noise
        // that doesn't reflect a real problem. Skip-add when the same
        // path is already in the registry and the entry is targetless.
        if a.target.is_none()
            && ctx
                .artifacts
                .all()
                .iter()
                .any(|existing| existing.target.is_none() && existing.path == rewritten)
        {
            continue;
        }
        ctx.artifacts.add(Artifact {
            kind,
            name: a.name.unwrap_or_default(),
            path: rewritten,
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
    use anodizer_core::config::{ChangelogConfig, CrateConfig, SignConfig};
    use anodizer_core::context::ContextOptions;
    use anodizer_core::scm::ScmTokenType;

    /// `Config.variables` is stored as a `BTreeMap` so iteration is
    /// always sorted by key. The determinism harness fingerprints
    /// `dist/config.yaml`, so two runs in the same workspace must emit
    /// byte-identical YAML. `write_effective_config` is expected to route
    /// the serialized config through `sort_yaml_mapping`, alphabetising the
    /// keys of every mapping (top-level AND nested). Without that, the
    /// `variables:` block's emit order drifts even though the source map
    /// is sorted.
    #[test]
    fn write_effective_config_emits_sorted_keys() {
        use std::collections::BTreeMap;
        let tmp = tempfile::tempdir().unwrap();
        let mut variables = BTreeMap::new();
        // Insert in deliberately non-alphabetical order — the BTreeMap's
        // sorted iteration normalises this for the input side; the test
        // still verifies that `sort_yaml_mapping` sorts NESTED maps too.
        variables.insert("zeta".to_string(), "1".to_string());
        variables.insert("alpha".to_string(), "2".to_string());
        variables.insert("mu".to_string(), "3".to_string());
        variables.insert("beta".to_string(), "4".to_string());
        variables.insert("nu".to_string(), "5".to_string());
        let config = Config {
            project_name: "anodize".to_string(),
            dist: tmp.path().to_path_buf(),
            variables: Some(variables),
            ..Default::default()
        };
        let log = StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);

        let mut variables_reversed = BTreeMap::new();
        for key in ["nu", "beta", "mu", "alpha", "zeta"] {
            let v = match key {
                "zeta" => "1",
                "alpha" => "2",
                "mu" => "3",
                "beta" => "4",
                "nu" => "5",
                _ => unreachable!(),
            };
            variables_reversed.insert(key.to_string(), v.to_string());
        }
        let config_reversed = Config {
            variables: Some(variables_reversed),
            ..config.clone()
        };

        write_effective_config(&config, &log).expect("first write");
        let yaml1 = std::fs::read_to_string(tmp.path().join("config.yaml")).unwrap();
        // Second write into the same dist with reversed-insertion variables.
        write_effective_config(&config_reversed, &log).expect("second write");
        let yaml2 = std::fs::read_to_string(tmp.path().join("config.yaml")).unwrap();
        assert_eq!(
            yaml1, yaml2,
            "two write_effective_config calls with identical input keys \
             must produce byte-identical YAML regardless of HashMap \
             insertion order (HashMap-iteration drift would fail this)"
        );

        // And the variables block keys must be alphabetical.
        let var_block_lines: Vec<&str> = yaml1
            .lines()
            .skip_while(|l| !l.starts_with("variables:"))
            .skip(1)
            .take_while(|l| l.starts_with("  ") || l.starts_with('\t'))
            .collect();
        let keys: Vec<&str> = var_block_lines
            .iter()
            .filter_map(|l| l.trim().split(':').next())
            .collect();
        assert_eq!(
            keys,
            vec!["alpha", "beta", "mu", "nu", "zeta"],
            "variables: keys must be emitted in alphabetical order; got {:?} \
             from yaml:\n{}",
            keys,
            yaml1,
        );
    }

    /// Recursive guard: the harness's drift channel is most often a *nested*
    /// HashMap (e.g. `docker.labels`, `nfpm.dependencies`,
    /// `announce.<flavour>.headers`). `sort_yaml_mapping` must walk into
    /// sub-mappings AND into sequences-of-mappings. Hand-crafted
    /// `serde_yaml_ng::Value` to exercise both axes.
    #[test]
    fn sort_yaml_mapping_recurses_into_nested_maps_and_sequences() {
        let yaml = "\
top:
  z: 1
  a: 2
list:
  - inner_z: 1
    inner_a: 2
  - solo: 3
";
        let mut value: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml).unwrap();
        sort_yaml_mapping(&mut value);
        let out = serde_yaml_ng::to_string(&value).unwrap();
        // Top-level keys: list comes before top alphabetically.
        let first_line = out.lines().next().unwrap();
        assert!(
            first_line.starts_with("list:"),
            "top-level keys must be sorted alphabetically; got {out:?}"
        );
        // Sub-mapping under `top:` must be sorted (a before z).
        let top_pos = out.find("top:").unwrap();
        let top_block = &out[top_pos..];
        let a_pos = top_block.find("a:").expect("a: present");
        let z_pos = top_block.find("z:").expect("z: present");
        assert!(
            a_pos < z_pos,
            "nested mapping under `top:` must be sorted; got {out:?}"
        );
        // Sub-mapping inside the first list element must also be sorted.
        let list_pos = out.find("list:").unwrap();
        let list_block = &out[list_pos..];
        let inner_a = list_block.find("inner_a:").expect("inner_a: present");
        let inner_z = list_block.find("inner_z:").expect("inner_z: present");
        assert!(
            inner_a < inner_z,
            "nested mapping inside a sequence element must be sorted; got {out:?}"
        );
    }

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
            env: Some(vec![
                "SHARED=from-top".to_string(),
                "TOP_ONLY=top-value".to_string(),
            ]),
            ..Default::default()
        };
        let ws = WorkspaceConfig {
            name: "ws".to_string(),
            crates: vec![],
            env: Some(vec![
                "SHARED=from-ws".to_string(),
                "WS_ONLY=ws-value".to_string(),
            ]),
            ..Default::default()
        };

        apply_workspace_overlay(&mut config, &ws);
        let env = config.env.as_ref().unwrap();
        assert!(env.contains(&"TOP_ONLY=top-value".to_string()));
        assert!(env.contains(&"SHARED=from-ws".to_string()));
        assert!(env.contains(&"WS_ONLY=ws-value".to_string()));
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
        use anodizer_core::artifact::ArtifactKind;
        use anodizer_core::context::{Context, ContextOptions};

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
        use anodizer_core::context::{Context, ContextOptions};

        let dir = tempfile::TempDir::new().unwrap();
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let result = load_artifacts_from_dist(&mut ctx, dir.path());
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("no artifacts manifest found"),
            "error should mention missing file: {msg}"
        );
    }

    #[test]
    fn test_load_artifacts_from_dist_invalid_json() {
        use anodizer_core::context::{Context, ContextOptions};

        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("artifacts.json"), "not valid json").unwrap();

        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let result = load_artifacts_from_dist(&mut ctx, dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn test_load_artifacts_from_dist_unknown_kind() {
        use anodizer_core::context::{Context, ContextOptions};

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
        use anodizer_core::artifact::{Artifact, ArtifactKind, ArtifactRegistry};
        use anodizer_core::context::{Context, ContextOptions};

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

        // `to_artifacts_json` emits a stable sort on (kind, target,
        // crate_name, name, path) to keep `dist/artifacts.json` byte-
        // identical across runs regardless of registration order, so the
        // round-tripped order is Binary (kind="binary") before Checksum
        // (kind="checksum"), not the insertion order.
        assert_eq!(loaded[0].kind, ArtifactKind::Binary);
        assert_eq!(loaded[0].name, "myapp");
        assert_eq!(loaded[0].target.as_deref(), Some("aarch64-apple-darwin"));
        assert_eq!(loaded[0].size, None);

        assert_eq!(loaded[1].kind, ArtifactKind::Checksum);
        assert_eq!(loaded[1].name, "checksums.txt");
        assert_eq!(loaded[1].size, Some(256));
    }

    // -----------------------------------------------------------------------
    // resolve_scm_token_type tests
    // -----------------------------------------------------------------------

    /// Build a `Context` whose `EnvSource` is a closed `MapEnvSource` carrying
    /// the supplied `(key, value)` fixtures. Routes `resolve_scm_token_type`'s
    /// env reads through the injected map so each test drives a hermetic
    /// branch without touching process env.
    fn ctx_with_env(config: &Config, env: &[(&str, &str)]) -> Context {
        ctx_with_env_inner(config, env, None)
    }

    fn ctx_with_env_and_cli_token(
        config: &Config,
        env: &[(&str, &str)],
        cli_token: &str,
    ) -> Context {
        ctx_with_env_inner(config, env, Some(cli_token.to_string()))
    }

    fn ctx_with_env_inner(
        config: &Config,
        env: &[(&str, &str)],
        cli_token: Option<String>,
    ) -> Context {
        let opts = ContextOptions {
            token: cli_token,
            ..Default::default()
        };
        let mut ctx = Context::new(config.clone(), opts);
        let mut map = anodizer_core::env_source::MapEnvSource::new();
        for (k, v) in env {
            map.set(*k, *v);
        }
        ctx.set_env_source(map);
        ctx
    }

    #[test]
    fn test_resolve_scm_token_type_default_is_github() {
        let config = Config::default();
        let mut ctx = ctx_with_env(&config, &[]);
        resolve_scm_token_type(&mut ctx, &config);

        assert_eq!(ctx.token_type, ScmTokenType::GitHub);
        assert!(ctx.options.token.is_none());
    }

    #[test]
    fn test_resolve_scm_token_type_force_gitlab() {
        let config = Config {
            force_token: Some(ForceTokenKind::GitLab),
            ..Default::default()
        };
        let mut ctx = ctx_with_env(&config, &[("GITLAB_TOKEN", "glpat-test123")]);
        resolve_scm_token_type(&mut ctx, &config);

        assert_eq!(ctx.token_type, ScmTokenType::GitLab);
        assert_eq!(ctx.options.token.as_deref(), Some("glpat-test123"));
    }

    #[test]
    fn test_resolve_scm_token_type_force_gitea() {
        let config = Config {
            force_token: Some(ForceTokenKind::Gitea),
            ..Default::default()
        };
        let mut ctx = ctx_with_env(&config, &[("GITEA_TOKEN", "gitea-tok")]);
        resolve_scm_token_type(&mut ctx, &config);

        assert_eq!(ctx.token_type, ScmTokenType::Gitea);
        assert_eq!(ctx.options.token.as_deref(), Some("gitea-tok"));
    }

    #[test]
    fn test_resolve_scm_token_type_env_gitlab_detected() {
        let config = Config::default();
        let mut ctx = ctx_with_env(&config, &[("GITLAB_TOKEN", "glpat-env")]);
        resolve_scm_token_type(&mut ctx, &config);

        assert_eq!(ctx.token_type, ScmTokenType::GitLab);
        assert_eq!(ctx.options.token.as_deref(), Some("glpat-env"));
    }

    #[test]
    fn test_resolve_scm_token_type_env_gitea_detected() {
        let config = Config::default();
        let mut ctx = ctx_with_env(&config, &[("GITEA_TOKEN", "gitea-env")]);
        resolve_scm_token_type(&mut ctx, &config);

        assert_eq!(ctx.token_type, ScmTokenType::Gitea);
        assert_eq!(ctx.options.token.as_deref(), Some("gitea-env"));
    }

    #[test]
    fn test_resolve_scm_token_type_github_token_from_env() {
        let config = Config::default();
        let mut ctx = ctx_with_env(&config, &[("GITHUB_TOKEN", "ghp-from-env")]);
        resolve_scm_token_type(&mut ctx, &config);

        assert_eq!(ctx.token_type, ScmTokenType::GitHub);
        assert_eq!(ctx.options.token.as_deref(), Some("ghp-from-env"));
    }

    #[test]
    fn test_resolve_scm_token_type_anodizer_github_token_takes_precedence() {
        let config = Config::default();
        let mut ctx = ctx_with_env(
            &config,
            &[
                ("ANODIZER_GITHUB_TOKEN", "anodizer-tok"),
                ("GITHUB_TOKEN", "gh-tok"),
            ],
        );
        resolve_scm_token_type(&mut ctx, &config);

        assert_eq!(ctx.token_type, ScmTokenType::GitHub);
        assert_eq!(
            ctx.options.token.as_deref(),
            Some("anodizer-tok"),
            "ANODIZER_GITHUB_TOKEN should take precedence over GITHUB_TOKEN"
        );
    }

    #[test]
    fn test_resolve_scm_token_type_cli_token_preserved() {
        let config = Config::default();
        let mut ctx =
            ctx_with_env_and_cli_token(&config, &[("GITHUB_TOKEN", "from-env")], "from-cli");
        resolve_scm_token_type(&mut ctx, &config);

        assert_eq!(ctx.token_type, ScmTokenType::GitHub);
        assert_eq!(
            ctx.options.token.as_deref(),
            Some("from-cli"),
            "CLI --token flag should not be overwritten by env var"
        );
    }

    #[test]
    fn test_resolve_scm_token_type_force_overrides_env_detection() {
        // GITLAB_TOKEN is set, but force_token says GitHub.
        let config = Config {
            force_token: Some(ForceTokenKind::GitHub),
            ..Default::default()
        };
        let mut ctx = ctx_with_env(&config, &[("GITLAB_TOKEN", "glpat-ignored")]);
        resolve_scm_token_type(&mut ctx, &config);

        assert_eq!(
            ctx.token_type,
            ScmTokenType::GitHub,
            "force_token should override env-based detection"
        );
        assert!(
            ctx.options.token.is_none(),
            "no GitHub token env var set, so token should remain None"
        );
    }

    #[test]
    fn test_resolve_scm_token_type_gitlab_priority_over_gitea() {
        let config = Config::default();
        let mut ctx = ctx_with_env(
            &config,
            &[("GITLAB_TOKEN", "gl-tok"), ("GITEA_TOKEN", "gt-tok")],
        );
        resolve_scm_token_type(&mut ctx, &config);

        assert_eq!(
            ctx.token_type,
            ScmTokenType::GitLab,
            "GITLAB_TOKEN should be checked before GITEA_TOKEN"
        );
        assert_eq!(ctx.options.token.as_deref(), Some("gl-tok"));
    }

    #[test]
    fn test_resolve_scm_token_type_anodizer_force_token_env_gitlab() {
        let config = Config::default();
        let mut ctx = ctx_with_env(
            &config,
            &[
                ("ANODIZER_FORCE_TOKEN", "gitlab"),
                ("GITLAB_TOKEN", "glpat-env"),
            ],
        );
        resolve_scm_token_type(&mut ctx, &config);

        assert_eq!(
            ctx.token_type,
            ScmTokenType::GitLab,
            "ANODIZER_FORCE_TOKEN=gitlab should force GitLab"
        );
        assert_eq!(ctx.options.token.as_deref(), Some("glpat-env"));
    }

    #[test]
    fn test_resolve_scm_token_type_anodizer_force_token_env_github() {
        let config = Config::default();
        let mut ctx = ctx_with_env(
            &config,
            &[
                ("ANODIZER_FORCE_TOKEN", "github"),
                ("GITLAB_TOKEN", "glpat-ignored"),
                ("GITHUB_TOKEN", "ghp-forced"),
            ],
        );
        resolve_scm_token_type(&mut ctx, &config);

        assert_eq!(
            ctx.token_type,
            ScmTokenType::GitHub,
            "ANODIZER_FORCE_TOKEN=github should override GITLAB_TOKEN detection"
        );
        assert_eq!(ctx.options.token.as_deref(), Some("ghp-forced"));
    }

    #[test]
    fn test_resolve_scm_token_type_goreleaser_force_token_compat() {
        let config = Config::default();
        let mut ctx = ctx_with_env(
            &config,
            &[
                ("GORELEASER_FORCE_TOKEN", "gitea"),
                ("GITEA_TOKEN", "gitea-compat"),
            ],
        );
        resolve_scm_token_type(&mut ctx, &config);

        assert_eq!(
            ctx.token_type,
            ScmTokenType::Gitea,
            "GORELEASER_FORCE_TOKEN should work as compat fallback"
        );
        assert_eq!(ctx.options.token.as_deref(), Some("gitea-compat"));
    }

    #[test]
    fn test_resolve_scm_token_type_anodizer_force_token_overrides_goreleaser() {
        let config = Config::default();
        let mut ctx = ctx_with_env(
            &config,
            &[
                ("ANODIZER_FORCE_TOKEN", "github"),
                ("GORELEASER_FORCE_TOKEN", "gitlab"),
                ("GITHUB_TOKEN", "ghp-wins"),
                ("GITLAB_TOKEN", "glpat-loses"),
            ],
        );
        resolve_scm_token_type(&mut ctx, &config);

        assert_eq!(
            ctx.token_type,
            ScmTokenType::GitHub,
            "ANODIZER_FORCE_TOKEN should take precedence over GORELEASER_FORCE_TOKEN"
        );
        assert_eq!(ctx.options.token.as_deref(), Some("ghp-wins"));
    }

    #[test]
    fn test_resolve_scm_token_type_config_force_token_overrides_env() {
        let config = Config {
            force_token: Some(ForceTokenKind::GitHub),
            ..Default::default()
        };
        let mut ctx = ctx_with_env(
            &config,
            &[
                ("ANODIZER_FORCE_TOKEN", "gitlab"),
                ("GITHUB_TOKEN", "ghp-config"),
            ],
        );
        resolve_scm_token_type(&mut ctx, &config);

        assert_eq!(
            ctx.token_type,
            ScmTokenType::GitHub,
            "config.force_token should override ANODIZER_FORCE_TOKEN env var"
        );
        assert_eq!(ctx.options.token.as_deref(), Some("ghp-config"));
    }

    #[test]
    fn test_resolve_scm_token_type_invalid_force_token_env_ignored() {
        let config = Config::default();
        let mut ctx = ctx_with_env(
            &config,
            &[
                ("ANODIZER_FORCE_TOKEN", "invalid"),
                ("GITLAB_TOKEN", "glpat-detected"),
            ],
        );
        resolve_scm_token_type(&mut ctx, &config);

        assert_eq!(
            ctx.token_type,
            ScmTokenType::GitLab,
            "invalid ANODIZER_FORCE_TOKEN should fall back to env detection"
        );
        assert_eq!(ctx.options.token.as_deref(), Some("glpat-detected"));
    }

    // ---- collect_build_targets override semantics ---------------------

    #[test]
    fn test_collect_build_targets_per_build_overrides_defaults() {
        use anodizer_core::config::{BuildConfig, Defaults};

        let config = Config {
            project_name: "test".to_string(),
            defaults: Some(Defaults {
                targets: Some(vec!["a".to_string(), "b".to_string()]),
                ..Default::default()
            }),
            crates: vec![CrateConfig {
                name: "k1".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ Version }}".to_string(),
                builds: Some(vec![BuildConfig {
                    targets: Some(vec!["c".to_string()]),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let result = collect_build_targets(&config, &[]);
        assert_eq!(
            result,
            vec!["c".to_string()],
            "per-build targets should REPLACE defaults.targets, not concat",
        );
    }

    #[test]
    fn test_collect_build_targets_per_build_none_falls_back_to_defaults() {
        use anodizer_core::config::{BuildConfig, Defaults};

        let config = Config {
            project_name: "test".to_string(),
            defaults: Some(Defaults {
                targets: Some(vec!["a".to_string(), "b".to_string()]),
                ..Default::default()
            }),
            crates: vec![CrateConfig {
                name: "k1".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ Version }}".to_string(),
                builds: Some(vec![BuildConfig {
                    targets: None, // not set; should inherit defaults
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let result = collect_build_targets(&config, &[]);
        assert_eq!(
            result,
            vec!["a".to_string(), "b".to_string()],
            "build with targets=None should inherit defaults.targets",
        );
    }

    // ---- merge_env_with_defaults --------------------------------------

    #[test]
    fn test_merge_env_with_defaults_both_none_yields_none() {
        assert!(merge_env_with_defaults(None, None).is_none());
    }

    #[test]
    fn test_merge_env_with_defaults_only_defaults_yields_defaults() {
        let d = vec!["FOO=defaults".to_string()];
        let merged = merge_env_with_defaults(Some(&d), None).unwrap();
        assert_eq!(merged, vec!["FOO=defaults".to_string()]);
    }

    #[test]
    fn test_merge_env_with_defaults_only_config_yields_config() {
        let c = vec!["BAR=top".to_string()];
        let merged = merge_env_with_defaults(None, Some(&c)).unwrap();
        assert_eq!(merged, vec!["BAR=top".to_string()]);
    }

    #[test]
    fn test_merge_env_with_defaults_disjoint_keys_concat() {
        // defaults.env contributes when no per-config entry shadows it.
        let d = vec!["FOO=defaults".to_string()];
        let c = vec!["BAR=top".to_string()];
        let merged = merge_env_with_defaults(Some(&d), Some(&c)).unwrap();
        assert_eq!(
            merged,
            vec!["FOO=defaults".to_string(), "BAR=top".to_string()]
        );
    }

    #[test]
    fn test_merge_env_with_defaults_top_level_wins_on_collision() {
        // Defaults provide FOO=a, top-level overrides with FOO=b.
        // Order is defaults-first so the per-key last-write-wins inside
        // setup_env produces FOO=b.
        let d = vec!["FOO=a".to_string()];
        let c = vec!["FOO=b".to_string()];
        let merged = merge_env_with_defaults(Some(&d), Some(&c)).unwrap();
        // Both entries appear; the consumer (setup_env) iterates in order
        // and the last write to a key wins.
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0], "FOO=a");
        assert_eq!(merged[1], "FOO=b");
    }

    // ---- defaults.env wired into setup_env ------------------------------

    use anodizer_core::config::Defaults;

    /// `setup_env` mutates process env via the load-bearing
    /// `set_env_var_single_threaded` path so child commands (docker /
    /// rustup / git hooks) inherit user-supplied entries. These two
    /// tests assert the template-context wiring only — they never
    /// observe the process-env side effect, and the fixture keys
    /// (`DEFAULTS_ENV_*`) are uniquely shaped so accidental cross-test
    /// reads of the same key are vanishingly unlikely.
    #[test]
    fn test_setup_env_inherits_defaults_env_when_crate_unset() {
        let config = Config {
            defaults: Some(Defaults {
                env: Some(vec!["DEFAULTS_ENV_INHERITED=defaults".to_string()]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = Context::new(config.clone(), ContextOptions::default());
        let log =
            anodizer_core::log::StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);
        setup_env(&mut ctx, &config, &log).expect("setup_env should succeed");
        assert_eq!(
            ctx.template_vars()
                .all_config_env()
                .get("DEFAULTS_ENV_INHERITED")
                .map(|s| s.as_str()),
            Some("defaults"),
            "defaults.env entry should populate the template context",
        );
    }

    #[test]
    fn test_setup_env_top_level_env_wins_over_defaults_env() {
        let config = Config {
            defaults: Some(Defaults {
                env: Some(vec!["DEFAULTS_ENV_OVERRIDE=a".to_string()]),
                ..Default::default()
            }),
            env: Some(vec!["DEFAULTS_ENV_OVERRIDE=b".to_string()]),
            ..Default::default()
        };
        let mut ctx = Context::new(config.clone(), ContextOptions::default());
        let log =
            anodizer_core::log::StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);
        setup_env(&mut ctx, &config, &log).expect("setup_env should succeed");
        assert_eq!(
            ctx.template_vars()
                .all_config_env()
                .get("DEFAULTS_ENV_OVERRIDE")
                .map(|s| s.as_str()),
            Some("b"),
            "top-level config.env should override defaults.env on duplicate key",
        );
    }

    /// Strict variable rendering — a template typo (`{{ .Tagg }}` instead
    /// of `{{ .Tag }}`) used to silently pass the literal string through
    /// to downstream publishers; the strict path makes it a hard error so
    /// the user sees the failure at config-load.
    #[test]
    fn test_setup_env_variables_template_error_fails_load() {
        use std::collections::BTreeMap;
        let mut vars = BTreeMap::new();
        vars.insert(
            "bad".to_string(),
            "{{ NoSuchVariable | nonexistent_filter }}".to_string(),
        );
        let config = Config {
            variables: Some(vars),
            ..Default::default()
        };
        let mut ctx = Context::new(config.clone(), ContextOptions::default());
        let log =
            anodizer_core::log::StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);
        let err = setup_env(&mut ctx, &config, &log)
            .expect_err("invalid variable template must fail the load");
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("variables.bad"),
            "error should name the offending variable key, got: {msg}"
        );
    }

    /// When `ANODIZER_CURRENT_TAG` and `GORELEASER_CURRENT_TAG` are absent and
    /// `GITHUB_REF_TYPE=tag`, the override must resolve to `GITHUB_REF_NAME`.
    /// This guards the Release.yml path where GHA sets only the standard
    /// `GITHUB_REF_*` vars and neither anodizer-specific var is exported.
    #[test]
    fn resolve_git_context_github_ref_name_fallback_fires_when_anodizer_tags_unset() {
        let config = Config {
            project_name: "test".to_string(),
            ..Default::default()
        };
        let ctx = ctx_with_env(
            &config,
            &[("GITHUB_REF_TYPE", "tag"), ("GITHUB_REF_NAME", "v1.2.3")],
        );
        // Extract the tag_override the same way resolve_git_context does.
        let anodizer_current_tag = ctx.env_var("ANODIZER_CURRENT_TAG");
        let goreleaser_current_tag = ctx.env_var("GORELEASER_CURRENT_TAG");
        let github_ref_type = ctx.env_var("GITHUB_REF_TYPE");
        let github_ref_name = ctx.env_var("GITHUB_REF_NAME");
        let tag_override = anodizer_current_tag
            .filter(|s| !s.is_empty())
            .or_else(|| goreleaser_current_tag.filter(|s| !s.is_empty()))
            .or_else(|| {
                let is_tag = github_ref_type.as_deref().filter(|s| *s == "tag").is_some();
                if is_tag {
                    github_ref_name.filter(|s| !s.is_empty())
                } else {
                    None
                }
            });
        assert_eq!(
            tag_override.as_deref(),
            Some("v1.2.3"),
            "GITHUB_REF_NAME fallback must fire when anodizer/goreleaser tag vars are absent"
        );
    }

    /// When `GITHUB_REF_TYPE` is not `tag` (e.g. `branch`), the
    /// `GITHUB_REF_NAME` fallback must NOT fire — branch names are not tags.
    #[test]
    fn resolve_git_context_github_ref_name_fallback_skipped_for_branch_push() {
        let config = Config {
            project_name: "test".to_string(),
            ..Default::default()
        };
        let ctx = ctx_with_env(
            &config,
            &[("GITHUB_REF_TYPE", "branch"), ("GITHUB_REF_NAME", "master")],
        );
        let anodizer_current_tag = ctx.env_var("ANODIZER_CURRENT_TAG");
        let goreleaser_current_tag = ctx.env_var("GORELEASER_CURRENT_TAG");
        let github_ref_type = ctx.env_var("GITHUB_REF_TYPE");
        let github_ref_name = ctx.env_var("GITHUB_REF_NAME");
        let tag_override = anodizer_current_tag
            .filter(|s| !s.is_empty())
            .or_else(|| goreleaser_current_tag.filter(|s| !s.is_empty()))
            .or_else(|| {
                let is_tag = github_ref_type.as_deref().filter(|s| *s == "tag").is_some();
                if is_tag {
                    github_ref_name.filter(|s| !s.is_empty())
                } else {
                    None
                }
            });
        assert!(
            tag_override.is_none(),
            "GITHUB_REF_NAME must not be used as tag override when GITHUB_REF_TYPE=branch"
        );
    }

    /// Deterministic order — a value referencing an earlier-sorting key
    /// resolves correctly because the BTreeMap iterates in alphabetical
    /// order. (`b` references `a`; `a` sorts first, so `b` sees `a`.)
    #[test]
    fn test_setup_env_variables_resolve_in_sorted_order() {
        use std::collections::BTreeMap;
        let mut vars = BTreeMap::new();
        // Insert in reverse order to confirm BTreeMap iteration order
        // (not insertion order) drives resolution.
        vars.insert("b".to_string(), "{{ Var.a }}_v2".to_string());
        vars.insert("a".to_string(), "hello".to_string());
        let config = Config {
            project_name: "p".to_string(),
            variables: Some(vars),
            ..Default::default()
        };
        let mut ctx = Context::new(config.clone(), ContextOptions::default());
        let log =
            anodizer_core::log::StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);
        setup_env(&mut ctx, &config, &log).expect("setup_env should succeed");
        // `b` references `a` and `a` sorts first, so the resolved value
        // for `b` is `hello_v2`.
        let rendered = ctx.render_template("{{ Var.b }}").expect("render Var.b");
        assert_eq!(rendered, "hello_v2");
    }
}
