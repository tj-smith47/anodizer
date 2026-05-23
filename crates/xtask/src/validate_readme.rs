//! Extract every YAML code block from `README.md` and attempt to parse each
//! as an anodizer [`Config`].
//!
//! Not every YAML block in the README is an anodizer config — some are GitHub
//! Actions workflows, shell sessions, or CLI references. The validator
//! silently skips blocks that look like GitHub Actions (`on:`, `jobs:`,
//! `name:` without any anodizer-specific keys) and only hard-fails on blocks
//! that contain at least one anodizer-shaped key but fail Config deserialization.
//!
//! Run via `cargo xtask validate-readme` or `task docs:validate-readme`.

use anodizer_core::config::Config;
use std::path::Path;

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

pub fn run(readme: Option<&Path>) -> Result<(), String> {
    let readme_path = readme.unwrap_or_else(|| Path::new("README.md"));
    let content = std::fs::read_to_string(readme_path)
        .map_err(|e| format!("failed to read {}: {e}", readme_path.display()))?;

    let blocks = extract_yaml_blocks(&content);
    if blocks.is_empty() {
        println!(
            "validate-readme: no YAML blocks found in {}",
            readme_path.display()
        );
        return Ok(());
    }

    let mut validated = 0usize;
    let mut skipped = 0usize;
    let mut errors: Vec<String> = Vec::new();

    for (idx, block) in &blocks {
        // Parse as generic YAML first to inspect structure.
        let value: serde_yaml_ng::Value = match serde_yaml_ng::from_str(block) {
            Ok(v) => v,
            Err(e) => {
                // YAML parse failure is always reported.
                errors.push(format!(
                    "YAML block #{idx} in {}: YAML parse error: {e}",
                    readme_path.display()
                ));
                continue;
            }
        };

        if is_github_actions_block(&value) {
            skipped += 1;
            println!("validate-readme: block #{idx} — skipped (GitHub Actions workflow)");
            continue;
        }

        if !looks_like_anodizer_config(&value) {
            skipped += 1;
            println!("validate-readme: block #{idx} — skipped (no anodizer-shaped keys)");
            continue;
        }

        // Try to deserialize as Config.
        match serde_yaml_ng::from_str::<Config>(block) {
            Ok(_) => {
                validated += 1;
                println!("validate-readme: block #{idx} — OK");
            }
            Err(e) => {
                errors.push(format!(
                    "YAML block #{idx} in {}: Config parse error: {e}\n\
                     Block content:\n{block}",
                    readme_path.display()
                ));
            }
        }
    }

    println!(
        "validate-readme: {validated} anodizer config block(s) validated, {skipped} skipped, {} error(s)",
        errors.len()
    );

    if !errors.is_empty() {
        for e in &errors {
            eprintln!("error: {e}");
        }
        return Err(format!(
            "{} README YAML block(s) failed validation — update the README to match the current schema",
            errors.len()
        ));
    }

    Ok(())
}
