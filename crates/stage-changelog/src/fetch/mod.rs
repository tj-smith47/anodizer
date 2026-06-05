//! Commit-fetching backends.
//!
//! `mod.rs` contains the local-git path (`fetch_git_commits`,
//! `fetch_git_commits_in_paths`, `parse_git_log_records`) and small helpers
//! shared across backends (`relative_filter`, `should_preempt_scm_to_git`).
//! The per-SCM compare-API fetchers live in sibling files.

use anyhow::{Context as _, Result};

use anodizer_core::git::{
    get_all_commits_paths_in, get_all_commits_paths_with_files_in, get_commits_between_paths_in,
    get_commits_between_paths_with_files_in,
};
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

/// Build the shared `git log` argument vector for `from..to` over `paths`.
///
/// `paths` empty ⇒ no `--` pathspec (whole repo). Range follows the same
/// two-dot semantics documented on [`fetch_git_commits_in_paths`].
fn git_log_args(
    workspace_root: &std::path::Path,
    from: Option<&str>,
    to: Option<&str>,
    paths: &[String],
    name_only: bool,
) -> (Vec<String>, String) {
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
    ];
    if name_only {
        args.push("--name-only".into());
    }
    args.push("--pretty=format:%H%x1f%h%x1f%s%x1f%an%x1f%ae%x1f%b%x1e".into());
    args.push(range.clone());
    if !paths.is_empty() {
        args.push("--".into());
        for p in paths {
            args.push(p.clone());
        }
    }
    (args, range)
}

/// Run the shared `git log`, mapping a bad-revision exit to a fail-loud error
/// and any other non-zero exit to "no commits". Returns the raw stdout.
fn run_git_log(args: &[String], range: &str) -> Result<Option<String>> {
    use std::process::Command;
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
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()))
}

/// Fetch commits in `from..to` filtered to every path in `paths`
/// (`git log -- p1 p2 ...`); `paths` empty ⇒ whole repo.
///
/// The range follows git's `<from>..<to>` two-dot semantics:
/// - `from = Some, to = Some` → `<from>..<to>`
/// - `from = Some, to = None` → `<from>..HEAD`
/// - `from = None, to = Some` → `<to>` (all ancestors of `to`)
/// - `from = None, to = None` → `HEAD`
///
/// A non-zero git exit (e.g. an unknown tag, an empty range) is treated as
/// "no commits" rather than an error, matching the changelog stage's
/// best-effort history walk; a bad-revision exit fails loud.
pub(crate) fn fetch_git_commits_in_paths(
    workspace_root: &std::path::Path,
    from: Option<&str>,
    to: Option<&str>,
    paths: &[String],
) -> Result<Vec<anodizer_core::git::Commit>> {
    let (args, range) = git_log_args(workspace_root, from, to, paths, false);
    match run_git_log(&args, &range)? {
        Some(text) => Ok(parse_git_log_records(&text)),
        None => Ok(Vec::new()),
    }
}

/// `--name-only` sibling of [`fetch_git_commits_in_paths`]: each commit is
/// paired with its touched files for the precise `changelog.paths` glob
/// intersect over the directory pathspec.
pub(crate) fn fetch_git_commits_with_files_in(
    workspace_root: &std::path::Path,
    from: Option<&str>,
    to: Option<&str>,
    paths: &[String],
) -> Result<Vec<anodizer_core::git::CommitWithFiles>> {
    let (args, range) = git_log_args(workspace_root, from, to, paths, true);
    match run_git_log(&args, &range)? {
        Some(text) => Ok(anodizer_core::git::parse_commit_output_with_files(&text)),
        None => Ok(Vec::new()),
    }
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

/// Decode the `%H%x1f...%b%x1e` git-log wire format into [`Commit`]s by
/// delegating to the single core record decoder
/// (`anodizer_core::git::parse_commit_output`), so the body / author handling
/// can never drift from the rest of the changelog pipeline.
pub(crate) fn parse_git_log_records(text: &str) -> Vec<anodizer_core::git::Commit> {
    anodizer_core::git::parse_commit_output(text)
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

/// Fetch git commits scoped by `scope.dirs`, then apply the precise
/// `changelog.paths` glob intersect ([`anodizer_core::changelog_scope::ChangelogScope::narrow`]).
///
/// Only called when `scope.narrow.is_some()`: it fetches the commits' touched
/// files via `git log --name-only` (bounded by the same directory pathspec the
/// metadata-only path uses) and drops commits whose every touched file falls
/// outside the configured `changelog.paths` globs. The result is the exact
/// intersection of the derived directory scope with the glob filter.
///
/// `workspace_root` is the explicit repo root the fetch runs against — passed
/// in rather than read from the process cwd so the narrowed path matches the
/// engine-backed fetch and does not depend on implicit cwd.
pub(crate) fn fetch_git_commits_narrowed(
    workspace_root: &std::path::Path,
    prev_tag: &Option<String>,
    scope: &anodizer_core::changelog_scope::ChangelogScope,
    crate_name: &str,
    log: &StageLogger,
) -> Result<Vec<CommitInfo>> {
    let paths = scope.pathspecs();
    let pairs = match prev_tag {
        Some(tag) => get_commits_between_paths_with_files_in(workspace_root, tag, "HEAD", paths)
            .with_context(|| {
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
            get_all_commits_paths_with_files_in(workspace_root, paths).with_context(|| {
                format!("changelog: read all git commits for crate '{}'", crate_name)
            })?
        }
    };

    let mut all_commit_infos = Vec::new();
    for pair in pairs {
        if !scope.commit_survives_narrow(&pair.files) {
            continue;
        }
        let mut info = parse_commit_message(&pair.commit.message);
        info.hash = pair.commit.short_hash.clone();
        info.full_hash = pair.commit.hash.clone();
        info.author_name = pair.commit.author_name.clone();
        info.author_email = pair.commit.author_email.clone();
        info.co_authors = extract_co_authors(&pair.commit.body);
        all_commit_infos.push(info);
    }
    Ok(all_commit_infos)
}

pub(crate) fn fetch_git_commits(
    workspace_root: &std::path::Path,
    prev_tag: &Option<String>,
    paths: &[String],
    crate_name: &str,
    log: &StageLogger,
) -> Result<Vec<CommitInfo>> {
    let raw_commits = match prev_tag {
        Some(tag) => get_commits_between_paths_in(workspace_root, tag, "HEAD", paths)
            .with_context(|| {
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
            get_all_commits_paths_in(workspace_root, paths).with_context(|| {
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

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::log::{StageLogger, Verbosity};
    use anodizer_core::test_helpers::CwdGuard;
    use std::process::Command;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git runs")
            .success();
        assert!(ok, "git {args:?} failed");
    }

    // m1: the non-narrowed git fetch must run against the discovered
    // workspace_root, not the process cwd — so an off-root caller (cwd ≠ repo)
    // still reads the repo's commits.
    #[test]
    #[serial_test::serial]
    fn fetch_git_commits_uses_workspace_root_not_cwd() {
        let repo = tempfile::tempdir().expect("repo tempdir");
        let repo_path = repo.path();
        git(repo_path, &["init", "-q"]);
        git(repo_path, &["config", "user.email", "test@example.com"]);
        git(repo_path, &["config", "user.name", "Test"]);
        std::fs::write(repo_path.join("a.txt"), "seed").expect("write seed");
        git(repo_path, &["add", "."]);
        git(repo_path, &["commit", "-q", "-m", "feat: only-in-repo"]);

        // Point the process cwd at a DIFFERENT empty dir (not a git repo).
        let elsewhere = tempfile::tempdir().expect("cwd tempdir");
        let _cwd = CwdGuard::new(elsewhere.path()).expect("cwd guard");

        let log = StageLogger::new("changelog", Verbosity::Quiet);
        let commits = fetch_git_commits(repo_path, &None, &[], "demo", &log)
            .expect("fetch reads the workspace_root repo despite a foreign cwd");
        assert!(
            commits
                .iter()
                .any(|c| c.raw_message.contains("only-in-repo")),
            "the workspace_root repo's commit is returned: {commits:?}"
        );
    }
}
