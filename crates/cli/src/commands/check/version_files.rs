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

use crate::commands::bump::cargo_edit::load_workspace;
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

    // De-duplicate (path, version) pairs so a file enrolled by several crates
    // that resolve to the same version is reported once. The version is part of
    // the key so the per-crate mode — where one shared file would be a genuine
    // conflict at different versions — still surfaces both expectations.
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();

    for unit in enrolled_units(config) {
        let version = match crate_reference_version(repo_root, &unit.path) {
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
            let files = c
                .version_files
                .clone()
                .unwrap_or_else(|| top_level.to_vec());
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

/// Resolve a crate's current declared version: its literal `[package].version`,
/// or the inherited `[workspace.package].version` from the repo-root manifest
/// when the crate uses `version.workspace = true` (lockstep mode).
fn crate_reference_version(repo_root: &Path, crate_path: &str) -> Result<String> {
    use toml_edit::{DocumentMut, Item, Value};

    let manifest = repo_root.join(crate_path).join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest)
        .with_context(|| format!("failed to read {}", manifest.display()))?;
    let doc = text
        .parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", manifest.display()))?;

    let version_item = doc.get("package").and_then(|p| p.get("version"));
    if let Some(Item::Value(Value::String(s))) = version_item {
        return Ok(s.value().to_string());
    }

    // `version.workspace = true` (inline table or dotted table), or no
    // `[package].version` at all: fall back to the workspace-inherited version.
    load_workspace(repo_root)
        .with_context(|| format!("resolving inherited version for {}", manifest.display()))?
        .workspace_package_version
        .with_context(|| {
            format!(
                "{} inherits the workspace version but the repo root has no [workspace.package].version",
                manifest.display()
            )
        })
}
