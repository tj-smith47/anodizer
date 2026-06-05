//! `schemastore:` publisher config — registers a tool's JSON Schema(s) on
//! SchemaStore. Field presence selects the mode: `url` ⇒ external (catalog
//! entry only), `schema_file` ⇒ vendor (file copied into the SchemaStore repo).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::super::{StringOrBool, deserialize_string_or_bool_opt};
use super::{CommitAuthorConfig, RepositoryConfig};

/// Top-level `schemastore:` block. Shared fields here are defaults for every
/// entry in `schemas`; a per-entry field overrides them (cascade).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SchemastoreConfig {
    /// Fork of `SchemaStore/schemastore` to push branches to and open the PR from.
    pub repository: Option<RepositoryConfig>,
    /// Commit author for the SchemaStore commit (defaults to git config).
    pub commit_author: Option<CommitAuthorConfig>,
    /// Default for `SchemaEntry::versioned`.
    pub versioned: Option<bool>,
    /// Skip the whole publisher. Alias: `disable`.
    #[serde(
        deserialize_with = "deserialize_string_or_bool_opt",
        default,
        alias = "disable"
    )]
    pub skip: Option<StringOrBool>,
    /// Tera condition; when it renders falsy the publisher is skipped.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
    /// The schema entries to register/refresh.
    pub schemas: Vec<SchemaEntry>,
}

/// One schema registration. `url` XOR `schema_file` selects the mode.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SchemaEntry {
    /// Catalog display name (may be Title Case, e.g. `Anodizer`).
    pub name: String,
    /// Vendor filename / url basename. Defaults to `name` slugified. Vendor-only.
    pub slug: Option<String>,
    /// Well-known config filenames this schema validates (folder globs need `**/`).
    pub file_match: Vec<String>,
    /// EXTERNAL mode: the URL you host the schema at.
    pub url: Option<String>,
    /// VENDOR mode: repo-root-relative path to the generated schema file.
    pub schema_file: Option<String>,
    /// Crate whose version a vendored/versioned schema tracks (per-crate workspaces).
    #[serde(rename = "crate")]
    pub crate_: Option<String>,
    /// Catalog description (required at publish time; derived if omitted).
    pub description: Option<String>,
    /// Emit a version-suffixed vendored file + `versions` map. Vendor-only.
    pub versioned: Option<bool>,
    /// Whether a failure here fails the release. Collapsed across `schemas`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    /// Per-entry skip. Alias: `disable`.
    #[serde(
        deserialize_with = "deserialize_string_or_bool_opt",
        default,
        alias = "disable"
    )]
    pub skip: Option<StringOrBool>,
    /// Per-entry Tera condition.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
}

/// Hosting mode, inferred from which source field is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaMode {
    External,
    Vendor,
}

impl SchemaEntry {
    /// Infer the mode from field presence. Error if neither/both source fields set.
    pub fn mode(&self) -> anyhow::Result<SchemaMode> {
        match (self.url.is_some(), self.schema_file.is_some()) {
            (true, false) => Ok(SchemaMode::External),
            (false, true) => Ok(SchemaMode::Vendor),
            (false, false) => anyhow::bail!(
                "schemastore schema `{}`: set `url` or `schema_file`",
                self.name
            ),
            (true, true) => anyhow::bail!(
                "schemastore schema `{}`: set `url` or `schema_file`, not both",
                self.name
            ),
        }
    }

    /// Config-shape validation (mode + file_match). Content rules that need the
    /// resolved description/dialect are checked later in `manifest`.
    pub fn validate(&self) -> anyhow::Result<()> {
        self.mode()?;
        if self.file_match.is_empty() {
            anyhow::bail!(
                "schemastore schema `{}`: `file_match` must list at least one filename",
                self.name
            );
        }
        Ok(())
    }
}

impl SchemastoreConfig {
    /// Effective `repository` for an entry (block-level; one fork per PR).
    pub fn resolved_repository(&self, _entry: &SchemaEntry) -> Option<&RepositoryConfig> {
        self.repository.as_ref()
    }

    /// Effective `commit_author` (block-level).
    pub fn resolved_commit_author(&self, _entry: &SchemaEntry) -> Option<&CommitAuthorConfig> {
        self.commit_author.as_ref()
    }

    /// Effective `versioned`: per-entry wins, else block default, else false.
    pub fn resolved_versioned(&self, entry: &SchemaEntry) -> bool {
        entry.versioned.or(self.versioned).unwrap_or(false)
    }

    /// Effective `skip`: true if either the entry or the block sets it truthy.
    pub fn resolved_skip(&self, entry: &SchemaEntry) -> bool {
        let block = self
            .skip
            .as_ref()
            .map(StringOrBool::as_bool)
            .unwrap_or(false);
        let per = entry
            .skip
            .as_ref()
            .map(StringOrBool::as_bool)
            .unwrap_or(false);
        block || per
    }

    /// Effective `if` condition: per-entry wins, else block.
    pub fn resolved_if<'a>(&'a self, entry: &'a SchemaEntry) -> Option<&'a str> {
        entry
            .if_condition
            .as_deref()
            .or(self.if_condition.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_external_and_vendor_entries() {
        let yaml = r#"
repository: { owner: tj-smith47, name: schemastore }
versioned: false
schemas:
  - name: Anodizer
    file_match: [".anodizer.yaml", ".anodizer.yml"]
    url: "https://tj-smith47.github.io/anodizer/schema.json"
    description: "Anodizer Rust release-automation configuration file"
  - name: cfgd-config
    file_match: ["cfgd.yaml"]
    schema_file: "schemas/cfgd-config.schema.json"
    crate: cfgd
"#;
        let cfg: SchemastoreConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.schemas.len(), 2);
        assert_eq!(cfg.schemas[0].name, "Anodizer");
        assert_eq!(
            cfg.schemas[0].url.as_deref(),
            Some("https://tj-smith47.github.io/anodizer/schema.json")
        );
        assert_eq!(
            cfg.schemas[1].schema_file.as_deref(),
            Some("schemas/cfgd-config.schema.json")
        );
        assert_eq!(cfg.schemas[1].crate_.as_deref(), Some("cfgd"));
    }

    #[test]
    fn rejects_unknown_field() {
        let yaml = "schemas: []\nbogus: 1\n";
        assert!(serde_yaml_ng::from_str::<SchemastoreConfig>(yaml).is_err());
    }

    #[test]
    fn per_entry_versioned_overrides_block_default() {
        let cfg = SchemastoreConfig {
            versioned: Some(false),
            schemas: vec![
                SchemaEntry {
                    name: "a".into(),
                    versioned: None,
                    ..Default::default()
                },
                SchemaEntry {
                    name: "b".into(),
                    versioned: Some(true),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert!(!cfg.resolved_versioned(&cfg.schemas[0])); // inherits block
        assert!(cfg.resolved_versioned(&cfg.schemas[1])); // overrides
    }

    #[test]
    fn repository_and_author_fall_through_to_block() {
        let repo = RepositoryConfig {
            owner: Some("tj-smith47".into()),
            name: Some("schemastore".into()),
            ..Default::default()
        };
        let cfg = SchemastoreConfig {
            repository: Some(repo),
            schemas: vec![SchemaEntry {
                name: "a".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            cfg.resolved_repository(&cfg.schemas[0])
                .unwrap()
                .owner
                .as_deref(),
            Some("tj-smith47")
        );
    }

    #[test]
    fn mode_inferred_from_field_presence() {
        let ext = SchemaEntry {
            name: "a".into(),
            url: Some("https://x/s.json".into()),
            file_match: vec!["a.yaml".into()],
            ..Default::default()
        };
        let ven = SchemaEntry {
            name: "b".into(),
            schema_file: Some("s.json".into()),
            file_match: vec!["b.yaml".into()],
            ..Default::default()
        };
        assert_eq!(ext.mode().unwrap(), SchemaMode::External);
        assert_eq!(ven.mode().unwrap(), SchemaMode::Vendor);
    }

    #[test]
    fn validate_rejects_neither_both_and_empty_filematch() {
        let neither = SchemaEntry {
            name: "a".into(),
            file_match: vec!["a.yaml".into()],
            ..Default::default()
        };
        assert!(
            neither
                .validate()
                .unwrap_err()
                .to_string()
                .contains("url` or `schema_file")
        );
        let both = SchemaEntry {
            name: "a".into(),
            url: Some("u".into()),
            schema_file: Some("s".into()),
            file_match: vec!["a.yaml".into()],
            ..Default::default()
        };
        assert!(
            both.validate()
                .unwrap_err()
                .to_string()
                .contains("not both")
        );
        let no_fm = SchemaEntry {
            name: "a".into(),
            url: Some("u".into()),
            file_match: vec![],
            ..Default::default()
        };
        assert!(
            no_fm
                .validate()
                .unwrap_err()
                .to_string()
                .contains("file_match")
        );
    }

    #[test]
    fn disable_alias_is_accepted() {
        let yaml = "schemas: []\nskip: true\n";
        let cfg: SchemastoreConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(cfg.skip.is_some());

        let via_alias = "schemas: []\ndisable: true\n";
        let cfg2: SchemastoreConfig = serde_yaml_ng::from_str(via_alias).unwrap();
        assert!(cfg2.skip.is_some());
    }
}
