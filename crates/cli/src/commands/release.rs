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
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
    if let Some(ref ws_name) = opts.workspace {
        let ws = resolve_workspace(&config, ws_name)?.clone();
        helpers::apply_workspace_overlay(&mut config, &ws);
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
    if !opts.merge
        && !opts.split
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
        return run_split(&mut ctx, &config, &log);
    }

    // --merge: load artifacts from split jobs, then run post-build stages
    if opts.merge {
        return run_merge(&mut ctx, &config, &log, opts.dry_run, None);
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
        close_milestones(milestones, ctx, dry_run, log)?;
    }

    // Run after hooks
    if let Some(after) = &config.after
        && let Some(ref hooks) = after.post
    {
        pipeline::run_hooks(hooks, "after", dry_run, log, Some(ctx.template_vars()))?;
    }

    Ok(())
}

/// Close milestones on the VCS provider after a release.
///
/// For each milestone config with `close: true`, renders the name template,
/// resolves the repo owner/name, and calls the GitHub/GitLab/Gitea API to
/// close the milestone. Errors are logged as warnings unless `fail_on_error` is set.
fn close_milestones(
    milestones: &[anodize_core::config::MilestoneConfig],
    ctx: &mut Context,
    dry_run: bool,
    log: &StageLogger,
) -> Result<()> {
    let token = ctx.options.token.clone().unwrap_or_default();

    for milestone_cfg in milestones {
        if !milestone_cfg.close.unwrap_or(false) {
            continue;
        }

        let name_template = milestone_cfg
            .name_template
            .as_deref()
            .unwrap_or("{{ Tag }}");
        let milestone_name = ctx
            .render_template(name_template)
            .context("milestone: render name_template")?;

        if milestone_name.is_empty() {
            log.verbose("milestone: skipping empty name");
            continue;
        }

        // Determine repo owner/name from milestone config or release config
        let (owner, repo_name) = resolve_milestone_repo(milestone_cfg, &ctx.config);

        if owner.is_empty() || repo_name.is_empty() {
            if milestone_cfg.fail_on_error.unwrap_or(false) {
                anyhow::bail!("milestone: repo owner/name not configured");
            }
            log.warn("milestone: skipping — repo owner/name not configured");
            continue;
        }

        if dry_run {
            log.status(&format!(
                "(dry-run) would close milestone '{}' on {}/{}",
                milestone_name, owner, repo_name
            ));
            continue;
        }

        log.status(&format!(
            "closing milestone '{}' on {}/{}",
            milestone_name, owner, repo_name
        ));

        // GoReleaser parity: close milestones on GitHub, GitLab, and Gitea.
        let provider = resolve_milestone_provider(milestone_cfg, &ctx.config);
        let api_url = resolve_milestone_api_url(milestone_cfg, &ctx.config);
        let close_result = match provider.as_str() {
            "github" => close_milestone_github(&token, &owner, &repo_name, &milestone_name),
            "gitlab" => close_milestone_gitlab(
                &token,
                &owner,
                &repo_name,
                &milestone_name,
                api_url.as_deref(),
            ),
            "gitea" => close_milestone_gitea(
                &token,
                &owner,
                &repo_name,
                &milestone_name,
                api_url.as_deref(),
            ),
            other => {
                let msg = format!(
                    "milestone: unknown provider '{}' — cannot close milestone",
                    other
                );
                if milestone_cfg.fail_on_error.unwrap_or(false) {
                    anyhow::bail!("{}", msg);
                }
                log.warn(&msg);
                continue;
            }
        };
        match close_result {
            Ok(()) => {
                log.status(&format!("milestone '{}' closed", milestone_name));
            }
            Err(e) => {
                if milestone_cfg.fail_on_error.unwrap_or(false) {
                    return Err(
                        e.context(format!("milestone: failed to close '{}'", milestone_name))
                    );
                }
                log.warn(&format!(
                    "milestone: could not close '{}': {}",
                    milestone_name, e
                ));
            }
        }
    }
    Ok(())
}

fn resolve_milestone_repo(
    milestone_cfg: &anodize_core::config::MilestoneConfig,
    config: &Config,
) -> (String, String) {
    if let Some(ref repo_cfg) = milestone_cfg.repo
        && !repo_cfg.owner.is_empty()
        && !repo_cfg.name.is_empty()
    {
        return (repo_cfg.owner.clone(), repo_cfg.name.clone());
    }

    // Fall back to the first crate's release config
    for crate_cfg in &config.crates {
        if let Some(ref release_cfg) = crate_cfg.release {
            if let Some(ref gh) = release_cfg.github {
                return (gh.owner.clone(), gh.name.clone());
            }
            if let Some(ref gl) = release_cfg.gitlab {
                return (gl.owner.clone(), gl.name.clone());
            }
            if let Some(ref gt) = release_cfg.gitea {
                return (gt.owner.clone(), gt.name.clone());
            }
        }
    }

    (String::new(), String::new())
}

/// Determine the SCM provider type for milestone operations.
/// Returns "github", "gitlab", "gitea", or "unknown".
fn resolve_milestone_provider(
    milestone_cfg: &anodize_core::config::MilestoneConfig,
    config: &Config,
) -> String {
    // If the milestone config specifies a repo, check what provider type the
    // first crate's release config uses (since MilestoneConfig.repo doesn't
    // have a provider field).
    let _ = milestone_cfg;
    for crate_cfg in &config.crates {
        if let Some(ref release_cfg) = crate_cfg.release {
            if release_cfg.github.is_some() {
                return "github".to_string();
            }
            if release_cfg.gitlab.is_some() {
                return "gitlab".to_string();
            }
            if release_cfg.gitea.is_some() {
                return "gitea".to_string();
            }
        }
    }
    "unknown".to_string()
}

/// Close a GitHub milestone by name using the REST API.
fn close_milestone_github(
    token: &str,
    owner: &str,
    repo: &str,
    milestone_name: &str,
) -> Result<()> {
    if token.is_empty() {
        anyhow::bail!("no authentication token available for milestone close");
    }

    let rt = tokio::runtime::Runtime::new().context("milestone: create tokio runtime")?;
    rt.block_on(async {
        let client = reqwest::Client::new();

        // List milestones with pagination to find the one with the matching title.
        // GitHub returns at most 100 per page.
        let mut page = 1u32;
        let mut milestone_number: Option<u64> = None;

        loop {
            let url = format!(
                "https://api.github.com/repos/{}/{}/milestones?state=open&per_page=100&page={}",
                owner, repo, page
            );
            let resp = client
                .get(&url)
                .header("Authorization", format!("Bearer {}", token))
                .header("Accept", "application/vnd.github+json")
                .header("User-Agent", "anodize")
                .send()
                .await
                .context("milestone: list milestones request failed")?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!(
                    "milestone: list milestones failed (HTTP {}): {}",
                    status,
                    body
                );
            }

            let milestones: Vec<serde_json::Value> = resp
                .json()
                .await
                .context("milestone: parse milestones response")?;

            if milestones.is_empty() {
                break;
            }

            if let Some(m) = milestones.iter().find(|m| {
                m.get("title")
                    .and_then(|t| t.as_str())
                    .is_some_and(|t| t == milestone_name)
            }) {
                milestone_number = m.get("number").and_then(|n| n.as_u64());
                break;
            }

            // If we got fewer than 100 results, there are no more pages.
            if milestones.len() < 100 {
                break;
            }
            page += 1;
        }

        let milestone_number = match milestone_number {
            Some(n) => n,
            None => {
                // Milestone not found -- treat as success (may have been closed already)
                return Ok(());
            }
        };

        // Close the milestone
        let close_url = format!(
            "https://api.github.com/repos/{}/{}/milestones/{}",
            owner, repo, milestone_number
        );
        let resp = client
            .patch(&close_url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "anodize")
            .json(&serde_json::json!({ "state": "closed" }))
            .send()
            .await
            .context("milestone: close milestone request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("milestone: close failed (HTTP {}): {}", status, body);
        }

        Ok(())
    })
}

/// Simple percent-encoding for URL path segments.
fn url_encode(s: &str) -> String {
    let mut result = String::with_capacity(s.len() * 3);
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(byte as char);
            }
            _ => {
                result.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    result
}

/// Resolve the API base URL for milestone operations on GitLab/Gitea.
fn resolve_milestone_api_url(
    _milestone_cfg: &anodize_core::config::MilestoneConfig,
    config: &Config,
) -> Option<String> {
    // Check top-level gitlab_urls / gitea_urls config
    if let Some(ref gitlab) = config.gitlab_urls
        && let Some(ref api) = gitlab.api
    {
        // Strip trailing /api/v4/ to get base URL
        let base = api.trim_end_matches('/').trim_end_matches("/api/v4");
        return Some(base.to_string());
    }
    if let Some(ref gitea) = config.gitea_urls
        && let Some(ref api) = gitea.api
    {
        let base = api.trim_end_matches('/').trim_end_matches("/api/v1");
        return Some(base.to_string());
    }
    None
}

/// Close a GitLab milestone by name using the REST API.
fn close_milestone_gitlab(
    token: &str,
    owner: &str,
    repo: &str,
    milestone_name: &str,
    api_url: Option<&str>,
) -> Result<()> {
    if token.is_empty() {
        anyhow::bail!("no authentication token available for GitLab milestone close");
    }
    let base = api_url.unwrap_or("https://gitlab.com");

    let rt = tokio::runtime::Runtime::new().context("milestone: create tokio runtime")?;
    rt.block_on(async {
        let client = reqwest::Client::new();
        let project_path = format!("{}/{}", owner, repo);
        let encoded_path = url_encode(&project_path);

        // List milestones to find matching title
        let url = format!(
            "{}/api/v4/projects/{}/milestones?title={}",
            base,
            encoded_path,
            url_encode(milestone_name)
        );
        let resp = client
            .get(&url)
            .header("PRIVATE-TOKEN", token)
            .header("User-Agent", "anodize")
            .send()
            .await
            .context("milestone: GitLab list milestones failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "milestone: GitLab list milestones failed (HTTP {}): {}",
                status,
                body
            );
        }

        let milestones: Vec<serde_json::Value> = resp
            .json()
            .await
            .context("milestone: parse GitLab milestones")?;

        let milestone_id = milestones
            .iter()
            .find(|m| {
                m.get("title")
                    .and_then(|t| t.as_str())
                    .is_some_and(|t| t == milestone_name)
            })
            .and_then(|m| m.get("id").and_then(|i| i.as_u64()));

        let milestone_id = match milestone_id {
            Some(id) => id,
            None => return Ok(()), // Not found — may be already closed
        };

        // Close the milestone (GoReleaser: StateEvent = "close")
        let close_url = format!(
            "{}/api/v4/projects/{}/milestones/{}",
            base, encoded_path, milestone_id
        );
        let resp = client
            .put(&close_url)
            .header("PRIVATE-TOKEN", token)
            .header("User-Agent", "anodize")
            .json(&serde_json::json!({ "state_event": "close" }))
            .send()
            .await
            .context("milestone: GitLab close milestone failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("milestone: GitLab close failed (HTTP {}): {}", status, body);
        }
        Ok(())
    })
}

/// Close a Gitea milestone by name using the REST API.
fn close_milestone_gitea(
    token: &str,
    owner: &str,
    repo: &str,
    milestone_name: &str,
    api_url: Option<&str>,
) -> Result<()> {
    if token.is_empty() {
        anyhow::bail!("no authentication token available for Gitea milestone close");
    }
    let base = api_url.unwrap_or("https://gitea.com");

    let rt = tokio::runtime::Runtime::new().context("milestone: create tokio runtime")?;
    rt.block_on(async {
        let client = reqwest::Client::new();

        // List milestones to find matching title
        let url = format!(
            "{}/api/v1/repos/{}/{}/milestones?state=open&name={}",
            base,
            owner,
            repo,
            url_encode(milestone_name)
        );
        let resp = client
            .get(&url)
            .header("Authorization", format!("token {}", token))
            .header("User-Agent", "anodize")
            .send()
            .await
            .context("milestone: Gitea list milestones failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "milestone: Gitea list milestones failed (HTTP {}): {}",
                status,
                body
            );
        }

        let milestones: Vec<serde_json::Value> = resp
            .json()
            .await
            .context("milestone: parse Gitea milestones")?;

        let milestone_id = milestones
            .iter()
            .find(|m| {
                m.get("title")
                    .and_then(|t| t.as_str())
                    .is_some_and(|t| t == milestone_name)
            })
            .and_then(|m| m.get("id").and_then(|i| i.as_u64()));

        let milestone_id = match milestone_id {
            Some(id) => id,
            None => return Ok(()), // Not found — may be already closed
        };

        // Close the milestone (GoReleaser: state = "closed")
        let close_url = format!(
            "{}/api/v1/repos/{}/{}/milestones/{}",
            base, owner, repo, milestone_id
        );
        let resp = client
            .patch(&close_url)
            .header("Authorization", format!("token {}", token))
            .header("User-Agent", "anodize")
            .json(&serde_json::json!({ "state": "closed", "title": milestone_name }))
            .send()
            .await
            .context("milestone: Gitea close milestone failed")?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            // GoReleaser parity: 404 means milestone not found
            return Ok(());
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("milestone: Gitea close failed (HTTP {}): {}", status, body);
        }
        Ok(())
    })
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
// Split/Merge CI Fan-Out — GoReleaser Pro Parity
// ---------------------------------------------------------------------------

/// Rich artifact format for split/merge serialization.
/// Mirrors GoReleaser's artifact JSON with OS/arch metadata.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct SplitArtifact {
    /// Artifact filename (basename).
    pub name: String,
    /// Full path to the artifact file.
    pub path: String,
    /// OS component (e.g., "linux", "darwin", "windows").
    pub goos: Option<String>,
    /// Arch component (e.g., "amd64", "arm64").
    pub goarch: Option<String>,
    /// Full target triple (e.g., "x86_64-unknown-linux-gnu").
    pub target: Option<String>,
    /// Artifact kind for internal routing.
    #[serde(rename = "internal_type")]
    pub kind: String,
    /// Human-readable type string.
    #[serde(rename = "type")]
    pub type_s: String,
    /// Crate that produced this artifact.
    pub crate_name: String,
    /// Rich metadata.
    pub extra: HashMap<String, serde_json::Value>,
}

/// Full context serialized during split for merge recovery.
/// Includes config, git info, template vars, and artifacts.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct SplitContext {
    /// The partial target that was used for filtering.
    pub partial_target: String,
    /// Template variables (all resolved values at split time).
    pub template_vars: HashMap<String, String>,
    /// Environment variables accessible as {{ Env.VAR }} in templates.
    #[serde(default)]
    pub env_vars: HashMap<String, String>,
    /// Git info snapshot.
    pub git_tag: Option<String>,
    pub git_commit: Option<String>,
    pub git_branch: Option<String>,
    /// Artifacts produced by this split job.
    pub artifacts: Vec<SplitArtifact>,
}

/// GitHub Actions matrix with runner suggestions.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct SplitMatrix {
    /// How the build was split.
    pub split_by: String,
    /// Matrix entries with target and suggested runner.
    pub include: Vec<MatrixEntry>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct MatrixEntry {
    /// OS name (goos mode) or full target triple (target mode).
    pub target: String,
    /// Suggested GitHub Actions runner.
    pub runner: String,
}

/// Convert Artifact to SplitArtifact for serialization.
fn artifact_to_split(a: &artifact::Artifact) -> SplitArtifact {
    SplitArtifact {
        name: a.name().to_string(),
        path: a.path.to_string_lossy().into_owned(),
        goos: a.goos(),
        goarch: a.goarch(),
        target: a.target.clone(),
        kind: a.kind.as_str().to_string(),
        type_s: format!("{:?}", a.kind),
        crate_name: a.crate_name.clone(),
        extra: a
            .metadata
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect(),
    }
}

/// Run in --split mode: resolve partial target, build filtered targets,
/// serialize context to dist subdirectory, generate matrix.
fn run_split(
    ctx: &mut Context,
    config: &Config,
    log: &anodize_core::log::StageLogger,
) -> Result<()> {
    // Resolve partial target from env vars / host detection
    let partial_target = anodize_core::partial::resolve_partial_target(&config.partial)?;
    let subdir = partial_target.dist_subdir();

    log.status(&format!(
        "split mode: building for {} (dist/{})",
        match &partial_target {
            anodize_core::partial::PartialTarget::Exact(t) => t.clone(),
            anodize_core::partial::PartialTarget::OsArch { os, arch } => {
                if let Some(a) = arch {
                    format!("{}/{}", os, a)
                } else {
                    os.clone()
                }
            }
        },
        subdir
    ));

    // Validate that the partial target matches at least one configured build target
    let all_targets = collect_build_targets(config, ctx);
    let matching = partial_target.filter_targets(&all_targets);
    if matching.is_empty() && !all_targets.is_empty() {
        anyhow::bail!(
            "split: no build targets match {}. Available targets: [{}]",
            match &partial_target {
                anodize_core::partial::PartialTarget::Exact(t) => format!("TARGET={}", t),
                anodize_core::partial::PartialTarget::OsArch { os, arch } => {
                    if let Some(a) = arch {
                        format!("ANODIZE_OS={}, ANODIZE_ARCH={}", os, a)
                    } else {
                        format!("ANODIZE_OS={}", os)
                    }
                }
            },
            all_targets.join(", ")
        );
    }

    // Set partial target on context so build stage filters targets
    ctx.options.partial_target = Some(partial_target.clone());

    // Route output to dist subdirectory
    let original_dist = config.dist.clone();
    let split_dist = original_dist.join(&subdir);
    // We modify the config dist in-place so all stages write to the subdirectory
    ctx.config.dist = split_dist.clone();

    std::fs::create_dir_all(&split_dist)
        .with_context(|| format!("create split dist directory: {}", split_dist.display()))?;

    // Run only the build pipeline
    let p = pipeline::build_split_pipeline();
    p.run(ctx, log)?;

    // Copy binary artifacts into the split dist directory so they survive
    // upload/download between split and merge machines.  Update the artifact
    // paths to point at the copies inside dist/.
    for artifact in ctx.artifacts.all_mut() {
        if !artifact.path.exists() {
            continue; // dry-run or already relocated
        }
        if let Some(file_name) = artifact.path.file_name().map(|n| n.to_os_string()) {
            let dest = split_dist.join(&file_name);
            if artifact.path != dest {
                std::fs::copy(&artifact.path, &dest).with_context(|| {
                    format!(
                        "split: copy {} -> {}",
                        artifact.path.display(),
                        dest.display()
                    )
                })?;
                artifact.path = dest;
            }
        }
    }

    // Serialize split context (config + git + template vars + artifacts)
    let split_artifacts: Vec<SplitArtifact> =
        ctx.artifacts.all().iter().map(artifact_to_split).collect();

    let split_ctx = SplitContext {
        partial_target: subdir.clone(),
        template_vars: ctx.template_vars().all().clone(),
        env_vars: ctx.template_vars().all_env().clone(),
        git_tag: ctx.template_vars().get("Tag").map(String::from),
        git_commit: ctx.template_vars().get("FullCommit").map(String::from),
        git_branch: ctx.template_vars().get("Branch").map(String::from),
        artifacts: split_artifacts,
    };

    let ctx_path = split_dist.join("context.json");
    let json = serde_json::to_string_pretty(&split_ctx).context("serialize split context")?;
    std::fs::write(&ctx_path, &json)
        .with_context(|| format!("write split context to {}", ctx_path.display()))?;

    log.status(&format!(
        "split: wrote {} artifact(s) + context to {}",
        split_ctx.artifacts.len(),
        ctx_path.display()
    ));

    // Generate matrix.json at the top-level dist directory (not in the subdirectory)
    let all_targets = collect_build_targets(config, ctx);
    if !all_targets.is_empty() {
        let split_by = config
            .partial
            .as_ref()
            .and_then(|p| p.by.as_deref())
            .unwrap_or("goos");

        let matrix = build_matrix(&all_targets, split_by);
        let matrix_json = serde_json::to_string_pretty(&matrix).context("serialize matrix")?;
        let matrix_path = original_dist.join("matrix.json");
        std::fs::create_dir_all(&original_dist)?;
        std::fs::write(&matrix_path, &matrix_json)
            .with_context(|| format!("write matrix to {}", matrix_path.display()))?;
        log.status(&format!(
            "split: wrote matrix to {} ({} entries, split by: {})",
            matrix_path.display(),
            matrix.include.len(),
            split_by
        ));
    }

    Ok(())
}

/// Build a CI matrix from targets, deduplicating by OS when split_by=goos.
fn build_matrix(targets: &[String], split_by: &str) -> SplitMatrix {
    let mut entries = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for t in targets {
        let entry_target = if split_by == "goos" {
            let (os, _) = anodize_core::target::map_target(t);
            os
        } else {
            t.clone()
        };

        if seen.insert(entry_target.clone()) {
            // For target mode, extract OS component for runner suggestion
            let (os, _) = anodize_core::target::map_target(t);
            let runner = anodize_core::partial::suggest_runner(&os);
            entries.push(MatrixEntry {
                target: entry_target,
                runner: runner.to_string(),
            });
        }
    }

    SplitMatrix {
        split_by: split_by.to_string(),
        include: entries,
    }
}

/// Run in --merge mode: load split contexts, merge artifacts, run post-build stages.
pub fn run_merge(
    ctx: &mut Context,
    config: &Config,
    log: &anodize_core::log::StageLogger,
    dry_run: bool,
    dist_override: Option<&Path>,
) -> Result<()> {
    log.status("running in merge mode (post-build stages)...");

    let dist = dist_override.unwrap_or(&config.dist);

    // Find all context.json files in dist/ subdirectories (new format).
    // Fall back to artifacts.json for backward compat with old split format.
    let context_files = find_split_contexts(dist)?;
    if context_files.is_empty() {
        // Try legacy artifacts.json format
        let artifact_files = find_split_artifacts(dist)?;
        if artifact_files.is_empty() {
            anyhow::bail!(
                "merge: no context.json or artifacts.json files found in {}. \
                 Run `anodize release --split` first.",
                dist.display()
            );
        }
        return run_merge_legacy(ctx, config, log, dry_run, &artifact_files);
    }

    // Load and merge all split contexts
    let mut total_loaded = 0;
    let mut seen_paths = std::collections::HashSet::new();
    let mut first_vars: Option<HashMap<String, String>> = None;

    for ctx_file in &context_files {
        let content = std::fs::read_to_string(ctx_file)
            .with_context(|| format!("read split context: {}", ctx_file.display()))?;
        let split_ctx: SplitContext = serde_json::from_str(&content)
            .with_context(|| format!("parse split context: {}", ctx_file.display()))?;

        // Restore template vars and env vars from first split context
        if first_vars.is_none() {
            for (key, value) in &split_ctx.template_vars {
                ctx.template_vars_mut().set(key, value);
            }
            for (key, value) in &split_ctx.env_vars {
                ctx.template_vars_mut().set_env(key, value);
            }
            first_vars = Some(split_ctx.template_vars.clone());
        }

        for sa in &split_ctx.artifacts {
            if !seen_paths.insert(sa.path.clone()) {
                continue;
            }
            let kind = match artifact::ArtifactKind::parse(&sa.kind) {
                Some(k) => k,
                None => {
                    log.warn(&format!(
                        "merge: unknown artifact kind '{}' in {}, skipping",
                        sa.kind,
                        ctx_file.display()
                    ));
                    continue;
                }
            };
            // Convert extra back to flat string metadata
            let metadata: HashMap<String, String> = sa
                .extra
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect();
            ctx.artifacts.add(artifact::Artifact {
                kind,
                name: String::new(),
                path: PathBuf::from(&sa.path),
                target: sa.target.clone(),
                crate_name: sa.crate_name.clone(),
                metadata,
                size: None,
            });
            total_loaded += 1;
        }
    }

    log.status(&format!(
        "merge: loaded {} artifact(s) from {} context(s)",
        total_loaded,
        context_files.len()
    ));

    // Run post-build pipeline
    let p = pipeline::build_merge_pipeline();
    let result = p.run(ctx, log);

    if result.is_ok() {
        run_post_pipeline(ctx, config, dry_run, log)?;
    }

    result
}

/// Legacy merge from old-format artifacts.json files.
fn run_merge_legacy(
    ctx: &mut Context,
    config: &Config,
    log: &anodize_core::log::StageLogger,
    dry_run: bool,
    artifact_files: &[PathBuf],
) -> Result<()> {
    #[derive(serde::Deserialize)]
    struct LegacyOutput {
        artifacts: Vec<LegacyArtifact>,
    }
    #[derive(serde::Deserialize)]
    struct LegacyArtifact {
        kind: String,
        path: String,
        target: Option<String>,
        crate_name: String,
        #[serde(default)]
        metadata: HashMap<String, String>,
    }

    let mut total_loaded = 0;
    let mut seen_paths = std::collections::HashSet::new();

    for artifact_file in artifact_files {
        let content = std::fs::read_to_string(artifact_file)
            .with_context(|| format!("read split artifacts: {}", artifact_file.display()))?;
        let output: LegacyOutput = serde_json::from_str(&content)
            .with_context(|| format!("parse split artifacts: {}", artifact_file.display()))?;

        for sa in &output.artifacts {
            if !seen_paths.insert(sa.path.clone()) {
                continue;
            }
            let kind = artifact::ArtifactKind::parse(&sa.kind)
                .ok_or_else(|| anyhow::anyhow!("unknown artifact kind: {}", sa.kind))?;
            ctx.artifacts.add(artifact::Artifact {
                kind,
                name: String::new(),
                path: PathBuf::from(&sa.path),
                target: sa.target.clone(),
                crate_name: sa.crate_name.clone(),
                metadata: sa.metadata.clone(),
                size: None,
            });
            total_loaded += 1;
        }
    }

    log.status(&format!(
        "merge (legacy): loaded {} artifact(s) from {} file(s)",
        total_loaded,
        artifact_files.len()
    ));

    let p = pipeline::build_merge_pipeline();
    let result = p.run(ctx, log);
    if result.is_ok() {
        run_post_pipeline(ctx, config, dry_run, log)?;
    }
    result
}

/// Collect all build targets from config for matrix generation.
fn collect_build_targets(config: &Config, ctx: &Context) -> Vec<String> {
    let mut targets = Vec::new();

    for krate in &config.crates {
        if !ctx.options.selected_crates.is_empty()
            && !ctx.options.selected_crates.contains(&krate.name)
        {
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

    // Filter out ignored os/arch combinations
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

/// Find all context.json files in dist/ subdirectories (new split format).
fn find_split_contexts(dist: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();

    if dist.is_dir()
        && let Ok(entries) = std::fs::read_dir(dist)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let ctx_file = path.join("context.json");
                if ctx_file.exists() {
                    files.push(ctx_file);
                }
            }
        }
    }

    Ok(files)
}

/// Find all artifacts.json files in dist/ (legacy split format).
fn find_split_artifacts(dist: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();

    let top = dist.join("artifacts.json");
    if top.exists() {
        files.push(top);
    }

    if dist.is_dir()
        && let Ok(entries) = std::fs::read_dir(dist)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let sub_artifacts = path.join("artifacts.json");
                if sub_artifacts.exists() {
                    files.push(sub_artifacts);
                }
            }
        }
    }

    Ok(files)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use anodize_core::config::{CrateConfig, WorkspaceConfig};
    use std::collections::HashMap;

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

    // -----------------------------------------------------------------------
    // Split/merge tests
    // -----------------------------------------------------------------------

    fn make_split_artifact(kind: &str, path: &str, target: Option<&str>) -> SplitArtifact {
        SplitArtifact {
            name: std::path::Path::new(path)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
            path: path.to_string(),
            goos: target.map(|t| anodize_core::target::map_target(t).0),
            goarch: target.map(|t| anodize_core::target::map_target(t).1),
            target: target.map(String::from),
            kind: kind.to_string(),
            type_s: kind.to_string(),
            crate_name: "myapp".to_string(),
            extra: HashMap::new(),
        }
    }

    #[test]
    fn test_split_artifact_serialization_roundtrip() {
        let artifact =
            make_split_artifact("binary", "/tmp/myapp", Some("x86_64-unknown-linux-gnu"));

        let json = serde_json::to_string(&artifact).unwrap();
        let deserialized: SplitArtifact = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.kind, "binary");
        assert_eq!(deserialized.path, "/tmp/myapp");
        assert_eq!(
            deserialized.target.as_deref(),
            Some("x86_64-unknown-linux-gnu")
        );
        assert_eq!(deserialized.goos.as_deref(), Some("linux"));
        assert_eq!(deserialized.goarch.as_deref(), Some("amd64"));
        assert_eq!(deserialized.crate_name, "myapp");
    }

    #[test]
    fn test_split_context_serialization_roundtrip() {
        let ctx = SplitContext {
            partial_target: "linux".to_string(),
            template_vars: HashMap::from([
                ("Tag".to_string(), "v1.0.0".to_string()),
                ("ProjectName".to_string(), "myapp".to_string()),
            ]),
            env_vars: HashMap::from([("GITHUB_TOKEN".to_string(), "ghp_secret".to_string())]),
            git_tag: Some("v1.0.0".to_string()),
            git_commit: Some("abc123".to_string()),
            git_branch: Some("main".to_string()),
            artifacts: vec![
                make_split_artifact("binary", "/tmp/myapp", Some("aarch64-apple-darwin")),
                make_split_artifact("archive", "/tmp/myapp.tar.gz", Some("aarch64-apple-darwin")),
            ],
        };

        let json = serde_json::to_string_pretty(&ctx).unwrap();
        let deserialized: SplitContext = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.partial_target, "linux");
        assert_eq!(deserialized.template_vars.get("Tag").unwrap(), "v1.0.0");
        assert_eq!(deserialized.git_tag.as_deref(), Some("v1.0.0"));
        assert_eq!(deserialized.artifacts.len(), 2);
        assert_eq!(deserialized.artifacts[0].kind, "binary");
        assert_eq!(deserialized.artifacts[1].kind, "archive");
    }

    #[test]
    fn test_split_context_empty() {
        let ctx = SplitContext {
            partial_target: "linux".to_string(),
            template_vars: HashMap::new(),
            env_vars: HashMap::new(),
            git_tag: None,
            git_commit: None,
            git_branch: None,
            artifacts: vec![],
        };
        let json = serde_json::to_string(&ctx).unwrap();
        let deserialized: SplitContext = serde_json::from_str(&json).unwrap();
        assert!(deserialized.artifacts.is_empty());
        assert!(deserialized.git_tag.is_none());
    }

    #[test]
    fn test_find_split_artifacts_top_level() {
        let tmp = tempfile::TempDir::new().unwrap();
        let artifacts_path = tmp.path().join("artifacts.json");
        std::fs::write(&artifacts_path, "{}").unwrap();

        let files = find_split_artifacts(tmp.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0], artifacts_path);
    }

    #[test]
    fn test_find_split_artifacts_subdirectories() {
        let tmp = tempfile::TempDir::new().unwrap();

        // Create subdirectories with artifacts.json
        let linux_dir = tmp.path().join("linux");
        std::fs::create_dir(&linux_dir).unwrap();
        std::fs::write(linux_dir.join("artifacts.json"), "{}").unwrap();

        let darwin_dir = tmp.path().join("darwin");
        std::fs::create_dir(&darwin_dir).unwrap();
        std::fs::write(darwin_dir.join("artifacts.json"), "{}").unwrap();

        let files = find_split_artifacts(tmp.path()).unwrap();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn test_find_split_artifacts_both_levels() {
        let tmp = tempfile::TempDir::new().unwrap();

        // Top-level
        std::fs::write(tmp.path().join("artifacts.json"), "{}").unwrap();

        // Subdirectory
        let sub = tmp.path().join("linux");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("artifacts.json"), "{}").unwrap();

        let files = find_split_artifacts(tmp.path()).unwrap();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn test_find_split_artifacts_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let files = find_split_artifacts(tmp.path()).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn test_find_split_artifacts_nonexistent_dir() {
        let files = find_split_artifacts(std::path::Path::new("/nonexistent/path")).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn test_collect_build_targets() {
        use anodize_core::config::BuildConfig;

        let config = Config {
            project_name: "test".to_string(),
            crates: vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                builds: Some(vec![BuildConfig {
                    binary: "myapp".to_string(),
                    targets: Some(vec![
                        "x86_64-unknown-linux-gnu".to_string(),
                        "aarch64-apple-darwin".to_string(),
                    ]),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let opts = anodize_core::context::ContextOptions::default();
        let ctx = anodize_core::context::Context::new(config.clone(), opts);
        let targets = collect_build_targets(&config, &ctx);
        assert_eq!(targets.len(), 2);
        assert!(targets.contains(&"x86_64-unknown-linux-gnu".to_string()));
        assert!(targets.contains(&"aarch64-apple-darwin".to_string()));
    }

    #[test]
    fn test_collect_build_targets_deduplicates() {
        use anodize_core::config::BuildConfig;

        let config = Config {
            project_name: "test".to_string(),
            crates: vec![
                CrateConfig {
                    name: "a".to_string(),
                    path: ".".to_string(),
                    builds: Some(vec![BuildConfig {
                        binary: "a".to_string(),
                        targets: Some(vec!["x86_64-unknown-linux-gnu".to_string()]),
                        ..Default::default()
                    }]),
                    ..Default::default()
                },
                CrateConfig {
                    name: "b".to_string(),
                    path: ".".to_string(),
                    builds: Some(vec![BuildConfig {
                        binary: "b".to_string(),
                        targets: Some(vec!["x86_64-unknown-linux-gnu".to_string()]),
                        ..Default::default()
                    }]),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let opts = anodize_core::context::ContextOptions::default();
        let ctx = anodize_core::context::Context::new(config.clone(), opts);
        let targets = collect_build_targets(&config, &ctx);
        assert_eq!(targets.len(), 1, "should deduplicate targets");
    }

    #[test]
    fn test_collect_build_targets_from_defaults() {
        use anodize_core::config::Defaults;

        let config = Config {
            project_name: "test".to_string(),
            defaults: Some(Defaults {
                targets: Some(vec![
                    "x86_64-unknown-linux-gnu".to_string(),
                    "x86_64-pc-windows-msvc".to_string(),
                ]),
                ..Default::default()
            }),
            crates: vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let opts = anodize_core::context::ContextOptions::default();
        let ctx = anodize_core::context::Context::new(config.clone(), opts);
        let targets = collect_build_targets(&config, &ctx);
        assert_eq!(targets.len(), 2);
    }

    #[test]
    fn test_split_matrix_serialization() {
        let matrix = SplitMatrix {
            split_by: "target".to_string(),
            include: vec![
                MatrixEntry {
                    target: "x86_64-unknown-linux-gnu".to_string(),
                    runner: "ubuntu-latest".to_string(),
                },
                MatrixEntry {
                    target: "aarch64-apple-darwin".to_string(),
                    runner: "macos-latest".to_string(),
                },
            ],
        };
        let json = serde_json::to_string_pretty(&matrix).unwrap();
        assert!(json.contains("x86_64-unknown-linux-gnu"));
        assert!(json.contains("ubuntu-latest"));
        assert!(json.contains("macos-latest"));

        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed["include"].is_array());
        assert_eq!(parsed["include"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_build_matrix_goos_deduplicates() {
        let targets = vec![
            "x86_64-unknown-linux-gnu".to_string(),
            "aarch64-unknown-linux-gnu".to_string(),
            "x86_64-apple-darwin".to_string(),
            "aarch64-apple-darwin".to_string(),
            "x86_64-pc-windows-msvc".to_string(),
        ];
        let matrix = build_matrix(&targets, "goos");
        assert_eq!(matrix.include.len(), 3, "should deduplicate by OS");
        assert_eq!(matrix.include[0].target, "linux");
        assert_eq!(matrix.include[0].runner, "ubuntu-latest");
        assert_eq!(matrix.include[1].target, "darwin");
        assert_eq!(matrix.include[1].runner, "macos-latest");
        assert_eq!(matrix.include[2].target, "windows");
        assert_eq!(matrix.include[2].runner, "windows-latest");
    }

    #[test]
    fn test_build_matrix_target_no_dedup() {
        let targets = vec![
            "x86_64-unknown-linux-gnu".to_string(),
            "aarch64-unknown-linux-gnu".to_string(),
        ];
        let matrix = build_matrix(&targets, "target");
        assert_eq!(
            matrix.include.len(),
            2,
            "target mode should not deduplicate"
        );
    }

    #[test]
    fn test_find_split_contexts() {
        let tmp = tempfile::TempDir::new().unwrap();

        // Create subdirectories with context.json
        let linux_dir = tmp.path().join("linux");
        std::fs::create_dir(&linux_dir).unwrap();
        std::fs::write(linux_dir.join("context.json"), "{}").unwrap();

        let darwin_dir = tmp.path().join("darwin");
        std::fs::create_dir(&darwin_dir).unwrap();
        std::fs::write(darwin_dir.join("context.json"), "{}").unwrap();

        let files = find_split_contexts(tmp.path()).unwrap();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn test_find_split_contexts_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let files = find_split_contexts(tmp.path()).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn test_split_merge_artifact_kind_roundtrip() {
        use anodize_core::artifact::ArtifactKind;

        // All artifact kinds should round-trip through as_str/from_str
        let kinds = [
            ArtifactKind::Binary,
            ArtifactKind::Archive,
            ArtifactKind::Checksum,
            ArtifactKind::DockerImage,
            ArtifactKind::LinuxPackage,
            ArtifactKind::Metadata,
            ArtifactKind::Library,
            ArtifactKind::Wasm,
            ArtifactKind::SourceArchive,
            ArtifactKind::Sbom,
            ArtifactKind::Snap,
            ArtifactKind::DiskImage,
            ArtifactKind::Installer,
            ArtifactKind::MacOsPackage,
        ];
        for kind in &kinds {
            let s = kind.as_str();
            let parsed = ArtifactKind::parse(s);
            assert!(
                parsed.is_some(),
                "ArtifactKind::parse({:?}) should succeed",
                s
            );
            assert_eq!(*kind, parsed.unwrap());
        }
    }

    #[test]
    fn test_artifact_kind_from_str_unknown() {
        use anodize_core::artifact::ArtifactKind;
        assert!(ArtifactKind::parse("unknown_kind").is_none());
        assert!(ArtifactKind::parse("").is_none());
    }
}
