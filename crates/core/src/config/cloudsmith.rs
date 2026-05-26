use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{StringOrBool, deserialize_string_or_bool_opt};

// ---------------------------------------------------------------------------
// CloudSmith publisher
// ---------------------------------------------------------------------------

/// Per-format distribution value. Accepts either a single distribution string
/// (`deb: "ubuntu/focal"`) or an array of distribution slugs
/// (`deb: ["ubuntu/focal", "ubuntu/jammy"]`) — the array form mirrors
/// GoReleaser Pro v2.8+ and causes the publisher to issue one upload per
/// distribution slug.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum CloudSmithDistributions {
    /// Single distribution slug (`"ubuntu/focal"`).
    Single(String),
    /// Multiple distribution slugs; the publisher uploads once per entry.
    Multiple(Vec<String>),
}

impl CloudSmithDistributions {
    /// Materialize as a `Vec<String>` regardless of which YAML form the user
    /// wrote. A `Single` value yields a one-element vec so the caller can
    /// always iterate.
    pub fn as_slice(&self) -> Vec<&str> {
        match self {
            Self::Single(s) => vec![s.as_str()],
            Self::Multiple(v) => v.iter().map(String::as_str).collect(),
        }
    }
}

/// CloudSmith publisher configuration.
/// Pushes packages to CloudSmith repositories.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct CloudSmithConfig {
    /// CloudSmith organization slug.
    pub organization: Option<String>,
    /// CloudSmith repository slug.
    pub repository: Option<String>,
    /// Build IDs filter: only publish artifacts from builds whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Package format filter: only publish artifacts matching these formats.
    pub formats: Option<Vec<String>>,
    /// Distribution mapping per format. Each entry accepts either a single
    /// slug (`deb: "ubuntu/focal"`) or an array of slugs
    /// (`deb: ["ubuntu/focal", "ubuntu/jammy"]`); the array form issues one
    /// upload per entry. Mirrors GoReleaser Pro v2.8+.
    pub distributions: Option<HashMap<String, CloudSmithDistributions>>,
    /// Debian component name (e.g. "main").
    pub component: Option<String>,
    /// Environment variable name containing the CloudSmith API key.
    pub secret_name: Option<String>,
    /// Template-conditional skip: if rendered result is `"true"`, skip this publisher.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// When true, allow republishing over existing package versions.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub republish: Option<StringOrBool>,
    /// Override whether this publisher failing should fail the overall release.
    ///
    /// Default: `false` — a failure here is logged but does not abort the release.
    /// Set to `true` to fail the release on any error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    /// Template-conditional gate: when the rendered result is falsy
    /// (`"false"` / `"0"` / `"no"` / empty), the CloudSmith publisher is
    /// skipped. Render failure hard-errors. Mirrors GoReleaser Pro
    /// `cloudsmiths[].if:`.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
}
