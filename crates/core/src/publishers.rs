//! Publisher config types lifted out of the monolithic `crate::config`
//! module per the WAVE 5 split (see `.claude/known-bugs.md`'s "WAVE 5
//! deferred" entry).
//!
//! Currently houses [`KrewConfig`], [`NixConfig`], and [`NixDependency`].
//! The remaining publisher configs (`HomebrewConfig`, `HomebrewCaskConfig`,
//! `ScoopConfig`, `ChocolateyConfig`, `WingetConfig`, `AurConfig`,
//! `AurSourceConfig`, `CargoPublishConfig`, `DockerV2Config`,
//! `DockerDigestConfig`, etc.) still live in `config.rs` pending future
//! extraction passes — each one's deserializer / schema helpers move
//! alongside its struct.
//!
//! Public API path is preserved by re-exports in `config.rs`.

use crate::config::{
    CommitAuthorConfig, RepositoryConfig, StringOrBool, deserialize_string_or_bool_opt,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// KrewConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct KrewConfig {
    /// Override the plugin name (default: crate name).
    pub name: Option<String>,
    /// Build IDs filter: only include artifacts whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Unified repository config with branch, token, PR, git SSH support.
    /// (Replaces the legacy `manifests_repo:` / `upstream_repo:` form.) The
    /// upstream PR target is derived from `repository.pull_request.base`
    /// when set, falling back to the canonical kubernetes-sigs/krew-index.
    pub repository: Option<RepositoryConfig>,
    /// Commit author with optional signing.
    pub commit_author: Option<CommitAuthorConfig>,
    /// Custom commit message template.
    pub commit_msg_template: Option<String>,
    /// Full description of the kubectl plugin.
    pub description: Option<String>,
    /// One-line summary of the kubectl plugin (max 255 chars).
    pub short_description: Option<String>,
    /// Project homepage URL for the plugin.
    pub homepage: Option<String>,
    /// Custom URL template for download URLs (overrides release URL).
    pub url_template: Option<String>,
    /// Post-install message shown to the user.
    pub caveats: Option<String>,
    /// Skip publishing. `"true"` always skips; `"auto"` skips for prereleases.
    /// Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip_upload: Option<StringOrBool>,
    /// Skip this Krew config. Accepts bool or template string
    /// (e.g. `"{{ if .IsSnapshot }}true{{ endif }}"` for conditional skip).
    /// Distinct from `skip_upload` so users can opt out of generating the
    /// manifest entirely (common when a project is not a kubectl plugin and
    /// has no krew channel).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// amd64 microarchitecture variant filter (e.g. "v1", "v2", "v3", "v4").
    /// Only artifacts matching this variant are included. Default: "v1".
    pub amd64_variant: Option<String>,
    /// ARM version filter (e.g. "6", "7"). Only artifacts matching this
    /// variant are included.
    pub arm_variant: Option<String>,
}

// ---------------------------------------------------------------------------
// NixConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NixConfig {
    /// Override the derivation name (default: crate name).
    pub name: Option<String>,
    /// Path for the .nix file in the repository (default: `pkgs/<name>/default.nix`).
    pub path: Option<String>,
    /// Unified repository config with branch, token, PR, git SSH support.
    pub repository: Option<RepositoryConfig>,
    /// Commit author with optional signing.
    pub commit_author: Option<CommitAuthorConfig>,
    /// Custom commit message template.
    pub commit_msg_template: Option<String>,
    /// Build IDs filter: only include artifacts whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Custom URL template for download URLs (overrides release URL).
    pub url_template: Option<String>,
    /// Skip publishing. `"true"` always skips; `"auto"` skips for prereleases.
    /// Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip_upload: Option<StringOrBool>,
    /// Skip this Nix config. Accepts bool or template string
    /// (e.g. `"{{ if .IsSnapshot }}true{{ endif }}"` for conditional skip).
    /// Distinct from `skip_upload` so users can model both intents — disable
    /// means "don't generate at all", skip_upload means "generate but don't
    /// push". Without this field, `nix: { skip: true }` was silently
    /// dropped by the serde unknown-field default.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Custom install commands (replaces auto-generated binary install).
    pub install: Option<String>,
    /// Additional install commands appended after the main install.
    pub extra_install: Option<String>,
    /// Post-install commands (postInstall phase).
    pub post_install: Option<String>,
    /// Short description of the Nix derivation.
    pub description: Option<String>,
    /// Project homepage URL.
    pub homepage: Option<String>,
    /// Nix license identifier (e.g. "mit", "asl20"). Validated against known licenses.
    pub license: Option<String>,
    /// Nix package dependencies with optional OS filtering.
    pub dependencies: Option<Vec<NixDependency>>,
    /// Nix formatter to run on the generated file: "alejandra" or "nixfmt".
    pub formatter: Option<String>,
    /// amd64 microarchitecture variant filter (e.g. "v1", "v2", "v3", "v4").
    /// Only artifacts matching this variant are included. Default: "v1".
    pub amd64_variant: Option<String>,
}

/// Nix package dependency with optional OS restriction.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NixDependency {
    /// Nix attribute path for the dependency (e.g., "openssl", "pkgs.libgit2").
    pub name: String,
    /// OS restriction: "linux", "darwin", or empty for all.
    pub os: Option<String>,
}
