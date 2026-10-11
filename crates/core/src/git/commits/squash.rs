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

use regex::Regex;
use std::sync::LazyLock;

/// A body line that is a squashed commit subject: a `* ` / `- ` bullet whose
/// text has the `type(scope)!: description` shape. The space after the colon
/// is required, as Conventional Commits requires it, so a `scheme://` URL
/// bullet is not a subject.
static SQUASHED_SUBJECT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*[*-]\s+([a-zA-Z]+(?:\([^)]*\))?!?:\s+\S.*)$")
        .unwrap_or_else(|e| panic!("static SQUASHED_SUBJECT_RE must compile: {e}"))
});

/// The ` (#N)` GitHub appends to a squash-merge title and to each bullet
/// it writes from a branch commit.
static PR_NUMBER_SUFFIX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\s*\(#\d+\)$")
        .unwrap_or_else(|e| panic!("static PR_NUMBER_SUFFIX_RE must compile: {e}"))
});

/// A subject with its trailing ` (#N)` removed, for comparing the title with
/// a bullet: GitHub appends the number to the title and not to a bullet it
/// copies from a branch commit, so `feat: x (#40)` and `feat: x` are one
/// change.
fn without_pr_number(subject: &str) -> &str {
    let subject = subject.trim();
    PR_NUMBER_SUFFIX_RE
        .find(subject)
        .map_or(subject, |m| subject[..m.start()].trim_end())
}

/// The conventional subjects a squash-merge body lists as bullets, in order,
/// trimmed. Lines that are not bullets, or whose text is not a conventional
/// subject, are skipped; a bullet repeating `subject` (with or without its
/// ` (#N)`) is skipped too, so a PR whose title is also its one commit is not
/// counted twice.
pub fn squashed_subjects(subject: &str, body: &str) -> Vec<String> {
    let title = without_pr_number(subject);
    body.lines()
        .filter_map(|line| SQUASHED_SUBJECT_RE.captures(line))
        .map(|caps| caps[1].trim_end().to_string())
        .filter(|bullet| without_pr_number(bullet) != title)
        .collect()
}

/// The messages a commit stands for when it is read for a release signal:
/// the commit's own message (subject, then body when there is one), followed,
/// when `expand_squash_merges` is set, by one message per squashed subject.
pub fn messages_with_squashed(
    subject: &str,
    body: &str,
    expand_squash_merges: bool,
) -> Vec<String> {
    let own = if body.trim().is_empty() {
        subject.to_string()
    } else {
        format!("{subject}\n\n{body}")
    };
    let bullets = if expand_squash_merges {
        squashed_subjects(subject, body)
    } else {
        Vec::new()
    };
    std::iter::once(own).chain(bullets).collect()
}
