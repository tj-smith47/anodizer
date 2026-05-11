use anyhow::{Context as _, Result, bail};
use std::process::Command;

use super::git_output;

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

/// Get commits between two refs, optionally filtered to a path.
pub fn get_commits_between(from: &str, to: &str, path_filter: Option<&str>) -> Result<Vec<Commit>> {
    get_commits_between_paths(
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
    let output = git_output(&arg_refs)?;
    Ok(parse_commit_output(&output))
}

/// Get all commits reachable from HEAD, optionally filtered to a path.
/// Used for initial releases where there is no previous tag.
pub fn get_all_commits(path_filter: Option<&str>) -> Result<Vec<Commit>> {
    get_all_commits_paths(
        &path_filter
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>(),
    )
}

/// Get all commits reachable from HEAD, filtered to multiple paths.
pub fn get_all_commits_paths(paths: &[String]) -> Result<Vec<Commit>> {
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
    let output = git_output(&arg_refs)?;
    Ok(parse_commit_output(&output))
}

/// Get last N commit subjects.
pub fn get_last_commit_messages(count: usize) -> Result<Vec<String>> {
    let output = git_output(&[
        "-c",
        "log.showSignature=false",
        "log",
        &format!("-{count}"),
        "--pretty=format:%s",
    ])?;
    Ok(output.lines().map(str::to_string).collect())
}

/// Get commit subjects between two refs.
pub fn get_commit_messages_between(from: &str, to: &str) -> Result<Vec<String>> {
    let output = git_output(&[
        "-c",
        "log.showSignature=false",
        "log",
        "--pretty=format:%s",
        &format!("{from}..{to}"),
    ])?;
    Ok(output.lines().map(str::to_string).collect())
}

/// Get the current branch name.
pub fn get_current_branch() -> Result<String> {
    git_output(&["rev-parse", "--abbrev-ref", "HEAD"])
}

/// Check if there are any commits since a given tag.
pub fn has_commits_since_tag(tag: &str) -> Result<bool> {
    let range = format!("{}..HEAD", tag);
    let output = git_output(&["-c", "log.showSignature=false", "log", "--oneline", &range])?;
    Ok(!output.is_empty())
}

/// Get the short commit hash of HEAD.
pub fn get_short_commit() -> Result<String> {
    git_output(&["rev-parse", "--short", "HEAD"])
}

/// Get the full commit hash of HEAD.
///
/// Mirrors `ctx.Git.FullCommit` in GoReleaser (resolved at git-pipe time and
/// reused everywhere downstream). Used by the source-archive stage to
/// produce deterministic archives across consecutive commits when
/// `git_info` was not pre-populated by an earlier pipe.
pub fn get_head_commit() -> Result<String> {
    git_output(&["rev-parse", "HEAD"])
}

/// Check if there are changes in a path since a given tag.
pub fn has_changes_since(tag: &str, path: &str) -> Result<bool> {
    let output = git_output(&["diff", "--name-only", &format!("{}..HEAD", tag), "--", path])?;
    Ok(!output.is_empty())
}

/// Get last N commit subjects that touched a specific path.
pub fn get_last_commit_messages_path(count: usize, path: &str) -> Result<Vec<String>> {
    let output = git_output(&[
        "-c",
        "log.showSignature=false",
        "log",
        &format!("-{count}"),
        "--pretty=format:%s",
        "--",
        path,
    ])?;
    Ok(output.lines().map(str::to_string).collect())
}

/// Get commit subjects between two refs that touched a specific path.
pub fn get_commit_messages_between_path(from: &str, to: &str, path: &str) -> Result<Vec<String>> {
    let output = git_output(&[
        "-c",
        "log.showSignature=false",
        "log",
        "--pretty=format:%s",
        &format!("{from}..{to}"),
        "--",
        path,
    ])?;
    Ok(output.lines().map(str::to_string).collect())
}

/// Stage specific files and create a commit.
pub fn stage_and_commit(files: &[&str], message: &str) -> Result<()> {
    let mut args = vec!["add", "--"];
    args.extend(files.iter().copied());
    git_output(&args)?;
    git_output(&["commit", "-m", message])?;
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
        let stderr = crate::redact::redact_process_env(&stderr_raw);
        bail!("git add {} failed: {}", rel.display(), stderr.trim());
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
        let stderr = crate::redact::redact_process_env(&stderr_raw);
        bail!("git commit failed: {}", stderr.trim());
    }
    Ok(())
}

/// `git diff --name-only <tag>..HEAD -- <paths>...` — return `true` when
/// any of the named paths changed between `tag` and `HEAD`. Returns
/// `Ok(false)` when git fails (e.g. not a git repo) so callers can treat
/// the absence-of-info case as "no changes".
pub fn paths_changed_since_tag(tag: &str, paths: &[&str]) -> Result<bool> {
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
    let output = Command::new("git").args(&arg_refs).output()?;
    if output.status.success() {
        Ok(!String::from_utf8_lossy(&output.stdout).trim().is_empty())
    } else {
        Ok(false)
    }
}
