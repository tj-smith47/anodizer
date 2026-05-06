use anyhow::{Result, bail};
use std::process::Command;

use super::git_output;

/// Check whether the working tree has uncommitted changes.
pub fn is_git_dirty() -> bool {
    git_output(&["status", "--porcelain"])
        .map(|s| !s.is_empty())
        .unwrap_or(false)
}

/// Read `git config user.name`, or `None` if unset / git is unavailable.
pub fn local_git_user_name() -> Option<String> {
    git_output(&["config", "user.name"])
        .ok()
        .filter(|s| !s.is_empty())
}

/// Read `git config user.email`, or `None` if unset / git is unavailable.
pub fn local_git_user_email() -> Option<String> {
    git_output(&["config", "user.email"])
        .ok()
        .filter(|s| !s.is_empty())
}

/// Check whether `git` is available in PATH.
pub fn check_git_available() -> Result<()> {
    let output = Command::new("git").arg("--version").output();
    match output {
        Ok(o) if o.status.success() => Ok(()),
        _ => bail!("git is not installed or not in PATH. Install git and try again."),
    }
}

/// Check whether the current directory is inside a git repository.
pub fn is_git_repo() -> bool {
    git_output(&["rev-parse", "--git-dir"]).is_ok()
}

/// Return the `git status --porcelain` output showing dirty files.
pub fn git_status_porcelain() -> String {
    git_output(&["status", "--porcelain"]).unwrap_or_default()
}

/// Check whether the current repository is a shallow clone.
///
/// Returns `true` if the `.git/shallow` sentinel file exists, which git creates
/// when a repository was cloned with `--depth`.
pub fn is_shallow_clone() -> bool {
    // Use `git rev-parse --git-dir` to find the actual .git directory,
    // which handles worktrees and non-standard layouts.
    let git_dir = git_output(&["rev-parse", "--git-dir"]).unwrap_or_else(|_| ".git".to_string());
    std::path::Path::new(&git_dir).join("shallow").exists()
}
