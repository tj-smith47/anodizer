use std::path::PathBuf;
use anyhow::{Context as _, Result};
use anodize_core::context::{Context, ContextOptions};
use anodize_core::config::{CrateConfig, GitHubConfig};
use anodize_core::artifact;
use anodize_core::git;
use crate::pipeline;

pub struct ReleaseOpts {
    pub crate_names: Vec<String>,
    pub all: bool,
    pub force: bool,
    pub snapshot: bool,
    pub dry_run: bool,
    pub clean: bool,
    pub skip: Vec<String>,
    pub token: Option<String>,
    pub verbose: bool,
    pub debug: bool,
    pub config_override: Option<PathBuf>,
    pub parallelism: usize,
    pub single_target: Option<String>,
    pub release_notes: Option<PathBuf>,
}

pub fn run(opts: ReleaseOpts) -> Result<()> {
    let mut config = pipeline::load_config(&pipeline::find_config(opts.config_override.as_deref())?)?;

    // Auto-detect GitHub owner/name from git remote when release config is
    // present but the `github` section is omitted. Detect once and reuse.
    let detected_github = git::detect_github_repo().ok();
    for crate_cfg in &mut config.crates {
        if let Some(ref mut release) = crate_cfg.release
            && release.github.is_none()
        {
            if let Some((ref owner, ref name)) = detected_github {
                release.github = Some(GitHubConfig { owner: owner.clone(), name: name.clone() });
            } else {
                eprintln!("[release] warning: could not auto-detect GitHub repo from git remote");
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

    // TODO: call detect_git_info() + ctx.populate_git_vars() once tag resolution is implemented
    let p = pipeline::build_release_pipeline();
    let result = p.run(&mut ctx);

    // Post-pipeline: report sizes and write metadata (only if pipeline succeeded, not in dry-run)
    if result.is_ok() && !ctx.is_dry_run() {
        // Print artifact size table if configured
        if config.report_sizes.unwrap_or(false) {
            artifact::print_size_report(&ctx.artifacts);
        }

        // Write metadata.json to dist/
        let metadata = ctx.artifacts.to_metadata_json()
            .context("failed to serialize artifact metadata")?;
        let dist = &config.dist;
        std::fs::create_dir_all(dist)
            .with_context(|| format!("failed to create dist directory: {}", dist.display()))?;
        let metadata_path = dist.join("metadata.json");
        let json_str = serde_json::to_string_pretty(&metadata)
            .context("failed to serialize metadata JSON")?;
        std::fs::write(&metadata_path, &json_str)
            .with_context(|| format!("failed to write {}", metadata_path.display()))?;
        eprintln!("  wrote {}", metadata_path.display());
    }

    // Run hooks after pipeline (only if pipeline succeeded)
    if result.is_ok()
        && let Some(after) = &config.after {
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
                    let is_older = oldest_tag.as_ref().and_then(|t| git::parse_semver(t).ok()).map(|osv| {
                        sv.major < osv.major
                            || (sv.major == osv.major && sv.minor < osv.minor)
                            || (sv.major == osv.major && sv.minor == osv.minor && sv.patch < osv.patch)
                    }).unwrap_or(true);
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
        .args(["diff", "--name-only", &format!("{}..HEAD", tag), "--", "Cargo.toml", "Cargo.lock"])
        .output()?;
    if output.status.success() {
        Ok(!String::from_utf8_lossy(&output.stdout).trim().is_empty())
    } else {
        // If git command fails (e.g. not a git repo), assume no changes
        Ok(false)
    }
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
    for (i, c) in filtered.iter().enumerate() {
        if !result.contains(&c.name) {
            result.push(c.name.clone());
        }
        let _ = i;
    }

    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use anodize_core::config::CrateConfig;

    fn make_crate(name: &str, deps: Option<Vec<&str>>) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: format!("{}-v{{{{ .Version }}}}", name),
            depends_on: deps.map(|d| d.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        }
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
}
