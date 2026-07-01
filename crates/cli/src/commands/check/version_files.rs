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
use anodizer_stage_build::version_sync::read_cargo_version;
use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::path::Path;

pub fn run(config_override: Option<&Path>, verbose: bool, debug: bool, quiet: bool) -> Result<()> {
    let log = StageLogger::new("check", Verbosity::from_flags(quiet, verbose, debug));

    // Resolve enrolled (repo-root-relative) paths against the discovered
    // workspace root, not the process cwd, so the guard inspects the same files
    // `tag`'s rewrite touches even when run from a subdirectory. A rootless
    // layout (no Cargo.toml in the chain) has no workspace root to discover, so
    // fall back to the process cwd — the crate is then its own root.
    let repo_root = match crate::commands::helpers::discover_workspace_root(config_override) {
        Ok(root) => root,
        Err(_) => std::env::current_dir().context("resolving repo root")?,
    };

    // Load the config at that root (an explicit `--config` override still wins)
    // so a subdirectory invocation finds the repo-root `.anodizer.yaml` and its
    // `version_files` enrollment rather than whatever sits in the cwd.
    let config = match config_override {
        Some(p) => {
            log.verbose(&format!("loading config from {}", p.display()));
            pipeline::load_config(p)?
        }
        None => {
            log.verbose(&format!("loading config from {}", repo_root.display()));
            pipeline::load_repo_config(&repo_root)?
        }
    };

    run_guard(&config, &repo_root, &log)
}

/// Core guard logic, factored out of [`run`] so the config-loading shell stays
/// thin. Accumulates findings (one per drifted / unreadable file) and bails
/// non-zero if any are present; otherwise logs an all-in-sync line.
fn run_guard(config: &Config, repo_root: &Path, log: &StageLogger) -> Result<()> {
    let mut findings: Vec<String> = vec![];
    let mut checked = 0usize;

    let units = enrolled_units(config);
    // Load the workspace graph once (shared with `bump`) so a unit that matches
    // a member resolves via `resolve_member_version` — correctly handling
    // inherited `version.workspace = true`. A rootless / non-cargo layout (no
    // root `Cargo.toml`) has no graph (`Ok(None)`), and a standalone crate
    // that isn't a `[workspace].members` entry resolves against its own
    // manifest instead, so the ABSENCE of a graph never fails the whole
    // command. A `Cargo.toml` that exists but is malformed is a real error
    // (`?`), not silently treated as "no graph" — which would resolve
    // version-file expectations against a wrong base.
    let ws = load_workspace(repo_root)?;

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
            // Enrolled paths are repo-root-relative; resolve against the
            // discovered root for the read while keeping `file` (relative) for
            // user-facing messages.
            let abs = repo_root.join(file).to_string_lossy().into_owned();
            match check_version_present(std::slice::from_ref(&abs), &version) {
                Ok(results) => {
                    checked += 1;
                    let present = results.first().map(|(_, p)| *p).unwrap_or(false);
                    if present {
                        log.verbose(&format!("{file} contains {version}"));
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
    /// `true` only for the synthetic repo-root unit a lockstep workspace with no
    /// `crates:` block contributes. Its reference version is the shared
    /// `[workspace.package].version` — the only case allowed to fall back to it.
    is_lockstep_root: bool,
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
                is_lockstep_root: false,
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
            is_lockstep_root: true,
        });
    }

    units
}

/// Resolve a unit's current declared version, dispatching on what the unit is:
///
/// 1. **Workspace member** — when the unit's directory matches a
///    `[workspace].members` entry, resolve via [`resolve_member_version`], which
///    handles both a literal own version and an inherited
///    `version.workspace = true`.
/// 2. **Synthetic lockstep repo-root unit** — falls back to the shared
///    `[workspace.package].version`. This is the ONLY case allowed to use that
///    fallback, so a real crate that simply isn't a member never silently
///    resolves against the workspace version.
/// 3. **Standalone / non-member crate** — a configured crate excluded from the
///    member globs, outside them, or in a rootless (no `[workspace]`) layout.
///    Read its own `[package].version` via the sanctioned single-crate accessor
///    [`read_cargo_version`].
fn unit_reference_version(
    repo_root: &Path,
    unit: &EnrolledUnit,
    ws: Option<&WorkspaceInfo>,
) -> Result<String> {
    let unit_dir = repo_root.join(&unit.path);

    if let Some(ws) = ws
        && let Some(member) = ws.members.iter().find(|m| member_matches(m, &unit_dir))
    {
        return resolve_member_version(member, ws);
    }

    if unit.is_lockstep_root {
        return ws
            .and_then(|ws| ws.workspace_package_version.clone())
            .with_context(|| {
                format!(
                    "lockstep workspace at {} has no [workspace.package].version",
                    unit_dir.display()
                )
            });
    }

    // Standalone / non-member crate: read its own manifest version through the
    // canonical single-crate accessor the tag path uses.
    let own = read_cargo_version(&unit_dir.to_string_lossy())?;
    // "0.0.0" is `read_cargo_version`'s sentinel for a non-literal
    // `[package].version` — i.e. `version.workspace = true`. A non-member crate
    // that inherits the workspace version has its real reference there, so
    // prefer the workspace value over emitting a nonsense `expected 0.0.0`.
    if own == "0.0.0"
        && let Some(ws_version) = ws.and_then(|w| w.workspace_package_version.clone())
    {
        return Ok(ws_version);
    }
    Ok(own)
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
