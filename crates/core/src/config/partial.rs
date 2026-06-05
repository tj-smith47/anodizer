use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

// ---------------------------------------------------------------------------
// PartialConfig (split/merge CI fan-out)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct PartialConfig {
    /// How to split builds: "os" (by OS, default) or "target" (by full triple).
    /// "os" groups all arch variants for the same OS into one split job.
    /// "target" gives each unique target triple its own split job.
    ///
    /// The legacy `goos` spelling is accepted as a back-compat alias for `os`
    /// (folded at parse time, with a deprecation warning); imported configs
    /// keep loading.
    #[serde(default, deserialize_with = "deserialize_partial_by")]
    pub by: Option<String>,
}

/// Normalize `partial.by` at parse time so the legacy Go-style `goos` spelling
/// folds into the canonical `os` value. Any other value passes through
/// unchanged for the central validator (`config::validate_partial`) to accept
/// or reject. A deprecation warning fires when the alias is hit.
fn deserialize_partial_by<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    Ok(raw.map(|v| {
        if v == "goos" {
            tracing::warn!(
                "DEPRECATION: partial.by: 'goos' is renamed to 'os'. The 'goos' \
                 spelling still works but will be removed in a future release; \
                 switch to 'os'."
            );
            "os".to_string()
        } else {
            v
        }
    }))
}

#[cfg(test)]
mod by_alias_tests {
    use super::*;

    /// The legacy `goos` spelling folds into the canonical `os` at parse time
    /// so imported configs keep loading.
    #[test]
    fn goos_alias_folds_into_os() {
        let cfg: PartialConfig = serde_yaml_ng::from_str("by: goos").unwrap();
        assert_eq!(cfg.by.as_deref(), Some("os"));
    }

    #[test]
    fn canonical_os_passes_through() {
        let cfg: PartialConfig = serde_yaml_ng::from_str("by: os").unwrap();
        assert_eq!(cfg.by.as_deref(), Some("os"));
    }

    #[test]
    fn target_passes_through() {
        let cfg: PartialConfig = serde_yaml_ng::from_str("by: target").unwrap();
        assert_eq!(cfg.by.as_deref(), Some("target"));
    }

    /// A genuinely unknown value is left intact for the central validator
    /// (`config::validate_partial`) to reject — the alias step only rewrites
    /// `goos`.
    #[test]
    fn unknown_value_passes_through_unchanged() {
        let cfg: PartialConfig = serde_yaml_ng::from_str("by: bogus").unwrap();
        assert_eq!(cfg.by.as_deref(), Some("bogus"));
    }
}
