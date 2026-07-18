use super::*;
use anyhow::{Context as _, Result, bail};
use std::path::Path;
use std::process::Command;

/// Committer identity (author + committer name/email) for the rare path
/// where a git invocation lands on a host with no `user.email` /
/// `user.name` configured — notably `actions/checkout@v6`, which does
/// NOT set committer identity for the workflow runner. Resolved once per
/// caller and threaded through to [`revert_commit_in`] so the CLI never
/// mutates the repo's git config (env-only, scoped to the single spawn).
///
/// Convention: when both `name` and `email` are populated, the values
/// are exported as `GIT_AUTHOR_NAME` / `GIT_AUTHOR_EMAIL` AND
/// `GIT_COMMITTER_NAME` / `GIT_COMMITTER_EMAIL` on the git child
/// processes (revert + amend). When `None`, the child inherits whatever
/// the parent / repo config provides.
#[derive(Debug, Clone, Default)]
pub struct CommitterIdentity {
    pub name: Option<String>,
    pub email: Option<String>,
}

impl CommitterIdentity {
    /// Return a default committer identity to use when `user.email` and
    /// `user.name` are both unset on the host. Email uses the
    /// short-hostname (best-effort; falls back to `"localhost"`) so a
    /// reviewer can tell at a glance which machine emitted the
    /// rollback commit.
    pub fn default_for_rollback() -> Self {
        let host = std::env::var("HOSTNAME")
            .ok()
            .or_else(|| std::env::var("COMPUTERNAME").ok())
            .and_then(|h| h.split('.').next().map(str::to_string))
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        Self {
            name: Some("anodize-rollback".to_string()),
            email: Some(format!("anodize-rollback@{host}")),
        }
    }

    fn apply_to(&self, cmd: &mut Command) {
        if let Some(n) = &self.name {
            cmd.env("GIT_AUTHOR_NAME", n).env("GIT_COMMITTER_NAME", n);
        }
        if let Some(e) = &self.email {
            cmd.env("GIT_AUTHOR_EMAIL", e).env("GIT_COMMITTER_EMAIL", e);
        }
    }
}

/// Read `git config user.email` / `user.name` in `cwd`. Returns
/// `(name, email)`, each `Some(value)` when configured (and non-empty)
/// or `None` when unset. Used by [`revert_commit_in`] to detect the
/// CI-checkout case where neither identity is configured and the
/// committer env fallback must fire.
pub(super) fn read_git_identity(cwd: &Path) -> (Option<String>, Option<String>) {
    let one = |key: &str| -> Option<String> {
        let out = Command::new("git")
            .current_dir(cwd)
            .args(["config", "--get", key])
            .env("LC_ALL", "C")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if value.is_empty() { None } else { Some(value) }
    };
    (one("user.name"), one("user.email"))
}

/// Resolve the committer identity to use for a rollback-style commit.
/// When the host already has `user.name` AND `user.email` configured
/// (or `GIT_AUTHOR_*` / `GIT_COMMITTER_*` are set in the parent env),
/// returns an empty identity so the child inherits the existing
/// values. Otherwise returns a synthetic identity so the commit
/// doesn't fail with "Author identity unknown" on bare-CI hosts.
pub fn resolve_rollback_identity(cwd: &Path) -> CommitterIdentity {
    let env_author_set =
        std::env::var("GIT_AUTHOR_EMAIL").is_ok() && std::env::var("GIT_AUTHOR_NAME").is_ok();
    let env_committer_set =
        std::env::var("GIT_COMMITTER_EMAIL").is_ok() && std::env::var("GIT_COMMITTER_NAME").is_ok();
    if env_author_set && env_committer_set {
        return CommitterIdentity::default();
    }
    let (name, email) = read_git_identity(cwd);
    if name.is_some() && email.is_some() {
        return CommitterIdentity::default();
    }
    CommitterIdentity::default_for_rollback()
}

/// Run `git revert --no-edit <sha>` in `cwd`, optionally followed by
/// `git commit --amend -m <message>`.
///
/// Refuses against a dirty working tree (`git revert` would surface a
/// less actionable "your local changes would be overwritten" message
/// otherwise). Mirrors the dirty-tree guard used by
/// `stage-publish/src/util/git_revert.rs`. The guard counts only
/// TRACKED modifications (`--untracked-files=no`): a revert never
/// touches untracked files, and a failure-recovery rollback runs right
/// after a release wrote `dist/` — in repos that don't gitignore their
/// dist, an untracked-counts-as-dirty guard would refuse every
/// post-release rollback. The one genuine hazard (an untracked file
/// where the revert must restore a tracked one) is refused by git
/// itself with an explicit "would be overwritten" error.
///
/// On revert failure (typically a merge conflict against later commits
/// on top of the bump), runs `git revert --abort` to restore the
/// working tree before bubbling the error — otherwise the next
/// rollback attempt would trip the dirty-tree guard and the operator
/// would be stuck.
///
/// `identity` is threaded through as committer env vars so the call
/// works on bare-CI hosts where the workflow checkout doesn't set
/// `user.email` / `user.name`. The env is scoped to the spawn; the
/// repo's git config is never mutated.
pub fn revert_commit_in(
    cwd: &Path,
    sha: &str,
    message: Option<&str>,
    identity: &CommitterIdentity,
) -> Result<()> {
    let status = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .current_dir(cwd)
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .with_context(|| format!("revert_commit_in: git status in {}", cwd.display()))?;
    if !status.status.success() {
        let stderr_raw = String::from_utf8_lossy(&status.stderr);
        let raw = format!("git status failed: {}", stderr_raw.trim());
        bail!("{}", crate::redact::redact_process_env(&raw));
    }
    if !status.stdout.is_empty() {
        bail!(
            "refusing to revert in a dirty working tree at {}\nstatus:\n{}",
            cwd.display(),
            String::from_utf8_lossy(&status.stdout),
        );
    }

    let mut revert_cmd = Command::new("git");
    revert_cmd
        .current_dir(cwd)
        .args(["revert", "--no-edit", sha])
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0");
    identity.apply_to(&mut revert_cmd);
    let out = revert_cmd
        .output()
        .with_context(|| format!("revert_commit_in: git revert in {}", cwd.display()))?;
    if !out.status.success() {
        let stderr_raw = String::from_utf8_lossy(&out.stderr);
        // Restore the working tree before bubbling — otherwise the dirty-tree
        // guard above traps a subsequent rollback retry forever.
        let _ = Command::new("git")
            .current_dir(cwd)
            .args(["revert", "--abort"])
            .env("LC_ALL", "C")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output();
        let raw = format!(
            "git revert {sha} hit conflicts and was aborted (working tree restored). \
             The bump commit overlaps with later changes — resolve manually, \
             or re-run with --mode=reset to force.\nstderr: {}",
            stderr_raw.trim()
        );
        bail!("{}", crate::redact::redact_process_env(&raw));
    }
    if let Some(msg) = message {
        let mut amend_cmd = Command::new("git");
        amend_cmd
            .current_dir(cwd)
            .args(["commit", "--amend", "-m", msg])
            .env("LC_ALL", "C")
            .env("GIT_TERMINAL_PROMPT", "0");
        identity.apply_to(&mut amend_cmd);
        let out = amend_cmd.output().with_context(|| {
            format!("revert_commit_in: git commit --amend in {}", cwd.display())
        })?;
        if !out.status.success() {
            let stderr_raw = String::from_utf8_lossy(&out.stderr);
            let raw = format!("git commit --amend failed: {}", stderr_raw.trim());
            bail!("{}", crate::redact::redact_process_env(&raw));
        }
    }
    Ok(())
}

/// Run `git reset --hard <sha>` in `cwd`. **Destructive** — rewrites HEAD
/// and the index in place; callers must surface a warning before invoking.
pub fn reset_hard_in(cwd: &Path, sha: &str) -> Result<()> {
    git_output_in(cwd, &["reset", "--hard", sha])?;
    Ok(())
}

/// Push a branch (`HEAD:refs/heads/<branch>`) to the `origin` remote.
///
/// Errors when no `origin` remote is configured — callers driving local-only
/// flows should pass `--no-push` to skip the call entirely.
pub fn push_branch_in(cwd: &Path, branch: &str) -> Result<()> {
    if !super::has_remote_in(cwd, "origin") {
        bail!("no 'origin' remote configured, cannot push branch '{branch}'");
    }
    let refspec = format!("HEAD:refs/heads/{}", branch);
    let out = Command::new("git")
        .current_dir(cwd)
        .args(["push", "origin", &refspec])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .with_context(|| format!("push_branch_in: git push origin {refspec}"))?;
    if !out.status.success() {
        let stderr_raw = String::from_utf8_lossy(&out.stderr);
        let raw = format!("git push origin {} failed: {}", refspec, stderr_raw.trim());
        bail!("{}", crate::redact::redact_process_env(&raw));
    }
    Ok(())
}

/// `git -C <repo> log -1 --format=%ct HEAD` — return HEAD's committer
/// timestamp (seconds since UNIX epoch) for the given repository. Used by
/// the determinism harness as the non-snapshot SDE seed.
pub fn head_commit_timestamp_in(repo: &std::path::Path) -> Result<i64> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["log", "-1", "--format=%ct", "HEAD"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
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
