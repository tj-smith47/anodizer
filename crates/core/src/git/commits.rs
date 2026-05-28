use anyhow::{Context as _, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::git_output_in;

#[derive(Debug, Clone)]
pub struct Commit {
    pub hash: String,
    pub short_hash: String,
    pub message: String,
    pub author_name: String,
    pub author_email: String,
    /// Full commit message body (everything after the subject line).
    /// Contains trailers like `Co-Authored-By:`.
    pub body: String,
}

/// Parse git log output (formatted as `%H%x1f%h%x1f%s%x1f%an%x1f%ae%x1f%b%x1e`)
/// into a vec of [`Commit`]s.
///
/// Uses ASCII record separator (0x1e) between commits and unit separator (0x1f)
/// between fields, so multi-line body text doesn't break parsing.
fn parse_commit_output(output: &str) -> Vec<Commit> {
    if output.is_empty() {
        return vec![];
    }
    output
        .split('\x1e')
        .filter(|record| !record.trim().is_empty())
        .filter_map(|record| {
            let fields: Vec<&str> = record.split('\x1f').collect();
            if fields.len() >= 5 {
                Some(Commit {
                    hash: fields[0].trim().to_string(),
                    short_hash: fields[1].to_string(),
                    message: fields[2].to_string(),
                    author_name: fields[3].to_string(),
                    author_email: fields[4].to_string(),
                    body: fields.get(5).unwrap_or(&"").trim().to_string(),
                })
            } else {
                None
            }
        })
        .collect()
}

fn cwd_or_dot() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Get commits between two refs, optionally filtered to a path.
pub fn get_commits_between(from: &str, to: &str, path_filter: Option<&str>) -> Result<Vec<Commit>> {
    get_commits_between_in(&cwd_or_dot(), from, to, path_filter)
}

/// Path-taking sibling of [`get_commits_between`].
pub fn get_commits_between_in(
    cwd: &Path,
    from: &str,
    to: &str,
    path_filter: Option<&str>,
) -> Result<Vec<Commit>> {
    get_commits_between_paths_in(
        cwd,
        from,
        to,
        &path_filter
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>(),
    )
}

/// Get commits between two refs, filtered to multiple paths (git log -- path1 path2 ...).
pub fn get_commits_between_paths(from: &str, to: &str, paths: &[String]) -> Result<Vec<Commit>> {
    get_commits_between_paths_in(&cwd_or_dot(), from, to, paths)
}

/// Path-taking sibling of [`get_commits_between_paths`].
pub fn get_commits_between_paths_in(
    cwd: &Path,
    from: &str,
    to: &str,
    paths: &[String],
) -> Result<Vec<Commit>> {
    let range = format!("{}..{}", from, to);
    let mut args = vec![
        "-c".to_string(),
        "log.showSignature=false".to_string(),
        "log".to_string(),
        "--pretty=format:%H%x1f%h%x1f%s%x1f%an%x1f%ae%x1f%b%x1e".to_string(),
        range,
    ];
    if !paths.is_empty() {
        args.push("--".to_string());
        for p in paths {
            args.push(p.clone());
        }
    }
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let output = git_output_in(cwd, &arg_refs)?;
    Ok(parse_commit_output(&output))
}

/// Get all commits reachable from HEAD, optionally filtered to a path.
/// Used for initial releases where there is no previous tag.
pub fn get_all_commits(path_filter: Option<&str>) -> Result<Vec<Commit>> {
    get_all_commits_in(&cwd_or_dot(), path_filter)
}

/// Path-taking sibling of [`get_all_commits`].
pub fn get_all_commits_in(cwd: &Path, path_filter: Option<&str>) -> Result<Vec<Commit>> {
    get_all_commits_paths_in(
        cwd,
        &path_filter
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>(),
    )
}

/// Get all commits reachable from HEAD, filtered to multiple paths.
pub fn get_all_commits_paths(paths: &[String]) -> Result<Vec<Commit>> {
    get_all_commits_paths_in(&cwd_or_dot(), paths)
}

/// Path-taking sibling of [`get_all_commits_paths`].
pub fn get_all_commits_paths_in(cwd: &Path, paths: &[String]) -> Result<Vec<Commit>> {
    let mut args = vec![
        "-c".to_string(),
        "log.showSignature=false".to_string(),
        "log".to_string(),
        "--pretty=format:%H%x1f%h%x1f%s%x1f%an%x1f%ae%x1f%b%x1e".to_string(),
        "HEAD".to_string(),
    ];
    if !paths.is_empty() {
        args.push("--".to_string());
        for p in paths {
            args.push(p.clone());
        }
    }
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let output = git_output_in(cwd, &arg_refs)?;
    Ok(parse_commit_output(&output))
}

/// Get last N commit subjects.
pub fn get_last_commit_messages(count: usize) -> Result<Vec<String>> {
    get_last_commit_messages_in(&cwd_or_dot(), count)
}

/// Path-taking sibling of [`get_last_commit_messages`].
pub fn get_last_commit_messages_in(cwd: &Path, count: usize) -> Result<Vec<String>> {
    let output = git_output_in(
        cwd,
        &[
            "-c",
            "log.showSignature=false",
            "log",
            &format!("-{count}"),
            "--pretty=format:%s",
        ],
    )?;
    Ok(output.lines().map(str::to_string).collect())
}

/// Get commit subjects between two refs.
pub fn get_commit_messages_between(from: &str, to: &str) -> Result<Vec<String>> {
    get_commit_messages_between_in(&cwd_or_dot(), from, to)
}

/// Path-taking sibling of [`get_commit_messages_between`].
pub fn get_commit_messages_between_in(cwd: &Path, from: &str, to: &str) -> Result<Vec<String>> {
    let output = git_output_in(
        cwd,
        &[
            "-c",
            "log.showSignature=false",
            "log",
            "--pretty=format:%s",
            &format!("{from}..{to}"),
        ],
    )?;
    Ok(output.lines().map(str::to_string).collect())
}

/// Get the current branch name.
pub fn get_current_branch() -> Result<String> {
    get_current_branch_in(&cwd_or_dot())
}

/// Path-taking sibling of [`get_current_branch`].
///
/// Handles detached-HEAD checkouts (e.g. `actions/checkout@v4` with `ref:`)
/// by resolving the branch HEAD points at via `for-each-ref`, falling back
/// to the remote's default branch and finally `GITHUB_REF_NAME` when set —
/// so downstream `git push origin <branch>` produces a valid refspec
/// instead of a literal `HEAD` that git can't auto-qualify.
pub fn get_current_branch_in(cwd: &Path) -> Result<String> {
    if let Ok(name) = git_output_in(cwd, &["symbolic-ref", "--short", "HEAD"]) {
        return Ok(name);
    }
    if let Ok(out) = git_output_in(
        cwd,
        &[
            "for-each-ref",
            "--points-at",
            "HEAD",
            "--format=%(refname:short)",
            "refs/heads/",
        ],
    ) && !out.is_empty()
    {
        let branches: Vec<&str> = out.lines().collect();
        for preferred in ["master", "main"] {
            if branches.contains(&preferred) {
                return Ok(preferred.to_string());
            }
        }
        if let Some(first) = branches.first() {
            return Ok((*first).to_string());
        }
    }
    if let Ok(out) = git_output_in(
        cwd,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    ) && let Some(name) = out.strip_prefix("origin/")
    {
        return Ok(name.to_string());
    }
    if let Ok(name) = std::env::var("GITHUB_REF_NAME")
        && !name.is_empty()
    {
        return Ok(name);
    }
    anyhow::bail!(
        "could not resolve current branch: HEAD is detached and no fallback (points-at-HEAD branches, origin/HEAD, GITHUB_REF_NAME) succeeded"
    )
}

/// Check if there are any commits since a given tag.
pub fn has_commits_since_tag(tag: &str) -> Result<bool> {
    has_commits_since_tag_in(&cwd_or_dot(), tag)
}

/// Path-taking sibling of [`has_commits_since_tag`].
pub fn has_commits_since_tag_in(cwd: &Path, tag: &str) -> Result<bool> {
    let range = format!("{}..HEAD", tag);
    let output = git_output_in(
        cwd,
        &["-c", "log.showSignature=false", "log", "--oneline", &range],
    )?;
    Ok(!output.is_empty())
}

/// Get the short commit hash of HEAD.
pub fn get_short_commit() -> Result<String> {
    get_short_commit_in(&cwd_or_dot())
}

/// Path-taking sibling of [`get_short_commit`].
pub fn get_short_commit_in(cwd: &Path) -> Result<String> {
    git_output_in(cwd, &["rev-parse", "--short", "HEAD"])
}

/// Default short-commit length used across error messages, log
/// output, and any place that needs to truncate a full SHA for
/// human display. Matches git's `--short` default (7) — and the
/// `ShortCommit` template var populated by [`super::detect_git_info`]
/// (which delegates to `git rev-parse --short`).
pub const SHORT_COMMIT_LEN: usize = 7;

/// Truncate a full commit SHA string to [`SHORT_COMMIT_LEN`]
/// characters. Returns the input unchanged when it's already shorter
/// or equal in length. Use this any time the SHA arrives as a string
/// (e.g. deserialized from a manifest or read from a template var)
/// rather than running `git rev-parse --short` again — saves a
/// subprocess and keeps the length convention in one place.
///
/// Empty input returns empty; callers needing fail-closed semantics
/// (e.g. publish-only's commit cross-check) check `is_empty()`
/// before calling.
pub fn short_commit_str(commit: &str) -> String {
    if commit.len() > SHORT_COMMIT_LEN {
        commit[..SHORT_COMMIT_LEN].to_string()
    } else {
        commit.to_string()
    }
}

/// Get the full commit hash of HEAD.
///
/// Mirrors `ctx.Git.FullCommit` in GoReleaser (resolved at git-pipe time and
/// reused everywhere downstream). Used by the source-archive stage to
/// produce deterministic archives across consecutive commits when
/// `git_info` was not pre-populated by an earlier pipe.
pub fn get_head_commit() -> Result<String> {
    get_head_commit_in(&cwd_or_dot())
}

/// Path-taking sibling of [`get_head_commit`].
pub fn get_head_commit_in(cwd: &Path) -> Result<String> {
    git_output_in(cwd, &["rev-parse", "HEAD"])
}

/// Check if there are changes in a path since a given tag.
pub fn has_changes_since(tag: &str, path: &str) -> Result<bool> {
    has_changes_since_in(&cwd_or_dot(), tag, path)
}

/// Path-taking sibling of [`has_changes_since`].
pub fn has_changes_since_in(cwd: &Path, tag: &str, path: &str) -> Result<bool> {
    let output = git_output_in(
        cwd,
        &["diff", "--name-only", &format!("{}..HEAD", tag), "--", path],
    )?;
    Ok(!output.is_empty())
}

/// Get last N commit subjects that touched a specific path.
pub fn get_last_commit_messages_path(count: usize, path: &str) -> Result<Vec<String>> {
    get_last_commit_messages_path_in(&cwd_or_dot(), count, path)
}

/// Path-taking sibling of [`get_last_commit_messages_path`].
pub fn get_last_commit_messages_path_in(
    cwd: &Path,
    count: usize,
    path: &str,
) -> Result<Vec<String>> {
    let output = git_output_in(
        cwd,
        &[
            "-c",
            "log.showSignature=false",
            "log",
            &format!("-{count}"),
            "--pretty=format:%s",
            "--",
            path,
        ],
    )?;
    Ok(output.lines().map(str::to_string).collect())
}

/// Get commit subjects between two refs that touched a specific path.
pub fn get_commit_messages_between_path(from: &str, to: &str, path: &str) -> Result<Vec<String>> {
    get_commit_messages_between_path_in(&cwd_or_dot(), from, to, path)
}

/// Path-taking sibling of [`get_commit_messages_between_path`].
pub fn get_commit_messages_between_path_in(
    cwd: &Path,
    from: &str,
    to: &str,
    path: &str,
) -> Result<Vec<String>> {
    let output = git_output_in(
        cwd,
        &[
            "-c",
            "log.showSignature=false",
            "log",
            "--pretty=format:%s",
            &format!("{from}..{to}"),
            "--",
            path,
        ],
    )?;
    Ok(output.lines().map(str::to_string).collect())
}

/// Stage specific files and create a commit.
pub fn stage_and_commit(files: &[&str], message: &str) -> Result<()> {
    stage_and_commit_in(&cwd_or_dot(), files, message)
}

/// Path-taking sibling of [`stage_and_commit`].
pub fn stage_and_commit_in(cwd: &Path, files: &[&str], message: &str) -> Result<()> {
    let mut args = vec!["add", "--"];
    args.extend(files.iter().copied());
    git_output_in(cwd, &args)?;
    git_output_in(cwd, &["commit", "-m", message])?;
    Ok(())
}

/// `git -C <workspace_root> -c log.showSignature=false log
/// --pretty=format:%B%x1e <range> -- <rel_path>` — list commit message
/// bodies (subject+body) for commits in `range` touching `rel_path`,
/// using the `\x1e` (RS) byte as a between-commits separator so multi-line
/// bodies survive parsing.
///
/// `range` is the git revision range as a string (e.g. `"HEAD"`,
/// `"v0.3.0..HEAD"`); the empty string is invalid (caller must pre-filter).
/// Returns `Ok(Vec::new())` when git fails so callers treat
/// "range doesn't exist yet" as a non-error.
pub fn log_subjects_for_range(
    workspace_root: &std::path::Path,
    range: &str,
    rel_path: &str,
) -> Result<Vec<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args([
            "-c",
            "log.showSignature=false",
            "log",
            "--pretty=format:%B%x1e",
            range,
            "--",
            rel_path,
        ])
        .output()?;
    if !out.status.success() {
        // Range may not exist yet (no last_tag, path not in history).
        return Ok(Vec::new());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .split('\x1e')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

/// `git -C <workspace_root> add <rel>` — stage a single relative path.
pub fn add_path_in(workspace_root: &std::path::Path, rel: &std::path::Path) -> Result<()> {
    let out = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .arg("add")
        .arg(rel)
        .output()
        .context("failed to invoke git add")?;
    if !out.status.success() {
        let stderr_raw = String::from_utf8_lossy(&out.stderr);
        let raw = format!("git add {} failed: {}", rel.display(), stderr_raw.trim());
        bail!("{}", crate::redact::redact_process_env(&raw));
    }
    Ok(())
}

/// `git -C <workspace_root> commit [-S] -m <message>` — create a commit
/// with the given message, optionally GPG-signed.
pub fn commit_in(workspace_root: &std::path::Path, message: &str, sign: bool) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(workspace_root).arg("commit");
    if sign {
        cmd.arg("-S");
    }
    cmd.arg("-m").arg(message);
    let out = cmd.output().context("failed to invoke git commit")?;
    if !out.status.success() {
        let stderr_raw = String::from_utf8_lossy(&out.stderr);
        let raw = format!("git commit failed: {}", stderr_raw.trim());
        bail!("{}", crate::redact::redact_process_env(&raw));
    }
    Ok(())
}

/// `git diff --name-only <tag>..HEAD -- <paths>...` — return `true` when
/// any of the named paths changed between `tag` and `HEAD`. Returns
/// `Ok(false)` when git fails (e.g. not a git repo) so callers can treat
/// the absence-of-info case as "no changes".
pub fn paths_changed_since_tag(tag: &str, paths: &[&str]) -> Result<bool> {
    paths_changed_since_tag_in(&cwd_or_dot(), tag, paths)
}

/// Path-taking sibling of [`paths_changed_since_tag`].
pub fn paths_changed_since_tag_in(cwd: &Path, tag: &str, paths: &[&str]) -> Result<bool> {
    let mut args: Vec<String> = vec![
        "diff".to_string(),
        "--name-only".to_string(),
        format!("{tag}..HEAD"),
        "--".to_string(),
    ];
    for p in paths {
        args.push((*p).to_string());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = Command::new("git")
        .current_dir(cwd)
        .args(&arg_refs)
        .output()?;
    if output.status.success() {
        Ok(!String::from_utf8_lossy(&output.stdout).trim().is_empty())
    } else {
        Ok(false)
    }
}

/// `git -C <repo> rev-parse HEAD` — return HEAD's full commit hash for the
/// given repository (or worktree). Path-taking sibling of
/// [`get_head_commit`] so callers (the determinism harness, future CI
/// glue) can resolve HEAD without `cd`-ing into the repo first.
pub fn head_commit_hash_in(repo: &std::path::Path) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .context("failed to invoke git rev-parse HEAD")?;
    if !out.status.success() {
        let stderr_raw = String::from_utf8_lossy(&out.stderr);
        let raw = format!("git rev-parse HEAD failed: {}", stderr_raw.trim());
        bail!("{}", crate::redact::redact_process_env(&raw));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `git -C <repo> log -1 --format=%ct HEAD` — return HEAD's committer
/// timestamp (seconds since UNIX epoch) for the given repository. Used by
/// the determinism harness as the non-snapshot SDE seed.
pub fn head_commit_timestamp_in(repo: &std::path::Path) -> Result<i64> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["log", "-1", "--format=%ct", "HEAD"])
        .output()
        .context("failed to invoke git log -1 --format=%ct HEAD")?;
    if !out.status.success() {
        let stderr_raw = String::from_utf8_lossy(&out.stderr);
        let raw = format!("git log -1 --format=%ct HEAD failed: {}", stderr_raw.trim());
        bail!("{}", crate::redact::redact_process_env(&raw));
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    text.parse::<i64>()
        .with_context(|| format!("git log --format=%ct returned non-i64 timestamp: {}", text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn init_repo_with_commits(dir: &Path, files: &[&str]) {
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t.com")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed");
        };
        run(&["init"]);
        run(&["config", "user.email", "t@t.com"]);
        run(&["config", "user.name", "t"]);
        for (i, f) in files.iter().enumerate() {
            std::fs::write(dir.join(f), format!("c{i}")).unwrap();
            run(&["add", "."]);
            run(&["commit", "-m", &format!("commit-{i}: {f}")]);
        }
    }

    #[test]
    fn get_head_commit_in_returns_tempdirs_head_sha() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo_with_commits(tmp.path(), &["a"]);
        let expected = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(tmp.path())
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        let sha = get_head_commit_in(tmp.path()).unwrap();
        assert_eq!(sha, expected);
    }

    #[test]
    fn get_short_commit_in_returns_tempdirs_short_sha() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo_with_commits(tmp.path(), &["a"]);
        let expected = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "--short", "HEAD"])
                .current_dir(tmp.path())
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        let short = get_short_commit_in(tmp.path()).unwrap();
        assert_eq!(short, expected);
    }

    #[test]
    fn has_commits_since_tag_in_returns_false_when_tag_is_head() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_commits(dir, &["a"]);
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t.com")
                .output()
                .unwrap();
        };
        run(&["tag", "v1.0.0"]);
        assert!(!has_commits_since_tag_in(dir, "v1.0.0").unwrap());
    }

    #[test]
    fn get_current_branch_in_returns_branch_name() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t.com")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed");
        };
        run(&["-c", "init.defaultBranch=t1-test-branch", "init"]);
        run(&["config", "user.email", "t@t.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.join("a"), "1").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "c1"]);
        let branch = get_current_branch_in(dir).unwrap();
        assert_eq!(branch, "t1-test-branch");
    }

    #[test]
    fn get_current_branch_in_resolves_detached_head_via_points_at() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t.com")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed");
        };
        run(&["-c", "init.defaultBranch=master", "init"]);
        run(&["config", "user.email", "t@t.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.join("a"), "1").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "c1"]);
        let sha = get_head_commit_in(dir).unwrap();
        run(&["checkout", "--detach", &sha]);
        let branch = get_current_branch_in(dir).unwrap();
        assert_eq!(
            branch, "master",
            "detached HEAD pointing at master must resolve to master, not literal HEAD"
        );
    }
}
