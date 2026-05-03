use super::*;

// ---------------------------------------------------------------------------
// CloudSmith publisher
// ---------------------------------------------------------------------------

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
    /// Distribution mapping per format (e.g. `deb: "ubuntu/focal"`).
    pub distributions: Option<HashMap<String, serde_json::Value>>,
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
}
