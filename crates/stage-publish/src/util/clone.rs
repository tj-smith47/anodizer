//! Repository cloning helpers for publishers — HTTPS (token-based),
//! SSH (private-key or `GIT_SSH_COMMAND`), and the `clone_repo` smart
//! dispatcher that picks one based on `RepositoryConfig`.

use anodizer_core::config::RepositoryConfig;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};
use std::path::Path;
use std::process::Command;

use super::cmd::{redact_output_token, run_cmd_in, run_cmd_in_redacted};

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

    let output = Command::new("git")
        .args(&clone_args)
        .arg(repo_path_str.as_ref())
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
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

    if let Some(ssh_cmd) = ssh_command {
        // Explicit ssh_command takes precedence.
        cmd.env("GIT_SSH_COMMAND", ssh_cmd);
        ssh_cmd_for_config = Some(ssh_cmd.to_string());
    } else if let Some(key_content) = private_key {
        // Write the private key to a file inside the clone target directory's
        // parent so it lives as long as the caller's tempdir.  We use a
        // sibling directory to avoid conflicts with the clone itself.
        let key_dir = tmp_dir.parent().unwrap_or(tmp_dir);
        let key_path = key_dir.join(".anodizer_ssh_key");
        write_ssh_key_secure(&key_path, key_content)
            .with_context(|| format!("{label}: write SSH private key"))?;
        let built_ssh_cmd = format!(
            "ssh -i {} -o StrictHostKeyChecking=accept-new -F /dev/null",
            key_path.display()
        );
        cmd.env("GIT_SSH_COMMAND", &built_ssh_cmd);
        ssh_cmd_for_config = Some(built_ssh_cmd);
    }

    let output = cmd
        .output()
        .with_context(|| format!("{label}: git clone via SSH: spawn"))?;
    // SSH credentials are passed via `GIT_SSH_COMMAND` env / sidecar key
    // file — they never appear in `argv` or in git's stdio. We still call
    // through `redact_output_token` with `None` to keep the call shape
    // symmetric with `clone_repo_with_auth` and to make the absence of a
    // secret-on-argv contract explicit at the read-site.
    let output = redact_output_token(output, None);
    log.check_output(output, &format!("{label}: git clone (SSH)"))?;

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
/// The content is written with a guaranteed trailing newline (see below): an
/// OpenSSH-format private key without one is rejected by `ssh` at parse time.
fn write_ssh_key_secure(path: &Path, key_content: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut f = open_secure_key_file(path)?;
    f.write_all(key_content.as_bytes())?;
    // OpenSSH-format private keys (always the case for ed25519) require a
    // trailing newline. Secret/env round-trips routinely strip it — e.g.
    // `gh secret set -b "$(cat key)"`, where command substitution drops the
    // final newline — after which `ssh` rejects the key with "error in
    // libcrypto" → "Permission denied (publickey)". Append exactly one when
    // the content lacks it; an existing trailing newline is left untouched so
    // a correctly-formed key is never double-terminated.
    if !key_content.ends_with('\n') {
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
pub(crate) fn clone_repo(
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
                    "{label}: repository.token_type={tt:?} is not yet supported; only \"github\" is currently implemented"
                ));
        }
    }

    // Check if RepositoryConfig specifies a Git SSH URL.
    if let Some(r) = repo
        && let Some(ref git) = r.git
        && let Some(ref url) = git.url
    {
        return clone_repo_ssh(
            url,
            git.private_key.as_deref(),
            git.ssh_command.as_deref(),
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
    use std::process::Command;
    use std::sync::OnceLock;

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
        INIT.get_or_init(|| unsafe {
            std::env::set_var("GIT_AUTHOR_NAME", "Anodize Test");
            std::env::set_var("GIT_AUTHOR_EMAIL", "test@anodize.local");
            std::env::set_var("GIT_COMMITTER_NAME", "Anodize Test");
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
            Command::new("git")
                .args(["init", "--bare", "-b", "master"])
                .arg(bare.path())
                .status()
                .unwrap()
                .success()
        );

        for args in [
            vec!["init", "-b", "master"],
            vec!["config", "user.email", "t@example.invalid"],
            vec!["config", "user.name", "T"],
            vec!["config", "commit.gpgsign", "false"],
        ] {
            assert!(
                Command::new("git")
                    .args(&args)
                    .current_dir(work.path())
                    .status()
                    .unwrap()
                    .success()
            );
        }
        std::fs::write(work.path().join("README"), "hi\n").unwrap();
        for args in [vec!["add", "README"], vec!["commit", "-m", "initial"]] {
            assert!(
                Command::new("git")
                    .args(&args)
                    .current_dir(work.path())
                    .status()
                    .unwrap()
                    .success()
            );
        }
        assert!(
            Command::new("git")
                .args(["remote", "add", "origin"])
                .arg(bare.path())
                .current_dir(work.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["push", "-u", "origin", "master"])
                .current_dir(work.path())
                .status()
                .unwrap()
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

    /// When `private_key` is provided, the helper writes the key to a
    /// sibling `.anodizer_ssh_key` file with 0o600 perms (Unix). We can
    /// observe the side-effect even if the clone itself fails downstream,
    /// because the key write happens BEFORE the spawn. Use a parent dir
    /// with a not-yet-existing child so the sibling-write logic kicks in.
    #[cfg(unix)]
    #[test]
    fn clone_repo_ssh_private_key_writes_keyfile_with_0600_perms() {
        use std::os::unix::fs::PermissionsExt;
        let log = StageLogger::new("test", Verbosity::Quiet);
        let parent = tempfile::tempdir().unwrap();
        let dest = parent.path().join("clone-target");
        // Don't create the dest dir — `git clone` will. We just need
        // its parent to exist so the sibling `.anodizer_ssh_key` lands
        // somewhere we can inspect.
        let _ = clone_repo_ssh(
            "ssh://git@127.0.0.1:1/never.git",
            Some("FAKE-KEY-MATERIAL\n"),
            None,
            &dest,
            "ssh-key-test",
            &log,
        );
        // The keyfile lives in the parent of `dest` (see source). Clone
        // will fail (the SSH URL is fake) but the key write happens
        // first, so the file should exist.
        let key_path = dest.parent().unwrap().join(".anodizer_ssh_key");
        assert!(
            key_path.exists(),
            "expected SSH private key sidecar to be written at {}",
            key_path.display()
        );
        let mode = std::fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "key file must be 0600 for ssh to accept it");
        let body = std::fs::read_to_string(&key_path).unwrap();
        assert_eq!(body, "FAKE-KEY-MATERIAL\n");
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
        clone_repo(Some(&repo), "o", "n", None, dest.path(), "warn-test", &log)
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
            clone_repo(Some(&repo), "o", "n", None, dest.path(), "ok", &log)
                .expect("should clone cleanly");
            assert_eq!(
                cap.warn_count(),
                0,
                "expected no warn for token_type={tt:?}"
            );
        }
    }
}
