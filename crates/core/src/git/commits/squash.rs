//! Squash-merged pull requests carry the branch's commits as body bullets.
//!
//! GitHub's "Squash and merge" writes one commit whose subject is the PR
//! title and whose body lists every squashed commit as `* <subject> (#N)`.
//! Read as one commit, the changelog gets one line and the auto-bump sees
//! one subject, so a `fix:` or a `#minor` that only lived on the branch is
//! lost. Every reader of commit messages therefore expands a commit into
//! itself plus one synthetic commit per bullet that is a conventional
//! subject, so the PR's commits count the way they would have on a rebase
//! merge. Prose bullets, trailers and plain body lines are not subjects and
//! are left alone.

use super::Commit;
use regex::Regex;
use std::sync::LazyLock;

/// A body line that is a squashed commit subject: a `* ` / `- ` bullet whose
/// text has the `type(scope)!: description` shape.
static SQUASHED_SUBJECT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*[*-]\s+([a-zA-Z]+(?:\([^)]*\))?!?:\s*\S.*)$")
        .unwrap_or_else(|e| panic!("static SQUASHED_SUBJECT_RE must compile: {e}"))
});

/// The conventional subjects a squash-merge body lists as bullets, in order,
/// trimmed. Lines that are not bullets, or whose text is not a conventional
/// subject, are skipped; a bullet repeating `subject` is skipped too, so a
/// PR whose title is also its one commit is not counted twice.
pub fn squashed_subjects(subject: &str, body: &str) -> Vec<String> {
    body.lines()
        .filter_map(|line| SQUASHED_SUBJECT_RE.captures(line))
        .map(|caps| caps[1].trim_end().to_string())
        .filter(|bullet| bullet != subject.trim())
        .collect()
}

/// The messages a commit stands for when it is read for a release signal:
/// the commit's own message (subject, then body when there is one), followed
/// by one message per squashed subject.
pub fn messages_with_squashed(subject: &str, body: &str) -> Vec<String> {
    let own = if body.trim().is_empty() {
        subject.to_string()
    } else {
        format!("{subject}\n\n{body}")
    };
    std::iter::once(own)
        .chain(squashed_subjects(subject, body))
        .collect()
}

/// Every commit followed by one synthetic [`Commit`] per squashed subject:
/// same hash and author, the bullet as its message, no body.
pub fn expand_squashed(commits: Vec<Commit>) -> Vec<Commit> {
    let mut out = Vec::with_capacity(commits.len());
    for commit in commits {
        let bullets = squashed_subjects(&commit.message, &commit.body);
        for bullet in bullets {
            out.push(Commit {
                message: bullet,
                body: String::new(),
                ..commit.clone()
            });
        }
        out.push(commit);
    }
    out
}
