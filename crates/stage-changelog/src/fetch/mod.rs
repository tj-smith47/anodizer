//! Commit-fetching backends.
//!
//! `mod.rs` contains the local-git path (`fetch_git_commits`,
//! `fetch_git_commits_in`, `parse_git_log_records`) and small helpers shared
//! across backends (`relative_filter`, `should_preempt_scm_to_git`). The
//! per-SCM compare-API fetchers live in sibling files.

use anyhow::{Context as _, Result};

use anodizer_core::git::{get_all_commits_paths, get_commits_between_paths};
use anodizer_core::log::StageLogger;

use crate::group::{CommitInfo, extract_co_authors, parse_commit_message};

pub(crate) mod gitea;
pub(crate) mod github;
pub(crate) mod gitlab;

pub(crate) use gitea::fetch_gitea_commits;
pub(crate) use github::fetch_github_commits;
pub(crate) use gitlab::fetch_gitlab_commits;

/// Translate `crate_path` into a path relative to `workspace_root` for use as
/// a `git log -- <path>` filter. Returns `None` for the workspace-root crate
/// itself (which would be `.` or empty) so the caller can omit `--`.
pub(crate) fn relative_filter(
    workspace_root: &std::path::Path,
    crate_path: &std::path::Path,
) -> Option<String> {
    let rel = crate_path
        .strip_prefix(workspace_root)
        .unwrap_or(crate_path);
    let s = rel.to_string_lossy().to_string();
    if s.is_empty() || s == "." {
        None
    } else {
        Some(s)
    }
}

/// Fetch path-filtered commits in the range `from..to` via `git log`.
///
/// The range follows git's `<from>..<to>` two-dot semantics:
/// - `from = Some, to = Some` → `<from>..<to>`
/// - `from = Some, to = None` → `<from>..HEAD`
/// - `from = None, to = Some` → `<to>` (all ancestors of `to`)
/// - `from = None, to = None` → `HEAD`
///
/// A non-zero git exit (e.g. an unknown tag, an empty range) is treated as
/// "no commits" rather than an error, matching the changelog stage's
/// best-effort history walk.
pub(crate) fn fetch_git_commits_in(
    workspace_root: &std::path::Path,
    from: Option<&str>,
    to: Option<&str>,
    path_filter: Option<&str>,
) -> Result<Vec<anodizer_core::git::Commit>> {
    use std::process::Command;
    let upper = to.unwrap_or("HEAD");
    let range = match from {
        Some(t) => format!("{}..{}", t, upper),
        None => upper.to_string(),
    };
    let mut args: Vec<String> = vec![
        "-C".into(),
        workspace_root.to_string_lossy().into_owned(),
        "-c".into(),
        "log.showSignature=false".into(),
        "log".into(),
        "--pretty=format:%H%x1f%h%x1f%s%x1f%an%x1f%ae%x1f%b%x1e".into(),
        range.clone(),
    ];
    if let Some(p) = path_filter {
        args.push("--".into());
        args.push(p.to_string());
    }
    let out = Command::new("git")
        .args(args.iter().map(|s| s.as_str()))
        .output()
        .with_context(|| "failed to invoke git log".to_string())?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        // A typo'd or nonexistent `from`/`to` ref must surface as an error
        // rather than a silently-empty changelog: git names it an
        // unknown/ambiguous revision. Any other non-zero exit (e.g. an empty
        // range on git versions that exit non-zero for that) is still treated
        // as "no commits".
        if is_bad_revision(&stderr) {
            anyhow::bail!("git log failed for range {:?}: {}", range, stderr.trim());
        }
        return Ok(Vec::new());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(parse_git_log_records(&text))
}

/// Whether git's stderr names an unknown/ambiguous revision — the signature of
/// a typo'd or nonexistent `from`/`to` ref, as opposed to a genuinely empty
/// commit range. Matched case-insensitively against the phrases git emits
/// across versions.
fn is_bad_revision(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("unknown revision")
        || lower.contains("ambiguous argument")
        || lower.contains("bad revision")
        || lower.contains("fatal: ambiguous")
}

pub(crate) fn parse_git_log_records(text: &str) -> Vec<anodizer_core::git::Commit> {
    use anodizer_core::git::Commit;
    text.split('\x1e')
        .map(|s| s.trim_matches(['\n', '\r']))
        .filter(|s| !s.is_empty())
        .filter_map(|record| {
            let mut fields = record.split('\x1f');
            let hash = fields.next()?.to_string();
            let short_hash = fields.next()?.to_string();
            let message = fields.next()?.to_string();
            let author_name = fields.next()?.to_string();
            let author_email = fields.next()?.to_string();
            let body = fields.next().unwrap_or("").to_string();
            Some(Commit {
                hash,
                short_hash,
                message,
                author_name,
                author_email,
                body,
            })
        })
        .collect()
}
// ---------------------------------------------------------------------------
// Helper: SCM pre-empt decision
// ---------------------------------------------------------------------------

/// Returns `true` when an SCM-mode changelog should pre-empt to the git
/// fallback because there is no previous tag to compare against.
///
/// Resolve the commit source: for `use: github` / `use: gitlab` /
/// `use: gitea`, when `ctx.Git.PreviousTag == ""`, it warns and returns the
/// git changeloger directly instead of issuing an SCM compare-API call (which
/// would 404 / produce nothing useful with no base ref). The pre-empt also
/// avoids transient API failures interrupting a first release.
pub(crate) fn should_preempt_scm_to_git(
    use_github: bool,
    use_gitlab: bool,
    use_gitea: bool,
    prev_tag: &Option<String>,
) -> bool {
    (use_github || use_gitlab || use_gitea) && prev_tag.is_none()
}

// ---------------------------------------------------------------------------
// Helper: fetch commits from local git
// ---------------------------------------------------------------------------

pub(crate) fn fetch_git_commits(
    prev_tag: &Option<String>,
    paths: &[String],
    crate_name: &str,
    log: &StageLogger,
) -> Result<Vec<CommitInfo>> {
    let raw_commits = match prev_tag {
        Some(tag) => get_commits_between_paths(tag, "HEAD", paths).with_context(|| {
            format!(
                "changelog: read git commits between {}..HEAD for crate '{}'",
                tag, crate_name
            )
        })?,
        None => {
            log.status(&format!(
                "no previous tag found for crate '{}', using all commits",
                crate_name
            ));
            get_all_commits_paths(paths).with_context(|| {
                format!("changelog: read all git commits for crate '{}'", crate_name)
            })?
        }
    };

    let mut all_commit_infos = Vec::new();
    for commit in raw_commits {
        let mut info = parse_commit_message(&commit.message);
        info.hash = commit.short_hash.clone();
        info.full_hash = commit.hash.clone();
        info.author_name = commit.author_name.clone();
        info.author_email = commit.author_email.clone();
        // Extract co-authors from the commit body (trailers).
        info.co_authors = extract_co_authors(&commit.body);
        all_commit_infos.push(info);
    }
    Ok(all_commit_infos)
}
