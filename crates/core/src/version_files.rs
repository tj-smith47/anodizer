//! Tag-time version-string rewriting for repo-committed files that embed the
//! release version outside `Cargo.toml` (Helm `Chart.yaml`, install docs,
//! README badges, ...).
//!
//! The `tag` command bumps `Cargo.toml` / `Cargo.lock` and creates a bump
//! commit; files enrolled via the `version_files` config are rewritten in
//! that same commit so their embedded version never drifts from the tag.
//!
//! Rewrites are word-boundary anchored so `0.1.0` does not match inside
//! `10.1.0`, and cover both the bare (`0.1.0`) and `v`-prefixed (`v0.1.0`)
//! spellings a file may carry. This module is pure: it reads and writes files
//! and returns data; it never spawns a subprocess or writes to stdout/stderr.

use std::fs;

use anyhow::{Context, Result};
use regex::Regex;

/// Outcome of rewriting one enrolled file.
///
/// `replacements == 0` means the version string was not found in the file —
/// not an error here; the caller decides whether to warn (an enrolled file
/// that does not contain the old version is usually a stale enrollment).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewriteOutcome {
    /// The enrolled file path, exactly as supplied by the caller.
    pub path: String,
    /// Number of occurrences rewritten (bare and `v`-prefixed forms combined).
    pub replacements: usize,
}

/// Build the word-boundary-anchored matcher for `version`, covering both the
/// bare form and the `v`-prefixed form. The version is `regex::escape`d so the
/// `.` separators match literally rather than as the any-character class.
fn version_regexes(version: &str) -> Result<(Regex, Regex)> {
    let escaped = regex::escape(version);
    let bare = Regex::new(&format!(r"\b{escaped}\b"))
        .with_context(|| format!("failed to build version matcher for {version:?}"))?;
    let prefixed = Regex::new(&format!(r"\bv{escaped}\b"))
        .with_context(|| format!("failed to build v-prefixed version matcher for {version:?}"))?;
    Ok((bare, prefixed))
}

/// Replace word-boundary occurrences of `old` with `new` in `content`,
/// covering both the bare and `v`-prefixed forms, and return the rewritten
/// content plus the number of replacements made.
///
/// The `v`-prefixed form is handled first so a `v`-prefixed occurrence is
/// rewritten to the `v`-prefixed new version in one pass; the `\b` anchor on
/// the bare matcher then sits between the `v` and the digit, so the bare pass
/// cannot re-touch an already-rewritten `v`-prefixed occurrence.
fn rewrite_content(content: &str, old: &str, new: &str) -> Result<(String, usize)> {
    let (bare_re, prefixed_re) = version_regexes(old)?;

    let prefixed_hits = prefixed_re.find_iter(content).count();
    let prefixed_replaced = prefixed_re
        .replace_all(content, format!("v{new}").as_str())
        .into_owned();

    let bare_hits = bare_re.find_iter(&prefixed_replaced).count();
    let bare_replaced = bare_re.replace_all(&prefixed_replaced, new).into_owned();

    Ok((bare_replaced, prefixed_hits + bare_hits))
}

/// Rewrite word-boundary occurrences of `old` with `new` in each file,
/// covering both the bare and `v`-prefixed forms. Returns one
/// [`RewriteOutcome`] per enrolled file in input order
/// (`replacements == 0` means the version was not found — the caller decides
/// how to warn). When `dry_run` is set, replacement counts are computed but no
/// file is written.
///
/// When `old == new` this is a no-op: every file reports `replacements == 0`
/// and nothing is written.
///
/// Errors if an enrolled file is missing or unreadable, or (outside
/// `dry_run`) cannot be written.
pub fn rewrite_version_in_files(
    files: &[String],
    old: &str,
    new: &str,
    dry_run: bool,
) -> Result<Vec<RewriteOutcome>> {
    let mut outcomes = Vec::with_capacity(files.len());

    if old == new {
        for path in files {
            // Read to surface a missing/unreadable enrolled file as an error
            // even when the bump is a no-op, so a stale enrollment is caught.
            fs::read_to_string(path)
                .with_context(|| format!("failed to read version file {path}"))?;
            outcomes.push(RewriteOutcome {
                path: path.clone(),
                replacements: 0,
            });
        }
        return Ok(outcomes);
    }

    for path in files {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read version file {path}"))?;
        let (rewritten, replacements) = rewrite_content(&content, old, new)?;
        if !dry_run && replacements > 0 {
            fs::write(path, &rewritten)
                .with_context(|| format!("failed to write version file {path}"))?;
        }
        outcomes.push(RewriteOutcome {
            path: path.clone(),
            replacements,
        });
    }

    Ok(outcomes)
}

/// Read-only check: for each file, whether it currently contains `version`
/// (bare or `v`-prefixed), word-boundary anchored. Returns one `(path,
/// present)` pair per file in input order.
///
/// Errors if an enrolled file is missing or unreadable.
pub fn check_version_present(files: &[String], version: &str) -> Result<Vec<(String, bool)>> {
    let (bare_re, prefixed_re) = version_regexes(version)?;
    let mut results = Vec::with_capacity(files.len());
    for path in files {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read version file {path}"))?;
        let present = bare_re.is_match(&content) || prefixed_re.is_match(&content);
        results.push((path.clone(), present));
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(dir: &TempDir, name: &str, body: &str) -> String {
        let path = dir.path().join(name);
        fs::write(&path, body).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn rewrites_bare_and_v_prefixed() {
        let dir = TempDir::new().unwrap();
        let f = write(&dir, "Chart.yaml", "version: 0.1.0\nappVersion: v0.1.0\n");
        let out =
            rewrite_version_in_files(std::slice::from_ref(&f), "0.1.0", "0.2.0", false).unwrap();
        assert_eq!(out[0].replacements, 2);
        let body = fs::read_to_string(&f).unwrap();
        assert_eq!(body, "version: 0.2.0\nappVersion: v0.2.0\n");
    }

    #[test]
    fn word_boundary_does_not_match_inside_longer_version() {
        let dir = TempDir::new().unwrap();
        let f = write(&dir, "doc.md", "use 10.1.0 not 0.1.0\n");
        let out =
            rewrite_version_in_files(std::slice::from_ref(&f), "0.1.0", "0.2.0", false).unwrap();
        assert_eq!(out[0].replacements, 1);
        assert_eq!(fs::read_to_string(&f).unwrap(), "use 10.1.0 not 0.2.0\n");
    }

    #[test]
    fn zero_matches_is_not_an_error() {
        let dir = TempDir::new().unwrap();
        let f = write(&dir, "doc.md", "no version here\n");
        let out =
            rewrite_version_in_files(std::slice::from_ref(&f), "0.1.0", "0.2.0", false).unwrap();
        assert_eq!(out[0].replacements, 0);
        assert_eq!(fs::read_to_string(&f).unwrap(), "no version here\n");
    }

    #[test]
    fn dry_run_computes_count_without_writing() {
        let dir = TempDir::new().unwrap();
        let f = write(&dir, "doc.md", "v0.1.0\n");
        let out =
            rewrite_version_in_files(std::slice::from_ref(&f), "0.1.0", "0.2.0", true).unwrap();
        assert_eq!(out[0].replacements, 1);
        assert_eq!(fs::read_to_string(&f).unwrap(), "v0.1.0\n");
    }

    #[test]
    fn equal_old_new_is_noop() {
        let dir = TempDir::new().unwrap();
        let f = write(&dir, "doc.md", "0.1.0\n");
        let out =
            rewrite_version_in_files(std::slice::from_ref(&f), "0.1.0", "0.1.0", false).unwrap();
        assert_eq!(out[0].replacements, 0);
        assert_eq!(fs::read_to_string(&f).unwrap(), "0.1.0\n");
    }

    #[test]
    fn prerelease_version_with_hyphen_rewrites() {
        let dir = TempDir::new().unwrap();
        let f = write(&dir, "doc.md", "tag v0.1.0-beta here\n");
        let out =
            rewrite_version_in_files(std::slice::from_ref(&f), "0.1.0-beta", "0.2.0-beta", false)
                .unwrap();
        assert_eq!(out[0].replacements, 1);
        assert_eq!(fs::read_to_string(&f).unwrap(), "tag v0.2.0-beta here\n");
    }

    #[test]
    fn missing_file_is_an_error() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("nope.yaml").to_string_lossy().into_owned();
        let err = rewrite_version_in_files(&[missing], "0.1.0", "0.2.0", false).unwrap_err();
        assert!(err.to_string().contains("failed to read version file"));
    }

    #[test]
    fn check_version_present_reports_per_file() {
        let dir = TempDir::new().unwrap();
        let a = write(&dir, "has.md", "v0.1.0\n");
        let b = write(&dir, "hasnot.md", "10.1.0\n");
        let res = check_version_present(&[a.clone(), b.clone()], "0.1.0").unwrap();
        assert_eq!(res, vec![(a, true), (b, false)]);
    }

    #[test]
    fn multiple_files_reported_in_input_order() {
        let dir = TempDir::new().unwrap();
        let a = write(&dir, "a.md", "0.1.0\n0.1.0\n");
        let b = write(&dir, "b.md", "nothing\n");
        let out =
            rewrite_version_in_files(&[a.clone(), b.clone()], "0.1.0", "0.2.0", false).unwrap();
        assert_eq!(out[0].path, a);
        assert_eq!(out[0].replacements, 2);
        assert_eq!(out[1].path, b);
        assert_eq!(out[1].replacements, 0);
    }
}
