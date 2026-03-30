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
    /// Binary-specific signing configs (same shape as `signs` but only for binary artifacts).
    #[serde(default, alias = "binary_sign", deserialize_with = "deserialize_signs")]
    #[schemars(schema_with = "signs_schema")]
    pub binary_signs: Vec<SignConfig>,
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
    pub partial: Option<PartialConfig>,
    pub workspaces: Option<Vec<WorkspaceConfig>>,
    pub source: Option<SourceConfig>,
    pub sbom: Option<SbomConfig>,
    pub release: Option<ReleaseConfig>,
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
            binary_signs: Vec::new(),
            docker_signs: None,
            upx: Vec::new(),
            snapshot: None,
            nightly: None,
            announce: None,
            report_sizes: None,
            env: None,
            publishers: None,
            tag: None,
            partial: None,
            workspaces: None,
            source: None,
            sbom: None,
            release: None,
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
    pub docker_manifests: Option<Vec<DockerManifestConfig>>,
    pub nfpm: Option<Vec<NfpmConfig>>,
    pub snapcrafts: Option<Vec<SnapcraftConfig>>,
    pub dmgs: Option<Vec<DmgConfig>>,
    pub msis: Option<Vec<MsiConfig>>,
    pub pkgs: Option<Vec<PkgConfig>>,
    pub blobs: Option<Vec<BlobConfig>>,
    pub binstall: Option<BinstallConfig>,
    pub version_sync: Option<VersionSyncConfig>,
    pub universal_binaries: Option<Vec<UniversalBinaryConfig>>,
    /// When true, all build outputs are placed in a flat `dist/` directory
    /// instead of `dist/{target}/`.
    pub no_unique_dist_dir: Option<bool>,
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
            docker_manifests: None,
            nfpm: None,
            snapcrafts: None,
            dmgs: None,
            msis: None,
            pkgs: None,
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
    pub id: Option<String>,
    pub binary: String,
    pub skip: Option<bool>,
    pub targets: Option<Vec<String>>,
    pub features: Option<Vec<String>>,
    pub no_default_features: Option<bool>,
    pub env: Option<HashMap<String, HashMap<String, String>>>,
    pub copy_from: Option<String>,
    pub flags: Option<String>,
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
    pub pre: Option<Vec<HookEntry>>,
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
// ArchiveConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ArchiveConfig {
    /// Unique identifier for cross-referencing this archive from other configs.
    pub id: Option<String>,
    pub name_template: Option<String>,
    pub format: Option<String>,
    /// Produce multiple archive formats per config (plural, in addition to singular `format`).
    pub formats: Option<Vec<String>>,
    pub format_overrides: Option<Vec<FormatOverride>>,
    pub files: Option<Vec<ArchiveFileSpec>>,
    pub binaries: Option<Vec<String>>,
    pub wrap_in_directory: Option<String>,
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
    pub os: String,
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
    pub owner: Option<String>,
    pub group: Option<String>,
    pub mode: Option<String>,
    pub mtime: Option<String>,
}

/// Backward-compatible alias for archive code.
pub type ArchiveFileInfo = FileInfo;

/// Parse an octal mode string into a `u32`, handling common YAML-friendly
/// representations: `"0755"`, `"0o755"`, `"0O755"`, `"755"`, and `"0"`.
pub fn parse_octal_mode(s: &str) -> Option<u32> {
    let cleaned = s.strip_prefix("0o").or_else(|| s.strip_prefix("0O")).unwrap_or(s);
    let cleaned = if cleaned.is_empty() { "0" } else { cleaned };
    u32::from_str_radix(cleaned, 8).ok()
}

/// The set of archive format strings recognised by the archive stage.
/// Used for early validation so typos are caught at config load time rather
/// than mid-pipeline.
pub const VALID_ARCHIVE_FORMATS: &[&str] = &[
    "tar.gz", "tgz", "tar.xz", "txz", "tar.zst", "tzst", "tar", "zip", "gz", "binary",
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

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ChecksumConfig {
    pub name_template: Option<String>,
    pub algorithm: Option<String>,
    /// Disable checksums. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub disable: Option<StringOrBool>,
    pub extra_files: Option<Vec<ExtraFileSpec>>,
    pub ids: Option<Vec<String>>,
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
    pub github: Option<GitHubConfig>,
    pub draft: Option<bool>,
    #[schemars(schema_with = "prerelease_schema")]
    pub prerelease: Option<PrereleaseConfig>,
    #[schemars(schema_with = "make_latest_schema")]
    pub make_latest: Option<MakeLatestConfig>,
    pub name_template: Option<String>,
    pub header: Option<ContentSource>,
    pub footer: Option<ContentSource>,
    pub extra_files: Option<Vec<ExtraFileSpec>>,
    pub skip_upload: Option<bool>,
    pub replace_existing_draft: Option<bool>,
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
fn skip_push_schema(
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

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GitHubConfig {
    pub owner: String,
    pub name: String,
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

/// `make_latest` can be the string `"auto"` or a boolean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MakeLatestConfig {
    Auto,
    Bool(bool),
}

impl_auto_or_bool_serde!(
    MakeLatestConfig,
    MakeLatestConfig::Auto,
    MakeLatestConfig::Bool
);

/// `skip_push` can be the string `"auto"` (skip for prereleases) or a boolean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipPushConfig {
    Auto,
    Bool(bool),
}

impl_auto_or_bool_serde!(
    SkipPushConfig,
    SkipPushConfig::Auto,
    SkipPushConfig::Bool
);

// ---------------------------------------------------------------------------
// Shared publisher config types: RepositoryConfig, CommitAuthorConfig
// ---------------------------------------------------------------------------

/// Shared repository configuration used by all git-based publishers
/// (Homebrew, Scoop, Winget, Krew, Nix). Equivalent to GoReleaser's `RepoRef`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct RepositoryConfig {
    pub owner: Option<String>,
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
    pub owner: Option<String>,
    pub name: Option<String>,
    pub branch: Option<String>,
}

/// Shared commit author configuration with optional GPG/SSH signing.
/// Equivalent to GoReleaser's `CommitAuthor`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct CommitAuthorConfig {
    pub name: Option<String>,
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
    pub crates: Option<CratesPublishConfig>,
    pub homebrew: Option<HomebrewConfig>,
    pub scoop: Option<ScoopConfig>,
    pub chocolatey: Option<ChocolateyConfig>,
    pub winget: Option<WingetConfig>,
    pub aur: Option<AurConfig>,
    pub krew: Option<KrewConfig>,
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
    pub description: Option<String>,
    pub license: Option<String>,
    pub install: Option<String>,
    /// Additional install commands appended after the main install block.
    pub extra_install: Option<String>,
    /// Post-install commands (separate `def post_install` block in formula).
    pub post_install: Option<String>,
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
    /// for prerelease versions.
    pub skip_upload: Option<String>,
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
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, JsonSchema)]
#[serde(default)]
pub struct HomebrewDependency {
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
    pub description: Option<String>,
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
    /// for prerelease versions.
    pub skip_upload: Option<String>,
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
    /// Override the package name (default: crate name).
    pub name: Option<String>,
    /// Build IDs filter: only include artifacts whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// The GitHub project repo (owner/name). Used to derive download URLs.
    pub project_repo: Option<ChocolateyRepoConfig>,
    /// URL shown as the package source in the Chocolatey gallery.
    pub package_source_url: Option<String>,
    /// Package owners (Chocolatey gallery user).
    pub owners: Option<String>,
    /// Package title (default: project name).
    pub title: Option<String>,
    pub authors: Option<String>,
    pub project_url: Option<String>,
    /// Custom URL template for download URLs (overrides release URL).
    pub url_template: Option<String>,
    pub icon_url: Option<String>,
    /// Copyright notice.
    pub copyright: Option<String>,
    pub description: Option<String>,
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
    #[serde(deserialize_with = "deserialize_space_separated_string_or_vec_opt", default)]
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
    /// Skip publishing. When true, only generates the package.
    pub skip_publish: Option<bool>,
    /// Artifact selection: "archive" (default), "msi", or "nsis".
    #[serde(rename = "use")]
    pub use_artifact: Option<String>,
}

/// Chocolatey package dependency with optional version constraint.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ChocolateyDependency {
    pub id: String,
    pub version: Option<String>,
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
    /// Override the package name (default: crate name).
    pub name: Option<String>,
    /// Package name as displayed (default: same as name).
    pub package_name: Option<String>,
    /// WinGet package identifier (e.g. "Publisher.AppName"). Auto-generated if empty.
    pub package_identifier: Option<String>,
    /// Publisher name (required).
    pub publisher: Option<String>,
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
    pub description: Option<String>,
    /// Project homepage URL.
    pub homepage: Option<String>,
    /// Custom URL template for download URLs (overrides release URL).
    pub url_template: Option<String>,
    /// Build IDs filter: only include artifacts whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Skip publishing. `"true"` always skips; `"auto"` skips for prereleases.
    pub skip_upload: Option<String>,
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
}

/// WinGet package dependency.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct WingetDependency {
    pub package_identifier: String,
    pub minimum_version: Option<String>,
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
    /// Override the package name (default: crate name + "-bin").
    #[serde(alias = "package_name")]
    pub name: Option<String>,
    /// Build IDs filter: only include artifacts whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Commit author with optional signing.
    pub commit_author: Option<CommitAuthorConfig>,
    /// Custom commit message template. Default: "Update to {{ version }}".
    pub commit_msg_template: Option<String>,
    pub description: Option<String>,
    /// Project homepage URL.
    pub homepage: Option<String>,
    pub license: Option<String>,
    /// Skip publishing. `"true"` always skips; `"auto"` skips for prereleases.
    pub skip_upload: Option<String>,
    /// Custom URL template for download URLs (overrides release URL).
    pub url_template: Option<String>,
    pub maintainers: Option<Vec<String>>,
    /// Contributors listed in PKGBUILD comments.
    pub contributors: Option<Vec<String>>,
    pub provides: Option<Vec<String>>,
    pub conflicts: Option<Vec<String>>,
    pub depends: Option<Vec<String>>,
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
    /// Disable this AUR config. Accepts bool or template string.
    pub disable: Option<String>,
    /// Content for a .install file (post-install/pre-remove scripts).
    pub install: Option<String>,
    /// Legacy project URL field.
    pub url: Option<String>,
    pub replaces: Option<Vec<String>>,
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
    pub description: Option<String>,
    pub short_description: Option<String>,
    pub homepage: Option<String>,
    /// Custom URL template for download URLs (overrides release URL).
    pub url_template: Option<String>,
    /// Post-install message shown to the user.
    pub caveats: Option<String>,
    /// Skip publishing. `"true"` always skips; `"auto"` skips for prereleases.
    pub skip_upload: Option<String>,
    /// Legacy upstream repo for PR target. Use `repository.pull_request.base` instead.
    pub upstream_repo: Option<KrewManifestsRepoConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct KrewManifestsRepoConfig {
    pub owner: String,
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
    pub skip_upload: Option<String>,
    /// Custom install commands (replaces auto-generated binary install).
    pub install: Option<String>,
    /// Additional install commands appended after the main install.
    pub extra_install: Option<String>,
    /// Post-install commands (postInstall phase).
    pub post_install: Option<String>,
    pub description: Option<String>,
    /// Project homepage URL.
    pub homepage: Option<String>,
    /// Nix license identifier (e.g. "mit", "asl20"). Validated against known licenses.
    pub license: Option<String>,
    /// Nix package dependencies with optional OS filtering.
    pub dependencies: Option<Vec<NixDependency>>,
    /// Nix formatter to run on the generated file: "alejandra" or "nixfmt".
    pub formatter: Option<String>,
}

/// Nix package dependency with optional OS restriction.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NixDependency {
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
    pub id: Option<String>,
    pub image_templates: Vec<String>,
    pub dockerfile: String,
    pub platforms: Option<Vec<String>>,
    pub binaries: Option<Vec<String>>,
    pub build_flag_templates: Option<Vec<String>>,
    /// Skip push: true, false, or "auto" (skip for prereleases).
    #[schemars(schema_with = "skip_push_schema")]
    pub skip_push: Option<SkipPushConfig>,
    pub extra_files: Option<Vec<String>>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DockerRetryConfig {
    pub attempts: Option<u32>,
    /// Duration string, e.g. "1s", "500ms".
    pub delay: Option<String>,
    /// Maximum delay between retries, e.g. "30s".
    pub max_delay: Option<String>,
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
}

// ---------------------------------------------------------------------------
// NfpmConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NfpmConfig {
    /// Unique identifier for cross-referencing this nFPM config.
    pub id: Option<String>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NfpmScripts {
    pub preinstall: Option<String>,
    pub postinstall: Option<String>,
    pub preremove: Option<String>,
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
    pub src: String,
    pub dst: String,
    #[serde(rename = "type")]
    pub content_type: Option<String>,
    pub file_info: Option<NfpmFileInfo>,
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
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NfpmDebTriggers {
    pub interest: Option<Vec<String>>,
    pub interest_await: Option<Vec<String>>,
    pub interest_noawait: Option<Vec<String>>,
    pub activate: Option<Vec<String>>,
    pub activate_await: Option<Vec<String>>,
    pub activate_noawait: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NfpmApkConfig {
    /// APK signing configuration.
    pub signature: Option<NfpmSignatureConfig>,
}

impl NfpmApkConfig {
    /// Returns `true` when every field is `None` — the YAML section would be
    /// empty and should be omitted.
    pub fn is_empty(&self) -> bool {
        self.signature.is_none()
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
    /// The snap package name in the store.
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
    /// Additional static files to bundle.
    pub extra_files: Option<Vec<String>>,
    /// Template for the output snap filename.
    pub name_template: Option<String>,
    /// Disable this snapcraft config.
    pub disable: Option<bool>,
    /// Remove source archives from artifacts, keeping only snap.
    pub replace: Option<bool>,
    /// Output timestamp for reproducible builds.
    pub mod_timestamp: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct SnapcraftApp {
    /// Command to run (relative to snap root).
    pub command: Option<String>,
    /// Daemon type: simple, forking, oneshot, notify.
    pub daemon: Option<String>,
    /// How to stop the daemon: sigterm, sigkill, etc.
    pub stop_mode: Option<String>,
    /// Interface plugs the app needs.
    pub plugs: Option<Vec<String>>,
    /// Environment variables for the app.
    pub environment: Option<HashMap<String, String>>,
    /// Additional arguments passed to the command.
    pub args: Option<String>,
    /// Restart condition: on-failure, always, on-success, on-abnormal, on-abort, on-watchdog, never.
    pub restart_condition: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct SnapcraftLayout {
    /// Bind-mount a directory to the snap's layout.
    pub bind: Option<String>,
    /// Symlink a path to a location in the snap.
    pub symlink: Option<String>,
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
    /// Additional files to include in the DMG.
    pub extra_files: Option<Vec<String>>,
    /// Remove source archives from artifacts, keeping only DMG.
    pub replace: Option<bool>,
    /// Output timestamp for reproducible builds.
    pub mod_timestamp: Option<String>,
    /// Disable this DMG config.
    pub disable: Option<bool>,
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
    /// Disable this MSI config.
    pub disable: Option<bool>,
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
    /// Additional files to include in the package.
    pub extra_files: Option<Vec<String>>,
    /// Remove source archives from artifacts, keeping only PKG.
    pub replace: Option<bool>,
    /// Output timestamp for reproducible builds.
    pub mod_timestamp: Option<String>,
    /// Disable this PKG config.
    pub disable: Option<bool>,
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
    /// Upload only extra files (skip artifacts).
    pub extra_files_only: Option<bool>,
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
    /// Nested subgroups within this group. Rendered as sub-sections (e.g. `###`).
    pub groups: Option<Vec<ChangelogGroup>>,
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
    pub env: Option<HashMap<String, String>>,
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
    pub id: Option<String>,
    pub artifacts: Option<String>,
    pub cmd: Option<String>,
    pub args: Option<Vec<String>>,
    pub ids: Option<Vec<String>>,
    pub stdin: Option<String>,
    pub stdin_file: Option<String>,
    pub env: Option<HashMap<String, String>>,
    /// Capture and log stdout/stderr of the docker signing command.
    pub output: Option<bool>,
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
    /// Template-conditional skip: if rendered to "true", skip the entire announce stage.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    pub discord: Option<DiscordAnnounce>,
    pub slack: Option<SlackAnnounce>,
    pub webhook: Option<WebhookConfig>,
    pub telegram: Option<TelegramAnnounce>,
    pub teams: Option<TeamsAnnounce>,
    pub mattermost: Option<MattermostAnnounce>,
    pub email: Option<EmailAnnounce>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DiscordAnnounce {
    pub enabled: Option<bool>,
    pub webhook_url: Option<String>,
    pub message_template: Option<String>,
    /// Author name displayed in the embed (optional)
    pub author: Option<String>,
    /// Embed color as a decimal integer (default: 3553599, GoReleaser blue)
    pub color: Option<u32>,
    /// Icon URL for the embed footer (optional)
    pub icon_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct WebhookConfig {
    pub enabled: Option<bool>,
    pub endpoint_url: Option<String>,
    pub headers: Option<HashMap<String, String>>,
    pub content_type: Option<String>,
    pub message_template: Option<String>,
    /// When true, skip TLS certificate verification for the webhook endpoint
    pub skip_tls_verify: Option<bool>,
    /// HTTP status codes to accept as success (default: [200, 201, 202, 204])
    #[serde(default)]
    pub expected_status_codes: Vec<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct TelegramAnnounce {
    pub enabled: Option<bool>,
    pub bot_token: Option<String>,
    pub chat_id: Option<String>,
    pub message_template: Option<String>,
    /// Optional parse mode: "MarkdownV2" or "HTML" (defaults to "MarkdownV2")
    pub parse_mode: Option<String>,
    /// Optional message thread ID for sending to a specific topic in a forum group
    pub message_thread_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct TeamsAnnounce {
    pub enabled: Option<bool>,
    pub webhook_url: Option<String>,
    pub message_template: Option<String>,
    /// Optional title template for the Adaptive Card header
    pub title_template: Option<String>,
    /// Optional theme color for the card (hex string, e.g. "0076D7")
    pub color: Option<String>,
    /// Optional icon URL displayed in the card header
    pub icon_url: Option<String>,
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
    /// Optional icon emoji for the bot post (e.g. ":rocket:")
    pub icon_emoji: Option<String>,
    /// Optional attachment color (hex string, e.g. "#36a64f")
    pub color: Option<String>,
    pub message_template: Option<String>,
    /// Optional title template for the Mattermost attachment
    pub title_template: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct SlackAnnounce {
    pub enabled: Option<bool>,
    pub webhook_url: Option<String>,
    pub message_template: Option<String>,
    pub channel: Option<String>,
    pub username: Option<String>,
    pub icon_emoji: Option<String>,
    pub icon_url: Option<String>,
    pub blocks: Option<serde_json::Value>,
    pub attachments: Option<serde_json::Value>,
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
    /// Working directory for the publisher command.
    pub dir: Option<String>,
    /// Template-conditional disable: if rendered result is `"true"`, skip this publisher.
    pub disable: Option<String>,
    /// Include checksums in published artifacts.
    pub checksum: Option<bool>,
    /// Include signatures in published artifacts.
    pub signature: Option<bool>,
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
    pub pre: Option<Vec<HookEntry>>,
    pub post: Option<Vec<HookEntry>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct StructuredHook {
    pub cmd: String,
    pub dir: Option<String>,
    pub env: Option<HashMap<String, String>>,
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
    /// Binary-specific signing configs (same shape as `signs` but only for binary artifacts).
    #[serde(default, alias = "binary_sign", deserialize_with = "deserialize_signs")]
    #[schemars(schema_with = "signs_schema")]
    pub binary_signs: Vec<SignConfig>,
    pub before: Option<HooksConfig>,
    pub after: Option<HooksConfig>,
    pub env: Option<HashMap<String, String>>,
}

// ---------------------------------------------------------------------------
// StringOrBool — accepts bool or template string in YAML
// ---------------------------------------------------------------------------

/// A value that can be either a bool or a template string.
/// Used by `BlobConfig.disable` to support both `disable: true` and
/// `disable: "{{ if IsSnapshot }}true{{ endif }}"`.
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
    pub fn is_disabled(&self, render: impl Fn(&str) -> anyhow::Result<String>) -> bool {
        if self.is_template() {
            render(self.as_str())
                .map(|r| r.trim() == "true")
                .unwrap_or(false)
        } else {
            self.as_bool()
        }
    }
}

impl Default for StringOrBool {
    fn default() -> Self {
        StringOrBool::Bool(false)
    }
}

/// Custom deserializer for `Option<StringOrBool>`.
fn deserialize_string_or_bool_opt<'de, D>(
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

/// Custom deserializer for `Option<Vec<String>>` that accepts either a single
/// string or an array of strings. Used by `BlobConfig.cache_control`.
fn deserialize_string_or_vec_opt<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<String>>, D::Error>
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
            ExtraFileSpec::Detailed { glob, name_template } => {
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
            Some(ContentSource::Inline(
                "---\nPowered by anodize".to_string()
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
        assert_eq!(
            files[0].name_template(),
            Some("{{ .ArtifactName }}.sig")
        );
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
      target_commitish: main
      discussion_category_name: Announcements
      include_meta: true
      use_existing_draft: false
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
        assert_eq!(release.skip_upload, Some(false));
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
        assert_eq!(hb.skip_upload.as_deref(), Some("auto"));
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
        assert_eq!(sc.skip_upload.as_deref(), Some("true"));

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
}
