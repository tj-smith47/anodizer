use super::*;

// ---------------------------------------------------------------------------
// VersionSyncConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct VersionSyncConfig {
    /// When true, synchronize the crate version with the git tag during release.
    pub enabled: Option<bool>,
    /// Sync mode: "cargo" (updates Cargo.toml) or "tag" (derives version from tag).
    pub mode: Option<String>,
}
