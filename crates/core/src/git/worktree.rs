//! `git worktree` wrapper for hermetic per-run workspaces.
//!
//! The determinism harness uses this to obtain a clean copy of the
//! workspace rooted at a specific commit, so `.gitignored` byproducts
//! (`target/`, `dist/`, `node_modules/`, etc.) from prior runs cannot
//! leak between builds.
//!
//! `Worktree::add` constructs the worktree (detached HEAD at the supplied
//! commit). `Drop` is best-effort: it runs `git worktree remove --force`
//! against the parent repo so the temporary tree is cleaned up even on
//! panic. Failure to remove is intentionally swallowed (we never panic
//! during `Drop`) — operators can run `git worktree prune` to recover
//! from a leak if the cleanup ever raced an I/O error.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Worktree {
    repo_root: PathBuf,
    path: PathBuf,
}

impl Worktree {
    /// Create a new detached worktree at `path`, checked out at `commit`.
    ///
    /// `commit` may be any valid git revision (sha, ref name, `HEAD`).
    /// The parent repository is `repo_root`; `git -C <repo_root>
    /// worktree add --detach <path> <commit>` is invoked verbatim.
    pub fn add(repo_root: &Path, path: &Path, commit: &str) -> Result<Self> {
        let status = Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .args(["worktree", "add", "--detach"])
            .arg(path)
            .arg(commit)
            .status()?;
        anyhow::ensure!(status.success(), "git worktree add failed");
        Ok(Self {
            repo_root: repo_root.to_path_buf(),
            path: path.to_path_buf(),
        })
    }

    /// Absolute path to the worktree on disk.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        // Best-effort: never panic in Drop. If `git worktree remove`
        // fails (e.g. the worktree was already removed manually, or the
        // path was deleted externally), there's nothing useful we can
        // do here — the next `git worktree prune` in the parent repo
        // will reap the stale administrative entry.
        let _ = Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .args(["worktree", "remove", "--force"])
            .arg(&self.path)
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn init_repo() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .arg("init")
            .output()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["config", "user.email", "test@example.com"])
            .output()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["config", "user.name", "test"])
            .output()
            .unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["add", "a.txt"])
            .output()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["commit", "-m", "init"])
            .output()
            .unwrap();
        dir
    }

    #[test]
    fn worktree_add_creates_directory_at_given_path() {
        let repo = init_repo();
        let wt_dir = tempfile::tempdir().unwrap();
        let wt = Worktree::add(repo.path(), &wt_dir.path().join("wt1"), "HEAD").unwrap();
        assert!(wt.path().exists());
        assert!(wt.path().join("a.txt").exists());
    }

    #[test]
    fn worktree_drop_removes_directory_and_prunes() {
        let repo = init_repo();
        let wt_dir = tempfile::tempdir().unwrap();
        let path: PathBuf;
        {
            let wt = Worktree::add(repo.path(), &wt_dir.path().join("wt2"), "HEAD").unwrap();
            path = wt.path().to_path_buf();
            assert!(path.exists());
        } // dropped here
        // After drop, the path should be gone.
        assert!(!path.exists(), "worktree path persisted after Drop");
    }

    #[test]
    fn worktree_add_for_explicit_commit_checks_out_that_commit() {
        let repo = init_repo();
        // Get HEAD commit hash
        let out = Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let head_hash = String::from_utf8(out.stdout).unwrap().trim().to_string();
        let wt_dir = tempfile::tempdir().unwrap();
        let wt = Worktree::add(repo.path(), &wt_dir.path().join("wt3"), &head_hash).unwrap();
        // Verify the worktree's HEAD matches.
        let out = Command::new("git")
            .arg("-C")
            .arg(wt.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let wt_head = String::from_utf8(out.stdout).unwrap().trim().to_string();
        assert_eq!(wt_head, head_hash);
    }

    #[test]
    fn worktree_concurrent_adds_do_not_collide() {
        let repo = init_repo();
        let wt_dir = tempfile::tempdir().unwrap();
        let wt1 = Worktree::add(repo.path(), &wt_dir.path().join("wt-a"), "HEAD").unwrap();
        let wt2 = Worktree::add(repo.path(), &wt_dir.path().join("wt-b"), "HEAD").unwrap();
        assert_ne!(wt1.path(), wt2.path());
        assert!(wt1.path().exists());
        assert!(wt2.path().exists());
    }
}
