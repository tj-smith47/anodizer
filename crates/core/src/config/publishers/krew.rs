use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::super::{StringOrBool, deserialize_string_or_bool_opt};
use super::{CommitAuthorConfig, RepositoryConfig};

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
    /// When true, force-push the updated plugin manifest to the existing PR
    /// branch when a PR for the same head branch already exists. The PR content
    /// is updated in place rather than creating a duplicate. When false
    /// (default), the push is skipped and a warning is emitted so the operator
    /// sees that the publisher did not update the PR.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub update_existing_pr: Option<StringOrBool>,
    /// Override whether this publisher failing should fail the overall release.
    ///
    /// Default: `false` — a failure here is logged but does not abort the release.
    /// Set to `true` to fail the release on any error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    /// Template-conditional gate: when the rendered result is falsy
    /// (`"false"` / `"0"` / `"no"` / empty), the Krew publisher is
    /// skipped. Render failure hard-errors. Config key: `krews[].if:`.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
    /// Which krew-index submission path to take.
    ///
    /// - `auto` (default): probe whether the plugin already exists in
    ///   `kubernetes-sigs/krew-index`. Already present → `bot` (the
    ///   hosted krew-release-bot opens the version-bump PR server-side);
    ///   definitively absent → `pr-direct` (anodizer opens the initial
    ///   fork PR). A probe that can't reach a definitive answer
    ///   (rate-limit, network error) hard-errors rather than guessing,
    ///   so a transient blip never routes an existing plugin into a
    ///   maintainer-hostile fork PR.
    /// - `bot`: always POST to the krew-release-bot webhook. Use when
    ///   the plugin is known to be in krew-index and you want to skip
    ///   the membership probe entirely.
    /// - `pr-direct`: always open a fork PR against krew-index. Use for
    ///   the initial submission, or a self-hosted krew-index mirror the
    ///   hosted bot can't reach.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<KrewMode>,
    /// When `true`, a triggered rollback leaves this publisher's work in
    /// place rather than attempting to undo it. Default `false`.
    pub retain_on_rollback: Option<bool>,
}

/// Which krew-index submission path the krew publisher takes.
///
/// Selects between the self-contained krew-release-bot webhook and the
/// fork-PR flow. Defaults to [`KrewMode::Auto`], which probes krew-index
/// membership to decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum KrewMode {
    /// Probe krew-index membership and pick `bot` (present) or
    /// `pr-direct` (absent). An indeterminate probe hard-errors.
    #[default]
    Auto,
    /// Always submit via the krew-release-bot webhook.
    Bot,
    /// Always open a fork PR against krew-index.
    PrDirect,
}
