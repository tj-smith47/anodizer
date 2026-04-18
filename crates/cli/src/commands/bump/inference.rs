//! Conventional-Commit → semver-level inference for `anodize bump`.
//!
//! Rules (matches the spec in `.claude/plans/2026-04-18-bump-command.md`):
//!   - `BREAKING CHANGE:` / `BREAKING-CHANGE:` footer, or `!` after the type → major
//!   - `feat(...)` → minor
//!   - `fix(...)`, `perf(...)` → patch
//!   - `chore / docs / refactor / test / build / ci / style` → no bump
//!
//! Commit scope is the crate's directory, matched against the last tag
//! whose name follows `<crate>-v<semver>`. If no such tag exists, we walk
//! from the beginning of the repo history for that crate.

use anyhow::Result;

use super::cargo_edit::MemberInfo;
use super::plan::BumpLevel;

pub struct InferenceResult {
    pub level: BumpLevel,
    pub reason: String,
}

/// Infer the per-crate bump level from commits since the crate's last tag.
///
/// `tag_prefix_override` is the prefix to scan for tags (typically derived
/// from the crate's `.anodize.yaml` `tag_template`). When `None`, the
/// fallback `<crate-name>-v` convention is used — handy for workspaces
/// that have no `.anodize.yaml` at all.
pub fn infer_for_crate(
    workspace_root: &std::path::Path,
    m: &MemberInfo,
    tag_prefix_override: Option<&str>,
) -> Result<InferenceResult> {
    let tag_prefix = tag_prefix_override
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("{}-v", m.name));
    let last_tag = find_last_tag_for_prefix(workspace_root, &tag_prefix)?;

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
struct LevelCounts {
    major: usize,
    minor: usize,
    patch: usize,
    other: usize,
}

fn classify(messages: &[String]) -> (BumpLevel, LevelCounts) {
    let mut counts = LevelCounts::default();
    for msg in messages {
        let hit = classify_one(msg);
        match hit {
            BumpLevel::Major => counts.major += 1,
            BumpLevel::Minor => counts.minor += 1,
            BumpLevel::Patch => counts.patch += 1,
            _ => counts.other += 1,
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

fn classify_one(msg: &str) -> BumpLevel {
    // Check for BREAKING CHANGE: / BREAKING-CHANGE: footer (checked on raw message,
    // allows for multi-line commits which git log --format=%B would include).
    if msg.contains("BREAKING CHANGE:") || msg.contains("BREAKING-CHANGE:") {
        return BumpLevel::Major;
    }
    // Subject line only for type prefix + `!`.
    let subject = msg.lines().next().unwrap_or("").trim();
    let (ty, bang) = parse_type(subject);
    if bang {
        return BumpLevel::Major;
    }
    match ty.as_deref() {
        Some("feat") => BumpLevel::Minor,
        Some("fix") | Some("perf") => BumpLevel::Patch,
        _ => BumpLevel::Skip,
    }
}

/// Parse the `type(scope)?!?:` prefix from a commit subject.
/// Returns `(type, breaking)` — type is `None` if the subject isn't conventional.
fn parse_type(subject: &str) -> (Option<String>, bool) {
    let colon = match subject.find(':') {
        Some(i) => i,
        None => return (None, false),
    };
    let head = &subject[..colon];
    // Strip optional scope.
    let (ty, rest) = match head.find('(') {
        Some(paren) => {
            let close = match head.find(')') {
                Some(c) => c,
                None => return (None, false),
            };
            if close <= paren {
                return (None, false);
            }
            (&head[..paren], &head[close + 1..])
        }
        None => (head, ""),
    };
    let bang = rest.contains('!') || ty.ends_with('!');
    let ty = ty.trim_end_matches('!').trim().to_string();
    if ty.is_empty() || !ty.chars().all(|c| c.is_ascii_alphabetic()) {
        return (None, false);
    }
    (Some(ty), bang)
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
                "fix/perf commit"
            } else {
                "fix/perf commits"
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
pub(super) fn find_last_tag_for_prefix(
    workspace_root: &std::path::Path,
    prefix: &str,
) -> Result<Option<String>> {
    use std::process::Command;
    let out = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["tag", "--list", "--sort=-v:refname"])
        .arg(format!("{}*", prefix))
        .output()?;
    if !out.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(prefix)
            && semver::Version::parse(rest).is_ok()
        {
            return Ok(Some(line.to_string()));
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
    use std::process::Command;
    let range = if from.is_empty() {
        "HEAD".to_string()
    } else {
        format!("{}..HEAD", from)
    };
    let out = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args([
            "-c",
            "log.showSignature=false",
            "log",
            "--pretty=format:%B%x1e",
            &range,
            "--",
            rel_path,
        ])
        .output()?;
    if !out.status.success() {
        // Range may not exist yet (no last_tag, path not in history).
        return Ok(Vec::new());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let commits: Vec<String> = text
        .split('\x1e')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    Ok(commits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_breaking_footer_is_major() {
        assert!(matches!(
            classify_one("feat(api): new endpoint\n\nBREAKING CHANGE: old endpoint removed"),
            BumpLevel::Major
        ));
    }

    #[test]
    fn classify_bang_is_major() {
        assert!(matches!(
            classify_one("feat!: drop legacy auth"),
            BumpLevel::Major
        ));
        assert!(matches!(
            classify_one("feat(core)!: rewrite pipeline"),
            BumpLevel::Major
        ));
    }

    #[test]
    fn classify_feat_is_minor() {
        assert!(matches!(classify_one("feat: new stage"), BumpLevel::Minor));
        assert!(matches!(
            classify_one("feat(build): add cache key"),
            BumpLevel::Minor
        ));
    }

    #[test]
    fn classify_fix_and_perf_are_patch() {
        assert!(matches!(classify_one("fix: race"), BumpLevel::Patch));
        assert!(matches!(
            classify_one("perf: faster loop"),
            BumpLevel::Patch
        ));
    }

    #[test]
    fn classify_chore_is_skip() {
        assert!(matches!(
            classify_one("chore: update deps"),
            BumpLevel::Skip
        ));
        assert!(matches!(classify_one("docs: fix link"), BumpLevel::Skip));
        assert!(matches!(classify_one("refactor: rename"), BumpLevel::Skip));
    }

    #[test]
    fn classify_non_conventional_is_skip() {
        assert!(matches!(classify_one("random subject"), BumpLevel::Skip));
    }

    #[test]
    fn aggregate_takes_highest_severity() {
        let msgs: Vec<String> = vec!["chore: x".into(), "fix: y".into(), "feat: z".into()];
        let (lvl, _) = classify(&msgs);
        assert_eq!(lvl, BumpLevel::Minor);

        let msgs: Vec<String> = vec!["fix: a".into(), "feat!: b".into(), "feat: c".into()];
        let (lvl, _) = classify(&msgs);
        assert_eq!(lvl, BumpLevel::Major);
    }
}
