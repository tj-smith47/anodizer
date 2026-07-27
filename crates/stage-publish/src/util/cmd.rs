//! Subprocess helper used across publisher git/PR/clone flows.
//!
//! Also exposes byte-level token-redaction helpers (`redact_output_token`
//! and `replace_bytes`) used by [`clone`](super::clone) and by the
//! `_redacted` variant of [`run_cmd_in`] below. We do this here — rather
//! than reaching for `core::redact::string` — because the values we need
//! to scrub are raw `Vec<u8>` from `std::process::Output` and the secret
//! is passed in directly (no env-driven heuristic, no key/value mapping).

use anodizer_core::log::StageLogger;
use anodizer_core::retry::{RetryLog, RetryPolicy, is_retriable, retry_sync_deadline};
use anodizer_core::run::run_capture_timeout;
use anyhow::{Context as _, Result};
use std::ops::ControlFlow;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// Replacement marker used in place of redacted secret bytes.
pub(crate) const REDACTED_TOKEN_MARKER: &[u8] = b"<REDACTED_TOKEN>";

/// Run a command in a specific working directory, failing with `label`
/// on spawn failure or non-zero exit. Captures stdout/stderr so that
/// diagnostics are included in the error message.
pub(crate) fn run_cmd_in(dir: &Path, program: &str, args: &[&str], label: &str) -> Result<()> {
    run_cmd_in_envs(dir, program, args, label, &[])
}

/// Variant of [`run_cmd_in`] that sets additional environment variables on
/// the spawned child before running it. Used by the commit step to make a
/// configured author/committer identity authoritative: passing
/// `GIT_AUTHOR_NAME` / `GIT_AUTHOR_EMAIL` (and the committer pair) as child
/// env overrides BOTH an inherited ambient `GIT_AUTHOR_*` env var AND the
/// repo's `user.name` / `user.email` config — `-c user.name=` does neither,
/// since git's identity precedence is `--author` > `GIT_AUTHOR_*` env >
/// `user.*` config.
pub(crate) fn run_cmd_in_envs(
    dir: &Path,
    program: &str,
    args: &[&str],
    label: &str,
    envs: &[(&str, &str)],
) -> Result<()> {
    let mut cmd = Command::new(program);
    cmd.args(args).current_dir(dir);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let output = cmd
        .output()
        .with_context(|| format!("{label}: failed to run {program} {}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        anyhow::bail!(
            "{}: {} {} failed (exit {})\nstderr: {}\nstdout: {}",
            label,
            program,
            args.join(" "),
            output.status.code().unwrap_or(-1),
            stderr,
            stdout
        );
    }
    Ok(())
}

/// Timeout-bounded variant of [`run_cmd_in`] for network-touching git ops
/// (`git push` / `git fetch` to a remote). A remote that accepts the
/// connection then never drains — a wedged TLS handshake, a half-open
/// connection to AUR/a tap repo — would otherwise hang the release forever
/// with no deadline. On expiry the whole git subtree is killed and the call
/// returns a [`Retriable`](anodizer_core::retry::Retriable)-wrapped timeout
/// error, so a call site inside a retry loop can route it to a retry and a
/// non-looping caller surfaces a clear timeout instead of hanging.
///
/// `secret` is scrubbed from `argv`/`stderr`/`stdout` in the error embed
/// exactly as [`run_cmd_in_redacted`] does (pass `None` when no element of
/// `args` can carry a token).
pub(crate) fn run_cmd_in_timeout(
    dir: &Path,
    program: &str,
    args: &[&str],
    label: &str,
    secret: Option<&str>,
    log: &StageLogger,
    timeout: Duration,
) -> Result<()> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0");
    let output = run_capture_timeout(&mut cmd, log, label, timeout).with_context(|| {
        let joined = redact_str(&args.join(" "), secret);
        format!("{label}: failed to run {program} {joined}")
    })?;
    if !output.status.success() {
        let output = redact_output_token(output, secret);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let joined = redact_str(&args.join(" "), secret);
        anyhow::bail!(
            "{}: {} {} failed (exit {})\nstderr: {}\nstdout: {}",
            label,
            program,
            joined,
            output.status.code().unwrap_or(-1),
            stderr,
            stdout
        );
    }
    Ok(())
}

/// Variant of [`run_cmd_in`] that scrubs `secret` from any captured
/// `argv`, `stderr`, and `stdout` before they enter the error message.
///
/// Use this whenever any element of `args` may contain a secret (for
/// example a git remote URL like
/// `https://x-access-token:<TOKEN>@github.com/owner/repo.git`). The
/// success path is unaffected — redaction only kicks in when an error
/// surface is being constructed.
pub(crate) fn run_cmd_in_redacted(
    dir: &Path,
    program: &str,
    args: &[&str],
    label: &str,
    secret: Option<&str>,
) -> Result<()> {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| {
            let joined = redact_str(&args.join(" "), secret);
            format!("{label}: failed to run {program} {joined}")
        })?;
    if !output.status.success() {
        let output = redact_output_token(output, secret);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let joined = redact_str(&args.join(" "), secret);
        anyhow::bail!(
            "{}: {} {} failed (exit {})\nstderr: {}\nstdout: {}",
            label,
            program,
            joined,
            output.status.code().unwrap_or(-1),
            stderr,
            stdout
        );
    }
    Ok(())
}

/// Replace every byte-string occurrence of `secret` in `output.stderr`
/// and `output.stdout` with `<REDACTED_TOKEN>`. Used to scrub git's
/// failure messages that echo a remote URL containing the token.
///
/// `secret = None` and `secret = Some("")` are no-ops: the input is
/// returned unchanged. We intentionally refuse to redact the empty
/// string so that an unset token doesn't replace every empty substring
/// in the byte stream.
pub(crate) fn redact_output_token(mut output: Output, secret: Option<&str>) -> Output {
    if let Some(tok) = secret
        && !tok.is_empty()
    {
        output.stderr = replace_bytes(&output.stderr, tok.as_bytes(), REDACTED_TOKEN_MARKER);
        output.stdout = replace_bytes(&output.stdout, tok.as_bytes(), REDACTED_TOKEN_MARKER);
    }
    output
}

/// String-level twin of [`redact_output_token`] for the `argv` we embed
/// into error messages. Returns the input unchanged when `secret` is
/// `None` or empty.
fn redact_str(input: &str, secret: Option<&str>) -> String {
    match secret {
        Some(tok) if !tok.is_empty() => input.replace(tok, "<REDACTED_TOKEN>"),
        _ => input.to_string(),
    }
}

/// Non-overlapping byte replacement: every occurrence of `needle` in
/// `haystack` is replaced with `replacement`. After a match the cursor
/// jumps past the consumed needle, so overlapping matches collapse to
/// the leftmost ones (e.g. `aa` in `aaaa` → two replacements, not three).
///
/// Returns a clone of `haystack` when `needle` or `haystack` is empty.
pub(crate) fn replace_bytes(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    if needle.is_empty() || haystack.is_empty() {
        return haystack.to_vec();
    }
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if haystack[i..].starts_with(needle) {
            out.extend_from_slice(replacement);
            i += needle.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out
}

/// The retry ladder applied to one `git push`: the backoff shape plus the
/// wall-clock ceiling it shares with the rest of the enclosing publisher
/// invocation.
///
/// Bundled rather than passed as two parameters because they are only
/// meaningful together — a policy handed around without its budget lets every
/// seam of one invocation spend the whole ceiling again, which is exactly what
/// [`anodizer_core::context::Context::retry_deadline`] exists to prevent.
#[derive(Clone, Copy)]
pub(crate) struct PushLadder<'a> {
    /// Backoff shape: attempt count and the delay ladder between attempts.
    pub policy: &'a RetryPolicy,
    /// Absolute instant past which no further attempt is started. `None`
    /// bounds the ladder by attempt count alone.
    pub deadline: Option<Instant>,
}

/// Run a `git push` to a publisher-owned remote under a retry ladder.
///
/// Every such push crosses a network the release does not control — AUR over
/// SSH, GitHub / GitLab over HTTPS — and the remotes rate-limit and drop
/// connections on their own schedule. A single dropped connection used to fail
/// the publisher outright, and with it a release whose artifacts were already
/// built, signed and uploaded.
///
/// Transport failures are classified by the shared
/// [`anodizer_core::retry::is_retriable`], which already carries the
/// vocabulary these drops surface as (`connection closed`, `connection reset`,
/// `broken pipe`, `operation timed out`, …). A push rejected for any other
/// reason — bad key, protected branch, non-fast-forward, a stale
/// `--force-with-lease` — matches no needle and fast-fails on the first
/// attempt, so a push that can never succeed is not held behind a ladder.
///
/// One consequence worth naming: if a push lands server-side and the
/// connection drops before git sees the ack, the next attempt on a
/// `--force-with-lease` refspec fails the lease and stops. That is the
/// intended outcome — the remote already holds the desired state, and the
/// publisher's own tree-hash comparison reports `NoChanges` on the next run
/// rather than overwriting a ref whose current value it no longer knows.
pub(crate) fn run_git_push_retrying(
    dir: &Path,
    args: &[&str],
    label: &str,
    secret: Option<&str>,
    log: &StageLogger,
    timeout: Duration,
    ladder: PushLadder<'_>,
) -> Result<()> {
    retry_sync_deadline(
        RetryLog::new(label, log),
        ladder.policy,
        ladder.deadline,
        |_attempt| {
            run_cmd_in_timeout(dir, "git", args, label, secret, log, timeout).map_err(|e| {
                if is_retriable(e.as_ref()) {
                    ControlFlow::Continue(e)
                } else {
                    ControlFlow::Break(e)
                }
            })
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::log::{StageLogger, Verbosity};
    use std::time::Instant;

    /// A network git op (`git push`/`fetch`) that wedges on an unreachable
    /// remote must be killed at the deadline and surface a retriable timeout —
    /// never hang the release. This is the publisher-stage analogue of
    /// `core::run`'s `run_capture_timeout_kills_hung_child`, exercised through
    /// the shared `run_cmd_in_timeout` bound that every git remote site uses.
    #[test]
    fn run_cmd_in_timeout_kills_hung_child() {
        let (log, _cap) = StageLogger::with_capture("test", Verbosity::Normal);
        let dir = tempfile::tempdir().expect("tempdir");
        let start = Instant::now();
        // A real `git push` to a dead remote can block indefinitely; `sleep`
        // is the deterministic stand-in for that wedge.
        let err = run_cmd_in_timeout(
            dir.path(),
            "sleep",
            &["30"],
            "hung git push",
            None,
            &log,
            Duration::from_millis(200),
        )
        .expect_err("a child outliving the timeout must surface as Err");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "the bound must return promptly (killed the child), took {elapsed:?}"
        );
        assert!(
            anodizer_core::retry::is_retriable(err.as_ref()),
            "a deadline kill must classify retriable so a looping caller retries; got: {err:#}"
        );
    }

    /// The bound must be transparent on the fast path: a child that exits well
    /// inside the deadline returns success exactly like the unbounded
    /// `run_cmd_in`, so wrapping a remote git op in the timeout changes nothing
    /// when the remote responds.
    #[test]
    fn run_cmd_in_timeout_succeeds_within_deadline() {
        let (log, _cap) = StageLogger::with_capture("test", Verbosity::Normal);
        let dir = tempfile::tempdir().expect("tempdir");
        run_cmd_in_timeout(
            dir.path(),
            "true",
            &[],
            "fast op",
            None,
            &log,
            Duration::from_secs(30),
        )
        .expect("a child that exits inside the deadline must succeed");
    }

    /// Ladder used by the push tests: three attempts with no real backoff, so
    /// the retry path is exercised without spending wall-clock in sleeps.
    const FAST_LADDER: RetryPolicy = RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(1),
    };

    /// Initialize a repo with one commit and `origin` pointed at `remote_url`,
    /// so `git push origin master` reaches (or fails to reach) that remote.
    fn repo_with_origin(remote_url: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for args in [
            vec!["init", "-q", "-b", "master"],
            vec!["config", "user.name", "t"],
            vec!["config", "user.email", "t@example.invalid"],
            vec!["commit", "-q", "--allow-empty", "-m", "seed"],
            vec!["remote", "add", "origin", remote_url],
        ] {
            let ok = anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(&args).current_dir(dir.path());
                    cmd
                },
                "git",
            );
            assert!(ok.status.success(), "git {args:?} failed");
        }
        dir
    }

    /// A dropped connection to the remote — the exact shape that failed a
    /// release when AUR closed the SSH connection mid-push — must be retried,
    /// not fast-failed. Port 1 on loopback refuses instantly, so the transient
    /// classification is exercised without a network or a timeout.
    #[test]
    fn git_push_retries_a_refused_connection() {
        let (log, cap) = StageLogger::with_capture("test", Verbosity::Normal);
        let dir = repo_with_origin("ssh://git@127.0.0.1:1/unreachable.git");
        let err = run_git_push_retrying(
            dir.path(),
            &["push", "origin", "master"],
            "test: git push",
            None,
            &log,
            Duration::from_secs(30),
            PushLadder {
                policy: &FAST_LADDER,
                deadline: None,
            },
        )
        .expect_err("an unreachable remote must still end in Err");
        assert!(
            anodizer_core::retry::is_retriable(err.as_ref()),
            "a refused connection must classify retriable; got: {err:#}"
        );
        let logged = cap.warn_messages().join("\n");
        assert!(
            logged.contains("attempt 2/3"),
            "the ladder must report a second attempt; log was:\n{logged}"
        );
    }

    /// A push the remote REJECTS is not a transport problem — no number of
    /// retries changes the answer, and sleeping through a ladder only delays
    /// the operator's error. It must fail on the first attempt.
    #[test]
    fn git_push_fast_fails_a_rejected_ref() {
        let (log, cap) = StageLogger::with_capture("test", Verbosity::Normal);
        let bare = tempfile::tempdir().expect("bare tempdir");
        let ok = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(["init", "-q", "--bare", "-b", "master", "."])
                    .current_dir(bare.path());
                cmd
            },
            "git init --bare",
        );
        assert!(ok.status.success());
        let bare_url = bare.path().to_string_lossy().to_string();

        // Seed the remote from one clone, then push a DIVERGENT history from
        // another: git rejects the second as non-fast-forward.
        let seeded = repo_with_origin(&bare_url);
        assert!(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["push", "-q", "origin", "master"])
                        .current_dir(seeded.path());
                    cmd
                },
                "git push",
            )
            .status
            .success()
        );
        // The seed commit hashes IDENTICALLY in both repos (same empty tree,
        // message, identity and second), so ADDING a commit here would still
        // be a fast-forward. Rewriting the root is what makes the two
        // histories share no commit and the push genuinely non-fast-forward.
        let diverged = repo_with_origin(&bare_url);
        assert!(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args([
                        "commit",
                        "-q",
                        "--amend",
                        "--allow-empty",
                        "-m",
                        "divergent root",
                    ])
                    .current_dir(diverged.path());
                    cmd
                },
                "git commit --amend",
            )
            .status
            .success()
        );

        let err = run_git_push_retrying(
            diverged.path(),
            &["push", "origin", "master"],
            "test: git push",
            None,
            &log,
            Duration::from_secs(30),
            PushLadder {
                policy: &FAST_LADDER,
                deadline: None,
            },
        )
        .expect_err("a non-fast-forward push must fail");
        assert!(
            !anodizer_core::retry::is_retriable(err.as_ref()),
            "a rejected ref must NOT classify retriable; got: {err:#}"
        );
        let logged = cap.warn_messages().join("\n");
        assert!(
            !logged.contains("attempt 2/3"),
            "a rejected ref must not be retried; log was:\n{logged}"
        );
    }

    /// A push that the remote accepts must behave exactly like the unretried
    /// call it replaced — one attempt, no ladder chatter.
    #[test]
    fn git_push_succeeds_without_retrying() {
        let (log, cap) = StageLogger::with_capture("test", Verbosity::Normal);
        let bare = tempfile::tempdir().expect("bare tempdir");
        assert!(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["init", "-q", "--bare", "-b", "master", "."])
                        .current_dir(bare.path());
                    cmd
                },
                "git init --bare",
            )
            .status
            .success()
        );
        let dir = repo_with_origin(&bare.path().to_string_lossy());
        run_git_push_retrying(
            dir.path(),
            &["push", "origin", "master"],
            "test: git push",
            None,
            &log,
            Duration::from_secs(30),
            PushLadder {
                policy: &FAST_LADDER,
                deadline: None,
            },
        )
        .expect("a reachable remote must accept the push");
        let logged = cap.warn_messages().join("\n");
        assert!(
            !logged.contains("attempt"),
            "a clean push must stay silent; log was:\n{logged}"
        );
    }
}
