//! Well-known config file discovery.
//!
//! The single source of the candidate-name list every discovery surface
//! probes — the CLI's `find_config` family and the changelog engine's
//! lightweight raw readers alike — so no code path can honor a different
//! name set.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

/// The well-known config file names auto-discovery probes, in precedence
/// order.
pub const CONFIG_CANDIDATES: &[&str] = &[
    ".anodizer.yaml",
    ".anodizer.yml",
    ".anodizer.toml",
    "anodizer.yaml",
    "anodizer.yml",
    "anodizer.toml",
];

/// Find the first [`CONFIG_CANDIDATES`] entry that exists under `base`,
/// joined against `base`. `None` when no candidate exists — callers with a
/// `Cargo.toml` defaults fallback (the CLI loader) layer it on top of this.
///
/// An empty `base` joins to the bare candidate names, so the probe runs
/// relative to the process cwd and the returned path stays relative —
/// the CLI's cwd-anchored discovery builds on that.
pub fn find_config_candidate_in(base: &Path) -> Option<PathBuf> {
    CONFIG_CANDIDATES
        .iter()
        .map(|name| base.join(name))
        .find(|path| path.exists())
}

/// Transcode a parsed `toml::Value` into a `serde_yaml_ng::Value` — the one
/// conversion route shared by every TOML-accepting config surface (the raw
/// readers here and the CLI loader's include merging). Serializes the TOML
/// tree straight into YAML with no `serde_json` intermediate hop: the two
/// routes differ only on non-finite floats (`inf`/`nan`, which TOML and YAML
/// both represent but JSON cannot), and the direct route preserves them.
pub fn toml_value_to_yaml(
    value: &toml::Value,
) -> std::result::Result<serde_yaml_ng::Value, serde_yaml_ng::Error> {
    serde_yaml_ng::to_value(value)
}

/// Read a discovered config file into a raw `serde_yaml_ng::Value`,
/// format-detected by extension (YAML parsed directly, TOML transcoded
/// through its own parser), so raw-field readers handle every
/// [`CONFIG_CANDIDATES`] entry — not just the YAML spellings — with one
/// traversal API. Deliberately performs none of the CLI loader's
/// include/validation machinery; callers that need the typed [`Config`]
/// go through the CLI's `load_config`.
///
/// [`Config`]: crate::config::Config
pub fn load_raw_config_value(path: &Path) -> Result<serde_yaml_ng::Value> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "yaml" | "yml" => serde_yaml_ng::from_str(&text)
            .with_context(|| format!("failed to parse YAML at {}", path.display())),
        "toml" => {
            let value: toml::Value = toml::from_str(&text)
                .with_context(|| format!("failed to parse TOML at {}", path.display()))?;
            toml_value_to_yaml(&value)
                .with_context(|| format!("failed to convert TOML at {}", path.display()))
        }
        other => bail!(
            "unsupported config format '{}' at {}",
            other,
            path.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_config_candidate_honors_precedence_order() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("anodizer.yaml"), "project_name: b\n").unwrap();
        std::fs::write(tmp.path().join(".anodizer.yaml"), "project_name: a\n").unwrap();
        let found = find_config_candidate_in(tmp.path()).expect("candidate");
        assert_eq!(found, tmp.path().join(".anodizer.yaml"));
    }

    #[test]
    fn find_config_candidate_returns_none_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(find_config_candidate_in(tmp.path()).is_none());
    }

    #[test]
    fn toml_value_to_yaml_preserves_non_finite_floats() {
        // Pins the direct toml→yaml route: a serde_json hop would reject
        // `inf`/`nan` (JSON cannot represent them) even though both TOML
        // and YAML can. These must survive the transcode.
        let value: toml::Value = toml::from_str("a = inf\nb = -inf\nc = nan\n").unwrap();
        let yaml = toml_value_to_yaml(&value).expect("non-finite floats must transcode");
        assert_eq!(yaml.get("a").and_then(|v| v.as_f64()), Some(f64::INFINITY));
        assert_eq!(
            yaml.get("b").and_then(|v| v.as_f64()),
            Some(f64::NEG_INFINITY)
        );
        assert!(
            yaml.get("c")
                .and_then(|v| v.as_f64())
                .is_some_and(f64::is_nan)
        );
    }

    #[test]
    fn load_raw_config_value_parses_yaml_and_toml_alike() {
        let tmp = tempfile::tempdir().unwrap();
        let yaml = tmp.path().join("anodizer.yaml");
        std::fs::write(&yaml, "project_name: demo\n").unwrap();
        let toml_path = tmp.path().join("anodizer.toml");
        std::fs::write(&toml_path, "project_name = \"demo\"\n").unwrap();

        for path in [&yaml, &toml_path] {
            let raw = load_raw_config_value(path).expect("raw value");
            assert_eq!(
                raw.get("project_name").and_then(|v| v.as_str()),
                Some("demo"),
                "{} must yield the same raw value",
                path.display()
            );
        }
    }
}
