//! Shared resolution of the effective `version_files` enrollment list.
//!
//! A single source of truth for the crate → top-level precedence used at tag
//! time (to rewrite enrolled files into the bump commit) and by the
//! `check version-files` drift guard. Keeping one copy prevents the two paths
//! from drifting apart.

use anodizer_core::config::{Config, CrateConfig};

/// Resolve the effective `version_files` list for a crate.
///
/// Precedence: a crate's own `version_files` (already reflecting crate →
/// `defaults` folding applied at config load) wins; otherwise the top-level
/// `Config.version_files` is the fallback (the lockstep enrollment). Returns an
/// empty list when neither is set.
pub(crate) fn resolve_version_files(
    crate_cfg: Option<&CrateConfig>,
    config: Option<&Config>,
) -> Vec<String> {
    crate_cfg
        .and_then(|c| c.version_files.clone())
        .or_else(|| config.and_then(|c| c.version_files.clone()))
        .unwrap_or_default()
}
