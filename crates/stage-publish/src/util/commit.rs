//! Commit-author resolution and the staged-add+commit+push helper used
//! by every publisher that touches a remote git repo (homebrew, scoop,
//! winget, krew, aur, aur_source, nix, chocolatey).

use anodizer_core::context::Context;
use anyhow::Result;
use std::path::Path;
use std::process::Command;

use super::cmd::run_cmd_in;

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

/// Optional overrides for the git commit step.
#[derive(Default)]
pub(crate) struct CommitOptions<'a> {
    /// Git commit author name (passed via `-c user.name=X`). Owned because
    /// `resolve_commit_opts` may shell out to `git config user.name`, whose
    /// result is a fresh String.
    pub author_name: Option<String>,
    /// Git commit author email (passed via `-c user.email=X`).
    pub author_email: Option<String>,
    /// Enable GPG/SSH signing for the commit.
    pub signing: Option<&'a anodizer_core::config::CommitSigningConfig>,
    /// Q-author1: when true, suppress emitting `-c user.name=` /
    /// `-c user.email=` so the running git client uses the GitHub App's
    /// identity (already configured in the repo's local git config by the
    /// Actions checkout step). Mirrors GR
    /// `internal/git/config/github.go:381`.
    pub use_github_app_token: bool,
}

/// Default commit author name used when no author is configured.
/// Mirrors GoReleaser's default of "goreleaserbot" (internal/commitauthor/author.go:11).
const DEFAULT_COMMIT_AUTHOR_NAME: &str = "anodizer";

/// Default commit author email used when no author is configured.
/// Mirrors GoReleaser's default of "bot@goreleaser.com" (internal/commitauthor/author.go:12).
const DEFAULT_COMMIT_AUTHOR_EMAIL: &str = "bot@anodizer.dev";

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
/// Q-author1: GoReleaser's `commitauthor.Get(ctx, og)`
/// (`internal/commitauthor/author.go`) template-renders `Name`, `Email`, and
/// `Signing.{Key, Program, Format}` before applying built-in defaults. We
/// mirror that here by passing each non-empty config-supplied value through
/// `ctx.render_template(...)` before the local-git / built-in fallbacks.
/// Templates that fail to render fall back to the literal string (rather
/// than returning an error that would break the publish stage), matching
/// the fail-soft behaviour of `resolve_token` and friends in this module —
/// diagnostic logging still surfaces the underlying issue at the call site.
///
/// `use_github_app_token` is propagated from the config struct onto the
/// resulting `CommitOptions`. When true, downstream
/// `commit_and_push_with_opts` skips the `-c user.name=…` / `-c user.email=…`
/// overrides so the local git config (configured by the Actions checkout
/// step) wins. Mirrors GR `internal/git/config/github.go:381`.
pub(crate) fn resolve_commit_opts<'a>(
    ctx: &Context,
    commit_author: Option<&'a anodizer_core::config::CommitAuthorConfig>,
) -> CommitOptions<'a> {
    let render =
        |raw: &str| -> String { ctx.render_template(raw).unwrap_or_else(|_| raw.to_string()) };

    let (cfg_name, cfg_email, signing, use_github_app_token) = if let Some(ca) = commit_author {
        (
            ca.name.as_deref(),
            ca.email.as_deref(),
            ca.signing.as_ref(),
            ca.use_github_app_token,
        )
    } else {
        (None, None, None, false)
    };

    let name = cfg_name
        .map(render)
        .or_else(anodizer_core::git::local_git_user_name)
        .unwrap_or_else(|| DEFAULT_COMMIT_AUTHOR_NAME.to_string());
    let email = cfg_email
        .map(render)
        .or_else(anodizer_core::git::local_git_user_email)
        .unwrap_or_else(|| DEFAULT_COMMIT_AUTHOR_EMAIL.to_string());

    CommitOptions {
        author_name: Some(name),
        author_email: Some(email),
        signing,
        use_github_app_token,
    }
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
    opts: &CommitOptions<'_>,
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
        let _ = Command::new("git")
            .args(["fetch", "--depth=1", "origin", &refspec])
            .current_dir(repo_path)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output();
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

    // Build commit args, optionally injecting -c user.name / -c user.email / signing.
    //
    // Q-author1: when `use_github_app_token` is set on the CommitAuthorConfig,
    // skip the explicit `-c user.name=` / `-c user.email=` overrides so the
    // local git config (already populated by the GitHub Actions checkout step
    // with the App's `<app-slug>[bot]` identity) is the authority on commit
    // identity. Mirrors GR `internal/git/config/github.go:381`.
    let mut commit_args: Vec<&str> = Vec::new();
    let name_cfg;
    let email_cfg;
    let sign_cfg;
    let sign_key_cfg;
    let sign_program_cfg;
    let sign_format_cfg;
    if !opts.use_github_app_token {
        if let Some(ref name) = opts.author_name {
            name_cfg = format!("user.name={}", name);
            commit_args.extend_from_slice(&["-c", &name_cfg]);
        }
        if let Some(ref email) = opts.author_email {
            email_cfg = format!("user.email={}", email);
            commit_args.extend_from_slice(&["-c", &email_cfg]);
        }
    }
    // Handle commit signing config
    let do_sign = opts.signing.and_then(|s| s.enabled).unwrap_or(false);
    if do_sign {
        sign_cfg = "commit.gpgsign=true".to_string();
        commit_args.extend_from_slice(&["-c", &sign_cfg]);
        if let Some(key) = opts.signing.and_then(|s| s.key.as_deref()) {
            sign_key_cfg = format!("user.signingkey={}", key);
            commit_args.extend_from_slice(&["-c", &sign_key_cfg]);
        }
        if let Some(program) = opts.signing.and_then(|s| s.program.as_deref()) {
            sign_program_cfg = format!("gpg.program={}", program);
            commit_args.extend_from_slice(&["-c", &sign_program_cfg]);
        }
        // GoReleaser commitauthor/author.go:49-52 defaults signing.format to
        // "openpgp" when signing is enabled but format is unset — otherwise
        // users inherit the system's `gpg.format` (ssh/x509) which isn't what
        // they asked for.
        let fmt = opts
            .signing
            .and_then(|s| s.format.as_deref())
            .unwrap_or("openpgp");
        sign_format_cfg = format!("gpg.format={}", fmt);
        commit_args.extend_from_slice(&["-c", &sign_format_cfg]);
    }
    commit_args.extend_from_slice(&["commit", "-m", message]);

    run_cmd_in(
        repo_path,
        "git",
        &commit_args,
        &format!("{label}: git commit"),
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
    run_cmd_in(repo_path, "git", &push_args, &format!("{label}: git push"))?;
    Ok(CommitOutcome::Pushed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as Cmd;

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
        let outcome =
            commit_and_push_with_opts(&local_dir, &["data.txt"], "add data", None, "test", &opts)
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
        let outcome =
            commit_and_push_with_opts(&local_dir, &["data.txt"], "no-op", None, "test", &opts)
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
}
