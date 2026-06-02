use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use super::StringOrU32;

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

    /// Apply defaults to `prefix_template`: when unset and `name_template` is
    /// set, default `prefix_template` to the same string as `name_template`.
    ///
    /// Q-src3: the doc has long claimed "Defaults to `name_template` value",
    /// but downstream consumers were reading the raw `Option<String>` and
    /// substituting empty string. Filling the field at defaults-resolution
    /// time honors the documented contract — stage code that already
    /// renders the (now-Some) field needs no behavioral change.
    ///
    /// Defaults to
    /// empty; this is anodize-additive (more ergonomic default), aligning
    /// behavior with the long-standing doc.
    pub fn apply_prefix_template_default(&mut self) {
        if self.prefix_template.is_none()
            && let Some(ref name_tpl) = self.name_template
        {
            self.prefix_template = Some(name_tpl.clone());
        }
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
