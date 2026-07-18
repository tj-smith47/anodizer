use super::super::helpers;
use crate::pipeline;
use anodizer_core::config::{Config, CrateConfig};
use anodizer_core::log::{StageLogger, Verbosity};
use anyhow::{Result, bail};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub fn run(
    config_override: Option<&Path>,
    workspace: Option<&str>,
    publishers: &[String],
    verbose: bool,
    debug: bool,
    quiet: bool,
) -> Result<()> {
    let log = StageLogger::new("check", Verbosity::from_flags(quiet, verbose, debug));

    let path = pipeline::find_config_with_logger(config_override, Some(&log))?;
    log.verbose(&format!("loading config from {}", path.display()));
    let mut config = pipeline::load_config_logged(&path, &log)?;

    // Auto-infer project_name from Cargo.toml when not set in config so
    // check validates the same project_name the release pipeline would see.
    helpers::infer_project_name(&mut config, &log);

    // The raw whole-file pass belongs to the no-flag form only: a
    // `--workspace` run validates the OVERLAID config below, so a sibling
    // workspace's error (out of this run's scope) never fails it.
    if workspace.is_none() {
        log.status("validating configuration");
        run_checks(&config, true, &log, Path::new("."))?;
    }

    // Resolve the overlaid config ONCE when `--workspace` is given: both the
    // publisher-allowlist validation and the resolved-config check pass below
    // must see the SAME overlay, so building it twice would be a drift seam.
    let overlaid: Option<(&str, Config)> = match workspace {
        Some(ws_name) => {
            let ws = helpers::resolve_workspace(&config, ws_name)?;
            let mut resolved = config.clone();
            helpers::apply_workspace_overlay(&mut resolved, ws);
            Some((ws_name, resolved))
        }
        None => None,
    };

    // `--publishers` is a config-validation selector: each name must be a
    // publisher the active config actually enables, so the configured (not
    // merely the known) set is the floor. Validate against the config the
    // pipeline would resolve for this invocation — overlaid when --workspace
    // is given — so a per-workspace publish block is honored.
    if !publishers.is_empty() {
        let publisher_config = overlaid
            .as_ref()
            .map(|(_, resolved)| resolved.clone())
            .unwrap_or_else(|| config.clone());
        let ctx = anodizer_core::context::Context::new(
            publisher_config,
            anodizer_core::context::ContextOptions::default(),
        );
        // Return the raw validator message: the top-level error handler wraps
        // returned errors in `render_error`, so prefixing it here would double
        // the `Error` label.
        if let Err(msg) = anodizer_stage_publish::registry::validate_publisher_allowlist_configured(
            publishers, &ctx,
        ) {
            bail!("{msg}");
        }
    }

    // When --workspace is specified, validate the resolved (overlaid) config
    if let Some((ws_name, resolved)) = &overlaid {
        log.status(&format!(
            "validating resolved config for workspace '{}'",
            ws_name
        ));
        run_checks(resolved, true, &log, Path::new("."))?;
    }

    Ok(())
}

/// Core validation logic. `check_env` controls whether env/tool checks are run
/// (so tests can skip them). `base_dir` is the on-disk root the Cargo
/// workspace membership guard ([`check_workspace_membership`]) reads from;
/// production callers pass `Path::new(".")`, tests pass a hermetic tempdir
/// (or a sentinel path with no `Cargo.toml`, which no-ops the guard).
pub fn run_checks(
    config: &Config,
    check_env: bool,
    log: &StageLogger,
    base_dir: &Path,
) -> Result<()> {
    let mut errors: Vec<String> = vec![];
    let mut warnings: Vec<String> = vec![];

    let all_crate_names = flatten_crate_names(config);

    check_workspaces(config, &all_crate_names, &mut errors);
    check_top_level_crate_names(config, &mut errors);
    check_top_level_depends_on(config, &all_crate_names, &mut errors);
    check_cycles(config, &mut errors);
    check_top_level_tag_templates(config, &mut errors);
    check_copy_from(config, &mut errors);
    check_target_triples(config, &mut warnings);
    check_changelog(config, &mut warnings);
    check_announce_secret_exposure(config, &mut warnings);
    check_checksum_skip_conflicts(config, &mut warnings);
    check_crate_paths(config, &mut errors);
    check_workspace_membership(config, base_dir, &all_crate_names, &mut errors);
    check_sign_artifact_filters(config, &mut warnings);
    check_checksum_algorithms(config, &mut warnings);
    check_source_format(config, &mut errors);
    check_sbom_configs(config, &mut errors);
    check_blob_configs(config, &mut errors);

    if check_env {
        check_environment(config, &mut warnings);
    }

    report_results(log, &warnings, &errors)
}

/// Flatten the crate-name set across top-level crates and every workspace's
/// crates. The release engine topo-sorts using this flattened set, so
/// `depends_on` references can cross workspace boundaries at release time
/// (e.g. a workspace crate depending on a crate in another workspace). The
/// validator must mirror that resolution to avoid false positives.
fn flatten_crate_names(config: &Config) -> HashSet<&str> {
    config
        .crate_universe()
        .into_iter()
        .map(|c| c.name.as_str())
        .collect()
}

mod content;
mod structure;
mod tooling;

use content::*;
use structure::*;
use tooling::*;

#[cfg(test)]
mod tests;

/// Emit warnings, then either log success or emit all errors and bail.
fn report_results(log: &StageLogger, warnings: &[String], errors: &[String]) -> Result<()> {
    for w in warnings {
        log.warn(w);
    }

    if errors.is_empty() {
        log.status("Config is valid.");
        Ok(())
    } else {
        for e in errors {
            log.error(e);
        }
        bail!("config validation failed with {} error(s)", errors.len());
    }
}

/// DFS-based cycle detection. Returns the cycle path if one exists.
pub fn find_cycle(crates: &[CrateConfig]) -> Option<Vec<String>> {
    let name_to_idx: HashMap<&str, usize> = crates
        .iter()
        .enumerate()
        .map(|(i, c)| (c.name.as_str(), i))
        .collect();

    // Build adjacency list
    let mut adj: Vec<Vec<usize>> = vec![vec![]; crates.len()];
    for (i, c) in crates.iter().enumerate() {
        if let Some(deps) = &c.depends_on {
            for dep in deps {
                if let Some(&j) = name_to_idx.get(dep.as_str()) {
                    // i depends on j → edge j→i in "needs" direction, but for cycle
                    // detection we walk: if i depends on j, j must be processed before i.
                    // We build edges i→j meaning "i needs j" to detect cycles in that graph.
                    adj[i].push(j);
                }
            }
        }
    }

    // 0 = unvisited, 1 = in-stack (gray), 2 = done (black)
    let mut color = vec![0u8; crates.len()];
    let mut parent = vec![usize::MAX; crates.len()];

    for start in 0..crates.len() {
        if color[start] != 0 {
            continue;
        }
        // Iterative DFS
        let mut stack: Vec<(usize, usize)> = vec![(start, 0)]; // (node, adj_index)
        color[start] = 1;

        while let Some((node, adj_idx)) = stack.last_mut() {
            let node = *node;
            if *adj_idx < adj[node].len() {
                let next = adj[node][*adj_idx];
                *adj_idx += 1;
                match color[next] {
                    0 => {
                        color[next] = 1;
                        parent[next] = node;
                        stack.push((next, 0));
                    }
                    1 => {
                        // Back edge → cycle found; reconstruct path
                        let mut cycle = vec![crates[next].name.clone()];
                        let mut cur = node;
                        while cur != next {
                            cycle.push(crates[cur].name.clone());
                            cur = parent[cur];
                            if cur == usize::MAX {
                                break;
                            }
                        }
                        cycle.push(crates[next].name.clone());
                        cycle.reverse();
                        return Some(cycle);
                    }
                    _ => {} // already done
                }
            } else {
                color[node] = 2;
                stack.pop();
            }
        }
    }
    None
}

/// Validate that a tag_template contains a Version placeholder.
/// `context` is a human-readable prefix for the error message (e.g. "crate 'foo'" or
/// "workspace 'ws': crate 'bar'").
fn validate_tag_template(tag_template: &str, context: &str, errors: &mut Vec<String>) {
    if !tag_template.is_empty() && !anodizer_core::git::has_version_placeholder(tag_template) {
        errors.push(format!(
            "{}: tag_template '{}' must contain '{{{{ .Version }}}}' or '{{{{ Version }}}}' \
             (e.g. 'v{{{{ .Version }}}}' or 'myapp-v{{{{ Version }}}}')",
            context, tag_template
        ));
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
