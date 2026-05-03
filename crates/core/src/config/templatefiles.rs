use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// TemplateFileConfig
// ---------------------------------------------------------------------------

/// Configuration for a template file that is rendered through the template
/// engine and placed in the dist directory as a release artifact.
///
/// GoReleaser Pro feature: all rendered template files are uploaded to the
/// release by default. Both `src` and `dst` paths support template rendering.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
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
}
