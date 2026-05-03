use super::*;

// ---------------------------------------------------------------------------
// SnapshotConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SnapshotConfig {
    /// Version string template for snapshot builds (e.g., "{{ .Commit }}-SNAPSHOT").
    pub version_template: String,
}

// ---------------------------------------------------------------------------
// NightlyConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NightlyConfig {
    /// Template for the release name. Default: "{{ .ProjectName }}-nightly"
    pub name_template: Option<String>,
    /// Tag name used for the nightly release. Default: "nightly".
    pub tag_name: Option<String>,
}

// ---------------------------------------------------------------------------
// MetadataConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct MetadataConfig {
    /// Human-readable project description (exposed as `{{ .Metadata.Description }}`).
    pub description: Option<String>,
    /// Project homepage URL (exposed as `{{ .Metadata.Homepage }}`).
    pub homepage: Option<String>,
    /// Project license identifier, e.g. "MIT" or "Apache-2.0" (exposed as `{{ .Metadata.License }}`).
    pub license: Option<String>,
    /// List of project maintainers (exposed as `{{ .Metadata.Maintainers }}`).
    pub maintainers: Option<Vec<String>>,
    /// Global modification timestamp for metadata output files (metadata.json and artifacts.json).
    /// Template string (e.g. "{{ .CommitTimestamp }}") or unix timestamp.
    /// When set, rendered late in the pipeline and applied as file mtime.
    /// Exposed as `{{ .Metadata.ModTimestamp }}`.
    pub mod_timestamp: Option<String>,
    /// Long-form project description (GoReleaser Pro v2.1+). Supports inline
    /// string, `from_file`, or `from_url`. Exposed as `{{ .Metadata.FullDescription }}`.
    /// FromUrl is resolved lazily (requires the release stage); FromFile is resolved
    /// at context-populate time with template-rendered path.
    pub full_description: Option<ContentSource>,
    /// Commit author identity for Pro commit workflows (GoReleaser Pro v2.12+).
    /// Reuses the shared `CommitAuthorConfig` (name + email + optional signing).
    /// Exposed as `{{ .Metadata.CommitAuthor.Name }}` / `{{ .Metadata.CommitAuthor.Email }}`.
    pub commit_author: Option<CommitAuthorConfig>,
}
