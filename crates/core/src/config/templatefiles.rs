use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{StringOrBool, deserialize_string_or_bool_opt};

// ---------------------------------------------------------------------------
// TemplateFileConfig
// ---------------------------------------------------------------------------

/// Configuration for a template file that is rendered through the template
/// engine and placed in the dist directory as a release artifact.
///
/// All rendered template files are uploaded to the
/// release by default. Both `src` and `dst` paths support template rendering.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
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
    /// Skip this entry when truthy. Accepts a literal bool or a Tera
    /// template that renders to `"true"`/`"false"` (e.g.
    /// `'{{ if eq .Os "windows" }}true{{ end }}'`). Mirrors the
    /// per-entry `skip:` pattern used by `ChangelogConfig`,
    /// `ChecksumConfig`, and the publishers.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
}
