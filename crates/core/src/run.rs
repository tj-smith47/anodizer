//! Single subprocess-execution helper that captures stdout/stderr and routes
//! the result through [`StageLogger::check_output`].
//!
//! Consolidates the spawn / capture / surface-on-failure pattern that every
//! stage repeats by hand so the success/failure surface stays consistent:
//!
//! - **Default (quiet) verbosity** — the child is captured silently
//!   (`Command::output()`); on success nothing prints, on non-zero exit the
//!   logger emits the redacted stderr/stdout and `bail!`s with a
//!   tail-truncated, redacted stderr tail embedded in the error chain. This
//!   matches GoReleaser's `CombinedOutput()`-then-surface model and produces
//!   zero behavioral drift versus the open-coded `cmd.output()` +
//!   `log.check_output(...)` sites it replaces.
//! - **Verbose / debug** — the child's stdout and stderr are *teed* live to
//!   the terminal (after secret redaction) AND captured into in-memory
//!   buffers, so a long-running tool (cargo, snapcraft, nix-build, upx) shows
//!   progress as it runs while the failure path keeps the full captured
//!   output for the error embed. The verbose tee is an anodizer-only superset:
//!   GoReleaser never streams live.
//!
//! This module does **not** construct a [`std::process::Command`] — that would
//! make `core` a subprocess-spawn surface, which the module-boundary rule
//! forbids. It runs an already-built command supplied by the caller (`&mut
//! Command`), which `module-boundaries.md` explicitly sanctions.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Output, Stdio};

use anyhow::{Context as _, Result};

use crate::log::StageLogger;

/// Run an already-constructed `cmd`, capturing stdout and stderr, and route
/// the result through [`StageLogger::check_output`].
///
/// - Success → returns the captured [`Output`] (the caller logs anything it
///   needs at verbose; `check_output` already echoes stdout at verbose on the
///   quiet path).
/// - Non-zero exit → bails via `check_output` (tail-truncated, redacted stderr
///   embedded in the error).
///
/// When `log.is_verbose()` the child's stdout/stderr are streamed live (each
/// line redacted) while still being captured, so the failure embed keeps the
/// full output and the live stream is not double-printed.
///
/// stdin is left untouched, preserving the sign stage's `Stdio::inherit()`
/// stdin (gpg pinentry reads the tty). Use [`run_checked_with_stdin`] to feed
/// bytes to the child's stdin.
pub fn run_checked(cmd: &mut Command, log: &StageLogger, label: &str) -> Result<Output> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    if log.is_verbose() {
        run_streamed(cmd, log, label)
    } else {
        let output = cmd
            .output()
            .with_context(|| format!("failed to spawn {label}"))?;
        log.check_output(output, label)
    }
}

/// Like [`run_checked`], but writes `stdin` to the child's standard input
/// before waiting (the cosign / gh / kms / email pipe-input pattern).
///
/// The child's stdin is set to a pipe; stdout/stderr capture and the
/// verbose live-stream behave exactly as in [`run_checked`].
pub fn run_checked_with_stdin(
    cmd: &mut Command,
    stdin: &[u8],
    log: &StageLogger,
    label: &str,
) -> Result<Output> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if log.is_verbose() {
        run_streamed_with_stdin(cmd, Some(stdin), log, label)
    } else {
        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn {label}"))?;
        child
            .stdin
            .take()
            .with_context(|| format!("{label}: child has no stdin pipe"))?
            .write_all(stdin)
            .with_context(|| format!("{label}: failed to write stdin"))?;
        let output = child
            .wait_with_output()
            .with_context(|| format!("{label}: failed to wait for child"))?;
        log.check_output(output, label)
    }
}

/// Verbose path for [`run_checked`]: spawn, tee both streams live while
/// capturing, then route the reconstructed [`Output`] through
/// `check_output_streamed` (which suppresses the re-emit the tee already did).
fn run_streamed(cmd: &mut Command, log: &StageLogger, label: &str) -> Result<Output> {
    run_streamed_with_stdin(cmd, None, log, label)
}

/// Shared verbose tee for the stdin and no-stdin paths. When `stdin` is
/// `Some`, the bytes are written to the child's standard input before the
/// reader threads drain its output.
fn run_streamed_with_stdin(
    cmd: &mut Command,
    stdin: Option<&[u8]>,
    log: &StageLogger,
    label: &str,
) -> Result<Output> {
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn {label}"))?;

    if let Some(bytes) = stdin {
        child
            .stdin
            .take()
            .with_context(|| format!("{label}: child has no stdin pipe"))?
            .write_all(bytes)
            .with_context(|| format!("{label}: failed to write stdin"))?;
    }

    let child_stdout = child
        .stdout
        .take()
        .with_context(|| format!("{label}: child has no stdout pipe"))?;
    let child_stderr = child
        .stderr
        .take()
        .with_context(|| format!("{label}: child has no stderr pipe"))?;

    // Tee each stream line-by-line: write the redacted line to the terminal
    // and append the raw bytes to a buffer. The scoped threads borrow `log`
    // and the buffers without `'static` / `Arc`, and join before the scope
    // returns so both buffers are fully populated for the reconstructed
    // `Output`.
    let mut out_buf: Vec<u8> = Vec::new();
    let mut err_buf: Vec<u8> = Vec::new();
    std::thread::scope(|s| {
        let out_handle = s.spawn(|| tee_stream(child_stdout, log, false));
        let err_handle = s.spawn(|| tee_stream(child_stderr, log, true));
        // join() only errs if the reader thread panicked; an I/O error inside
        // the tee degrades to whatever was captured so far (the wait + status
        // below still drive the success/failure decision).
        out_buf = out_handle.join().unwrap_or_default();
        err_buf = err_handle.join().unwrap_or_default();
    });

    let status = child
        .wait()
        .with_context(|| format!("{label}: failed to wait for child"))?;

    let output = Output {
        status,
        stdout: out_buf,
        stderr: err_buf,
    };
    // The tee already printed both streams live; suppress check_output's own
    // re-emit so nothing prints twice, while keeping the bail! embed.
    log.check_output_streamed(output, label)
}

/// Drain `reader` line-by-line, teeing each redacted line to the terminal
/// (`is_stderr` selects stdout vs stderr) and appending the raw bytes
/// (with the line terminator) to the returned capture buffer.
///
/// Returns whatever was captured even if a mid-stream read errors, so a
/// transient pipe hiccup never loses the bytes already read (the buffer
/// still drives the failure embed).
fn tee_stream<R: std::io::Read>(reader: R, log: &StageLogger, is_stderr: bool) -> Vec<u8> {
    let mut buf = BufReader::new(reader);
    let mut capture: Vec<u8> = Vec::new();
    let mut line: Vec<u8> = Vec::new();
    loop {
        line.clear();
        match buf.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {
                capture.extend_from_slice(&line);
                let text = String::from_utf8_lossy(&line);
                let stripped = text.trim_end_matches(['\n', '\r']);
                if is_stderr {
                    log.stream_child_stderr(stripped);
                } else {
                    log.stream_child_stdout(stripped);
                }
            }
            Err(_) => break,
        }
    }
    capture
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{LogLevel, StageLogger, Verbosity};

    /// `sh -c` wrapper so the tests run a portable shell snippet.
    fn sh(script: &str) -> Command {
        let mut c = Command::new("sh");
        c.arg("-c").arg(script);
        c
    }

    /// Count capture records at a given level (the public `LogCapture`
    /// surface exposes per-level counters for status/warn/error but not for
    /// verbose, so derive it from the message snapshot here).
    fn count_level(cap: &crate::log::LogCapture, level: LogLevel) -> usize {
        cap.all_messages()
            .into_iter()
            .filter(|(lvl, _)| *lvl == level)
            .count()
    }

    #[test]
    fn run_checked_success_is_silent_at_default_verbosity() {
        let (log, cap) = StageLogger::with_capture("test", Verbosity::Normal);
        let out = run_checked(&mut sh("echo hi"), &log, "echo").expect("echo must succeed");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("hi"),
            "captured stdout must contain the child's output"
        );
        // No status / verbose / error lines on the silent success path.
        assert_eq!(
            cap.status_count(),
            0,
            "default-verbosity success must emit no status lines"
        );
        assert_eq!(
            count_level(&cap, LogLevel::Verbose),
            0,
            "default-verbosity success must emit no verbose lines"
        );
        assert_eq!(cap.error_count(), 0, "success must emit no error lines");
    }

    #[test]
    fn run_checked_failure_embeds_child_stderr() {
        let (log, _cap) = StageLogger::with_capture("test", Verbosity::Normal);
        let err = run_checked(&mut sh("echo boom >&2; exit 3"), &log, "boomer")
            .expect_err("non-zero exit must surface as Err");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("boom"),
            "error must embed the child's stderr; got: {chain}"
        );
        assert!(
            chain.contains("exit code: 3"),
            "error must name the exit code; got: {chain}"
        );
    }

    #[test]
    fn run_checked_verbose_emits_stdout_line() {
        let (log, cap) = StageLogger::with_capture("test", Verbosity::Verbose);
        run_checked(&mut sh("echo hi"), &log, "echo").expect("echo must succeed");
        let verbose: Vec<_> = cap
            .all_messages()
            .into_iter()
            .filter(|(lvl, _)| *lvl == LogLevel::Verbose)
            .collect();
        assert!(
            verbose.iter().any(|(_, msg)| msg.contains("hi")),
            "verbose run must record a verbose line containing the child's stdout; got: {verbose:?}"
        );
    }

    #[test]
    fn run_checked_verbose_failure_no_double_emit() {
        let (log, cap) = StageLogger::with_capture("test", Verbosity::Verbose);
        let _ = run_checked(&mut sh("echo BOOMTOKEN >&2; exit 1"), &log, "boomer")
            .expect_err("non-zero exit must surface as Err");
        // The tee streams stderr live (one Verbose record); check_output_streamed
        // must NOT re-emit it as an Error record. Exactly one capture record
        // carries the token across the whole capture.
        let hits = cap
            .all_messages()
            .into_iter()
            .filter(|(_, msg)| msg.contains("BOOMTOKEN"))
            .count();
        assert_eq!(
            hits, 1,
            "verbose failure must surface its stderr exactly once (no double-emit)"
        );
    }

    #[test]
    fn run_checked_with_stdin_roundtrips() {
        let (log, _cap) = StageLogger::with_capture("test", Verbosity::Normal);
        let out = run_checked_with_stdin(&mut Command::new("cat"), b"piped-in\n", &log, "cat")
            .expect("cat must succeed");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim_end(),
            "piped-in",
            "cat must echo the piped stdin back on stdout"
        );
    }

    #[test]
    fn run_checked_with_stdin_verbose_roundtrips() {
        let (log, cap) = StageLogger::with_capture("test", Verbosity::Verbose);
        let out = run_checked_with_stdin(&mut Command::new("cat"), b"streamed-in\n", &log, "cat")
            .expect("cat must succeed");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim_end(),
            "streamed-in",
            "verbose stdin path must also round-trip the piped input"
        );
        let verbose: Vec<_> = cap
            .all_messages()
            .into_iter()
            .filter(|(lvl, _)| *lvl == LogLevel::Verbose)
            .collect();
        assert!(
            verbose.iter().any(|(_, msg)| msg.contains("streamed-in")),
            "verbose stdin run must tee the child's stdout; got: {verbose:?}"
        );
    }
}
