//! Shared `git revert HEAD --no-edit` + `git push` helper used by every
//! git-revert publisher (homebrew / scoop / nix / our-AUR) whose rollback
//! shape is "create a revert commit on the publisher-owned repo, push
//! it to the same branch".
//!
//! Why re-clone instead of reuse a `target/anodize/<publisher>/` clone?
//! All four git-revert publishers clone into a `tempfile::tempdir()` that
//! is dropped at the end of `publish_to_X`. Persisting the clone would
//! change the publish path's working-tree footprint (each publisher's
//! `publish_to_X` body intentionally leaves no on-disk state) and would
//! leak secrets onto disk for longer than necessary. Recording
//! `{repo_url, branch, ssh hints}` in
//! [`anodizer_core::PublishEvidence::extra`] and re-cloning at rollback
//! time keeps the publish path intact and trades one extra `git clone`
//! (rare, only on rollback) for a smaller blast radius.
//!
//! The helper itself shells out to real `git` via
//! [`std::process::Command`]. The tests use a tempdir-backed real repo
//! with a bare remote so the helper exercises the same code path it
//! will hit in production.

use anyhow::{Context as _, Result};
use std::path::Path;
use std::process::Command;

use super::clone::{clone_repo_ssh, clone_repo_with_auth};
use anodizer_core::log::StageLogger;

/// Description of a publisher-owned repo whose HEAD should be reverted +
/// pushed.
///
/// This is the structured form of what gets serialized into
/// `PublishEvidence.extra` and decoded back at rollback time. Each
/// git-revert publisher records one of these per target it pushed to.
///
/// `target` identifies the artifact within the publisher (formula
/// name, manifest name, AUR package name, ...) so rollback warnings
/// can include it. The remaining fields describe how to re-clone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RevertTarget {
    /// Per-publisher human label for the target — typically the
    /// crate name, formula name, or cask name. Surfaces in log lines.
    pub target: String,
    /// HTTPS clone URL (`https://github.com/owner/name.git`) OR a non-
    /// HTTPS git URL (AUR: `ssh://aur@aur.archlinux.org/<pkg>.git`).
    pub repo_url: String,
    /// Branch to push back to. `None` means "use the cloned default
    /// branch" — `clone --depth=1` puts HEAD on the default branch,
    /// so the helper falls back to `HEAD` in the push refspec.
    pub branch: Option<String>,
    /// Auth token (used only when `repo_url` is HTTPS). Captured at
    /// run-time from the same env-var resolver `clone_repo` uses so
    /// rollback works without re-resolving config. `None` means use
    /// the system's git credential helper / unauthenticated clone.
    ///
    /// SECURITY: the token never lands in `PublishEvidence.extra` —
    /// rollback resolves it from the env at rollback time via
    /// [`crate::util::resolve_repo_token`]. This struct's `token`
    /// field is populated only in the in-process call shape passed
    /// directly to [`run_git_revert_and_push`]; it is NOT serialized.
    pub token: Option<String>,
    /// SSH private-key material (only when `repo_url` is non-HTTPS).
    /// Same secret-safety contract as `token` above — never
    /// serialized into evidence; resolved at rollback time from the
    /// caller-provided source.
    pub private_key: Option<String>,
    /// Custom `GIT_SSH_COMMAND` override; mirrors the publish path's
    /// SSH plumbing in [`crate::util::clone_repo_ssh`].
    pub ssh_command: Option<String>,
}

/// Re-clone the publisher-owned repo, create a `git revert HEAD --no-edit`
/// commit on top, and push it to the same branch.
///
/// Failure modes (any one is a hard error from this helper — callers
/// catch + warn so other targets still get tried):
///
/// 1. Clone fails (network, auth, repo gone).
/// 2. `git revert HEAD --no-edit` fails (empty repo, merge conflict).
/// 3. `git push` fails (branch protection, race, auth revoked).
///
/// Success path on a clean repo: a single new commit lands on the
/// branch, formatted by `git revert` as `Revert "<subject>"`.
pub(crate) fn run_git_revert_and_push(target: &RevertTarget, log: &StageLogger) -> Result<()> {
    let tmp_dir = tempfile::tempdir().context("git_revert: create temp dir")?;
    let repo_path = tmp_dir.path();

    // Re-clone using the same shape as the publish path: SSH when an
    // SSH URL or private-key is present, HTTPS-with-token otherwise.
    // This keeps the rollback code path on the same authentication
    // contract the publish path validated at run-time.
    let is_ssh = !target.repo_url.starts_with("https://")
        || target.private_key.is_some()
        || target.ssh_command.is_some();
    if is_ssh {
        clone_repo_ssh(
            &target.repo_url,
            target.private_key.as_deref(),
            target.ssh_command.as_deref(),
            repo_path,
            "git_revert",
            log,
        )?;
    } else {
        clone_repo_with_auth(
            &target.repo_url,
            target.token.as_deref(),
            repo_path,
            "git_revert",
            log,
        )?;
    }

    revert_head_in(repo_path)?;
    push_after_revert(repo_path, target.branch.as_deref())?;
    Ok(())
}

/// Run `git revert HEAD --no-edit` inside `path`. Captures stderr on
/// failure so a merge-conflict / empty-repo failure mode is visible.
///
/// `--no-edit` keeps the revert non-interactive (no $EDITOR invocation
/// in CI / non-tty environments).
fn revert_head_in(path: &Path) -> Result<()> {
    // Reject a dirty working tree up front. `git revert` would otherwise
    // either succeed against a clean tree or fail with a less actionable
    // "your local changes would be overwritten" message. We surface the
    // dirty-tree condition explicitly so the warn line a caller logs
    // points at the right cause.
    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(path)
        .output()
        .with_context(|| format!("git_revert: git status in {}", path.display()))?;
    if !status.status.success() {
        anyhow::bail!(
            "git_revert: git status failed in {} (exit {})\nstderr: {}",
            path.display(),
            status.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&status.stderr),
        );
    }
    if !status.stdout.is_empty() {
        anyhow::bail!(
            "git_revert: refusing to revert in a dirty working tree at {}\nstatus:\n{}",
            path.display(),
            String::from_utf8_lossy(&status.stdout),
        );
    }

    let output = Command::new("git")
        .args(["revert", "HEAD", "--no-edit"])
        .current_dir(path)
        .output()
        .with_context(|| format!("git_revert: git revert in {}", path.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "git_revert: git revert HEAD failed in {} (exit {})\nstderr: {}",
            path.display(),
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    Ok(())
}

/// Push the revert commit back to the publisher-owned remote.
///
/// `branch = Some("master")` pushes to `origin master`. `branch = None`
/// pushes `HEAD` to its current upstream — same shape `commit_and_push_
/// with_opts` uses for publishers that don't pin a branch.
fn push_after_revert(path: &Path, branch: Option<&str>) -> Result<()> {
    let args: Vec<&str> = match branch {
        Some(b) => vec!["push", "origin", b],
        None => vec!["push", "origin", "HEAD"],
    };
    let output = Command::new("git")
        .args(&args)
        .current_dir(path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .with_context(|| format!("git_revert: git push in {}", path.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "git_revert: git push failed in {} (exit {})\nstderr: {}",
            path.display(),
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::log::{StageLogger, Verbosity};
    use std::process::Command;
    use std::sync::OnceLock;

    /// Ensure the test process has a git identity that `git revert` /
    /// `git commit` can use. Cargo CI runners (Linux + macOS + Windows)
    /// do not ship a global ~/.gitconfig, so without this the helper's
    /// internal `git revert` (which spawns its own `Command::new("git")`
    /// inheriting our env) fails with "Author identity unknown". We can't
    /// pass `.env()` to that internal spawn from the test, but env vars
    /// set on the parent process propagate. Set them once per test
    /// process via `OnceLock` to avoid the parallel-test race that
    /// repeated `set_var` calls would otherwise introduce.
    fn ensure_git_identity() {
        static INIT: OnceLock<()> = OnceLock::new();
        INIT.get_or_init(|| {
            // SAFETY: env mutation runs once per process, guarded by
            // OnceLock; no other test thread observes a half-applied
            // identity. The values are constants, not user input.
            unsafe {
                // env-ok: idempotent OnceLock set of constant git identity, never mutated after
                std::env::set_var("GIT_AUTHOR_NAME", "Anodize Test");
                // env-ok: idempotent OnceLock set of constant git identity, never mutated after
                std::env::set_var("GIT_AUTHOR_EMAIL", "test@anodize.local");
                // env-ok: idempotent OnceLock set of constant git identity, never mutated after
                std::env::set_var("GIT_COMMITTER_NAME", "Anodize Test");
                // env-ok: idempotent OnceLock set of constant git identity, never mutated after
                std::env::set_var("GIT_COMMITTER_EMAIL", "test@anodize.local");
            }
        });
    }

    /// Build a bare remote + a working clone with one commit on `master`.
    /// Returns `(bare_remote_url, _tmp_holder_for_lifetime)`.
    fn init_bare_remote_with_one_commit() -> (String, tempfile::TempDir, tempfile::TempDir) {
        ensure_git_identity();
        let bare = tempfile::tempdir().expect("bare tempdir");
        let work = tempfile::tempdir().expect("work tempdir");

        // Init the bare remote.
        let ok = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(["init", "--bare", "-b", "master"])
                    .arg(bare.path());
                cmd
            },
            "git",
        )
        .status
        .success();
        assert!(ok, "git init --bare failed");

        // Init the working clone, commit a file, push to the bare remote.
        for args in [
            vec!["init", "-b", "master"],
            vec!["config", "user.email", "test@example.invalid"],
            vec!["config", "user.name", "Test"],
            vec!["config", "commit.gpgsign", "false"],
        ] {
            assert!(
                anodizer_core::test_helpers::output_with_spawn_retry(
                    || {
                        let mut cmd = Command::new("git");
                        cmd.args(&args).current_dir(work.path());
                        cmd
                    },
                    "git",
                )
                .status
                .success(),
                "git {:?} failed",
                args
            );
        }
        std::fs::write(work.path().join("README"), "hello\n").unwrap();
        for args in [
            vec!["add", "README"],
            vec!["commit", "-m", "initial commit"],
        ] {
            assert!(
                anodizer_core::test_helpers::output_with_spawn_retry(
                    || {
                        let mut cmd = Command::new("git");
                        cmd.args(&args).current_dir(work.path());
                        cmd
                    },
                    "git",
                )
                .status
                .success(),
                "git {:?} failed",
                args
            );
        }
        // `git remote add origin <path>` takes a filesystem path argument
        // which OsStr-based Command::arg handles directly; keep it out of
        // the &str-loop above.
        assert!(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["remote", "add", "origin"])
                        .arg(bare.path())
                        .current_dir(work.path());
                    cmd
                },
                "git",
            )
            .status
            .success(),
            "git remote add origin failed"
        );
        assert!(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["push", "-u", "origin", "master"])
                        .current_dir(work.path());
                    cmd
                },
                "git",
            )
            .status
            .success()
        );

        let url = bare.path().to_string_lossy().into_owned();
        (url, bare, work)
    }

    #[test]
    fn git_revert_and_push_creates_revert_commit_on_clean_repo() {
        let (url, _bare, _work) = init_bare_remote_with_one_commit();
        let log = StageLogger::new("test", Verbosity::Normal);
        let target = RevertTarget {
            target: "demo".into(),
            repo_url: url.clone(),
            branch: Some("master".into()),
            token: None,
            private_key: None,
            ssh_command: None,
        };
        // The helper re-clones, reverts HEAD, pushes back to the bare
        // remote. We then verify a fresh clone has HEAD as a revert
        // commit (subject starts with `Revert`).
        run_git_revert_and_push(&target, &log).expect("revert+push ok");

        let verify_dir = tempfile::tempdir().expect("verify tempdir");
        let ok = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(["clone", "--depth=2", &url])
                    .arg(verify_dir.path().join("repo"));
                cmd
            },
            "git",
        )
        .status
        .success();
        assert!(ok, "git clone for verification failed");
        let log_out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(["log", "-1", "--pretty=%s"])
                    .current_dir(verify_dir.path().join("repo"));
                cmd
            },
            "git",
        );
        let subject = String::from_utf8_lossy(&log_out.stdout).trim().to_string();
        assert!(
            subject.starts_with("Revert"),
            "expected HEAD to be a revert commit, got subject={subject:?}"
        );
    }

    #[test]
    fn git_revert_and_push_fails_loudly_on_dirty_tree() {
        // Stand up a real repo, add a stray unstaged file, then call
        // `revert_head_in` directly: it must refuse and bail.
        let (_url, _bare, work) = init_bare_remote_with_one_commit();
        std::fs::write(work.path().join("stray"), "dirty\n").unwrap();
        let err =
            revert_head_in(work.path()).expect_err("dirty tree should fail before revert runs");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("dirty working tree"),
            "expected dirty-tree error, got: {msg}"
        );
    }
}
