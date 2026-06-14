use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::super::{StringOrBool, deserialize_string_or_bool_opt};
use super::{CommitAuthorConfig, RepositoryConfig};

// ---------------------------------------------------------------------------
// HomebrewConfig / ScoopConfig / TapConfig / BucketConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HomebrewConfig {
    /// Unified repository config with branch, token, PR, git SSH support.
    /// (Replaces the legacy `tap: TapConfig` owner/name-only form.)
    pub repository: Option<RepositoryConfig>,
    /// Commit author with optional signing.
    pub commit_author: Option<CommitAuthorConfig>,
    /// Formula directory in the tap (e.g. "Formula").
    pub directory: Option<String>,
    /// Override the formula name (default: crate name).
    pub name: Option<String>,
    /// Short description of the formula (shown in `brew info`).
    pub description: Option<String>,
    /// SPDX license identifier (e.g., "MIT", "Apache-2.0").
    pub license: Option<String>,
    /// Ruby `install` block content for the formula.
    pub install: Option<String>,
    /// Additional install commands appended after the main install block.
    pub extra_install: Option<String>,
    /// Post-install commands (separate `def post_install` block in formula).
    pub post_install: Option<String>,
    /// Ruby `test` block content for the formula (run by `brew test`).
    pub test: Option<String>,
    /// Project homepage URL. Falls back to the GitHub release URL when unset.
    pub homepage: Option<String>,
    /// Package dependencies (e.g. `openssl`, `libgit2`).
    pub dependencies: Option<Vec<HomebrewDependency>>,
    /// Conflicting formula names with optional reason.
    pub conflicts: Option<Vec<HomebrewConflict>>,
    /// Post-install user-facing notes shown by `brew info`.
    pub caveats: Option<String>,
    /// Skip publishing the formula.  `"true"` always skips; `"auto"` skips
    /// for prerelease versions. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip_upload: Option<StringOrBool>,
    /// Custom commit message template. Rendered via Tera with the standard
    /// release template variables (`ProjectName`, `Tag`, `Version`, etc.).
    /// Default: `"Brew formula update for {{ ProjectName }} version {{ Tag }}"`
    /// (set in `crates/stage-publish/src/homebrew.rs::default_commit_msg_template`).
    pub commit_msg_template: Option<String>,
    // Legacy flat `commit_author_name` / `commit_author_email` fields are
    // gone; use the structured `commit_author: { name, email, signing }`.
    /// Build IDs filter: only include artifacts whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Custom URL template for download URLs (overrides release URL).
    pub url_template: Option<String>,
    /// HTTP headers to include in download requests (e.g. for private repos).
    pub url_headers: Option<Vec<String>>,
    /// Custom download strategy class name (e.g. `:using => GitHubPrivateRepositoryReleaseDownloadStrategy`).
    pub download_strategy: Option<String>,
    /// Ruby `require` statement for custom download strategies.
    pub custom_require: Option<String>,
    /// Custom Ruby code block inserted into the formula class body.
    pub custom_block: Option<String>,
    /// Launchd plist content for `brew services`.
    pub plist: Option<String>,
    /// Homebrew service block content (alternative to plist).
    pub service: Option<String>,
    /// Manpage file paths to install into the formula's `man1` (e.g.
    /// `["mytool.1"]`). Each entry renders a `man1.install "<path>"` line in
    /// the install block, mirroring real Rust-CLI formulae (ripgrep, fd, bat).
    /// A path ending in `.N` (where N is 1–8) routes to the matching `manN`
    /// section; anything else defaults to `man1`.
    pub manpages: Option<Vec<String>>,
    /// Prebuilt shell-completion file paths to install. When set, the formula
    /// emits `bash_completion.install "<path>"` / `zsh_completion.install` /
    /// `fish_completion.install` in its install block — the form used when the
    /// archive ships ready-made completion files.
    pub completions: Option<HomebrewCaskCompletions>,
    /// Generate completions by running the installed binary at install time.
    /// Renders the modern homebrew-core idiom
    /// `generate_completions_from_executable(bin/"<exe>", ...)` in the install
    /// block. Preferred over `completions` when the binary can emit its
    /// own completions; the two are independent and may both be set.
    pub generate_completions_from_executable: Option<HomebrewCaskGeneratedCompletions>,
    /// `livecheck` stanza configuration for the formula. When unset, a binary
    /// tap formula emits `livecheck { skip "Auto-generated on release." }` to
    /// match the cask (the archive URL/sha are rewritten on every release, so
    /// `brew livecheck` cannot meaningfully poll). Set `strategy:` /
    /// `regex:`/`url:` to opt into active version detection instead.
    pub livecheck: Option<HomebrewLivecheck>,
    /// Homebrew Cask configuration (macOS .app bundles).
    pub cask: Option<HomebrewCaskConfig>,
    /// amd64 microarchitecture variant filter (e.g. "v1", "v2", "v3", "v4").
    /// Only artifacts matching this variant are included. Default: "v1".
    pub amd64_variant: Option<String>,
    /// ARM version filter (e.g. "6", "7"). Only artifacts matching this
    /// variant are included.
    pub arm_variant: Option<String>,
    /// Override whether this publisher failing should fail the overall release.
    ///
    /// Default: `false` — a failure here is logged but does not abort the release.
    /// Set to `true` to fail the release on any error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    /// Template-conditional gate: when the rendered result is falsy
    /// (`"false"` / `"0"` / `"no"` / empty), the Homebrew publisher is
    /// skipped. Render failure hard-errors. Config key: `brews[].if:`.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
    /// When `true`, a triggered rollback leaves this publisher's work in
    /// place rather than attempting to undo it. Default `false`.
    pub retain_on_rollback: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HomebrewDependency {
    /// Homebrew formula name of the dependency.
    pub name: String,
    /// Restrict to a specific OS: `"mac"` or `"linux"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// Dependency type, e.g. `"optional"`.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub dep_type: Option<String>,
    /// Version constraint for the dependency (e.g. `">= 1.1"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// A Homebrew conflict entry, supporting both a bare name string and a
/// structured object with an optional `because` reason.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
#[serde(untagged)]
pub enum HomebrewConflict {
    /// Just the formula name (e.g. `"other-tool"`).
    Name(String),
    /// Name with reason (e.g. `{name: "other-tool", because: "both install a bin/foo binary"}`).
    WithReason {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        because: Option<String>,
    },
}

impl HomebrewConflict {
    pub fn name(&self) -> &str {
        match self {
            Self::Name(n) => n,
            Self::WithReason { name, .. } => name,
        }
    }
    pub fn because(&self) -> Option<&str> {
        match self {
            Self::Name(_) => None,
            Self::WithReason { because, .. } => because.as_deref(),
        }
    }
}

/// `livecheck` stanza configuration for a Homebrew formula.
///
/// Default (the struct absent from config): the formula emits a
/// `livecheck { skip "Auto-generated on release." }` block — correct for a
/// binary tap whose archive URL/sha256 are rewritten every release. To opt
/// into active version polling, set `skip: false` and a `strategy:` (and
/// optionally `url:` / `regex:`):
///
/// ```yaml
/// livecheck:
///   strategy: github_latest
/// ```
///
/// renders:
///
/// ```ruby
/// livecheck do
///   url :stable
///   strategy :github_latest
/// end
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HomebrewLivecheck {
    /// Force the `skip "<reason>"` form even when a strategy is set, or set to
    /// `false` to opt out of the default skip and emit an active livecheck.
    /// When `None` (the default), the formula skips iff no `strategy`/`url`
    /// is configured.
    pub skip: Option<bool>,
    /// Reason text for the `skip "<reason>"` form.
    /// Default: `"Auto-generated on release."`.
    pub skip_reason: Option<String>,
    /// `livecheck` strategy symbol (e.g. `github_latest`, `git`, `page_match`).
    /// Rendered as a Ruby symbol: `strategy :github_latest`.
    pub strategy: Option<String>,
    /// `url` for the livecheck. Accepts a Ruby symbol shorthand
    /// (`stable` / `head` / `homepage` → `url :stable`) or a literal URL
    /// string (`url "https://..."`). Defaults to `:stable` when a strategy
    /// is set without an explicit url.
    pub url: Option<String>,
    /// `regex(...)` argument for `page_match`-style strategies. Emitted
    /// verbatim inside `regex(...)`, so it is raw Ruby (e.g. `%r{v(\d+\.\d+)}i`).
    pub regex: Option<String>,
}

/// Unified Homebrew Cask configuration.
///
/// Used at both call-sites:
/// - `homebrew_casks:` — top-level array; carries `repository`,
///   `commit_author`, `directory`, `ids`, `url`, structured `uninstall`/`zap`, etc.
/// - `crates[].publish.homebrew_cask:` — per-crate override; same shape, with
///   `url_template` as the simpler URL alternative.
///
/// Fields from both original types are present; any field may be `None` at either
/// call-site. The union avoids a two-type bifurcation while keeping both axes.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HomebrewCaskConfig {
    // ----- Identity -----
    /// Cask name (default: crate / project name).
    pub name: Option<String>,
    /// Alternative cask names (aliases).
    pub alternative_names: Option<Vec<String>>,

    // ----- Tap repository (top-level axis) -----
    /// Unified repository config for the Homebrew tap.
    pub repository: Option<RepositoryConfig>,
    /// Commit author with optional signing.
    pub commit_author: Option<CommitAuthorConfig>,
    /// Custom commit message template.
    /// Default: "Brew cask update for {{ ProjectName }} version {{ Tag }}"
    pub commit_msg_template: Option<String>,
    /// Subdirectory in the tap repo for cask placement (default: "Casks").
    pub directory: Option<String>,

    // ----- Artifact selection -----
    /// Build IDs filter: only include artifacts from builds whose `id` is in this list.
    pub ids: Option<Vec<String>>,

    // ----- Download URL -----
    /// Simple URL template for the .dmg/.zip download (per-crate shorthand).
    ///
    /// Cannot be combined with `url.template:` — set one or the other.
    /// If both are present, config validation rejects the config at parse time.
    /// Use `url:` for the structured form (verified domain, custom headers, etc.)
    /// or `url_template:` for a bare string shorthand — never both simultaneously.
    pub url_template: Option<String>,
    /// Structured download URL configuration (top-level axis).
    pub url: Option<HomebrewCaskURL>,

    // ----- macOS bundle -----
    /// macOS .app bundle name (e.g. "MyApp.app").
    pub app: Option<String>,
    /// Binary stubs to create in /usr/local/bin.
    ///
    /// Each entry is either a bare string (`"my-cli"` → emits
    /// `binary "my-cli"`) or a structured `{ name, target }` object
    /// (`{ name: "my-cli", target: "mycli" }` → emits
    /// `binary "my-cli", target: "mycli"`). The `target:` form mirrors
    /// the Homebrew Ruby cask DSL for binary renames — without it, a
    /// wrapped binary installs at the wrong path.
    /// Cask binary entry.
    pub binaries: Option<Vec<HomebrewCaskBinary>>,
    /// Deprecated singular spelling of [`Self::binaries`]. The upstream
    /// replaced `binary: foo` with `binaries: [foo]`; this field captures the
    /// legacy spelling so imported configs keep parsing.
    /// [`apply_homebrew_cask_legacy_singulars`](super::super::apply_homebrew_cask_legacy_singulars)
    /// folds the value into [`Self::binaries`] at config-load time and emits
    /// a one-time deprecation warning per occurrence. The field is excluded
    /// from serialization so a round-tripped config emits only the canonical
    /// plural form.
    #[serde(default, rename = "binary", skip_serializing)]
    pub legacy_binary: Option<String>,

    // ----- Metadata -----
    /// Cask description.
    pub description: Option<String>,
    /// Project homepage URL.
    pub homepage: Option<String>,
    /// License identifier (SPDX).
    pub license: Option<String>,
    /// Custom caveats shown after install.
    pub caveats: Option<String>,

    // ----- Ruby block -----
    /// Arbitrary Ruby code inserted into the cask block.
    pub custom_block: Option<String>,
    /// Homebrew service definition.
    pub service: Option<String>,

    // ----- Completions / manpages -----
    /// Manual page references to install.
    pub manpages: Option<Vec<String>>,
    /// Deprecated singular spelling of [`Self::manpages`]. The upstream replaced
    /// `manpage: foo.1` with `manpages: [foo.1]`; this field captures the
    /// legacy spelling so imported configs keep parsing.
    /// [`apply_homebrew_cask_legacy_singulars`](super::super::apply_homebrew_cask_legacy_singulars)
    /// folds the value into [`Self::manpages`] at config-load time and emits
    /// a one-time deprecation warning per occurrence. The field is excluded
    /// from serialization so a round-tripped config emits only the canonical
    /// plural form.
    #[serde(default, rename = "manpage", skip_serializing)]
    pub legacy_manpage: Option<String>,
    /// Shell completion definitions.
    pub completions: Option<HomebrewCaskCompletions>,
    /// Auto-generate shell completions from an executable.
    pub generate_completions_from_executable: Option<HomebrewCaskGeneratedCompletions>,

    // ----- Dependencies / conflicts -----
    /// Cask dependencies (other casks or formulae).
    pub dependencies: Option<Vec<HomebrewCaskDependencyEntry>>,
    /// Conflicting casks or formulae.
    pub conflicts: Option<Vec<HomebrewCaskConflictEntry>>,

    // ----- Lifecycle hooks -----
    /// Pre/post install/uninstall hooks.
    pub hooks: Option<HomebrewCaskHooks>,

    // ----- Uninstall / zap -----
    /// Structured uninstall stanza configuration.
    pub uninstall: Option<HomebrewCaskUninstall>,
    /// Deep uninstall (zap) stanza configuration.
    pub zap: Option<HomebrewCaskUninstall>,

    // ----- Publishing control -----
    /// Skip publishing the cask. `"true"` always skips; `"auto"` skips
    /// for prerelease versions. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip_upload: Option<StringOrBool>,
    /// When true, force-push the updated cask file to the existing PR branch
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
    /// (`"false"` / `"0"` / `"no"` / empty), the Homebrew Cask config is
    /// skipped. Render failure hard-errors. Config key: `homebrew_casks[].if:`.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
    /// When `true`, a triggered rollback leaves this publisher's work in
    /// place rather than attempting to undo it. Default `false`.
    pub retain_on_rollback: Option<bool>,
}

/// Structured URL configuration for Homebrew Cask downloads.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HomebrewCaskURL {
    /// URL template for the download.
    pub template: Option<String>,
    /// Verification string (domain shown to user).
    pub verified: Option<String>,
    /// Custom downloader (e.g. `:homebrew_curl`, `:post`).
    pub using: Option<String>,
    /// HTTP cookies for the download.
    pub cookies: Option<HashMap<String, String>>,
    /// Referer header for the download.
    pub referer: Option<String>,
    /// Custom HTTP headers.
    pub headers: Option<Vec<String>>,
    /// Custom user agent string.
    pub user_agent: Option<String>,
    /// POST data for form submissions.
    pub data: Option<HashMap<String, String>>,
}

/// Structured uninstall/zap configuration for Homebrew Cask.
/// Used for both `uninstall` and `zap` stanzas.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HomebrewCaskUninstall {
    /// Launch daemon/agent identifiers to stop.
    pub launchctl: Option<Vec<String>>,
    /// Application bundle IDs to quit.
    pub quit: Option<Vec<String>>,
    /// Login item names to remove.
    pub login_item: Option<Vec<String>>,
    /// File paths to delete.
    pub delete: Option<Vec<String>>,
    /// File paths to trash (preserves app state).
    pub trash: Option<Vec<String>>,
}

/// Pre/post install/uninstall hooks for Homebrew Cask.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HomebrewCaskHooks {
    /// Pre-install/uninstall hooks.
    pub pre: Option<HomebrewCaskHook>,
    /// Post-install/uninstall hooks.
    pub post: Option<HomebrewCaskHook>,
}

/// Individual hook for install/uninstall phases.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HomebrewCaskHook {
    /// Ruby code for preflight/postflight during install.
    pub install: Option<String>,
    /// Ruby code for uninstall_preflight/uninstall_postflight.
    pub uninstall: Option<String>,
}

/// Shell completion file paths for Homebrew Cask.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HomebrewCaskCompletions {
    /// Path to bash completion file.
    pub bash: Option<String>,
    /// Path to zsh completion file.
    pub zsh: Option<String>,
    /// Path to fish completion file.
    pub fish: Option<String>,
}

/// Cask `binary` stanza entry.
///
/// Two shapes accepted in YAML:
/// - bare string — `"my-cli"` → renders `binary "my-cli"`.
/// - `{ name, target }` object — `{ name: "my-cli", target: "mycli" }`
///   → renders `binary "my-cli", target: "mycli"`. The `target:` form is
///   the Homebrew Ruby cask DSL rename: install the symlink at
///   `/usr/local/bin/<target>` instead of `/usr/local/bin/<name>`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
#[serde(untagged)]
pub enum HomebrewCaskBinary {
    /// Bare binary name. Equivalent to `{ name: "<n>", target: None }`.
    Name(String),
    /// Structured `{ name, target }` rename form.
    WithTarget {
        /// Path inside the .app bundle (e.g. `"my-cli"`).
        name: String,
        /// Optional rename target — the symlink name in `/usr/local/bin`.
        /// When `None`, the symlink uses `name`.
        #[serde(skip_serializing_if = "Option::is_none")]
        target: Option<String>,
    },
}

impl HomebrewCaskBinary {
    /// The binary name (the path inside the .app bundle).
    pub fn name(&self) -> &str {
        match self {
            Self::Name(n) => n,
            Self::WithTarget { name, .. } => name,
        }
    }
    /// The optional rename target. `None` for bare-string entries and for
    /// `{ name, target }` objects without `target` set.
    pub fn target(&self) -> Option<&str> {
        match self {
            Self::Name(_) => None,
            Self::WithTarget { target, .. } => target.as_deref(),
        }
    }
}

/// Cask dependency (on another cask or formula).
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HomebrewCaskDependencyEntry {
    /// Dependent cask name.
    pub cask: Option<String>,
    /// Dependent formula name.
    pub formula: Option<String>,
}

/// Cask conflict (with another cask or formula).
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HomebrewCaskConflictEntry {
    /// Conflicting cask name.
    pub cask: Option<String>,
    /// Conflicting formula name (deprecated by Homebrew).
    pub formula: Option<String>,
}

/// Auto-generate shell completions from an executable.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HomebrewCaskGeneratedCompletions {
    /// Binary to generate completions from.
    pub executable: Option<String>,
    /// Arguments to pass to the executable.
    pub args: Option<Vec<String>>,
    /// Base name for completion files.
    pub base_name: Option<String>,
    /// Shell completion framework type (arg, clap, click, cobra, flag, none, typer).
    pub shell_parameter_format: Option<String>,
    /// Target shells (bash, zsh, fish, pwsh).
    pub shells: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ScoopConfig {
    /// Unified repository config with branch, token, PR, git SSH support.
    /// (Replaces the legacy `bucket: BucketConfig` owner/name-only form.)
    pub repository: Option<RepositoryConfig>,
    /// Commit author with optional signing.
    pub commit_author: Option<CommitAuthorConfig>,
    /// Override the manifest name (default: crate name).
    pub name: Option<String>,
    /// Subdirectory in the bucket repo for manifest placement.
    pub directory: Option<String>,
    /// Short description of the package (shown in `scoop info`).
    pub description: Option<String>,
    /// SPDX license identifier (e.g., "MIT", "Apache-2.0").
    pub license: Option<String>,
    /// Project homepage URL. Falls back to the GitHub-derived URL when unset.
    pub homepage: Option<String>,
    /// Data paths persisted between Scoop updates.
    pub persist: Option<Vec<String>>,
    /// Application dependencies (other Scoop packages).
    pub depends: Option<Vec<String>>,
    /// Commands to run before installation.
    pub pre_install: Option<Vec<String>>,
    /// Commands to run after installation.
    pub post_install: Option<Vec<String>>,
    /// Start menu shortcuts as `[executable, label]` pairs.
    pub shortcuts: Option<Vec<Vec<String>>>,
    /// Skip publishing the manifest.  `"true"` always skips; `"auto"` skips
    /// for prerelease versions. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip_upload: Option<StringOrBool>,
    /// Custom commit message template.
    pub commit_msg_template: Option<String>,
    // Use the structured `commit_author: { name, email, signing }` form for
    // commit author identity (legacy flat `commit_author_name` /
    // `commit_author_email` fields are not accepted).
    /// Build IDs filter: only include artifacts whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Custom URL template for download URLs (overrides release URL).
    pub url_template: Option<String>,
    /// Artifact selection: "archive" (default), "msi", or "nsis".
    #[serde(rename = "use")]
    pub use_artifact: Option<String>,
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
    /// (`"false"` / `"0"` / `"no"` / empty), the Scoop publisher is
    /// skipped. Render failure hard-errors. Config key: `scoop[].if:`.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
    /// When `true`, a triggered rollback leaves this publisher's work in
    /// place rather than attempting to undo it. Default `false`.
    pub retain_on_rollback: Option<bool>,
}

// `TapConfig` / `BucketConfig` (legacy {owner, name}-only repo types) live
// nowhere — every publisher now carries `repository: RepositoryConfig`
// with the broader feature set (token / branch / git SSH / pull_request).
