use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::publishers::CommitAuthorConfig;
use super::{StringOrBool, deserialize_string_or_bool_opt};

// ---------------------------------------------------------------------------
// AurSourceConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AurSourceConfig {
    /// Override the package name (default: crate name, no -bin suffix).
    pub name: Option<String>,
    /// Build IDs filter.
    pub ids: Option<Vec<String>>,
    /// Commit author with optional signing.
    pub commit_author: Option<CommitAuthorConfig>,
    /// Custom commit message template.
    pub commit_msg_template: Option<String>,
    /// Short description of the package.
    pub description: Option<String>,
    /// Project homepage URL.
    pub homepage: Option<String>,
    /// SPDX license identifier.
    pub license: Option<String>,
    /// Skip publishing. `"true"` always skips; `"auto"` skips for prereleases.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip_upload: Option<StringOrBool>,
    /// Custom URL template for download URLs.
    pub url_template: Option<String>,
    /// PKGBUILD maintainer entries.
    pub maintainers: Option<Vec<String>>,
    /// Contributors listed in PKGBUILD comments.
    pub contributors: Option<Vec<String>>,
    /// Packages this PKGBUILD provides.
    pub provides: Option<Vec<String>>,
    /// Packages this PKGBUILD conflicts with.
    pub conflicts: Option<Vec<String>>,
    /// Runtime dependencies.
    pub depends: Option<Vec<String>>,
    /// Optional dependencies.
    pub optdepends: Option<Vec<String>>,
    /// Build-time dependencies (source packages need these).
    pub makedepends: Option<Vec<String>>,
    /// Backup files to preserve on upgrade.
    pub backup: Option<Vec<String>>,
    /// Package release number (default: "1").
    pub rel: Option<String>,
    /// Custom `prepare()` function body for PKGBUILD.
    pub prepare: Option<String>,
    /// Custom `build()` function body for PKGBUILD.
    pub build: Option<String>,
    /// Custom `package()` function body for PKGBUILD.
    pub package: Option<String>,
    /// AUR SSH git URL.
    pub git_url: Option<String>,
    /// Custom SSH command for git operations.
    pub git_ssh_command: Option<String>,
    /// Path to SSH private key file.
    pub private_key: Option<String>,
    /// Subdirectory in the git repo for committed files.
    pub directory: Option<String>,
    /// Skip this config.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Explicit architecture list (default: auto-detect from artifacts).
    pub arches: Option<Vec<String>>,
    /// `x86_64` micro-architecture variant — `v1` (baseline), `v2`, `v3`
    /// (AVX2), or `v4`. Equivalent to GR `AurSource.Goamd64`. Constrained
    /// to a typed enum because AUR source pkgs build from the upstream
    /// tarball (no binary artifacts to filter), so the value's only role
    /// is as the `Amd64` template var consumed by `prepare:` / `build:` /
    /// `package:` script bodies — typos must fail at parse time, not
    /// silently render an invalid string into the PKGBUILD.
    /// When unset, defaults to `v1` at template-render time.
    pub amd64_variant: Option<Amd64Variant>,
    /// Override whether this publisher failing should fail the overall release.
    /// When unset, falls through to the built-in default for this publisher.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
}

/// `x86_64` micro-architecture variant. Mirrors GoReleaser's `Goamd64` typed
/// values. Used by [`AurSourceConfig::amd64_variant`] to constrain the
/// `prepare:` / `build:` / `package:` template var surface to a known set —
/// AUR source pkgs build from the upstream tarball so the value is
/// template-only (no artifact filter) and a typo would render an invalid
/// PKGBUILD silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Amd64Variant {
    V1,
    V2,
    V3,
    V4,
}

impl Amd64Variant {
    /// Canonical lowercase string form (`"v1"`..`"v4"`). Matches the GR
    /// `Goamd64` external surface and the value rendered into the PKGBUILD
    /// `Amd64` template var.
    pub fn as_str(&self) -> &'static str {
        match self {
            Amd64Variant::V1 => "v1",
            Amd64Variant::V2 => "v2",
            Amd64Variant::V3 => "v3",
            Amd64Variant::V4 => "v4",
        }
    }
}
