use std::collections::HashMap;
use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

// ---------------------------------------------------------------------------
// Top-level config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Config {
    /// Schema version. Currently supports 1 (implicit default) and 2.
    pub version: Option<u32>,
    pub project_name: String,
    #[serde(default = "default_dist")]
    pub dist: PathBuf,
    pub includes: Option<Vec<String>>,
    /// List of .env files to load before template expansion.
    pub env_files: Option<Vec<String>>,
    pub defaults: Option<Defaults>,
    pub before: Option<HooksConfig>,
    pub after: Option<HooksConfig>,
    pub crates: Vec<CrateConfig>,
    pub changelog: Option<ChangelogConfig>,
    #[serde(default, alias = "sign", deserialize_with = "deserialize_signs")]
    #[schemars(schema_with = "signs_schema")]
    pub signs: Vec<SignConfig>,
    pub docker_signs: Option<Vec<DockerSignConfig>>,
    // No `alias` attribute needed: unlike `signs`/`sign`, "upx" is already
    // both singular and plural, so a separate alias adds no value.
    #[serde(default, deserialize_with = "deserialize_upx")]
    #[schemars(schema_with = "upx_schema")]
    pub upx: Vec<UpxConfig>,
    pub snapshot: Option<SnapshotConfig>,
    pub nightly: Option<NightlyConfig>,
    pub announce: Option<AnnounceConfig>,
    pub report_sizes: Option<bool>,
    pub env: Option<HashMap<String, String>>,
    pub publishers: Option<Vec<PublisherConfig>>,
    pub tag: Option<TagConfig>,
    pub workspaces: Option<Vec<WorkspaceConfig>>,
    pub source: Option<SourceConfig>,
    pub sbom: Option<SbomConfig>,
}

/// Helper schema function for the signs field (accepts object or array).
fn signs_schema(generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
    generator.subschema_for::<Vec<SignConfig>>()
}

/// Helper schema function for the upx field (accepts object or array).
fn upx_schema(generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
    generator.subschema_for::<Vec<UpxConfig>>()
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
            docker_signs: None,
            upx: Vec::new(),
            snapshot: None,
            nightly: None,
            announce: None,
            report_sizes: None,
            env: None,
            publishers: None,
            tag: None,
            workspaces: None,
            source: None,
            sbom: None,
        }
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

/// Load environment variables from .env-style files.
/// Each file is read as KEY=VALUE lines. Lines starting with # and empty lines are skipped.
pub fn load_env_files(files: &[String]) -> Result<(), String> {
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
                    eprintln!(
                        "[env] warning: skipping line with empty key in '{}': {}",
                        file_path, line.trim()
                    );
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
                // SAFETY: set_var is unsafe since Rust 1.66 in multi-threaded contexts,
                // but we call this early before spawning threads.
                unsafe {
                    std::env::set_var(key, value);
                }
            } else {
                eprintln!(
                    "[env] warning: skipping line without '=' in '{}': {}",
                    file_path, trimmed
                );
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct Defaults {
    pub targets: Option<Vec<String>>,
    pub cross: Option<CrossStrategy>,
    pub flags: Option<String>,
    pub archives: Option<DefaultArchiveConfig>,
    pub checksum: Option<ChecksumConfig>,
    /// Exclude specific os/arch combinations from builds.
    pub ignore: Option<Vec<BuildIgnore>>,
    /// Per-target overrides for env, flags, and features.
    pub overrides: Option<Vec<BuildOverride>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DefaultArchiveConfig {
    pub format: Option<String>,
    pub format_overrides: Option<Vec<FormatOverride>>,
}

// ---------------------------------------------------------------------------
// BuildIgnore — exclude specific os/arch combos from builds
// ---------------------------------------------------------------------------

/// Exclude a specific os/arch combination from the build matrix.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BuildIgnore {
    pub os: String,
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
    pub name: String,
    pub path: String,
    pub tag_template: String,
    pub depends_on: Option<Vec<String>>,
    pub builds: Option<Vec<BuildConfig>>,
    pub cross: Option<CrossStrategy>,
    #[serde(default, deserialize_with = "deserialize_archives_config")]
    #[schemars(schema_with = "archives_schema")]
    pub archives: ArchivesConfig,
    pub checksum: Option<ChecksumConfig>,
    pub release: Option<ReleaseConfig>,
    pub publish: Option<PublishConfig>,
    pub docker: Option<Vec<DockerConfig>>,
    pub nfpm: Option<Vec<NfpmConfig>>,
    pub binstall: Option<BinstallConfig>,
    pub version_sync: Option<VersionSyncConfig>,
    pub universal_binaries: Option<Vec<UniversalBinaryConfig>>,
}

/// Helper schema function for archives (accepts false or array).
fn archives_schema(generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
    generator.subschema_for::<Option<Vec<ArchiveConfig>>>()
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
            nfpm: None,
            binstall: None,
            version_sync: None,
            universal_binaries: None,
        }
    }
}

// ---------------------------------------------------------------------------
// UniversalBinaryConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct UniversalBinaryConfig {
    pub name_template: Option<String>,
    pub replace: Option<bool>,
    pub ids: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// BuildConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct BuildConfig {
    pub binary: String,
    pub targets: Option<Vec<String>>,
    pub features: Option<Vec<String>>,
    pub no_default_features: Option<bool>,
    pub env: Option<HashMap<String, HashMap<String, String>>>,
    pub copy_from: Option<String>,
    pub flags: Option<String>,
    pub reproducible: Option<bool>,
}

// ---------------------------------------------------------------------------
// ArchivesConfig — untagged enum: false => Disabled, array => Configs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub enum ArchivesConfig {
    Disabled,
    Configs(Vec<ArchiveConfig>),
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
// ArchiveConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ArchiveConfig {
    pub name_template: Option<String>,
    pub format: Option<String>,
    pub format_overrides: Option<Vec<FormatOverride>>,
    pub files: Option<Vec<String>>,
    pub binaries: Option<Vec<String>>,
    pub wrap_in_directory: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FormatOverride {
    pub os: String,
    pub format: String,
}

// ---------------------------------------------------------------------------
// ChecksumConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ChecksumConfig {
    pub name_template: Option<String>,
    pub algorithm: Option<String>,
    pub disable: Option<bool>,
    pub extra_files: Option<Vec<String>>,
    pub ids: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// ReleaseConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ReleaseConfig {
    pub github: Option<GitHubConfig>,
    pub draft: Option<bool>,
    #[schemars(schema_with = "prerelease_schema")]
    pub prerelease: Option<PrereleaseConfig>,
    #[schemars(schema_with = "make_latest_schema")]
    pub make_latest: Option<MakeLatestConfig>,
    pub name_template: Option<String>,
    pub header: Option<String>,
    pub footer: Option<String>,
    pub extra_files: Option<Vec<String>>,
    pub skip_upload: Option<bool>,
    pub replace_existing_draft: Option<bool>,
    pub replace_existing_artifacts: Option<bool>,
}

/// Schema for prerelease: "auto" or boolean.
fn prerelease_schema(_generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
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
fn make_latest_schema(_generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
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
pub struct GitHubConfig {
    pub owner: String,
    pub name: String,
}

/// `prerelease` can be the string `"auto"` or a boolean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrereleaseConfig {
    Auto,
    Bool(bool),
}

impl Serialize for PrereleaseConfig {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        match self {
            PrereleaseConfig::Auto => serializer.serialize_str("auto"),
            PrereleaseConfig::Bool(b) => serializer.serialize_bool(*b),
        }
    }
}

impl<'de> Deserialize<'de> for PrereleaseConfig {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct PrereleaseVisitor;
        impl serde::de::Visitor<'_> for PrereleaseVisitor {
            type Value = PrereleaseConfig;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "\"auto\" or a boolean")
            }
            fn visit_bool<E: serde::de::Error>(
                self,
                v: bool,
            ) -> std::result::Result<PrereleaseConfig, E> {
                Ok(PrereleaseConfig::Bool(v))
            }
            fn visit_str<E: serde::de::Error>(
                self,
                v: &str,
            ) -> std::result::Result<PrereleaseConfig, E> {
                if v == "auto" {
                    Ok(PrereleaseConfig::Auto)
                } else {
                    Err(E::custom(format!("expected \"auto\", got \"{}\"", v)))
                }
            }
        }
        deserializer.deserialize_any(PrereleaseVisitor)
    }
}

/// `make_latest` can be the string `"auto"` or a boolean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MakeLatestConfig {
    Auto,
    Bool(bool),
}

impl Serialize for MakeLatestConfig {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        match self {
            MakeLatestConfig::Auto => serializer.serialize_str("auto"),
            MakeLatestConfig::Bool(b) => serializer.serialize_bool(*b),
        }
    }
}

impl<'de> Deserialize<'de> for MakeLatestConfig {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct MakeLatestVisitor;
        impl serde::de::Visitor<'_> for MakeLatestVisitor {
            type Value = MakeLatestConfig;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "\"auto\" or a boolean")
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
                if v == "auto" {
                    Ok(MakeLatestConfig::Auto)
                } else {
                    Err(E::custom(format!("expected \"auto\", got \"{}\"", v)))
                }
            }
        }
        deserializer.deserialize_any(MakeLatestVisitor)
    }
}

// ---------------------------------------------------------------------------
// PublishConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct PublishConfig {
    #[schemars(schema_with = "crates_publish_schema")]
    pub crates: Option<CratesPublishConfig>,
    pub homebrew: Option<HomebrewConfig>,
    pub scoop: Option<ScoopConfig>,
    pub chocolatey: Option<ChocolateyConfig>,
    pub winget: Option<WingetConfig>,
    pub aur: Option<AurConfig>,
    pub krew: Option<KrewConfig>,
}

/// Schema for crates publish config (bool or object).
fn crates_publish_schema(_generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
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
    pub tap: Option<TapConfig>,
    pub folder: Option<String>,
    pub description: Option<String>,
    pub license: Option<String>,
    pub install: Option<String>,
    pub test: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ScoopConfig {
    pub bucket: Option<BucketConfig>,
    pub description: Option<String>,
    pub license: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TapConfig {
    pub owner: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BucketConfig {
    pub owner: String,
    pub name: String,
}

// ---------------------------------------------------------------------------
// ChocolateyConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ChocolateyConfig {
    /// The GitHub project repo (owner/name). Used to derive download URLs
    /// and project metadata.
    #[serde(alias = "source_repo")]
    pub project_repo: Option<ChocolateyRepoConfig>,
    pub description: Option<String>,
    pub license: Option<String>,
    /// Optional explicit license URL. Falls back to
    /// `https://opensource.org/licenses/<license>` when not set.
    pub license_url: Option<String>,
    pub tags: Option<Vec<String>>,
    pub authors: Option<String>,
    pub project_url: Option<String>,
    pub icon_url: Option<String>,
    /// Chocolatey API key for `choco push`. If not set, falls back to the
    /// `CHOCOLATEY_API_KEY` environment variable.
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ChocolateyRepoConfig {
    pub owner: String,
    pub name: String,
}

// ---------------------------------------------------------------------------
// WingetConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct WingetConfig {
    pub manifests_repo: Option<WingetManifestsRepoConfig>,
    pub description: Option<String>,
    pub license: Option<String>,
    pub package_identifier: Option<String>,
    pub publisher: Option<String>,
    pub publisher_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WingetManifestsRepoConfig {
    pub owner: String,
    pub name: String,
}

// ---------------------------------------------------------------------------
// AurConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct AurConfig {
    /// AUR SSH git URL (e.g., `ssh://aur@aur.archlinux.org/<package>.git`).
    ///
    /// Required for publishing. The field is `Option` for serde compatibility
    /// (omitted in config means "no AUR publishing"), but `publish_to_aur`
    /// will return an error if it is `None` at publish time.
    pub git_url: Option<String>,
    pub package_name: Option<String>,
    pub description: Option<String>,
    pub license: Option<String>,
    pub maintainers: Option<Vec<String>>,
    pub depends: Option<Vec<String>>,
    pub optdepends: Option<Vec<String>>,
    pub conflicts: Option<Vec<String>>,
    pub provides: Option<Vec<String>>,
    pub replaces: Option<Vec<String>>,
    /// List of config files to preserve on upgrade (relative to `/`).
    pub backup: Option<Vec<String>>,
    pub url: Option<String>,
    /// Custom install template for the PKGBUILD `package()` function.
    ///
    /// When omitted, defaults to:
    /// ```text
    /// install -Dm755 "$srcdir/<binary>" "$pkgdir/usr/bin/<binary>"
    /// ```
    ///
    /// Use this when the archive has a subdirectory structure, e.g.:
    /// ```text
    /// install -Dm755 "$srcdir/<binary>-${pkgver}/<binary>" "$pkgdir/usr/bin/<binary>"
    /// ```
    pub install_template: Option<String>,
}

// ---------------------------------------------------------------------------
// KrewConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct KrewConfig {
    /// The krew-index fork repo (owner/name) to which the plugin manifest is
    /// submitted, similar to winget's `manifests_repo`.
    pub manifests_repo: Option<KrewManifestsRepoConfig>,
    pub description: Option<String>,
    pub short_description: Option<String>,
    pub homepage: Option<String>,
    /// Post-install message shown to the user.
    pub caveats: Option<String>,
    /// The upstream repo to submit the PR against (e.g. `kubernetes-sigs/krew-index`).
    /// When omitted, defaults to `manifests_repo` owner/name, which is the fork.
    /// Set this when your `manifests_repo` is a fork and PRs should target the upstream.
    pub upstream_repo: Option<KrewManifestsRepoConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct KrewManifestsRepoConfig {
    pub owner: String,
    pub name: String,
}

// ---------------------------------------------------------------------------
// DockerConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DockerConfig {
    pub image_templates: Vec<String>,
    pub dockerfile: String,
    pub platforms: Option<Vec<String>>,
    pub binaries: Option<Vec<String>>,
    pub build_flag_templates: Option<Vec<String>>,
    pub skip_push: Option<bool>,
    pub extra_files: Option<Vec<String>>,
    pub push_flags: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// NfpmConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NfpmConfig {
    pub package_name: Option<String>,
    pub formats: Vec<String>,
    pub vendor: Option<String>,
    pub homepage: Option<String>,
    pub maintainer: Option<String>,
    pub description: Option<String>,
    pub license: Option<String>,
    pub bindir: Option<String>,
    pub contents: Option<Vec<NfpmContent>>,
    pub dependencies: Option<HashMap<String, Vec<String>>>,
    pub overrides: Option<HashMap<String, serde_json::Value>>,
    pub file_name_template: Option<String>,
    pub scripts: Option<NfpmScripts>,
    pub recommends: Option<Vec<String>>,
    pub suggests: Option<Vec<String>>,
    pub conflicts: Option<Vec<String>>,
    pub replaces: Option<Vec<String>>,
    pub provides: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NfpmScripts {
    pub preinstall: Option<String>,
    pub postinstall: Option<String>,
    pub preremove: Option<String>,
    pub postremove: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NfpmFileInfo {
    pub owner: Option<String>,
    pub group: Option<String>,
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NfpmContent {
    pub src: String,
    pub dst: String,
    #[serde(rename = "type")]
    pub content_type: Option<String>,
    pub file_info: Option<NfpmFileInfo>,
}

// ---------------------------------------------------------------------------
// BinstallConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct BinstallConfig {
    pub enabled: Option<bool>,
    pub pkg_url: Option<String>,
    pub bin_dir: Option<String>,
    pub pkg_fmt: Option<String>,
}

// ---------------------------------------------------------------------------
// SourceConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct SourceConfig {
    pub enabled: Option<bool>,
    pub format: Option<String>,
    pub name_template: Option<String>,
    pub files: Option<Vec<String>>,
}

impl SourceConfig {
    /// Whether source archive generation is enabled (default: false).
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }

    /// The archive format to use (default: "tar.gz").
    pub fn archive_format(&self) -> &str {
        self.format.as_deref().unwrap_or("tar.gz")
    }
}

// ---------------------------------------------------------------------------
// SbomConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct SbomConfig {
    pub enabled: Option<bool>,
    pub format: Option<String>,
}

impl SbomConfig {
    /// Whether SBOM generation is enabled (default: false).
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }

    /// The SBOM format to use (default: "cyclonedx"). Also supports "spdx".
    pub fn sbom_format(&self) -> &str {
        self.format.as_deref().unwrap_or("cyclonedx")
    }
}

// ---------------------------------------------------------------------------
// VersionSyncConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct VersionSyncConfig {
    pub enabled: Option<bool>,
    pub mode: Option<String>,
}

// ---------------------------------------------------------------------------
// ChangelogConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ChangelogConfig {
    pub sort: Option<String>,
    pub filters: Option<ChangelogFilters>,
    pub groups: Option<Vec<ChangelogGroup>>,
    pub header: Option<String>,
    pub footer: Option<String>,
    pub disable: Option<bool>,
    /// Changelog source: `"git"` (default) or `"github-native"`.
    #[serde(rename = "use")]
    pub use_source: Option<String>,
    /// Hash abbreviation length (default 7).
    pub abbrev: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ChangelogFilters {
    pub exclude: Option<Vec<String>>,
    pub include: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ChangelogGroup {
    pub title: String,
    pub regexp: Option<String>,
    pub order: Option<i32>,
}

// ---------------------------------------------------------------------------
// SignConfig / DockerSignConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct SignConfig {
    pub id: Option<String>,
    pub artifacts: Option<String>,
    pub cmd: Option<String>,
    pub args: Option<Vec<String>>,
    pub signature: Option<String>,
    pub stdin: Option<String>,
    pub stdin_file: Option<String>,
    pub ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DockerSignConfig {
    pub artifacts: Option<String>,
    pub cmd: Option<String>,
    pub args: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// UpxConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct UpxConfig {
    pub id: Option<String>,
    pub ids: Option<Vec<String>>,
    pub enabled: bool,
    pub binary: String,
    pub args: Vec<String>,
    pub required: bool,
    pub targets: Option<Vec<String>>,
}

impl Default for UpxConfig {
    fn default() -> Self {
        UpxConfig {
            id: None,
            ids: None,
            enabled: true,
            binary: "upx".to_string(),
            args: Vec::new(),
            required: false,
            targets: None,
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
    /// Tag name used for the nightly release. Default: "nightly"
    pub tag_name: Option<String>,
}

// ---------------------------------------------------------------------------
// AnnounceConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct AnnounceConfig {
    pub discord: Option<AnnounceProviderConfig>,
    pub slack: Option<AnnounceProviderConfig>,
    pub webhook: Option<WebhookConfig>,
    pub telegram: Option<TelegramAnnounce>,
    pub teams: Option<TeamsAnnounce>,
    pub mattermost: Option<MattermostAnnounce>,
    pub email: Option<EmailAnnounce>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct AnnounceProviderConfig {
    pub enabled: Option<bool>,
    pub webhook_url: Option<String>,
    pub message_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct WebhookConfig {
    pub enabled: Option<bool>,
    pub endpoint_url: Option<String>,
    pub headers: Option<HashMap<String, String>>,
    pub content_type: Option<String>,
    pub message_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct TelegramAnnounce {
    pub enabled: Option<bool>,
    pub bot_token: Option<String>,
    pub chat_id: Option<String>,
    pub message_template: Option<String>,
    /// Optional parse mode: "MarkdownV2" or "HTML"
    pub parse_mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct TeamsAnnounce {
    pub enabled: Option<bool>,
    pub webhook_url: Option<String>,
    pub message_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct MattermostAnnounce {
    pub enabled: Option<bool>,
    pub webhook_url: Option<String>,
    /// Optional channel override (e.g. "town-square")
    pub channel: Option<String>,
    /// Optional username override for the bot post
    pub username: Option<String>,
    /// Optional icon URL for the bot post
    pub icon_url: Option<String>,
    pub message_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct EmailAnnounce {
    pub enabled: Option<bool>,
    pub from: Option<String>,
    #[serde(default)]
    pub to: Vec<String>,
    pub subject_template: Option<String>,
    pub message_template: Option<String>,
}

// ---------------------------------------------------------------------------
// PublisherConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct PublisherConfig {
    pub name: Option<String>,
    pub cmd: String,
    pub args: Option<Vec<String>>,
    pub ids: Option<Vec<String>>,
    pub artifact_types: Option<Vec<String>>,
    pub env: Option<HashMap<String, String>>,
}

// ---------------------------------------------------------------------------
// HooksConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct HooksConfig {
    pub hooks: Vec<String>,
}

// ---------------------------------------------------------------------------
// TagConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct TagConfig {
    pub default_bump: Option<String>,
    pub tag_prefix: Option<String>,
    pub release_branches: Option<Vec<String>>,
    pub custom_tag: Option<String>,
    pub tag_context: Option<String>,
    pub branch_history: Option<String>,
    pub initial_version: Option<String>,
    pub prerelease: Option<bool>,
    pub prerelease_suffix: Option<String>,
    pub force_without_changes: Option<bool>,
    pub force_without_changes_pre: Option<bool>,
    pub major_string_token: Option<String>,
    pub minor_string_token: Option<String>,
    pub patch_string_token: Option<String>,
    pub none_string_token: Option<String>,
    pub git_api_tagging: Option<bool>,
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
    pub name: String,
    pub crates: Vec<CrateConfig>,
    pub changelog: Option<ChangelogConfig>,
    #[serde(default, alias = "sign", deserialize_with = "deserialize_signs")]
    #[schemars(schema_with = "signs_schema")]
    pub signs: Vec<SignConfig>,
    pub before: Option<HooksConfig>,
    pub after: Option<HooksConfig>,
    pub env: Option<HashMap<String, String>>,
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
    fn test_make_latest_invalid_string() {
        let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      make_latest: "bogus"
"#;
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(result.is_err());
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
        assert_eq!(cl.disable, Some(true));
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
        assert_eq!(cl.disable, Some(false));
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
        assert_eq!(checksum.disable, Some(true));
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
        assert_eq!(checksum.disable, Some(true));
        assert_eq!(checksum.algorithm, Some("sha512".to_string()));
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
    }

    // ---- ReleaseConfig header/footer tests ----

    #[test]
    fn test_release_header_footer() {
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
        assert_eq!(release.header, Some("## Custom Header".to_string()));
        assert_eq!(release.footer, Some("---\nPowered by anodize".to_string()));
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
    fn test_release_extra_files() {
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
        assert_eq!(files[0], "dist/*.sig");
        assert_eq!(files[1], "CHANGELOG.md");
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
        assert_eq!(release.skip_upload, Some(true));
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
        assert_eq!(release.skip_upload, Some(false));
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
"##;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let release = config.crates[0].release.as_ref().unwrap();
        assert_eq!(release.header, Some("# Release Notes".to_string()));
        assert_eq!(release.footer, Some("Thank you!".to_string()));
        assert_eq!(release.extra_files.as_ref().unwrap(), &["dist/extra.zip"]);
        assert_eq!(release.skip_upload, Some(false));
        assert_eq!(release.replace_existing_draft, Some(true));
        assert_eq!(release.replace_existing_artifacts, Some(false));
        assert_eq!(release.make_latest, Some(MakeLatestConfig::Auto));
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
        source_repo:
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
        source_repo:
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

[crates.publish.chocolatey.source_repo]
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
        assert_eq!(
            winget.package_identifier,
            Some("MyOrg.MyTool".to_string())
        );
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
        assert_eq!(
            winget.package_identifier,
            Some("MyOrg.MyTool".to_string())
        );
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
        assert_eq!(
            winget.package_identifier,
            Some("Org.Tool".to_string())
        );
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
        assert_eq!(aur.package_name, Some("mytool-bin".to_string()));
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
        assert_eq!(
            aur.backup,
            Some(vec!["etc/mytool/config.toml".to_string()])
        );
        assert_eq!(
            aur.url,
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
        assert!(aur.package_name.is_none());
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
        assert_eq!(
            krew.short_description,
            Some("A kubectl plugin".to_string())
        );
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

        assert_eq!(
            krew.short_description,
            Some("A kubectl plugin".to_string())
        );
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
        source_repo:
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
        assert!(result.unwrap_err().contains("unsupported config version: 99"));
    }

    // ---- env_files tests ----

    #[test]
    fn test_env_files_field_parses() {
        let yaml = r#"
project_name: test
env_files:
  - ".env"
  - ".release.env"
crates: []
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let files = config.env_files.unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0], ".env");
        assert_eq!(files[1], ".release.env");
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
    fn test_load_env_files_sets_vars() {
        use std::io::Write;
        let dir = tempfile::TempDir::new().unwrap();
        let env_path = dir.path().join(".env");
        let mut f = std::fs::File::create(&env_path).unwrap();
        writeln!(f, "# comment line").unwrap();
        writeln!(f, "").unwrap();
        writeln!(f, "TEST_ANODIZE_KEY=hello_world").unwrap();
        writeln!(f, "TEST_ANODIZE_QUOTED=\"with quotes\"").unwrap();
        writeln!(f, "TEST_ANODIZE_SINGLE='single_quoted'").unwrap();
        writeln!(f, "export TEST_ANODIZE_EXPORT=exported_val").unwrap();
        drop(f);

        load_env_files(&[env_path.to_string_lossy().to_string()]).unwrap();
        assert_eq!(std::env::var("TEST_ANODIZE_KEY").unwrap(), "hello_world");
        assert_eq!(std::env::var("TEST_ANODIZE_QUOTED").unwrap(), "with quotes");
        assert_eq!(
            std::env::var("TEST_ANODIZE_SINGLE").unwrap(),
            "single_quoted",
            "single-quoted values should have quotes stripped"
        );
        assert_eq!(
            std::env::var("TEST_ANODIZE_EXPORT").unwrap(),
            "exported_val",
            "export prefix should be stripped"
        );

        // Clean up
        unsafe {
            std::env::remove_var("TEST_ANODIZE_KEY");
            std::env::remove_var("TEST_ANODIZE_QUOTED");
            std::env::remove_var("TEST_ANODIZE_SINGLE");
            std::env::remove_var("TEST_ANODIZE_EXPORT");
        }
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

        load_env_files(&[env_path.to_string_lossy().to_string()]).unwrap();
        // The single-quote value should be kept as-is (not stripped, length < 2 for
        // matching quotes)
        assert_eq!(std::env::var("TEST_ANODIZE_SINGLEQ").unwrap(), "\"");

        // Clean up
        unsafe {
            std::env::remove_var("TEST_ANODIZE_SINGLEQ");
        }
    }

    #[test]
    fn test_load_env_files_nonexistent_returns_error() {
        let result = load_env_files(&["/tmp/nonexistent_anodize_env_file_12345".to_string()]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("failed to read env file"));
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
        assert_eq!(
            overrides[0].env.as_ref().unwrap().get("CC").unwrap(),
            "gcc"
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
}
