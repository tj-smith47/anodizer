use anodizer_core::config::{GitConfig, TagConfig};
use anodizer_core::git;
use anodizer_core::hooks::run_hooks;
use anodizer_core::log::{StageLogger, Verbosity};
use anodizer_core::template::TemplateVars;
use anyhow::{Result, bail};
use regex::Regex;
use std::path::{Path, PathBuf};

use crate::commands::bump::cargo_edit::{WorkspaceInfo, apply_plan, load_workspace};
use crate::commands::bump::plan::{BumpLevel, PlanRow};

pub struct TagOpts {
    pub dry_run: bool,
    pub custom_tag: Option<String>,
    pub default_bump: Option<String>,
    /// When set, select a specific crate's tag_template for tagging.
    pub crate_name: Option<String>,
    pub config_override: Option<std::path::PathBuf>,
    pub verbose: bool,
    pub debug: bool,
    pub quiet: bool,
    pub strict: bool,
}

/// Resolved tag configuration with defaults applied.
struct ResolvedConfig {
    default_bump: String,
    tag_prefix: String,
    release_branches: Vec<String>,
    custom_tag: Option<String>,
    tag_context: String,
    branch_history: String,
    initial_version: String,
    prerelease: bool,
    prerelease_suffix: String,
    force_without_changes: bool,
    force_without_changes_pre: bool,
    major_string_token: String,
    minor_string_token: String,
    patch_string_token: String,
    none_string_token: String,
    git_api_tagging: bool,
}

impl ResolvedConfig {
    fn from_tag_config(cfg: &TagConfig, opts: &TagOpts) -> Self {
        ResolvedConfig {
            default_bump: opts
                .default_bump
                .clone()
                .or_else(|| cfg.default_bump.clone())
                .unwrap_or_else(|| "minor".to_string()),
            tag_prefix: cfg.tag_prefix.clone().unwrap_or_else(|| "v".to_string()),
            release_branches: cfg.release_branches.clone().unwrap_or_default(),
            custom_tag: opts.custom_tag.clone().or_else(|| cfg.custom_tag.clone()),
            tag_context: cfg
                .tag_context
                .clone()
                .unwrap_or_else(|| "repo".to_string()),
            branch_history: cfg
                .branch_history
                .clone()
                .unwrap_or_else(|| "compare".to_string()),
            initial_version: cfg
                .initial_version
                .clone()
                .unwrap_or_else(|| "0.0.0".to_string()),
            prerelease: cfg.prerelease.unwrap_or(false),
            prerelease_suffix: cfg
                .prerelease_suffix
                .clone()
                .unwrap_or_else(|| "beta".to_string()),
            force_without_changes: cfg.force_without_changes.unwrap_or(false),
            force_without_changes_pre: cfg.force_without_changes_pre.unwrap_or(false),
            major_string_token: cfg
                .major_string_token
                .clone()
                .unwrap_or_else(|| "#major".to_string()),
            minor_string_token: cfg
                .minor_string_token
                .clone()
                .unwrap_or_else(|| "#minor".to_string()),
            patch_string_token: cfg
                .patch_string_token
                .clone()
                .unwrap_or_else(|| "#patch".to_string()),
            none_string_token: cfg
                .none_string_token
                .clone()
                .unwrap_or_else(|| "#none".to_string()),
            git_api_tagging: cfg.git_api_tagging.unwrap_or(false),
        }
    }
}

pub fn run(opts: TagOpts) -> Result<()> {
    // Load config if available, but don't fail if there's no config file
    let tag_config = load_tag_config(&opts);
    let git_config = load_git_config(&opts);

    let mut cfg = ResolvedConfig::from_tag_config(&tag_config, &opts);

    // When --crate is given, look up the crate in config and derive the tag
    // prefix from its tag_template.  Also capture the crate path so we can
    // scope change detection to only that directory.
    let mut crate_path: Option<String> = None;
    let mut version_sync_enabled = false;
    if let Some(ref crate_name) = opts.crate_name
        && let Some(info) = load_crate_tag_info(&opts, crate_name)
    {
        cfg.tag_prefix = info.tag_prefix;
        crate_path = Some(info.path);
        version_sync_enabled = info.version_sync;
    }

    // Workspace-mode: with no --crate, treat a Cargo workspace whose members
    // inherit [workspace.package].version as a single versioned unit. The
    // tag-derived version gets applied to root Cargo.toml + every member
    // manifest + workspace.dependencies pins before the tag is created, so
    // the tagged commit has Cargo.toml at the version the tag advertises.
    let workspace_root_path = std::env::current_dir().ok();
    let workspace_info: Option<WorkspaceInfo> = match (&opts.crate_name, &workspace_root_path) {
        (None, Some(root)) => load_workspace(root)
            .ok()
            .filter(|ws| ws.workspace_package_version.is_some()),
        _ => None,
    };

    // Merge verbose from config: if config says verbose=true and CLI doesn't say quiet, enable verbose
    let config_verbose = tag_config.verbose.unwrap_or(false);
    let effective_verbose = opts.verbose || (config_verbose && !opts.quiet);
    let log = StageLogger::new(
        "tag",
        Verbosity::from_flags(opts.quiet, effective_verbose, opts.debug),
    );

    log.status(&format!(
        "running auto-tag{}",
        if opts.dry_run { " (dry-run)" } else { "" }
    ));

    // Helper closure to create a tag via the appropriate method, with
    // tag_pre_hooks / tag_post_hooks wrapping. Hooks receive template vars
    // `{{ .Tag }}`, `{{ .PrefixedTag }}`, `{{ .Version }}`, `{{ .PreviousTag }}`
    // and process env `ANODIZER_CURRENT_TAG` / `ANODIZER_PREVIOUS_TAG`.
    let strict = opts.strict;
    let tag_prefix_for_hooks = cfg.tag_prefix.clone();
    let pre_hooks = tag_config.tag_pre_hooks.clone().unwrap_or_default();
    let post_hooks = tag_config.tag_post_hooks.clone().unwrap_or_default();
    let create_tag = |tag: &str, message: &str, dry_run: bool, prev: Option<&str>| -> Result<()> {
        let mut tv = TemplateVars::new();
        tv.set("Tag", tag);
        tv.set("PrefixedTag", tag);
        let version = tag
            .strip_prefix(tag_prefix_for_hooks.as_str())
            .unwrap_or(tag);
        tv.set("Version", version);
        if let Some(p) = prev {
            tv.set("PreviousTag", p);
        }

        // SAFETY: the tag subcommand runs single-threaded — no worker threads
        // exist here, so mutating the process env is safe. Hooks read these
        // via their subprocess environment.
        unsafe {
            std::env::set_var("ANODIZER_CURRENT_TAG", tag);
            if let Some(p) = prev {
                std::env::set_var("ANODIZER_PREVIOUS_TAG", p);
            }
        }

        if !pre_hooks.is_empty() {
            run_hooks(&pre_hooks, "tag-pre", dry_run, &log, Some(&tv))?;
        }

        if cfg.git_api_tagging {
            log.verbose("using GitHub API for tagging (git_api_tagging=true)");
            git::create_tag_via_github_api(tag, message, dry_run, &log, strict)?;
        } else {
            git::create_and_push_tag(tag, message, dry_run, &log, strict)?;
        }

        if !post_hooks.is_empty() {
            run_hooks(&post_hooks, "tag-post", dry_run, &log, Some(&tv))?;
        }
        Ok(())
    };

    // If custom_tag is set, use it directly
    if let Some(ref custom) = cfg.custom_tag {
        let new_tag = if custom.starts_with(&cfg.tag_prefix) {
            custom.clone()
        } else {
            format!("{}{}", cfg.tag_prefix, custom)
        };
        log.verbose(&format!("using custom tag: {}", new_tag));
        let prev_for_custom = find_previous_tag(&cfg, git_config.as_ref()).ok().flatten();
        create_tag(
            &new_tag,
            &format!("Release {}", new_tag),
            opts.dry_run,
            prev_for_custom.as_deref(),
        )?;
        println!("new_tag={}", new_tag);
        println!("old_tag=");
        println!("part=custom");
        return Ok(());
    }

    // Check release branches
    let current_branch = git::get_current_branch()?;
    if !cfg.release_branches.is_empty() && !branch_matches(&current_branch, &cfg.release_branches) {
        // Non-release branch: produce a hash-postfixed version, don't tag
        let short_commit = git::get_short_commit()?;
        let prev_tag = find_previous_tag(&cfg, git_config.as_ref())?;
        let base_version = match &prev_tag {
            Some(tag) => {
                let sv = git::parse_semver_tag(tag)?;
                format!("{}.{}.{}", sv.major, sv.minor, sv.patch)
            }
            None => cfg.initial_version.clone(),
        };
        let hash_tag = format!("{}{}-{}", cfg.tag_prefix, base_version, short_commit);
        log.verbose(&format!(
            "branch '{}' is not a release branch, producing hash-postfixed version: {}",
            current_branch, hash_tag
        ));
        println!("new_tag={}", hash_tag);
        println!("old_tag={}", prev_tag.as_deref().unwrap_or(""));
        println!("part=none");
        return Ok(());
    }

    // Find previous tag
    let prev_tag = find_previous_tag(&cfg, git_config.as_ref())?;

    log.verbose(&format!(
        "previous tag: {}",
        prev_tag.as_deref().unwrap_or("(none)")
    ));

    // Check for changes since last tag.  When a crate path is known, scope
    // to that directory so unrelated commits don't trigger a spurious bump.
    if let Some(ref tag) = prev_tag {
        let has_changes = if let Some(ref path) = crate_path {
            git::has_changes_since(tag, path)?
        } else {
            git::has_commits_since_tag(tag)?
        };
        if !has_changes {
            let force = if cfg.prerelease {
                cfg.force_without_changes_pre
            } else {
                cfg.force_without_changes
            };
            if !force {
                log.verbose(&format!("no changes since {} -- skipping", tag));
                println!("new_tag={}", tag);
                println!("old_tag={}", tag);
                println!("part=none");
                return Ok(());
            }
            log.verbose(&format!(
                "no changes since {}, but force_without_changes is enabled",
                tag
            ));
        }
    }

    // Scan commit messages to determine bump.  When a crate path is set,
    // only consider commits that actually touched that directory.
    let messages = get_messages_for_bump(&cfg, prev_tag.as_deref(), crate_path.as_deref())?;
    log.verbose(&format!("scanned {} commit message(s)", messages.len()));

    // Detect bump
    let bump = detect_bump(&messages, &cfg);
    log.verbose(&format!("detected bump: {:?}", bump));

    // A manually-bumped Cargo.toml that is strictly ahead of the previous
    // tag is itself a release signal — the operator has explicitly set the
    // next version. Honor it even when no per-commit bump signal fired and
    // even when the crate path had no changes. This prevents autotag from
    // stalling at the old tag after a manual `cargo set-version` bump.
    let cargo_ahead = if let Some(ws) = &workspace_info {
        match (
            ws.workspace_package_version
                .as_deref()
                .and_then(|v| git::parse_semver(v).ok()),
            prev_tag
                .as_deref()
                .and_then(|t| git::parse_semver_tag(t).ok()),
        ) {
            (Some(c), Some(p)) => (c.major, c.minor, c.patch) > (p.major, p.minor, p.patch),
            _ => false,
        }
    } else {
        version_sync_enabled
            && match (crate_path.as_deref(), prev_tag.as_deref()) {
                (Some(path), Some(prev)) => {
                    match (
                        anodizer_stage_build::version_sync::read_cargo_version(path)
                            .ok()
                            .and_then(|v| git::parse_semver(&v).ok()),
                        git::parse_semver_tag(prev).ok(),
                    ) {
                        (Some(c), Some(p)) => {
                            (c.major, c.minor, c.patch) > (p.major, p.minor, p.patch)
                        }
                        _ => false,
                    }
                }
                _ => false,
            }
    };

    // If #none token detected (and Cargo.toml isn't explicitly ahead), skip.
    if bump == BumpKind::None && !cargo_ahead {
        log.verbose("no bump signal and Cargo.toml not ahead -- skipping tag");
        println!("new_tag={}", prev_tag.as_deref().unwrap_or(""));
        println!("old_tag={}", prev_tag.as_deref().unwrap_or(""));
        println!("part=none");
        return Ok(());
    }

    // Determine base version.
    // When there is no previous tag, use initial_version directly without bumping
    // (matching github-tag-action behavior: initial_version IS the first tag).
    let (new_major, new_minor, new_patch, old_tag_str) = if let Some(ref prev) = prev_tag {
        let base = git::parse_semver_tag(prev)?;
        let (maj, min, pat) = apply_bump(base.major, base.minor, base.patch, &bump);
        (maj, min, pat, prev.as_str())
    } else {
        let base = git::parse_semver_tag(&format!("{}{}", cfg.tag_prefix, cfg.initial_version))
            .unwrap_or(git::SemVer {
                major: 0,
                minor: 1,
                patch: 0,
                prerelease: None,
                build_metadata: None,
            });
        (base.major, base.minor, base.patch, "")
    };

    // Build new version string
    let mut new_version = format!("{}.{}.{}", new_major, new_minor, new_patch);

    // Handle prerelease
    if cfg.prerelease {
        new_version = format!("{}-{}", new_version, cfg.prerelease_suffix);
    }

    // Bug fix: when version_sync is enabled, check if the current Cargo.toml
    // version is already higher than the tag-derived version. If so, use the
    // Cargo.toml version to avoid downgrading manually bumped versions.
    let cargo_current_ver: Option<String> = if let Some(ws) = &workspace_info {
        ws.workspace_package_version.clone()
    } else if version_sync_enabled && let Some(ref path) = crate_path {
        anodizer_stage_build::version_sync::read_cargo_version(path).ok()
    } else {
        None
    };
    if let Some(cargo_ver) = cargo_current_ver
        && let Ok(cargo_sv) = git::parse_semver(&cargo_ver)
    {
        let tag_tuple = (new_major, new_minor, new_patch);
        let cargo_tuple = (cargo_sv.major, cargo_sv.minor, cargo_sv.patch);
        if cargo_tuple > tag_tuple {
            log.status(&format!(
                "Cargo.toml version {} > tag-derived {}, using Cargo.toml version",
                cargo_ver, new_version
            ));
            new_version = cargo_ver;
        }
    }

    let new_tag = format!("{}{}", cfg.tag_prefix, new_version);

    log.verbose(&format!("{} -> {}", old_tag_str, new_tag));

    // When version_sync is enabled for this crate, update the Cargo.toml
    // version and commit before tagging so the tagged commit has the correct
    // version embedded.  This ensures cargo publish reads the right version.
    //
    // Also update intra-workspace dependency version specs so that other
    // crates referencing this one via path+version don't break.
    //
    // The commit message intentionally OMITS `[skip ci]`. Earlier revisions
    // added the marker to suppress the master push's follow-up CI run, but
    // GitHub also suppresses tag-push workflow triggers when the tag target
    // commit's message contains `[skip ci]` — which silently broke the
    // release workflow trigger for autotag-created tags. The version-sync
    // commit is treated like any normal commit: its master push re-runs
    // CI, but the autotag job on that re-run no-ops because no new
    // release-worthy commits are present since the freshly-created tag
    // (see the conventional-commit gate in detect_bump).
    if let Some(ws) = &workspace_info {
        let root = workspace_root_path
            .as_deref()
            .unwrap_or_else(|| Path::new("."));
        apply_workspace_bump(root, ws, &new_version, opts.dry_run, &log)?;
    } else if let Some(ref path) = crate_path
        && version_sync_enabled
    {
        anodizer_stage_build::version_sync::sync_version(path, &new_version, opts.dry_run, &log)?;

        // Determine workspace root for cross-crate dep updates.
        let workspace_root = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".to_string());

        // Read the crate name from its Cargo.toml for dep scanning.
        let crate_cargo = std::path::Path::new(path).join("Cargo.toml");
        let crate_name = if let Ok(content) = std::fs::read_to_string(&crate_cargo) {
            content
                .parse::<toml_edit::DocumentMut>()
                .ok()
                .and_then(|doc| {
                    doc.get("package")
                        .and_then(|p| p.get("name"))
                        .and_then(|n| n.as_str())
                        .map(|s| s.to_string())
                })
        } else {
            None
        };

        // Update dependency version specs in other workspace crates.
        let dep_modified = if let Some(ref name) = crate_name {
            anodizer_stage_build::version_sync::sync_workspace_deps(
                &workspace_root,
                name,
                &new_version,
                opts.dry_run,
                &log,
            )?
        } else {
            vec![]
        };

        if !opts.dry_run {
            // Regenerate Cargo.lock to match the bumped Cargo.toml versions.
            // Without this, the tagged commit has Cargo.toml at the new version
            // but Cargo.lock at the old version, causing `cargo test` (from
            // before hooks) to update Cargo.lock and dirty the tree.
            let lock_updated = std::process::Command::new("cargo")
                .args(["update", "--workspace"])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if !lock_updated {
                log.warn(
                    "version-sync: `cargo update --workspace` failed; Cargo.lock may be stale",
                );
            }

            let cargo_toml = format!("{}/Cargo.toml", path);
            let mut files_to_stage: Vec<&str> = vec![&cargo_toml, "Cargo.lock"];
            for f in &dep_modified {
                files_to_stage.push(f);
            }
            let _ = git::stage_and_commit(
                &files_to_stage,
                &format!("chore: bump {} to {}", path, new_version),
            );
        }
    }

    // Create and push tag
    let prev_for_hook = if old_tag_str.is_empty() {
        None
    } else {
        Some(old_tag_str)
    };
    create_tag(
        &new_tag,
        &format!("Release {}", new_tag),
        opts.dry_run,
        prev_for_hook,
    )?;

    let part_str = match bump {
        BumpKind::Major => "major",
        BumpKind::Minor => "minor",
        BumpKind::Patch => "patch",
        BumpKind::None => "none",
    };

    println!("new_tag={}", new_tag);
    println!("old_tag={}", old_tag_str);
    println!("part={}", part_str);

    Ok(())
}

/// Workspace-wide version bump for the "single tag, many crates" layout.
///
/// Rewrites `[workspace.package].version`, every member manifest that doesn't
/// inherit, and every `[workspace.dependencies].*.version` / sibling
/// `[dependencies].*.version` pin for bumped crates. Then regenerates
/// Cargo.lock and creates a single `chore(release): bump workspace → X`
/// commit covering the edits.
fn apply_workspace_bump(
    workspace_root: &Path,
    ws: &WorkspaceInfo,
    new_version: &str,
    dry_run: bool,
    log: &StageLogger,
) -> Result<()> {
    let rows: Vec<PlanRow> = ws
        .members
        .iter()
        .map(|m| {
            let current = if m.inherits_workspace_version {
                ws.workspace_package_version.clone().unwrap_or_default()
            } else {
                m.own_version.clone().unwrap_or_default()
            };
            let level = if current == new_version {
                BumpLevel::Skip
            } else {
                BumpLevel::Explicit
            };
            PlanRow {
                crate_name: m.name.clone(),
                current,
                next: new_version.to_string(),
                level,
                reason: "workspace tag".into(),
                edited_files: vec![],
                manifest: m.manifest_path.clone(),
                inherits_workspace_version: m.inherits_workspace_version,
            }
        })
        .collect();

    if rows.iter().all(|r| r.level == BumpLevel::Skip) {
        log.verbose(&format!(
            "workspace already at {}, nothing to sync",
            new_version
        ));
        return Ok(());
    }

    if dry_run {
        log.status(&format!(
            "(dry-run) workspace version-sync: would bump {} crate(s) → {}",
            rows.iter().filter(|r| r.level != BumpLevel::Skip).count(),
            new_version
        ));
        return Ok(());
    }

    apply_plan(workspace_root, &rows, false, log)?;

    let lock_updated = std::process::Command::new("cargo")
        .args(["update", "--workspace"])
        .current_dir(workspace_root)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !lock_updated {
        log.warn("version-sync: `cargo update --workspace` failed; Cargo.lock may be stale");
    }

    let mut staged: Vec<PathBuf> = Vec::new();
    let root_manifest = workspace_root.join("Cargo.toml");
    staged.push(root_manifest.clone());
    for m in &ws.members {
        if m.manifest_path != root_manifest && !staged.contains(&m.manifest_path) {
            staged.push(m.manifest_path.clone());
        }
    }
    let lockfile = workspace_root.join("Cargo.lock");
    if lockfile.is_file() {
        staged.push(lockfile);
    }

    let staged_rel: Vec<String> = staged
        .iter()
        .map(|p| {
            p.strip_prefix(workspace_root)
                .unwrap_or(p.as_path())
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    let staged_refs: Vec<&str> = staged_rel.iter().map(|s| s.as_str()).collect();

    git::stage_and_commit(
        &staged_refs,
        &format!("chore(release): bump workspace → {}", new_version),
    )?;

    log.status(&format!("workspace version-sync: bumped → {}", new_version));
    Ok(())
}

/// Resolve the config file path from CLI overrides or auto-detection.
fn resolve_config_path(opts: &TagOpts) -> Option<std::path::PathBuf> {
    opts.config_override
        .as_deref()
        .filter(|p| p.exists())
        .map(|p| p.to_path_buf())
        .or_else(|| crate::pipeline::find_config(None).ok())
}

fn load_tag_config(opts: &TagOpts) -> TagConfig {
    if let Some(path) = resolve_config_path(opts)
        && let Ok(config) = crate::pipeline::load_config(&path)
    {
        return config.tag.unwrap_or_default();
    }
    TagConfig::default()
}

fn load_git_config(opts: &TagOpts) -> Option<GitConfig> {
    let path = resolve_config_path(opts)?;
    let config = crate::pipeline::load_config(&path).ok()?;
    config.git
}

/// Info extracted from a crate's config for path-scoped tagging.
struct CrateTagInfo {
    tag_prefix: String,
    path: String,
    version_sync: bool,
}

/// When `--crate` is specified, look up the crate in top-level crates and
/// workspace crates.  Returns the tag prefix (from `tag_template`) and the
/// crate's `path` so change detection can be scoped to that directory.
fn load_crate_tag_info(opts: &TagOpts, crate_name: &str) -> Option<CrateTagInfo> {
    let config_path = resolve_config_path(opts)?;
    let config = crate::pipeline::load_config(&config_path).ok()?;

    // Search top-level crates first, then workspace crates.
    let crate_cfg = config
        .crates
        .iter()
        .find(|c| c.name == crate_name)
        .or_else(|| {
            config
                .workspaces
                .as_deref()
                .unwrap_or_default()
                .iter()
                .flat_map(|w| &w.crates)
                .find(|c| c.name == crate_name)
        })?;

    let tag_prefix = git::extract_tag_prefix(&crate_cfg.tag_template)?;
    let version_sync = crate_cfg
        .version_sync
        .as_ref()
        .and_then(|vs| vs.enabled)
        .unwrap_or(false);
    Some(CrateTagInfo {
        tag_prefix,
        path: crate_cfg.path.clone(),
        version_sync,
    })
}

fn find_previous_tag(
    cfg: &ResolvedConfig,
    git_config: Option<&GitConfig>,
) -> Result<Option<String>> {
    let tags = match cfg.tag_context.as_str() {
        "branch" => git::get_branch_semver_tags(&cfg.tag_prefix, git_config, None)?,
        _ => git::get_all_semver_tags(&cfg.tag_prefix, git_config, None)?,
    };
    Ok(tags.into_iter().next())
}

fn branch_matches(branch: &str, patterns: &[String]) -> bool {
    for pattern in patterns {
        // Try exact match first
        if branch == pattern {
            return true;
        }
        // Try regex match (anchored to prevent partial matches)
        if let Ok(re) = Regex::new(&format!("^{}$", pattern))
            && re.is_match(branch)
        {
            return true;
        }
    }
    false
}

fn get_messages_for_bump(
    cfg: &ResolvedConfig,
    prev_tag: Option<&str>,
    path: Option<&str>,
) -> Result<Vec<String>> {
    match cfg.branch_history.as_str() {
        "last" => match path {
            Some(p) => git::get_last_commit_messages_path(1, p),
            None => git::get_last_commit_messages(1),
        },
        "full" | "compare" => match (prev_tag, path) {
            (Some(tag), Some(p)) => git::get_commit_messages_between_path(tag, "HEAD", p),
            (Some(tag), None) => git::get_commit_messages_between(tag, "HEAD"),
            (None, Some(p)) => git::get_last_commit_messages_path(500, p),
            (None, None) => git::get_last_commit_messages(500),
        },
        other => {
            bail!("unknown branch_history mode: {}", other);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BumpKind {
    Major,
    Minor,
    Patch,
    None,
}

fn detect_bump(messages: &[String], cfg: &ResolvedConfig) -> BumpKind {
    detect_bump_from_tokens(
        messages,
        &cfg.major_string_token,
        &cfg.minor_string_token,
        &cfg.patch_string_token,
        &cfg.none_string_token,
        &cfg.default_bump,
    )
}

/// Core bump detection logic, separated for unit testing without needing the full config.
///
/// Detection layers (applied in order):
/// 1. Explicit tokens (`#major`, `#minor`, `#patch`, `#none`) — highest signal.
///    `#none` always wins; `#major` > `#minor` > `#patch` among the rest.
/// 2. Conventional-commit markers if no explicit token matched — `feat:` →
///    minor, `fix:` / `perf:` / `revert:` → patch, any line containing
///    `BREAKING CHANGE` or a `<type>!:` shorthand → major. A message that
///    starts with `chore:` / `docs:` / `style:` / `refactor:` / `test:` /
///    `build:` / `ci:` is NOT release-worthy, so it contributes nothing.
/// 3. `default_bump` fallback when neither a token nor a conventional
///    marker matched. Configure `default_bump: none` to require every
///    release-worthy commit to carry either a `#...` token or a
///    conventional marker (prevents autotag from producing a patch bump
///    over chore-only ranges).
pub(crate) fn detect_bump_from_tokens(
    messages: &[String],
    major_token: &str,
    minor_token: &str,
    patch_token: &str,
    none_token: &str,
    default_bump: &str,
) -> BumpKind {
    // Whole-token (not substring) match: a commit body containing
    // `"chore: revert #none commit"` in prose, or a word like `#handsome`,
    // must NOT trigger the `#none` signal. Tokens are recognized only when
    // they appear as a standalone whitespace-separated word.
    let message_has_token = |msg: &str, token: &str| -> bool {
        msg.split(|c: char| c.is_whitespace()).any(|w| w == token)
    };

    let mut has_major = false;
    let mut has_minor = false;
    let mut has_patch = false;
    let mut has_none = false;

    for msg in messages {
        if message_has_token(msg, none_token) {
            has_none = true;
        }
        if message_has_token(msg, major_token) {
            has_major = true;
        }
        if message_has_token(msg, minor_token) {
            has_minor = true;
        }
        if message_has_token(msg, patch_token) {
            has_patch = true;
        }
    }

    // Priority: major > minor > patch among explicit tokens.
    if has_major {
        return BumpKind::Major;
    }
    if has_minor {
        return BumpKind::Minor;
    }
    if has_patch {
        return BumpKind::Patch;
    }

    // Conventional-commit layer: fires when no explicit #token matched. A
    // release-worthy conventional marker wins over `#none` because `#none`
    // represents "no default bump intended" — it's a veto over the implicit
    // fallback, not a veto over explicit release signals.
    if let Some(bump) = detect_conventional_bump(messages) {
        return bump;
    }

    // No explicit token, no conventional marker. `#none` now takes effect:
    // ranges where the only "signal" is `#none` explicitly skip, regardless
    // of default_bump.
    if has_none {
        return BumpKind::None;
    }

    // Fall back to default_bump
    match default_bump {
        "major" => BumpKind::Major,
        "minor" => BumpKind::Minor,
        "patch" => BumpKind::Patch,
        "none" | "false" => BumpKind::None,
        _ => BumpKind::Minor,
    }
}

/// Scan messages for Conventional-Commits release-worthy markers.
///
/// Returns `Some(kind)` when at least one message matches a bump-worthy
/// pattern; `None` when the range contains only non-release-worthy commit
/// types (chore, docs, style, refactor, test, build, ci) or unstructured
/// messages. The caller decides how to treat `None` — typically fall back
/// to the configured `default_bump`.
///
/// Patterns:
/// - `BREAKING CHANGE` in any line, or a `<type>!:` shorthand → major
/// - Message starts with `feat:` / `feat(scope):` → minor
/// - Message starts with `fix:` / `perf:` / `revert:` (and scoped variants)
///   → patch
fn detect_conventional_bump(messages: &[String]) -> Option<BumpKind> {
    let mut has_breaking = false;
    let mut has_feat = false;
    let mut has_fix_or_perf = false;

    for msg in messages {
        // BREAKING CHANGE footer or body line → major
        if msg.contains("BREAKING CHANGE") || msg.contains("BREAKING-CHANGE") {
            has_breaking = true;
        }
        // Inspect only the subject line for the type prefix.
        let subject = msg.lines().next().unwrap_or("").trim_start();
        let (ty, rest) = match subject.split_once(':') {
            Some(pair) => pair,
            None => continue,
        };
        // Strip a `(scope)` suffix and capture the `!` breaking marker.
        let (head, marker) = ty.split_once('(').map_or((ty, ""), |(h, scope_rest)| {
            // scope_rest is like `scope)!` or `scope)` — extract the post-`)` part.
            let after_scope = scope_rest.split_once(')').map_or("", |x| x.1);
            (h, after_scope)
        });
        let is_breaking_shorthand = marker.starts_with('!') || ty.ends_with('!');
        // Ignore pattern where `rest` is empty (e.g. `feat:` with nothing after) —
        // still counts as a typed commit. We only require the prefix match.
        let _ = rest;

        if is_breaking_shorthand {
            has_breaking = true;
        }
        match head.trim() {
            "feat" => has_feat = true,
            "fix" | "perf" | "revert" => has_fix_or_perf = true,
            _ => {}
        }
    }

    if has_breaking {
        Some(BumpKind::Major)
    } else if has_feat {
        Some(BumpKind::Minor)
    } else if has_fix_or_perf {
        Some(BumpKind::Patch)
    } else {
        None
    }
}

/// Apply a bump to semver components. Returns (major, minor, patch).
pub(crate) fn apply_bump(major: u64, minor: u64, patch: u64, bump: &BumpKind) -> (u64, u64, u64) {
    match bump {
        BumpKind::Major => (major + 1, 0, 0),
        BumpKind::Minor => (major, minor + 1, 0),
        BumpKind::Patch => (major, minor, patch + 1),
        BumpKind::None => (major, minor, patch),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Bump detection tests ----

    #[test]
    fn test_detect_bump_major_takes_precedence() {
        let messages = vec![
            "fix: something #patch".to_string(),
            "feat: big change #major".to_string(),
            "feat: small change #minor".to_string(),
        ];
        let result =
            detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "minor");
        assert_eq!(result, BumpKind::Major);
    }

    #[test]
    fn test_detect_bump_minor_over_patch() {
        let messages = vec![
            "fix: something #patch".to_string(),
            "feat: new feature #minor".to_string(),
        ];
        let result =
            detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "patch");
        assert_eq!(result, BumpKind::Minor);
    }

    #[test]
    fn test_detect_bump_patch_only() {
        let messages = vec!["fix: a bug #patch".to_string()];
        let result =
            detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "minor");
        assert_eq!(result, BumpKind::Patch);
    }

    #[test]
    fn test_detect_bump_none_token_loses_to_explicit_major() {
        // `#none` is a veto over the default_bump fallback, NOT over explicit
        // release signals. If any commit in the range explicitly asks for a
        // bump, that wins regardless of a sibling `#none`.
        let messages = vec![
            "chore: update deps #none".to_string(),
            "feat: something #major".to_string(),
        ];
        let result =
            detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "minor");
        assert_eq!(result, BumpKind::Major);
    }

    #[test]
    fn test_detect_bump_none_suppresses_default_fallback() {
        // No explicit token, no conventional marker, but `#none` present →
        // range is intentionally non-release-worthy. Skip regardless of
        // default_bump.
        let messages = vec!["chore: prep #none".to_string()];
        let result =
            detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "minor");
        assert_eq!(result, BumpKind::None);
    }

    #[test]
    fn test_detect_bump_none_loses_to_conventional_fix() {
        // A legit `fix:` in the range is a release signal. A `#none` on a
        // sibling cleanup commit must not mask it.
        let messages = vec![
            "fix: deref bug".to_string(),
            "chore: revert local-only churn #none".to_string(),
        ];
        let result =
            detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "none");
        assert_eq!(result, BumpKind::Patch);
    }

    #[test]
    fn test_detect_bump_default_when_no_tokens() {
        // Messages carry no explicit token and no release-worthy conventional
        // marker (docs: doesn't bump), so the default_bump fallback takes effect.
        let messages = vec![
            "unstructured message".to_string(),
            "docs: update readme".to_string(),
        ];
        let result =
            detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "minor");
        assert_eq!(result, BumpKind::Minor);
    }

    #[test]
    fn test_detect_bump_default_patch() {
        let messages = vec!["chore: deps bump".to_string()];
        let result =
            detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "patch");
        assert_eq!(result, BumpKind::Patch);
    }

    #[test]
    fn test_detect_bump_default_major() {
        let messages = vec!["chore: deps bump".to_string()];
        let result =
            detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "major");
        assert_eq!(result, BumpKind::Major);
    }

    #[test]
    fn test_detect_bump_default_none() {
        let messages = vec!["chore: deps bump".to_string()];
        let result =
            detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "none");
        assert_eq!(result, BumpKind::None);
    }

    // ------------------------------------------------------------------
    // Conventional-commit layer tests
    // ------------------------------------------------------------------

    #[test]
    fn test_conventional_fix_triggers_patch() {
        let messages = vec!["fix: null deref in parser".to_string()];
        let result =
            detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "none");
        assert_eq!(result, BumpKind::Patch);
    }

    #[test]
    fn test_conventional_feat_triggers_minor() {
        let messages = vec!["feat(api): add pagination".to_string()];
        let result =
            detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "none");
        assert_eq!(result, BumpKind::Minor);
    }

    #[test]
    fn test_conventional_perf_triggers_patch() {
        let messages = vec!["perf: skip redundant clone".to_string()];
        let result =
            detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "none");
        assert_eq!(result, BumpKind::Patch);
    }

    #[test]
    fn test_conventional_breaking_change_footer_triggers_major() {
        let messages = vec![
            "feat: rename flags\n\nBREAKING CHANGE: --dry replaced with --dry-run".to_string(),
        ];
        let result =
            detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "none");
        assert_eq!(result, BumpKind::Major);
    }

    #[test]
    fn test_conventional_breaking_shorthand_triggers_major() {
        let messages = vec!["feat!: rewrite config layer".to_string()];
        let result =
            detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "none");
        assert_eq!(result, BumpKind::Major);
    }

    #[test]
    fn test_conventional_scoped_breaking_shorthand_triggers_major() {
        let messages = vec!["fix(config)!: rename layer field".to_string()];
        let result =
            detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "none");
        assert_eq!(result, BumpKind::Major);
    }

    #[test]
    fn test_conventional_chore_only_range_noops_with_none_default() {
        // This is the cfgd dogfood scenario: a stable lib crate gets a test/chore
        // touch but no release-worthy commit. default_bump=none means autotag
        // should NOT mint a new tag — matches the intent.
        let messages = vec![
            "chore: bump dep".to_string(),
            "test: new harness".to_string(),
            "refactor: cleaner helper".to_string(),
        ];
        let result =
            detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "none");
        assert_eq!(result, BumpKind::None);
    }

    #[test]
    fn test_conventional_ignored_when_explicit_token_present() {
        // `#major` wins over `feat:` — explicit intent overrides the
        // conventional-commit layer.
        let messages = vec!["feat: add thing\n\n#major".to_string()];
        let result =
            detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "none");
        assert_eq!(result, BumpKind::Major);
    }

    #[test]
    fn test_detect_bump_empty_messages_uses_default() {
        let result = detect_bump_from_tokens(&[], "#major", "#minor", "#patch", "#none", "patch");
        assert_eq!(result, BumpKind::Patch);
    }

    #[test]
    fn test_detect_bump_custom_tokens() {
        let messages = vec!["BREAKING CHANGE: rewrite".to_string()];
        let result = detect_bump_from_tokens(
            &messages,
            "BREAKING CHANGE",
            "feat:",
            "fix:",
            "skip:",
            "patch",
        );
        assert_eq!(result, BumpKind::Major);
    }

    // ---- Apply bump tests ----

    #[test]
    fn test_apply_bump_major() {
        assert_eq!(apply_bump(1, 2, 3, &BumpKind::Major), (2, 0, 0));
    }

    #[test]
    fn test_apply_bump_minor() {
        assert_eq!(apply_bump(1, 2, 3, &BumpKind::Minor), (1, 3, 0));
    }

    #[test]
    fn test_apply_bump_patch() {
        assert_eq!(apply_bump(1, 2, 3, &BumpKind::Patch), (1, 2, 4));
    }

    #[test]
    fn test_apply_bump_none() {
        assert_eq!(apply_bump(1, 2, 3, &BumpKind::None), (1, 2, 3));
    }

    #[test]
    fn test_apply_bump_from_zero() {
        assert_eq!(apply_bump(0, 0, 0, &BumpKind::Patch), (0, 0, 1));
        assert_eq!(apply_bump(0, 0, 0, &BumpKind::Minor), (0, 1, 0));
        assert_eq!(apply_bump(0, 0, 0, &BumpKind::Major), (1, 0, 0));
    }

    // ---- branch_matches tests ----

    #[test]
    fn test_branch_matches_exact() {
        assert!(branch_matches("main", &["main".to_string()]));
        assert!(branch_matches("master", &["master".to_string()]));
    }

    #[test]
    fn test_branch_matches_regex() {
        assert!(branch_matches("release/1.0", &["release/.*".to_string()]));
    }

    #[test]
    fn test_branch_no_match() {
        assert!(!branch_matches(
            "feature/foo",
            &["main".to_string(), "master".to_string()]
        ));
    }

    #[test]
    fn test_branch_matches_empty_patterns() {
        assert!(!branch_matches("main", &[]));
    }

    // ---- Prerelease suffix tests ----

    #[test]
    fn test_prerelease_suffix_application() {
        // Simulate the prerelease logic
        let version = "1.2.0";
        let suffix = "beta";
        let result = format!("{}-{}", version, suffix);
        assert_eq!(result, "1.2.0-beta");
    }

    #[test]
    fn test_prerelease_suffix_custom() {
        let version = "2.0.0";
        let suffix = "rc.1";
        let result = format!("{}-{}", version, suffix);
        assert_eq!(result, "2.0.0-rc.1");
    }

    // ---- Custom tag override tests ----

    #[test]
    fn test_custom_tag_with_prefix() {
        // If custom tag already has prefix, don't duplicate
        let custom = "v5.0.0";
        let prefix = "v";
        let tag = if custom.starts_with(prefix) {
            custom.to_string()
        } else {
            format!("{}{}", prefix, custom)
        };
        assert_eq!(tag, "v5.0.0");
    }

    #[test]
    fn test_custom_tag_without_prefix() {
        let custom = "5.0.0";
        let prefix = "v";
        let tag = if custom.starts_with(prefix) {
            custom.to_string()
        } else {
            format!("{}{}", prefix, custom)
        };
        assert_eq!(tag, "v5.0.0");
    }

    // ---- Config resolution tests ----

    #[test]
    fn test_resolved_config_defaults() {
        let cfg = TagConfig::default();
        let opts = TagOpts {
            dry_run: false,
            custom_tag: None,
            default_bump: None,
            crate_name: None,
            config_override: None,
            verbose: false,
            debug: false,
            quiet: false,
            strict: false,
        };
        let resolved = ResolvedConfig::from_tag_config(&cfg, &opts);
        assert_eq!(resolved.default_bump, "minor");
        assert_eq!(resolved.tag_prefix, "v");
        assert_eq!(resolved.tag_context, "repo");
        assert_eq!(resolved.branch_history, "compare");
        assert_eq!(resolved.initial_version, "0.0.0");
        assert!(!resolved.prerelease);
        assert_eq!(resolved.prerelease_suffix, "beta");
        assert!(!resolved.force_without_changes);
        assert!(!resolved.force_without_changes_pre);
        assert_eq!(resolved.major_string_token, "#major");
        assert_eq!(resolved.minor_string_token, "#minor");
        assert_eq!(resolved.patch_string_token, "#patch");
        assert_eq!(resolved.none_string_token, "#none");
    }

    #[test]
    fn test_resolved_config_cli_overrides() {
        let cfg = TagConfig {
            default_bump: Some("minor".to_string()),
            ..Default::default()
        };
        let opts = TagOpts {
            dry_run: false,
            custom_tag: Some("v9.9.9".to_string()),
            default_bump: Some("major".to_string()),
            crate_name: None,
            config_override: None,
            verbose: false,
            debug: false,
            quiet: false,
            strict: false,
        };
        let resolved = ResolvedConfig::from_tag_config(&cfg, &opts);
        assert_eq!(resolved.default_bump, "major");
        assert_eq!(resolved.custom_tag, Some("v9.9.9".to_string()));
    }

    #[test]
    fn test_resolved_config_full_config() {
        let cfg = TagConfig {
            default_bump: Some("patch".to_string()),
            tag_prefix: Some("release-v".to_string()),
            release_branches: Some(vec!["main".to_string(), "release/.*".to_string()]),
            custom_tag: None,
            tag_context: Some("branch".to_string()),
            branch_history: Some("last".to_string()),
            initial_version: Some("1.0.0".to_string()),
            prerelease: Some(true),
            prerelease_suffix: Some("alpha".to_string()),
            force_without_changes: Some(true),
            force_without_changes_pre: Some(true),
            major_string_token: Some("BREAKING".to_string()),
            minor_string_token: Some("feat:".to_string()),
            patch_string_token: Some("fix:".to_string()),
            none_string_token: Some("skip".to_string()),
            git_api_tagging: Some(false),
            verbose: Some(false),
            tag_pre_hooks: None,
            tag_post_hooks: None,
        };
        let opts = TagOpts {
            dry_run: false,
            custom_tag: None,
            default_bump: None,
            crate_name: None,
            config_override: None,
            verbose: false,
            debug: false,
            quiet: false,
            strict: false,
        };
        let resolved = ResolvedConfig::from_tag_config(&cfg, &opts);
        assert_eq!(resolved.default_bump, "patch");
        assert_eq!(resolved.tag_prefix, "release-v");
        assert_eq!(resolved.release_branches.len(), 2);
        assert_eq!(resolved.tag_context, "branch");
        assert_eq!(resolved.branch_history, "last");
        assert_eq!(resolved.initial_version, "1.0.0");
        assert!(resolved.prerelease);
        assert_eq!(resolved.prerelease_suffix, "alpha");
        assert!(resolved.force_without_changes);
        assert!(resolved.force_without_changes_pre);
        assert_eq!(resolved.major_string_token, "BREAKING");
        assert_eq!(resolved.minor_string_token, "feat:");
        assert_eq!(resolved.patch_string_token, "fix:");
        assert_eq!(resolved.none_string_token, "skip");
    }

    // ---- Config parsing from YAML tests ----

    #[test]
    fn test_tag_config_from_yaml_full() {
        let yaml = r##"
default_bump: patch
tag_prefix: "v"
release_branches:
  - main
  - "release/.*"
tag_context: branch
branch_history: last
initial_version: "1.0.0"
prerelease: true
prerelease_suffix: rc
force_without_changes: true
force_without_changes_pre: false
major_string_token: "#major"
minor_string_token: "#minor"
patch_string_token: "#patch"
none_string_token: "#none"
git_api_tagging: true
verbose: false
"##;
        let cfg: TagConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.default_bump, Some("patch".to_string()));
        assert_eq!(cfg.tag_prefix, Some("v".to_string()));
        assert_eq!(
            cfg.release_branches,
            Some(vec!["main".to_string(), "release/.*".to_string()])
        );
        assert_eq!(cfg.tag_context, Some("branch".to_string()));
        assert_eq!(cfg.branch_history, Some("last".to_string()));
        assert_eq!(cfg.initial_version, Some("1.0.0".to_string()));
        assert_eq!(cfg.prerelease, Some(true));
        assert_eq!(cfg.prerelease_suffix, Some("rc".to_string()));
        assert_eq!(cfg.force_without_changes, Some(true));
        assert_eq!(cfg.force_without_changes_pre, Some(false));
        assert_eq!(cfg.git_api_tagging, Some(true));
        assert_eq!(cfg.verbose, Some(false));
    }

    #[test]
    fn test_tag_config_from_yaml_minimal() {
        let yaml = "{}";
        let cfg: TagConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.default_bump, None);
        assert_eq!(cfg.tag_prefix, None);
        assert_eq!(cfg.release_branches, None);
    }

    #[test]
    fn test_tag_config_from_yaml_defaults() {
        let yaml = "default_bump: major";
        let cfg: TagConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.default_bump, Some("major".to_string()));
        assert_eq!(cfg.tag_prefix, None); // not set, will use default when resolved
    }

    #[test]
    fn test_top_level_config_with_tag_section() {
        let yaml = r#"
project_name: myproject
crates:
  - name: myproject
    path: "."
    tag_template: "v{{ .Version }}"
tag:
  default_bump: patch
  tag_prefix: "v"
  branch_history: last
"#;
        let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let tag = config.tag.unwrap();
        assert_eq!(tag.default_bump, Some("patch".to_string()));
        assert_eq!(tag.branch_history, Some("last".to_string()));
    }

    #[test]
    fn test_tag_pre_post_hooks_yaml_roundtrip() {
        // Both simple-string and structured hook forms must parse; the
        // structured form carries `cmd` / `dir` / `env` so an update-lockfile
        // hook can run inside a workspace subdirectory with its own env.
        let yaml = r#"
tag_pre_hooks:
  - "cargo update --workspace"
  - cmd: "scripts/pre-tag.sh {{ .Tag }}"
    dir: "."
tag_post_hooks:
  - "git push --follow-tags"
"#;
        let cfg: TagConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let pre = cfg.tag_pre_hooks.as_ref().unwrap();
        assert_eq!(pre.len(), 2);
        assert!(matches!(
            pre[0],
            anodizer_core::config::HookEntry::Simple(ref s) if s == "cargo update --workspace"
        ));
        let post = cfg.tag_post_hooks.as_ref().unwrap();
        assert_eq!(post.len(), 1);
        assert!(matches!(
            post[0],
            anodizer_core::config::HookEntry::Simple(ref s) if s == "git push --follow-tags"
        ));
    }

    #[test]
    fn test_tag_hooks_default_none() {
        // Absent in YAML means Option::None — the `create_tag` closure treats
        // this as "no hooks" and skips invocation.
        let cfg: TagConfig = serde_yaml_ng::from_str("default_bump: minor").unwrap();
        assert!(cfg.tag_pre_hooks.is_none());
        assert!(cfg.tag_post_hooks.is_none());
    }

    // ---- Integration-style bump logic tests ----

    #[test]
    fn test_full_bump_flow_major() {
        let messages = vec!["feat: breaking change #major".to_string()];
        let bump =
            detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "patch");
        assert_eq!(bump, BumpKind::Major);
        let (maj, min, pat) = apply_bump(1, 5, 3, &bump);
        assert_eq!((maj, min, pat), (2, 0, 0));
        let new_tag = format!("v{}.{}.{}", maj, min, pat);
        assert_eq!(new_tag, "v2.0.0");
    }

    #[test]
    fn test_full_bump_flow_minor_default() {
        let messages = vec!["docs: update readme".to_string()];
        let bump =
            detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "minor");
        assert_eq!(bump, BumpKind::Minor);
        let (maj, min, pat) = apply_bump(1, 2, 3, &bump);
        assert_eq!((maj, min, pat), (1, 3, 0));
    }

    #[test]
    fn test_full_bump_flow_prerelease() {
        let messages = vec!["feat: new thing #minor".to_string()];
        let bump =
            detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "patch");
        assert_eq!(bump, BumpKind::Minor);
        let (maj, min, pat) = apply_bump(1, 2, 3, &bump);
        let version = format!("{}.{}.{}-beta", maj, min, pat);
        assert_eq!(version, "1.3.0-beta");
    }
}
