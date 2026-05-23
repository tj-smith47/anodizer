//! Extract every YAML code block from each README in the workspace and
//! attempt to parse each as an anodizer [`Config`].
//!
//! Not every YAML block in a README is an anodizer config — some are GitHub
//! Actions workflows, shell sessions, or CLI references. The validator
//! silently skips blocks that look like GitHub Actions (`on:`, `jobs:`,
//! `name:` without any anodizer-specific keys) and only hard-fails on blocks
//! that contain at least one anodizer-shaped key but fail Config deserialization.
//!
//! Run via `cargo xtask validate-readme` or `task docs:validate-readme`.

use anodizer_core::config::Config;
use std::path::{Path, PathBuf};

/// READMEs that ship with the workspace and may contain anodizer YAML
/// config examples. Each entry is a path relative to the workspace root.
///
/// Add new READMEs explicitly here so the validator's scope is auditable
/// at a glance and CI doesn't silently pick up unrelated READMEs that
/// happen to contain GitHub Actions YAML.
const DEFAULT_READMES: &[&str] = &["README.md", "crates/cli/README.md"];

/// Resolve the workspace root from xtask's own manifest directory.
/// xtask lives at `crates/xtask/`, so going up two levels lands at the
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
/// so a single CI invocation surfaces every broken block at once instead
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

        match serde_yaml_ng::from_str::<Config>(block) {
            Ok(_) => {
                result.validated += 1;
                println!(
                    "validate-readme: {} block #{idx} — OK",
                    readme_path.display()
                );
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
        "validate-readme: {total_validated} anodizer config block(s) validated across {} file(s), \
         {total_skipped} skipped, {} error(s)",
        paths.len(),
        all_errors.len()
    );

    if !all_errors.is_empty() {
        for e in &all_errors {
            eprintln!("error: {e}");
        }
        return Err(format!(
            "{} README YAML block(s) failed validation — update the README(s) to match the current schema",
            all_errors.len()
        ));
    }

    Ok(())
}
