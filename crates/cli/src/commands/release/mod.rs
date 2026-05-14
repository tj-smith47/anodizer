mod milestones;
mod split;

pub use split::run_merge;

use super::helpers;
use crate::pipeline;
use anodizer_core::config::{Config, CrateConfig, WorkspaceConfig};
use anodizer_core::context::{Context, ContextOptions, RollbackMode};
use anodizer_core::git;
use anodizer_core::log::{StageLogger, Verbosity};
use anodizer_core::template;
use anyhow::{Context as _, Result};
use chrono::Utc;
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
    pub strict: bool,
    /// `--prepare` (GoReleaser Pro parity): run local build/archive/sign/checksum/sbom
    /// stages but NOT release/publish/announce. Implemented by augmenting `skip` with
    /// those three stages at the top of `run()`; artifacts still land under `dist/`.
    pub prepare: bool,
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
    /// prior run report. The replay logic lands in a follow-up task
    /// (tracked in `.claude/known-bugs.md`); `run()` bails with a
    /// clear "not yet implemented" error in this revision so the
    /// flag is discoverable via `--help`.
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
}

/// Decide whether the pre-flight publisher-state check should run.
///
/// Encodes the gating rules so they can be unit-tested without dragging
/// the entire pipeline up. The rules are:
///
/// - `--no-preflight` always wins → false.
/// - `--snapshot` / `--dry-run` / `--split` skip → no upstream side effects.
/// - `publish` in `skip` → caller opted out of one-way doors.
/// - otherwise → true.
///
/// Note: this is the implicit-run decision. `--preflight` (the explicit
/// check-only mode) gates separately in the call site and always runs the
/// check independently of this predicate.
pub(crate) fn should_run_preflight_auto(
    no_preflight: bool,
    snapshot: bool,
    dry_run: bool,
    split: bool,
    publish_skipped: bool,
) -> bool {
    !no_preflight && !snapshot && !dry_run && !split && !publish_skipped
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
    for stage in ["release", "publish", "announce"] {
        if !skip.iter().any(|s| s == stage) {
            skip.push(stage.to_string());
        }
    }
}

pub fn run(mut opts: ReleaseOpts) -> Result<()> {
    // Augment skip BEFORE any stage wiring so the semantic matches
    // `--skip=release,publish,announce` exactly.
    if opts.prepare {
        apply_prepare_mode_to_skip(&mut opts.skip);
    }

    // `--strict` and `--allow-nondeterministic` are mutually exclusive:
    // strict mode forbids the determinism stage from suppressing
    // findings, the allowlist's whole purpose is to suppress one. clap
    // can't express this directly (--strict lives on the top-level Cli
    // struct and the allowlist on the Release variant), so the check
    // runs here.
    if opts.strict && !opts.allow_nondeterministic.is_empty() {
        anyhow::bail!(
            "--strict and --allow-nondeterministic are mutually exclusive (drop --strict if a runtime exemption is required)"
        );
    }

    let log = StageLogger::new(
        "release",
        Verbosity::from_flags(opts.quiet, opts.verbose, opts.debug),
    );

    // Check git is available before doing anything else.
    git::check_git_available()?;

    if opts.snapshot && opts.nightly {
        anyhow::bail!("--snapshot and --nightly cannot be combined");
    }

    let config_path = pipeline::find_config(opts.config_override.as_deref())?;
    let mut config = pipeline::load_config(&config_path)?;

    // If --workspace is specified, resolve the workspace and overlay its config
    // onto the top-level config (replacing crates, changelog, signs, etc.).
    // Also capture any workspace-level skip stages for merging into skip_stages.
    let mut workspace_skip: Vec<String> = Vec::new();
    if let Some(ref ws_name) = opts.workspace {
        let ws = resolve_workspace(&config, ws_name)?.clone();
        workspace_skip = ws.skip.clone();
        helpers::apply_workspace_overlay(&mut config, &ws);
    } else if !opts.crate_names.is_empty() && config.crates.is_empty() {
        // No --workspace given, but --crate X was — infer the workspace that
        // contains X and apply its overlay. Without this, every downstream
        // stage (publish, release, snapcraft-publish, …) iterates
        // ctx.config.crates which is empty in workspace-based configs and
        // silently does nothing. Matches the behaviour users intuitively
        // expect: "release crate X" should release X's workspace.
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
            helpers::apply_workspace_overlay(&mut config, &ws);
        }
    }

    // Auto-infer project_name from Cargo.toml when not set in config.
    helpers::infer_project_name(&mut config, &log);

    // Auto-detect GitHub owner/name from git remote
    helpers::auto_detect_github(&mut config, &log);

    // CLI overrides for release config
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
    // --release-header-tmpl overrides --release-header: file content is
    // stored as-is and rendered through the template engine by the release stage.
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
    // --release-footer-tmpl overrides --release-footer (template-rendered).
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

    if opts.clean && !opts.dry_run {
        let dist = &config.dist;
        if dist.exists() {
            std::fs::remove_dir_all(dist)?;
        }
    } else if opts.clean && opts.dry_run {
        log.status("(dry-run) would clean dist directory");
    }

    // Error if dist directory is non-empty and --clean was not passed
    // (like GoReleaser's ErrDirtyDist).
    // Skip in --merge mode: dist must contain split artifacts.
    // Skip in --rollback-only mode: the whole point of the flag is to
    // read `<dist>/run-<id>/report.json` from a prior populated run.
    if !opts.clean && !opts.merge && !opts.rollback_only {
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

    // Flatten every known crate — top-level plus anything under workspaces —
    // so that `--crate X` and `--all` resolve the same way regardless of whether
    // the config is flat or workspace-based. apply_workspace_overlay already
    // copies workspace crates into config.crates when --workspace is set, but
    // without --workspace we still need to look inside workspaces ourselves.
    let all_known_crates: Vec<CrateConfig> = {
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
    };

    // Determine selected crates
    let selected = if opts.all {
        if opts.force {
            // --all --force: include every crate
            all_known_crates.iter().map(|c| c.name.clone()).collect()
        } else {
            detect_changed_crates(
                &all_known_crates,
                config.git.as_ref(),
                config.monorepo_tag_prefix(),
                &log,
            )?
        }
    } else {
        opts.crate_names.clone()
    };

    // Topological sort of selected crates (respect depends_on ordering).
    // Passing the flattened crate list means --crate cfgd resolves correctly
    // whether `cfgd` is a top-level crate or lives inside a workspace.
    let selected_sorted = topo_sort_selected(&all_known_crates, &selected);

    let mut skip_stages = opts.skip;
    // Merge workspace-level skip stages (e.g., skip: [announce] in workspace config).
    for stage in &workspace_skip {
        if !skip_stages.iter().any(|s| s == stage) {
            skip_stages.push(stage.clone());
        }
    }
    // Snapshot mode automatically skips every stage that performs an external
    // upload: `publish` (registries / package indexes), `snapcraft-publish`
    // (Snap Store), `blob` (S3 / GCS / Azure object storage), and `announce`.
    // The release stage is NOT skipped — it handles snapshot mode internally
    // (e.g. creating draft releases for testing). Matches GoReleaser behaviour
    // and prevents `--snapshot` from accidentally pushing artifacts upstream.
    if opts.snapshot {
        for stage in &["publish", "snapcraft-publish", "blob", "announce"] {
            if !skip_stages.iter().any(|s| s == stage) {
                skip_stages.push(stage.to_string());
            }
        }
    }

    // Skipping publish implies skipping announce (like GoReleaser).
    if skip_stages.contains(&"publish".to_string())
        && !skip_stages.contains(&"announce".to_string())
    {
        skip_stages.push("announce".to_string());
    }

    // Determine release notes path: --release-notes-tmpl overrides --release-notes.
    // Template files are rendered using template vars and written to dist/.
    let release_notes_path = if let Some(ref tmpl_path) = opts.release_notes_tmpl {
        let content = std::fs::read_to_string(tmpl_path).with_context(|| {
            format!(
                "failed to read release notes template: {}",
                tmpl_path.display()
            )
        })?;
        // We'll render the template after context is created (need template vars).
        // Store raw content for now, render after populate.
        Some((tmpl_path.clone(), content))
    } else {
        None
    };

    // Translate `--rollback=<v>` into the enum; reject invalid values
    // up front so the dispatch site can rely on a clean value.
    let rollback_mode: Option<RollbackMode> = match opts.rollback.as_deref() {
        Some("none") => Some(RollbackMode::None),
        Some("best-effort") => Some(RollbackMode::BestEffort),
        Some(other) => {
            anyhow::bail!(
                "invalid --rollback value: {} (expected: none, best-effort)",
                other
            );
        }
        None => None,
    };

    // `--simulate-failure` is a test-only flag and is gated by the
    // `ANODIZE_TEST_HARNESS=1` env var. Production releases that
    // accidentally set the flag get a hard error rather than silent
    // pass-through, so the surface cannot be weaponized.
    let simulate_failure_publishers = if std::env::var("ANODIZE_TEST_HARNESS").as_deref() == Ok("1")
    {
        std::mem::take(&mut opts.simulate_failure)
    } else if !opts.simulate_failure.is_empty() {
        anyhow::bail!(
            "--simulate-failure requires ANODIZE_TEST_HARNESS=1 (test-harness gated flag)"
        );
    } else {
        Vec::new()
    };

    // Translate `--allow-nondeterministic name=reason` (repeatable)
    // into `(name, reason)` tuples. Empty reasons are rejected so the
    // run summary always carries a human-readable justification.
    let runtime_nondeterministic_allowlist: Vec<(String, String)> = opts
        .allow_nondeterministic
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
        .collect::<Result<Vec<_>, _>>()?;

    let ctx_opts = ContextOptions {
        snapshot: opts.snapshot,
        nightly: opts.nightly,
        dry_run: opts.dry_run,
        quiet: opts.quiet,
        verbose: opts.verbose,
        debug: opts.debug,
        skip_stages,
        selected_crates: selected_sorted,
        token: opts.token,
        parallelism: opts.parallelism,
        single_target: opts.single_target,
        release_notes_path: opts.release_notes,
        fail_fast: opts.fail_fast,
        partial_target: None, // Set by --split mode in run_split()
        merge: opts.merge,
        project_root: None,
        strict: opts.strict,
        resume_release: opts.resume_release,
        replace_existing_artifacts: opts.replace_existing,
        skip_post_publish_poll: opts.no_post_publish_poll,
        // `--no-gate-submitter` flips to `Some(false)`; absent flag
        // means `None`, which the dispatch site resolves to gate-on
        // via `unwrap_or(true)`.
        gate_submitter: if opts.no_gate_submitter {
            Some(false)
        } else {
            None
        },
        rollback_mode,
        simulate_failure_publishers,
        rollback_only: opts.rollback_only,
        allow_rerun: opts.allow_rerun,
        from_run: opts.from_run,
        runtime_nondeterministic_allowlist,
        summary_json_path: opts.summary_json,
    };
    let mut ctx = Context::new(config.clone(), ctx_opts);
    helpers::resolve_scm_token_type(&mut ctx, &config);
    ctx.populate_time_vars();
    ctx.populate_runtime_vars();
    ctx.populate_metadata_var()?;

    // `--rollback-only` short-circuits the pipeline: load the prior run's
    // `report.json`, re-attempt rollback for every Succeeded /
    // RollbackFailed entry, persist the result to `rollback.json`, and
    // return. No build / publish / announce stages run in this mode.
    if ctx.options.rollback_only {
        // Clone the run id so the borrow on `ctx.options` ends before
        // `rollback_only::run` takes `&mut ctx`.
        let run_id = ctx
            .options
            .from_run
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--rollback-only requires --from-run=<id>"))?;
        let updated_report = anodizer_stage_publish::rollback_only::run(&mut ctx, &run_id)?;
        ctx.set_publish_report(updated_report);
        return Ok(());
    }

    // Populate the GR-Pro `IsPrepare` template var so user templates can
    // branch on prepare-mode (e.g. emit a different artifact_name for
    // pre-release archives generated in PR CI). Mirrors `IsRelease` /
    // `IsMerging` (`crates/core/src/context.rs:557-566`); kept on the CLI
    // side because `--prepare` is a CLI-only switch (no equivalent
    // `ctx_opts.prepare` field). Set explicitly to `"true"`/`"false"` so
    // `{% if IsPrepare %}` evaluates correctly in either branch.
    ctx.template_vars_mut()
        .set("IsPrepare", if opts.prepare { "true" } else { "false" });

    // Populate user-defined env vars into template context
    helpers::setup_env(&mut ctx, &config, &log)?;

    // Resolve tag and populate git variables before running the pipeline.
    // GoReleaser runs git.Pipe (index 2) BEFORE before.Pipe (index 7) so
    // before-hooks can rely on Git.Tag, Git.Commit, etc. in their template
    // vars (pipeline.go:69,79).
    helpers::resolve_git_context(&mut ctx, &config, &log)?;

    // Run before-hooks now that env AND git vars are populated. Respect
    // `--skip=before` (matching GoReleaser's skip.Before). Skip in --merge
    // and --split modes: CI already validates the code before tagging, and
    // hook compilation can dirty the working tree.
    if !opts.merge
        && !opts.split
        && !ctx.should_skip("before")
        && let Some(before) = &config.before
        && let Some(ref hooks) = before.hooks
    {
        pipeline::run_hooks(
            hooks,
            "before",
            opts.dry_run,
            &log,
            Some(ctx.template_vars()),
        )?;
    }

    // Render --release-notes-tmpl now that template vars are populated.
    // This overrides --release-notes.
    if let Some((_tmpl_path, raw_content)) = release_notes_path {
        let rendered = template::render(&raw_content, ctx.template_vars()).with_context(|| {
            format!(
                "failed to render release notes template: {}",
                _tmpl_path.display()
            )
        })?;
        // Write rendered content to dist/release-notes.md and use that as the notes path
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

    // Dirty repo gate: error out if the repo has uncommitted changes unless
    // running in snapshot, nightly, or dry-run mode (matching GoReleaser behaviour).
    if git::is_git_dirty() && !ctx.is_snapshot() && !ctx.is_nightly() && !ctx.is_dry_run() {
        let status = git::git_status_porcelain();
        anyhow::bail!(
            "git repository is dirty; use --snapshot to release from a dirty tree, or commit your changes first.\n\nDirty files:\n{}",
            status
        );
    }

    // Apply nightly overrides after git vars are populated.
    if ctx.is_nightly() {
        let nightly_cfg = config.nightly.as_ref();
        let date_str = Utc::now().format("%Y%m%d").to_string();

        // Build the nightly version: take existing Version (major.minor.patch) and append
        // the nightly prerelease suffix.
        let base_version = ctx
            .template_vars()
            .get("Version")
            .cloned()
            .unwrap_or_else(|| "0.1.0".to_string());
        // Strip any existing prerelease suffix to get the numeric base.
        let numeric_base = base_version
            .split('-')
            .next()
            .unwrap_or(&base_version)
            .to_string();
        let nightly_version = format!("{}-nightly.{}", numeric_base, date_str);

        // Override Version, RawVersion, and Tag to nightly values.
        ctx.template_vars_mut().set("Version", &nightly_version);
        ctx.template_vars_mut().set("RawVersion", &nightly_version);

        let nightly_tag = nightly_cfg
            .and_then(|c| c.tag_name.as_deref())
            .unwrap_or("nightly")
            .to_string();
        ctx.template_vars_mut().set("Tag", &nightly_tag);

        // IsNightly is already set by populate_git_vars via ctx.options.nightly,
        // but set it explicitly here too for clarity.
        ctx.template_vars_mut().set("IsNightly", "true");

        // Render and set the release name from name_template.
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
    }

    // Apply snapshot version template (GoReleaser always applies one).
    // Default: "{{ Version }}-SNAPSHOT-{{ ShortCommit }}" when no snapshot config exists.
    if ctx.is_snapshot() {
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
        // GoReleaser snapshot.go:37-39: empty snapshot name is an error.
        if rendered_name.trim().is_empty() {
            anyhow::bail!("empty snapshot name after rendering version_template");
        }
        ctx.template_vars_mut().set("Version", &rendered_name);
        // Note: RawVersion is intentionally NOT overwritten here.
        // GoReleaser preserves RawVersion as the numeric semver base
        // (Major.Minor.Patch) even in snapshot mode.
        ctx.template_vars_mut().set("ReleaseName", &rendered_name);
        log.verbose(&format!(
            "snapshot: version={}, release_name={}",
            rendered_name, rendered_name
        ));
    }

    // Dump effective (resolved) config to dist/config.yaml before pipeline runs.
    // GoReleaser always writes this, including in dry-run mode.
    helpers::write_effective_config(&config, &log)?;

    // Pre-flight milestone resolution so a misconfigured `milestones:` block
    // (empty rendered name, unresolvable repo) fails fast — at validate time
    // — instead of after the full build/archive/sign pipeline. Runs in normal
    // and `--merge` modes (close_milestones runs in run_post_pipeline for
    // both). Skipped in `--split` mode: split only emits build artifacts and
    // exits without invoking run_post_pipeline, so milestone close never runs
    // there and pre-flighting it would warn about a stage that won't fire.
    if !opts.split
        && let Some(ref milestones) = config.milestones
    {
        milestones::preflight_milestones(milestones, &mut ctx, &log)?;
    }

    // Pre-flight publisher-state check. Walk each enabled one-way-door
    // publisher (cargo, choco, winget, aur) and bail early if the target
    // version is already submitted / approved / pending — saves an entire
    // wasted release cycle. Skip in snapshot / dry-run / split modes (no
    // upstream side-effects) and when `publish` is already in skip_stages.
    let should_run_preflight = should_run_preflight_auto(
        opts.no_preflight,
        opts.snapshot,
        opts.dry_run,
        opts.split,
        ctx.should_skip("publish"),
    );
    if opts.preflight || should_run_preflight {
        let report = anodizer_stage_publish::preflight::run_preflight(&mut ctx, &log)?;
        if report.entries.is_empty() {
            log.verbose("preflight: no one-way-door publishers configured; skipping check");
        } else {
            // Route the report through the stage logger (same channel as
            // every other status string in this function) instead of a raw
            // `print!` so verbosity / quiet flags / future redirection
            // apply uniformly. The Display impl is multi-line; splitting
            // line-by-line preserves the existing single-line cadence used
            // by surrounding `log.status` / `log.verbose` calls.
            for line in report.to_string().trim_end_matches('\n').lines() {
                log.status(line);
            }
        }
        // `--strict` already plumbs strict mode globally; treat it as
        // implying preflight-strict. `--strict-preflight` is kept as an
        // explicit alias for back-compat with anyone who already plumbed
        // it through their CI.
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
        // Publisher::preflight() returns) live in their own channel; bail
        // when any is present so the operator sees the problem before the
        // pipeline starts.
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
        // `--preflight` is a check-only mode: exit successfully without
        // running the rest of the release pipeline.
        if opts.preflight {
            return Ok(());
        }
    }

    // --split: run only the build stage, serialize artifacts to dist/, then exit
    if opts.split {
        return split::run_split(&mut ctx, &config, &log);
    }

    // --merge: load artifacts from split jobs, then run post-build stages
    if opts.merge {
        return split::run_merge(&mut ctx, &config, &log, opts.dry_run, None);
    }

    let p = pipeline::build_release_pipeline();
    let result = p.run(&mut ctx, &log);

    if result.is_ok() {
        run_post_pipeline(&mut ctx, &config, opts.dry_run, &log)?;
    }

    result
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

    // Close milestones
    if let Some(ref milestones) = config.milestones {
        milestones::close_milestones(milestones, ctx, dry_run, log)?;
    }

    // Run after hooks
    if let Some(after) = &config.after
        && let Some(ref hooks) = after.post
    {
        pipeline::run_hooks(hooks, "after", dry_run, log, Some(ctx.template_vars()))?;
    }

    Ok(())
}

/// Detect which crates have changes since their last tag.
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
    use anodizer_core::config::{CrateConfig, WorkspaceConfig};

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
                "announce".to_string()
            ],
            "--prepare on empty skip should add all three upstream stages"
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
        assert!(
            skip.contains(&"release".to_string())
                && skip.contains(&"publish".to_string())
                && skip.contains(&"announce".to_string()),
            "--prepare adds release/publish/announce alongside user skips"
        );
    }

    #[test]
    fn test_apply_prepare_mode_to_skip_composes_with_snapshot_marker() {
        // A5-S6: `--prepare --snapshot` must produce a skip list that still
        // includes release/publish/announce, independent of any snapshot-only
        // entries a caller may have pre-added. The augmentation is purely
        // additive — snapshot semantics remain owned by the snapshot flag.
        let mut skip = vec!["sign".to_string()];
        apply_prepare_mode_to_skip(&mut skip);
        for stage in ["release", "publish", "announce"] {
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
        let mut skip = vec!["release".to_string(), "publish".to_string()];
        apply_prepare_mode_to_skip(&mut skip);
        // No duplicate "release" or "publish" — only "announce" added.
        let release_count = skip.iter().filter(|s| s.as_str() == "release").count();
        let publish_count = skip.iter().filter(|s| s.as_str() == "publish").count();
        assert_eq!(release_count, 1, "no duplicate release");
        assert_eq!(publish_count, 1, "no duplicate publish");
        assert!(skip.contains(&"announce".to_string()));
    }

    // ---- preflight auto-run gating ---------------------------------------

    #[test]
    fn should_run_preflight_auto_default_runs() {
        // No flag set → run.
        assert!(should_run_preflight_auto(false, false, false, false, false));
    }

    #[test]
    fn should_run_preflight_auto_no_preflight_skips() {
        assert!(!should_run_preflight_auto(true, false, false, false, false));
    }

    #[test]
    fn should_run_preflight_auto_snapshot_skips() {
        assert!(!should_run_preflight_auto(false, true, false, false, false));
    }

    #[test]
    fn should_run_preflight_auto_dry_run_skips() {
        assert!(!should_run_preflight_auto(false, false, true, false, false));
    }

    #[test]
    fn should_run_preflight_auto_split_skips() {
        assert!(!should_run_preflight_auto(false, false, false, true, false));
    }

    #[test]
    fn should_run_preflight_auto_publish_skipped_skips() {
        assert!(!should_run_preflight_auto(false, false, false, false, true));
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
}
