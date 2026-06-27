//! Repository cloning helpers for publishers — HTTPS (token-based),
//! SSH (private-key or `GIT_SSH_COMMAND`), and the `clone_repo` smart
//! dispatcher that picks one based on `RepositoryConfig`.

use anodizer_core::config::RepositoryConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::run::run_capture_timeout;
use anyhow::{Context as _, Result};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use super::cmd::{redact_output_token, run_cmd_in, run_cmd_in_redacted};
use super::template::render_or_warn;

/// Wall-clock bound on `git clone` of a publisher index repo (a tap, AUR,
/// winget-pkgs, krew-index). The clone hits the remote, so an unreachable or
/// stalled host would otherwise hang the release forever with no deadline; on
/// expiry the clone subtree is killed and the error surfaces (redacted). Sized
/// as a remote clone/fetch, matching the release-backend/bucket bound.
const GIT_CLONE_TIMEOUT: Duration = Duration::from_secs(300);

/// Clone a git repo into `tmp_dir` with token-based auth.
///
/// Uses a git credential helper that injects the token, which is more
/// reliable than `http.extraheader` across different GitHub token types
/// (classic PATs, fine-grained PATs, GitHub App tokens, GITHUB_TOKEN).
/// The `http.extraheader=Authorization: bearer` approach can be overridden
/// by system credential helpers and doesn't work with all token types.
pub(crate) fn clone_repo_with_auth(
    repo_url: &str,
    token: Option<&str>,
    tmp_dir: &Path,
    label: &str,
    log: &StageLogger,
) -> Result<()> {
    // Embed token in the URL for the clone operation.  This is the same
    // approach used by actions/checkout and is reliable for all GitHub
    // token types.  The URL is only used locally in the subprocess.
    let effective_url = if let Some(tok) = token {
        inject_token_in_url(repo_url, tok)
    } else {
        repo_url.to_string()
    };

    let clone_args: Vec<&str> = vec!["clone", "--depth=1", &effective_url];
    let repo_path_str = tmp_dir.to_string_lossy();

    let mut cmd = Command::new("git");
    cmd.args(&clone_args)
        .arg(repo_path_str.as_ref())
        .env("GIT_TERMINAL_PROMPT", "0");
    // Bounded: the clone hits the remote, so a stalled host must not hang the
    // release with no deadline. A deadline kill surfaces as a Retriable error.
    let output = run_capture_timeout(
        &mut cmd,
        log,
        &format!("{label}: git clone"),
        GIT_CLONE_TIMEOUT,
    )
    .with_context(|| format!("{label}: git clone: spawn"))?;
    // Pre-redact: git's stderr on failure typically echoes the full URL
    // (`fatal: unable to access 'https://x-access-token:<TOKEN>@host/...'`).
    // Scrub the token bytes BEFORE handing the `Output` to `check_output`,
    // which logs `stderr` and `stdout` verbatim.
    let output = redact_output_token(output, token);
    log.check_output(output, &format!("{label}: git clone"))?;

    // Configure the remote URL for subsequent push operations with auth.
    // The push URL embeds the token in `argv`, so we route through the
    // `_redacted` variant so failures don't leak it via the error message.
    if let Some(tok) = token {
        let push_url = inject_token_in_url(repo_url, tok);
        run_cmd_in_redacted(
            tmp_dir,
            "git",
            &["remote", "set-url", "origin", &push_url],
            &format!("{label}: git set push URL"),
            Some(tok),
        )?;
    }

    Ok(())
}

/// Inject an auth token into an HTTPS git URL.
/// `https://github.com/owner/repo.git` → `https://x-access-token:<token>@github.com/owner/repo.git`
fn inject_token_in_url(url: &str, token: &str) -> String {
    if let Some(rest) = url.strip_prefix("https://") {
        format!("https://x-access-token:{}@{}", token, rest)
    } else {
        url.to_string()
    }
}

/// Clone a git repo via SSH, optionally using a private key file or custom
/// SSH command.  When `private_key` is set, it is written to a temporary
/// file and referenced via `GIT_SSH_COMMAND`.  When `ssh_command` is set
/// directly, it takes precedence.
pub(crate) fn clone_repo_ssh(
    git_url: &str,
    private_key: Option<&str>,
    ssh_command: Option<&str>,
    tmp_dir: &Path,
    label: &str,
    log: &StageLogger,
) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.args(["clone", "--depth=1", git_url]);
    let repo_path_str = tmp_dir.to_string_lossy();
    cmd.arg(repo_path_str.as_ref());

    // Determine the GIT_SSH_COMMAND to use.
    // We may need to persist the SSH command string for configuring the repo
    // after clone, so track it here.
    let mut ssh_cmd_for_config: Option<String> = None;
    // Guard for the bootstrap key's tempdir. Holding it here keeps the key
    // alive for the clone; dropping it on EVERY exit path (success, clone
    // failure, panic) removes the key from disk, so the only key bytes that
    // can outlive this call are the ones persisted inside the caller-owned
    // clone directory below.
    let mut bootstrap_key: Option<tempfile::TempDir> = None;

    if let Some(ssh_cmd) = ssh_command {
        // Explicit ssh_command takes precedence.
        cmd.env("GIT_SSH_COMMAND", ssh_cmd);
        ssh_cmd_for_config = Some(ssh_cmd.to_string());
    } else if let Some(key_content) = private_key {
        // The key must exist on disk before the clone can authenticate, but
        // the clone target must not exist yet (git refuses a non-empty
        // target), so the target can't host it. Bootstrap the key from a
        // dedicated fresh tempdir instead: unique per invocation (a stale
        // key from a prior failed run can never collide or be reused) and
        // removed when `bootstrap_key` drops.
        let (key_dir, key_path) = stage_bootstrap_key(key_content, label)?;
        cmd.env("GIT_SSH_COMMAND", ssh_key_command(&key_path));
        bootstrap_key = Some(key_dir);
    }

    // Bounded: the SSH clone hits the remote (AUR, a tap), so a wedged ssh
    // handshake must not hang the release. A deadline kill is a Retriable error.
    let output = run_capture_timeout(
        &mut cmd,
        log,
        &format!("{label}: git clone (SSH)"),
        GIT_CLONE_TIMEOUT,
    )
    .with_context(|| format!("{label}: git clone via SSH: spawn"))?;
    // SSH credentials are passed via `GIT_SSH_COMMAND` env / sidecar key
    // file — they never appear in `argv` or in git's stdio. We still call
    // through `redact_output_token` with `None` to keep the call shape
    // symmetric with `clone_repo_with_auth` and to make the absence of a
    // secret-on-argv contract explicit at the read-site.
    let output = redact_output_token(output, None);
    log.check_output(output, &format!("{label}: git clone (SSH)"))?;

    // Pushes after the clone reuse the key via `core.sshCommand`, so it must
    // outlive this function. Persist it inside the clone's `.git` directory —
    // never the worktree, where it could be staged and pushed — so it shares
    // the caller's clone-dir lifetime and is removed along with it. The
    // bootstrap copy is deleted when `bootstrap_key` drops at return.
    if bootstrap_key.is_some()
        && let Some(key_content) = private_key
    {
        let persisted = tmp_dir.join(".git").join("anodizer_ssh_key");
        write_ssh_key_secure(&persisted, key_content)
            .with_context(|| format!("{label}: persist SSH private key for push"))?;
        ssh_cmd_for_config = Some(ssh_key_command(&persisted));
    }

    // Configure core.sshCommand in the cloned repo so that subsequent push
    // operations use the same SSH credentials.
    if let Some(ref ssh_cfg) = ssh_cmd_for_config {
        run_cmd_in(
            tmp_dir,
            "git",
            &["config", "core.sshCommand", ssh_cfg],
            &format!("{label}: git config sshCommand"),
        )?;
    }

    Ok(())
}

/// Stage the SSH private key used to authenticate the clone itself in a
/// fresh per-invocation tempdir. Returns the tempdir guard plus the key's
/// path; the key is deleted from disk as soon as the guard drops, so the
/// caller controls exactly how long the bootstrap copy survives.
fn stage_bootstrap_key(
    key_content: &str,
    label: &str,
) -> Result<(tempfile::TempDir, std::path::PathBuf)> {
    let key_dir =
        tempfile::tempdir().with_context(|| format!("{label}: create SSH key temp dir"))?;
    let key_path = key_dir.path().join("anodizer_ssh_key");
    write_ssh_key_secure(&key_path, key_content)
        .with_context(|| format!("{label}: write SSH private key"))?;
    Ok((key_dir, key_path))
}

/// `GIT_SSH_COMMAND` string that authenticates with the key at `key_path`.
///
/// The path is single-quoted: git hands this string to a shell, and an
/// unquoted Windows path would have its backslashes consumed as shell escape
/// characters (`C:\Users\…` → `C:Users…`). Embedded single quotes are
/// escaped with the POSIX `'\''` idiom — without it a path component like
/// `O'Brien` would terminate the quoting early and break the `-i` argument.
fn ssh_key_command(key_path: &Path) -> String {
    let path = key_path.display().to_string().replace('\'', r"'\''");
    format!("ssh -i '{path}' -o StrictHostKeyChecking=accept-new -F /dev/null")
}

/// Write an SSH private key to `path` such that it is never world-readable,
/// even for the instant between creation and the mode being applied.
///
/// On unix the file is created with mode `0o600` from the start via
/// `OpenOptions::mode` + `create_new` — a plain `fs::write` followed by a
/// `set_permissions` call would leave a credential-leak window during which
/// the key sits at the umask default (commonly `0o644`, world-readable) on a
/// shared CI runner. `create_new` also rejects an already-present path so a
/// stale key left by an earlier run cannot be silently reused.
///
/// The content is written with exactly one trailing newline (see below): an
/// OpenSSH-format private key without one is rejected by `ssh` at parse time.
fn write_ssh_key_secure(path: &Path, key_content: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut f = open_secure_key_file(path)?;
    // OpenSSH-format private keys (always the case for ed25519) require a
    // trailing newline. Secret/env round-trips routinely strip it — e.g.
    // `gh secret set -b "$(cat key)"`, where command substitution drops the
    // final newline — after which `ssh` rejects the key with "error in
    // libcrypto" → "Permission denied (publickey)". Normalize to exactly one:
    // a missing newline is appended, surplus trailing newlines (a sloppy
    // paste, a doubly-terminated secret) are collapsed. Trailing newlines are
    // never key material, so the rewrite can't corrupt a valid key. Content
    // with no key bytes at all stays empty (an invalid key either way —
    // don't manufacture a lone-newline file).
    let body = key_content.trim_end_matches('\n');
    f.write_all(body.as_bytes())?;
    if !body.is_empty() {
        f.write_all(b"\n")?;
    }
    f.flush()
}

/// Open `path` for writing a private key with `0o600` perms from creation
/// (unix) and `create_new` semantics so a stale key can't be silently reused.
#[cfg(unix)]
fn open_secure_key_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// Windows has no `std`-level analogue of the unix `mode(0o600)` open:
/// `std` exposes no DACL surface, and the `readonly` attribute does not
/// restrict reads, so an in-process ACL tightening would require a winapi
/// dependency. The mitigation is containment instead: every key this module
/// writes lives either in a per-invocation `tempfile` dir or in the clone's
/// `.git` dir — both under `%TEMP%`, which sits inside the invoking user's
/// profile and inherits user-only ACLs — and is deleted when its owning
/// directory guard drops. On a runner whose temp dir is shared AND
/// world-readable, the key may be readable by other local users for the
/// lifetime of the publish; `create_new` still guarantees no stale-key reuse.
#[cfg(not(unix))]
fn open_secure_key_file(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// Smart clone: decide between HTTPS and SSH based on RepositoryConfig.
///
/// When `repo.git.url` is set, uses SSH-based cloning with optional
/// `private_key` / `ssh_command`.  Otherwise falls back to HTTPS via
/// `clone_repo_with_auth`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn clone_repo(
    ctx: &Context,
    repo: Option<&RepositoryConfig>,
    fallback_owner: &str,
    fallback_name: &str,
    token: Option<&str>,
    tmp_dir: &Path,
    label: &str,
    log: &StageLogger,
) -> Result<()> {
    // Warn when token_type is set to a non-GitHub value, since anodizer
    // currently only supports GitHub-based repository publishing.
    if let Some(r) = repo
        && let Some(ref tt) = r.token_type
    {
        let tt_lower = tt.to_lowercase();
        if tt_lower != "github" && !tt_lower.is_empty() {
            log.warn(&format!(
                    "repository.token_type={tt:?} for {label} is not yet supported; only \"github\" is currently implemented"
                ));
        }
    }

    // Check if RepositoryConfig specifies a Git SSH URL.
    if let Some(r) = repo
        && let Some(ref git) = r.git
        && let Some(ref url) = git.url
    {
        // The url / private_key / ssh_command may be templated
        // (`{{ .Env.X }}`). Render before they reach the git+ssh
        // subprocess, or the literal template text is written to the
        // SSH key file / used as the clone URL and ssh fails.
        let rendered_url = render_or_warn(ctx, log, "repository.git.url", url)?;
        let rendered_key = match git.private_key.as_deref() {
            Some(pk) => Some(render_or_warn(ctx, log, "repository.git.private_key", pk)?),
            None => None,
        };
        let rendered_ssh = match git.ssh_command.as_deref() {
            Some(sc) => Some(render_or_warn(ctx, log, "repository.git.ssh_command", sc)?),
            None => None,
        };
        return clone_repo_ssh(
            &rendered_url,
            rendered_key.as_deref(),
            rendered_ssh.as_deref(),
            tmp_dir,
            label,
            log,
        );
    }

    // Fall back to HTTPS clone.
    let repo_url = format!(
        "https://github.com/{}/{}.git",
        fallback_owner, fallback_name
    );
    clone_repo_with_auth(&repo_url, token, tmp_dir, label, log)
}

/// Build the canonical AUR SSH remote for a resolved package name.
///
/// AUR repositories always live at
/// `ssh://aur@aur.archlinux.org/<package>.git`, where `<package>` is the
/// `pkgbase`/`pkgname` the publisher already resolved. Used as the default
/// `git_url` for both the binary (`aur`) and source (`aur_source`)
/// publishers when no explicit override is configured, so the push target
/// can never drift from the package name written into PKGBUILD/.SRCINFO.
pub(crate) fn aur_default_git_url(package_name: &str) -> String {
    format!("ssh://aur@aur.archlinux.org/{}.git", package_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::{GitRepoConfig, RepositoryConfig};
    use anodizer_core::log::{StageLogger, Verbosity};
    use anodizer_core::test_helpers::TestContextBuilder;
    use std::process::Command;
    use std::sync::OnceLock;

    #[test]
    fn ssh_key_command_escapes_single_quotes_in_key_path() {
        let cmd = ssh_key_command(Path::new(
            r"C:\Users\O'Brien\AppData\Local\Temp\anodizer_ssh_key",
        ));
        assert_eq!(
            cmd,
            r"ssh -i 'C:\Users\O'\''Brien\AppData\Local\Temp\anodizer_ssh_key' -o StrictHostKeyChecking=accept-new -F /dev/null",
        );

        let plain = ssh_key_command(Path::new("/tmp/key"));
        assert_eq!(
            plain,
            "ssh -i '/tmp/key' -o StrictHostKeyChecking=accept-new -F /dev/null",
        );
    }

    #[test]
    fn aur_default_git_url_is_canonical_remote() {
        assert_eq!(
            aur_default_git_url("mytool-bin"),
            "ssh://aur@aur.archlinux.org/mytool-bin.git",
        );
        assert_eq!(
            aur_default_git_url("widget"),
            "ssh://aur@aur.archlinux.org/widget.git",
        );
    }

    /// Ensure the test process has a git identity. Subprocess `git`
    /// invocations inside the clone helpers inherit env from the test
    /// process; without this they bail with "Author identity unknown".
    /// One-shot via `OnceLock` to avoid the parallel-test set_var race.
    fn ensure_git_identity() {
        static INIT: OnceLock<()> = OnceLock::new();
        // SAFETY: env mutation runs exactly once per process, guarded by
        // OnceLock; no other test thread observes a half-applied identity.
        // The values are constants, idempotently set and never removed.
        INIT.get_or_init(|| unsafe {
            // env-ok: idempotent OnceLock set of constant git identity, never mutated after
            std::env::set_var("GIT_AUTHOR_NAME", "Anodize Test");
            // env-ok: idempotent OnceLock set of constant git identity, never mutated after
            std::env::set_var("GIT_AUTHOR_EMAIL", "test@anodize.local");
            // env-ok: idempotent OnceLock set of constant git identity, never mutated after
            std::env::set_var("GIT_COMMITTER_NAME", "Anodize Test");
            // env-ok: idempotent OnceLock set of constant git identity, never mutated after
            std::env::set_var("GIT_COMMITTER_EMAIL", "test@anodize.local");
        });
    }

    /// Build a bare remote with one commit on `master`. Returns the
    /// filesystem path to the bare repo (suitable as a `git clone` URL).
    fn make_bare_remote() -> (String, tempfile::TempDir, tempfile::TempDir) {
        ensure_git_identity();
        let bare = tempfile::tempdir().expect("bare");
        let work = tempfile::tempdir().expect("work");

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
            vec!["config", "user.email", "t@example.invalid"],
            vec!["config", "user.name", "T"],
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
                .success()
            );
        }
        std::fs::write(work.path().join("README"), "hi\n").unwrap();
        for args in [vec!["add", "README"], vec!["commit", "-m", "initial"]] {
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
                .success()
            );
        }
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
        (bare.path().to_string_lossy().into_owned(), bare, work)
    }

    // ------------------------------------------------------------------
    // inject_token_in_url — pure string transform.
    // ------------------------------------------------------------------

    /// HTTPS URLs get the `x-access-token:<tok>@` prefix injected after
    /// the scheme — the exact shape `actions/checkout` uses and the only
    /// shape that works across all GitHub token types per the rustdoc.
    #[test]
    fn inject_token_in_url_https_inserts_userinfo() {
        let out = inject_token_in_url("https://github.com/owner/repo.git", "ghp_xyz");
        assert_eq!(
            out,
            "https://x-access-token:ghp_xyz@github.com/owner/repo.git"
        );
    }

    /// Non-HTTPS URLs (SSH, file://, etc.) pass through unchanged — the
    /// dispatcher routes those to the SSH path, where token-in-URL would
    /// be both meaningless and confusing.
    #[test]
    fn inject_token_in_url_non_https_passthrough() {
        let ssh = "ssh://git@github.com/owner/repo.git";
        assert_eq!(inject_token_in_url(ssh, "tok"), ssh);
        let path = "/tmp/foo/bar";
        assert_eq!(inject_token_in_url(path, "tok"), path);
    }

    // ------------------------------------------------------------------
    // clone_repo_with_auth — token-less happy path against local bare.
    // ------------------------------------------------------------------

    /// Cloning a real local bare remote with `token = None` must succeed
    /// and populate the working tree with the committed file. Exercises
    /// the no-auth branch of `clone_repo_with_auth` end-to-end.
    #[test]
    fn clone_repo_with_auth_no_token_clones_local_bare() {
        let (url, _bare, _work) = make_bare_remote();
        let log = StageLogger::new("test", Verbosity::Quiet);
        let dest = tempfile::tempdir().unwrap();
        // tempdir creates the dir; git refuses to clone into an existing
        // non-empty path, but an empty fresh tempdir is fine.
        clone_repo_with_auth(&url, None, dest.path(), "demo", &log)
            .expect("clone local bare with no token");
        assert!(
            dest.path().join("README").exists(),
            "expected README from initial commit to be present in clone"
        );
    }

    /// A bad URL must surface as an Err (not a panic). `label` should
    /// appear in the bubbled error to help operators correlate the
    /// failure with the calling publisher.
    #[test]
    fn clone_repo_with_auth_fails_on_bad_url() {
        let log = StageLogger::new("test", Verbosity::Quiet);
        let dest = tempfile::tempdir().unwrap();
        let err = clone_repo_with_auth(
            "/this/path/does/not/exist/zzz.git",
            None,
            dest.path(),
            "demo-label",
            &log,
        )
        .expect_err("expected clone of nonexistent path to fail");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("demo-label"),
            "expected label in error chain, got: {msg}"
        );
    }

    // ------------------------------------------------------------------
    // clone_repo_ssh — exercise key-writing + ssh_command pass-through.
    // ------------------------------------------------------------------

    /// With no `private_key` / `ssh_command`, `clone_repo_ssh` falls
    /// through to a plain `git clone` (no `GIT_SSH_COMMAND` set). Using
    /// a local filesystem path as the URL skips the actual SSH transport
    /// while still exercising the code path that builds the `Command`.
    #[test]
    fn clone_repo_ssh_no_key_clones_local_path() {
        let (url, _bare, _work) = make_bare_remote();
        let log = StageLogger::new("test", Verbosity::Quiet);
        let dest = tempfile::tempdir().unwrap();
        clone_repo_ssh(&url, None, None, dest.path(), "ssh-test", &log)
            .expect("ssh clone of local bare with no key");
        assert!(dest.path().join("README").exists());
    }

    /// When `private_key` is provided and the clone succeeds, the key is
    /// persisted at `<clone>/.git/anodizer_ssh_key` (0o600 on Unix) and
    /// `core.sshCommand` points at that persisted copy, so subsequent pushes
    /// authenticate with a key whose lifetime is bound to the clone dir. A
    /// local-path URL skips the real SSH transport (git ignores
    /// `GIT_SSH_COMMAND` for filesystem paths) while exercising the full
    /// write → clone → persist path.
    #[cfg(unix)]
    #[test]
    fn clone_repo_ssh_private_key_persists_key_inside_git_dir() {
        use std::os::unix::fs::PermissionsExt;
        let (url, _bare, _work) = make_bare_remote();
        let log = StageLogger::new("test", Verbosity::Quiet);
        let parent = tempfile::tempdir().unwrap();
        let dest = parent.path().join("clone-target");
        clone_repo_ssh(&url, Some("FAKE-KEY-MATERIAL\n"), None, &dest, "key", &log)
            .expect("local-path clone with private_key succeeds");

        let key_path = dest.join(".git").join("anodizer_ssh_key");
        assert!(
            key_path.exists(),
            "expected SSH private key persisted at {}",
            key_path.display()
        );
        let mode = std::fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "key file must be 0600 for ssh to accept it");
        assert_eq!(
            std::fs::read_to_string(&key_path).unwrap(),
            "FAKE-KEY-MATERIAL\n"
        );

        // core.sshCommand must reference the PERSISTED key, not the
        // (already-deleted) bootstrap copy.
        let cfg = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(["config", "core.sshCommand"]).current_dir(&dest);
                cmd
            },
            "git",
        );
        let cfg = String::from_utf8_lossy(&cfg.stdout);
        assert!(
            cfg.contains(&key_path.display().to_string()),
            "core.sshCommand must point at the persisted key, got: {cfg}"
        );

        // No key sidecar may remain next to the clone target (the historical
        // leak location: a sibling of a tempdir-root clone target lands in
        // the SHARED system temp dir and was never cleaned up).
        let siblings: Vec<_> = std::fs::read_dir(parent.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .filter(|n| n.contains("anodizer_ssh"))
            .collect();
        assert!(
            siblings.is_empty(),
            "no key sidecar may be left next to the clone target, found: {siblings:?}"
        );
    }

    /// A FAILED clone must leave no key bytes behind: the bootstrap tempdir
    /// guard drops on the error path and removes the key with it, and no
    /// sidecar is ever written next to the clone target.
    #[test]
    fn clone_repo_ssh_failed_clone_leaves_no_key_sidecar() {
        let log = StageLogger::new("test", Verbosity::Quiet);
        let parent = tempfile::tempdir().unwrap();
        let dest = parent.path().join("clone-target");
        clone_repo_ssh(
            "ssh://git@127.0.0.1:1/never.git",
            Some("FAKE-KEY-MATERIAL\n"),
            None,
            &dest,
            "ssh-key-test",
            &log,
        )
        .expect_err("clone of a fake SSH URL must fail");
        let leftovers: Vec<_> = std::fs::read_dir(parent.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect();
        assert!(
            leftovers.iter().all(|n| !n.contains("anodizer_ssh")),
            "failed clone must not leave a key sidecar, found: {leftovers:?}"
        );
    }

    /// The bootstrap key vanishes from disk the moment its tempdir guard
    /// drops — the cleanup guarantee `clone_repo_ssh` relies on for every
    /// exit path, pinned at the unit level.
    #[test]
    fn stage_bootstrap_key_removes_key_when_guard_drops() {
        let (guard, key_path) =
            stage_bootstrap_key("FAKE-KEY\n", "bootstrap-test").expect("stage key");
        assert!(key_path.exists(), "key must exist while the guard lives");
        assert_eq!(std::fs::read_to_string(&key_path).unwrap(), "FAKE-KEY\n");
        drop(guard);
        assert!(
            !key_path.exists(),
            "key must be deleted when the guard drops"
        );
    }

    /// `write_ssh_key_secure` must never expose a world-readable window: the
    /// file is created with `0o600` from the start. The helper applies no
    /// separate `chmod`, so observing `0o600` immediately after creation
    /// proves the mode was carried by `OpenOptions::mode` at `open` time
    /// rather than relaxed-then-tightened the way a `fs::write` + chmod pair
    /// would briefly leave it at the umask default (commonly `0o644`).
    #[cfg(unix)]
    #[test]
    fn write_ssh_key_secure_creates_with_0600_at_open_time() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join(".anodizer_ssh_key");

        write_ssh_key_secure(&key_path, "FAKE-KEY\n").expect("secure key write succeeds");

        let mode = std::fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "key file must be created at 0600 with no permissive window, got {mode:o}"
        );
        assert_eq!(std::fs::read_to_string(&key_path).unwrap(), "FAKE-KEY\n");
    }

    /// `create_new` semantics: an already-present key path is rejected rather
    /// than silently truncated/reused, so a stale sidecar from an earlier run
    /// cannot leak into a fresh clone.
    #[cfg(unix)]
    #[test]
    fn write_ssh_key_secure_rejects_existing_path() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join(".anodizer_ssh_key");
        write_ssh_key_secure(&key_path, "first\n").expect("first write succeeds");
        let err = write_ssh_key_secure(&key_path, "second\n")
            .expect_err("second write must fail on an existing path");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    }

    /// A key content lacking a trailing newline (the shape a secret left by
    /// `$(cat key)` command substitution arrives in) must be written back with
    /// exactly one appended — an OpenSSH-format key without it fails `ssh` parse
    /// with "error in libcrypto".
    #[test]
    fn write_ssh_key_secure_appends_missing_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join(".anodizer_ssh_key");
        write_ssh_key_secure(&key_path, "-----END OPENSSH PRIVATE KEY-----")
            .expect("write succeeds");
        assert_eq!(
            std::fs::read_to_string(&key_path).unwrap(),
            "-----END OPENSSH PRIVATE KEY-----\n",
            "a missing trailing newline must be appended exactly once"
        );
    }

    /// A correctly-terminated key is left byte-for-byte intact — the helper
    /// must not double-terminate a key that already ends in a newline.
    #[test]
    fn write_ssh_key_secure_preserves_existing_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join(".anodizer_ssh_key");
        write_ssh_key_secure(&key_path, "-----END OPENSSH PRIVATE KEY-----\n")
            .expect("write succeeds");
        assert_eq!(
            std::fs::read_to_string(&key_path).unwrap(),
            "-----END OPENSSH PRIVATE KEY-----\n",
            "an existing trailing newline must not be doubled"
        );
    }

    /// Surplus trailing newlines (a sloppy paste, a doubly-terminated secret)
    /// are collapsed to exactly one — trailing newlines are never key
    /// material, so the rewrite cannot corrupt a valid key.
    #[test]
    fn write_ssh_key_secure_normalizes_multiple_trailing_newlines() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join(".anodizer_ssh_key");
        write_ssh_key_secure(&key_path, "-----END OPENSSH PRIVATE KEY-----\n\n\n")
            .expect("write succeeds");
        assert_eq!(
            std::fs::read_to_string(&key_path).unwrap(),
            "-----END OPENSSH PRIVATE KEY-----\n",
            "surplus trailing newlines must be collapsed to exactly one"
        );
    }

    /// Content that is nothing but newlines carries no key bytes and is
    /// treated like empty content: the file stays empty rather than keeping
    /// meaningless newline-only bytes.
    #[test]
    fn write_ssh_key_secure_treats_newline_only_content_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join(".anodizer_ssh_key");
        write_ssh_key_secure(&key_path, "\n\n").expect("write succeeds");
        assert_eq!(
            std::fs::read(&key_path).unwrap(),
            Vec::<u8>::new(),
            "newline-only content must produce an empty file"
        );
    }

    /// Empty content yields an empty file — the newline-normalization must not
    /// manufacture a lone-newline file from nothing (an empty key is invalid
    /// regardless, but the byte output should be faithful, not fabricated).
    #[test]
    fn write_ssh_key_secure_leaves_empty_content_empty() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join(".anodizer_ssh_key");
        write_ssh_key_secure(&key_path, "").expect("write succeeds");
        assert_eq!(
            std::fs::read(&key_path).unwrap(),
            Vec::<u8>::new(),
            "empty content must produce an empty file, not a lone newline"
        );
    }

    // ------------------------------------------------------------------
    // clone_repo dispatcher — picks SSH vs HTTPS.
    // ------------------------------------------------------------------

    /// When `repository.git.url` is set, the dispatcher routes to the
    /// SSH path and ignores the HTTPS fallback owner/name. Local-path
    /// URL stands in for an SSH URL — `clone_repo_ssh` doesn't enforce
    /// the scheme, only the dispatcher's discriminator does.
    #[test]
    fn clone_repo_dispatcher_uses_git_url_when_set() {
        let (url, _bare, _work) = make_bare_remote();
        let ctx = TestContextBuilder::new().build();
        let log = StageLogger::new("test", Verbosity::Quiet);
        let dest = tempfile::tempdir().unwrap();
        let repo = RepositoryConfig {
            git: Some(GitRepoConfig {
                url: Some(url.clone()),
                ..Default::default()
            }),
            ..Default::default()
        };
        clone_repo(
            &ctx,
            Some(&repo),
            "ignored-owner",
            "ignored-name",
            None,
            dest.path(),
            "dispatch",
            &log,
        )
        .expect("dispatcher should clone via SSH path when git.url is set");
        assert!(dest.path().join("README").exists());
    }

    /// A templated `git.url` (`{{ .Env.X }}`) must be rendered to the env
    /// value before it reaches `git clone`; the literal template text must
    /// never be used as the clone target. Drives the SSH dispatch branch
    /// with the real local bare remote injected through the env.
    #[test]
    fn clone_repo_renders_templated_git_url() {
        let (url, _bare, _work) = make_bare_remote();
        let mut ctx = TestContextBuilder::new().build();
        ctx.template_vars_mut().set_env("AUR_GIT_URL", &url);
        let log = StageLogger::new("test", Verbosity::Quiet);
        let dest = tempfile::tempdir().unwrap();
        let repo = RepositoryConfig {
            git: Some(GitRepoConfig {
                url: Some("{{ .Env.AUR_GIT_URL }}".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        clone_repo(
            &ctx,
            Some(&repo),
            "ignored-owner",
            "ignored-name",
            None,
            dest.path(),
            "dispatch",
            &log,
        )
        .expect("templated git.url must render to the env value before clone");
        assert!(
            dest.path().join("README").exists(),
            "clone must have used the rendered URL, not the literal template"
        );
    }

    /// A templated `git.private_key` (`{{ .Env.X }}`) must be rendered
    /// before the key bytes are written to disk; the literal `{{` must
    /// never reach the SSH key file (the canonical `error in libcrypto`
    /// failure). The clone target is the local bare remote (git ignores
    /// `GIT_SSH_COMMAND` for filesystem paths), so the clone succeeds and
    /// the persisted key in `.git/` is inspectable.
    #[cfg(unix)]
    #[test]
    fn clone_repo_renders_templated_private_key_before_write() {
        let (url, _bare, _work) = make_bare_remote();
        let mut ctx = TestContextBuilder::new().build();
        ctx.template_vars_mut()
            .set_env("AUR_SSH_KEY", "RENDERED-KEY-MATERIAL\n");
        let log = StageLogger::new("test", Verbosity::Quiet);
        let parent = tempfile::tempdir().unwrap();
        let dest = parent.path().join("clone-target");
        let repo = RepositoryConfig {
            git: Some(GitRepoConfig {
                url: Some(url.clone()),
                private_key: Some("{{ .Env.AUR_SSH_KEY }}".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        clone_repo(&ctx, Some(&repo), "o", "n", None, &dest, "key-render", &log)
            .expect("local-path SSH clone with templated key succeeds");
        let key_path = dest.join(".git").join("anodizer_ssh_key");
        let body = std::fs::read_to_string(&key_path).expect("persisted key must be written");
        assert_eq!(
            body, "RENDERED-KEY-MATERIAL\n",
            "the private key must be the rendered env value, never the literal template"
        );
        assert!(
            !body.contains("{{"),
            "the literal template `{{{{` must never reach the SSH key file"
        );
    }

    // HTTPS fallback branch coverage is deferred — the production
    // dispatcher builds `https://github.com/<owner>/<name>.git`
    // unconditionally on the no-git-url path, so testing that branch
    // without a real network round-trip would need either a local
    // HTTPS reverse proxy or a refactor to inject the base URL.
    // Neither is worth the complexity for a single branch arrow.

    /// A non-"github" `token_type` triggers a warn but does NOT abort
    /// dispatch — anodizer currently only implements GitHub, but the
    /// user-facing contract is "warn, don't fail". We assert the warn
    /// landed in the capture sink, AND that the call proceeded into the
    /// SSH path (succeeds with our local bare remote).
    #[test]
    fn clone_repo_warns_on_non_github_token_type_but_proceeds() {
        let (url, _bare, _work) = make_bare_remote();
        let ctx = TestContextBuilder::new().build();
        let (log, cap) = StageLogger::with_capture("test", Verbosity::Normal);
        let dest = tempfile::tempdir().unwrap();
        let repo = RepositoryConfig {
            token_type: Some("gitlab".into()),
            git: Some(GitRepoConfig {
                url: Some(url.clone()),
                ..Default::default()
            }),
            ..Default::default()
        };
        clone_repo(
            &ctx,
            Some(&repo),
            "o",
            "n",
            None,
            dest.path(),
            "warn-test",
            &log,
        )
        .expect("should proceed despite unsupported token_type");
        assert_eq!(cap.warn_count(), 1, "expected one warn for token_type");
        let msgs = cap.all_messages();
        assert!(
            msgs.iter()
                .any(|(_, m)| m.contains("token_type") && m.contains("gitlab")),
            "expected warn naming the offending token_type, got: {msgs:?}"
        );
    }

    /// `token_type = "github"` (the supported value) must NOT emit a
    /// warning. Same for the case-insensitive variants and the empty
    /// string (treated as "unset" per the source).
    #[test]
    fn clone_repo_does_not_warn_for_github_token_type() {
        let (url, _bare, _work) = make_bare_remote();
        let ctx = TestContextBuilder::new().build();
        for tt in ["github", "GitHub", "GITHUB", ""] {
            let (log, cap) = StageLogger::with_capture("test", Verbosity::Normal);
            let dest = tempfile::tempdir().unwrap();
            let repo = RepositoryConfig {
                token_type: Some(tt.into()),
                git: Some(GitRepoConfig {
                    url: Some(url.clone()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            clone_repo(&ctx, Some(&repo), "o", "n", None, dest.path(), "ok", &log)
                .expect("should clone cleanly");
            assert_eq!(
                cap.warn_count(),
                0,
                "expected no warn for token_type={tt:?}"
            );
        }
    }
}
