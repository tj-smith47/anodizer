use crate::pipeline;
use anodize_core::artifact;
use anodize_core::config::{Config, CrateConfig, GitHubConfig, WorkspaceConfig};
use anodize_core::context::{Context, ContextOptions};
use anodize_core::git;
use anodize_core::log::{StageLogger, Verbosity};
use anodize_core::template;
use anyhow::{Context as _, Result};
use chrono::Utc;
use std::collections::HashMap;
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
    pub workspace: Option<String>,
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

    // Load .env files early (before template expansion)
    if let Some(ref env_files) = config.env_files {
        anodize_core::config::load_env_files(env_files).map_err(|e| anyhow::anyhow!("{}", e))?;
    }

    // If --workspace is specified, resolve the workspace and overlay its config
    // onto the top-level config (replacing crates, changelog, signs, etc.).
    if let Some(ref ws_name) = opts.workspace {
        let ws = resolve_workspace(&config, ws_name)?.clone();
        config.crates = ws.crates;
        if ws.changelog.is_some() {
            config.changelog = ws.changelog;
        }
        if !ws.signs.is_empty() {
            config.signs = ws.signs;
        }
        if ws.before.is_some() {
            config.before = ws.before;
        }
        if ws.after.is_some() {
            config.after = ws.after;
        }
        if let Some(env_map) = ws.env {
            let merged = config.env.get_or_insert_with(HashMap::new);
            for (k, v) in env_map {
                merged.insert(k, v);
            }
        }
    }

    // Auto-detect GitHub owner/name from git remote when release config is
    // present but the `github` section is omitted. Detect once and reuse.
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
        pipeline::run_hooks(&before.hooks, "before", opts.dry_run)?;
    }

    let ctx_opts = ContextOptions {
        snapshot: opts.snapshot,
        nightly: opts.nightly,
        dry_run: opts.dry_run,
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

    // Populate user-defined env vars into template context
    if let Some(ref env_map) = config.env {
        for (key, value) in env_map {
            ctx.template_vars_mut().set_env(key, value);
        }
    }

    // Resolve tag and populate git variables before running the pipeline.
    // Each selected crate gets its own tag via tag_template; for now we resolve
    // the first selected crate's tag (monorepo support will iterate later).
    let first_crate = ctx
        .options
        .selected_crates
        .first()
        .and_then(|name| config.crates.iter().find(|c| &c.name == name))
        .or_else(|| config.crates.first());

    if let Some(crate_cfg) = first_crate {
        // Find latest existing tag matching this crate's tag_template
        let latest_tag = git::find_latest_tag_matching(&crate_cfg.tag_template)
            .ok()
            .flatten();

        // Determine the tag to use for git info.
        // Use latest existing tag as base, or fall back to v0.0.0 for first release.
        let tag = latest_tag.clone().unwrap_or_else(|| "v0.0.0".to_string());

        match git::detect_git_info(&tag) {
            Ok(mut git_info) => {
                // Set previous tag
                git_info.previous_tag = latest_tag;
                ctx.git_info = Some(git_info);
                ctx.populate_git_vars();
            }
            Err(e) => {
                log.warn(&format!("could not detect git info: {e}"));
                // Still populate snapshot/draft vars even without git info
                ctx.populate_git_vars();
            }
        }
    } else {
        // No crates configured; populate non-git vars only
        ctx.populate_git_vars();
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

    let p = pipeline::build_release_pipeline();
    let result = p.run(&mut ctx);

    // Post-pipeline: report sizes and write metadata (only if pipeline succeeded, not in dry-run)
    if result.is_ok() && !ctx.is_dry_run() {
        // Print artifact size table if configured
        if config.report_sizes.unwrap_or(false) {
            artifact::print_size_report(&ctx.artifacts);
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

    // Run custom publishers (only if pipeline succeeded)
    if result.is_ok()
        && let Some(ref publishers) = config.publishers
        && !publishers.is_empty()
    {
        log.status("running custom publishers...");
        super::publisher::run_publishers(
            publishers,
            ctx.artifacts.all(),
            ctx.template_vars(),
            opts.dry_run,
        )?;
    }

    // Run hooks after pipeline (only if pipeline succeeded)
    if result.is_ok()
        && let Some(after) = &config.after
    {
        pipeline::run_hooks(&after.hooks, "after", opts.dry_run)?;
    }

    result
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
                let sv = git::parse_semver(tag).ok();
                if let Some(sv) = sv {
                    let is_older = oldest_tag
                        .as_ref()
                        .and_then(|t| git::parse_semver(t).ok())
                        .map(|osv| {
                            sv.major < osv.major
                                || (sv.major == osv.major && sv.minor < osv.minor)
                                || (sv.major == osv.major
                                    && sv.minor == osv.minor
                                    && sv.patch < osv.patch)
                        })
                        .unwrap_or(true);
                    if is_older {
                        oldest_tag = Some(tag.clone());
                    }
                }
            }
        }
    }

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
    use std::collections::{HashMap, VecDeque};

    let selected_set: std::collections::HashSet<&str> =
        selected.iter().map(|s| s.as_str()).collect();

    // Only consider selected crates
    let filtered: Vec<&CrateConfig> = all_crates
        .iter()
        .filter(|c| selected_set.contains(c.name.as_str()))
        .collect();

    let name_to_idx: HashMap<&str, usize> = filtered
        .iter()
        .enumerate()
        .map(|(i, c)| (c.name.as_str(), i))
        .collect();

    let n = filtered.len();
    let mut in_degree = vec![0usize; n];
    let mut adj: Vec<Vec<usize>> = vec![vec![]; n];

    for (i, c) in filtered.iter().enumerate() {
        if let Some(deps) = &c.depends_on {
            for dep in deps {
                if let Some(&j) = name_to_idx.get(dep.as_str()) {
                    adj[j].push(i);
                    in_degree[i] += 1;
                }
            }
        }
    }

    let mut queue: VecDeque<usize> = (0..n).filter(|&i| in_degree[i] == 0).collect();
    let mut result = vec![];

    while let Some(node) = queue.pop_front() {
        result.push(filtered[node].name.clone());
        for &next in &adj[node] {
            in_degree[next] -= 1;
            if in_degree[next] == 0 {
                queue.push_back(next);
            }
        }
    }

    // Append any remaining (cycle case — shouldn't happen post-check)
    for c in &filtered {
        if !result.contains(&c.name) {
            result.push(c.name.clone());
        }
    }

    result
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

        // Simulate the overlay logic from run() for --workspace=ws
        let ws = config
            .workspaces
            .as_ref()
            .unwrap()
            .iter()
            .find(|w| w.name == "ws")
            .unwrap()
            .clone();

        config.crates = ws.crates;
        if ws.changelog.is_some() {
            config.changelog = ws.changelog;
        }
        if !ws.signs.is_empty() {
            config.signs = ws.signs;
        }
        if let Some(env_map) = ws.env {
            let merged = config.env.get_or_insert_with(HashMap::new);
            for (k, v) in env_map {
                merged.insert(k, v);
            }
        }

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
}
