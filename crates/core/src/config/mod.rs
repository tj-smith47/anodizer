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

impl ChecksumConfig {
    /// Default checksum filename template (combined mode). Mirrors
    /// `internal/pipe/checksums/checksums.go:48` in GoReleaser.
    pub const DEFAULT_NAME_TEMPLATE: &'static str = "{{ ProjectName }}_{{ Version }}_checksums.txt";

    /// Default hash algorithm. Mirrors GoReleaser
    /// (`internal/pipe/checksums/checksums.go:42`).
    pub const DEFAULT_ALGORITHM: &'static str = "sha256";

    /// Resolve the hash algorithm, falling back to the project default
    /// when the user did not specify one. Stages MUST call this rather
    /// than reading `self.algorithm` directly, so a future default change
    /// (or user-facing override resolution) lands in one place. See the
    /// lazy-vs-eager defaults policy in `.claude/audits/2026-04-config-gaps/`.
    pub fn resolved_algorithm(&self) -> &str {
        self.algorithm.as_deref().unwrap_or(Self::DEFAULT_ALGORITHM)
    }

    /// Whether split-mode (one sidecar per artifact) is requested.
    /// Defaults to `false` (combined-file mode, matching GoReleaser).
    pub fn resolved_split(&self) -> bool {
        self.split.unwrap_or(false)
    }

    /// Resolve the combined-mode checksum filename template, falling back
    /// to the GoReleaser-canonical default. Returns the raw template
    /// string; the caller still renders it through Tera.
    ///
    /// Split mode constructs sidecar names per-artifact at the call site
    /// (`<artifact>.<algo>` literal format) and intentionally does NOT
    /// route through this accessor — that path needs no template rendering.
    pub fn resolved_combined_name_template(&self) -> &str {
        self.name_template
            .as_deref()
            .unwrap_or(Self::DEFAULT_NAME_TEMPLATE)
    }
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

impl ReleaseConfig {
    /// Default release-name template. Mirrors GoReleaser
    /// `internal/pipe/release/release.go` (`cfg.NameTemplate = "{{.Tag}}"`).
    /// Anodize uses Tera-style `{{ Tag }}` (no dot prefix); the rendered
    /// value is identical for any tag the project produces.
    pub const DEFAULT_NAME_TEMPLATE: &'static str = "{{ Tag }}";

    /// Default release `mode`. Mirrors GoReleaser default
    /// (`internal/pipe/release/release.go`: empty string is treated as
    /// "keep-existing" — keep current release notes, don't overwrite).
    pub const DEFAULT_MODE: &'static str = "keep-existing";

    /// Valid `mode:` values. Anything else is a config error.
    pub const VALID_MODES: &[&'static str] = &["keep-existing", "append", "prepend", "replace"];

    /// Resolve the `name_template`, falling back to
    /// [`Self::DEFAULT_NAME_TEMPLATE`].
    pub fn resolved_name_template(&self) -> &str {
        self.name_template
            .as_deref()
            .unwrap_or(Self::DEFAULT_NAME_TEMPLATE)
    }

    /// Resolve the release `mode`, validating and falling back to
    /// [`Self::DEFAULT_MODE`] when unset or empty. Returns an error when
    /// the user supplied a value outside [`Self::VALID_MODES`] so the
    /// invalid mode surfaces at the call site instead of producing a
    /// silent no-op publish.
    pub fn resolved_mode(&self) -> anyhow::Result<&str> {
        match self.mode.as_deref() {
            None | Some("") => Ok(Self::DEFAULT_MODE),
            Some(m) if Self::VALID_MODES.contains(&m) => Ok(m),
            Some(other) => Err(anyhow::anyhow!(
                "release: invalid mode '{}', must be one of: {}",
                other,
                Self::VALID_MODES.join(", ")
            )),
        }
    }

    /// Resolve `draft`, falling back to `false`.
    pub fn resolved_draft(&self) -> bool {
        self.draft.unwrap_or(false)
    }

    /// Resolve `replace_existing_draft`, falling back to `false`.
    pub fn resolved_replace_existing_draft(&self) -> bool {
        self.replace_existing_draft.unwrap_or(false)
    }

    /// Resolve `replace_existing_artifacts`, falling back to `false`.
    pub fn resolved_replace_existing_artifacts(&self) -> bool {
        self.replace_existing_artifacts.unwrap_or(false)
    }

    /// Resolve `include_meta`, falling back to `false` (don't upload
    /// metadata.json / artifacts.json as release assets by default).
    pub fn resolved_include_meta(&self) -> bool {
        self.include_meta.unwrap_or(false)
    }

    /// Resolve `use_existing_draft`, falling back to `false` (always
    /// create a fresh draft when one isn't found by default).
    pub fn resolved_use_existing_draft(&self) -> bool {
        self.use_existing_draft.unwrap_or(false)
    }
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

mod nfpm;
pub use nfpm::*;
// ---------------------------------------------------------------------------
// SnapcraftConfig
// ---------------------------------------------------------------------------

mod snapcraft;
pub use snapcraft::*;
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

mod notarize;
pub use notarize::*;
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

impl SbomConfig {
    /// Default `id` when an SBOM config has none. Mirrors GoReleaser
    /// `internal/pipe/sbom/sbom.go` (`cfg.ID = "default"`).
    pub const DEFAULT_ID: &'static str = "default";

    /// Default SBOM-generation command. Mirrors GoReleaser `sbom.go`
    /// (`cfg.Cmd = "syft"`).
    pub const DEFAULT_CMD: &'static str = "syft";

    /// Default `artifacts` filter. Mirrors GoReleaser `sbom.go`
    /// (`cfg.Artifacts = "archive"`).
    pub const DEFAULT_ARTIFACTS: &'static str = "archive";

    /// Default document-path template when `artifacts: binary`. Includes
    /// per-target Os/Arch suffix so per-arch SBOMs don't collide.
    /// Mirrors GoReleaser `sbom.go`.
    pub const DEFAULT_DOCUMENT_BINARY: &'static str =
        "{{ .Binary }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}.sbom.json";

    /// Default document-path template for any non-binary, non-any
    /// `artifacts:` filter. Mirrors GoReleaser `sbom.go`.
    pub const DEFAULT_DOCUMENT_OTHER: &'static str = "{{ .ArtifactName }}.sbom.json";

    /// Default `args` for the syft command. Mirrors GoReleaser
    /// `sbom.go`. Anodize matches GR's shell-style `$artifact` /
    /// `$document` placeholders verbatim — the arg-renderer rewrites
    /// these to per-artifact values at execution time.
    pub const DEFAULT_SYFT_ARGS: &[&'static str] = &[
        "$artifact",
        "--output",
        "spdx-json=$document",
        "--enrich",
        "all",
    ];

    /// Env entry that syft requires to emit file paths in the SBOM
    /// when cataloging archives or source. Mirrors GoReleaser `sbom.go`.
    pub const DEFAULT_SYFT_ENV_KEY: &'static str = "SYFT_FILE_METADATA_CATALOGER_ENABLED";
    pub const DEFAULT_SYFT_ENV_VAL: &'static str = "true";

    /// Resolve the SBOM-config id, falling back to `"default"`.
    pub fn resolved_id(&self) -> &str {
        self.id.as_deref().unwrap_or(Self::DEFAULT_ID)
    }

    /// Resolve the SBOM command, falling back to `"syft"`.
    pub fn resolved_cmd(&self) -> &str {
        self.cmd.as_deref().unwrap_or(Self::DEFAULT_CMD)
    }

    /// Resolve the `artifacts:` filter, falling back to `"archive"`.
    pub fn resolved_artifacts(&self) -> &str {
        self.artifacts.as_deref().unwrap_or(Self::DEFAULT_ARTIFACTS)
    }

    /// Resolve `documents`, falling back to the artifact-type-specific
    /// default when unset. Caller should pass the result of
    /// [`Self::resolved_artifacts`] for `artifacts`.
    pub fn resolved_documents(&self, artifacts: &str) -> Vec<String> {
        self.documents.clone().unwrap_or_else(|| match artifacts {
            "binary" => vec![Self::DEFAULT_DOCUMENT_BINARY.to_string()],
            "any" => vec![],
            _ => vec![Self::DEFAULT_DOCUMENT_OTHER.to_string()],
        })
    }

    /// Resolve `args`, falling back to [`Self::DEFAULT_SYFT_ARGS`] when
    /// `cmd` is `"syft"`; empty vec otherwise (matches GoReleaser:
    /// `sbom.go` only initializes args when cmd is syft, and leaves
    /// args empty for other cmds).
    pub fn resolved_args(&self, cmd: &str) -> Vec<String> {
        self.args.clone().unwrap_or_else(|| {
            if cmd == Self::DEFAULT_CMD {
                Self::DEFAULT_SYFT_ARGS
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect()
            } else {
                Vec::new()
            }
        })
    }

    /// Default env additions for the syft sub-process. Empty unless cmd
    /// is syft AND artifacts is source/archive — in which case syft
    /// needs the file-metadata cataloger enabled to produce file paths
    /// in the SBOM. Mirrors GoReleaser `sbom.go`.
    pub fn default_syft_env_for(cmd: &str, artifacts: &str) -> Vec<(String, String)> {
        if cmd == Self::DEFAULT_CMD && matches!(artifacts, "source" | "archive") {
            vec![(
                Self::DEFAULT_SYFT_ENV_KEY.to_string(),
                Self::DEFAULT_SYFT_ENV_VAL.to_string(),
            )]
        } else {
            Vec::new()
        }
    }
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
