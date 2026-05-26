use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::super::{PostPublishPollConfig, StringOrBool, deserialize_string_or_bool_opt};
use super::RepositoryConfig;

// ---------------------------------------------------------------------------
// ChocolateyConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ChocolateyConfig {
    /// Override the package name (default: crate name).
    pub name: Option<String>,
    /// Build IDs filter: only include artifacts whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Unified project repo config (owner/name). Used to derive
    /// `<projectUrl>` (the Chocolatey gallery link) and download URLs.
    /// `<projectUrl>` resolves through `project_url:` (if set) → derived
    /// `https://github.com/{repository.owner}/{repository.name}`.
    pub repository: Option<RepositoryConfig>,
    /// URL shown as the package source in the Chocolatey gallery.
    pub package_source_url: Option<String>,
    /// Package owners (Chocolatey gallery user).
    pub owners: Option<String>,
    /// Package title (default: project name).
    pub title: Option<String>,
    /// Package author(s) displayed in the Chocolatey gallery.
    pub authors: Option<String>,
    /// Project homepage URL.
    pub project_url: Option<String>,
    /// Custom URL template for download URLs (overrides release URL).
    pub url_template: Option<String>,
    /// URL to the package icon image shown in the Chocolatey gallery.
    pub icon_url: Option<String>,
    /// Copyright notice.
    pub copyright: Option<String>,
    /// Package description (supports markdown).
    pub description: Option<String>,
    /// SPDX license identifier (e.g., "MIT", "Apache-2.0").
    pub license: Option<String>,
    /// Optional explicit license URL. Falls back to
    /// `https://opensource.org/licenses/<license>` when not set.
    pub license_url: Option<String>,
    /// Require license acceptance before install.
    pub require_license_acceptance: Option<bool>,
    /// Source code project URL.
    pub project_source_url: Option<String>,
    /// Documentation URL.
    pub docs_url: Option<String>,
    /// Bug tracker URL.
    pub bug_tracker_url: Option<String>,
    /// Tags for the Chocolatey gallery (joined with single spaces in the
    /// emitted nuspec). Always a typed list — the legacy
    /// space-separated-string form was dropped now for
    /// IDE-completion friendliness and to remove whitespace ambiguity.
    pub tags: Option<Vec<String>>,
    /// Short summary of the package.
    pub summary: Option<String>,
    /// Release notes for this version.
    pub release_notes: Option<String>,
    /// Package dependencies with optional version constraints.
    pub dependencies: Option<Vec<ChocolateyDependency>>,
    /// Chocolatey API key for `choco push`. Falls back to `CHOCOLATEY_API_KEY` env var.
    pub api_key: Option<String>,
    /// Push source URL (default: "https://push.chocolatey.org/").
    pub source_repo: Option<String>,
    /// Skip pushing to the Chocolatey community repository. Bool, string, or
    /// template expression (e.g. `"{{ .IsSnapshot }}"`). Accepts the legacy
    /// `skip_publish:` spelling for back-compat with configs;
    /// canonical name is `skip:` to align with every other publisher.
    #[serde(
        default,
        alias = "skip_publish",
        deserialize_with = "deserialize_string_or_bool_opt"
    )]
    pub skip: Option<StringOrBool>,
    /// Artifact selection: "archive" (default), "msi", or "nsis".
    #[serde(rename = "use")]
    pub use_artifact: Option<String>,
    /// amd64 microarchitecture variant filter (e.g. "v1", "v2", "v3", "v4").
    /// Only artifacts matching this variant are included. Default: "v1".
    pub amd64_variant: Option<String>,
    /// Post-publish moderation-queue polling settings. When unset, polling
    /// runs with defaults (enabled, 30s interval, 30m timeout). Polling can
    /// be disabled globally via `--no-post-publish-poll`.
    pub post_publish_poll: Option<PostPublishPollConfig>,
    /// When true, re-push the nupkg even when a version is already in the
    /// community moderation queue (PackageStatus=Submitted). Chocolatey's API
    /// accepts re-pushes of in-moderation versions; the new nupkg replaces the
    /// queued one. When false (default), the push is skipped and a warning is
    /// emitted so the operator sees that the publisher did not push.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub republish_in_moderation: Option<StringOrBool>,
    /// Override whether this publisher failing should fail the overall release.
    ///
    /// Default: `false` — a failure here is logged but does not abort the release.
    /// Set to `true` to fail the release on any error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
}

/// Chocolatey package dependency with optional version constraint.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ChocolateyDependency {
    /// Chocolatey package ID of the dependency.
    pub id: String,
    /// Minimum version constraint for the dependency (e.g., "[1.0.0,)").
    pub version: Option<String>,
}
