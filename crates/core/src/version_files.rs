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

/// One planned rewrite: `path` (repo-relative), the optional `match` anchor,
/// the `old` → `new` pair, and a label naming the enrolling crate for the
/// unmatched-anchor error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRewrite {
    /// The enrolled path, relative to the repo root the caller passes in. It is
    /// joined with that root for IO and printed as-is in every message, so a
    /// user never sees two spellings of one enrollment.
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

/// The word-boundary matcher for one version, covering the bare (`0.1.0`) and
/// `v`-prefixed (`v0.1.0`) spellings in a single pass so each occurrence is
/// located once and its `v` (when present) is part of the match.
///
/// The ONE matcher for a plain version string: `tag` decides what to rewrite
/// with it and `check version-files` decides what is in sync with it, so the
/// two can never disagree about what counts as an occurrence.
fn occurrence_regex(version: &str) -> Result<Regex> {
    let escaped = regex::escape(version);
    Regex::new(&format!(r"\bv?{escaped}\b"))
        .with_context(|| format!("failed to build version matcher for {version:?}"))
}

/// One planned byte-range replacement in the ORIGINAL file content.
struct Edit {
    start: usize,
    end: usize,
    text: String,
}

/// Claim every unclaimed occurrence of `old` inside `window` — a slice of the
/// original content starting at byte `offset` — and record its replacement.
///
/// Claiming makes two entries on one file non-overlapping by construction: an
/// occurrence already spoken for by an earlier entry is skipped rather than
/// rewritten twice. Returns how many occurrences this entry claimed.
fn claim_occurrences(
    window: &str,
    offset: usize,
    occurrence: &Regex,
    new: &str,
    claimed: &mut Vec<(usize, usize)>,
    edits: &mut Vec<Edit>,
) -> usize {
    let mut count = 0;
    for m in occurrence.find_iter(window) {
        let (start, end) = (offset + m.start(), offset + m.end());
        if claimed.iter().any(|(cs, ce)| start < *ce && *cs < end) {
            continue;
        }
        claimed.push((start, end));
        edits.push(Edit {
            start,
            end,
            text: if m.as_str().starts_with('v') {
                format!("v{new}")
            } else {
                new.to_string()
            },
        });
        count += 1;
    }
    count
}

/// Splice every edit into `original`. Edits never overlap (claiming guarantees
/// it), so applying them in ascending order rewrites each byte once.
fn apply_edits(original: &str, mut edits: Vec<Edit>) -> String {
    edits.sort_by_key(|e| e.start);
    let mut out = String::with_capacity(original.len());
    let mut cursor = 0usize;
    for edit in edits {
        out.push_str(&original[cursor..edit.start]);
        out.push_str(&edit.text);
        cursor = edit.end;
    }
    out.push_str(&original[cursor..]);
    out
}

/// Apply every planned rewrite. All files are read and rewritten IN MEMORY
/// first; an anchored entry that matched nothing errors before any file is
/// written, so a half-applied bump is impossible. Returns one
/// [`RewriteOutcome`] per input entry, in input order (`replacements == 0` on a
/// bare entry means the version was not found — the caller decides how to
/// warn). When `dry_run` is set, counts are computed but no file is written.
///
/// Every entry on one file selects its occurrences from the ORIGINAL content,
/// never from a partially rewritten copy, and each occurrence is claimed by the
/// first entry to select it: anchored entries claim their regions before the
/// bare sweep, and within each half the longest `old` goes first so a shorter
/// `old` that is a word-boundary prefix of a longer one (`0.1.0` inside
/// `0.1.0-rc1`) cannot consume it. A bare entry and an anchored one can
/// therefore share a file — each rewrites its own bytes, exactly once.
///
/// An entry whose `old` equals its `new` is a no-op reporting zero
/// replacements; the file is still read, so a stale enrollment pointing at a
/// missing file is caught.
///
/// Errors if an enrolled file is missing or unreadable, if an anchor is invalid
/// or matches nothing, or (outside `dry_run`) if a file cannot be written.
pub fn rewrite_version_in_files(
    root: &Path,
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
        let original = fs::read_to_string(root.join(&path))
            .with_context(|| format!("failed to read version file {path}"))?;
        // Anchored entries claim their regions before the bare sweep sees the
        // file, and within each half the longest `old` goes first — a shorter
        // `old` that is a word-boundary prefix of a longer one (`0.1.0` inside
        // `0.1.0-rc1`) would otherwise consume it.
        indices.sort_by_key(|idx| {
            (
                rewrites[*idx].anchor.is_none(),
                std::cmp::Reverse(rewrites[*idx].old.len()),
            )
        });

        let mut claimed: Vec<(usize, usize)> = Vec::new();
        let mut edits: Vec<Edit> = Vec::new();
        for idx in indices {
            let rewrite = &rewrites[idx];
            if rewrite.old == rewrite.new {
                continue;
            }
            let occurrence = occurrence_regex(&rewrite.old)?;
            match rewrite.anchor.as_deref() {
                None => {
                    let replacements = claim_occurrences(
                        &original,
                        0,
                        &occurrence,
                        &rewrite.new,
                        &mut claimed,
                        &mut edits,
                    );
                    counts[idx] = (replacements, None);
                }
                Some(anchor) => {
                    let re = anchor_regex(&rewrite.path, anchor, &rewrite.old)?;
                    let regions: Vec<(usize, usize)> = re
                        .find_iter(&original)
                        .map(|m| (m.start(), m.end()))
                        .collect();
                    if regions.is_empty() {
                        bail!(
                            "version_files: crate '{}' enrolled {} with match {:?} but it matched \
                             nothing (expected version {}); fix the anchor or remove the enrollment",
                            rewrite.owner,
                            rewrite.path,
                            anchor,
                            rewrite.old,
                        );
                    }
                    let mut replacements = 0;
                    for (start, end) in &regions {
                        replacements += claim_occurrences(
                            &original[*start..*end],
                            *start,
                            &occurrence,
                            &rewrite.new,
                            &mut claimed,
                            &mut edits,
                        );
                    }
                    counts[idx] = (replacements, Some(regions.len()));
                }
            }
        }
        if !edits.is_empty() {
            pending.push((path, apply_edits(&original, edits)));
        }
    }

    if !dry_run {
        for (path, body) in pending {
            crate::fs_atomic::atomic_write_str(&root.join(&path), &body)
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
    Ok(occurrence_regex(version)?.is_match(content))
}

/// Read-only check: for each `(path, anchor)` entry, whether the file currently
/// contains `version`. A bare entry (`anchor` is `None`) matches anywhere in the
/// file, bare or `v`-prefixed and word-boundary anchored; an anchored entry is
/// present only when its `match` regex — compiled against `version` — selects a
/// region. Paths are relative to `root`: joined with it for the read, echoed
/// as given in every message. Returns one `(path, present)` pair per entry in
/// input order.
///
/// Errors if an enrolled file is missing or unreadable, or an anchor is invalid.
pub fn check_version_present(
    root: &Path,
    entries: &[(String, Option<String>)],
    version: &str,
) -> Result<Vec<(String, bool)>> {
    let occurrence = occurrence_regex(version)?;
    let mut results = Vec::with_capacity(entries.len());
    for (path, anchor) in entries {
        let content = fs::read_to_string(root.join(path))
            .with_context(|| format!("failed to read version file {path}"))?;
        let present = match anchor {
            Some(anchor) => anchor_regex(path, anchor, version)?.is_match(&content),
            None => occurrence.is_match(&content),
        };
        results.push((path.clone(), present));
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::test_sources::{production_half, rust_sources};
    use std::fs;
    use tempfile::TempDir;

    /// The version-matcher walk skips a `tests/` module directory, not only a
    /// `tests.rs` sibling. A test module scanned as production would report a
    /// version literal inside a fixture as a matcher the population declares.
    #[test]
    fn rust_sources_skips_a_gated_tests_directory() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("mod.rs"),
            "#[cfg(test)]\nmod tests;\npub fn build() {}\n",
        )
        .unwrap();
        fs::write(dir.path().join("engine.rs"), "pub fn run() {}\n").unwrap();
        fs::create_dir(dir.path().join("tests")).unwrap();
        fs::write(dir.path().join("tests/mod.rs"), "fn fixture() {}\n").unwrap();

        let mut found = rust_sources(dir.path());
        found.sort();
        assert_eq!(
            found,
            vec![dir.path().join("engine.rs"), dir.path().join("mod.rs")],
            "a gated `tests/` module directory is not production source"
        );
    }

    /// Every regex over a VERSION in the version_files population is built by
    /// one of two functions here. A second builder is exactly the drift that
    /// once split `tag` from `check` (`version_regexes` beside
    /// `occurrence_regex`, one used by each), and nothing mechanical stopped it
    /// coming back: this walk does, across the production half of every file
    /// either command resolves through.
    ///
    /// The two rollback entries are the deliberate exception, named here rather
    /// than pattern-matched away: they validate a TAG REF's grammar, never a
    /// version inside a repo file.
    #[test]
    fn every_version_matcher_is_built_by_one_of_the_named_builders() {
        let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("repo root above crates/core");
        let population = [
            "crates/core/src/version_files.rs",
            "crates/cli/src/commands/tag",
            "crates/cli/src/commands/check",
            "crates/cli/src/commands/version_files_resolve.rs",
            "crates/stage-build/src/version_sync.rs",
        ];

        let mut sources: Vec<std::path::PathBuf> = Vec::new();
        for entry in population {
            let path = repo_root.join(entry);
            if path.is_dir() {
                sources.extend(rust_sources(&path));
            } else {
                assert!(
                    path.is_file(),
                    "population entry missing: {}",
                    path.display()
                );
                sources.push(path);
            }
        }

        let mut owners: Vec<(String, String)> = Vec::new();
        for source in &sources {
            let text = fs::read_to_string(source).expect("read source");
            // A test that exercises a matcher spells a version too; only the
            // production half can introduce a matcher.
            let lines: Vec<&str> = production_half(&text).lines().collect();
            for (i, line) in lines.iter().enumerate() {
                if !line.contains("Regex::new") && !line.contains("RegexBuilder") {
                    continue;
                }
                // A version matcher is a Regex built from a version: the
                // literal spells one (`\d+\.\d+`, the `{version}` anchor
                // token) or the pattern is composed from a version binding.
                let start = i.saturating_sub(4);
                let window = lines[start..(i + 3).min(lines.len())].join(" ");
                let builds_from_a_version = window.contains(r"\d+\.\d+")
                    || window.contains("{version}")
                    || window.contains("version")
                    || window.contains("escaped");
                if !builds_from_a_version {
                    continue;
                }
                let owner = lines[..=i]
                    .iter()
                    .rev()
                    .find_map(|l| item_name(l))
                    .unwrap_or_else(|| panic!("no owning item for {}:{}", source.display(), i + 1));
                owners.push((
                    source
                        .strip_prefix(repo_root)
                        .unwrap_or(source)
                        .display()
                        .to_string(),
                    owner,
                ));
            }
        }
        owners.sort();

        let expected = vec![
            (
                "crates/cli/src/commands/tag/rollback/tags.rs".to_string(),
                "LOCKSTEP_TAG_RE".to_string(),
            ),
            (
                "crates/cli/src/commands/tag/rollback/tags.rs".to_string(),
                "PER_CRATE_TAG_RE".to_string(),
            ),
            (
                "crates/core/src/version_files.rs".to_string(),
                "anchor_regex".to_string(),
            ),
            (
                "crates/core/src/version_files.rs".to_string(),
                "occurrence_regex".to_string(),
            ),
        ];
        assert_eq!(
            owners, expected,
            "a version matcher outside `occurrence_regex` / `anchor_regex` (or the two \
             tag-grammar validators) means `tag` and `check` can disagree again"
        );
    }

    /// The `fn` or `static` an item line declares, if any.
    fn item_name(line: &str) -> Option<String> {
        let trimmed = line.trim_start();
        for keyword in ["fn ", "static "] {
            if let Some(rest) = trimmed
                .strip_prefix(keyword)
                .or_else(|| trimmed.split_once(&format!(" {keyword}")).map(|(_, r)| r))
            {
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if !name.is_empty() {
                    return Some(name);
                }
            }
        }
        None
    }

    /// Writes `name` under `dir` and returns the ROOT-RELATIVE name — the
    /// spelling the engine takes, logs and errors with.
    fn write(dir: &TempDir, name: &str, body: &str) -> String {
        fs::write(dir.path().join(name), body).unwrap();
        name.to_string()
    }

    fn read(dir: &TempDir, name: &str) -> String {
        fs::read_to_string(dir.path().join(name)).unwrap()
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
        let out =
            rewrite_version_in_files(dir.path(), &[bare(&f, "0.1.0", "0.2.0")], false).unwrap();
        assert_eq!(out[0].replacements, 2);
        let body = read(&dir, &f);
        assert_eq!(body, "version: 0.2.0\nappVersion: v0.2.0\n");
    }

    #[test]
    fn word_boundary_does_not_match_inside_longer_version() {
        let dir = TempDir::new().unwrap();
        let f = write(&dir, "doc.md", "use 10.1.0 not 0.1.0\n");
        let out =
            rewrite_version_in_files(dir.path(), &[bare(&f, "0.1.0", "0.2.0")], false).unwrap();
        assert_eq!(out[0].replacements, 1);
        assert_eq!(read(&dir, &f), "use 10.1.0 not 0.2.0\n");
    }

    #[test]
    fn zero_matches_is_not_an_error() {
        let dir = TempDir::new().unwrap();
        let f = write(&dir, "doc.md", "no version here\n");
        let out =
            rewrite_version_in_files(dir.path(), &[bare(&f, "0.1.0", "0.2.0")], false).unwrap();
        assert_eq!(out[0].replacements, 0);
        assert_eq!(read(&dir, &f), "no version here\n");
    }

    #[test]
    fn dry_run_computes_count_without_writing() {
        let dir = TempDir::new().unwrap();
        let f = write(&dir, "doc.md", "v0.1.0\n");
        let out =
            rewrite_version_in_files(dir.path(), &[bare(&f, "0.1.0", "0.2.0")], true).unwrap();
        assert_eq!(out[0].replacements, 1);
        assert_eq!(read(&dir, &f), "v0.1.0\n");
    }

    #[test]
    fn equal_old_new_is_noop() {
        let dir = TempDir::new().unwrap();
        let f = write(&dir, "doc.md", "0.1.0\n");
        let out =
            rewrite_version_in_files(dir.path(), &[bare(&f, "0.1.0", "0.1.0")], false).unwrap();
        assert_eq!(out[0].replacements, 0);
        assert_eq!(read(&dir, &f), "0.1.0\n");
    }

    #[test]
    fn prerelease_version_with_hyphen_rewrites() {
        let dir = TempDir::new().unwrap();
        let f = write(&dir, "doc.md", "tag v0.1.0-beta here\n");
        let out =
            rewrite_version_in_files(dir.path(), &[bare(&f, "0.1.0-beta", "0.2.0-beta")], false)
                .unwrap();
        assert_eq!(out[0].replacements, 1);
        assert_eq!(read(&dir, &f), "tag v0.2.0-beta here\n");
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
        let out =
            rewrite_version_in_files(dir.path(), &[bare(&f, "0.1.0", "0.2.0")], false).unwrap();
        assert_eq!(out[0].replacements, 1);
        assert_eq!(read(&dir, &f), "pinned at 0.2.0-rc1 today\n");
    }

    #[test]
    fn missing_file_is_an_error() {
        let dir = TempDir::new().unwrap();
        let missing = "nope.yaml".to_string();
        let err = rewrite_version_in_files(dir.path(), &[bare(&missing, "0.1.0", "0.2.0")], false)
            .unwrap_err();
        let err = err.to_string();
        assert!(
            err.contains("failed to read version file nope.yaml"),
            "err: {err}"
        );
        assert!(
            !err.contains(&dir.path().to_string_lossy().into_owned()),
            "err leaked the resolved absolute path: {err}"
        );
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
        let res =
            check_version_present(dir.path(), &[(a.clone(), None), (b.clone(), None)], "0.1.0")
                .unwrap();
        assert_eq!(res, vec![(a, true), (b, false)]);
    }

    #[test]
    fn multiple_files_reported_in_input_order() {
        let dir = TempDir::new().unwrap();
        let a = write(&dir, "a.md", "0.1.0\n0.1.0\n");
        let b = write(&dir, "b.md", "nothing\n");
        let out = rewrite_version_in_files(
            dir.path(),
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
            dir.path(),
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
            read(&dir, &f),
            "operator:\n  image: ghcr.io/x/operator:v0.8.0\ncsi:\n  image: ghcr.io/x/csi:v0.7.0\n"
        );
    }

    #[test]
    fn anchored_rewrite_applies_to_every_match() {
        let dir = TempDir::new().unwrap();
        let f = write(&dir, "doc.md", "pin: v0.7.0\nother: 0.7.0\npin: v0.7.0\n");
        let out = rewrite_version_in_files(
            dir.path(),
            &[anchored(&f, r"pin: v{version}", "0.7.0", "0.8.0")],
            false,
        )
        .unwrap();
        assert_eq!(out[0].replacements, 2);
        assert_eq!(out[0].matched_regions, Some(2));
        assert_eq!(read(&dir, &f), "pin: v0.8.0\nother: 0.7.0\npin: v0.8.0\n");
    }

    #[test]
    fn anchored_zero_match_errors_before_writing() {
        let dir = TempDir::new().unwrap();
        let good = write(&dir, "good.md", "pin: v0.7.0\n");
        let bad = write(&dir, "bad.md", "nothing to see\n");
        let err = rewrite_version_in_files(
            dir.path(),
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
        // The enrolled spelling, not the resolved one: one path per enrollment.
        assert!(
            !err.contains(&dir.path().to_string_lossy().into_owned()),
            "err leaked the resolved absolute path: {err}"
        );
        // The entry that DID match is left unwritten: validation precedes IO.
        assert_eq!(read(&dir, &good), "pin: v0.7.0\n");
    }

    #[test]
    fn bare_entry_zero_match_still_warns_not_errors() {
        let dir = TempDir::new().unwrap();
        let f = write(&dir, "doc.md", "nothing to see\n");
        let out =
            rewrite_version_in_files(dir.path(), &[bare(&f, "0.7.0", "0.8.0")], false).unwrap();
        assert_eq!(out[0].replacements, 0);
        assert_eq!(out[0].matched_regions, None);
    }

    /// A bare entry and an anchored entry on one file under the same bump: the
    /// anchored entry claims its region, the bare sweep takes the rest, and no
    /// byte is rewritten twice. Selecting against a partially rewritten copy
    /// would make the anchor "match nothing" after the bare pass consumed it.
    #[test]
    fn bare_and_anchored_on_one_file_each_rewrite_once() {
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "chart.yaml", "pin: v1.2.3\nother: 1.2.3\n");
        let outcomes = rewrite_version_in_files(
            dir.path(),
            &[
                bare(&path, "1.2.3", "1.2.4"),
                anchored(&path, r"pin: v{version}", "1.2.3", "1.2.4"),
            ],
            false,
        )
        .unwrap();
        assert_eq!(read(&dir, &path), "pin: v1.2.4\nother: 1.2.4\n");
        assert_eq!(outcomes[0].replacements, 1, "bare: {outcomes:?}");
        assert_eq!(outcomes[1].replacements, 1, "anchored: {outcomes:?}");
        assert_eq!(outcomes[1].matched_regions, Some(1));
    }

    /// The enrollment order must not decide the outcome: an anchored entry
    /// listed after a bare one still claims its own region first.
    #[test]
    fn bare_listed_first_still_leaves_the_anchor_its_region() {
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "chart.yaml", "pin: v0.9.0\nother: 0.9.0\n");
        let outcomes = rewrite_version_in_files(
            dir.path(),
            &[
                bare(&path, "0.9.0", "0.10.0"),
                anchored(&path, r"pin: v{version}", "0.9.0", "0.10.0"),
            ],
            false,
        )
        .unwrap();
        assert_eq!(read(&dir, &path), "pin: v0.10.0\nother: 0.10.0\n");
        assert_eq!(outcomes[1].replacements, 1, "anchor starved: {outcomes:?}");
    }

    #[test]
    fn longest_old_applied_first_within_a_file() {
        let dir = TempDir::new().unwrap();
        let f = write(&dir, "doc.md", "a 0.1.0-rc1 b 0.1.0\n");
        // First-seen order puts the SHORTER old first; the engine must still
        // rewrite `0.1.0-rc1` before the `0.1.0` matcher can eat its prefix.
        let out = rewrite_version_in_files(
            dir.path(),
            &[bare(&f, "0.1.0", "0.5.0"), bare(&f, "0.1.0-rc1", "0.9.9")],
            false,
        )
        .unwrap();
        assert_eq!(read(&dir, &f), "a 0.9.9 b 0.5.0\n");
        assert_eq!(out[0].replacements, 1);
        assert_eq!(out[1].replacements, 1);
    }

    #[test]
    fn check_version_present_scopes_to_anchor() {
        let dir = TempDir::new().unwrap();
        let f = write(&dir, "values.yaml", "pin: v0.7.0\nloose: 0.9.0\n");
        let res = check_version_present(
            dir.path(),
            &[(f.clone(), Some(r"pin: v{version}".to_string()))],
            "0.7.0",
        )
        .unwrap();
        assert_eq!(res, vec![(f.clone(), true)]);
        // 0.9.0 IS in the file, but not inside the anchor.
        let res = check_version_present(
            dir.path(),
            &[(f.clone(), Some(r"pin: v{version}".to_string()))],
            "0.9.0",
        )
        .unwrap();
        assert_eq!(res, vec![(f, false)]);
    }
}
