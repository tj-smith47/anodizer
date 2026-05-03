use std::collections::HashMap;
use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

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
///       path: ./config/goreleaser.yaml           # structured file path
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
pub struct IncludeFilePath {
    /// Path to the include file (relative to the config file).
    pub path: String,
}

/// URL configuration for a structured include.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
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
/// fields at parse time, matching GoReleaser's `yaml.UnmarshalStrict`.
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
    /// List of `KEY=VALUE` strings (matches GoReleaser):
    /// `env: ["MY_VAR=hello", "DEPLOY_ENV=staging"]`. Order is preserved so
    /// chained env applications (sign + sbom + notarize) see entries in
    /// declared order. Values are rendered through the template engine before
    /// being set, so expressions like `{{ .Tag }}` or `{{ .Date }}` are
    /// expanded.
    #[serde(default)]
    pub env: Option<Vec<String>>,
    /// Custom template variables accessible as {{ .Var.key }} in templates.
    /// Provides a way to define reusable values, especially useful with config includes.
    pub variables: Option<HashMap<String, String>>,
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
    /// GoReleaser Pro monorepo configuration.
    /// When configured, tag discovery filters by tag_prefix and the working
    /// directory is scoped to dir.
    pub monorepo: Option<MonorepoConfig>,
    /// Makeself self-extracting archive configurations.
    #[serde(default, deserialize_with = "deserialize_makeselfs")]
    #[schemars(schema_with = "makeselfs_schema")]
    pub makeselfs: Vec<MakeselfConfig>,
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
}

/// Helper schema function for the signs field (accepts object or array).
fn signs_schema(generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
    let mut schema = generator.subschema_for::<Vec<SignConfig>>();
    if let schemars::schema::Schema::Object(ref mut obj) = schema {
        obj.metadata().description = Some("Artifact signing configurations (cosign, GPG, etc.). Accepts a single object or array.".to_owned());
    }
    schema
}

/// Helper schema function for the upx field (accepts object or array).
fn upx_schema(generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
    let mut schema = generator.subschema_for::<Vec<UpxConfig>>();
    if let schemars::schema::Schema::Object(ref mut obj) = schema {
        obj.metadata().description = Some(
            "UPX binary compression configurations. Accepts a single object or array.".to_owned(),
        );
    }
    schema
}

/// Helper schema function for the sboms field (accepts object or array).
fn sboms_schema(generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
    let mut schema = generator.subschema_for::<Vec<SbomConfig>>();
    if let schemars::schema::Schema::Object(ref mut obj) = schema {
        obj.metadata().description =
            Some("SBOM generation configurations. Accepts a single object or array.".to_owned());
    }
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
            tag: None,
            git: None,
            partial: None,
            workspaces: None,
            source: None,
            sboms: Vec::new(),
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
            srpms: None,
            milestones: None,
            uploads: None,
            aur_sources: None,
        }
    }
}

impl Config {
    /// Return the monorepo tag prefix, if configured.
    ///
    /// Shorthand for `config.monorepo.as_ref().and_then(|m| m.tag_prefix.as_deref())`.
    pub fn monorepo_tag_prefix(&self) -> Option<&str> {
        self.monorepo.as_ref().and_then(|m| m.tag_prefix.as_deref())
    }

    /// Return the monorepo working directory, if configured.
    ///
    /// Shorthand for `config.monorepo.as_ref().and_then(|m| m.dir.as_deref())`.
    pub fn monorepo_dir(&self) -> Option<&str> {
        self.monorepo.as_ref().and_then(|m| m.dir.as_deref())
    }

    // --- Project metadata defaulting helpers (GoReleaser Pro parity) ---
    //
    // Publishers that expose homepage/license/description/maintainer fields
    // should fall back to these when their own field is unset, so a project
    // only needs to declare metadata once. Pattern:
    //
    //   let homepage = nfpm_cfg.homepage
    //       .as_deref()
    //       .or_else(|| cfg.meta_homepage());
    //
    // Returns None if the `metadata` section is missing or the field is unset.

    /// Project homepage from `metadata.homepage` (Pro default source for publishers).
    pub fn meta_homepage(&self) -> Option<&str> {
        self.metadata.as_ref().and_then(|m| m.homepage.as_deref())
    }

    /// Project license from `metadata.license`.
    pub fn meta_license(&self) -> Option<&str> {
        self.metadata.as_ref().and_then(|m| m.license.as_deref())
    }

    /// Project description from `metadata.description`.
    pub fn meta_description(&self) -> Option<&str> {
        self.metadata
            .as_ref()
            .and_then(|m| m.description.as_deref())
    }

    /// Project maintainers from `metadata.maintainers`.
    pub fn meta_maintainers(&self) -> &[String] {
        self.metadata
            .as_ref()
            .and_then(|m| m.maintainers.as_deref())
            .unwrap_or(&[])
    }

    /// First maintainer as "Name <email>" or just "Name" (publisher convention).
    /// Returns None when no maintainers are configured.
    pub fn meta_first_maintainer(&self) -> Option<&str> {
        self.meta_maintainers().first().map(|s| s.as_str())
    }
}

/// Validate the config schema version. Accepts version 1 (default) and 2.
/// Returns an error for unknown versions.
pub fn validate_version(config: &Config) -> Result<(), String> {
    match config.version {
        None | Some(1) | Some(2) => Ok(()),
        Some(v) => Err(format!(
            "unsupported config version: {}. Supported versions are 1 and 2.",
            v
        )),
    }
}

/// Validate `git.tag_sort` if present. Accepted values:
/// - `"-version:refname"` (default, lexicographic version sort)
/// - `"-version:creatordate"` (sort by tag creation date, newest first)
///
/// Returns an error for unrecognized values.
pub fn validate_tag_sort(config: &Config) -> Result<(), String> {
    if let Some(ref git) = config.git
        && let Some(ref sort) = git.tag_sort
    {
        match sort.as_str() {
            "-version:refname" | "-version:creatordate" => {}
            other => {
                return Err(format!(
                    "unsupported git.tag_sort value: \"{}\". \
                     Accepted values: \"-version:refname\", \"-version:creatordate\".",
                    other
                ));
            }
        }
    }
    Ok(())
}

/// Known GOOS values accepted by `archives[].format_overrides[].goos`.
/// Mirrors the Go runtime's `runtime.GOOS` values GoReleaser's archive pipe
/// recognises; anything outside this set is almost always a typo
/// (e.g. a Rust target triple slice like `pc-windows-msvc`).
const KNOWN_GOOS: &[&str] = &[
    "aix",
    "android",
    "darwin",
    "dragonfly",
    "freebsd",
    "illumos",
    "ios",
    "js",
    "linux",
    "netbsd",
    "openbsd",
    "plan9",
    "solaris",
    "wasip1",
    "windows",
];

/// Validate that each crate's `release:` block configures at most one SCM
/// backend. Matches GoReleaser release.go:41-53 `ErrMultipleReleases`, which
/// errors at `Default()` time. Anodizer dispatches on `ctx.token_type` at
/// runtime so a silently-ignored extra backend is easy to miss.
pub fn validate_release_backends(config: &Config) -> Result<(), String> {
    let check = |crate_name: &str, release: &ReleaseConfig| -> Result<(), String> {
        let mut set = Vec::new();
        if release.github.is_some() {
            set.push("github");
        }
        if release.gitlab.is_some() {
            set.push("gitlab");
        }
        if release.gitea.is_some() {
            set.push("gitea");
        }
        if set.len() > 1 {
            return Err(format!(
                "crate {}: release config sets multiple mutually-exclusive SCM \
                 backends ({}). Pick one.",
                crate_name,
                set.join(" + ")
            ));
        }
        Ok(())
    };
    for krate in &config.crates {
        if let Some(ref release) = krate.release {
            check(&krate.name, release)?;
        }
    }
    if let Some(ws_list) = config.workspaces.as_ref() {
        for ws in ws_list {
            for krate in &ws.crates {
                if let Some(ref release) = krate.release {
                    check(&krate.name, release)?;
                }
            }
        }
    }
    Ok(())
}

/// Marker prefix for the axis-mismatch validation error class. Existing
/// validators in this module return `Result<(), String>` rather than a
/// typed enum, so we expose this constant (instead of a `ConfigError`
/// variant) for callers that want to recognise the error class
/// programmatically.
///
/// The prefix is emitted at the start of every error returned by
/// [`validate_defaults_axis`] (formatted as `"DefaultsAxisMismatch: …"`),
/// so callers can match with `err.starts_with(ERR_DEFAULTS_AXIS_MISMATCH)`
/// or `err.contains(ERR_DEFAULTS_AXIS_MISMATCH)` without depending on the
/// exact human-readable wording.
///
/// ```ignore
/// match validate_defaults_axis(&config) {
///     Err(e) if e.starts_with(ERR_DEFAULTS_AXIS_MISMATCH) => {
///         // handle the axis-mismatch error class
///     }
///     other => other?,
/// }
/// ```
///
/// Future error-type unification can rename to
/// `ConfigError::DefaultsAxisMismatch` without changing call-sites that
/// match on this prefix.
pub const ERR_DEFAULTS_AXIS_MISMATCH: &str = "DefaultsAxisMismatch";

/// Validate that `defaults.crates:` and `defaults.workspaces:` match the
/// top-level axis (DEC-4).
///
/// Rules:
/// - `defaults.crates:` is set → top-level `crates:` MUST be present.
/// - `defaults.workspaces:` is set → top-level `workspaces:` MUST be present.
/// - Both `defaults.crates` and `defaults.workspaces` set simultaneously → error
///   (mutually exclusive).
/// - Wrong-axis (e.g. `defaults.crates:` while top-level uses `workspaces:`) → error.
pub fn validate_defaults_axis(config: &Config) -> Result<(), String> {
    let Some(ref defaults) = config.defaults else {
        return Ok(());
    };
    let has_crate_block = defaults.crates.is_some();
    let has_workspace_block = defaults.workspaces.is_some();

    if has_crate_block && has_workspace_block {
        return Err(format!(
            "{ERR_DEFAULTS_AXIS_MISMATCH}: defaults.crates and defaults.workspaces are \
             mutually exclusive — pick the axis that matches the top-level config \
             (`crates:` or `workspaces:`)",
        ));
    }

    let top_uses_workspaces = config.workspaces.as_ref().is_some_and(|w| !w.is_empty());
    let top_uses_crates = !config.crates.is_empty();

    if has_crate_block && !top_uses_crates {
        return Err(format!(
            "{ERR_DEFAULTS_AXIS_MISMATCH}: defaults.crates is set but top-level `crates:` \
             is {}; move defaults under `defaults.workspaces:` or remove the block",
            if top_uses_workspaces {
                "absent (top-level uses `workspaces:`)"
            } else {
                "absent"
            },
        ));
    }
    if has_workspace_block && !top_uses_workspaces {
        return Err(format!(
            "{ERR_DEFAULTS_AXIS_MISMATCH}: defaults.workspaces is set but top-level \
             `workspaces:` is {}; move defaults under `defaults.crates:` or remove the block",
            if top_uses_crates {
                "absent (top-level uses `crates:`)"
            } else {
                "absent"
            },
        ));
    }

    Ok(())
}

/// Validate `archives[].format_overrides[].goos` values reject unknown OSes.
/// GoReleaser silently no-ops unknown overrides, which has burned users typing
/// Rust triples like `apple` or `pc-windows-msvc`.
///
/// Walks every `archives[]` location in the config:
/// - `crates[].archives:`
/// - `workspaces[].crates[].archives:`
/// - `defaults.archives:` (an unknown `os` here would otherwise pass silently
///   and propagate to every inheriting crate at merge time).
pub fn validate_format_overrides(config: &Config) -> Result<(), String> {
    let check = |location: &str, archives: &[ArchiveConfig]| -> Result<(), String> {
        for (idx, archive) in archives.iter().enumerate() {
            let Some(ref overrides) = archive.format_overrides else {
                continue;
            };
            for over in overrides {
                if !KNOWN_GOOS.contains(&over.os.as_str()) {
                    let archive_id = archive.id.as_deref().unwrap_or("default");
                    return Err(format!(
                        "{}: archives[{}] (id={}): format_overrides.goos=\"{}\" is not a recognised OS. \
                         Accepted values: {}.",
                        location,
                        idx,
                        archive_id,
                        over.os,
                        KNOWN_GOOS.join(", ")
                    ));
                }
            }
        }
        Ok(())
    };
    for krate in &config.crates {
        if let ArchivesConfig::Configs(ref list) = krate.archives {
            check(&format!("crate {}", krate.name), list)?;
        }
    }
    if let Some(ws_list) = config.workspaces.as_ref() {
        for ws in ws_list {
            for krate in &ws.crates {
                if let ArchivesConfig::Configs(ref list) = krate.archives {
                    check(&format!("crate {}", krate.name), list)?;
                }
            }
        }
    }
    if let Some(ref defaults) = config.defaults
        && let Some(ref archive) = defaults.archives
    {
        // defaults.archives is a single ArchiveConfig (not a list); wrap it
        // into a one-element slice so the same checker walks it.
        check("defaults.archives", std::slice::from_ref(archive))?;
    }
    Ok(())
}

/// Validate that no [`HomebrewCaskConfig`] sets both `url_template` AND
/// `url.template` simultaneously — they are mutually exclusive shorthands
/// for the same URL field and combining them is ambiguous.
///
/// Inspects every occurrence of `HomebrewCaskConfig` in the config:
/// - `homebrew_casks:` (top-level array)
/// - `crates[].publish.homebrew_cask:`
/// - `workspaces[].crates[].publish.homebrew_cask:`
/// - `defaults.publish.homebrew_cask:`
pub fn validate_homebrew_cask_url_template(config: &Config) -> Result<(), String> {
    let check = |location: &str, cask: &HomebrewCaskConfig| -> Result<(), String> {
        let has_url_template = cask.url_template.is_some();
        let has_url_dot_template = cask.url.as_ref().is_some_and(|u| u.template.is_some());
        if has_url_template && has_url_dot_template {
            return Err(format!(
                "{location}: homebrew_cask sets both `url_template` and `url.template`. \
                 These are mutually exclusive — use one or the other."
            ));
        }
        Ok(())
    };

    // Top-level homebrew_casks array
    if let Some(ref casks) = config.homebrew_casks {
        for (i, cask) in casks.iter().enumerate() {
            check(&format!("homebrew_casks[{i}]"), cask)?;
        }
    }

    // Per-crate publish.homebrew_cask
    for krate in &config.crates {
        if let Some(ref publish) = krate.publish
            && let Some(ref cask) = publish.homebrew_cask
        {
            check(
                &format!("crates[{}].publish.homebrew_cask", krate.name),
                cask,
            )?;
        }
    }

    // Workspace crates
    if let Some(ref workspaces) = config.workspaces {
        for ws in workspaces {
            for krate in &ws.crates {
                if let Some(ref publish) = krate.publish
                    && let Some(ref cask) = publish.homebrew_cask
                {
                    check(
                        &format!(
                            "workspaces[{}].crates[{}].publish.homebrew_cask",
                            ws.name, krate.name
                        ),
                        cask,
                    )?;
                }
            }
        }
    }

    // defaults.publish.homebrew_cask
    if let Some(ref defaults) = config.defaults
        && let Some(ref publish) = defaults.publish
        && let Some(ref cask) = publish.homebrew_cask
    {
        check("defaults.publish.homebrew_cask", cask)?;
    }

    Ok(())
}

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
// KrewConfig + NixConfig — lifted to `crate::publishers`
// ---------------------------------------------------------------------------
//
// WAVE 5 split: see `crate::publishers` for the type definitions. The
// re-exports below preserve the historical
// `anodizer_core::config::{KrewConfig, NixConfig, NixDependency}`
// import paths used by `stage-publish/krew.rs` / `stage-publish/nix.rs`.
// Remaining publisher configs (`HomebrewConfig`, `ScoopConfig`,
// `ChocolateyConfig`, `WingetConfig`, `AurConfig`, `AurSourceConfig`,
// `CargoPublishConfig`, `DockerV2Config`, `DockerDigestConfig`, ...)
// still live in this file pending future split passes.

pub use crate::publishers::{KrewConfig, NixConfig, NixDependency};

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
// WAVE 5 split: see `crate::signing` for the type definitions. The
// re-exports below preserve the historical
// `anodizer_core::config::{SignConfig, DockerSignConfig}` import paths
// used by every stage that consumes a sign config.

pub use crate::signing::{DockerSignConfig, SignConfig};

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

/// DockerHub description sync configuration.
/// Pushes image descriptions and README content to DockerHub repositories.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DockerHubConfig {
    /// DockerHub username for authentication.
    pub username: Option<String>,
    /// Environment variable name containing the DockerHub token.
    pub secret_name: Option<String>,
    /// DockerHub image names to update (e.g. `myorg/myapp`).
    pub images: Option<Vec<String>>,
    /// Short description for the DockerHub repository (max 100 chars).
    pub description: Option<String>,
    /// Full description (README) source for the DockerHub repository.
    pub full_description: Option<DockerHubFullDescription>,
    /// Skip this publisher. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
}

/// Full description source for DockerHub: either from a URL or a local file.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DockerHubFullDescription {
    /// Fetch full description content from a URL.
    pub from_url: Option<DockerHubFromUrl>,
    /// Read full description content from a local file.
    pub from_file: Option<DockerHubFromFile>,
}

/// Fetch DockerHub full description content from a URL.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DockerHubFromUrl {
    /// URL to fetch the full description from.
    pub url: String,
    /// Optional HTTP headers for the request.
    pub headers: Option<HashMap<String, String>>,
}

/// Read DockerHub full description content from a local file.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DockerHubFromFile {
    /// Path to the file containing the full description.
    pub path: String,
}

// ---------------------------------------------------------------------------
// Artifactory publisher
// ---------------------------------------------------------------------------

/// Artifactory upload configuration.
/// Uploads artifacts to JFrog Artifactory repositories.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ArtifactoryConfig {
    /// Human-readable name for this publisher (used in logs).
    pub name: Option<String>,
    /// Target URL template for uploads (supports template variables).
    pub target: Option<String>,
    /// Upload mode: "archive" (upload archives) or "binary" (upload binaries).
    pub mode: Option<String>,
    /// Artifactory username for authentication.
    pub username: Option<String>,
    /// Artifactory password or API key (or env var reference).
    pub password: Option<String>,
    /// Build IDs filter: only upload artifacts from builds whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// File extension filter: only upload artifacts matching these extensions.
    pub exts: Option<Vec<String>>,
    /// Path to client X.509 certificate for mTLS authentication.
    pub client_x509_cert: Option<String>,
    /// Path to client X.509 private key for mTLS authentication.
    pub client_x509_key: Option<String>,
    /// Custom HTTP headers sent with each upload request.
    pub custom_headers: Option<HashMap<String, String>>,
    /// Header name used for checksum verification (e.g. `X-Checksum-Sha256`).
    pub checksum_header: Option<String>,
    /// Extra files to upload alongside build artifacts.
    pub extra_files: Option<Vec<ExtraFileSpec>>,
    /// Include checksums in uploaded artifacts.
    pub checksum: Option<bool>,
    /// Include signatures in uploaded artifacts.
    pub signature: Option<bool>,
    /// Include metadata artifacts in uploaded artifacts.
    pub meta: Option<bool>,
    /// Use custom artifact naming instead of default.
    pub custom_artifact_name: Option<bool>,
    /// When true, upload only extra_files (skip normal artifacts).
    pub extra_files_only: Option<bool>,
    /// HTTP method to use for uploads (default: "PUT").
    pub method: Option<String>,
    /// PEM-encoded trusted CA certificates for TLS verification.
    /// Appended to the system certificate pool.
    pub trusted_certificates: Option<String>,
    /// Template-conditional skip: if rendered result is `"true"`, skip this publisher.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
}

// ---------------------------------------------------------------------------
// CloudSmith publisher
// ---------------------------------------------------------------------------

/// CloudSmith publisher configuration.
/// Pushes packages to CloudSmith repositories.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct CloudSmithConfig {
    /// CloudSmith organization slug.
    pub organization: Option<String>,
    /// CloudSmith repository slug.
    pub repository: Option<String>,
    /// Build IDs filter: only publish artifacts from builds whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Package format filter: only publish artifacts matching these formats.
    pub formats: Option<Vec<String>>,
    /// Distribution mapping per format (e.g. `deb: "ubuntu/focal"`).
    pub distributions: Option<HashMap<String, serde_json::Value>>,
    /// Debian component name (e.g. "main").
    pub component: Option<String>,
    /// Environment variable name containing the CloudSmith API key.
    pub secret_name: Option<String>,
    /// Template-conditional skip: if rendered result is `"true"`, skip this publisher.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// When true, allow republishing over existing package versions.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub republish: Option<StringOrBool>,
}

// ---------------------------------------------------------------------------
// PublisherConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct PublisherConfig {
    /// Human-readable name for this publisher (used in logs).
    pub name: Option<String>,
    /// Command to invoke for publishing.
    pub cmd: String,
    /// Arguments passed to the publish command (supports templates).
    pub args: Option<Vec<String>>,
    /// Build IDs filter: only publish artifacts from builds whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Artifact type filter: only publish artifacts of these types (e.g., "archive", "binary").
    pub artifact_types: Option<Vec<String>>,
    /// Environment variables passed to the publish command.
    #[serde(default)]
    pub env: Option<Vec<String>>,
    /// Working directory for the publisher command.
    pub dir: Option<String>,
    /// Template-conditional skip: if rendered result is `"true"`, skip this publisher.
    /// Accepts bool or template string (e.g. `"{{ if .IsSnapshot }}true{{ endif }}"`).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Include checksums in published artifacts.
    pub checksum: Option<bool>,
    /// Include signatures in published artifacts.
    pub signature: Option<bool>,
    /// Include metadata artifacts in published artifacts.
    pub meta: Option<bool>,
    /// Extra files to include in publishing (glob patterns with optional name override).
    pub extra_files: Option<Vec<ExtraFileSpec>>,
    /// Extra files whose contents are rendered through the template engine before publishing.
    /// Unlike `extra_files` which copy as-is, template variables like `{{ .Tag }}` are expanded.
    /// GoReleaser Pro feature.
    pub templated_extra_files: Option<Vec<TemplatedExtraFile>>,
}

// ---------------------------------------------------------------------------
// HooksConfig
// ---------------------------------------------------------------------------

/// Top-level lifecycle hooks for `before` and `after` blocks.
/// Each block has `pre` and `post` lists of hook commands that run around the
/// entire pipeline (not individual stages).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct HooksConfig {
    /// Commands to run before the pipeline or stage starts. Matches GoReleaser
    /// `before.hooks` canonically.
    pub hooks: Option<Vec<HookEntry>>,
    /// Commands to run after the pipeline or stage completes. Anodizer extension
    /// (GoReleaser has no top-level `after:` block).
    pub post: Option<Vec<HookEntry>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct StructuredHook {
    /// Command to run (passed through the shell).
    pub cmd: String,
    /// Working directory for the command (defaults to project root).
    pub dir: Option<String>,
    /// Environment variables for the command.
    #[serde(default)]
    pub env: Option<Vec<String>>,
    /// When true, capture and log stdout/stderr of the command.
    pub output: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum HookEntry {
    Simple(String),
    Structured(StructuredHook),
}

impl PartialEq<&str> for HookEntry {
    fn eq(&self, other: &&str) -> bool {
        match self {
            HookEntry::Simple(s) => s.as_str() == *other,
            HookEntry::Structured(h) => h.cmd.as_str() == *other,
        }
    }
}

impl<'de> Deserialize<'de> for HookEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match &value {
            serde_json::Value::String(s) => Ok(HookEntry::Simple(s.clone())),
            serde_json::Value::Object(_) => {
                let hook: StructuredHook =
                    serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(HookEntry::Structured(hook))
            }
            _ => Err(serde::de::Error::custom(
                "hook entry must be a string or an object with cmd/dir/env/output",
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// GitConfig
// ---------------------------------------------------------------------------

/// Git-level tag discovery and sorting settings.
///
/// Controls how anodizer discovers and orders tags when determining the current
/// and previous versions. This is separate from `TagConfig`, which controls
/// version *bumping* logic.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct GitConfig {
    /// How to sort git tags when determining the latest version.
    ///
    /// Accepted values:
    /// - `"-version:refname"` (default) — lexicographic version sort on the tag name.
    /// - `"-version:creatordate"` — sort by the tag's creation date (newest first).
    pub tag_sort: Option<String>,
    /// Tag patterns to ignore during version detection (supports templates).
    /// Tags matching any pattern in this list are excluded from version
    /// detection entirely.
    pub ignore_tags: Option<Vec<String>>,
    /// Tag prefixes to ignore during version detection (supports templates).
    /// Tags starting with any prefix in this list are excluded.
    /// Mirrors GoReleaser Pro's ignore_tag_prefixes feature.
    pub ignore_tag_prefixes: Option<Vec<String>>,
    /// Suffix that identifies pre-release tags for sorting purposes.
    /// When set, tags ending with this suffix are treated as pre-releases
    /// and sorted accordingly during tag discovery.
    pub prerelease_suffix: Option<String>,
}

// ---------------------------------------------------------------------------
// MonorepoConfig
// ---------------------------------------------------------------------------

/// GoReleaser Pro monorepo configuration.
///
/// When configured, tag discovery filters by `tag_prefix` and the working
/// directory is scoped to `dir`.
///
/// This is DIFFERENT from `TagConfig.tag_prefix`:
/// - `MonorepoConfig.tag_prefix`: tags in git already HAVE the prefix
///   (e.g. `subproject1/v1.2.3`). The prefix is STRIPPED for `{{ .Tag }}`
///   while `{{ .PrefixedTag }}` retains the full tag.
/// - `TagConfig.tag_prefix`: a prefix to PREPEND when constructing
///   `{{ .PrefixedTag }}` from a plain tag.
///
/// When `monorepo` is configured, it takes precedence over `tag.tag_prefix`
/// for `PrefixedTag` / `PrefixedPreviousTag` behavior.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MonorepoConfig {
    /// Tag prefix for this subproject (e.g. `"subproject1/"`).
    ///
    /// Tags matching this prefix are selected during tag discovery, and the
    /// prefix is stripped from `{{ .Tag }}` while `{{ .PrefixedTag }}` retains
    /// the full tag.
    pub tag_prefix: Option<String>,
    /// Working directory for this subproject.
    ///
    /// Used for changelog path filtering (when no explicit `changelog.paths`
    /// or `crate.path` is configured) and as the default build `dir`.
    pub dir: Option<String>,
}

// ---------------------------------------------------------------------------
// TagConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct TagConfig {
    /// Default version bump type when no conventional commit token is found: "major", "minor", "patch", or "none".
    pub default_bump: Option<String>,
    /// Prefix prepended to version tags (e.g., "v" produces "v1.2.3").
    pub tag_prefix: Option<String>,
    /// Branch name patterns (supports wildcards) that trigger releases (default: ["master", "main"]).
    pub release_branches: Option<Vec<String>>,
    /// Custom version tag to use instead of auto-incrementing.
    pub custom_tag: Option<String>,
    /// Source for determining the previous tag: "repo" (default) or "branch".
    pub tag_context: Option<String>,
    /// Branch history mode for determining the previous tag: "full" or "last".
    pub branch_history: Option<String>,
    /// Version string to use when no previous tag exists (default: "0.1.0").
    pub initial_version: Option<String>,
    /// When true, apply a pre-release suffix to the generated version.
    pub prerelease: Option<bool>,
    /// Suffix appended to pre-release versions (e.g., "beta").
    pub prerelease_suffix: Option<String>,
    /// When true, create a new tag even if no commits have changed since the last tag.
    pub force_without_changes: Option<bool>,
    /// Like force_without_changes but only for pre-release versions.
    pub force_without_changes_pre: Option<bool>,
    /// Conventional commit token triggering a major bump (default: "major").
    pub major_string_token: Option<String>,
    /// Conventional commit token triggering a minor bump (default: "minor" or "feat").
    pub minor_string_token: Option<String>,
    /// Conventional commit token triggering a patch bump (default: "patch" or "fix").
    pub patch_string_token: Option<String>,
    /// Conventional commit token suppressing a version bump entirely (default: "none").
    pub none_string_token: Option<String>,
    /// When true, use the GitHub/GitLab API for tagging instead of git CLI.
    pub git_api_tagging: Option<bool>,
    /// When true, print verbose tag calculation output.
    pub verbose: Option<bool>,
    /// Commands to run before `anodizer tag` creates the tag. Useful for updating
    /// lockfiles or committing sibling changes that must be part of the tagged
    /// commit. Env: `ANODIZER_CURRENT_TAG`, `ANODIZER_PREVIOUS_TAG` are set;
    /// template vars `{{ .Tag }}`, `{{ .PreviousTag }}`, `{{ .Version }}`,
    /// `{{ .PrefixedTag }}` are available.
    pub tag_pre_hooks: Option<Vec<HookEntry>>,
    /// Commands to run after `anodizer tag` successfully creates and pushes the
    /// tag. Env and template vars same as `tag_pre_hooks`.
    pub tag_post_hooks: Option<Vec<HookEntry>>,
}

// ---------------------------------------------------------------------------
// WorkspaceConfig
// ---------------------------------------------------------------------------

/// A workspace represents an independent project root within a monorepo.
/// Each workspace has its own crates, changelog, and release configuration,
/// allowing independently-versioned components that aren't Cargo workspace members.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct WorkspaceConfig {
    /// Workspace identifier used in logs and template variables.
    pub name: String,
    /// Crates belonging to this workspace.
    pub crates: Vec<CrateConfig>,
    /// Changelog configuration for this workspace.
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
    /// Hooks run before this workspace's pipeline starts.
    pub before: Option<HooksConfig>,
    /// Hooks run after this workspace's pipeline completes.
    pub after: Option<HooksConfig>,
    /// Environment variables scoped to this workspace.
    ///
    /// List of `KEY=VALUE` strings (GoReleaser parity). Order is preserved.
    /// Values are template-rendered at pipeline startup.
    #[serde(default)]
    pub env: Option<Vec<String>>,
    /// Pipeline stages to skip when releasing this workspace.
    /// Stage names match the CLI `--skip` flag (e.g., `announce`, `publish`).
    #[serde(default)]
    pub skip: Vec<String>,
}

// ---------------------------------------------------------------------------
// StringOrBool — accepts bool or template string in YAML
// ---------------------------------------------------------------------------

mod string_or_bool;
pub use string_or_bool::*;
// ---------------------------------------------------------------------------
// MakeselfConfig + SrpmConfig — lifted to `crate::packagers`
// ---------------------------------------------------------------------------
//
// WAVE 5 split: see `crate::packagers` for the type definitions and
// associated `deserialize_makeselfs` / `makeselfs_schema` helpers. The
// re-exports below preserve the historical
// `anodizer_core::config::{MakeselfConfig, MakeselfFile, SrpmConfig}`
// import paths used by stages and tests. The remaining packaging types
// (`NfpmConfig`, `SnapcraftConfig`, `FlatpakConfig`, `AppBundleConfig`,
// `DmgConfig`, `PkgConfig`, `MsiConfig`, `NsisConfig`) still live in
// this file pending future split passes.

pub use crate::packagers::{MakeselfConfig, MakeselfFile, SrpmConfig};
pub(crate) use crate::packagers::{deserialize_makeselfs, makeselfs_schema};

// ---------------------------------------------------------------------------
// MilestoneConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct MilestoneConfig {
    /// Repository owner/name. Auto-detected from git remote if not set.
    pub repo: Option<ScmRepoConfig>,
    /// Close the milestone on release. Default: false.
    pub close: Option<bool>,
    /// Fail the pipeline if milestone close fails. Default: false.
    pub fail_on_error: Option<bool>,
    /// Milestone name template (default: "{{ .Tag }}").
    pub name_template: Option<String>,
}

impl MilestoneConfig {
    /// Default milestone name template. Mirrors GoReleaser
    /// `internal/pipe/milestone/milestone.go` (`cfg.NameTemplate = "{{.Tag}}"`).
    /// Anodize uses Tera-style `{{ Tag }}`; the rendered value is
    /// identical for any tag the project produces.
    pub const DEFAULT_NAME_TEMPLATE: &'static str = "{{ Tag }}";

    /// Resolve the milestone name template, falling back to
    /// [`Self::DEFAULT_NAME_TEMPLATE`].
    pub fn resolved_name_template(&self) -> &str {
        self.name_template
            .as_deref()
            .unwrap_or(Self::DEFAULT_NAME_TEMPLATE)
    }

    /// Resolve `close`, falling back to `false` (don't close milestones
    /// on release by default).
    pub fn resolved_close(&self) -> bool {
        self.close.unwrap_or(false)
    }

    /// Resolve `fail_on_error`, falling back to `false` (milestone close
    /// errors are warnings by default; opt in to fail-the-build).
    pub fn resolved_fail_on_error(&self) -> bool {
        self.fail_on_error.unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------
// UploadConfig (generic HTTP upload)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct UploadConfig {
    /// Human-readable name for this upload config.
    pub name: Option<String>,
    /// Build IDs filter: only upload artifacts whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// File extension filter: only upload artifacts with these extensions.
    pub exts: Option<Vec<String>>,
    /// Target URL template (supports template variables like {{ .ProjectName }}, {{ .Version }}).
    pub target: String,
    /// Username for HTTP basic auth.
    /// Resolution order: rendered `username` template → env `UPLOAD_{NAME}_USERNAME`.
    /// Set this to a literal value or a `{{ .Env.X }}` template.
    pub username: Option<String>,
    /// Password for HTTP basic auth (env var template strongly recommended;
    /// in-config plaintext leaves the value in `dist/config.yaml` after dry-run).
    /// Resolution order: rendered `password` template → env `UPLOAD_{NAME}_SECRET`.
    /// Mirrors GoReleaser's `Upload.Password` cascade (added in upstream v2.12).
    pub password: Option<String>,
    /// HTTP method: PUT or POST (default: PUT).
    pub method: Option<String>,
    /// Upload mode: "archive" (default) or "binary".
    pub mode: Option<String>,
    /// Header name for the SHA256 checksum of the artifact.
    pub checksum_header: Option<String>,
    /// Path to PEM-encoded trusted CA certificates.
    pub trusted_certificates: Option<String>,
    /// Path to PEM-encoded client X.509 certificate for mTLS.
    pub client_x509_cert: Option<String>,
    /// Path to PEM-encoded client X.509 key for mTLS.
    pub client_x509_key: Option<String>,
    /// Include checksums in uploaded artifacts.
    pub checksum: Option<bool>,
    /// Include signatures in uploaded artifacts.
    pub signature: Option<bool>,
    /// Include metadata artifacts in uploaded artifacts.
    pub meta: Option<bool>,
    /// Custom HTTP headers (each value is template-expanded).
    pub custom_headers: Option<HashMap<String, String>>,
    /// When true, use the artifact name as-is (don't append to target URL).
    pub custom_artifact_name: Option<bool>,
    /// Extra files to include in uploading.
    pub extra_files: Option<Vec<ExtraFileSpec>>,
    /// Upload only extra files, skip normal artifacts.
    pub extra_files_only: Option<bool>,
    /// Skip condition template (if rendered to "true", skip this upload).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
}

// ---------------------------------------------------------------------------
// AurSourceConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AurSourceConfig {
    /// Override the package name (default: crate name, no -bin suffix).
    pub name: Option<String>,
    /// Build IDs filter.
    pub ids: Option<Vec<String>>,
    /// Commit author with optional signing.
    pub commit_author: Option<CommitAuthorConfig>,
    /// Custom commit message template.
    pub commit_msg_template: Option<String>,
    /// Short description of the package.
    pub description: Option<String>,
    /// Project homepage URL.
    pub homepage: Option<String>,
    /// SPDX license identifier.
    pub license: Option<String>,
    /// Skip publishing. `"true"` always skips; `"auto"` skips for prereleases.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip_upload: Option<StringOrBool>,
    /// Custom URL template for download URLs.
    pub url_template: Option<String>,
    /// PKGBUILD maintainer entries.
    pub maintainers: Option<Vec<String>>,
    /// Contributors listed in PKGBUILD comments.
    pub contributors: Option<Vec<String>>,
    /// Packages this PKGBUILD provides.
    pub provides: Option<Vec<String>>,
    /// Packages this PKGBUILD conflicts with.
    pub conflicts: Option<Vec<String>>,
    /// Runtime dependencies.
    pub depends: Option<Vec<String>>,
    /// Optional dependencies.
    pub optdepends: Option<Vec<String>>,
    /// Build-time dependencies (source packages need these).
    pub makedepends: Option<Vec<String>>,
    /// Backup files to preserve on upgrade.
    pub backup: Option<Vec<String>>,
    /// Package release number (default: "1").
    pub rel: Option<String>,
    /// Custom `prepare()` function body for PKGBUILD.
    pub prepare: Option<String>,
    /// Custom `build()` function body for PKGBUILD.
    pub build: Option<String>,
    /// Custom `package()` function body for PKGBUILD.
    pub package: Option<String>,
    /// AUR SSH git URL.
    pub git_url: Option<String>,
    /// Custom SSH command for git operations.
    pub git_ssh_command: Option<String>,
    /// Path to SSH private key file.
    pub private_key: Option<String>,
    /// Subdirectory in the git repo for committed files.
    pub directory: Option<String>,
    /// Skip this config.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Explicit architecture list (default: auto-detect from artifacts).
    pub arches: Option<Vec<String>>,
    /// `x86_64` micro-architecture variant — `v1` (baseline), `v2`, `v3`
    /// (AVX2), or `v4`. Equivalent to GR `AurSource.Goamd64`. Constrained
    /// to a typed enum because AUR source pkgs build from the upstream
    /// tarball (no binary artifacts to filter), so the value's only role
    /// is as the `Amd64` template var consumed by `prepare:` / `build:` /
    /// `package:` script bodies — typos must fail at parse time, not
    /// silently render an invalid string into the PKGBUILD.
    /// When unset, defaults to `v1` at template-render time.
    pub amd64_variant: Option<Amd64Variant>,
}

/// `x86_64` micro-architecture variant. Mirrors GoReleaser's `Goamd64` typed
/// values. Used by [`AurSourceConfig::amd64_variant`] to constrain the
/// `prepare:` / `build:` / `package:` template var surface to a known set —
/// AUR source pkgs build from the upstream tarball so the value is
/// template-only (no artifact filter) and a typo would render an invalid
/// PKGBUILD silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Amd64Variant {
    V1,
    V2,
    V3,
    V4,
}

impl Amd64Variant {
    /// Canonical lowercase string form (`"v1"`..`"v4"`). Matches the GR
    /// `Goamd64` external surface and the value rendered into the PKGBUILD
    /// `Amd64` template var.
    pub fn as_str(&self) -> &'static str {
        match self {
            Amd64Variant::V1 => "v1",
            Amd64Variant::V2 => "v2",
            Amd64Variant::V3 => "v3",
            Amd64Variant::V4 => "v4",
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
