//! `anodize check version-files` — read-only drift guard for the repo-committed
//! files enrolled under `version_files`.
//!
//! For every configured crate the guard resolves that crate's CURRENT declared
//! version (its `[package].version`, or the inherited `[workspace.package].version`
//! in a lockstep workspace) and verifies each enrolled file still contains that
//! version string — bare or `v`-prefixed, word-boundary anchored. A file whose
//! version has drifted, or that is missing/unreadable, is reported and the
//! command exits non-zero so CI can fail the build before a release. When no
//! crate enrolls any `version_files`, the guard exits 0 with a short note.

use crate::commands::bump::cargo_edit::{MemberInfo, WorkspaceInfo, load_workspace};
use crate::commands::bump::plan::resolve_member_version;
use crate::commands::version_files_resolve::resolve_version_files;
use crate::pipeline;
use anodizer_core::config::{Config, CrateConfig};
use anodizer_core::log::{StageLogger, Verbosity};
use anodizer_core::version_files::check_version_present;
use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::path::Path;

pub fn run(config_override: Option<&Path>, verbose: bool, debug: bool, quiet: bool) -> Result<()> {
    let log = StageLogger::new("check", Verbosity::from_flags(quiet, verbose, debug));

    let path = pipeline::find_config_with_logger(config_override, Some(&log))?;
    log.verbose(&format!("loading config from {}", path.display()));
    let config = pipeline::load_config(&path)?;

    let repo_root = std::env::current_dir().context("resolving repo root")?;

    run_guard(&config, &repo_root, &log)
}

/// Core guard logic, factored out of [`run`] so the config-loading shell stays
/// thin. Accumulates findings (one per drifted / unreadable file) and bails
/// non-zero if any are present; otherwise logs an all-in-sync line.
fn run_guard(config: &Config, repo_root: &Path, log: &StageLogger) -> Result<()> {
    let mut findings: Vec<String> = vec![];
    let mut checked = 0usize;

    let units = enrolled_units(config);
    // Load the workspace graph once (shared with `bump`) so each unit's
    // reference version routes through `resolve_member_version` rather than a
    // hand-rolled manifest parse. Defer the error until a unit actually needs
    // it, so a no-`version_files` repo never trips on an unreadable workspace.
    let ws = load_workspace(repo_root);

    // De-duplicate (path, version) pairs so a file enrolled by several crates
    // that resolve to the same version is reported once. The version is part of
    // the key so the per-crate mode — where one shared file would be a genuine
    // conflict at different versions — still surfaces both expectations.
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();

    for unit in units {
        let version = match unit_reference_version(repo_root, &unit, ws.as_ref()) {
            Ok(v) => v,
            Err(e) => {
                findings.push(format!(
                    "{}: cannot read current version: {e:#}",
                    unit.label
                ));
                continue;
            }
        };
        let file_list = unit.files;

        for file in &file_list {
            if !seen.insert((file.clone(), version.clone())) {
                continue;
            }
            match check_version_present(std::slice::from_ref(file), &version) {
                Ok(results) => {
                    checked += 1;
                    let (_, present) = &results[0];
                    if *present {
                        log.verbose(&format!("OK: {file} contains {version}"));
                    } else {
                        findings.push(format!("STALE: {file} (expected {version}, not found)"));
                    }
                }
                Err(e) => {
                    findings.push(format!("STALE: {file} ({e:#})"));
                }
            }
        }
    }

    if checked == 0 && findings.is_empty() {
        log.status("no version_files configured");
        return Ok(());
    }

    if findings.is_empty() {
        log.status(&format!("all {checked} version_files are in sync"));
        Ok(())
    } else {
        for f in &findings {
            log.error(f);
        }
        bail!(
            "version_files check failed with {} finding(s)",
            findings.len()
        );
    }
}

/// One enrollment unit to check: a human label, the crate-directory path whose
/// `Cargo.toml` supplies the reference version, and the effective
/// `version_files` list.
struct EnrolledUnit {
    label: String,
    path: String,
    files: Vec<String>,
}

/// Resolve the enrollment units to check across all three config modes.
///
/// Each configured crate (top-level `crates:` plus every workspace's crates)
/// contributes a unit scoped to that crate's directory and version. A crate's
/// per-crate `version_files` already reflects crate → `defaults` precedence
/// (folded by `apply_defaults` at load time); the top-level
/// `Config.version_files` is the fallback for a crate that enrolls none of its
/// own.
///
/// A lockstep workspace that declares only the top-level `version_files` (no
/// `crates:` block) contributes a single unit scoped to the repo root, whose
/// reference version is the inherited `[workspace.package].version`.
fn enrolled_units(config: &Config) -> Vec<EnrolledUnit> {
    let top_level = config.version_files.as_deref().unwrap_or_default();

    let all_crates: Vec<&CrateConfig> = config
        .crates
        .iter()
        .chain(
            config
                .workspaces
                .iter()
                .flatten()
                .flat_map(|ws| ws.crates.iter()),
        )
        .collect();

    let mut units: Vec<EnrolledUnit> = all_crates
        .iter()
        .filter_map(|c| {
            let files = resolve_version_files(Some(c), Some(config));
            (!files.is_empty()).then(|| EnrolledUnit {
                label: format!("crate '{}'", c.name),
                path: c.path.clone(),
                files,
            })
        })
        .collect();

    // Lockstep with no `crates:` block: the top-level enrollment is checked
    // against the repo-root manifest's (inherited) version.
    if all_crates.is_empty() && !top_level.is_empty() {
        units.push(EnrolledUnit {
            label: "workspace".to_string(),
            path: ".".to_string(),
            files: top_level.to_vec(),
        });
    }

    units
}

/// Resolve a unit's current declared version through the shared workspace graph.
///
/// A unit scoped to a crate directory matches the workspace member rooted there
/// and resolves via [`resolve_member_version`] (literal own version, or the
/// inherited `[workspace.package].version`). The lockstep repo-root unit (no
/// matching member in a virtual workspace) falls back to the workspace package
/// version directly.
fn unit_reference_version(
    repo_root: &Path,
    unit: &EnrolledUnit,
    ws: Result<&WorkspaceInfo, &anyhow::Error>,
) -> Result<String> {
    let ws = ws.map_err(|e| anyhow::anyhow!("{e:#}"))?;
    let unit_dir = repo_root.join(&unit.path);

    if let Some(member) = ws.members.iter().find(|m| member_matches(m, &unit_dir)) {
        return resolve_member_version(member, ws);
    }

    // No member rooted at this directory — the lockstep repo-root unit in a
    // virtual workspace. Its version is the shared `[workspace.package].version`.
    ws.workspace_package_version.clone().with_context(|| {
        format!(
            "{} has no matching workspace member and the repo root has no [workspace.package].version",
            unit_dir.display()
        )
    })
}

/// Whether a workspace member is rooted at `unit_dir`, comparing canonicalized
/// paths so a `.`-relative unit path and the member's absolute `crate_dir`
/// resolve to the same location.
fn member_matches(member: &MemberInfo, unit_dir: &Path) -> bool {
    match (member.crate_dir.canonicalize(), unit_dir.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        // Fall back to a lexical compare when either side can't be canonicalized
        // (e.g. a fixture path that doesn't exist on disk).
        _ => member.crate_dir == unit_dir,
    }
}
