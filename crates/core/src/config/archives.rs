use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use super::{
    ArchiveHooksConfig, SignConfig, StringOrBool, StringOrU32, deserialize_string_or_bool_opt,
};

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
pub(super) fn deserialize_archives_config<'de, D>(
    deserializer: D,
) -> Result<ArchivesConfig, D::Error>
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
pub(super) fn deserialize_signs<'de, D>(deserializer: D) -> Result<Vec<SignConfig>, D::Error>
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
pub(super) fn deserialize_binary_signs<'de, D>(deserializer: D) -> Result<Vec<SignConfig>, D::Error>
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
    /// Archive formats: tar.gz, tar.xz, tar.zst, tar, zip, gz, xz, or binary.
    /// `gz` and `xz` are single-file compressors — supplying multiple input
    /// files errors. Plural list; one archive per format is produced for each
    /// target.
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
    /// Plural format overrides for this OS: tar.gz, tar.xz, tar.zst, tar, zip,
    /// gz, xz, or binary.
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
    "tar.gz", "tgz", "tar.xz", "txz", "tar.zst", "tzst", "tar", "zip", "gz", "xz", "binary", "none",
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
