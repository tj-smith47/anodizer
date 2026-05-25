use anyhow::{Result, bail};
use std::path::Path;
use std::process::Command;

use super::git_output_in;

/// Check whether the working tree has uncommitted changes.
pub fn is_git_dirty() -> bool {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    is_git_dirty_in(&cwd)
}

/// Check whether the working tree in `cwd` has uncommitted changes.
///
/// Path-taking sibling of [`is_git_dirty`] so callers (notably tests against a
/// fixture repo under `tempfile::tempdir()`) don't have to mutate the process cwd.
pub fn is_git_dirty_in(cwd: &Path) -> bool {
    git_output_in(cwd, &["status", "--porcelain"])
        .map(|s| !s.is_empty())
        .unwrap_or(false)
}

/// Read `git config user.name`, or `None` if unset / git is unavailable.
pub fn local_git_user_name() -> Option<String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    local_git_user_name_in(&cwd)
}

/// Read `git config user.name` from a repository at `cwd`.
///
/// Path-taking sibling of [`local_git_user_name`].
pub fn local_git_user_name_in(cwd: &Path) -> Option<String> {
    git_output_in(cwd, &["config", "user.name"])
        .ok()
        .filter(|s| !s.is_empty())
}

/// Read `git config user.email`, or `None` if unset / git is unavailable.
pub fn local_git_user_email() -> Option<String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    local_git_user_email_in(&cwd)
}

/// Read `git config user.email` from a repository at `cwd`.
///
/// Path-taking sibling of [`local_git_user_email`].
pub fn local_git_user_email_in(cwd: &Path) -> Option<String> {
    git_output_in(cwd, &["config", "user.email"])
        .ok()
        .filter(|s| !s.is_empty())
}

/// Check whether `git` is available in PATH.
///
/// Binary-presence probe; the working directory has no effect on
/// `git --version`, so this function deliberately has no `_in` sibling.
pub fn check_git_available() -> Result<()> {
    let output = Command::new("git").arg("--version").output();
    match output {
        Ok(o) if o.status.success() => Ok(()),
        _ => bail!("git is not installed or not in PATH. Install git and try again."),
    }
}

/// Check whether the current directory is inside a git repository.
pub fn is_git_repo() -> bool {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    is_git_repo_in(&cwd)
}

/// Check whether `cwd` is inside a git repository.
///
/// Path-taking sibling of [`is_git_repo`].
pub fn is_git_repo_in(cwd: &Path) -> bool {
    git_output_in(cwd, &["rev-parse", "--git-dir"]).is_ok()
}

/// Return the `git status --porcelain` output showing dirty files.
pub fn git_status_porcelain() -> String {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    git_status_porcelain_in(&cwd)
}

/// Return the `git status --porcelain` output from a repository at `cwd`.
///
/// Path-taking sibling of [`git_status_porcelain`].
pub fn git_status_porcelain_in(cwd: &Path) -> String {
    git_output_in(cwd, &["status", "--porcelain"]).unwrap_or_default()
}

/// Check whether the current repository is a shallow clone.
///
/// Returns `true` if the `.git/shallow` sentinel file exists, which git creates
/// when a repository was cloned with `--depth`.
pub fn is_shallow_clone() -> bool {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    is_shallow_clone_in(&cwd)
}

/// Check whether the repository at `cwd` is a shallow clone.
///
/// Path-taking sibling of [`is_shallow_clone`]. The `.git/shallow` sentinel
/// is resolved relative to `cwd` via `git rev-parse --git-dir`; when that
/// command returns a relative path (the common case for non-worktree repos),
/// it is joined onto `cwd` so the check stays self-contained.
pub fn is_shallow_clone_in(cwd: &Path) -> bool {
    // Use `git rev-parse --git-dir` to find the actual .git directory,
    // which handles worktrees and non-standard layouts.
    let git_dir =
        git_output_in(cwd, &["rev-parse", "--git-dir"]).unwrap_or_else(|_| ".git".to_string());
    let git_dir_path = Path::new(&git_dir);
    let shallow = if git_dir_path.is_absolute() {
        git_dir_path.join("shallow")
    } else {
        cwd.join(git_dir_path).join("shallow")
    };
    shallow.exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn init_repo(dir: &Path) {
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@test.com")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@test.com")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init"]);
        run(&["config", "user.email", "test@test.com"]);
        run(&["config", "user.name", "Status Tester"]);
        std::fs::write(dir.join("README"), "init").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "initial"]);
    }

    #[test]
    fn is_git_repo_in_returns_false_for_non_git_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!is_git_repo_in(tmp.path()));
    }

    #[test]
    fn is_git_repo_in_returns_true_for_initialized_repo() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        assert!(is_git_repo_in(tmp.path()));
    }

    #[test]
    fn is_git_dirty_in_is_false_for_clean_repo() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        assert!(!is_git_dirty_in(tmp.path()));
    }

    #[test]
    fn is_git_dirty_in_is_true_after_untracked_change() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        std::fs::write(tmp.path().join("new.txt"), "hello").unwrap();
        assert!(is_git_dirty_in(tmp.path()));
    }

    #[test]
    fn git_status_porcelain_in_reflects_dirty_state() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        std::fs::write(tmp.path().join("staged.txt"), "x").unwrap();
        let status = git_status_porcelain_in(tmp.path());
        assert!(status.contains("staged.txt"), "got: {status:?}");
    }

    #[test]
    fn local_git_user_name_in_reads_repo_config() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        assert_eq!(
            local_git_user_name_in(tmp.path()).as_deref(),
            Some("Status Tester")
        );
    }

    #[test]
    fn local_git_user_email_in_reads_repo_config() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        assert_eq!(
            local_git_user_email_in(tmp.path()).as_deref(),
            Some("test@test.com")
        );
    }

    #[test]
    fn is_shallow_clone_in_is_false_for_full_clone() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        assert!(!is_shallow_clone_in(tmp.path()));
    }
}
