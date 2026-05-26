use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::aur_source::AurSourceConfig;
use super::{StringOrBool, deserialize_string_or_bool_opt};

mod homebrew;
pub use homebrew::*;

mod chocolatey;
pub use chocolatey::*;

mod winget;
pub use winget::*;

mod aur;
pub use aur::*;

mod krew;
pub use krew::*;

mod nix;
pub use nix::*;

// ---------------------------------------------------------------------------
// Shared publisher config types: RepositoryConfig, CommitAuthorConfig
// ---------------------------------------------------------------------------

/// Shared repository configuration used by all git-based publishers
/// (Homebrew, Scoop, Winget, Krew, Nix). Equivalent to GoReleaser's `RepoRef`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct RepositoryConfig {
    /// Repository owner (GitHub user or organization).
    pub owner: Option<String>,
    /// Repository name.
    pub name: Option<String>,
    /// Auth token for the repository. Falls back to env-based resolution.
    pub token: Option<String>,
    /// Token type: "github" (default), "gitlab", "gitea".
    pub token_type: Option<String>,
    /// Branch to push to (default: repo default branch).
    pub branch: Option<String>,
    /// Git-specific settings for SSH-based publishing.
    pub git: Option<GitRepoConfig>,
    /// Pull request settings for fork-based workflows.
    pub pull_request: Option<PullRequestConfig>,
}

/// Git-specific repository settings for SSH-based publishing.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct GitRepoConfig {
    /// Git URL (e.g. `ssh://git@github.com/owner/repo.git`).
    pub url: Option<String>,
    /// Custom SSH command (e.g. `ssh -i /path/to/key`).
    pub ssh_command: Option<String>,
    /// Path to SSH private key file.
    pub private_key: Option<String>,
}

/// Pull request configuration for fork-based publisher workflows.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct PullRequestConfig {
    /// Enable PR creation instead of direct push.
    pub enabled: Option<bool>,
    /// Create PR as draft.
    pub draft: Option<bool>,
    /// Body text for the pull request.
    pub body: Option<String>,
    /// Target base repository/branch for the PR.
    pub base: Option<PullRequestBaseConfig>,
}

/// Target base for pull requests (upstream repo to PR against).
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct PullRequestBaseConfig {
    /// Owner of the upstream repository to PR against.
    pub owner: Option<String>,
    /// Name of the upstream repository to PR against.
    pub name: Option<String>,
    /// Base branch of the upstream repository to target with the PR.
    pub branch: Option<String>,
}

/// Shared commit author configuration with optional GPG/SSH signing.
/// Equivalent to GoReleaser's `CommitAuthor`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct CommitAuthorConfig {
    /// Git commit author display name.
    pub name: Option<String>,
    /// Git commit author email address.
    pub email: Option<String>,
    /// Commit signing configuration.
    pub signing: Option<CommitSigningConfig>,
    /// When true, omit the explicit `-c user.name=` / `-c user.email=`
    /// overrides at commit time and let the running git client use the
    /// invoking GitHub App's identity (i.e. the `<app-slug>[bot]@users.noreply.github.com`
    /// account that the GitHub Actions checkout step has already configured
    /// in the repo's local git config).
    ///
    /// Mirrors GoReleaser's `CommitAuthor.UseGithubAppToken`
    /// (`internal/git/config/github.go:381`); the canonical use-case is
    /// PRs against `homebrew/homebrew-core` / `kubernetes-sigs/krew-index`
    /// / `microsoft/winget-pkgs` opened from a GitHub App workflow, where
    /// EasyCLA / DCO / signed-commit policies require the App's identity
    /// (rather than a per-user bot identity) to land the merge.
    #[serde(default)]
    pub use_github_app_token: bool,
}

impl CommitAuthorConfig {
    /// Fill in the anodizer default name/email when either field is empty.
    /// Matches GoReleaser's `commitauthor.Default(brew.CommitAuthor)` which
    /// runs during the Default pass — so validation messages that reference
    /// commit-author identity see non-empty strings rather than blanks.
    pub fn normalize_defaults(&mut self) {
        if self.name.as_deref().is_none_or(str::is_empty) {
            self.name = Some("anodizer".to_string());
        }
        if self.email.as_deref().is_none_or(str::is_empty) {
            self.email = Some("bot@anodizer.dev".to_string());
        }
    }
}

/// Commit signing configuration (GPG, x509, or SSH).
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct CommitSigningConfig {
    /// Enable commit signing.
    pub enabled: Option<bool>,
    /// Signing key identifier.
    pub key: Option<String>,
    /// Signing program (e.g. `gpg`, `gpg2`).
    pub program: Option<String>,
    /// Signing format: "openpgp" (default), "x509", or "ssh".
    pub format: Option<String>,
}

// ---------------------------------------------------------------------------
// PublishConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PublishConfig {
    /// Publish to crates.io. Presence opts in; use `cargo: { skip: true }` to opt out.
    pub cargo: Option<CargoPublishConfig>,
    /// Homebrew formula publishing configuration.
    pub homebrew: Option<HomebrewConfig>,
    /// Homebrew Cask publishing configuration (macOS .app bundles).
    ///
    /// Uses the unified `HomebrewCaskConfig` which carries all fields from both
    /// the per-crate cask config and the top-level `homebrew_casks:` config.
    pub homebrew_cask: Option<HomebrewCaskConfig>,
    /// Scoop manifest publishing configuration.
    pub scoop: Option<ScoopConfig>,
    /// Chocolatey package publishing configuration.
    pub chocolatey: Option<ChocolateyConfig>,
    /// WinGet manifest publishing configuration.
    pub winget: Option<WingetConfig>,
    /// AUR (Arch User Repository) binary package publishing configuration.
    pub aur: Option<AurConfig>,
    /// AUR source package publishing configuration (source-only PKGBUILD, not -bin).
    pub aur_source: Option<AurSourceConfig>,
    /// Krew (kubectl plugin manager) manifest publishing configuration.
    pub krew: Option<KrewConfig>,
    /// Nix derivation publishing configuration.
    pub nix: Option<NixConfig>,
}

/// `cargo publish` flag surface.
///
/// Presence under `publish:` opts the crate in; use `skip: true` (or a
/// truthy template) to opt out. There is no `enabled` field — presence is
/// the on-switch.
///
/// Fields intentionally omitted because anodizer owns them:
/// - `--package` / `--workspace` / `--exclude`: the top-level `crates[]`
///   axis owns crate selection.
/// - `--dry-run`: pipeline-level CLI ergonomics (`anodizer release --dry-run`).
/// - `-v` / `-q` / `--color`: CLI ergonomics, not config.
/// - `--config` / `-Z`: cargo CLI escape hatches; out of scope.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CargoPublishConfig {
    // ----- Registry selection -----
    /// Alternate registry name from `~/.cargo/config.toml` (`--registry`).
    pub registry: Option<String>,
    /// Registry index URL (`--index`).
    pub index: Option<String>,
    /// Seconds to wait for the crates.io sparse index to publish a crate
    /// before its dependents are pushed (anodizer-original — no `cargo
    /// publish` equivalent).
    pub index_timeout: Option<u64>,

    // ----- Verify / dirty -----
    /// Skip the local `cargo build --release` verification step (`--no-verify`).
    pub no_verify: Option<bool>,
    /// Allow publishing with an uncommitted working tree (`--allow-dirty`).
    pub allow_dirty: Option<bool>,

    // ----- Feature selection -----
    /// Crate features to activate (`--features`).
    pub features: Option<Vec<String>>,
    /// Activate every feature, including `default` (`--all-features`).
    pub all_features: Option<bool>,
    /// Disable the `default` feature set (`--no-default-features`).
    pub no_default_features: Option<bool>,

    // ----- Compilation -----
    /// Build target triple for the verification step (`--target`).
    pub target: Option<String>,
    /// Override the cargo target directory (`--target-dir`).
    pub target_dir: Option<PathBuf>,
    /// Number of parallel compile jobs for verification (`--jobs`).
    pub jobs: Option<u32>,
    /// Continue on errors when verifying multiple crates (`--keep-going`).
    pub keep_going: Option<bool>,

    // ----- Manifest -----
    /// Path to the crate's `Cargo.toml` (`--manifest-path`).
    pub manifest_path: Option<PathBuf>,
    /// Require an up-to-date `Cargo.lock` matching the resolver (`--locked`).
    pub locked: Option<bool>,
    /// Require offline resolution; never hit the network (`--offline`).
    pub offline: Option<bool>,
    /// Both `--locked` and `--offline` (`--frozen`).
    pub frozen: Option<bool>,

    // ----- Peer-publisher pattern -----
    /// Skip this publisher; supports template strings or bool.
    /// Truthy renders disable the publisher without removing the block.
    #[serde(default, deserialize_with = "deserialize_string_or_bool_opt")]
    pub skip: Option<StringOrBool>,
    /// Override whether this publisher failing should fail the overall release.
    ///
    /// Default: `true` — a failure here aborts the release.
    /// Set to `false` to log failures but continue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    /// Template-conditional gate: when the rendered result is falsy
    /// (`"false"` / `"0"` / `"no"` / empty), the cargo publisher is
    /// skipped. Render failure hard-errors. Mirrors GoReleaser Pro
    /// publisher `if:` semantics.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
}
