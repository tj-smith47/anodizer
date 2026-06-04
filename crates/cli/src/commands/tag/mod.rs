pub mod rollback;

use anodizer_core::config::{CrateConfig, GitConfig, TagConfig};
use anodizer_core::git;
use anodizer_core::hooks::{HookRunContext, run_hooks};
use anodizer_core::log::{StageLogger, Verbosity};
use anodizer_core::template::TemplateVars;
use anyhow::{Result, bail};
use regex::Regex;
use std::path::{Path, PathBuf};

use crate::commands::bump::cargo_edit::{WorkspaceInfo, apply_plan, load_workspace};
use crate::commands::bump::plan::{BumpLevel, PlanRow};
use crate::commands::changelog_sync::{
    ChangelogRouting, ChangelogTarget, render_and_stage_changelogs, resolve_changelog_enabled,
};
use crate::commands::version_files_resolve::resolve_version_files;

pub struct TagOpts {
    pub dry_run: bool,
    pub custom_tag: Option<String>,
    pub default_bump: Option<String>,
    /// When set, select a specific crate's tag_template for tagging.
    pub crate_name: Option<String>,
    /// Push the version-sync bump commit to the release branch atomically with the tag.
    pub push: bool,
    /// Do not push the version-sync bump commit; push the tag only (leaves the bump commit local).
    pub no_push: bool,
    /// Remote to push to; defaults to `origin` when unset.
    pub push_remote: Option<String>,
    /// Preview the `git push` commands `--push` would run, without executing.
    pub push_dry_run: bool,
    /// Refresh `CHANGELOG.md` as part of this tag. Opt-in: the refresh runs only
    /// when set AND a `changelog:` config block is present and not skipped.
    pub changelog: bool,
    pub config_override: Option<std::path::PathBuf>,
    pub verbose: bool,
    pub debug: bool,
    pub quiet: bool,
    pub strict: bool,
}

/// Resolve whether the version-sync bump commit (the branch HEAD) should be
/// pushed alongside the tag.
///
/// `--no-push` always wins; then an explicit `--push` or `tag.push = true`
/// selects a branch push; otherwise the per-path default applies (`false` for
/// the single / lockstep / `--crate` paths, `true` for per-crate
/// auto-dispatch, whose atomic branch+tags push is the long-standing default).
fn resolve_effective_push(opts: &TagOpts, config_push: Option<bool>, path_default: bool) -> bool {
    if opts.no_push {
        false
    } else if opts.push || config_push == Some(true) {
        true
    } else {
        path_default
    }
}

/// Resolve the branch to push the bump commit to, or `None` when the bump
/// commit should stay local (tag-only push).
///
/// Returns `Some(current_branch)` when [`resolve_effective_push`] selects a
/// branch push for the given `path_default`; otherwise `None`.
fn resolve_tag_push_branch(
    opts: &TagOpts,
    config_push: Option<bool>,
    path_default: bool,
) -> Result<Option<String>> {
    if resolve_effective_push(opts, config_push, path_default) {
        Ok(Some(git::get_current_branch()?))
    } else {
        Ok(None)
    }
}

/// Resolved tag configuration with defaults applied.
#[derive(Clone)]
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
    skip_ci_on_bump: bool,
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
            skip_ci_on_bump: cfg.skip_ci_on_bump.unwrap_or(false),
        }
    }
}

/// `[skip ci]` suffix appended to a bump-commit subject, or empty when
/// `skip_ci_on_bump` is off (the default). Returned with a leading space so
/// callers can append it directly after the subject body.
fn skip_ci_suffix(skip_ci_on_bump: bool) -> &'static str {
    if skip_ci_on_bump { " [skip ci]" } else { "" }
}

pub fn run(opts: TagOpts) -> Result<()> {
    // Load the full config + Cargo workspace once so all downstream helpers
    // share the same parse (eliminates the previous triple workspace-file
    // read on lockstep repos).
    let loaded_config: Option<anodizer_core::config::Config> =
        resolve_config_path(&opts).and_then(|p| crate::pipeline::load_config(&p).ok());
    let workspace_root_path = std::env::current_dir().ok();
    let loaded_workspace: Option<WorkspaceInfo> = workspace_root_path
        .as_ref()
        .and_then(|root| load_workspace(root).ok());

    let tag_config = loaded_config
        .as_ref()
        .and_then(|c| c.tag.clone())
        .unwrap_or_default();
    let git_config: Option<anodizer_core::config::GitConfig> =
        loaded_config.as_ref().and_then(|c| c.git.clone());

    // Refresh CHANGELOG.md into the version-bump commit (riding the same
    // `git add` as the Cargo.toml / version_files edits) when `changelog:` is
    // configured and not skipped — `tag` is what release CI runs, so without
    // this the changelogs rot between releases even though `bump` refreshes them.
    let changelog_enabled = resolve_changelog_enabled(loaded_config.as_ref(), opts.changelog);

    let mut cfg = ResolvedConfig::from_tag_config(&tag_config, &opts);

    // Push controls shared by every tagging path. `remote` defaults to origin;
    // `effective_push` per-path resolution is computed at each call site so the
    // per-crate path can carry its own (true) default.
    let remote = opts.push_remote.as_deref().unwrap_or("origin").to_string();
    let config_push = tag_config.push;

    // When --crate is given, look up the crate in config and derive the tag
    // prefix from its tag_template.  Also capture the crate path so we can
    // scope change detection to only that directory.
    let mut crate_path: Option<String> = None;
    let mut version_sync_enabled = false;
    let mut crate_version_files: Vec<String> = Vec::new();
    if let Some(ref crate_name) = opts.crate_name
        && let Some(info) = load_crate_tag_info(&opts, crate_name)
    {
        cfg.tag_prefix = info.tag_prefix;
        crate_path = Some(info.path);
        version_sync_enabled = info.version_sync;
        crate_version_files = info.version_files;
    }

    // Per-crate / hybrid-workspace dispatch: when no --crate is given and the
    // repository has per-crate versions (anodizer workspaces: or multiple crates:
    // entries without a lockstep [workspace.package].version), delegate to the
    // multi-crate handler which runs change detection, bumps all selected crates
    // in one commit, creates per-crate tags, and pushes atomically.
    //
    // custom_tag is incompatible with per-crate mode: the whole point of a
    // custom tag is to override version computation for one unit. In per-crate
    // mode there is no single unit — use --crate to target a specific crate.
    if let Some(ref ct) = cfg.custom_tag
        && opts.crate_name.is_none()
    {
        // Peek at the repo shape without consuming it, so we can give a useful
        // error rather than silently discarding the custom_tag value.
        if matches!(
            detect_repo_shape(loaded_config.as_ref(), loaded_workspace.as_ref()),
            RepoShape::PerCrate(_)
        ) {
            anyhow::bail!(
                "--custom-tag {:?} is incompatible with per-crate workspace mode; \
                 pass --crate <name> to override a single crate's tag",
                ct
            );
        }
    }
    if opts.crate_name.is_none() {
        match detect_repo_shape(loaded_config.as_ref(), loaded_workspace.as_ref()) {
            RepoShape::PerCrate(groups) => {
                // Build log early so status messages are consistent.
                let config_verbose = tag_config.verbose.unwrap_or(false);
                let effective_verbose = opts.verbose || (config_verbose && !opts.quiet);
                let log = StageLogger::new(
                    "tag",
                    Verbosity::from_flags(opts.quiet, effective_verbose, opts.debug),
                );
                log.status(&format!(
                    "running auto-tag (per-crate){}",
                    if opts.dry_run { " (dry-run)" } else { "" }
                ));
                return run_per_crate_tag(
                    groups,
                    &opts,
                    &cfg,
                    git_config.as_ref(),
                    loaded_config.as_ref(),
                    PushControls {
                        remote: &remote,
                        config_push,
                        changelog_enabled,
                    },
                    &log,
                );
            }
            // Single or Lockstep fall through to existing paths below.
            RepoShape::Single | RepoShape::Lockstep => {}
        }
    }

    // Workspace-mode: with no --crate, treat a Cargo workspace whose members
    // inherit [workspace.package].version as a single versioned unit. The
    // tag-derived version gets applied to root Cargo.toml + every member
    // manifest + workspace.dependencies pins before the tag is created, so
    // the tagged commit has Cargo.toml at the version the tag advertises.
    let workspace_info: Option<&WorkspaceInfo> = if opts.crate_name.is_none() {
        loaded_workspace
            .as_ref()
            .filter(|ws| ws.workspace_package_version.is_some())
    } else {
        None
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

    // Single / lockstep / --crate share a `false` push default: today's
    // behavior pushes only the tag and leaves the bump commit local. `--push`,
    // `tag.push=true`, or `--push-dry-run` (preview) opt into also pushing the
    // bump commit (the branch HEAD) atomically with the tag.
    //
    // `--push-dry-run` previews the push commands `--push` would run: treat it
    // as push-mode-on, but every `git push` is replaced by a "(dry-run) would
    // push …" log line.
    let push_mode = resolve_effective_push(&opts, config_push, false) || opts.push_dry_run;
    let push_preview = opts.push_dry_run;
    let push_branch = if push_mode {
        Some(git::get_current_branch()?)
    } else {
        None
    };

    // cwd is invariant for the command — bind once and reuse across every tag.
    let cwd = std::env::current_dir()?;

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
            run_hooks(
                &pre_hooks,
                "tag-pre",
                HookRunContext::new(dry_run, &log, Some(&tv)),
            )?;
        }

        // Whether the actual push step runs in dry-run/preview mode (creates
        // the tag locally but only prints the push commands).
        let push_dry = dry_run || push_preview;

        if cfg.git_api_tagging {
            log.verbose("using GitHub API for tagging (git_api_tagging=true)");
            if push_mode {
                // Push the branch first so the bump commit lands on the remote,
                // THEN create the tag via the API (which references the
                // now-pushed HEAD commit).
                git::push_branch_and_tags_atomic_in(
                    &cwd,
                    &git::AtomicPushSpec {
                        remote: &remote,
                        branch: push_branch.as_deref(),
                        tags: &[],
                        dry_run: push_dry,
                        strict,
                    },
                    &log,
                )?;
            }
            git::create_tag_via_github_api(tag, message, dry_run, &log, strict)?;
        } else if push_mode {
            // Create the tag locally, then push branch + tag atomically so
            // neither an orphan tag NOR an orphan bump commit is possible.
            git::create_tag_local_only(&cwd, tag, message, dry_run, &log)?;
            git::push_branch_and_tags_atomic_in(
                &cwd,
                &git::AtomicPushSpec {
                    remote: &remote,
                    branch: push_branch.as_deref(),
                    tags: std::slice::from_ref(&tag.to_string()),
                    dry_run: push_dry,
                    strict,
                },
                &log,
            )?;
        } else {
            git::create_and_push_tag(tag, message, dry_run, &log, strict)?;
        }

        if !post_hooks.is_empty() {
            run_hooks(
                &post_hooks,
                "tag-post",
                HookRunContext::new(dry_run, &log, Some(&tv)),
            )?;
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

    // The current manifest version for this tagging unit: the workspace
    // `[workspace.package].version` in lockstep mode, else the version-synced
    // crate's own `Cargo.toml`. Read+parsed once here and reused by both the
    // `cargo_ahead` release-signal check and the downgrade guard below so the
    // two never drift on which manifest they consult.
    let cargo_current_ver: Option<String> = if let Some(ws) = workspace_info {
        ws.workspace_package_version.clone()
    } else if version_sync_enabled && let Some(ref path) = crate_path {
        anodizer_stage_build::version_sync::read_cargo_version(path).ok()
    } else {
        None
    };

    // A manually-bumped Cargo.toml that is strictly ahead of the previous
    // tag is itself a release signal — the operator has explicitly set the
    // next version. Honor it even when no per-commit bump signal fired and
    // even when the crate path had no changes. This prevents autotag from
    // stalling at the old tag after a manual `cargo set-version` bump.
    let cargo_ahead = match (
        cargo_current_ver
            .as_deref()
            .and_then(|v| git::parse_semver(v).ok()),
        prev_tag
            .as_deref()
            .and_then(|t| git::parse_semver_tag(t).ok()),
    ) {
        (Some(c), Some(p)) => (c.major, c.minor, c.patch) > (p.major, p.minor, p.patch),
        _ => false,
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
    // `[skip ci]` is opt-in via `tag.skip_ci_on_bump` (default off). It is NOT
    // a free CI-cost saving: the bump commit becomes the tag target, and a
    // `[skip ci]` tag target suppresses BOTH the master-push CI re-run AND any
    // `on: push: tags:` release trigger. It is only safe with a
    // `workflow_run`-triggered release; the tag-push pattern
    // must leave it off or the release silently never fires.
    let mut bump_commit_created = false;
    if let Some(ws) = workspace_info {
        let root = workspace_root_path
            .as_deref()
            .unwrap_or_else(|| Path::new("."));
        // Lockstep shares one version across the whole workspace, so the
        // top-level `Config.version_files` list (no single crate to scope to)
        // is the enrollment, rewritten with the shared old→new.
        let ws_version_files = resolve_version_files(None, loaded_config.as_ref());
        let ws_old = bare_version_from_tag(old_tag_str);
        let ws_from_tag = (!old_tag_str.is_empty()).then_some(old_tag_str);
        let cl_config = changelog_config_for(loaded_config.as_ref());
        let cl_routing = ChangelogRouting::from_config(&cl_config);
        bump_commit_created = apply_workspace_bump(
            root,
            ws,
            &new_version,
            &WorkspaceBumpEdits {
                vf: VersionFilesBump {
                    old: ws_old.as_deref(),
                    files: &ws_version_files,
                },
                cl: ChangelogBump {
                    enabled: changelog_enabled,
                    from_tag: ws_from_tag,
                    full_tag: &new_tag,
                    routing: &cl_routing,
                },
            },
            opts.dry_run,
            cfg.skip_ci_on_bump,
            &log,
        )?;
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

        // Update dependency version specs in other crates that belong to the
        // SAME Cargo workspace as the bumped crate. Scoping to the owning
        // workspace prevents this bump from rewriting a path-dep pin in an
        // independent release group on a different cadence.
        let dep_modified = if let Some(ref name) = crate_name {
            anodizer_stage_build::version_sync::sync_workspace_deps(
                &workspace_root,
                path,
                name,
                &new_version,
                opts.dry_run,
                &log,
            )?
        } else {
            vec![]
        };

        // Rewrite enrolled version_files in the same bump commit so a Helm
        // Chart.yaml / install doc / README badge never drifts from the tag.
        // Old version comes from the previous tag; absent a previous tag there
        // is nothing to rewrite from. Runs in BOTH dry-run and real modes — the
        // helper logs per-file replacement counts (and the zero-match warning)
        // either way, and under dry-run writes/stages nothing — so the preview
        // matches the lockstep and per-crate paths.
        let vf_old = bare_version_from_tag(old_tag_str);
        let vf_changed = match vf_old {
            Some(ref old) => rewrite_and_stage_version_files(
                &crate_version_files,
                old,
                &new_version,
                opts.dry_run,
                &log,
            )?,
            None => Vec::new(),
        };

        // Refresh CHANGELOG.md alongside the version_files rewrites, on the same
        // dry-run-preview / real-write-and-stage split. The previous tag bounds
        // the rendered commit range (`old_tag_str` is empty on a first tag).
        let ws_root = Path::new(&workspace_root);
        let cl_changed = if changelog_enabled {
            let from_tag = (!old_tag_str.is_empty()).then(|| old_tag_str.to_string());
            let targets = crate_name
                .as_ref()
                .map(|name| {
                    vec![ChangelogTarget {
                        crate_name: name.clone(),
                        crate_dir: ws_root.join(path),
                        from_tag,
                        to_version: new_version.clone(),
                        full_tag: new_tag.clone(),
                    }]
                })
                .unwrap_or_default();
            let cl_config = changelog_config_for(loaded_config.as_ref());
            let routing = ChangelogRouting::from_config(&cl_config);
            render_and_stage_changelogs(ws_root, &targets, &routing, opts.dry_run, &log)?
        } else {
            Vec::new()
        };

        if !opts.dry_run {
            // Regenerate Cargo.lock to match the bumped Cargo.toml versions.
            // Without this, the tagged commit has Cargo.toml at the new version
            // but Cargo.lock at the old version, causing `cargo test` (from
            // before hooks) to update Cargo.lock and dirty the tree.
            match anodizer_core::cargo_lock::cargo_update_workspace(None) {
                Ok(true) => {}
                Ok(false) => log.warn(
                    "version-sync: `cargo update --workspace` exited non-zero; Cargo.lock may be stale",
                ),
                Err(e) => log.warn(&format!(
                    "version-sync: could not spawn `cargo update --workspace` ({e}); Cargo.lock may be stale"
                )),
            }

            let cargo_toml = format!("{}/Cargo.toml", path);
            let mut files_to_stage: Vec<&str> = vec![&cargo_toml, "Cargo.lock"];
            for f in &dep_modified {
                files_to_stage.push(f);
            }
            for f in &vf_changed {
                files_to_stage.push(f);
            }
            for f in &cl_changed {
                if !files_to_stage.contains(&f.as_str()) {
                    files_to_stage.push(f);
                }
            }
            // Propagate a commit failure (index lock, hook rejection, …)
            // before any tag is created: tagging a commit whose Cargo.toml is
            // NOT at `new_version` would ship an orphan tag pointing at the
            // wrong version. `Ok(false)` (no diff to commit) likewise means no
            // bump commit was produced, so the orphan-bump hint must not fire.
            bump_commit_created = git::stage_and_commit(
                &files_to_stage,
                &format!(
                    "chore: bump {} to {}{}",
                    path,
                    new_version,
                    skip_ci_suffix(cfg.skip_ci_on_bump)
                ),
            )?;
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

    // When a version-sync bump commit was created but the branch will NOT be
    // pushed, the freshly-pushed tag references a commit absent from the remote
    // branch — the orphan footgun this feature exists to kill. Surface a gentle
    // one-line hint on the implicit tag-only default; stay silent when the user
    // explicitly chose --no-push (they acknowledged the tradeoff) or in any
    // dry-run/preview mode (nothing was pushed).
    if bump_commit_created
        && push_branch.is_none()
        && !opts.no_push
        && !opts.dry_run
        && !opts.push_dry_run
    {
        log.status(
            "tagged a version-sync bump commit but left it local; \
             pass --push to push the bump commit + tag atomically (or push the branch yourself)",
        );
    }

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
///
/// Returns `true` when a bump commit was actually created, `false` when the
/// workspace was already at the target (or in `dry_run`).
/// The shared old→new bump and the files enrolled to be rewritten by it,
/// passed through to the workspace bump so `version_files` rewriting rides in
/// the same commit. `old` is `None` when there is no previous tag to rewrite
/// from.
struct VersionFilesBump<'a> {
    old: Option<&'a str>,
    files: &'a [String],
}

/// Lockstep changelog-refresh inputs for [`apply_workspace_bump`]. The shared
/// workspace tag bounds every member's rendered commit range, so a single
/// `from_tag` applies to all members, and the single shared `full_tag` keys the
/// aggregate root section.
struct ChangelogBump<'a> {
    enabled: bool,
    from_tag: Option<&'a str>,
    /// The shared workspace tag for this release (e.g. `v1.2.0`).
    full_tag: &'a str,
    /// The resolved routing for the lockstep changelog destinations.
    routing: &'a ChangelogRouting<'a>,
}

/// The repo-committed edits [`apply_workspace_bump`] folds into the bump commit
/// beyond the manifests: `version_files` rewrites and the `CHANGELOG.md`
/// refresh, grouped so the workspace bump takes one edits carrier.
struct WorkspaceBumpEdits<'a> {
    vf: VersionFilesBump<'a>,
    cl: ChangelogBump<'a>,
}

/// Resolve the effective `changelog:` block (owned), falling back to the
/// default (root-only) when no config or no `changelog:` block is present, so
/// the routing decision is uniform across every tagging mode.
fn changelog_config_for(
    config: Option<&anodizer_core::config::Config>,
) -> anodizer_core::config::ChangelogConfig {
    config.and_then(|c| c.changelog.clone()).unwrap_or_default()
}

fn apply_workspace_bump(
    workspace_root: &Path,
    ws: &WorkspaceInfo,
    new_version: &str,
    edits: &WorkspaceBumpEdits<'_>,
    dry_run: bool,
    skip_ci_on_bump: bool,
    log: &StageLogger,
) -> Result<bool> {
    let WorkspaceBumpEdits { vf, cl } = edits;
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
        return Ok(false);
    }

    // Lockstep splits the changelog destinations: per-crate files get one target
    // per member, but the shared root gets a SINGLE aggregate target. The members
    // all share the one workspace tag, so promoting per-member into the root would
    // strand every member after the first (the `## [tag]` heading already exists).
    // The aggregate target spans the whole workspace (`crate_dir = workspace_root`,
    // unfiltered) so the root section aggregates the entire release.
    let per_crate_targets: Vec<ChangelogTarget> = if cl.enabled && cl.routing.per_crate {
        ws.members
            .iter()
            .map(|m| ChangelogTarget {
                crate_name: m.name.clone(),
                crate_dir: m
                    .manifest_path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| workspace_root.to_path_buf()),
                from_tag: cl.from_tag.map(str::to_string),
                to_version: new_version.to_string(),
                full_tag: cl.full_tag.to_string(),
            })
            .collect()
    } else {
        Vec::new()
    };
    let per_crate_routing = ChangelogRouting {
        root_enabled: false,
        per_crate: true,
        chronology: cl.routing.chronology,
        root_crates: cl.routing.root_crates,
        single_track: false,
    };

    let root_aggregate_target: Vec<ChangelogTarget> = if cl.enabled && cl.routing.root_enabled {
        vec![ChangelogTarget {
            crate_name: ws
                .members
                .first()
                .map(|m| m.name.clone())
                .unwrap_or_default(),
            crate_dir: workspace_root.to_path_buf(),
            from_tag: cl.from_tag.map(str::to_string),
            to_version: new_version.to_string(),
            full_tag: cl.full_tag.to_string(),
        }]
    } else {
        Vec::new()
    };
    let root_routing = ChangelogRouting {
        root_enabled: true,
        per_crate: false,
        chronology: cl.routing.chronology,
        // The lockstep aggregate is one flat whole-release section (not a
        // per-crate `### subsection`), so the per-crate `root.crates` filter
        // must not gate it; filtering on the arbitrary first-member name would
        // silently drop the entire lockstep root changelog.
        root_crates: None,
        // One shared workspace tag over all members: the root holds one flat
        // whole-release block. Force the flat roll so a curated `[Unreleased]`
        // whose `### <Heading>` titles diverge from the configured `groups:`
        // is not misread as multi-track and grafted with a `### <crate>`.
        single_track: true,
    };

    if dry_run {
        log.status(&format!(
            "(dry-run) workspace version-sync: would bump {} crate(s) → {}",
            rows.iter().filter(|r| r.level != BumpLevel::Skip).count(),
            new_version
        ));
        if let Some(old) = vf.old {
            rewrite_and_stage_version_files(vf.files, old, new_version, true, log)?;
        }
        render_and_stage_changelogs(
            workspace_root,
            &per_crate_targets,
            &per_crate_routing,
            true,
            log,
        )?;
        render_and_stage_changelogs(
            workspace_root,
            &root_aggregate_target,
            &root_routing,
            true,
            log,
        )?;
        return Ok(false);
    }

    apply_plan(workspace_root, &rows, false, log)?;

    match anodizer_core::cargo_lock::cargo_update_workspace(Some(workspace_root)) {
        Ok(true) => {}
        Ok(false) => log.warn(
            "version-sync: `cargo update --workspace` exited non-zero; Cargo.lock may be stale",
        ),
        Err(e) => log.warn(&format!(
            "version-sync: could not spawn `cargo update --workspace` ({e}); Cargo.lock may be stale"
        )),
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

    let mut staged_rel: Vec<String> = staged
        .iter()
        .map(|p| {
            p.strip_prefix(workspace_root)
                .unwrap_or(p.as_path())
                .to_string_lossy()
                .into_owned()
        })
        .collect();

    // version_files are repo-root-relative already; rewrite the shared old→new
    // and fold the changed paths into the same bump commit.
    if let Some(old) = vf.old {
        let vf_changed = rewrite_and_stage_version_files(vf.files, old, new_version, false, log)?;
        for f in vf_changed {
            if !staged_rel.contains(&f) {
                staged_rel.push(f);
            }
        }
    }

    // Refresh the per-crate files (one section per member) and the single
    // aggregate root section, folding every written (repo-relative) path into
    // the same bump commit.
    let mut cl_changed = render_and_stage_changelogs(
        workspace_root,
        &per_crate_targets,
        &per_crate_routing,
        false,
        log,
    )?;
    cl_changed.extend(render_and_stage_changelogs(
        workspace_root,
        &root_aggregate_target,
        &root_routing,
        false,
        log,
    )?);
    for f in cl_changed {
        if !staged_rel.contains(&f) {
            staged_rel.push(f);
        }
    }

    let staged_refs: Vec<&str> = staged_rel.iter().map(|s| s.as_str()).collect();

    git::stage_and_commit(
        &staged_refs,
        &format!(
            "chore(release): bump workspace → {}{}",
            new_version,
            skip_ci_suffix(skip_ci_on_bump)
        ),
    )?;

    log.status(&format!("workspace version-sync: bumped → {}", new_version));
    Ok(true)
}

/// Resolve the config file path from CLI overrides or auto-detection.
fn resolve_config_path(opts: &TagOpts) -> Option<std::path::PathBuf> {
    opts.config_override
        .as_deref()
        .filter(|p| p.exists())
        .map(|p| p.to_path_buf())
        .or_else(|| crate::pipeline::find_config(None).ok())
}

/// Repository shape as detected from Cargo.toml + `.anodizer.yaml`.
pub(crate) enum RepoShape {
    /// Single crate or no config — use single-crate path unchanged.
    Single,
    /// `[workspace.package].version` is set — lockstep workspace, existing path.
    Lockstep,
    /// Each member has its own `[package].version` (no `[workspace.package].version`)
    /// AND the anodizer config has a per-crate/workspace definition.
    /// Carries the groups to iterate: each `Vec<CrateConfig>` is one lockstep
    /// group (singleton = independent release).
    PerCrate(Vec<Vec<CrateConfig>>),
}

/// Detect the repository shape for default (no `--crate`) tag behaviour.
///
/// Reads the Cargo workspace and anodizer config. Precedence:
/// 1. If lockstep workspace → `Lockstep`.
/// 2. If anodizer config has `workspaces:` with multiple groups → `PerCrate` (hybrid).
/// 3. If anodizer config has `crates:` with >1 entry → `PerCrate` (flat multi-crate).
/// 4. Otherwise → `Single`.
pub(crate) fn detect_repo_shape(
    preloaded_config: Option<&anodizer_core::config::Config>,
    preloaded_workspace: Option<&WorkspaceInfo>,
) -> RepoShape {
    let workspace_root = match std::env::current_dir().ok() {
        Some(p) => p,
        None => return RepoShape::Single,
    };

    // `.anodizer.yaml`'s `workspaces:` block is an explicit operator
    // declaration of per-crate-with-grouping intent and takes precedence
    // over `[workspace.package].version`, which is often default cruft
    // from `cargo init` that survives even when each crate sets its own
    // `version =`. A repo that genuinely wants lockstep should leave
    // `workspaces:` unset and let the Cargo-level signal speak.
    if let Some(config) = preloaded_config
        && let Some(ref ws_list) = config.workspaces
        && !ws_list.is_empty()
    {
        let groups: Vec<Vec<CrateConfig>> = ws_list.iter().map(|ws| ws.crates.clone()).collect();
        return RepoShape::PerCrate(groups);
    }

    let lockstep = if let Some(ws) = preloaded_workspace {
        ws.workspace_package_version.is_some()
    } else {
        load_workspace(&workspace_root)
            .ok()
            .is_some_and(|ws| ws.workspace_package_version.is_some())
    };
    if lockstep {
        return RepoShape::Lockstep;
    }

    let config = match preloaded_config {
        Some(c) => c,
        None => return RepoShape::Single,
    };

    if config.crates.len() > 1 {
        let groups: Vec<Vec<CrateConfig>> = config.crates.iter().map(|c| vec![c.clone()]).collect();
        return RepoShape::PerCrate(groups);
    }

    RepoShape::Single
}

/// Result of per-crate tag computation for one group.
struct GroupTagResult {
    /// Crate names in this group.
    crate_names: Vec<String>,
    /// New tags to create (one per crate in the group).
    new_tags: Vec<(String, String)>,
    /// Bump commit paths that need version updates.
    version_updates: Vec<(String, String)>,
    /// Bare previous version this group bumps FROM, or `None` when the group
    /// has no previous tag (nothing to rewrite version_files from).
    old_version: Option<String>,
    /// The group's previous tag ref (e.g. `core-v0.1.0`), or `None` on a first
    /// tag — bounds the rendered changelog commit range per crate.
    prev_tag: Option<String>,
    /// Effective `version_files` enrollment per crate, parallel to
    /// `version_updates` (same crate order within the group).
    crate_version_files: Vec<Vec<String>>,
}

/// Compute tag results for all per-crate groups, performing change detection.
///
/// Returns the list of groups that need tagging, skipping groups with no
/// changes.
fn compute_per_crate_tags(
    groups: &[Vec<CrateConfig>],
    opts: &TagOpts,
    cfg: &ResolvedConfig,
    git_config: Option<&GitConfig>,
    preloaded_config: Option<&anodizer_core::config::Config>,
    log: &StageLogger,
) -> Result<Vec<GroupTagResult>> {
    use crate::commands::release::{detect_changed_crates_pub, flatten_known_crates};

    // Use the already-loaded config when available to avoid a redundant disk
    // read; fall back to a fresh load, then to an empty default for fixture
    // repos that have no config file (e.g. integration-test temp dirs).
    let fallback: anodizer_core::config::Config;
    let anodizer_config: &anodizer_core::config::Config = if let Some(c) = preloaded_config {
        c
    } else {
        fallback = resolve_config_path(opts)
            .and_then(|p| crate::pipeline::load_config(&p).ok())
            .unwrap_or_default();
        &fallback
    };

    // Run change detection across ALL crates so depends_on propagation works.
    let all_known = flatten_known_crates(anodizer_config);
    let changed_names = detect_changed_crates_pub(
        &all_known,
        anodizer_config.git.as_ref(),
        anodizer_config.monorepo_tag_prefix(),
        log,
    )?;

    if changed_names.is_empty() {
        return Ok(vec![]);
    }

    use std::collections::HashSet;
    let changed_set: HashSet<&str> = changed_names.iter().map(|s| s.as_str()).collect();

    let mut results: Vec<GroupTagResult> = Vec::new();

    for group in groups {
        // A group is selected if any of its crates appears in changed_names.
        let group_selected = group.iter().any(|c| changed_set.contains(c.name.as_str()));
        if !group_selected {
            continue;
        }

        let first = &group[0];
        let tag_prefix =
            git::extract_tag_prefix(&first.tag_template).unwrap_or_else(|| cfg.tag_prefix.clone());

        // Determine the previous tag for this group (use first crate's template).
        // Per-group only the tag_prefix (from this group's template) and
        // custom_tag (never applies in per-crate dispatch) differ from `cfg`.
        let group_cfg = ResolvedConfig {
            tag_prefix: tag_prefix.clone(),
            custom_tag: None,
            ..cfg.clone()
        };

        let prev_tag = find_previous_tag(&group_cfg, git_config)?;

        // Scan commits across all paths in the group.
        let mut all_messages: Vec<String> = Vec::new();
        for crate_cfg in group {
            let msgs = get_messages_for_bump(cfg, prev_tag.as_deref(), Some(&crate_cfg.path))
                .unwrap_or_default();
            all_messages.extend(msgs);
        }
        let bump = detect_bump(&all_messages, &group_cfg);

        if bump == BumpKind::None {
            log.verbose(&format!(
                "group {:?}: no bump signal — skipping",
                group.iter().map(|c| c.name.as_str()).collect::<Vec<_>>()
            ));
            continue;
        }

        let (new_major, new_minor, new_patch, old_tag_str) = if let Some(ref prev) = prev_tag {
            let base = git::parse_semver_tag(prev)?;
            let (maj, min, pat) = apply_bump(base.major, base.minor, base.patch, &bump);
            (maj, min, pat, prev.as_str())
        } else {
            let base = git::parse_semver_tag(&format!("{}{}", tag_prefix, cfg.initial_version))
                .unwrap_or(git::SemVer {
                    major: 0,
                    minor: 1,
                    patch: 0,
                    prerelease: None,
                    build_metadata: None,
                });
            (base.major, base.minor, base.patch, "")
        };

        let mut new_version = format!("{}.{}.{}", new_major, new_minor, new_patch);
        if cfg.prerelease {
            new_version = format!("{}-{}", new_version, cfg.prerelease_suffix);
        }

        log.verbose(&format!(
            "group {:?}: {} -> {}{}",
            group.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            old_tag_str,
            tag_prefix,
            new_version
        ));

        // Build per-crate tags and version updates.
        let mut new_tags: Vec<(String, String)> = Vec::new();
        let mut version_updates: Vec<(String, String)> = Vec::new();
        let mut crate_version_files: Vec<Vec<String>> = Vec::new();
        for crate_cfg in group {
            let crate_prefix = git::extract_tag_prefix(&crate_cfg.tag_template)
                .unwrap_or_else(|| tag_prefix.clone());
            let new_tag = format!("{}{}", crate_prefix, new_version);
            let message = format!("Release {}", new_tag);
            new_tags.push((new_tag, message));
            version_updates.push((crate_cfg.path.clone(), new_version.clone()));
            crate_version_files.push(resolve_version_files(
                Some(crate_cfg),
                Some(anodizer_config),
            ));
        }

        results.push(GroupTagResult {
            crate_names: group.iter().map(|c| c.name.clone()).collect(),
            new_tags,
            version_updates,
            old_version: bare_version_from_tag(old_tag_str),
            prev_tag: (!old_tag_str.is_empty()).then(|| old_tag_str.to_string()),
            crate_version_files,
        });
    }

    Ok(results)
}

/// Execute per-crate / hybrid-workspace tagging when no `--crate` is given
/// and the repository has per-crate versions (not lockstep).
///
/// Runs change detection → computes new tags for changed groups → writes one
/// bump commit for all changed crates → creates all tags → pushes commit and
/// tags atomically → emits `anodizer-output crates=[...]` line.
/// Per-run controls threaded into the per-crate tagging path: the push-target
/// remote + resolved `tag.push` config value (CLI flags on [`TagOpts`] override
/// it), and whether the `CHANGELOG.md` refresh is enabled for this run.
#[derive(Debug, Clone, Copy)]
struct PushControls<'a> {
    remote: &'a str,
    config_push: Option<bool>,
    changelog_enabled: bool,
}

fn run_per_crate_tag(
    groups: Vec<Vec<CrateConfig>>,
    opts: &TagOpts,
    cfg: &ResolvedConfig,
    git_config: Option<&GitConfig>,
    anodizer_config: Option<&anodizer_core::config::Config>,
    controls: PushControls<'_>,
    log: &StageLogger,
) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let tag_results = compute_per_crate_tags(&groups, opts, cfg, git_config, anodizer_config, log)?;

    if tag_results.is_empty() {
        log.verbose("no changed crates — nothing to tag");
        println!("anodizer-output crates=[]");
        println!("anodizer-output versions={{}}");
        return Ok(());
    }

    let all_tagged_crates: Vec<String> = tag_results
        .iter()
        .flat_map(|r| r.crate_names.iter().cloned())
        .collect();

    let all_version_updates: Vec<(String, String)> = tag_results
        .iter()
        .flat_map(|r| r.version_updates.iter().cloned())
        .collect();

    // Dedupe across groups: same-prefix crates bumping to the same version
    // resolve to ONE shared tag, which must be created and pushed exactly once.
    let mut all_new_tags: Vec<String> = Vec::new();
    for r in &tag_results {
        for (t, _) in &r.new_tags {
            if !all_new_tags.contains(t) {
                all_new_tags.push(t.clone());
            }
        }
    }

    // Conflict-check + dedupe the version_files rewrites BEFORE the
    // dry-run/real split so a conflicting config bails identically in both
    // modes (and before any manifest is touched in the real run).
    let vf_plan = plan_version_files_rewrites(&tag_results)?;

    // One changelog target per bumped crate across all groups, each rendered
    // from its OWN group's previous tag to ITS new version. Empty when the
    // refresh is disabled.
    let mut changelog_targets = if controls.changelog_enabled {
        plan_changelog_targets(&cwd, &tag_results)
    } else {
        Vec::new()
    };
    // Per-crate(multi-track) targets already carry distinct correct tags, so a
    // single routed call covers both destinations (no aggregate split needed).
    let cl_config = changelog_config_for(anodizer_config);
    let mut changelog_routing = ChangelogRouting::from_config(&cl_config);

    // Collapse same-prefix shared-root targets to ONE flat aggregate. A flat
    // `crates:` list whose members all share one tag track (e.g.
    // `tag_template: "v{{ .Version }}"`) bumps to N identically-prefixed tags;
    // routed to one shared root they are a single lockstep release, not N
    // multi-track subsections. Promoting each member's section under the same
    // `## [v<X.Y.Z>]` heading would strand every member after the first and
    // graft spurious `### <crate>` subsections — the same bug the `changelog`
    // command collapses. Distinct prefixes or per-crate files are left as-is.
    if collapse_targets_to_flat_aggregate(
        &mut changelog_targets,
        &cwd,
        anodizer_config,
        changelog_routing.root_enabled,
        changelog_routing.per_crate,
    ) {
        changelog_routing.single_track = true;
    }

    if !opts.dry_run {
        // Apply version bumps across all changed crates in a single commit.
        for (path, new_version) in &all_version_updates {
            anodizer_stage_build::version_sync::sync_version(path, new_version, false, log)?;
        }

        // Propagate every bumped crate's new version into sibling manifests'
        // intra-workspace `[dependencies].<crate>.version` pins. Without this,
        // a workspace member that depends on a sibling at the old pinned
        // version (e.g. `cfgd-core = { path = "../cfgd-core", version = "0.3.5" }`)
        // still references the pre-bump version after sync_version() rewrites
        // only `[package].version`, and `cargo publish` later fails with
        // "failed to select a version for the requirement <sibling> = ^<old>".
        //
        // Each propagation is scoped to the Cargo workspace that owns the
        // bumped crate, so a bump in one release group never rewrites a pin in
        // an independent group whose crates live in a separate Cargo workspace.
        let workspace_root = std::env::current_dir()?;
        let workspace_root_str = workspace_root.to_string_lossy().into_owned();
        let mut intra_ws_modified: Vec<String> = Vec::new();
        for group_result in &tag_results {
            for (crate_name, (crate_path, new_version)) in group_result
                .crate_names
                .iter()
                .zip(group_result.version_updates.iter())
            {
                let modified = anodizer_stage_build::version_sync::sync_workspace_deps(
                    &workspace_root_str,
                    crate_path,
                    crate_name,
                    new_version,
                    false,
                    log,
                )?;
                intra_ws_modified.extend(modified);
            }
        }

        // Update Cargo.lock to match bumped manifests.
        match anodizer_core::cargo_lock::cargo_update_workspace(None) {
            Ok(_) => {}
            Err(e) => log.warn(&format!(
                "version-sync: could not spawn `cargo update --workspace` ({e}); Cargo.lock may be stale"
            )),
        }

        // Stage all bumped Cargo.toml files + intra-workspace dep rewrites +
        // Cargo.lock. Convert absolute intra-ws paths to repo-relative so
        // `git add` recognizes them.
        let mut files_to_stage: Vec<String> = all_version_updates
            .iter()
            .map(|(path, _)| format!("{}/Cargo.toml", path))
            .collect();
        for abs in &intra_ws_modified {
            let rel = Path::new(abs)
                .strip_prefix(&workspace_root)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| abs.clone());
            if !files_to_stage.contains(&rel) {
                files_to_stage.push(rel);
            }
        }
        // Rewrite enrolled version_files using each crate's group old→new.
        // The plan is conflict-checked (a shared path with non-identical
        // (old,new) pairs bails) and deduped once, identically to the dry-run
        // branch, so the preview matches the real run.
        for rewrite in &vf_plan {
            let vf_changed = rewrite_and_stage_version_files(
                std::slice::from_ref(&rewrite.file),
                &rewrite.old,
                &rewrite.new,
                false,
                log,
            )?;
            for f in vf_changed {
                if !files_to_stage.contains(&f) {
                    files_to_stage.push(f);
                }
            }
        }

        // Refresh each bumped crate's CHANGELOG.md and fold the written
        // (repo-relative) paths into the same bump commit.
        let cl_changed =
            render_and_stage_changelogs(&cwd, &changelog_targets, &changelog_routing, false, log)?;
        for f in cl_changed {
            if !files_to_stage.contains(&f) {
                files_to_stage.push(f);
            }
        }

        files_to_stage.push("Cargo.lock".to_string());
        let staged_refs: Vec<&str> = files_to_stage.iter().map(|s| s.as_str()).collect();

        // Build per-crate version arrows for the commit subject so each
        // crate's new version is visible (core→1.1.0, cli→2.1.0) instead of
        // using a single version that may only be correct for one group.
        let version_arrows: Vec<String> = tag_results
            .iter()
            .flat_map(|r| r.version_updates.iter())
            .map(|(path, ver)| {
                // Use the last path component as a short label.
                let label = std::path::Path::new(path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(path.as_str());
                format!("{}→{}", label, ver)
            })
            .collect();
        let bump_summary = if version_arrows.is_empty() {
            all_tagged_crates.join(", ")
        } else {
            version_arrows.join(", ")
        };
        git::stage_and_commit(
            &staged_refs,
            &format!(
                "chore(release): bump {}{}",
                bump_summary,
                skip_ci_suffix(cfg.skip_ci_on_bump)
            ),
        )?;

        // Create all tags locally; push happens atomically below. Crates that
        // share one tag prefix AND bump to the same version resolve to the SAME
        // tag (a lockstep aggregate expressed as a flat `crates:` list); creating
        // it once per crate would fail the second with "tag already exists", so
        // dedupe to one creation per distinct tag.
        let mut created: Vec<&str> = Vec::new();
        for group_result in &tag_results {
            for (tag, message) in &group_result.new_tags {
                if created.contains(&tag.as_str()) {
                    continue;
                }
                git::create_tag_local_only(&cwd, tag, message, false, log)?;
                created.push(tag.as_str());
            }
        }
    } else {
        // Dry-run: preview the same conflict-checked, deduped rewrite plan
        // without touching disk.
        for rewrite in &vf_plan {
            rewrite_and_stage_version_files(
                std::slice::from_ref(&rewrite.file),
                &rewrite.old,
                &rewrite.new,
                true,
                log,
            )?;
        }
        render_and_stage_changelogs(&cwd, &changelog_targets, &changelog_routing, true, log)?;
    }

    // Build the structured-output payloads up front, but DON'T print them
    // yet: a downstream consumer treats `anodizer-output crates=…` as "these
    // crates are tagged and pushed". Emitting before the atomic push would
    // advertise a successful tagging even when the push then fails (the `?`
    // below aborts mid-command, leaving the consumer believing the tags
    // landed). Defer the `println!`s until after the push returns Ok.
    let crates_json =
        serde_json::to_string(&all_tagged_crates).unwrap_or_else(|_| "[]".to_string());

    // Build crate-name → new-version map from version_updates (path → version),
    // joined against crate_names so the output uses canonical crate names rather
    // than filesystem paths. Each group's crates share the same new version.
    let versions_map: std::collections::HashMap<String, String> = tag_results
        .iter()
        .flat_map(|r| {
            r.crate_names
                .iter()
                .zip(r.version_updates.iter())
                .map(|(name, (_, ver))| (name.clone(), ver.clone()))
        })
        .collect();
    let versions_json = serde_json::to_string(&versions_map).unwrap_or_else(|_| "{}".to_string());

    // Per-crate auto-dispatch defaults to pushing the bump commit + tags
    // atomically (path_default = true). `--no-push` pushes the tags only,
    // leaving the bump commit local; `--push-dry-run` previews the push.
    let push_dry = opts.dry_run || opts.push_dry_run;
    let push_branch = resolve_tag_push_branch(opts, controls.config_push, true)?;
    git::push_branch_and_tags_atomic_in(
        &cwd,
        &git::AtomicPushSpec {
            remote: controls.remote,
            branch: push_branch.as_deref(),
            tags: &all_new_tags,
            dry_run: push_dry,
            strict: opts.strict,
        },
        log,
    )?;

    // Push succeeded (or was a dry-run/preview no-op that returns Ok). Now it
    // is safe to advertise the tagged crates + versions. In dry-run the lines
    // still appear so CI can observe what would be tagged.
    println!("anodizer-output crates={}", crates_json);
    println!("anodizer-output versions={}", versions_json);

    Ok(())
}

/// Info extracted from a crate's config for path-scoped tagging.
struct CrateTagInfo {
    tag_prefix: String,
    path: String,
    version_sync: bool,
    /// Effective `version_files` enrollment for this crate (per-crate /
    /// defaults list, else the top-level `Config.version_files`).
    version_files: Vec<String>,
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
    let version_files = resolve_version_files(Some(crate_cfg), Some(&config));
    Some(CrateTagInfo {
        tag_prefix,
        path: crate_cfg.path.clone(),
        version_sync,
        version_files,
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

    let tag_sort = git_config
        .and_then(|gc| gc.tag_sort.as_deref())
        .unwrap_or("-version:refname");
    if tag_sort == "smartsemver" && !cfg.prerelease {
        // When targeting a non-prerelease version, skip prerelease candidates
        // so the changelog base points at the previous stable release rather
        // than an intervening beta or RC.
        for tag in tags {
            if let Ok(sv) = git::parse_semver_tag(&tag)
                && !sv.is_prerelease()
            {
                return Ok(Some(tag));
            }
        }
        return Ok(None);
    }

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

/// Bare semver version embedded in a tag string (the tag with its prefix
/// stripped). Returns `None` for an empty tag (no previous release) or a tag
/// that does not parse as a semver tag.
fn bare_version_from_tag(tag: &str) -> Option<String> {
    if tag.is_empty() {
        return None;
    }
    let sv = git::parse_semver_tag(tag).ok()?;
    let mut v = format!("{}.{}.{}", sv.major, sv.minor, sv.patch);
    if let Some(pre) = sv.prerelease {
        v.push('-');
        v.push_str(&pre);
    }
    Some(v)
}

/// Rewrite the old version to the new version in every enrolled `version_files`
/// entry, log the per-file outcome, and return the repo-relative paths that
/// actually changed (so the caller can stage them into the bump commit).
///
/// A file with zero matches is reported via `warn` but is not an error: a stale
/// enrollment should surface loudly without aborting the tag. When `dry_run` is
/// set, counts are logged but no file is written and no path is returned for
/// staging. A no-op (`old == new`) returns immediately.
fn rewrite_and_stage_version_files(
    files: &[String],
    old: &str,
    new: &str,
    dry_run: bool,
    log: &StageLogger,
) -> Result<Vec<String>> {
    if files.is_empty() || old == new {
        return Ok(Vec::new());
    }
    let outcomes =
        anodizer_core::version_files::rewrite_version_in_files(files, old, new, dry_run)?;
    let mut changed = Vec::new();
    for outcome in &outcomes {
        if outcome.replacements > 0 {
            log.status(&format!(
                "{}version_files: rewrote {} occurrence(s) of {} → {} in {}",
                if dry_run { "(dry-run) " } else { "" },
                outcome.replacements,
                old,
                new,
                outcome.path
            ));
            if !dry_run {
                changed.push(outcome.path.clone());
            }
        } else {
            log.warn(&format!(
                "version_files: enrolled file {} did not contain version {} (nothing rewritten)",
                outcome.path, old
            ));
        }
    }
    Ok(changed)
}

/// One planned `version_files` rewrite: rewrite `old` → `new` in `file`.
struct VersionFileRewrite {
    file: String,
    old: String,
    new: String,
}

/// Build the deduped, conflict-checked set of `version_files` rewrites across
/// every per-crate group, in first-seen order.
///
/// A shared enrolled path is a conflict whenever two crates enrolling it carry
/// NON-IDENTICAL `(old, new)` pairs — a file cannot simultaneously rewrite
/// `0.1.0 → 0.2.0` and `0.1.5 → 0.2.0` (the second crate's occurrences would be
/// left stale), nor hold two different new versions at once. Identical pairs
/// dedupe to a single rewrite (lockstep crates share one pair, so they never
/// conflict). On conflict this `bail!`s naming the file and both crates/pairs.
///
/// Runs identically for dry-run and real tagging so the preview matches the
/// outcome — the validated plan is computed once, then either previewed or
/// applied by the caller.
fn plan_version_files_rewrites(tag_results: &[GroupTagResult]) -> Result<Vec<VersionFileRewrite>> {
    use std::collections::HashMap;
    // file → (old, new, owning-crate) of the first group that enrolled it.
    let mut seen: HashMap<String, (String, String, String)> = HashMap::new();
    let mut plan: Vec<VersionFileRewrite> = Vec::new();

    for group_result in tag_results {
        let Some(ref old) = group_result.old_version else {
            continue;
        };
        let owner = group_result
            .crate_names
            .first()
            .cloned()
            .unwrap_or_else(|| "?".to_string());
        for ((_, new_version), files) in group_result
            .version_updates
            .iter()
            .zip(group_result.crate_version_files.iter())
        {
            for file in files {
                match seen.get(file) {
                    Some((existing_old, existing_new, existing_crate))
                        if existing_old != old || existing_new != new_version =>
                    {
                        bail!(
                            "version_files conflict: {} is enrolled by crates with different \
                             version bumps ({} {} → {} vs {} {} → {}); a file cannot hold two \
                             versions in one tag run",
                            file,
                            existing_crate,
                            existing_old,
                            existing_new,
                            owner,
                            old,
                            new_version,
                        );
                    }
                    Some(_) => {}
                    None => {
                        seen.insert(
                            file.clone(),
                            (old.clone(), new_version.clone(), owner.clone()),
                        );
                        plan.push(VersionFileRewrite {
                            file: file.clone(),
                            old: old.clone(),
                            new: new_version.clone(),
                        });
                    }
                }
            }
        }
    }

    Ok(plan)
}

/// Build one [`ChangelogTarget`] per bumped crate across all groups.
///
/// Each crate renders from ITS group's previous tag (`prev_tag`) to ITS new
/// version, so independently-versioned crates each get a section keyed to their
/// own bump. `crate_dir` is resolved to an absolute path under `workspace_root`
/// so the changelog engine reads/writes the correct `CHANGELOG.md`.
fn plan_changelog_targets(
    workspace_root: &Path,
    tag_results: &[GroupTagResult],
) -> Vec<ChangelogTarget> {
    let mut targets = Vec::new();
    for group_result in tag_results {
        for ((crate_name, (crate_path, new_version)), (full_tag, _msg)) in group_result
            .crate_names
            .iter()
            .zip(group_result.version_updates.iter())
            .zip(group_result.new_tags.iter())
        {
            targets.push(ChangelogTarget {
                crate_name: crate_name.clone(),
                crate_dir: workspace_root.join(crate_path),
                from_tag: group_result.prev_tag.clone(),
                to_version: new_version.clone(),
                full_tag: full_tag.clone(),
            });
        }
    }
    targets
}

/// Collapse `targets` in place to ONE flat whole-workspace aggregate when the
/// per-crate targets are actually a single lockstep track: shared-root-only
/// routing (`root_enabled && !per_crate`) AND every target's tag shares one
/// prefix. Returns `true` when collapsed (the caller then sets the routing's
/// `single_track`), `false` otherwise (genuine multi-track / per-crate files /
/// nothing to collapse — `targets` left untouched).
///
/// The aggregate spans the workspace (`crate_dir = workspace_root`), keyed by
/// `project_name`, with the shared `from_tag` / `full_tag` every member already
/// carries (identical across a lockstep set).
fn collapse_targets_to_flat_aggregate(
    targets: &mut Vec<ChangelogTarget>,
    workspace_root: &Path,
    config: Option<&anodizer_core::config::Config>,
    root_enabled: bool,
    per_crate: bool,
) -> bool {
    if targets.len() <= 1 {
        return false;
    }
    let Some(config) = config else {
        return false;
    };
    // Resolve each configured crate's tag prefix from its `tag_template` (the
    // same source `changelog`'s `select_crates` uses). Concrete `full_tag`s
    // (e.g. `v0.6.0`) carry no template, so the template is the reliable prefix
    // source. The bumped `targets` are a subset of these crates, so a uniform
    // configured prefix implies a uniform target prefix.
    let prefixes: Vec<String> = config
        .crates
        .iter()
        .chain(
            config
                .workspaces
                .as_deref()
                .unwrap_or_default()
                .iter()
                .flat_map(|w| &w.crates),
        )
        .map(|c| {
            git::extract_tag_prefix(&c.tag_template).unwrap_or_else(|| format!("{}-v", c.name))
        })
        .collect();
    if !crate::commands::changelog_sync::is_flat_aggregate(&prefixes, root_enabled, per_crate) {
        return false;
    }
    let project_name = config.project_name.clone();
    // Every member shares one tag in a lockstep set; take the first's range
    // bounds for the whole-release aggregate.
    let first = match targets.first() {
        Some(t) => t,
        None => return false,
    };
    let aggregate = ChangelogTarget {
        crate_name: project_name,
        crate_dir: workspace_root.to_path_buf(),
        from_tag: first.from_tag.clone(),
        to_version: first.to_version.clone(),
        full_tag: first.full_tag.clone(),
    };
    *targets = vec![aggregate];
    true
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

    // ---- Push resolution tests ----

    /// Build a `TagOpts` carrying only the two push toggles under test;
    /// everything else is left at its inert default.
    fn push_opts(push: bool, no_push: bool) -> TagOpts {
        TagOpts {
            dry_run: false,
            custom_tag: None,
            default_bump: None,
            crate_name: None,
            push,
            no_push,
            push_remote: None,
            push_dry_run: false,
            changelog: false,
            config_override: None,
            verbose: false,
            debug: false,
            quiet: false,
            strict: false,
        }
    }

    #[test]
    fn resolve_effective_push_matrix() {
        // (push, no_push, config_push, path_default) -> expected
        let cases: &[(bool, bool, Option<bool>, bool, bool)] = &[
            // --no-push wins over everything.
            (false, true, Some(true), true, false),
            (true, true, Some(true), true, false), // (clap forbids push+no_push, but the resolver must still be safe)
            (false, true, None, true, false),
            // --push forces a branch push even on a false default.
            (true, false, None, false, true),
            // config push=true forces a branch push on a false default.
            (false, false, Some(true), false, true),
            // config push=false does not force; default passes through.
            (false, false, Some(false), false, false),
            (false, false, Some(false), true, true),
            // No signal: the per-path default passes through (both polarities).
            (false, false, None, false, false),
            (false, false, None, true, true),
        ];
        for &(push, no_push, config_push, path_default, expected) in cases {
            let opts = push_opts(push, no_push);
            assert_eq!(
                resolve_effective_push(&opts, config_push, path_default),
                expected,
                "push={push} no_push={no_push} config_push={config_push:?} path_default={path_default}"
            );
        }
    }

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
            push: false,
            no_push: false,
            push_remote: None,
            push_dry_run: false,
            changelog: false,
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
            push: false,
            no_push: false,
            push_remote: None,
            push_dry_run: false,
            changelog: false,
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
            push: None,
            skip_ci_on_bump: None,
            verbose: Some(false),
            tag_pre_hooks: None,
            tag_post_hooks: None,
        };
        let opts = TagOpts {
            dry_run: false,
            custom_tag: None,
            default_bump: None,
            crate_name: None,
            push: false,
            no_push: false,
            push_remote: None,
            push_dry_run: false,
            changelog: false,
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

    // ---- detect_repo_shape unit tests ----

    fn crate_cfg(name: &str, path: &str, template: &str) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: path.to_string(),
            tag_template: template.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn detect_repo_shape_no_config_no_workspace_returns_single() {
        // Bare repo: no anodizer config, no Cargo workspace info → Single.
        let shape = detect_repo_shape(None, None);
        assert!(matches!(shape, RepoShape::Single));
    }

    #[test]
    fn detect_repo_shape_single_crate_config_returns_single() {
        let config = anodizer_core::config::Config {
            project_name: "app".to_string(),
            crates: vec![crate_cfg("app", ".", "v{{ .Version }}")],
            ..Default::default()
        };
        let shape = detect_repo_shape(Some(&config), None);
        assert!(matches!(shape, RepoShape::Single));
    }

    #[test]
    fn detect_repo_shape_lockstep_workspace_wins_over_per_crate_config() {
        // [workspace.package].version is authoritative — even when the
        // anodizer config has multiple flat crates, a lockstep workspace
        // returns Lockstep so the operator's Cargo-level intent wins.
        let config = anodizer_core::config::Config {
            project_name: "ws".to_string(),
            crates: vec![
                crate_cfg("a", "crates/a", "a-v{{ .Version }}"),
                crate_cfg("b", "crates/b", "b-v{{ .Version }}"),
            ],
            ..Default::default()
        };
        let ws = WorkspaceInfo {
            workspace_package_version: Some("0.1.0".to_string()),
            members: vec![],
        };
        let shape = detect_repo_shape(Some(&config), Some(&ws));
        assert!(matches!(shape, RepoShape::Lockstep));
    }

    #[test]
    fn detect_repo_shape_flat_multi_crate_returns_per_crate() {
        let config = anodizer_core::config::Config {
            project_name: "ws".to_string(),
            crates: vec![
                crate_cfg("core", "crates/core", "core-v{{ .Version }}"),
                crate_cfg("cli", "crates/cli", "v{{ .Version }}"),
            ],
            ..Default::default()
        };
        let shape = detect_repo_shape(Some(&config), None);
        match shape {
            RepoShape::PerCrate(groups) => {
                assert_eq!(groups.len(), 2);
                // Flat layout: each crate is its own singleton group.
                assert_eq!(groups[0][0].name, "core");
                assert_eq!(groups[1][0].name, "cli");
            }
            other => panic!(
                "expected PerCrate, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn detect_repo_shape_hybrid_workspaces_returns_per_crate_groups() {
        // workspaces: with two groups (one singleton, one lockstep pair) →
        // PerCrate, preserving group boundaries so each group bumps as a unit.
        let ws1 = anodizer_core::config::WorkspaceConfig {
            name: "group-a".to_string(),
            crates: vec![crate_cfg("core", "crates/core", "core-v{{ .Version }}")],
            ..Default::default()
        };
        let ws2 = anodizer_core::config::WorkspaceConfig {
            name: "group-b".to_string(),
            crates: vec![
                crate_cfg("bin-a", "crates/bin-a", "bin-a-v{{ .Version }}"),
                crate_cfg("bin-b", "crates/bin-b", "bin-b-v{{ .Version }}"),
            ],
            ..Default::default()
        };
        let config = anodizer_core::config::Config {
            project_name: "myproj".to_string(),
            workspaces: Some(vec![ws1, ws2]),
            ..Default::default()
        };
        let shape = detect_repo_shape(Some(&config), None);
        match shape {
            RepoShape::PerCrate(groups) => {
                assert_eq!(groups.len(), 2);
                assert_eq!(groups[0].len(), 1);
                assert_eq!(groups[0][0].name, "core");
                assert_eq!(groups[1].len(), 2);
                assert_eq!(groups[1][0].name, "bin-a");
                assert_eq!(groups[1][1].name, "bin-b");
            }
            other => panic!(
                "expected PerCrate, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn detect_repo_shape_workspaces_block_wins_over_workspace_package_version() {
        let ws1 = anodizer_core::config::WorkspaceConfig {
            name: "group".to_string(),
            crates: vec![
                crate_cfg("a", "crates/a", "a-v{{ .Version }}"),
                crate_cfg("b", "crates/b", "b-v{{ .Version }}"),
            ],
            ..Default::default()
        };
        let config = anodizer_core::config::Config {
            project_name: "p".to_string(),
            workspaces: Some(vec![ws1]),
            ..Default::default()
        };
        let ws = WorkspaceInfo {
            workspace_package_version: Some("0.2.0".to_string()),
            members: vec![],
        };
        let shape = detect_repo_shape(Some(&config), Some(&ws));
        match shape {
            RepoShape::PerCrate(groups) => {
                assert_eq!(groups.len(), 1);
                assert_eq!(groups[0].len(), 2);
            }
            other => panic!(
                "expected PerCrate (workspaces: declaration wins over [workspace.package].version), got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn detect_repo_shape_single_flat_crate_returns_single() {
        // A flat config with exactly one crate is NOT per-crate (no group
        // routing needed); it falls through to the single-crate path.
        let config = anodizer_core::config::Config {
            project_name: "solo".to_string(),
            crates: vec![crate_cfg("solo", ".", "v{{ .Version }}")],
            ..Default::default()
        };
        let shape = detect_repo_shape(Some(&config), None);
        assert!(matches!(shape, RepoShape::Single));
    }

    // ---- anodizer-output line format tests ----

    #[test]
    fn anodizer_output_format_empty() {
        let crates: Vec<String> = vec![];
        let json = serde_json::to_string(&crates).unwrap();
        assert_eq!(json, "[]");
        let line = format!("anodizer-output crates={}", json);
        assert_eq!(line, "anodizer-output crates=[]");
    }

    #[test]
    fn anodizer_output_format_single_crate() {
        let crates = vec!["myproj-core".to_string()];
        let json = serde_json::to_string(&crates).unwrap();
        let line = format!("anodizer-output crates={}", json);
        assert_eq!(line, "anodizer-output crates=[\"myproj-core\"]");
    }

    #[test]
    fn anodizer_output_format_multi_crate() {
        let crates = vec!["core".to_string(), "bin-a".to_string(), "bin-b".to_string()];
        let json = serde_json::to_string(&crates).unwrap();
        let line = format!("anodizer-output crates={}", json);
        assert_eq!(
            line,
            "anodizer-output crates=[\"core\",\"bin-a\",\"bin-b\"]"
        );
    }

    #[test]
    fn anodizer_output_versions_format_empty() {
        // Zero-change push must emit a stable `versions={}` literal so
        // downstream `fromJson()` parsers always see a valid empty object.
        let versions: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let json = serde_json::to_string(&versions).unwrap();
        assert_eq!(json, "{}");
    }

    #[test]
    fn anodizer_output_versions_format_single_crate() {
        let mut versions = std::collections::HashMap::new();
        versions.insert("cfgd-core".to_string(), "0.4.0".to_string());
        let json = serde_json::to_string(&versions).unwrap();
        // serde_json::to_string for a single-entry map is deterministic.
        assert_eq!(json, "{\"cfgd-core\":\"0.4.0\"}");
    }
}
