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

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
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
    #[serde(default, alias = "sign", deserialize_with = "deserialize_signs")]
    #[schemars(schema_with = "signs_schema")]
    pub signs: Vec<SignConfig>,
    /// Binary-specific signing configs (same shape as `signs` but only for binary artifacts).
    #[serde(default, alias = "binary_sign", deserialize_with = "deserialize_signs")]
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
    /// Accepts two YAML forms:
    /// - **Map form**: `env: { MY_VAR: hello, DEPLOY_ENV: staging }`
    /// - **List form** (GoReleaser parity): `env: ["MY_VAR=hello", "DEPLOY_ENV=staging"]`
    ///
    /// Values are rendered through the template engine before being set, so
    /// expressions like `{{ .Tag }}` or `{{ .Date }}` are expanded.
    #[serde(default, deserialize_with = "deserialize_env_map")]
    pub env: Option<HashMap<String, String>>,
    /// Custom template variables accessible as {{ .Var.key }} in templates.
    /// Provides a way to define reusable values, especially useful with config includes.
    pub variables: Option<HashMap<String, String>>,
    /// Generic artifact publisher configurations.
    pub publishers: Option<Vec<PublisherConfig>>,
    /// DockerHub description sync configurations.
    pub dockerhub: Option<Vec<DockerHubConfig>>,
    /// Artifactory upload configurations.
    pub artifactories: Option<Vec<ArtifactoryConfig>>,
    /// GemFury publisher configurations.
    #[serde(alias = "gemfury")]
    pub fury: Option<Vec<FuryConfig>>,
    /// CloudSmith publisher configurations.
    pub cloudsmiths: Option<Vec<CloudSmithConfig>>,
    /// NPM publisher configurations.
    pub npms: Option<Vec<NpmConfig>>,
    /// Top-level Homebrew Cask configurations.
    /// GoReleaser parity: `homebrew_casks` is a top-level array with its own
    /// repository, commit_author, directory, skip_upload, hooks, dependencies,
    /// conflicts, completions, manpages, structured uninstall/zap, etc.
    pub homebrew_casks: Option<Vec<TopLevelHomebrewCaskConfig>>,
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
    #[serde(default, alias = "sbom", deserialize_with = "deserialize_sboms")]
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
        obj.metadata().description = Some(
            "SBOM generation configurations. Accepts a single object or array.".to_owned(),
        );
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
            fury: None,
            cloudsmiths: None,
            npms: None,
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
    if let Some(ref git) = config.git {
        if let Some(ref sort) = git.tag_sort {
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
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// EnvFilesConfig — accepts list of .env paths OR structured token file paths
// ---------------------------------------------------------------------------

/// Environment file configuration.
///
/// Accepts two forms:
/// - **List form** (anodize extension): array of `.env` file paths loaded as KEY=VALUE.
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
                let list: Vec<String> = serde_yaml_ng::from_value(value)
                    .map_err(serde::de::Error::custom)?;
                Ok(EnvFilesConfig::List(list))
            }
            serde_yaml_ng::Value::Mapping(_) => {
                let tokens: EnvFilesTokenConfig = serde_yaml_ng::from_value(value)
                    .map_err(serde::de::Error::custom)?;
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

    let mappings = [
        (
            "GITHUB_TOKEN",
            config
                .github_token
                .as_deref()
                .unwrap_or("~/.config/goreleaser/github_token"),
        ),
        (
            "GITLAB_TOKEN",
            config
                .gitlab_token
                .as_deref()
                .unwrap_or("~/.config/goreleaser/gitlab_token"),
        ),
        (
            "GITEA_TOKEN",
            config
                .gitea_token
                .as_deref()
                .unwrap_or("~/.config/goreleaser/gitea_token"),
        ),
    ];

    for (env_name, file_path) in &mappings {
        // Skip if the env var is already set in the process environment
        if std::env::var(env_name).ok().filter(|v| !v.is_empty()).is_some() {
            log.verbose(&format!("using {} from process environment", env_name));
            continue;
        }
        match read_token_file(file_path) {
            Ok(Some(token)) => {
                log.verbose(&format!("loaded {} from {}", env_name, file_path));
                vars.insert(env_name.to_string(), token);
            }
            Ok(None) => {
                // File doesn't exist or is empty — not an error, just skip
            }
            Err(e) => {
                return Err(e);
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
) -> Result<std::collections::HashMap<String, String>, String> {
    let mut vars = std::collections::HashMap::new();
    for file_path in files {
        let content = std::fs::read_to_string(file_path)
            .map_err(|e| format!("failed to read env file '{}': {}", file_path, e))?;
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
// deserialize_env_map — accepts YAML mapping OR list-of-KEY=VALUE strings
// ---------------------------------------------------------------------------

/// Custom deserializer for `env` fields that accepts both forms:
///
/// - **Map form** (YAML mapping):
///   ```yaml
///   env:
///     MY_VAR: hello
///     DEPLOY_ENV: staging
///   ```
///
/// - **List form** (GoReleaser parity — list of `KEY=VALUE` strings):
///   ```yaml
///   env:
///     - MY_VAR=hello
///     - DEPLOY_ENV=staging
///   ```
///
/// Both forms are normalized to `Option<HashMap<String, String>>`.
/// Lines without `=` in the list form are rejected with a deserialization error.
fn deserialize_env_map<'de, D>(
    deserializer: D,
) -> Result<Option<HashMap<String, String>>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::{self, Visitor};

    struct EnvMapVisitor;

    impl<'de> Visitor<'de> for EnvMapVisitor {
        type Value = Option<HashMap<String, String>>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(
                "a mapping of env vars (KEY: VALUE) or a list of KEY=VALUE strings",
            )
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_map<M: de::MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
            let mut result = HashMap::new();
            while let Some((key, value)) = map.next_entry::<String, String>()? {
                result.insert(key, value);
            }
            Ok(Some(result))
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut result = HashMap::new();
            while let Some(entry) = seq.next_element::<String>()? {
                match entry.split_once('=') {
                    Some((key, value)) => {
                        let key = key.trim();
                        if key.is_empty() {
                            return Err(de::Error::custom(format!(
                                "env list entry has empty key: {:?}",
                                entry
                            )));
                        }
                        result.insert(key.to_string(), value.to_string());
                    }
                    None => {
                        return Err(de::Error::custom(format!(
                            "env list entry must be KEY=VALUE, got: {:?}",
                            entry
                        )));
                    }
                }
            }
            Ok(Some(result))
        }
    }

    deserializer.deserialize_any(EnvMapVisitor)
}

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct Defaults {
    /// Default build targets (e.g., ["x86_64-unknown-linux-gnu", "aarch64-apple-darwin"]).
    pub targets: Option<Vec<String>>,
    /// Default cross-compilation strategy: auto, zigbuild, cross, or cargo.
    pub cross: Option<CrossStrategy>,
    /// Default extra flags passed to cargo build.
    pub flags: Option<String>,
    /// Default archive settings applied to all crates.
    pub archives: Option<DefaultArchiveConfig>,
    /// Default checksum settings applied to all crates.
    pub checksum: Option<ChecksumConfig>,
    /// Exclude specific os/arch combinations from builds.
    pub ignore: Option<Vec<BuildIgnore>>,
    /// Per-target overrides for env, flags, and features.
    pub overrides: Option<Vec<BuildOverride>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DefaultArchiveConfig {
    /// Default archive format for all crates: tar.gz, tar.xz, tar.zst, zip, or binary.
    pub format: Option<String>,
    /// Per-OS format overrides applied by default to all crates.
    pub format_overrides: Option<Vec<FormatOverride>>,
}

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
    #[serde(default, deserialize_with = "deserialize_env_map")]
    pub env: Option<HashMap<String, String>>,
    /// Extra flags to append for matching targets.
    pub flags: Option<String>,
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
    /// Docker image build configurations for this crate (legacy API).
    pub docker: Option<Vec<DockerConfig>>,
    /// Docker V2 image build configurations for this crate (newer API with images+tags, annotations, build_args, sbom, disable).
    pub docker_v2: Option<Vec<DockerV2Config>>,
    /// Docker multi-platform manifest configurations for this crate.
    pub docker_manifests: Option<Vec<DockerManifestConfig>>,
    /// Linux package (deb, rpm, apk) configurations for this crate.
    pub nfpm: Option<Vec<NfpmConfig>>,
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
    /// When true, all build outputs are placed in a flat `dist/` directory
    /// instead of `dist/{target}/`.
    pub no_unique_dist_dir: Option<bool>,
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
            depends_on: None,
            builds: None,
            cross: None,
            archives: ArchivesConfig::Configs(vec![]),
            checksum: None,
            release: None,
            publish: None,
            docker: None,
            docker_v2: None,
            docker_manifests: None,
            nfpm: None,
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
    pub binary: String,
    /// When true, skip this build entirely.
    pub skip: Option<bool>,
    /// Target triples to build for (overrides defaults.targets for this build).
    pub targets: Option<Vec<String>>,
    /// Cargo features to enable for this build.
    pub features: Option<Vec<String>>,
    /// When true, pass --no-default-features to cargo build.
    pub no_default_features: Option<bool>,
    /// Per-target environment variables keyed as {target: {KEY: VALUE}}.
    pub env: Option<HashMap<String, HashMap<String, String>>>,
    /// Copy the binary from another build ID instead of building it.
    pub copy_from: Option<String>,
    /// Extra flags passed to cargo build (e.g., "--locked").
    pub flags: Option<String>,
    /// When true, enable reproducible builds by stripping timestamps.
    pub reproducible: Option<bool>,
    /// Per-build hooks executed before and after compilation.
    pub hooks: Option<BuildHooksConfig>,
    /// Exclude specific os/arch combinations from this build's target matrix.
    /// Falls back to `defaults.ignore` when not set.
    pub ignore: Option<Vec<BuildIgnore>>,
    /// Per-target overrides for env, flags, and features for this build.
    /// Falls back to `defaults.overrides` when not set.
    pub overrides: Option<Vec<BuildOverride>>,
    /// Override the cross-compilation tool binary path (e.g., a custom `cross` wrapper).
    /// When set, this binary is used instead of cargo/cross/zigbuild.
    pub cross_tool: Option<String>,
    /// Override the modification timestamp of built binaries for reproducible builds.
    /// Template string (e.g. `"{{ .CommitTimestamp }}"`) or unix timestamp.
    pub mod_timestamp: Option<String>,
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
    /// Commands to run before the build (or archive) step.
    pub pre: Option<Vec<HookEntry>>,
    /// Commands to run after the build (or archive) step.
    pub post: Option<Vec<HookEntry>>,
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

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ArchiveConfig {
    /// Unique identifier for cross-referencing this archive from other configs.
    pub id: Option<String>,
    /// Archive filename template (supports templates, e.g., "{{ .ProjectName }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}").
    pub name_template: Option<String>,
    /// Archive format: tar.gz, tar.xz, tar.zst, zip, or binary.
    pub format: Option<String>,
    /// Produce multiple archive formats per config (plural, in addition to singular `format`).
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
    /// Pre/post archive hooks.
    pub hooks: Option<BuildHooksConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FormatOverride {
    /// Operating system this override applies to (e.g., "windows", "darwin", "linux").
    /// GoReleaser uses `goos` as the YAML key; both `os` and `goos` are accepted.
    #[serde(alias = "goos")]
    pub os: String,
    /// Archive format override for this OS: tar.gz, tar.xz, tar.zst, zip, or binary.
    pub format: Option<String>,
    /// Plural format overrides (v2.6+). Takes priority over singular format.
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
    /// File permission mode in octal (e.g., "0755" or "0o755").
    pub mode: Option<String>,
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
    pub disable: Option<StringOrBool>,
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
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ContentSource {
    Inline(String),
    FromFile { from_file: String },
    FromUrl { from_url: String },
}

impl PartialEq for ContentSource {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Inline(a), Self::Inline(b)) => a == b,
            (Self::FromFile { from_file: a }, Self::FromFile { from_file: b }) => a == b,
            (Self::FromUrl { from_url: a }, Self::FromUrl { from_url: b }) => a == b,
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
    /// Disable the release stage. Accepts bool or template string
    /// (e.g. `"{{ if IsSnapshot }}true{{ endif }}"` for conditional disable).
    /// GoReleaser supports template strings here since v1.15.0.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub disable: Option<StringOrBool>,
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
    /// This is a GoReleaser Pro feature provided for free by anodize.
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
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            SkipPushConfig::Auto => serializer.serialize_str("auto"),
            SkipPushConfig::Bool(b) => serializer.serialize_bool(*b),
            SkipPushConfig::Template(s) => serializer.serialize_str(s),
        }
    }
}

impl<'de> Deserialize<'de> for SkipPushConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = SkipPushConfig;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "\"auto\", a boolean, or a template string")
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> std::result::Result<SkipPushConfig, E> {
                Ok(SkipPushConfig::Bool(v))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<SkipPushConfig, E> {
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
#[serde(default)]
pub struct PublishConfig {
    #[schemars(schema_with = "crates_publish_schema")]
    /// Publish to crates.io: true/false or object with enabled and index_timeout fields.
    pub crates: Option<CratesPublishConfig>,
    /// Homebrew formula publishing configuration.
    pub homebrew: Option<HomebrewConfig>,
    /// Scoop manifest publishing configuration.
    pub scoop: Option<ScoopConfig>,
    /// Chocolatey package publishing configuration.
    pub chocolatey: Option<ChocolateyConfig>,
    /// WinGet manifest publishing configuration.
    pub winget: Option<WingetConfig>,
    /// AUR (Arch User Repository) package publishing configuration.
    pub aur: Option<AurConfig>,
    /// Krew (kubectl plugin manager) manifest publishing configuration.
    pub krew: Option<KrewConfig>,
    /// Nix derivation publishing configuration.
    pub nix: Option<NixConfig>,
}

/// Schema for crates publish config (bool or object).
fn crates_publish_schema(
    _generator: &mut schemars::r#gen::SchemaGenerator,
) -> schemars::schema::Schema {
    schemars::schema::Schema::Bool(true)
}

impl PublishConfig {
    pub fn crates_config(&self) -> CratesPublishSettings {
        match &self.crates {
            None => CratesPublishSettings::default(),
            Some(CratesPublishConfig::Bool(enabled)) => CratesPublishSettings {
                enabled: *enabled,
                index_timeout: 300,
            },
            Some(CratesPublishConfig::Object {
                enabled,
                index_timeout,
            }) => CratesPublishSettings {
                enabled: *enabled,
                index_timeout: *index_timeout,
            },
        }
    }
}

/// The `crates` field inside `publish` accepts either a bool or an object.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum CratesPublishConfig {
    Bool(bool),
    Object {
        enabled: bool,
        #[serde(default = "default_index_timeout")]
        index_timeout: u64,
    },
}

fn default_index_timeout() -> u64 {
    300
}

/// Resolved settings after interpreting `CratesPublishConfig`.
#[derive(Debug, Clone)]
pub struct CratesPublishSettings {
    pub enabled: bool,
    pub index_timeout: u64,
}

impl Default for CratesPublishSettings {
    fn default() -> Self {
        CratesPublishSettings {
            enabled: false,
            index_timeout: 300,
        }
    }
}

// ---------------------------------------------------------------------------
// HomebrewConfig / ScoopConfig / TapConfig / BucketConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct HomebrewConfig {
    /// Legacy tap config (owner/name). Prefer `repository` for new configs.
    pub tap: Option<TapConfig>,
    /// Unified repository config with branch, token, PR, git SSH support.
    pub repository: Option<RepositoryConfig>,
    /// Commit author with optional signing.
    pub commit_author: Option<CommitAuthorConfig>,
    /// Formula directory in the tap (e.g. "Formula").
    #[serde(alias = "directory")]
    pub folder: Option<String>,
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
    /// Custom commit message template.  Rendered via Tera with `name` and
    /// `version` variables.  Defaults to `"chore: update {{ name }} formula to {{ version }}"`.
    pub commit_msg_template: Option<String>,
    /// Git commit author name for tap updates (legacy; prefer `commit_author`).
    pub commit_author_name: Option<String>,
    /// Git commit author email for tap updates (legacy; prefer `commit_author`).
    pub commit_author_email: Option<String>,
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
    pub goamd64: Option<String>,
    /// ARM version filter (e.g. "6", "7"). Only artifacts matching this
    /// variant are included.
    pub goarm: Option<String>,
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

/// Homebrew Cask configuration for macOS .app bundles.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct HomebrewCaskConfig {
    /// Override the cask name (default: crate name).
    pub name: Option<String>,
    /// Alternative cask names (aliases).
    pub alternative_names: Option<Vec<String>>,
    /// macOS .app bundle name (e.g. "MyApp.app").
    pub app: Option<String>,
    /// Binary stubs to create in /usr/local/bin (paths inside the .app bundle).
    pub binaries: Option<Vec<String>>,
    /// Cask description.
    pub description: Option<String>,
    /// Project homepage URL.
    pub homepage: Option<String>,
    /// URL template for the .dmg/.zip download.
    pub url_template: Option<String>,
    /// Custom caveats shown after install.
    pub caveats: Option<String>,
    /// Zap stanza for complete uninstall cleanup.
    pub zap: Option<Vec<String>>,
    /// Uninstall stanza directives.
    pub uninstall: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Top-level Homebrew Cask config (GoReleaser `homebrew_casks` parity)
// ---------------------------------------------------------------------------

/// Top-level Homebrew Cask configuration.
/// GoReleaser has `homebrew_casks` as a top-level config array with its own
/// repository, commit_author, directory, skip_upload, etc.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct TopLevelHomebrewCaskConfig {
    /// Cask name (default: project name).
    pub name: Option<String>,
    /// Unified repository config for the Homebrew tap.
    pub repository: Option<RepositoryConfig>,
    /// Commit author with optional signing.
    pub commit_author: Option<CommitAuthorConfig>,
    /// Custom commit message template.
    /// Default: "Brew cask update for {{ .ProjectName }} version {{ .Tag }}"
    pub commit_msg_template: Option<String>,
    /// Subdirectory in the tap repo for cask placement (default: "Casks").
    pub directory: Option<String>,
    /// Cask description.
    pub description: Option<String>,
    /// Project homepage URL.
    pub homepage: Option<String>,
    /// Skip publishing the cask. `"true"` always skips; `"auto"` skips
    /// for prerelease versions. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip_upload: Option<StringOrBool>,
    /// Custom Ruby code block inserted into the cask definition.
    pub custom_block: Option<String>,
    /// Build IDs filter: only include artifacts from builds whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Homebrew service block content.
    pub service: Option<String>,
    /// Binary stubs to create in /usr/local/bin.
    pub binaries: Option<Vec<String>>,
    /// Manpage file paths (glob patterns supported).
    pub manpages: Option<Vec<String>>,
    /// Custom caveats shown after install.
    pub caveats: Option<String>,
    /// SPDX license identifier.
    pub license: Option<String>,
    /// Download URL configuration.
    pub url: Option<HomebrewCaskURL>,
    /// Shell completion file paths.
    pub completions: Option<HomebrewCaskCompletions>,
    /// Cask/formula dependencies.
    pub dependencies: Option<Vec<HomebrewCaskDependencyEntry>>,
    /// Conflicting casks/formulas.
    pub conflicts: Option<Vec<HomebrewCaskConflictEntry>>,
    /// Pre/post install/uninstall hooks.
    pub hooks: Option<HomebrewCaskHooks>,
    /// Uninstall stanza configuration.
    pub uninstall: Option<HomebrewCaskUninstall>,
    /// Deep uninstall (zap) stanza configuration.
    pub zap: Option<HomebrewCaskUninstall>,
    /// Auto-generate shell completions from an executable.
    pub generate_completions_from_executable: Option<HomebrewCaskGeneratedCompletions>,
    /// macOS .app bundle name (e.g. "MyApp.app").
    pub app: Option<String>,
    /// Alternative cask names (aliases).
    pub alternative_names: Option<Vec<String>>,
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
#[serde(default)]
pub struct ScoopConfig {
    /// Legacy bucket config (owner/name). Prefer `repository` for new configs.
    pub bucket: Option<BucketConfig>,
    /// Unified repository config with branch, token, PR, git SSH support.
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
    /// Git commit author name (legacy; prefer `commit_author`).
    pub commit_author_name: Option<String>,
    /// Git commit author email (legacy; prefer `commit_author`).
    pub commit_author_email: Option<String>,
    /// Build IDs filter: only include artifacts whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Custom URL template for download URLs (overrides release URL).
    pub url_template: Option<String>,
    /// Artifact selection: "archive" (default), "msi", or "nsis".
    #[serde(rename = "use")]
    pub use_artifact: Option<String>,
    /// amd64 microarchitecture variant filter (e.g. "v1", "v2", "v3", "v4").
    /// Only artifacts matching this variant are included. Default: "v1".
    pub goamd64: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TapConfig {
    /// GitHub owner of the Homebrew tap repository.
    pub owner: String,
    /// Name of the Homebrew tap repository (e.g., "homebrew-tap").
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BucketConfig {
    /// GitHub owner of the Scoop bucket repository.
    pub owner: String,
    /// Name of the Scoop bucket repository (e.g., "scoop-bucket").
    pub name: String,
}

// ---------------------------------------------------------------------------
// ChocolateyConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ChocolateyConfig {
    /// Override the package name (default: crate name).
    pub name: Option<String>,
    /// Build IDs filter: only include artifacts whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// GitHub project repo (owner/name). Used to derive download URLs.
    pub project_repo: Option<ChocolateyRepoConfig>,
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
    /// Space-separated tags for the Chocolatey gallery.
    /// Accepts either a space-separated string (GoReleaser compat) or an array.
    #[serde(
        deserialize_with = "deserialize_space_separated_string_or_vec_opt",
        default
    )]
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
    /// Skip pushing to the Chocolatey community repository. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip_publish: Option<StringOrBool>,
    /// Disable this chocolatey config. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub disable: Option<StringOrBool>,
    /// Artifact selection: "archive" (default), "msi", or "nsis".
    #[serde(rename = "use")]
    pub use_artifact: Option<String>,
    /// amd64 microarchitecture variant filter (e.g. "v1", "v2", "v3", "v4").
    /// Only artifacts matching this variant are included. Default: "v1".
    pub goamd64: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ChocolateyRepoConfig {
    /// GitHub owner of the project repository.
    pub owner: String,
    /// GitHub repository name of the project.
    pub name: String,
}

// ---------------------------------------------------------------------------
// WingetConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
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
    /// Legacy manifests repo config (owner/name). Prefer `repository`.
    pub manifests_repo: Option<WingetManifestsRepoConfig>,
    /// Unified repository config with branch, token, PR, git SSH support.
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
    pub goamd64: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WingetManifestsRepoConfig {
    /// GitHub owner of the WinGet community repository fork.
    pub owner: String,
    /// GitHub repository name of the WinGet community repository fork.
    pub name: String,
}

// ---------------------------------------------------------------------------
// AurConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct AurConfig {
    /// Override the package name (default: crate name + "-bin").
    #[serde(alias = "package_name")]
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
    #[serde(alias = "install_template")]
    pub package: Option<String>,
    /// AUR SSH git URL (e.g., `ssh://aur@aur.archlinux.org/<package>.git`).
    pub git_url: Option<String>,
    /// Custom SSH command for git operations.
    pub git_ssh_command: Option<String>,
    /// Path to SSH private key file.
    pub private_key: Option<String>,
    /// Subdirectory in the git repo for committed files.
    pub directory: Option<String>,
    /// Disable this AUR config. Accepts bool or template string
    /// (e.g. `"{{ if .IsSnapshot }}true{{ endif }}"` for conditional disable).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub disable: Option<StringOrBool>,
    /// Content for a .install file (post-install/pre-remove scripts).
    pub install: Option<String>,
    /// Legacy project URL field.
    pub url: Option<String>,
    /// Packages this PKGBUILD replaces (for upgrade paths from old package names).
    pub replaces: Option<Vec<String>>,
    /// amd64 microarchitecture variant filter (e.g. "v1", "v2", "v3", "v4").
    /// Only artifacts matching this variant are included. Default: "v1".
    pub goamd64: Option<String>,
}

// ---------------------------------------------------------------------------
// KrewConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct KrewConfig {
    /// Override the plugin name (default: crate name).
    pub name: Option<String>,
    /// Build IDs filter: only include artifacts whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Legacy krew-index fork repo (owner/name). Prefer `repository`.
    pub manifests_repo: Option<KrewManifestsRepoConfig>,
    /// Unified repository config with branch, token, PR, git SSH support.
    pub repository: Option<RepositoryConfig>,
    /// Commit author with optional signing.
    pub commit_author: Option<CommitAuthorConfig>,
    /// Custom commit message template.
    pub commit_msg_template: Option<String>,
    /// Full description of the kubectl plugin.
    pub description: Option<String>,
    /// One-line summary of the kubectl plugin (max 255 chars).
    pub short_description: Option<String>,
    /// Project homepage URL for the plugin.
    pub homepage: Option<String>,
    /// Custom URL template for download URLs (overrides release URL).
    pub url_template: Option<String>,
    /// Post-install message shown to the user.
    pub caveats: Option<String>,
    /// Skip publishing. `"true"` always skips; `"auto"` skips for prereleases.
    /// Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip_upload: Option<StringOrBool>,
    /// Legacy upstream repo for PR target. Use `repository.pull_request.base` instead.
    pub upstream_repo: Option<KrewManifestsRepoConfig>,
    /// amd64 microarchitecture variant filter (e.g. "v1", "v2", "v3", "v4").
    /// Only artifacts matching this variant are included. Default: "v1".
    pub goamd64: Option<String>,
    /// ARM version filter (e.g. "6", "7"). Only artifacts matching this
    /// variant are included.
    pub goarm: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct KrewManifestsRepoConfig {
    /// GitHub owner of the krew-index fork.
    pub owner: String,
    /// GitHub repository name of the krew-index fork.
    pub name: String,
}

// ---------------------------------------------------------------------------
// NixConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NixConfig {
    /// Override the derivation name (default: crate name).
    pub name: Option<String>,
    /// Path for the .nix file in the repository (default: `pkgs/<name>/default.nix`).
    pub path: Option<String>,
    /// Unified repository config with branch, token, PR, git SSH support.
    pub repository: Option<RepositoryConfig>,
    /// Commit author with optional signing.
    pub commit_author: Option<CommitAuthorConfig>,
    /// Custom commit message template.
    pub commit_msg_template: Option<String>,
    /// Build IDs filter: only include artifacts whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Custom URL template for download URLs (overrides release URL).
    pub url_template: Option<String>,
    /// Skip publishing. `"true"` always skips; `"auto"` skips for prereleases.
    /// Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip_upload: Option<StringOrBool>,
    /// Custom install commands (replaces auto-generated binary install).
    pub install: Option<String>,
    /// Additional install commands appended after the main install.
    pub extra_install: Option<String>,
    /// Post-install commands (postInstall phase).
    pub post_install: Option<String>,
    /// Short description of the Nix derivation.
    pub description: Option<String>,
    /// Project homepage URL.
    pub homepage: Option<String>,
    /// Nix license identifier (e.g. "mit", "asl20"). Validated against known licenses.
    pub license: Option<String>,
    /// Nix package dependencies with optional OS filtering.
    pub dependencies: Option<Vec<NixDependency>>,
    /// Nix formatter to run on the generated file: "alejandra" or "nixfmt".
    pub formatter: Option<String>,
    /// amd64 microarchitecture variant filter (e.g. "v1", "v2", "v3", "v4").
    /// Only artifacts matching this variant are included. Default: "v1".
    pub goamd64: Option<String>,
}

/// Nix package dependency with optional OS restriction.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NixDependency {
    /// Nix attribute path for the dependency (e.g., "openssl", "pkgs.libgit2").
    pub name: String,
    /// OS restriction: "linux", "darwin", or empty for all.
    pub os: Option<String>,
}

// ---------------------------------------------------------------------------
// DockerConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DockerConfig {
    /// Unique identifier for this Docker config.
    pub id: Option<String>,
    /// Image tags to build and push (supports templates, e.g., "ghcr.io/owner/app:{{ .Version }}").
    pub image_templates: Vec<String>,
    /// Path to the Dockerfile relative to the project root.
    pub dockerfile: String,
    /// Target platforms for multi-arch builds (e.g., ["linux/amd64", "linux/arm64"]).
    pub platforms: Option<Vec<String>>,
    /// Binary names to copy into the image (defaults to all binaries from matched builds).
    pub binaries: Option<Vec<String>>,
    /// Extra `--build-arg` and `--label` flags as templates (e.g., "--build-arg VERSION={{ .Version }}").
    pub build_flag_templates: Option<Vec<String>>,
    /// Skip push: true, false, or "auto" (skip for prereleases).
    #[schemars(schema_with = "skip_push_schema")]
    pub skip_push: Option<SkipPushConfig>,
    /// Extra files to copy into the Docker build context.
    pub extra_files: Option<Vec<String>>,
    /// Extra files whose contents are rendered through the template engine before copying.
    /// Unlike `extra_files` which copy as-is, template variables like `{{ .Tag }}` are expanded.
    /// GoReleaser Pro feature.
    pub templated_extra_files: Option<Vec<TemplatedExtraFile>>,
    /// Extra flags passed to `docker push`.
    pub push_flags: Option<Vec<String>>,
    /// Build IDs filter: only include binary artifacts whose metadata `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// OCI labels to apply to the image via `--label key=value` flags.
    pub labels: Option<HashMap<String, String>>,
    /// Retry configuration for docker push operations.
    pub retry: Option<DockerRetryConfig>,
    /// Docker backend: "docker", "buildx" (default), or "podman".
    #[serde(rename = "use")]
    pub use_backend: Option<String>,
    /// When truthy, skip this docker build entirely. Supports templates.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub disable: Option<StringOrBool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DockerRetryConfig {
    /// Number of retry attempts for failed docker push operations (default: 3).
    pub attempts: Option<u32>,
    /// Duration string, e.g. "1s", "500ms".
    pub delay: Option<String>,
    /// Maximum delay between retries, e.g. "30s".
    pub max_delay: Option<String>,
}

// ---------------------------------------------------------------------------
// DockerV2Config
// ---------------------------------------------------------------------------

/// Docker V2 configuration — the newer, cleaner Docker build API.
///
/// Key differences from the legacy [`DockerConfig`]:
/// - `images` + `tags` instead of `image_templates` (cleaner separation)
/// - `annotations` map (OCI annotations via `--annotation`)
/// - `build_args` as a map instead of `build_flag_templates`
/// - `disable` as [`StringOrBool`] template
/// - `sbom` as [`StringOrBool`] — when truthy, adds `--sbom=true` to buildx
/// - `flags` for arbitrary extra flags
/// - No `goos`/`goarch`/`goarm` fields — uses `platforms` only
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
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
    pub disable: Option<StringOrBool>,
    /// When truthy, adds `--sbom=true` to buildx. Supports templates.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub sbom: Option<StringOrBool>,
    /// When truthy, skip pushing images after build. Supports templates.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip_push: Option<StringOrBool>,
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
    /// When truthy, skip this docker manifest entirely. Supports templates.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub disable: Option<StringOrBool>,
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
    /// File permission umask (e.g. "0o002").
    pub umask: Option<String>,
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
    /// CGo library installation directories (header, carchive, cshared).
    pub libdirs: Option<NfpmLibdirs>,
    /// Path to a YAML-format changelog file for deb/rpm packages.
    pub changelog: Option<String>,
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

/// A single file/directory entry in an nFPM package's `contents` list.
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

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NfpmSignatureConfig {
    /// Path to the signing key file.
    pub key_file: Option<String>,
    /// Key ID to use for signing.
    pub key_id: Option<String>,
    /// Passphrase for the signing key.
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
#[serde(default)]
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
    /// Required interface permissions (e.g. home, network, personal-files).
    pub plugs: Option<Vec<String>>,
    /// Shared code/data interface slots for other snaps.
    pub slots: Option<Vec<String>>,
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
    /// Disable this snapcraft config. Accepts bool or template string
    /// (e.g. `"{{ if .IsSnapshot }}true{{ endif }}"` for conditional disable).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub disable: Option<StringOrBool>,
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
    /// Disable this DMG config. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub disable: Option<StringOrBool>,
    /// Which artifact type to package: "binary" (default) or "appbundle".
    #[serde(rename = "use")]
    pub use_: Option<String>,
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
    /// Disable this MSI config. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub disable: Option<StringOrBool>,
    /// Additional files available in the WiX build context (simple filenames).
    pub extra_files: Option<Vec<String>>,
    /// WiX extensions to enable (e.g., "WixUIExtension"). Templates allowed.
    pub extensions: Option<Vec<String>>,
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
    /// Disable this PKG config. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub disable: Option<StringOrBool>,
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
    /// Disable this NSIS config. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub disable: Option<StringOrBool>,
    /// Remove source archives from artifacts, keeping only the installer.
    pub replace: Option<bool>,
    /// Output timestamp for reproducible builds.
    pub mod_timestamp: Option<String>,
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
    /// Disable this app bundle config. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub disable: Option<StringOrBool>,
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
    /// Disable this Flatpak config. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub disable: Option<StringOrBool>,
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
    /// ACL for uploaded objects (S3, e.g. "public-read", "private").
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
    /// Disable this blob config. Accepts bool or template string
    /// (e.g. `"{{ if IsSnapshot }}true{{ endif }}"` for conditional disable).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub disable: Option<StringOrBool>,
    /// Also upload metadata.json and artifacts.json.
    pub include_meta: Option<bool>,
    /// Pre-existing files to upload (supports glob patterns).
    pub extra_files: Option<Vec<ExtraFile>>,
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

/// An extra file to upload, with optional name override.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ExtraFile {
    /// Glob pattern for the file(s) to upload.
    pub glob: String,
    /// Optional override for the upload filename.
    #[serde(alias = "name_template")]
    pub name: Option<String>,
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
#[serde(default)]
pub struct NotarizeConfig {
    /// Cross-platform signing/notarization (rcodesign-based, works on any OS).
    pub macos: Option<Vec<MacOSSignNotarizeConfig>>,
    /// Native signing/notarization (codesign + xcrun, macOS only).
    pub macos_native: Option<Vec<MacOSNativeSignNotarizeConfig>>,
}

/// Cross-platform macOS signing and notarization via `rcodesign`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct MacOSSignNotarizeConfig {
    /// Build IDs to filter. Default: project name.
    pub ids: Option<Vec<String>>,
    /// Enable this configuration. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
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
    /// Timeout for notarization status polling. Default: "10m".
    pub timeout: Option<String>,
    /// Whether to wait for notarization to complete.
    pub wait: Option<bool>,
}

/// Native macOS signing and notarization via `codesign` + `xcrun notarytool`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct MacOSNativeSignNotarizeConfig {
    /// Build IDs to filter. Default: project name.
    pub ids: Option<Vec<String>>,
    /// Enable this configuration. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
    /// Which artifact type to sign/notarize: "dmg" (default) or "pkg".
    #[serde(rename = "use")]
    pub use_: Option<String>,
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
    /// Timeout in seconds for `xcrun notarytool submit --timeout`. Templates allowed.
    pub timeout: Option<String>,
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
    /// File permissions mode (octal).
    pub mode: Option<u32>,
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
            f.write_str(
                "a string, a source file entry object, or an array of strings/objects",
            )
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
                        let entry = SourceFileEntry::deserialize(other)
                            .map_err(de::Error::custom)?;
                        entries.push(entry);
                    }
                }
            }
            Ok(entries)
        }

        fn visit_map<M: de::MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
            let entry =
                SourceFileEntry::deserialize(de::value::MapAccessDeserializer::new(map))?;
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
    /// Environment variables to pass to the command.
    ///
    /// Accepts both map form (`KEY: value`) and GoReleaser list form
    /// (`- KEY=value`). Values are template-rendered before being set.
    #[serde(default, deserialize_with = "deserialize_env_map")]
    pub env: Option<HashMap<String, String>>,
    /// Command-line arguments (supports templates and $artifact, $document vars).
    pub args: Option<Vec<String>>,
    /// Output document path templates (supports templates).
    pub documents: Option<Vec<String>>,
    /// Which artifacts to catalog: "source", "archive", "binary", "package", "diskimage", "installer", "any" (default: "archive").
    pub artifacts: Option<String>,
    /// Filter by artifact IDs (ignored if artifacts="source").
    pub ids: Option<Vec<String>>,
    /// Disable this SBOM config. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub disable: Option<StringOrBool>,
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
    /// Text prepended to the changelog (inline string or path).
    pub header: Option<String>,
    /// Text appended to the changelog (inline string or path).
    pub footer: Option<String>,
    /// Disable changelog generation. Accepts bool or template string
    /// (e.g. `"{{ if IsSnapshot }}true{{ endif }}"` for conditional disable).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub disable: Option<StringOrBool>,
    /// Changelog source: `"git"` (default), `"github"`, or `"github-native"`.
    /// `"github"` fetches commits via the GitHub API, enriching entries with
    /// author login information (available as the `Logins` template variable).
    /// `"github-native"` delegates entirely to GitHub's auto-generated notes.
    #[serde(rename = "use")]
    pub use_source: Option<String>,
    /// Hash abbreviation length. Default: 7. Set to -1 to omit the hash entirely.
    pub abbrev: Option<i32>,
    /// Template for each changelog commit line.
    /// Available variables: SHA (full hash), ShortSHA (abbreviated), Message (commit subject),
    /// AuthorName, AuthorEmail, Login (per-commit GitHub username, `github` backend only),
    /// Logins (comma-separated list of all GitHub usernames in the release, `github` backend only).
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
    Url { url: String, headers: Option<std::collections::HashMap<String, String>> },
    /// No source configured.
    None,
}

impl ChangelogAiPromptSource {
    /// Resolve the prompt source applying priority: from_file overrides from_url.
    pub fn resolve(&self) -> ResolvedPromptSource {
        if let Some(ref file) = self.from_file {
            if let Some(ref path) = file.path {
                return ResolvedPromptSource::File(path.clone());
            }
        }
        if let Some(ref url_cfg) = self.from_url {
            if let Some(ref url) = url_cfg.url {
                return ResolvedPromptSource::Url {
                    url: url.clone(),
                    headers: url_cfg.headers.clone(),
                };
            }
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
// SignConfig / DockerSignConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct SignConfig {
    /// Unique identifier for this sign config.
    pub id: Option<String>,
    /// Artifact types to sign: "all", "archive", "binary", "checksum", "package", "sbom" (default: "none").
    pub artifacts: Option<String>,
    /// Signing command to invoke (default: "cosign" or "gpg").
    pub cmd: Option<String>,
    /// Arguments passed to the signing command (supports templates with ${artifact} and ${signature}).
    pub args: Option<Vec<String>>,
    /// Signature output filename template (supports templates).
    pub signature: Option<String>,
    /// Content written to the signing command's stdin.
    pub stdin: Option<String>,
    /// Path to a file whose content is written to the signing command's stdin.
    pub stdin_file: Option<String>,
    /// Build IDs filter: only sign artifacts from builds whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Environment variables passed to the signing command.
    #[serde(default, deserialize_with = "deserialize_env_map")]
    pub env: Option<HashMap<String, String>>,
    /// Certificate file to embed in the signature (Cosign bundle signing).
    pub certificate: Option<String>,
    /// Capture and log stdout/stderr of the signing command.
    pub output: Option<bool>,
    /// Template-conditional: skip this sign config if rendered result is "false" or empty.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DockerSignConfig {
    /// Unique identifier for this docker sign config.
    pub id: Option<String>,
    /// Docker artifact types to sign: "all", "image", or "manifest" (default: "none").
    pub artifacts: Option<String>,
    /// Signing command to invoke (default: "cosign").
    pub cmd: Option<String>,
    /// Arguments passed to the signing command (supports templates).
    pub args: Option<Vec<String>>,
    /// Signature output filename template (supports templates).
    pub signature: Option<String>,
    /// Certificate file to embed in the signature (Cosign bundle signing).
    pub certificate: Option<String>,
    /// Docker config IDs filter: only sign images from configs whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Content written to the signing command's stdin.
    pub stdin: Option<String>,
    /// Path to a file whose content is written to the signing command's stdin.
    pub stdin_file: Option<String>,
    /// Environment variables passed to the signing command.
    #[serde(default, deserialize_with = "deserialize_env_map")]
    pub env: Option<HashMap<String, String>>,
    /// Capture and log stdout/stderr of the docker signing command.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub output: Option<StringOrBool>,
    /// Template-conditional: skip this docker sign config if rendered result is "false" or empty.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
}

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
    /// When true, compress binaries with UPX (default: true).
    pub enabled: bool,
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
            enabled: false,
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
    /// Primary field is `version_template` (GoReleaser convention); `name_template` is the
    /// deprecated alias kept for backwards compatibility.
    #[serde(alias = "name_template", rename = "version_template")]
    pub name_template: String,
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
    /// Email announcement configuration.
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
    /// Enable Bluesky announcements.
    pub enabled: Option<bool>,
    /// Bluesky handle/username (e.g. "user.bsky.social").
    pub username: Option<String>,
    /// Message template for the post. Default: "{{ .ProjectName }} {{ .Tag }} is out! Check it out at {{ .ReleaseURL }}"
    pub message_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DiscourseAnnounce {
    /// Enable Discourse announcements.
    pub enabled: Option<bool>,
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
    /// Enable LinkedIn announcements. Requires LINKEDIN_ACCESS_TOKEN env var.
    pub enabled: Option<bool>,
    /// Message template for the LinkedIn share post. Default: "{{ .ProjectName }} {{ .Tag }} is out! Check it out at {{ .ReleaseURL }}"
    pub message_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct OpenCollectiveAnnounce {
    /// Enable OpenCollective announcements. Requires OPENCOLLECTIVE_TOKEN env var.
    pub enabled: Option<bool>,
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
    /// Enable Twitter/X announcements. Requires TWITTER_CONSUMER_KEY, TWITTER_CONSUMER_SECRET, TWITTER_ACCESS_TOKEN, TWITTER_ACCESS_TOKEN_SECRET env vars.
    pub enabled: Option<bool>,
    /// Tweet message template. Default: "{{ .ProjectName }} {{ .Tag }} is out! Check it out at {{ .ReleaseURL }}"
    pub message_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct MastodonAnnounce {
    /// Enable Mastodon announcements. Requires MASTODON_CLIENT_ID, MASTODON_CLIENT_SECRET, MASTODON_ACCESS_TOKEN env vars.
    pub enabled: Option<bool>,
    /// Mastodon instance URL (e.g. "https://mastodon.social").
    pub server: Option<String>,
    /// Toot message template. Default: "{{ .ProjectName }} {{ .Tag }} is out! Check it out at {{ .ReleaseURL }}"
    pub message_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DiscordAnnounce {
    /// Enable Discord announcements.
    pub enabled: Option<bool>,
    /// Discord webhook URL. Use templates like `{{ Env.DISCORD_WEBHOOK_ID }}` to reference environment variables.
    pub webhook_url: Option<String>,
    /// Message template for the Discord embed. Default: "{{ .ProjectName }} {{ .Tag }} is out! Check it out at {{ .ReleaseURL }}"
    pub message_template: Option<String>,
    /// Author name displayed in the embed.
    pub author: Option<String>,
    /// Embed color as a decimal integer (default: 3553599, GoReleaser blue).
    pub color: Option<u32>,
    /// Icon URL for the embed footer.
    pub icon_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct WebhookConfig {
    /// Enable generic webhook announcements.
    pub enabled: Option<bool>,
    /// Webhook endpoint URL (supports template variables).
    pub endpoint_url: Option<String>,
    /// Custom HTTP headers to include in the request.
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
    /// Enable Telegram announcements. Requires bot_token and chat_id.
    pub enabled: Option<bool>,
    /// Telegram Bot API token. Get one from @BotFather.
    pub bot_token: Option<String>,
    /// Telegram chat ID to send the message to (supports template variables).
    pub chat_id: Option<String>,
    /// Message template. Default: "{{ .ProjectName }} {{ .Tag }} is out! Check it out at {{ .ReleaseURL }}"
    pub message_template: Option<String>,
    /// Parse mode: "MarkdownV2" or "HTML" (defaults to "MarkdownV2").
    pub parse_mode: Option<String>,
    /// Message thread ID for sending to a specific topic in a forum group.
    pub message_thread_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct TeamsAnnounce {
    /// Enable Microsoft Teams announcements.
    pub enabled: Option<bool>,
    /// Teams incoming webhook URL.
    pub webhook_url: Option<String>,
    /// Message template for the Adaptive Card body. Default: "{{ .ProjectName }} {{ .Tag }} is out! Check it out at {{ .ReleaseURL }}"
    pub message_template: Option<String>,
    /// Title template for the Adaptive Card header.
    pub title_template: Option<String>,
    /// Theme color for the card (hex string, e.g. "0076D7").
    pub color: Option<String>,
    /// Icon URL displayed in the card header.
    pub icon_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct MattermostAnnounce {
    /// Enable Mattermost announcements.
    pub enabled: Option<bool>,
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
    /// Enable email announcements.
    pub enabled: Option<bool>,
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
    /// Email subject template. Default: "{{ .ProjectName }} {{ .Tag }} released"
    pub subject_template: Option<String>,
    /// Body template (called body_template in GoReleaser, message_template here for consistency).
    pub message_template: Option<String>,
    /// Skip TLS certificate verification (default: false).
    pub insecure_skip_verify: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct RedditAnnounce {
    /// Enable Reddit announcements. Requires REDDIT_SECRET and REDDIT_PASSWORD env vars.
    pub enabled: Option<bool>,
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
    /// Enable Slack announcements.
    pub enabled: Option<bool>,
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
    /// Disable this publisher. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub disable: Option<StringOrBool>,
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
// GemFury publisher
// ---------------------------------------------------------------------------

/// GemFury publisher configuration.
/// Pushes packages to GemFury (fury.io) package hosting.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct FuryConfig {
    /// GemFury account name.
    pub account: Option<String>,
    /// Disable this publisher. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub disable: Option<StringOrBool>,
    /// Environment variable name containing the GemFury push token.
    pub secret_name: Option<String>,
    /// Build IDs filter: only publish artifacts from builds whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Package format filter: only publish artifacts matching these formats (e.g. "deb", "rpm").
    pub formats: Option<Vec<String>>,
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
// NPM publisher
// ---------------------------------------------------------------------------

/// NPM publisher configuration.
/// Publishes packages to NPM registries.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NpmConfig {
    /// Unique identifier for this NPM publisher (when multiple are configured).
    pub id: Option<String>,
    /// NPM package name (e.g. `@myorg/mypackage`).
    pub name: Option<String>,
    /// Package description.
    pub description: Option<String>,
    /// Package homepage URL.
    pub homepage: Option<String>,
    /// Package keywords for NPM search.
    pub keywords: Option<Vec<String>>,
    /// SPDX license identifier (e.g. "MIT", "Apache-2.0").
    pub license: Option<String>,
    /// Package author (e.g. `"Jane Doe <jane@example.com>"`).
    pub author: Option<String>,
    /// Repository URL for package.json.
    pub repository: Option<String>,
    /// Bug tracker URL for package.json.
    pub bugs: Option<String>,
    /// NPM access level: "public" or "restricted".
    pub access: Option<String>,
    /// NPM dist-tag (e.g. "latest", "next", "beta").
    pub tag: Option<String>,
    /// Package format: "tgz" (default) or other supported NPM formats.
    pub format: Option<String>,
    /// Build IDs filter: only publish artifacts from builds whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Extra files to include in the NPM package.
    pub extra_files: Option<Vec<ExtraFileSpec>>,
    /// Extra files whose contents are rendered through the template engine before inclusion.
    pub templated_extra_files: Option<Vec<TemplatedExtraFile>>,
    /// Disable this publisher. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub disable: Option<StringOrBool>,
    /// Custom URL template for package downloads.
    pub url_template: Option<String>,
    /// Template-conditional: only run this publisher if the condition evaluates to true.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
    /// Additional package.json fields as key-value pairs.
    pub extra: Option<HashMap<String, serde_json::Value>>,
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
    #[serde(default, deserialize_with = "deserialize_env_map")]
    pub env: Option<HashMap<String, String>>,
    /// Working directory for the publisher command.
    pub dir: Option<String>,
    /// Template-conditional disable: if rendered result is `"true"`, skip this publisher.
    /// Accepts bool or template string (e.g. `"{{ if .IsSnapshot }}true{{ endif }}"`).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub disable: Option<StringOrBool>,
    /// Include checksums in published artifacts.
    pub checksum: Option<bool>,
    /// Include signatures in published artifacts.
    pub signature: Option<bool>,
    /// Include metadata artifacts in published artifacts.
    pub meta: Option<bool>,
    /// Extra files to include in publishing (glob patterns with optional name override).
    pub extra_files: Option<Vec<ExtraFile>>,
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
    /// Commands to run before the pipeline or stage starts.
    /// GoReleaser uses `hooks` as the field name under `before:`. We accept
    /// both `pre` and `hooks` for migration compatibility.
    #[serde(alias = "hooks")]
    pub pre: Option<Vec<HookEntry>>,
    /// Commands to run after the pipeline or stage completes.
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
    #[serde(default, deserialize_with = "deserialize_env_map")]
    pub env: Option<HashMap<String, String>>,
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
/// Controls how anodize discovers and orders tags when determining the current
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
#[serde(default)]
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
}

// ---------------------------------------------------------------------------
// WorkspaceConfig
// ---------------------------------------------------------------------------

/// A workspace represents an independent project root within a monorepo.
/// Each workspace has its own crates, changelog, and release configuration,
/// allowing independently-versioned components that aren't Cargo workspace members.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct WorkspaceConfig {
    /// Workspace identifier used in logs and template variables.
    pub name: String,
    /// Crates belonging to this workspace.
    pub crates: Vec<CrateConfig>,
    /// Changelog configuration for this workspace.
    pub changelog: Option<ChangelogConfig>,
    /// Signing configurations for binaries, archives, and checksums.
    #[serde(default, alias = "sign", deserialize_with = "deserialize_signs")]
    #[schemars(schema_with = "signs_schema")]
    pub signs: Vec<SignConfig>,
    /// Binary-specific signing configs (same shape as `signs` but only for binary artifacts).
    #[serde(default, alias = "binary_sign", deserialize_with = "deserialize_signs")]
    #[schemars(schema_with = "signs_schema")]
    pub binary_signs: Vec<SignConfig>,
    /// Hooks run before this workspace's pipeline starts.
    pub before: Option<HooksConfig>,
    /// Hooks run after this workspace's pipeline completes.
    pub after: Option<HooksConfig>,
    /// Environment variables scoped to this workspace.
    ///
    /// Accepts both map form (`MY_VAR: hello`) and GoReleaser list form
    /// (`- MY_VAR=hello`). Values are template-rendered at pipeline startup.
    #[serde(default, deserialize_with = "deserialize_env_map")]
    pub env: Option<HashMap<String, String>>,
}

// ---------------------------------------------------------------------------
// StringOrBool — accepts bool or template string in YAML
// ---------------------------------------------------------------------------

/// A value that can be either a bool or a template string.
/// Used by `disable`, `skip_upload`, and similar fields across multiple config
/// structs to support both `disable: true` and template conditionals like
/// `disable: "{{ if .IsSnapshot }}true{{ endif }}"`.
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

    /// Evaluate whether this value means "disabled".
    ///
    /// If the value is a template string (contains `{`), it is rendered via
    /// the provided closure and the result is compared to `"true"`.
    /// Otherwise, the plain bool / string value is evaluated directly.
    /// Evaluate whether this value resolves to `true`.
    ///
    /// If the value is a template string (contains `{`), it is rendered via
    /// the provided closure and the result is compared to `"true"`.
    /// Otherwise, the plain bool / string value is evaluated directly.
    pub fn evaluates_to_true(&self, render: impl Fn(&str) -> anyhow::Result<String>) -> bool {
        if self.is_template() {
            render(self.as_str())
                .map(|r| r.trim() == "true")
                .unwrap_or(false)
        } else {
            self.as_bool()
        }
    }

    /// Evaluate whether this value means "disabled".
    ///
    /// Delegates to [`evaluates_to_true`](Self::evaluates_to_true) — a
    /// convenience alias with domain-specific semantics.
    pub fn is_disabled(&self, render: impl Fn(&str) -> anyhow::Result<String>) -> bool {
        self.evaluates_to_true(render)
    }
}

impl Default for StringOrBool {
    fn default() -> Self {
        StringOrBool::Bool(false)
    }
}

/// Custom deserializer for `Option<StringOrBool>`.
fn deserialize_string_or_bool_opt<'de, D>(deserializer: D) -> Result<Option<StringOrBool>, D::Error>
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

/// Custom deserializer for `Option<Vec<String>>` that accepts either a
/// space-separated string (split into individual tags) or an array of strings.
/// Used by `ChocolateyConfig.tags` for GoReleaser compatibility where tags
/// are a single space-delimited string.
fn deserialize_space_separated_string_or_vec_opt<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::{self, Visitor};

    struct SpaceSepOrVecVisitor;

    impl<'de> Visitor<'de> for SpaceSepOrVecVisitor {
        type Value = Option<Vec<String>>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a space-separated string, a list of strings, or null")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            let tags: Vec<String> = v.split_whitespace().map(|s| s.to_owned()).collect();
            if tags.is_empty() {
                Ok(None)
            } else {
                Ok(Some(tags))
            }
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
            self.visit_str(&v)
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut items = Vec::new();
            while let Some(item) = seq.next_element::<String>()? {
                items.push(item);
            }
            if items.is_empty() {
                Ok(None)
            } else {
                Ok(Some(items))
            }
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
    }

    deserializer.deserialize_any(SpaceSepOrVecVisitor)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
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
    format: tar.gz
    format_overrides:
      - os: windows
        format: zip
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
  name_template: "{{ .Version }}-SNAPSHOT-{{ .ShortCommit }}"
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(
            config.snapshot.unwrap().name_template,
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
    fn test_publish_crates_bool_and_object() {
        let yaml_bool = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      crates: true
"#;
        let config: Config = serde_yaml_ng::from_str(yaml_bool).unwrap();
        assert!(
            config.crates[0]
                .publish
                .as_ref()
                .unwrap()
                .crates_config()
                .enabled
        );

        let yaml_obj = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      crates:
        enabled: true
        index_timeout: 120
"#;
        let config: Config = serde_yaml_ng::from_str(yaml_obj).unwrap();
        let crates_cfg = config.crates[0].publish.as_ref().unwrap().crates_config();
        assert!(crates_cfg.enabled);
        assert_eq!(crates_cfg.index_timeout, 120);
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
  footer: "---\nGenerated by anodize"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"##;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let cl = config.changelog.as_ref().unwrap();
        assert_eq!(cl.header, Some("# My Release Notes".to_string()));
        assert_eq!(cl.footer, Some("---\nGenerated by anodize".to_string()));
    }

    #[test]
    fn test_changelog_disable() {
        let yaml = r#"
project_name: test
changelog:
  disable: true
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let cl = config.changelog.as_ref().unwrap();
        assert_eq!(cl.disable, Some(StringOrBool::Bool(true)));
    }

    #[test]
    fn test_changelog_disable_false() {
        let yaml = r#"
project_name: test
changelog:
  disable: false
  sort: desc
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let cl = config.changelog.as_ref().unwrap();
        assert_eq!(cl.disable, Some(StringOrBool::Bool(false)));
        assert_eq!(cl.sort, Some("desc".to_string()));
    }

    // ---- ChecksumConfig disable tests ----

    #[test]
    fn test_checksum_disable() {
        let yaml = r#"
project_name: test
defaults:
  checksum:
    disable: true
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let checksum = config.defaults.as_ref().unwrap().checksum.as_ref().unwrap();
        assert_eq!(checksum.disable, Some(StringOrBool::Bool(true)));
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
      disable: true
      algorithm: sha512
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let checksum = config.crates[0].checksum.as_ref().unwrap();
        assert_eq!(checksum.disable, Some(StringOrBool::Bool(true)));
        assert_eq!(checksum.algorithm, Some("sha512".to_string()));
    }

    #[test]
    fn test_checksum_disable_template_string() {
        let yaml = r#"
project_name: test
defaults:
  checksum:
    disable: "{{ if .IsSnapshot }}true{{ end }}"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let checksum = config.defaults.as_ref().unwrap().checksum.as_ref().unwrap();
        match &checksum.disable {
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

        let tmpl = MakeLatestConfig::String("{{ if .IsSnapshot }}false{{ else }}true{{ end }}".to_string());
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
      footer: "---\nPowered by anodize"
"###;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let release = config.crates[0].release.as_ref().unwrap();
        assert_eq!(
            release.header,
            Some(ContentSource::Inline("## Custom Header".to_string()))
        );
        assert_eq!(
            release.footer,
            Some(ContentSource::Inline("---\nPowered by anodize".to_string()))
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
                from_url: "https://example.com/header.md".to_string()
            })
        );
        assert_eq!(
            release.footer,
            Some(ContentSource::FromUrl {
                from_url: "https://example.com/footer.md".to_string()
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
    fn test_signs_single_object_backward_compat() {
        let yaml = r#"
project_name: test
sign:
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
sign:
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

[sign]
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
  MY_VAR: hello
  DEPLOY_ENV: staging
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let env = config.env.as_ref().unwrap();
        assert_eq!(env.get("MY_VAR").unwrap(), "hello");
        assert_eq!(env.get("DEPLOY_ENV").unwrap(), "staging");
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

[env]
API_KEY = "secret123"
STAGE = "prod"

[[crates]]
name = "a"
path = "."
tag_template = "v{{ .Version }}"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let env = config.env.as_ref().unwrap();
        assert_eq!(env.get("API_KEY").unwrap(), "secret123");
        assert_eq!(env.get("STAGE").unwrap(), "prod");
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
        assert_eq!(env.get("MY_VAR").unwrap(), "hello");
        assert_eq!(env.get("STAGE").unwrap(), "prod");
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
        assert_eq!(env.get("MY_VAR").unwrap(), "hello");
        assert_eq!(env.get("DEPLOY_ENV").unwrap(), "staging");
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
        assert_eq!(env.get("MY_VERSION").unwrap(), "{{ .Tag }}");
        assert_eq!(env.get("BUILD_DATE").unwrap(), "{{ .Date }}");
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
        assert_eq!(
            env.get("LDFLAGS").unwrap(),
            "-X main.version=1.0.0",
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
        assert_eq!(env.get("EMPTY_VAR").unwrap(), "");
    }

    #[test]
    fn test_env_list_form_no_equals_is_error() {
        let yaml = r#"
project_name: test
env:
  - "NO_EQUALS"
crates: []
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(result.is_err(), "list entries without = should be rejected");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("KEY=VALUE"),
            "error should mention KEY=VALUE format, got: {}",
            err
        );
    }

    #[test]
    fn test_env_list_form_empty_key_is_error() {
        let yaml = r#"
project_name: test
env:
  - "=orphan_value"
crates: []
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(result.is_err(), "list entries with empty key should be rejected");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("empty key"),
            "error should mention empty key, got: {}",
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
        assert_eq!(
            env.get("DUPED").unwrap(),
            "second",
            "later entries should override earlier ones"
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
        assert_eq!(env.get("WS_VAR").unwrap(), "from-workspace");
        assert_eq!(env.get("WS_BUILD").unwrap(), "{{ .Tag }}");
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
        let config = result.expect("empty YAML should parse to Config defaults");
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
    fn test_unknown_top_level_fields_ignored() {
        // serde(default) without deny_unknown_fields should silently ignore extras
        let yaml = r#"
project_name: test
unknown_top_level_field: "this should be ignored"
another_mystery: 42
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.project_name, "test");
        assert_eq!(config.crates.len(), 1);
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
      MY_VAR: hello
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let ws = &config.workspaces.as_ref().unwrap()[0];
        assert_eq!(ws.name, "myws");
        assert_eq!(ws.signs.len(), 1);
        assert!(ws.before.is_some());
        assert!(ws.after.is_some());
        assert_eq!(ws.env.as_ref().unwrap().get("MY_VAR").unwrap(), "hello");
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
    fn test_chocolatey_config_yaml() {
        let yaml = r#"
project_name: test
crates:
  - name: mytool
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      chocolatey:
        project_repo:
          owner: myorg
          name: mytool
        description: "A great tool"
        license: MIT
        tags:
          - cli
          - tool
        authors: "Test Author"
        project_url: "https://github.com/myorg/mytool"
        icon_url: "https://example.com/icon.png"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let choco = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .chocolatey
            .as_ref()
            .unwrap();

        let repo = choco.project_repo.as_ref().unwrap();
        assert_eq!(repo.owner, "myorg");
        assert_eq!(repo.name, "mytool");
        assert_eq!(choco.description, Some("A great tool".to_string()));
        assert_eq!(choco.license, Some("MIT".to_string()));
        assert_eq!(
            choco.tags,
            Some(vec!["cli".to_string(), "tool".to_string()])
        );
        assert_eq!(choco.authors, Some("Test Author".to_string()));
        assert_eq!(
            choco.project_url,
            Some("https://github.com/myorg/mytool".to_string())
        );
        assert_eq!(
            choco.icon_url,
            Some("https://example.com/icon.png".to_string())
        );
    }

    #[test]
    fn test_chocolatey_config_minimal() {
        let yaml = r#"
project_name: test
crates:
  - name: mytool
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      chocolatey:
        project_repo:
          owner: myorg
          name: mytool
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let choco = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .chocolatey
            .as_ref()
            .unwrap();

        let repo = choco.project_repo.as_ref().unwrap();
        assert_eq!(repo.owner, "myorg");
        assert_eq!(repo.name, "mytool");
        assert!(choco.description.is_none());
        assert!(choco.license.is_none());
        assert!(choco.tags.is_none());
        assert!(choco.authors.is_none());
        assert!(choco.project_url.is_none());
        assert!(choco.icon_url.is_none());
    }

    #[test]
    fn test_chocolatey_config_toml() {
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

[crates.publish.chocolatey.project_repo]
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
        let repo = choco.project_repo.as_ref().unwrap();
        assert_eq!(repo.owner, "org");
    }

    #[test]
    fn test_chocolatey_tags_space_separated_string() {
        // GoReleaser uses a plain space-separated string for tags.
        let yaml = r#"
project_name: test
crates:
  - name: mytool
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      chocolatey:
        project_repo:
          owner: myorg
          name: mytool
        tags: "cli tool automation"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let choco = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .chocolatey
            .as_ref()
            .unwrap();

        assert_eq!(
            choco.tags,
            Some(vec![
                "cli".to_string(),
                "tool".to_string(),
                "automation".to_string()
            ])
        );
    }

    #[test]
    fn test_chocolatey_tags_empty_string_is_none() {
        let yaml = r#"
project_name: test
crates:
  - name: mytool
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      chocolatey:
        project_repo:
          owner: myorg
          name: mytool
        tags: ""
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let choco = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .chocolatey
            .as_ref()
            .unwrap();

        assert!(choco.tags.is_none());
    }

    // ---- WingetConfig tests ----

    #[test]
    fn test_winget_config_yaml() {
        let yaml = r#"
project_name: test
crates:
  - name: mytool
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      winget:
        manifests_repo:
          owner: myorg
          name: winget-pkgs
        description: "A great tool"
        license: MIT
        package_identifier: "MyOrg.MyTool"
        publisher: "My Org"
        publisher_url: "https://github.com/myorg"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let winget = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .winget
            .as_ref()
            .unwrap();

        let repo = winget.manifests_repo.as_ref().unwrap();
        assert_eq!(repo.owner, "myorg");
        assert_eq!(repo.name, "winget-pkgs");
        assert_eq!(winget.description, Some("A great tool".to_string()));
        assert_eq!(winget.license, Some("MIT".to_string()));
        assert_eq!(winget.package_identifier, Some("MyOrg.MyTool".to_string()));
        assert_eq!(winget.publisher, Some("My Org".to_string()));
        assert_eq!(
            winget.publisher_url,
            Some("https://github.com/myorg".to_string())
        );
    }

    #[test]
    fn test_winget_config_minimal() {
        let yaml = r#"
project_name: test
crates:
  - name: mytool
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      winget:
        manifests_repo:
          owner: myorg
          name: winget-pkgs
        package_identifier: "MyOrg.MyTool"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let winget = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .winget
            .as_ref()
            .unwrap();

        let repo = winget.manifests_repo.as_ref().unwrap();
        assert_eq!(repo.owner, "myorg");
        assert_eq!(repo.name, "winget-pkgs");
        assert_eq!(winget.package_identifier, Some("MyOrg.MyTool".to_string()));
        assert!(winget.description.is_none());
        assert!(winget.license.is_none());
        assert!(winget.publisher.is_none());
        assert!(winget.publisher_url.is_none());
    }

    #[test]
    fn test_winget_config_toml() {
        let toml_str = r#"
project_name = "test"

[[crates]]
name = "mytool"
path = "."
tag_template = "v{{ .Version }}"

[crates.publish.winget]
description = "A tool"
license = "MIT"
package_identifier = "Org.Tool"
publisher = "Org"

[crates.publish.winget.manifests_repo]
owner = "org"
name = "winget-pkgs"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let winget = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .winget
            .as_ref()
            .unwrap();

        assert_eq!(winget.description, Some("A tool".to_string()));
        assert_eq!(winget.package_identifier, Some("Org.Tool".to_string()));
        let repo = winget.manifests_repo.as_ref().unwrap();
        assert_eq!(repo.owner, "org");
    }

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
        package_name: mytool-bin
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
        url: "https://github.com/org/mytool"
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
        assert_eq!(aur.url, Some("https://github.com/org/mytool".to_string()));
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

    #[test]
    fn test_krew_config_yaml() {
        let yaml = r#"
project_name: test
crates:
  - name: kubectl-mytool
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      krew:
        manifests_repo:
          owner: myorg
          name: krew-index
        description: "A comprehensive kubectl plugin"
        short_description: "A kubectl plugin"
        homepage: "https://github.com/myorg/kubectl-mytool"
        caveats: "Run 'kubectl mytool init' after installation."
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let krew = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .krew
            .as_ref()
            .unwrap();

        let repo = krew.manifests_repo.as_ref().unwrap();
        assert_eq!(repo.owner, "myorg");
        assert_eq!(repo.name, "krew-index");
        assert_eq!(
            krew.description,
            Some("A comprehensive kubectl plugin".to_string())
        );
        assert_eq!(krew.short_description, Some("A kubectl plugin".to_string()));
        assert_eq!(
            krew.homepage,
            Some("https://github.com/myorg/kubectl-mytool".to_string())
        );
        assert_eq!(
            krew.caveats,
            Some("Run 'kubectl mytool init' after installation.".to_string())
        );
    }

    #[test]
    fn test_krew_config_minimal() {
        let yaml = r#"
project_name: test
crates:
  - name: kubectl-mytool
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      krew:
        manifests_repo:
          owner: myorg
          name: krew-index
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let krew = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .krew
            .as_ref()
            .unwrap();

        let repo = krew.manifests_repo.as_ref().unwrap();
        assert_eq!(repo.owner, "myorg");
        assert_eq!(repo.name, "krew-index");
        assert!(krew.description.is_none());
        assert!(krew.short_description.is_none());
        assert!(krew.homepage.is_none());
        assert!(krew.caveats.is_none());
    }

    #[test]
    fn test_krew_config_toml() {
        let toml_str = r#"
project_name = "test"

[[crates]]
name = "kubectl-mytool"
path = "."
tag_template = "v{{ .Version }}"

[crates.publish.krew]
short_description = "A kubectl plugin"
homepage = "https://example.com"

[crates.publish.krew.manifests_repo]
owner = "org"
name = "krew-index"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let krew = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .krew
            .as_ref()
            .unwrap();

        assert_eq!(krew.short_description, Some("A kubectl plugin".to_string()));
        let repo = krew.manifests_repo.as_ref().unwrap();
        assert_eq!(repo.owner, "org");
    }

    // ---- Combined all publishers ----

    #[test]
    fn test_all_seven_publishers_config() {
        let yaml = r#"
project_name: test
crates:
  - name: mytool
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      crates: true
      homebrew:
        tap:
          owner: org
          name: homebrew-tap
      scoop:
        bucket:
          owner: org
          name: scoop-bucket
      chocolatey:
        project_repo:
          owner: org
          name: mytool
      winget:
        manifests_repo:
          owner: org
          name: winget-pkgs
        package_identifier: "Org.MyTool"
      aur:
        git_url: "ssh://aur@aur.archlinux.org/mytool.git"
      krew:
        manifests_repo:
          owner: org
          name: krew-index
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let publish = config.crates[0].publish.as_ref().unwrap();

        assert!(publish.crates.is_some());
        assert!(publish.homebrew.is_some());
        assert!(publish.scoop.is_some());
        assert!(publish.chocolatey.is_some());
        assert!(publish.winget.is_some());
        assert!(publish.aur.is_some());
        assert!(publish.krew.is_some());
    }

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
        let files = env_files.as_list().expect("expected List variant");
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
        let tokens = env_files.as_token_files().expect("expected TokenFiles variant");
        assert_eq!(
            tokens.github_token.as_deref(),
            Some("~/.config/goreleaser/github_token")
        );
        assert_eq!(
            tokens.gitlab_token.as_deref(),
            Some("/etc/tokens/gitlab")
        );
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
        let tokens = env_files.as_token_files().expect("expected TokenFiles variant");
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
            if let Some(v) = orig_gh { std::env::set_var("GITHUB_TOKEN", v); }
            if let Some(v) = orig_gl { std::env::set_var("GITLAB_TOKEN", v); }
            if let Some(v) = orig_gt { std::env::set_var("GITEA_TOKEN", v); }
        }

        assert_eq!(vars.get("GITHUB_TOKEN").unwrap(), "ghp_test123");
        assert_eq!(vars.get("GITLAB_TOKEN").unwrap(), "glpat-test456");
        // GITEA_TOKEN not present — default file doesn't exist
        assert!(vars.get("GITEA_TOKEN").is_none());
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
        unsafe { std::env::set_var("GITHUB_TOKEN", "env_token"); }

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
            vars.get("GITHUB_TOKEN").is_none(),
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
        unsafe { std::env::set_var("HOME", dir.path()); }

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
        writeln!(f, "TEST_ANODIZE_KEY=hello_world").unwrap();
        writeln!(f, "TEST_ANODIZE_QUOTED=\"with quotes\"").unwrap();
        writeln!(f, "TEST_ANODIZE_SINGLE='single_quoted'").unwrap();
        writeln!(f, "export TEST_ANODIZE_EXPORT=exported_val").unwrap();
        drop(f);

        let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Normal);
        let vars = load_env_files(&[env_path.to_string_lossy().to_string()], &log).unwrap();
        assert_eq!(vars.get("TEST_ANODIZE_KEY").unwrap(), "hello_world");
        assert_eq!(vars.get("TEST_ANODIZE_QUOTED").unwrap(), "with quotes");
        assert_eq!(
            vars.get("TEST_ANODIZE_SINGLE").unwrap(),
            "single_quoted",
            "single-quoted values should have quotes stripped"
        );
        assert_eq!(
            vars.get("TEST_ANODIZE_EXPORT").unwrap(),
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
        writeln!(f, "TEST_ANODIZE_SINGLEQ=\"").unwrap();
        // Empty key line (=value) should be skipped
        writeln!(f, "=orphan_value").unwrap();
        // Line without = should be skipped with warning
        writeln!(f, "NO_EQUALS_HERE").unwrap();
        drop(f);

        let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Normal);
        let vars = load_env_files(&[env_path.to_string_lossy().to_string()], &log).unwrap();
        // The single-quote value should be kept as-is (not stripped, length < 2 for
        // matching quotes)
        assert_eq!(vars.get("TEST_ANODIZE_SINGLEQ").unwrap(), "\"");
        // Empty key and no-equals lines should have been skipped
        assert!(!vars.contains_key(""), "empty key should be skipped");
    }

    #[test]
    fn test_load_env_files_nonexistent_returns_error() {
        let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Normal);
        let result = load_env_files(
            &["/tmp/nonexistent_anodize_env_file_12345".to_string()],
            &log,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("failed to read env file"));
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
        let files = wrapper.env_files.as_list().expect("expected List variant");
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
            .expect("expected TokenFiles variant");
        assert_eq!(
            tokens.github_token.as_deref(),
            Some("~/.config/goreleaser/github_token")
        );
        assert_eq!(
            tokens.gitlab_token.as_deref(),
            Some("/etc/tokens/gitlab")
        );
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
        let yaml = r#"
project_name: test
defaults:
  targets:
    - x86_64-unknown-linux-gnu
    - aarch64-unknown-linux-gnu
  ignore:
    - os: windows
      arch: arm64
    - os: linux
      arch: "386"
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let defaults = config.defaults.unwrap();
        let ignores = defaults.ignore.unwrap();
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
        assert!(defaults.ignore.is_none());
    }

    // ---- BuildOverride tests ----

    #[test]
    fn test_build_override_parses() {
        let yaml = r#"
project_name: test
defaults:
  overrides:
    - targets:
        - "x86_64-*"
      features:
        - simd
      flags: "--release"
      env:
        CC: gcc
    - targets:
        - "*-apple-darwin"
      features:
        - metal
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let defaults = config.defaults.unwrap();
        let overrides = defaults.overrides.unwrap();
        assert_eq!(overrides.len(), 2);
        assert_eq!(overrides[0].targets, vec!["x86_64-*"]);
        assert_eq!(overrides[0].features, Some(vec!["simd".to_string()]));
        assert_eq!(overrides[0].flags, Some("--release".to_string()));
        assert_eq!(overrides[0].env.as_ref().unwrap().get("CC").unwrap(), "gcc");
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
        assert!(defaults.overrides.is_none());
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

    #[test]
    fn test_homebrew_config_new_fields() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      homebrew:
        tap:
          owner: myorg
          name: homebrew-tap
        homepage: "https://example.com"
        dependencies:
          - name: openssl
          - name: libgit2
            os: mac
          - name: zlib
            type: optional
        conflicts:
          - other-tool
          - old-tool
        caveats: "Run `tool init` after installing."
        skip_upload: "auto"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let hb = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .homebrew
            .as_ref()
            .unwrap();
        assert_eq!(hb.homepage.as_deref(), Some("https://example.com"));
        assert_eq!(
            hb.skip_upload,
            Some(StringOrBool::String("auto".to_string()))
        );
        assert_eq!(
            hb.caveats.as_deref(),
            Some("Run `tool init` after installing.")
        );

        let conflicts = hb.conflicts.as_ref().unwrap();
        assert_eq!(
            conflicts,
            &[
                HomebrewConflict::Name("other-tool".to_string()),
                HomebrewConflict::Name("old-tool".to_string()),
            ]
        );

        let deps = hb.dependencies.as_ref().unwrap();
        assert_eq!(deps.len(), 3);
        assert_eq!(deps[0].name, "openssl");
        assert_eq!(deps[0].os, None);
        assert_eq!(deps[0].dep_type, None);
        assert_eq!(deps[1].name, "libgit2");
        assert_eq!(deps[1].os.as_deref(), Some("mac"));
        assert_eq!(deps[2].name, "zlib");
        assert_eq!(deps[2].dep_type.as_deref(), Some("optional"));
    }

    #[test]
    fn test_homebrew_config_defaults_when_new_fields_omitted() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      homebrew:
        tap:
          owner: myorg
          name: homebrew-tap
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let hb = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .homebrew
            .as_ref()
            .unwrap();
        assert!(hb.homepage.is_none());
        assert!(hb.dependencies.is_none());
        assert!(hb.conflicts.is_none());
        assert!(hb.caveats.is_none());
        assert!(hb.skip_upload.is_none());
    }

    // ---- Scoop new fields parsing tests ----

    #[test]
    fn test_scoop_config_new_fields() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      scoop:
        bucket:
          owner: myorg
          name: scoop-bucket
        homepage: "https://example.com"
        persist:
          - data
          - config.ini
        depends:
          - git
          - 7zip
        pre_install:
          - "Write-Host 'Installing...'"
        post_install:
          - "Write-Host 'Done!'"
        shortcuts:
          - ["myapp.exe", "My App"]
          - ["myapp.exe", "My App CLI", "--cli"]
        skip_upload: "true"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let sc = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .scoop
            .as_ref()
            .unwrap();
        assert_eq!(sc.homepage.as_deref(), Some("https://example.com"));
        assert_eq!(
            sc.skip_upload,
            Some(StringOrBool::String("true".to_string()))
        );

        let persist = sc.persist.as_ref().unwrap();
        assert_eq!(persist, &["data", "config.ini"]);

        let depends = sc.depends.as_ref().unwrap();
        assert_eq!(depends, &["git", "7zip"]);

        let pre = sc.pre_install.as_ref().unwrap();
        assert_eq!(pre, &["Write-Host 'Installing...'"]);

        let post = sc.post_install.as_ref().unwrap();
        assert_eq!(post, &["Write-Host 'Done!'"]);

        let shortcuts = sc.shortcuts.as_ref().unwrap();
        assert_eq!(shortcuts.len(), 2);
        assert_eq!(shortcuts[0], vec!["myapp.exe", "My App"]);
        assert_eq!(shortcuts[1], vec!["myapp.exe", "My App CLI", "--cli"]);
    }

    #[test]
    fn test_scoop_config_defaults_when_new_fields_omitted() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      scoop:
        bucket:
          owner: myorg
          name: scoop-bucket
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let sc = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .scoop
            .as_ref()
            .unwrap();
        assert!(sc.homepage.is_none());
        assert!(sc.persist.is_none());
        assert!(sc.depends.is_none());
        assert!(sc.pre_install.is_none());
        assert!(sc.post_install.is_none());
        assert!(sc.shortcuts.is_none());
        assert!(sc.skip_upload.is_none());
    }

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
        let git = config.git.expect("git section should be present");
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
        let git = config.git.expect("git section should be present");
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
        assert!(err.contains("alphabetical"), "error should contain the bad value: {}", err);
        assert!(err.contains("-version:refname"), "error should list accepted values: {}", err);
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
        assert_eq!(
            meta.mod_timestamp.unwrap(),
            "{{ .CommitTimestamp }}"
        );
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
      - disable: true
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let snap = &config.crates[0].snapcrafts.as_ref().unwrap()[0];
        assert_eq!(snap.disable, Some(StringOrBool::Bool(true)));
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
      - disable: false
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let snap = &config.crates[0].snapcrafts.as_ref().unwrap()[0];
        assert_eq!(snap.disable, Some(StringOrBool::Bool(false)));
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
      - disable: "{{ if .IsSnapshot }}true{{ end }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let snap = &config.crates[0].snapcrafts.as_ref().unwrap()[0];
        match &snap.disable {
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
        assert!(snap.disable.is_none());
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
        disable: true
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
        assert_eq!(aur.disable, Some(StringOrBool::Bool(true)));
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
        disable: "{{ if .IsSnapshot }}true{{ end }}"
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
        match &aur.disable {
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
        assert!(aur.disable.is_none());
    }

    // ---- PublisherConfig disable StringOrBool tests ----

    #[test]
    fn test_publisher_disable_bool_true() {
        let yaml = r#"
project_name: test
publishers:
  - cmd: "echo hello"
    disable: true
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let pub_cfg = &config.publishers.as_ref().unwrap()[0];
        assert_eq!(pub_cfg.disable, Some(StringOrBool::Bool(true)));
    }

    #[test]
    fn test_publisher_disable_template_string() {
        let yaml = r#"
project_name: test
publishers:
  - cmd: "echo hello"
    disable: "{{ if .IsSnapshot }}true{{ end }}"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let pub_cfg = &config.publishers.as_ref().unwrap()[0];
        match &pub_cfg.disable {
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
        assert!(pub_cfg.disable.is_none());
    }

    // ---- skip_upload StringOrBool tests for publisher configs ----

    #[test]
    fn test_homebrew_skip_upload_bool_true() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      homebrew:
        skip_upload: true
        tap:
          owner: org
          name: homebrew-tap
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let hb = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .homebrew
            .as_ref()
            .unwrap();
        assert_eq!(hb.skip_upload, Some(StringOrBool::Bool(true)));
    }

    #[test]
    fn test_scoop_skip_upload_bool_true() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      scoop:
        skip_upload: true
        bucket:
          owner: org
          name: scoop-bucket
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let sc = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .scoop
            .as_ref()
            .unwrap();
        assert_eq!(sc.skip_upload, Some(StringOrBool::Bool(true)));
    }

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
    fn test_winget_skip_upload_bool_true() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      winget:
        skip_upload: true
        manifests_repo:
          owner: org
          name: winget-pkgs
        package_identifier: "Org.App"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let wg = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .winget
            .as_ref()
            .unwrap();
        assert_eq!(wg.skip_upload, Some(StringOrBool::Bool(true)));
    }

    #[test]
    fn test_krew_skip_upload_auto_string() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      krew:
        skip_upload: "auto"
        manifests_repo:
          owner: org
          name: krew-index
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let krew = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .krew
            .as_ref()
            .unwrap();
        assert_eq!(
            krew.skip_upload,
            Some(StringOrBool::String("auto".to_string()))
        );
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

    #[test]
    fn test_skip_upload_string_or_bool() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      homebrew:
        name: test
        skip_upload: "{{ if .IsSnapshot }}true{{ endif }}"
        tap:
          owner: org
          name: homebrew-tap
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let hb = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .homebrew
            .as_ref()
            .unwrap();
        match &hb.skip_upload {
            Some(StringOrBool::String(s)) => {
                assert!(
                    s.contains(".IsSnapshot"),
                    "expected template with .IsSnapshot, got: {}",
                    s
                );
            }
            other => panic!(
                "expected StringOrBool::String with template, got {:?}",
                other
            ),
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
        assert!(matches!(&includes[1], IncludeSpec::FromFile { from_file } if from_file.path == "./config/shared.yaml"));
        assert!(matches!(&includes[2], IncludeSpec::FromUrl { from_url } if from_url.url == "https://example.com/config.yaml"));
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
                assert_eq!(
                    from_url.url,
                    "caarlos0/goreleaserfiles/main/packages.yml"
                );
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
        assert_eq!(cfg.api.as_deref(), Some("https://github.example.com/api/v3/"));
        assert_eq!(cfg.upload.as_deref(), Some("https://github.example.com/api/uploads/"));
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
        assert_eq!(cfg.api.as_deref(), Some("https://gitlab.example.com/api/v4/"));
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
        assert_eq!(cfg.api.as_deref(), Some("https://gitea.example.com/api/v1/"));
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
        assert_eq!(urls.upload.as_deref(), Some("https://ghe.corp.com/api/uploads/"));
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
    disable: true
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
        assert_eq!(dh.disable, Some(StringOrBool::Bool(true)));
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
    fn test_fury_config_parse() {
        let yaml = r#"
project_name: test
fury:
  - account: myaccount
    secret_name: FURY_TOKEN
    ids:
      - packages
    formats:
      - deb
      - rpm
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let fury = &cfg.fury.unwrap()[0];
        assert_eq!(fury.account.as_deref(), Some("myaccount"));
        assert_eq!(fury.secret_name.as_deref(), Some("FURY_TOKEN"));
        assert_eq!(fury.ids.as_ref().unwrap(), &["packages"]);
        assert_eq!(fury.formats.as_ref().unwrap(), &["deb", "rpm"]);
    }

    #[test]
    fn test_fury_gemfury_alias_parse() {
        let yaml = r#"
project_name: test
gemfury:
  - account: aliasaccount
    secret_name: GF_TOKEN
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let fury = &cfg.fury.unwrap()[0];
        assert_eq!(fury.account.as_deref(), Some("aliasaccount"));
        assert_eq!(fury.secret_name.as_deref(), Some("GF_TOKEN"));
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

    #[test]
    fn test_npm_config_parse() {
        let yaml = r#"
project_name: test
npms:
  - name: "@myorg/mypackage"
    description: "My CLI tool"
    license: MIT
    author: "Jane Doe <jane@example.com>"
    access: public
    tag: latest
    if: "{{ .IsSnapshot }}"
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let npm = &cfg.npms.unwrap()[0];
        assert_eq!(npm.name.as_deref(), Some("@myorg/mypackage"));
        assert_eq!(npm.description.as_deref(), Some("My CLI tool"));
        assert_eq!(npm.license.as_deref(), Some("MIT"));
        assert_eq!(
            npm.author.as_deref(),
            Some("Jane Doe <jane@example.com>")
        );
        assert_eq!(npm.access.as_deref(), Some("public"));
        assert_eq!(npm.tag.as_deref(), Some("latest"));
        assert_eq!(
            npm.if_condition.as_deref(),
            Some("{{ .IsSnapshot }}")
        );
    }

    // -----------------------------------------------------------------------
    // deserialize_env_map tests — map, list-of-strings, null/missing
    // -----------------------------------------------------------------------

    #[test]
    fn test_docker_sign_env_map_format() {
        let yaml = r#"
project_name: test
docker_signs:
  - cmd: cosign
    env:
      COSIGN_PASSWORD: hunter2
      COSIGN_KEY: /path/to/key
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let ds = &cfg.docker_signs.as_ref().unwrap()[0];
        let env = ds.env.as_ref().expect("env should be Some");
        assert_eq!(env.get("COSIGN_PASSWORD").unwrap(), "hunter2");
        assert_eq!(env.get("COSIGN_KEY").unwrap(), "/path/to/key");
    }

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
        assert_eq!(env.get("COSIGN_PASSWORD").unwrap(), "hunter2");
        assert_eq!(env.get("COSIGN_KEY").unwrap(), "/path/to/key");
    }

    #[test]
    fn test_docker_sign_env_list_split_on_first_equals() {
        let yaml = r#"
project_name: test
docker_signs:
  - cmd: cosign
    env:
      - FLAGS=--key=val --other=stuff
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let ds = &cfg.docker_signs.as_ref().unwrap()[0];
        let env = ds.env.as_ref().expect("env should be Some");
        assert_eq!(env.get("FLAGS").unwrap(), "--key=val --other=stuff");
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
    fn test_docker_sign_env_list_invalid_no_equals() {
        let yaml = r#"
project_name: test
docker_signs:
  - cmd: cosign
    env:
      - COSIGN_PASSWORD
"#;
        let result = serde_yaml_ng::from_str::<Config>(yaml);
        assert!(result.is_err(), "entry without '=' should fail");
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
        assert_eq!(env.get("GPG_KEY").unwrap(), "ABCDEF");
        assert_eq!(env.get("GPG_TTY").unwrap(), "/dev/pts/0");
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
        assert_eq!(env.get("API_TOKEN").unwrap(), "secret123");
    }

    // -----------------------------------------------------------------------
    // BuildOverride.env — list and map format tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_override_env_list_format() {
        let yaml = r#"
project_name: test
defaults:
  targets:
    - x86_64-unknown-linux-gnu
  overrides:
    - targets:
        - "x86_64-*"
      env:
        - CC=gcc-12
        - CFLAGS=-O2 -Wall
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let overrides = config.defaults.unwrap().overrides.unwrap();
        let env = overrides[0].env.as_ref().expect("env should be Some");
        assert_eq!(env.get("CC").unwrap(), "gcc-12");
        assert_eq!(env.get("CFLAGS").unwrap(), "-O2 -Wall");
    }

    #[test]
    fn test_build_override_env_map_format() {
        let yaml = r#"
project_name: test
defaults:
  targets:
    - x86_64-unknown-linux-gnu
  overrides:
    - targets:
        - "x86_64-*"
      env:
        CC: gcc-12
        CFLAGS: "-O2 -Wall"
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let overrides = config.defaults.unwrap().overrides.unwrap();
        let env = overrides[0].env.as_ref().expect("env should be Some");
        assert_eq!(env.get("CC").unwrap(), "gcc-12");
        assert_eq!(env.get("CFLAGS").unwrap(), "-O2 -Wall");
    }

    // -----------------------------------------------------------------------
    // StructuredHook.env — list and map format tests
    // -----------------------------------------------------------------------

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
        let hooks = cfg.before.as_ref().unwrap().pre.as_ref().unwrap();
        match &hooks[0] {
            HookEntry::Structured(h) => {
                let env = h.env.as_ref().expect("env should be Some");
                assert_eq!(env.get("MY_VAR").unwrap(), "foo");
                assert_eq!(env.get("OTHER").unwrap(), "bar=baz");
            }
            HookEntry::Simple(_) => panic!("expected Structured hook"),
        }
    }

    #[test]
    fn test_structured_hook_env_map_format() {
        let yaml = r#"
project_name: test
before:
  hooks:
    - cmd: echo hello
      env:
        MY_VAR: foo
        OTHER: "bar=baz"
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let hooks = cfg.before.as_ref().unwrap().pre.as_ref().unwrap();
        match &hooks[0] {
            HookEntry::Structured(h) => {
                let env = h.env.as_ref().expect("env should be Some");
                assert_eq!(env.get("MY_VAR").unwrap(), "foo");
                assert_eq!(env.get("OTHER").unwrap(), "bar=baz");
            }
            HookEntry::Simple(_) => panic!("expected Structured hook"),
        }
    }

    // -----------------------------------------------------------------------
    // SignConfig.env — map format test (list already covered above)
    // -----------------------------------------------------------------------

    #[test]
    fn test_sign_config_env_map_format() {
        let yaml = r#"
project_name: test
signs:
  - cmd: gpg
    env:
      GPG_KEY: ABCDEF
      GPG_TTY: /dev/pts/0
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let s = &cfg.signs[0];
        let env = s.env.as_ref().expect("env should be Some");
        assert_eq!(env.get("GPG_KEY").unwrap(), "ABCDEF");
        assert_eq!(env.get("GPG_TTY").unwrap(), "/dev/pts/0");
    }

    // -----------------------------------------------------------------------
    // PublisherConfig.env — map format test (list already covered above)
    // -----------------------------------------------------------------------

    #[test]
    fn test_publisher_env_map_format() {
        let yaml = r#"
project_name: test
publishers:
  - name: mypub
    cmd: publish.sh
    env:
      API_TOKEN: secret123
      DEPLOY_ENV: staging
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let p = &cfg.publishers.as_ref().unwrap()[0];
        let env = p.env.as_ref().expect("env should be Some");
        assert_eq!(env.get("API_TOKEN").unwrap(), "secret123");
        assert_eq!(env.get("DEPLOY_ENV").unwrap(), "staging");
    }

    // -----------------------------------------------------------------------
    // SbomConfig.env — list and map format tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_sbom_config_env_map_format() {
        let yaml = r#"
project_name: test
sboms:
  - cmd: syft
    env:
      SYFT_FILE_METADATA_CATALOGER_ENABLED: "true"
      SYFT_SCOPE: all-layers
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let s = &cfg.sboms[0];
        let env = s.env.as_ref().expect("env should be Some");
        assert_eq!(env.get("SYFT_FILE_METADATA_CATALOGER_ENABLED").unwrap(), "true");
        assert_eq!(env.get("SYFT_SCOPE").unwrap(), "all-layers");
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
        assert_eq!(env.get("SYFT_FILE_METADATA_CATALOGER_ENABLED").unwrap(), "true");
        assert_eq!(env.get("SYFT_SCOPE").unwrap(), "all-layers");
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
}
