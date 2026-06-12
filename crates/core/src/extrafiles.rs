//! Shared `extra_files` glob resolution.
//!
//! Canonical implementation of extra-file glob resolution —
//! a single resolver used by every pipe that accepts an `extra_files:` config
//! (checksum, blob upload, custom publisher, artifactory/fury/cloudsmith,
//! release body uploads).
//!
//! Semantics:
//! - An empty glob (after template rendering) emits a warning and is skipped.
//! - Glob expansion errors bubble up as `Err`.
//! - If a `name_template` is set on a spec, the glob must match **exactly one**
//!   file — multi-match with a name template is an error (you can't give many
//!   files the same overridden name).
//! - Directory entries are filtered out.
//! - Duplicate paths across multiple specs are deduplicated (first wins).
//! - The returned list is sorted by path for deterministic output.

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::config::ExtraFileSpec;
use crate::log::StageLogger;

/// One resolved `extra_files` entry.
///
/// `name_template` is the raw, unrendered template string from the user's
/// config (e.g. `"{{ .ProjectName }}.txt"`). Callers render it with their own
/// `TemplateVars` so they can inject per-site variables (e.g. stage-blob sets
/// a `Filename` var so users can write `"renamed-{{ .Filename }}"`).
#[derive(Debug, Clone)]
pub struct ResolvedExtraFile {
    pub path: PathBuf,
    pub name_template: Option<String>,
}

/// Resolve a list of `ExtraFileSpec`s into deduplicated, path-sorted resolved
/// entries. See module docs for the full semantic.
pub fn resolve(specs: &[ExtraFileSpec], log: &StageLogger) -> Result<Vec<ResolvedExtraFile>> {
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut out: Vec<ResolvedExtraFile> = Vec::new();

    for spec in specs {
        let pattern = spec.glob();
        let name_tmpl = spec.name_template().map(str::to_owned);

        if pattern.is_empty() {
            log.warn("ignoring empty extra_files glob");
            continue;
        }

        let matches: Vec<PathBuf> = glob::glob(pattern)
            .with_context(|| format!("extra_files: invalid glob '{pattern}'"))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| format!("extra_files: error expanding glob '{pattern}'"))?;

        if matches.is_empty() {
            log.warn(&format!(
                "extra_files glob '{pattern}' matched no files, skipping"
            ));
            continue;
        }

        if name_tmpl.is_some() && matches.len() > 1 {
            bail!(
                "extra_files: glob '{}' with name_template matched {} files (must match exactly one)",
                pattern,
                matches.len()
            );
        }

        for path in matches.into_iter().filter(|p| p.is_file()) {
            if seen.insert(path.clone()) {
                out.push(ResolvedExtraFile {
                    path,
                    name_template: name_tmpl.clone(),
                });
            }
        }
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn log() -> StageLogger {
        StageLogger::new("test", crate::log::Verbosity::Quiet)
    }

    #[test]
    fn empty_specs_returns_empty() {
        let result = resolve(&[], &log()).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn empty_glob_is_skipped() {
        let specs = vec![ExtraFileSpec::Glob(String::new())];
        let result = resolve(&specs, &log()).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn no_match_is_skipped_not_error() {
        let specs = vec![ExtraFileSpec::Glob(
            "/tmp/nonexistent-prefix-xyz-*.bin".to_string(),
        )];
        let result = resolve(&specs, &log()).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn multi_match_with_name_template_errors() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("b.bin"), b"b").unwrap();

        let glob_pattern = format!("{}/*.bin", tmp.path().display());
        let specs = vec![ExtraFileSpec::Detailed {
            glob: glob_pattern,
            name_template: Some("collapsed.bin".to_string()),
            allow_empty: false,
        }];

        let err = resolve(&specs, &log()).unwrap_err();
        assert!(err.to_string().contains("must match exactly one"));
    }

    #[test]
    fn dedupes_across_specs() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.bin"), b"a").unwrap();

        let glob1 = format!("{}/*.bin", tmp.path().display());
        let glob2 = format!("{}/a.bin", tmp.path().display());
        let specs = vec![ExtraFileSpec::Glob(glob1), ExtraFileSpec::Glob(glob2)];

        let result = resolve(&specs, &log()).unwrap();
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn results_sorted_by_path() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("c.bin"), b"c").unwrap();
        std::fs::write(tmp.path().join("a.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("b.bin"), b"b").unwrap();

        let specs = vec![ExtraFileSpec::Glob(format!(
            "{}/*.bin",
            tmp.path().display()
        ))];
        let result = resolve(&specs, &log()).unwrap();
        assert_eq!(result.len(), 3);
        assert!(result[0].path.to_string_lossy().ends_with("a.bin"));
        assert!(result[1].path.to_string_lossy().ends_with("b.bin"));
        assert!(result[2].path.to_string_lossy().ends_with("c.bin"));
    }

    #[test]
    fn directories_filtered_out() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("subdir")).unwrap();
        std::fs::write(tmp.path().join("real.bin"), b"x").unwrap();

        let specs = vec![ExtraFileSpec::Glob(format!("{}/*", tmp.path().display()))];
        let result = resolve(&specs, &log()).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].path.to_string_lossy().ends_with("real.bin"));
    }
}
