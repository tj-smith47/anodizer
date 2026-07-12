use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::aur_source::AurSourceConfig;
use super::hooks::HookEntry;
use super::string_or_bool::HumanDuration;
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

mod schemastore;
pub use schemastore::{SchemaEntry, SchemaMode, SchemastoreConfig};

// ---------------------------------------------------------------------------
// Shared publisher config types: RepositoryConfig, CommitAuthorConfig
// ---------------------------------------------------------------------------

/// Shared repository configuration used by all git-based publishers
/// (Homebrew, Scoop, Winget, Krew, Nix). A repository reference.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
pub struct PullRequestBaseConfig {
    /// Owner of the upstream repository to PR against.
    pub owner: Option<String>,
    /// Name of the upstream repository to PR against.
    pub name: Option<String>,
    /// Base branch of the upstream repository to target with the PR.
    pub branch: Option<String>,
}

/// Shared commit author configuration with optional GPG/SSH signing.
/// Commit-author identity for publisher commits.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
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
    /// The use-github-app-token toggle
    /// uses the local git identity; the canonical use-case is
    /// PRs against `homebrew/homebrew-core` / `kubernetes-sigs/krew-index`
    /// / `microsoft/winget-pkgs` opened from a GitHub App workflow, where
    /// EasyCLA / DCO / signed-commit policies require the App's identity
    /// (rather than a per-user bot identity) to land the merge.
    #[serde(default)]
    pub use_github_app_token: bool,
}

impl CommitAuthorConfig {
    /// Fill in the anodizer default name/email when either field is empty.
    /// The commit-author defaulting, which
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
#[serde(default, deny_unknown_fields)]
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

    /// Hooks that fire once per FAILED publisher, after rollback has been
    /// attempted. Each entry is a standard hook (`cmd` / `dir` / `env` /
    /// `output`); the template surface adds `{{ .Publisher }}`,
    /// `{{ .Error }}`, `{{ .Version }}`, `{{ .Tag }}`, `{{ .Group }}`
    /// (Assets/Manager/Submitter), `{{ .Required }}`,
    /// `{{ .RolledBack }}` — true if any publisher was rolled back (or
    /// rollback was attempted and failed) during this run — and
    /// `{{ .RunReport }}`, the path of this run's already-written
    /// `dist/run-<id>/report.json` (per-publisher outcomes including
    /// rollback results; empty in snapshot/dry-run or when the report
    /// could not be persisted). The same values
    /// are also exported to the hook process as environment variables:
    /// `ANODIZER_PUBLISHER`, `ANODIZER_ERROR`, `ANODIZER_VERSION`,
    /// `ANODIZER_TAG`, `ANODIZER_GROUP`, `ANODIZER_REQUIRED`,
    /// `ANODIZER_ROLLED_BACK`, `ANODIZER_RUN_REPORT`. A hook's own
    /// failure is logged as a warning and never changes the release outcome.
    ///
    /// Security: the rendered `cmd` string is parsed by `sh -c`, and
    /// `{{ .Error }}` carries untrusted remote text (HTTP error bodies, git
    /// stderr) — interpolating it into `cmd` lets crafted error content
    /// break quoting and execute. Read untrusted values from the env vars
    /// instead (`$ANODIZER_ERROR`), and pass `anodizer notify --raw` so the
    /// text is sent literally rather than Tera-rendered. The outbound
    /// notification body is secret-redacted by default, so a secret reference
    /// smuggled into the error body is masked (sent as `$NAME`) even without
    /// `--raw`; `--raw` stays recommended because it avoids re-rendering
    /// already-final text and keeps untrusted content out of the shell-parsed
    /// `cmd` string:
    ///
    /// ```yaml
    /// publish:
    ///   on_error:
    ///     - cmd: 'anodizer notify --raw "anodizer: $ANODIZER_PUBLISHER failed @ $ANODIZER_VERSION: $ANODIZER_ERROR"'
    /// ```
    pub on_error: Option<Vec<HookEntry>>,

    /// Hooks that fire once per publisher that a triggered rollback REVERTED —
    /// including a publisher that itself `Succeeded` and was only reverted
    /// because a sibling required publisher failed (the case `on_error`, which
    /// fires solely for the failed publisher, never reaches). A publisher whose
    /// rollback was attempted but could not complete fires this too, with
    /// `{{ .RollbackFailed }}` set to `true` so the hook can escalate the
    /// orphaned-artifact case. Each entry is a standard hook (`cmd` / `dir` /
    /// `env` / `output`); the template surface adds `{{ .Publisher }}`,
    /// `{{ .Version }}`, `{{ .Tag }}`, `{{ .Group }}`
    /// (Assets/Manager/Submitter), `{{ .Required }}`, `{{ .RollbackFailed }}`
    /// (`true` when the revert itself failed), `{{ .Error }}` (the rollback
    /// failure message, empty on a clean revert), and `{{ .Reason }}` (the
    /// run-wide sibling failure(s) that triggered the unwind — distinct from
    /// `{{ .Error }}`; empty on a `--rollback-only` replay). The same values are
    /// exported to the hook process as `ANODIZER_PUBLISHER`, `ANODIZER_VERSION`,
    /// `ANODIZER_TAG`, `ANODIZER_GROUP`, `ANODIZER_REQUIRED`,
    /// `ANODIZER_ROLLBACK_FAILED`, `ANODIZER_ERROR`, and
    /// `ANODIZER_ROLLBACK_REASON`. A hook's own failure is
    /// logged as a warning and never changes the release outcome or aborts the
    /// remaining rollbacks. It is independent of `on_error`: a publisher that
    /// both failed and was rolled back fires both.
    ///
    /// ```yaml
    /// publish:
    ///   on_rollback:
    ///     - cmd: 'anodizer notify --raw "anodizer: $ANODIZER_PUBLISHER reverted @ $ANODIZER_VERSION (rollback_failed=$ANODIZER_ROLLBACK_FAILED)"'
    /// ```
    pub on_rollback: Option<Vec<HookEntry>>,
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
    /// Pre-publish gate that polls crates.io for every workspace-internal
    /// dep of the crate being published, blocking until each is queryable
    /// at its expected version. Required for multi-tag-multi-crate
    /// workspaces (e.g. cfgd) where per-crate tags fire independent
    /// `Release.yml` runs that would otherwise race the sparse-index
    /// propagation.
    ///
    /// Single-crate workspaces and lockstep-bumped monorepos (anodizer
    /// itself) leave this off — there is no inter-tag race to gate on.
    pub wait_for_workspace_deps: Option<WaitForWorkspaceDepsConfig>,

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
    /// skipped. Render failure hard-errors. Config key: the publisher's `if:`.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
    /// When `true`, a triggered rollback leaves this publisher's work in
    /// place rather than attempting to undo it. Default `false`.
    pub retain_on_rollback: Option<bool>,
}

/// Pre-publish polling gate for `cargo publish`. When `enabled`, the cargo
/// publisher reads its crate's manifest, identifies every dep that points
/// at another crate in the same anodize workspace, and polls
/// `https://index.crates.io/<prefix>/<name>` until each `(name, version)`
/// pair is queryable. Only then does `cargo publish` run.
///
/// Default: disabled. Anodize's own workspaces publish lockstep with one
/// tag; this feature only kicks in for multi-tag-multi-crate workspaces
/// like cfgd where downstream crates can otherwise race the sparse-index
/// propagation of their upstream deps.
///
/// Complementary to `cargo.index_timeout`: this gate runs BEFORE publish
/// (waits for *upstream* deps to land), while `index_timeout` runs AFTER
/// publish (waits for the *just-published* crate to land before the next
/// dependent in the same run starts).
///
/// ```yaml
/// publish:
///   cargo:
///     wait_for_workspace_deps:
///       enabled: true
///       poll_interval: 5s
///       max_wait: 5m
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct WaitForWorkspaceDepsConfig {
    /// Master switch. Default `false` preserves today's behavior for
    /// single-crate workspaces and lockstep monorepos.
    pub enabled: Option<bool>,
    /// Time between successive index probes. Humantime-style string
    /// (e.g. `"5s"`, `"500ms"`, `"1m"`). Default: `"5s"`.
    pub poll_interval: Option<HumanDuration>,
    /// Hard ceiling on the total wait. The publisher bails with a clear
    /// error once `max_wait` elapses without every dep appearing.
    /// Humantime-style string (e.g. `"5m"`, `"30s"`). Default: `"5m"`.
    pub max_wait: Option<HumanDuration>,
}

impl WaitForWorkspaceDepsConfig {
    /// Default poll interval — short enough to feel snappy when the
    /// upstream's publish lands quickly, long enough that a 5-minute
    /// wait window costs at most 60 HTTP probes.
    pub const DEFAULT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

    /// Default ceiling — five minutes matches the historical
    /// `index_timeout` default and covers the worst-case sparse-index
    /// CDN propagation window observed in practice.
    pub const DEFAULT_MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(300);

    /// Resolve `enabled`, defaulting to `false` (master switch off).
    pub fn resolved_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }

    /// Resolve `poll_interval`, falling back to
    /// [`Self::DEFAULT_POLL_INTERVAL`].
    pub fn resolved_poll_interval(&self) -> std::time::Duration {
        self.poll_interval
            .map(|d| d.duration())
            .unwrap_or(Self::DEFAULT_POLL_INTERVAL)
    }

    /// Resolve `max_wait`, falling back to [`Self::DEFAULT_MAX_WAIT`].
    pub fn resolved_max_wait(&self) -> std::time::Duration {
        self.max_wait
            .map(|d| d.duration())
            .unwrap_or(Self::DEFAULT_MAX_WAIT)
    }
}

// ---------------------------------------------------------------------------
// Publisher gate overrides
// ---------------------------------------------------------------------------

/// Uniform access to the `required` / `retain_on_rollback` release-gate
/// overrides that every publisher config block carries.
///
/// The publish registry collapses these overrides across all of a
/// publisher's config sources (per-crate blocks over the full crate
/// universe, top-level entry lists) into a single publisher-level value.
/// Implementing this trait — rather than hand-copying the field accessor
/// chain per publisher — makes it impossible for a publisher to collapse
/// `required` but forget the parallel `retain_on_rollback` collapse (or
/// vice versa).
pub trait PublisherGateOverrides {
    /// Config-level `required:` override. `None` keeps the publisher's
    /// built-in default; `Some(true)` anywhere escalates the release gate.
    fn required_override(&self) -> Option<bool>;
    /// Config-level `retain_on_rollback:` override. `Some(true)` anywhere
    /// opts the publisher's successful work out of rollback.
    fn retain_on_rollback_override(&self) -> Option<bool>;
}

macro_rules! impl_publisher_gate_overrides {
    ($($t:ty),+ $(,)?) => {
        $(impl PublisherGateOverrides for $t {
            fn required_override(&self) -> Option<bool> {
                self.required
            }
            fn retain_on_rollback_override(&self) -> Option<bool> {
                self.retain_on_rollback
            }
        })+
    };
}

impl_publisher_gate_overrides!(
    CargoPublishConfig,
    HomebrewConfig,
    HomebrewCaskConfig,
    ScoopConfig,
    ChocolateyConfig,
    WingetConfig,
    AurConfig,
    AurSourceConfig,
    KrewConfig,
    NixConfig,
    super::ReleaseConfig,
    super::DockerHubConfig,
    super::ArtifactoryConfig,
    super::UploadConfig,
    super::CloudSmithConfig,
    super::NpmConfig,
    super::GemFuryConfig,
    super::PypiConfig,
    super::HomebrewCoreConfig,
);
