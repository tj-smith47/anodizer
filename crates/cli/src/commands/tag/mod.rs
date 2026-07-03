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
    /// Explicit `--version`: tag exactly this version, bypassing autotag
    /// derivation and the Cargo.toml-ahead guard. Accepts `1.2.3` or `v1.2.3`;
    /// normalized + validated in [`run`].
    pub version_override: Option<String>,
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
    bump_minor_pre_major: bool,
    bump_patch_for_minor_pre_major: bool,
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
                .unwrap_or_else(|| "none".to_string()),
            bump_minor_pre_major: cfg.bump_minor_pre_major.unwrap_or(false),
            bump_patch_for_minor_pre_major: cfg.bump_patch_for_minor_pre_major.unwrap_or(false),
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
    // Discover the workspace root once, config-derived, so `tag` resolves the
    // same root whether invoked from the repo root or a subdirectory (matching
    // `bump` and `changelog`); every workspace-load / git-working-dir site below
    // threads this one value instead of re-reading the cwd.
    let workspace_root_path =
        crate::commands::helpers::discover_workspace_root(resolve_config_path(&opts).as_deref())?;
    // Load the full config + Cargo workspace once so all downstream helpers
    // share the same parse (eliminates the previous triple workspace-file
    // read on lockstep repos). Resolved at the discovered workspace root so a
    // subdirectory invocation still finds the repo-root `.anodizer.yaml` and its
    // `version_files` enrollment, not just whatever sits in the cwd.
    let loaded_config: Option<anodizer_core::config::Config> =
        Some(load_config_at(&opts, &workspace_root_path)?);
    let loaded_workspace: Option<WorkspaceInfo> = load_workspace(&workspace_root_path)?;

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

    // Reject an incoherent flat-aggregate config (members sharing one tag prefix
    // but disagreeing on `[package].version`) before any work, identically to
    // `changelog` and `bump`.
    guard_flat_aggregate_coherence(
        loaded_config.as_ref(),
        loaded_workspace.as_ref(),
        &workspace_root_path,
    )?;

    let mut cfg = ResolvedConfig::from_tag_config(&tag_config, &opts);

    // Validate + normalize the explicit `--version` override once, up front, so
    // an ill-formed value fails before any git/manifest work. The bare
    // `MAJOR.MINOR.PATCH[-pre][+build]` form is retained (the configured tag
    // prefix is re-applied at tag-creation time); accepting both `1.2.3` and
    // `v1.2.3` is exactly `parse_semver`'s contract.
    let version_override: Option<String> = match opts.version_override.as_deref() {
        Some(raw) => {
            let sv = git::parse_semver(raw).map_err(|_| {
                anyhow::anyhow!("--version {:?} is not a valid semver version", raw)
            })?;
            Some(sv.version_string())
        }
        None => None,
    };

    // Push controls shared by every tagging path. `remote` defaults to origin;
    // `effective_push` per-path resolution is computed at each call site so the
    // per-crate path can carry its own (true) default.
    let remote = opts.push_remote.as_deref().unwrap_or("origin").to_string();
    let config_push = tag_config.push;

    // When --crate is given, look up the crate in config and derive the tag
    // prefix from its tag_template.  Also capture the crate path to
    // scope change detection to only that directory.
    let mut crate_path: Option<String> = None;
    let mut version_sync_enabled = false;
    let mut crate_version_files: Vec<String> = Vec::new();
    if let Some(ref crate_name) = opts.crate_name
        && let Some(info) = loaded_config
            .as_ref()
            .and_then(|c| load_crate_tag_info(c, crate_name))
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
        // Peek at the repo shape without consuming it, to surface a useful
        // error rather than silently discarding the custom_tag value.
        if matches!(
            detect_repo_shape(
                &workspace_root_path,
                loaded_config.as_ref(),
                loaded_workspace.as_ref()
            ),
            RepoShape::PerCrate(_)
        ) {
            anyhow::bail!(
                "--custom-tag {:?} is incompatible with per-crate workspace mode; \
                 pass --crate <name> to override a single crate's tag",
                ct
            );
        }
    }

    // `--version` pins ONE version. In per-crate / flat-aggregate dispatch there
    // is no single versioned unit — applying one version across independently
    // versioned crates would corrupt their cadences — so reject it unless
    // `--crate <name>` narrows to a single crate (which routes through the
    // single-crate derivation path below where the override is honored).
    if let Some(ref v) = version_override
        && opts.crate_name.is_none()
        && matches!(
            detect_repo_shape(
                &workspace_root_path,
                loaded_config.as_ref(),
                loaded_workspace.as_ref()
            ),
            RepoShape::PerCrate(_) | RepoShape::FlatAggregate(_)
        )
    {
        anyhow::bail!(
            "--version {:?} is incompatible with per-crate workspace mode; \
             pass --crate <name> to pin a single crate's version",
            v
        );
    }
    if opts.crate_name.is_none() {
        // A `FlatAggregate` (shared-prefix flat `crates:` list, no
        // `[workspace.package].version`) has its versions in N per-crate
        // `[package].version` manifests, so it is bumped by the per-crate engine
        // applied as ONE group: that creates the single shared tag (its built-in
        // dedup) and the one collapsed root changelog section. A custom tag names
        // ONE explicit tag for the shared unit; it is honored by the lockstep
        // custom-tag fall-through below, not the per-crate engine (which ignores
        // `custom_tag`), so a `FlatAggregate` WITH a custom tag stays out of the
        // group dispatch.
        let mut is_flat_aggregate = false;
        let groups = match detect_repo_shape(
            &workspace_root_path,
            loaded_config.as_ref(),
            loaded_workspace.as_ref(),
        ) {
            RepoShape::PerCrate(groups) => Some(groups),
            RepoShape::FlatAggregate(crates) if cfg.custom_tag.is_none() => {
                is_flat_aggregate = true;
                Some(vec![crates])
            }
            // Genuine Cargo-workspace lockstep, single-crate repos, and a
            // `FlatAggregate` carrying a custom tag keep the existing
            // fall-through paths below.
            RepoShape::Single | RepoShape::Lockstep | RepoShape::FlatAggregate(_) => None,
        };

        if let Some(groups) = groups {
            // Build log early so status messages are consistent.
            let config_verbose = tag_config.verbose.unwrap_or(false);
            let effective_verbose = opts.verbose || (config_verbose && !opts.quiet);
            let log = StageLogger::new(
                "tag",
                Verbosity::from_flags(opts.quiet, effective_verbose, opts.debug),
            );
            // Submitter moderation-queue advisories are verbose-only; emit them
            // once off the single load (hidden at the default log level).
            if let Some(c) = loaded_config.as_ref() {
                crate::pipeline::emit_config_advisories(c, &log);
            }
            log.status(&format!(
                "running auto-tag (per-crate){}",
                if opts.dry_run { " (dry-run)" } else { "" }
            ));
            return run_per_crate_tag(
                PerCrateDispatch {
                    groups,
                    is_flat_aggregate,
                    workspace_root: workspace_root_path.clone(),
                },
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
    // Submitter moderation-queue advisories are verbose-only; emit them once
    // off the single load (hidden at the default log level).
    if let Some(c) = loaded_config.as_ref() {
        crate::pipeline::emit_config_advisories(c, &log);
    }

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

    // The git working dir is the discovered workspace root — bind once and
    // reuse across every tag so git ops run from the repo root even when the
    // command was invoked from a subdirectory.
    let cwd = workspace_root_path.clone();

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
            // Resolve the repo identity once (config override -> origin
            // remote) and hand it to the API tagger so it agrees with the
            // rest of the pipeline instead of re-parsing the remote itself.
            let release_github = loaded_config
                .as_ref()
                .and_then(|c| c.release.as_ref())
                .and_then(|r| r.github.as_ref());
            let slug = git::resolve_github_slug_in(
                release_github.map(|g| g.owner.as_str()),
                release_github.map(|g| g.name.as_str()),
                &cwd,
            )?;
            git::create_tag_via_github_api_in(
                &cwd,
                std::path::Path::new("gh"),
                &slug,
                tag,
                message,
                dry_run,
                &log,
                strict,
            )?;
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
        log.verbose(&format!("using custom tag {}", new_tag));
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

    // Check release branches. An explicit `--version` is an authoritative
    // "tag exactly this" request, so it bypasses the non-release-branch
    // hash-postfix guard (recovery tagging often runs off the release branch).
    let current_branch = git::get_current_branch()?;
    if version_override.is_none()
        && !cfg.release_branches.is_empty()
        && !branch_matches(&current_branch, &cfg.release_branches)
    {
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
        "previous tag = {}",
        prev_tag.as_deref().unwrap_or("(none)")
    ));

    // Check for changes since last tag.  When a crate path is known, scope
    // to that directory so unrelated commits don't trigger a spurious bump.
    if let Some(ref tag) = prev_tag {
        let has_changes = if let Some(ref path) = crate_path {
            git::has_changes_since_in(&workspace_root_path, tag, path)?
        } else {
            git::has_commits_since_tag(tag)?
        };
        if !has_changes {
            // An explicit `--version` is an authoritative release request, so it
            // forces past the "no changes since last tag" skip the same way
            // `force_without_changes` does (release-recovery re-tags often carry
            // no new commits).
            let force = version_override.is_some()
                || if cfg.prerelease {
                    cfg.force_without_changes_pre
                } else {
                    cfg.force_without_changes
                };
            if !force {
                log.verbose(&format!("skipped tag — no changes since {}", tag));
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
    let messages = get_messages_for_bump(
        &workspace_root_path,
        &cfg,
        prev_tag.as_deref(),
        crate_path.as_deref(),
    )?;
    log.verbose(&format!("scanned {} commit message(s)", messages.len()));

    // Detect bump (with pre-major demotion applied to inferred bumps).
    let bump = detect_bump_demoted(&messages, &cfg, prev_tag.as_deref());
    log.verbose(&format!("detected bump {:?}", bump));

    // The current manifest version for this tagging unit: the workspace
    // `[workspace.package].version` in lockstep mode, else the version-synced
    // crate's own `Cargo.toml`. Read+parsed once here and reused by both the
    // `cargo_ahead` release-signal check and the downgrade guard below so the
    // two never drift on which manifest they consult.
    let cargo_current_ver: Option<String> = if let Some(ws) = workspace_info {
        ws.workspace_package_version.clone()
    } else if version_sync_enabled && let Some(ref path) = crate_path {
        // Resolve against the discovered workspace root so the manifest read
        // matches the git working dir when `tag` runs from a subdirectory.
        let abs = workspace_root_path.join(path);
        anodizer_stage_build::version_sync::read_cargo_version(&abs.to_string_lossy()).ok()
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
    // An explicit `--version` is itself the release signal, so it tags
    // regardless of any per-commit bump directive.
    if bump == BumpKind::None && !cargo_ahead && version_override.is_none() {
        log.verbose("skipped tag — no bump signal and Cargo.toml not ahead");
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

    // When version_sync is enabled, a Cargo.toml version already higher than
    // the tag-derived version wins, to avoid downgrading a manual bump. This is
    // the Cargo.toml-ahead guard; computing it here (even with `--version` set)
    // yields the version autotag *would* have produced, so the override warning
    // can name the true derived value the operator is overriding.
    if let Some(cargo_ver) = cargo_current_ver
        && let Ok(cargo_sv) = git::parse_semver(&cargo_ver)
    {
        let tag_tuple = (new_major, new_minor, new_patch);
        let cargo_tuple = (cargo_sv.major, cargo_sv.minor, cargo_sv.patch);
        if cargo_tuple > tag_tuple {
            if version_override.is_none() {
                log.status(&format!(
                    "Cargo.toml version {} > tag-derived {}, using Cargo.toml version",
                    cargo_ver, new_version
                ));
            }
            new_version = cargo_ver;
        }
    }

    if let Some(pinned) = version_override {
        // The operator is authoritative: pin the explicit version verbatim,
        // bypassing the autotag bump AND the Cargo.toml-ahead guard above. Warn
        // when it disagrees with the version derivation would have produced
        // (`new_version` now holds that fully-derived value) so the divergence
        // is visible, then proceed with the explicit one.
        if pinned != new_version {
            log.warn(&format!(
                "--version {} overrides the derived version {} (autotag + Cargo.toml-ahead guard bypassed)",
                pinned, new_version
            ));
        }
        new_version = pinned;
    }

    let new_tag = format!("{}{}", cfg.tag_prefix, new_version);

    log.verbose(&format!("{} → {}", old_tag_str, new_tag));

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
        let root = workspace_root_path.as_path();
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
        // `path` is the config-declared (repo-root-relative) crate directory.
        // Resolve it against the discovered workspace root so the manifest /
        // dep-scan file IO hits the same tree git operates on even when `tag`
        // is invoked from a subdirectory.
        let abs_crate_dir = workspace_root_path
            .join(path)
            .to_string_lossy()
            .into_owned();
        anodizer_stage_build::version_sync::sync_version(
            &abs_crate_dir,
            &new_version,
            opts.dry_run,
            &log,
        )?;

        // Cross-crate dep updates scan from the discovered workspace root.
        let workspace_root = workspace_root_path.to_string_lossy().to_string();

        // Read the crate name from its Cargo.toml for dep scanning.
        let crate_cargo = std::path::Path::new(&abs_crate_dir).join("Cargo.toml");
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
                &abs_crate_dir,
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
                &workspace_root_path,
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
            let mut routing = ChangelogRouting::from_config(&cl_config);
            // `--crate <name>` single-target on a PerCrate workspace: topology
            // count is 1, so the renderer relies on the crate-name-aware
            // fallback. Supply the FULL root-routed crate set so an existing
            // `### <crate>` subsection is detected and a foreign heading is not.
            if let Some(cfg) = loaded_config.as_ref() {
                routing.root_crate_names = crate::commands::changelog_sync::config_root_crate_names(
                    cfg,
                    routing.root_crates,
                );
            }
            render_and_stage_changelogs(ws_root, &targets, &routing, opts.dry_run, &log)?
        } else {
            Vec::new()
        };

        if !opts.dry_run {
            // Regenerate Cargo.lock to match the bumped Cargo.toml versions.
            // Without this, the tagged commit has Cargo.toml at the new version
            // but Cargo.lock at the old version, causing `cargo test` (from
            // before hooks) to update Cargo.lock and dirty the tree.
            match anodizer_core::cargo_lock::cargo_update_workspace(Some(workspace_root_path.as_path())) {
                Ok(true) => {}
                Ok(false) => log.warn(
                    "`cargo update --workspace` exited non-zero after version sync; Cargo.lock may be stale",
                ),
                Err(e) => log.warn(&format!(
                    "could not spawn `cargo update --workspace` ({e}); Cargo.lock may be stale"
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
            // Staged from the discovered workspace root so the repo-relative
            // paths resolve there, not against a subdirectory cwd.
            bump_commit_created = git::stage_and_commit_in(
                &workspace_root_path,
                &files_to_stage,
                &git::release_bump_subject(
                    &format!("{} → {}", path, new_version),
                    skip_ci_suffix(cfg.skip_ci_on_bump),
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
        // Per-crate files are flat and independent; the root is handled by the
        // separate aggregate routing below.
        multitrack: false,
        root_crate_names: Vec::new(),
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
        // A lockstep aggregate is flat, never multitrack.
        multitrack: false,
        root_crate_names: Vec::new(),
    };

    if dry_run {
        log.status(&format!(
            "(dry-run) would bump {} workspace crate(s) → {}",
            rows.iter().filter(|r| r.level != BumpLevel::Skip).count(),
            new_version
        ));
        if let Some(old) = vf.old {
            rewrite_and_stage_version_files(workspace_root, vf.files, old, new_version, true, log)?;
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
            "`cargo update --workspace` exited non-zero after version sync; Cargo.lock may be stale",
        ),
        Err(e) => log.warn(&format!(
            "could not spawn `cargo update --workspace` ({e}); Cargo.lock may be stale"
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
        let vf_changed = rewrite_and_stage_version_files(
            workspace_root,
            vf.files,
            old,
            new_version,
            false,
            log,
        )?;
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

    git::stage_and_commit_in(
        workspace_root,
        &staged_refs,
        &git::release_bump_subject(
            &format!("workspace → {}", new_version),
            skip_ci_suffix(skip_ci_on_bump),
        ),
    )?;

    log.status(&format!(
        "bumped {} workspace crate(s) → {}",
        rows.iter().filter(|r| r.level != BumpLevel::Skip).count(),
        new_version
    ));
    Ok(true)
}

/// Resolve the config file path from CLI overrides or cwd-relative
/// auto-detection.
///
/// Used only for the bootstrap workspace-root discovery, where the root is not
/// yet known. Once the root is known, prefer [`resolve_config_path_at`] so a
/// subdirectory invocation still finds the repo-root config.
fn resolve_config_path(opts: &TagOpts) -> Option<std::path::PathBuf> {
    opts.config_override
        .as_deref()
        .filter(|p| p.exists())
        .map(|p| p.to_path_buf())
        .or_else(|| crate::pipeline::find_config(None).ok())
}

/// Load the anodizer config against a known workspace `root`.
///
/// A `--config` override always wins. Otherwise the config is searched at the
/// discovered workspace root (via [`crate::pipeline::load_repo_config`]) rather
/// than the process cwd, so `tag` invoked from a subdirectory still loads the
/// repo-root `.anodizer.yaml` and its `version_files` enrollment. Returns `None`
/// when no config is found or it fails to parse.
fn load_config_at(opts: &TagOpts, root: &Path) -> Result<anodizer_core::config::Config> {
    match opts.config_override.as_deref() {
        Some(p) => {
            // An explicitly-named `--config` path that doesn't exist is an
            // operator error, not a silent fall-through to the repo config
            // (which would read a DIFFERENT config than the one named).
            if !p.exists() {
                anyhow::bail!("--config path does not exist: {}", p.display());
            }
            crate::pipeline::load_config(p)
        }
        // `load_repo_config` returns `Ok(Config::default())` for a repo with a
        // Cargo.toml but no `.anodizer.yaml` (the Cargo.toml fallback), and
        // only `Err`s when the config file exists but fails to read/parse (or
        // neither a config nor a Cargo.toml is present). Propagating that
        // `Err` — instead of the old `.ok()` that flattened it to a silent
        // default — is what stops a malformed `.anodizer.yaml` from cutting
        // the wrong tag (lost `default_bump` / changelog / version_files).
        None => crate::pipeline::load_repo_config(root),
    }
}

/// Repository shape as detected from Cargo.toml + `.anodizer.yaml`.
pub(crate) enum RepoShape {
    /// Single crate or no config — use single-crate path unchanged.
    Single,
    /// `[workspace.package].version` is set — genuine lockstep workspace. One
    /// shared version drives one tag and one changelog, bumped by rewriting the
    /// single root manifest field.
    Lockstep,
    /// A flat `crates:` list of >1 crate whose `tag_template`s ALL yield the
    /// same extractable prefix (one shared `v*` tag namespace) but with per-crate
    /// `[package].version` and no `[workspace.package].version`. Semantically a
    /// lockstep release (one shared tag, one flat changelog), but bumped by
    /// writing N per-crate manifest version fields — so it routes through the
    /// per-crate engine as ONE group rather than the single-manifest lockstep
    /// bump. Carries the flat crate list.
    FlatAggregate(Vec<CrateConfig>),
    /// Each member has its own `[package].version` (no `[workspace.package].version`)
    /// AND the anodizer config has a per-crate/workspace definition.
    /// Carries the groups to iterate: each `Vec<CrateConfig>` is one lockstep
    /// group (singleton = independent release).
    PerCrate(Vec<Vec<CrateConfig>>),
}

/// Detect the repository shape for default (no `--crate`) tag behaviour.
///
/// Reads the Cargo workspace and anodizer config. Precedence:
/// 1. If anodizer config has `workspaces:` with groups → `PerCrate` (hybrid;
///    explicit operator intent, wins over a lockstep `[workspace.package].version`).
///    Top-level `crates:` entries not in any group join as singleton groups
///    (independent tracks).
/// 2. If `[workspace.package].version` is set → `Lockstep`.
/// 3. If anodizer config has `crates:` with >1 entry:
///    - all sharing ONE explicit tag prefix → `FlatAggregate` (one prefix = one
///      shared tag namespace, so the crates release in lockstep — `v0.2.0`
///      cannot independently belong to two crates — but each carries its own
///      `[package].version`, so it bumps N manifests under one shared tag);
///    - otherwise → `PerCrate` (flat multi-crate, distinct tracks).
/// 4. Otherwise → `Single`.
pub(crate) fn detect_repo_shape(
    workspace_root: &Path,
    preloaded_config: Option<&anodizer_core::config::Config>,
    preloaded_workspace: Option<&WorkspaceInfo>,
) -> RepoShape {
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
        let mut groups: Vec<Vec<CrateConfig>> =
            ws_list.iter().map(|ws| ws.crates.clone()).collect();
        // A top-level crate alongside `workspaces:` is its own independent
        // track (a singleton group, exactly like the flat PerCrate arm
        // below) — dropping it would leave it untagged forever. One that
        // also appears in a workspace group stays with its group: the group
        // defines its lockstep cadence, and the top-level duplicate is the
        // same crate (the universe's first-seen dedup), not a second track.
        for c in &config.crates {
            if !groups.iter().flatten().any(|g| g.name == c.name) {
                groups.push(vec![c.clone()]);
            }
        }
        return RepoShape::PerCrate(groups);
    }

    let lockstep = if let Some(ws) = preloaded_workspace {
        ws.workspace_package_version.is_some()
    } else {
        // Unpreloaded fallback (standalone/test callers only — every
        // production caller passes `preloaded_workspace`, and they load it
        // with `?` so a malformed manifest bails before reaching here). A
        // `None` here is an absent Cargo.toml (non-lockstep); even if a
        // parse error were swallowed to `None`, the actual bump execution
        // re-loads via `load_workspace` and bails, so no wrong tag is cut.
        load_workspace(workspace_root)
            .ok()
            .flatten()
            .is_some_and(|ws| ws.workspace_package_version.is_some())
    };
    if lockstep {
        return RepoShape::Lockstep;
    }

    let config = match preloaded_config {
        Some(c) => c,
        None => return RepoShape::Single,
    };

    // Raw `config.crates` walks from here on are the whole universe: a
    // config with `workspaces:` entries returned in the precedence branch
    // above, so no workspace crate can reach this point.
    if config.crates.len() > 1 {
        // A flat `crates:` list that ALL share one explicit tag prefix lives in
        // one tag namespace: `v0.2.0` cannot simultaneously be two crates'
        // independent tag, so the crates necessarily release in lockstep. Only
        // an EXPLICIT shared prefix collapses — a crate with no extractable
        // `tag_template` prefix (the per-crate `{crate}-v` fallback) is treated
        // as distinct, keeping genuinely independent crates per-crate.
        if shared_tag_prefix(&config.crates).is_some() {
            return RepoShape::FlatAggregate(config.crates.clone());
        }
        let groups: Vec<Vec<CrateConfig>> = config.crates.iter().map(|c| vec![c.clone()]).collect();
        return RepoShape::PerCrate(groups);
    }

    RepoShape::Single
}

/// The single tag prefix shared by EVERY crate in a flat `crates:` list, or
/// `None` when they do not all share one extractable prefix.
///
/// Returns `Some(prefix)` only when every crate's `tag_template` yields a
/// concrete prefix via [`git::extract_tag_prefix`] AND all those prefixes are
/// equal. A crate whose template has no extractable prefix (so tagging would
/// fall back to a per-crate `{crate}-v`) makes the set non-shared → `None`,
/// since two crates without an explicit common prefix are independent tracks.
fn shared_tag_prefix(crates: &[CrateConfig]) -> Option<String> {
    let mut iter = crates.iter();
    let first = git::extract_tag_prefix(&iter.next()?.tag_template)?;
    for c in iter {
        if git::extract_tag_prefix(&c.tag_template)? != first {
            return None;
        }
    }
    Some(first)
}

/// Reject an incoherent flat-aggregate config before any tag/changelog work.
///
/// A flat `crates:` list whose members share one tag prefix releases under ONE
/// shared tag (e.g. `v0.2.0`). If those members carry DIFFERENT
/// `[package].version` values, that single tag cannot carry two versions — the
/// config is impossible. Read each member's on-disk `[package].version` and
/// bail (listing every member's `crate → version` and the shared prefix, so an
/// N-way divergence is fully visible) when any two disagree; steer the user
/// toward lockstep (`[workspace.package].version`) or independent prefixes.
///
/// Only `RepoShape::FlatAggregate` can be incoherent this way, so any other
/// shape is a no-op. A member without a readable literal `[package].version`
/// (absent manifest, or a virtual / `version.workspace = true` manifest) is
/// skipped: the guard fires only on genuine version strings it can compare, so
/// a versionless member never trips the check nor masks a real divergence.
pub(crate) fn guard_flat_aggregate_coherence(
    config: Option<&anodizer_core::config::Config>,
    workspace: Option<&WorkspaceInfo>,
    workspace_root: &Path,
) -> Result<()> {
    let RepoShape::FlatAggregate(crates) = detect_repo_shape(workspace_root, config, workspace)
    else {
        return Ok(());
    };
    let prefix = shared_tag_prefix(&crates).unwrap_or_else(|| "v".to_string());
    // Read each member's literal `[package].version`, keyed by crate name. Skip
    // members with no readable literal version (no value to compare).
    let mut versions: Vec<(String, String)> = Vec::new();
    for c in &crates {
        let crate_dir = workspace_root.join(&c.path);
        if let Ok(Some(ver)) =
            anodizer_stage_build::version_sync::read_cargo_version_opt(&crate_dir.to_string_lossy())
        {
            versions.push((c.name.clone(), ver));
        }
    }
    let Some((_, first_ver)) = versions.first() else {
        return Ok(());
    };
    if versions.iter().any(|(_, v)| v != first_ver) {
        let listing = versions
            .iter()
            .map(|(name, ver)| format!("'{name}' ({ver})"))
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "crates {listing} share tag prefix '{prefix}' but set different [package].version \
             values; one tag can't carry two versions. For lockstep set \
             [workspace.package].version; for independent releases give each crate a distinct \
             tag_template prefix."
        );
    }
    Ok(())
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
    workspace_root: &Path,
    groups: &[Vec<CrateConfig>],
    opts: &TagOpts,
    cfg: &ResolvedConfig,
    git_config: Option<&GitConfig>,
    preloaded_config: Option<&anodizer_core::config::Config>,
    log: &StageLogger,
) -> Result<Vec<GroupTagResult>> {
    use crate::commands::release::detect_changed_crates_pub;

    // Use the already-loaded config when available to avoid a redundant disk
    // read; fall back to a fresh load, then to an empty default for fixture
    // repos that have no config file (e.g. integration-test temp dirs).
    let fallback: anodizer_core::config::Config;
    let anodizer_config: &anodizer_core::config::Config = if let Some(c) = preloaded_config {
        c
    } else {
        // A malformed explicitly-resolved config must fail, not silently
        // fall back to an empty default (the old `.ok().unwrap_or_default()`);
        // the default is reserved for fixture repos with genuinely no config.
        fallback = match resolve_config_path(opts) {
            Some(p) => crate::pipeline::load_config(&p)?,
            None => anodizer_core::config::Config::default(),
        };
        &fallback
    };

    // Run change detection across ALL crates so depends_on propagation works.
    let all_known: Vec<CrateConfig> = anodizer_config
        .crate_universe()
        .into_iter()
        .cloned()
        .collect();
    let changed_names = detect_changed_crates_pub(
        workspace_root,
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
            let msgs = get_messages_for_bump(
                workspace_root,
                cfg,
                prev_tag.as_deref(),
                Some(&crate_cfg.path),
            )
            .unwrap_or_default();
            all_messages.extend(msgs);
        }
        let bump = detect_bump_demoted(&all_messages, &group_cfg, prev_tag.as_deref());

        if bump == BumpKind::None {
            log.verbose(&format!(
                "skipped group {:?} — no bump signal",
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
            "group {:?}: {} → {}{}",
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

/// The per-crate engine's dispatched unit: the lockstep groups to tag plus
/// whether they are a single `FlatAggregate` (shared-prefix flat `crates:`
/// list). The flag drives the one-flat-section changelog collapse; it is
/// resolved once by [`detect_repo_shape`] so the collapse decision is never
/// re-derived from prefixes here.
struct PerCrateDispatch {
    groups: Vec<Vec<CrateConfig>>,
    is_flat_aggregate: bool,
    /// The config-derived workspace root threaded from `run` so the per-crate
    /// engine's git ops and cross-crate scans resolve the same root from a
    /// subdirectory as from the repo root.
    workspace_root: PathBuf,
}

fn run_per_crate_tag(
    dispatch: PerCrateDispatch,
    opts: &TagOpts,
    cfg: &ResolvedConfig,
    git_config: Option<&GitConfig>,
    anodizer_config: Option<&anodizer_core::config::Config>,
    controls: PushControls<'_>,
    log: &StageLogger,
) -> Result<()> {
    let PerCrateDispatch {
        groups,
        is_flat_aggregate,
        workspace_root,
    } = dispatch;
    let cwd = workspace_root.clone();
    let tag_results = compute_per_crate_tags(
        &workspace_root,
        &groups,
        opts,
        cfg,
        git_config,
        anodizer_config,
        log,
    )?;

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

    // Collapse the per-crate targets to ONE flat aggregate when this group is a
    // `FlatAggregate` (a flat `crates:` list whose members share one tag track,
    // bumped to N identically-prefixed tags) routed to one shared root: it is a
    // single lockstep release, not N multi-track subsections. Promoting each
    // member's section under the same `## [v<X.Y.Z>]` heading would strand every
    // member after the first and graft spurious `### <crate>` subsections — the
    // same bug the `changelog` command collapses. Genuine multi-track
    // (`PerCrate`) or per-crate files keep their per-crate targets.
    if collapse_targets_to_flat_aggregate(
        &mut changelog_targets,
        &cwd,
        anodizer_config,
        is_flat_aggregate && changelog_routing.root_enabled && !changelog_routing.per_crate,
    ) {
        changelog_routing.single_track = true;
    }

    // Genuine multi-track root: each target owns a `### <crate>` subsection.
    // Multi-track-ness is a property of the repo TOPOLOGY — more than one crate
    // routed to the shared root — not of how many tracks bump in this run. A
    // PerCrate release bumps one crate per tag, yet that crate's section still
    // belongs in the shared root as a tag-prefixed `## [<tag>]` section promoted
    // from its `### <crate>` subsection; gating on the per-run bump count would
    // drop every single-crate release to the flat roll (bare `## [<version>]`
    // headings that collide across tracks). Gate on the full root-routed crate
    // count from the one shared `config_root_crate_names` source — also the
    // crate-name set the renderer uses to bootstrap and classify subsections —
    // so this matches the refresh path and the two cannot diverge.
    changelog_routing.root_crate_names = anodizer_config
        .map(|cfg| {
            crate::commands::changelog_sync::config_root_crate_names(
                cfg,
                changelog_routing.root_crates,
            )
        })
        .unwrap_or_default();
    changelog_routing.multitrack = changelog_routing.root_enabled
        && !changelog_routing.single_track
        && changelog_routing.root_crate_names.len() > 1;

    if !opts.dry_run {
        // Apply version bumps across all changed crates in a single commit.
        // Crate paths are repo-root-relative; resolve each against the
        // discovered workspace root so the manifest IO matches the git working
        // dir even when `tag` runs from a subdirectory.
        for (path, new_version) in &all_version_updates {
            let abs_crate_dir = workspace_root.join(path).to_string_lossy().into_owned();
            anodizer_stage_build::version_sync::sync_version(
                &abs_crate_dir,
                new_version,
                false,
                log,
            )?;
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
        let workspace_root_str = workspace_root.to_string_lossy().into_owned();
        let mut intra_ws_modified: Vec<String> = Vec::new();
        for group_result in &tag_results {
            for (crate_name, (crate_path, new_version)) in group_result
                .crate_names
                .iter()
                .zip(group_result.version_updates.iter())
            {
                let abs_crate_dir = workspace_root
                    .join(crate_path)
                    .to_string_lossy()
                    .into_owned();
                let modified = anodizer_stage_build::version_sync::sync_workspace_deps(
                    &workspace_root_str,
                    &abs_crate_dir,
                    crate_name,
                    new_version,
                    false,
                    log,
                )?;
                intra_ws_modified.extend(modified);
            }
        }

        // Update Cargo.lock to match bumped manifests.
        match anodizer_core::cargo_lock::cargo_update_workspace(Some(workspace_root.as_path())) {
            Ok(_) => {}
            Err(e) => log.warn(&format!(
                "could not spawn `cargo update --workspace` ({e}); Cargo.lock may be stale"
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
                &workspace_root,
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
        git::stage_and_commit_in(
            &workspace_root,
            &staged_refs,
            &git::release_bump_subject(&bump_summary, skip_ci_suffix(cfg.skip_ci_on_bump)),
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
                &workspace_root,
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
///
/// Takes the command's single shared config load rather than re-loading:
/// every `load_config` re-emits the load-time legacy-alias warnings, so a
/// second load doubled them on the `--crate` path.
fn load_crate_tag_info(
    config: &anodizer_core::config::Config,
    crate_name: &str,
) -> Option<CrateTagInfo> {
    let crate_cfg = config.find_crate(crate_name)?;

    let tag_prefix = git::extract_tag_prefix(&crate_cfg.tag_template)?;
    let version_sync = crate_cfg
        .version_sync
        .as_ref()
        .and_then(|vs| vs.enabled)
        .unwrap_or(false);
    let version_files = resolve_version_files(Some(crate_cfg), Some(config));
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
    workspace_root: &Path,
    cfg: &ResolvedConfig,
    prev_tag: Option<&str>,
    path: Option<&str>,
) -> Result<Vec<String>> {
    match cfg.branch_history.as_str() {
        "last" => match path {
            Some(p) => git::get_last_commit_messages_path_in(workspace_root, 1, p),
            None => git::get_last_commit_messages_in(workspace_root, 1),
        },
        "full" | "compare" => match (prev_tag, path) {
            (Some(tag), Some(p)) => {
                git::get_commit_messages_between_path_in(workspace_root, tag, "HEAD", p)
            }
            (Some(tag), None) => git::get_commit_messages_between_in(workspace_root, tag, "HEAD"),
            (None, Some(p)) => git::get_last_commit_messages_path_in(workspace_root, 500, p),
            (None, None) => git::get_last_commit_messages_in(workspace_root, 500),
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

/// Detect the bump, then apply pre-major demotion for an inferred bump.
///
/// A bump driven by an explicit `#major`/`#minor`/`#patch` token is operator
/// intent and is returned untouched (see [`has_explicit_bump_token`]). A bump
/// derived from the conventional-commit layer or the `default_bump` fallback is
/// subject to [`demote_pre_major`] when the governing major (from `prev_tag`, or
/// `0` when there is no prior tag) is still `0`. The demotion is computed here,
/// once, so the lockstep-workspace and per-crate tagging paths share it.
fn detect_bump_demoted(
    messages: &[String],
    cfg: &ResolvedConfig,
    prev_tag: Option<&str>,
) -> BumpKind {
    let bump = detect_bump(messages, cfg);
    if has_explicit_bump_token(messages, cfg) {
        return bump;
    }
    let base_major = prev_tag
        .and_then(|t| git::parse_semver_tag(t).ok())
        .map_or(0, |sv| sv.major);
    demote_pre_major(
        bump,
        base_major,
        cfg.bump_minor_pre_major,
        cfg.bump_patch_for_minor_pre_major,
    )
}

/// Whole-word (not substring) token match: a token counts only when it appears
/// as a standalone whitespace-separated word, so a `#none` in prose
/// (`"revert the #none commit"`) or a word like `#handsome` does not trigger it.
/// Shared by [`has_explicit_bump_token`] and [`detect_bump_from_tokens`] so the
/// two never drift on what "a token is present" means — their agreement is a
/// correctness invariant (the explicit-token layer and the gate must see the
/// same tokens).
fn message_has_token(msg: &str, token: &str) -> bool {
    msg.split(|c: char| c.is_whitespace()).any(|w| w == token)
}

/// Whether a message carries a standalone `#major`/`#minor`/`#patch` token that
/// *drives* the bump. Those three are matched ahead of the conventional-commit
/// layer in [`detect_bump_from_tokens`], so their presence always determines the
/// result — the bump is explicit operator intent that pre-major demotion must
/// not touch. `#none` is excluded on purpose: it is the lowest-priority token (a
/// conventional marker overrides it), so a `#none` sharing a range with a
/// `feat!:` does NOT drive the bump and must not block that breaking change's
/// demotion; a `#none` that does win already yields `BumpKind::None`, which
/// demotion passes through untouched. Whole-word match mirrors
/// [`detect_bump_from_tokens`] (a token embedded in prose does not count).
fn has_explicit_bump_token(messages: &[String], cfg: &ResolvedConfig) -> bool {
    let has = |token: &str| messages.iter().any(|m| message_has_token(m, token));
    has(&cfg.major_string_token) || has(&cfg.minor_string_token) || has(&cfg.patch_string_token)
}

/// SemVer "major version zero" demotion (release-please's `bump-minor-pre-major`
/// / `bump-patch-for-minor-pre-major`). While `base_major == 0` the public API
/// is unstable, so an inferred breaking change need not force `1.0.0` and an
/// inferred feature need not force a minor.
///
/// - `bump_minor_pre_major`: [`BumpKind::Major`] → [`BumpKind::Minor`]
/// - `bump_patch_for_minor_pre_major`: [`BumpKind::Minor`] → [`BumpKind::Patch`]
///
/// The two axes are independent (a breaking change is governed by the first,
/// a feature by the second — no cascade). Once `base_major >= 1` the project
/// has committed to a stable API, so both toggles are inert.
fn demote_pre_major(
    bump: BumpKind,
    base_major: u64,
    bump_minor_pre_major: bool,
    bump_patch_for_minor_pre_major: bool,
) -> BumpKind {
    if base_major != 0 {
        return bump;
    }
    match bump {
        BumpKind::Major if bump_minor_pre_major => BumpKind::Minor,
        BumpKind::Minor if bump_patch_for_minor_pre_major => BumpKind::Patch,
        other => other,
    }
}

/// Core bump detection logic, separated for unit testing without needing the full config.
///
/// Resolution order (highest precedence first):
/// 1. Explicit bump tokens `#major` > `#minor` > `#patch` — operator intent,
///    always wins. `#none` is deliberately NOT in this layer.
/// 2. Conventional-commit markers when no `#major`/`#minor`/`#patch` matched —
///    a line containing `BREAKING CHANGE` or a `<type>!:` shorthand → major,
///    `feat:` → minor, `fix:` / `perf:` / `revert:` → patch. A message that
///    starts with `chore:` / `docs:` / `style:` / `refactor:` / `test:` /
///    `build:` / `ci:` is NOT release-worthy, so it contributes nothing. A
///    release-worthy marker beats `#none` (an explicit release signal
///    overrides the veto).
/// 3. `#none` — vetoes the `default_bump` fallback, so a range whose only
///    signal is `#none` skips the release.
/// 4. `default_bump` fallback when nothing above matched (default `none`:
///    chore-only ranges no-op; set `patch`/`minor` to release every range).
fn detect_bump_from_tokens(
    messages: &[String],
    major_token: &str,
    minor_token: &str,
    patch_token: &str,
    none_token: &str,
    default_bump: &str,
) -> BumpKind {
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
        // An unrecognized default_bump value fails safe to no release rather
        // than a surprise bump (the unset default is "none" — see line above).
        _ => BumpKind::None,
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
/// Per-commit classification delegates to the shared
/// [`anodizer_core::git::classify_commit`] rules (the same classifier
/// `anodizer bump` infers from, so a `bump --dry-run` preview and the
/// auto-tag cut can never disagree); the strongest signal in the range wins.
fn detect_conventional_bump(messages: &[String]) -> Option<BumpKind> {
    use anodizer_core::git::ConventionalLevel;
    messages
        .iter()
        .filter_map(|msg| anodizer_core::git::classify_commit(msg))
        .max()
        .map(|level| match level {
            ConventionalLevel::Major => BumpKind::Major,
            ConventionalLevel::Minor => BumpKind::Minor,
            ConventionalLevel::Patch => BumpKind::Patch,
        })
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
/// Enrolled paths are repo-root-relative; each is resolved against `root` (the
/// discovered workspace root) for the read/write so the rewrite hits the same
/// files git operates on even when `tag` is invoked from a subdirectory. The
/// logged and returned paths stay repo-relative so staging via
/// [`git::stage_and_commit_in`] (rooted at the same `root`) matches.
///
/// A file with zero matches is reported via `warn` but is not an error: a stale
/// enrollment should surface loudly without aborting the tag. When `dry_run` is
/// set, counts are logged but no file is written and no path is returned for
/// staging. A no-op (`old == new`) returns immediately.
fn rewrite_and_stage_version_files(
    root: &Path,
    files: &[String],
    old: &str,
    new: &str,
    dry_run: bool,
    log: &StageLogger,
) -> Result<Vec<String>> {
    if files.is_empty() || old == new {
        return Ok(Vec::new());
    }
    let resolved: Vec<String> = files
        .iter()
        .map(|f| root.join(f).to_string_lossy().into_owned())
        .collect();
    let outcomes =
        anodizer_core::version_files::rewrite_version_in_files(&resolved, old, new, dry_run)?;
    let mut changed = Vec::new();
    for (outcome, rel) in outcomes.iter().zip(files.iter()) {
        if outcome.replacements > 0 {
            log.status(&format!(
                "{}rewrote {} occurrence(s) of {} → {} in {}",
                if dry_run { "(dry-run) " } else { "" },
                outcome.replacements,
                old,
                new,
                rel
            ));
            if !dry_run {
                changed.push(rel.clone());
            }
        } else {
            log.warn(&format!(
                "enrolled version_files entry {} did not contain version {} (nothing rewritten)",
                rel, old
            ));
        }
    }
    Ok(changed)
}

/// One planned `version_files` rewrite: rewrite `old` → `new` in `file`.
#[derive(Debug, PartialEq)]
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

/// Collapse `targets` in place to ONE flat whole-workspace aggregate when
/// `collapse` is set (the caller has resolved a `FlatAggregate` shape routed to
/// one shared root). Returns `true` when collapsed (the caller then sets the
/// routing's `single_track`), `false` otherwise (`targets` left untouched).
///
/// The flat-aggregate DECISION lives in [`detect_repo_shape`] (via
/// [`shared_tag_prefix`]); this helper only applies it, so the prefix-equality
/// comparison is not re-derived here.
///
/// The aggregate spans the workspace (`crate_dir = workspace_root`), keyed by
/// `project_name`, with the shared `from_tag` / `full_tag` every member already
/// carries (identical across a lockstep set).
fn collapse_targets_to_flat_aggregate(
    targets: &mut Vec<ChangelogTarget>,
    workspace_root: &Path,
    config: Option<&anodizer_core::config::Config>,
    collapse: bool,
) -> bool {
    if !collapse || targets.len() <= 1 {
        return false;
    }
    let Some(config) = config else {
        return false;
    };
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
            version_override: None,
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

    // ---- pre-major demotion tests ----

    /// `demote_pre_major` only touches an inferred Major/Minor while the
    /// governing major is `0`, and the two axes never cascade.
    #[test]
    fn demote_pre_major_axes() {
        // major == 0: each flag governs its own axis.
        assert_eq!(
            demote_pre_major(BumpKind::Major, 0, true, false),
            BumpKind::Minor
        );
        assert_eq!(
            demote_pre_major(BumpKind::Major, 0, false, false),
            BumpKind::Major
        );
        assert_eq!(
            demote_pre_major(BumpKind::Minor, 0, false, true),
            BumpKind::Patch
        );
        assert_eq!(
            demote_pre_major(BumpKind::Minor, 0, false, false),
            BumpKind::Minor
        );
        // Both flags on: breaking → minor (NOT cascaded to patch), feat → patch.
        assert_eq!(
            demote_pre_major(BumpKind::Major, 0, true, true),
            BumpKind::Minor
        );
        assert_eq!(
            demote_pre_major(BumpKind::Minor, 0, true, true),
            BumpKind::Patch
        );
        // Patch / None are never demoted.
        assert_eq!(
            demote_pre_major(BumpKind::Patch, 0, true, true),
            BumpKind::Patch
        );
        assert_eq!(
            demote_pre_major(BumpKind::None, 0, true, true),
            BumpKind::None
        );
    }

    /// Once a real tag reaches `1.x`, both toggles are inert.
    #[test]
    fn demote_pre_major_inert_at_one() {
        assert_eq!(
            demote_pre_major(BumpKind::Major, 1, true, true),
            BumpKind::Major
        );
        assert_eq!(
            demote_pre_major(BumpKind::Minor, 1, true, true),
            BumpKind::Minor
        );
        assert_eq!(
            demote_pre_major(BumpKind::Major, 2, true, false),
            BumpKind::Major
        );
    }

    fn cfg_with_pre_major(minor_pre_major: bool, patch_for_minor: bool) -> ResolvedConfig {
        let tag_cfg = TagConfig {
            bump_minor_pre_major: Some(minor_pre_major),
            bump_patch_for_minor_pre_major: Some(patch_for_minor),
            ..Default::default()
        };
        ResolvedConfig::from_tag_config(&tag_cfg, &push_opts(false, false))
    }

    #[test]
    fn has_explicit_bump_token_whole_word_only() {
        let cfg = cfg_with_pre_major(true, false);
        assert!(has_explicit_bump_token(
            &["chore: x #minor".to_string()],
            &cfg
        ));
        assert!(has_explicit_bump_token(
            &["release #major".to_string()],
            &cfg
        ));
        // Conventional-only ranges carry no token.
        assert!(!has_explicit_bump_token(
            &["feat!: break".to_string()],
            &cfg
        ));
        // A token embedded in a larger word is not a token.
        assert!(!has_explicit_bump_token(
            &["fix #minorbug".to_string()],
            &cfg
        ));
    }

    /// End-to-end precedence: an explicit token always wins; an inferred
    /// breaking change demotes only while pre-1.0 and only when the flag is on.
    #[test]
    fn detect_bump_demoted_precedence() {
        // feat! with bump_minor_pre_major on, base 0.x → Minor.
        assert_eq!(
            detect_bump_demoted(
                &["feat!: break".to_string()],
                &cfg_with_pre_major(true, false),
                Some("v0.5.0")
            ),
            BumpKind::Minor
        );
        // Same input, flag off → Major (consensus default).
        assert_eq!(
            detect_bump_demoted(
                &["feat!: break".to_string()],
                &cfg_with_pre_major(false, false),
                Some("v0.5.0")
            ),
            BumpKind::Major
        );
        // Explicit #major token wins over demotion even with the flag on.
        assert_eq!(
            detect_bump_demoted(
                &["feat!: break".to_string(), "stabilize #major".to_string()],
                &cfg_with_pre_major(true, false),
                Some("v0.5.0"),
            ),
            BumpKind::Major
        );
        // Inert once the base tag is 1.x.
        assert_eq!(
            detect_bump_demoted(
                &["feat!: break".to_string()],
                &cfg_with_pre_major(true, false),
                Some("v1.2.0")
            ),
            BumpKind::Major
        );
        // No prior tag is treated as pre-major (base major 0).
        assert_eq!(
            detect_bump_demoted(
                &["feat!: break".to_string()],
                &cfg_with_pre_major(true, false),
                None
            ),
            BumpKind::Minor
        );
        // bump_patch_for_minor_pre_major: a plain feat demotes to patch pre-1.0.
        assert_eq!(
            detect_bump_demoted(
                &["feat: thing".to_string()],
                &cfg_with_pre_major(false, true),
                Some("v0.5.0")
            ),
            BumpKind::Patch
        );
    }

    /// A `#none` token is overridden by a conventional marker in the same range,
    /// so it must NOT suppress that breaking change's pre-major demotion.
    #[test]
    fn detect_bump_demoted_none_token_does_not_block_demotion() {
        // #none loses to feat!: -> the breaking change still demotes to Minor.
        assert_eq!(
            detect_bump_demoted(
                &["feat!: break #none".to_string()],
                &cfg_with_pre_major(true, false),
                Some("v0.5.0")
            ),
            BumpKind::Minor
        );
        // A standalone #none (no conventional marker) still skips the bump.
        assert_eq!(
            detect_bump_demoted(
                &["chore: housekeeping #none".to_string()],
                &cfg_with_pre_major(true, false),
                Some("v0.5.0")
            ),
            BumpKind::None
        );
    }

    /// `has_explicit_bump_token` resolves the SAME configurable tokens as
    /// `detect_bump_from_tokens`, so a custom major token still wins over
    /// demotion under non-default token config.
    #[test]
    fn detect_bump_demoted_honors_custom_tokens() {
        let tag_cfg = TagConfig {
            major_string_token: Some("#breaking".to_string()),
            bump_minor_pre_major: Some(true),
            ..Default::default()
        };
        let cfg = ResolvedConfig::from_tag_config(&tag_cfg, &push_opts(false, false));
        // Custom #breaking token drives Major and is not demoted.
        assert_eq!(
            detect_bump_demoted(&["rework #breaking".to_string()], &cfg, Some("v0.5.0")),
            BumpKind::Major
        );
        // A conventional feat!: (no custom token) still demotes.
        assert_eq!(
            detect_bump_demoted(&["feat!: rework".to_string()], &cfg, Some("v0.5.0")),
            BumpKind::Minor
        );
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
            version_override: None,
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
        assert_eq!(resolved.default_bump, "none");
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
            version_override: None,
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
            bump_minor_pre_major: None,
            bump_patch_for_minor_pre_major: None,
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
            version_override: None,
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

    /// A workspace root with no `Cargo.toml`, so `load_workspace` returns `Err`
    /// and the Cargo lockstep signal stays absent. Pinning the root explicitly
    /// (instead of `detect_repo_shape` reading the runner's cwd) keeps each
    /// shape assertion hermetic — run from the anodizer workspace root it would
    /// otherwise flip to `Lockstep` off the real `[workspace.package].version`.
    fn empty_root() -> tempfile::TempDir {
        tempfile::tempdir().expect("create temp workspace root")
    }

    #[test]
    fn detect_repo_shape_no_config_no_workspace_returns_single() {
        // Bare repo: no anodizer config, no Cargo workspace info → Single.
        let root = empty_root();
        let shape = detect_repo_shape(root.path(), None, None);
        assert!(matches!(shape, RepoShape::Single));
    }

    #[test]
    fn detect_repo_shape_single_crate_config_returns_single() {
        let config = anodizer_core::config::Config {
            project_name: "app".to_string(),
            crates: vec![crate_cfg("app", ".", "v{{ .Version }}")],
            ..Default::default()
        };
        let root = empty_root();
        let shape = detect_repo_shape(root.path(), Some(&config), None);
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
        let root = empty_root();
        let shape = detect_repo_shape(root.path(), Some(&config), Some(&ws));
        assert!(matches!(shape, RepoShape::Lockstep));
    }

    #[test]
    fn detect_repo_shape_mixed_config_keeps_top_level_crates_as_tracks() {
        // Top-level `crates:` alongside `workspaces:`: the workspace group
        // stays intact and each top-level crate not in any group becomes
        // its own singleton track — never silently dropped from tag
        // dispatch. A top-level duplicate of a group member stays with its
        // group (no double dispatch).
        let config = anodizer_core::config::Config {
            project_name: "ws".to_string(),
            crates: vec![
                crate_cfg("root", ".", "root-v{{ .Version }}"),
                crate_cfg("member", "crates/member", "member-v{{ .Version }}"),
            ],
            workspaces: Some(vec![anodizer_core::config::WorkspaceConfig {
                name: "grp".to_string(),
                crates: vec![
                    crate_cfg("member", "crates/member", "member-v{{ .Version }}"),
                    crate_cfg("sibling", "crates/sibling", "sibling-v{{ .Version }}"),
                ],
                ..Default::default()
            }]),
            ..Default::default()
        };
        let root = empty_root();
        let shape = detect_repo_shape(root.path(), Some(&config), None);
        match shape {
            RepoShape::PerCrate(groups) => {
                let names: Vec<Vec<&str>> = groups
                    .iter()
                    .map(|g| g.iter().map(|c| c.name.as_str()).collect())
                    .collect();
                assert_eq!(names, vec![vec!["member", "sibling"], vec!["root"]]);
            }
            other => panic!(
                "expected PerCrate, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
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
        let root = empty_root();
        let shape = detect_repo_shape(root.path(), Some(&config), None);
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
        let root = empty_root();
        let shape = detect_repo_shape(root.path(), Some(&config), None);
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
        let root = empty_root();
        let shape = detect_repo_shape(root.path(), Some(&config), Some(&ws));
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
        let root = empty_root();
        let shape = detect_repo_shape(root.path(), Some(&config), None);
        assert!(matches!(shape, RepoShape::Single));
    }

    /// A `WorkspaceInfo` with no `[workspace.package].version`, so the Cargo
    /// signal does NOT force `Lockstep` — the prefix axis decides. Passed
    /// explicitly so the result is hermetic regardless of the test's cwd.
    fn ws_no_lockstep() -> WorkspaceInfo {
        WorkspaceInfo {
            workspace_package_version: None,
            members: vec![],
        }
    }

    #[test]
    fn detect_repo_shape_same_prefix_flat_crates_returns_flat_aggregate() {
        // ≥2 flat crates all on `v{{ Version }}` with no workspace version:
        // one shared tag prefix is one shared tag namespace, so they release in
        // lockstep — `v0.2.0` cannot be two crates' independent tag — but each
        // carries its own `[package].version`, so the shape is `FlatAggregate`
        // (bumped by N per-crate manifests), not genuine `Lockstep`.
        let config = anodizer_core::config::Config {
            project_name: "ws".to_string(),
            crates: vec![
                crate_cfg("core", "crates/core", "v{{ .Version }}"),
                crate_cfg("cli", "crates/cli", "v{{ .Version }}"),
            ],
            ..Default::default()
        };
        let root = empty_root();
        let shape = detect_repo_shape(root.path(), Some(&config), Some(&ws_no_lockstep()));
        match shape {
            RepoShape::FlatAggregate(crates) => {
                assert_eq!(crates.len(), 2, "carries the flat crate list");
                assert_eq!(crates[0].name, "core");
                assert_eq!(crates[1].name, "cli");
            }
            other => panic!(
                "same-prefix flat crates must classify as FlatAggregate, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn detect_repo_shape_distinct_prefix_flat_crates_returns_per_crate() {
        // Distinct prefixes (`core-v*` + `v*`) are independent tracks → PerCrate.
        let config = anodizer_core::config::Config {
            project_name: "ws".to_string(),
            crates: vec![
                crate_cfg("core", "crates/core", "core-v{{ .Version }}"),
                crate_cfg("cli", "crates/cli", "v{{ .Version }}"),
            ],
            ..Default::default()
        };
        let root = empty_root();
        let shape = detect_repo_shape(root.path(), Some(&config), Some(&ws_no_lockstep()));
        match shape {
            RepoShape::PerCrate(groups) => assert_eq!(groups.len(), 2),
            other => panic!(
                "expected PerCrate for distinct prefixes, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn detect_repo_shape_no_tag_template_flat_crates_returns_per_crate() {
        // No `tag_template` → no extractable shared prefix (each would fall back
        // to a per-crate `{crate}-v`), so the crates stay distinct → PerCrate.
        let config = anodizer_core::config::Config {
            project_name: "ws".to_string(),
            crates: vec![
                crate_cfg("core", "crates/core", ""),
                crate_cfg("cli", "crates/cli", ""),
            ],
            ..Default::default()
        };
        let root = empty_root();
        let shape = detect_repo_shape(root.path(), Some(&config), Some(&ws_no_lockstep()));
        match shape {
            RepoShape::PerCrate(groups) => assert_eq!(groups.len(), 2),
            other => panic!(
                "expected PerCrate when no tag_template yields a shared prefix, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn detect_repo_shape_explicit_workspaces_shared_prefix_still_per_crate() {
        // An explicit `workspaces:` block is operator intent and wins at step 1,
        // even when its crates coincidentally share one tag prefix — the
        // same-prefix → FlatAggregate collapse applies ONLY to inferred flat
        // `crates:`.
        let ws1 = anodizer_core::config::WorkspaceConfig {
            name: "group-a".to_string(),
            crates: vec![crate_cfg("a", "crates/a", "v{{ .Version }}")],
            ..Default::default()
        };
        let ws2 = anodizer_core::config::WorkspaceConfig {
            name: "group-b".to_string(),
            crates: vec![crate_cfg("b", "crates/b", "v{{ .Version }}")],
            ..Default::default()
        };
        let config = anodizer_core::config::Config {
            project_name: "p".to_string(),
            workspaces: Some(vec![ws1, ws2]),
            ..Default::default()
        };
        let root = empty_root();
        let shape = detect_repo_shape(root.path(), Some(&config), Some(&ws_no_lockstep()));
        match shape {
            RepoShape::PerCrate(groups) => assert_eq!(groups.len(), 2),
            other => panic!(
                "explicit workspaces: must stay PerCrate despite a shared prefix, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    // ---- flat-aggregate coherence guard tests ----

    /// Write a two-crate flat workspace whose members share `v{{ Version }}` but
    /// carry the supplied `[package].version` values, returning the root dir.
    fn flat_aggregate_versions_fixture(
        core_ver: &str,
        cli_ver: &str,
    ) -> (tempfile::TempDir, anodizer_core::config::Config) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for (name, ver) in [("core", core_ver), ("cli", cli_ver)] {
            let dir = root.join(format!("crates/{name}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nversion = \"{ver}\"\n"),
            )
            .unwrap();
        }
        let config = anodizer_core::config::Config {
            project_name: "agg".to_string(),
            crates: vec![
                crate_cfg("core", "crates/core", "v{{ .Version }}"),
                crate_cfg("cli", "crates/cli", "v{{ .Version }}"),
            ],
            ..Default::default()
        };
        (tmp, config)
    }

    #[test]
    fn coherence_guard_passes_when_versions_agree() {
        let (tmp, config) = flat_aggregate_versions_fixture("0.2.0", "0.2.0");
        let res =
            guard_flat_aggregate_coherence(Some(&config), Some(&ws_no_lockstep()), tmp.path());
        assert!(res.is_ok(), "all-agree flat aggregate must pass: {res:?}");
    }

    #[test]
    fn coherence_guard_rejects_divergent_versions() {
        let (tmp, config) = flat_aggregate_versions_fixture("0.5.0", "0.1.0");
        let err =
            guard_flat_aggregate_coherence(Some(&config), Some(&ws_no_lockstep()), tmp.path())
                .unwrap_err()
                .to_string();
        assert!(err.contains("core"), "names conflicting crate core: {err}");
        assert!(err.contains("cli"), "names conflicting crate cli: {err}");
        assert!(err.contains("0.5.0") && err.contains("0.1.0"), "{err}");
        assert!(err.contains("prefix 'v'"), "names the shared prefix: {err}");
        assert!(
            err.contains("[workspace.package].version"),
            "steers toward lockstep: {err}"
        );
        assert!(
            err.contains("distinct tag_template prefix"),
            "steers toward independent prefixes: {err}"
        );
    }

    /// A missing member manifest is skipped, not errored: the guard fires only
    /// on versions it can actually read.
    #[test]
    fn coherence_guard_skips_missing_manifests() {
        let tmp = tempfile::tempdir().unwrap();
        let config = anodizer_core::config::Config {
            project_name: "agg".to_string(),
            crates: vec![
                crate_cfg("core", "crates/core", "v{{ .Version }}"),
                crate_cfg("cli", "crates/cli", "v{{ .Version }}"),
            ],
            ..Default::default()
        };
        // No Cargo.toml on disk → every member skipped → no versions to compare.
        let res =
            guard_flat_aggregate_coherence(Some(&config), Some(&ws_no_lockstep()), tmp.path());
        assert!(res.is_ok(), "missing manifests must be skipped: {res:?}");
    }

    /// Non-flat-aggregate shapes (here a distinct-prefix `PerCrate`) are a no-op
    /// even with divergent versions — one tag never spans both crates.
    #[test]
    fn coherence_guard_noop_for_non_flat_aggregate() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for (name, ver) in [("core", "0.5.0"), ("cli", "0.1.0")] {
            let dir = root.join(format!("crates/{name}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nversion = \"{ver}\"\n"),
            )
            .unwrap();
        }
        let config = anodizer_core::config::Config {
            project_name: "p".to_string(),
            crates: vec![
                crate_cfg("core", "crates/core", "core-v{{ .Version }}"),
                crate_cfg("cli", "crates/cli", "cli-v{{ .Version }}"),
            ],
            ..Default::default()
        };
        let res = guard_flat_aggregate_coherence(Some(&config), Some(&ws_no_lockstep()), root);
        assert!(
            res.is_ok(),
            "distinct-prefix PerCrate is not guarded: {res:?}"
        );
    }

    /// A member whose manifest is PRESENT but carries no literal
    /// `[package].version` (a virtual / workspace-inheriting manifest) must be
    /// skipped, not compared as a `0.0.0` sentinel: it neither trips the guard
    /// against a real sibling nor masks a real divergence.
    #[test]
    fn coherence_guard_skips_versionless_member() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // `core` carries a real version; `cli` declares no `[package].version`.
        std::fs::create_dir_all(root.join("crates/core")).unwrap();
        std::fs::write(
            root.join("crates/core/Cargo.toml"),
            "[package]\nname = \"core\"\nversion = \"0.2.0\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("crates/cli")).unwrap();
        std::fs::write(
            root.join("crates/cli/Cargo.toml"),
            "[package]\nname = \"cli\"\nversion.workspace = true\n",
        )
        .unwrap();
        let config = anodizer_core::config::Config {
            project_name: "agg".to_string(),
            crates: vec![
                crate_cfg("core", "crates/core", "v{{ .Version }}"),
                crate_cfg("cli", "crates/cli", "v{{ .Version }}"),
            ],
            ..Default::default()
        };
        let res = guard_flat_aggregate_coherence(Some(&config), Some(&ws_no_lockstep()), root);
        assert!(
            res.is_ok(),
            "versionless member must be skipped, not compared as 0.0.0: {res:?}"
        );
    }

    /// A 3-way divergence names EVERY member (not just the first conflicting
    /// pair), so a `[0.2.0, 0.2.0, 0.5.0]` split is fully visible.
    #[test]
    fn coherence_guard_lists_all_members_on_n_way_divergence() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for (name, ver) in [("a", "0.2.0"), ("b", "0.2.0"), ("c", "0.5.0")] {
            let dir = root.join(format!("crates/{name}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nversion = \"{ver}\"\n"),
            )
            .unwrap();
        }
        let config = anodizer_core::config::Config {
            project_name: "agg".to_string(),
            crates: vec![
                crate_cfg("a", "crates/a", "v{{ .Version }}"),
                crate_cfg("b", "crates/b", "v{{ .Version }}"),
                crate_cfg("c", "crates/c", "v{{ .Version }}"),
            ],
            ..Default::default()
        };
        let err = guard_flat_aggregate_coherence(Some(&config), Some(&ws_no_lockstep()), root)
            .unwrap_err()
            .to_string();
        // All three members appear with their versions — including the two that
        // agree (`a`/`b`), which a first-pair-only message would have dropped.
        assert!(err.contains("'a' (0.2.0)"), "lists member a: {err}");
        assert!(err.contains("'b' (0.2.0)"), "lists member b: {err}");
        assert!(err.contains("'c' (0.5.0)"), "lists member c: {err}");
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

    // -----------------------------------------------------------------------
    // skip_ci_suffix
    // -----------------------------------------------------------------------

    #[test]
    fn skip_ci_suffix_on_appends_marker_with_leading_space() {
        assert_eq!(skip_ci_suffix(true), " [skip ci]");
    }

    #[test]
    fn skip_ci_suffix_off_is_empty() {
        assert_eq!(skip_ci_suffix(false), "");
    }

    // -----------------------------------------------------------------------
    // shared_tag_prefix
    // -----------------------------------------------------------------------

    #[test]
    fn shared_tag_prefix_uniform_prefix_returns_it() {
        let crates = vec![
            crate_cfg("a", "crates/a", "v{{ .Version }}"),
            crate_cfg("b", "crates/b", "v{{ .Version }}"),
        ];
        assert_eq!(shared_tag_prefix(&crates), Some("v".to_string()));
    }

    #[test]
    fn shared_tag_prefix_divergent_prefixes_returns_none() {
        let crates = vec![
            crate_cfg("a", "crates/a", "a-v{{ .Version }}"),
            crate_cfg("b", "crates/b", "b-v{{ .Version }}"),
        ];
        assert_eq!(shared_tag_prefix(&crates), None);
    }

    #[test]
    fn shared_tag_prefix_single_crate_returns_its_prefix() {
        let crates = vec![crate_cfg("core", "crates/core", "core-v{{ .Version }}")];
        assert_eq!(shared_tag_prefix(&crates), Some("core-v".to_string()));
    }

    #[test]
    fn shared_tag_prefix_empty_slice_returns_none() {
        assert_eq!(shared_tag_prefix(&[]), None);
    }

    // -----------------------------------------------------------------------
    // bare_version_from_tag
    // -----------------------------------------------------------------------

    #[test]
    fn bare_version_from_tag_strips_v_prefix() {
        assert_eq!(bare_version_from_tag("v1.2.3"), Some("1.2.3".to_string()));
    }

    #[test]
    fn bare_version_from_tag_keeps_prerelease() {
        assert_eq!(
            bare_version_from_tag("v0.4.0-beta.1"),
            Some("0.4.0-beta.1".to_string())
        );
    }

    #[test]
    fn bare_version_from_tag_handles_monorepo_prefix() {
        assert_eq!(
            bare_version_from_tag("core-v2.0.1"),
            Some("2.0.1".to_string())
        );
    }

    #[test]
    fn bare_version_from_tag_empty_is_none() {
        assert_eq!(bare_version_from_tag(""), None);
    }

    #[test]
    fn bare_version_from_tag_non_semver_is_none() {
        assert_eq!(bare_version_from_tag("not-a-version"), None);
    }

    // -----------------------------------------------------------------------
    // message_has_token (whole-word, not substring)
    // -----------------------------------------------------------------------

    #[test]
    fn message_has_token_matches_standalone_word() {
        assert!(message_has_token("fix: a bug #patch", "#patch"));
    }

    #[test]
    fn message_has_token_rejects_substring_within_word() {
        assert!(!message_has_token("this is #handsome", "#hand"));
        assert!(!message_has_token("#patches galore", "#patch"));
    }

    #[test]
    fn message_has_token_matches_token_anywhere_in_whitespace_split() {
        assert!(message_has_token(
            "subject\nbody line #major footer",
            "#major"
        ));
    }

    // -----------------------------------------------------------------------
    // detect_conventional_bump
    // -----------------------------------------------------------------------

    #[test]
    fn detect_conventional_bump_feat_is_minor() {
        let msgs = vec!["feat: add thing".to_string()];
        assert_eq!(detect_conventional_bump(&msgs), Some(BumpKind::Minor));
    }

    #[test]
    fn detect_conventional_bump_fix_is_patch() {
        let msgs = vec!["fix(core): correct it".to_string()];
        assert_eq!(detect_conventional_bump(&msgs), Some(BumpKind::Patch));
    }

    #[test]
    fn detect_conventional_bump_breaking_shorthand_is_major() {
        let msgs = vec!["feat!: drop old API".to_string()];
        assert_eq!(detect_conventional_bump(&msgs), Some(BumpKind::Major));
    }

    #[test]
    fn detect_conventional_bump_chore_only_is_none() {
        let msgs = vec!["chore: bump deps".to_string(), "docs: tweak".to_string()];
        assert_eq!(detect_conventional_bump(&msgs), None);
    }

    #[test]
    fn detect_conventional_bump_major_wins_over_minor_and_patch() {
        let msgs = vec![
            "fix: x".to_string(),
            "feat: y".to_string(),
            "refactor!: z".to_string(),
        ];
        assert_eq!(detect_conventional_bump(&msgs), Some(BumpKind::Major));
    }

    /// The same commit corpus must classify identically through the `tag`
    /// consumer (`detect_conventional_bump`, feeding the auto-tag precedence
    /// layers) and the `bump` consumer (`inference::classify`, feeding the
    /// dry-run plan) — the two commands previewing/cutting different releases
    /// from the same range is the drift this pins against.
    #[test]
    fn conventional_classification_is_lockstep_between_tag_and_bump() {
        use crate::commands::bump::inference;
        use crate::commands::bump::plan::BumpLevel;

        let corpus: &[(&[&str], Option<BumpKind>)] = &[
            (&["revert: undo broken feature"], Some(BumpKind::Patch)),
            (
                &["feat: x\n\nBREAKING CHANGE removed the old endpoint"],
                Some(BumpKind::Major),
            ),
            (
                &["fix: y\n\nBREAKING-CHANGE: dropped the flag"],
                Some(BumpKind::Major),
            ),
            (&["feat!: drop legacy auth"], Some(BumpKind::Major)),
            (&["feat(core)!: rewrite pipeline"], Some(BumpKind::Major)),
            (&["refactor!: drop the shim"], Some(BumpKind::Major)),
            (&["feat: new stage"], Some(BumpKind::Minor)),
            (&["feat(build): add cache key"], Some(BumpKind::Minor)),
            (&["fix: race"], Some(BumpKind::Patch)),
            (&["perf: faster loop"], Some(BumpKind::Patch)),
            (&["feat(broken: unclosed scope"], Some(BumpKind::Minor)),
            (&["chore: deps", "docs: tweak"], None),
            (&["random subject"], None),
            (&["fix: a", "feat: b", "chore: c"], Some(BumpKind::Minor)),
            (&["fix: a", "feat!: b"], Some(BumpKind::Major)),
        ];

        for (msgs, expected_tag) in corpus {
            let msgs: Vec<String> = msgs.iter().map(|s| s.to_string()).collect();
            assert_eq!(
                detect_conventional_bump(&msgs),
                *expected_tag,
                "tag-side classification for {msgs:?}"
            );
            let expected_bump = match expected_tag {
                Some(BumpKind::Major) => BumpLevel::Major,
                Some(BumpKind::Minor) => BumpLevel::Minor,
                Some(BumpKind::Patch) => BumpLevel::Patch,
                Some(BumpKind::None) | None => BumpLevel::Skip,
            };
            let (level, _) = inference::classify(&msgs);
            assert_eq!(
                level, expected_bump,
                "bump-side classification for {msgs:?}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // plan_changelog_targets / collapse_targets_to_flat_aggregate /
    // plan_version_files_rewrites — small fixture builder for GroupTagResult.
    // -----------------------------------------------------------------------

    fn group_result(
        crate_names: &[&str],
        new_tags: &[(&str, &str)],
        version_updates: &[(&str, &str)],
        old_version: Option<&str>,
        prev_tag: Option<&str>,
        crate_version_files: Vec<Vec<String>>,
    ) -> GroupTagResult {
        GroupTagResult {
            crate_names: crate_names.iter().map(|s| s.to_string()).collect(),
            new_tags: new_tags
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
            version_updates: version_updates
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
            old_version: old_version.map(str::to_string),
            prev_tag: prev_tag.map(str::to_string),
            crate_version_files,
        }
    }

    #[test]
    fn plan_changelog_targets_one_target_per_bumped_crate() {
        let root = Path::new("/ws");
        let groups = vec![
            group_result(
                &["core"],
                &[("core-v0.2.0", "msg")],
                &[("crates/core", "0.2.0")],
                Some("0.1.0"),
                Some("core-v0.1.0"),
                vec![vec![]],
            ),
            group_result(
                &["cli"],
                &[("cli-v1.0.0", "msg")],
                &[("crates/cli", "1.0.0")],
                None,
                None,
                vec![vec![]],
            ),
        ];
        let targets = plan_changelog_targets(root, &groups);
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].crate_name, "core");
        assert_eq!(targets[0].crate_dir, root.join("crates/core"));
        assert_eq!(targets[0].from_tag.as_deref(), Some("core-v0.1.0"));
        assert_eq!(targets[0].to_version, "0.2.0");
        assert_eq!(targets[0].full_tag, "core-v0.2.0");
        assert_eq!(targets[1].crate_name, "cli");
        assert_eq!(targets[1].from_tag, None);
        assert_eq!(targets[1].full_tag, "cli-v1.0.0");
    }

    #[test]
    fn collapse_targets_to_flat_aggregate_collapses_lockstep_set() {
        let root = Path::new("/ws");
        let groups = vec![group_result(
            &["a", "b"],
            &[("v0.5.0", "m"), ("v0.5.0", "m")],
            &[("crates/a", "0.5.0"), ("crates/b", "0.5.0")],
            Some("0.4.0"),
            Some("v0.4.0"),
            vec![vec![], vec![]],
        )];
        let mut targets = plan_changelog_targets(root, &groups);
        assert_eq!(targets.len(), 2, "precondition: two per-crate targets");
        let config = anodizer_core::config::Config {
            project_name: "myproj".to_string(),
            ..Default::default()
        };
        let collapsed = collapse_targets_to_flat_aggregate(&mut targets, root, Some(&config), true);
        assert!(collapsed);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].crate_name, "myproj");
        assert_eq!(targets[0].crate_dir, root.to_path_buf());
        assert_eq!(targets[0].from_tag.as_deref(), Some("v0.4.0"));
        assert_eq!(targets[0].to_version, "0.5.0");
    }

    #[test]
    fn collapse_targets_to_flat_aggregate_noop_when_collapse_false() {
        let root = Path::new("/ws");
        let mut targets = plan_changelog_targets(
            root,
            &[group_result(
                &["a", "b"],
                &[("v1.0.0", "m"), ("v1.0.0", "m")],
                &[("crates/a", "1.0.0"), ("crates/b", "1.0.0")],
                Some("0.9.0"),
                Some("v0.9.0"),
                vec![vec![], vec![]],
            )],
        );
        let config = anodizer_core::config::Config::default();
        let collapsed =
            collapse_targets_to_flat_aggregate(&mut targets, root, Some(&config), false);
        assert!(!collapsed);
        assert_eq!(targets.len(), 2, "targets must be left untouched");
    }

    #[test]
    fn collapse_targets_to_flat_aggregate_noop_for_single_target() {
        let root = Path::new("/ws");
        let mut targets = plan_changelog_targets(
            root,
            &[group_result(
                &["solo"],
                &[("v1.0.0", "m")],
                &[("crates/solo", "1.0.0")],
                Some("0.9.0"),
                Some("v0.9.0"),
                vec![vec![]],
            )],
        );
        let config = anodizer_core::config::Config::default();
        assert!(!collapse_targets_to_flat_aggregate(
            &mut targets,
            root,
            Some(&config),
            true
        ));
        assert_eq!(targets.len(), 1);
    }

    #[test]
    fn plan_version_files_rewrites_dedupes_identical_lockstep_pair() {
        // Two crates in one group enroll the same file with the same (old,new):
        // a lockstep set dedupes to a single rewrite.
        let groups = vec![group_result(
            &["a", "b"],
            &[("v0.2.0", "m"), ("v0.2.0", "m")],
            &[("crates/a", "0.2.0"), ("crates/b", "0.2.0")],
            Some("0.1.0"),
            Some("v0.1.0"),
            vec![vec!["README.md".to_string()], vec!["README.md".to_string()]],
        )];
        let plan = plan_version_files_rewrites(&groups).unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].file, "README.md");
        assert_eq!(plan[0].old, "0.1.0");
        assert_eq!(plan[0].new, "0.2.0");
    }

    #[test]
    fn plan_version_files_rewrites_conflicting_old_versions_bail() {
        // Two crates enroll the SAME file but bump from different old versions:
        // a file cannot hold two source versions in one tag run.
        let groups = vec![
            group_result(
                &["a"],
                &[("a-v0.2.0", "m")],
                &[("crates/a", "0.2.0")],
                Some("0.1.0"),
                Some("a-v0.1.0"),
                vec![vec!["shared.txt".to_string()]],
            ),
            group_result(
                &["b"],
                &[("b-v0.2.0", "m")],
                &[("crates/b", "0.2.0")],
                Some("0.1.5"),
                Some("b-v0.1.5"),
                vec![vec!["shared.txt".to_string()]],
            ),
        ];
        let err = plan_version_files_rewrites(&groups)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("version_files conflict") && err.contains("shared.txt"),
            "conflict must name the file, got: {err}"
        );
    }

    #[test]
    fn plan_version_files_rewrites_skips_group_with_no_old_version() {
        // A first-tag group (old_version=None) has nothing to rewrite from.
        let groups = vec![group_result(
            &["new"],
            &[("new-v0.1.0", "m")],
            &[("crates/new", "0.1.0")],
            None,
            None,
            vec![vec!["VERSION".to_string()]],
        )];
        let plan = plan_version_files_rewrites(&groups).unwrap();
        assert!(plan.is_empty());
    }
}
