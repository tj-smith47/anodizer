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
/// `git --version`, so this function deliberately has no `_in` sibling. The
/// spawn is pinned to a guaranteed-existing dir so the probe survives an
/// inherited cwd that was removed (see `path_util::probe_dir`).
pub fn check_git_available() -> Result<()> {
    let output = Command::new("git")
        .arg("--version")
        .current_dir(crate::path_util::probe_dir())
        .output();
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
/// Path-taking sibling of [`is_git_repo`]. A failed check is reported through
/// `tracing` at `warn` before `false` is returned, so a repository git refuses
/// to read — the canonical case being `detected dubious ownership in repository
/// at '<path>'` — is distinguishable from a directory that is genuinely not a
/// repository.
pub fn is_git_repo_in(cwd: &Path) -> bool {
    match git_output_in(cwd, &["rev-parse", "--git-dir"]) {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!("git repository check failed: {e}");
            false
        }
    }
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

/// Return the `git status --porcelain` output from a repository at `cwd`,
/// surfacing the underlying git failure instead of swallowing it.
///
/// `Ok(String)` carries the porcelain output (empty = clean tree); `Err`
/// signals that the `git status` invocation itself could not run or determine
/// cleanliness (cwd is not a git repository, git is absent, the index is
/// locked, …). Use this — not [`git_status_porcelain_in`] — for any guard that
/// must FAIL when it cannot prove the tree is clean, rather than treating an
/// indeterminate result as clean.
pub fn git_status_porcelain_result_in(cwd: &Path) -> Result<String> {
    git_output_in(cwd, &["status", "--porcelain"])
}

/// List the repository's tracked files (`git ls-files`) as repo-relative paths.
///
/// Drives the `anodizer init --version-files` enrollment discovery: the
/// candidate set is the tracked, text files that embed the current version, so
/// untracked build output and ignored scratch never enter the prompt. Returns
/// an empty list when the repository tracks no files; errors only if `git`
/// itself fails (not a repository, git unavailable).
pub fn list_tracked_files_in(cwd: &Path) -> Result<Vec<String>> {
    let out = git_output_in(cwd, &["ls-files", "-z"])?;
    Ok(out
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect())
}

/// Check whether the current repository is a shallow clone.
///
/// Delegates to [`is_shallow_clone_in`] on the process cwd.
pub fn is_shallow_clone() -> bool {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    is_shallow_clone_in(&cwd)
}

/// Check whether the repository at `cwd` is a shallow clone.
///
/// Path-taking sibling of [`is_shallow_clone`]. Asks `git rev-parse
/// --is-shallow-repository`, which reads the `shallow` file in the COMMON
/// dir, so a linked worktree of a shallow clone answers `true` too; a
/// `<git-dir>/shallow` probe missed it, the file living beside the main
/// worktree's `.git` only. A git older than 2.15 does not know the flag and
/// echoes it back instead of `true` / `false`; that answer falls back to the
/// existence of `<git-common-dir>/shallow`, the file the flag reads. A git
/// failure answers `false`.
pub fn is_shallow_clone_in(cwd: &Path) -> bool {
    match git_output_in(cwd, &["rev-parse", "--is-shallow-repository"]) {
        Ok(out) if out.trim() == "true" => true,
        Ok(out) if out.trim() == "false" => false,
        Ok(_) => git_output_in(cwd, &["rev-parse", "--git-common-dir"])
            .map(|common| cwd.join(common.trim()).join("shallow").exists())
            .unwrap_or(false),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn init_repo(dir: &Path) {
        let run = |args: &[&str]| {
            let out = anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(args)
                        .current_dir(dir)
                        .env("GIT_AUTHOR_NAME", "test")
                        .env("GIT_AUTHOR_EMAIL", "test@test.com")
                        .env("GIT_COMMITTER_NAME", "test")
                        .env("GIT_COMMITTER_EMAIL", "test@test.com");
                    cmd
                },
                "git",
            );
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
    #[serial_test::serial(tracing)]
    fn is_git_repo_in_warns_when_git_refuses_the_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let captured = crate::test_helpers::tracing_capture::capture_tracing_warnings(|| {
            assert!(!is_git_repo_in(tmp.path()));
        });
        assert!(
            captured.contains("git repository check failed"),
            "a refused repository check must be reported: {captured}"
        );
        assert!(
            captured.contains("not a git repository"),
            "git's own text must reach the log: {captured}"
        );
    }

    #[test]
    #[serial_test::serial(tracing)]
    fn is_git_repo_in_is_silent_for_a_real_repository() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let captured = crate::test_helpers::tracing_capture::capture_tracing_warnings(|| {
            assert!(is_git_repo_in(tmp.path()));
        });
        assert!(
            captured.is_empty(),
            "a readable repository warns about nothing: {captured}"
        );
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
    fn list_tracked_files_in_returns_committed_paths() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        std::fs::write(tmp.path().join("extra.txt"), "x").unwrap();
        let run = |args: &[&str]| {
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(args).current_dir(tmp.path());
                    cmd
                },
                "git",
            );
        };
        run(&["add", "extra.txt"]);
        run(&["commit", "-m", "add extra"]);
        let files = list_tracked_files_in(tmp.path()).unwrap();
        assert!(files.contains(&"README".to_string()), "got: {files:?}");
        assert!(files.contains(&"extra.txt".to_string()), "got: {files:?}");
    }

    #[test]
    fn is_shallow_clone_in_is_false_for_full_clone() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        assert!(!is_shallow_clone_in(tmp.path()));
    }

    #[test]
    fn is_shallow_clone_in_is_true_for_a_shallow_clone_and_its_linked_worktree() {
        let origin = tempfile::tempdir().unwrap();
        init_repo(origin.path());
        let git = |dir: &Path, args: &[&str]| {
            let out = anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(args).current_dir(dir);
                    cmd
                },
                "git",
            );
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        let shallow = tempfile::tempdir().unwrap();
        let url = format!("file://{}", origin.path().display());
        git(shallow.path(), &["clone", "-q", "--depth", "1", &url, "."]);
        assert!(is_shallow_clone_in(shallow.path()));

        let linked = shallow.path().join("linked");
        git(
            shallow.path(),
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                linked.to_str().unwrap(),
            ],
        );
        assert!(
            is_shallow_clone_in(&linked),
            "a linked worktree shares the shallow clone's history"
        );
    }

    /// A git that does not know `--is-shallow-repository` echoes the flag;
    /// the answer then comes from the `shallow` file in the common dir.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn is_shallow_clone_in_falls_back_to_the_shallow_file_when_git_echoes_the_flag() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let real_git = which_git();
        let tools = anodizer_core::test_helpers::fake_tool::FakeToolDir::new();
        tools
            .tool("git")
            .script(format!(
                "case \"$*\" in *--is-shallow-repository*) printf '%s\\n' --is-shallow-repository ;; \
                 *) exec \"{real_git}\" \"$@\" ;; esac\n"
            ))
            .install();
        let _path = tools.activate();
        assert!(
            !is_shallow_clone_in(tmp.path()),
            "no shallow file: a full clone"
        );
        std::fs::write(tmp.path().join(".git/shallow"), "").unwrap();
        assert!(is_shallow_clone_in(tmp.path()), "the shallow file decides");
    }

    /// The real `git` on PATH, resolved before a stub shadows it.
    #[cfg(unix)]
    fn which_git() -> String {
        let path = std::env::var_os("PATH").unwrap_or_default();
        std::env::split_paths(&path)
            .map(|p| p.join("git"))
            .find(|p| p.is_file())
            .expect("git on PATH")
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn porcelain_result_is_ok_empty_for_clean_repo() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let out = git_status_porcelain_result_in(tmp.path())
            .expect("a clean git repo must yield Ok(empty)");
        assert!(
            out.trim().is_empty(),
            "clean tree has no porcelain: {out:?}"
        );
    }

    #[test]
    fn porcelain_result_is_ok_with_paths_for_dirty_repo() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        std::fs::write(tmp.path().join("dirty.txt"), "x").unwrap();
        let out = git_status_porcelain_result_in(tmp.path())
            .expect("a reachable repo yields Ok even when dirty");
        assert!(out.contains("dirty.txt"), "dirty path listed: {out:?}");
    }

    #[test]
    fn porcelain_result_is_err_for_non_git_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            git_status_porcelain_result_in(tmp.path()).is_err(),
            "a non-git dir cannot prove cleanliness — must surface Err, not fail open"
        );
    }
}
