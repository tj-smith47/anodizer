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
//! spellings a file may carry. An enrollment may additionally carry a `match`
//! anchor, scoping its rewrite to the regions that regex selects so two crates
//! can share one file — even one literal. This module is pure: it reads and
//! writes files and returns data; it never spawns a subprocess or writes to
//! stdout/stderr.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use regex::Regex;

/// Outcome of rewriting one enrolled entry.
///
/// `replacements == 0` means the version string was not found — not an error
/// for a bare entry; the caller decides whether to warn (an enrolled file that
/// does not contain the old version is usually a stale enrollment). An anchored
/// entry that selects no region is an error and never reaches an outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewriteOutcome {
    /// The enrolled file path, exactly as supplied by the caller.
    pub path: String,
    /// The entry's `match` anchor, or `None` for a bare (whole-file) entry.
    pub anchor: Option<String>,
    /// Number of occurrences rewritten (bare and `v`-prefixed forms combined).
    pub replacements: usize,
    /// Regions the `match` anchor selected, or `None` for a bare entry.
    pub matched_regions: Option<usize>,
}

/// One planned rewrite: `path` (already resolved for IO), the optional `match`
/// anchor, the `old` → `new` pair, and a label naming the enrolling crate for
/// the unmatched-anchor error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRewrite {
    /// Path to read and write, resolved by the caller.
    pub path: String,
    /// Regex scoping this rewrite to its own occurrences, or `None` to sweep
    /// the whole file.
    pub anchor: Option<String>,
    /// Version being rewritten away.
    pub old: String,
    /// Version being rewritten in.
    pub new: String,
    /// Name of the crate that enrolled this entry.
    pub owner: String,
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

/// Compile a `version_files` `match` anchor into a matcher for `version`: the
/// literal token `{version}` is replaced with the regex-escaped version and
/// everything else is used as written, so regex quantifiers such as `\d{2}` are
/// left alone. `path` names the enrolled file in the error messages.
///
/// The single source of truth for `{version}` expansion — the `tag` rewrite and
/// the `check version-files` drift guard both compile anchors here.
///
/// Errors when the anchor omits `{version}` (an anchor that cannot be verified
/// would silently widen to a region matcher) or is not a valid regex.
pub fn anchor_regex(path: &str, anchor: &str, version: &str) -> Result<Regex> {
    if !anchor.contains("{version}") {
        bail!(
            "version_files anchor for {path} must contain the {{version}} placeholder (got {anchor:?})"
        );
    }
    let pattern = anchor.replace("{version}", &regex::escape(version));
    Regex::new(&pattern)
        .map_err(|e| anyhow::anyhow!("version_files anchor for {path} is not a valid regex: {e}"))
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

/// Rewrite `old` → `new` inside every region `re` selects, splicing the
/// rewritten regions back into `content`. Returns the new content, the total
/// replacement count, and the number of regions the anchor matched.
///
/// The in-region rewrite is [`rewrite_content`], so an anchored entry and a
/// bare one apply the same word-boundary matcher.
fn rewrite_anchored(
    content: &str,
    re: &Regex,
    old: &str,
    new: &str,
) -> Result<(String, usize, usize)> {
    let mut out = String::with_capacity(content.len());
    let mut cursor = 0usize;
    let mut replacements = 0usize;
    let mut regions = 0usize;
    for m in re.find_iter(content) {
        regions += 1;
        out.push_str(&content[cursor..m.start()]);
        let (rewritten, n) = rewrite_content(m.as_str(), old, new)?;
        out.push_str(&rewritten);
        replacements += n;
        cursor = m.end();
    }
    out.push_str(&content[cursor..]);
    Ok((out, replacements, regions))
}

/// Apply every planned rewrite. All files are read and rewritten IN MEMORY
/// first; an anchored entry that matched nothing errors before any file is
/// written, so a half-applied bump is impossible. Returns one
/// [`RewriteOutcome`] per input entry, in input order (`replacements == 0` on a
/// bare entry means the version was not found — the caller decides how to
/// warn). When `dry_run` is set, counts are computed but no file is written.
///
/// Rewrites touching the same file are applied longest-`old`-first, so a
/// shorter `old` that is a word-boundary prefix of a longer one (`0.1.0` inside
/// `0.1.0-rc1`) cannot consume it.
///
/// An entry whose `old` equals its `new` is a no-op reporting zero
/// replacements; the file is still read, so a stale enrollment pointing at a
/// missing file is caught.
///
/// Errors if an enrolled file is missing or unreadable, if an anchor is invalid
/// or matches nothing, or (outside `dry_run`) if a file cannot be written.
pub fn rewrite_version_in_files(
    rewrites: &[FileRewrite],
    dry_run: bool,
) -> Result<Vec<RewriteOutcome>> {
    // Entry indices grouped by path in first-seen order, so one file is read
    // once, rewritten by every entry that names it, and written once.
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    for (idx, rewrite) in rewrites.iter().enumerate() {
        match groups.iter_mut().find(|(path, _)| path == &rewrite.path) {
            Some((_, indices)) => indices.push(idx),
            None => groups.push((rewrite.path.clone(), vec![idx])),
        }
    }

    let mut counts: Vec<(usize, Option<usize>)> = vec![(0, None); rewrites.len()];
    let mut pending: Vec<(String, String)> = Vec::new();

    for (path, mut indices) in groups {
        let original = fs::read_to_string(&path)
            .with_context(|| format!("failed to read version file {path}"))?;
        indices.sort_by_key(|idx| std::cmp::Reverse(rewrites[*idx].old.len()));

        let mut current = original.clone();
        for idx in indices {
            let rewrite = &rewrites[idx];
            if rewrite.old == rewrite.new {
                continue;
            }
            match rewrite.anchor.as_deref() {
                None => {
                    let (next, replacements) =
                        rewrite_content(&current, &rewrite.old, &rewrite.new)?;
                    current = next;
                    counts[idx] = (replacements, None);
                }
                Some(anchor) => {
                    let re = anchor_regex(&rewrite.path, anchor, &rewrite.old)?;
                    let (next, replacements, regions) =
                        rewrite_anchored(&current, &re, &rewrite.old, &rewrite.new)?;
                    if regions == 0 {
                        bail!(
                            "version_files: crate '{}' enrolled {} with match {:?} but it matched \
                             nothing (expected version {}); fix the anchor or remove the enrollment",
                            rewrite.owner,
                            rewrite.path,
                            anchor,
                            rewrite.old,
                        );
                    }
                    current = next;
                    counts[idx] = (replacements, Some(regions));
                }
            }
        }
        if current != original {
            pending.push((path, current));
        }
    }

    if !dry_run {
        for (path, body) in pending {
            crate::fs_atomic::atomic_write_str(Path::new(&path), &body)
                .with_context(|| format!("failed to write version file {path}"))?;
        }
    }

    Ok(rewrites
        .iter()
        .zip(counts)
        .map(
            |(rewrite, (replacements, matched_regions))| RewriteOutcome {
                path: rewrite.path.clone(),
                anchor: rewrite.anchor.clone(),
                replacements,
                matched_regions,
            },
        )
        .collect())
}

/// Whether `content` contains `version` (bare or `v`-prefixed), word-boundary
/// anchored — the same matcher [`check_version_present`] and
/// [`rewrite_version_in_files`] apply, exposed so the enrollment-discovery flow
/// (`anodizer init --version-files`) can probe a candidate file's text, and so
/// the tag-time conflict guard can ask whether one crate's new version would be
/// re-matched by another's old version, with one shared regex rather than a
/// second copy of the boundary logic.
///
/// Errors only if `version` cannot be compiled into a matcher.
pub fn contains_version(content: &str, version: &str) -> Result<bool> {
    let (bare_re, prefixed_re) = version_regexes(version)?;
    Ok(bare_re.is_match(content) || prefixed_re.is_match(content))
}

/// Read-only check: for each `(path, anchor)` entry, whether the file currently
/// contains `version`. A bare entry (`anchor` is `None`) matches anywhere in the
/// file, bare or `v`-prefixed and word-boundary anchored; an anchored entry is
/// present only when its `match` regex — compiled against `version` — selects a
/// region. Returns one `(path, present)` pair per entry in input order.
///
/// Errors if an enrolled file is missing or unreadable, or an anchor is invalid.
pub fn check_version_present(
    entries: &[(String, Option<String>)],
    version: &str,
) -> Result<Vec<(String, bool)>> {
    let (bare_re, prefixed_re) = version_regexes(version)?;
    let mut results = Vec::with_capacity(entries.len());
    for (path, anchor) in entries {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read version file {path}"))?;
        let present = match anchor {
            Some(anchor) => anchor_regex(path, anchor, version)?.is_match(&content),
            None => bare_re.is_match(&content) || prefixed_re.is_match(&content),
        };
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

    fn bare(path: &str, old: &str, new: &str) -> FileRewrite {
        FileRewrite {
            path: path.to_string(),
            anchor: None,
            old: old.to_string(),
            new: new.to_string(),
            owner: "app".to_string(),
        }
    }

    fn anchored(path: &str, anchor: &str, old: &str, new: &str) -> FileRewrite {
        FileRewrite {
            anchor: Some(anchor.to_string()),
            ..bare(path, old, new)
        }
    }

    #[test]
    fn rewrites_bare_and_v_prefixed() {
        let dir = TempDir::new().unwrap();
        let f = write(&dir, "Chart.yaml", "version: 0.1.0\nappVersion: v0.1.0\n");
        let out = rewrite_version_in_files(&[bare(&f, "0.1.0", "0.2.0")], false).unwrap();
        assert_eq!(out[0].replacements, 2);
        let body = fs::read_to_string(&f).unwrap();
        assert_eq!(body, "version: 0.2.0\nappVersion: v0.2.0\n");
    }

    #[test]
    fn word_boundary_does_not_match_inside_longer_version() {
        let dir = TempDir::new().unwrap();
        let f = write(&dir, "doc.md", "use 10.1.0 not 0.1.0\n");
        let out = rewrite_version_in_files(&[bare(&f, "0.1.0", "0.2.0")], false).unwrap();
        assert_eq!(out[0].replacements, 1);
        assert_eq!(fs::read_to_string(&f).unwrap(), "use 10.1.0 not 0.2.0\n");
    }

    #[test]
    fn zero_matches_is_not_an_error() {
        let dir = TempDir::new().unwrap();
        let f = write(&dir, "doc.md", "no version here\n");
        let out = rewrite_version_in_files(&[bare(&f, "0.1.0", "0.2.0")], false).unwrap();
        assert_eq!(out[0].replacements, 0);
        assert_eq!(fs::read_to_string(&f).unwrap(), "no version here\n");
    }

    #[test]
    fn dry_run_computes_count_without_writing() {
        let dir = TempDir::new().unwrap();
        let f = write(&dir, "doc.md", "v0.1.0\n");
        let out = rewrite_version_in_files(&[bare(&f, "0.1.0", "0.2.0")], true).unwrap();
        assert_eq!(out[0].replacements, 1);
        assert_eq!(fs::read_to_string(&f).unwrap(), "v0.1.0\n");
    }

    #[test]
    fn equal_old_new_is_noop() {
        let dir = TempDir::new().unwrap();
        let f = write(&dir, "doc.md", "0.1.0\n");
        let out = rewrite_version_in_files(&[bare(&f, "0.1.0", "0.1.0")], false).unwrap();
        assert_eq!(out[0].replacements, 0);
        assert_eq!(fs::read_to_string(&f).unwrap(), "0.1.0\n");
    }

    #[test]
    fn prerelease_version_with_hyphen_rewrites() {
        let dir = TempDir::new().unwrap();
        let f = write(&dir, "doc.md", "tag v0.1.0-beta here\n");
        let out = rewrite_version_in_files(&[bare(&f, "0.1.0-beta", "0.2.0-beta")], false).unwrap();
        assert_eq!(out[0].replacements, 1);
        assert_eq!(fs::read_to_string(&f).unwrap(), "tag v0.2.0-beta here\n");
    }

    /// Pin the raw engine behavior at the prerelease boundary: a hyphen is a
    /// non-word char, so `\b` sits between the trailing digit and the `-`, and
    /// the bare `old = "0.1.0"` matcher DOES fire inside `0.1.0-rc1`, rewriting
    /// the release core and leaving the `-rc1` suffix intact (→ `0.2.0-rc1`).
    ///
    /// This is an engine-level edge that production never reaches: at tag time
    /// `old` is reconstructed by `bare_version_from_tag`, which carries the full
    /// `0.1.0-rc1` prerelease string, so the bare `0.1.0` form is never the
    /// `old` applied to a prerelease line. The test exists so that boundary
    /// behavior can't regress silently if the regex anchoring ever changes.
    #[test]
    fn bare_old_matches_release_core_of_a_prerelease_line() {
        let dir = TempDir::new().unwrap();
        let f = write(&dir, "doc.md", "pinned at 0.1.0-rc1 today\n");
        let out = rewrite_version_in_files(&[bare(&f, "0.1.0", "0.2.0")], false).unwrap();
        assert_eq!(out[0].replacements, 1);
        assert_eq!(
            fs::read_to_string(&f).unwrap(),
            "pinned at 0.2.0-rc1 today\n"
        );
    }

    #[test]
    fn missing_file_is_an_error() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("nope.yaml").to_string_lossy().into_owned();
        let err = rewrite_version_in_files(&[bare(&missing, "0.1.0", "0.2.0")], false).unwrap_err();
        assert!(err.to_string().contains("failed to read version file"));
    }

    #[test]
    fn contains_version_matches_bare_and_v_prefixed() {
        assert!(contains_version("appVersion: 0.1.0\n", "0.1.0").unwrap());
        assert!(contains_version("tag v0.1.0 here\n", "0.1.0").unwrap());
    }

    #[test]
    fn contains_version_respects_word_boundary() {
        assert!(!contains_version("pinned 10.1.0\n", "0.1.0").unwrap());
        assert!(!contains_version("no version here\n", "0.1.0").unwrap());
    }

    #[test]
    fn check_version_present_reports_per_file() {
        let dir = TempDir::new().unwrap();
        let a = write(&dir, "has.md", "v0.1.0\n");
        let b = write(&dir, "hasnot.md", "10.1.0\n");
        let res = check_version_present(&[(a.clone(), None), (b.clone(), None)], "0.1.0").unwrap();
        assert_eq!(res, vec![(a, true), (b, false)]);
    }

    #[test]
    fn multiple_files_reported_in_input_order() {
        let dir = TempDir::new().unwrap();
        let a = write(&dir, "a.md", "0.1.0\n0.1.0\n");
        let b = write(&dir, "b.md", "nothing\n");
        let out = rewrite_version_in_files(
            &[bare(&a, "0.1.0", "0.2.0"), bare(&b, "0.1.0", "0.2.0")],
            false,
        )
        .unwrap();
        assert_eq!(out[0].path, a);
        assert_eq!(out[0].replacements, 2);
        assert_eq!(out[1].path, b);
        assert_eq!(out[1].replacements, 0);
    }

    // -----------------------------------------------------------------------
    // `match` anchors
    // -----------------------------------------------------------------------

    #[test]
    fn anchor_regex_escapes_version_and_keeps_quantifiers() {
        let re = anchor_regex("values.yaml", r"pin-\d{2}: v{version}", "0.7.0").unwrap();
        assert!(re.is_match("pin-42: v0.7.0"));
        // The `.` separators are escaped, so they match literally.
        assert!(!re.is_match("pin-42: v0X7.0"));
        // The `\d{2}` quantifier survived the `{version}` substitution.
        assert!(!re.is_match("pin-4: v0.7.0"));
    }

    #[test]
    fn anchor_regex_requires_version_placeholder() {
        let err = anchor_regex("values.yaml", r"image:.*", "0.7.0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("values.yaml"), "err: {err}");
        assert!(err.contains("{version} placeholder"), "err: {err}");
        assert!(err.contains("image:.*"), "err: {err}");
    }

    #[test]
    fn anchor_regex_rejects_an_invalid_regex() {
        let err = anchor_regex("values.yaml", r"image:[.*v{version}", "0.7.0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("is not a valid regex"), "err: {err}");
    }

    #[test]
    fn anchored_rewrite_touches_only_its_region() {
        let dir = TempDir::new().unwrap();
        let f = write(
            &dir,
            "values.yaml",
            "operator:\n  image: ghcr.io/x/operator:v0.7.0\ncsi:\n  image: ghcr.io/x/csi:v0.7.0\n",
        );
        let out = rewrite_version_in_files(
            &[anchored(
                &f,
                r"operator:\s+image:.*:v{version}",
                "0.7.0",
                "0.8.0",
            )],
            false,
        )
        .unwrap();
        assert_eq!(out[0].replacements, 1);
        assert_eq!(out[0].matched_regions, Some(1));
        assert_eq!(
            fs::read_to_string(&f).unwrap(),
            "operator:\n  image: ghcr.io/x/operator:v0.8.0\ncsi:\n  image: ghcr.io/x/csi:v0.7.0\n"
        );
    }

    #[test]
    fn anchored_rewrite_applies_to_every_match() {
        let dir = TempDir::new().unwrap();
        let f = write(&dir, "doc.md", "pin: v0.7.0\nother: 0.7.0\npin: v0.7.0\n");
        let out =
            rewrite_version_in_files(&[anchored(&f, r"pin: v{version}", "0.7.0", "0.8.0")], false)
                .unwrap();
        assert_eq!(out[0].replacements, 2);
        assert_eq!(out[0].matched_regions, Some(2));
        assert_eq!(
            fs::read_to_string(&f).unwrap(),
            "pin: v0.8.0\nother: 0.7.0\npin: v0.8.0\n"
        );
    }

    #[test]
    fn anchored_zero_match_errors_before_writing() {
        let dir = TempDir::new().unwrap();
        let good = write(&dir, "good.md", "pin: v0.7.0\n");
        let bad = write(&dir, "bad.md", "nothing to see\n");
        let err = rewrite_version_in_files(
            &[
                bare(&good, "0.7.0", "0.8.0"),
                anchored(&bad, r"pin: v{version}", "0.7.0", "0.8.0"),
            ],
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("crate 'app'"), "err: {err}");
        assert!(err.contains("bad.md"), "err: {err}");
        assert!(err.contains("matched nothing"), "err: {err}");
        // The entry that DID match is left unwritten: validation precedes IO.
        assert_eq!(fs::read_to_string(&good).unwrap(), "pin: v0.7.0\n");
    }

    #[test]
    fn bare_entry_zero_match_still_warns_not_errors() {
        let dir = TempDir::new().unwrap();
        let f = write(&dir, "doc.md", "nothing to see\n");
        let out = rewrite_version_in_files(&[bare(&f, "0.7.0", "0.8.0")], false).unwrap();
        assert_eq!(out[0].replacements, 0);
        assert_eq!(out[0].matched_regions, None);
    }

    #[test]
    fn longest_old_applied_first_within_a_file() {
        let dir = TempDir::new().unwrap();
        let f = write(&dir, "doc.md", "a 0.1.0-rc1 b 0.1.0\n");
        // First-seen order puts the SHORTER old first; the engine must still
        // rewrite `0.1.0-rc1` before the `0.1.0` matcher can eat its prefix.
        let out = rewrite_version_in_files(
            &[bare(&f, "0.1.0", "0.5.0"), bare(&f, "0.1.0-rc1", "0.9.9")],
            false,
        )
        .unwrap();
        assert_eq!(fs::read_to_string(&f).unwrap(), "a 0.9.9 b 0.5.0\n");
        assert_eq!(out[0].replacements, 1);
        assert_eq!(out[1].replacements, 1);
    }

    #[test]
    fn check_version_present_scopes_to_anchor() {
        let dir = TempDir::new().unwrap();
        let f = write(&dir, "values.yaml", "pin: v0.7.0\nloose: 0.9.0\n");
        let res = check_version_present(
            &[(f.clone(), Some(r"pin: v{version}".to_string()))],
            "0.7.0",
        )
        .unwrap();
        assert_eq!(res, vec![(f.clone(), true)]);
        // 0.9.0 IS in the file, but not inside the anchor.
        let res = check_version_present(
            &[(f.clone(), Some(r"pin: v{version}".to_string()))],
            "0.9.0",
        )
        .unwrap();
        assert_eq!(res, vec![(f, false)]);
    }
}
