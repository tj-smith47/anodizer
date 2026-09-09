//! Shared resolution of the effective `version_files` enrollment list.
//!
//! A single source of truth for WHICH files are enrolled and WHO owns each of
//! them, used at tag time (to rewrite enrolled files into the bump commit) and
//! by the `check version-files` drift guard. One resolver keeps the set `tag`
//! rewrites identical to the set `check` validates, in every config mode.

use anodizer_core::config::{Config, CrateConfig, VersionFileEntry};

/// Resolve the effective `version_files` list for a crate.
///
/// Precedence: a crate's own `version_files` (already reflecting crate →
/// `defaults` folding applied at config load) wins; otherwise the top-level
/// `Config.version_files` is the fallback (the lockstep enrollment). Returns an
/// empty list when neither is set.
pub(crate) fn resolve_version_files(
    crate_cfg: Option<&CrateConfig>,
    config: Option<&Config>,
) -> Vec<VersionFileEntry> {
    crate_cfg
        .and_then(|c| c.version_files.clone())
        .or_else(|| config.and_then(|c| c.version_files.clone()))
        .unwrap_or_default()
}

/// One enrolled unit: the owner whose bump moves these files, the directory
/// whose `Cargo.toml` supplies the reference version, and the effective list.
pub(crate) struct EnrolledUnit {
    /// Crate name, or the project name for the synthetic repo-root unit.
    pub owner: String,
    /// Crate directory, or `"."` for the repo-root unit.
    pub path: String,
    pub files: Vec<VersionFileEntry>,
    /// `true` only for the synthetic repo-root unit a config with no `crates:`
    /// block contributes. Its reference version is the root manifest's own
    /// version, or the shared `[workspace.package].version` — the only case
    /// allowed to fall back to it.
    pub is_lockstep_root: bool,
}

impl EnrolledUnit {
    /// How the unit is named in a user-facing finding.
    pub fn label(&self) -> String {
        if self.is_lockstep_root {
            "workspace".to_string()
        } else {
            format!("crate '{}'", self.owner)
        }
    }
}

/// Resolve every enrolled unit for a config, across all config modes — the one
/// list `tag` rewrites and `check version-files` validates.
///
/// Each configured crate (top-level `crates:` plus every workspace's crates)
/// contributes a unit scoped to that crate's directory and version. A crate's
/// per-crate `version_files` already reflects crate → `defaults` precedence
/// (folded by `apply_defaults` at config load); the top-level
/// `Config.version_files` is the fallback for a crate that enrolls none of its
/// own — so a top-level list is checked, and rewritten, once per crate that
/// inherits it rather than once for the repo.
///
/// A config with no `crates:` block contributes a single unit scoped to the
/// repo root, owned by the project.
pub(crate) fn enrolled_units(config: &Config) -> Vec<EnrolledUnit> {
    let top_level = config.version_files.as_deref().unwrap_or_default();
    let all_crates: Vec<&CrateConfig> = config.crate_universe();

    let mut units: Vec<EnrolledUnit> = all_crates
        .iter()
        .filter_map(|c| {
            let files = resolve_version_files(Some(c), Some(config));
            (!files.is_empty()).then(|| EnrolledUnit {
                owner: c.name.clone(),
                path: c.path.clone(),
                files,
                is_lockstep_root: false,
            })
        })
        .collect();

    if all_crates.is_empty() && !top_level.is_empty() {
        units.push(EnrolledUnit {
            owner: config.project_name.clone(),
            path: ".".to_string(),
            files: top_level.to_vec(),
            is_lockstep_root: true,
        });
    }

    units
}
