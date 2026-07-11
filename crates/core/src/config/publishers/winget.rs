use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::super::{
    Amd64Variant, PostPublishPollConfig, StringOrBool, deserialize_string_or_bool_opt,
};
use super::{CommitAuthorConfig, RepositoryConfig};

// ---------------------------------------------------------------------------
// WingetConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct WingetConfig {
    /// Override the package name (default: crate name).
    pub name: Option<String>,
    /// Package name as displayed (default: same as name).
    pub package_name: Option<String>,
    /// WinGet package identifier (e.g. "Publisher.AppName"). Auto-generated if empty.
    pub package_identifier: Option<String>,
    /// Publisher name (required).
    pub publisher: Option<String>,
    /// Publisher homepage URL shown in the WinGet manifest.
    pub publisher_url: Option<String>,
    /// Publisher support URL.
    pub publisher_support_url: Option<String>,
    /// Privacy policy URL.
    pub privacy_url: Option<String>,
    /// Author name.
    pub author: Option<String>,
    /// Copyright notice.
    pub copyright: Option<String>,
    /// Copyright URL.
    pub copyright_url: Option<String>,
    /// License identifier (required, e.g. "MIT").
    pub license: Option<String>,
    /// License URL.
    pub license_url: Option<String>,
    /// Short description (required, max 256 chars).
    pub short_description: Option<String>,
    /// Locale stamped into the WinGet manifests: the version manifest's
    /// `DefaultLocale`, the installer manifest's `InstallerLocale`, the
    /// locale manifest's `PackageLocale`, and the locale manifest's
    /// `.locale.<locale>.yaml` file name. Supports templates.
    /// Default: `en-US`.
    ///
    /// ```yaml
    /// winget:
    ///   default_locale: "pt-BR"
    /// ```
    pub default_locale: Option<String>,
    /// Full package description displayed in the WinGet gallery.
    pub description: Option<String>,
    /// Project homepage URL.
    pub homepage: Option<String>,
    /// Custom URL template for download URLs (overrides release URL).
    pub url_template: Option<String>,
    /// Build IDs filter: only include artifacts whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Skip publishing. `"true"` always skips; `"auto"` skips for prereleases.
    /// Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip_upload: Option<StringOrBool>,
    /// Custom commit message template.
    pub commit_msg_template: Option<String>,
    /// Manifest file path (auto-generated if empty from publisher/name/version).
    pub path: Option<String>,
    /// Release notes for this version.
    pub release_notes: Option<String>,
    /// URL to full release notes.
    pub release_notes_url: Option<String>,
    /// Post-install notes shown to the user.
    pub installation_notes: Option<String>,
    /// Tags for package discovery (lowercased, spaces→hyphens).
    pub tags: Option<Vec<String>>,
    /// Package dependencies.
    pub dependencies: Option<Vec<WingetDependency>>,
    /// Unified repository config with branch, token, PR, git SSH support.
    /// (Replaces the legacy `manifests_repo: WingetManifestsRepoConfig`.)
    pub repository: Option<RepositoryConfig>,
    /// Commit author with optional signing.
    pub commit_author: Option<CommitAuthorConfig>,
    /// Product code for the installer (used in Add/Remove Programs).
    pub product_code: Option<String>,
    /// Short invoke alias shown as the package `Moniker` (e.g. `rg` for
    /// ripgrep, `fd` for fd). This is the command users type, NOT the
    /// package/crate name. When unset, anodizer derives it from the single
    /// published binary name; with multiple binaries and no override the
    /// Moniker is omitted (winget treats it as optional).
    ///
    /// Example: `moniker: "rg"`.
    pub moniker: Option<String>,
    /// Documentation links rendered as the `Documentations[]` block on the
    /// locale manifest. Each entry is a `{ label, url }` pair surfaced in the
    /// winget gallery (real ripgrep emits a `FAQ` and a `User Guide` entry).
    /// Omitted entirely when empty.
    ///
    /// Example:
    /// ```yaml
    /// documentations:
    ///   - label: "User Guide"
    ///     url: "https://github.com/owner/repo/blob/master/GUIDE.md"
    /// ```
    pub documentations: Option<Vec<WingetDocumentation>>,
    /// Installer `UpgradeBehavior` for every installer entry. winget accepts
    /// `install`, `uninstallPrevious`, and `deny`. Defaults to `install` —
    /// the correct behavior for portable-zip CLI tools (`uninstallPrevious`
    /// forces a clobbering reinstall).
    ///
    /// Example: `upgrade_behavior: "uninstallPrevious"`.
    pub upgrade_behavior: Option<String>,
    /// Silent-install switch string emitted as `InstallerSwitches.Silent` for
    /// actual installers (`wix`/`msi`/`exe`/`nsis`). When unset, anodizer
    /// derives the switch from the installer type (`/quiet` for msi, `/S` for
    /// exe/nsis). Never emitted for `zip`/`portable` artifacts.
    ///
    /// Example: `silent_switch: "/qn"`.
    pub silent_switch: Option<String>,
    /// Artifact selection: "archive" (default), "msi", or "nsis".
    #[serde(rename = "use")]
    pub use_artifact: Option<String>,
    /// amd64 microarchitecture variant filter (`v1` / `v2` / `v3` / `v4`).
    /// Only artifacts matching this variant are included. Default: `v1`.
    /// Typed as [`Amd64Variant`], so any value outside `v1`..`v4` is
    /// rejected when the config is parsed.
    pub amd64_variant: Option<Amd64Variant>,
    /// Post-publish PR-validation polling settings. Polling is
    /// disabled by default — winget-pkgs PR validation routinely
    /// takes hours to days, and blocking a CI workflow on that wait
    /// is wrong. Opt in per-publisher with
    /// `post_publish_poll: { enabled: true }` when running locally and
    /// willing to wait, or disable globally via `--no-post-publish-poll`.
    pub post_publish_poll: Option<PostPublishPollConfig>,
    /// When true, force-push the updated manifest to the existing PR branch
    /// when a PR for the same head branch already exists. The PR content is
    /// updated in place rather than creating a duplicate. When false (default),
    /// the push is skipped and a warning is emitted so the operator sees that
    /// the publisher did not update the PR.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub update_existing_pr: Option<StringOrBool>,
    /// Override whether this publisher failing should fail the overall release.
    ///
    /// Default: `false` — a failure here is logged but does not abort the release.
    /// Set to `true` to fail the release on any error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    /// Template-conditional gate: when the rendered result is falsy
    /// (`"false"` / `"0"` / `"no"` / empty), the WinGet publisher is
    /// skipped. Render failure hard-errors. Config key: `winget[].if:`.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
    /// When `true`, a triggered rollback leaves this publisher's work in
    /// place rather than attempting to undo it. Default `false`.
    pub retain_on_rollback: Option<bool>,
}

/// The WinGet architecture vocabulary an installer manifest may carry, and the
/// only values a `WingetDependency.architectures` scope may name. Mirrors the
/// output domain of the publisher's raw-arch → WinGet-arch mapping (`amd64`→
/// `x64`, `386`/`i686`→`x86`, `arm64`→`arm64`). A scope value outside this set
/// matches no installer, so the dependency would silently vanish from the
/// manifest — config validation rejects it up front.
pub const WINGET_ARCHITECTURES: [&str; 3] = ["x64", "arm64", "x86"];

/// A single documentation link rendered into the winget locale manifest's
/// `Documentations[]` block as `{ DocumentLabel, DocumentUrl }`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct WingetDocumentation {
    /// Display label for the link (e.g. `FAQ`, `User Guide`).
    pub label: String,
    /// Target URL for the documentation entry.
    pub url: String,
}

/// WinGet package dependency.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct WingetDependency {
    /// WinGet package identifier of the dependency (e.g., "Publisher.App").
    pub package_identifier: String,
    /// Minimum required version of the dependency.
    pub minimum_version: Option<String>,
    /// Architecture scope: attach this dependency only to installers whose
    /// architecture matches one of these WinGet architecture names (`x64`,
    /// `arm64`, `x86`). When unset or empty the dependency applies to every
    /// installer (the default — preserves the manifest-wide behavior).
    ///
    /// Use this when a runtime dependency is architecture-specific: e.g. an
    /// `x64` build needs the x64 VC++ runtime while the native `arm64` build
    /// needs the arm64 one, so each must be scoped to its own installer rather
    /// than attached to all of them.
    ///
    /// Example:
    /// ```yaml
    /// dependencies:
    ///   - package_identifier: "Microsoft.VCRedist.2015+.x64"
    ///     architectures: ["x64"]
    ///   - package_identifier: "Microsoft.VCRedist.2015+.arm64"
    ///     architectures: ["arm64"]
    ///   # unscoped — applies to every installer:
    ///   - package_identifier: "Acme.CommonRuntime"
    /// ```
    pub architectures: Option<Vec<String>>,
}
