//! Commit-author resolution and the staged-add+commit+push helper used
//! by every publisher that touches a remote git repo (homebrew, scoop,
//! winget, krew, aur, aur_source, nix, chocolatey).

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::Result;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use super::cmd::{run_cmd_in, run_cmd_in_envs, run_cmd_in_timeout};

/// Wall-clock bound on `git push` to a publisher's remote repo (a tap, AUR,
/// winget-pkgs, krew-index, ...). A wedged push to the remote would otherwise
/// hang the release forever, so the subtree is killed at the deadline. Sized
/// as a large remote upload, matching the snapcraft/docker-push bound.
const GIT_PUSH_TIMEOUT: Duration = Duration::from_secs(600);

/// Wall-clock bound on the `git fetch` of an existing remote branch. The fetch
/// only pulls a single shallow ref, so a shorter remote-metadata bound suffices;
/// its failure is already ignored (the branch may simply not exist remotely), so
/// a deadline kill degrades to the same "no remote branch" path.
const GIT_FETCH_TIMEOUT: Duration = Duration::from_secs(300);

/// Outcome of a `commit_and_push_with_opts` call.
///
/// Callers must distinguish these to avoid recording evidence or logging
/// "pushed" after a genuine no-op.  When the remote already matches the
/// staged tree (idempotent retry) or the staged index has no delta vs HEAD
/// (writer produced identical content), no git objects were created and
/// nothing was pushed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommitOutcome {
    /// A commit was created and pushed to the remote.
    Pushed,
    /// The remote or local state was already up to date; nothing was committed or pushed.
    NoChanges,
}

impl CommitOutcome {
    pub(crate) fn is_pushed(self) -> bool {
        matches!(self, Self::Pushed)
    }
}

/// Optional overrides for the git commit step.
#[derive(Debug, Default)]
pub(crate) struct CommitOptions {
    /// Git commit author name, applied as `GIT_AUTHOR_NAME` /
    /// `GIT_COMMITTER_NAME` on the commit child so it overrides both an
    /// ambient `GIT_AUTHOR_NAME` env var and the repo's `user.name` config
    /// (git precedence: `GIT_AUTHOR_*` env > `user.*` config). Owned because
    /// `resolve_commit_opts` may shell out to `git config user.name`, whose
    /// result is a fresh String.
    pub author_name: Option<String>,
    /// Git commit author email, applied as `GIT_AUTHOR_EMAIL` /
    /// `GIT_COMMITTER_EMAIL` on the commit child (same override rationale as
    /// `author_name`).
    pub author_email: Option<String>,
    /// Enable GPG/SSH signing for the commit. Owned (not a config reference)
    /// so that `key`, `program`, and `format` can be template-rendered before
    /// being passed to `git -c` args.
    pub signing: Option<anodizer_core::config::CommitSigningConfig>,
    /// When true, suppress the `GIT_AUTHOR_*` / `GIT_COMMITTER_*` identity
    /// overrides so the running git client uses the GitHub App's identity
    /// (already configured in the repo's local git config by the Actions
    /// checkout step).
    pub use_github_app_token: bool,
}

/// Default commit author name used when no author is configured.
const DEFAULT_COMMIT_AUTHOR_NAME: &str = "anodizer";

/// Default commit author email used when no author is configured.
const DEFAULT_COMMIT_AUTHOR_EMAIL: &str = "bot@anodizer.dev";

/// Resolve the default commit identity used whenever no config-supplied
/// author is in scope: the local `git config user.{name,email}` (read from
/// the process's current directory — the source repo being released, not
/// the publisher-owned repo a commit lands in), falling back to the
/// built-in defaults above when unset.
///
/// This is the single source of truth for that fallback chain, shared by
/// [`resolve_commit_opts`] (when a publisher's `commit_author` config
/// supplies no name/email) and the rollback git-revert path
/// (`super::git_revert`), so a revert commit is authored identically to the
/// forward publish commit it undoes rather than duplicating the default
/// literals a second time and risking drift.
pub(crate) fn resolved_commit_identity() -> (String, String) {
    let name = anodizer_core::git::local_git_user_name()
        .unwrap_or_else(|| DEFAULT_COMMIT_AUTHOR_NAME.to_string());
    let email = anodizer_core::git::local_git_user_email()
        .unwrap_or_else(|| DEFAULT_COMMIT_AUTHOR_EMAIL.to_string());
    (name, email)
}

/// Resolve commit author name/email from a CommitAuthorConfig, falling back
/// to the local `git config user.{name, email}`, then to built-in defaults.
///
/// The `git config` step exists so that publisher PRs (Homebrew tap, AUR,
/// krew-index, winget-pkgs, ...) carry the release engineer's identity
/// instead of `anodizer <bot@anodizer.dev>`. The bot identity does not match
/// any GitHub-registered email, so it cannot pass CNCF EasyCLA on
/// kubernetes-sigs/krew-index PRs and similar workflows.
///
/// Returns CommitOptions whose `author_name` / `author_email` are owned strings
/// so that values read from git config (which need allocation) can be returned
/// alongside borrowed config values.
///
/// The commit-author resolution template-renders the config-supplied `name`
/// and `email` (each via [`render_or_warn`](super::template::render_or_warn))
/// before the local-git / built-in fallbacks apply when a value is unset.
/// Render-error handling follows the strict-aware policy: under the
/// pre-publish guard (or the user's global `--strict`) a malformed
/// name/email template returns `Err`; otherwise it warns and falls back to
/// the raw literal so a forgiving dry-run / snapshot keeps building.
///
/// `use_github_app_token` is propagated from the config struct onto the
/// resulting `CommitOptions`. When true, downstream
/// `commit_and_push_with_opts` skips the `GIT_AUTHOR_*` / `GIT_COMMITTER_*`
/// identity overrides so the local git config (configured by the Actions
/// checkout step) wins.
pub(crate) fn resolve_commit_opts(
    ctx: &Context,
    commit_author: Option<&anodizer_core::config::CommitAuthorConfig>,
    log: &StageLogger,
) -> Result<CommitOptions> {
    let (cfg_name, cfg_email, signing_raw, use_github_app_token) = if let Some(ca) = commit_author {
        (
            ca.name.as_deref(),
            ca.email.as_deref(),
            ca.signing.as_ref(),
            ca.use_github_app_token,
        )
    } else {
        (None, None, None, false)
    };

    let (default_name, default_email) = resolved_commit_identity();
    let name = match cfg_name {
        Some(raw) => super::template::render_or_warn(ctx, log, "commit_author.name", raw)?,
        None => default_name,
    };
    let email = match cfg_email {
        Some(raw) => super::template::render_or_warn(ctx, log, "commit_author.email", raw)?,
        None => default_email,
    };

    // Render template variables in signing config fields so that e.g.
    // `key: "{{ .Env.GPG_KEY_ID }}"` resolves before being passed to
    // `git -c user.signingkey=...`.
    let signing = signing_raw
        .map(|s| {
            let key = s
                .key
                .as_deref()
                .map(|v| super::template::render_or_warn(ctx, log, "commit_author.signing.key", v))
                .transpose()?;
            let program = s
                .program
                .as_deref()
                .map(|v| {
                    super::template::render_or_warn(ctx, log, "commit_author.signing.program", v)
                })
                .transpose()?;
            let format = s
                .format
                .as_deref()
                .map(|v| {
                    super::template::render_or_warn(ctx, log, "commit_author.signing.format", v)
                })
                .transpose()?;
            Ok::<_, anyhow::Error>(anodizer_core::config::CommitSigningConfig {
                enabled: s.enabled,
                key,
                program,
                format,
            })
        })
        .transpose()?;

    Ok(CommitOptions {
        author_name: Some(name),
        author_email: Some(email),
        signing,
        use_github_app_token,
    })
}

/// Stage files, commit, and push with optional commit author overrides.
///
/// Semantics for publisher branches (winget, krew, etc.): these are typically
/// versioned, one-shot, disposable — e.g. `TJSmith.cfgd-0.3.5`. A previous
/// failed run may have left an orphan commit on `origin/<branch>` with stale
/// or incomplete content. The correct behavior on retry is to replace that
/// orphan wholesale with the current attempt's output, not to rebase on top
/// of it.
///
/// Algorithm:
///
/// 1. Fetch `origin/<branch>` (if it exists remotely). `clone_repo_with_auth`
///    uses `--depth=1` and only populates the default branch, so any
///    per-version branch from a prior run is absent from the local ref
///    store without this explicit fetch.
/// 2. Create (or reset) the local branch at the current HEAD — i.e. the
///    default branch tip. The caller has already written files to the
///    working tree; we deliberately do not `checkout -B branch origin/branch`
///    because that would either overwrite the caller's writes with stale
///    orphan content or fail on untracked-file conflicts.
/// 3. Stage the caller's files.
/// 4. If the remote branch exists and its tree hash equals our staged tree
///    hash, the remote already matches desired state — return success
///    without committing or pushing.
/// 5. Commit on top of the default branch base.
/// 6. Push:
///    - When no remote counterpart exists: plain `git push -u origin <branch>`.
///    - When a remote orphan exists: `git push --force-with-lease=<branch>:<sha>
///      --force-if-includes origin <branch>` using the explicit SHA we just
///      fetched. The lease guarantees we only overwrite the orphan we saw;
///      any racing push between fetch and push invalidates the lease.
pub(crate) fn commit_and_push_with_opts(
    repo_path: &Path,
    files: &[&str],
    message: &str,
    branch: Option<&str>,
    label: &str,
    opts: &CommitOptions,
    log: &StageLogger,
) -> Result<CommitOutcome> {
    // Pre-fetch the target branch (if any) so `origin/<branch>` is populated
    // in the local ref store. We must use an explicit refspec: `clone
    // --depth=1` implies `--single-branch`, which restricts the remote's
    // default fetch refspec to just the cloned branch. Without the explicit
    // `+refs/heads/<b>:refs/remotes/origin/<b>` mapping, a plain
    // `git fetch origin <b>` fetches the commit into FETCH_HEAD but never
    // updates the remote-tracking ref, leaving us unable to detect an
    // existing remote branch. Ignore failure: the branch genuinely may not
    // exist remotely.
    let remote_sha: Option<String> = if let Some(branch_name) = branch {
        let refspec = format!("+refs/heads/{0}:refs/remotes/origin/{0}", branch_name);
        // Bounded: the fetch hits the remote, so a stalled connection must not
        // hang here with no deadline. Failure (incl. a deadline kill) is ignored
        // — the branch genuinely may not exist remotely.
        let _ = run_cmd_in_timeout(
            repo_path,
            "git",
            &["fetch", "--depth=1", "origin", &refspec],
            &format!("{label}: git fetch origin {branch_name}"),
            None,
            log,
            GIT_FETCH_TIMEOUT,
        );
        Command::new("git")
            .args([
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/remotes/origin/{}", branch_name),
            ])
            .current_dir(repo_path)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    } else {
        None
    };

    // Create/reset local branch at current HEAD (default branch tip).
    if let Some(branch_name) = branch {
        run_cmd_in(
            repo_path,
            "git",
            &["checkout", "-B", branch_name],
            &format!("{label}: git checkout -B {}", branch_name),
        )?;
    }

    for file in files {
        run_cmd_in(
            repo_path,
            "git",
            &["add", file],
            &format!("{label}: git add"),
        )?;
    }

    // Idempotent no-op: if the remote branch's tree matches what we just
    // staged, there's nothing to push.
    if let Some(ref sha) = remote_sha {
        let remote_tree = Command::new("git")
            .args(["rev-parse", &format!("{}^{{tree}}", sha)])
            .current_dir(repo_path)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
        let staged_tree = Command::new("git")
            .args(["write-tree"])
            .current_dir(repo_path)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
        if let (Some(r), Some(s)) = (remote_tree, staged_tree)
            && r == s
        {
            return Ok(CommitOutcome::NoChanges);
        }
    }

    // No diff vs. current HEAD means the caller's writes were redundant —
    // nothing to commit.
    let diff_output = Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .current_dir(repo_path)
        .status();
    if let Ok(status) = diff_output
        && status.success()
    {
        return Ok(CommitOutcome::NoChanges);
    }

    // Build commit args + identity env, optionally injecting the configured
    // author/committer and signing config.
    //
    // The configured author is applied via `GIT_AUTHOR_NAME` /
    // `GIT_AUTHOR_EMAIL` (and the matching committer pair) set on the git
    // child — NOT via `-c user.name=` / `-c user.email=`. Git's identity
    // precedence is `--author` > `GIT_AUTHOR_*` env > `user.*` config, so a
    // `-c user.name=` override is silently defeated whenever an ambient
    // `GIT_AUTHOR_NAME` is already exported in the environment (CI runners,
    // wrapper scripts). Setting the identity env vars makes the config
    // authoritative over both that ambient env and the repo config — the
    // whole point of `commit_author`, which exists so publisher PRs carry the
    // release engineer's CLA-registered identity rather than a bot's. Mirrors
    // `core::git::CommitterIdentity::apply_to`, which uses the same mechanism
    // for rollback commits.
    //
    // When `use_github_app_token` is set, the identity env is NOT applied so
    // the local git config (already populated by the GitHub Actions checkout
    // step with the App's `<app-slug>[bot]` identity) stays authoritative.
    let mut commit_envs: Vec<(&str, &str)> = Vec::new();
    if !opts.use_github_app_token {
        if let Some(ref name) = opts.author_name {
            commit_envs.push(("GIT_AUTHOR_NAME", name));
            commit_envs.push(("GIT_COMMITTER_NAME", name));
        }
        if let Some(ref email) = opts.author_email {
            commit_envs.push(("GIT_AUTHOR_EMAIL", email));
            commit_envs.push(("GIT_COMMITTER_EMAIL", email));
        }
    }

    let mut commit_args: Vec<&str> = Vec::new();
    let sign_cfg;
    let sign_key_cfg;
    let sign_program_cfg;
    let sign_format_cfg;
    let do_sign = opts
        .signing
        .as_ref()
        .and_then(|s| s.enabled)
        .unwrap_or(false);
    if do_sign {
        sign_cfg = "commit.gpgsign=true".to_string();
        commit_args.extend_from_slice(&["-c", &sign_cfg]);
        if let Some(key) = opts.signing.as_ref().and_then(|s| s.key.as_deref()) {
            sign_key_cfg = format!("user.signingkey={}", key);
            commit_args.extend_from_slice(&["-c", &sign_key_cfg]);
        }
        if let Some(program) = opts.signing.as_ref().and_then(|s| s.program.as_deref()) {
            sign_program_cfg = format!("gpg.program={}", program);
            commit_args.extend_from_slice(&["-c", &sign_program_cfg]);
        }
        // signing.format defaults to "openpgp" when signing is enabled but
        // format is unset — otherwise users inherit the system's `gpg.format`
        // (ssh/x509) which isn't what they asked for.
        let fmt = opts
            .signing
            .as_ref()
            .and_then(|s| s.format.as_deref())
            .unwrap_or("openpgp");
        sign_format_cfg = format!("gpg.format={}", fmt);
        commit_args.extend_from_slice(&["-c", &sign_format_cfg]);
    }
    commit_args.extend_from_slice(&["commit", "-m", message]);

    run_cmd_in_envs(
        repo_path,
        "git",
        &commit_args,
        &format!("{label}: git commit"),
        &commit_envs,
    )?;

    // Push strategy:
    // - No branch specified: plain push against the current HEAD's upstream.
    // - Branch, no remote counterpart: plain `push -u` to create it.
    // - Branch with a remote counterpart (orphan from prior failed run, or
    //   an unrelated stale tip): `--force-with-lease=<branch>:<sha>` using
    //   the SHA captured from our pre-fetch. The explicit lease is
    //   race-safe: if anything pushed to the branch between our fetch and
    //   our push, the lease invalidates and we bail without overwriting.
    //
    // Note: `--force-if-includes` would block orphan replacement. The lease
    // alone is the correct protection here because our local commit is
    // based on the default branch tip, not on the orphan.
    let lease_arg;
    let push_args: Vec<&str> = match (branch, remote_sha.as_deref()) {
        (Some(branch_name), Some(sha)) => {
            lease_arg = format!("--force-with-lease={}:{}", branch_name, sha);
            vec!["push", "-u", &lease_arg, "origin", branch_name]
        }
        (Some(branch_name), None) => vec!["push", "-u", "origin", branch_name],
        (None, _) => vec!["push"],
    };
    run_cmd_in_timeout(
        repo_path,
        "git",
        &push_args,
        &format!("{label}: git push"),
        None,
        log,
        GIT_PUSH_TIMEOUT,
    )?;
    Ok(CommitOutcome::Pushed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::log::Verbosity;
    use serial_test::serial;
    use std::process::Command as Cmd;

    #[test]
    fn is_pushed_reflects_variant() {
        assert!(CommitOutcome::Pushed.is_pushed());
        assert!(!CommitOutcome::NoChanges.is_pushed());
    }

    fn init_bare_remote(dir: &std::path::Path) {
        Cmd::new("git")
            .args(["init", "--bare"])
            .current_dir(dir)
            .status()
            .unwrap();
    }

    fn init_local_with_remote(local: &std::path::Path, remote: &std::path::Path) {
        Cmd::new("git")
            .args(["init", "-b", "main"])
            .current_dir(local)
            .status()
            .unwrap();
        Cmd::new("git")
            .args([
                "-c",
                "user.email=test@test.com",
                "-c",
                "user.name=Test",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ])
            .current_dir(local)
            .status()
            .unwrap();
        Cmd::new("git")
            .args(["remote", "add", "origin", &remote.to_string_lossy()])
            .current_dir(local)
            .status()
            .unwrap();
        Cmd::new("git")
            .args(["push", "-u", "origin", "main"])
            .current_dir(local)
            .status()
            .unwrap();
    }

    #[test]
    fn returns_pushed_when_staged_change_is_committed_and_pushed() {
        let tmp = tempfile::tempdir().unwrap();
        let remote_dir = tmp.path().join("remote");
        let local_dir = tmp.path().join("local");
        std::fs::create_dir_all(&remote_dir).unwrap();
        std::fs::create_dir_all(&local_dir).unwrap();

        init_bare_remote(&remote_dir);
        init_local_with_remote(&local_dir, &remote_dir);

        let test_file = local_dir.join("data.txt");
        std::fs::write(&test_file, "hello").unwrap();

        let opts = CommitOptions {
            author_name: Some("Test".to_string()),
            author_email: Some("test@test.com".to_string()),
            signing: None,
            use_github_app_token: false,
        };
        let outcome = commit_and_push_with_opts(
            &local_dir,
            &["data.txt"],
            "add data",
            None,
            "test",
            &opts,
            &StageLogger::new("test", Verbosity::Normal),
        )
        .unwrap();

        assert_eq!(outcome, CommitOutcome::Pushed);
    }

    #[test]
    fn returns_no_changes_when_nothing_staged() {
        let tmp = tempfile::tempdir().unwrap();
        let remote_dir = tmp.path().join("remote");
        let local_dir = tmp.path().join("local");
        std::fs::create_dir_all(&remote_dir).unwrap();
        std::fs::create_dir_all(&local_dir).unwrap();

        init_bare_remote(&remote_dir);
        init_local_with_remote(&local_dir, &remote_dir);

        // Write and commit the file first so it's tracked with identical content.
        let test_file = local_dir.join("data.txt");
        std::fs::write(&test_file, "same content").unwrap();
        Cmd::new("git")
            .args([
                "-c",
                "user.email=test@test.com",
                "-c",
                "user.name=Test",
                "add",
                "data.txt",
            ])
            .current_dir(&local_dir)
            .status()
            .unwrap();
        Cmd::new("git")
            .args([
                "-c",
                "user.email=test@test.com",
                "-c",
                "user.name=Test",
                "commit",
                "-m",
                "pre",
            ])
            .current_dir(&local_dir)
            .status()
            .unwrap();
        Cmd::new("git")
            .args(["push"])
            .current_dir(&local_dir)
            .status()
            .unwrap();

        // Rewrite with identical content — diff --cached will be empty.
        std::fs::write(&test_file, "same content").unwrap();

        let opts = CommitOptions {
            author_name: Some("Test".to_string()),
            author_email: Some("test@test.com".to_string()),
            signing: None,
            use_github_app_token: false,
        };
        let outcome = commit_and_push_with_opts(
            &local_dir,
            &["data.txt"],
            "no-op",
            None,
            "test",
            &opts,
            &StageLogger::new("test", Verbosity::Normal),
        )
        .unwrap();

        assert_eq!(outcome, CommitOutcome::NoChanges);
    }

    /// Anchors the idempotent-retry path used by versioned publisher
    /// branches (winget, krew): a second invocation with the same
    /// content and same target branch must observe matching remote /
    /// staged tree hashes and short-circuit before committing.
    #[test]
    fn returns_no_changes_when_remote_tree_matches_staged() {
        let tmp = tempfile::tempdir().unwrap();
        let remote_dir = tmp.path().join("remote");
        let local_dir = tmp.path().join("local");
        std::fs::create_dir_all(&remote_dir).unwrap();
        std::fs::create_dir_all(&local_dir).unwrap();

        init_bare_remote(&remote_dir);
        init_local_with_remote(&local_dir, &remote_dir);

        let test_file = local_dir.join("manifest.yaml");
        std::fs::write(&test_file, "version: 1.0.0\n").unwrap();

        let opts = CommitOptions {
            author_name: Some("Test".to_string()),
            author_email: Some("test@test.com".to_string()),
            signing: None,
            use_github_app_token: false,
        };

        // First push creates the versioned branch on the remote.
        let first = commit_and_push_with_opts(
            &local_dir,
            &["manifest.yaml"],
            "publish v1.0.0",
            Some("publisher-v1.0.0"),
            "test",
            &opts,
            &StageLogger::new("test", Verbosity::Normal),
        )
        .unwrap();
        assert_eq!(first, CommitOutcome::Pushed);

        // Capture the head sha on the versioned branch before the retry —
        // a successful no-op must leave it untouched.
        let head_before = Cmd::new("git")
            .args(["rev-parse", "refs/remotes/origin/publisher-v1.0.0"])
            .current_dir(&local_dir)
            .output()
            .unwrap();
        let sha_before = String::from_utf8(head_before.stdout)
            .unwrap()
            .trim()
            .to_string();

        // Reset the local branch back to the default tip so the retry
        // mirrors the real-world flow: each publisher run starts from a
        // fresh clone where HEAD is the default branch tip, then writes
        // the artifact. The remote-tree-match check fires only when the
        // staged tree on top of HEAD equals the existing remote tree.
        Cmd::new("git")
            .args(["checkout", "main"])
            .current_dir(&local_dir)
            .status()
            .unwrap();
        Cmd::new("git")
            .args(["branch", "-D", "publisher-v1.0.0"])
            .current_dir(&local_dir)
            .status()
            .unwrap();
        std::fs::write(&test_file, "version: 1.0.0\n").unwrap();

        // Re-invoke with identical content + same target branch — the
        // function fetches origin/publisher-v1.0.0, sees the tree matches
        // the staged tree, and returns NoChanges before committing.
        let retry = commit_and_push_with_opts(
            &local_dir,
            &["manifest.yaml"],
            "publish v1.0.0",
            Some("publisher-v1.0.0"),
            "test",
            &opts,
            &StageLogger::new("test", Verbosity::Normal),
        )
        .unwrap();
        assert_eq!(retry, CommitOutcome::NoChanges);

        let head_after = Cmd::new("git")
            .args(["rev-parse", "refs/remotes/origin/publisher-v1.0.0"])
            .current_dir(&local_dir)
            .output()
            .unwrap();
        let sha_after = String::from_utf8(head_after.stdout)
            .unwrap()
            .trim()
            .to_string();
        assert_eq!(
            sha_before, sha_after,
            "no-op retry must not create a new commit"
        );
    }

    /// Restores (or clears) a process env var on drop so a test that
    /// overrides an ambient `GIT_AUTHOR_*` value cannot leak it to siblings.
    struct EnvVarGuard {
        key: &'static str,
        prev: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var(key).ok();
            // SAFETY: every caller test carries `#[serial(git_env)]`, so no
            // other git-identity test reads or writes the environment
            // concurrently; the guard restores the prior value on drop.
            // env-ok: EnvVarGuard set/restore; every caller test is #[serial(git_env)]
            unsafe { std::env::set_var(key, value) };
            Self { key, prev }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            // SAFETY: see `EnvVarGuard::set` — serialized, single-threaded
            // access for the lifetime of the guard.
            unsafe {
                match &self.prev {
                    // env-ok: EnvVarGuard set/restore; every caller test is #[serial(git_env)]
                    Some(v) => std::env::set_var(self.key, v),
                    // env-ok: EnvVarGuard set/restore; every caller test is #[serial(git_env)]
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    /// Regression: a configured `commit_author` must win over an ambient
    /// `GIT_AUTHOR_NAME` / `GIT_AUTHOR_EMAIL` exported in the process env.
    /// Before the fix the identity was applied via `-c user.name=` /
    /// `-c user.email=`, which git's precedence (`GIT_AUTHOR_*` env > `user.*`
    /// config) silently defeats — so the commit carried the ambient identity
    /// (e.g. a CI runner's), breaking the CLA-registered-author guarantee.
    #[test]
    #[serial(git_env)]
    fn configured_author_overrides_ambient_git_author_env() {
        let tmp = tempfile::tempdir().unwrap();
        let remote_dir = tmp.path().join("remote");
        let local_dir = tmp.path().join("local");
        std::fs::create_dir_all(&remote_dir).unwrap();
        std::fs::create_dir_all(&local_dir).unwrap();

        init_bare_remote(&remote_dir);
        init_local_with_remote(&local_dir, &remote_dir);

        // Ambient env that would hijack a `-c user.name=` override.
        let _name = EnvVarGuard::set("GIT_AUTHOR_NAME", "Ambient Runner");
        let _email = EnvVarGuard::set("GIT_AUTHOR_EMAIL", "runner@ci.invalid");
        let _cname = EnvVarGuard::set("GIT_COMMITTER_NAME", "Ambient Runner");
        let _cemail = EnvVarGuard::set("GIT_COMMITTER_EMAIL", "runner@ci.invalid");

        std::fs::write(local_dir.join("data.txt"), "hello").unwrap();
        let opts = CommitOptions {
            author_name: Some("Release Eng".to_string()),
            author_email: Some("eng@example.invalid".to_string()),
            signing: None,
            use_github_app_token: false,
        };
        let outcome = commit_and_push_with_opts(
            &local_dir,
            &["data.txt"],
            "add data",
            None,
            "test",
            &opts,
            &StageLogger::new("test", Verbosity::Normal),
        )
        .unwrap();
        assert_eq!(outcome, CommitOutcome::Pushed);

        let an = Cmd::new("git")
            .args(["log", "-1", "--pretty=%an"])
            .current_dir(&local_dir)
            .output()
            .unwrap();
        let ae = Cmd::new("git")
            .args(["log", "-1", "--pretty=%ae"])
            .current_dir(&local_dir)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&an.stdout).trim(),
            "Release Eng",
            "configured author name must override the ambient GIT_AUTHOR_NAME"
        );
        assert_eq!(
            String::from_utf8_lossy(&ae.stdout).trim(),
            "eng@example.invalid",
            "configured author email must override the ambient GIT_AUTHOR_EMAIL"
        );
    }

    /// `use_github_app_token` must NOT override the ambient/local identity:
    /// the GitHub App's `<slug>[bot]` identity (set by actions/checkout in the
    /// repo's git config / env) stays authoritative.
    #[test]
    #[serial(git_env)]
    fn use_github_app_token_leaves_ambient_identity_intact() {
        let tmp = tempfile::tempdir().unwrap();
        let remote_dir = tmp.path().join("remote");
        let local_dir = tmp.path().join("local");
        std::fs::create_dir_all(&remote_dir).unwrap();
        std::fs::create_dir_all(&local_dir).unwrap();

        init_bare_remote(&remote_dir);
        init_local_with_remote(&local_dir, &remote_dir);

        let _name = EnvVarGuard::set("GIT_AUTHOR_NAME", "anodizer[bot]");
        let _email = EnvVarGuard::set("GIT_AUTHOR_EMAIL", "bot@users.noreply.github.com");
        let _cname = EnvVarGuard::set("GIT_COMMITTER_NAME", "anodizer[bot]");
        let _cemail = EnvVarGuard::set("GIT_COMMITTER_EMAIL", "bot@users.noreply.github.com");

        std::fs::write(local_dir.join("data.txt"), "hello").unwrap();
        let opts = CommitOptions {
            // A configured author is present but must be ignored because the
            // App-token flag defers to the checkout-configured identity.
            author_name: Some("Release Eng".to_string()),
            author_email: Some("eng@example.invalid".to_string()),
            signing: None,
            use_github_app_token: true,
        };
        commit_and_push_with_opts(
            &local_dir,
            &["data.txt"],
            "add data",
            None,
            "test",
            &opts,
            &StageLogger::new("test", Verbosity::Normal),
        )
        .unwrap();

        let an = Cmd::new("git")
            .args(["log", "-1", "--pretty=%an"])
            .current_dir(&local_dir)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&an.stdout).trim(),
            "anodizer[bot]",
            "use_github_app_token must defer to the ambient App identity, not the config"
        );
    }
}
