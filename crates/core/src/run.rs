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
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};

use crate::log::StageLogger;
use crate::retry::Retriable;

/// Poll cadence for the bounded-wait watchdog. Short enough that a child that
/// exits just after a poll is reaped promptly, long enough not to spin a core.
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Place a to-be-spawned, timeout-bounded child in its OWN process group so the
/// watchdog can kill the WHOLE subtree on expiry — not just the immediate
/// child.
///
/// A bare `Child::kill()` reaps only the direct child; a child that forked a
/// grandchild holding the inherited stdout/stderr pipe (e.g. a `sh -c` wrapper
/// around the real tool, or a relay that double-forks) would keep those pipes
/// open after the parent died, so the reader threads never hit EOF and the run
/// would hang until the grandchild exited on its own. Killing the process group
/// closes every inherited pipe at once. Applied ONLY on the timeout path so the
/// untimed `Command` setup is byte-for-byte unchanged.
#[cfg(unix)]
fn set_own_process_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    // 0 → put the child in a new group whose pgid equals its pid.
    cmd.process_group(0);
}

#[cfg(windows)]
fn set_own_process_group(cmd: &mut Command) {
    use std::os::windows::process::CommandExt as _;
    // CREATE_NEW_PROCESS_GROUP isolates the child from console control events
    // aimed at our own group (a stray Ctrl-C won't race the watchdog). The
    // subtree kill itself is done by `taskkill /T` in `kill_child_tree` — unlike
    // Unix process groups, a Windows group is NOT a kill target for
    // TerminateProcess.
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, windows)))]
fn set_own_process_group(_cmd: &mut Command) {}

/// Kill `child` and its entire process subtree, so a forked grandchild holding
/// an inherited pipe dies too (otherwise the reader threads never EOF and the
/// timeout fails to bound the call). Best-effort: a child that already exited
/// yields a benign error.
fn kill_child_tree(child: &mut Child) {
    #[cfg(unix)]
    {
        // Negative pid targets the process GROUP (pgid == child pid, set at
        // spawn via `set_own_process_group`). SIGKILL the whole group so no
        // descendant survives holding our pipe ends.
        let pid = child.id() as i32;
        // SAFETY: `kill(2)` with a negative pid and SIGKILL has no memory
        // effects; an already-reaped group yields ESRCH, which is ignored.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        // `child.kill()` (TerminateProcess) reaps ONLY the direct child;
        // CREATE_NEW_PROCESS_GROUP does not extend termination to descendants.
        // A forked grandchild (the `sh -c <tool>` wrapper shape) would survive
        // holding the inherited stdout/stderr pipe, leaving the reader threads
        // blocked until it exits on its own — so the timeout would not actually
        // bound the call. `taskkill /T` walks the process tree (by PPID linkage)
        // and terminates every descendant present at snapshot time, closing
        // those pipes. It MUST run before `child.kill()` below: Windows never
        // reparents orphans, so once the parent is killed its PID can be
        // recycled by an unrelated process — the grandchild's now-stale PPID
        // would then point the /T walk at the wrong subtree (or miss it
        // entirely). Keeping the parent alive holds the tree linkage valid for
        // the walk. Resolved by absolute path (System32) so a sanitized PATH
        // can't strip the tool and silently drop us back to the 30s-hang bug.
        // Best-effort — an already-exited tree yields a non-zero status we
        // ignore.
        let taskkill = std::env::var_os("SystemRoot")
            .map(|root| {
                std::path::Path::new(&root)
                    .join("System32")
                    .join("taskkill.exe")
            })
            .unwrap_or_else(|| std::path::PathBuf::from("taskkill.exe"));
        let _ = std::process::Command::new(taskkill)
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    // The direct child kill is the portable fallback (and the only path on a
    // platform without group/tree semantics): it still reaps the immediate
    // child when the subtree kill above was a no-op or unavailable.
    let _ = child.kill();
}

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
    run_inner(cmd, Some(stdin), log, label, None)
}

/// Like [`run_checked_with_stdin`], but bounds the child to `timeout`: if it
/// has not exited within that window the child is **killed** (not merely
/// abandoned) and the call returns a retriable timeout error.
///
/// This is the pipe-input analogue of the bounded SMTP relay timeout. A
/// transport with no wall-clock bound — the canonical case being `sendmail -t`
/// /
/// `msmtp -t` blocking on an unreachable MX — would otherwise hang the caller
/// indefinitely AND leak the child, since the per-stage and aggregate deadlines
/// the announce stage applies live one layer up and cannot reach into a spawned
/// subprocess. Killing on expiry releases both the worker thread and the child.
///
/// The timeout error is wrapped in [`Retriable`] so the announce retry profile
/// treats a transient hang like any other network blip (one bounded retry)
/// rather than fast-failing.
pub fn run_checked_with_stdin_timeout(
    cmd: &mut Command,
    stdin: &[u8],
    log: &StageLogger,
    label: &str,
    timeout: Duration,
) -> Result<Output> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_inner(cmd, Some(stdin), log, label, Some(timeout))
}

/// Verbose path for [`run_checked`] with no stdin to feed.
fn run_streamed(cmd: &mut Command, log: &StageLogger, label: &str) -> Result<Output> {
    run_inner(cmd, None, log, label, None)
}

/// Wait for `child` to exit, killing it if it outlives `timeout`.
///
/// Polls [`Child::try_wait`] on a short cadence so a child that exits on its own
/// is reaped promptly; on the deadline it sends `kill()` (closing the child's
/// stdout/stderr, which lets the concurrent reader threads hit EOF and the
/// surrounding [`std::thread::scope`] unwind). Returns `Ok(Some(status))` when
/// the child exited under its own power, `Ok(None)` when it was killed for
/// exceeding `timeout`, and `Err` only on an OS-level wait failure.
///
/// `child` is shared with the main thread (which performs the final reaping
/// `wait`) through a `Mutex`; the lock is held only for each non-blocking
/// `try_wait` / `kill`, never across a sleep, so the main thread can still
/// acquire it to drain the zombie after a kill.
fn wait_or_kill(child: &Mutex<Child>, timeout: Duration) -> std::io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + timeout;
    loop {
        {
            let mut guard = child.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(status) = guard.try_wait()? {
                return Ok(Some(status));
            }
            if Instant::now() >= deadline {
                // Kill the whole subtree (group), not just the direct child, so
                // a forked grandchild holding the inherited pipe dies too and
                // the readers can EOF. Benign if the child already exited.
                kill_child_tree(&mut guard);
                return Ok(None);
            }
        }
        std::thread::sleep(WAIT_POLL_INTERVAL);
    }
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
///
/// When `timeout` is `Some`, a fourth scoped thread watches the child: if it
/// outlives the deadline it is **killed**, which closes its pipes so the reader
/// threads reach EOF and the scope can unwind instead of blocking forever on a
/// hung child. A killed-for-timeout run returns a retriable timeout error
/// rather than the child's (nonexistent) exit status.
fn run_inner(
    cmd: &mut Command,
    stdin: Option<&[u8]>,
    log: &StageLogger,
    label: &str,
    timeout: Option<Duration>,
) -> Result<Output> {
    let verbose = log.is_verbose();
    // A timeout-bounded child runs in its own process group so the watchdog can
    // kill its whole subtree on expiry (a forked grandchild holding the
    // inherited pipe would otherwise keep the readers blocked past the kill).
    if timeout.is_some() {
        set_own_process_group(cmd);
    }
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

    // Shared with the watchdog (which kills on deadline) and the post-scope
    // reaping wait. The lock is only ever held for a non-blocking try_wait /
    // kill, never across a sleep, so both sides make progress.
    let child = Mutex::new(child);

    let mut out_buf: Vec<u8> = Vec::new();
    let mut err_buf: Vec<u8> = Vec::new();
    // Carries a non-fatal stdin-write I/O error out of the writer thread.
    let mut stdin_err: Option<std::io::Error> = None;
    // Set by the watchdog when it killed the child for exceeding `timeout`.
    let mut timed_out = false;
    // Carries an OS-level wait failure out of the watchdog thread.
    let mut watchdog_err: Option<std::io::Error> = None;
    // A shared reference (Copy) the watchdog can move without taking the Mutex
    // itself, leaving `child` available for the post-scope reaping wait.
    let child_ref = &child;
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

        // Bounded-wait watchdog: kills the child if it outlives `timeout`,
        // unblocking the readers (killed pipes → EOF) so this scope can exit.
        let watchdog = timeout.map(|t| s.spawn(move || wait_or_kill(child_ref, t)));

        // A reader-thread panic must not vanish the captured stream (it drives
        // the failure embed). Warn loudly and fall back to an empty buffer
        // instead of silently swallowing it.
        out_buf = join_capture(out_handle, log, "stdout");
        err_buf = join_capture(err_handle, log, "stderr");

        if let Some(h) = watchdog {
            match h.join() {
                Ok(Ok(Some(_status))) => {} // child exited on its own
                Ok(Ok(None)) => timed_out = true,
                Ok(Err(e)) => watchdog_err = Some(e),
                Err(_) => log.warn(&format!("{label}: timeout watchdog thread panicked")),
            }
        }

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

    // Always reap the (now-exited-or-killed) child so no zombie leaks, even on
    // the timeout path. Done after the scope so the watchdog has released the
    // lock.
    let reaped = {
        let mut guard = child.lock().unwrap_or_else(|p| p.into_inner());
        guard.wait()
    };

    if let Some(e) = watchdog_err {
        return Err(anyhow::Error::new(e).context(format!("{label}: failed to wait for child")));
    }

    // Timeout takes precedence over a stdin write error (the latter is the
    // symptom — the child stopped reading because it hung). Surface a retriable
    // timeout so the announce retry profile treats it like a transient blip.
    if timed_out {
        let secs = timeout.map(|t| t.as_secs_f64()).unwrap_or_default();
        return Err(anyhow::Error::new(Retriable::new(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("{label}: child did not exit within {secs:.0}s; killed"),
        ))));
    }

    // A non-broken-pipe stdin error is a real failure to deliver input.
    if let Some(e) = stdin_err
        && e.kind() != std::io::ErrorKind::BrokenPipe
    {
        return Err(anyhow::Error::new(e).context(format!("{label}: failed to write stdin")));
    }

    let status = reaped.with_context(|| format!("{label}: failed to wait for child"))?;

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

    /// A child that ignores its stdin and sleeps far past the timeout (the
    /// `sendmail -t blocked on an unreachable MX` shape) must be KILLED at the
    /// deadline, not awaited: the call returns promptly with a retriable
    /// timeout error and the child does not outlive it.
    #[test]
    fn run_checked_with_stdin_timeout_kills_hung_child() {
        let (log, _cap) = StageLogger::with_capture("test", Verbosity::Normal);
        let start = Instant::now();
        let err = run_checked_with_stdin_timeout(
            // Reads nothing, holds its stdout pipe open, sleeps 30s — a hang.
            &mut sh("sleep 30"),
            b"ignored stdin\n",
            &log,
            "hung",
            Duration::from_millis(200),
        )
        .expect_err("a child outliving the timeout must surface as Err");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "timeout must return promptly (killed the child), took {elapsed:?}"
        );
        // The timeout error is retriable so the announce retry profile treats
        // it like a transient blip.
        assert!(
            err.downcast_ref::<crate::retry::Retriable>().is_some(),
            "timeout error must be Retriable; got: {err:#}"
        );
        let chain = format!("{err:#}");
        assert!(
            chain.contains("did not exit") && chain.contains("killed"),
            "timeout error must name the kill; got: {chain}"
        );
    }

    /// A child that completes well within its timeout takes the normal success
    /// path: the timeout never fires and the captured stdout round-trips.
    #[test]
    fn run_checked_with_stdin_timeout_fast_child_succeeds() {
        let (log, _cap) = StageLogger::with_capture("test", Verbosity::Normal);
        let out = run_checked_with_stdin_timeout(
            &mut Command::new("cat"),
            b"within-deadline\n",
            &log,
            "cat",
            Duration::from_secs(30),
        )
        .expect("a fast child must succeed under a generous timeout");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim_end(),
            "within-deadline",
            "the fast-path timeout call must still round-trip stdin to stdout"
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
