use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::super::{StringOrBool, deserialize_string_or_bool_opt};
use super::CommitAuthorConfig;

// ---------------------------------------------------------------------------
// AurConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AurConfig {
    /// Override the package name (default: crate name + "-bin").
    pub name: Option<String>,
    /// Build IDs filter: only include artifacts whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Commit author with optional signing.
    pub commit_author: Option<CommitAuthorConfig>,
    /// Custom commit message template. Default: "Update to {{ version }}".
    pub commit_msg_template: Option<String>,
    /// Short description of the package for PKGBUILD.
    pub description: Option<String>,
    /// Project homepage URL.
    pub homepage: Option<String>,
    /// SPDX license identifier (e.g., "MIT", "Apache-2.0").
    pub license: Option<String>,
    /// Skip publishing. `"true"` always skips; `"auto"` skips for prereleases.
    /// Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip_upload: Option<StringOrBool>,
    /// Custom URL template for download URLs (overrides release URL).
    pub url_template: Option<String>,
    /// PKGBUILD maintainer entries (e.g., "Name <email@example.com>").
    pub maintainers: Option<Vec<String>>,
    /// Contributors listed in PKGBUILD comments.
    pub contributors: Option<Vec<String>>,
    /// Packages this PKGBUILD provides (virtual package names).
    pub provides: Option<Vec<String>>,
    /// Packages this PKGBUILD conflicts with.
    pub conflicts: Option<Vec<String>>,
    /// Runtime dependencies required by this package.
    pub depends: Option<Vec<String>>,
    /// Optional dependencies with descriptions (e.g., "fzf: fuzzy finder support").
    pub optdepends: Option<Vec<String>>,
    /// List of config files to preserve on upgrade (relative to `/`).
    pub backup: Option<Vec<String>>,
    /// Package release number (default: "1").
    pub rel: Option<String>,
    /// Custom PKGBUILD `package()` function body.
    pub package: Option<String>,
    /// AUR SSH git URL (e.g., `ssh://aur@aur.archlinux.org/<package>.git`).
    pub git_url: Option<String>,
    /// Custom SSH command for git operations.
    pub git_ssh_command: Option<String>,
    /// Path to SSH private key file.
    pub private_key: Option<String>,
    /// Subdirectory in the git repo for committed files.
    pub directory: Option<String>,
    /// Skip this AUR config. Accepts bool or template string
    /// (e.g. `"{{ if .IsSnapshot }}true{{ endif }}"` for conditional skip).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Content for a .install file (post-install/pre-remove scripts).
    pub install: Option<String>,
    // The PKGBUILD `url=` line resolves through `homepage:` →
    // crate metadata `homepage` → derived
    // `https://github.com/{release.github.owner}/{release.github.name}`.
    /// Packages this PKGBUILD replaces (for upgrade paths from old package names).
    pub replaces: Option<Vec<String>>,
    /// amd64 microarchitecture variant filter (e.g. "v1", "v2", "v3", "v4").
    /// Only artifacts matching this variant are included. Default: "v1".
    pub amd64_variant: Option<String>,
    /// Override whether this publisher failing should fail the overall release.
    ///
    /// Default: `false` — a failure here is logged but does not abort the release.
    /// Set to `true` to fail the release on any error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    /// Template-conditional gate: when the rendered result is falsy
    /// (`"false"` / `"0"` / `"no"` / empty), the AUR publisher is skipped.
    /// Render failure hard-errors. Mirrors GoReleaser Pro `aurs[].if:`.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
}
