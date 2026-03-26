use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize};

// ---------------------------------------------------------------------------
// Top-level config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub project_name: String,
    #[serde(default = "default_dist")]
    pub dist: PathBuf,
    pub defaults: Option<Defaults>,
    pub before: Option<HooksConfig>,
    pub after: Option<HooksConfig>,
    pub crates: Vec<CrateConfig>,
    pub changelog: Option<ChangelogConfig>,
    #[serde(default, alias = "sign", deserialize_with = "deserialize_signs")]
    pub signs: Vec<SignConfig>,
    pub docker_signs: Option<Vec<DockerSignConfig>>,
    pub snapshot: Option<SnapshotConfig>,
    pub announce: Option<AnnounceConfig>,
    pub report_sizes: Option<bool>,
    pub env: Option<HashMap<String, String>>,
    pub publishers: Option<Vec<PublisherConfig>>,
}

fn default_dist() -> PathBuf {
    PathBuf::from("./dist")
}

impl Default for Config {
    fn default() -> Self {
        Config {
            project_name: String::new(),
            dist: default_dist(),
            defaults: None,
            before: None,
            after: None,
            crates: Vec::new(),
            changelog: None,
            signs: Vec::new(),
            docker_signs: None,
            snapshot: None,
            announce: None,
            report_sizes: None,
            env: None,
            publishers: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Defaults {
    pub targets: Option<Vec<String>>,
    pub cross: Option<CrossStrategy>,
    pub flags: Option<String>,
    pub archives: Option<DefaultArchiveConfig>,
    pub checksum: Option<ChecksumConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DefaultArchiveConfig {
    pub format: Option<String>,
    pub format_overrides: Option<Vec<FormatOverride>>,
}

// ---------------------------------------------------------------------------
// CrossStrategy
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CrateConfig {
    pub name: String,
    pub path: String,
    pub tag_template: String,
    pub depends_on: Option<Vec<String>>,
    pub builds: Option<Vec<BuildConfig>>,
    pub cross: Option<CrossStrategy>,
    #[serde(default, deserialize_with = "deserialize_archives_config")]
    pub archives: ArchivesConfig,
    pub checksum: Option<ChecksumConfig>,
    pub release: Option<ReleaseConfig>,
    pub publish: Option<PublishConfig>,
    pub docker: Option<Vec<DockerConfig>>,
    pub nfpm: Option<Vec<NfpmConfig>>,
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
        }
    }
}

// ---------------------------------------------------------------------------
// BuildConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BuildConfig {
    pub binary: String,
    pub targets: Option<Vec<String>>,
    pub features: Option<Vec<String>>,
    pub no_default_features: Option<bool>,
    pub env: Option<HashMap<String, HashMap<String, String>>>,
    pub copy_from: Option<String>,
    pub flags: Option<String>,
}

// ---------------------------------------------------------------------------
// ArchivesConfig — untagged enum: false => Disabled, array => Configs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ArchiveConfig {
    pub name_template: Option<String>,
    pub format: Option<String>,
    pub format_overrides: Option<Vec<FormatOverride>>,
    pub files: Option<Vec<String>>,
    pub binaries: Option<Vec<String>>,
    pub wrap_in_directory: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormatOverride {
    pub os: String,
    pub format: String,
}

// ---------------------------------------------------------------------------
// ChecksumConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ReleaseConfig {
    pub github: Option<GitHubConfig>,
    pub draft: Option<bool>,
    pub prerelease: Option<PrereleaseConfig>,
    pub make_latest: Option<MakeLatestConfig>,
    pub name_template: Option<String>,
    pub header: Option<String>,
    pub footer: Option<String>,
    pub extra_files: Option<Vec<String>>,
    pub skip_upload: Option<bool>,
    pub replace_existing_draft: Option<bool>,
    pub replace_existing_artifacts: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PublishConfig {
    pub crates: Option<CratesPublishConfig>,
    pub homebrew: Option<HomebrewConfig>,
    pub scoop: Option<ScoopConfig>,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct HomebrewConfig {
    pub tap: Option<TapConfig>,
    pub folder: Option<String>,
    pub description: Option<String>,
    pub license: Option<String>,
    pub install: Option<String>,
    pub test: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ScoopConfig {
    pub bucket: Option<BucketConfig>,
    pub description: Option<String>,
    pub license: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TapConfig {
    pub owner: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BucketConfig {
    pub owner: String,
    pub name: String,
}

// ---------------------------------------------------------------------------
// DockerConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct NfpmScripts {
    pub preinstall: Option<String>,
    pub postinstall: Option<String>,
    pub preremove: Option<String>,
    pub postremove: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct NfpmFileInfo {
    pub owner: Option<String>,
    pub group: Option<String>,
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NfpmContent {
    pub src: String,
    pub dst: String,
    #[serde(rename = "type")]
    pub content_type: Option<String>,
    pub file_info: Option<NfpmFileInfo>,
}

// ---------------------------------------------------------------------------
// ChangelogConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ChangelogFilters {
    pub exclude: Option<Vec<String>>,
    pub include: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ChangelogGroup {
    pub title: String,
    pub regexp: Option<String>,
    pub order: Option<i32>,
}

// ---------------------------------------------------------------------------
// SignConfig / DockerSignConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DockerSignConfig {
    pub artifacts: Option<String>,
    pub cmd: Option<String>,
    pub args: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// SnapshotConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotConfig {
    pub name_template: String,
}

// ---------------------------------------------------------------------------
// AnnounceConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AnnounceConfig {
    pub discord: Option<AnnounceProviderConfig>,
    pub slack: Option<AnnounceProviderConfig>,
    pub webhook: Option<WebhookConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AnnounceProviderConfig {
    pub enabled: Option<bool>,
    pub webhook_url: Option<String>,
    pub message_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct WebhookConfig {
    pub enabled: Option<bool>,
    pub endpoint_url: Option<String>,
    pub headers: Option<HashMap<String, String>>,
    pub content_type: Option<String>,
    pub message_template: Option<String>,
}

// ---------------------------------------------------------------------------
// PublisherConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct HooksConfig {
    pub hooks: Vec<String>,
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml_bool).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml_obj).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let result: Result<Config, _> = serde_yaml::from_str(yaml);
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let result: Result<Config, _> = serde_yaml::from_str(yaml);
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
        let result: Result<Config, _> = serde_yaml::from_str(yaml);
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
        // should be rejected by serde_yaml, unlike a number which gets coerced.
        let yaml = r#"
project_name:
  nested: object
  another: field
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let result: Result<Config, _> = serde_yaml::from_str(yaml);
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
        let result: Result<Config, _> = serde_yaml::from_str(yaml);
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
        let result: Result<Config, _> = serde_yaml::from_str(yaml);
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
        let result: Result<Config, _> = serde_yaml::from_str(yaml);
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
        let result: Result<Config, _> = serde_yaml::from_str(yaml);
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
        // serde_yaml treats empty input as `null`, which the default impl handles.
        let yaml = "";
        let result: Result<Config, _> = serde_yaml::from_str(yaml);
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
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
}
