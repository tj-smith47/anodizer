//! Subprocess helper used across publisher git/PR/clone flows.
//!
//! Also exposes byte-level token-redaction helpers (`redact_output_token`
//! and `replace_bytes`) used by [`clone`](super::clone) and by the
//! `_redacted` variant of [`run_cmd_in`] below. We do this here — rather
//! than reaching for `core::redact::string` — because the values we need
//! to scrub are raw `Vec<u8>` from `std::process::Output` and the secret
//! is passed in directly (no env-driven heuristic, no key/value mapping).

use anyhow::{Context as _, Result};
use std::path::Path;
use std::process::{Command, Output};

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
