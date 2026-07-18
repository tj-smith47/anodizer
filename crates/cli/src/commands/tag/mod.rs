pub mod rollback;

mod bump_detect;
mod crate_info;
mod per_crate;
mod repo_shape;
mod run;
mod version_plan;
mod workspace_bump;

pub(crate) use bump_detect::*;
pub(crate) use crate_info::*;
pub(crate) use per_crate::*;
pub(crate) use repo_shape::*;
pub use run::run;
pub(crate) use version_plan::*;
pub(crate) use workspace_bump::*;

#[cfg(test)]
mod tests;

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
    /// Do not push anything; the tag and the version-sync bump commit both stay local.
    pub no_push: bool,
    /// Push the tag(s) but NOT the version-sync bump commit. The explicit
    /// opt-in for the deferred-branch CI pattern: the release pipeline pushes
    /// the tag to trigger publishing and fast-forwards the branch onto the
    /// bump commit only after publish succeeds, so a failed release advances
    /// nothing. The branch MUST be advanced separately or the remote tag
    /// permanently references a commit missing from every branch.
    pub push_tags_only: bool,
    /// Create the version tag as a signed annotated tag (`git tag -s`). The
    /// signing key/method come from the user's git config (`user.signingkey`,
    /// `gpg.format`). Overrides `tag.sign = false`.
    pub sign: bool,
    /// Create the version tag unsigned (`git tag -a`), overriding
    /// `tag.sign = true`. Wins over `--sign` and config, mirroring `--no-push`.
    pub no_sign: bool,
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

/// Resolve whether this run pushes at all (the bump commit + tag(s),
/// atomically). A run either pushes branch and tags together or pushes
/// nothing — a remote tag referencing an unpushed bump commit (an orphan
/// tag) is not a representable outcome.
///
/// Every dispatch shape — single, lockstep, `--crate`, and per-crate
/// auto-dispatch — shares one default: **fully local**. Cutting a tag never
/// touches the remote unless a push is explicitly requested, mirroring
/// `git tag`. `--no-push` always wins (redundant with the default); then an
/// explicit `--push` or `tag.push = true` selects the atomic push; otherwise
/// nothing is pushed.
fn resolve_effective_push(opts: &TagOpts, config_push: Option<bool>) -> bool {
    if opts.no_push {
        false
    } else {
        opts.push || config_push == Some(true)
    }
}

/// Resolve whether the version tag is created signed (`git tag -s`) or unsigned
/// (`git tag -a`). Workspace-global: the resolved value applies to every tag
/// this run cuts in single-crate, lockstep, and per-crate modes. Precedence
/// mirrors `resolve_effective_push`: `--no-sign` always wins, then `--sign` or
/// `tag.sign = true` selects a signed tag; otherwise the tag is unsigned.
fn resolve_effective_sign(opts: &TagOpts, config_sign: Option<bool>) -> bool {
    if opts.no_sign {
        false
    } else {
        opts.sign || config_sign == Some(true)
    }
}

/// Resolved tag configuration with defaults applied.
#[derive(Clone)]
pub(crate) struct ResolvedConfig {
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

/// Warn about a stale `Cargo.lock` after a version writeback.
///
/// Staleness is warn-and-continue by design (a missing/broken `cargo` on PATH
/// must not block tagging), but the stale lockfile WILL break the release
/// later — publish/determinism reject a lockfile that disagrees with the
/// bumped manifests — so the warn names that consequence and the remedy.
fn warn_cargo_lock_stale(log: &StageLogger, cause: &str) {
    log.warn(&format!(
        "{cause}; Cargo.lock is now stale relative to the bumped Cargo.toml, and \
         `release` (publish / determinism) will fail on it later — run \
         `cargo update --workspace` and fold Cargo.lock into the bump commit \
         before releasing"
    ));
}

/// `[skip ci]` suffix appended to a bump-commit subject, or empty when
/// `skip_ci_on_bump` is off (the default). Returned with a leading space so
/// callers can append it directly after the subject body.
fn skip_ci_suffix(skip_ci_on_bump: bool) -> &'static str {
    if skip_ci_on_bump { " [skip ci]" } else { "" }
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
pub(crate) struct VersionFilesBump<'a> {
    old: Option<&'a str>,
    files: &'a [String],
}

/// Lockstep changelog-refresh inputs for [`apply_workspace_bump`]. The shared
/// workspace tag bounds every member's rendered commit range, so a single
/// `from_tag` applies to all members, and the single shared `full_tag` keys the
/// aggregate root section.
pub(crate) struct ChangelogBump<'a> {
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
pub(crate) struct WorkspaceBumpEdits<'a> {
    vf: VersionFilesBump<'a>,
    cl: ChangelogBump<'a>,
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

/// Result of per-crate tag computation for one group.
pub(crate) struct GroupTagResult {
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
pub(crate) struct PushControls<'a> {
    remote: &'a str,
    config_push: Option<bool>,
    /// Resolved signed-tag selection (`--sign`/`--no-sign`/`tag.sign`), threaded
    /// in so per-crate tags are signed identically to the single/lockstep path.
    sign: bool,
    changelog_enabled: bool,
    /// `tag_pre_hooks` / `tag_post_hooks`, threaded in so the per-crate path
    /// honors the same hook config as the single/lockstep `create_tag` closure.
    pre_hooks: &'a [anodizer_core::config::HookEntry],
    post_hooks: &'a [anodizer_core::config::HookEntry],
    /// Tag names present on `remote` (one ls-remote per invocation, fetched by
    /// [`run`]); `None` when there is no remote or the fetch failed (local
    /// fallback). Threaded into previous-tag resolution so a remotely-deleted
    /// tag that survives in this clone never counts as "previous".
    remote_tags: Option<&'a std::collections::HashSet<String>>,
}

/// The per-crate engine's dispatched unit: the lockstep groups to tag plus
/// whether they are a single `FlatAggregate` (shared-prefix flat `crates:`
/// list). The flag drives the one-flat-section changelog collapse; it is
/// resolved once by [`detect_repo_shape`] so the collapse decision is never
/// re-derived from prefixes here.
pub(crate) struct PerCrateDispatch {
    groups: Vec<Vec<CrateConfig>>,
    is_flat_aggregate: bool,
    /// The config-derived workspace root threaded from `run` so the per-crate
    /// engine's git ops and cross-crate scans resolve the same root from a
    /// subdirectory as from the repo root.
    workspace_root: PathBuf,
}

/// Info extracted from a crate's config for path-scoped tagging.
pub(crate) struct CrateTagInfo {
    tag_prefix: String,
    path: String,
    version_sync: bool,
    /// Effective `version_files` enrollment for this crate (per-crate /
    /// defaults list, else the top-level `Config.version_files`).
    version_files: Vec<String>,
}

/// One planned `version_files` rewrite: rewrite `old` → `new` in `file`.
#[derive(Debug, PartialEq)]
pub(crate) struct VersionFileRewrite {
    file: String,
    old: String,
    new: String,
}
