use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{CommitAuthorConfig, ContentSource};

// ---------------------------------------------------------------------------
// SnapshotConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SnapshotConfig {
    /// Version string template for snapshot builds (e.g., "{{ .Commit }}-SNAPSHOT").
    /// F3: accepts the deprecated `name_template:` GR alias (renamed to
    /// `version_template` upstream). GR ref:
    /// `internal/pipe/snapshot/snapshot.go:25-28` —
    /// `if NameTemplate != "" { VersionTemplate = NameTemplate }`.
    /// A deprecation warning is emitted at config-load time when the alias
    /// is hit (see `apply_snapshot_legacy_aliases`).
    #[serde(alias = "name_template")]
    pub version_template: String,
}

// ---------------------------------------------------------------------------
// NightlyConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NightlyConfig {
    /// Template for the rendered version string the nightly run sets on
    /// `Version` / `RawVersion`. GoReleaser default:
    /// `"{{ incpatch(v=Version) }}-{{ ShortCommit }}-nightly"` — produces
    /// commit-immutable nightly versions (two same-day commits yield two
    /// distinct nightly versions).
    pub version_template: Option<String>,
    /// Template for the release name. Default: `"{{ ProjectName }}-nightly"`.
    pub name_template: Option<String>,
    /// Tag name used for the nightly release. Default: `"nightly"`.
    /// Templates allowed (GoReleaser v2.16+).
    pub tag_name: Option<String>,
    /// Whether to publish a GitHub Release at all. Default: `true`.
    /// Set `false` for nightly-only docker pushes / blob uploads.
    pub publish_release: Option<bool>,
    /// Delete the prior release that points at the same tag before
    /// creating the new one. Default: `false`. Set `true` to maintain a
    /// single rolling nightly release on GitHub. Destructive: deletes a
    /// published release via the GitHub Releases API. GitHub-only (GoReleaser parity).
    pub keep_single_release: Option<bool>,
    /// Override `release.draft` for nightly runs only (GoReleaser v2.12+).
    /// `None` falls through to `release.draft`; `Some(v)` overrides it.
    pub draft: Option<bool>,
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
