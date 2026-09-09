use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

// ---------------------------------------------------------------------------
// VersionFileEntry — accepts a bare path OR a path scoped by a `match` anchor
// ---------------------------------------------------------------------------

/// One `version_files` enrollment.
///
/// Accepts two forms:
/// - **Bare path** — every occurrence of the version in the file is rewritten.
///   ```yaml
///   version_files:
///     - docs/installation.md
///   ```
/// - **Anchored** — a path plus a `match` regex that selects this enrollment's
///   own occurrences, so two crates can share one file (and even one literal)
///   without rewriting each other's lines.
///   ```yaml
///   version_files:
///     - path: chart/cfgd/values.yaml
///       match: 'operator:\s+image:.*:v{version}'
///   ```
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum VersionFileEntry {
    /// Bare repo-root-relative path — a whole-file sweep.
    Path(String),
    /// Path plus a regex anchor scoping the rewrite to its own occurrences.
    Anchored(AnchoredVersionFile),
}

/// A `version_files` entry scoped to the occurrences its regex selects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnchoredVersionFile {
    /// Repo-root-relative path of the enrolled file.
    pub path: String,
    /// Regex selecting this enrollment's own occurrences. `{version}` stands
    /// for the crate's version (regex-escaped when matching) and must appear at
    /// least once. Every match is rewritten; zero matches is an error.
    #[serde(rename = "match")]
    pub match_pattern: String,
}

impl VersionFileEntry {
    /// The enrolled repo-root-relative path, for either form.
    pub fn path(&self) -> &str {
        match self {
            VersionFileEntry::Path(p) => p,
            VersionFileEntry::Anchored(a) => &a.path,
        }
    }

    /// The `match` anchor, or `None` for a bare path entry.
    pub fn anchor(&self) -> Option<&str> {
        match self {
            VersionFileEntry::Path(_) => None,
            VersionFileEntry::Anchored(a) => Some(&a.match_pattern),
        }
    }
}

impl<'de> Deserialize<'de> for VersionFileEntry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_yaml_ng::Value::deserialize(deserializer)?;
        match &value {
            serde_yaml_ng::Value::String(_) => {
                let path: String =
                    serde_yaml_ng::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(VersionFileEntry::Path(path))
            }
            serde_yaml_ng::Value::Mapping(_) => {
                let anchored: AnchoredVersionFile =
                    serde_yaml_ng::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(VersionFileEntry::Anchored(anchored))
            }
            _ => Err(serde::de::Error::custom(
                "version_files entry must be a path string or a mapping with `path` and `match`",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_file_entry_accepts_string_and_mapping() {
        let entries: Vec<VersionFileEntry> = serde_yaml_ng::from_str(
            "- a.md\n- path: chart/values.yaml\n  match: 'csi:.*v{version}'\n",
        )
        .unwrap();
        assert_eq!(entries[0], VersionFileEntry::Path("a.md".to_string()));
        assert_eq!(entries[0].path(), "a.md");
        assert_eq!(entries[0].anchor(), None);
        assert_eq!(entries[1].path(), "chart/values.yaml");
        assert_eq!(entries[1].anchor(), Some("csi:.*v{version}"));

        let extra_key = serde_yaml_ng::from_str::<Vec<VersionFileEntry>>(
            "- path: a.md\n  match: 'v{version}'\n  nope: 1\n",
        )
        .unwrap_err()
        .to_string();
        assert!(extra_key.contains("nope"), "err: {extra_key}");

        let wrong_kind = serde_yaml_ng::from_str::<Vec<VersionFileEntry>>("- 12\n")
            .unwrap_err()
            .to_string();
        assert!(
            wrong_kind.contains(
                "version_files entry must be a path string or a mapping with `path` and `match`"
            ),
            "err: {wrong_kind}"
        );
    }

    #[test]
    fn version_file_entry_round_trips_bare_as_a_scalar() {
        let yaml =
            serde_yaml_ng::to_string(&vec![VersionFileEntry::Path("a.md".to_string())]).unwrap();
        assert_eq!(yaml, "- a.md\n");
        let back: Vec<VersionFileEntry> = serde_yaml_ng::from_str(&yaml).unwrap();
        assert_eq!(back[0], VersionFileEntry::Path("a.md".to_string()));
    }
}
