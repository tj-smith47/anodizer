use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use super::StringOrU32;

// ---------------------------------------------------------------------------
// SourceConfig
// ---------------------------------------------------------------------------

/// An individual file entry for the source archive, supporting src/dst mapping
/// and file metadata overrides.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
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
fn source_files_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let mut schema = generator.subschema_for::<Vec<SourceFileEntry>>();
    schema.ensure_object().insert(
        "description".to_owned(),
        "Extra files for the source archive. Accepts strings (glob patterns), objects with src/dst/info, or a mixed array.".into(),
    );
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> SourceConfig {
        serde_yaml_ng::from_str(yaml).expect("valid SourceConfig YAML")
    }

    #[test]
    fn is_enabled_defaults_false_when_unset() {
        let cfg = SourceConfig::default();
        assert!(!cfg.is_enabled());
    }

    #[test]
    fn is_enabled_honors_explicit_values() {
        assert!(parse("enabled: true").is_enabled());
        assert!(!parse("enabled: false").is_enabled());
    }

    #[test]
    fn archive_format_defaults_to_tar_gz() {
        assert_eq!(SourceConfig::default().archive_format(), "tar.gz");
    }

    #[test]
    fn archive_format_uses_explicit_format() {
        assert_eq!(parse("format: zip").archive_format(), "zip");
    }

    #[test]
    fn prefix_default_copies_name_template_when_prefix_unset() {
        let mut cfg = parse("name_template: \"{{ .ProjectName }}-{{ .Version }}\"");
        assert!(cfg.prefix_template.is_none());
        cfg.apply_prefix_template_default();
        assert_eq!(
            cfg.prefix_template.as_deref(),
            Some("{{ .ProjectName }}-{{ .Version }}")
        );
    }

    #[test]
    fn prefix_default_preserves_explicit_prefix() {
        let mut cfg = parse("name_template: name-tpl\nprefix_template: prefix-tpl");
        cfg.apply_prefix_template_default();
        assert_eq!(cfg.prefix_template.as_deref(), Some("prefix-tpl"));
    }

    #[test]
    fn prefix_default_noop_when_name_template_also_unset() {
        let mut cfg = SourceConfig::default();
        cfg.apply_prefix_template_default();
        assert!(cfg.prefix_template.is_none());
    }

    #[test]
    fn files_missing_is_empty_vec() {
        assert!(parse("enabled: true").files.is_empty());
    }

    #[test]
    fn files_single_string_becomes_one_entry() {
        let cfg = parse("files: LICENSE");
        assert_eq!(cfg.files.len(), 1);
        assert_eq!(cfg.files[0].src, "LICENSE");
        assert!(cfg.files[0].dst.is_none());
    }

    #[test]
    fn files_single_object_becomes_one_entry() {
        let cfg = parse("files:\n  src: README.md\n  dst: docs/README.md");
        assert_eq!(cfg.files.len(), 1);
        assert_eq!(cfg.files[0].src, "README.md");
        assert_eq!(cfg.files[0].dst.as_deref(), Some("docs/README.md"));
    }

    #[test]
    fn files_mixed_array_parses_strings_and_objects() {
        let cfg = parse("files:\n  - LICENSE\n  - src: README.md\n    dst: docs/README.md\n");
        assert_eq!(cfg.files.len(), 2);
        assert_eq!(cfg.files[0].src, "LICENSE");
        assert!(cfg.files[0].dst.is_none());
        assert_eq!(cfg.files[1].src, "README.md");
        assert_eq!(cfg.files[1].dst.as_deref(), Some("docs/README.md"));
    }

    #[test]
    fn files_null_is_empty_vec() {
        let cfg = parse("files: null");
        assert!(cfg.files.is_empty());
    }

    #[test]
    fn file_entry_info_mode_parses_octal_string() {
        let cfg = parse(
            "files:\n  - src: bin/app\n    info:\n      owner: root\n      mode: \"0o755\"\n",
        );
        let info = cfg.files[0].info.as_ref().expect("info present");
        assert_eq!(info.owner.as_deref(), Some("root"));
        assert_eq!(info.mode.map(|m| m.value()), Some(0o755));
    }

    #[test]
    fn deny_unknown_fields_rejects_typos() {
        let err = serde_yaml_ng::from_str::<SourceConfig>("enabledd: true");
        assert!(err.is_err(), "unknown field must be rejected");
    }

    #[test]
    fn archive_format_passes_through_tgz_tar_and_zip() {
        // archive_format only overrides the default when format is Some; each
        // explicit value must survive verbatim (no normalization).
        assert_eq!(parse("format: tgz").archive_format(), "tgz");
        assert_eq!(parse("format: tar").archive_format(), "tar");
        assert_eq!(parse("format: zip").archive_format(), "zip");
    }

    #[test]
    fn prefix_default_keeps_explicit_prefix_when_name_template_absent() {
        // The unset-name guard must not clobber a prefix the user set alone.
        let mut cfg = parse("prefix_template: only-prefix");
        cfg.apply_prefix_template_default();
        assert_eq!(cfg.prefix_template.as_deref(), Some("only-prefix"));
    }

    #[test]
    fn file_entry_strip_parent_round_trips() {
        let cfg = parse("files:\n  - src: a/b/c.txt\n    strip_parent: true\n");
        assert_eq!(cfg.files[0].strip_parent, Some(true));
        // unset on a plain string entry, not defaulted to a value
        let bare = parse("files: a/b/c.txt");
        assert_eq!(bare.files[0].strip_parent, None);
    }

    #[test]
    fn file_info_mode_accepts_bare_decimal_int() {
        // A bare YAML int is decimal: 493 == 0o755. Distinct path from the
        // octal-prefixed-string case already covered above.
        let cfg = parse("files:\n  - src: bin/app\n    info:\n      mode: 493\n");
        let info = cfg.files[0].info.as_ref().expect("info present");
        assert_eq!(info.mode.map(|m| m.value()), Some(0o755));
    }

    #[test]
    fn file_info_captures_group_and_mtime() {
        let cfg = parse(
            "files:\n  - src: f\n    info:\n      group: staff\n      mtime: \"2024-01-01T00:00:00Z\"\n",
        );
        let info = cfg.files[0].info.as_ref().unwrap();
        assert_eq!(info.group.as_deref(), Some("staff"));
        assert_eq!(info.mtime.as_deref(), Some("2024-01-01T00:00:00Z"));
        assert!(info.owner.is_none());
    }

    #[test]
    fn nested_file_entry_rejects_unknown_field() {
        let err = serde_yaml_ng::from_str::<SourceConfig>("files:\n  - src: f\n    bogus: 1\n");
        assert!(err.is_err(), "SourceFileEntry must deny unknown fields");
    }

    #[test]
    fn nested_file_info_rejects_unknown_field() {
        let err = serde_yaml_ng::from_str::<SourceConfig>(
            "files:\n  - src: f\n    info:\n      moed: 7\n",
        );
        assert!(err.is_err(), "SourceFileInfo must deny unknown fields");
    }

    #[test]
    fn empty_array_files_is_empty_vec() {
        // visit_seq with zero elements is a distinct branch from visit_none.
        let cfg = parse("files: []");
        assert!(cfg.files.is_empty());
    }

    #[test]
    fn files_array_of_objects_only() {
        let cfg = parse("files:\n  - src: a\n    dst: x/a\n  - src: b\n");
        assert_eq!(cfg.files.len(), 2);
        assert_eq!(cfg.files[0].dst.as_deref(), Some("x/a"));
        assert_eq!(cfg.files[1].src, "b");
        assert!(cfg.files[1].dst.is_none());
    }

    #[test]
    fn config_round_trips_through_serde() {
        let cfg = parse(
            "enabled: true\nformat: zip\nname_template: src-{{ .Version }}\nfiles:\n  - LICENSE\n",
        );
        let yaml = serde_yaml_ng::to_string(&cfg).unwrap();
        let back: SourceConfig = serde_yaml_ng::from_str(&yaml).unwrap();
        assert!(back.is_enabled());
        assert_eq!(back.archive_format(), "zip");
        assert_eq!(back.files.len(), 1);
        assert_eq!(back.files[0].src, "LICENSE");
    }
}
