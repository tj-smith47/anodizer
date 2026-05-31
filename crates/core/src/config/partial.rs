use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// PartialConfig (split/merge CI fan-out)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct PartialConfig {
    /// How to split builds: "os" (by OS, default) or "target" (by full triple).
    /// "os" groups all arch variants for the same OS into one split job.
    /// "target" gives each unique target triple its own split job.
    pub by: Option<String>,
}
