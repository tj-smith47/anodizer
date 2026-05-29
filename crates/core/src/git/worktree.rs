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
//! panic. Failure to remove is surfaced via `tracing::warn!` with the
//! captured stderr — we never panic during `Drop`, but silent swallowing
//! left operators with no signal when the cleanup raced an I/O error.
//! Operators can still run `git worktree prune` to reap the stale
//! administrative entry after such a leak.

use anyhow::{Context as _, Result};
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
    ///
    /// On failure (path collision, locked worktree, invalid commit,
    /// dirty index) the returned error includes the captured stderr
    /// from git so the operator has an actionable detail rather than
    /// an opaque "git worktree add failed".
    ///
    /// # Errors
    ///
    /// Returns an error if `path` contains ASCII whitespace. The
    /// determinism harness composes `path` into RUSTFLAGS via
    /// `--remap-path-prefix=<path>=/anodize`, and RUSTFLAGS is a
    /// space-delimited token list with no quoting mechanism — a path
    /// containing whitespace would be parsed as multiple arguments by
    /// rustc and either silently misremap or hard-fail the build. Reject
    /// at construction so the operator sees a clear "rename the
    /// scratch dir" message instead of an opaque rustc parse error
    /// later.
    pub fn add(repo_root: &Path, path: &Path, commit: &str) -> Result<Self> {
        if path.to_string_lossy().chars().any(char::is_whitespace) {
            anyhow::bail!(
                "git worktree path {} contains whitespace; pick a scratch directory \
                 without spaces or tabs (the determinism harness composes this path \
                 into RUSTFLAGS via `--remap-path-prefix`, which is space-delimited \
                 with no quoting support)",
                path.display()
            );
        }
        let out = Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .args(["worktree", "add", "--detach"])
            .arg(path)
            .arg(commit)
            .output()
            .with_context(|| format!("spawn 'git worktree add' for {}", path.display()))?;
        if !out.status.success() {
            anyhow::bail!(
                "git worktree add failed (exit {:?}) for {}: {}",
                out.status.code(),
                path.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
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
        // path was deleted externally), surface the failure via
        // `tracing::warn!` with the captured stderr so the operator can
        // run `git worktree prune` in the parent repo to reap the
        // stale administrative entry. Silent swallowing (the previous
        // behavior) left operators with no signal that a leak had
        // happened.
        match Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .args(["worktree", "remove", "--force"])
            .arg(&self.path)
            .output()
        {
            Ok(out) if out.status.success() => {}
            Ok(out) => {
                tracing::warn!(
                    "git worktree remove '{}' failed during Drop (exit {:?}: {}); \
                     run `git worktree prune` in the parent repo to reap the stale entry",
                    self.path.display(),
                    out.status.code(),
                    String::from_utf8_lossy(&out.stderr).trim(),
                );
            }
            Err(err) => {
                tracing::warn!(
                    "failed to spawn 'git worktree remove' for '{}' during Drop ({err}); \
                     run `git worktree prune` in the parent repo to reap the stale entry",
                    self.path.display(),
                );
            }
        }
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

    #[test]
    fn worktree_add_surfaces_stderr_on_failure() {
        // Invalid commit-ish: git emits a `fatal:` stderr line. The
        // returned error must carry that detail through, not the
        // previous opaque "git worktree add failed" string.
        let repo = init_repo();
        let wt_dir = tempfile::tempdir().unwrap();
        let result = Worktree::add(
            repo.path(),
            &wt_dir.path().join("wt-bad"),
            "this-ref-does-not-exist-anywhere",
        );
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("invalid commit must error"),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("fatal:")
                || msg.contains("invalid reference")
                || msg.contains("not a valid"),
            "error must include captured git stderr; got: {msg}",
        );
        assert!(
            msg.contains("git worktree add failed"),
            "error must still identify the failing operation; got: {msg}",
        );
    }

    #[test]
    fn worktree_add_rejects_whitespace_in_path() {
        // Whitespace in the worktree path breaks RUSTFLAGS injection
        // downstream (--remap-path-prefix=<path>=...), so Worktree::add
        // must reject the path before git ever runs.
        let repo = init_repo();
        let wt_dir = tempfile::tempdir().unwrap();
        let bad_path = wt_dir.path().join("wt with spaces");
        let err = match Worktree::add(repo.path(), &bad_path, "HEAD") {
            Err(e) => e,
            Ok(_) => panic!("whitespace path must be rejected"),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("whitespace"),
            "error must explain the whitespace constraint; got: {msg}"
        );
        assert!(
            msg.contains("RUSTFLAGS"),
            "error must point at the downstream RUSTFLAGS reason; got: {msg}"
        );
    }

    #[test]
    fn worktree_drop_does_not_panic_when_path_already_removed() {
        // Simulate an external actor (operator, racing CI cleanup)
        // removing the worktree directory out from under us. Drop must
        // not panic; the failure is surfaced via tracing::warn! which
        // the test harness does not assert on directly — we only
        // assert the absence of a panic.
        let repo = init_repo();
        let wt_dir = tempfile::tempdir().unwrap();
        let wt = Worktree::add(repo.path(), &wt_dir.path().join("wt-vanish"), "HEAD").unwrap();
        let path = wt.path().to_path_buf();
        std::fs::remove_dir_all(&path).expect("manual remove should succeed");
        assert!(!path.exists());
        // Drop here — must not panic even though the path is gone.
        drop(wt);
    }
}
