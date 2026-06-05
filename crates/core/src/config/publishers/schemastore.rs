//! `schemastore:` publisher config — registers a tool's JSON Schema(s) on
//! SchemaStore. Field presence selects the mode: `url` ⇒ external (catalog
//! entry only), `schema_file` ⇒ vendor (file copied into the SchemaStore repo).
//! See `.claude/specs/2026-06-05-schemastore-publisher.md`.

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
}
