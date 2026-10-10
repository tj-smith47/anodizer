//! Per-publisher run evidence (the `evidence.json` shape).
//!
//! [`PublishEvidence`] captures what a publisher actually pushed plus
//! the operator-public coordinates a later `anodizer tag rollback`
//! consumes. The [`extra`] slot used to be a free-form
//! `serde_json::Value`; it is now a typed enum
//! ([`PublishEvidenceExtra`]) so the type system structurally
//! prevents credential leakage — a publisher cannot serialize a
//! credential-shaped field into evidence because the variant struct
//! has no such field to hold it.
//!
//! Wire format is preserved: `#[serde(untagged)]` on the enum keeps
//! the rendered JSON identical to the prior free-form
//! `{ "<publisher>_targets": [...] }` shape, so consumers of
//! `dist/run-<id>/report.json` and `summary.json` see the same bytes.
//!
//! [`extra`]: PublishEvidence::extra

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// One entry in [`HomebrewExtra::homebrew_targets`] — the operator-public
/// snapshot of a single tap push. Mirrors the serialized field set of
/// `HomebrewTarget` in `stage-publish/src/homebrew/publisher.rs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct HomebrewTargetSnapshot {
    /// Per-target label — formula name, cask name, or `homebrew_casks`
    /// for the top-level path.
    pub target: String,
    /// HTTPS clone URL of the tap repo.
    pub repo_url: String,
    /// Branch the publish path pushed to. `None` means default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Env var NAME to consult for the rollback re-clone token.
    /// NEVER the token VALUE.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env_var: Option<String>,
    /// Full sha of the commit this run pushed to the repository. A rollback
    /// reverts exactly this commit; a record without one (written before the
    /// field existed) is skipped with a warning, because the branch head may
    /// by then be someone else's commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct HomebrewExtra {
    pub homebrew_targets: Vec<HomebrewTargetSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ScoopTargetSnapshot {
    pub target: String,
    pub repo_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env_var: Option<String>,
    /// Full sha of the commit this run pushed to the repository. A rollback
    /// reverts exactly this commit; a record without one (written before the
    /// field existed) is skipped with a warning, because the branch head may
    /// by then be someone else's commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ScoopExtra {
    pub scoop_targets: Vec<ScoopTargetSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct NixTargetSnapshot {
    pub target: String,
    pub repo_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env_var: Option<String>,
    /// Full sha of the commit this run pushed to the repository. A rollback
    /// reverts exactly this commit; a record without one (written before the
    /// field existed) is skipped with a warning, because the branch head may
    /// by then be someone else's commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct NixExtra {
    pub nix_targets: Vec<NixTargetSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct WingetTargetSnapshot {
    pub target: String,
    pub crate_name: String,
    pub package_id: String,
    pub version: String,
    pub upstream_owner: String,
    pub upstream_repo: String,
    pub fork_owner: String,
    pub branch: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct WingetExtra {
    pub winget_targets: Vec<WingetTargetSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ChocolateyTargetSnapshot {
    pub target: String,
    pub crate_name: String,
    pub package_id: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ChocolateyExtra {
    pub chocolatey_targets: Vec<ChocolateyTargetSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct KrewTargetSnapshot {
    pub target: String,
    pub upstream_owner: String,
    pub upstream_repo: String,
    pub fork_owner: String,
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env_var: Option<String>,
}

// NOTE: no `deny_unknown_fields` here (every sibling extra struct has it).
// An intra-branch iteration carried a `bot_template_pre_image_shas` field
// that was removed before any release tag. Tolerating unknown keys lets a
// `report.json` written by such a build still deserialize for rollback;
// the orphan key is simply ignored. Adding it back would resurrect a field
// that never shipped, so the lenient decode is the durable fix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct KrewExtra {
    /// One entry per crate whose krew publish opened a PR against the
    /// upstream krew-index (the `PrDirect` initial-submission flow).
    /// Empty for plugins already in krew-index, which take the
    /// self-contained webhook path: the krew-release-bot server owns
    /// the krew-index PR there, so anodizer has no PR to roll back.
    pub krew_targets: Vec<KrewTargetSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AurTargetSnapshot {
    pub target: String,
    /// AUR SSH URL — operator-public coordinate.
    pub git_url: String,
    /// Full sha of the commit this run pushed to the repository. A rollback
    /// reverts exactly this commit; a record without one (written before the
    /// field existed) is skipped with a warning, because the branch head may
    /// by then be someone else's commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AurExtra {
    pub aur_our_targets: Vec<AurTargetSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AurSourceTargetSnapshot {
    pub target: String,
    pub package: String,
    pub tag: String,
    pub git_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AurSourceExtra {
    pub aur_source_targets: Vec<AurSourceTargetSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct McpTargetSnapshot {
    pub target: String,
    pub server_name: String,
    pub registry_url: String,
    pub version: String,
    /// MCP auth method — operator-public; carries no credential bytes.
    /// Serializes as `"none"` / `"github"` / `"github-oidc"` per the
    /// rename annotations on [`crate::config::McpAuthMethod`].
    pub auth_method: crate::config::McpAuthMethod,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct McpExtra {
    pub mcp_targets: Vec<McpTargetSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct DockerhubTargetSnapshot {
    pub target: String,
    pub repo_url: String,
    pub namespace: String,
    pub name: String,
    /// DockerHub login — operator-public.
    pub username: String,
    /// Env var NAME the rollback path consults to re-resolve the password.
    /// NEVER the password VALUE.
    pub secret_env_var: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_full_description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct DockerhubExtra {
    pub dockerhub_targets: Vec<DockerhubTargetSnapshot>,
}

/// One crate whose `cargo publish` SUCCEEDED during this run, recorded
/// at the moment of success so rollback can yank exactly what went live.
///
/// `version` is the per-crate version actually published (workspaces with
/// mixed cadences publish different versions per crate). `registry` /
/// `index` mirror the `publish.cargo.registry` / `publish.cargo.index`
/// the publish used so the yank targets the SAME registry — both are
/// operator-public identifiers (a registry name, an index URL), never
/// credential bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CargoYankTargetSnapshot {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CargoExtra {
    pub cargo_yank_targets: Vec<CargoYankTargetSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ArtifactoryTargetSnapshot {
    pub entry: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ArtifactoryExtra {
    pub artifactory_targets: Vec<ArtifactoryTargetSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CloudsmithTargetSnapshot {
    pub org: String,
    pub repo: String,
    pub filename: String,
    #[serde(default)]
    pub slug: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CloudsmithExtra {
    pub cloudsmith_targets: Vec<CloudsmithTargetSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct BlobTargetSnapshot {
    pub provider: String,
    pub bucket: String,
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// The key already held an object before this release wrote to it —
    /// different content, or content that could not be compared. A rollback
    /// leaves such an object in place and says so: deleting it would remove
    /// a key this release did not create, and the earlier content cannot be
    /// put back.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub overwrote: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct BlobExtra {
    pub blob_targets: Vec<BlobTargetSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SnapcraftTargetSnapshot {
    pub crate_name: String,
    pub package_name: String,
    #[serde(default)]
    pub channel: Option<String>,
    /// The snapcraft architecture this entry's revision was issued for
    /// (`amd64`, `arm64`, …). A dual-arch snap creates one Snap Store revision
    /// per architecture, so the evidence carries one entry per arch and a
    /// `promote --from-run` releases every arch's recorded revision. `None`
    /// on a planned-but-unprocessed snapshot or an older report predating the
    /// per-arch model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    #[serde(default)]
    pub revision: Option<String>,
    /// The version this run uploaded, so post-publish verification can probe
    /// the store's channel map for it.
    #[serde(default)]
    pub version: Option<String>,
    /// The Snap Store answered the upload with a manual-review hold: the
    /// binary was accepted but is NOT live in any channel until a human
    /// approves it. Recorded so the verify-release landing check and
    /// `anodizer tag rollback` consumers see the unresolved state
    /// instead of a silent "uploaded".
    #[serde(default)]
    pub held_for_review: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SnapcraftExtra {
    pub snapcraft_targets: Vec<SnapcraftTargetSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct GithubReleaseTargetSnapshot {
    pub crate_name: String,
    pub owner: String,
    pub repo: String,
    pub tag: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct GithubReleaseExtra {
    pub github_release_targets: Vec<GithubReleaseTargetSnapshot>,
}

/// Operator-public snapshot of a single NPM `package@version` publish.
/// Stored in [`NpmExtra::npm_targets`] so a later `anodizer tag rollback`
/// has the exact coordinates required to attempt `npm unpublish` within
/// the 72-hour window.
///
/// **CREDENTIAL CONTRACT**: no token field — the auth token is resolved
/// at publish/rollback time from the env var named by `token_env_var`
/// (default `NPM_TOKEN`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct NpmTargetSnapshot {
    /// Per-target label — the package name (scoped or unscoped).
    pub target: String,
    /// NPM package name as published (e.g. `@scope/foo`).
    pub package: String,
    /// Published version (semver string, no `v` prefix).
    pub version: String,
    /// Registry endpoint URL (e.g. `https://registry.npmjs.org`).
    pub registry: String,
    /// Dist-tag the version was pushed under (default `latest`).
    pub dist_tag: String,
    /// Env var NAME to consult for the rollback `npm unpublish` token.
    /// NEVER the token VALUE.
    pub token_env_var: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct NpmExtra {
    pub npm_targets: Vec<NpmTargetSnapshot>,
}

/// Operator-public snapshot of a single GemFury push of one artifact file.
/// Stored in [`GemFuryExtra::gemfury_targets`] so a later
/// `anodizer tag rollback` has the exact coordinates required to
/// issue `DELETE https://api.fury.io/<account>/packages/<name>/versions/<version>`
/// against the Fury delete API.
///
/// **CREDENTIAL CONTRACT**: no token fields — push and delete tokens are
/// resolved at publish/rollback time from the env vars named by
/// `push_token_env_var` (default `FURY_PUSH_TOKEN`) and `api_token_env_var`
/// (default `FURY_API_TOKEN`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct GemFuryTargetSnapshot {
    /// Per-target label — `<account>/<package>` for log lines.
    pub target: String,
    /// GemFury account name (operator-public; the `<account>` segment of
    /// `https://push.fury.io/<account>`).
    pub account: String,
    /// Package basename pushed (e.g. `mytool_1.2.3_amd64.deb`).
    pub package: String,
    /// Published version (semver string, no `v` prefix).
    pub version: String,
    /// Artifact format as detected from the filename extension
    /// (`deb` / `rpm` / `apk`).
    pub format: String,
    /// Env var NAME the rollback path consults to re-resolve the push
    /// token. NEVER the token VALUE.
    pub push_token_env_var: String,
    /// Env var NAME the rollback path consults to re-resolve the API
    /// (delete) token. NEVER the token VALUE.
    pub api_token_env_var: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct GemFuryExtra {
    pub gemfury_targets: Vec<GemFuryTargetSnapshot>,
}

/// Operator-public snapshot of a single file uploaded to a PyPI-compatible
/// index (one wheel or one sdist). Stored in [`PypiExtra::pypi_files`].
/// PyPI uploads are one-way — a published filename can NEVER be re-uploaded,
/// even after deletion — so these snapshots exist for the audit trail and
/// the run report, not for a programmatic rollback.
///
/// **CREDENTIAL CONTRACT**: no token field — the upload token is resolved
/// at publish time from config / `PYPI_TOKEN` / `MATURIN_PYPI_TOKEN` and is
/// never persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PypiFileSnapshot {
    /// Uploaded filename (e.g. `my_tool-1.2.3-py3-none-manylinux_2_28_x86_64.whl`).
    pub filename: String,
    /// Wheel platform tag (e.g. `manylinux_2_28_x86_64`), or `source` for an
    /// sdist.
    pub platform_tag: String,
    /// Hex SHA-256 of the uploaded file.
    pub sha256: String,
    /// Upload endpoint URL the file was sent to.
    pub repository: String,
    /// `true` when the index rejected the file as already existing and the
    /// entry's `skip_existing` treated that as an idempotent skip — the file
    /// was already live from an earlier run, not placed by this one.
    pub skipped_existing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PypiExtra {
    pub pypi_files: Vec<PypiFileSnapshot>,
}

/// Operator-public snapshot of a single SchemaStore registration PR — the
/// fork branch anodizer pushed and the upstream it opened the PR against.
/// Stored in [`SchemastoreExtra::schemastore_targets`] so a later
/// `anodizer tag rollback` can find and close the open PR.
///
/// **CREDENTIAL CONTRACT**: no token field — the rollback token is
/// resolved at rollback time from the env var named by `token_env_var`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SchemastoreTargetSnapshot {
    /// Upstream `SchemaStore/schemastore` owner the PR targets.
    pub upstream_owner: String,
    /// Upstream repo name (`schemastore`).
    pub upstream_repo: String,
    /// Login of the fork the branch was pushed to.
    pub fork_owner: String,
    /// Branch pushed to the fork (the PR head).
    pub branch: String,
    /// Env var NAME the rollback path consults for the close-PR token.
    /// NEVER the token VALUE.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env_var: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SchemastoreExtra {
    pub schemastore_targets: Vec<SchemastoreTargetSnapshot>,
}

/// Operator-public snapshot of a single homebrew-core formula bump — the
/// branch anodizer committed the rewritten formula to and the upstream the
/// PR targets. Stored in [`HomebrewCoreExtra::homebrew_core_targets`] so a
/// later `anodizer tag rollback` can find and close the open PR.
///
/// **CREDENTIAL CONTRACT**: no token field — the rollback token is
/// resolved at rollback time from the env var named by `token_env_var`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct HomebrewCoreTargetSnapshot {
    /// Formula name that was bumped (e.g. `my-tool`).
    pub formula: String,
    /// Version the formula was bumped to.
    pub version: String,
    /// Owner of the formula repository the PR targets (`Homebrew` for core).
    pub upstream_owner: String,
    /// Name of the formula repository (`homebrew-core` for core).
    pub upstream_repo: String,
    /// Login of the repo the bump branch was pushed to — the fork owner on
    /// the fork+PR path, the upstream owner on the same-repo-branch path.
    pub head_owner: String,
    /// Branch carrying the bump commit (the PR head branch). Empty on the
    /// `direct_commit` path, where the bump reached the base branch.
    pub branch: String,
    /// `true` when the bump was committed straight to the base branch
    /// (`direct_commit: true`) — nothing to close on rollback.
    pub direct_commit: bool,
    /// URL of the opened pull request, when one was opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    /// Env var NAME the rollback path consults for the close-PR token.
    /// NEVER the token VALUE.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env_var: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct HomebrewCoreExtra {
    pub homebrew_core_targets: Vec<HomebrewCoreTargetSnapshot>,
}

/// Typed `extra` payload for [`PublishEvidence`]. Untagged on the wire —
/// each variant's JSON shape matches the prior free-form
/// `serde_json::json!({"<publisher>_targets": [...]})` form so existing
/// consumers of `dist/run-<id>/report.json` and `summary.json` see no
/// byte-shape change.
///
/// **CREDENTIAL CONTRACT**: every variant's inner struct exposes ONLY
/// operator-public fields. Credential VALUES (token bytes, passwords,
/// SSH key material) have no field to end up in — the type system rejects
/// any future leak attempt at the compile boundary. Per-publisher
/// runtime credentials (resolved from env / config at publish time)
/// live in crate-local `*Target` structs with `#[serde(skip)]`
/// discipline; they convert into the snapshots above at the encode
/// boundary, dropping the secret fields by definition.
///
/// The [`Empty`](Self::Empty) variant covers publishers that have no
/// per-evidence operator-public fields (or that no-op'd the run).
/// Serializes as `null` on the wire and is the deserialization
/// fallback for the same shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(untagged)]
pub enum PublishEvidenceExtra {
    Homebrew(HomebrewExtra),
    Scoop(ScoopExtra),
    Nix(NixExtra),
    Winget(WingetExtra),
    Chocolatey(ChocolateyExtra),
    Krew(KrewExtra),
    Aur(AurExtra),
    AurSource(AurSourceExtra),
    Mcp(McpExtra),
    Dockerhub(DockerhubExtra),
    Cargo(CargoExtra),
    Artifactory(ArtifactoryExtra),
    Cloudsmith(CloudsmithExtra),
    Blob(BlobExtra),
    Snapcraft(SnapcraftExtra),
    GithubRelease(GithubReleaseExtra),
    Npm(NpmExtra),
    GemFury(GemFuryExtra),
    Pypi(PypiExtra),
    Schemastore(SchemastoreExtra),
    HomebrewCore(HomebrewCoreExtra),
    /// Default for publishers with no per-evidence operator-public fields,
    /// or for runs that no-op'd. Serializes as JSON `null`.
    #[default]
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishEvidence {
    pub schema_version: u32,
    pub publisher: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_ref: Option<String>,
    pub artifact_paths: Vec<PathBuf>,
    /// Content-hash snapshot (artifact NAME → `sha256:<hex>`) taken from
    /// each in-scope [`crate::artifact::Artifact`]'s `Checksum` metadata
    /// at publish time. Used by the ledger fast-path: a later reconcile
    /// consults this map to tell "rebuilt identical bytes" (safe to
    /// short-circuit) apart from "rebuilt different bytes, same version"
    /// (must fall through to the network probe). `#[serde(default)]` so
    /// v1 `summary.json` files written before this field existed still
    /// deserialize — a reader that hard-failed on a missing field would
    /// silently degrade every reconcile to a network round-trip.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub artifact_digests: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nondeterministic: Option<String>,
    /// Operator-public metadata for the publisher run.
    ///
    /// **CREDENTIAL CONTRACT**: this field is persisted to
    /// `dist/run-<id>/report.json`, summarised in `summary.json`, and
    /// may be attached to the GitHub Release body via the announce
    /// stage. It carries only operator-public identifiers (URLs,
    /// env-var NAMES, PR numbers, tag strings, branch names). Token
    /// VALUES, private keys, passwords, OAuth secrets, SSH key
    /// material have no variant field to end up in — the
    /// [`PublishEvidenceExtra`] enum's per-variant struct list is the
    /// schema, and serde rejects fields it does not name.
    ///
    /// Per-publisher rollback state (runtime-only credentials read
    /// from env / config at publish time) lives in crate-local
    /// `*Target` structs with `#[serde(skip)]` discipline; those
    /// convert into the [`PublishEvidenceExtra`] variant snapshots at
    /// the encode boundary, dropping the secret fields by definition.
    #[serde(default, deserialize_with = "deserialize_extra_compat")]
    pub extra: PublishEvidenceExtra,
}

/// Deserialize the `extra:` field with backwards-compatibility for
/// reports written before the typed [`PublishEvidenceExtra`] enum
/// published. Those reports carried `extra: {}` (an empty object) where
/// the typed enum's [`Empty`](PublishEvidenceExtra::Empty) variant
/// serializes as `null`. With `#[serde(untagged)]` neither null nor
/// the typed struct variants match `{}`, so a literal `{}` from an
/// older report fails to deserialize. This shim coerces null and `{}`
/// to `Empty`; any other shape goes through the normal untagged
/// dispatch.
fn deserialize_extra_compat<'de, D>(deserializer: D) -> Result<PublishEvidenceExtra, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    if value.is_null() {
        return Ok(PublishEvidenceExtra::Empty);
    }
    if let Some(map) = value.as_object()
        && map.is_empty()
    {
        return Ok(PublishEvidenceExtra::Empty);
    }
    serde_json::from_value(value).map_err(serde::de::Error::custom)
}

/// Append to `into` every element of `from` it does not already hold.
fn union_into<T: PartialEq + Clone>(into: &mut Vec<T>, from: &[T]) {
    for item in from {
        if !into.contains(item) {
            into.push(item.clone());
        }
    }
}

impl PublishEvidence {
    /// Whether this evidence names work the run committed — something a
    /// rollback could act on.
    ///
    /// A typed `extra` answers through its own list, so an empty list is no
    /// work whatever `primary_ref` says. Two kinds list entries that are not
    /// this run's work: a PyPI file the index already held
    /// (`skipped_existing`) and a snap entry with no uploaded revision.
    pub fn records_published_work(&self) -> bool {
        use PublishEvidenceExtra as E;
        match &self.extra {
            E::Empty => self.primary_ref.is_some() || !self.artifact_paths.is_empty(),
            E::Homebrew(x) => !x.homebrew_targets.is_empty(),
            E::Scoop(x) => !x.scoop_targets.is_empty(),
            E::Nix(x) => !x.nix_targets.is_empty(),
            E::Winget(x) => !x.winget_targets.is_empty(),
            E::Chocolatey(x) => !x.chocolatey_targets.is_empty(),
            E::Krew(x) => !x.krew_targets.is_empty(),
            E::Aur(x) => !x.aur_our_targets.is_empty(),
            E::AurSource(x) => !x.aur_source_targets.is_empty(),
            E::Mcp(x) => !x.mcp_targets.is_empty(),
            E::Dockerhub(x) => !x.dockerhub_targets.is_empty(),
            E::Cargo(x) => !x.cargo_yank_targets.is_empty(),
            E::Artifactory(x) => !x.artifactory_targets.is_empty(),
            E::Cloudsmith(x) => !x.cloudsmith_targets.is_empty(),
            E::Blob(x) => !x.blob_targets.is_empty(),
            E::Snapcraft(x) => x.snapcraft_targets.iter().any(|t| t.revision.is_some()),
            E::GithubRelease(x) => !x.github_release_targets.is_empty(),
            E::Npm(x) => !x.npm_targets.is_empty(),
            E::GemFury(x) => !x.gemfury_targets.is_empty(),
            E::Pypi(x) => x.pypi_files.iter().any(|f| !f.skipped_existing),
            E::Schemastore(x) => !x.schemastore_targets.is_empty(),
            E::HomebrewCore(x) => !x.homebrew_core_targets.is_empty(),
        }
    }

    /// Fold what an earlier run of the same release recorded into this
    /// run's evidence, returning whether the two could be combined.
    ///
    /// Two records of the same kind combine into one list holding each
    /// entry once. A re-run publishes only what the earlier run left
    /// missing, so without the union its record would name a part of the
    /// release and a rollback would leave the rest behind. A tap entry
    /// carries the commit it pushed, so two pushes to one tap are two
    /// entries and each is reverted by its own sha.
    ///
    /// Records of different kinds, and a record with no typed list, return
    /// `false` and are left alone; the caller keeps the earlier row beside
    /// this one. A record equal to this one is already held and returns
    /// `true`.
    pub fn absorb_prior(&mut self, prior: &PublishEvidence) -> bool {
        use PublishEvidenceExtra as E;
        // The same record read back (the pipeline end rewriting what the
        // publish stage wrote) adds nothing and is not a second row.
        if *self == *prior {
            return true;
        }
        match (&mut self.extra, &prior.extra) {
            (E::Blob(mine), E::Blob(theirs)) => {
                for earlier in &theirs.blob_targets {
                    let same_object = |t: &&mut BlobTargetSnapshot| {
                        (&t.provider, &t.bucket, &t.key, &t.region, &t.endpoint)
                            == (
                                &earlier.provider,
                                &earlier.bucket,
                                &earlier.key,
                                &earlier.region,
                                &earlier.endpoint,
                            )
                    };
                    match mine.blob_targets.iter_mut().find(same_object) {
                        // The earliest write of the release is the one that
                        // saw whether the key existed before it.
                        Some(t) => t.overwrote = earlier.overwrote,
                        None => mine.blob_targets.push(earlier.clone()),
                    }
                }
            }
            (E::Homebrew(m), E::Homebrew(t)) => {
                union_into(&mut m.homebrew_targets, &t.homebrew_targets)
            }
            (E::Scoop(m), E::Scoop(t)) => union_into(&mut m.scoop_targets, &t.scoop_targets),
            (E::Nix(m), E::Nix(t)) => union_into(&mut m.nix_targets, &t.nix_targets),
            (E::Winget(m), E::Winget(t)) => union_into(&mut m.winget_targets, &t.winget_targets),
            (E::Chocolatey(m), E::Chocolatey(t)) => {
                union_into(&mut m.chocolatey_targets, &t.chocolatey_targets)
            }
            (E::Krew(m), E::Krew(t)) => union_into(&mut m.krew_targets, &t.krew_targets),
            (E::Aur(m), E::Aur(t)) => union_into(&mut m.aur_our_targets, &t.aur_our_targets),
            (E::AurSource(m), E::AurSource(t)) => {
                union_into(&mut m.aur_source_targets, &t.aur_source_targets)
            }
            (E::Mcp(m), E::Mcp(t)) => union_into(&mut m.mcp_targets, &t.mcp_targets),
            (E::Dockerhub(m), E::Dockerhub(t)) => {
                union_into(&mut m.dockerhub_targets, &t.dockerhub_targets)
            }
            (E::Cargo(m), E::Cargo(t)) => {
                union_into(&mut m.cargo_yank_targets, &t.cargo_yank_targets)
            }
            (E::Artifactory(m), E::Artifactory(t)) => {
                union_into(&mut m.artifactory_targets, &t.artifactory_targets)
            }
            (E::Cloudsmith(m), E::Cloudsmith(t)) => {
                union_into(&mut m.cloudsmith_targets, &t.cloudsmith_targets)
            }
            (E::Snapcraft(m), E::Snapcraft(t)) => {
                union_into(&mut m.snapcraft_targets, &t.snapcraft_targets)
            }
            (E::GithubRelease(m), E::GithubRelease(t)) => {
                union_into(&mut m.github_release_targets, &t.github_release_targets)
            }
            (E::Npm(m), E::Npm(t)) => union_into(&mut m.npm_targets, &t.npm_targets),
            (E::GemFury(m), E::GemFury(t)) => {
                union_into(&mut m.gemfury_targets, &t.gemfury_targets)
            }
            (E::Pypi(m), E::Pypi(t)) => union_into(&mut m.pypi_files, &t.pypi_files),
            (E::Schemastore(m), E::Schemastore(t)) => {
                union_into(&mut m.schemastore_targets, &t.schemastore_targets)
            }
            (E::HomebrewCore(m), E::HomebrewCore(t)) => {
                union_into(&mut m.homebrew_core_targets, &t.homebrew_core_targets)
            }
            _ => return false,
        }
        union_into(&mut self.artifact_paths, &prior.artifact_paths);
        if self.primary_ref.is_none() {
            self.primary_ref = prior.primary_ref.clone();
        }
        true
    }

    /// Version of the evidence wire format. Operators reading the
    /// constant know whether their installed anodizer matches the
    /// producer that wrote a given `report.json` blob; a format change
    /// bumps this alongside the corresponding decoder update.
    pub const CURRENT_SCHEMA_VERSION: u32 = 2;

    pub fn new(publisher: impl Into<String>) -> Self {
        Self {
            schema_version: Self::CURRENT_SCHEMA_VERSION,
            publisher: publisher.into(),
            primary_ref: None,
            artifact_paths: Vec::new(),
            artifact_digests: BTreeMap::new(),
            nondeterministic: None,
            extra: PublishEvidenceExtra::Empty,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_evidence_naming_something_records_published_work() {
        assert!(!PublishEvidence::new("x").records_published_work());
        let mut by_ref = PublishEvidence::new("x");
        by_ref.primary_ref = Some("r".into());
        assert!(by_ref.records_published_work());
        let mut by_path = PublishEvidence::new("x");
        by_path.artifact_paths.push(PathBuf::from("p"));
        assert!(by_path.records_published_work());
    }

    /// A typed list that names nothing is no work, whatever else the record
    /// carries: every variant is asked with its default (empty) payload and
    /// a `primary_ref` set.
    #[test]
    fn an_empty_typed_list_records_no_published_work() {
        use PublishEvidenceExtra as E;
        let empties = [
            E::Homebrew(Default::default()),
            E::Scoop(Default::default()),
            E::Nix(Default::default()),
            E::Winget(Default::default()),
            E::Chocolatey(Default::default()),
            E::Krew(Default::default()),
            E::Aur(Default::default()),
            E::AurSource(Default::default()),
            E::Mcp(Default::default()),
            E::Dockerhub(Default::default()),
            E::Cargo(Default::default()),
            E::Artifactory(Default::default()),
            E::Cloudsmith(Default::default()),
            E::Blob(Default::default()),
            E::Snapcraft(Default::default()),
            E::GithubRelease(Default::default()),
            E::Npm(Default::default()),
            E::GemFury(Default::default()),
            E::Pypi(Default::default()),
            E::Schemastore(Default::default()),
            E::HomebrewCore(Default::default()),
        ];
        for extra in empties {
            let mut e = PublishEvidence::new("x");
            e.primary_ref = Some("r".into());
            e.extra = extra;
            assert!(!e.records_published_work(), "{:?}", e.extra);
        }

        let mut blob = PublishEvidence::new("blob");
        blob.extra = E::Blob(BlobExtra {
            blob_targets: vec![BlobTargetSnapshot::default()],
        });
        assert!(blob.records_published_work());
    }

    /// Entries that are listed without being this run's work: a file the
    /// index already held, and a snap with no uploaded revision.
    #[test]
    fn entries_the_run_did_not_publish_are_not_work() {
        let mut pypi = PublishEvidence::new("pypi");
        let file = |skipped_existing| PypiFileSnapshot {
            skipped_existing,
            ..Default::default()
        };
        pypi.extra = PublishEvidenceExtra::Pypi(PypiExtra {
            pypi_files: vec![file(true)],
        });
        assert!(!pypi.records_published_work());
        pypi.extra = PublishEvidenceExtra::Pypi(PypiExtra {
            pypi_files: vec![file(true), file(false)],
        });
        assert!(pypi.records_published_work());

        let mut snap = PublishEvidence::new("snapcraft");
        let entry = |revision: Option<&str>| SnapcraftTargetSnapshot {
            revision: revision.map(str::to_string),
            ..Default::default()
        };
        snap.extra = PublishEvidenceExtra::Snapcraft(SnapcraftExtra {
            snapcraft_targets: vec![entry(None)],
        });
        assert!(!snap.records_published_work());
        snap.extra = PublishEvidenceExtra::Snapcraft(SnapcraftExtra {
            snapcraft_targets: vec![entry(None), entry(Some("7"))],
        });
        assert!(snap.records_published_work());
    }

    #[test]
    fn absorb_prior_joins_same_kind_lists_once_and_leaves_other_kinds_alone() {
        let blob = |keys: &[&str]| {
            let mut e = PublishEvidence::new("blob");
            e.artifact_paths = keys.iter().map(PathBuf::from).collect();
            e.extra = PublishEvidenceExtra::Blob(BlobExtra {
                blob_targets: keys
                    .iter()
                    .map(|k| BlobTargetSnapshot {
                        provider: "s3".into(),
                        bucket: "b".into(),
                        key: (*k).into(),
                        ..Default::default()
                    })
                    .collect(),
            });
            e
        };
        let mut mine = blob(&["c", "b"]);
        assert!(mine.absorb_prior(&blob(&["a", "b"])));
        assert_eq!(mine, blob(&["c", "b", "a"]));

        // Evidence with no typed list has nothing to join, unless it is the
        // same record.
        let mut untyped = PublishEvidence::new("homebrew");
        untyped.primary_ref = Some("second".into());
        let before = untyped.clone();
        let mut earlier = PublishEvidence::new("homebrew");
        earlier.primary_ref = Some("first".into());
        assert!(!untyped.absorb_prior(&earlier));
        assert_eq!(untyped, before);
        assert!(untyped.absorb_prior(&before));
        assert_eq!(untyped, before);
        // Two different kinds never join either.
        assert!(!mine.absorb_prior(&earlier));
    }

    /// Two pushes to one tap are two commits: both stay, each under its own
    /// sha, and the same push recorded twice stays once.
    #[test]
    fn absorb_prior_keeps_one_tap_entry_per_pushed_commit() {
        let tap = |commits: &[&str]| {
            let mut e = PublishEvidence::new("homebrew");
            e.extra = PublishEvidenceExtra::Homebrew(HomebrewExtra {
                homebrew_targets: commits
                    .iter()
                    .map(|c| HomebrewTargetSnapshot {
                        target: "app".into(),
                        repo_url: "https://github.com/o/tap.git".into(),
                        commit: Some((*c).into()),
                        ..Default::default()
                    })
                    .collect(),
            });
            e
        };
        let mut mine = tap(&["bbb"]);
        assert!(mine.absorb_prior(&tap(&["aaa", "bbb"])));
        assert_eq!(mine, tap(&["bbb", "aaa"]));
    }

    /// Whether a key existed before the release is known to the run that
    /// wrote it first; a later run overwriting its own object does not make
    /// the key one the release did not create, and the reverse holds too.
    #[test]
    fn absorb_prior_takes_overwrote_from_the_earliest_write() {
        let blob = |overwrote| {
            let mut e = PublishEvidence::new("blob");
            e.extra = PublishEvidenceExtra::Blob(BlobExtra {
                blob_targets: vec![BlobTargetSnapshot {
                    provider: "s3".into(),
                    bucket: "b".into(),
                    key: "latest/app".into(),
                    overwrote,
                    ..Default::default()
                }],
            });
            e
        };
        let mut rerun = blob(true);
        assert!(rerun.absorb_prior(&blob(false)));
        assert_eq!(rerun, blob(false));

        let mut rerun = blob(false);
        assert!(rerun.absorb_prior(&blob(true)));
        assert_eq!(rerun, blob(true));
    }

    #[test]
    fn publish_evidence_roundtrips_through_json() {
        let mut e = PublishEvidence::new("homebrew");
        e.primary_ref = Some("refs/heads/main".to_string());
        e.artifact_paths.push(PathBuf::from("dist/foo.tar.gz"));
        e.nondeterministic = Some("timestamp".to_string());
        e.extra = PublishEvidenceExtra::Homebrew(HomebrewExtra {
            homebrew_targets: vec![HomebrewTargetSnapshot {
                target: "demo".into(),
                repo_url: "https://github.com/acme/homebrew-tap.git".into(),
                branch: Some("main".into()),
                token_env_var: Some("HOMEBREW_TAP_TOKEN".into()),
                commit: None,
            }],
        });

        let s = serde_json::to_string(&e).expect("serialize");
        let back: PublishEvidence = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(e, back);
    }

    #[test]
    fn cargo_extra_roundtrips_and_omits_none_registry_index() {
        let mut e = PublishEvidence::new("cargo");
        e.extra = PublishEvidenceExtra::Cargo(CargoExtra {
            cargo_yank_targets: vec![
                CargoYankTargetSnapshot {
                    name: "crate-a".into(),
                    version: "1.0.0".into(),
                    registry: Some("my-registry".into()),
                    index: None,
                },
                CargoYankTargetSnapshot {
                    name: "crate-b".into(),
                    version: "2.0.0".into(),
                    registry: None,
                    index: None,
                },
            ],
        });

        let s = serde_json::to_string(&e).expect("serialize");
        // None registry/index are omitted; the variant key is the typed shape.
        assert!(
            s.contains("cargo_yank_targets"),
            "wire carries variant key: {s}"
        );
        assert!(s.contains("my-registry"), "registry preserved: {s}");
        assert!(
            !s.contains("\"index\""),
            "None index omitted on the wire: {s}"
        );

        let back: PublishEvidence = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(e, back);
        // Untagged disjointness: a Cargo payload decodes to the Cargo
        // variant, not an earlier `*_targets` variant.
        assert!(matches!(back.extra, PublishEvidenceExtra::Cargo(_)));
    }

    #[test]
    fn artifact_digests_defaults_empty_on_v1_json_missing_the_field() {
        let v1 = r#"{
            "schema_version": 1,
            "publisher": "cargo",
            "primary_ref": null,
            "artifact_paths": [],
            "nondeterministic": null,
            "extra": null
        }"#;
        let e: PublishEvidence = serde_json::from_str(v1).expect("v1 summaries must still parse");
        assert!(e.artifact_digests.is_empty());
    }

    #[test]
    fn artifact_digests_roundtrips_through_json() {
        let mut e = PublishEvidence::new("cargo");
        e.artifact_digests
            .insert("myapp-x86_64.tar.gz".into(), "sha256:abc123".into());
        let s = serde_json::to_string(&e).expect("serialize");
        assert!(s.contains("myapp-x86_64.tar.gz"), "{s}");
        let back: PublishEvidence = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(e, back);
    }

    #[test]
    fn publish_evidence_omits_none_fields_on_serialize() {
        let e = PublishEvidence::new("homebrew");
        let s = serde_json::to_string(&e).expect("serialize");
        assert!(
            !s.contains("primary_ref"),
            "primary_ref should be omitted when None: {s}"
        );
        assert!(
            !s.contains("nondeterministic"),
            "nondeterministic should be omitted when None: {s}"
        );
        let back: PublishEvidence = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(e, back);
    }

    #[test]
    fn publish_evidence_rejects_unknown_fields() {
        let bad = r#"{
            "schema_version": 1,
            "publisher": "homebrew",
            "primary_ref": null,
            "artifact_paths": [],
            "nondeterministic": null,
            "extra": null,
            "future_field": "boom"
        }"#;
        let r: Result<PublishEvidence, _> = serde_json::from_str(bad);
        assert!(r.is_err(), "deny_unknown_fields should reject future_field");
    }

    #[test]
    fn empty_variant_serializes_as_null() {
        // The Empty variant is the default for newly constructed evidence;
        // pinning its wire shape ensures back-compat with the prior `{}` /
        // null default and avoids accidental shape drift.
        let e = PublishEvidence::new("homebrew");
        let s = serde_json::to_string(&e).expect("serialize");
        let v: serde_json::Value = serde_json::from_str(&s).expect("parse");
        assert_eq!(v["extra"], serde_json::Value::Null);
    }

    #[test]
    fn empty_variant_deserializes_from_null() {
        // Untagged enum: null reaches Empty (the unit variant is the
        // only one that accepts a null payload). Pin the wire shape
        // so a future variant addition that breaks this path fails
        // here.
        let from_null = serde_json::from_str::<PublishEvidenceExtra>("null").expect("null");
        assert_eq!(from_null, PublishEvidenceExtra::Empty);
    }

    #[test]
    fn krew_extra_roundtrips_pr_direct_targets() {
        // The PrDirect flow records one `KrewTargetSnapshot` per crate
        // whose publish opened a krew-index PR; the BotWebhook flow
        // records none. Pin the round-trip so the rollback consumer
        // always reads back the coordinates it needs to close the PR.
        let extra = KrewExtra {
            krew_targets: vec![KrewTargetSnapshot {
                target: "mytool".into(),
                upstream_owner: "kubernetes-sigs".into(),
                upstream_repo: "krew-index".into(),
                fork_owner: "acme".into(),
                branch: "mytool-v1.2.3".into(),
                token_env_var: Some("KREW_INDEX_TOKEN".into()),
            }],
        };
        let json = serde_json::to_string(&extra).expect("serialize");
        let back: KrewExtra = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, extra);
    }

    #[test]
    fn krew_extra_empty_targets_serializes() {
        // The BotWebhook flow records no targets — an empty `KrewExtra`
        // must still round-trip cleanly.
        let extra = KrewExtra {
            krew_targets: vec![],
        };
        let json = serde_json::to_string(&extra).expect("serialize");
        let back: KrewExtra = serde_json::from_str(&json).expect("deserialize");
        assert!(back.krew_targets.is_empty());
    }

    #[test]
    fn krew_extra_tolerates_orphan_bot_template_key() {
        // A `report.json` written by an intra-branch build that still
        // carried the since-removed `bot_template_pre_image_shas` field
        // must still deserialize for rollback — the orphan key is ignored.
        let blob = r#"{
            "krew_targets": [{
                "target": "mytool",
                "upstream_owner": "kubernetes-sigs",
                "upstream_repo": "krew-index",
                "fork_owner": "acme",
                "branch": "mytool-v1.2.3"
            }],
            "bot_template_pre_image_shas": {"plugins/mytool.yaml": "deadbeef"}
        }"#;
        let back: KrewExtra = serde_json::from_str(blob).expect("orphan key must be tolerated");
        assert_eq!(back.krew_targets.len(), 1);
        assert_eq!(back.krew_targets[0].target, "mytool");

        // The same blob must deserialize through the full untagged
        // `PublishEvidenceExtra` dispatch (the path rollback actually
        // takes), ending up in the Krew variant.
        let via_enum: PublishEvidenceExtra =
            serde_json::from_str(blob).expect("untagged dispatch must tolerate orphan key");
        assert!(matches!(via_enum, PublishEvidenceExtra::Krew(_)));
    }

    #[test]
    fn publish_evidence_schema_version_is_two() {
        // The constant is the operator-visible signal for the evidence
        // wire format. A future bump must update this test alongside
        // the constant.
        assert_eq!(PublishEvidence::CURRENT_SCHEMA_VERSION, 2);
    }

    #[test]
    fn publish_evidence_extra_json_shape_matches_pre_typed_form() {
        // Wire-format pin: downstream consumers of
        // `dist/run-<id>/report.json` see the same byte shape that
        // shipped pre-typed-enum. A variant addition that drifts the
        // shape (e.g. wraps the homebrew_targets array in an extra
        // object) fails this test.
        let e = PublishEvidence {
            extra: PublishEvidenceExtra::Homebrew(HomebrewExtra {
                homebrew_targets: vec![HomebrewTargetSnapshot {
                    target: "demo".into(),
                    repo_url: "https://github.com/owner/tap".into(),
                    branch: Some("anodizer-update".into()),
                    token_env_var: Some("ANODIZER_GITHUB_TOKEN".into()),
                    commit: None,
                }],
            }),
            ..PublishEvidence::new("homebrew")
        };
        let s = serde_json::to_string(&e).expect("serialize");
        let v: serde_json::Value = serde_json::from_str(&s).expect("parse");
        let t = &v["extra"]["homebrew_targets"][0];
        assert_eq!(t["target"], "demo");
        assert_eq!(t["repo_url"], "https://github.com/owner/tap");
        assert_eq!(t["branch"], "anodizer-update");
        assert_eq!(t["token_env_var"], "ANODIZER_GITHUB_TOKEN");
        // Defense-in-depth: no credential-shaped keys in the rendered
        // form (matches the per-publisher `*_extra_carries_no_secret_material`
        // tests but pinned at the core wire-format level).
        assert!(!s.contains("\"token\":"), "{s}");
        assert!(!s.contains("\"password\":"), "{s}");
        assert!(!s.contains("\"pat\":"), "{s}");
        assert!(!s.contains("\"private_key\":"), "{s}");
        assert!(!s.contains("\"secret\":"), "{s}");
        assert!(!s.contains("\"api_key\":"), "{s}");
    }
}
