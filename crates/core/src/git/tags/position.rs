//! Where a ref sits: tag-versus-`HEAD` position, tags at a commit, and the
//! first commit in the repository.
//!
//! [`TagPosition`] is the load-bearing type — a release gate needs to tell a
//! version not yet cut from one being resumed at `HEAD` from one `HEAD` has
//! already moved past, and folding those into a boolean loses the distinction.
//! Everything here is a read-only revision query; the write side lives in
//! [`super::mutate`].

use anyhow::Result;
use std::path::Path;
use std::process::Command;

use super::git_output_in;

/// Return the SHA of the very first commit in the repository.
///
/// Runs `git rev-list --max-parents=0 HEAD` and returns the first line
/// (repositories with multiple roots will return the oldest).
pub fn get_first_commit() -> Result<String> {
    get_first_commit_in(&std::env::current_dir()?)
}

/// Path-taking sibling of [`get_first_commit`].
pub fn get_first_commit_in(cwd: &Path) -> Result<String> {
    let output = git_output_in(cwd, &["rev-list", "--max-parents=0", "HEAD"])?;
    // In repos with multiple roots, take the last line (oldest commit).
    output
        .lines()
        .last()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("no commits found in repository"))
}

/// Where a tag's ref sits relative to the current `HEAD`.
///
/// These are distinguishable answers, not shades of one boolean: to a release
/// gate, a tag that does not exist yet (a version about to be cut), a tag at
/// `HEAD` (a resume of that exact version), and a tag `HEAD` has moved past (a
/// version already released) mean three different things.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagPosition {
    /// No ref of that name resolves in the repository.
    Missing,
    /// The tag's dereferenced object IS the `HEAD` commit.
    AtHead,
    /// The tag is an ancestor of `HEAD`: it exists on earlier history, and
    /// `HEAD` carries commits made after it.
    AncestorOfHead,
    /// The tag resolves but is neither `HEAD` nor reachable from it — `HEAD`
    /// is behind it, or sits on a divergent branch.
    UnrelatedToHead,
}

/// Resolve where `tag` sits relative to `HEAD`, in the repository at `cwd`.
///
/// Dereferences the tag object (`git rev-parse --verify --quiet {tag}^{{}}`),
/// compares it with `HEAD`, and — when they differ — asks `git merge-base
/// --is-ancestor` which side of `HEAD` it falls on. A name that does not
/// resolve is [`TagPosition::Missing`] — an absent tag is an ordinary answer,
/// not a failure. An error is returned only when git itself cannot be invoked,
/// `HEAD` cannot be resolved (an empty repository), or the reachability query
/// fails outright (e.g. the tag names a blob rather than a commit).
///
/// Works with any tag name including monorepo-prefixed tags (e.g.
/// `subproject1/v1.2.3`), since `git rev-parse` resolves tag refs by
/// name regardless of slashes or prefixes.
pub fn tag_position_in(cwd: &Path, tag: &str) -> Result<TagPosition> {
    let deref = format!("{}^{{}}", tag);
    let Some(tag_sha) = rev_parse_verify_in(cwd, &deref)? else {
        return Ok(TagPosition::Missing);
    };
    let head_sha = git_output_in(cwd, &["rev-parse", "HEAD"])?;
    if tag_sha == head_sha {
        return Ok(TagPosition::AtHead);
    }
    let out = Command::new("git")
        .current_dir(cwd)
        .args(["merge-base", "--is-ancestor", &tag_sha, "HEAD"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| anyhow::anyhow!("failed to invoke git merge-base --is-ancestor: {e}"))?;
    if out.status.success() {
        return Ok(TagPosition::AncestorOfHead);
    }
    // `git merge-base --is-ancestor` reserves exit 1 for the negative ANSWER
    // and uses >=2 (128 for a bad object, 129 for usage) to report that it
    // could not answer at all. Folding the latter into "not an ancestor"
    // would let a git failure pass itself off as a position.
    if out.status.code() == Some(1) {
        return Ok(TagPosition::UnrelatedToHead);
    }
    let detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
    anyhow::bail!(
        "git merge-base --is-ancestor {tag_sha} HEAD failed ({}): {detail}",
        out.status
    )
}

/// `git rev-parse --verify --quiet <rev>` — `Ok(None)` when the revision does
/// not resolve, `Err` only when git could not be run at all. `--verify
/// --quiet` is what separates the two: an unknown revision exits non-zero with
/// empty output instead of printing to stderr.
pub(super) fn rev_parse_verify_in(cwd: &Path, rev: &str) -> Result<Option<String>> {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(["rev-parse", "--verify", "--quiet", rev])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| anyhow::anyhow!("failed to invoke git rev-parse {rev}: {e}"))?;
    if !out.status.success() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
    ))
}

/// Check whether `tag` points at the current HEAD commit.
///
/// Compares the dereferenced tag object with `git rev-parse HEAD`.
///
/// Errors when the tag does not resolve at all, so a caller cannot mistake a
/// not-yet-created tag for one that sits on another commit; use
/// [`tag_position_in`] when that distinction matters.
///
/// Works with any tag name including monorepo-prefixed tags (e.g.
/// `subproject1/v1.2.3`), since `git rev-parse` resolves tag refs by
/// name regardless of slashes or prefixes.
pub fn tag_points_at_head(tag: &str) -> Result<bool> {
    tag_points_at_head_in(&std::env::current_dir()?, tag)
}

/// Path-taking sibling of [`tag_points_at_head`], with the same
/// missing-tag-is-an-error contract.
pub fn tag_points_at_head_in(cwd: &Path, tag: &str) -> Result<bool> {
    match tag_position_in(cwd, tag)? {
        TagPosition::Missing => {
            anyhow::bail!("git rev-parse {tag}^{{}} failed: no such tag in this repository")
        }
        position => Ok(position == TagPosition::AtHead),
    }
}

/// Returns `true` when HEAD coincides with a tag.
///
/// HEAD-with-no-tag is the common case for development branches and
/// must not error; only inability to invoke git at all does.
pub fn head_is_at_tag(repo: &std::path::Path) -> Result<bool> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["describe", "--tags", "--exact-match", "HEAD"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| {
            anyhow::anyhow!("failed to invoke git describe --tags --exact-match HEAD: {e}")
        })?;
    Ok(out.status.success())
}

/// Return all tags that point at the current HEAD commit.
///
/// Runs `git tag --points-at HEAD`. An empty repository or a HEAD with no
/// tags returns `Ok(vec![])` rather than an error.
pub fn get_tags_at_head() -> Result<Vec<String>> {
    get_tags_at_head_in(&std::env::current_dir()?)
}

/// Path-taking sibling of [`get_tags_at_head`].
pub fn get_tags_at_head_in(cwd: &Path) -> Result<Vec<String>> {
    get_tags_at_sha_in(cwd, "HEAD")
}

/// Return all tags that point at the given commit (any revision spec).
///
/// Runs `git tag --points-at <sha>`. Failures (unknown sha, not a git
/// repo) return `Ok(vec![])` rather than an error so callers can treat
/// "no tags at that ref" as the empty case.
pub fn get_tags_at_sha_in(cwd: &Path, sha: &str) -> Result<Vec<String>> {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(["tag", "--points-at", sha])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| anyhow::anyhow!("failed to invoke git tag --points-at {sha}: {e}"))?;
    if !out.status.success() {
        // A real git failure (corrupt repo, bad sha that isn't merely
        // "unknown") must not masquerade as "no tags here". Warn with the
        // stderr so the empty result isn't silently misread as a clean
        // no-tags case.
        let stderr = String::from_utf8_lossy(&out.stderr);
        tracing::warn!(
            sha = sha,
            stderr = %stderr.trim(),
            "git tag --points-at exited non-zero; returning no tags"
        );
        return Ok(Vec::new());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}
