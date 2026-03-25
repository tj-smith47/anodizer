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
    pub sign: Option<SignConfig>,
    pub docker_signs: Option<Vec<DockerSignConfig>>,
    pub snapshot: Option<SnapshotConfig>,
    pub announce: Option<AnnounceConfig>,
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
            sign: None,
            docker_signs: None,
            snapshot: None,
            announce: None,
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
                Err(E::custom("archives: true is not valid; use false or a list"))
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
    pub name_template: Option<String>,
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
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            PrereleaseConfig::Auto => serializer.serialize_str("auto"),
            PrereleaseConfig::Bool(b) => serializer.serialize_bool(*b),
        }
    }
}

impl<'de> Deserialize<'de> for PrereleaseConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct PrereleaseVisitor;
        impl serde::de::Visitor<'_> for PrereleaseVisitor {
            type Value = PrereleaseConfig;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "\"auto\" or a boolean")
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> std::result::Result<PrereleaseConfig, E> {
                Ok(PrereleaseConfig::Bool(v))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<PrereleaseConfig, E> {
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
            Some(CratesPublishConfig::Object { enabled, index_timeout }) => CratesPublishSettings {
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NfpmContent {
    pub src: String,
    pub dst: String,
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
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ChangelogFilters {
    pub exclude: Option<Vec<String>>,
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
    pub artifacts: Option<String>,
    pub cmd: Option<String>,
    pub args: Option<Vec<String>>,
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
        assert!(matches!(config.crates[0].archives, ArchivesConfig::Disabled));
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
        assert!(config.crates[0].publish.as_ref().unwrap().crates_config().enabled);

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
}
