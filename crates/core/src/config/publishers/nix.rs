use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::super::{StringOrBool, deserialize_string_or_bool_opt};
use super::{CommitAuthorConfig, RepositoryConfig};

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
    /// Value for `meta.mainProgram` in the generated Nix derivation.
    /// When set, the rendered derivation includes
    /// `mainProgram = "<value>";` inside the `meta` block, telling Nix
    /// which binary `nix run` should execute when the derivation
    /// contains multiple executables. Templated: supports
    /// `{{ Version }}` etc. Omitted when unset.
    pub main_program: Option<String>,
    /// Override whether this publisher failing should fail the overall release.
    ///
    /// Default: `false` — a failure here is logged but does not abort the release.
    /// Set to `true` to fail the release on any error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    /// Template-conditional gate: when the rendered result is falsy
    /// (`"false"` / `"0"` / `"no"` / empty), the Nix publisher is
    /// skipped. Render failure hard-errors. Config key: `nix[].if:`.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
}

/// Nix package dependency with optional OS restriction.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NixDependency {
    /// Nix attribute path for the dependency (e.g., "openssl", "pkgs.libgit2").
    pub name: String,
    /// OS restriction: "linux", "darwin", or empty for all.
    pub os: Option<String>,
}
