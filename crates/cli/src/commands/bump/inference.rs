//! Conventional-Commit → semver-level inference for `anodizer bump`.
//!
//! Per-commit classification is [`anodizer_core::git::classify_commit`] —
//! the same rules the `tag` command's auto-bump layer consumes (breaking
//! change / `<type>!:` → major, `feat:` → minor, `fix:`/`perf:`/`revert:` →
//! patch, everything else → no bump) — so a `bump --dry-run` preview always
//! matches what auto-tag would cut.
//!
//! Commit scope is the crate's directory, matched against the last tag
//! whose name starts with the crate's resolved tag prefix. If no such tag
//! exists, we walk from the beginning of the repo history for that crate.

use anyhow::Result;

use super::cargo_edit::MemberInfo;
use super::plan::BumpLevel;

pub struct InferenceResult {
    pub level: BumpLevel,
    pub reason: String,
}

/// Infer the per-crate bump level from commits since the crate's last tag.
///
/// `tag_prefix` is the crate's resolved tag-family prefix
/// ([`anodizer_core::git::per_crate_tag_prefix`] — the `tag_template`
/// extraction with the `<crate-name>-v` fallback already applied).
pub fn infer_for_crate(
    workspace_root: &std::path::Path,
    m: &MemberInfo,
    tag_prefix: &str,
) -> Result<InferenceResult> {
    let last_tag = find_last_tag_for_prefix(workspace_root, tag_prefix)?;

    let rel_crate_dir = m
        .crate_dir
        .strip_prefix(workspace_root)
        .unwrap_or(&m.crate_dir)
        .to_string_lossy()
        .to_string();

    let range_from = last_tag.clone().unwrap_or_default();
    let messages = git_log_subjects(workspace_root, &range_from, &rel_crate_dir)?;

    if messages.is_empty() {
        let reason = if last_tag.is_some() {
            format!("no commits since {}", last_tag.unwrap_or_default())
        } else {
            "no commits touching this crate".to_string()
        };
        return Ok(InferenceResult {
            level: BumpLevel::Skip,
            reason,
        });
    }

    let (level, counts) = classify(&messages);
    let reason = format_reason(level, &counts, last_tag.as_deref());
    Ok(InferenceResult { level, reason })
}

#[derive(Default, Debug, Clone, Copy)]
pub(crate) struct LevelCounts {
    major: usize,
    minor: usize,
    patch: usize,
    other: usize,
}

pub(crate) fn classify(messages: &[String]) -> (BumpLevel, LevelCounts) {
    use anodizer_core::git::ConventionalLevel;
    let mut counts = LevelCounts::default();
    for msg in messages {
        match anodizer_core::git::classify_commit(msg) {
            Some(ConventionalLevel::Major) => counts.major += 1,
            Some(ConventionalLevel::Minor) => counts.minor += 1,
            Some(ConventionalLevel::Patch) => counts.patch += 1,
            None => counts.other += 1,
        }
    }
    let level = if counts.major > 0 {
        BumpLevel::Major
    } else if counts.minor > 0 {
        BumpLevel::Minor
    } else if counts.patch > 0 {
        BumpLevel::Patch
    } else {
        BumpLevel::Skip
    };
    (level, counts)
}

fn format_reason(level: BumpLevel, c: &LevelCounts, last_tag: Option<&str>) -> String {
    let total = c.major + c.minor + c.patch + c.other;
    let scope = last_tag
        .map(|t| format!(" since {}", t))
        .unwrap_or_default();
    match level {
        BumpLevel::Major => {
            if c.major == 1 {
                format!("1 breaking change{}", scope)
            } else {
                format!("{} breaking changes{}", c.major, scope)
            }
        }
        BumpLevel::Minor => {
            if c.minor == 1 {
                format!("1 feat commit{}", scope)
            } else {
                format!("{} feat commits{}", c.minor, scope)
            }
        }
        BumpLevel::Patch => {
            let label = if c.patch == 1 {
                "fix/perf/revert commit"
            } else {
                "fix/perf/revert commits"
            };
            format!("{} {}{}", c.patch, label, scope)
        }
        BumpLevel::Skip => {
            if total == 0 {
                format!("no commits{}", scope)
            } else {
                format!("{} non-bumping commit(s){}", total, scope)
            }
        }
        _ => String::new(),
    }
}

/// Find the latest tag whose name starts with `prefix` and whose suffix parses
/// as a semver version. Returns `None` if no such tag exists.
pub(crate) fn find_last_tag_for_prefix(
    workspace_root: &std::path::Path,
    prefix: &str,
) -> Result<Option<String>> {
    let tags = anodizer_core::git::list_tags_with_prefix(workspace_root, prefix)?;
    for line in tags {
        if let Some(rest) = line.strip_prefix(prefix)
            && semver::Version::parse(rest).is_ok()
        {
            return Ok(Some(line));
        }
    }
    Ok(None)
}

/// `git log` subject+body for commits in `<from>..HEAD` touching `rel_path`.
/// `from` may be empty — then the range becomes `HEAD` (all history for that path).
fn git_log_subjects(
    workspace_root: &std::path::Path,
    from: &str,
    rel_path: &str,
) -> Result<Vec<String>> {
    let range = if from.is_empty() {
        "HEAD".to_string()
    } else {
        format!("{}..HEAD", from)
    };
    anodizer_core::git::log_subjects_for_range(workspace_root, &range, rel_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_takes_highest_severity() {
        let msgs: Vec<String> = vec!["chore: x".into(), "fix: y".into(), "feat: z".into()];
        let (lvl, _) = classify(&msgs);
        assert_eq!(lvl, BumpLevel::Minor);

        let msgs: Vec<String> = vec!["fix: a".into(), "feat!: b".into(), "feat: c".into()];
        let (lvl, _) = classify(&msgs);
        assert_eq!(lvl, BumpLevel::Major);
    }

    #[test]
    fn aggregate_of_non_release_worthy_commits_is_skip() {
        let msgs: Vec<String> = vec!["chore: deps".into(), "random subject".into()];
        let (lvl, counts) = classify(&msgs);
        assert_eq!(lvl, BumpLevel::Skip);
        assert_eq!(counts.other, 2);
    }

    #[test]
    fn aggregate_counts_revert_as_patch() {
        let msgs: Vec<String> = vec!["revert: undo broken feature".into()];
        let (lvl, counts) = classify(&msgs);
        assert_eq!(lvl, BumpLevel::Patch);
        assert_eq!(counts.patch, 1);
    }
}
