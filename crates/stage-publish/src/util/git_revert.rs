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
use std::time::Duration;

use super::clone::{clone_repo_ssh, clone_repo_with_auth};
use anodizer_core::log::StageLogger;
use anodizer_core::run::run_capture_timeout;

/// Wall-clock bound on the rollback `git push` of a revert commit. The push hits
/// the publisher remote, so a wedged connection must not hang the rollback
/// forever; on expiry the subtree is killed and the failure surfaces. Sized as a
/// remote push.
const GIT_REVERT_PUSH_TIMEOUT: Duration = Duration::from_secs(600);

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
    push_after_revert(repo_path, target.branch.as_deref(), log)?;
    Ok(())
}

/// Run `git revert HEAD --no-edit` inside `path`. Captures stderr on
/// failure so a merge-conflict / empty-repo failure mode is visible.
///
/// `--no-edit` keeps the revert non-interactive (no $EDITOR invocation
/// in CI / non-tty environments). The revert commit carries an explicit
/// identity via `GIT_AUTHOR_*`/`GIT_COMMITTER_*` env vars (same mechanism
/// and same resolved name/email as the forward publish commit, via
/// [`super::commit::resolved_commit_identity`]) because self-hosted
/// runners without a global `~/.gitconfig` have no ambient identity for
/// `git revert` to fall back on, and would otherwise fail every rollback
/// with "Author identity unknown" (exit 128).
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

    let (author_name, author_email) = super::commit::resolved_commit_identity();
    let output = Command::new("git")
        .args(["revert", "HEAD", "--no-edit"])
        .current_dir(path)
        .env("GIT_AUTHOR_NAME", &author_name)
        .env("GIT_AUTHOR_EMAIL", &author_email)
        .env("GIT_COMMITTER_NAME", &author_name)
        .env("GIT_COMMITTER_EMAIL", &author_email)
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
fn push_after_revert(path: &Path, branch: Option<&str>, log: &StageLogger) -> Result<()> {
    let args: Vec<&str> = match branch {
        Some(b) => vec!["push", "origin", b],
        None => vec!["push", "origin", "HEAD"],
    };
    let mut cmd = Command::new("git");
    cmd.args(&args)
        .current_dir(path)
        .env("GIT_TERMINAL_PROMPT", "0");
    // Bounded: the rollback push hits the remote, so a stalled connection must
    // not hang the rollback. A deadline kill surfaces as a Retriable error.
    let output = run_capture_timeout(
        &mut cmd,
        log,
        "git_revert: git push",
        GIT_REVERT_PUSH_TIMEOUT,
    )
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
    use serial_test::serial;
    use std::process::Command;

    /// Build a bare remote + a working clone with one commit on `master`.
    /// Returns `(bare_remote_url, _tmp_holder_for_lifetime)`.
    fn init_bare_remote_with_one_commit() -> (String, tempfile::TempDir, tempfile::TempDir) {
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
            anodizer_core::test_helpers::git_test_ok(work.path(), &args);
        }
        std::fs::write(work.path().join("README"), "hello\n").unwrap();
        for args in [
            vec!["add", "README"],
            vec!["commit", "-m", "initial commit"],
        ] {
            anodizer_core::test_helpers::git_test_ok(work.path(), &args);
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

    /// Sets or removes a process env var for the guard's lifetime, restoring
    /// the prior value (or absence) on drop. Every caller carries
    /// `#[serial(git_env)]`, shared with `commit.rs`'s identical group, so no
    /// other git-identity test reads or writes the environment concurrently.
    enum EnvGuard {
        Set {
            key: &'static str,
            prev: Option<String>,
        },
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var(key).ok();
            // SAFETY: serialized via `#[serial(git_env)]`; restored on drop.
            // env-ok: EnvGuard set/restore; every caller test is #[serial(git_env)]
            unsafe { std::env::set_var(key, value) };
            Self::Set { key, prev }
        }

        /// Removes `key` from the process env for the guard's lifetime, so
        /// no ambient identity can mask the "no ambient identity" bug this
        /// proves fixed.
        fn remove(key: &'static str) -> Self {
            let prev = std::env::var(key).ok();
            // SAFETY: serialized via `#[serial(git_env)]`; restored on drop.
            // env-ok: EnvGuard set/restore; every caller test is #[serial(git_env)]
            unsafe { std::env::remove_var(key) };
            Self::Set { key, prev }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            let Self::Set { key, prev } = self;
            // SAFETY: see `EnvGuard::set`/`remove` — serialized, restored
            // immediately on drop.
            unsafe {
                match prev {
                    // env-ok: EnvGuard set/restore; every caller test is #[serial(git_env)]
                    Some(v) => std::env::set_var(key, v),
                    // env-ok: EnvGuard set/restore; every caller test is #[serial(git_env)]
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    /// Regression for the runner failure mode: on a host with no ambient git
    /// identity anywhere in the resolution chain (`GIT_AUTHOR_*`/
    /// `GIT_COMMITTER_*` env unset, no global config, no system config, and a
    /// fresh clone has no local `user.*` either), `git revert` used to fail
    /// with "Author identity unknown" because `revert_head_in` set no
    /// identity on the child. It now applies the same resolved identity the
    /// forward publish commit uses. This test does NOT use
    /// `init_bare_remote_with_one_commit()` — its fixture commits would
    /// otherwise be indistinguishable from the identity resolution under
    /// test; the guards below strip every ambient `GIT_*` identity source
    /// so only `revert_head_in`'s own resolution can supply one.
    #[test]
    #[serial(git_env)]
    fn revert_head_in_succeeds_with_no_ambient_identity() {
        let bare = tempfile::tempdir().expect("bare tempdir");
        let work = tempfile::tempdir().expect("work tempdir");

        // Neutralize the ambient git config for the WHOLE test, before the
        // first git command runs, so the clone's checkout obeys it too: on a
        // host whose system gitconfig enables `core.autocrlf` (git-for-Windows)
        // a clone under the real config would check `README` out as CRLF, and
        // the later autocrlf-off neutralization would make `revert_head_in`'s
        // dirty-tree guard compare that CRLF tree against the LF blob and
        // fabricate a phantom `M README`. With one config in effect throughout
        // (autocrlf off), the checkout stays LF, the tree is clean, and the
        // revert proceeds. This also strips ambient GIT_AUTHOR_*/GIT_COMMITTER_*
        // and points global/system config at nothing so no host
        // ~/.gitconfig / /etc/gitconfig identity can leak in and mask the bug.
        let _author_name = EnvGuard::remove("GIT_AUTHOR_NAME");
        let _author_email = EnvGuard::remove("GIT_AUTHOR_EMAIL");
        let _committer_name = EnvGuard::remove("GIT_COMMITTER_NAME");
        let _committer_email = EnvGuard::remove("GIT_COMMITTER_EMAIL");
        let _config_global = EnvGuard::set("GIT_CONFIG_GLOBAL", "/dev/null");
        let _config_nosystem = EnvGuard::set("GIT_CONFIG_NOSYSTEM", "1");

        assert!(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["init", "--bare", "-b", "master"])
                        .arg(bare.path());
                    cmd
                },
                "git",
            )
            .status
            .success()
        );
        for args in [
            vec!["init", "-b", "master"],
            vec!["config", "user.email", "seed@example.invalid"],
            vec!["config", "user.name", "Seed"],
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
                "git {args:?} failed"
            );
        }
        std::fs::write(work.path().join("README"), "hello\n").unwrap();
        assert!(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["add", "README"]).current_dir(work.path());
                    cmd
                },
                "git",
            )
            .status
            .success()
        );
        assert!(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["commit", "-m", "initial commit"])
                        .current_dir(work.path());
                    cmd
                },
                "git",
            )
            .status
            .success()
        );
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
            .success()
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

        // A fresh clone carries no local `user.*` config of its own.
        let clone_dir = tempfile::tempdir().expect("clone tempdir");
        let repo_path = clone_dir.path().join("repo");
        assert!(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["clone", &bare.path().to_string_lossy()])
                        .arg(&repo_path);
                    cmd
                },
                "git",
            )
            .status
            .success()
        );

        revert_head_in(&repo_path).expect(
            "revert must succeed even with no ambient GIT_AUTHOR_*/GIT_COMMITTER_* env, \
             no global config, and no system config -- the fix must supply an explicit \
             identity on the git-revert child, or this fails with 'Author identity unknown'",
        );

        let log_out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(["log", "-1", "--pretty=%s"])
                    .current_dir(&repo_path);
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
}
