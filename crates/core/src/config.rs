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

/// Environment file configuration.
///
/// Accepts two forms:
/// - **List form** (anodizer extension): array of `.env` file paths loaded as KEY=VALUE.
///   ```yaml
///   env_files:
///     - .env
///     - .release.env
///   ```
/// - **Struct form** (GoReleaser parity): paths to files containing provider tokens.
///   ```yaml
///   env_files:
///     github_token: ~/.config/goreleaser/github_token
///     gitlab_token: ~/.config/goreleaser/gitlab_token
///     gitea_token: ~/.config/goreleaser/gitea_token
///   ```
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum EnvFilesConfig {
    /// List of `.env` file paths to load (KEY=VALUE format).
    List(Vec<String>),
    /// Structured token file paths (GoReleaser parity).
    TokenFiles(EnvFilesTokenConfig),
}

impl<'de> Deserialize<'de> for EnvFilesConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_yaml_ng::Value::deserialize(deserializer)?;
        match &value {
            serde_yaml_ng::Value::Sequence(_) => {
                let list: Vec<String> =
                    serde_yaml_ng::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(EnvFilesConfig::List(list))
            }
            serde_yaml_ng::Value::Mapping(_) => {
                let tokens: EnvFilesTokenConfig =
                    serde_yaml_ng::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(EnvFilesConfig::TokenFiles(tokens))
            }
            _ => Err(serde::de::Error::custom(
                "env_files must be an array of file paths or a mapping with token file paths",
            )),
        }
    }
}

impl EnvFilesConfig {
    /// Returns the list of .env file paths if this is the List variant.
    pub fn as_list(&self) -> Option<&[String]> {
        match self {
            EnvFilesConfig::List(files) => Some(files),
            EnvFilesConfig::TokenFiles(_) => None,
        }
    }

    /// Returns the token files config if this is the TokenFiles variant.
    pub fn as_token_files(&self) -> Option<&EnvFilesTokenConfig> {
        match self {
            EnvFilesConfig::List(_) => None,
            EnvFilesConfig::TokenFiles(tokens) => Some(tokens),
        }
    }
}

/// Structured token file paths for provider authentication.
///
/// Each field points to a file containing a single-line token. When present,
/// the file is read and the corresponding environment variable is set
/// (e.g., `github_token` file -> `GITHUB_TOKEN` env var).
///
/// Matches GoReleaser's `EnvFiles` struct.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct EnvFilesTokenConfig {
    /// Path to file containing the GitHub token. Default: `~/.config/goreleaser/github_token`.
    pub github_token: Option<String>,
    /// Path to file containing the GitLab token. Default: `~/.config/goreleaser/gitlab_token`.
    pub gitlab_token: Option<String>,
    /// Path to file containing the Gitea token. Default: `~/.config/goreleaser/gitea_token`.
    pub gitea_token: Option<String>,
}

/// Read a single token from a file, returning the first line trimmed.
///
/// Returns `Ok(None)` if the file does not exist.
/// Returns `Err` if the file exists but cannot be read.
pub fn read_token_file(path: &str) -> Result<Option<String>, String> {
    // Expand ~ to home directory
    let expanded = if let Some(suffix) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            format!("{}/{}", home, suffix)
        } else {
            path.to_string()
        }
    } else {
        path.to_string()
    };

    match std::fs::read_to_string(&expanded) {
        Ok(content) => {
            let token = content.lines().next().unwrap_or("").trim().to_string();
            if token.is_empty() {
                Ok(None)
            } else {
                Ok(Some(token))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("failed to read token file '{}': {}", path, e)),
    }
}

/// Load tokens from structured `env_files` config.
///
/// For each configured token file path, reads the file and returns the
/// corresponding environment variable name and token value.
/// Falls back to GoReleaser defaults (`~/.config/goreleaser/...`) when
/// a field is not specified.
///
/// Only returns entries where the corresponding process env var is NOT already
/// set, matching GoReleaser's `loadEnv` behavior (env var takes precedence).
pub fn load_token_files(
    config: &EnvFilesTokenConfig,
    log: &crate::log::StageLogger,
) -> Result<std::collections::HashMap<String, String>, String> {
    let mut vars = std::collections::HashMap::new();

    // Per-token candidate paths. The user's explicit `github_token` / etc.
    // config value wins if present; otherwise we try anodizer-native first,
    // then the goreleaser-compat path for users migrating in.
    let github_candidates: Vec<&str> = match config.github_token.as_deref() {
        Some(p) => vec![p],
        None => vec![
            "~/.config/anodizer/github_token",
            "~/.config/goreleaser/github_token",
        ],
    };
    let gitlab_candidates: Vec<&str> = match config.gitlab_token.as_deref() {
        Some(p) => vec![p],
        None => vec![
            "~/.config/anodizer/gitlab_token",
            "~/.config/goreleaser/gitlab_token",
        ],
    };
    let gitea_candidates: Vec<&str> = match config.gitea_token.as_deref() {
        Some(p) => vec![p],
        None => vec![
            "~/.config/anodizer/gitea_token",
            "~/.config/goreleaser/gitea_token",
        ],
    };
    let mappings: [(&str, &[&str]); 3] = [
        ("GITHUB_TOKEN", &github_candidates),
        ("GITLAB_TOKEN", &gitlab_candidates),
        ("GITEA_TOKEN", &gitea_candidates),
    ];

    for (env_name, candidates) in &mappings {
        // Skip if the env var is already set in the process environment
        if std::env::var(env_name)
            .ok()
            .filter(|v| !v.is_empty())
            .is_some()
        {
            log.verbose(&format!("using {} from process environment", env_name));
            continue;
        }
        for file_path in candidates.iter() {
            match read_token_file(file_path) {
                Ok(Some(token)) => {
                    log.verbose(&format!("loaded {} from {}", env_name, file_path));
                    vars.insert(env_name.to_string(), token);
                    break;
                }
                Ok(None) => {
                    // File doesn't exist or is empty — try next candidate
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }
    }

    Ok(vars)
}

/// Load environment variables from .env-style files.
/// Each file is read as KEY=VALUE lines. Lines starting with # and empty lines are skipped.
/// Returns a HashMap of parsed key-value pairs. Does NOT mutate the process
/// environment — callers should inject these into the template context via
/// `set_env()` and pass them to subprocesses via `Command::envs()`.
pub fn load_env_files(
    files: &[String],
    log: &crate::log::StageLogger,
    strict: bool,
) -> Result<std::collections::HashMap<String, String>, String> {
    let mut vars = std::collections::HashMap::new();
    for file_path in files {
        let content = match std::fs::read_to_string(file_path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if strict {
                    return Err(format!("env file '{}' not found (strict mode)", file_path));
                }
                log.warn(&format!("env file '{}' not found, skipping", file_path));
                continue;
            }
            Err(e) => {
                return Err(format!("failed to read env file '{}': {}", file_path, e));
            }
        };
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            // Strip `export ` prefix (common in .env files)
            let trimmed = trimmed.strip_prefix("export ").unwrap_or(trimmed);
            if let Some((key, value)) = trimmed.split_once('=') {
                let key = key.trim();
                if key.is_empty() {
                    log.warn(&format!(
                        "skipping line with empty key in '{}': {}",
                        file_path,
                        line.trim()
                    ));
                    continue;
                }
                let value = value.trim();
                // Strip surrounding quotes from value if present
                let value = if value.len() >= 2
                    && ((value.starts_with('"') && value.ends_with('"'))
                        || (value.starts_with('\'') && value.ends_with('\'')))
                {
                    &value[1..value.len() - 1]
                } else {
                    value
                };
                vars.insert(key.to_string(), value.to_string());
            } else {
                log.warn(&format!(
                    "skipping line without '=' in '{}': {}",
                    file_path, trimmed
                ));
            }
        }
    }
    Ok(vars)
}

// ---------------------------------------------------------------------------
// env helpers — Vec<String> of "KEY=VAL" entries
// ---------------------------------------------------------------------------
//
// Lifted to `crate::env` so they are reachable as
// `anodizer_core::env::*` directly. The re-exports below preserve the
// historical `anodizer_core::config::*` import paths used by stages and
// publishers.

pub use crate::env::{parse_env_entries, render_env_entries, split_env_entry};

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

/// Workspace-level defaults that path-mirror the `CrateConfig` (and select
/// top-level `Config`) shape. Each field here is folded into every resolved
/// crate by `defaults_merge::apply_defaults` according to the deep-merge /
/// merge-by-identity semantics documented in `defaults_merge`.
///
/// Multi-publisher fields are single-struct on both sides today: defaults
/// supplies one struct per publisher, and per-crate `publish.*` fields are
/// also single-struct. A future change may introduce list-or-scalar via
/// `OneOrMany<T>` on the per-crate side so a crate can declare multiple
/// homebrew taps / scoop buckets / etc.; the defaults side would stay
/// single-struct and merge into the first per-crate entry by identity.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct Defaults {
    // --- Build axis ---
    /// Default build settings applied to every crate's builds (deep-merged
    /// into each `CrateConfig.builds[]` entry by identity on `id`/`binary`).
    pub builds: Option<BuildConfig>,
    /// Default archive settings applied to all crates.
    pub archives: Option<ArchiveConfig>,
    /// Default source-archive settings applied to all crates.
    pub source: Option<SourceConfig>,
    /// Default UPX compression settings applied to all crates.
    pub upx: Option<UpxConfig>,

    // --- Packaging axis ---
    /// Default nfpm (deb/rpm/apk) settings applied to all crates.
    pub nfpms: Option<NfpmConfig>,
    /// Default snapcraft settings applied to all crates.
    pub snapcrafts: Option<SnapcraftConfig>,
    /// Default flatpak settings applied to all crates.
    pub flatpaks: Option<FlatpakConfig>,
    /// Default app-bundle settings applied to all crates.
    pub app_bundles: Option<AppBundleConfig>,
    /// Default DMG settings applied to all crates.
    pub dmgs: Option<DmgConfig>,
    /// Default macOS PKG settings applied to all crates.
    pub pkgs: Option<PkgConfig>,
    /// Default MSI settings applied to all crates.
    pub msis: Option<MsiConfig>,
    /// Default NSIS settings applied to all crates.
    pub nsis: Option<NsisConfig>,
    /// Default makeself settings applied to all crates.
    pub makeselves: Option<MakeselfConfig>,
    /// Default SRPM settings applied to all crates.
    pub srpms: Option<SrpmConfig>,
    /// Default Docker (V2 API) image settings applied to all crates.
    pub docker_v2: Option<DockerV2Config>,

    // --- Publish axis ---
    /// Default publisher configurations (single-struct per publisher).
    /// Per-crate `publish.*` entries are merged into these by identity.
    pub publish: Option<PublishDefaults>,

    // --- Sign / notarize / sbom ---
    /// Default artifact signing settings.
    pub sign: Option<SignConfig>,
    /// Default binary-signing settings.
    pub binary_signs: Option<SignConfig>,
    /// Default Docker image signing settings.
    pub docker_signs: Option<DockerSignConfig>,
    /// Default macOS notarization settings.
    pub notarize: Option<NotarizeConfig>,
    /// Default SBOM generation settings.
    pub sbom: Option<SbomConfig>,

    // --- Cross-cutting ---
    /// Default build targets (e.g., ["x86_64-unknown-linux-gnu", "aarch64-apple-darwin"]).
    pub targets: Option<Vec<String>>,
    /// Default environment variables (`KEY=VALUE` strings) hoisted across crates.
    pub env: Option<Vec<String>>,
    /// Default cross-compilation strategy: auto, zigbuild, cross, or cargo.
    /// Mirrors `CrateConfig.cross` so the strategy can be hoisted to defaults.
    pub cross: Option<CrossStrategy>,
    /// Default checksum settings applied to all crates.
    /// Mirrors `CrateConfig.checksum` so checksum config can be hoisted to defaults.
    pub checksum: Option<ChecksumConfig>,

    // --- Crate-axis vs workspace-axis (mutually exclusive — DEC-4) ---
    /// Crate-axis defaults marker. Only valid when top-level `crates:` is set.
    /// Reserved for per-crate overrides keyed by crate id (future waves).
    pub crates: Option<DefaultsCrateBlock>,
    /// Workspace-axis defaults marker. Only valid when top-level `workspaces:` is set.
    /// Reserved for per-workspace overrides keyed by workspace name (future waves).
    pub workspaces: Option<DefaultsWorkspaceBlock>,
}

/// Workspace-default publishers (DEC-3). Each publisher is single-struct in
/// defaults; per-crate `publish.*` may be either a single struct or a list,
/// reconciled by the merge engine.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct PublishDefaults {
    /// Default Homebrew formula settings.
    pub homebrew: Option<HomebrewConfig>,
    /// Default Homebrew Cask settings, merged into per-crate `publish.homebrew_cask`.
    ///
    /// Single-struct per DEC-3.
    pub homebrew_cask: Option<HomebrewCaskConfig>,
    /// Default crates.io publish settings, merged into per-crate `publish.cargo`.
    ///
    /// Single-struct per DEC-3.
    pub cargo: Option<CargoPublishConfig>,
    /// Default Scoop manifest settings.
    pub scoop: Option<ScoopConfig>,
    /// Default WinGet manifest settings.
    pub winget: Option<WingetConfig>,
    /// Default Chocolatey package settings.
    pub chocolatey: Option<ChocolateyConfig>,
    /// Default Krew (kubectl plugin manager) settings.
    pub krew: Option<KrewConfig>,
    /// Default Nix derivation settings.
    pub nix: Option<NixConfig>,
    /// Default AUR (binary) settings.
    pub aur: Option<AurConfig>,
    /// Default AUR (source) settings.
    pub aur_source: Option<AurSourceConfig>,
}

/// Marker block under `defaults.crates:` that signals crate-axis defaults
/// scope. Required to drive the DEC-4 axis-mismatch validator. Currently
/// empty; future per-crate-id overrides will live here.
///
/// `deny_unknown_fields` so that typing `defaults.crates: { foo: bar }`
/// surfaces as a parse error rather than silently being accepted — without
/// it, the empty struct is a sink that swallows arbitrary keys.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DefaultsCrateBlock {}

/// Marker block under `defaults.workspaces:` that signals workspace-axis
/// defaults scope. Required to drive the DEC-4 axis-mismatch validator.
/// Currently empty; future per-workspace-name overrides will live here.
///
/// `deny_unknown_fields` per the same rationale as `DefaultsCrateBlock`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DefaultsWorkspaceBlock {}

// ---------------------------------------------------------------------------
// BuildIgnore — exclude specific os/arch combos from builds
// ---------------------------------------------------------------------------

/// Exclude a specific os/arch combination from the build matrix.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BuildIgnore {
    /// Operating system to exclude (e.g., "linux", "darwin", "windows").
    pub os: String,
    /// Architecture to exclude (e.g., "amd64", "arm64", "386").
    pub arch: String,
}

// ---------------------------------------------------------------------------
// BuildOverride — per-target env, flags, features
// ---------------------------------------------------------------------------

/// Override env, flags, or features for targets matching glob patterns.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct BuildOverride {
    /// Glob patterns to match against target triples (e.g., `["x86_64-*", "*-linux-*"]`).
    pub targets: Vec<String>,
    /// Extra environment variables to set for matching targets.
    #[serde(default)]
    pub env: Option<Vec<String>>,
    /// Extra flags to append for matching targets, one per list entry.
    pub flags: Option<Vec<String>>,
    /// Extra features to enable for matching targets.
    pub features: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// CrossStrategy
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CrossStrategy {
    Auto,
    Zigbuild,
    Cross,
    Cargo,
}

// ---------------------------------------------------------------------------
// CrateConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct CrateConfig {
    /// Crate name as published (must match the Cargo.toml package name).
    pub name: String,
    /// Relative path to the crate directory from the project root.
    pub path: String,
    /// Git tag template used to tag and identify releases (supports templates).
    pub tag_template: String,
    /// Pinned semver version. When set, `anodizer bump --strict` refuses to
    /// edit this crate's `Cargo.toml` to anything other than this value;
    /// without `--strict`, the bump proceeds with a warning. Lets a release
    /// captain freeze a crate's version while still running broad
    /// `--workspace` bumps.
    pub version: Option<String>,
    /// Other crates this crate depends on; ensures release ordering.
    pub depends_on: Option<Vec<String>>,
    /// Build configurations for this crate. One entry per binary by default.
    pub builds: Option<Vec<BuildConfig>>,
    /// Cross-compilation strategy for this crate: auto, zigbuild, cross, or cargo.
    pub cross: Option<CrossStrategy>,
    #[serde(default, deserialize_with = "deserialize_archives_config")]
    #[schemars(schema_with = "archives_schema")]
    pub archives: ArchivesConfig,
    /// Checksum configuration for this crate.
    pub checksum: Option<ChecksumConfig>,
    /// GitHub release configuration for this crate.
    pub release: Option<ReleaseConfig>,
    /// Publishing targets (Homebrew, Scoop, AUR, etc.) for this crate.
    pub publish: Option<PublishConfig>,
    /// Docker V2 image build configurations for this crate (canonical API:
    /// images+tags, annotations, build_args, sbom, disable). The legacy
    /// `docker:` block was removed; this is the only docker surface.
    pub docker_v2: Option<Vec<DockerV2Config>>,
    /// Docker image digest file configuration for this crate.
    pub docker_digest: Option<DockerDigestConfig>,
    /// Docker multi-platform manifest configurations for this crate.
    pub docker_manifests: Option<Vec<DockerManifestConfig>>,
    /// Linux package (deb, rpm, apk) configurations for this crate. Renamed
    /// from `nfpm:` (singular) for spelling parity with `Defaults.nfpms` and
    /// the rest of the plural-name per-crate packaging lists (`dmgs`, `msis`,
    /// `pkgs`, `nsis`, ...). The `nfpm:` spelling is still accepted via serde
    /// alias for back-compat.
    #[serde(alias = "nfpm")]
    pub nfpms: Option<Vec<NfpmConfig>>,
    /// Snapcraft package configurations for this crate.
    pub snapcrafts: Option<Vec<SnapcraftConfig>>,
    /// macOS DMG disk image configurations for this crate.
    pub dmgs: Option<Vec<DmgConfig>>,
    /// Windows MSI installer configurations for this crate.
    pub msis: Option<Vec<MsiConfig>>,
    /// macOS PKG installer configurations for this crate.
    pub pkgs: Option<Vec<PkgConfig>>,
    /// NSIS installer configurations for this crate.
    pub nsis: Option<Vec<NsisConfig>>,
    /// macOS app bundle configurations for this crate.
    pub app_bundles: Option<Vec<AppBundleConfig>>,
    /// Linux Flatpak bundle configurations for this crate.
    pub flatpaks: Option<Vec<FlatpakConfig>>,
    /// Cloud storage (S3/GCS/Azure) upload configurations for this crate.
    pub blobs: Option<Vec<BlobConfig>>,
    /// cargo-binstall metadata configuration for this crate.
    pub binstall: Option<BinstallConfig>,
    /// Automatic version number synchronization configuration for this crate.
    pub version_sync: Option<VersionSyncConfig>,
    /// macOS universal binary (fat binary) configurations for this crate.
    pub universal_binaries: Option<Vec<UniversalBinaryConfig>>,
    /// When true (or template evaluating to "true"), all build outputs are
    /// placed in a flat `dist/` directory instead of `dist/{target}/`.
    #[serde(default, deserialize_with = "deserialize_string_or_bool_opt")]
    pub no_unique_dist_dir: Option<StringOrBool>,
}

/// Helper schema function for archives (accepts false or array).
fn archives_schema(generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
    let mut schema = generator.subschema_for::<Option<Vec<ArchiveConfig>>>();
    if let schemars::schema::Schema::Object(ref mut obj) = schema {
        obj.metadata().description = Some("Archive configurations for this crate. Set to false to disable archiving, or provide an array of archive configs.".to_owned());
    }
    schema
}

impl Default for CrateConfig {
    fn default() -> Self {
        CrateConfig {
            name: String::new(),
            path: String::new(),
            tag_template: String::new(),
            version: None,
            depends_on: None,
            builds: None,
            cross: None,
            archives: ArchivesConfig::Configs(vec![]),
            checksum: None,
            release: None,
            publish: None,
            docker_v2: None,
            docker_digest: None,
            docker_manifests: None,
            nfpms: None,
            snapcrafts: None,
            dmgs: None,
            msis: None,
            pkgs: None,
            nsis: None,
            app_bundles: None,
            flatpaks: None,
            blobs: None,
            binstall: None,
            version_sync: None,
            universal_binaries: None,
            no_unique_dist_dir: None,
        }
    }
}

// ---------------------------------------------------------------------------
// UniversalBinaryConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct UniversalBinaryConfig {
    /// Unique identifier for this universal binary, propagated into the
    /// artifact's metadata as `id` (GoReleaser universalbinary.go:42-44).
    #[serde(default)]
    pub id: Option<String>,
    /// Output filename template for the universal binary (supports templates).
    pub name_template: Option<String>,
    /// When true, remove the individual arch binaries after creating the universal binary.
    pub replace: Option<bool>,
    /// Build IDs filter: only combine artifacts from builds whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Pre/post hooks around universal binary creation.
    pub hooks: Option<BuildHooksConfig>,
    /// Override the modification timestamp for reproducible universal binaries.
    pub mod_timestamp: Option<String>,
}

// ---------------------------------------------------------------------------
// BuildConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct BuildConfig {
    /// Unique identifier for this build, used to reference it from archives and other configs.
    pub id: Option<String>,
    /// Binary name to build (must match a Cargo binary target in the crate).
    ///
    /// Optional so that `defaults.builds` (a path-mirrored template that
    /// applies to every crate) can omit `binary` — the per-crate `builds[]`
    /// entry supplies it. When the binary is absent at the per-crate level
    /// it falls back to the crate's `name` field.
    pub binary: Option<String>,
    /// When true (or template evaluating to "true"), skip this build entirely.
    #[serde(default, deserialize_with = "deserialize_string_or_bool_opt")]
    pub skip: Option<StringOrBool>,
    /// Target triples to build for. When set, REPLACES `defaults.targets`
    /// for this build (override semantics — the per-build value wins
    /// outright, no concat). When `None`, this build inherits
    /// `defaults.targets` verbatim. Both `cli::commands::helpers::
    /// collect_build_targets` and `stage-build` enforce this rule.
    pub targets: Option<Vec<String>>,
    /// Cargo features to enable for this build.
    pub features: Option<Vec<String>>,
    /// When true, pass --no-default-features to cargo build.
    pub no_default_features: Option<bool>,
    /// Per-target environment variables keyed as {target: {KEY: VALUE}}.
    pub env: Option<HashMap<String, HashMap<String, String>>>,
    /// Copy the binary from another build ID instead of building it.
    pub copy_from: Option<String>,
    /// Extra flags passed to cargo build, one per list entry (e.g., `["--release", "--locked"]`).
    /// Each entry is template-rendered then passed verbatim as a single argv token,
    /// so quoted shell arguments (`--cfg=feature="foo bar"`) survive intact.
    pub flags: Option<Vec<String>>,
    /// When true, enable reproducible builds by stripping timestamps.
    pub reproducible: Option<bool>,
    /// Per-build hooks executed before and after compilation.
    pub hooks: Option<BuildHooksConfig>,
    /// Exclude specific os/arch combinations from this build's target matrix.
    /// Falls back to `defaults.builds.ignore` when not set.
    pub ignore: Option<Vec<BuildIgnore>>,
    /// Per-target overrides for env, flags, and features for this build.
    /// Falls back to `defaults.builds.overrides` when not set.
    pub overrides: Option<Vec<BuildOverride>>,
    /// Override the cross-compilation tool binary path (e.g., a custom `cross` wrapper).
    /// When set, this binary is used instead of cargo/cross/zigbuild.
    pub cross_tool: Option<String>,
    /// Override the modification timestamp of built binaries for reproducible builds.
    /// Template string (e.g. `"{{ .CommitTimestamp }}"`) or unix timestamp.
    pub mod_timestamp: Option<String>,
    /// Override the cargo subcommand (default: auto-detected "build" or "zigbuild").
    /// Enables e.g. `cargo auditable build` by setting `command: "auditable build"`.
    pub command: Option<String>,
    /// When true (or template evaluating to "true"), place binaries in flat dist/
    /// instead of dist/{target}/. Overrides the crate-level setting.
    #[serde(default, deserialize_with = "deserialize_string_or_bool_opt")]
    pub no_unique_dist_dir: Option<StringOrBool>,
}

/// Pre/post hook configuration shared across multiple stages. Despite the
/// `Build` prefix in the name, this type is used by both the **build** stage
/// (pre/post compilation hooks) and the **archive** stage (pre/post archiving
/// hooks). The name is kept for backward compatibility with existing configs.
/// **Not** to be confused with the top-level `HooksConfig` (which carries a
/// flat `hooks: Vec<String>` list for `before`/`after` lifecycle hooks).
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct BuildHooksConfig {
    /// Commands to run before the build step.
    pub pre: Option<Vec<HookEntry>>,
    /// Commands to run after the build step.
    pub post: Option<Vec<HookEntry>>,
}

/// Pre/post archive hook configuration.
///
/// Archive hooks use `before`/`after` (matching GoReleaser's archive pipe);
/// build hooks use `pre`/`post` (matching GoReleaser's build pipe).
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ArchiveHooksConfig {
    /// Commands to run before the archive step.
    pub before: Option<Vec<HookEntry>>,
    /// Commands to run after the archive step.
    pub after: Option<Vec<HookEntry>>,
}

// ---------------------------------------------------------------------------
// ArchivesConfig — untagged enum: false => Disabled, array => Configs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, JsonSchema)]
pub enum ArchivesConfig {
    Disabled,
    Configs(Vec<ArchiveConfig>),
}

impl Serialize for ArchivesConfig {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        match self {
            ArchivesConfig::Disabled => serializer.serialize_bool(false),
            ArchivesConfig::Configs(configs) => configs.serialize(serializer),
        }
    }
}

impl Default for ArchivesConfig {
    fn default() -> Self {
        ArchivesConfig::Configs(vec![])
    }
}

/// Custom deserializer for ArchivesConfig.
/// Accepts:
///   - boolean `false`  → Disabled
///   - array            → Configs(...)
///   - missing/null     → Configs([])  (via serde default)
fn deserialize_archives_config<'de, D>(deserializer: D) -> Result<ArchivesConfig, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::{self, Visitor};

    struct ArchivesVisitor;

    impl<'de> Visitor<'de> for ArchivesVisitor {
        type Value = ArchivesConfig;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("false or a list of archive configs")
        }

        fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
            if !v {
                Ok(ArchivesConfig::Disabled)
            } else {
                Err(E::custom(
                    "archives: true is not valid; use false or a list",
                ))
            }
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut configs = Vec::new();
            while let Some(item) = seq.next_element::<ArchiveConfig>()? {
                configs.push(item);
            }
            Ok(ArchivesConfig::Configs(configs))
        }

        // Handle YAML null / missing when serde calls the deserializer explicitly.
        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(ArchivesConfig::Configs(vec![]))
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(ArchivesConfig::Configs(vec![]))
        }
    }

    deserializer.deserialize_any(ArchivesVisitor)
}

/// Custom deserializer for the `signs` / `sign` field.
/// Accepts:
///   - null/missing → empty vec (via serde default)
///   - a single object → vec of one SignConfig
///   - an array → vec of SignConfig
fn deserialize_signs<'de, D>(deserializer: D) -> Result<Vec<SignConfig>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::{self, Visitor};

    struct SignsVisitor;

    impl<'de> Visitor<'de> for SignsVisitor {
        type Value = Vec<SignConfig>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a sign config object or an array of sign config objects")
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut configs = Vec::new();
            while let Some(item) = seq.next_element::<SignConfig>()? {
                configs.push(item);
            }
            Ok(configs)
        }

        fn visit_map<M: de::MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
            let config = SignConfig::deserialize(de::value::MapAccessDeserializer::new(map))?;
            Ok(vec![config])
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }
    }

    deserializer.deserialize_any(SignsVisitor)
}

// `binary_signs[].artifacts` is constrained at deserialize time (not as a
// serde-typed enum) because `SignConfig` is shared with the top-level `signs:`
// field, which legitimately accepts a wider set (`all`, `archive`, `binary`,
// `checksum`, `package`, `sbom`, `none`). Promoting `artifacts` to an enum
// would either narrow that surface or require a parallel `BinarySignConfig`
// type duplicating every `SignConfig` field — the runtime check below keeps
// `SignConfig` a single shared shape while still rejecting misconfigured
// `binary_signs` entries at config-load time.
//
// The JSON schema for `binary_signs[]` therefore inherits `SignConfig`'s
// unconstrained `artifacts: Option<String>` — the constraint lives in the
// custom deserializer below and is exercised by the parse-time tests
// `test_binary_signs_artifacts_*` further down this file.

/// Wraps [`deserialize_signs`] and enforces that each entry's `artifacts`
/// is one of the binary-only allowed values (`binary`, `none`, or omitted).
/// Catches misconfiguration at load time instead of producing a silent
/// no-op signing pipe.
fn deserialize_binary_signs<'de, D>(deserializer: D) -> Result<Vec<SignConfig>, D::Error>
where
    D: Deserializer<'de>,
{
    let configs = deserialize_signs(deserializer)?;
    for (idx, cfg) in configs.iter().enumerate() {
        if let Some(art) = cfg.artifacts.as_deref()
            && art != "binary"
            && art != "none"
        {
            return Err(serde::de::Error::custom(format!(
                "binary_signs[{idx}].artifacts: '{art}' is not allowed; \
                 binary_signs accepts only 'binary' or 'none' (use top-level \
                 `signs:` for broader artifact filters)"
            )));
        }
    }
    Ok(configs)
}

// ---------------------------------------------------------------------------
// WrapInDirectory – accepts bool (true = default dir name) or string
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum WrapInDirectory {
    Bool(bool),
    Name(String),
}

impl<'de> serde::Deserialize<'de> for WrapInDirectory {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_yaml_ng::Value::deserialize(deserializer)?;
        match value {
            serde_yaml_ng::Value::Bool(b) => Ok(WrapInDirectory::Bool(b)),
            serde_yaml_ng::Value::String(s) => Ok(WrapInDirectory::Name(s)),
            _ => Err(serde::de::Error::custom("expected bool or string")),
        }
    }
}

impl WrapInDirectory {
    /// Resolve the directory name to wrap archive contents in.
    ///
    /// When `true`, uses `default_name` (typically the archive stem).
    /// When `false` or an empty string, returns `None` (no wrapping).
    /// Otherwise returns the custom name.
    pub fn directory_name(&self, default_name: &str) -> Option<String> {
        match self {
            WrapInDirectory::Bool(true) => Some(default_name.to_string()),
            WrapInDirectory::Bool(false) => None,
            WrapInDirectory::Name(s) if s.is_empty() => None,
            WrapInDirectory::Name(s) => Some(s.clone()),
        }
    }
}

// ---------------------------------------------------------------------------
// ArchiveConfig
// ---------------------------------------------------------------------------

fn default_archive_id() -> Option<String> {
    Some("default".to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ArchiveConfig {
    /// Unique identifier for cross-referencing this archive from other configs.
    /// Defaults to `"default"` so a parse->serialise->reparse round-trip is
    /// stable (GoReleaser stores this verbatim, not as an Option).
    #[serde(default = "default_archive_id")]
    pub id: Option<String>,
    /// Archive filename template (supports templates, e.g., "{{ .ProjectName }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}").
    pub name_template: Option<String>,
    /// Archive formats: tar.gz, tar.xz, tar.zst, zip, or binary. Plural list;
    /// one archive per format is produced for each target.
    pub formats: Option<Vec<String>>,
    /// Per-OS format overrides for this archive config.
    pub format_overrides: Option<Vec<FormatOverride>>,
    /// Extra files to include in the archive (glob patterns or detailed src/dst specs).
    pub files: Option<Vec<ArchiveFileSpec>>,
    /// Binary names to include (defaults to all binaries from matched builds).
    pub binaries: Option<Vec<String>>,
    /// When set, wrap archive contents in a top-level directory.
    /// Accepts `true` (use archive stem as directory name), `false` (no wrapping),
    /// or a string template for a custom directory name.
    pub wrap_in_directory: Option<WrapInDirectory>,
    /// Build IDs filter: only include artifacts from builds whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// When true, create archive with no binaries (metadata-only).
    pub meta: Option<bool>,
    /// File permissions applied to binaries in archives.
    pub builds_info: Option<ArchiveFileInfo>,
    /// Strip binary parent directory in archive (place binaries at archive root).
    pub strip_binary_directory: Option<bool>,
    /// Allow different binary counts across targets. Default false (warn on mismatch).
    pub allow_different_binary_count: Option<bool>,
    /// Pre/post archive hooks (`before`/`after`).
    pub hooks: Option<ArchiveHooksConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FormatOverride {
    /// Operating system this override applies to (e.g., "windows", "darwin", "linux").
    pub os: String,
    /// Plural format overrides for this OS: tar.gz, tar.xz, tar.zst, zip, or binary.
    pub formats: Option<Vec<String>>,
}

/// Specifies a file to include in archives. Can be a simple glob string or a
/// detailed object with src/dst/info fields for controlling archive placement
/// and file metadata.
///
/// NOTE: This is intentionally a separate type from [`ExtraFileSpec`] (used for
/// checksum/release extra_files). `ArchiveFileSpec` needs `src`/`dst`/`info`
/// fields for archive placement and file metadata (owner, group, mode, mtime),
/// while `ExtraFileSpec` needs `glob`/`name_template` for checksumming and
/// upload renaming. The fields and semantics are different enough that a unified
/// type would be confusing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ArchiveFileSpec {
    Glob(String),
    Detailed {
        src: String,
        dst: Option<String>,
        info: Option<ArchiveFileInfo>,
        /// When true, strip the parent directory from the file path in the archive.
        strip_parent: Option<bool>,
    },
}

impl PartialEq<&str> for ArchiveFileSpec {
    fn eq(&self, other: &&str) -> bool {
        match self {
            ArchiveFileSpec::Glob(s) => s.as_str() == *other,
            _ => false,
        }
    }
}

/// Shared file metadata (owner, group, mode, mtime) used by both archive entries
/// and nFPM package contents. Previously duplicated as `ArchiveFileInfo` and
/// `NfpmFileInfo`; now unified.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct FileInfo {
    /// File owner name (e.g., "root").
    pub owner: Option<String>,
    /// File group name (e.g., "root").
    pub group: Option<String>,
    /// File permission mode. Accepts a YAML int (decimal, e.g. `420` for
    /// `0o644`) or an octal-prefixed string (`"0o644"`, `"0644"`). This
    /// matches GoReleaser's `uint32` type for `Mode` on archive/nfpm contents
    /// while letting users spell octal naturally in YAML.
    pub mode: Option<StringOrU32>,
    /// File modification time in RFC3339 format (e.g., "2024-01-01T00:00:00Z").
    pub mtime: Option<String>,
}

/// Backward-compatible alias for archive code.
pub type ArchiveFileInfo = FileInfo;

/// Parse an octal mode string into a `u32`, handling common YAML-friendly
/// representations: `"0755"`, `"0o755"`, `"0O755"`, `"755"`, and `"0"`.
pub fn parse_octal_mode(s: &str) -> Option<u32> {
    let cleaned = s
        .strip_prefix("0o")
        .or_else(|| s.strip_prefix("0O"))
        .unwrap_or(s);
    let cleaned = if cleaned.is_empty() { "0" } else { cleaned };
    u32::from_str_radix(cleaned, 8).ok()
}

/// The set of archive format strings recognised by the archive stage.
/// Used for early validation so typos are caught at config load time rather
/// than mid-pipeline.
pub const VALID_ARCHIVE_FORMATS: &[&str] = &[
    "tar.gz", "tgz", "tar.xz", "txz", "tar.zst", "tzst", "tar", "zip", "gz", "binary", "none",
];

// ---------------------------------------------------------------------------
// ChecksumConfig
// ---------------------------------------------------------------------------

/// Specifies an extra file to include in checksums or release uploads. Can be a
/// simple glob string or a detailed object with glob and name_template fields.
///
/// See [`ArchiveFileSpec`] doc comment for why this is a separate type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ExtraFileSpec {
    Glob(String),
    Detailed {
        glob: String,
        /// Optional override for the upload filename.
        #[serde(default)]
        name_template: Option<String>,
    },
}

impl ExtraFileSpec {
    /// Return the glob pattern for this spec.
    pub fn glob(&self) -> &str {
        match self {
            ExtraFileSpec::Glob(s) => s,
            ExtraFileSpec::Detailed { glob, .. } => glob,
        }
    }

    /// Return the optional name_template (only present in Detailed variant).
    pub fn name_template(&self) -> Option<&str> {
        match self {
            ExtraFileSpec::Glob(_) => None,
            ExtraFileSpec::Detailed { name_template, .. } => name_template.as_deref(),
        }
    }
}

/// A file whose contents are rendered through the template engine before use.
/// Used by `templated_extra_files` across multiple stages (GoReleaser Pro feature).
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema, PartialEq)]
#[serde(default)]
pub struct TemplatedExtraFile {
    /// Source template file path.
    pub src: String,
    /// Destination filename for the rendered output.
    /// Supports template variables (e.g. `"{{ .ProjectName }}-NOTES.txt"`).
    pub dst: Option<String>,
    /// File permissions in octal notation as a string, e.g. `"0755"`.
    /// Parsed at runtime via `parse_octal_mode()` to avoid YAML interpreting as decimal.
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ChecksumConfig {
    /// Checksum filename template (default: "{{ .ProjectName }}_{{ .Version }}_checksums.txt").
    pub name_template: Option<String>,
    /// Hash algorithm: sha256, sha512, sha1, md5, crc32 (default: sha256).
    pub algorithm: Option<String>,
    /// Disable checksums. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Extra files to include in the checksum file (beyond build artifacts).
    pub extra_files: Option<Vec<ExtraFileSpec>>,
    /// Extra files whose contents are rendered through the template engine before inclusion.
    /// Unlike `extra_files` which copy as-is, template variables like `{{ .Tag }}` are expanded.
    /// GoReleaser Pro feature.
    pub templated_extra_files: Option<Vec<TemplatedExtraFile>>,
    /// Build IDs filter: only checksum artifacts from builds whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// When true, produce one checksum file per artifact instead of a combined file.
    pub split: Option<bool>,
}

// ---------------------------------------------------------------------------
// ContentSource — inline string, from_file, or from_url
// ---------------------------------------------------------------------------

/// A content source that can be an inline string, read from a file, or fetched
/// from a URL. Used for release header/footer values.
///
/// YAML examples:
///   header: "inline text"
///   header:
///     from_file: ./RELEASE_HEADER.md
///   header:
///     from_url: https://example.com/header.md
///   header:
///     from_url: https://example.com/header.md
///     headers:
///       X-API-Token: "{{ .Env.API_TOKEN }}"
///       Accept: "text/markdown"
///
/// Both `from_file` path and `from_url` URL are template-rendered before use.
/// Header values are template-rendered. (GoReleaser Pro parity.)
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ContentSource {
    Inline(String),
    FromFile {
        from_file: String,
    },
    FromUrl {
        from_url: String,
        /// Optional HTTP headers (value templates allowed). Enables private
        /// mirrors and authenticated endpoints.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        headers: Option<HashMap<String, String>>,
    },
}

impl PartialEq for ContentSource {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Inline(a), Self::Inline(b)) => a == b,
            (Self::FromFile { from_file: a }, Self::FromFile { from_file: b }) => a == b,
            (
                Self::FromUrl {
                    from_url: a,
                    headers: ha,
                },
                Self::FromUrl {
                    from_url: b,
                    headers: hb,
                },
            ) => a == b && ha == hb,
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------
// ReleaseConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ReleaseConfig {
    /// GitHub repository to release to (owner and name).
    pub github: Option<ScmRepoConfig>,
    /// GitLab repository to release to (owner and name).
    pub gitlab: Option<ScmRepoConfig>,
    /// Gitea repository to release to (owner and name).
    pub gitea: Option<ScmRepoConfig>,
    /// When true, create the release as a draft (unpublished).
    pub draft: Option<bool>,
    #[schemars(schema_with = "prerelease_schema")]
    /// Mark release as pre-release: true, false, or "auto" (inferred from tag).
    pub prerelease: Option<PrereleaseConfig>,
    #[schemars(schema_with = "make_latest_schema")]
    /// Mark release as latest: true, false, or "auto" (latest non-prerelease).
    pub make_latest: Option<MakeLatestConfig>,
    /// Release title template (supports templates).
    pub name_template: Option<String>,
    /// Text prepended to the release body (inline string, from_file, or from_url).
    pub header: Option<ContentSource>,
    /// Text appended to the release body (inline string, from_file, or from_url).
    pub footer: Option<ContentSource>,
    /// Extra files to upload to the release beyond build artifacts.
    pub extra_files: Option<Vec<ExtraFileSpec>>,
    /// Extra files whose contents are rendered through the template engine before upload.
    /// Unlike `extra_files` which copy as-is, template variables like `{{ .Tag }}` are expanded.
    /// GoReleaser Pro feature.
    pub templated_extra_files: Option<Vec<TemplatedExtraFile>>,
    /// Skip uploading artifacts: true, false, or "auto" (skip for snapshots).
    /// Accepts bool or template string (GoReleaser uses string type).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip_upload: Option<StringOrBool>,
    /// When true, replace an existing draft release instead of failing.
    pub replace_existing_draft: Option<bool>,
    /// When true, replace existing release artifacts with the same name.
    pub replace_existing_artifacts: Option<bool>,
    /// Skip the release stage. Accepts bool or template string
    /// (e.g. `"{{ if IsSnapshot }}true{{ endif }}"` for conditional skip).
    /// GoReleaser supports template strings here since v1.15.0.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Release mode: "keep-existing", "append", "prepend", or "replace".
    pub mode: Option<String>,
    /// Artifact IDs filter for uploads.
    pub ids: Option<Vec<String>>,
    /// Target branch or SHA for the release tag.
    pub target_commitish: Option<String>,
    /// GitHub Discussion category name for the release.
    pub discussion_category_name: Option<String>,
    /// Upload metadata.json and artifacts.json as release assets.
    pub include_meta: Option<bool>,
    /// Reuse an existing draft release instead of creating a new one.
    pub use_existing_draft: Option<bool>,
    /// Override the release tag (template string). When set, this tag is used
    /// as the `tag_name` in the GitHub release API instead of the crate's
    /// `tag_template`. Useful in monorepo setups to strip a tag prefix
    /// (e.g. `"{{ .Tag }}"` to publish `v1.0.0` instead of `myapp/v1.0.0`).
    /// This is a GoReleaser Pro feature provided for free by anodizer.
    pub tag: Option<String>,
}

/// Schema for prerelease: "auto" or boolean.
fn prerelease_schema(
    _generator: &mut schemars::r#gen::SchemaGenerator,
) -> schemars::schema::Schema {
    use schemars::schema::{InstanceType, Schema, SchemaObject, SingleOrVec, SubschemaValidation};
    Schema::Object(SchemaObject {
        subschemas: Some(Box::new(SubschemaValidation {
            one_of: Some(vec![
                Schema::Object(SchemaObject {
                    instance_type: Some(SingleOrVec::Single(Box::new(InstanceType::String))),
                    enum_values: Some(vec![serde_json::json!("auto")]),
                    ..Default::default()
                }),
                Schema::Object(SchemaObject {
                    instance_type: Some(SingleOrVec::Single(Box::new(InstanceType::Boolean))),
                    ..Default::default()
                }),
            ]),
            ..Default::default()
        })),
        ..Default::default()
    })
}

/// Schema for make_latest: "auto" or boolean.
fn make_latest_schema(
    _generator: &mut schemars::r#gen::SchemaGenerator,
) -> schemars::schema::Schema {
    use schemars::schema::{InstanceType, Schema, SchemaObject, SingleOrVec, SubschemaValidation};
    Schema::Object(SchemaObject {
        subschemas: Some(Box::new(SubschemaValidation {
            one_of: Some(vec![
                Schema::Object(SchemaObject {
                    instance_type: Some(SingleOrVec::Single(Box::new(InstanceType::String))),
                    enum_values: Some(vec![serde_json::json!("auto")]),
                    ..Default::default()
                }),
                Schema::Object(SchemaObject {
                    instance_type: Some(SingleOrVec::Single(Box::new(InstanceType::Boolean))),
                    ..Default::default()
                }),
            ]),
            ..Default::default()
        })),
        ..Default::default()
    })
}

/// Schema for skip_push: "auto" or boolean.
fn skip_push_schema(_generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
    use schemars::schema::{InstanceType, Schema, SchemaObject, SingleOrVec, SubschemaValidation};
    Schema::Object(SchemaObject {
        subschemas: Some(Box::new(SubschemaValidation {
            one_of: Some(vec![
                Schema::Object(SchemaObject {
                    instance_type: Some(SingleOrVec::Single(Box::new(InstanceType::String))),
                    enum_values: Some(vec![serde_json::json!("auto")]),
                    ..Default::default()
                }),
                Schema::Object(SchemaObject {
                    instance_type: Some(SingleOrVec::Single(Box::new(InstanceType::Boolean))),
                    ..Default::default()
                }),
            ]),
            ..Default::default()
        })),
        ..Default::default()
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ScmRepoConfig {
    /// Repository owner (user or organization).
    pub owner: String,
    /// Repository name.
    pub name: String,
}

/// Backward-compatible alias — existing code can continue to use `GitHubConfig`.
pub type GitHubConfig = ScmRepoConfig;

// ---------------------------------------------------------------------------
// ForceTokenKind
// ---------------------------------------------------------------------------

/// Which SCM token to force for authentication, overriding automatic detection.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ForceTokenKind {
    GitHub,
    GitLab,
    Gitea,
}

// ---------------------------------------------------------------------------
// Platform URL configs (GitHub Enterprise, GitLab self-hosted, Gitea)
// ---------------------------------------------------------------------------

/// Custom GitHub API/upload/download URLs for GitHub Enterprise installations.
/// Matches GoReleaser's `GitHubURLs` struct.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GitHubUrlsConfig {
    /// GitHub API base URL (e.g. `https://github.example.com/api/v3/`).
    pub api: Option<String>,
    /// GitHub upload URL for release assets (e.g. `https://github.example.com/api/uploads/`).
    pub upload: Option<String>,
    /// GitHub download URL for release assets (e.g. `https://github.example.com/`).
    pub download: Option<String>,
    /// When true, skip TLS certificate verification for the custom URLs.
    pub skip_tls_verify: Option<bool>,
}

/// Custom GitLab API/download URLs for self-hosted GitLab installations.
/// Matches GoReleaser's `GitLabURLs` struct.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GitLabUrlsConfig {
    /// GitLab API base URL (e.g. `https://gitlab.example.com/api/v4/`).
    pub api: Option<String>,
    /// GitLab download URL for release assets.
    pub download: Option<String>,
    /// When true, skip TLS certificate verification for the custom URLs.
    pub skip_tls_verify: Option<bool>,
    /// When true, use the GitLab Package Registry for uploads instead of Generic Packages.
    pub use_package_registry: Option<bool>,
    /// When true, use the CI_JOB_TOKEN for authentication instead of a personal token.
    pub use_job_token: Option<bool>,
}

/// Custom Gitea API/download URLs for self-hosted Gitea installations.
/// Matches GoReleaser's `GiteaURLs` struct.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GiteaUrlsConfig {
    /// Gitea API base URL (e.g. `https://gitea.example.com/api/v1/`).
    pub api: Option<String>,
    /// Gitea download URL for release assets.
    pub download: Option<String>,
    /// When true, skip TLS certificate verification for the custom URLs.
    pub skip_tls_verify: Option<bool>,
}

// ---------------------------------------------------------------------------
// "auto" | bool enum — shared serde implementation
// ---------------------------------------------------------------------------

/// Generates `Serialize` and `Deserialize` impls for enums with `Auto` and
/// `Bool(bool)` variants that accept the string `"auto"` or a boolean in YAML.
macro_rules! impl_auto_or_bool_serde {
    ($ty:ty, $auto:path, $bool_variant:path) => {
        impl Serialize for $ty {
            fn serialize<S: serde::Serializer>(
                &self,
                serializer: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                match self {
                    $auto => serializer.serialize_str("auto"),
                    $bool_variant(b) => serializer.serialize_bool(*b),
                }
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: serde::Deserializer<'de>>(
                deserializer: D,
            ) -> std::result::Result<Self, D::Error> {
                struct Visitor;
                impl serde::de::Visitor<'_> for Visitor {
                    type Value = $ty;
                    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                        write!(f, "\"auto\" or a boolean")
                    }
                    fn visit_bool<E: serde::de::Error>(
                        self,
                        v: bool,
                    ) -> std::result::Result<$ty, E> {
                        Ok($bool_variant(v))
                    }
                    fn visit_str<E: serde::de::Error>(
                        self,
                        v: &str,
                    ) -> std::result::Result<$ty, E> {
                        if v == "auto" {
                            Ok($auto)
                        } else {
                            Err(E::custom(format!("expected \"auto\", got \"{}\"", v)))
                        }
                    }
                }
                deserializer.deserialize_any(Visitor)
            }
        }
    };
}

/// `prerelease` can be the string `"auto"` or a boolean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrereleaseConfig {
    Auto,
    Bool(bool),
}

impl_auto_or_bool_serde!(
    PrereleaseConfig,
    PrereleaseConfig::Auto,
    PrereleaseConfig::Bool
);

/// `make_latest` can be the string `"auto"`, a boolean, or a template string.
/// GoReleaser renders this field through its template engine at publish time,
/// so we accept arbitrary strings (e.g. `"{{ if .IsSnapshot }}false{{ else }}true{{ end }}"`)
/// and defer resolution to the release stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MakeLatestConfig {
    Auto,
    Bool(bool),
    /// An arbitrary template string to be rendered at publish time.
    String(String),
}

impl Serialize for MakeLatestConfig {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        match self {
            MakeLatestConfig::Auto => serializer.serialize_str("auto"),
            MakeLatestConfig::Bool(b) => serializer.serialize_bool(*b),
            MakeLatestConfig::String(s) => serializer.serialize_str(s),
        }
    }
}

impl<'de> Deserialize<'de> for MakeLatestConfig {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = MakeLatestConfig;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "\"auto\", a boolean, or a template string")
            }
            fn visit_bool<E: serde::de::Error>(
                self,
                v: bool,
            ) -> std::result::Result<MakeLatestConfig, E> {
                Ok(MakeLatestConfig::Bool(v))
            }
            fn visit_str<E: serde::de::Error>(
                self,
                v: &str,
            ) -> std::result::Result<MakeLatestConfig, E> {
                match v {
                    "auto" => Ok(MakeLatestConfig::Auto),
                    "true" => Ok(MakeLatestConfig::Bool(true)),
                    "false" => Ok(MakeLatestConfig::Bool(false)),
                    other => Ok(MakeLatestConfig::String(other.to_string())),
                }
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// `skip_push` can be `"auto"` (skip for prereleases), a boolean, or a template string.
/// GoReleaser accepts template expressions like `"{{ if .IsSnapshot }}true{{ end }}"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipPushConfig {
    Auto,
    Bool(bool),
    /// Arbitrary template string — rendered at runtime, truthy result means skip push.
    Template(String),
}

impl Serialize for SkipPushConfig {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        match self {
            SkipPushConfig::Auto => serializer.serialize_str("auto"),
            SkipPushConfig::Bool(b) => serializer.serialize_bool(*b),
            SkipPushConfig::Template(s) => serializer.serialize_str(s),
        }
    }
}

impl<'de> Deserialize<'de> for SkipPushConfig {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = SkipPushConfig;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "\"auto\", a boolean, or a template string")
            }
            fn visit_bool<E: serde::de::Error>(
                self,
                v: bool,
            ) -> std::result::Result<SkipPushConfig, E> {
                Ok(SkipPushConfig::Bool(v))
            }
            fn visit_str<E: serde::de::Error>(
                self,
                v: &str,
            ) -> std::result::Result<SkipPushConfig, E> {
                match v {
                    "auto" => Ok(SkipPushConfig::Auto),
                    "true" => Ok(SkipPushConfig::Bool(true)),
                    "false" => Ok(SkipPushConfig::Bool(false)),
                    other => Ok(SkipPushConfig::Template(other.to_string())),
                }
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

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

/// `cargo publish` flag surface (DEC-10 — WAVE 3).
///
/// Presence under `publish:` opts the crate in; use `skip: true` (or a
/// truthy template) to opt out. There is no `enabled` field — presence is
/// the on-switch (DEC-6 / ITEM-3 hard-break).
///
/// Fields intentionally omitted because anodizer owns them (DEC-10):
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
}

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
    /// Formula directory in the tap (e.g. "Formula"). Matches GoReleaser `directory`.
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
    /// Homebrew Cask configuration (macOS .app bundles).
    pub cask: Option<HomebrewCaskConfig>,
    /// amd64 microarchitecture variant filter (e.g. "v1", "v2", "v3", "v4").
    /// Only artifacts matching this variant are included. Default: "v1".
    pub amd64_variant: Option<String>,
    /// ARM version filter (e.g. "6", "7"). Only artifacts matching this
    /// variant are included.
    pub arm_variant: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, JsonSchema)]
#[serde(default)]
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

/// Unified Homebrew Cask configuration (WAVE 4).
///
/// Used at both call-sites:
/// - `homebrew_casks:` — top-level array (GoReleaser parity); carries `repository`,
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
    /// Default: "Brew cask update for {{ .ProjectName }} version {{ .Tag }}"
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
    /// Binary stubs to create in /usr/local/bin (paths inside the .app bundle).
    pub binaries: Option<Vec<String>>,

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
}

/// Structured URL configuration for Homebrew Cask downloads.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
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
#[serde(default)]
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
#[serde(default)]
pub struct HomebrewCaskHooks {
    /// Pre-install/uninstall hooks.
    pub pre: Option<HomebrewCaskHook>,
    /// Post-install/uninstall hooks.
    pub post: Option<HomebrewCaskHook>,
}

/// Individual hook for install/uninstall phases.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct HomebrewCaskHook {
    /// Ruby code for preflight/postflight during install.
    pub install: Option<String>,
    /// Ruby code for uninstall_preflight/uninstall_postflight.
    pub uninstall: Option<String>,
}

/// Shell completion file paths for Homebrew Cask.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct HomebrewCaskCompletions {
    /// Path to bash completion file.
    pub bash: Option<String>,
    /// Path to zsh completion file.
    pub zsh: Option<String>,
    /// Path to fish completion file.
    pub fish: Option<String>,
}

/// Cask dependency (on another cask or formula).
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct HomebrewCaskDependencyEntry {
    /// Dependent cask name.
    pub cask: Option<String>,
    /// Dependent formula name.
    pub formula: Option<String>,
}

/// Cask conflict (with another cask or formula).
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct HomebrewCaskConflictEntry {
    /// Conflicting cask name.
    pub cask: Option<String>,
    /// Conflicting formula name (deprecated by Homebrew).
    pub formula: Option<String>,
}

/// Auto-generate shell completions from an executable.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
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
}

// `TapConfig` / `BucketConfig` (legacy {owner, name}-only repo types) live
// nowhere — every publisher now carries `repository: RepositoryConfig`
// with the broader feature set (token / branch / git SSH / pull_request).

// ---------------------------------------------------------------------------
// ChocolateyConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ChocolateyConfig {
    /// Override the package name (default: crate name).
    pub name: Option<String>,
    /// Build IDs filter: only include artifacts whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Unified project repo config (owner/name). Used to derive
    /// `<projectUrl>` (the Chocolatey gallery link) and download URLs.
    /// `<projectUrl>` resolves through `project_url:` (if set) → derived
    /// `https://github.com/{repository.owner}/{repository.name}`.
    pub repository: Option<RepositoryConfig>,
    /// URL shown as the package source in the Chocolatey gallery.
    pub package_source_url: Option<String>,
    /// Package owners (Chocolatey gallery user).
    pub owners: Option<String>,
    /// Package title (default: project name).
    pub title: Option<String>,
    /// Package author(s) displayed in the Chocolatey gallery.
    pub authors: Option<String>,
    /// Project homepage URL.
    pub project_url: Option<String>,
    /// Custom URL template for download URLs (overrides release URL).
    pub url_template: Option<String>,
    /// URL to the package icon image shown in the Chocolatey gallery.
    pub icon_url: Option<String>,
    /// Copyright notice.
    pub copyright: Option<String>,
    /// Package description (supports markdown).
    pub description: Option<String>,
    /// SPDX license identifier (e.g., "MIT", "Apache-2.0").
    pub license: Option<String>,
    /// Optional explicit license URL. Falls back to
    /// `https://opensource.org/licenses/<license>` when not set.
    pub license_url: Option<String>,
    /// Require license acceptance before install.
    pub require_license_acceptance: Option<bool>,
    /// Source code project URL.
    pub project_source_url: Option<String>,
    /// Documentation URL.
    pub docs_url: Option<String>,
    /// Bug tracker URL.
    pub bug_tracker_url: Option<String>,
    /// Tags for the Chocolatey gallery (joined with single spaces in the
    /// emitted nuspec). Always a typed list — the legacy
    /// space-separated-string form was dropped in WAVE 5.1 (DEC-11) for
    /// IDE-completion friendliness and to remove whitespace ambiguity.
    pub tags: Option<Vec<String>>,
    /// Short summary of the package.
    pub summary: Option<String>,
    /// Release notes for this version.
    pub release_notes: Option<String>,
    /// Package dependencies with optional version constraints.
    pub dependencies: Option<Vec<ChocolateyDependency>>,
    /// Chocolatey API key for `choco push`. Falls back to `CHOCOLATEY_API_KEY` env var.
    pub api_key: Option<String>,
    /// Push source URL (default: "https://push.chocolatey.org/").
    pub source_repo: Option<String>,
    /// Skip pushing to the Chocolatey community repository. Bool, string, or
    /// template expression (e.g. `"{{ .IsSnapshot }}"`). Accepts the legacy
    /// `skip_publish:` spelling for back-compat with pre-DEC-12 configs;
    /// canonical name is `skip:` to align with every other publisher.
    #[serde(
        default,
        alias = "skip_publish",
        deserialize_with = "deserialize_string_or_bool_opt"
    )]
    pub skip: Option<StringOrBool>,
    /// Artifact selection: "archive" (default), "msi", or "nsis".
    #[serde(rename = "use")]
    pub use_artifact: Option<String>,
    /// amd64 microarchitecture variant filter (e.g. "v1", "v2", "v3", "v4").
    /// Only artifacts matching this variant are included. Default: "v1".
    pub amd64_variant: Option<String>,
}

/// Chocolatey package dependency with optional version constraint.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ChocolateyDependency {
    /// Chocolatey package ID of the dependency.
    pub id: String,
    /// Minimum version constraint for the dependency (e.g., "[1.0.0,)").
    pub version: Option<String>,
}

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
    /// Artifact selection: "archive" (default), "msi", or "nsis".
    #[serde(rename = "use")]
    pub use_artifact: Option<String>,
    /// amd64 microarchitecture variant filter (e.g. "v1", "v2", "v3", "v4").
    /// Only artifacts matching this variant are included. Default: "v1".
    pub amd64_variant: Option<String>,
}

/// WinGet package dependency.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct WingetDependency {
    /// WinGet package identifier of the dependency (e.g., "Publisher.App").
    pub package_identifier: String,
    /// Minimum required version of the dependency.
    pub minimum_version: Option<String>,
}

// ---------------------------------------------------------------------------
// AurConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AurConfig {
    /// Override the package name (default: crate name + "-bin").
    pub name: Option<String>,
    /// Build IDs filter: only include artifacts whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Commit author with optional signing.
    pub commit_author: Option<CommitAuthorConfig>,
    /// Custom commit message template. Default: "Update to {{ version }}".
    pub commit_msg_template: Option<String>,
    /// Short description of the package for PKGBUILD.
    pub description: Option<String>,
    /// Project homepage URL.
    pub homepage: Option<String>,
    /// SPDX license identifier (e.g., "MIT", "Apache-2.0").
    pub license: Option<String>,
    /// Skip publishing. `"true"` always skips; `"auto"` skips for prereleases.
    /// Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip_upload: Option<StringOrBool>,
    /// Custom URL template for download URLs (overrides release URL).
    pub url_template: Option<String>,
    /// PKGBUILD maintainer entries (e.g., "Name <email@example.com>").
    pub maintainers: Option<Vec<String>>,
    /// Contributors listed in PKGBUILD comments.
    pub contributors: Option<Vec<String>>,
    /// Packages this PKGBUILD provides (virtual package names).
    pub provides: Option<Vec<String>>,
    /// Packages this PKGBUILD conflicts with.
    pub conflicts: Option<Vec<String>>,
    /// Runtime dependencies required by this package.
    pub depends: Option<Vec<String>>,
    /// Optional dependencies with descriptions (e.g., "fzf: fuzzy finder support").
    pub optdepends: Option<Vec<String>>,
    /// List of config files to preserve on upgrade (relative to `/`).
    pub backup: Option<Vec<String>>,
    /// Package release number (default: "1").
    pub rel: Option<String>,
    /// Custom PKGBUILD `package()` function body.
    pub package: Option<String>,
    /// AUR SSH git URL (e.g., `ssh://aur@aur.archlinux.org/<package>.git`).
    pub git_url: Option<String>,
    /// Custom SSH command for git operations.
    pub git_ssh_command: Option<String>,
    /// Path to SSH private key file.
    pub private_key: Option<String>,
    /// Subdirectory in the git repo for committed files.
    pub directory: Option<String>,
    /// Skip this AUR config. Accepts bool or template string
    /// (e.g. `"{{ if .IsSnapshot }}true{{ endif }}"` for conditional skip).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Content for a .install file (post-install/pre-remove scripts).
    pub install: Option<String>,
    // The PKGBUILD `url=` line resolves through `homepage:` →
    // crate metadata `homepage` → derived
    // `https://github.com/{release.github.owner}/{release.github.name}`.
    /// Packages this PKGBUILD replaces (for upgrade paths from old package names).
    pub replaces: Option<Vec<String>>,
    /// amd64 microarchitecture variant filter (e.g. "v1", "v2", "v3", "v4").
    /// Only artifacts matching this variant are included. Default: "v1".
    pub amd64_variant: Option<String>,
}

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

// Use `DockerV2Config` (canonical) for docker image builds.

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DockerRetryConfig {
    /// Number of retry attempts for failed docker push operations
    /// (default: 10, set in `crates/stage-docker/src/lib.rs::resolve_retry_settings`).
    pub attempts: Option<u32>,
    /// Duration string for the initial retry delay (default: `"10s"`).
    /// Examples: `"1s"`, `"500ms"`.
    pub delay: Option<String>,
    /// Maximum delay between retries (default: `"5m"`). Caps the exponential
    /// backoff so attempt-9 with a 10s base does not stretch to ~42 min.
    /// Example: `"30s"`.
    pub max_delay: Option<String>,
}

// ---------------------------------------------------------------------------
// DockerV2Config
// ---------------------------------------------------------------------------

/// Docker V2 configuration — the canonical Docker build API.
///
/// Notable surface:
/// - `images` + `tags` (cleaner separation than a single `image_templates` list)
/// - `annotations` map for OCI annotations (`--annotation`)
/// - `build_args` map for build-time variables
/// - `skip` as a [`StringOrBool`] template for conditional opt-out
/// - `sbom` as a [`StringOrBool`] — when truthy, adds `--sbom=true` to buildx
/// - `flags` for arbitrary extra `docker build` flags
/// - `platforms` is the only target selector — no per-arch field overrides
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DockerV2Config {
    /// Unique identifier for this Docker V2 config.
    pub id: Option<String>,
    /// Build IDs filter: only include binary artifacts whose metadata `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Path to the Dockerfile relative to the project root.
    pub dockerfile: String,
    /// Base image names (e.g., ["ghcr.io/owner/app"]). Combined with `tags` to form full references.
    pub images: Vec<String>,
    /// Tag suffixes (e.g., ["latest", "{{ .Version }}"]). Each image is tagged with each tag.
    pub tags: Vec<String>,
    /// OCI labels to apply to the image via `--label key=value` flags.
    pub labels: Option<HashMap<String, String>>,
    /// OCI annotations to apply via `--annotation key=value` flags.
    pub annotations: Option<HashMap<String, String>>,
    /// Extra files to copy into the Docker build context.
    pub extra_files: Option<Vec<String>>,
    /// Target platforms for multi-arch builds (e.g., ["linux/amd64", "linux/arm64"]).
    pub platforms: Option<Vec<String>>,
    /// Build arguments passed as `--build-arg KEY=VALUE`.
    pub build_args: Option<HashMap<String, String>>,
    /// Retry configuration for docker push operations.
    pub retry: Option<DockerRetryConfig>,
    /// Arbitrary extra flags passed to the docker build command.
    pub flags: Option<Vec<String>>,
    /// When truthy, skip this docker build entirely. Supports templates.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// When truthy, adds `--sbom=true` to buildx. Supports templates.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub sbom: Option<StringOrBool>,
    // No `skip_push` field — use the canonical `skip:` (DEC-6) to suppress
    // the publish step (matches every other publisher / pipe in anodizer).
}

// ---------------------------------------------------------------------------
// DockerDigestConfig
// ---------------------------------------------------------------------------

/// Controls docker image digest file creation.
///
/// After each docker image push, a digest file (containing the sha256 digest)
/// is written to the dist directory. This config controls whether that happens
/// and how the files are named.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DockerDigestConfig {
    /// When truthy, disable docker digest artifact creation.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Template for the digest artifact filename.
    /// Default: tag-based naming (e.g., "ghcr.io_owner_app_v1.0.0.digest").
    pub name_template: Option<String>,
}

// ---------------------------------------------------------------------------
// DockerManifestConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DockerManifestConfig {
    /// Template for the manifest name, e.g. "ghcr.io/owner/app:{{ .Version }}".
    pub name_template: String,
    /// Image references to include in the manifest.
    pub image_templates: Vec<String>,
    /// Extra flags for `docker manifest create`.
    pub create_flags: Option<Vec<String>>,
    /// Extra flags for `docker manifest push`.
    pub push_flags: Option<Vec<String>>,
    /// Skip push: true, false, or "auto" (skip for prereleases).
    #[schemars(schema_with = "skip_push_schema")]
    pub skip_push: Option<SkipPushConfig>,
    /// Unique identifier for this manifest config.
    pub id: Option<String>,
    /// Docker backend for manifest commands: "docker" (default) or "podman".
    #[serde(rename = "use")]
    pub use_backend: Option<String>,
    /// Retry configuration for manifest push (handles transient registry errors).
    pub retry: Option<DockerRetryConfig>,
}

// ---------------------------------------------------------------------------
// NfpmConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NfpmConfig {
    /// Unique identifier for cross-referencing this nFPM config.
    pub id: Option<String>,
    /// Package name (defaults to crate name).
    pub package_name: Option<String>,
    /// Package formats to produce: deb, rpm, apk, archlinux (at least one required).
    pub formats: Vec<String>,
    /// Package vendor name.
    pub vendor: Option<String>,
    /// Project homepage URL.
    pub homepage: Option<String>,
    /// Package maintainer in "Name <email>" format.
    pub maintainer: Option<String>,
    /// Package description (multiline supported).
    pub description: Option<String>,
    /// SPDX license identifier (e.g., "MIT", "Apache-2.0").
    pub license: Option<String>,
    /// Installation directory for binaries (default: /usr/bin).
    pub bindir: Option<String>,
    /// Files to include in the package beyond the main binary.
    pub contents: Option<Vec<NfpmContent>>,
    /// Runtime package dependencies keyed by format (e.g., {"deb": ["libc6"], "rpm": ["glibc"]}).
    pub dependencies: Option<HashMap<String, Vec<String>>>,
    /// Per-format setting overrides (e.g., {"deb": {compression: "xz"}}).
    pub overrides: Option<HashMap<String, serde_json::Value>>,
    /// Package filename template (supports templates).
    pub file_name_template: Option<String>,
    /// Package lifecycle scripts (preinstall, postinstall, preremove, postremove).
    pub scripts: Option<NfpmScripts>,
    /// Packages recommended (soft dependency) by this package.
    pub recommends: Option<Vec<String>>,
    /// Packages suggested (weaker than recommends) by this package.
    pub suggests: Option<Vec<String>>,
    /// Packages this package conflicts with.
    pub conflicts: Option<Vec<String>>,
    /// Packages this package replaces (for upgrade paths from old package names).
    pub replaces: Option<Vec<String>>,
    /// Virtual packages provided by this package.
    pub provides: Option<Vec<String>>,
    /// Build IDs filter: only include artifacts from builds whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Package epoch for versioning (integer as string).
    pub epoch: Option<String>,
    /// Package release number.
    pub release: Option<String>,
    /// Prerelease version suffix.
    pub prerelease: Option<String>,
    /// Version metadata (e.g. git commit hash).
    pub version_metadata: Option<String>,
    /// Package section (e.g. "utils", "devel").
    pub section: Option<String>,
    /// Package priority (e.g. "optional", "required").
    pub priority: Option<String>,
    /// Whether this is a meta-package (no files, only dependencies).
    pub meta: Option<bool>,
    /// File permission umask. Accepts a YAML int (`18`), an octal-prefixed
    /// string (`"0o022"`), or a leading-zero octal string (`"022"`).
    pub umask: Option<StringOrU32>,
    /// Default modification time for files in the package.
    pub mtime: Option<String>,
    /// RPM-specific configuration.
    pub rpm: Option<NfpmRpmConfig>,
    /// Deb-specific configuration.
    pub deb: Option<NfpmDebConfig>,
    /// APK-specific configuration.
    pub apk: Option<NfpmApkConfig>,
    /// Archlinux-specific configuration.
    pub archlinux: Option<NfpmArchlinuxConfig>,
    /// IPK-specific configuration (OpenWrt packages).
    pub ipk: Option<NfpmIpkConfig>,
    /// CGo library installation directories (header, carchive, cshared).
    pub libdirs: Option<NfpmLibdirs>,
    /// Path to a YAML-format changelog file for deb/rpm packages.
    pub changelog: Option<String>,
    /// Template-conditional: skip this nfpm config if rendered result is "false" or empty.
    /// (GoReleaser Pro v2.4+.)
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
    /// Extra file contents whose source files are Tera-rendered before packaging (GoReleaser Pro).
    /// Each entry mirrors `contents`; the difference is that at stage time the file at `src` is
    /// read, rendered through the template engine, written to a temp file, and then included
    /// in the package at `dst` using the temp file as the real source. Useful for shipping
    /// config files with templated values (version, commit, maintainer, etc.).
    pub templated_contents: Option<Vec<NfpmContent>>,
    /// Lifecycle scripts whose script-file bodies are Tera-rendered before packaging
    /// (GoReleaser Pro). Each path is read, rendered through the template engine, written to
    /// a temp file, and used as the real script. If a field is set on both `scripts` and
    /// `templated_scripts`, the templated version wins.
    pub templated_scripts: Option<NfpmScripts>,
}

/// Installation directories for CGo library outputs.
///
/// Controls where header files, static archives, and shared libraries
/// are installed in the package.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NfpmLibdirs {
    /// Installation directory for C header files.
    pub header: Option<String>,
    /// Installation directory for carchive (.a) static libraries.
    pub carchive: Option<String>,
    /// Installation directory for cshared (.so / .dylib) shared libraries.
    pub cshared: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NfpmScripts {
    /// Path to script run before package installation.
    pub preinstall: Option<String>,
    /// Path to script run after package installation.
    pub postinstall: Option<String>,
    /// Path to script run before package removal.
    pub preremove: Option<String>,
    /// Path to script run after package removal.
    pub postremove: Option<String>,
}

/// Backward-compatible alias — nFPM contents share the same `FileInfo` struct.
pub type NfpmFileInfo = FileInfo;

/// A single file/directory entry in an nFPM (or SRPM) package's `contents`
/// list. SCH-8 (WAVE 5.4) merged the formerly-separate `NfpmContentConfig`
/// (used for SRPM) into this struct — `source` / `destination` / `type` are
/// accepted as aliases for `src` / `dst` / the renamed `type` so srpm-style
/// keys still parse.
///
/// `Default` is intentionally **not** derived because `src` and `dst` are
/// required fields with no meaningful defaults — forcing callers to provide
/// them explicitly prevents accidentally packaging empty paths.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NfpmContent {
    /// Source path on the build machine (supports glob patterns).
    pub src: String,
    /// Destination path inside the package (absolute path).
    pub dst: String,
    /// Content entry type: "config", "config|noreplace", "doc", "dir", "symlink", "ghost", or empty for regular file.
    #[serde(rename = "type")]
    pub content_type: Option<String>,
    /// File ownership and permission metadata.
    pub file_info: Option<NfpmFileInfo>,
    /// Per-packager filter: only include this content entry for the specified packager
    /// (e.g. "deb", "rpm", "apk").
    pub packager: Option<String>,
    /// When true, expand template variables in the `src` and `dst` paths.
    pub expand: Option<bool>,
}

// ---------------------------------------------------------------------------
// nFPM format-specific configs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NfpmRpmConfig {
    /// One-line package summary (RPM Summary tag).
    pub summary: Option<String>,
    /// RPM compression algorithm (e.g. "lzma", "gzip", "xz", "zstd").
    pub compression: Option<String>,
    /// RPM group classification (e.g. "System/Tools").
    pub group: Option<String>,
    /// RPM packager identity (e.g. "Build Team <build@example.com>").
    pub packager: Option<String>,
    /// Relocatable RPM prefix paths (e.g. ["/usr", "/etc"]).
    pub prefixes: Option<Vec<String>>,
    /// RPM signing configuration.
    pub signature: Option<NfpmSignatureConfig>,
    /// RPM-specific lifecycle scripts (pretrans/posttrans).
    pub scripts: Option<NfpmRpmScripts>,
    /// RPM BuildHost tag value.
    pub build_host: Option<String>,
}

/// RPM-specific transaction scripts that run outside the normal install/remove lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NfpmRpmScripts {
    /// Script to run before the RPM transaction begins.
    pub pretrans: Option<String>,
    /// Script to run after the RPM transaction completes.
    pub posttrans: Option<String>,
}

impl NfpmRpmConfig {
    /// Returns `true` when every field is `None` — the YAML section would be
    /// empty and should be omitted.
    pub fn is_empty(&self) -> bool {
        self.summary.is_none()
            && self.compression.is_none()
            && self.group.is_none()
            && self.packager.is_none()
            && self.prefixes.is_none()
            && self.signature.is_none()
            && self.scripts.is_none()
            && self.build_host.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NfpmDebConfig {
    /// Deb compression algorithm (e.g. "gzip", "xz", "zstd", "none").
    pub compression: Option<String>,
    /// Pre-dependency packages (stronger than Depends).
    pub predepends: Option<Vec<String>>,
    /// Deb trigger definitions.
    pub triggers: Option<NfpmDebTriggers>,
    /// Packages this package breaks (Breaks relationship).
    pub breaks: Option<Vec<String>>,
    /// Lintian overrides to embed in the package.
    pub lintian_overrides: Option<Vec<String>>,
    /// Deb signing configuration.
    pub signature: Option<NfpmSignatureConfig>,
    /// Additional control fields (e.g. Bugs, Built-Using).
    pub fields: Option<HashMap<String, String>>,
    /// Deb-specific maintainer scripts (rules, templates, config).
    pub scripts: Option<NfpmDebScripts>,
    /// amd64 microarchitecture variant propagated to nfpm's `deb.arch_variant`
    /// (`v1`, `v2`, `v3`, `v4`). Auto-derived from artifact metadata's
    /// `amd64_variant` when unset.
    pub arch_variant: Option<String>,
}

/// Deb-specific maintainer scripts for package configuration and rules.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NfpmDebScripts {
    /// Path to debian/rules file.
    pub rules: Option<String>,
    /// Path to debian/templates file (debconf templates).
    pub templates: Option<String>,
    /// Path to debian/config script (debconf configuration).
    pub config: Option<String>,
}

impl NfpmDebConfig {
    /// Returns `true` when every field is `None` — the YAML section would be
    /// empty and should be omitted.
    pub fn is_empty(&self) -> bool {
        self.compression.is_none()
            && self.predepends.is_none()
            && self.triggers.is_none()
            && self.breaks.is_none()
            && self.lintian_overrides.is_none()
            && self.signature.is_none()
            && self.fields.is_none()
            && self.scripts.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NfpmDebTriggers {
    /// Deb interest triggers: package waits for these triggers to complete.
    pub interest: Option<Vec<String>>,
    /// Deb interest-await triggers: package waits with synchronous trigger processing.
    pub interest_await: Option<Vec<String>>,
    /// Deb interest-noawait triggers: package registers interest without waiting.
    pub interest_noawait: Option<Vec<String>>,
    /// Deb activate triggers: package activates these triggers after install.
    pub activate: Option<Vec<String>>,
    /// Deb activate-await triggers: activate and wait for synchronous trigger processing.
    pub activate_await: Option<Vec<String>>,
    /// Deb activate-noawait triggers: activate without waiting.
    pub activate_noawait: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NfpmApkConfig {
    /// APK signing configuration.
    pub signature: Option<NfpmSignatureConfig>,
    /// APK-specific lifecycle scripts (preupgrade/postupgrade).
    pub scripts: Option<NfpmApkScripts>,
}

/// APK-specific upgrade lifecycle scripts.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NfpmApkScripts {
    /// Script to run before upgrading an existing package.
    pub preupgrade: Option<String>,
    /// Script to run after upgrading an existing package.
    pub postupgrade: Option<String>,
}

impl NfpmApkConfig {
    /// Returns `true` when every field is `None` — the YAML section would be
    /// empty and should be omitted.
    pub fn is_empty(&self) -> bool {
        self.signature.is_none() && self.scripts.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NfpmArchlinuxConfig {
    /// Base package name for split packages.
    pub pkgbase: Option<String>,
    /// Packager identity (e.g. "Build Team <build@example.com>").
    pub packager: Option<String>,
    /// Archlinux-specific lifecycle scripts.
    pub scripts: Option<NfpmArchlinuxScripts>,
}

impl NfpmArchlinuxConfig {
    /// Returns `true` when every field is `None` — the YAML section would be
    /// empty and should be omitted.
    pub fn is_empty(&self) -> bool {
        self.pkgbase.is_none() && self.packager.is_none() && self.scripts.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NfpmArchlinuxScripts {
    /// Script to run before upgrading an existing package.
    pub preupgrade: Option<String>,
    /// Script to run after upgrading an existing package.
    pub postupgrade: Option<String>,
}

/// IPK (OpenWrt) package-specific configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NfpmIpkConfig {
    /// ABI version string for the package.
    pub abi_version: Option<String>,
    /// Alternative file links managed by the update-alternatives system.
    pub alternatives: Option<Vec<NfpmIpkAlternative>>,
    /// Whether the package was automatically installed as a dependency.
    pub auto_installed: Option<bool>,
    /// Whether the package is essential for the system.
    pub essential: Option<bool>,
    /// Strong pre-dependencies that must be fully installed before this package.
    pub predepends: Option<Vec<String>>,
    /// Tags for categorizing the package.
    pub tags: Option<Vec<String>>,
    /// Additional control fields as key-value pairs.
    pub fields: Option<HashMap<String, String>>,
}

impl NfpmIpkConfig {
    /// Returns `true` when every field is `None` — the YAML section would be
    /// empty and should be omitted.
    pub fn is_empty(&self) -> bool {
        self.abi_version.is_none()
            && self.alternatives.is_none()
            && self.auto_installed.is_none()
            && self.essential.is_none()
            && self.predepends.is_none()
            && self.tags.is_none()
            && self.fields.is_none()
    }
}

/// An alternative file link for IPK's update-alternatives system.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NfpmIpkAlternative {
    /// Priority for alternative selection (higher wins).
    pub priority: Option<i32>,
    /// Target file path that the alternative points to.
    pub target: Option<String>,
    /// Symlink name in the alternatives directory.
    pub link_name: Option<String>,
}

/// Unified signature configuration shared by nFPM (deb/rpm/apk) and SRPM
/// packages — SRPM's surface is a strict subset, so a single struct covers
/// both. The legacy SRPM `passphrase:` key is accepted as a serde alias
/// for `key_passphrase:` so both spellings parse.
///
/// GR keeps three distinct signature types (`NFPMRPMSignature`,
/// `NFPMDebSignature`, `NFPMAPKSignature`) with overlapping but slightly
/// different fields. Anodizer's union here avoids the 3-struct cascade
/// when 90% of fields overlap.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NfpmSignatureConfig {
    /// Path to the signing key file.
    pub key_file: Option<String>,
    /// Key ID to use for signing.
    pub key_id: Option<String>,
    /// Passphrase for the signing key. Falls back to `NFPM_PASSPHRASE` /
    /// `SRPM_PASSPHRASE` env vars in their respective stages.
    pub key_passphrase: Option<String>,
    /// Public key name for APK signatures (defaults to `<maintainer email>.rsa.pub`).
    pub key_name: Option<String>,
    /// Signature type for deb packages: "origin", "maint", or "archive" (default: "origin").
    #[serde(rename = "type")]
    pub type_: Option<String>,
}

// ---------------------------------------------------------------------------
// SnapcraftConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SnapcraftConfig {
    /// Unique identifier for this snapcraft config.
    pub id: Option<String>,
    /// Build IDs to include. Empty means all builds.
    pub ids: Option<Vec<String>>,
    /// Snap package name in the store.
    pub name: Option<String>,
    /// Canonical application title (user-facing in store).
    pub title: Option<String>,
    /// Single-line elevator pitch (max 79 characters).
    pub summary: Option<String>,
    /// Extended description (user-facing in store).
    pub description: Option<String>,
    /// Path to icon image file.
    pub icon: Option<String>,
    /// Runtime base snap: core, core18, core20, core22, core24, bare.
    pub base: Option<String>,
    /// Release stability level: stable, devel.
    pub grade: Option<String>,
    /// License identifier (SPDX format).
    pub license: Option<String>,
    /// Whether to publish to the snapcraft store.
    pub publish: Option<bool>,
    /// Distribution channels: edge, beta, candidate, stable.
    pub channel_templates: Option<Vec<String>>,
    /// Security confinement level: strict, devmode, classic.
    pub confinement: Option<String>,
    /// Top-level snap plug definitions (structured map).
    /// Keys are plug names, values are either `null` (simple plug) or an object
    /// with `interface` and optional attributes (e.g. `{ interface: "content", target: "$SNAP/shared" }`).
    /// GoReleaser uses `map[string]any` for this field.
    pub plugs: Option<HashMap<String, serde_json::Value>>,
    // No top-level `slots:` — Snapcraft itself has no top-level slots
    // concept; use `apps.<name>.slots` for per-app slots.
    /// Required snapd features/versions.
    pub assumes: Option<Vec<String>>,
    /// Application configurations defining daemons, commands, env vars.
    pub apps: Option<HashMap<String, SnapcraftApp>>,
    /// Directory mappings for sandbox accessibility.
    pub layouts: Option<HashMap<String, SnapcraftLayout>>,
    /// Additional static files to bundle (string shorthand or structured form).
    pub extra_files: Option<Vec<SnapcraftExtraFileSpec>>,
    /// Extra files whose contents are rendered through the template engine before bundling.
    /// Unlike `extra_files` which copy as-is, template variables like `{{ .Tag }}` are expanded.
    /// GoReleaser Pro feature.
    pub templated_extra_files: Option<Vec<TemplatedExtraFile>>,
    /// Template for the output snap filename.
    pub name_template: Option<String>,
    /// Skip this snapcraft config. Accepts bool or template string
    /// (e.g. `"{{ if .IsSnapshot }}true{{ endif }}"` for conditional skip).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Remove source archives from artifacts, keeping only snap.
    pub replace: Option<bool>,
    /// Output timestamp for reproducible builds.
    pub mod_timestamp: Option<String>,
    /// Snap hooks — maps hook name to arbitrary hook config.
    pub hooks: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct SnapcraftApp {
    /// Command to run (relative to snap root).
    pub command: Option<String>,
    /// Daemon type: simple, forking, oneshot, notify, dbus.
    pub daemon: Option<String>,
    /// How to stop the daemon: sigterm, sigkill, etc.
    #[serde(alias = "stop-mode")]
    pub stop_mode: Option<String>,
    /// Interface plugs the app needs.
    pub plugs: Option<Vec<String>>,
    /// Environment variables for the app (supports string, integer, and boolean values).
    pub environment: Option<HashMap<String, serde_json::Value>>,
    /// Additional arguments passed to the command.
    pub args: Option<String>,
    /// Restart condition: on-failure, always, on-success, on-abnormal, on-abort, on-watchdog, never.
    #[serde(alias = "restart-condition")]
    pub restart_condition: Option<String>,
    /// Snap adapter type: "none" or "full" (default: "full").
    pub adapter: Option<String>,
    /// Services that must start before this app.
    pub after: Option<Vec<String>>,
    /// Alternative names for the command.
    pub aliases: Option<Vec<String>>,
    /// Desktop file for autostart.
    pub autostart: Option<String>,
    /// Services that must start after this app.
    pub before: Option<Vec<String>>,
    /// D-Bus well-known bus name.
    #[serde(alias = "bus-name")]
    pub bus_name: Option<String>,
    /// Wrapper commands run before the main command.
    #[serde(alias = "command-chain")]
    pub command_chain: Option<Vec<String>>,
    /// AppStream metadata common ID.
    #[serde(alias = "common-id")]
    pub common_id: Option<String>,
    /// Path to bash completion script relative to snap.
    pub completer: Option<String>,
    /// Path to .desktop file relative to snap.
    pub desktop: Option<String>,
    /// Snap extensions to apply.
    pub extensions: Option<Vec<String>>,
    /// Installation mode: "enable" or "disable".
    #[serde(alias = "install-mode")]
    pub install_mode: Option<String>,
    /// Arbitrary YAML passed through to snap.yaml.
    pub passthrough: Option<HashMap<String, serde_json::Value>>,
    /// Command to run after daemon stops.
    #[serde(alias = "post-stop-command")]
    pub post_stop_command: Option<String>,
    /// Refresh behavior: "endure" or "restart".
    #[serde(alias = "refresh-mode")]
    pub refresh_mode: Option<String>,
    /// Command to reload daemon config.
    #[serde(alias = "reload-command")]
    pub reload_command: Option<String>,
    /// Delay between restarts (duration string).
    #[serde(alias = "restart-delay")]
    pub restart_delay: Option<String>,
    /// Interface slots this app provides.
    pub slots: Option<Vec<String>>,
    /// Socket definitions map.
    pub sockets: Option<HashMap<String, serde_json::Value>>,
    /// Start timeout duration string.
    #[serde(alias = "start-timeout")]
    pub start_timeout: Option<String>,
    /// Command to gracefully stop the daemon.
    #[serde(alias = "stop-command")]
    pub stop_command: Option<String>,
    /// Stop timeout duration string.
    #[serde(alias = "stop-timeout")]
    pub stop_timeout: Option<String>,
    /// Timer definition (systemd timer syntax).
    pub timer: Option<String>,
    /// Watchdog timeout duration string.
    #[serde(alias = "watchdog-timeout")]
    pub watchdog_timeout: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct SnapcraftLayout {
    /// Bind-mount a directory to the snap's layout.
    pub bind: Option<String>,
    /// Bind-mount a single file to the snap's layout.
    pub bind_file: Option<String>,
    /// Symlink a path to a location in the snap.
    pub symlink: Option<String>,
    /// Layout entry type.
    #[serde(rename = "type")]
    pub type_: Option<String>,
}

/// Specifies an extra file for snapcraft. Can be a simple source path string or
/// a structured object with source, destination, and mode fields (matching
/// GoReleaser's SnapcraftExtraFiles).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum SnapcraftExtraFileSpec {
    /// Simple source path string.
    Source(String),
    /// Structured form with source, destination, and mode.
    Detailed {
        source: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        destination: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        mode: Option<u32>,
    },
}

impl SnapcraftExtraFileSpec {
    /// Return the source path for this spec.
    pub fn source(&self) -> &str {
        match self {
            SnapcraftExtraFileSpec::Source(s) => s,
            SnapcraftExtraFileSpec::Detailed { source, .. } => source,
        }
    }

    /// Return the optional destination path.
    pub fn destination(&self) -> Option<&str> {
        match self {
            SnapcraftExtraFileSpec::Source(_) => None,
            SnapcraftExtraFileSpec::Detailed { destination, .. } => destination.as_deref(),
        }
    }

    /// Return the optional file mode.
    pub fn mode(&self) -> Option<u32> {
        match self {
            SnapcraftExtraFileSpec::Source(_) => None,
            SnapcraftExtraFileSpec::Detailed { mode, .. } => *mode,
        }
    }
}

// ---------------------------------------------------------------------------
// DmgConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DmgConfig {
    /// Unique identifier for this DMG config.
    pub id: Option<String>,
    /// Build IDs to include. Empty means all builds.
    pub ids: Option<Vec<String>>,
    /// Output DMG filename (supports templates).
    pub name: Option<String>,
    /// Additional files to include in the DMG (glob or {glob, name_template}).
    pub extra_files: Option<Vec<ExtraFileSpec>>,
    /// Extra files whose contents are rendered through the template engine before inclusion.
    /// Unlike `extra_files` which copy as-is, template variables like `{{ .Tag }}` are expanded.
    /// GoReleaser Pro feature.
    pub templated_extra_files: Option<Vec<TemplatedExtraFile>>,
    /// Remove source archives from artifacts, keeping only DMG.
    pub replace: Option<bool>,
    /// Output timestamp for reproducible builds.
    pub mod_timestamp: Option<String>,
    /// Skip this DMG config. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Which artifact type to package: "binary" (default) or "appbundle".
    #[serde(rename = "use")]
    pub use_: Option<String>,
    /// Template-conditional: skip this DMG config if rendered result is "false"
    /// or empty (GoReleaser Pro). Render failure hard-errors (not silent-skip).
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
}

// ---------------------------------------------------------------------------
// MsiConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct MsiConfig {
    /// Unique identifier for this MSI config.
    pub id: Option<String>,
    /// Build IDs to include. Empty means all builds.
    pub ids: Option<Vec<String>>,
    /// Path to the WiX source file (.wxs). Goes through template engine. Required.
    pub wxs: Option<String>,
    /// Output MSI filename (supports templates).
    pub name: Option<String>,
    /// WiX schema version: v3 or v4 (auto-detected from .wxs if omitted).
    pub version: Option<String>,
    /// Remove source archives from artifacts, keeping only MSI.
    pub replace: Option<bool>,
    /// Output timestamp for reproducible builds.
    pub mod_timestamp: Option<String>,
    /// Skip this MSI config. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Additional files available in the WiX build context (simple filenames).
    pub extra_files: Option<Vec<String>>,
    /// WiX extensions to enable (e.g., "WixUIExtension"). Templates allowed.
    pub extensions: Option<Vec<String>>,
    /// Template-conditional: skip this MSI config if rendered result is "false"
    /// or empty (GoReleaser Pro). Render failure hard-errors (not silent-skip).
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
    /// Pre/post MSI-build hooks (GoReleaser Pro v2.14+). Accepts `pre`/`post`
    /// or `before`/`after` via BuildHooksConfig's serde aliases. Runs before
    /// / after candle+light for each matched artifact.
    pub hooks: Option<BuildHooksConfig>,
}

// ---------------------------------------------------------------------------
// PkgConfig (macOS .pkg installer)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct PkgConfig {
    /// Unique identifier for this PKG config.
    pub id: Option<String>,
    /// Build IDs to include. Empty means all builds.
    pub ids: Option<Vec<String>>,
    /// Package identifier in reverse-domain notation (e.g. com.example.myapp). Required.
    pub identifier: Option<String>,
    /// Output PKG filename (supports templates).
    pub name: Option<String>,
    /// Installation path. Default: /usr/local/bin.
    pub install_location: Option<String>,
    /// Path to scripts directory containing preinstall/postinstall scripts.
    pub scripts: Option<String>,
    /// Additional files to include in the package (glob or {glob, name_template}).
    pub extra_files: Option<Vec<ExtraFileSpec>>,
    /// Remove source archives from artifacts, keeping only PKG.
    pub replace: Option<bool>,
    /// Output timestamp for reproducible builds.
    pub mod_timestamp: Option<String>,
    /// Which artifact type to package: "binary" (default) or "appbundle".
    #[serde(rename = "use")]
    pub use_: Option<String>,
    /// Minimum macOS version (e.g. "10.13"). Forwarded to `productbuild --min-os-version`.
    pub min_os_version: Option<String>,
    /// Skip this PKG config. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Template-conditional: skip this PKG config if rendered result is "false"
    /// or empty (GoReleaser Pro). Render failure hard-errors (not silent-skip).
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
}

// ---------------------------------------------------------------------------
// NsisConfig (Windows NSIS installer)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NsisConfig {
    /// Unique identifier for this NSIS config.
    pub id: Option<String>,
    /// Build IDs to include. Empty means all builds.
    pub ids: Option<Vec<String>>,
    /// Output installer filename (supports templates).
    pub name: Option<String>,
    /// Path to the NSIS script template (.nsi). Goes through template engine.
    pub script: Option<String>,
    /// Additional files to include alongside the installer (glob or {glob, name_template}).
    pub extra_files: Option<Vec<ExtraFileSpec>>,
    /// Extra files whose contents are rendered through the template engine before inclusion.
    /// Unlike `extra_files` which copy as-is, template variables like `{{ .Tag }}` are expanded.
    /// GoReleaser Pro feature.
    pub templated_extra_files: Option<Vec<TemplatedExtraFile>>,
    /// Skip this NSIS config. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Remove source archives from artifacts, keeping only the installer.
    pub replace: Option<bool>,
    /// Output timestamp for reproducible builds.
    pub mod_timestamp: Option<String>,
    /// Template-conditional: skip this NSIS config if rendered result is "false"
    /// or empty (GoReleaser Pro). Render failure hard-errors (not silent-skip).
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
}

// ---------------------------------------------------------------------------
// AppBundleConfig (macOS .app bundle)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct AppBundleConfig {
    /// Unique identifier for this app bundle config.
    pub id: Option<String>,
    /// Build IDs to include. Empty means all builds.
    pub ids: Option<Vec<String>>,
    /// Output .app bundle name (supports templates).
    pub name: Option<String>,
    /// Path to .icns icon file for the app bundle (supports templates).
    pub icon: Option<String>,
    /// Bundle identifier in reverse-DNS notation (e.g. com.example.myapp). Required.
    pub bundle: Option<String>,
    /// Additional files to include in the bundle (src/dst/info objects or glob strings).
    pub extra_files: Option<Vec<ArchiveFileSpec>>,
    /// Extra files whose contents are rendered through the template engine before inclusion.
    /// Unlike `extra_files` which copy as-is, template variables like `{{ .Tag }}` are expanded.
    /// GoReleaser Pro feature.
    pub templated_extra_files: Option<Vec<TemplatedExtraFile>>,
    /// Output timestamp for reproducible builds.
    pub mod_timestamp: Option<String>,
    /// Remove source archives from artifacts, keeping only the app bundle.
    pub replace: Option<bool>,
    /// Skip this app bundle config. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Template-conditional: skip this app bundle config if rendered result is
    /// "false" or empty (GoReleaser Pro). Render failure hard-errors (not silent-skip).
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
}

// ---------------------------------------------------------------------------
// FlatpakConfig (Linux Flatpak bundle)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct FlatpakConfig {
    /// Unique identifier for this Flatpak config.
    pub id: Option<String>,
    /// Build IDs to include. Empty means all builds.
    pub ids: Option<Vec<String>>,
    /// Output .flatpak filename (supports templates).
    pub name_template: Option<String>,
    /// Flatpak application ID in reverse-DNS notation (e.g. org.example.MyApp). Required.
    pub app_id: Option<String>,
    /// Flatpak runtime (e.g. org.freedesktop.Platform). Required.
    pub runtime: Option<String>,
    /// Flatpak runtime version (e.g. "24.08"). Required.
    pub runtime_version: Option<String>,
    /// Flatpak SDK (e.g. org.freedesktop.Sdk). Required.
    pub sdk: Option<String>,
    /// Command to run inside the Flatpak sandbox. Defaults to first binary name.
    pub command: Option<String>,
    /// Sandbox permissions (e.g. --share=network, --socket=x11).
    pub finish_args: Option<Vec<String>>,
    /// Additional files to include alongside the binary (glob or {glob, name_template}).
    pub extra_files: Option<Vec<ExtraFileSpec>>,
    /// Remove source archives from artifacts, keeping only the Flatpak bundle.
    pub replace: Option<bool>,
    /// Output timestamp for reproducible builds.
    pub mod_timestamp: Option<String>,
    /// Skip this Flatpak config. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
}

// ---------------------------------------------------------------------------
// BlobConfig (S3/GCS/Azure cloud storage)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct BlobConfig {
    /// Unique identifier for this blob config.
    pub id: Option<String>,
    /// Cloud storage provider: s3, gcs (or gs), or azblob (or azure).
    pub provider: String,
    /// Bucket or container name (supports templates).
    pub bucket: String,
    /// Directory/folder within the bucket (supports templates).
    /// Default: `{{ ProjectName }}/{{ Tag }}`.
    pub directory: Option<String>,
    /// AWS region (S3 only).
    pub region: Option<String>,
    /// Custom endpoint URL for S3-compatible storage (e.g. MinIO, R2, DO Spaces).
    pub endpoint: Option<String>,
    /// Disable SSL for the connection (S3 only, default: false).
    pub disable_ssl: Option<bool>,
    /// Enable path-style addressing for S3-compatible backends.
    /// Defaults to `true` when `endpoint` is set (MinIO, R2, DO Spaces need this),
    /// `false` otherwise (standard AWS virtual-hosted style).
    pub s3_force_path_style: Option<bool>,
    /// Canned ACL for uploaded objects.
    /// **S3**: one of `private` (default), `public-read`, `public-read-write`,
    /// `authenticated-read`, `aws-exec-read`, `bucket-owner-read`,
    /// `bucket-owner-full-control`. Matches GoReleaser's accepted set;
    /// AWS's `log-delivery-write` is intentionally omitted because it is only
    /// valid on `S3LogBucket` targets and would silently fail on a normal bucket.
    /// **GCS**: pass the camelCase predefined-ACL name (e.g. `publicRead`,
    /// `bucketOwnerFullControl`); not validated up-front, so a typo surfaces
    /// as a 400 from the GCS API at upload time.
    pub acl: Option<String>,
    /// HTTP Cache-Control header values, joined with ", " when uploading.
    /// Accepts a string (single value) or array of strings in YAML.
    #[serde(deserialize_with = "deserialize_string_or_vec_opt", default)]
    pub cache_control: Option<Vec<String>>,
    /// HTTP Content-Disposition header (supports templates).
    /// Default: `"attachment;filename={{Filename}}"`. Set to `"-"` to disable.
    pub content_disposition: Option<String>,
    /// AWS KMS encryption key for server-side encryption (S3 only).
    pub kms_key: Option<String>,
    /// Build IDs to include. Empty means all artifacts.
    pub ids: Option<Vec<String>>,
    /// Skip this blob config. Accepts bool or template string
    /// (e.g. `"{{ if IsSnapshot }}true{{ endif }}"` for conditional skip).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Also upload metadata.json and artifacts.json.
    pub include_meta: Option<bool>,
    /// Pre-existing files to upload (supports glob patterns).
    pub extra_files: Option<Vec<ExtraFileSpec>>,
    /// Extra files whose contents are rendered through the template engine before upload.
    /// Unlike `extra_files` which copy as-is, template variables like `{{ .Tag }}` are expanded.
    /// GoReleaser Pro feature.
    pub templated_extra_files: Option<Vec<TemplatedExtraFile>>,
    /// Upload only extra files (skip artifacts).
    pub extra_files_only: Option<bool>,
    /// Maximum number of parallel uploads for this blob config.
    /// Overrides the global `--parallelism` setting when set.
    pub parallelism: Option<usize>,
}

// ---------------------------------------------------------------------------
// PartialConfig (split/merge CI fan-out)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct PartialConfig {
    /// How to split builds: "goos" (by OS, default) or "target" (by full triple).
    /// "goos" groups all arch variants for the same OS into one split job.
    /// "target" gives each unique target triple its own split job.
    pub by: Option<String>,
}

// ---------------------------------------------------------------------------
// BinstallConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct BinstallConfig {
    /// When true, generate a .cargo/config.toml binstall section for cargo-binstall.
    pub enabled: Option<bool>,
    /// Custom download URL template for cargo-binstall (supports templates).
    pub pkg_url: Option<String>,
    /// Directory within the archive where binaries are located.
    pub bin_dir: Option<String>,
    /// Package format hint for cargo-binstall: tgz, tar.gz, tar.xz, zip, bin, etc.
    pub pkg_fmt: Option<String>,
}

// ---------------------------------------------------------------------------
// NotarizeConfig (macOS code signing and notarization)
// ---------------------------------------------------------------------------

/// Top-level notarization configuration supporting both cross-platform
/// (`rcodesign`) and native macOS (`codesign` + `xcrun notarytool`) modes.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NotarizeConfig {
    /// Skip all notarization. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Cross-platform signing/notarization (rcodesign-based, works on any OS).
    pub macos: Option<Vec<MacOSSignNotarizeConfig>>,
    /// Native signing/notarization (codesign + xcrun, macOS only).
    pub macos_native: Option<Vec<MacOSNativeSignNotarizeConfig>>,
}

/// Cross-platform macOS signing and notarization via `rcodesign`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MacOSSignNotarizeConfig {
    /// Build IDs to filter. Default: project name.
    pub ids: Option<Vec<String>>,
    /// Skip this configuration. Accepts bool or template string. SCH-30
    /// (WAVE 5.5) replaced the previous `enabled:` toggle with the canonical
    /// `skip:` (inverted semantic) to align with every other publisher /
    /// pipe in anodizer (DEC-6).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Signing configuration (P12 certificate).
    pub sign: Option<MacOSSignConfig>,
    /// Notarization configuration (App Store Connect API key). Omit for sign-only.
    pub notarize: Option<MacOSNotarizeApiConfig>,
}

/// P12-certificate signing configuration for `rcodesign sign`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct MacOSSignConfig {
    /// Path to .p12 certificate file or base64-encoded contents. Templates allowed.
    pub certificate: Option<String>,
    /// Password for the .p12 certificate. Templates allowed.
    pub password: Option<String>,
    /// Path to entitlements XML file. Templates allowed.
    pub entitlements: Option<String>,
    /// RFC-3161 timestamp service URL passed to `rcodesign sign --timestamp-url`.
    /// Defaults to Apple's public timestamp service. Override when running
    /// behind a corporate proxy or when Apple's service is unreachable.
    pub timestamp_url: Option<String>,
}

/// App Store Connect API key configuration for `rcodesign notary-submit`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct MacOSNotarizeApiConfig {
    /// App Store Connect API key issuer UUID. Templates allowed.
    pub issuer_id: Option<String>,
    /// Path to .p8 key file or base64-encoded contents. Templates allowed.
    pub key: Option<String>,
    /// API key ID. Templates allowed.
    pub key_id: Option<String>,
    /// Timeout for notarization status polling. Humantime-style string
    /// (e.g. `"10m"`, `"15s"`, `"1h"`). Default when omitted: `"10m"`.
    pub timeout: Option<HumanDuration>,
    /// Whether to wait for notarization to complete.
    pub wait: Option<bool>,
}

/// Artifact-type selector for native macOS notarization. Constrains the YAML
/// `use:` field on `notarize.macos_native` so an unsupported value fails at
/// parse time. Only `dmg` and `pkg` are valid — `notarytool` (the only
/// supported tool) is implicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MacOSNativeArtifactKind {
    Dmg,
    Pkg,
}

/// Native macOS signing and notarization via `codesign` + `xcrun notarytool`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MacOSNativeSignNotarizeConfig {
    /// Build IDs to filter. Default: project name.
    pub ids: Option<Vec<String>>,
    /// Skip this configuration. Accepts bool or template string. SCH-30
    /// (WAVE 5.5) replaced `enabled:` with the canonical `skip:` (DEC-6).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Artifact type to sign and notarize: `dmg` (default) or `pkg`.
    ///
    /// Anodizer-original. GR's notarize.macos has no equivalent (signs
    /// binaries directly via rcodesign). Constrained to a typed enum at
    /// parse time so an unsupported value (`zip`, `app`, etc.) fails fast
    /// instead of producing a silent no-op signing pipe.
    #[serde(rename = "use")]
    pub use_: Option<MacOSNativeArtifactKind>,
    /// Native signing configuration (Keychain).
    pub sign: Option<MacOSNativeSignConfig>,
    /// Native notarization configuration (xcrun notarytool).
    pub notarize: Option<MacOSNativeNotarizeConfig>,
}

/// Keychain-based signing configuration for native `codesign`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct MacOSNativeSignConfig {
    /// Keychain identity (e.g., "Developer ID Application: Name"). Templates allowed.
    pub identity: Option<String>,
    /// Path to Keychain file. Templates allowed.
    pub keychain: Option<String>,
    /// Options to pass to codesign (e.g., ["runtime"]). Only used for DMGs.
    pub options: Option<Vec<String>>,
    /// Path to entitlements XML file. Only used for DMGs. Templates allowed.
    pub entitlements: Option<String>,
}

/// Native notarization configuration for `xcrun notarytool`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct MacOSNativeNotarizeConfig {
    /// Notarytool stored credentials profile name. Templates allowed.
    pub profile_name: Option<String>,
    /// Whether to wait for notarization to complete.
    pub wait: Option<bool>,
    /// Timeout for `xcrun notarytool submit --timeout`. Humantime-style
    /// string (e.g. `"10m"`, `"15s"`, `"1h"`).
    pub timeout: Option<HumanDuration>,
}

// ---------------------------------------------------------------------------
// SourceConfig
// ---------------------------------------------------------------------------

/// An individual file entry for the source archive, supporting src/dst mapping
/// and file metadata overrides.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct SourceFileEntry {
    /// Source file path or glob pattern.
    pub src: String,
    /// Destination path within the archive prefix directory.
    pub dst: Option<String>,
    /// Strip the parent directory from the source path.
    pub strip_parent: Option<bool>,
    /// File metadata overrides.
    pub info: Option<SourceFileInfo>,
}

/// File metadata overrides for source archive entries.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct SourceFileInfo {
    /// File owner.
    pub owner: Option<String>,
    /// File group.
    pub group: Option<String>,
    /// File permissions mode. Accepts a YAML int (decimal) or an
    /// octal-prefixed string (`"0o755"`, `"0755"`). Stored as a `u32` after
    /// parsing — see [`StringOrU32`].
    pub mode: Option<StringOrU32>,
    /// Modification time in RFC3339 format (supports templates).
    pub mtime: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct SourceConfig {
    /// When true, generate a source code archive for the release.
    pub enabled: Option<bool>,
    /// Archive format for the source tarball: tar.gz, tgz, tar, or zip (default: tar.gz).
    pub format: Option<String>,
    /// Filename template for the source archive (supports templates).
    pub name_template: Option<String>,
    /// Prefix prepended to all paths inside the archive (supports templates).
    /// Defaults to name_template value. Use this to set a different prefix than the archive name.
    pub prefix_template: Option<String>,
    /// Extra files to include in the source archive. Accepts strings (glob patterns) or objects with src/dst/info.
    #[serde(default, deserialize_with = "deserialize_source_files")]
    #[schemars(schema_with = "source_files_schema")]
    pub files: Vec<SourceFileEntry>,
}

impl SourceConfig {
    /// Whether source archive generation is enabled (default: false).
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }

    /// Archive format to use (default: "tar.gz").
    pub fn archive_format(&self) -> &str {
        self.format.as_deref().unwrap_or("tar.gz")
    }
}

/// Helper schema function for the source files field (accepts strings, objects, or mixed arrays).
fn source_files_schema(
    generator: &mut schemars::r#gen::SchemaGenerator,
) -> schemars::schema::Schema {
    let mut schema = generator.subschema_for::<Vec<SourceFileEntry>>();
    if let schemars::schema::Schema::Object(ref mut obj) = schema {
        obj.metadata().description = Some(
            "Extra files for the source archive. Accepts strings (glob patterns), objects with src/dst/info, or a mixed array.".to_owned(),
        );
    }
    schema
}

/// Custom deserializer for the source `files` field.
/// Accepts:
///   - null/missing → empty vec (via serde default)
///   - a single string → vec of one SourceFileEntry with that src
///   - a single object → vec of one SourceFileEntry
///   - an array of mixed strings/objects → vec of SourceFileEntry
fn deserialize_source_files<'de, D>(deserializer: D) -> Result<Vec<SourceFileEntry>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::{self, SeqAccess, Visitor};

    struct SourceFilesVisitor;

    impl<'de> Visitor<'de> for SourceFilesVisitor {
        type Value = Vec<SourceFileEntry>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a string, a source file entry object, or an array of strings/objects")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(vec![SourceFileEntry {
                src: v.to_string(),
                ..Default::default()
            }])
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut entries = Vec::new();
            while let Some(value) = seq.next_element::<serde_yaml_ng::Value>()? {
                match value {
                    serde_yaml_ng::Value::String(s) => {
                        entries.push(SourceFileEntry {
                            src: s,
                            ..Default::default()
                        });
                    }
                    other => {
                        let entry =
                            SourceFileEntry::deserialize(other).map_err(de::Error::custom)?;
                        entries.push(entry);
                    }
                }
            }
            Ok(entries)
        }

        fn visit_map<M: de::MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
            let entry = SourceFileEntry::deserialize(de::value::MapAccessDeserializer::new(map))?;
            Ok(vec![entry])
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }
    }

    deserializer.deserialize_any(SourceFilesVisitor)
}

// ---------------------------------------------------------------------------
// SbomConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct SbomConfig {
    /// Unique identifier for this SBOM config (default: "default").
    pub id: Option<String>,
    /// Command to run for SBOM generation (default: "syft").
    pub cmd: Option<String>,
    /// Environment variables to pass to the command, as `KEY=VALUE` strings.
    /// Order is preserved. Values are template-rendered before being set.
    #[serde(default)]
    pub env: Option<Vec<String>>,
    /// Command-line arguments (supports templates and $artifact, $document vars).
    pub args: Option<Vec<String>>,
    /// Output document path templates (supports templates).
    pub documents: Option<Vec<String>>,
    /// Which artifacts to catalog: "source", "archive", "binary", "package", "diskimage", "installer", "any" (default: "archive").
    pub artifacts: Option<String>,
    /// Filter by artifact IDs (ignored if artifacts="source").
    pub ids: Option<Vec<String>>,
    /// Skip this SBOM config. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
}

/// Custom deserializer for the `sboms` / `sbom` field.
/// Accepts:
///   - null/missing → empty vec (via serde default)
///   - a single object → vec of one SbomConfig
///   - an array → vec of SbomConfig
fn deserialize_sboms<'de, D>(deserializer: D) -> Result<Vec<SbomConfig>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::{self, Visitor};

    struct SbomsVisitor;

    impl<'de> Visitor<'de> for SbomsVisitor {
        type Value = Vec<SbomConfig>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("an SBOM config object or an array of SBOM config objects")
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut configs = Vec::new();
            while let Some(item) = seq.next_element::<SbomConfig>()? {
                configs.push(item);
            }
            Ok(configs)
        }

        fn visit_map<M: de::MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
            let config = SbomConfig::deserialize(de::value::MapAccessDeserializer::new(map))?;
            Ok(vec![config])
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }
    }

    deserializer.deserialize_any(SbomsVisitor)
}

// ---------------------------------------------------------------------------
// VersionSyncConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct VersionSyncConfig {
    /// When true, synchronize the crate version with the git tag during release.
    pub enabled: Option<bool>,
    /// Sync mode: "cargo" (updates Cargo.toml) or "tag" (derives version from tag).
    pub mode: Option<String>,
}

// ---------------------------------------------------------------------------
// ChangelogConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ChangelogConfig {
    /// Sort order for changelog entries: "asc" or "desc" (default: "asc").
    pub sort: Option<String>,
    /// Commit message filters to include or exclude from the changelog.
    pub filters: Option<ChangelogFilters>,
    /// Groups for organizing changelog entries by commit message prefix.
    pub groups: Option<Vec<ChangelogGroup>>,
    /// Text prepended to the changelog. Inline string, `from_file: <path>`,
    /// or `from_url: <url>` — symmetric with the release block's header/footer
    /// so users can compose headers from a templated file or remote endpoint
    /// (GoReleaser uses a plain string here; anodizer extends to ContentSource
    /// for consistency with `release.header`).
    pub header: Option<ContentSource>,
    /// Text appended to the changelog. Same shape as `header`.
    pub footer: Option<ContentSource>,
    /// Skip changelog generation. Accepts bool or template string
    /// (e.g. `"{{ if IsSnapshot }}true{{ endif }}"` for conditional skip).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Changelog source: `"git"` (default), `"github"`, or `"github-native"`.
    /// `"github"` fetches commits via the GitHub API, enriching entries with
    /// author login information (available as the `{{ Logins }}` per-entry
    /// template variable and the `{{ AllLogins }}` release-wide variable).
    /// `"github-native"` delegates entirely to GitHub's auto-generated notes.
    #[serde(rename = "use")]
    pub use_source: Option<String>,
    /// Hash abbreviation length. Default: 7. Set to -1 to omit the hash entirely.
    pub abbrev: Option<i32>,
    /// Template for each changelog commit line.
    /// Available variables: SHA (full hash), ShortSHA (abbreviated), Message (commit subject),
    /// AuthorName, AuthorEmail, Login (per-commit GitHub username, `github` backend only),
    /// Logins (per-entry comma-separated list of GitHub usernames for that commit,
    /// `github` backend only), AllLogins (comma-separated list of all GitHub usernames
    /// across the entire release, `github` backend only).
    /// Default: `"{{ ShortSHA }} {{ Message }}"`
    pub format: Option<String>,
    /// File paths to filter commits by. Only commits touching files under these
    /// paths are included. Works with `use: git` for precise per-commit filtering.
    /// With `use: github`, only the first path is used for API queries; multi-path
    /// filtering is coarse. Supports template rendering.
    pub paths: Option<Vec<String>>,
    /// Title heading for the changelog. Default: "Changelog". Supports templates.
    pub title: Option<String>,
    /// Divider string inserted between changelog groups (e.g. `"---"`). Supports templates.
    pub divider: Option<String>,
    /// AI-powered changelog enhancement configuration.
    pub ai: Option<ChangelogAiConfig>,
    /// When `true`, render the changelog even in snapshot mode. Anodizer
    /// matches GoReleaser's default (skip changelog on `ctx.Snapshot`) and
    /// lets users opt back in here for local preview / draft generation.
    /// Wired in `crates/stage-changelog/src/lib.rs::ChangelogStage::run`.
    pub snapshot: Option<bool>,
}

/// AI-powered changelog enhancement configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ChangelogAiConfig {
    /// AI provider to use. Valid: "anthropic", "openai", "ollama".
    /// Empty disables the feature.
    #[serde(rename = "use")]
    pub provider: Option<String>,
    /// Model name (e.g. "gpt-4", "claude-sonnet-4-20250514"). Defaults to provider's default.
    pub model: Option<String>,
    /// Prompt template for the AI. Can be a string, or use `from_url`/`from_file`.
    /// Template variable `.ReleaseNotes` contains the current changelog.
    pub prompt: Option<ChangelogAiPrompt>,
}

/// Prompt source for AI changelog: inline string, URL, or file path.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ChangelogAiPrompt {
    /// Inline prompt string (supports templates).
    Inline(String),
    /// Structured prompt with from_url/from_file sources.
    Source(ChangelogAiPromptSource),
}

/// Structured prompt source: load from URL or file.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ChangelogAiPromptSource {
    /// Load prompt from a URL.
    pub from_url: Option<ContentFromUrl>,
    /// Load prompt from a local file. Overrides from_url if both set.
    pub from_file: Option<ContentFromFile>,
}

/// Resolved prompt source kind after applying priority rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedPromptSource {
    /// Load from a local file path.
    File(String),
    /// Load from a URL (with optional headers).
    Url {
        url: String,
        headers: Option<std::collections::HashMap<String, String>>,
    },
    /// No source configured.
    None,
}

impl ChangelogAiPromptSource {
    /// Resolve the prompt source applying priority: from_file overrides from_url.
    pub fn resolve(&self) -> ResolvedPromptSource {
        if let Some(ref file) = self.from_file
            && let Some(ref path) = file.path
        {
            return ResolvedPromptSource::File(path.clone());
        }
        if let Some(ref url_cfg) = self.from_url
            && let Some(ref url) = url_cfg.url
        {
            return ResolvedPromptSource::Url {
                url: url.clone(),
                headers: url_cfg.headers.clone(),
            };
        }
        ResolvedPromptSource::None
    }
}

/// Load content from a URL with optional headers.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ContentFromUrl {
    /// URL to fetch (supports templates).
    pub url: Option<String>,
    /// HTTP headers to send with the request.
    pub headers: Option<std::collections::HashMap<String, String>>,
}

/// Load content from a local file.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ContentFromFile {
    /// Path to the file (supports templates).
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ChangelogFilters {
    /// Regex patterns: commits matching any of these are excluded from the changelog.
    pub exclude: Option<Vec<String>>,
    /// Regex patterns: only commits matching at least one of these are included.
    pub include: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ChangelogGroup {
    /// Section heading for this group (e.g., "Features", "Bug Fixes").
    pub title: String,
    /// Regex pattern matching commit messages to include in this group.
    pub regexp: Option<String>,
    /// Sort order for this group relative to other groups (lower = first).
    pub order: Option<i32>,
    /// Nested subgroups within this group. Rendered as sub-sections (e.g. `###`).
    pub groups: Option<Vec<ChangelogGroup>>,
}

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

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct UpxConfig {
    /// Unique identifier for this UPX config.
    pub id: Option<String>,
    /// Build IDs filter: only compress binaries from builds whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Whether to compress binaries with UPX.
    /// Accepts bool or template string (GoReleaser parity: `tmpl.Bool(upx.Enabled)`).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
    /// UPX executable path or name (default: "upx").
    pub binary: String,
    /// Extra arguments passed to UPX (e.g., ["-9", "--brute"]).
    pub args: Vec<String>,
    /// When true, fail the build if UPX is not found.
    pub required: bool,
    /// Target triples to compress binaries for (empty means all targets).
    pub targets: Option<Vec<String>>,
    /// UPX compression level string (e.g., "1"-"9", "best"). Maps to `--compress` flag.
    pub compress: Option<String>,
    /// Use LZMA compression (--lzma flag).
    pub lzma: Option<bool>,
    /// Use brute-force compression (--brute flag). Very slow but produces smallest output.
    pub brute: Option<bool>,
}

impl Default for UpxConfig {
    fn default() -> Self {
        UpxConfig {
            id: None,
            ids: None,
            enabled: None,
            binary: "upx".to_string(),
            args: Vec::new(),
            required: false,
            targets: None,
            compress: None,
            lzma: None,
            brute: None,
        }
    }
}

/// Custom deserializer for the `upx` field.
/// Accepts:
///   - null/missing → empty vec (via serde default)
///   - a single object → vec of one UpxConfig
///   - an array → vec of UpxConfig
fn deserialize_upx<'de, D>(deserializer: D) -> Result<Vec<UpxConfig>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::{self, Visitor};

    struct UpxVisitor;

    impl<'de> Visitor<'de> for UpxVisitor {
        type Value = Vec<UpxConfig>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a UPX config object or an array of UPX config objects")
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut configs = Vec::new();
            while let Some(item) = seq.next_element::<UpxConfig>()? {
                configs.push(item);
            }
            Ok(configs)
        }

        fn visit_map<M: de::MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
            let config = UpxConfig::deserialize(de::value::MapAccessDeserializer::new(map))?;
            Ok(vec![config])
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }
    }

    deserializer.deserialize_any(UpxVisitor)
}

// ---------------------------------------------------------------------------
// SnapshotConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SnapshotConfig {
    /// Version string template for snapshot builds (e.g., "{{ .Commit }}-SNAPSHOT").
    pub version_template: String,
}

// ---------------------------------------------------------------------------
// NightlyConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NightlyConfig {
    /// Template for the release name. Default: "{{ .ProjectName }}-nightly"
    pub name_template: Option<String>,
    /// Tag name used for the nightly release. Default: "nightly".
    pub tag_name: Option<String>,
}

// ---------------------------------------------------------------------------
// MetadataConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct MetadataConfig {
    /// Human-readable project description (exposed as `{{ .Metadata.Description }}`).
    pub description: Option<String>,
    /// Project homepage URL (exposed as `{{ .Metadata.Homepage }}`).
    pub homepage: Option<String>,
    /// Project license identifier, e.g. "MIT" or "Apache-2.0" (exposed as `{{ .Metadata.License }}`).
    pub license: Option<String>,
    /// List of project maintainers (exposed as `{{ .Metadata.Maintainers }}`).
    pub maintainers: Option<Vec<String>>,
    /// Global modification timestamp for metadata output files (metadata.json and artifacts.json).
    /// Template string (e.g. "{{ .CommitTimestamp }}") or unix timestamp.
    /// When set, rendered late in the pipeline and applied as file mtime.
    /// Exposed as `{{ .Metadata.ModTimestamp }}`.
    pub mod_timestamp: Option<String>,
    /// Long-form project description (GoReleaser Pro v2.1+). Supports inline
    /// string, `from_file`, or `from_url`. Exposed as `{{ .Metadata.FullDescription }}`.
    /// FromUrl is resolved lazily (requires the release stage); FromFile is resolved
    /// at context-populate time with template-rendered path.
    pub full_description: Option<ContentSource>,
    /// Commit author identity for Pro commit workflows (GoReleaser Pro v2.12+).
    /// Reuses the shared `CommitAuthorConfig` (name + email + optional signing).
    /// Exposed as `{{ .Metadata.CommitAuthor.Name }}` / `{{ .Metadata.CommitAuthor.Email }}`.
    pub commit_author: Option<CommitAuthorConfig>,
}

// ---------------------------------------------------------------------------
// TemplateFileConfig
// ---------------------------------------------------------------------------

/// Configuration for a template file that is rendered through the template
/// engine and placed in the dist directory as a release artifact.
///
/// GoReleaser Pro feature: all rendered template files are uploaded to the
/// release by default. Both `src` and `dst` paths support template rendering.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct TemplateFileConfig {
    /// Identifier for this template file entry (default: "default").
    pub id: Option<String>,
    /// Source template file path. The file contents are rendered through the template engine.
    /// Templates: allowed (in path itself).
    pub src: String,
    /// Destination filename, prefixed with the dist directory.
    /// Templates: allowed.
    pub dst: String,
    /// File permissions in octal notation as a string, e.g. `"0755"` (default: `"0655"`).
    /// Parsed at runtime via `parse_octal_mode()` to avoid YAML interpreting as decimal.
    pub mode: Option<String>,
}

// ---------------------------------------------------------------------------
// AnnounceConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct AnnounceConfig {
    /// Template-conditional skip: if rendered to "true", skip the entire announce stage.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Discord announcement configuration.
    pub discord: Option<DiscordAnnounce>,
    /// Discourse announcement configuration.
    pub discourse: Option<DiscourseAnnounce>,
    /// Slack announcement configuration.
    pub slack: Option<SlackAnnounce>,
    /// Generic webhook announcement configuration.
    pub webhook: Option<WebhookConfig>,
    /// Telegram announcement configuration.
    pub telegram: Option<TelegramAnnounce>,
    /// Microsoft Teams announcement configuration.
    pub teams: Option<TeamsAnnounce>,
    /// Mattermost announcement configuration.
    pub mattermost: Option<MattermostAnnounce>,
    /// Email announcement configuration. SCH-34 (WAVE 5.6) — accepts the
    /// historical `smtp:` key as an alias because GR itself renamed
    /// `smtp:` -> `email:` in v1.21+ and kept the alias for migration.
    /// Mirroring GR's own alias keeps "use what GR uses today" consistent
    /// without forcing a re-yaml of legacy GR configs.
    #[serde(alias = "smtp")]
    pub email: Option<EmailAnnounce>,
    /// Reddit announcement configuration.
    pub reddit: Option<RedditAnnounce>,
    /// Twitter/X announcement configuration.
    pub twitter: Option<TwitterAnnounce>,
    /// Mastodon announcement configuration.
    pub mastodon: Option<MastodonAnnounce>,
    /// Bluesky announcement configuration.
    pub bluesky: Option<BlueskyAnnounce>,
    /// LinkedIn announcement configuration.
    pub linkedin: Option<LinkedInAnnounce>,
    /// OpenCollective announcement configuration.
    pub opencollective: Option<OpenCollectiveAnnounce>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct BlueskyAnnounce {
    /// Enable Bluesky announcements (supports template expressions).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
    /// Bluesky handle/username (e.g. "user.bsky.social").
    pub username: Option<String>,
    /// Message template for the post. Default: "{{ .ProjectName }} {{ .Tag }} is out! Check it out at {{ .ReleaseURL }}"
    pub message_template: Option<String>,
    /// Override the Bluesky PDS (Personal Data Server) URL. Defaults to
    /// `https://bsky.social`. Set this to point at a self-hosted PDS or
    /// alternative instance (e.g. `https://pds.example.com`).
    pub pds_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DiscourseAnnounce {
    /// Enable Discourse announcements (supports template expressions).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
    /// Discourse forum URL (e.g. "https://forum.example.com").
    pub server: Option<String>,
    /// Category ID to post in (required, must be non-zero).
    pub category_id: Option<u64>,
    /// Username for the API request (default: "system").
    pub username: Option<String>,
    /// Title template for the forum topic. Default: "{{ .ProjectName }} {{ .Tag }} is out!"
    pub title_template: Option<String>,
    /// Message body template for the forum topic. Default: "{{ .ProjectName }} {{ .Tag }} is out! Check it out at {{ .ReleaseURL }}"
    pub message_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct LinkedInAnnounce {
    /// Enable LinkedIn announcements. Requires LINKEDIN_ACCESS_TOKEN env var (supports template expressions).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
    /// Message template for the LinkedIn share post. Default: "{{ .ProjectName }} {{ .Tag }} is out! Check it out at {{ .ReleaseURL }}"
    pub message_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct OpenCollectiveAnnounce {
    /// Enable OpenCollective announcements. Requires OPENCOLLECTIVE_TOKEN env var (supports template expressions).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
    /// Collective slug (e.g. "my-project").
    pub slug: Option<String>,
    /// Title template for the update. Default: "{{ .Tag }}"
    pub title_template: Option<String>,
    /// HTML message template for the update. Default includes <br/> and <a> tags with ReleaseURL.
    pub message_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct TwitterAnnounce {
    /// Enable Twitter/X announcements. Requires TWITTER_CONSUMER_KEY, TWITTER_CONSUMER_SECRET, TWITTER_ACCESS_TOKEN, TWITTER_ACCESS_TOKEN_SECRET env vars (supports template expressions).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
    /// Tweet message template. Default: "{{ .ProjectName }} {{ .Tag }} is out! Check it out at {{ .ReleaseURL }}"
    pub message_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct MastodonAnnounce {
    /// Enable Mastodon announcements. Requires `MASTODON_ACCESS_TOKEN` env var (supports template expressions).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
    /// Mastodon instance URL (e.g. "https://mastodon.social").
    pub server: Option<String>,
    /// Toot message template. Default: "{{ .ProjectName }} {{ .Tag }} is out! Check it out at {{ .ReleaseURL }}"
    pub message_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DiscordAnnounce {
    /// Enable Discord announcements (supports template expressions).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
    /// Discord webhook URL. Use templates like `{{ Env.DISCORD_WEBHOOK_ID }}` to reference environment variables.
    pub webhook_url: Option<String>,
    /// Message template for the Discord embed. Default: "{{ .ProjectName }} {{ .Tag }} is out! Check it out at {{ .ReleaseURL }}"
    pub message_template: Option<String>,
    /// Author name displayed in the embed.
    pub author: Option<String>,
    /// Embed color as a decimal integer string (default: "3888754", GoReleaser blue).
    /// Parsed to u32 at runtime. Supports template expressions.
    pub color: Option<String>,
    /// Icon URL for the embed footer.
    pub icon_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct WebhookConfig {
    /// Enable generic webhook announcements (supports template expressions).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
    /// Webhook endpoint URL (supports template variables).
    pub endpoint_url: Option<String>,
    /// Custom HTTP headers to include in the request.
    ///
    /// Precedence — **anodizer diverges from GoReleaser here**:
    /// - anodizer: a config-supplied `Authorization` header wins over the
    ///   `BASIC_AUTH_HEADER_VALUE` / `BEARER_TOKEN_HEADER_VALUE` env var.
    /// - GoReleaser (webhook.go:104-115): env-supplied `Authorization` is
    ///   appended FIRST; most servers honour the first occurrence, so the
    ///   env value effectively wins.
    ///
    /// Migrating configs that relied on env-overriding the config header
    /// must either remove the config entry or be reconfigured. Use
    /// templated config (`Authorization: "Bearer {{ .Env.MY_TOKEN }}"`) for
    /// the cleanest migration.
    pub headers: Option<HashMap<String, String>>,
    /// Content-Type header value. Default: "application/json".
    pub content_type: Option<String>,
    /// Message body template. Default: "{{ .ProjectName }} {{ .Tag }} is out! Check it out at {{ .ReleaseURL }}"
    pub message_template: Option<String>,
    /// When true, skip TLS certificate verification for the webhook endpoint.
    pub skip_tls_verify: Option<bool>,
    /// HTTP status codes to accept as success (default: [200, 201, 202, 204]).
    #[serde(default)]
    pub expected_status_codes: Vec<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct TelegramAnnounce {
    /// Enable Telegram announcements. Requires bot_token and chat_id (supports template expressions).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
    /// Telegram Bot API token. Get one from @BotFather.
    pub bot_token: Option<String>,
    /// Telegram chat ID to send the message to (supports template variables).
    pub chat_id: Option<String>,
    /// Message template. Default: "{{ .ProjectName }} {{ .Tag }} is out! Check it out at {{ .ReleaseURL }}"
    pub message_template: Option<String>,
    /// Parse mode: "MarkdownV2" or "HTML" (defaults to "MarkdownV2").
    pub parse_mode: Option<String>,
    /// Message thread ID for sending to a specific topic in a forum group.
    /// Supports template expressions; parsed to i64 at runtime.
    pub message_thread_id: Option<String>,
}

/// Default Adaptive Card title for Teams announcements. Centralised so that a
/// config-load round-trip (parse → serialise → re-parse) preserves the value
/// instead of stripping it back to `None`.
pub const TEAMS_DEFAULT_TITLE_TEMPLATE: &str = "{{ ProjectName }} {{ Tag }} is out!";

fn default_teams_title_template() -> Option<String> {
    Some(TEAMS_DEFAULT_TITLE_TEMPLATE.to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct TeamsAnnounce {
    /// Enable Microsoft Teams announcements (supports template expressions).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
    /// Teams incoming webhook URL.
    pub webhook_url: Option<String>,
    /// Message template for the Adaptive Card body. Default: "{{ .ProjectName }} {{ .Tag }} is out! Check it out at {{ .ReleaseURL }}"
    pub message_template: Option<String>,
    /// Title template for the Adaptive Card header. Default: "{{ ProjectName }} {{ Tag }} is out!"
    #[serde(default = "default_teams_title_template")]
    pub title_template: Option<String>,
    /// Theme color for the card (hex string, e.g. "0076D7").
    pub color: Option<String>,
    /// Icon URL displayed in the card header.
    pub icon_url: Option<String>,
}

impl Default for TeamsAnnounce {
    fn default() -> Self {
        Self {
            enabled: None,
            webhook_url: None,
            message_template: None,
            title_template: default_teams_title_template(),
            color: None,
            icon_url: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct MattermostAnnounce {
    /// Enable Mattermost announcements (supports template expressions).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
    /// Mattermost incoming webhook URL.
    pub webhook_url: Option<String>,
    /// Channel override (e.g. "town-square").
    pub channel: Option<String>,
    /// Username override for the bot post.
    pub username: Option<String>,
    /// Icon URL for the bot post.
    pub icon_url: Option<String>,
    /// Icon emoji for the bot post (e.g. ":rocket:").
    pub icon_emoji: Option<String>,
    /// Attachment color (hex string, e.g. "#36a64f").
    pub color: Option<String>,
    /// Message template for the Mattermost post. Default: "{{ .ProjectName }} {{ .Tag }} is out! Check it out at {{ .ReleaseURL }}"
    pub message_template: Option<String>,
    /// Title template for the Mattermost attachment.
    pub title_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct EmailAnnounce {
    /// Enable email announcements (supports template expressions).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
    /// SMTP server hostname. When set, uses SMTP transport.
    /// When absent, falls back to sendmail/msmtp.
    pub host: Option<String>,
    /// SMTP server port (default: 587 for STARTTLS).
    pub port: Option<u16>,
    /// SMTP username (can also be set via SMTP_USERNAME env var).
    pub username: Option<String>,
    /// Sender email address.
    pub from: Option<String>,
    /// Recipient email addresses.
    #[serde(default)]
    pub to: Vec<String>,
    /// Email subject template. Default: "{{ .ProjectName }} {{ .Tag }} is out!"
    pub subject_template: Option<String>,
    /// Email body template.
    pub message_template: Option<String>,
    /// Skip TLS certificate verification (default: false).
    pub insecure_skip_verify: Option<bool>,
    /// Transport encryption mode. `auto` (the default) picks SMTPS for port
    /// 465, plain SMTP for port 25, and STARTTLS for everything else; `tls`
    /// forces SMTPS, `starttls` forces STARTTLS, `none` forces plain SMTP.
    pub encryption: Option<EmailEncryption>,
}

/// Email transport encryption mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum EmailEncryption {
    /// Pick based on port: 465 → SMTPS, 25 → none, otherwise STARTTLS.
    #[default]
    Auto,
    /// Implicit TLS on connect (typically port 465).
    Tls,
    /// Plain SMTP that upgrades to TLS via STARTTLS (typically port 587).
    Starttls,
    /// Plain SMTP, no TLS. Only safe on trusted local relays (port 25).
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct RedditAnnounce {
    /// Enable Reddit announcements. Requires REDDIT_SECRET and REDDIT_PASSWORD env vars (supports template expressions).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
    /// Reddit application (OAuth client) ID.
    pub application_id: Option<String>,
    /// Reddit username for posting.
    pub username: Option<String>,
    /// Subreddit to post to (without /r/ prefix).
    pub sub: Option<String>,
    /// Title template for the Reddit link post. Default: "{{ .ProjectName }} {{ .Tag }} is out!"
    pub title_template: Option<String>,
    /// URL template for the Reddit link post. Default: "{{ .ReleaseURL }}"
    pub url_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct SlackAnnounce {
    /// Enable Slack announcements (supports template expressions).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
    /// Slack incoming webhook URL. Use template `{{ Env.SLACK_WEBHOOK }}` to reference an environment variable.
    pub webhook_url: Option<String>,
    /// Message template for the Slack post. Default: "{{ .ProjectName }} {{ .Tag }} is out! Check it out at {{ .ReleaseURL }}"
    pub message_template: Option<String>,
    /// Override the webhook's default channel (e.g. "#releases").
    pub channel: Option<String>,
    /// Override the webhook's default username (e.g. "release-bot").
    pub username: Option<String>,
    /// Override the webhook's default icon with an emoji (e.g. ":rocket:").
    pub icon_emoji: Option<String>,
    /// Override the webhook's default icon with an image URL.
    pub icon_url: Option<String>,
    /// Slack Block Kit blocks (typed for schema validation).
    pub blocks: Option<Vec<SlackBlock>>,
    /// Slack legacy attachments (typed for schema validation).
    pub attachments: Option<Vec<SlackAttachment>>,
}

/// A Slack Block Kit block element.
/// Common fields are typed; additional block-type-specific fields are captured via flatten.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
pub struct SlackBlock {
    /// Block type (e.g., "header", "section", "divider", "actions", "context", "image").
    #[serde(rename = "type")]
    pub block_type: String,
    /// Text object for the block (used by header, section, context types).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<SlackTextObject>,
    /// Block ID for interactive payloads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_id: Option<String>,
    /// Additional block-specific fields (elements, accessory, fields, etc.).
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// A Slack text composition object.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
pub struct SlackTextObject {
    /// Text type: "plain_text" or "mrkdwn".
    #[serde(rename = "type")]
    pub text_type: String,
    /// Text content (supports template variables).
    pub text: String,
    /// Whether to render emoji shortcodes (plain_text only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emoji: Option<bool>,
    /// Whether to render verbatim (mrkdwn only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verbatim: Option<bool>,
}

/// A Slack legacy attachment.
/// Common fields are typed; additional fields are captured via flatten.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
pub struct SlackAttachment {
    /// Attachment sidebar color (hex string, e.g., "#36a64f" for green).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Main body text of the attachment (supports template variables).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Bold title text at the top of the attachment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Plain-text summary shown in notifications that cannot render attachments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
    /// Text shown above the attachment block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pretext: Option<String>,
    /// Small text shown at the bottom of the attachment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub footer: Option<String>,
    /// Additional attachment-specific fields.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

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

/// A value that can be either a bool or a template string.
/// Used by `skip`, `skip_upload`, and similar fields across multiple config
/// structs to support both `skip: true` and template conditionals like
/// `skip: "{{ if .IsSnapshot }}true{{ endif }}"`.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum StringOrBool {
    Bool(bool),
    String(String),
}

impl StringOrBool {
    /// Evaluate this value to a bool. If it's a string, treat "true" / "1" as true,
    /// everything else as false.
    pub fn as_bool(&self) -> bool {
        match self {
            StringOrBool::Bool(b) => *b,
            StringOrBool::String(s) => matches!(s.trim(), "true" | "1"),
        }
    }

    /// Return the raw string value for template rendering, or the bool as a string.
    pub fn as_str(&self) -> &str {
        match self {
            StringOrBool::Bool(true) => "true",
            StringOrBool::Bool(false) => "false",
            StringOrBool::String(s) => s,
        }
    }

    /// Whether this value contains a template expression that needs rendering.
    pub fn is_template(&self) -> bool {
        matches!(self, StringOrBool::String(s) if s.contains('{'))
    }

    /// Evaluate whether this value resolves to `true`.
    ///
    /// If the value is a template string (contains `{`), it is rendered via
    /// the provided closure and the result is compared to `"true"`.
    /// Otherwise, the plain bool / string value is evaluated directly.
    ///
    /// Returns the render error so callers can fail fast on a malformed
    /// template instead of silently treating it as `false`.
    ///
    /// Used for both `skip:` evaluation (most callers) and `output:` / `sbom:`
    /// bool-or-template fields — there is no separate alias; call this directly.
    pub fn try_evaluates_to_true(
        &self,
        render: impl Fn(&str) -> anyhow::Result<String>,
    ) -> anyhow::Result<bool> {
        if self.is_template() {
            Ok(render(self.as_str())?.trim() == "true")
        } else {
            Ok(self.as_bool())
        }
    }
}

impl Default for StringOrBool {
    fn default() -> Self {
        StringOrBool::Bool(false)
    }
}

/// Custom deserializer for `Option<StringOrBool>`.
pub(crate) fn deserialize_string_or_bool_opt<'de, D>(
    deserializer: D,
) -> Result<Option<StringOrBool>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::{self, Visitor};

    struct StringOrBoolVisitor;

    impl<'de> Visitor<'de> for StringOrBoolVisitor {
        type Value = Option<StringOrBool>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a bool, a string, or null")
        }

        fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
            Ok(Some(StringOrBool::Bool(v)))
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(Some(StringOrBool::String(v.to_owned())))
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
            Ok(Some(StringOrBool::String(v)))
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
    }

    deserializer.deserialize_any(StringOrBoolVisitor)
}

/// A typed duration value parsed from a humantime-style string in YAML.
///
/// Accepts `"10m"`, `"15s"`, `"1h30m"`, `"500ms"`, etc. Used by notarize
/// timeouts so the schema is typed and validation catches malformed values
/// at config-load time instead of during the notarize stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
pub struct HumanDuration(
    #[serde(serialize_with = "serialize_human_duration")] pub std::time::Duration,
);

impl HumanDuration {
    /// Get the underlying `Duration` value.
    pub fn duration(&self) -> std::time::Duration {
        self.0
    }

    /// Format the duration back to its canonical string form (`{seconds}s` or
    /// `{minutes}m{seconds}s` depending on whole-minute alignment). Matches
    /// the form `xcrun notarytool --timeout` accepts (a unit-suffixed integer).
    pub fn as_humantime_string(&self) -> String {
        let total_secs = self.0.as_secs();
        if total_secs == 0 {
            // Sub-second; fall back to ms.
            return format!("{}ms", self.0.as_millis());
        }
        let hours = total_secs / 3600;
        let mins = (total_secs % 3600) / 60;
        let secs = total_secs % 60;
        let mut out = String::new();
        if hours > 0 {
            out.push_str(&format!("{hours}h"));
        }
        if mins > 0 {
            out.push_str(&format!("{mins}m"));
        }
        if secs > 0 || out.is_empty() {
            out.push_str(&format!("{secs}s"));
        }
        out
    }
}

impl<'de> Deserialize<'de> for HumanDuration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::{self, Visitor};

        struct DurVisitor;

        impl<'de> Visitor<'de> for DurVisitor {
            type Value = HumanDuration;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(
                    "a duration string with unit suffix (e.g. \"10m\", \"15s\", \"1h30m\", \"500ms\")",
                )
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                parse_humantime_duration(v)
                    .map(HumanDuration)
                    .map_err(E::custom)
            }

            fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
                self.visit_str(&v)
            }
        }

        deserializer.deserialize_str(DurVisitor)
    }
}

fn serialize_human_duration<S: serde::Serializer>(
    d: &std::time::Duration,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&HumanDuration(*d).as_humantime_string())
}

/// Parse a humantime-style duration string. Recognizes `ms`, `s`, `m`, `h`,
/// `d` units and concatenated forms like `"1h30m"`. Whitespace between
/// components is tolerated.
fn parse_humantime_duration(input: &str) -> Result<std::time::Duration, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty duration string".to_string());
    }
    let mut total = std::time::Duration::ZERO;
    let mut number_buf = String::new();
    let mut had_any = false;
    let mut iter = s.chars().peekable();
    while let Some(&c) = iter.peek() {
        if c.is_whitespace() {
            iter.next();
            continue;
        }
        if c.is_ascii_digit() {
            number_buf.push(c);
            iter.next();
            continue;
        }
        if number_buf.is_empty() {
            return Err(format!("expected digit before unit in '{input}'"));
        }
        // Read unit (1 or 2 chars: ms, s, m, h, d).
        let mut unit = String::new();
        unit.push(c);
        iter.next();
        if let Some(&next) = iter.peek()
            && unit == "m"
            && next == 's'
        {
            unit.push('s');
            iter.next();
        }
        let n: u64 = number_buf
            .parse()
            .map_err(|e| format!("invalid number '{number_buf}' in '{input}': {e}"))?;
        let segment = match unit.as_str() {
            "ms" => std::time::Duration::from_millis(n),
            "s" => std::time::Duration::from_secs(n),
            "m" => std::time::Duration::from_secs(n * 60),
            "h" => std::time::Duration::from_secs(n * 3600),
            "d" => std::time::Duration::from_secs(n * 86_400),
            other => return Err(format!("unknown duration unit '{other}' in '{input}'")),
        };
        total += segment;
        number_buf.clear();
        had_any = true;
    }
    if !number_buf.is_empty() {
        return Err(format!(
            "trailing number '{number_buf}' without a unit in '{input}'"
        ));
    }
    if !had_any {
        return Err(format!("no duration components found in '{input}'"));
    }
    Ok(total)
}

/// A value that can be either a `u32` or a string parsed as octal/decimal.
///
/// Used by `NfpmConfig.umask` (and any future field that GoReleaser specifies
/// as `int OR string` in YAML — the parser canonicalizes both forms to a
/// `u32`). Accepts: `0o022`, `"0o022"`, `"022"`, `"18"`, `18`. Bare numeric
/// YAML values are interpreted as decimal; YAML-string forms accept the
/// `0o`/`0O` prefix to spell octal explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(transparent)]
pub struct StringOrU32(#[serde(deserialize_with = "deserialize_u32_from_string_or_int")] pub u32);

impl StringOrU32 {
    /// Get the underlying `u32` value.
    pub fn value(&self) -> u32 {
        self.0
    }
}

/// Deserialize a `u32` from either a YAML int or a string in octal/decimal.
fn deserialize_u32_from_string_or_int<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::{self, Visitor};

    struct U32Visitor;

    impl<'de> Visitor<'de> for U32Visitor {
        type Value = u32;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a u32 integer or a string parseable as octal/decimal (e.g. 18, \"0o022\", \"022\")")
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
            u32::try_from(v).map_err(|_| E::custom(format!("value {v} does not fit in u32")))
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
            u32::try_from(v).map_err(|_| E::custom(format!("value {v} does not fit in u32")))
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            let trimmed = v.trim();
            if let Some(rest) = trimmed
                .strip_prefix("0o")
                .or_else(|| trimmed.strip_prefix("0O"))
            {
                return u32::from_str_radix(rest, 8)
                    .map_err(|e| E::custom(format!("invalid octal '{v}': {e}")));
            }
            // Bare leading-zero strings (e.g. "022") are octal — match the
            // typical convention for unix file mode strings.
            if trimmed.starts_with('0') && trimmed.len() > 1 {
                return u32::from_str_radix(trimmed, 8)
                    .map_err(|e| E::custom(format!("invalid octal '{v}': {e}")));
            }
            trimmed
                .parse::<u32>()
                .map_err(|e| E::custom(format!("invalid u32 '{v}': {e}")))
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
            self.visit_str(&v)
        }
    }

    deserializer.deserialize_any(U32Visitor)
}

/// Custom deserializer for `Option<Vec<String>>` that accepts either a single
/// string or an array of strings. Used by `BlobConfig.cache_control`.
fn deserialize_string_or_vec_opt<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::{self, Visitor};

    struct StringOrVecVisitor;

    impl<'de> Visitor<'de> for StringOrVecVisitor {
        type Value = Option<Vec<String>>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a string, a list of strings, or null")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(Some(vec![v.to_owned()]))
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
            Ok(Some(vec![v]))
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut items = Vec::new();
            while let Some(item) = seq.next_element::<String>()? {
                items.push(item);
            }
            Ok(Some(items))
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
    }

    deserializer.deserialize_any(StringOrVecVisitor)
}

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
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    #[test]
    fn test_minimal_yaml_config() {
        let yaml = r#"
project_name: myproject
crates:
  - name: myproject
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.project_name, "myproject");
        assert_eq!(config.crates.len(), 1);
        assert_eq!(config.dist, std::path::PathBuf::from("./dist"));
    }

    #[test]
    fn test_minimal_toml_config() {
        let toml_str = r#"
project_name = "myproject"

[[crates]]
name = "myproject"
path = "."
tag_template = "v{{ .Version }}"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.project_name, "myproject");
    }

    #[test]
    fn test_full_config_with_defaults() {
        let yaml = r#"
project_name: cfgd
dist: ./dist
defaults:
  targets:
    - x86_64-unknown-linux-gnu
    - aarch64-apple-darwin
  cross: auto
  flags: --release
  archives:
    formats: [tar.gz]
    format_overrides:
      - os: windows
        formats: [zip]
  checksum:
    algorithm: sha256
crates:
  - name: cfgd
    path: crates/cfgd
    tag_template: "v{{ .Version }}"
    builds:
      - binary: cfgd
        features: []
        no_default_features: false
    archives:
      - name_template: "{{ .ProjectName }}-{{ .Version }}-{{ .Os }}-{{ .Arch }}"
        files:
          - LICENSE
    release:
      github:
        owner: tj-smith47
        name: cfgd
      draft: false
      prerelease: auto
      name_template: "{{ .Tag }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let defaults = config.defaults.unwrap();
        assert_eq!(defaults.targets.unwrap().len(), 2);
        assert_eq!(defaults.cross, Some(CrossStrategy::Auto));
        let release = config.crates[0].release.as_ref().unwrap();
        assert_eq!(release.name_template, Some("{{ .Tag }}".to_string()));
    }

    #[test]
    fn test_snapshot_config() {
        let yaml = r#"
project_name: test
snapshot:
  version_template: "{{ .Version }}-SNAPSHOT-{{ .ShortCommit }}"
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(
            config.snapshot.unwrap().version_template,
            "{{ .Version }}-SNAPSHOT-{{ .ShortCommit }}"
        );
    }

    #[test]
    fn test_archives_false() {
        let yaml = r#"
project_name: test
crates:
  - name: operator
    path: crates/operator
    tag_template: "v{{ .Version }}"
    archives: false
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(matches!(
            config.crates[0].archives,
            ArchivesConfig::Disabled
        ));
    }

    #[test]
    fn test_publish_cargo_present_and_with_options() {
        // Presence of `cargo:` opts the crate in (DEC-6 / ITEM-3 — no
        // `enabled` field, no bool shorthand).
        let yaml_present = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      cargo: {}
"#;
        let config: Config = serde_yaml_ng::from_str(yaml_present).unwrap();
        assert!(config.crates[0].publish.as_ref().unwrap().cargo.is_some());

        let yaml_obj = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      cargo:
        index_timeout: 120
        no_verify: true
        allow_dirty: true
        features: [foo, bar]
"#;
        let config: Config = serde_yaml_ng::from_str(yaml_obj).unwrap();
        let cargo = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .cargo
            .as_ref()
            .unwrap();
        assert_eq!(cargo.index_timeout, Some(120));
        assert_eq!(cargo.no_verify, Some(true));
        assert_eq!(cargo.allow_dirty, Some(true));
        assert_eq!(
            cargo.features,
            Some(vec!["foo".to_string(), "bar".to_string()])
        );
    }

    #[test]
    fn test_publish_cargo_bool_shorthand_rejected() {
        // ITEM-3 hard-break: `cargo: true` is no longer a valid shorthand.
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      cargo: true
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(
            result.is_err(),
            "publish.cargo: true must fail to parse (no bool shorthand)"
        );
    }

    #[test]
    fn test_publish_cargo_legacy_crates_key_rejected() {
        // ITEM-3 hard-break: the old `publish.crates:` key was renamed to
        // `publish.cargo:` with no alias (DEC-5).
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      crates: true
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(
            result.is_err(),
            "publish.crates is no longer a valid key (renamed to cargo); deny_unknown_fields must reject it"
        );
    }

    // ---- MakeLatestConfig tests ----

    #[test]
    fn test_make_latest_auto() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      make_latest: auto
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let release = config.crates[0].release.as_ref().unwrap();
        assert_eq!(release.make_latest, Some(MakeLatestConfig::Auto));
    }

    #[test]
    fn test_make_latest_true() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      make_latest: true
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let release = config.crates[0].release.as_ref().unwrap();
        assert_eq!(release.make_latest, Some(MakeLatestConfig::Bool(true)));
    }

    #[test]
    fn test_make_latest_false() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      make_latest: false
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let release = config.crates[0].release.as_ref().unwrap();
        assert_eq!(release.make_latest, Some(MakeLatestConfig::Bool(false)));
    }

    #[test]
    fn test_make_latest_omitted() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      draft: false
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let release = config.crates[0].release.as_ref().unwrap();
        assert_eq!(release.make_latest, None);
    }

    #[test]
    fn test_make_latest_template_string() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      make_latest: "{{ if .IsSnapshot }}false{{ else }}true{{ end }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let release = config.crates[0].release.as_ref().unwrap();
        assert_eq!(
            release.make_latest,
            Some(MakeLatestConfig::String(
                "{{ if .IsSnapshot }}false{{ else }}true{{ end }}".to_string()
            ))
        );
    }

    #[test]
    fn test_make_latest_string_true() {
        // The string "true" should deserialize to Bool(true) for consistency.
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      make_latest: "true"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let release = config.crates[0].release.as_ref().unwrap();
        assert_eq!(release.make_latest, Some(MakeLatestConfig::Bool(true)));
    }

    #[test]
    fn test_make_latest_string_false() {
        // The string "false" should deserialize to Bool(false) for consistency.
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      make_latest: "false"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let release = config.crates[0].release.as_ref().unwrap();
        assert_eq!(release.make_latest, Some(MakeLatestConfig::Bool(false)));
    }

    // ---- ChangelogConfig header/footer/disable tests ----

    #[test]
    fn test_changelog_header_footer() {
        let yaml = r##"
project_name: test
changelog:
  header: "# My Release Notes"
  footer: "---\nGenerated by anodizer"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"##;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let cl = config.changelog.as_ref().unwrap();
        assert_eq!(
            cl.header,
            Some(ContentSource::Inline("# My Release Notes".to_string()))
        );
        assert_eq!(
            cl.footer,
            Some(ContentSource::Inline(
                "---\nGenerated by anodizer".to_string()
            ))
        );
    }

    #[test]
    fn test_changelog_header_from_file_and_url() {
        let yaml = r#"
project_name: test
changelog:
  header:
    from_file: ./HEADER.md
  footer:
    from_url: https://example.com/footer.md
    headers:
      Accept: text/markdown
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let cl = config.changelog.as_ref().unwrap();
        match cl.header.as_ref().unwrap() {
            ContentSource::FromFile { from_file } => assert_eq!(from_file, "./HEADER.md"),
            other => panic!("expected FromFile, got {other:?}"),
        }
        match cl.footer.as_ref().unwrap() {
            ContentSource::FromUrl { from_url, headers } => {
                assert_eq!(from_url, "https://example.com/footer.md");
                assert_eq!(
                    headers
                        .as_ref()
                        .and_then(|m| m.get("Accept"))
                        .map(String::as_str),
                    Some("text/markdown")
                );
            }
            other => panic!("expected FromUrl, got {other:?}"),
        }
    }

    #[test]
    fn test_changelog_disable() {
        let yaml = r#"
project_name: test
changelog:
  skip: true
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let cl = config.changelog.as_ref().unwrap();
        assert_eq!(cl.skip, Some(StringOrBool::Bool(true)));
    }

    #[test]
    fn test_changelog_disable_false() {
        let yaml = r#"
project_name: test
changelog:
  skip: false
  sort: desc
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let cl = config.changelog.as_ref().unwrap();
        assert_eq!(cl.skip, Some(StringOrBool::Bool(false)));
        assert_eq!(cl.sort, Some("desc".to_string()));
    }

    // ---- ChecksumConfig disable tests ----

    #[test]
    fn test_checksum_disable() {
        let yaml = r#"
project_name: test
defaults:
  checksum:
    skip: true
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let checksum = config.defaults.as_ref().unwrap().checksum.as_ref().unwrap();
        assert_eq!(checksum.skip, Some(StringOrBool::Bool(true)));
    }

    #[test]
    fn test_checksum_disable_per_crate() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    checksum:
      skip: true
      algorithm: sha512
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let checksum = config.crates[0].checksum.as_ref().unwrap();
        assert_eq!(checksum.skip, Some(StringOrBool::Bool(true)));
        assert_eq!(checksum.algorithm, Some("sha512".to_string()));
    }

    #[test]
    fn test_checksum_disable_template_string() {
        let yaml = r#"
project_name: test
defaults:
  checksum:
    skip: "{{ if .IsSnapshot }}true{{ end }}"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let checksum = config.defaults.as_ref().unwrap().checksum.as_ref().unwrap();
        match &checksum.skip {
            Some(StringOrBool::String(s)) => {
                assert!(s.contains("IsSnapshot"));
            }
            other => panic!("expected StringOrBool::String, got {:?}", other),
        }
    }

    #[test]
    fn test_checksum_extra_files_object_form() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    checksum:
      extra_files:
        - "dist/*.bin"
        - glob: "release/*.deb"
          name_template: "{{ .ArtifactName }}.checksum"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let checksum = config.crates[0].checksum.as_ref().unwrap();
        let extra = checksum.extra_files.as_ref().unwrap();
        assert_eq!(extra.len(), 2);
        assert_eq!(extra[0], ExtraFileSpec::Glob("dist/*.bin".to_string()));
        match &extra[1] {
            ExtraFileSpec::Detailed {
                glob,
                name_template,
            } => {
                assert_eq!(glob, "release/*.deb");
                assert_eq!(
                    name_template.as_deref(),
                    Some("{{ .ArtifactName }}.checksum")
                );
            }
            other => panic!("expected ExtraFileSpec::Detailed, got {:?}", other),
        }
    }

    // ---- MakeLatestConfig serialization roundtrip ----

    #[test]
    fn test_make_latest_serialize_roundtrip() {
        let auto = MakeLatestConfig::Auto;
        let json = serde_json::to_string(&auto).unwrap();
        assert_eq!(json, "\"auto\"");

        let bool_true = MakeLatestConfig::Bool(true);
        let json = serde_json::to_string(&bool_true).unwrap();
        assert_eq!(json, "true");

        let bool_false = MakeLatestConfig::Bool(false);
        let json = serde_json::to_string(&bool_false).unwrap();
        assert_eq!(json, "false");

        let tmpl = MakeLatestConfig::String(
            "{{ if .IsSnapshot }}false{{ else }}true{{ end }}".to_string(),
        );
        let json = serde_json::to_string(&tmpl).unwrap();
        assert_eq!(json, "\"{{ if .IsSnapshot }}false{{ else }}true{{ end }}\"");
    }

    // ---- ReleaseConfig header/footer tests ----

    #[test]
    fn test_release_header_footer_inline() {
        let yaml = r###"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      header: "## Custom Header"
      footer: "---\nPowered by anodizer"
"###;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let release = config.crates[0].release.as_ref().unwrap();
        assert_eq!(
            release.header,
            Some(ContentSource::Inline("## Custom Header".to_string()))
        );
        assert_eq!(
            release.footer,
            Some(ContentSource::Inline(
                "---\nPowered by anodizer".to_string()
            ))
        );
    }

    #[test]
    fn test_release_header_footer_from_file() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      header:
        from_file: ./RELEASE_HEADER.md
      footer:
        from_file: ./RELEASE_FOOTER.md
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let release = config.crates[0].release.as_ref().unwrap();
        assert_eq!(
            release.header,
            Some(ContentSource::FromFile {
                from_file: "./RELEASE_HEADER.md".to_string()
            })
        );
        assert_eq!(
            release.footer,
            Some(ContentSource::FromFile {
                from_file: "./RELEASE_FOOTER.md".to_string()
            })
        );
    }

    #[test]
    fn test_release_header_footer_from_url() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      header:
        from_url: https://example.com/header.md
      footer:
        from_url: https://example.com/footer.md
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let release = config.crates[0].release.as_ref().unwrap();
        assert_eq!(
            release.header,
            Some(ContentSource::FromUrl {
                from_url: "https://example.com/header.md".to_string(),
                headers: None,
            })
        );
        assert_eq!(
            release.footer,
            Some(ContentSource::FromUrl {
                from_url: "https://example.com/footer.md".to_string(),
                headers: None,
            })
        );
    }

    #[test]
    fn test_release_header_footer_omitted() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      draft: false
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let release = config.crates[0].release.as_ref().unwrap();
        assert_eq!(release.header, None);
        assert_eq!(release.footer, None);
    }

    // ---- ReleaseConfig extra_files tests ----

    #[test]
    fn test_release_extra_files_glob_strings() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      extra_files:
        - "dist/*.sig"
        - "CHANGELOG.md"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let release = config.crates[0].release.as_ref().unwrap();
        let files = release.extra_files.as_ref().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0], ExtraFileSpec::Glob("dist/*.sig".to_string()));
        assert_eq!(files[1], ExtraFileSpec::Glob("CHANGELOG.md".to_string()));
    }

    #[test]
    fn test_release_extra_files_detailed_objects() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      extra_files:
        - glob: "dist/*.sig"
          name_template: "{{ .ArtifactName }}.sig"
        - glob: "docs/*.pdf"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let release = config.crates[0].release.as_ref().unwrap();
        let files = release.extra_files.as_ref().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].glob(), "dist/*.sig");
        assert_eq!(files[0].name_template(), Some("{{ .ArtifactName }}.sig"));
        assert_eq!(files[1].glob(), "docs/*.pdf");
        assert_eq!(files[1].name_template(), None);
    }

    #[test]
    fn test_release_extra_files_mixed() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      extra_files:
        - "dist/*.sig"
        - glob: "docs/*.pdf"
          name_template: "{{ .ArtifactName }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let release = config.crates[0].release.as_ref().unwrap();
        let files = release.extra_files.as_ref().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0], ExtraFileSpec::Glob("dist/*.sig".to_string()));
        assert_eq!(files[1].glob(), "docs/*.pdf");
    }

    #[test]
    fn test_release_extra_files_omitted() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      draft: true
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let release = config.crates[0].release.as_ref().unwrap();
        assert_eq!(release.extra_files, None);
    }

    // ---- ReleaseConfig templated_extra_files tests ----

    #[test]
    fn test_release_templated_extra_files_parsed() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      templated_extra_files:
        - src: LICENSE.tpl
          dst: LICENSE.txt
        - src: README.md.tpl
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let release = config.crates[0].release.as_ref().unwrap();
        let tpl = release.templated_extra_files.as_ref().unwrap();
        assert_eq!(tpl.len(), 2);
        assert_eq!(tpl[0].src, "LICENSE.tpl");
        assert_eq!(tpl[0].dst.as_deref(), Some("LICENSE.txt"));
        assert_eq!(tpl[1].src, "README.md.tpl");
        assert_eq!(tpl[1].dst, None);
    }

    #[test]
    fn test_release_templated_extra_files_defaults_to_none() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      draft: true
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let release = config.crates[0].release.as_ref().unwrap();
        assert_eq!(release.templated_extra_files, None);
    }

    #[test]
    fn test_checksum_templated_extra_files_parsed() {
        let yaml = r#"
name_template: "checksums.txt"
templated_extra_files:
  - src: "notes.tpl"
    dst: "RELEASE_NOTES.txt"
"#;
        let cfg: ChecksumConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let tpl = cfg.templated_extra_files.as_ref().unwrap();
        assert_eq!(tpl.len(), 1);
        assert_eq!(tpl[0].src, "notes.tpl");
        assert_eq!(tpl[0].dst.as_deref(), Some("RELEASE_NOTES.txt"));
    }

    // ---- ReleaseConfig skip_upload tests ----

    #[test]
    fn test_release_skip_upload() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      skip_upload: true
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let release = config.crates[0].release.as_ref().unwrap();
        assert_eq!(release.skip_upload, Some(StringOrBool::Bool(true)));
    }

    #[test]
    fn test_release_skip_upload_false() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      skip_upload: false
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let release = config.crates[0].release.as_ref().unwrap();
        assert_eq!(release.skip_upload, Some(StringOrBool::Bool(false)));
    }

    #[test]
    fn test_release_skip_upload_auto() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      skip_upload: "auto"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let release = config.crates[0].release.as_ref().unwrap();
        assert_eq!(
            release.skip_upload,
            Some(StringOrBool::String("auto".to_string()))
        );
    }

    // ---- ReleaseConfig replace_existing_draft / replace_existing_artifacts tests ----

    #[test]
    fn test_release_replace_existing_draft() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      replace_existing_draft: true
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let release = config.crates[0].release.as_ref().unwrap();
        assert_eq!(release.replace_existing_draft, Some(true));
    }

    #[test]
    fn test_release_replace_existing_artifacts() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      replace_existing_artifacts: true
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let release = config.crates[0].release.as_ref().unwrap();
        assert_eq!(release.replace_existing_artifacts, Some(true));
    }

    // ---- ReleaseConfig tag override tests ----

    #[test]
    fn test_release_tag_override_parsed() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "myapp/v{{ .Version }}"
    release:
      tag: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let release = config.crates[0].release.as_ref().unwrap();
        assert_eq!(release.tag, Some("v{{ .Version }}".to_string()));
    }

    #[test]
    fn test_release_tag_override_omitted() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      draft: false
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let release = config.crates[0].release.as_ref().unwrap();
        assert_eq!(release.tag, None);
    }

    #[test]
    fn test_release_all_new_fields() {
        let yaml = r##"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      github:
        owner: myorg
        name: myrepo
      draft: true
      make_latest: auto
      header: "# Release Notes"
      footer: "Thank you!"
      extra_files:
        - "dist/extra.zip"
      skip_upload: false
      replace_existing_draft: true
      replace_existing_artifacts: false
      target_commitish: main
      discussion_category_name: Announcements
      include_meta: true
      use_existing_draft: false
      tag: "v{{ .Version }}"
"##;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let release = config.crates[0].release.as_ref().unwrap();
        assert_eq!(
            release.header,
            Some(ContentSource::Inline("# Release Notes".to_string()))
        );
        assert_eq!(
            release.footer,
            Some(ContentSource::Inline("Thank you!".to_string()))
        );
        assert_eq!(
            release.extra_files.as_ref().unwrap(),
            &[ExtraFileSpec::Glob("dist/extra.zip".to_string())]
        );
        assert_eq!(release.skip_upload, Some(StringOrBool::Bool(false)));
        assert_eq!(release.replace_existing_draft, Some(true));
        assert_eq!(release.replace_existing_artifacts, Some(false));
        assert_eq!(release.make_latest, Some(MakeLatestConfig::Auto));
        assert_eq!(release.target_commitish, Some("main".to_string()));
        assert_eq!(
            release.discussion_category_name,
            Some("Announcements".to_string())
        );
        assert_eq!(release.include_meta, Some(true));
        assert_eq!(release.use_existing_draft, Some(false));
        assert_eq!(release.tag, Some("v{{ .Version }}".to_string()));
    }

    // ---- SignConfig / signs migration tests ----

    #[test]
    fn test_signs_single_object() {
        let yaml = r#"
project_name: test
signs:
  artifacts: all
  cmd: gpg
  args:
    - "--detach-sig"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.signs.len(), 1);
        assert_eq!(config.signs[0].artifacts, Some("all".to_string()));
        assert_eq!(config.signs[0].cmd, Some("gpg".to_string()));
        assert_eq!(config.signs[0].args.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn test_signs_array_format() {
        let yaml = r#"
project_name: test
signs:
  - id: gpg-sign
    artifacts: checksum
    cmd: gpg
    args:
      - "--detach-sig"
  - id: cosign-sign
    artifacts: binary
    cmd: cosign
    args:
      - "sign"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.signs.len(), 2);
        assert_eq!(config.signs[0].id, Some("gpg-sign".to_string()));
        assert_eq!(config.signs[0].artifacts, Some("checksum".to_string()));
        assert_eq!(config.signs[1].id, Some("cosign-sign".to_string()));
        assert_eq!(config.signs[1].artifacts, Some("binary".to_string()));
    }

    #[test]
    fn test_signs_omitted_is_empty() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.signs.is_empty());
    }

    #[test]
    fn test_signs_new_fields() {
        let yaml = r#"
project_name: test
signs:
  - id: my-signer
    artifacts: archive
    cmd: gpg
    args:
      - "--detach-sig"
    signature: "{{ .Artifact }}.asc"
    stdin: "my-passphrase"
    ids:
      - my-archive
      - my-binary
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.signs.len(), 1);
        let sign = &config.signs[0];
        assert_eq!(sign.id, Some("my-signer".to_string()));
        assert_eq!(sign.artifacts, Some("archive".to_string()));
        assert_eq!(sign.signature, Some("{{ .Artifact }}.asc".to_string()));
        assert_eq!(sign.stdin, Some("my-passphrase".to_string()));
        assert_eq!(sign.ids.as_ref().unwrap().len(), 2);
        assert_eq!(sign.ids.as_ref().unwrap()[0], "my-archive");
    }

    #[test]
    fn test_signs_stdin_file_field() {
        let yaml = r#"
project_name: test
signs:
  - artifacts: all
    cmd: gpg
    stdin_file: "/path/to/passphrase.txt"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.signs.len(), 1);
        assert_eq!(
            config.signs[0].stdin_file,
            Some("/path/to/passphrase.txt".to_string())
        );
    }

    #[test]
    fn test_signs_single_object_with_new_fields() {
        let yaml = r#"
project_name: test
signs:
  id: default
  artifacts: package
  cmd: gpg
  signature: "{{ .Artifact }}.sig"
  stdin: "pass"
  ids:
    - pkg-id
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.signs.len(), 1);
        let sign = &config.signs[0];
        assert_eq!(sign.id, Some("default".to_string()));
        assert_eq!(sign.artifacts, Some("package".to_string()));
        assert_eq!(sign.signature, Some("{{ .Artifact }}.sig".to_string()));
        assert_eq!(sign.stdin, Some("pass".to_string()));
        assert_eq!(sign.ids.as_ref().unwrap(), &["pkg-id"]);
    }

    #[test]
    fn test_signs_toml_single_object() {
        let toml_str = r#"
project_name = "test"

[signs]
artifacts = "checksum"
cmd = "gpg"

[[crates]]
name = "a"
path = "."
tag_template = "v{{ .Version }}"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.signs.len(), 1);
        assert_eq!(config.signs[0].artifacts, Some("checksum".to_string()));
    }

    #[test]
    fn test_signs_toml_array() {
        let toml_str = r#"
project_name = "test"

[[signs]]
id = "first"
artifacts = "all"
cmd = "gpg"

[[signs]]
id = "second"
artifacts = "binary"
cmd = "cosign"

[[crates]]
name = "a"
path = "."
tag_template = "v{{ .Version }}"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.signs.len(), 2);
        assert_eq!(config.signs[0].id, Some("first".to_string()));
        assert_eq!(config.signs[1].id, Some("second".to_string()));
    }

    #[test]
    fn test_signs_default_config_has_empty_signs() {
        let config = Config::default();
        assert!(config.signs.is_empty());
    }

    // ---- binary_signs artifacts constraint (SCH-27) ----

    #[test]
    fn test_binary_signs_artifacts_binary_accepted() {
        let yaml = r#"
project_name: test
binary_signs:
  - id: cosign-binary
    artifacts: binary
    cmd: cosign
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.binary_signs.len(), 1);
        assert_eq!(config.binary_signs[0].artifacts.as_deref(), Some("binary"));
    }

    #[test]
    fn test_binary_signs_artifacts_none_accepted() {
        let yaml = r#"
project_name: test
binary_signs:
  - artifacts: none
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.binary_signs[0].artifacts.as_deref(), Some("none"));
    }

    #[test]
    fn test_binary_signs_artifacts_omitted_accepted() {
        let yaml = r#"
project_name: test
binary_signs:
  - id: implicit
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.binary_signs[0].artifacts, None);
    }

    #[test]
    fn test_binary_signs_artifacts_archive_rejected() {
        // Anything broader than `binary` / `none` would silently match
        // nothing because the binary-sign loop only iterates Binary
        // artifacts; reject at parse time instead.
        let yaml = r#"
project_name: test
binary_signs:
  - artifacts: archive
crates: []
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(
            result.is_err(),
            "binary_signs[].artifacts: archive must be rejected"
        );
    }

    #[test]
    fn test_binary_signs_artifacts_all_rejected() {
        let yaml = r#"
project_name: test
binary_signs:
  - artifacts: all
crates: []
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(
            result.is_err(),
            "binary_signs[].artifacts: all must be rejected"
        );
    }

    #[test]
    fn test_binary_signs_artifacts_schema_is_runtime_constrained() {
        // The constraint on `binary_signs[].artifacts` lives in the custom
        // deserializer, not as a serde-typed enum, because `SignConfig` is
        // shared with the top-level `signs:` field (which legitimately
        // accepts a wider artifact filter set). The JSON schema therefore
        // inherits the unconstrained `Option<String>` shape from `SignConfig`
        // — this test pins that contract so any future schema-typing attempt
        // surfaces as a deliberate decision (and updates this test + the
        // documenting comment above `deserialize_binary_signs`).
        let schema = schemars::schema_for!(Config);
        let json = serde_json::to_value(&schema).expect("schema must serialize");
        let sign_artifacts = json
            .pointer("/definitions/SignConfig/properties/artifacts")
            .expect("SignConfig.artifacts must appear in the generated schema");
        // `artifacts` is `Option<String>` → schemars emits a nullable string
        // (`type: ["string", "null"]` on Draft-07). Either form is acceptable
        // here — the assertion is that no `enum` constraint has been added.
        assert!(
            sign_artifacts.get("enum").is_none(),
            "binary_signs[].artifacts schema must remain unconstrained \
             (constraint lives in deserialize_binary_signs); got: {sign_artifacts}"
        );
    }

    // ---- report_sizes tests ----

    #[test]
    fn test_report_sizes_true() {
        let yaml = r#"
project_name: test
report_sizes: true
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.report_sizes, Some(true));
    }

    #[test]
    fn test_report_sizes_false() {
        let yaml = r#"
project_name: test
report_sizes: false
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.report_sizes, Some(false));
    }

    #[test]
    fn test_report_sizes_omitted() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.report_sizes, None);
    }

    // ---- env tests ----

    #[test]
    fn test_env_field_parsed() {
        let yaml = r#"
project_name: test
env:
  - MY_VAR=hello
  - DEPLOY_ENV=staging
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let env = config.env.as_ref().unwrap();
        assert!(env.contains(&"MY_VAR=hello".to_string()));
        assert!(env.contains(&"DEPLOY_ENV=staging".to_string()));
    }

    #[test]
    fn test_env_field_omitted() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.env, None);
    }

    #[test]
    fn test_env_field_toml() {
        let toml_str = r#"
project_name = "test"
env = ["API_KEY=secret123", "STAGE=prod"]

[[crates]]
name = "a"
path = "."
tag_template = "v{{ .Version }}"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let env = config.env.as_ref().unwrap();
        assert!(env.contains(&"API_KEY=secret123".to_string()));
        assert!(env.contains(&"STAGE=prod".to_string()));
    }

    #[test]
    fn test_env_list_form_toml() {
        let toml_str = r#"
project_name = "test"
env = ["MY_VAR=hello", "STAGE=prod"]
crates = []
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let env = config.env.as_ref().unwrap();
        assert!(env.contains(&"MY_VAR=hello".to_string()));
        assert!(env.contains(&"STAGE=prod".to_string()));
    }

    // ---- env list form tests (GoReleaser parity) ----

    #[test]
    fn test_env_list_form_parsed() {
        let yaml = r#"
project_name: test
env:
  - MY_VAR=hello
  - DEPLOY_ENV=staging
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let env = config.env.as_ref().unwrap();
        assert!(env.contains(&"MY_VAR=hello".to_string()));
        assert!(env.contains(&"DEPLOY_ENV=staging".to_string()));
    }

    #[test]
    fn test_env_list_form_with_template_expressions() {
        let yaml = r#"
project_name: test
env:
  - "MY_VERSION={{ .Tag }}"
  - "BUILD_DATE={{ .Date }}"
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let env = config.env.as_ref().unwrap();
        // Values are stored raw; template rendering happens at setup_env time.
        assert!(env.contains(&"MY_VERSION={{ .Tag }}".to_string()));
        assert!(env.contains(&"BUILD_DATE={{ .Date }}".to_string()));
    }

    #[test]
    fn test_env_list_form_value_with_equals() {
        // Values can contain = signs (only the first = splits key from value).
        let yaml = r#"
project_name: test
env:
  - "LDFLAGS=-X main.version=1.0.0"
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let env = config.env.as_ref().unwrap();
        assert!(
            env.contains(&"LDFLAGS=-X main.version=1.0.0".to_string()),
            "only first = should split key from value"
        );
    }

    #[test]
    fn test_env_list_form_empty_value() {
        let yaml = r#"
project_name: test
env:
  - "EMPTY_VAR="
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let env = config.env.as_ref().unwrap();
        assert!(env.contains(&"EMPTY_VAR=".to_string()));
    }

    #[test]
    fn test_env_list_form_no_equals_is_error() {
        // Vec<String> accepts any string at parse time; validation happens when
        // parse_env_entries is called by consumers (e.g. setup_env).
        let yaml = r#"
project_name: test
env:
  - "NO_EQUALS"
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let env = config.env.as_ref().unwrap();
        let err = super::parse_env_entries(env).unwrap_err();
        assert!(
            err.to_string().contains("KEY=VALUE"),
            "parse_env_entries should mention KEY=VALUE format, got: {}",
            err
        );
    }

    #[test]
    fn test_env_list_form_empty_key_is_error() {
        // Vec<String> accepts any string at parse time; validation happens when
        // parse_env_entries is called by consumers (e.g. setup_env).
        let yaml = r#"
project_name: test
env:
  - "=orphan_value"
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let env = config.env.as_ref().unwrap();
        let err = super::parse_env_entries(env).unwrap_err();
        assert!(
            err.to_string().contains("empty key"),
            "parse_env_entries should mention empty key, got: {}",
            err
        );
    }

    #[test]
    fn test_env_list_form_last_wins_on_duplicates() {
        let yaml = r#"
project_name: test
env:
  - "DUPED=first"
  - "DUPED=second"
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let env = config.env.as_ref().unwrap();
        // Vec<String> preserves all entries; consumers use last-wins semantics when iterating
        assert!(
            env.contains(&"DUPED=second".to_string()),
            "later entries should be present"
        );
    }

    #[test]
    fn test_workspace_env_list_form() {
        let yaml = r#"
project_name: test
crates: []
workspaces:
  - name: ws1
    crates: []
    env:
      - "WS_VAR=from-workspace"
      - "WS_BUILD={{ .Tag }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let ws = &config.workspaces.as_ref().unwrap()[0];
        let env = ws.env.as_ref().unwrap();
        assert!(env.contains(&"WS_VAR=from-workspace".to_string()));
        assert!(env.contains(&"WS_BUILD={{ .Tag }}".to_string()));
    }

    // ---- Error path tests (Task 3B) ----

    #[test]
    fn test_malformed_yaml_syntax_error() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
  invalid_indentation
    this_is_broken: [
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(result.is_err(), "malformed YAML should fail to parse");
        let err = result.unwrap_err().to_string();
        // Serde_yaml errors include line/column info
        assert!(!err.is_empty(), "error message should not be empty");
    }

    #[test]
    fn test_type_mismatch_string_where_array_expected() {
        let yaml = r#"
project_name: test
crates: "this should be an array"
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(result.is_err(), "string where array expected should fail");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("invalid type") || err.contains("expected a sequence"),
            "error should mention type mismatch, got: {err}"
        );
    }

    #[test]
    fn test_type_mismatch_object_where_string_expected() {
        // An object (mapping) where a string is expected for project_name
        // should be rejected by serde_yaml_ng, unlike a number which gets coerced.
        let yaml = r#"
project_name:
  nested: object
  another: field
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(
            result.is_err(),
            "mapping where string expected should fail to parse"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("invalid type") || err.contains("expected a string"),
            "error should mention type mismatch, got: {err}"
        );
    }

    #[test]
    fn test_type_mismatch_bool_where_array_expected_for_targets() {
        let yaml = r#"
project_name: test
defaults:
  targets: true
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(
            result.is_err(),
            "bool where array expected for targets should fail"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("invalid type")
                || err.contains("expected a sequence")
                || err.contains("targets"),
            "error should mention type mismatch for targets, got: {err}"
        );
    }

    #[test]
    fn test_invalid_cross_strategy_value() {
        let yaml = r#"
project_name: test
defaults:
  cross: invalid_strategy
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(
            result.is_err(),
            "invalid cross strategy should fail to parse"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("unknown variant") || err.contains("invalid_strategy"),
            "error should mention the invalid variant, got: {err}"
        );
    }

    #[test]
    fn test_prerelease_invalid_string_value() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      prerelease: "always"
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(
            result.is_err(),
            "prerelease: 'always' should fail (only 'auto' or bool accepted)"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("auto") || err.contains("always"),
            "error should mention expected values, got: {err}"
        );
    }

    #[test]
    fn test_archives_true_is_invalid() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    archives: true
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(
            result.is_err(),
            "archives: true should be rejected (only false or array accepted)"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("true is not valid") || err.contains("false or a list"),
            "error should explain valid archives values, got: {err}"
        );
    }

    #[test]
    fn test_completely_empty_yaml() {
        // Empty YAML deserializes to defaults because Config uses #[serde(default)].
        // serde_yaml_ng treats empty input as `null`, which the default impl handles.
        let yaml = "";
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        let config =
            result.unwrap_or_else(|e| panic!("empty YAML should parse to Config defaults: {e}"));
        assert!(
            config.project_name.is_empty(),
            "default project_name should be empty"
        );
        assert!(config.crates.is_empty(), "default crates should be empty");
        assert_eq!(
            config.dist,
            std::path::PathBuf::from("./dist"),
            "default dist should be ./dist"
        );
    }

    // ---- Unknown fields tests ----

    // ---- BinstallConfig / VersionSyncConfig tests ----

    #[test]
    fn test_binstall_config_parsed() {
        let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    binstall:
      enabled: true
      pkg_url: "https://example.com/{{ .Version }}/{ target }"
      bin_dir: "{ bin }{ binary-ext }"
      pkg_fmt: tgz
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let bs = config.crates[0].binstall.as_ref().unwrap();
        assert_eq!(bs.enabled, Some(true));
        assert_eq!(
            bs.pkg_url,
            Some("https://example.com/{{ .Version }}/{ target }".to_string())
        );
        assert_eq!(bs.bin_dir, Some("{ bin }{ binary-ext }".to_string()));
        assert_eq!(bs.pkg_fmt, Some("tgz".to_string()));
    }

    #[test]
    fn test_binstall_config_omitted() {
        let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.crates[0].binstall.is_none());
    }

    #[test]
    fn test_binstall_config_partial() {
        let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    binstall:
      enabled: true
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let bs = config.crates[0].binstall.as_ref().unwrap();
        assert_eq!(bs.enabled, Some(true));
        assert_eq!(bs.pkg_url, None);
        assert_eq!(bs.bin_dir, None);
        assert_eq!(bs.pkg_fmt, None);
    }

    #[test]
    fn test_version_sync_config_parsed() {
        let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    version_sync:
      enabled: true
      mode: tag
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let vs = config.crates[0].version_sync.as_ref().unwrap();
        assert_eq!(vs.enabled, Some(true));
        assert_eq!(vs.mode, Some("tag".to_string()));
    }

    #[test]
    fn test_version_sync_config_explicit_mode() {
        let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    version_sync:
      enabled: true
      mode: explicit
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let vs = config.crates[0].version_sync.as_ref().unwrap();
        assert_eq!(vs.mode, Some("explicit".to_string()));
    }

    #[test]
    fn test_version_sync_config_omitted() {
        let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.crates[0].version_sync.is_none());
    }

    #[test]
    fn test_binstall_and_version_sync_together() {
        let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    binstall:
      enabled: true
      pkg_fmt: zip
    version_sync:
      enabled: true
      mode: tag
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.crates[0].binstall.is_some());
        assert!(config.crates[0].version_sync.is_some());
    }

    #[test]
    fn test_binstall_config_toml() {
        let toml_str = r#"
project_name = "test"

[[crates]]
name = "myapp"
path = "."
tag_template = "v{{ .Version }}"

[crates.binstall]
enabled = true
pkg_url = "https://example.com"
pkg_fmt = "tgz"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let bs = config.crates[0].binstall.as_ref().unwrap();
        assert_eq!(bs.enabled, Some(true));
        assert_eq!(bs.pkg_url, Some("https://example.com".to_string()));
    }

    #[test]
    fn test_version_sync_config_toml() {
        let toml_str = r#"
project_name = "test"

[[crates]]
name = "myapp"
path = "."
tag_template = "v{{ .Version }}"

[crates.version_sync]
enabled = true
mode = "tag"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let vs = config.crates[0].version_sync.as_ref().unwrap();
        assert_eq!(vs.enabled, Some(true));
        assert_eq!(vs.mode, Some("tag".to_string()));
    }

    #[test]
    fn test_crate_config_default_has_none_binstall_version_sync() {
        let config = CrateConfig::default();
        assert!(config.binstall.is_none());
        assert!(config.version_sync.is_none());
    }

    // ---- Unknown fields tests ----

    #[test]
    fn test_unknown_top_level_fields_rejected() {
        // strict YAML parsing rejects unknown fields
        let yaml = r#"
project_name: test
unknown_top_level_field: "this should be rejected"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(
            result.is_err(),
            "unknown top-level fields should be rejected"
        );
        assert!(
            result.unwrap_err().to_string().contains("unknown field"),
            "error should mention unknown field"
        );
    }

    #[test]
    fn test_unknown_crate_level_fields_ignored() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    nonexistent_field: true
    something_else: "hello"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.crates[0].name, "a");
    }

    #[test]
    fn test_unknown_nested_fields_ignored() {
        let yaml = r#"
project_name: test
defaults:
  targets:
    - x86_64-unknown-linux-gnu
  unknown_default_field: "ignored"
changelog:
  sort: asc
  mystery_option: true
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    checksum:
      algorithm: sha256
      future_field: "ignored"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(
            config
                .defaults
                .as_ref()
                .unwrap()
                .targets
                .as_ref()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            config.changelog.as_ref().unwrap().sort,
            Some("asc".to_string())
        );
        assert_eq!(
            config.crates[0].checksum.as_ref().unwrap().algorithm,
            Some("sha256".to_string())
        );
    }

    // ---- BuildConfig reproducible field tests ----

    #[test]
    fn test_build_config_reproducible_true() {
        let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: myapp
        reproducible: true
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let build = &config.crates[0].builds.as_ref().unwrap()[0];
        assert_eq!(build.reproducible, Some(true));
    }

    #[test]
    fn test_build_config_reproducible_false() {
        let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: myapp
        reproducible: false
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let build = &config.crates[0].builds.as_ref().unwrap()[0];
        assert_eq!(build.reproducible, Some(false));
    }

    #[test]
    fn test_build_config_reproducible_omitted() {
        let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: myapp
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let build = &config.crates[0].builds.as_ref().unwrap()[0];
        assert_eq!(build.reproducible, None);
    }

    // ---- WorkspaceConfig tests ----

    #[test]
    fn test_workspace_config_parses() {
        let yaml = r#"
project_name: monorepo
crates: []
workspaces:
  - name: frontend
    crates:
      - name: frontend-app
        path: "apps/frontend"
        tag_template: "frontend-v{{ .Version }}"
    changelog:
      sort: asc
  - name: backend
    crates:
      - name: backend-api
        path: "apps/backend"
        tag_template: "backend-v{{ .Version }}"
      - name: backend-worker
        path: "apps/worker"
        tag_template: "worker-v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let workspaces = config.workspaces.as_ref().unwrap();
        assert_eq!(workspaces.len(), 2);
        assert_eq!(workspaces[0].name, "frontend");
        assert_eq!(workspaces[0].crates.len(), 1);
        assert_eq!(workspaces[0].crates[0].name, "frontend-app");
        assert!(workspaces[0].changelog.is_some());
        assert_eq!(workspaces[1].name, "backend");
        assert_eq!(workspaces[1].crates.len(), 2);
    }

    #[test]
    fn test_workspace_config_with_signs_and_hooks() {
        let yaml = r#"
project_name: monorepo
crates: []
workspaces:
  - name: myws
    crates:
      - name: mylib
        path: "."
        tag_template: "v{{ .Version }}"
    signs:
      - artifacts: all
        cmd: gpg
    before:
      hooks:
        - echo before
    after:
      hooks:
        - echo after
    env:
      - MY_VAR=hello
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let ws = &config.workspaces.as_ref().unwrap()[0];
        assert_eq!(ws.name, "myws");
        assert_eq!(ws.signs.len(), 1);
        assert!(ws.before.is_some());
        assert!(ws.after.is_some());
        assert!(
            ws.env
                .as_ref()
                .unwrap()
                .contains(&"MY_VAR=hello".to_string())
        );
    }

    #[test]
    fn test_workspace_config_omitted() {
        let yaml = r#"
project_name: simple
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.workspaces.is_none());
    }

    #[test]
    fn test_workspace_config_empty_array() {
        let yaml = r#"
project_name: test
crates: []
workspaces: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let workspaces = config.workspaces.as_ref().unwrap();
        assert!(workspaces.is_empty());
    }

    // ---- ChocolateyConfig tests ----

    #[test]
    fn test_chocolatey_config_toml() {
        // ChocolateyConfig.repository is the unified RepositoryConfig form
        // (owner/name + token/branch/...).
        let toml_str = r#"
project_name = "test"

[[crates]]
name = "mytool"
path = "."
tag_template = "v{{ .Version }}"

[crates.publish.chocolatey]
description = "A tool"
license = "MIT"
authors = "Author"
tags = ["cli"]

[crates.publish.chocolatey.repository]
owner = "org"
name = "tool"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let choco = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .chocolatey
            .as_ref()
            .unwrap();

        assert_eq!(choco.description, Some("A tool".to_string()));
        let repo = choco.repository.as_ref().unwrap();
        assert_eq!(repo.owner.as_deref(), Some("org"));
    }

    // ---- WAVE 5.7 behavior-toggle test (SCH-26) ----

    #[test]
    fn test_changelog_snapshot_field_parses() {
        // The `changelog.snapshot: true` opt-in parses + round-trips on
        // ChangelogConfig. Behavior wiring lives in
        // `crates/stage-changelog/src/lib.rs::ChangelogStage::run`.
        let yaml = r#"
project_name: test
changelog:
  snapshot: true
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let cl = config.changelog.as_ref().unwrap();
        assert_eq!(cl.snapshot, Some(true));
    }

    #[test]
    fn test_changelog_snapshot_omitted_is_none() {
        let yaml = r#"
project_name: test
changelog:
  sort: asc
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let cl = config.changelog.as_ref().unwrap();
        assert_eq!(cl.snapshot, None);
    }

    // ---- Plural-canonical key + alias tests ----

    #[test]
    fn test_top_level_plural_canonical_keys_parse() {
        // The plural canonical keys (nfpms, dmgs, msis, flatpaks) are
        // anodizer's only spelling at top level — singular forms would
        // be rejected as unknown fields.
        let yaml = r#"
project_name: test
defaults:
  nfpms:
    formats: [deb]
  dmgs:
    name: test
  msis:
    name: test
  flatpaks:
    runtime: org.freedesktop.Platform
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let d = config.defaults.unwrap();
        assert!(d.nfpms.is_some());
        assert!(d.dmgs.is_some());
        assert!(d.msis.is_some());
        assert!(d.flatpaks.is_some());
    }

    #[test]
    fn test_makeself_filename_field() {
        // SCH-11 (DEC-5 hard-break): `filename:` is the canonical field name.
        let yaml = r#"
project_name: test
makeselfs:
  - id: default
    filename: "myapp-{{ .Version }}.run"
    script: install.sh
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(
            config.makeselfs[0].filename.as_deref(),
            Some("myapp-{{ .Version }}.run")
        );
    }

    #[test]
    fn test_announce_smtp_aliases_email() {
        // Mirrors GR's own `smtp:` → `email:` rename (GR keeps both as
        // aliases; anodizer matches).
        let yaml = r#"
project_name: test
announce:
  smtp:
    enabled: true
    host: smtp.example.com
    port: 587
    username: user
    from: from@example.com
    to: ["to@example.com"]
    subject_template: "Release {{ .Version }}"
    message_template: "Body"
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(
            config.announce.unwrap().email.is_some(),
            "smtp: should alias to email:"
        );
    }

    #[test]
    fn test_announce_canonical_email_still_works() {
        let yaml = r#"
project_name: test
announce:
  email:
    enabled: true
    host: smtp.example.com
    port: 587
    username: user
    from: from@example.com
    to: ["to@example.com"]
    subject_template: "Release {{ .Version }}"
    message_template: "Body"
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.announce.unwrap().email.is_some());
    }

    // ---- Legacy-field rejection tests (post-DEC-5 hard-break shape) ----

    #[test]
    fn test_legacy_docker_field_rejected() {
        // `crates[].docker:` is no longer a recognized field. Any value
        // parses (CrateConfig isn't deny_unknown_fields) but it has nowhere
        // to land — confirm via explicit absence.
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    docker_v2:
      - images: [registry/img]
        tags: ["{{ .Version }}"]
        dockerfile: Dockerfile
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.crates[0].docker_v2.is_some());
        // No `docker` field exists on CrateConfig anymore.
    }

    #[test]
    fn test_homebrew_legacy_commit_author_flat_fields_rejected() {
        // HomebrewConfig has `#[serde(deny_unknown_fields)]`, so the
        // dropped flat fields fail to parse outright.
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      homebrew:
        commit_author_name: TJ
        commit_author_email: tj@example.com
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(
            result.is_err(),
            "homebrew.commit_author_name must be rejected; use commit_author block"
        );
    }

    // ScoopConfig has `#[serde(deny_unknown_fields)]`. Use the structured
    // `commit_author: { name, email, signing }` block; the flat fields
    // must fail parsing.
    #[test]
    fn test_scoop_legacy_commit_author_flat_fields_rejected() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      scoop:
        commit_author_name: TJ
        commit_author_email: tj@example.com
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(
            result.is_err(),
            "scoop.commit_author_name must be rejected; use commit_author block"
        );
    }

    #[test]
    fn test_aur_legacy_url_field_rejected() {
        // AurConfig has `deny_unknown_fields`; the dropped legacy `url:`
        // field must fail parsing (PKGBUILD url= resolves through
        // homepage → crate metadata → derived github URL).
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      aur:
        url: "https://example.com/a"
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(result.is_err(), "aur.url must be rejected; use homepage");
    }

    #[test]
    fn test_homebrew_legacy_tap_field_rejected() {
        // HomebrewConfig has `deny_unknown_fields`; legacy `tap:` is
        // gone (use `repository:`).
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      homebrew:
        tap:
          owner: x
          name: y
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(result.is_err(), "homebrew.tap must be rejected");
    }

    #[test]
    fn test_scoop_legacy_bucket_field_rejected() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      scoop:
        bucket:
          owner: x
          name: y
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(result.is_err(), "scoop.bucket must be rejected");
    }

    #[test]
    fn test_winget_legacy_manifests_repo_rejected() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      winget:
        manifests_repo:
          owner: x
          name: y
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(result.is_err(), "winget.manifests_repo must be rejected");
    }

    #[test]
    fn test_chocolatey_legacy_project_repo_rejected() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      chocolatey:
        project_repo:
          owner: x
          name: y
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(
            result.is_err(),
            "chocolatey.project_repo must be rejected (use repository)"
        );
    }

    #[test]
    fn test_krew_legacy_manifests_repo_rejected() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      krew:
        manifests_repo:
          owner: x
          name: y
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(result.is_err(), "krew.manifests_repo must be rejected");
    }

    #[test]
    fn test_krew_legacy_upstream_repo_rejected() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      krew:
        upstream_repo:
          owner: x
          name: y
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(result.is_err(), "krew.upstream_repo must be rejected");
    }

    #[test]
    fn test_notarize_macos_skip_roundtrip() {
        // `skip:` is the canonical per-config gating field (DEC-6); known-good
        // YAML with `skip: false` parses cleanly.
        let yaml = r#"
notarize:
  macos:
    - skip: false
      sign:
        certificate: /tmp/cert.p12
        password: pw
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let macos = config.notarize.unwrap().macos.unwrap();
        assert_eq!(macos[0].skip, Some(StringOrBool::Bool(false)));
    }

    #[test]
    fn test_notarize_macos_legacy_enabled_rejected() {
        // `deny_unknown_fields` must reject legacy `enabled: true` on
        // MacOSSignNotarizeConfig — without it, the field would silently
        // drop and produce a confusing no-op pipeline run.
        let yaml = r#"
notarize:
  macos:
    - enabled: true
      sign:
        certificate: /tmp/cert.p12
        password: pw
crates: []
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(
            result.is_err(),
            "legacy `enabled:` on MacOSSignNotarizeConfig must be rejected by deny_unknown_fields"
        );
    }

    #[test]
    fn test_notarize_macos_native_legacy_enabled_rejected() {
        // Same `deny_unknown_fields` check for MacOSNativeSignNotarizeConfig.
        let yaml = r#"
notarize:
  macos_native:
    - enabled: true
crates: []
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(
            result.is_err(),
            "legacy `enabled:` on MacOSNativeSignNotarizeConfig must be rejected by deny_unknown_fields"
        );
    }

    #[test]
    fn test_notarize_top_level_unknown_field_rejected() {
        // Unknown fields on the top-level NotarizeConfig are also rejected
        // via `deny_unknown_fields`.
        let yaml = r#"
notarize:
  enabled: true
crates: []
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(
            result.is_err(),
            "unknown field `enabled` on NotarizeConfig must be rejected"
        );
    }

    // ---- Unified nFPM/SRPM content + signature tests ----

    #[test]
    fn test_nfpm_content_canonical_keys_in_srpm_full() {
        // SRPM contents share [`NfpmContent`]; canonical `src`/`dst` keys
        // are required (DEC-5 dropped the `source`/`destination` aliases).
        let yaml = r#"
project_name: test
srpm:
  enabled: true
  contents:
    - src: ./LICENSE
      dst: /usr/share/doc/myapp/LICENSE
      type: doc
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let contents = config.srpms.as_ref().unwrap().contents.as_ref().unwrap();
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].src, "./LICENSE");
        assert_eq!(contents[0].dst, "/usr/share/doc/myapp/LICENSE");
        assert_eq!(contents[0].content_type.as_deref(), Some("doc"));
    }

    #[test]
    fn test_nfpm_content_canonical_keys_in_srpm() {
        // Canonical `src` / `dst` keys also work in srpm contents.
        let yaml = r#"
project_name: test
srpm:
  enabled: true
  contents:
    - src: ./README.md
      dst: /usr/share/doc/myapp/README.md
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let contents = config.srpms.as_ref().unwrap().contents.as_ref().unwrap();
        assert_eq!(contents[0].src, "./README.md");
    }

    #[test]
    fn test_nfpm_signature_canonical_passphrase() {
        // SRPM signatures share [`NfpmSignatureConfig`]; canonical
        // `key_passphrase:` is the only accepted spelling (DEC-5 dropped
        // the `passphrase:` alias).
        let yaml = r#"
project_name: test
srpm:
  enabled: true
  signature:
    key_file: /keys/srpm.gpg
    key_passphrase: "s3cret"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let sig = config.srpms.as_ref().unwrap().signature.as_ref().unwrap();
        assert_eq!(sig.key_file.as_deref(), Some("/keys/srpm.gpg"));
        assert_eq!(sig.key_passphrase.as_deref(), Some("s3cret"));
    }

    #[test]
    fn test_srpm_singular_alias_still_accepted() {
        // H4: Config.srpm renamed to Config.srpms for parity with
        // Defaults.srpms; the legacy `srpm:` spelling stays accepted via
        // serde alias.
        let yaml_legacy = r#"
project_name: test
srpm:
  enabled: true
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let yaml_canonical = r#"
project_name: test
srpms:
  enabled: true
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let legacy: Config = serde_yaml_ng::from_str(yaml_legacy).unwrap();
        let canonical: Config = serde_yaml_ng::from_str(yaml_canonical).unwrap();
        assert!(legacy.srpms.is_some(), "srpm: alias must populate srpms");
        assert!(canonical.srpms.is_some(), "srpms: must populate srpms");
        assert_eq!(
            legacy.srpms.as_ref().unwrap().enabled,
            canonical.srpms.as_ref().unwrap().enabled
        );
    }

    #[test]
    fn test_nfpm_singular_alias_still_accepted() {
        // H4: CrateConfig.nfpm renamed to CrateConfig.nfpms for parity with
        // every other plural-name per-crate packaging list (`dmgs`, `msis`,
        // `pkgs`, ...). The legacy `nfpm:` spelling stays accepted via serde
        // alias.
        let yaml_legacy = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    nfpm:
      - id: deb
        formats: [deb]
"#;
        let yaml_canonical = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    nfpms:
      - id: deb
        formats: [deb]
"#;
        let legacy: Config = serde_yaml_ng::from_str(yaml_legacy).unwrap();
        let canonical: Config = serde_yaml_ng::from_str(yaml_canonical).unwrap();
        assert_eq!(legacy.crates[0].nfpms.as_ref().unwrap().len(), 1);
        assert_eq!(canonical.crates[0].nfpms.as_ref().unwrap().len(), 1);
        assert_eq!(
            legacy.crates[0].nfpms.as_ref().unwrap()[0].id,
            canonical.crates[0].nfpms.as_ref().unwrap()[0].id
        );
    }

    // ---- WingetConfig tests ----

    // ---- AurConfig tests ----

    #[test]
    fn test_aur_config_yaml() {
        let yaml = r#"
project_name: test
crates:
  - name: mytool
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      aur:
        git_url: "ssh://aur@aur.archlinux.org/mytool.git"
        name: mytool-bin
        description: "A great tool"
        license: MIT
        maintainers:
          - "Jane Doe <jane@example.com>"
        depends:
          - glibc
          - openssl
        optdepends:
          - "git: for VCS support"
        conflicts:
          - mytool-git
        provides:
          - mytool
        replaces:
          - old-mytool
        backup:
          - etc/mytool/config.toml
        homepage: "https://github.com/org/mytool"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let aur = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .aur
            .as_ref()
            .unwrap();

        assert_eq!(
            aur.git_url,
            Some("ssh://aur@aur.archlinux.org/mytool.git".to_string())
        );
        assert_eq!(aur.name, Some("mytool-bin".to_string()));
        assert_eq!(aur.description, Some("A great tool".to_string()));
        assert_eq!(aur.license, Some("MIT".to_string()));
        assert_eq!(
            aur.maintainers,
            Some(vec!["Jane Doe <jane@example.com>".to_string()])
        );
        assert_eq!(
            aur.depends,
            Some(vec!["glibc".to_string(), "openssl".to_string()])
        );
        assert_eq!(
            aur.optdepends,
            Some(vec!["git: for VCS support".to_string()])
        );
        assert_eq!(aur.conflicts, Some(vec!["mytool-git".to_string()]));
        assert_eq!(aur.provides, Some(vec!["mytool".to_string()]));
        assert_eq!(aur.replaces, Some(vec!["old-mytool".to_string()]));
        assert_eq!(aur.backup, Some(vec!["etc/mytool/config.toml".to_string()]));
        assert_eq!(
            aur.homepage,
            Some("https://github.com/org/mytool".to_string())
        );
    }

    #[test]
    fn test_aur_config_minimal() {
        let yaml = r#"
project_name: test
crates:
  - name: mytool
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      aur:
        git_url: "ssh://aur@aur.archlinux.org/mytool.git"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let aur = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .aur
            .as_ref()
            .unwrap();

        assert_eq!(
            aur.git_url,
            Some("ssh://aur@aur.archlinux.org/mytool.git".to_string())
        );
        assert!(aur.name.is_none());
        assert!(aur.description.is_none());
        assert!(aur.license.is_none());
        assert!(aur.maintainers.is_none());
        assert!(aur.depends.is_none());
        assert!(aur.optdepends.is_none());
        assert!(aur.conflicts.is_none());
        assert!(aur.provides.is_none());
        assert!(aur.replaces.is_none());
        assert!(aur.backup.is_none());
    }

    #[test]
    fn test_aur_config_toml() {
        let toml_str = r#"
project_name = "test"

[[crates]]
name = "mytool"
path = "."
tag_template = "v{{ .Version }}"

[crates.publish.aur]
git_url = "ssh://aur@aur.archlinux.org/mytool.git"
description = "A tool"
license = "MIT"
depends = ["glibc"]
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let aur = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .aur
            .as_ref()
            .unwrap();

        assert_eq!(
            aur.git_url,
            Some("ssh://aur@aur.archlinux.org/mytool.git".to_string())
        );
        assert_eq!(aur.description, Some("A tool".to_string()));
        assert_eq!(aur.depends, Some(vec!["glibc".to_string()]));
    }

    // ---- KrewConfig tests ----

    // ---- Combined all publishers ----

    // ---- Config version tests ----

    #[test]
    fn test_version_field_none_is_valid() {
        let config = Config::default();
        assert!(validate_version(&config).is_ok());
    }

    #[test]
    fn test_version_field_1_is_valid() {
        let yaml = r#"
project_name: test
version: 1
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.version, Some(1));
        assert!(validate_version(&config).is_ok());
    }

    #[test]
    fn test_version_field_2_is_valid() {
        let yaml = r#"
project_name: test
version: 2
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.version, Some(2));
        assert!(validate_version(&config).is_ok());
    }

    #[test]
    fn test_version_field_99_is_rejected() {
        let config = Config {
            version: Some(99),
            ..Default::default()
        };
        let result = validate_version(&config);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("unsupported config version: 99")
        );
    }

    // ---- env_files tests ----

    #[test]
    fn test_env_files_list_form_parses() {
        let yaml = r#"
project_name: test
env_files:
  - ".env"
  - ".release.env"
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let env_files = config.env_files.unwrap();
        let files = env_files
            .as_list()
            .unwrap_or_else(|| panic!("expected List variant"));
        assert_eq!(files.len(), 2);
        assert_eq!(files[0], ".env");
        assert_eq!(files[1], ".release.env");
    }

    #[test]
    fn test_env_files_struct_form_parses() {
        let yaml = r#"
project_name: test
env_files:
  github_token: "~/.config/goreleaser/github_token"
  gitlab_token: "/etc/tokens/gitlab"
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let env_files = config.env_files.unwrap();
        let tokens = env_files
            .as_token_files()
            .unwrap_or_else(|| panic!("expected TokenFiles variant"));
        assert_eq!(
            tokens.github_token.as_deref(),
            Some("~/.config/goreleaser/github_token")
        );
        assert_eq!(tokens.gitlab_token.as_deref(), Some("/etc/tokens/gitlab"));
        assert!(tokens.gitea_token.is_none());
    }

    #[test]
    fn test_env_files_struct_form_empty_mapping() {
        let yaml = r#"
project_name: test
env_files:
  gitea_token: "/tmp/gitea"
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let env_files = config.env_files.unwrap();
        let tokens = env_files
            .as_token_files()
            .unwrap_or_else(|| panic!("expected TokenFiles variant"));
        assert!(tokens.github_token.is_none());
        assert!(tokens.gitlab_token.is_none());
        assert_eq!(tokens.gitea_token.as_deref(), Some("/tmp/gitea"));
    }

    #[test]
    fn test_env_files_field_omitted() {
        let yaml = r#"
project_name: test
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.env_files.is_none());
    }

    #[test]
    fn test_read_token_file_reads_first_line() {
        use std::io::Write;
        let dir = tempfile::TempDir::new().unwrap();
        let token_path = dir.path().join("github_token");
        let mut f = std::fs::File::create(&token_path).unwrap();
        writeln!(f, "ghp_abc123xyz").unwrap();
        writeln!(f, "this line should be ignored").unwrap();
        drop(f);

        let result = read_token_file(&token_path.to_string_lossy()).unwrap();
        assert_eq!(result, Some("ghp_abc123xyz".to_string()));
    }

    #[test]
    fn test_read_token_file_trims_whitespace() {
        use std::io::Write;
        let dir = tempfile::TempDir::new().unwrap();
        let token_path = dir.path().join("token");
        let mut f = std::fs::File::create(&token_path).unwrap();
        writeln!(f, "  spaced_token  ").unwrap();
        drop(f);

        let result = read_token_file(&token_path.to_string_lossy()).unwrap();
        assert_eq!(result, Some("spaced_token".to_string()));
    }

    #[test]
    fn test_read_token_file_nonexistent_returns_none() {
        let result = read_token_file("/tmp/nonexistent_token_file_99999").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_read_token_file_empty_returns_none() {
        let dir = tempfile::TempDir::new().unwrap();
        let token_path = dir.path().join("empty_token");
        std::fs::write(&token_path, "").unwrap();

        let result = read_token_file(&token_path.to_string_lossy()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    #[serial_test::serial]
    fn test_load_token_files_reads_tokens() {
        use std::io::Write;
        let dir = tempfile::TempDir::new().unwrap();

        let gh_path = dir.path().join("github_token");
        let mut f = std::fs::File::create(&gh_path).unwrap();
        writeln!(f, "ghp_test123").unwrap();
        drop(f);

        let gl_path = dir.path().join("gitlab_token");
        let mut f = std::fs::File::create(&gl_path).unwrap();
        writeln!(f, "glpat-test456").unwrap();
        drop(f);

        let config = EnvFilesTokenConfig {
            github_token: Some(gh_path.to_string_lossy().to_string()),
            gitlab_token: Some(gl_path.to_string_lossy().to_string()),
            gitea_token: None, // uses default path which won't exist
        };

        // Temporarily unset any existing tokens to avoid interference
        let orig_gh = std::env::var("GITHUB_TOKEN").ok();
        let orig_gl = std::env::var("GITLAB_TOKEN").ok();
        let orig_gt = std::env::var("GITEA_TOKEN").ok();
        // SAFETY: test runs serially
        unsafe {
            std::env::remove_var("GITHUB_TOKEN");
            std::env::remove_var("GITLAB_TOKEN");
            std::env::remove_var("GITEA_TOKEN");
        }

        let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Normal);
        let vars = load_token_files(&config, &log).unwrap();

        // Restore original env
        unsafe {
            if let Some(v) = orig_gh {
                std::env::set_var("GITHUB_TOKEN", v);
            }
            if let Some(v) = orig_gl {
                std::env::set_var("GITLAB_TOKEN", v);
            }
            if let Some(v) = orig_gt {
                std::env::set_var("GITEA_TOKEN", v);
            }
        }

        assert_eq!(vars.get("GITHUB_TOKEN").unwrap(), "ghp_test123");
        assert_eq!(vars.get("GITLAB_TOKEN").unwrap(), "glpat-test456");
        // GITEA_TOKEN not present — default file doesn't exist
        assert!(!vars.contains_key("GITEA_TOKEN"));
    }

    #[test]
    #[serial_test::serial]
    fn test_load_token_files_env_var_takes_precedence() {
        use std::io::Write;
        let dir = tempfile::TempDir::new().unwrap();

        let gh_path = dir.path().join("github_token");
        let mut f = std::fs::File::create(&gh_path).unwrap();
        writeln!(f, "file_token").unwrap();
        drop(f);

        let config = EnvFilesTokenConfig {
            github_token: Some(gh_path.to_string_lossy().to_string()),
            gitlab_token: None,
            gitea_token: None,
        };

        // Set GITHUB_TOKEN env var — should take precedence over file
        let orig = std::env::var("GITHUB_TOKEN").ok();
        // SAFETY: test runs serially
        unsafe {
            std::env::set_var("GITHUB_TOKEN", "env_token");
        }

        let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Normal);
        let vars = load_token_files(&config, &log).unwrap();

        // Restore
        unsafe {
            match orig {
                Some(v) => std::env::set_var("GITHUB_TOKEN", v),
                None => std::env::remove_var("GITHUB_TOKEN"),
            }
        }

        // File token should NOT be loaded because env var was set
        assert!(
            !vars.contains_key("GITHUB_TOKEN"),
            "env var should take precedence; file should not be loaded"
        );
    }

    #[test]
    fn test_read_token_file_tilde_expansion() {
        // Test that tilde expansion uses HOME env var
        let dir = tempfile::TempDir::new().unwrap();
        let token_path = dir.path().join(".config/goreleaser/github_token");
        std::fs::create_dir_all(token_path.parent().unwrap()).unwrap();
        std::fs::write(&token_path, "tilde_token\n").unwrap();

        let orig_home = std::env::var("HOME").ok();
        // SAFETY: test runs serially
        unsafe {
            std::env::set_var("HOME", dir.path());
        }

        let result = read_token_file("~/.config/goreleaser/github_token").unwrap();

        unsafe {
            match orig_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }

        assert_eq!(result, Some("tilde_token".to_string()));
    }

    #[test]
    fn test_load_env_files_sets_vars() {
        use std::io::Write;
        let dir = tempfile::TempDir::new().unwrap();
        let env_path = dir.path().join(".env");
        let mut f = std::fs::File::create(&env_path).unwrap();
        writeln!(f, "# comment line").unwrap();
        writeln!(f).unwrap();
        writeln!(f, "TEST_ANODIZER_KEY=hello_world").unwrap();
        writeln!(f, "TEST_ANODIZER_QUOTED=\"with quotes\"").unwrap();
        writeln!(f, "TEST_ANODIZER_SINGLE='single_quoted'").unwrap();
        writeln!(f, "export TEST_ANODIZER_EXPORT=exported_val").unwrap();
        drop(f);

        let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Normal);
        let vars = load_env_files(&[env_path.to_string_lossy().to_string()], &log, false).unwrap();
        assert_eq!(vars.get("TEST_ANODIZER_KEY").unwrap(), "hello_world");
        assert_eq!(vars.get("TEST_ANODIZER_QUOTED").unwrap(), "with quotes");
        assert_eq!(
            vars.get("TEST_ANODIZER_SINGLE").unwrap(),
            "single_quoted",
            "single-quoted values should have quotes stripped"
        );
        assert_eq!(
            vars.get("TEST_ANODIZER_EXPORT").unwrap(),
            "exported_val",
            "export prefix should be stripped"
        );
    }

    #[test]
    fn test_load_env_files_edge_cases() {
        use std::io::Write;
        let dir = tempfile::TempDir::new().unwrap();
        let env_path = dir.path().join(".env-edge");
        let mut f = std::fs::File::create(&env_path).unwrap();
        // Single quote char as value should not panic
        writeln!(f, "TEST_ANODIZER_SINGLEQ=\"").unwrap();
        // Empty key line (=value) should be skipped
        writeln!(f, "=orphan_value").unwrap();
        // Line without = should be skipped with warning
        writeln!(f, "NO_EQUALS_HERE").unwrap();
        drop(f);

        let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Normal);
        let vars = load_env_files(&[env_path.to_string_lossy().to_string()], &log, false).unwrap();
        // The single-quote value should be kept as-is (not stripped, length < 2 for
        // matching quotes)
        assert_eq!(vars.get("TEST_ANODIZER_SINGLEQ").unwrap(), "\"");
        // Empty key and no-equals lines should have been skipped
        assert!(!vars.contains_key(""), "empty key should be skipped");
    }

    #[test]
    fn test_load_env_files_nonexistent_skips_with_warning() {
        let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Normal);
        let result = load_env_files(
            &["/tmp/nonexistent_anodizer_env_file_12345".to_string()],
            &log,
            false,
        );
        // Missing env files should be skipped (not an error), returning empty vars.
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn test_load_env_files_nonexistent_strict_mode_errors() {
        let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Normal);
        let result = load_env_files(
            &["/tmp/nonexistent_anodizer_env_file_12345".to_string()],
            &log,
            true,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("strict mode"));
    }

    // ---- env_files TOML tests ----

    // NOTE: EnvFilesConfig uses a custom Deserialize impl that reads into
    // serde_yaml_ng::Value as an intermediate. Since serde_yaml_ng::Value
    // implements generic Deserialize, this works across formats (YAML, TOML,
    // JSON) -- the intermediate is populated via serde's data model, not
    // from literal YAML text.

    #[test]
    fn test_env_files_list_form_toml() {
        // TOML array should deserialize to EnvFilesConfig::List via the
        // serde_yaml_ng::Value intermediate.
        #[derive(Deserialize)]
        struct Wrapper {
            env_files: EnvFilesConfig,
        }
        let toml_str = r#"env_files = [".env", ".env.local"]"#;
        let wrapper: Wrapper = toml::from_str(toml_str).unwrap();
        let files = wrapper
            .env_files
            .as_list()
            .unwrap_or_else(|| panic!("expected List variant"));
        assert_eq!(files.len(), 2);
        assert_eq!(files[0], ".env");
        assert_eq!(files[1], ".env.local");
    }

    #[test]
    fn test_env_files_struct_form_toml() {
        // TOML table should deserialize to EnvFilesConfig::TokenFiles via
        // the serde_yaml_ng::Value intermediate.
        #[derive(Deserialize)]
        struct Wrapper {
            env_files: EnvFilesConfig,
        }
        let toml_str = r#"
[env_files]
github_token = "~/.config/goreleaser/github_token"
gitlab_token = "/etc/tokens/gitlab"
"#;
        let wrapper: Wrapper = toml::from_str(toml_str).unwrap();
        let tokens = wrapper
            .env_files
            .as_token_files()
            .unwrap_or_else(|| panic!("expected TokenFiles variant"));
        assert_eq!(
            tokens.github_token.as_deref(),
            Some("~/.config/goreleaser/github_token")
        );
        assert_eq!(tokens.gitlab_token.as_deref(), Some("/etc/tokens/gitlab"));
        assert!(tokens.gitea_token.is_none());
    }

    #[test]
    fn test_env_files_token_config_toml_rejects_unknown_fields() {
        // Verify deny_unknown_fields works: a typo like `github_tokne` must fail.
        let toml_str = r#"github_tokne = "~/.config/goreleaser/github_token""#;
        let result = toml::from_str::<EnvFilesTokenConfig>(toml_str);
        assert!(
            result.is_err(),
            "EnvFilesTokenConfig should reject unknown fields like 'github_tokne'"
        );
    }

    // ---- BuildIgnore tests ----

    #[test]
    fn test_build_ignore_parses() {
        // After WAVE 2, defaults.ignore moved to defaults.builds.ignore
        // (path-mirror of BuildConfig).
        let yaml = r#"
project_name: test
defaults:
  targets:
    - x86_64-unknown-linux-gnu
    - aarch64-unknown-linux-gnu
  builds:
    ignore:
      - os: windows
        arch: arm64
      - os: linux
        arch: "386"
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let defaults = config.defaults.unwrap();
        let ignores = defaults.builds.unwrap().ignore.unwrap();
        assert_eq!(ignores.len(), 2);
        assert_eq!(ignores[0].os, "windows");
        assert_eq!(ignores[0].arch, "arm64");
        assert_eq!(ignores[1].os, "linux");
        assert_eq!(ignores[1].arch, "386");
    }

    #[test]
    fn test_build_ignore_omitted() {
        let yaml = r#"
project_name: test
defaults:
  targets:
    - x86_64-unknown-linux-gnu
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let defaults = config.defaults.unwrap();
        assert!(defaults.builds.is_none());
    }

    // ---- BuildOverride tests ----

    #[test]
    fn test_build_override_parses() {
        // After WAVE 2, defaults.overrides moved to defaults.builds.overrides
        // (path-mirror of BuildConfig).
        let yaml = r#"
project_name: test
defaults:
  builds:
    overrides:
      - targets:
          - "x86_64-*"
        features:
          - simd
        flags:
          - "--release"
        env:
          - CC=gcc
      - targets:
          - "*-apple-darwin"
        features:
          - metal
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let defaults = config.defaults.unwrap();
        let overrides = defaults.builds.unwrap().overrides.unwrap();
        assert_eq!(overrides.len(), 2);
        assert_eq!(overrides[0].targets, vec!["x86_64-*"]);
        assert_eq!(overrides[0].features, Some(vec!["simd".to_string()]));
        assert_eq!(overrides[0].flags, Some(vec!["--release".to_string()]));
        assert!(
            overrides[0]
                .env
                .as_ref()
                .unwrap()
                .contains(&"CC=gcc".to_string())
        );
        assert_eq!(overrides[1].targets, vec!["*-apple-darwin"]);
        assert_eq!(overrides[1].features, Some(vec!["metal".to_string()]));
        assert!(overrides[1].env.is_none());
    }

    #[test]
    fn test_build_override_omitted() {
        let yaml = r#"
project_name: test
defaults:
  targets:
    - x86_64-unknown-linux-gnu
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let defaults = config.defaults.unwrap();
        assert!(defaults.builds.is_none());
    }

    // ---- JSON Schema generation test ----

    #[test]
    fn test_json_schema_generation() {
        let schema = schemars::schema_for!(Config);
        let json = serde_json::to_string_pretty(&schema).unwrap();
        assert!(json.contains("project_name"));
        assert!(json.contains("env_files"));
        assert!(json.contains("version"));
        assert!(json.contains("BuildIgnore"));
        assert!(json.contains("BuildOverride"));
    }

    // ---- Homebrew new fields parsing tests ----

    // ---- Scoop new fields parsing tests ----

    // -----------------------------------------------------------------------
    // GitConfig tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_git_config_all_fields() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
git:
  tag_sort: "-version:creatordate"
  ignore_tags:
    - "nightly*"
    - "legacy-*"
  ignore_tag_prefixes:
    - "internal/"
    - "test-"
  prerelease_suffix: "-rc"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let git = config
            .git
            .unwrap_or_else(|| panic!("git section should be present"));
        assert_eq!(git.tag_sort.as_deref(), Some("-version:creatordate"));
        assert_eq!(
            git.ignore_tags.as_deref(),
            Some(&["nightly*".to_string(), "legacy-*".to_string()][..])
        );
        assert_eq!(
            git.ignore_tag_prefixes.as_deref(),
            Some(&["internal/".to_string(), "test-".to_string()][..])
        );
        assert_eq!(git.prerelease_suffix.as_deref(), Some("-rc"));
    }

    #[test]
    fn test_git_config_omitted_is_none() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.git.is_none());
    }

    #[test]
    fn test_git_config_partial_only_tag_sort() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
git:
  tag_sort: "-version:refname"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let git = config
            .git
            .unwrap_or_else(|| panic!("git section should be present"));
        assert_eq!(git.tag_sort.as_deref(), Some("-version:refname"));
        assert!(git.ignore_tags.is_none());
        assert!(git.ignore_tag_prefixes.is_none());
        assert!(git.prerelease_suffix.is_none());
    }

    #[test]
    fn test_git_config_ignore_tags_accepts_array() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
git:
  ignore_tags:
    - "alpha*"
    - "beta*"
    - "rc-*"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let tags = config.git.unwrap().ignore_tags.unwrap();
        assert_eq!(tags.len(), 3);
        assert_eq!(tags[0], "alpha*");
        assert_eq!(tags[1], "beta*");
        assert_eq!(tags[2], "rc-*");
    }

    #[test]
    fn test_validate_tag_sort_valid_refname() {
        let config = Config {
            git: Some(GitConfig {
                tag_sort: Some("-version:refname".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(validate_tag_sort(&config).is_ok());
    }

    #[test]
    fn test_validate_tag_sort_valid_creatordate() {
        let config = Config {
            git: Some(GitConfig {
                tag_sort: Some("-version:creatordate".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(validate_tag_sort(&config).is_ok());
    }

    #[test]
    fn test_validate_tag_sort_none_is_valid() {
        let config = Config {
            git: Some(GitConfig::default()),
            ..Default::default()
        };
        assert!(validate_tag_sort(&config).is_ok());
    }

    #[test]
    fn test_validate_tag_sort_no_git_config_is_valid() {
        let config = Config::default();
        assert!(validate_tag_sort(&config).is_ok());
    }

    #[test]
    fn test_validate_tag_sort_invalid_rejected() {
        let config = Config {
            git: Some(GitConfig {
                tag_sort: Some("alphabetical".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let result = validate_tag_sort(&config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("alphabetical"),
            "error should contain the bad value: {}",
            err
        );
        assert!(
            err.contains("-version:refname"),
            "error should list accepted values: {}",
            err
        );
    }

    // ---- defaults axis-mismatch validation tests (DEC-4) ----

    #[test]
    fn test_validate_defaults_axis_no_defaults_is_ok() {
        let config = Config::default();
        assert!(validate_defaults_axis(&config).is_ok());
    }

    #[test]
    fn test_validate_defaults_axis_crates_block_with_top_level_crates_is_ok() {
        let yaml = r#"
project_name: test
defaults:
  crates: {}
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(validate_defaults_axis(&config).is_ok());
    }

    #[test]
    fn test_validate_defaults_axis_workspaces_block_with_top_level_workspaces_is_ok() {
        let yaml = r#"
project_name: test
defaults:
  workspaces: {}
workspaces:
  - name: ws1
    crates:
      - name: a
        path: "."
        tag_template: "v{{ .Version }}"
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(validate_defaults_axis(&config).is_ok());
    }

    #[test]
    fn test_validate_defaults_axis_crates_block_without_top_level_crates_errors() {
        let yaml = r#"
project_name: test
defaults:
  crates: {}
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let err = validate_defaults_axis(&config).unwrap_err();
        assert!(
            err.starts_with(ERR_DEFAULTS_AXIS_MISMATCH),
            "error should be tagged with the {ERR_DEFAULTS_AXIS_MISMATCH} marker prefix: {err}"
        );
        assert!(
            err.contains("defaults.crates"),
            "error should mention defaults.crates: {err}"
        );
    }

    #[test]
    fn test_validate_defaults_axis_workspaces_block_without_top_level_workspaces_errors() {
        let yaml = r#"
project_name: test
defaults:
  workspaces: {}
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let err = validate_defaults_axis(&config).unwrap_err();
        assert!(
            err.starts_with(ERR_DEFAULTS_AXIS_MISMATCH),
            "error should be tagged with the {ERR_DEFAULTS_AXIS_MISMATCH} marker prefix: {err}"
        );
        assert!(
            err.contains("defaults.workspaces"),
            "error should mention defaults.workspaces: {err}"
        );
    }

    #[test]
    fn test_validate_defaults_axis_both_blocks_set_errors() {
        let yaml = r#"
project_name: test
defaults:
  crates: {}
  workspaces: {}
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let err = validate_defaults_axis(&config).unwrap_err();
        assert!(
            err.starts_with(ERR_DEFAULTS_AXIS_MISMATCH),
            "error should be tagged with the {ERR_DEFAULTS_AXIS_MISMATCH} marker prefix: {err}"
        );
        assert!(
            err.contains("mutually exclusive"),
            "error should mention mutual exclusion: {err}"
        );
    }

    #[test]
    fn test_validate_defaults_axis_wrong_axis_errors() {
        // defaults.crates set but top-level uses workspaces
        let yaml = r#"
project_name: test
defaults:
  crates: {}
workspaces:
  - name: ws1
    crates:
      - name: a
        path: "."
        tag_template: "v{{ .Version }}"
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let err = validate_defaults_axis(&config).unwrap_err();
        assert!(
            err.starts_with(ERR_DEFAULTS_AXIS_MISMATCH),
            "error should be tagged with the {ERR_DEFAULTS_AXIS_MISMATCH} marker prefix: {err}"
        );
        assert!(
            err.contains("workspaces"),
            "error should mention top-level workspaces: {err}"
        );
    }

    // ---------------------------------------------------------------------------
    // validate_homebrew_cask_url_template tests
    // ---------------------------------------------------------------------------

    #[test]
    fn test_validate_homebrew_cask_url_template_both_set_rejected() {
        // Setting url_template AND url.template on the same HomebrewCaskConfig
        // must be a hard validation error.
        let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      homebrew_cask:
        url_template: "https://example.com/{{ .Tag }}/myapp.dmg"
        url:
          template: "https://example.com/{{ .Tag }}/myapp.dmg"
          verified: "example.com"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let err = validate_homebrew_cask_url_template(&config).unwrap_err();
        assert!(
            err.contains("url_template") && err.contains("url.template"),
            "error should mention both conflicting fields: {err}"
        );
        assert!(
            err.contains("mutually exclusive"),
            "error should say they are mutually exclusive: {err}"
        );
    }

    #[test]
    fn test_validate_homebrew_cask_url_template_only_url_template_is_ok() {
        let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      homebrew_cask:
        url_template: "https://example.com/{{ .Tag }}/myapp.dmg"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(validate_homebrew_cask_url_template(&config).is_ok());
    }

    #[test]
    fn test_validate_homebrew_cask_url_template_only_url_is_ok() {
        let yaml = r#"
project_name: test
homebrew_casks:
  - name: myapp
    url:
      template: "https://example.com/{{ .Tag }}/myapp.dmg"
      verified: "example.com"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(validate_homebrew_cask_url_template(&config).is_ok());
    }

    #[test]
    fn test_validate_homebrew_cask_url_template_top_level_both_set_rejected() {
        // Same conflict detected in top-level homebrew_casks array.
        let yaml = r#"
project_name: test
homebrew_casks:
  - name: myapp
    url_template: "https://example.com/{{ .Tag }}/myapp.dmg"
    url:
      template: "https://example.com/{{ .Tag }}/myapp.dmg"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let err = validate_homebrew_cask_url_template(&config).unwrap_err();
        assert!(
            err.contains("homebrew_casks[0]"),
            "error should identify the offending entry: {err}"
        );
    }

    #[test]
    fn test_git_config_ignore_tag_prefixes_accepts_array() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
git:
  ignore_tag_prefixes:
    - "wip/"
    - "experiment/"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let prefixes = config.git.unwrap().ignore_tag_prefixes.unwrap();
        assert_eq!(prefixes.len(), 2);
        assert_eq!(prefixes[0], "wip/");
        assert_eq!(prefixes[1], "experiment/");
    }

    #[test]
    fn test_metadata_config_with_mod_timestamp() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
metadata:
  mod_timestamp: "{{ .CommitTimestamp }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let meta = config.metadata.unwrap();
        assert_eq!(meta.mod_timestamp.unwrap(), "{{ .CommitTimestamp }}");
    }

    #[test]
    fn test_metadata_config_omitted_is_none() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.metadata.is_none());
    }

    #[test]
    fn test_metadata_config_empty_section() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
metadata: {}
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let meta = config.metadata.unwrap();
        assert!(meta.mod_timestamp.is_none());
    }

    #[test]
    fn test_variables_config_parsed() {
        let yaml = r#"
project_name: test
variables:
  description: "my project description"
  somethingElse: "yada yada yada"
  empty: ""
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let vars = config.variables.as_ref().unwrap();
        assert_eq!(vars.get("description").unwrap(), "my project description");
        assert_eq!(vars.get("somethingElse").unwrap(), "yada yada yada");
        assert_eq!(vars.get("empty").unwrap(), "");
        assert_eq!(vars.len(), 3);
    }

    #[test]
    fn test_variables_config_omitted_is_none() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.variables.is_none());
    }

    // ---- SnapcraftConfig disable StringOrBool tests ----

    #[test]
    fn test_snapcraft_disable_bool_true() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    snapcrafts:
      - skip: true
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let snap = &config.crates[0].snapcrafts.as_ref().unwrap()[0];
        assert_eq!(snap.skip, Some(StringOrBool::Bool(true)));
    }

    #[test]
    fn test_snapcraft_disable_bool_false() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    snapcrafts:
      - skip: false
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let snap = &config.crates[0].snapcrafts.as_ref().unwrap()[0];
        assert_eq!(snap.skip, Some(StringOrBool::Bool(false)));
    }

    #[test]
    fn test_snapcraft_disable_template_string() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    snapcrafts:
      - skip: "{{ if .IsSnapshot }}true{{ end }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let snap = &config.crates[0].snapcrafts.as_ref().unwrap()[0];
        match &snap.skip {
            Some(StringOrBool::String(s)) => {
                assert!(s.contains("IsSnapshot"));
            }
            other => panic!("expected StringOrBool::String, got {:?}", other),
        }
    }

    #[test]
    fn test_snapcraft_disable_omitted() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    snapcrafts:
      - name: mysnap
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let snap = &config.crates[0].snapcrafts.as_ref().unwrap()[0];
        assert!(snap.skip.is_none());
    }

    // `docker_v2[].skip_push` is not a recognized field; `deny_unknown_fields`
    // on `DockerV2Config` must reject it at parse time instead of silently
    // dropping. Use the canonical `skip:` (DEC-6) to suppress the publish step.
    #[test]
    fn test_docker_v2_skip_push_rejected() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    docker_v2:
      - dockerfile: Dockerfile
        images: ["ghcr.io/owner/app"]
        tags: ["{{ .Version }}"]
        skip_push: true
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(
            result.is_err(),
            "docker_v2[].skip_push must be rejected (use canonical `skip:`)"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("skip_push") || err.contains("unknown field"),
            "error should mention the rejected field; got: {err}"
        );
    }

    // Snapcraft has no top-level `slots:` concept (only per-app slots via
    // `apps.<name>.slots`); `deny_unknown_fields` on `SnapcraftConfig` must
    // reject the top-level form at parse time instead of silently dropping.
    #[test]
    fn test_snapcraft_top_level_slots_rejected() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    snapcrafts:
      - name: mysnap
        slots:
          dbus-svc:
            interface: dbus
            bus: session
            name: com.example.svc
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(
            result.is_err(),
            "snapcrafts[].slots must be rejected (use apps.<name>.slots)"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("slots") || err.contains("unknown field"),
            "error should mention the rejected field; got: {err}"
        );
    }

    // ---- AurConfig disable StringOrBool tests ----

    #[test]
    fn test_aur_disable_bool_true() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      aur:
        skip: true
        git_url: "ssh://aur@aur.archlinux.org/a.git"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let aur = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .aur
            .as_ref()
            .unwrap();
        assert_eq!(aur.skip, Some(StringOrBool::Bool(true)));
    }

    #[test]
    fn test_aur_disable_template_string() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      aur:
        skip: "{{ if .IsSnapshot }}true{{ end }}"
        git_url: "ssh://aur@aur.archlinux.org/a.git"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let aur = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .aur
            .as_ref()
            .unwrap();
        match &aur.skip {
            Some(StringOrBool::String(s)) => {
                assert!(s.contains("IsSnapshot"));
            }
            other => panic!("expected StringOrBool::String, got {:?}", other),
        }
    }

    #[test]
    fn test_aur_disable_omitted() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      aur:
        git_url: "ssh://aur@aur.archlinux.org/a.git"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let aur = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .aur
            .as_ref()
            .unwrap();
        assert!(aur.skip.is_none());
    }

    // ---- PublisherConfig disable StringOrBool tests ----

    #[test]
    fn test_publisher_disable_bool_true() {
        let yaml = r#"
project_name: test
publishers:
  - cmd: "echo hello"
    skip: true
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let pub_cfg = &config.publishers.as_ref().unwrap()[0];
        assert_eq!(pub_cfg.skip, Some(StringOrBool::Bool(true)));
    }

    #[test]
    fn test_publisher_disable_template_string() {
        let yaml = r#"
project_name: test
publishers:
  - cmd: "echo hello"
    skip: "{{ if .IsSnapshot }}true{{ end }}"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let pub_cfg = &config.publishers.as_ref().unwrap()[0];
        match &pub_cfg.skip {
            Some(StringOrBool::String(s)) => {
                assert!(s.contains("IsSnapshot"));
            }
            other => panic!("expected StringOrBool::String, got {:?}", other),
        }
    }

    #[test]
    fn test_publisher_disable_omitted() {
        let yaml = r#"
project_name: test
publishers:
  - cmd: "echo hello"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let pub_cfg = &config.publishers.as_ref().unwrap()[0];
        assert!(pub_cfg.skip.is_none());
    }

    // ---- skip_upload StringOrBool tests for publisher configs ----

    #[test]
    fn test_aur_skip_upload_bool_true() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      aur:
        skip_upload: true
        git_url: "ssh://aur@aur.archlinux.org/a.git"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let aur = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .aur
            .as_ref()
            .unwrap();
        assert_eq!(aur.skip_upload, Some(StringOrBool::Bool(true)));
    }

    #[test]
    fn test_nix_skip_upload_template() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      nix:
        skip_upload: "{{ .Env.SKIP }}"
        repository:
          owner: org
          name: nixpkgs
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let nix = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .nix
            .as_ref()
            .unwrap();
        match &nix.skip_upload {
            Some(StringOrBool::String(s)) => {
                assert!(s.contains(".Env.SKIP"));
            }
            other => panic!("expected StringOrBool::String, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // TemplateFileConfig tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_template_files_parses_from_yaml() {
        let yaml = r#"
project_name: myproject
crates: []
template_files:
  - id: install-script
    src: install.sh.tpl
    dst: install.sh
    mode: "0755"
  - src: README.md.tpl
    dst: README.md
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let tfs = config.template_files.unwrap();
        assert_eq!(tfs.len(), 2);

        assert_eq!(tfs[0].id.as_deref(), Some("install-script"));
        assert_eq!(tfs[0].src, "install.sh.tpl");
        assert_eq!(tfs[0].dst, "install.sh");
        assert_eq!(tfs[0].mode, Some("0755".to_string()));

        assert_eq!(tfs[1].id, None);
        assert_eq!(tfs[1].src, "README.md.tpl");
        assert_eq!(tfs[1].dst, "README.md");
        assert_eq!(tfs[1].mode, None);
    }

    #[test]
    fn test_template_files_defaults_to_none() {
        let yaml = r#"
project_name: myproject
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.template_files.is_none());
    }

    // -----------------------------------------------------------------------
    // IncludeSpec parsing tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_include_spec_plain_string() {
        let yaml = r#"
project_name: test
includes:
  - ./defaults.yaml
  - extra.yaml
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let includes = config.includes.unwrap();
        assert_eq!(includes.len(), 2);
        assert_eq!(
            includes[0],
            IncludeSpec::Path("./defaults.yaml".to_string())
        );
        assert_eq!(includes[1], IncludeSpec::Path("extra.yaml".to_string()));
    }

    #[test]
    fn test_include_spec_from_file() {
        let yaml = r#"
project_name: test
includes:
  - from_file:
      path: ./config/goreleaser.yaml
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let includes = config.includes.unwrap();
        assert_eq!(includes.len(), 1);
        assert_eq!(
            includes[0],
            IncludeSpec::FromFile {
                from_file: IncludeFilePath {
                    path: "./config/goreleaser.yaml".to_string(),
                },
            }
        );
    }

    #[test]
    fn test_include_spec_from_url_without_headers() {
        let yaml = r#"
project_name: test
includes:
  - from_url:
      url: https://example.com/config.yaml
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let includes = config.includes.unwrap();
        assert_eq!(includes.len(), 1);
        assert_eq!(
            includes[0],
            IncludeSpec::FromUrl {
                from_url: IncludeUrlConfig {
                    url: "https://example.com/config.yaml".to_string(),
                    headers: None,
                },
            }
        );
    }

    #[test]
    fn test_include_spec_from_url_with_headers() {
        let yaml = r#"
project_name: test
includes:
  - from_url:
      url: https://api.mycompany.com/configs/release.yaml
      headers:
        x-api-token: "${MYCOMPANY_TOKEN}"
        Authorization: "Bearer ${GITHUB_TOKEN}"
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let includes = config.includes.unwrap();
        assert_eq!(includes.len(), 1);
        match &includes[0] {
            IncludeSpec::FromUrl { from_url } => {
                assert_eq!(
                    from_url.url,
                    "https://api.mycompany.com/configs/release.yaml"
                );
                let headers = from_url.headers.as_ref().unwrap();
                assert_eq!(headers.len(), 2);
                assert_eq!(headers["x-api-token"], "${MYCOMPANY_TOKEN}");
                assert_eq!(headers["Authorization"], "Bearer ${GITHUB_TOKEN}");
            }
            other => panic!("expected FromUrl, got: {:?}", other),
        }
    }

    #[test]
    fn test_include_spec_mixed_forms() {
        let yaml = r#"
project_name: test
includes:
  - ./defaults.yaml
  - from_file:
      path: ./config/shared.yaml
  - from_url:
      url: https://example.com/config.yaml
      headers:
        x-token: secret
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let includes = config.includes.unwrap();
        assert_eq!(includes.len(), 3);
        assert!(matches!(&includes[0], IncludeSpec::Path(s) if s == "./defaults.yaml"));
        assert!(
            matches!(&includes[1], IncludeSpec::FromFile { from_file } if from_file.path == "./config/shared.yaml")
        );
        assert!(
            matches!(&includes[2], IncludeSpec::FromUrl { from_url } if from_url.url == "https://example.com/config.yaml")
        );
    }

    #[test]
    fn test_include_spec_no_includes_field() {
        let yaml = r#"
project_name: test
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.includes.is_none());
    }

    #[test]
    fn test_include_spec_empty_includes() {
        let yaml = r#"
project_name: test
includes: []
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.includes, Some(vec![]));
    }

    #[test]
    fn test_include_spec_github_shorthand_url() {
        // The GitHub shorthand (no https:// prefix) should parse fine as a URL
        // string — normalization happens at resolve time, not parse time.
        let yaml = r#"
project_name: test
includes:
  - from_url:
      url: caarlos0/goreleaserfiles/main/packages.yml
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let includes = config.includes.unwrap();
        assert_eq!(includes.len(), 1);
        match &includes[0] {
            IncludeSpec::FromUrl { from_url } => {
                assert_eq!(from_url.url, "caarlos0/goreleaserfiles/main/packages.yml");
            }
            other => panic!("expected FromUrl, got: {:?}", other),
        }
    }

    // ---- Platform URL config tests ----

    #[test]
    fn test_github_urls_config_all_fields() {
        let yaml = r#"
api: https://github.example.com/api/v3/
upload: https://github.example.com/api/uploads/
download: https://github.example.com/
skip_tls_verify: true
"#;
        let cfg: GitHubUrlsConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(
            cfg.api.as_deref(),
            Some("https://github.example.com/api/v3/")
        );
        assert_eq!(
            cfg.upload.as_deref(),
            Some("https://github.example.com/api/uploads/")
        );
        assert_eq!(cfg.download.as_deref(), Some("https://github.example.com/"));
        assert_eq!(cfg.skip_tls_verify, Some(true));
    }

    #[test]
    fn test_github_urls_config_defaults() {
        let yaml = "{}";
        let cfg: GitHubUrlsConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.api, None);
        assert_eq!(cfg.upload, None);
        assert_eq!(cfg.download, None);
        assert_eq!(cfg.skip_tls_verify, None);
    }

    #[test]
    fn test_gitlab_urls_config_all_fields() {
        let yaml = r#"
api: https://gitlab.example.com/api/v4/
download: https://gitlab.example.com/
skip_tls_verify: false
use_package_registry: true
use_job_token: true
"#;
        let cfg: GitLabUrlsConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(
            cfg.api.as_deref(),
            Some("https://gitlab.example.com/api/v4/")
        );
        assert_eq!(cfg.download.as_deref(), Some("https://gitlab.example.com/"));
        assert_eq!(cfg.skip_tls_verify, Some(false));
        assert_eq!(cfg.use_package_registry, Some(true));
        assert_eq!(cfg.use_job_token, Some(true));
    }

    #[test]
    fn test_gitlab_urls_config_defaults() {
        let yaml = "{}";
        let cfg: GitLabUrlsConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.api, None);
        assert_eq!(cfg.download, None);
        assert_eq!(cfg.skip_tls_verify, None);
        assert_eq!(cfg.use_package_registry, None);
        assert_eq!(cfg.use_job_token, None);
    }

    #[test]
    fn test_gitea_urls_config_all_fields() {
        let yaml = r#"
api: https://gitea.example.com/api/v1/
download: https://gitea.example.com/
skip_tls_verify: true
"#;
        let cfg: GiteaUrlsConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(
            cfg.api.as_deref(),
            Some("https://gitea.example.com/api/v1/")
        );
        assert_eq!(cfg.download.as_deref(), Some("https://gitea.example.com/"));
        assert_eq!(cfg.skip_tls_verify, Some(true));
    }

    #[test]
    fn test_gitea_urls_config_defaults() {
        let yaml = "{}";
        let cfg: GiteaUrlsConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.api, None);
        assert_eq!(cfg.download, None);
        assert_eq!(cfg.skip_tls_verify, None);
    }

    #[test]
    fn test_release_config_gitlab_gitea_fields() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      github:
        owner: gh-owner
        name: gh-repo
      gitlab:
        owner: gitlab-owner
        name: gitlab-repo
      gitea:
        owner: gitea-owner
        name: gitea-repo
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let release = config.crates[0].release.as_ref().unwrap();
        let github = release.github.as_ref().unwrap();
        assert_eq!(github.owner, "gh-owner");
        assert_eq!(github.name, "gh-repo");
        let gitlab = release.gitlab.as_ref().unwrap();
        assert_eq!(gitlab.owner, "gitlab-owner");
        assert_eq!(gitlab.name, "gitlab-repo");
        let gitea = release.gitea.as_ref().unwrap();
        assert_eq!(gitea.owner, "gitea-owner");
        assert_eq!(gitea.name, "gitea-repo");
    }

    #[test]
    fn test_config_github_urls_field() {
        let yaml = r#"
project_name: test
github_urls:
  api: https://ghe.corp.com/api/v3/
  upload: https://ghe.corp.com/api/uploads/
  download: https://ghe.corp.com/
  skip_tls_verify: true
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let urls = config.github_urls.as_ref().unwrap();
        assert_eq!(urls.api.as_deref(), Some("https://ghe.corp.com/api/v3/"));
        assert_eq!(
            urls.upload.as_deref(),
            Some("https://ghe.corp.com/api/uploads/")
        );
        assert_eq!(urls.download.as_deref(), Some("https://ghe.corp.com/"));
        assert_eq!(urls.skip_tls_verify, Some(true));
    }

    #[test]
    fn test_config_gitlab_urls_field() {
        let yaml = r#"
project_name: test
gitlab_urls:
  api: https://gitlab.corp.com/api/v4/
  download: https://gitlab.corp.com/
  skip_tls_verify: false
  use_package_registry: true
  use_job_token: false
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let urls = config.gitlab_urls.as_ref().unwrap();
        assert_eq!(urls.api.as_deref(), Some("https://gitlab.corp.com/api/v4/"));
        assert_eq!(urls.download.as_deref(), Some("https://gitlab.corp.com/"));
        assert_eq!(urls.skip_tls_verify, Some(false));
        assert_eq!(urls.use_package_registry, Some(true));
        assert_eq!(urls.use_job_token, Some(false));
    }

    #[test]
    fn test_config_gitea_urls_field() {
        let yaml = r#"
project_name: test
gitea_urls:
  api: https://gitea.corp.com/api/v1/
  download: https://gitea.corp.com/
  skip_tls_verify: true
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let urls = config.gitea_urls.as_ref().unwrap();
        assert_eq!(urls.api.as_deref(), Some("https://gitea.corp.com/api/v1/"));
        assert_eq!(urls.download.as_deref(), Some("https://gitea.corp.com/"));
        assert_eq!(urls.skip_tls_verify, Some(true));
    }

    #[test]
    fn test_config_force_token_field() {
        let yaml = r#"
project_name: test
force_token: gitlab
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.force_token, Some(ForceTokenKind::GitLab));
    }

    #[test]
    fn test_config_force_token_omitted() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.force_token, None::<ForceTokenKind>);
    }

    #[test]
    fn test_config_all_platform_urls_and_force_token() {
        let yaml = r#"
project_name: test
github_urls:
  api: https://ghe.corp.com/api/v3/
gitlab_urls:
  api: https://gitlab.corp.com/api/v4/
  use_job_token: true
gitea_urls:
  api: https://gitea.corp.com/api/v1/
force_token: github
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(
            config.github_urls.as_ref().unwrap().api.as_deref(),
            Some("https://ghe.corp.com/api/v3/")
        );
        assert_eq!(
            config.gitlab_urls.as_ref().unwrap().api.as_deref(),
            Some("https://gitlab.corp.com/api/v4/")
        );
        assert_eq!(
            config.gitlab_urls.as_ref().unwrap().use_job_token,
            Some(true)
        );
        assert_eq!(
            config.gitea_urls.as_ref().unwrap().api.as_deref(),
            Some("https://gitea.corp.com/api/v1/")
        );
        assert_eq!(config.force_token, Some(ForceTokenKind::GitHub));
    }

    #[test]
    fn test_dockerhub_config_parse() {
        let yaml = r#"
project_name: test
dockerhub:
  - username: myuser
    secret_name: DOCKER_TOKEN
    images:
      - myorg/myapp
    description: "My app"
    skip: true
    full_description:
      from_file:
        path: ./README.md
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let dh = &cfg.dockerhub.unwrap()[0];
        assert_eq!(dh.username.as_deref(), Some("myuser"));
        assert_eq!(dh.secret_name.as_deref(), Some("DOCKER_TOKEN"));
        assert_eq!(dh.images.as_ref().unwrap(), &["myorg/myapp"]);
        assert_eq!(dh.description.as_deref(), Some("My app"));
        assert_eq!(dh.skip, Some(StringOrBool::Bool(true)));
        let fd = dh.full_description.as_ref().unwrap();
        assert!(fd.from_url.is_none());
        let ff = fd.from_file.as_ref().unwrap();
        assert_eq!(ff.path, "./README.md");
    }

    #[test]
    fn test_dockerhub_from_url_parse() {
        let yaml = r#"
project_name: test
dockerhub:
  - username: myuser
    full_description:
      from_url:
        url: "https://raw.githubusercontent.com/org/repo/main/README.md"
        headers:
          Authorization: "Bearer {{ .Env.GH_TOKEN }}"
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let dh = &cfg.dockerhub.unwrap()[0];
        let fu = dh
            .full_description
            .as_ref()
            .unwrap()
            .from_url
            .as_ref()
            .unwrap();
        assert_eq!(
            fu.url,
            "https://raw.githubusercontent.com/org/repo/main/README.md"
        );
        let headers = fu.headers.as_ref().unwrap();
        assert_eq!(
            headers.get("Authorization").unwrap(),
            "Bearer {{ .Env.GH_TOKEN }}"
        );
    }

    #[test]
    fn test_artifactory_config_parse() {
        let yaml = r#"
project_name: test
artifactories:
  - name: production
    target: "https://artifactory.example.com/repo/{{ .ProjectName }}/{{ .Version }}/"
    username: deployer
    mode: archive
    skip: "{{ .Env.SKIP }}"
    ids:
      - default
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let art = &cfg.artifactories.unwrap()[0];
        assert_eq!(art.name.as_deref(), Some("production"));
        assert_eq!(
            art.target.as_deref(),
            Some("https://artifactory.example.com/repo/{{ .ProjectName }}/{{ .Version }}/")
        );
        assert_eq!(art.username.as_deref(), Some("deployer"));
        assert_eq!(art.mode.as_deref(), Some("archive"));
        assert_eq!(
            art.skip,
            Some(StringOrBool::String("{{ .Env.SKIP }}".to_string()))
        );
        assert_eq!(art.ids.as_ref().unwrap(), &["default"]);
    }

    #[test]
    fn test_cloudsmith_config_parse() {
        let yaml = r#"
project_name: test
cloudsmiths:
  - organization: myorg
    repository: myrepo
    formats:
      - deb
    distributions:
      deb: "ubuntu/focal"
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let cs = &cfg.cloudsmiths.unwrap()[0];
        assert_eq!(cs.organization.as_deref(), Some("myorg"));
        assert_eq!(cs.repository.as_deref(), Some("myrepo"));
        assert_eq!(cs.formats.as_ref().unwrap(), &["deb"]);
        let dists = cs.distributions.as_ref().unwrap();
        assert_eq!(dists.get("deb").unwrap(), "ubuntu/focal");
    }

    // -----------------------------------------------------------------------
    // env: Vec<String> tests — list-only, null/missing, parse helpers
    // -----------------------------------------------------------------------

    #[test]
    fn test_docker_sign_env_list_format() {
        let yaml = r#"
project_name: test
docker_signs:
  - cmd: cosign
    env:
      - COSIGN_PASSWORD=hunter2
      - COSIGN_KEY=/path/to/key
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let ds = &cfg.docker_signs.as_ref().unwrap()[0];
        let env = ds.env.as_ref().expect("env should be Some");
        assert_eq!(
            env,
            &vec!["COSIGN_PASSWORD=hunter2", "COSIGN_KEY=/path/to/key"]
        );
    }

    #[test]
    fn test_docker_sign_env_map_form_rejected() {
        let yaml = r#"
project_name: test
docker_signs:
  - cmd: cosign
    env:
      COSIGN_PASSWORD: hunter2
"#;
        let result = serde_yaml_ng::from_str::<Config>(yaml);
        assert!(
            result.is_err(),
            "map form should be rejected after Vec<String> migration"
        );
    }

    #[test]
    fn test_docker_sign_env_null() {
        let yaml = r#"
project_name: test
docker_signs:
  - cmd: cosign
    env: ~
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let ds = &cfg.docker_signs.as_ref().unwrap()[0];
        assert!(ds.env.is_none());
    }

    #[test]
    fn test_docker_sign_env_missing() {
        let yaml = r#"
project_name: test
docker_signs:
  - cmd: cosign
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let ds = &cfg.docker_signs.as_ref().unwrap()[0];
        assert!(ds.env.is_none());
    }

    #[test]
    fn test_sign_config_env_list_format() {
        let yaml = r#"
project_name: test
signs:
  - cmd: gpg
    env:
      - GPG_KEY=ABCDEF
      - GPG_TTY=/dev/pts/0
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let s = &cfg.signs[0];
        let env = s.env.as_ref().expect("env should be Some");
        assert_eq!(env, &vec!["GPG_KEY=ABCDEF", "GPG_TTY=/dev/pts/0"]);
    }

    #[test]
    fn test_publisher_env_list_format() {
        let yaml = r#"
project_name: test
publishers:
  - name: mypub
    cmd: publish.sh
    env:
      - API_TOKEN=secret123
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let p = &cfg.publishers.as_ref().unwrap()[0];
        let env = p.env.as_ref().expect("env should be Some");
        assert_eq!(env, &vec!["API_TOKEN=secret123"]);
    }

    #[test]
    fn test_build_override_env_list_format() {
        let yaml = r#"
project_name: test
defaults:
  targets:
    - x86_64-unknown-linux-gnu
  builds:
    overrides:
      - targets:
          - "x86_64-*"
        env:
          - CC=gcc-12
          - CFLAGS=-O2 -Wall
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let overrides = config.defaults.unwrap().builds.unwrap().overrides.unwrap();
        let env = overrides[0].env.as_ref().expect("env should be Some");
        assert_eq!(env, &vec!["CC=gcc-12", "CFLAGS=-O2 -Wall"]);
    }

    #[test]
    fn test_structured_hook_env_list_format() {
        let yaml = r#"
project_name: test
before:
  hooks:
    - cmd: echo hello
      env:
        - MY_VAR=foo
        - OTHER=bar=baz
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let hooks = cfg.before.as_ref().unwrap().hooks.as_ref().unwrap();
        match &hooks[0] {
            HookEntry::Structured(h) => {
                let env = h.env.as_ref().expect("env should be Some");
                assert_eq!(env, &vec!["MY_VAR=foo", "OTHER=bar=baz"]);
            }
            HookEntry::Simple(_) => panic!("expected Structured hook"),
        }
    }

    #[test]
    fn test_sbom_config_env_list_format() {
        let yaml = r#"
project_name: test
sboms:
  - cmd: syft
    env:
      - SYFT_FILE_METADATA_CATALOGER_ENABLED=true
      - SYFT_SCOPE=all-layers
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let s = &cfg.sboms[0];
        let env = s.env.as_ref().expect("env should be Some");
        assert_eq!(
            env,
            &vec![
                "SYFT_FILE_METADATA_CATALOGER_ENABLED=true",
                "SYFT_SCOPE=all-layers"
            ]
        );
    }

    #[test]
    fn test_sbom_config_env_missing() {
        let yaml = r#"
project_name: test
sboms:
  - cmd: syft
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let s = &cfg.sboms[0];
        assert!(s.env.is_none());
    }

    // ---- env map-form rejection tests (must be Vec<String>, not map) ----

    /// Assert that the given YAML fails to deserialize into `Config` because an
    /// `env:` field was supplied as a map (`KEY: value`) rather than the
    /// required `Vec<String>` (`- KEY=value`).
    #[track_caller]
    fn assert_env_map_rejected(yaml: &str, label: &str) {
        let result = serde_yaml_ng::from_str::<Config>(yaml);
        assert!(
            result.is_err(),
            "{label}.env map form should be rejected after Vec<String> migration"
        );
    }

    #[test]
    fn test_top_level_env_map_form_rejected() {
        let yaml = r#"
project_name: test
crates: []
env:
  MY_VAR: hello
"#;
        assert_env_map_rejected(yaml, "top-level Config");
    }

    #[test]
    fn test_build_override_env_map_form_rejected() {
        // After WAVE 2, defaults.overrides moved under defaults.builds.overrides.
        let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    builds:
      - binary: app
defaults:
  builds:
    overrides:
      - targets: ["x86_64-unknown-linux-gnu"]
        env:
          MY_VAR: hello
"#;
        assert_env_map_rejected(yaml, "BuildOverride");
    }

    #[test]
    fn test_sign_config_env_map_form_rejected() {
        let yaml = r#"
project_name: test
crates: []
signs:
  - cmd: cosign
    env:
      COSIGN_PASSWORD: hunter2
"#;
        assert_env_map_rejected(yaml, "SignConfig");
    }

    #[test]
    fn test_sbom_config_env_map_form_rejected() {
        let yaml = r#"
project_name: test
sboms:
  - cmd: syft
    env:
      MY_VAR: value
"#;
        assert_env_map_rejected(yaml, "SbomConfig");
    }

    #[test]
    fn test_workspace_env_map_form_rejected() {
        let yaml = r#"
project_name: test
workspaces:
  - name: myws
    crates: []
    env:
      MY_VAR: value
"#;
        assert_env_map_rejected(yaml, "WorkspaceConfig");
    }

    #[test]
    fn test_publisher_config_env_map_form_rejected() {
        let yaml = r#"
project_name: test
crates: []
publishers:
  - cmd: "my-publisher"
    env:
      MY_VAR: value
"#;
        assert_env_map_rejected(yaml, "PublisherConfig");
    }

    #[test]
    fn test_structured_hook_env_map_form_rejected() {
        let yaml = r#"
project_name: test
crates: []
before:
  hooks:
    - cmd: "echo hello"
      env:
        MY_VAR: value
"#;
        assert_env_map_rejected(yaml, "StructuredHook");
    }

    // ---- defaults.archives.format_overrides validation -------------------

    #[test]
    fn test_validate_format_overrides_in_defaults_block_rejects_unknown_os() {
        // defaults.archives.format_overrides[].os = "pc-windows-msvc" is a
        // common Rust-triple typo for "windows" and used to slip past the
        // validator because the defaults block was not walked.
        let yaml = r#"
project_name: test
defaults:
  archives:
    format_overrides:
      - os: pc-windows-msvc
        formats: [zip]
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let err = validate_format_overrides(&config).unwrap_err();
        assert!(
            err.contains("defaults.archives"),
            "error should locate the offender at defaults.archives: {err}"
        );
        assert!(
            err.contains("pc-windows-msvc"),
            "error should echo the bad os value: {err}"
        );
    }

    #[test]
    fn test_validate_format_overrides_in_defaults_block_accepts_known_os() {
        let yaml = r#"
project_name: test
defaults:
  archives:
    format_overrides:
      - os: windows
        formats: [zip]
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        validate_format_overrides(&config).expect("known os value should pass");
    }

    // ---- DefaultsCrateBlock / DefaultsWorkspaceBlock unknown-field rejection

    #[test]
    fn test_defaults_crates_block_rejects_unknown_field() {
        let yaml = r#"
project_name: test
defaults:
  crates:
    foo: bar
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        let err = result.expect_err("unknown field under defaults.crates should be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("unknown field"),
            "error should mention 'unknown field': {msg}"
        );
    }

    #[test]
    fn test_defaults_workspaces_block_rejects_unknown_field() {
        let yaml = r#"
project_name: test
defaults:
  workspaces:
    foo: bar
workspaces:
  - name: ws1
    crates:
      - name: a
        path: "."
        tag_template: "v{{ .Version }}"
crates: []
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        let err = result.expect_err("unknown field under defaults.workspaces should be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("unknown field"),
            "error should mention 'unknown field': {msg}"
        );
    }
}
