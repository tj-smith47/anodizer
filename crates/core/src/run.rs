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
//!   this process's **stderr** (after secret redaction) AND captured into
//!   in-memory buffers, so a long-running tool (cargo, snapcraft, nix-build,
//!   upx) shows progress as it runs while the failure path keeps the full
//!   captured output for the error embed. The tee deliberately goes to stderr,
//!   never stdout — anodizer's stdout is a machine-readable data channel (GHA
//!   step outputs, JSON payloads) that a teed child stream would corrupt. The
//!   verbose tee is an anodizer-only superset: GoReleaser never streams live.
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
/// (the cosign / gh / kms / email pipe-input pattern).
///
/// The child's stdin is set to a pipe; stdout/stderr capture and the verbose
/// live-stream behave exactly as in [`run_checked`]. The stdin write runs on
/// its own thread *concurrently* with the output readers, so a large stdin
/// paired with a large stdout cannot deadlock (neither side blocks the other).
pub fn run_checked_with_stdin(
    cmd: &mut Command,
    stdin: &[u8],
    log: &StageLogger,
    label: &str,
) -> Result<Output> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_inner(cmd, Some(stdin), log, label)
}

/// Verbose path for [`run_checked`] with no stdin to feed.
fn run_streamed(cmd: &mut Command, log: &StageLogger, label: &str) -> Result<Output> {
    run_inner(cmd, None, log, label)
}

/// Spawn `cmd` and collect its output, draining stdout and stderr
/// concurrently. When `stdin` is `Some`, its bytes are written on a dedicated
/// thread so the writer and the output readers run in parallel — a child that
/// fills its stdout pipe buffer (~64 KiB) while we are still feeding it a large
/// stdin cannot deadlock, because the readers keep draining. At verbose, each
/// output line is also teed live (redacted) to stderr.
///
/// All work happens inside one `std::thread::scope`: the optional stdin writer,
/// the stdout reader, and the stderr reader are scoped threads that borrow
/// `log` / `stdin` without `'static` / `Arc`, and all join before the scope
/// returns. `wait()` runs after the readers hit EOF, so the captured buffers
/// are complete before the success/failure decision.
fn run_inner(
    cmd: &mut Command,
    stdin: Option<&[u8]>,
    log: &StageLogger,
    label: &str,
) -> Result<Output> {
    let verbose = log.is_verbose();
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn {label}"))?;

    let child_stdin = match stdin {
        Some(_) => Some(
            child
                .stdin
                .take()
                .with_context(|| format!("{label}: child has no stdin pipe"))?,
        ),
        None => None,
    };
    let child_stdout = child
        .stdout
        .take()
        .with_context(|| format!("{label}: child has no stdout pipe"))?;
    let child_stderr = child
        .stderr
        .take()
        .with_context(|| format!("{label}: child has no stderr pipe"))?;

    let mut out_buf: Vec<u8> = Vec::new();
    let mut err_buf: Vec<u8> = Vec::new();
    // Carries a non-fatal stdin-write I/O error out of the writer thread.
    let mut stdin_err: Option<std::io::Error> = None;
    std::thread::scope(|s| {
        // Stdin writer (only when there is stdin): own thread so the readers
        // below drain concurrently and a full stdout pipe can't wedge us
        // mid-write. Dropping `pipe` after `write_all` closes stdin → EOF.
        let stdin_handle = child_stdin.map(|mut pipe| {
            let bytes = stdin.expect("child_stdin is Some only when stdin is Some");
            s.spawn(move || -> std::io::Result<()> {
                pipe.write_all(bytes)?;
                Ok(())
            })
        });

        let out_handle = s.spawn(|| tee_stream(child_stdout, log, false, verbose));
        let err_handle = s.spawn(|| tee_stream(child_stderr, log, true, verbose));

        // A reader-thread panic must not vanish the captured stream (it drives
        // the failure embed). Warn loudly and fall back to an empty buffer
        // instead of silently swallowing it.
        out_buf = join_capture(out_handle, log, "stdout");
        err_buf = join_capture(err_handle, log, "stderr");

        if let Some(h) = stdin_handle {
            match h.join() {
                // A broken-pipe write (child exited before reading all stdin)
                // is benign — surface only as the captured error, not a hard
                // fail, since the child's own exit status governs success.
                Ok(Ok(())) => {}
                Ok(Err(e)) => stdin_err = Some(e),
                Err(_) => log.warn(&format!("{label}: stdin writer thread panicked")),
            }
        }
    });

    // A non-broken-pipe stdin error is a real failure to deliver input.
    if let Some(e) = stdin_err
        && e.kind() != std::io::ErrorKind::BrokenPipe
    {
        return Err(anyhow::Error::new(e).context(format!("{label}: failed to write stdin")));
    }

    let status = child
        .wait()
        .with_context(|| format!("{label}: failed to wait for child"))?;

    let output = Output {
        status,
        stdout: out_buf,
        stderr: err_buf,
    };
    if verbose {
        // The tee already printed both streams live; suppress check_output's
        // own re-emit so nothing prints twice, while keeping the bail! embed.
        log.check_output_streamed(output, label)
    } else {
        log.check_output(output, label)
    }
}

/// Join a reader thread, returning its captured buffer. On a thread panic,
/// warn via `log` (naming the `stream`) and return an empty buffer rather than
/// silently dropping the capture — the non-crash policy is kept, but the loss
/// is no longer invisible.
fn join_capture(
    handle: std::thread::ScopedJoinHandle<'_, Vec<u8>>,
    log: &StageLogger,
    stream: &str,
) -> Vec<u8> {
    match handle.join() {
        Ok(buf) => buf,
        Err(_) => {
            log.warn(&format!(
                "internal: {stream} capture thread panicked; output for this step is lost"
            ));
            Vec::new()
        }
    }
}

/// Drain `reader` line-by-line into the returned capture buffer, appending the
/// raw bytes (line terminator included). When `tee` is set, each line is also
/// streamed live (redacted) to stderr — `is_stderr` selects the capture level
/// (stdout→Verbose, stderr→Error). At non-verbose verbosity `tee` is `false`,
/// so the reader still drains the pipe (preventing deadlock) but prints
/// nothing, leaving the captured buffer for `check_output` to surface only on
/// failure.
///
/// Returns whatever was captured even if a mid-stream read errors, so a
/// transient pipe hiccup never loses the bytes already read (the buffer
/// still drives the failure embed).
fn tee_stream<R: std::io::Read>(
    reader: R,
    log: &StageLogger,
    is_stderr: bool,
    tee: bool,
) -> Vec<u8> {
    let mut buf = BufReader::new(reader);
    let mut capture: Vec<u8> = Vec::new();
    let mut line: Vec<u8> = Vec::new();
    loop {
        line.clear();
        match buf.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {
                capture.extend_from_slice(&line);
                if tee {
                    let text = String::from_utf8_lossy(&line);
                    let stripped = text.trim_end_matches(['\n', '\r']);
                    if is_stderr {
                        log.stream_child_stderr(stripped);
                    } else {
                        log.stream_child_stdout(stripped);
                    }
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

    /// Build a >128 KiB stdin payload distinct from the child's own chatter.
    fn big_stdin() -> Vec<u8> {
        // ~192 KiB of `A` plus a trailing newline so `head -c` style readers
        // and `cat` both terminate cleanly.
        let mut v = vec![b'A'; 192 * 1024];
        v.push(b'\n');
        v
    }

    /// A child that reads ALL of a large stdin AND emits a large stdout
    /// concurrently. Before the stdin write moved to its own thread this
    /// deadlocked: the writer blocked filling the child's stdout pipe buffer
    /// (~64 KiB) while the child blocked writing stdout nobody was draining.
    /// The test asserts completion (a hang fails the suite by wall-clock) and
    /// that BOTH streams were captured whole.
    #[test]
    fn run_checked_with_stdin_large_in_and_out_no_deadlock() {
        let (log, _cap) = StageLogger::with_capture("test", Verbosity::Normal);
        let stdin = big_stdin();
        // `cat` echoes the full stdin to stdout; then we emit 100k extra lines
        // so the child's stdout far exceeds one pipe buffer while stdin is
        // still being fed.
        let out = run_checked_with_stdin(
            &mut sh("cat; i=0; while [ $i -lt 100000 ]; do echo line$i; i=$((i+1)); done"),
            &stdin,
            &log,
            "bigcat",
        )
        .expect("large in/out child must complete without hanging");
        // stdout = echoed stdin (192 KiB of A) + the 100k generated lines.
        assert!(
            out.stdout.len() > stdin.len() + 100_000,
            "captured stdout must include the echoed stdin AND the generated lines; \
             got {} bytes",
            out.stdout.len()
        );
        assert!(
            out.stdout.windows(3).any(|w| w == b"AAA"),
            "echoed stdin must be present in captured stdout"
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("line99999"),
            "the last generated stdout line must be captured"
        );
    }

    /// The same large-in/large-out child at VERBOSE: the tee path must also
    /// drain concurrently with the stdin write (no deadlock) and still capture
    /// both streams whole.
    #[test]
    fn run_checked_with_stdin_large_in_and_out_no_deadlock_verbose() {
        let (log, _cap) = StageLogger::with_capture("test", Verbosity::Verbose);
        let stdin = big_stdin();
        let out = run_checked_with_stdin(
            &mut sh("cat; i=0; while [ $i -lt 100000 ]; do echo line$i; i=$((i+1)); done"),
            &stdin,
            &log,
            "bigcat",
        )
        .expect("verbose large in/out child must complete without hanging");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("line99999"),
            "verbose path must capture the full stdout"
        );
    }
}
