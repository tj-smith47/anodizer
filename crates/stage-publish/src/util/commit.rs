//! Commit-author resolution and the staged-add+commit+push helper used
//! by every publisher that touches a remote git repo (homebrew, scoop,
//! winget, krew, aur, aur_source, nix, chocolatey).

use anyhow::Result;
use std::path::Path;
use std::process::Command;

use super::cmd::run_cmd_in;

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
pub(crate) fn resolve_commit_opts<'a>(
    commit_author: Option<&'a anodizer_core::config::CommitAuthorConfig>,
) -> CommitOptions<'a> {
    let (cfg_name, cfg_email, signing) = if let Some(ca) = commit_author {
        (ca.name.as_deref(), ca.email.as_deref(), ca.signing.as_ref())
    } else {
        (None, None, None)
    };

    let name = cfg_name
        .map(|s| s.to_string())
        .or_else(anodizer_core::git::local_git_user_name)
        .unwrap_or_else(|| DEFAULT_COMMIT_AUTHOR_NAME.to_string());
    let email = cfg_email
        .map(|s| s.to_string())
        .or_else(anodizer_core::git::local_git_user_email)
        .unwrap_or_else(|| DEFAULT_COMMIT_AUTHOR_EMAIL.to_string());

    CommitOptions {
        author_name: Some(name),
        author_email: Some(email),
        signing,
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
) -> Result<()> {
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
            return Ok(());
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
        return Ok(());
    }

    // Build commit args, optionally injecting -c user.name / -c user.email / signing.
    let mut commit_args: Vec<&str> = Vec::new();
    let name_cfg;
    let email_cfg;
    let sign_cfg;
    let sign_key_cfg;
    let sign_program_cfg;
    let sign_format_cfg;
    if let Some(ref name) = opts.author_name {
        name_cfg = format!("user.name={}", name);
        commit_args.extend_from_slice(&["-c", &name_cfg]);
    }
    if let Some(ref email) = opts.author_email {
        email_cfg = format!("user.email={}", email);
        commit_args.extend_from_slice(&["-c", &email_cfg]);
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
    run_cmd_in(repo_path, "git", &push_args, &format!("{label}: git push"))
}
