mod milestones;
mod split;

pub use split::run_merge;

use super::helpers;
use crate::pipeline;
use anodize_core::artifact;
use anodize_core::config::{Config, CrateConfig, WorkspaceConfig};
use anodize_core::context::{Context, ContextOptions};
use anodize_core::git;
use anodize_core::log::{StageLogger, Verbosity};
use anodize_core::template;
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
}

pub fn run(opts: ReleaseOpts) -> Result<()> {
    let log = StageLogger::new(
        "release",
        Verbosity::from_flags(opts.quiet, opts.verbose, opts.debug),
    );

    // Check git is available before doing anything else.
    git::check_git_available()?;

    if opts.snapshot && opts.nightly {
        anyhow::bail!("--snapshot and --nightly cannot be combined");
    }

    let mut config =
        pipeline::load_config(&pipeline::find_config(opts.config_override.as_deref())?)?;

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

    // Auto-infer project_name from Cargo.toml when not set in config
    // (GoReleaser project.go:22-43 infers from Cargo.toml/go.mod/git remote).
    if config.project_name.is_empty()
        && let Ok(cargo_toml) = std::fs::read_to_string("Cargo.toml")
        && let Ok(doc) = cargo_toml.parse::<toml_edit::DocumentMut>()
        && let Some(name) = doc
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
    {
        config.project_name = name.to_string();
        log.verbose(&format!("inferred project_name '{}' from Cargo.toml", name));
    }

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
        release.header = Some(anodize_core::config::ContentSource::Inline(header_content));
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
        release.header = Some(anodize_core::config::ContentSource::Inline(raw));
    }
    if let Some(ref footer_path) = opts.release_footer {
        let footer_content = std::fs::read_to_string(footer_path).with_context(|| {
            format!(
                "failed to read release footer file: {}",
                footer_path.display()
            )
        })?;
        let release = config.release.get_or_insert_with(Default::default);
        release.footer = Some(anodize_core::config::ContentSource::Inline(footer_content));
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
        release.footer = Some(anodize_core::config::ContentSource::Inline(raw));
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
    if !opts.clean && !opts.merge {
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
    // Snapshot mode automatically skips publish and announce stages
    // (like GoReleaser). The release stage is NOT skipped — it handles
    // snapshot mode internally (e.g. creating draft releases for testing).
    if opts.snapshot {
        for stage in &["publish", "announce"] {
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
    };
    let mut ctx = Context::new(config.clone(), ctx_opts);
    helpers::resolve_scm_token_type(&mut ctx, &config);
    ctx.populate_time_vars();
    ctx.populate_runtime_vars();
    ctx.populate_metadata_var();

    // Populate user-defined env vars into template context
    helpers::setup_env(&mut ctx, &config, &log)?;

    // Run hooks before pipeline — after env vars are populated so
    // that template variables (Env.*, ProjectName, etc.) are available,
    // but BEFORE git context resolution (matching GoReleaser ordering).
    // Skip in --merge and --split modes: CI already validates the code
    // before tagging, and hook compilation can dirty the working tree.
    // Also skip when running from a tag checkout (CI tag-triggered release):
    // hooks like `cargo test` can dirty the tree (Cargo.lock, binstall
    // metadata), causing the dirty-state check to fail. The tag was created
    // after CI already validated the code.
    let is_tag_checkout = std::env::var("GITHUB_REF")
        .map(|r| r.starts_with("refs/tags/"))
        .unwrap_or(false)
        || std::env::var("GITHUB_REF_TYPE")
            .map(|t| t == "tag")
            .unwrap_or(false);
    if !opts.merge
        && !opts.split
        && !is_tag_checkout
        && let Some(before) = &config.before
        && let Some(ref hooks) = before.pre
    {
        pipeline::run_hooks(
            hooks,
            "before",
            opts.dry_run,
            &log,
            Some(ctx.template_vars()),
        )?;
    }

    // Resolve tag and populate git variables before running the pipeline.
    helpers::resolve_git_context(&mut ctx, &config, &log)?;

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
            .map(|s| s.name_template.as_str())
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
    {
        let dist = config.dist.as_os_str();
        std::fs::create_dir_all(&config.dist).with_context(|| {
            format!(
                "failed to create dist directory: {}",
                dist.to_string_lossy()
            )
        })?;
        let effective_path = config.dist.join("config.yaml");
        let yaml =
            serde_yaml_ng::to_string(&config).context("failed to serialize effective config")?;
        std::fs::write(&effective_path, &yaml)
            .with_context(|| format!("failed to write {}", effective_path.display()))?;
        log.verbose(&format!(
            "wrote effective config to {}",
            effective_path.display()
        ));
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
    log: &anodize_core::log::StageLogger,
) -> Result<()> {
    // Print artifact size table if configured
    if config.report_sizes.unwrap_or(false) {
        artifact::print_size_report(&mut ctx.artifacts, log);
    }

    // GoReleaser writes metadata.json and artifacts.json even in dry-run mode.
    let dist = &config.dist;
    std::fs::create_dir_all(dist)
        .with_context(|| format!("failed to create dist directory: {}", dist.display()))?;

    // Write metadata.json with project metadata (GoReleaser parity).
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

    // Register metadata.json as an artifact.
    ctx.artifacts.add(anodize_core::artifact::Artifact {
        kind: anodize_core::artifact::ArtifactKind::Metadata,
        name: "metadata.json".to_string(),
        path: metadata_path.clone(),
        target: None,
        crate_name: config.project_name.clone(),
        metadata: Default::default(),
        size: None,
    });

    // Write artifacts.json with the artifact list.
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

    // Apply mod_timestamp to both metadata.json and artifacts.json if configured.
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
    git_config: Option<&anodize_core::config::GitConfig>,
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
    let output = std::process::Command::new("git")
        .args([
            "diff",
            "--name-only",
            &format!("{}..HEAD", tag),
            "--",
            "Cargo.toml",
            "Cargo.lock",
        ])
        .output()?;
    if output.status.success() {
        Ok(!String::from_utf8_lossy(&output.stdout).trim().is_empty())
    } else {
        // If git command fails (e.g. not a git repo), assume no changes
        Ok(false)
    }
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

    anodize_core::util::topological_sort(&items)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use anodize_core::config::{CrateConfig, WorkspaceConfig};

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
        use anodize_core::config::{ChangelogConfig, SignConfig};
        use std::collections::HashMap;

        // Build a top-level config with env, signs, and changelog
        let mut config = Config {
            project_name: "test".to_string(),
            crates: vec![make_crate("top-crate", None)],
            env: Some(HashMap::from([
                ("SHARED".to_string(), "from-top".to_string()),
                ("TOP_ONLY".to_string(), "top-value".to_string()),
            ])),
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
                env: Some(HashMap::from([
                    ("SHARED".to_string(), "from-ws".to_string()),
                    ("WS_ONLY".to_string(), "ws-value".to_string()),
                ])),
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

        // Verify env merged additively: TOP_ONLY preserved, SHARED overridden, WS_ONLY added
        let env = config.env.as_ref().unwrap();
        assert_eq!(
            env.get("TOP_ONLY").unwrap(),
            "top-value",
            "top-level-only key should be preserved"
        );
        assert_eq!(
            env.get("SHARED").unwrap(),
            "from-ws",
            "shared key should be overridden by workspace"
        );
        assert_eq!(
            env.get("WS_ONLY").unwrap(),
            "ws-value",
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
        use anodize_core::config::ReleaseConfig;

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
}
