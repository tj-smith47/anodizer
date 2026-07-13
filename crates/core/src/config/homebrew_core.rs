use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{CommitAuthorConfig, StringOrBool, deserialize_string_or_bool_opt};

/// homebrew-core formula-bump publisher configuration.
///
/// Bumps an EXISTING formula in `Homebrew/homebrew-core` (or any formula
/// repository override) purely through the GitHub API — no clone, no `brew`
/// invocation. The formula file's `url` (or `tag:`/`revision:` pair),
/// `sha256`, and `version` stanzas are rewritten to the new release, the
/// change is committed to a branch, and a pull request is opened against the
/// formula repository. Each `homebrew_cores[]` entry bumps one formula.
///
/// Every field is optional: the formula name defaults to the crate name, the
/// target repository defaults to `Homebrew/homebrew-core`, the formula path
/// defaults to the sharded core layout (`Formula/<letter>/<name>.rb`, falling
/// back to the flat `Formula/<name>.rb`), and the download URL defaults to
/// the GitHub source tarball for the release tag.
///
/// ```yaml
/// homebrew_cores:
///   - name: my-tool
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HomebrewCoreConfig {
    /// Unique identifier for selecting this entry from the CLI (`--id=...`).
    pub id: Option<String>,

    /// Crate scoping: when set, the formula `name` default is derived from
    /// the first crate named here instead of the workspace's primary crate.
    /// The workspace per-crate pattern — one `homebrew_cores[]` entry per
    /// crate, each scoped by `ids:`.
    pub ids: Option<Vec<String>>,

    /// Formula name (templated). Defaults to the scoped crate name (see
    /// `ids:`), then the workspace's primary crate name, then the project
    /// name.
    pub name: Option<String>,

    /// Target formula repository. Defaults to `Homebrew/homebrew-core`.
    /// Carries the auth token override (`repository.token`), the base
    /// branch (`repository.branch`, default: the repo's default branch),
    /// and PR settings (`repository.pull_request.draft` / `.body`).
    ///
    /// ```yaml
    /// homebrew_cores:
    ///   - repository: { owner: my-org, name: my-formulas }
    /// ```
    pub repository: Option<super::RepositoryConfig>,

    /// Formula file path inside the repository (templated). Defaults to the
    /// homebrew-core sharded layout `Formula/<first-letter>/<name>.rb`,
    /// falling back to the flat `Formula/<name>.rb` used by most personal
    /// taps when the sharded path does not exist.
    pub path: Option<String>,

    /// Templated download URL written into the formula's `url` stanza.
    /// Defaults to the GitHub source tarball for the release tag:
    /// `https://github.com/<owner>/<repo>/archive/refs/tags/<tag>.tar.gz`
    /// (owner/repo derived from the crate's release repository, then the
    /// git remote).
    pub download_url: Option<String>,

    /// Hex SHA-256 of the new download (templated). When unset, anodizer
    /// downloads `download_url` and hashes it — the same behavior as
    /// `brew bump-formula-pr` without `--sha256`.
    pub sha256: Option<String>,

    /// Templated commit message (also the PR title). Default:
    /// `"<formula> <version>"` — the message form homebrew-core's CI
    /// expects for version bumps.
    pub commit_msg_template: Option<String>,

    /// Commit author for the formula-bump commit, with optional signing.
    /// Its `use_github_app_token` is the canonical homebrew-core knob: when
    /// set, the `author`/`committer` fields are omitted from the contents-API
    /// commit so GitHub attributes it to the token's own account — the
    /// `<app-slug>[bot]` identity a GitHub App workflow needs to satisfy
    /// homebrew-core's DCO/CLA policy. Otherwise the resolved `name`/`email`
    /// (config → local git identity → the anodizer default) author the commit.
    ///
    /// Note: `signing` has no effect here — formula bumps commit through the
    /// GitHub contents API, which GitHub signs server-side; the git `-c
    /// commit.gpgsign` path the tap/winget/krew publishers use does not apply.
    ///
    /// ```yaml
    /// homebrew_cores:
    ///   - commit_author:
    ///       use_github_app_token: true
    /// ```
    pub commit_author: Option<CommitAuthorConfig>,

    /// Commit straight to the base branch instead of opening a pull
    /// request. Accepts bool or template string. Only honored for formula
    /// repositories you can push to — bumps targeting
    /// `Homebrew/homebrew-core` always go through a fork + PR, because
    /// homebrew-core never accepts direct pushes.
    ///
    /// A back-compat alias for `repository.pull_request.enabled: false`, the
    /// preferred spelling shared with the tap/scoop/nix publishers — either
    /// one selects the direct-commit path (both are still overridden by the
    /// always-fork-and-PR rule for `Homebrew/homebrew-core`).
    #[serde(default, deserialize_with = "deserialize_string_or_bool_opt")]
    pub direct_commit: Option<StringOrBool>,

    /// When truthy, refresh the existing bump PR in place instead of skipping
    /// it: a same-version re-cut force-resets the bump branch to the current
    /// base and re-commits the rewritten formula, so the open PR carries this
    /// run's content rather than a stale earlier attempt (and no duplicate PR
    /// is opened). When falsy (default), an already-open bump PR is left
    /// untouched and a warning names this toggle. Accepts bool or template
    /// string. Mirrors `winget` / `krew` / `homebrew_cask`'s
    /// `update_existing_pr`.
    #[serde(default, deserialize_with = "deserialize_string_or_bool_opt")]
    pub update_existing_pr: Option<StringOrBool>,

    /// Skip this publisher. Accepts bool or template string.
    /// Accepts the legacy `disable:` spelling via serde alias for back-compat.
    #[serde(
        default,
        alias = "disable",
        deserialize_with = "deserialize_string_or_bool_opt"
    )]
    pub skip: Option<StringOrBool>,

    /// Override whether this publisher failing should fail the overall release.
    ///
    /// Default: `false` — the bump is a PR that can be re-opened by hand, so
    /// a failure here is logged but does not abort the release. Set to
    /// `true` to fail the release on any error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,

    /// Template-conditional gate: when the rendered result is falsy
    /// (`"false"` / `"0"` / `"no"` / empty), this entry is skipped.
    /// Render failure hard-errors.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,

    /// When `true`, a triggered rollback leaves the opened pull request in
    /// place rather than closing it. Default `false`.
    pub retain_on_rollback: Option<bool>,
}
