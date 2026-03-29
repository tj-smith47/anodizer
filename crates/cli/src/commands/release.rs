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
    pub workspace: Option<String>,
    pub draft: bool,
    pub release_header: Option<PathBuf>,
    pub release_footer: Option<PathBuf>,
    pub split: bool,
    pub merge: bool,
}

pub fn run(opts: ReleaseOpts) -> Result<()> {
    let log = StageLogger::new(
        "release",
        Verbosity::from_flags(opts.quiet, opts.verbose, opts.debug),
    );

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
        release.header = Some(header_content);
    }
    if let Some(ref footer_path) = opts.release_footer {
        let footer_content = std::fs::read_to_string(footer_path).with_context(|| {
            format!(
                "failed to read release footer file: {}",
                footer_path.display()
            )
        })?;
        let release = config.release.get_or_insert_with(Default::default);
        release.footer = Some(footer_content);
    }

    if opts.clean {
        let dist = &config.dist;
        if dist.exists() {
            std::fs::remove_dir_all(dist)?;
        }
    }

    // Determine selected crates
    let selected = if opts.all {
        if opts.force {
            // --all --force: include every crate
            config.crates.iter().map(|c| c.name.clone()).collect()
        } else {
            detect_changed_crates(&config.crates)?
        }
    } else {
        opts.crate_names.clone()
    };

    // Topological sort of selected crates (respect depends_on ordering)
    let selected_sorted = topo_sort_selected(&config.crates, &selected);

    // Run hooks before pipeline
    if let Some(before) = &config.before {
        pipeline::run_hooks(&before.hooks, "before", opts.dry_run, &log)?;
    }

    let ctx_opts = ContextOptions {
        snapshot: opts.snapshot,
        nightly: opts.nightly,
        dry_run: opts.dry_run,
        quiet: opts.quiet,
        verbose: opts.verbose,
        debug: opts.debug,
        skip_stages: opts.skip,
        selected_crates: selected_sorted,
        token: opts.token,
        parallelism: opts.parallelism,
        single_target: opts.single_target,
        release_notes_path: opts.release_notes,
    };
    let mut ctx = Context::new(config.clone(), ctx_opts);
    ctx.populate_time_vars();
    ctx.populate_runtime_vars();

    // Populate user-defined env vars into template context
    helpers::setup_env(&mut ctx, &config, &log)?;

    // Resolve tag and populate git variables before running the pipeline.
    helpers::resolve_git_context(&mut ctx, &config, &log);

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

    // Apply snapshot name_template if configured.
    if ctx.is_snapshot()
        && let Some(ref snapshot_cfg) = config.snapshot
    {
        let rendered_name = template::render(&snapshot_cfg.name_template, ctx.template_vars())
            .with_context(|| {
                format!(
                    "failed to render snapshot name_template: {}",
                    snapshot_cfg.name_template
                )
            })?;
        ctx.template_vars_mut().set("ReleaseName", &rendered_name);
        log.verbose(&format!("snapshot: release_name={}", rendered_name));
    }

    // --split: run only the build stage, serialize artifacts to dist/, then exit
    if opts.split {
        return run_split(&mut ctx, &config, &log);
    }

    // --merge: load artifacts from split jobs, then run post-build stages
    if opts.merge {
        return run_merge(&mut ctx, &config, &log, opts.dry_run);
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
    if !dry_run {
        // Print artifact size table if configured
        if config.report_sizes.unwrap_or(false) {
            artifact::print_size_report(&ctx.artifacts, log);
        }

        // Write metadata.json to dist/
        let metadata = ctx
            .artifacts
            .to_metadata_json()
            .context("failed to serialize artifact metadata")?;
        let dist = &config.dist;
        std::fs::create_dir_all(dist)
            .with_context(|| format!("failed to create dist directory: {}", dist.display()))?;
        let metadata_path = dist.join("metadata.json");
        let json_str =
            serde_json::to_string_pretty(&metadata).context("failed to serialize metadata JSON")?;
        std::fs::write(&metadata_path, &json_str)
            .with_context(|| format!("failed to write {}", metadata_path.display()))?;
        log.status(&format!("wrote {}", metadata_path.display()));
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
        )?;
    }

    // Run after hooks
    if let Some(after) = &config.after {
        pipeline::run_hooks(&after.hooks, "after", dry_run, log)?;
    }

    Ok(())
}

/// Detect which crates have changes since their last tag.
fn detect_changed_crates(crates: &[CrateConfig]) -> Result<Vec<String>> {
    let mut changed = vec![];
    let mut oldest_tag: Option<String> = None;

    for c in crates {
        let latest_tag = git::find_latest_tag_matching(&c.tag_template)?;
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
                if let Ok(sv) = git::parse_semver(tag) {
                    let is_older = oldest_tag
                        .as_ref()
                        .and_then(|t| git::parse_semver(t).ok())
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
// Split/Merge CI Fan-Out
// ---------------------------------------------------------------------------

/// Serializable artifact for split/merge JSON.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct SplitArtifact {
    kind: String,
    path: String,
    target: Option<String>,
    crate_name: String,
    metadata: std::collections::HashMap<String, String>,
}

/// The JSON output of a --split build job.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
struct SplitOutput {
    /// The target triple that was built (if single-target).
    target: Option<String>,
    /// Artifacts produced by this split job.
    artifacts: Vec<SplitArtifact>,
}

/// GitHub Actions matrix definition for split builds.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
struct GithubActionsMatrix {
    /// How the build was split (e.g., "target").
    split_by: String,
    target: Vec<String>,
}

/// Run in --split mode: execute only the build stage, then serialize artifacts.
fn run_split(
    ctx: &mut Context,
    config: &Config,
    log: &anodize_core::log::StageLogger,
) -> Result<()> {
    log.status("running in split mode (build only)...");

    // Run only the build stage
    let p = pipeline::build_split_pipeline();
    p.run(ctx, log)?;

    // Serialize artifacts to dist/
    let dist = &config.dist;
    std::fs::create_dir_all(dist)
        .with_context(|| format!("create dist directory: {}", dist.display()))?;

    let artifacts: Vec<SplitArtifact> = ctx
        .artifacts
        .all()
        .iter()
        .map(|a| SplitArtifact {
            kind: a.kind.as_str().to_string(),
            path: a.path.to_string_lossy().into_owned(),
            target: a.target.clone(),
            crate_name: a.crate_name.clone(),
            metadata: a.metadata.clone(),
        })
        .collect();

    let split_output = SplitOutput {
        target: ctx.options.single_target.clone(),
        artifacts,
    };

    let json = serde_json::to_string_pretty(&split_output).context("serialize split output")?;

    let output_path = dist.join("artifacts.json");
    std::fs::write(&output_path, &json)
        .with_context(|| format!("write split artifacts to {}", output_path.display()))?;

    log.status(&format!(
        "split: wrote {} artifact(s) to {}",
        split_output.artifacts.len(),
        output_path.display()
    ));

    // Generate a GitHub Actions matrix JSON based on the partial.by strategy
    let split_by = config
        .partial
        .as_ref()
        .and_then(|p| p.by.as_deref())
        .unwrap_or("target");

    let targets = collect_build_targets(config, ctx);
    if !targets.is_empty() {
        let matrix = GithubActionsMatrix {
            split_by: split_by.to_string(),
            target: targets,
        };
        let matrix_json = serde_json::to_string(&matrix).context("serialize matrix")?;
        let matrix_path = dist.join("matrix.json");
        std::fs::write(&matrix_path, &matrix_json)
            .with_context(|| format!("write matrix to {}", matrix_path.display()))?;
        log.status(&format!(
            "split: wrote matrix to {} (split by: {})",
            matrix_path.display(),
            split_by
        ));
    }

    Ok(())
}

/// Run in --merge mode: load artifacts from split jobs, then run post-build stages.
fn run_merge(
    ctx: &mut Context,
    config: &Config,
    log: &anodize_core::log::StageLogger,
    dry_run: bool,
) -> Result<()> {
    log.status("running in merge mode (post-build stages)...");

    let dist = &config.dist;

    // Find all artifacts.json files in dist/ subdirectories
    let artifact_files = find_split_artifacts(dist)?;
    if artifact_files.is_empty() {
        anyhow::bail!(
            "merge: no artifacts.json files found in {}. \
             Run `anodize release --split` first to produce split outputs.",
            dist.display()
        );
    }

    // Load and merge all split artifacts, deduplicating by path
    let mut total_loaded = 0;
    let mut seen_paths = std::collections::HashSet::new();
    for artifact_file in &artifact_files {
        let content = std::fs::read_to_string(artifact_file)
            .with_context(|| format!("read split artifacts: {}", artifact_file.display()))?;
        let split_output: SplitOutput = serde_json::from_str(&content)
            .with_context(|| format!("parse split artifacts: {}", artifact_file.display()))?;

        for sa in &split_output.artifacts {
            // Deduplicate by path to handle overlapping artifact files
            if !seen_paths.insert(sa.path.clone()) {
                continue;
            }
            let kind = artifact::ArtifactKind::parse(sa.kind.as_str())
                .ok_or_else(|| anyhow::anyhow!("unknown artifact kind: {}", sa.kind))?;
            ctx.artifacts.add(artifact::Artifact {
                kind,
                path: PathBuf::from(&sa.path),
                target: sa.target.clone(),
                crate_name: sa.crate_name.clone(),
                metadata: sa.metadata.clone(),
            });
            total_loaded += 1;
        }
    }

    log.status(&format!(
        "merge: loaded {} artifact(s) from {} file(s)",
        total_loaded,
        artifact_files.len()
    ));

    // Run post-build pipeline stages (everything except build and upx)
    let p = pipeline::build_merge_pipeline();
    let result = p.run(ctx, log);

    if result.is_ok() {
        run_post_pipeline(ctx, config, dry_run, log)?;
    }

    result
}

/// Collect all build targets from config for matrix generation,
/// filtering out targets excluded by `defaults.ignore`.
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

        // Also check default targets
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

/// Find all artifacts.json files in dist/ directory.
/// Searches `dist/artifacts.json` and `dist/*/artifacts.json` (one level deep).
/// Duplicate artifacts are deduplicated by path during merge.
fn find_split_artifacts(dist: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();

    // Check top-level artifacts.json
    let top = dist.join("artifacts.json");
    if top.exists() {
        files.push(top);
    }

    // Check subdirectories (e.g., dist/linux/artifacts.json, dist/x86_64-unknown-linux-gnu/artifacts.json)
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

    #[test]
    fn test_split_artifact_serialization_roundtrip() {
        let artifact = SplitArtifact {
            kind: "binary".to_string(),
            path: "/tmp/myapp".to_string(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "linux-build".to_string())]),
        };

        let json = serde_json::to_string(&artifact).unwrap();
        let deserialized: SplitArtifact = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.kind, "binary");
        assert_eq!(deserialized.path, "/tmp/myapp");
        assert_eq!(
            deserialized.target.as_deref(),
            Some("x86_64-unknown-linux-gnu")
        );
        assert_eq!(deserialized.crate_name, "myapp");
        assert_eq!(deserialized.metadata.get("id").unwrap(), "linux-build");
    }

    #[test]
    fn test_split_output_serialization_roundtrip() {
        let output = SplitOutput {
            target: Some("aarch64-apple-darwin".to_string()),
            artifacts: vec![
                SplitArtifact {
                    kind: "binary".to_string(),
                    path: "/tmp/myapp".to_string(),
                    target: Some("aarch64-apple-darwin".to_string()),
                    crate_name: "myapp".to_string(),
                    metadata: HashMap::new(),
                },
                SplitArtifact {
                    kind: "archive".to_string(),
                    path: "/tmp/myapp.tar.gz".to_string(),
                    target: Some("aarch64-apple-darwin".to_string()),
                    crate_name: "myapp".to_string(),
                    metadata: HashMap::new(),
                },
            ],
        };

        let json = serde_json::to_string_pretty(&output).unwrap();
        let deserialized: SplitOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.target.as_deref(), Some("aarch64-apple-darwin"));
        assert_eq!(deserialized.artifacts.len(), 2);
        assert_eq!(deserialized.artifacts[0].kind, "binary");
        assert_eq!(deserialized.artifacts[1].kind, "archive");
    }

    #[test]
    fn test_split_output_no_target() {
        let output = SplitOutput {
            target: None,
            artifacts: vec![],
        };
        let json = serde_json::to_string(&output).unwrap();
        let deserialized: SplitOutput = serde_json::from_str(&json).unwrap();
        assert!(deserialized.target.is_none());
        assert!(deserialized.artifacts.is_empty());
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
    fn test_github_actions_matrix_serialization() {
        let matrix = GithubActionsMatrix {
            split_by: "target".to_string(),
            target: vec![
                "x86_64-unknown-linux-gnu".to_string(),
                "aarch64-apple-darwin".to_string(),
                "x86_64-pc-windows-msvc".to_string(),
            ],
        };
        let json = serde_json::to_string(&matrix).unwrap();
        assert!(json.contains("x86_64-unknown-linux-gnu"));
        assert!(json.contains("aarch64-apple-darwin"));
        assert!(json.contains("x86_64-pc-windows-msvc"));

        // Should be parseable as a JSON object with "target" array
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed["target"].is_array());
        assert_eq!(parsed["target"].as_array().unwrap().len(), 3);
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
