use super::*;

// ---------------------------------------------------------------------------
// HooksConfig
// ---------------------------------------------------------------------------

/// Top-level lifecycle hooks for `before` and `after` blocks.
/// Each block has `pre` and `post` lists of hook commands that run around the
/// entire pipeline (not individual stages).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct HooksConfig {
    /// Commands to run before the pipeline or stage starts. Matches GoReleaser
    /// `before.hooks` canonically.
    pub hooks: Option<Vec<HookEntry>>,
    /// Commands to run after the pipeline or stage completes. Anodizer extension
    /// (GoReleaser has no top-level `after:` block).
    pub post: Option<Vec<HookEntry>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct StructuredHook {
    /// Command to run (passed through the shell).
    pub cmd: String,
    /// Working directory for the command (defaults to project root).
    pub dir: Option<String>,
    /// Environment variables for the command.
    #[serde(default)]
    pub env: Option<Vec<String>>,
    /// When true, capture and log stdout/stderr of the command.
    pub output: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum HookEntry {
    Simple(String),
    Structured(StructuredHook),
}

impl PartialEq<&str> for HookEntry {
    fn eq(&self, other: &&str) -> bool {
        match self {
            HookEntry::Simple(s) => s.as_str() == *other,
            HookEntry::Structured(h) => h.cmd.as_str() == *other,
        }
    }
}

impl<'de> Deserialize<'de> for HookEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match &value {
            serde_json::Value::String(s) => Ok(HookEntry::Simple(s.clone())),
            serde_json::Value::Object(_) => {
                let hook: StructuredHook =
                    serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(HookEntry::Structured(hook))
            }
            _ => Err(serde::de::Error::custom(
                "hook entry must be a string or an object with cmd/dir/env/output",
            )),
        }
    }
}
