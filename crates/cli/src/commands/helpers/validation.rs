use anyhow::Result;
use std::path::{Path, PathBuf};

/// Parse a comma-separated list (e.g. `--targets=a,b,c` or `--stages=x,y`)
/// into the canonical `Option<Vec<String>>` form.
///
/// - `None`           → `None` (no filter).
/// - `Some("a,b")`    → `Some(["a", "b"])`.
/// - Empty / whitespace-only tokens (trailing comma, double comma,
///   surrounding spaces) are dropped — they're noise, not intent.
/// - `Some("")` or `Some(" , ")` (all-empty after trimming) → `Err`. The
///   operator clearly meant to pass *something*; surfacing the typo
///   beats silently degrading into a no-op filter.
///
/// `flag_help` is the `--flag=<example>` snippet appended to the error so
/// each call site gets a copy-pasteable hint specific to its CSV shape.
pub(crate) fn parse_csv_list(
    raw: Option<&str>,
    flag_help: &str,
) -> Result<Option<Vec<String>>, String> {
    match raw {
        None => Ok(None),
        Some(list) => {
            let parsed: Vec<String> = list
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            if parsed.is_empty() {
                return Err(format!(
                    "{flag_help} must list at least one entry (got empty / whitespace-only input)"
                ));
            }
            Ok(Some(parsed))
        }
    }
}

/// Walk an artifact path iterator and fail if any path appears more than
/// once. Used by post-load manifest validators (publish-only's per-shard
/// merge, `release --merge`'s split-worker merge) to surface accidental
/// shard overlap as a hard error rather than a silent double-publish
/// downstream.
pub(crate) fn detect_duplicate_paths<'a, I>(paths: I) -> Result<()>
where
    I: IntoIterator<Item = &'a Path>,
{
    use std::collections::BTreeMap;
    let mut counts: BTreeMap<PathBuf, usize> = BTreeMap::new();
    for p in paths {
        *counts.entry(p.to_path_buf()).or_insert(0) += 1;
    }
    let duplicates: Vec<(PathBuf, usize)> = counts.into_iter().filter(|(_, n)| *n > 1).collect();
    if duplicates.is_empty() {
        return Ok(());
    }
    let summary = duplicates
        .iter()
        .map(|(p, n)| format!("{} ({}×)", p.display(), n))
        .collect::<Vec<_>>()
        .join(", ");
    anyhow::bail!(
        "duplicate artifact path(s) after merging per-shard manifests: {summary}. \
         Hypothesis: two shards overlapped on the same target, so both \
         emitted an artifact for the same path. Inspect the matrix in \
         `.github/workflows/release.yml` (or the equivalent dispatcher) \
         to confirm the shards partition the target set."
    );
}

/// Walk an artifact path iterator and verify each file exists on disk
/// under `dist/`. Tries the literal path first (absolute or relative),
/// then `dist.join(<path>)`. Missing files are fatal so SignStage /
/// ChecksumStage emit an operator-friendly manifest-shaped diagnostic
/// rather than cosign / gpg's less actionable "file not found".
///
/// Files in `dist/` that are *absent* from the manifest are not flagged
/// — dist trees carry metadata.json, harness logs, etc. that aren't
/// part of the artifact registry.
pub(crate) fn detect_missing_files<'a, I>(paths: I, dist: &Path) -> Result<()>
where
    I: IntoIterator<Item = &'a Path>,
{
    let mut missing: Vec<PathBuf> = Vec::new();
    for p in paths {
        if p.is_absolute() {
            if !p.is_file() {
                missing.push(p.to_path_buf());
            }
        } else if !p.is_file() && !dist.join(p).is_file() {
            missing.push(p.to_path_buf());
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    missing.sort();
    let summary = missing
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    anyhow::bail!(
        "artifacts manifest references file(s) not present under {}: {summary}. \
         The preserved dist is incomplete; re-run \
         `anodize check determinism --preserve-dist=<dist>` to repopulate, or \
         remove the stale manifest entries before retrying.",
        dist.display(),
    );
}
