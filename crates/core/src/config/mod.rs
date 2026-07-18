use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Include specification types
// ---------------------------------------------------------------------------

/// An include specification: either a plain path string or a structured from_file/from_url.
///
/// YAML examples:
/// ```yaml
/// includes:
///   - ./defaults.yaml                           # plain string (backward compat)
///   - from_file:
///       path: ./config/release.yaml              # structured file path
///   - from_url:
///       url: https://example.com/config.yaml     # URL fetch
///       headers:
///         x-api-token: "${MYCOMPANY_TOKEN}"       # env var expansion in headers
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(untagged)]
pub enum IncludeSpec {
    /// Plain string path (backward compatible): "path/to/file.yaml"
    Path(String),
    /// Structured file include with `from_file.path`.
    FromFile { from_file: IncludeFilePath },
    /// Structured URL include with `from_url.url` and optional headers.
    FromUrl { from_url: IncludeUrlConfig },
}

/// File path for a structured include.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct IncludeFilePath {
    /// Path to the include file (relative to the config file).
    pub path: String,
}

/// URL configuration for a structured include.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct IncludeUrlConfig {
    /// URL to fetch. If it does not start with `http://` or `https://`,
    /// `https://raw.githubusercontent.com/` is prepended (GitHub shorthand).
    pub url: String,
    /// Optional HTTP headers. Values support `${VAR_NAME}` environment variable expansion.
    pub headers: Option<HashMap<String, String>>,
}

// ---------------------------------------------------------------------------
// Top-level config
// ---------------------------------------------------------------------------

/// `deny_unknown_fields` rejects typos and unknown config
/// fields at parse time (strict YAML unmarshalling).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Schema version. Currently supports 1 (implicit default) and 2.
    pub version: Option<u32>,
    /// Human-readable project name used in templates and release titles.
    pub project_name: String,
    /// Output directory for build artifacts (default: ./dist).
    #[serde(default = "default_dist")]
    pub dist: PathBuf,
    /// Additional config files to merge into this config.
    /// Supports plain string paths, `from_file:` for structured file paths,
    /// and `from_url:` for fetching configs from URLs with optional headers.
    pub includes: Option<Vec<IncludeSpec>>,
    /// Environment file configuration. Accepts either:
    /// - A list of `.env` file paths: `[".env", ".release.env"]`
    /// - A struct with token file paths: `{ github_token: "~/.config/goreleaser/github_token" }`
    pub env_files: Option<EnvFilesConfig>,
    /// Default values applied to all crates unless overridden.
    pub defaults: Option<Defaults>,
    /// Hooks run before the release pipeline starts.
    pub before: Option<HooksConfig>,
    /// Hooks run after the release pipeline completes.
    pub after: Option<HooksConfig>,
    /// Hooks run when the release pipeline fails at ANY stage (build,
    /// sign, publish, ...), after the failure policy (rollback / hold)
    /// has executed, so `{{ .RolledBack }}` reflects the taken path.
    ///
    /// Notification / cleanup hooks: a hook's own failure is logged as a
    /// warning and never masks the pipeline error. The failure context is
    /// exposed both as template vars (`{{ .Error }}`, `{{ .RolledBack }}`)
    /// and as `ANODIZER_*` env vars (`ANODIZER_ERROR`,
    /// `ANODIZER_ROLLED_BACK`, `ANODIZER_VERSION`, `ANODIZER_TAG`) so
    /// hooks can consume the error text without shell interpolation.
    ///
    /// ```yaml
    /// on_error:
    ///   hooks:
    ///     - cmd: ./notify-release-failed.sh
    /// ```
    pub on_error: Option<HooksConfig>,
    /// Hooks run after build/archive/sign/sbom/checksum complete but
    /// immediately before the publish phase dispatches any publisher.
    ///
    /// Use cases: smoke-test artifacts against the staged dist tree,
    /// run external validators (antivirus, vulnerability scanners),
    /// stage external state, or abort the release before any
    /// publisher writes to a registry.
    ///
    /// A non-zero exit code from any hook aborts the release before
    /// publish runs. Hooks fire in declared order. Use `--skip=before-publish`
    /// to bypass.
    pub before_publish: Option<HooksConfig>,
    /// List of crates in this project.
    pub crates: Vec<CrateConfig>,
    /// Changelog generation configuration.
    pub changelog: Option<ChangelogConfig>,
    /// Signing configurations for binaries, archives, and checksums.
    #[serde(default, deserialize_with = "deserialize_signs")]
    #[schemars(schema_with = "signs_schema")]
    pub signs: Vec<SignConfig>,
    /// Binary-specific signing configs (same shape as `signs` but only for
    /// binary artifacts). The `artifacts` field on each entry is constrained
    /// at parse time to `binary` / `none` (or omitted) — a broader filter on
    /// `binary_signs` would silently match nothing because the loop only
    /// iterates Binary artifacts. Constraint lives in `deserialize_binary_signs`.
    #[serde(default, deserialize_with = "deserialize_binary_signs")]
    #[schemars(schema_with = "signs_schema")]
    pub binary_signs: Vec<SignConfig>,
    /// Docker image signing configurations.
    pub docker_signs: Option<Vec<DockerSignConfig>>,
    // No `alias` attribute needed: unlike `signs`/`sign`, "upx" is already
    // both singular and plural, so a separate alias adds no value.
    /// UPX binary compression configurations.
    #[serde(default, deserialize_with = "deserialize_upx")]
    #[schemars(schema_with = "upx_schema")]
    pub upx: Vec<UpxConfig>,
    /// Snapshot release configuration (local/non-tag builds).
    pub snapshot: Option<SnapshotConfig>,
    /// Nightly release configuration.
    pub nightly: Option<NightlyConfig>,
    /// Announcement configuration (Slack, Discord, email, etc.).
    pub announce: Option<AnnounceConfig>,
    /// When true, log artifact file sizes after building.
    pub report_sizes: Option<bool>,
    /// Environment variables available to all template expressions.
    ///
    /// List of `KEY=VALUE` strings:
    /// `env: ["MY_VAR=hello", "DEPLOY_ENV=staging"]`. Order is preserved so
    /// chained env applications (sign + sbom + notarize) see entries in
    /// declared order. Values are rendered through the template engine before
    /// being set, so expressions like `{{ Tag }}` or `{{ Date }}` are
    /// expanded.
    #[serde(default)]
    pub env: Option<Vec<String>>,
    /// Custom template variables accessible as `{{ Var.<key> }}` in templates.
    /// Provides a way to define reusable values, especially useful with config includes.
    ///
    /// Stored as a `BTreeMap` so rendering iterates in deterministic
    /// (sorted) key order — without this guarantee, a value that references
    /// another variable (`b: "{{ Var.a }}_v2"`) could render before its
    /// dependency on a different process / host. The current resolver is
    /// single-pass (one render per value), so cross-variable references
    /// only resolve when the referenced key sorts earlier.
    pub variables: Option<BTreeMap<String, String>>,
    /// Generic artifact publisher configurations.
    pub publishers: Option<Vec<PublisherConfig>>,
    /// DockerHub description sync configurations.
    pub dockerhub: Option<Vec<DockerHubConfig>>,
    /// Artifactory upload configurations.
    pub artifactories: Option<Vec<ArtifactoryConfig>>,
    /// CloudSmith publisher configurations.
    pub cloudsmiths: Option<Vec<CloudSmithConfig>>,
    /// Top-level Homebrew Cask configurations.
    /// `homebrew_casks` is a top-level array with its own
    /// repository, commit_author, directory, skip_upload, hooks, dependencies,
    /// conflicts, completions, manpages, structured uninstall/zap, etc.
    pub homebrew_casks: Option<Vec<HomebrewCaskConfig>>,
    /// Repo-committed files that embed the release version outside
    /// `Cargo.toml` (e.g. a Helm `Chart.yaml`, an install doc, a README
    /// badge), given as repo-root-relative path strings. At `tag` time each
    /// listed file has its occurrences of the old version rewritten to the new
    /// version — both the bare (`0.1.0`) and `v`-prefixed (`v0.1.0`) forms,
    /// word-boundary anchored — and is staged into the same bump commit as
    /// `Cargo.toml` / `Cargo.lock`, so these files never drift from the tag.
    ///
    /// ```yaml
    /// version_files:
    ///   - charts/cfgd/Chart.yaml
    ///   - docs/installation.md
    /// ```
    pub version_files: Option<Vec<String>>,
    /// Automatic semantic version tagging configuration.
    pub tag: Option<TagConfig>,
    /// Git-level tag discovery and sorting settings.
    pub git: Option<GitConfig>,
    /// Partial/split build configuration for fan-out CI pipelines.
    pub partial: Option<PartialConfig>,
    /// Independent workspace roots in a monorepo.
    pub workspaces: Option<Vec<WorkspaceConfig>>,
    /// Source archive configuration.
    pub source: Option<SourceConfig>,
    /// Software bill of materials (SBOM) generation configurations.
    #[serde(default, deserialize_with = "deserialize_sboms")]
    #[schemars(schema_with = "sboms_schema")]
    pub sboms: Vec<SbomConfig>,
    /// SLSA build-provenance / attestation configuration for binaries and
    /// archives. In the default `subjects` mode, anodizer writes a subjects
    /// manifest for `actions/attest-build-provenance`; in `emit` mode it
    /// generates and signs a self-contained in-toto SLSA provenance statement.
    /// When omitted (or `enabled: false`), the attestation stage is a no-op.
    pub attestations: Option<AttestationConfig>,
    /// GitHub release configuration shared by all crates.
    pub release: Option<ReleaseConfig>,
    /// Custom GitHub API/upload/download URLs for GitHub Enterprise installations.
    pub github_urls: Option<GitHubUrlsConfig>,
    /// Custom GitLab API/download URLs for self-hosted GitLab installations.
    pub gitlab_urls: Option<GitLabUrlsConfig>,
    /// Custom Gitea API/download URLs for self-hosted Gitea installations.
    pub gitea_urls: Option<GiteaUrlsConfig>,
    /// Force a specific token type for authentication.
    /// When set, overrides automatic token detection from environment variables.
    pub force_token: Option<ForceTokenKind>,
    /// macOS code signing and notarization configuration.
    pub notarize: Option<NotarizeConfig>,
    /// Project metadata configuration (applied to metadata.json output files).
    pub metadata: Option<MetadataConfig>,
    /// Template files to render and include as release artifacts.
    /// File contents are processed through the template engine.
    pub template_files: Option<Vec<TemplateFileConfig>>,
    /// Monorepo configuration.
    /// When configured, tag discovery filters by tag_prefix and the working
    /// directory is scoped to dir.
    pub monorepo: Option<MonorepoConfig>,
    /// Makeself self-extracting archive configurations.
    #[serde(default, deserialize_with = "deserialize_makeselfs")]
    #[schemars(schema_with = "makeselfs_schema")]
    pub makeselfs: Vec<MakeselfConfig>,
    /// `curl | sh` installer-script configurations. Each entry emits a
    /// deterministic POSIX `install.sh` release asset that detects the host
    /// OS + arch, downloads and sha256-verifies the matching archive, and
    /// installs the binary.
    #[serde(default, deserialize_with = "deserialize_install_scripts")]
    #[schemars(schema_with = "install_scripts_schema")]
    pub install_scripts: Vec<InstallScriptConfig>,
    /// AppImage configurations. Each entry bundles a built Linux binary plus
    /// its desktop integration into a single self-contained `.AppImage` via
    /// linuxdeploy.
    #[serde(default, deserialize_with = "deserialize_appimages")]
    #[schemars(schema_with = "appimages_schema")]
    pub appimages: Vec<AppImageConfig>,
    /// Opt-in post-release verification gate. Runs LAST (after the release is
    /// created and every publisher has run) and REPORTS post-publish defects —
    /// missing assets, failed install smoke-tests, glibc-ceiling violations.
    /// Because it runs after the irreversible publish, a failure exits
    /// non-zero to flag CI but never undoes the release. Off unless
    /// `verify_release.enabled: true`.
    #[serde(default)]
    pub verify_release: VerifyReleaseConfig,
    /// Pre-publish preflight tuning. `preflight.strict: true` promotes
    /// indeterminate probe outcomes (5xx / rate-limit / network failure /
    /// undeterminable permissions) from warnings to hard blockers. The
    /// probes themselves always run read-only before any publisher mutates
    /// a registry; the default (lenient) behavior needs no config.
    #[serde(default)]
    pub preflight: PreflightConfig,
    /// Source RPM configuration. Renamed from `srpm:` (singular) for spelling
    /// parity with `Defaults.srpms` and the rest of the plural-name packaging
    /// fields. The `srpm:` spelling is still accepted via serde alias for
    /// back-compat.
    #[serde(alias = "srpm")]
    pub srpms: Option<SrpmConfig>,
    /// Milestone closing configurations.
    pub milestones: Option<Vec<MilestoneConfig>>,
    /// Generic HTTP upload configurations.
    pub uploads: Option<Vec<UploadConfig>>,
    /// AUR source package publishing configurations (source-only PKGBUILD, not -bin).
    pub aur_sources: Option<Vec<AurSourceConfig>>,
    /// Top-level retry configuration applied to network-bound operations
    /// (announcers, git providers, HTTP uploads, docker pipes). When omitted,
    /// `RetryConfig::default()` is used (10 attempts, 10s base, 5m cap —
    /// the project-level retry policy).
    pub retry: Option<RetryConfig>,
    /// MCP (Model Context Protocol) server registry publishing
    /// configuration. When `name` is empty (the default), the publisher is
    /// skipped. The `mcp:` publisher block.
    #[serde(default)]
    pub mcp: McpConfig,
    /// SchemaStore publisher. Registers the project's JSON Schema(s) on
    /// SchemaStore at release time. When `schemas` is empty (the default),
    /// the publisher is skipped. The `schemastore:` publisher block.
    #[serde(default)]
    pub schemastore: crate::config::publishers::SchemastoreConfig,
    /// NPM package registry publishing configurations. One entry per
    /// published package. In the default `optional-deps` mode anodizer emits
    /// npm's native per-platform packages (biome / git-cliff pattern); in
    /// `postinstall` mode it emits a download shim (the `npms:`
    /// parity).
    pub npms: Option<Vec<NpmConfig>>,
    /// GemFury (fury.io) deb/rpm/apk publishing configurations. Mirrors
    /// The `gemfury:` block. The legacy spelling
    /// `furies:` is accepted via serde alias; a one-time deprecation
    /// warning is emitted by [`warn_on_legacy_furies_alias`].
    #[serde(alias = "furies")]
    pub gemfury: Option<Vec<GemFuryConfig>>,
    /// PyPI publishing configurations. One entry per published project.
    /// Emits native `py3-none-<platform>` binary wheels from the built
    /// binaries (plus an optional `maturin sdist`) and uploads them via
    /// PyPI's legacy (twine-protocol) upload API. The `pypis:` block.
    pub pypis: Option<Vec<PypiConfig>>,
    /// homebrew-core formula-bump configurations. One entry per formula.
    /// Bumps an existing formula in `Homebrew/homebrew-core` (or a formula
    /// repository override) via the GitHub API and opens a pull request.
    /// The `homebrew_cores:` block.
    pub homebrew_cores: Option<Vec<HomebrewCoreConfig>>,
    /// Per-crate metadata derived from each crate's `Cargo.toml [package]`
    /// table (description / license / homepage / authors). Populated at
    /// config-load time by [`Config::populate_derived_metadata`], keyed by
    /// crate name. NOT a user-facing YAML field — it backs the
    /// crate-aware `meta_*_for` accessors so a plain Rust project gets its
    /// publisher metadata without repeating it in a top-level `metadata:`
    /// block. A hand-written `metadata:` field and per-publisher overrides
    /// still win.
    #[serde(skip)]
    #[schemars(skip)]
    pub derived_metadata: BTreeMap<String, MetadataConfig>,
}

/// Helper schema function for the signs field (accepts object or array).
fn signs_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let mut schema = generator.subschema_for::<Vec<SignConfig>>();
    schema.ensure_object().insert(
        "description".to_owned(),
        "Artifact signing configurations (cosign, GPG, etc.). Accepts a single object or array."
            .into(),
    );
    schema
}

/// Helper schema function for the upx field (accepts object or array).
fn upx_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let mut schema = generator.subschema_for::<Vec<UpxConfig>>();
    schema.ensure_object().insert(
        "description".to_owned(),
        "UPX binary compression configurations. Accepts a single object or array.".into(),
    );
    schema
}

/// Helper schema function for the sboms field (accepts object or array).
fn sboms_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let mut schema = generator.subschema_for::<Vec<SbomConfig>>();
    schema.ensure_object().insert(
        "description".to_owned(),
        "SBOM generation configurations. Accepts a single object or array.".into(),
    );
    schema
}

fn default_dist() -> PathBuf {
    PathBuf::from("./dist")
}

impl Default for Config {
    fn default() -> Self {
        Config {
            version: None,
            project_name: String::new(),
            dist: default_dist(),
            includes: None,
            env_files: None,
            defaults: None,
            before: None,
            after: None,
            on_error: None,
            before_publish: None,
            crates: Vec::new(),
            changelog: None,
            signs: Vec::new(),
            binary_signs: Vec::new(),
            docker_signs: None,
            upx: Vec::new(),
            snapshot: None,
            nightly: None,
            announce: None,
            report_sizes: None,
            env: None,
            variables: None,
            publishers: None,
            dockerhub: None,
            artifactories: None,
            cloudsmiths: None,
            homebrew_casks: None,
            version_files: None,
            tag: None,
            git: None,
            partial: None,
            workspaces: None,
            source: None,
            sboms: Vec::new(),
            attestations: None,
            release: None,
            github_urls: None,
            gitlab_urls: None,
            gitea_urls: None,
            force_token: None,
            notarize: None,
            metadata: None,
            template_files: None,
            monorepo: None,
            makeselfs: Vec::new(),
            install_scripts: Vec::new(),
            appimages: Vec::new(),
            verify_release: VerifyReleaseConfig::default(),
            preflight: PreflightConfig::default(),
            srpms: None,
            milestones: None,
            uploads: None,
            aur_sources: None,
            retry: None,
            mcp: McpConfig::default(),
            schemastore: crate::config::publishers::SchemastoreConfig::default(),
            npms: None,
            gemfury: None,
            pypis: None,
            homebrew_cores: None,
            derived_metadata: BTreeMap::new(),
        }
    }
}

mod accessors;

mod schema;
pub use schema::*;

/// Run a deserialization closure on a worker thread sized large enough that
/// the `Config` derive (60+ `Option<NestedStruct>` fields) cannot exhaust
/// the host's main-thread stack.
///
/// Background: debug builds of `serde_yaml_ng::from_value::<Config>` and
/// `toml::from_str::<Config>` consume several MiB of stack because each
/// generated visitor branch for the giant struct lives in a single
/// monomorphised frame and debug builds neither inline nor tail-call. The
/// Windows main-thread default reservation is 1 MiB, so any debug-built
/// integration test that triggers full-config deserialization overflows
/// before reaching the visitor's body.
///
/// Routing every full-`Config` deserialization through this helper keeps
/// every entry-point platform-agnostic without resorting to per-platform
/// linker flags or `RUST_MIN_STACK`.
pub fn deserialize_on_worker<F, T>(f: F) -> anyhow::Result<T>
where
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    use anyhow::Context as _;

    // 8 MiB matches the Linux/macOS process default and comfortably exceeds
    // the ~2 MiB peak observed for debug `Config` deserialization.
    const WORKER_STACK_SIZE: usize = 8 * 1024 * 1024;

    let handle = std::thread::Builder::new()
        .stack_size(WORKER_STACK_SIZE)
        .name("anodizer-config-deserialize".to_string())
        .spawn(f)
        .context("failed to spawn config deserialization worker thread")?;
    match handle.join() {
        Ok(result) => result,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

mod validate;
pub use validate::*;

mod publish_axis;
pub(crate) use publish_axis::*;

mod legacy;
pub use legacy::*;

// ---------------------------------------------------------------------------
// EnvFilesConfig — accepts list of .env paths OR structured token file paths
// ---------------------------------------------------------------------------

mod env_files;
pub use env_files::*;

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

mod defaults;
pub use defaults::*;

// ---------------------------------------------------------------------------
// BuildIgnore — exclude specific os/arch combos from builds
// ---------------------------------------------------------------------------

mod build;
pub use build::*;

// ---------------------------------------------------------------------------
// ArchivesConfig — untagged enum: false => Disabled, array => Configs
// ---------------------------------------------------------------------------

mod archives;
pub use archives::*;

mod completions;
pub use completions::*;

// ---------------------------------------------------------------------------
// ReleaseConfig
// ---------------------------------------------------------------------------

mod release;
pub use release::*;

// ---------------------------------------------------------------------------
// Shared publisher config types: RepositoryConfig, CommitAuthorConfig
// ---------------------------------------------------------------------------

mod publishers;
pub use publishers::*;

// ---------------------------------------------------------------------------
// DockerV2Config
// ---------------------------------------------------------------------------

mod docker;
pub use docker::*;

// ---------------------------------------------------------------------------
// NfpmConfig
// ---------------------------------------------------------------------------

mod nfpm;
pub use nfpm::*;

// ---------------------------------------------------------------------------
// SnapcraftConfig
// ---------------------------------------------------------------------------

mod snapcraft;
pub use snapcraft::*;
// ---------------------------------------------------------------------------
// DmgConfig / MsiConfig / PkgConfig / NsisConfig / AppBundleConfig / FlatpakConfig
// ---------------------------------------------------------------------------

mod installers;
pub use installers::*;

// ---------------------------------------------------------------------------
// BlobConfig (S3/GCS/Azure cloud storage)
// ---------------------------------------------------------------------------

mod blob;
pub use blob::*;

// ---------------------------------------------------------------------------
// PartialConfig (split/merge CI fan-out)
// ---------------------------------------------------------------------------

mod partial;
pub use partial::*;

// ---------------------------------------------------------------------------
// BinstallConfig
// ---------------------------------------------------------------------------

mod binstall;
pub use binstall::*;

// ---------------------------------------------------------------------------
// NotarizeConfig (macOS code signing and notarization)
// ---------------------------------------------------------------------------

mod notarize;
pub use notarize::*;
// ---------------------------------------------------------------------------
// SourceConfig
// ---------------------------------------------------------------------------

mod source;
pub use source::*;

// ---------------------------------------------------------------------------
// SbomConfig
// ---------------------------------------------------------------------------

mod sbom;
pub use sbom::*;

// ---------------------------------------------------------------------------
// AttestationConfig
// ---------------------------------------------------------------------------

mod attestation;
pub use attestation::*;

// ---------------------------------------------------------------------------
// VersionSyncConfig
// ---------------------------------------------------------------------------

mod version_sync;
pub use version_sync::*;

// ---------------------------------------------------------------------------
// ChangelogConfig
// ---------------------------------------------------------------------------

mod changelog;
pub use changelog::*;
// ---------------------------------------------------------------------------
// SignConfig / DockerSignConfig — lifted to `crate::signing`
// ---------------------------------------------------------------------------
//
// see `crate::signing` for the type definitions. The
// re-exports below preserve the historical
// `anodizer_core::config::{SignConfig, DockerSignConfig}` import paths
// used by every stage that consumes a sign config.

pub use crate::signing::{AuthenticodeConfig, DockerSignConfig, SignConfig, SignVerifyConfig};

// ---------------------------------------------------------------------------
// UpxConfig
// ---------------------------------------------------------------------------

mod upx;
pub use upx::*;

// ---------------------------------------------------------------------------
// SnapshotConfig
// ---------------------------------------------------------------------------

mod snapshot_nightly;
pub use snapshot_nightly::*;

mod cargo_metadata;
pub use cargo_metadata::derive_metadata_from_cargo_toml;

mod workspace_deps;
pub use workspace_deps::{
    derive_depends_on_from_cargo_toml, discover_cargo_workspace_member_names,
    extract_workspace_deps,
};

/// Extract the name portion of a `"Name <email>"` maintainer/author string,
/// dropping any `<…>` email suffix. Returns `None` when the result is empty
/// (e.g. a bare-email `<ada@example.com>`), so a derived Vendor / OCI `vendor`
/// value is never emitted blank.
pub fn maintainer_name_only(maintainer: &str) -> Option<String> {
    let name = maintainer.split('<').next().unwrap_or(maintainer).trim();
    (!name.is_empty()).then(|| name.to_string())
}

// ---------------------------------------------------------------------------
// TemplateFileConfig
// ---------------------------------------------------------------------------

mod templatefiles;
pub use templatefiles::*;

// ---------------------------------------------------------------------------
// AnnounceConfig
// ---------------------------------------------------------------------------
mod announce;
pub use announce::*;
// ---------------------------------------------------------------------------
// DockerHub description sync
// ---------------------------------------------------------------------------

mod dockerhub;
pub use dockerhub::*;

// ---------------------------------------------------------------------------
// Artifactory publisher
// ---------------------------------------------------------------------------

mod artifactory;
pub use artifactory::*;

// ---------------------------------------------------------------------------
// CloudSmith publisher
// ---------------------------------------------------------------------------

mod cloudsmith;
pub use cloudsmith::*;

// ---------------------------------------------------------------------------
// PublisherConfig
// ---------------------------------------------------------------------------

mod publisher;
pub use publisher::*;

// ---------------------------------------------------------------------------
// HooksConfig
// ---------------------------------------------------------------------------

mod hooks;
pub use hooks::*;

// ---------------------------------------------------------------------------
// GitConfig
// ---------------------------------------------------------------------------

mod git_config;
pub use git_config::*;

// ---------------------------------------------------------------------------
// MonorepoConfig
// ---------------------------------------------------------------------------

mod monorepo;
pub use monorepo::*;

// ---------------------------------------------------------------------------
// TagConfig
// ---------------------------------------------------------------------------

mod tag;
pub use tag::*;

// ---------------------------------------------------------------------------
// WorkspaceConfig
// ---------------------------------------------------------------------------

mod workspace;
pub use workspace::*;

// ---------------------------------------------------------------------------
// RetryConfig (top-level `retry:` block — bridges to crate::retry::RetryPolicy)
// ---------------------------------------------------------------------------

mod retry;
pub use retry::*;

// ---------------------------------------------------------------------------
// PostPublishPollConfig (per-publisher post-publish polling)
// ---------------------------------------------------------------------------

mod post_publish_poll;
pub use post_publish_poll::*;

// ---------------------------------------------------------------------------
// VerifyReleaseConfig (top-level `verify_release:` post-publish gate)
// ---------------------------------------------------------------------------

mod verify_release;
pub use verify_release::*;

// ---------------------------------------------------------------------------
// PreflightConfig (top-level `preflight:` pre-publish probe tuning)
// ---------------------------------------------------------------------------

mod preflight;
pub use preflight::*;

// ---------------------------------------------------------------------------
// StringOrBool — accepts bool or template string in YAML
// ---------------------------------------------------------------------------

mod string_or_bool;
pub use string_or_bool::*;

// ---------------------------------------------------------------------------
// MakeselfConfig + SrpmConfig — lifted to `crate::packagers`
// ---------------------------------------------------------------------------
//
// All packaging config types live in their own modules under
// `crate::packagers`. The re-exports below preserve the historical
// `anodizer_core::config::{MakeselfConfig, MakeselfFile, SrpmConfig}`
// import paths used by stages and tests.

pub use crate::packagers::{
    AppImageConfig, AppImageExtra, InstallScriptConfig, MakeselfConfig, MakeselfFile,
    RuntimeHarvest, SrpmConfig,
};
pub(crate) use crate::packagers::{
    appimages_schema, deserialize_appimages, deserialize_install_scripts, deserialize_makeselfs,
    install_scripts_schema, makeselfs_schema,
};

// ---------------------------------------------------------------------------
// MilestoneConfig
// ---------------------------------------------------------------------------

mod milestone;
pub use milestone::*;

// ---------------------------------------------------------------------------
// UploadConfig (generic HTTP upload)
// ---------------------------------------------------------------------------

mod upload;
pub use upload::*;

// ---------------------------------------------------------------------------
// AurSourceConfig
// ---------------------------------------------------------------------------

mod aur_source;
pub use aur_source::*;

// ---------------------------------------------------------------------------
// McpConfig (MCP registry publisher)
// ---------------------------------------------------------------------------

mod mcp;
pub use mcp::*;

// ---------------------------------------------------------------------------
// NpmConfig (NPM package registry publisher)
// ---------------------------------------------------------------------------

mod npm;
pub use npm::*;

// ---------------------------------------------------------------------------
// GemFuryConfig (Gemfury / fury.io publisher)
// ---------------------------------------------------------------------------

mod gemfury;
pub use gemfury::*;

// ---------------------------------------------------------------------------
// PypiConfig (PyPI binary-wheel publisher)
// ---------------------------------------------------------------------------

mod pypi;
pub use pypi::*;

// ---------------------------------------------------------------------------
// HomebrewCoreConfig (homebrew-core formula-bump publisher)
// ---------------------------------------------------------------------------

mod homebrew_core;
pub use homebrew_core::*;

// ---------------------------------------------------------------------------
// Well-known config file discovery
// ---------------------------------------------------------------------------

mod discovery;
pub use discovery::*;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
