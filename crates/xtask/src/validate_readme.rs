//! Extract every YAML code block from each README in the workspace and
//! attempt to parse each as an anodizer [`Config`].
//!
//! Not every YAML block in a README is an anodizer config — some are GitHub
//! Actions workflows, shell sessions, or CLI references. The validator
//! silently skips blocks that look like GitHub Actions (`on:`, `jobs:`,
//! `name:` without any anodizer-specific keys) and only hard-fails on blocks
//! that contain at least one anodizer-shaped key but fail Config deserialization,
//! or that load with a deprecation warning: a README example is copied as
//! written, so it must use the current form of every field.
//!
//! Run via `cargo xtask validate-readme` or `task docs:validate-readme`.

use anodizer_core::config::Config;
use anodizer_core::text::count_noun;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing_subscriber::layer::SubscriberExt as _;

/// READMEs that ship with the workspace and may contain anodizer YAML
/// config examples. Each entry is a path relative to the workspace root.
///
/// Add new READMEs explicitly here so the validator's scope is auditable
/// at a glance and CI doesn't silently pick up unrelated READMEs that
/// happen to contain GitHub Actions YAML.
const DEFAULT_READMES: &[&str] = &["README.md", "crates/cli/README.md"];

/// Resolve the workspace root from xtask's own manifest directory.
/// xtask lives at `crates/xtask/`, so going up two levels ends up at the
/// workspace root regardless of where `cargo xtask` is invoked from.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("xtask manifest must live two levels below workspace root")
        .to_path_buf()
}

/// Keys that signal a YAML block is likely an anodizer config rather than a
/// GitHub Actions workflow or other YAML document. A block without any of
/// these top-level keys is skipped.
const ANODIZER_KEYS: &[&str] = &[
    "project_name",
    "crates",
    "defaults",
    "signs",
    "checksum",
    "archives",
    "snapshot",
    "release",
    "publish",
    "announce",
    "source",
    "docker",
    "sbom",
];

/// Keys that mark a block as a GitHub Actions workflow. Blocks containing
/// these are always skipped regardless of other content.
const GITHUB_ACTIONS_KEYS: &[&str] = &["jobs", "on", "steps", "runs-on", "uses"];

/// Extract fenced YAML code blocks (```yaml … ```) from `content`.
/// Returns a vec of `(block_index, block_content)` where `block_index`
/// is 1-based for error messages.
fn extract_yaml_blocks(content: &str) -> Vec<(usize, String)> {
    let mut blocks = Vec::new();
    let mut idx = 0usize;
    let mut in_block = false;
    let mut current = String::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if !in_block {
            if trimmed == "```yaml" || trimmed == "```yml" {
                in_block = true;
                current.clear();
            }
        } else if trimmed == "```" {
            in_block = false;
            idx += 1;
            blocks.push((idx, current.clone()));
        } else {
            current.push_str(line);
            current.push('\n');
        }
    }

    blocks
}

/// Return true if the block looks like a GitHub Actions workflow that should
/// be skipped.
fn is_github_actions_block(value: &serde_yaml_ng::Value) -> bool {
    let map = match value.as_mapping() {
        Some(m) => m,
        None => return false,
    };
    GITHUB_ACTIONS_KEYS
        .iter()
        .any(|key| map.contains_key(serde_yaml_ng::Value::String(key.to_string())))
}

/// Return true if the block contains at least one anodizer-shaped top-level
/// key, indicating it should be validated as a Config.
fn looks_like_anodizer_config(value: &serde_yaml_ng::Value) -> bool {
    let map = match value.as_mapping() {
        Some(m) => m,
        None => return false,
    };
    ANODIZER_KEYS
        .iter()
        .any(|key| map.contains_key(serde_yaml_ng::Value::String(key.to_string())))
}

/// Per-README validation result. Errors accumulate across the whole run
/// so a single CI invocation reports every broken block at once instead
/// of failing on the first.
struct ReadmeResult {
    validated: usize,
    skipped: usize,
    errors: Vec<String>,
}

fn validate_readme(readme_path: &Path) -> ReadmeResult {
    let mut result = ReadmeResult {
        validated: 0,
        skipped: 0,
        errors: Vec::new(),
    };

    let content = match std::fs::read_to_string(readme_path) {
        Ok(c) => c,
        Err(e) => {
            result
                .errors
                .push(format!("failed to read {}: {e}", readme_path.display()));
            return result;
        }
    };

    let blocks = extract_yaml_blocks(&content);
    if blocks.is_empty() {
        println!(
            "validate-readme: no YAML blocks found in {}",
            readme_path.display()
        );
        return result;
    }

    for (idx, block) in &blocks {
        let value: serde_yaml_ng::Value = match serde_yaml_ng::from_str(block) {
            Ok(v) => v,
            Err(e) => {
                result.errors.push(format!(
                    "YAML block #{idx} in {}: YAML parse error: {e}",
                    readme_path.display()
                ));
                continue;
            }
        };

        if is_github_actions_block(&value) {
            result.skipped += 1;
            println!(
                "validate-readme: {} block #{idx} — skipped (GitHub Actions workflow)",
                readme_path.display()
            );
            continue;
        }

        if !looks_like_anodizer_config(&value) {
            result.skipped += 1;
            println!(
                "validate-readme: {} block #{idx} — skipped (no anodizer-shaped keys)",
                readme_path.display()
            );
            continue;
        }

        match load_with_warnings(block, &value) {
            Ok(warnings) if warnings.is_empty() => {
                result.validated += 1;
                println!(
                    "validate-readme: {} block #{idx} — OK",
                    readme_path.display()
                );
            }
            Ok(warnings) => {
                result.errors.push(format!(
                    "YAML block #{idx} in {}: loads with a deprecation warning; \
                     rewrite it in the current form:\n{}\nBlock content:\n{block}",
                    readme_path.display(),
                    warnings.join("\n")
                ));
            }
            Err(e) => {
                result.errors.push(format!(
                    "YAML block #{idx} in {}: Config parse error: {e}\n\
                     Block content:\n{block}",
                    readme_path.display()
                ));
            }
        }
    }

    result
}

/// Collects the message of every WARN event emitted while it is installed.
struct WarnCollector(Arc<Mutex<Vec<String>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for WarnCollector {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        struct Message(String);
        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }
        if *event.metadata().level() != tracing::Level::WARN {
            return;
        }
        let mut msg = Message(String::new());
        event.record(&mut msg);
        self.0.lock().expect("warning sink").push(msg.0);
    }
}

/// Parse `block` as a [`Config`] and run every load-time legacy check the
/// config loader runs, returning the deprecation warnings they print.
fn load_with_warnings(
    block: &str,
    raw: &serde_yaml_ng::Value,
) -> Result<Vec<String>, serde_yaml_ng::Error> {
    use anodizer_core::config as cfg;
    let sink = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(WarnCollector(Arc::clone(&sink)));
    let cask_warnings = tracing::subscriber::with_default(subscriber, || {
        cfg::warn_on_legacy_snapshot_name_template(raw);
        cfg::warn_on_legacy_furies_alias(raw);
        cfg::warn_on_legacy_nfpm_builds(raw);
        cfg::warn_on_legacy_disable_alias(raw);
        let mut config = serde_yaml_ng::from_str::<Config>(block)?;
        cfg::warn_on_legacy_homebrew_formula(&config);
        cfg::warn_on_legacy_docker_retry(&config);
        Ok::<_, serde_yaml_ng::Error>(cfg::homebrew_cask_legacy_warnings(&mut config))
    })?;
    let mut warnings = std::mem::take(&mut *sink.lock().expect("warning sink"));
    warnings.extend(cask_warnings);
    Ok(warnings)
}

pub fn run(readme: Option<&Path>) -> Result<(), String> {
    // Single-file override (e.g. `cargo xtask validate-readme --readme foo.md`)
    // takes precedence; otherwise validate every entry in DEFAULT_READMES
    // resolved relative to the workspace root.
    let paths: Vec<PathBuf> = match readme {
        Some(p) => vec![p.to_path_buf()],
        None => {
            let root = workspace_root();
            DEFAULT_READMES.iter().map(|r| root.join(r)).collect()
        }
    };

    let mut total_validated = 0usize;
    let mut total_skipped = 0usize;
    let mut all_errors: Vec<String> = Vec::new();

    for path in &paths {
        let r = validate_readme(path);
        total_validated += r.validated;
        total_skipped += r.skipped;
        all_errors.extend(r.errors);
    }

    println!(
        "validate-readme: {} validated across {}, {total_skipped} skipped, {}",
        count_noun(
            total_validated,
            "anodizer config block",
            "anodizer config blocks"
        ),
        count_noun(paths.len(), "file", "files"),
        count_noun(all_errors.len(), "error", "errors")
    );

    if !all_errors.is_empty() {
        for e in &all_errors {
            eprintln!("error: {e}");
        }
        return Err(format!(
            "{} failed validation — update the READMEs to match the current schema",
            count_noun(all_errors.len(), "README YAML block", "README YAML blocks")
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn warnings_of(block: &str) -> Vec<String> {
        let raw: serde_yaml_ng::Value = serde_yaml_ng::from_str(block).unwrap();
        load_with_warnings(block, &raw).unwrap()
    }

    #[test]
    fn a_publish_homebrew_formula_block_is_reported() {
        let warnings = warnings_of(
            "crates:\n  - name: a\n    path: .\n    tag_template: v1\n    publish:\n      homebrew:\n        repository:\n          owner: o\n          name: t\n",
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("publish.homebrew (Homebrew Formula) is deprecated"));
    }

    #[test]
    fn a_publish_homebrew_cask_block_loads_clean() {
        let warnings = warnings_of(
            "crates:\n  - name: a\n    path: .\n    tag_template: v1\n    publish:\n      homebrew_cask:\n        repository:\n          owner: o\n          name: t\n        binaries: [a]\n",
        );
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn a_raw_yaml_legacy_alias_is_reported() {
        let warnings = warnings_of("snapshot:\n  name_template: x\n");
        assert!(!warnings.is_empty());
    }
}
