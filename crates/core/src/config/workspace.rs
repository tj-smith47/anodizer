use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    ChangelogConfig, CrateConfig, HooksConfig, SignConfig, deserialize_binary_signs,
    deserialize_signs, signs_schema,
};

// ---------------------------------------------------------------------------
// WorkspaceConfig
// ---------------------------------------------------------------------------

/// A workspace represents an independent project root within a monorepo.
/// Each workspace has its own crates, changelog, and release configuration,
/// allowing independently-versioned components that aren't Cargo workspace members.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct WorkspaceConfig {
    /// Workspace identifier used in logs and template variables.
    pub name: String,
    /// Crates belonging to this workspace.
    pub crates: Vec<CrateConfig>,
    /// Changelog configuration for this workspace.
    pub changelog: Option<ChangelogConfig>,
    /// Signing configurations for binaries, archives, and checksums.
    #[serde(default, deserialize_with = "deserialize_signs")]
    #[schemars(schema_with = "signs_schema")]
    pub signs: Vec<SignConfig>,
    /// Binary-specific signing configs (same shape as `signs` but only for
    /// binary artifacts). The `artifacts` field on each entry is constrained
    /// at parse time to `binary` / `none` (or omitted) — a broader filter on
    /// `binary_signs` would silently match nothing because the loop only
    /// iterates Binary artifacts. Constraint lives in `deserialize_binary_signs`.
    #[serde(default, deserialize_with = "deserialize_binary_signs")]
    #[schemars(schema_with = "signs_schema")]
    pub binary_signs: Vec<SignConfig>,
    /// Hooks run before this workspace's pipeline starts.
    pub before: Option<HooksConfig>,
    /// Hooks run after this workspace's pipeline completes.
    pub after: Option<HooksConfig>,
    /// Environment variables scoped to this workspace.
    ///
    /// List of `KEY=VALUE` strings. Order is preserved.
    /// Values are template-rendered at pipeline startup.
    #[serde(default)]
    pub env: Option<Vec<String>>,
    /// Pipeline stages to skip when releasing this workspace.
    /// Stage names match the CLI `--skip` flag (e.g., `announce`, `publish`).
    #[serde(default)]
    pub skip: Vec<String>,
}
