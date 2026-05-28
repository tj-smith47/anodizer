mod announce_only;
mod milestones;
mod publish_only;
mod split;

pub use split::{load_split_contexts_into, run_merge};

use super::helpers;
use crate::pipeline;
use anodizer_core::config::{Config, CrateConfig, WorkspaceConfig};
use anodizer_core::context::{Context, ContextOptions, RollbackMode};
use anodizer_core::git;
use anodizer_core::log::{StageLogger, Verbosity};
use anodizer_core::template;
use anyhow::{Context as _, Result};
use std::path::PathBuf;

pub struct ReleaseOpts {
    pub crate_names: Vec<String>,
    pub all: bool,
    pub force: bool,
    pub snapshot: bool,
    pub nightly: bool,
    pub dry_run: bool,
    pub clean: bool,
    pub skip: Vec<String>,
    pub token: Option<String>,
    pub verbose: bool,
    pub debug: bool,
    pub quiet: bool,
    pub config_override: Option<PathBuf>,
    pub parallelism: usize,
    pub single_target: Option<String>,
    /// `--targets=<csv>`: restrict the build to a comma-separated subset
    /// of configured target triples. Used by the sharded Determinism
    /// Harness (each runner only validates its own native targets) and
    /// available to operators driving custom CI matrices. When `Some`,
    /// the release dispatcher populates
    /// `ContextOptions::partial_target = Some(PartialTarget::Targets(...))`
    /// so the existing build-stage filter (`partial.filter_targets`)
    /// trims the configured list down to the intersection. Mutually
    /// exclusive with `single_target` (clap-level `conflicts_with`).
    pub targets: Option<Vec<String>>,
    pub release_notes: Option<PathBuf>,
    pub release_notes_tmpl: Option<PathBuf>,
    pub workspace: Option<String>,
    pub draft: bool,
    pub release_header: Option<PathBuf>,
    pub release_header_tmpl: Option<PathBuf>,
    pub release_footer: Option<PathBuf>,
    pub release_footer_tmpl: Option<PathBuf>,
    pub fail_fast: bool,
    pub split: bool,
    pub merge: bool,
    /// `--publish-only`: load `dist/context.json` (preserved by
    /// `anodize check determinism --preserve-dist=...`) and run only
    /// the sign + publish pipeline. Mutually exclusive with `split` /
    /// `merge` at the clap level.
    pub publish_only: bool,
    pub strict: bool,
    /// `--prepare` (GoReleaser Pro parity): run local build/archive/sign/checksum/sbom
    /// stages but NOT release/publish/announce. Implemented by augmenting `skip` with
    /// those three stages at the top of `run()`; artifacts still land under `dist/`.
    pub prepare: bool,
    /// `--announce-only`: re-fire the announce stage after loading a
    /// prior run's `<dist>/run-<id>/report.json`. Use case: a
    /// transient announcer failure (Slack 502, Discord 5xx) after a
    /// successful publish — operator wants to retry notifications
    /// without re-creating the GitHub release or re-uploading
    /// archives. Skips every other stage in the pipeline.
    pub announce_only: bool,
    /// `--resume-release`: continue into an existing release rather than
    /// bailing on the leftover-assets pre-check. Plumbed into
    /// `ContextOptions::resume_release`.
    pub resume_release: bool,
    /// `--replace-existing`: CLI override for `release.replace_existing_artifacts: true`.
    /// Plumbed into `ContextOptions::replace_existing_artifacts`.
    pub replace_existing: bool,
    /// `--preflight`: run the pre-flight publisher-state check and exit
    /// (don't continue into the rest of the release pipeline).
    pub preflight: bool,
    /// `--no-preflight`: skip the automatic pre-flight check that normally
    /// runs as the first step of `release`.
    pub no_preflight: bool,
    /// `--strict-preflight`: treat `PublisherState::Unknown` results as
    /// blockers too. Useful in CI where any uncertainty should fail-fast.
    pub strict_preflight: bool,
    /// `--no-post-publish-poll`: skip the post-publish polling that
    /// otherwise waits on chocolatey moderation / winget PR validation
    /// after the publish step's HTTP 2xx. Plumbed into
    /// `ContextOptions::skip_post_publish_poll`.
    pub no_post_publish_poll: bool,
    /// `--no-gate-submitter`: disable the Submitter gate so Submitter
    /// publishers dispatch even when a required Assets/Manager
    /// publisher failed. Plumbed into
    /// `ContextOptions::gate_submitter` as `Some(false)`. Default
    /// (`None`) means gate-on.
    pub no_gate_submitter: bool,
    /// `--rollback=<none|best-effort>`: post-publish rollback policy
    /// override. Validated against the {none, best-effort} set in
    /// `run()` and stored as `ContextOptions::rollback_mode`.
    pub rollback: Option<String>,
    /// `--simulate-failure=<publisher>` (repeatable): names of
    /// publishers whose `run()` should be replaced with a synthetic
    /// failure in `stage-publish::dispatch`. Only honored when
    /// `ANODIZE_TEST_HARNESS=1` is set; otherwise rejected at the
    /// translation site so production releases cannot trip it.
    pub simulate_failure: Vec<String>,
    /// `--rollback-only`: skip publish; re-attempt rollback from a
    /// prior run report. The replay logic lands in a follow-up; `run()`
    /// bails with a clear "not yet implemented" error in this revision
    /// so the flag is discoverable via `--help`.
    pub rollback_only: bool,
    /// `--from-run=<id>`: prior run id whose `report.json` to load
    /// when running with `--rollback-only`.
    pub from_run: Option<String>,
    /// `--allow-rerun`: force `PublishStage::run` to proceed even when
    /// a prior `dist/run-<id>/report.json` exists. Plumbed into
    /// `ContextOptions::allow_rerun`. See the audit reference in
    /// `crates/stage-publish/src/lib.rs::PublishStage::run` for the
    /// duplicate-publish-risk rationale.
    pub allow_rerun: bool,
    /// `--allow-nondeterministic <name>=<reason>` (repeatable):
    /// runtime non-determinism opt-outs. Parsed at the translation
    /// site into `(name, reason)` tuples; empty reasons are rejected
    /// so the report always carries a human-readable justification.
    pub allow_nondeterministic: Vec<String>,
    /// `--summary-json=<path>`: when set, the per-publisher run
    /// summary is written here.
    pub summary_json: Option<PathBuf>,
    /// `--allow-ai-failure`: opt-in to degraded behaviour when
    /// `changelog.ai` is configured and the provider fails. Default
    /// (fail-closed) aborts the release on any provider error so the
    /// operator notices instead of shipping the pre-AI body silently.
    pub allow_ai_failure: bool,
}

/// Decide whether the pre-flight publisher-state check should run.
///
/// Encodes the gating rules so they can be unit-tested without dragging
/// the entire pipeline up. The rules are:
///
/// - `--no-preflight` always wins → false.
/// - `--snapshot` / `--dry-run` / `--split` skip → no upstream side effects.
/// - `--publish-only` skips → the publish-only branch does its own
///   credential preflight at the top of `publish_only::run`; running
///   the publisher-state preflight here first would make network
///   calls (chocolatey/winget/cargo/aur state probes) before the
///   credential gate, defeating the "fail before any mutation"
///   property the spec requires.
/// - `publish` in `skip` → caller opted out of one-way doors.
/// - otherwise → true.
///
/// Note: this is the implicit-run decision. `--preflight` (the explicit
/// check-only mode) gates separately in the call site and always runs the
/// check independently of this predicate. `--announce-only` is handled by
/// an earlier short-circuit in `run_publisher_preflight` and so is not a
/// parameter here.
pub(crate) fn should_run_preflight_auto(
    no_preflight: bool,
    snapshot: bool,
    dry_run: bool,
    split: bool,
    publish_only: bool,
    publish_skipped: bool,
) -> bool {
    !no_preflight && !snapshot && !dry_run && !split && !publish_only && !publish_skipped
}

/// GoReleaser Pro `--prepare`: runs local build/archive/sign/checksum/sbom stages
/// but skips anything that reaches upstream (release + publish + announce).
/// Idempotent — won't duplicate stages already present in `skip`.
///
/// Composition with `--snapshot`: well-defined — `--prepare --snapshot` emits
/// snapshot-prefixed artifacts (`Version`/`Tag` derived from
/// `<version>-SNAPSHOT-<shortcommit>`, no tag required) without publishing.
/// Useful for generating pre-release archives in PR CI without needing a real
/// tag or release. `--prepare` without `--snapshot` requires a real tag.
pub(crate) fn apply_prepare_mode_to_skip(skip: &mut Vec<String>) {
    for stage in [
        "release",
        "publish",
        "blob",
        "snapcraft-publish",
        "announce",
    ] {
        if !skip.iter().any(|s| s == stage) {
            skip.push(stage.to_string());
        }
    }
}

pub fn run(mut opts: ReleaseOpts) -> Result<()> {
    if opts.prepare {
        apply_prepare_mode_to_skip(&mut opts.skip);
    }
    validate_strict_vs_allowlist(&opts)?;

    let log = StageLogger::new(
        "release",
        Verbosity::from_flags(opts.quiet, opts.verbose, opts.debug),
    );

    git::check_git_available()?;

    if opts.snapshot && opts.nightly {
        anyhow::bail!("--snapshot and --nightly cannot be combined");
    }

    let config_path =
        pipeline::find_config_with_logger(opts.config_override.as_deref(), Some(&log))?;
    let mut config = pipeline::load_config(&config_path)?;

    let workspace_skip = apply_workspace_overlay_for_opts(&mut config, &opts, &log)?;

    helpers::infer_project_name(&mut config, &log);
    helpers::auto_detect_github(&mut config, &log);

    apply_release_meta_overrides(&mut config, &opts)?;
    enforce_dist_state(&config, &opts, &log)?;

    let all_known_crates = flatten_known_crates(&config);
    let selected_sorted = resolve_selected_crates(&opts, &all_known_crates, &config, &log)?;

    // Tags-at-HEAD default path: when no --crate and no --all were given and
    // HEAD has no matching tags, this is a no-op (the push that triggered this
    // run didn't include any release tags).
    //
    // Excluded modes: --snapshot / --nightly / --dry-run build without a real
    // tag; --publish-only / --announce-only / --rollback-only consume a prior
    // dist tree; --split / --merge drive a multi-host flow. All of those modes
    // use "empty selected_crates = all crates" and must not be short-circuited.
    if selected_sorted.is_empty()
        && opts.crate_names.is_empty()
        && !opts.all
        && !opts.snapshot
        && !opts.nightly
        && !opts.dry_run
        && !opts.publish_only
        && !opts.announce_only
        && !opts.rollback_only
        && !opts.split
        && !opts.merge
    {
        log.status("no release tags at HEAD — nothing to do");
        return Ok(());
    }

    let skip_stages = compute_skip_stages(opts.skip.clone(), &workspace_skip, opts.snapshot);

    let release_notes_path = read_release_notes_template(&opts)?;
    let rollback_mode = parse_rollback_mode(opts.rollback.as_deref())?;
    let simulate_failure_publishers = resolve_simulate_failure(&mut opts.simulate_failure)?;
    let runtime_nondeterministic_allowlist =
        parse_allow_nondeterministic(&opts.allow_nondeterministic)?;

    let ctx_opts = build_context_options(
        &opts,
        skip_stages,
        selected_sorted,
        rollback_mode,
        simulate_failure_publishers,
        runtime_nondeterministic_allowlist,
    );
    let mut ctx = Context::new(config.clone(), ctx_opts);
    helpers::resolve_scm_token_type(&mut ctx, &config);
    ctx.populate_time_vars();
    ctx.populate_runtime_vars();
    ctx.populate_metadata_var()?;

    if ctx.options.rollback_only {
        return run_rollback_only(&mut ctx);
    }

    // Set explicitly to "true"/"false" so `{% if IsPrepare %}` evaluates
    // correctly in either branch (a missing var would short-circuit the
    // truthy arm even when prepare mode is requested).
    ctx.template_vars_mut()
        .set("IsPrepare", if opts.prepare { "true" } else { "false" });

    helpers::setup_env(&mut ctx, &config, &log)?;
    helpers::resolve_git_context(&mut ctx, &config, &log)?;

    run_before_hooks(&ctx, &config, &opts, &log)?;
    render_release_notes_tmpl(&mut ctx, &config, &opts, release_notes_path, &log)?;
    enforce_dirty_repo_gate(&ctx)?;

    if ctx.is_nightly() {
        apply_nightly_template_vars(&mut ctx, &config, &log)?;
    }
    if ctx.is_snapshot() {
        apply_snapshot_template_vars(&mut ctx, &config, &log)?;
    }

    helpers::write_effective_config(&config, &log)?;

    if !opts.split
        && !opts.announce_only
        && let Some(ref milestones) = config.milestones
    {
        milestones::preflight_milestones(milestones, &mut ctx, &log)?;
    }

    if run_publisher_preflight(&mut ctx, &opts, &log)? {
        return Ok(());
    }

    if opts.publish_only {
        // --publish-only consumes the preserved dist tree (artifacts.json /
        // context.json) rather than git tags-at-HEAD. Crate selection comes
        // from what the harness built (recorded in <dist>/context.json), not
        // from `selected_sorted`, so the tags-at-HEAD no-op guard above is
        // intentionally bypassed for this mode.
        let dist = config.dist.clone();
        let run_opts = publish_only::RunOpts {
            dry_run: opts.dry_run,
            no_preflight: opts.no_preflight,
        };
        // When --crate is given, always use the flat path regardless of
        // layout. Per-crate auto-iteration is a no-crate-flag feature.
        if !opts.crate_names.is_empty() {
            return publish_only::run(&mut ctx, &config, &log, run_opts);
        }
        // Detect layout and dispatch.
        match publish_only::detect_dist_layout(&dist)? {
            publish_only::DistLayout::Flat => {
                return publish_only::run(&mut ctx, &config, &log, run_opts);
            }
            publish_only::DistLayout::PerCrate(subdirs) => {
                // Topo-sort discovered crate names so depends_on ordering
                // is respected. Fall back to alphabetical when none of the
                // discovered names match any configured crate.
                let all_known = flatten_known_crates(&config);
                let sorted = topo_sort_selected(&all_known, &subdirs);
                let order = if sorted.is_empty() { subdirs } else { sorted };
                return publish_only::run_per_crate(&mut ctx, &config, &log, run_opts, dist, order);
            }
            publish_only::DistLayout::Ambiguous { crate_subdirs } => {
                anyhow::bail!(
                    "publish-only: ambiguous dist layout at {} — found both a flat \
                     context.json at the root AND per-crate subdirectories ({}). \
                     Delete one or the other, or pass --crate <name> to select a \
                     specific crate.",
                    dist.display(),
                    crate_subdirs.join(", ")
                );
            }
        }
    }

    if opts.announce_only {
        return announce_only::run(&mut ctx, &config, &log, opts.dry_run);
    }

    if opts.split {
        return split::run_split(&mut ctx, &config, &log);
    }

    if opts.merge {
        return split::run_merge(&mut ctx, &config, &log, opts.dry_run, None);
    }

    let p = pipeline::build_release_pipeline();
    let result = p.run(&mut ctx, &log);

    if result.is_ok() {
        run_post_pipeline(&mut ctx, &config, opts.dry_run, &log)?;
    }

    if result.is_ok() {
        gate_required_failures(&ctx)?;
    }

    result
}

/// `--strict` and `--allow-nondeterministic` are mutually exclusive: strict
/// mode forbids the determinism stage from suppressing findings, the
/// allowlist's whole purpose is to suppress one. clap can't express this
/// directly (--strict lives on the top-level Cli struct and the allowlist on
/// the Release variant), so the check runs here.
fn validate_strict_vs_allowlist(opts: &ReleaseOpts) -> Result<()> {
    if opts.strict && !opts.allow_nondeterministic.is_empty() {
        anyhow::bail!(
            "--strict and --allow-nondeterministic are mutually exclusive (drop --strict if a runtime exemption is required)"
        );
    }
    Ok(())
}

/// Apply the workspace overlay (explicit `--workspace`, or inferred from the
/// first `--crate` when the top-level config has no crates). Returns the
/// list of workspace-level skip stages to merge later.
fn apply_workspace_overlay_for_opts(
    config: &mut Config,
    opts: &ReleaseOpts,
    log: &StageLogger,
) -> Result<Vec<String>> {
    let mut workspace_skip: Vec<String> = Vec::new();
    if let Some(ref ws_name) = opts.workspace {
        let ws = resolve_workspace(config, ws_name)?.clone();
        workspace_skip = ws.skip.clone();
        helpers::apply_workspace_overlay(config, &ws);
    } else if !opts.crate_names.is_empty() && config.crates.is_empty() {
        // No --workspace given, but --crate X was — infer the workspace that
        // contains X and apply its overlay. Without this, every downstream
        // stage iterates `ctx.config.crates` which is empty in workspace-based
        // configs and silently does nothing. Matches user intuition: "release
        // crate X" should release X's workspace.
        let target = &opts.crate_names[0];
        let ws_for_target = config
            .workspaces
            .as_ref()
            .and_then(|ws_list| {
                ws_list
                    .iter()
                    .find(|ws| ws.crates.iter().any(|c| &c.name == target))
            })
            .cloned();
        if let Some(ws) = ws_for_target {
            log.verbose(&format!(
                "--crate {} lives in workspace '{}'; applying workspace overlay",
                target, ws.name
            ));
            workspace_skip = ws.skip.clone();
            helpers::apply_workspace_overlay(config, &ws);
        }
    }
    Ok(workspace_skip)
}

/// Apply CLI overrides that mutate `config.release` (draft / header / footer
/// and their `_tmpl` variants). `*_tmpl` flags override their plain
/// counterparts; the template stage renders the content later.
fn apply_release_meta_overrides(config: &mut Config, opts: &ReleaseOpts) -> Result<()> {
    if opts.draft {
        let release = config.release.get_or_insert_with(Default::default);
        release.draft = Some(true);
    }
    if let Some(ref header_path) = opts.release_header {
        let header_content = std::fs::read_to_string(header_path).with_context(|| {
            format!(
                "failed to read release header file: {}",
                header_path.display()
            )
        })?;
        let release = config.release.get_or_insert_with(Default::default);
        release.header = Some(anodizer_core::config::ContentSource::Inline(header_content));
    }
    if let Some(ref header_tmpl_path) = opts.release_header_tmpl {
        let raw = std::fs::read_to_string(header_tmpl_path).with_context(|| {
            format!(
                "failed to read release header template file: {}",
                header_tmpl_path.display()
            )
        })?;
        let release = config.release.get_or_insert_with(Default::default);
        release.header = Some(anodizer_core::config::ContentSource::Inline(raw));
    }
    if let Some(ref footer_path) = opts.release_footer {
        let footer_content = std::fs::read_to_string(footer_path).with_context(|| {
            format!(
                "failed to read release footer file: {}",
                footer_path.display()
            )
        })?;
        let release = config.release.get_or_insert_with(Default::default);
        release.footer = Some(anodizer_core::config::ContentSource::Inline(footer_content));
    }
    if let Some(ref footer_tmpl_path) = opts.release_footer_tmpl {
        let raw = std::fs::read_to_string(footer_tmpl_path).with_context(|| {
            format!(
                "failed to read release footer template file: {}",
                footer_tmpl_path.display()
            )
        })?;
        let release = config.release.get_or_insert_with(Default::default);
        release.footer = Some(anodizer_core::config::ContentSource::Inline(raw));
    }
    Ok(())
}

/// Enforce the dist directory state: `--clean` removes it (logs in dry-run);
/// otherwise a populated dist is a hard error (GoReleaser's `ErrDirtyDist`).
/// `--merge` / `--publish-only` / `--rollback-only` skip the non-empty check
/// because each of those modes requires preserved dist content.
fn enforce_dist_state(config: &Config, opts: &ReleaseOpts, log: &StageLogger) -> Result<()> {
    if opts.clean && !opts.dry_run {
        let dist = &config.dist;
        if dist.exists() {
            std::fs::remove_dir_all(dist)?;
        }
    } else if opts.clean && opts.dry_run {
        log.status("(dry-run) would clean dist directory");
    }

    if !opts.clean
        && !opts.merge
        && !opts.publish_only
        && !opts.rollback_only
        && !opts.announce_only
    {
        let dist = &config.dist;
        if dist.exists()
            && let Ok(mut entries) = dist.read_dir()
            && entries.next().is_some()
        {
            anyhow::bail!(
                "dist directory '{}' is not empty; use --clean to remove it first",
                dist.display()
            );
        }
    }
    Ok(())
}

/// Flatten every known crate — top-level plus anything under workspaces —
/// so `--crate X` and `--all` resolve the same way regardless of whether
/// the config is flat or workspace-based.
pub(crate) fn flatten_known_crates(config: &Config) -> Vec<CrateConfig> {
    let mut acc: Vec<CrateConfig> = config.crates.clone();
    if let Some(ref ws_list) = config.workspaces {
        for ws in ws_list {
            for c in &ws.crates {
                if !acc.iter().any(|existing| existing.name == c.name) {
                    acc.push(c.clone());
                }
            }
        }
    }
    acc
}

/// Resolve the crate selection (`--all` + change detection, `--all --force`,
/// explicit `--crate` list, or tags-at-HEAD default) and topologically sort it.
fn resolve_selected_crates(
    opts: &ReleaseOpts,
    all_known_crates: &[CrateConfig],
    config: &Config,
    log: &StageLogger,
) -> Result<Vec<String>> {
    let selected = if opts.all {
        if opts.force {
            all_known_crates.iter().map(|c| c.name.clone()).collect()
        } else {
            detect_changed_crates(
                all_known_crates,
                config.git.as_ref(),
                config.monorepo_tag_prefix(),
                log,
            )?
        }
    } else if !opts.crate_names.is_empty() {
        opts.crate_names.clone()
    } else {
        // Default: read tags pointing at HEAD and map each to a crate.
        map_head_tags_to_crates(all_known_crates, log)?
    };
    Ok(topo_sort_selected(all_known_crates, &selected))
}

/// Read tags pointing at HEAD and resolve each to a crate name via
/// per-crate `tag_template` prefix matching.
///
/// Tags that don't match any configured crate are silently ignored — this
/// allows foreign tags (e.g. a nightly build tag) to coexist without
/// aborting the release pipeline.
///
/// Returns an empty vec when HEAD has no tags; the caller treats that as a
/// no-op.
fn map_head_tags_to_crates(
    all_known_crates: &[CrateConfig],
    log: &StageLogger,
) -> Result<Vec<String>> {
    let head_tags = git::get_tags_at_head().with_context(|| "failed to read tags at HEAD")?;
    if head_tags.is_empty() {
        log.verbose("no tags at HEAD — release no-op");
        return Ok(Vec::new());
    }
    log.verbose(&format!("tags at HEAD: {}", head_tags.join(", ")));

    let mut selected: Vec<String> = Vec::new();
    for tag in &head_tags {
        match resolve_tag_to_crate(tag, all_known_crates) {
            Some(c) if !selected.contains(&c.name) => {
                selected.push(c.name.clone());
                log.verbose(&format!("tag '{}' → crate '{}'", tag, c.name));
            }
            Some(_) => {}
            None => {
                log.verbose(&format!(
                    "tag '{}' does not match any configured crate — skipping",
                    tag
                ));
            }
        }
    }

    Ok(selected)
}

/// Resolve a single tag to a crate by longest-matching `tag_template` prefix.
///
/// Returns `Some(crate)` when the tag's prefix matches one of the configured
/// crates and the remainder is a numeric version (so `v1.0` matches but
/// `vendor-branch` would not). Prefers the longest matching prefix so a more
/// specific crate (`core-v`) wins over a shorter sibling (`v`).
///
/// Returns `None` for tags that don't match any configured crate — these are
/// silently ignored at the caller (e.g. nightly build tags coexist with
/// release tags without aborting the pipeline).
pub(crate) fn resolve_tag_to_crate<'a>(
    tag: &str,
    crates: &'a [CrateConfig],
) -> Option<&'a CrateConfig> {
    let mut best: Option<(&CrateConfig, usize)> = None;
    for c in crates {
        if let Some(prefix) = git::extract_tag_prefix(&c.tag_template)
            && tag.starts_with(&prefix)
        {
            let remainder = &tag[prefix.len()..];
            let is_version = remainder
                .split('.')
                .next()
                .is_some_and(|s| !s.is_empty() && s.chars().all(|ch| ch.is_ascii_digit()));
            if is_version && best.as_ref().is_none_or(|(_, len)| prefix.len() > *len) {
                best = Some((c, prefix.len()));
            }
        }
    }
    best.map(|(c, _)| c)
}

/// Merge CLI / workspace / snapshot-implied skip stages into one list.
/// Snapshot mode auto-skips every stage that performs an external upload
/// (`publish`, `snapcraft-publish`, `blob`, `announce`); the release stage
/// handles snapshot mode internally. Skipping `publish` implies skipping
/// `announce` (matches GoReleaser).
fn compute_skip_stages(
    mut skip_stages: Vec<String>,
    workspace_skip: &[String],
    snapshot: bool,
) -> Vec<String> {
    for stage in workspace_skip {
        if !skip_stages.iter().any(|s| s == stage) {
            skip_stages.push(stage.clone());
        }
    }
    if snapshot {
        for stage in &["publish", "snapcraft-publish", "blob", "announce"] {
            if !skip_stages.iter().any(|s| s == stage) {
                skip_stages.push(stage.to_string());
            }
        }
    }
    if skip_stages.contains(&"publish".to_string())
        && !skip_stages.contains(&"announce".to_string())
    {
        skip_stages.push("announce".to_string());
    }
    skip_stages
}

/// Read the `--release-notes-tmpl` file (when set) so its content can be
/// rendered post-`populate_*_vars`. `--release-notes-tmpl` overrides
/// `--release-notes`.
fn read_release_notes_template(opts: &ReleaseOpts) -> Result<Option<(PathBuf, String)>> {
    if let Some(ref tmpl_path) = opts.release_notes_tmpl {
        let content = std::fs::read_to_string(tmpl_path).with_context(|| {
            format!(
                "failed to read release notes template: {}",
                tmpl_path.display()
            )
        })?;
        Ok(Some((tmpl_path.clone(), content)))
    } else {
        Ok(None)
    }
}

/// Translate `--rollback=<v>` into the enum; reject invalid values up front
/// so the dispatch site can rely on a clean value.
fn parse_rollback_mode(rollback: Option<&str>) -> Result<Option<RollbackMode>> {
    match rollback {
        Some("none") => Ok(Some(RollbackMode::None)),
        Some("best-effort") => Ok(Some(RollbackMode::BestEffort)),
        Some(other) => anyhow::bail!(
            "invalid --rollback value: {} (expected: none, best-effort)",
            other
        ),
        None => Ok(None),
    }
}

/// Resolve the `--simulate-failure` list. The flag is test-only and gated by
/// `ANODIZE_TEST_HARNESS=1`; production releases that accidentally set the
/// flag get a hard error rather than silent pass-through so the surface
/// cannot be weaponized.
fn resolve_simulate_failure(simulate: &mut Vec<String>) -> Result<Vec<String>> {
    if std::env::var("ANODIZE_TEST_HARNESS").as_deref() == Ok("1") {
        Ok(std::mem::take(simulate))
    } else if !simulate.is_empty() {
        anyhow::bail!(
            "--simulate-failure requires ANODIZE_TEST_HARNESS=1 (test-harness gated flag)"
        );
    } else {
        Ok(Vec::new())
    }
}

/// Translate `--allow-nondeterministic name=reason` (repeatable) into
/// `(name, reason)` tuples. Empty reasons are rejected so the run summary
/// always carries a human-readable justification.
fn parse_allow_nondeterministic(entries: &[String]) -> Result<Vec<(String, String)>> {
    entries
        .iter()
        .map(|s| {
            let (name, reason) = s.split_once('=').ok_or_else(|| {
                anyhow::anyhow!("--allow-nondeterministic must be NAME=REASON, got: {}", s)
            })?;
            if reason.trim().is_empty() {
                anyhow::bail!("--allow-nondeterministic reason cannot be empty for: {}", s);
            }
            Ok::<_, anyhow::Error>((name.to_string(), reason.to_string()))
        })
        .collect()
}

/// Assemble the [`ContextOptions`] from parsed flags + derived state.
/// `resume_release` auto-enables under `--publish-only` so the publish
/// pipeline's `ReleaseStage` and `github-release` publisher target the same
/// tag without tripping the leftover-asset bail.
fn build_context_options(
    opts: &ReleaseOpts,
    skip_stages: Vec<String>,
    selected_sorted: Vec<String>,
    rollback_mode: Option<RollbackMode>,
    simulate_failure_publishers: Vec<String>,
    runtime_nondeterministic_allowlist: Vec<(String, String)>,
) -> ContextOptions {
    ContextOptions {
        snapshot: opts.snapshot,
        nightly: opts.nightly,
        dry_run: opts.dry_run,
        quiet: opts.quiet,
        verbose: opts.verbose,
        debug: opts.debug,
        skip_stages,
        selected_crates: selected_sorted,
        token: opts.token.clone(),
        parallelism: opts.parallelism,
        single_target: opts.single_target.clone(),
        release_notes_path: opts.release_notes.clone(),
        fail_fast: opts.fail_fast,
        partial_target: opts
            .targets
            .clone()
            .map(anodizer_core::partial::PartialTarget::Targets),
        merge: opts.merge,
        publish_only: opts.publish_only,
        project_root: None,
        strict: opts.strict,
        resume_release: opts.resume_release || opts.publish_only,
        replace_existing_artifacts: opts.replace_existing,
        skip_post_publish_poll: opts.no_post_publish_poll,
        gate_submitter: if opts.no_gate_submitter {
            Some(false)
        } else {
            None
        },
        rollback_mode,
        simulate_failure_publishers,
        rollback_only: opts.rollback_only,
        allow_rerun: opts.allow_rerun,
        from_run: opts.from_run.clone(),
        runtime_nondeterministic_allowlist,
        summary_json_path: opts.summary_json.clone(),
        allow_ai_failure: opts.allow_ai_failure,
    }
}

/// `--rollback-only` short-circuits the pipeline: load the prior run's
/// `report.json`, re-attempt rollback for every Succeeded / RollbackFailed
/// entry, persist the result to `rollback.json`, and return. No build /
/// publish / announce stages run in this mode.
///
/// The rollback-only branch bypasses `Pipeline::run` entirely, so it must
/// invoke `emit_summary` itself for `--summary-json=<path>` to land on disk.
/// The call wraps both the rollback dispatch result and the early-error
/// return so the summary fires regardless of how `rollback_only` resolved.
fn run_rollback_only(ctx: &mut Context) -> Result<()> {
    let outcome = (|| -> Result<()> {
        let run_id = ctx
            .options
            .from_run
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--rollback-only requires --from-run=<id>"))?;
        let updated_report = anodizer_stage_publish::rollback_only::run(ctx, &run_id)?;
        ctx.set_publish_report(updated_report);
        Ok(())
    })();
    anodizer_stage_announce::emit_summary(ctx);
    outcome
}

/// Run before-hooks once env AND git vars are populated. Respects
/// `--skip=before` (matches GoReleaser's `skip.Before`). Skipped in
/// `--merge` / `--split` / `--publish-only` modes — CI already validates
/// the code before tagging, and hook compilation can dirty the working
/// tree.
fn run_before_hooks(
    ctx: &Context,
    config: &Config,
    opts: &ReleaseOpts,
    log: &StageLogger,
) -> Result<()> {
    if !opts.merge
        && !opts.split
        && !opts.publish_only
        && !opts.announce_only
        && !ctx.should_skip("before")
        && let Some(before) = &config.before
        && let Some(ref hooks) = before.hooks
    {
        pipeline::run_hooks(
            hooks,
            "before",
            opts.dry_run,
            log,
            Some(ctx.template_vars()),
        )?;
    }
    Ok(())
}

/// Render `--release-notes-tmpl` now that template vars are populated and
/// point `ctx.options.release_notes_path` at the rendered file.
///
/// Skipped in `--publish-only` mode: the preserved dist already contains a
/// `release-notes.md` written by the upstream determinism-harness run from
/// the SAME template vars. Re-rendering here from the current tree could
/// clobber it with a divergent version (e.g. if the local checkout has new
/// commits since the harness ran). The harness-written file is the
/// authoritative one for the preserved bytes.
fn render_release_notes_tmpl(
    ctx: &mut Context,
    config: &Config,
    opts: &ReleaseOpts,
    release_notes_path: Option<(PathBuf, String)>,
    log: &StageLogger,
) -> Result<()> {
    if !opts.publish_only
        && !opts.announce_only
        && let Some((tmpl_path, raw_content)) = release_notes_path
    {
        let rendered = template::render(&raw_content, ctx.template_vars()).with_context(|| {
            format!(
                "failed to render release notes template: {}",
                tmpl_path.display()
            )
        })?;
        let dist = &config.dist;
        std::fs::create_dir_all(dist).ok();
        let rendered_path = dist.join("release-notes.md");
        std::fs::write(&rendered_path, &rendered).with_context(|| {
            format!(
                "failed to write rendered release notes: {}",
                rendered_path.display()
            )
        })?;
        ctx.options.release_notes_path = Some(rendered_path);
        log.verbose("rendered release notes template");
    }
    Ok(())
}

/// Dirty repo gate: error out if the repo has uncommitted changes unless
/// running in snapshot, nightly, or dry-run mode (matches GoReleaser).
fn enforce_dirty_repo_gate(ctx: &Context) -> Result<()> {
    if git::is_git_dirty() && !ctx.is_snapshot() && !ctx.is_nightly() && !ctx.is_dry_run() {
        let status = git::git_status_porcelain();
        anyhow::bail!(
            "git repository is dirty; use --snapshot to release from a dirty tree, or commit your changes first.\n\nDirty files:\n{}",
            status
        );
    }
    Ok(())
}

/// Apply nightly overrides after git vars are populated: render
/// `nightly.version_template` (default mirrors GoReleaser's
/// `"{{ incpatch(v=Version) }}-{{ ShortCommit }}-nightly"`), then override
/// `Version` / `RawVersion` / `Tag` / `IsNightly` / `ReleaseName` template
/// vars. SDE-aware so the harness's two from-clean rebuilds stay
/// byte-stable.
fn apply_nightly_template_vars(
    ctx: &mut Context,
    config: &Config,
    log: &StageLogger,
) -> Result<()> {
    let nightly_cfg = config.nightly.as_ref();

    // `IsNightly` must be set first so `version_template`, `tag_name`,
    // and `name_template` can all branch on `{{ if .IsNightly }}…{{ end }}`
    // when rendered below.
    ctx.template_vars_mut().set("IsNightly", "true");

    // GoReleaser default: `"{{ incpatch(v=Version) }}-{{ ShortCommit }}-nightly"`
    // — bumps the patch component and embeds the commit SHA so two nightly
    // runs at different commits produce distinct, commit-immutable
    // versions. Users can override with `nightly.version_template` to
    // match their own conventions (e.g. embed `{{ .Date }}` for a
    // date-stamped scheme).
    let version_tmpl = nightly_cfg
        .and_then(|c| c.version_template.as_deref())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("{{ incpatch(v=Version) }}-{{ ShortCommit }}-nightly");
    let nightly_version = template::render(version_tmpl, ctx.template_vars())
        .with_context(|| format!("failed to render nightly version_template: {version_tmpl}"))?;
    let nightly_version = nightly_version.trim().to_string();
    if nightly_version.is_empty() {
        anyhow::bail!(
            "nightly version_template rendered to an empty string (template: {version_tmpl})"
        );
    }
    ctx.template_vars_mut().set("Version", &nightly_version);
    ctx.template_vars_mut().set("RawVersion", &nightly_version);

    // GR v2.16 nightly templates `tag_name` (alongside `name_template`).
    // Render after `Version` / `RawVersion` / `IsNightly` are populated so
    // `{{ .Version }}` etc. resolve to the nightly-overridden values rather
    // than the base semver. Default literal "nightly" stays template-safe.
    let tag_tmpl = nightly_cfg
        .and_then(|c| c.tag_name.as_deref())
        .unwrap_or("nightly");
    let nightly_tag = template::render(tag_tmpl, ctx.template_vars())
        .with_context(|| format!("failed to render nightly tag_name: {tag_tmpl}"))?;
    // Trim before both the empty-check and the `Tag` set: a template
    // rendering to whitespace (e.g. `"  edge  "`) would otherwise pass
    // the gate AND store padded whitespace into `Tag`, which GitHub's
    // Releases API rejects.
    let nightly_tag = nightly_tag.trim().to_string();
    if nightly_tag.is_empty() {
        anyhow::bail!(
            "nightly tag_name rendered to an empty string (template: {tag_tmpl}). \
             An empty tag would be rejected by GitHub's Releases API."
        );
    }
    ctx.template_vars_mut().set("Tag", &nightly_tag);

    let name_tmpl = nightly_cfg
        .and_then(|c| c.name_template.as_deref())
        .unwrap_or("{{ ProjectName }}-nightly");
    let release_name = template::render(name_tmpl, ctx.template_vars())
        .with_context(|| format!("failed to render nightly name_template: {name_tmpl}"))?;
    ctx.template_vars_mut().set("ReleaseName", &release_name);

    log.verbose(&format!(
        "nightly: version={}, tag={}, name={}",
        nightly_version, nightly_tag, release_name
    ));
    Ok(())
}

/// Apply the snapshot version template (GoReleaser always applies one).
/// Default: `"{{ Version }}-SNAPSHOT-{{ ShortCommit }}"` when no snapshot
/// config exists. `RawVersion` is intentionally preserved as the numeric
/// semver base (GoReleaser parity).
fn apply_snapshot_template_vars(
    ctx: &mut Context,
    config: &Config,
    log: &StageLogger,
) -> Result<()> {
    let snapshot_tmpl = config
        .snapshot
        .as_ref()
        .map(|s| s.version_template.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("{{ Version }}-SNAPSHOT-{{ ShortCommit }}");
    let rendered_name =
        template::render(snapshot_tmpl, ctx.template_vars()).with_context(|| {
            format!(
                "failed to render snapshot version_template: {}",
                snapshot_tmpl
            )
        })?;
    if rendered_name.trim().is_empty() {
        anyhow::bail!("empty snapshot name after rendering version_template");
    }
    ctx.template_vars_mut().set("Version", &rendered_name);
    ctx.template_vars_mut().set("ReleaseName", &rendered_name);
    log.verbose(&format!(
        "snapshot: version={}, release_name={}",
        rendered_name, rendered_name
    ));
    Ok(())
}

/// Run the pre-flight publisher-state check. Returns `Ok(true)` when
/// `--preflight` (check-only) succeeded and the caller should exit
/// without continuing into the rest of the pipeline; `Ok(false)`
/// otherwise.
///
/// Walks each enabled one-way-door publisher (cargo, choco, winget, aur)
/// and bails early if the target version is already submitted / approved
/// / pending — saving an entire wasted release cycle. Skipped in snapshot
/// / dry-run / split modes (no upstream side-effects) and when `publish`
/// is already in `skip_stages`.
fn run_publisher_preflight(
    ctx: &mut Context,
    opts: &ReleaseOpts,
    log: &StageLogger,
) -> Result<bool> {
    // Preflight probes publisher state ahead of a publish; an already-published
    // release has no pending one-way-door transitions to guard against.
    if opts.announce_only {
        log.status("preflight skipped: --announce-only does not publish");
        return Ok(false);
    }
    let should_run_preflight = should_run_preflight_auto(
        opts.no_preflight,
        opts.snapshot,
        opts.dry_run,
        opts.split,
        opts.publish_only,
        ctx.should_skip("publish"),
    );
    if !(opts.preflight || should_run_preflight) {
        return Ok(false);
    }

    let report = anodizer_stage_publish::preflight::run_preflight(ctx, log)?;
    if report.entries.is_empty() {
        log.verbose("preflight: no one-way-door publishers configured; skipping check");
    } else {
        // Route the report through the stage logger (same channel as every
        // other status string in this function) instead of a raw `print!` so
        // verbosity / quiet flags / future redirection apply uniformly. The
        // Display impl is multi-line; splitting line-by-line preserves the
        // single-line cadence used by surrounding `log.status` calls.
        for line in report.to_string().trim_end_matches('\n').lines() {
            log.status(line);
        }
    }
    // `--strict` already plumbs strict mode globally; treat it as implying
    // preflight-strict. `--strict-preflight` is kept as an explicit alias for
    // back-compat with anyone who already plumbed it through their CI.
    let strict_preflight = opts.strict || opts.strict_preflight;
    if report.has_blockers(strict_preflight) {
        let blockers = report.blockers(strict_preflight);
        let labels: Vec<String> = blockers
            .iter()
            .map(|b| format!("{} ({})", b.publisher, b.state.label()))
            .collect();
        anyhow::bail!(
            "preflight: {} publisher(s) blocked the release: {}. \
             Resolve upstream (await moderation / merge or close the PR / bump version) \
             or re-run with --no-preflight to override.",
            blockers.len(),
            labels.join(", ")
        );
    }
    // Resilience-extension blockers (rollback-scope checks +
    // `Publisher::preflight()` returns) live in their own channel; bail when
    // any is present so the operator sees the problem before the pipeline
    // starts.
    if !report.blockers.is_empty() {
        anyhow::bail!(
            "preflight: {} resilience blocker(s): {}",
            report.blockers.len(),
            report.blockers.join("; "),
        );
    }
    log.status(&format!(
        "preflight: {} publisher(s) clean",
        report.clean_count()
    ));
    // `--preflight` is a check-only mode: signal early-exit to the caller.
    if opts.preflight { Ok(true) } else { Ok(false) }
}

/// End-of-pipeline gate: bail when any *required* publisher finished in a
/// failure state, so the CLI exits non-zero even though the pipeline body
/// returned Ok.
///
/// "Failure state" here counts both `Failed(_)` (publish itself failed)
/// and `RollbackFailed(_)` (publish ran, rollback was attempted, and the
/// rollback also failed — leaving the operator with a half-published
/// surface that needs manual intervention). Either way, a downstream
/// shell / CI caller MUST see a non-zero exit.
///
/// **Snapshot / dry-run skip**: publishers don't actually run in either
/// mode, so `required_failures` should already be 0; the explicit skip
/// is defense-in-depth in case a future stage starts recording
/// publisher results in those modes (e.g. for `--snapshot` evidence
/// preview).
pub(crate) fn gate_required_failures(ctx: &Context) -> Result<()> {
    if ctx.is_snapshot() || ctx.is_dry_run() {
        return Ok(());
    }
    let Some(report) = ctx.publish_report.as_ref() else {
        return Ok(());
    };
    let failed: Vec<&str> = report
        .results
        .iter()
        .filter(|r| {
            r.required
                && matches!(
                    r.outcome,
                    anodizer_core::publish_report::PublisherOutcome::Failed(_)
                        | anodizer_core::publish_report::PublisherOutcome::RollbackFailed(_)
                )
        })
        .map(|r| r.name.as_str())
        .collect();
    if failed.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "release pipeline finished but {} required publisher(s) failed: {}. \
         The pipeline ran to completion so rollback / announce-gating / \
         summary all observed final state; this non-zero exit ensures CI \
         and shell callers see the failure. Inspect dist/run-<id>/report.json \
         for details and use --rollback-only --from-run=<id> to retry rollback.",
        failed.len(),
        failed.join(", ")
    );
}

/// Post-pipeline tasks: metadata writing, publishers, after hooks.
fn run_post_pipeline(
    ctx: &mut Context,
    config: &Config,
    dry_run: bool,
    log: &anodizer_core::log::StageLogger,
) -> Result<()> {
    // Print artifact size table if configured
    helpers::run_report_sizes(ctx, config, log);

    // Write metadata.json and artifacts.json (GoReleaser writes these
    // even in dry-run mode; applies metadata.mod_timestamp when set).
    helpers::write_metadata_and_artifacts(ctx, config, log)?;

    // Run custom publishers
    if let Some(ref publishers) = config.publishers
        && !publishers.is_empty()
    {
        log.status("running custom publishers...");
        super::publisher::run_publishers(
            publishers,
            ctx.artifacts.all(),
            ctx.template_vars(),
            dry_run,
            log,
            ctx.options.parallelism,
            Some(&ctx.skip_memento),
        )?;
    }

    // Close milestones (skipped on nightly per GoReleaser parity — a
    // rolling nightly tag does not correspond to a milestone closure).
    if let Some(ref milestones) = config.milestones {
        if ctx.is_nightly() {
            log.status("milestone close skipped — nightly run (GoReleaser parity)");
        } else {
            milestones::close_milestones(milestones, ctx, dry_run, log)?;
        }
    }

    run_post_pipeline_after_hooks_only(ctx, config, dry_run, log)
}

/// Run only the user-defined `after:` hooks. Extracted so
/// `--announce-only` can fire them post-announce without re-running
/// custom publishers / milestones / metadata writes (which already
/// fired during the prior end-to-end run).
///
/// Canonical key is `after.hooks:` (GoReleaser Pro). The legacy
/// `after.post:` spelling is folded into `hooks:` at config-parse
/// time by `HooksConfig::merge_hook_aliases`, so this reader only
/// needs the canonical field.
///
/// Note on `--merge` interaction: `before:` hooks deliberately skip on
/// merge (see `run_before_hooks`) because the shards already compiled.
/// `after:` hooks intentionally DO run on merge — the shard pipeline
/// (`build_split_pipeline`) only executes the build stage and never
/// reaches `run_post_pipeline`, so the merge step is the only point at
/// which the user's post-release notifications / cleanup hooks fire.
/// Skipping them here would mean they never run.
pub(super) fn run_post_pipeline_after_hooks_only(
    ctx: &Context,
    config: &Config,
    dry_run: bool,
    log: &anodizer_core::log::StageLogger,
) -> Result<()> {
    if let Some(after) = &config.after
        && let Some(ref hooks) = after.hooks
    {
        pipeline::run_hooks(hooks, "after", dry_run, log, Some(ctx.template_vars()))?;
    }

    Ok(())
}

/// Detect which crates have changes since their last tag.
pub(crate) fn detect_changed_crates_pub(
    crates: &[CrateConfig],
    git_config: Option<&anodizer_core::config::GitConfig>,
    monorepo_prefix: Option<&str>,
    log: &StageLogger,
) -> Result<Vec<String>> {
    detect_changed_crates(crates, git_config, monorepo_prefix, log)
}

fn detect_changed_crates(
    crates: &[CrateConfig],
    git_config: Option<&anodizer_core::config::GitConfig>,
    monorepo_prefix: Option<&str>,
    log: &StageLogger,
) -> Result<Vec<String>> {
    // Log when ignore_tags/ignore_tag_prefixes contain template expressions
    // but template_vars are not yet available (we pass None below).
    if let Some(gc) = git_config {
        let has_templates = gc
            .ignore_tags
            .as_ref()
            .is_some_and(|tags| tags.iter().any(|t| t.contains("{{")))
            || gc
                .ignore_tag_prefixes
                .as_ref()
                .is_some_and(|pfx| pfx.iter().any(|p| p.contains("{{")));
        if has_templates {
            log.debug(
                "note: ignore_tags/ignore_tag_prefixes templates not rendered during \
                 change detection (template vars not yet available)",
            );
        }
    }

    let mut changed = vec![];
    let mut oldest_tag: Option<String> = None;

    for c in crates {
        let latest_tag = git::find_latest_tag_matching_with_prefix(
            &c.tag_template,
            git_config,
            None,
            monorepo_prefix,
        )?;
        match &latest_tag {
            None => {
                // No tag at all → always include
                changed.push(c.name.clone());
            }
            Some(tag) => {
                if git::has_changes_since(tag, &c.path)? {
                    changed.push(c.name.clone());
                }
                // Track the earliest tag for workspace-level check
                if let Ok(sv) = git::parse_semver_tag(tag) {
                    let is_older = oldest_tag
                        .as_ref()
                        .and_then(|t| git::parse_semver_tag(t).ok())
                        .is_none_or(|osv| sv < osv);
                    if is_older {
                        oldest_tag = Some(tag.clone());
                    }
                }
            }
        }
    }

    // Propagate changes transitively via depends_on: if crate B depends on
    // changed crate A, include B too. Use a fixed-point loop.
    changed = propagate_dependents(crates, changed);

    // Check workspace-level files against the oldest tag
    if let Some(ref tag) = oldest_tag {
        let ws_changed = check_workspace_files_changed(tag)?;
        if ws_changed {
            // Include all crates
            return Ok(crates.iter().map(|c| c.name.clone()).collect());
        }
    }

    Ok(changed)
}

/// Transitively propagate changed crates via `depends_on`.
///
/// If crate B depends on changed crate A, B is also included. Repeats until
/// the set stabilises (fixed-point loop).
fn propagate_dependents(crates: &[CrateConfig], changed: Vec<String>) -> Vec<String> {
    use std::collections::HashSet;

    let changed_set: HashSet<String> = changed.iter().cloned().collect();
    let mut result_set = changed_set;

    loop {
        let mut added = false;
        for c in crates {
            if result_set.contains(&c.name) {
                continue;
            }
            if let Some(deps) = &c.depends_on
                && deps.iter().any(|dep| result_set.contains(dep))
            {
                result_set.insert(c.name.clone());
                added = true;
            }
        }
        if !added {
            break;
        }
    }

    // Preserve original order from `changed`, then append newly added crates
    let mut propagated: Vec<String> = Vec::new();
    for name in &changed {
        if result_set.contains(name) {
            propagated.push(name.clone());
        }
    }
    for c in crates {
        if result_set.contains(&c.name) && !changed.contains(&c.name) {
            propagated.push(c.name.clone());
        }
    }
    propagated
}

/// Check if workspace-level files (Cargo.toml, Cargo.lock) changed since tag.
fn check_workspace_files_changed(tag: &str) -> Result<bool> {
    anodizer_core::git::paths_changed_since_tag(tag, &["Cargo.toml", "Cargo.lock"])
}

/// Resolve a workspace by name from the config. Returns an error if
/// `workspaces` is not configured or the given name is not found.
pub fn resolve_workspace<'a>(config: &'a Config, name: &str) -> Result<&'a WorkspaceConfig> {
    let workspaces = config.workspaces.as_ref().ok_or_else(|| {
        anyhow::anyhow!("--workspace specified but no workspaces defined in config")
    })?;

    workspaces.iter().find(|ws| ws.name == name).ok_or_else(|| {
        let available: Vec<&str> = workspaces.iter().map(|ws| ws.name.as_str()).collect();
        anyhow::anyhow!(
            "workspace '{}' not found (available: {})",
            name,
            available.join(", ")
        )
    })
}

/// Topologically sort the selected crates respecting depends_on order.
fn topo_sort_selected(all_crates: &[CrateConfig], selected: &[String]) -> Vec<String> {
    let selected_set: std::collections::HashSet<&str> =
        selected.iter().map(|s| s.as_str()).collect();

    let items: Vec<(String, Vec<String>)> = all_crates
        .iter()
        .filter(|c| selected_set.contains(c.name.as_str()))
        .map(|c| (c.name.clone(), c.depends_on.clone().unwrap_or_default()))
        .collect();

    anodizer_core::util::topological_sort(&items)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::{CrateConfig, NightlyConfig, WorkspaceConfig};

    fn make_crate(name: &str, deps: Option<Vec<&str>>) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: format!("{}-v{{{{ .Version }}}}", name),
            depends_on: deps.map(|d| d.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        }
    }

    fn make_config_with_workspaces(workspaces: Vec<WorkspaceConfig>) -> Config {
        Config {
            project_name: "test".to_string(),
            workspaces: Some(workspaces),
            ..Default::default()
        }
    }

    #[test]
    fn test_resolve_workspace_found() {
        let config = make_config_with_workspaces(vec![
            WorkspaceConfig {
                name: "frontend".to_string(),
                crates: vec![make_crate("fe-app", None)],
                ..Default::default()
            },
            WorkspaceConfig {
                name: "backend".to_string(),
                crates: vec![make_crate("be-api", None)],
                ..Default::default()
            },
        ]);
        let ws = resolve_workspace(&config, "backend").unwrap();
        assert_eq!(ws.name, "backend");
        assert_eq!(ws.crates.len(), 1);
        assert_eq!(ws.crates[0].name, "be-api");
    }

    #[test]
    fn test_resolve_workspace_not_found() {
        let config = make_config_with_workspaces(vec![WorkspaceConfig {
            name: "frontend".to_string(),
            crates: vec![make_crate("fe-app", None)],
            ..Default::default()
        }]);
        let result = resolve_workspace(&config, "nonexistent");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("nonexistent"),
            "error should mention the workspace name: {}",
            msg
        );
        assert!(
            msg.contains("frontend"),
            "error should list available workspaces: {}",
            msg
        );
    }

    #[test]
    fn test_resolve_workspace_no_workspaces_defined() {
        let config = Config {
            project_name: "test".to_string(),
            ..Default::default()
        };
        let result = resolve_workspace(&config, "anything");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("no workspaces defined"),
            "error should say no workspaces defined: {}",
            msg
        );
    }

    #[test]
    fn test_topo_sort_selected_respects_order() {
        let all = vec![
            make_crate("a", None),
            make_crate("b", Some(vec!["a"])),
            make_crate("c", Some(vec!["b"])),
        ];
        let selected = vec!["c".to_string(), "b".to_string(), "a".to_string()];
        let sorted = topo_sort_selected(&all, &selected);
        assert_eq!(sorted, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_topo_sort_selected_partial() {
        let all = vec![
            make_crate("a", None),
            make_crate("b", Some(vec!["a"])),
            make_crate("c", None),
        ];
        // Only select b and c (not a)
        let selected = vec!["b".to_string(), "c".to_string()];
        let sorted = topo_sort_selected(&all, &selected);
        // b has no selected deps, c has no deps — both should appear
        assert!(sorted.contains(&"b".to_string()));
        assert!(sorted.contains(&"c".to_string()));
        assert!(!sorted.contains(&"a".to_string()));
    }

    #[test]
    fn test_topo_sort_all_selected() {
        let all = vec![
            make_crate("core", None),
            make_crate("lib", Some(vec!["core"])),
            make_crate("cli", Some(vec!["lib", "core"])),
        ];
        let selected: Vec<String> = all.iter().map(|c| c.name.clone()).collect();
        let sorted = topo_sort_selected(&all, &selected);
        let core_pos = sorted.iter().position(|s| s == "core").unwrap();
        let lib_pos = sorted.iter().position(|s| s == "lib").unwrap();
        let cli_pos = sorted.iter().position(|s| s == "cli").unwrap();
        assert!(core_pos < lib_pos);
        assert!(core_pos < cli_pos);
        assert!(lib_pos < cli_pos);
    }

    /// Verify workspace overlay semantics:
    /// - `env` merges additively (workspace env adds to / overrides top-level env)
    /// - `signs` replaces top-level signs when workspace has its own
    /// - `changelog` replaces top-level changelog when workspace has its own
    #[test]
    fn test_workspace_overlay_semantics() {
        use anodizer_core::config::{ChangelogConfig, SignConfig};

        // Build a top-level config with env, signs, and changelog
        let mut config = Config {
            project_name: "test".to_string(),
            crates: vec![make_crate("top-crate", None)],
            env: Some(vec![
                "SHARED=from-top".to_string(),
                "TOP_ONLY=top-value".to_string(),
            ]),
            signs: vec![SignConfig {
                cmd: Some("gpg".to_string()),
                ..Default::default()
            }],
            changelog: Some(ChangelogConfig {
                sort: Some("asc".to_string()),
                ..Default::default()
            }),
            workspaces: Some(vec![WorkspaceConfig {
                name: "ws".to_string(),
                crates: vec![make_crate("ws-crate", None)],
                env: Some(vec![
                    "SHARED=from-ws".to_string(),
                    "WS_ONLY=ws-value".to_string(),
                ]),
                signs: vec![SignConfig {
                    cmd: Some("cosign".to_string()),
                    ..Default::default()
                }],
                changelog: Some(ChangelogConfig {
                    sort: Some("desc".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            ..Default::default()
        };

        // Apply the overlay using the shared helper
        let ws = config
            .workspaces
            .as_ref()
            .unwrap()
            .iter()
            .find(|w| w.name == "ws")
            .unwrap()
            .clone();
        helpers::apply_workspace_overlay(&mut config, &ws);

        // Verify crates were replaced
        assert_eq!(config.crates.len(), 1);
        assert_eq!(config.crates[0].name, "ws-crate");

        // Verify env merged additively: TOP_ONLY preserved, SHARED and WS_ONLY added from workspace
        let env = config.env.as_ref().unwrap();
        assert!(
            env.contains(&"TOP_ONLY=top-value".to_string()),
            "top-level-only key should be preserved"
        );
        assert!(
            env.contains(&"SHARED=from-ws".to_string()),
            "workspace SHARED entry should be present"
        );
        assert!(
            env.contains(&"WS_ONLY=ws-value".to_string()),
            "workspace-only key should be added"
        );

        // Verify signs were replaced (not merged)
        assert_eq!(config.signs.len(), 1);
        assert_eq!(
            config.signs[0].cmd.as_deref(),
            Some("cosign"),
            "signs should be replaced by workspace"
        );

        // Verify changelog was replaced
        let cl = config.changelog.as_ref().unwrap();
        assert_eq!(
            cl.sort.as_deref(),
            Some("desc"),
            "changelog should be replaced by workspace"
        );
    }

    // ---- depends_on propagation tests ----

    #[test]
    fn test_propagate_dependents_direct() {
        // B depends on A. If A changed, B should be included too.
        let crates = vec![
            make_crate("a", None),
            make_crate("b", Some(vec!["a"])),
            make_crate("c", None),
        ];
        let changed = vec!["a".to_string()];
        let result = propagate_dependents(&crates, changed);
        assert!(result.contains(&"a".to_string()));
        assert!(result.contains(&"b".to_string()));
        assert!(!result.contains(&"c".to_string()));
    }

    #[test]
    fn test_propagate_dependents_transitive() {
        // C depends on B, B depends on A. If A changed, both B and C should be included.
        let crates = vec![
            make_crate("a", None),
            make_crate("b", Some(vec!["a"])),
            make_crate("c", Some(vec!["b"])),
        ];
        let changed = vec!["a".to_string()];
        let result = propagate_dependents(&crates, changed);
        assert!(result.contains(&"a".to_string()));
        assert!(result.contains(&"b".to_string()));
        assert!(result.contains(&"c".to_string()));
    }

    #[test]
    fn test_propagate_dependents_no_deps() {
        let crates = vec![make_crate("a", None), make_crate("b", None)];
        let changed = vec!["a".to_string()];
        let result = propagate_dependents(&crates, changed);
        assert_eq!(result, vec!["a".to_string()]);
    }

    #[test]
    fn test_propagate_dependents_preserves_order() {
        let crates = vec![
            make_crate("a", None),
            make_crate("b", Some(vec!["a"])),
            make_crate("c", Some(vec!["a"])),
        ];
        let changed = vec!["a".to_string()];
        let result = propagate_dependents(&crates, changed);
        // a should come first (from original changed), then b and c (propagated, in crate order)
        assert_eq!(result[0], "a");
        assert!(result.contains(&"b".to_string()));
        assert!(result.contains(&"c".to_string()));
    }

    // -----------------------------------------------------------------------
    // CLI flag override tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_draft_flag_sets_release_config_draft() {
        // Start with a config that has no release config
        let mut config = Config {
            project_name: "test".to_string(),
            ..Default::default()
        };
        assert!(config.release.is_none());

        // Simulate what the release command does when --draft is true
        let release = config.release.get_or_insert_with(Default::default);
        release.draft = Some(true);

        assert_eq!(config.release.as_ref().unwrap().draft, Some(true));
    }

    #[test]
    fn test_draft_flag_overrides_existing_config() {
        use anodizer_core::config::ReleaseConfig;

        // Start with a config that has draft=false
        let mut config = Config {
            project_name: "test".to_string(),
            release: Some(ReleaseConfig {
                draft: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        };

        // Simulate --draft CLI override
        let release = config.release.get_or_insert_with(Default::default);
        release.draft = Some(true);

        assert_eq!(
            config.release.as_ref().unwrap().draft,
            Some(true),
            "CLI --draft should override config draft=false"
        );
    }

    // --- `--prepare` flag (GoReleaser Pro) ---

    #[test]
    fn test_apply_prepare_mode_to_skip_from_empty() {
        let mut skip: Vec<String> = Vec::new();
        apply_prepare_mode_to_skip(&mut skip);
        assert_eq!(
            skip,
            vec![
                "release".to_string(),
                "publish".to_string(),
                "blob".to_string(),
                "snapcraft-publish".to_string(),
                "announce".to_string(),
            ],
            "--prepare on empty skip should add all network-touching upstream stages"
        );
    }

    #[test]
    fn test_apply_prepare_mode_to_skip_preserves_user_skip() {
        let mut skip = vec!["docker".to_string(), "sign".to_string()];
        apply_prepare_mode_to_skip(&mut skip);
        assert!(
            skip.contains(&"docker".to_string()) && skip.contains(&"sign".to_string()),
            "existing user skips must be preserved"
        );
        for stage in [
            "release",
            "publish",
            "blob",
            "snapcraft-publish",
            "announce",
        ] {
            assert!(
                skip.contains(&stage.to_string()),
                "--prepare must add {stage} alongside user skips"
            );
        }
    }

    #[test]
    fn test_apply_prepare_mode_to_skip_composes_with_snapshot_marker() {
        // `--prepare --snapshot` must produce a skip list that includes all
        // network-touching stages, independent of any snapshot-only entries a
        // caller may have pre-added. The augmentation is purely additive —
        // snapshot semantics remain owned by the snapshot flag.
        let mut skip = vec!["sign".to_string()];
        apply_prepare_mode_to_skip(&mut skip);
        for stage in [
            "release",
            "publish",
            "blob",
            "snapcraft-publish",
            "announce",
        ] {
            assert!(
                skip.iter().any(|s| s == stage),
                "--prepare must add {stage} regardless of snapshot composition"
            );
        }
        assert!(
            skip.iter().any(|s| s == "sign"),
            "user-specified skip survives composition"
        );
    }

    #[test]
    fn test_apply_prepare_mode_to_skip_is_idempotent() {
        let mut skip = vec![
            "release".to_string(),
            "publish".to_string(),
            "blob".to_string(),
        ];
        apply_prepare_mode_to_skip(&mut skip);
        // No duplicates for stages that were pre-populated.
        let release_count = skip.iter().filter(|s| s.as_str() == "release").count();
        let publish_count = skip.iter().filter(|s| s.as_str() == "publish").count();
        let blob_count = skip.iter().filter(|s| s.as_str() == "blob").count();
        assert_eq!(release_count, 1, "no duplicate release");
        assert_eq!(publish_count, 1, "no duplicate publish");
        assert_eq!(blob_count, 1, "no duplicate blob");
        assert!(skip.contains(&"announce".to_string()));
        assert!(skip.contains(&"snapcraft-publish".to_string()));
    }

    // ---- preflight auto-run gating ---------------------------------------

    #[test]
    fn should_run_preflight_auto_default_runs() {
        // No flag set → run.
        assert!(should_run_preflight_auto(
            false, false, false, false, false, false
        ));
    }

    #[test]
    fn should_run_preflight_auto_no_preflight_skips() {
        assert!(!should_run_preflight_auto(
            true, false, false, false, false, false
        ));
    }

    #[test]
    fn should_run_preflight_auto_snapshot_skips() {
        assert!(!should_run_preflight_auto(
            false, true, false, false, false, false
        ));
    }

    #[test]
    fn should_run_preflight_auto_dry_run_skips() {
        assert!(!should_run_preflight_auto(
            false, false, true, false, false, false
        ));
    }

    #[test]
    fn should_run_preflight_auto_split_skips() {
        assert!(!should_run_preflight_auto(
            false, false, false, true, false, false
        ));
    }

    #[test]
    fn should_run_preflight_auto_publish_only_skips() {
        // `--publish-only` must skip the publisher-state preflight so
        // the credential preflight (which lives inside
        // `publish_only::run`) gets first crack at bailing before any
        // network call.
        assert!(!should_run_preflight_auto(
            false, false, false, false, true, false
        ));
    }

    #[test]
    fn should_run_preflight_auto_publish_skipped_skips() {
        assert!(!should_run_preflight_auto(
            false, false, false, false, false, true
        ));
    }

    /// `--strict-preflight` is folded into `--strict`: either flag (or both)
    /// must promote Unknown to a blocker, none of them leaves Unknown
    /// non-blocking. The combiner is a one-liner in the call site but it's
    /// the gating contract a CI script relies on, so pin it.
    #[test]
    fn strict_or_strict_preflight_promotes_unknown_to_blocker() {
        use anodizer_core::preflight::{PreflightEntry, PreflightReport, PublisherState};

        let mut report = PreflightReport::new();
        report.push(PreflightEntry {
            publisher: "aur".into(),
            package: "foo".into(),
            version: "1.0.0".into(),
            state: PublisherState::Unknown {
                reason: "timeout".into(),
            },
        });

        // Combiner used in the call site (`opts.strict || opts.strict_preflight`).
        let combine = |strict: bool, strict_pref: bool| strict || strict_pref;
        assert!(!report.has_blockers(combine(false, false)));
        assert!(report.has_blockers(combine(true, false)));
        assert!(report.has_blockers(combine(false, true)));
        assert!(report.has_blockers(combine(true, true)));
    }

    // ---- gate_required_failures -----------------------------------------

    /// Build a `Context` with a `publish_report` containing a single
    /// publisher result with the given outcome and `required` flag.
    fn ctx_with_report(
        name: &str,
        required: bool,
        outcome: anodizer_core::publish_report::PublisherOutcome,
        opts: ContextOptions,
    ) -> Context {
        use anodizer_core::publish_report::{PublishReport, PublisherGroup, PublisherResult};

        let mut ctx = Context::new(Config::default(), opts);
        let mut report = PublishReport::default();
        report.results.push(PublisherResult {
            name: name.to_string(),
            group: PublisherGroup::Manager,
            required,
            outcome,
            evidence: None,
        });
        ctx.set_publish_report(report);
        ctx
    }

    #[test]
    fn release_exits_nonzero_when_required_publisher_failed() {
        use anodizer_core::publish_report::PublisherOutcome;

        let ctx = ctx_with_report(
            "homebrew",
            true,
            PublisherOutcome::Failed("git push refused".into()),
            ContextOptions::default(),
        );
        let err = gate_required_failures(&ctx).expect_err("must error");
        let msg = format!("{err}");
        assert!(msg.contains("homebrew"), "error names publisher: {msg}");
        assert!(
            msg.contains("required publisher"),
            "error mentions required: {msg}"
        );
    }

    #[test]
    fn release_exits_zero_when_no_required_failures() {
        use anodizer_core::publish_report::{
            PublishReport, PublisherGroup, PublisherOutcome, PublisherResult,
        };

        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        let mut report = PublishReport::default();
        report.results.push(PublisherResult {
            name: "homebrew".to_string(),
            group: PublisherGroup::Manager,
            required: true,
            outcome: PublisherOutcome::Succeeded,
            evidence: None,
        });
        // A *non*-required publisher that failed must NOT trip the gate.
        report.results.push(PublisherResult {
            name: "scoop".to_string(),
            group: PublisherGroup::Manager,
            required: false,
            outcome: PublisherOutcome::Failed("network".to_string()),
            evidence: None,
        });
        ctx.set_publish_report(report);

        gate_required_failures(&ctx).expect("must succeed");
    }

    #[test]
    fn release_required_failures_gate_skipped_in_snapshot() {
        use anodizer_core::publish_report::PublisherOutcome;

        let opts = ContextOptions {
            snapshot: true,
            ..Default::default()
        };
        let ctx = ctx_with_report(
            "homebrew",
            true,
            PublisherOutcome::Failed("boom".into()),
            opts,
        );
        // Snapshot mode skips the gate (defense-in-depth — publishers
        // shouldn't run in snapshot mode at all).
        gate_required_failures(&ctx).expect("snapshot must short-circuit gate");
    }

    #[test]
    fn release_required_failures_gate_skipped_in_dry_run() {
        use anodizer_core::publish_report::PublisherOutcome;

        let opts = ContextOptions {
            dry_run: true,
            ..Default::default()
        };
        let ctx = ctx_with_report(
            "homebrew",
            true,
            PublisherOutcome::Failed("boom".into()),
            opts,
        );
        gate_required_failures(&ctx).expect("dry-run must short-circuit gate");
    }

    #[test]
    fn release_required_failures_counts_rollback_failed() {
        use anodizer_core::publish_report::PublisherOutcome;

        // A rolled-back-failed required publisher leaves the operator
        // with a half-published surface — must also produce non-zero exit.
        let ctx = ctx_with_report(
            "homebrew",
            true,
            PublisherOutcome::RollbackFailed("manual cleanup required".into()),
            ContextOptions::default(),
        );
        let err = gate_required_failures(&ctx).expect_err("rollback-failed must error");
        let msg = format!("{err}");
        assert!(msg.contains("homebrew"), "names publisher: {msg}");
    }

    #[test]
    fn release_required_failures_ignored_when_not_required() {
        use anodizer_core::publish_report::PublisherOutcome;

        // `required: false` + Failed must NOT trip the gate.
        let ctx = ctx_with_report(
            "scoop",
            false,
            PublisherOutcome::Failed("boom".into()),
            ContextOptions::default(),
        );
        gate_required_failures(&ctx).expect("optional failure must not gate");
    }

    #[test]
    fn release_required_failures_noop_without_report() {
        // No publish_report on the context at all (publish stage didn't
        // run, e.g. preflight-only) → gate is a no-op.
        let ctx = Context::new(Config::default(), ContextOptions::default());
        gate_required_failures(&ctx).expect("missing report must short-circuit");
    }

    // ---- apply_nightly_template_vars ------------------------------------
    //
    // GR v2.16 nightly: `tag_name` accepts template syntax (e.g.
    // `nightly-{{ .Version }}`) and is rendered AFTER `Version` /
    // `RawVersion` / `IsNightly` are populated, so user templates that
    // reference those vars resolve to the nightly-overridden values.

    fn make_nightly_log() -> StageLogger {
        StageLogger::new("test-nightly", anodizer_core::log::Verbosity::Quiet)
    }

    /// Shared scaffolding for the `apply_nightly_template_vars` tests:
    /// `project_name="myproj"` config (with the caller-supplied
    /// `tag_name`, or no `nightly` block at all when `tag_name` is
    /// `None`), a fresh `Context`, and `Version` / `ProjectName` /
    /// `ShortCommit` pre-populated (the GR default version_template
    /// references `ShortCommit`).
    fn setup_nightly_ctx(tag_name: Option<&str>, version: &str) -> (Config, Context) {
        let config = Config {
            project_name: "myproj".to_string(),
            nightly: tag_name.map(|t| NightlyConfig {
                tag_name: Some(t.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = Context::new(config.clone(), ContextOptions::default());
        ctx.template_vars_mut().set("Version", version);
        ctx.template_vars_mut().set("ProjectName", "myproj");
        ctx.template_vars_mut().set("ShortCommit", "abc123d");
        (config, ctx)
    }

    #[test]
    fn nightly_tag_name_default_is_literal_nightly() {
        let (config, mut ctx) = setup_nightly_ctx(None, "1.2.3");
        apply_nightly_template_vars(&mut ctx, &config, &make_nightly_log()).unwrap();
        assert_eq!(
            ctx.template_vars().get("Tag").map(String::as_str),
            Some("nightly")
        );
    }

    #[test]
    fn nightly_default_version_uses_incpatch_and_short_commit() {
        let (config, mut ctx) = setup_nightly_ctx(None, "1.2.3");
        apply_nightly_template_vars(&mut ctx, &config, &make_nightly_log()).unwrap();
        assert_eq!(
            ctx.template_vars().get("Version").map(String::as_str),
            Some("1.2.4-abc123d-nightly"),
            "GR-default nightly version: incpatch(1.2.3)-abc123d-nightly",
        );
        assert_eq!(
            ctx.template_vars().get("RawVersion").map(String::as_str),
            Some("1.2.4-abc123d-nightly"),
        );
    }

    #[test]
    fn nightly_version_template_user_override() {
        let config = Config {
            project_name: "myproj".to_string(),
            nightly: Some(NightlyConfig {
                version_template: Some("{{ Version }}-edge-{{ ShortCommit }}".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = Context::new(config.clone(), ContextOptions::default());
        ctx.template_vars_mut().set("Version", "2.0.0");
        ctx.template_vars_mut().set("ProjectName", "myproj");
        ctx.template_vars_mut().set("ShortCommit", "deadbee");
        apply_nightly_template_vars(&mut ctx, &config, &make_nightly_log()).unwrap();
        assert_eq!(
            ctx.template_vars().get("Version").map(String::as_str),
            Some("2.0.0-edge-deadbee"),
        );
    }

    #[test]
    fn nightly_tag_name_renders_version_template() {
        let (config, mut ctx) = setup_nightly_ctx(Some("nightly-{{ .Version }}"), "1.2.3");
        apply_nightly_template_vars(&mut ctx, &config, &make_nightly_log()).unwrap();
        // `{{ .Version }}` resolves to the nightly-overridden value (now
        // `1.2.4-abc123d-nightly`), not the base "1.2.3" — proving the
        // tag template is evaluated LATE, after Version is rewritten.
        let tag = ctx.template_vars().get("Tag").cloned().unwrap_or_default();
        assert_eq!(tag, "nightly-1.2.4-abc123d-nightly");
    }

    #[test]
    fn nightly_tag_name_can_use_is_nightly_branch() {
        let (config, mut ctx) = setup_nightly_ctx(
            Some("{{ if .IsNightly }}edge{{ else }}stable{{ end }}"),
            "0.1.0",
        );
        apply_nightly_template_vars(&mut ctx, &config, &make_nightly_log()).unwrap();
        assert_eq!(
            ctx.template_vars().get("Tag").map(String::as_str),
            Some("edge")
        );
    }

    #[test]
    fn nightly_tag_name_empty_render_bails() {
        let (config, mut ctx) = setup_nightly_ctx(Some("   "), "0.1.0");
        let err = apply_nightly_template_vars(&mut ctx, &config, &make_nightly_log())
            .expect_err("blank tag_name must bail");
        assert!(
            err.to_string().contains("empty"),
            "error should mention empty: {err}",
        );
    }

    // ---- map_head_tags_to_crates unit tests --------------------------------

    fn make_log() -> StageLogger {
        StageLogger::new(
            "test",
            anodizer_core::log::Verbosity::from_flags(true, false, false),
        )
    }

    #[test]
    fn map_head_tags_empty_returns_empty() {
        // No tags at HEAD → empty selection.
        let crates = vec![make_crate("app", None)];
        let log = make_log();
        // Simulate get_tags_at_head returning empty by calling with an empty list.
        // We test the core matching logic directly.
        let head_tags: &[String] = &[];
        let selected = run_tag_mapping(&crates, head_tags);
        assert!(selected.is_empty(), "no tags → empty selection");
        let _ = log;
    }

    #[test]
    fn map_head_tags_single_tag_matches_single_crate() {
        let crates = vec![
            make_crate_with_template("core", "crates/core", "core-v{{ .Version }}"),
            make_crate_with_template("cli", "crates/cli", "v{{ .Version }}"),
        ];
        let head_tags = vec!["core-v1.2.3".to_string()];
        let selected = run_tag_mapping(&crates, &head_tags);
        assert_eq!(selected, vec!["core"]);
    }

    #[test]
    fn map_head_tags_multiple_tags_maps_multiple_crates() {
        let crates = vec![
            make_crate_with_template("core", "crates/core", "core-v{{ .Version }}"),
            make_crate_with_template("cli", "crates/cli", "v{{ .Version }}"),
        ];
        let head_tags = vec!["core-v1.2.3".to_string(), "v1.2.3".to_string()];
        let selected = run_tag_mapping(&crates, &head_tags);
        assert!(selected.contains(&"core".to_string()));
        assert!(selected.contains(&"cli".to_string()));
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn map_head_tags_longer_prefix_wins() {
        // "core-v" is more specific than "v"; only "core" should match.
        let crates = vec![
            make_crate_with_template("app", ".", "v{{ .Version }}"),
            make_crate_with_template("core", "crates/core", "core-v{{ .Version }}"),
        ];
        let head_tags = vec!["core-v0.5.0".to_string()];
        let selected = run_tag_mapping(&crates, &head_tags);
        assert_eq!(selected, vec!["core"], "longer prefix must win");
    }

    #[test]
    fn map_head_tags_topo_sort_respects_depends_on() {
        // core → cli; both tags present; cli depends on core.
        // After topo_sort_selected, core must come before cli.
        let all = vec![
            make_crate_with_template("core", "crates/core", "core-v{{ .Version }}"),
            CrateConfig {
                name: "cli".to_string(),
                path: "crates/cli".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                depends_on: Some(vec!["core".to_string()]),
                ..Default::default()
            },
        ];
        let head_tags = vec!["v1.0.0".to_string(), "core-v1.0.0".to_string()];
        let selected = run_tag_mapping(&all, &head_tags);
        // Both should be selected.
        assert!(selected.contains(&"core".to_string()));
        assert!(selected.contains(&"cli".to_string()));
        let sorted = topo_sort_selected(&all, &selected);
        let core_pos = sorted.iter().position(|s| s == "core").unwrap();
        let cli_pos = sorted.iter().position(|s| s == "cli").unwrap();
        assert!(
            core_pos < cli_pos,
            "core must come before cli in topo order; got: {:?}",
            sorted
        );
    }

    #[test]
    fn map_head_tags_unrecognized_tag_is_ignored() {
        let crates = vec![make_crate_with_template("app", ".", "v{{ .Version }}")];
        let head_tags = vec!["nightly-20260527".to_string(), "v2.0.0".to_string()];
        let selected = run_tag_mapping(&crates, &head_tags);
        // nightly tag doesn't match any prefix → only "app" from v2.0.0.
        assert_eq!(selected, vec!["app"]);
    }

    #[test]
    fn map_head_tags_no_tags_at_head_is_noop() {
        let crates = vec![make_crate_with_template("app", ".", "v{{ .Version }}")];
        let head_tags: Vec<String> = vec![];
        let selected = run_tag_mapping(&crates, &head_tags);
        assert!(selected.is_empty(), "no tags → no-op, empty selection");
    }

    /// Helper: run the tag→crate mapping logic without spawning git.
    fn run_tag_mapping(crates: &[CrateConfig], head_tags: &[String]) -> Vec<String> {
        let mut selected: Vec<String> = Vec::new();
        for tag in head_tags {
            if let Some(c) = resolve_tag_to_crate(tag, crates)
                && !selected.contains(&c.name)
            {
                selected.push(c.name.clone());
            }
        }
        selected
    }

    fn make_crate_with_template(name: &str, path: &str, template: &str) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: path.to_string(),
            tag_template: template.to_string(),
            ..Default::default()
        }
    }
}
