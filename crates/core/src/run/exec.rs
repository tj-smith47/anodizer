//! The `run_*` exec API: spawn an already-built [`Command`], drain and
//! optionally tee its streams, bound it with a watchdog, and apply the
//! success/failure decision.
//!
//! Subtree isolation and reaping live in [`super::process_tree`]; this module
//! only marks a timeout-bounded child for group isolation, registers the
//! spawned tree, and asks for a reap when a deadline expires.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};

use crate::log::StageLogger;
use crate::retry::Retriable;

#[cfg(windows)]
use super::process_tree::windows_job;
use super::process_tree::{
    ChildTree, TreeRegistration, kill_child_tree, register_child_tree, set_own_process_group,
};

/// Poll cadence for the bounded-wait watchdog. Short enough that a child that
/// exits just after a poll is reaped promptly, long enough not to spin a core.
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Grace window granted to the reader threads to hit EOF AFTER the direct child
/// has exited. The common case completes in microseconds: the child's pipe ends
/// close on exit, the readers drain the last buffered bytes and EOF. A grace is
/// only ever consumed when a forked grandchild inherited and still holds the
/// pipe write-end (snapcraft → snapd, a backgrounded uploader): once it elapses
/// the watchdog reaps the whole process group so the leaked grandchild releases
/// the pipe and the readers EOF, instead of the drain hanging for the
/// grandchild's full lifetime and blowing past the deadline.
const POST_EXIT_DRAIN_GRACE: Duration = Duration::from_secs(3);

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
/// The child's stdin is detached (`Stdio::null`) inside the capture loop, so a
/// tool that would otherwise prompt on the inherited tty (a stray `cargo login`,
/// a git credential helper) fails fast instead of blocking on input that never
/// arrives. Use [`run_checked_with_stdin`] to feed bytes to the child's stdin.
pub fn run_checked(cmd: &mut Command, log: &StageLogger, label: &str) -> Result<Output> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    // Route BOTH verbosities through the shared `capture_inner` loop (via
    // `run_inner`), which runs the liveness heartbeat ticker: a slow silent
    // child (`cargo publish`, a registry push) is now narrated at DEFAULT
    // verbosity too, not only under `-v`. Matches the former non-verbose
    // `cmd.output()` fast-path byte-for-byte on capture and `check_output`
    // decision; the single deliberate drift is that the verbose path's stdin is
    // now null instead of inherited (see the stdin note in `capture_inner`), so
    // stdin semantics no longer vary by verbosity. A single code path is worth
    // the two reader-thread spawns per call — every production caller is a
    // heavyweight subprocess (cargo, docker, notarytool) where thread setup is
    // noise, and the deleted dual path is exactly where the stdin drift hid.
    run_inner(cmd, None, log, label, None)
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

/// Like [`run_checked`] (no stdin; a non-zero exit becomes an `Err`) but bounds
/// the child to `timeout`: if it has not exited within that window the whole
/// process subtree is **killed** and the call returns a [`Retriable`]-wrapped
/// timeout error. Use this for network-touching subprocesses — registry pushes,
/// `git push` over ssh, `gh` PR submission — whose remote side can stall a
/// connection indefinitely and would otherwise hang the entire release.
pub fn run_checked_timeout(
    cmd: &mut Command,
    log: &StageLogger,
    label: &str,
    timeout: Duration,
) -> Result<Output> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    run_inner(cmd, None, log, label, Some(timeout))
}

/// Wait for `child` to exit, killing it if it outlives `timeout`, and bound the
/// post-exit reader drain so a leaked grandchild can't hang past the deadline.
///
/// Polls [`Child::try_wait`] on a short cadence. Two deadline edges:
/// - **Child runtime** — if the direct child has not exited by `timeout`, the
///   whole subtree is killed and `Ok(None)` is returned (a true timeout).
/// - **Drain** — once the direct child HAS exited, the reader threads must hit
///   EOF for the surrounding [`std::thread::scope`] to unwind. They do so
///   immediately in the common case (the child's pipe ends closed on exit), but
///   a forked grandchild that inherited the pipe write-end keeps them blocked.
///   `readers_done` (incremented by each reader as it returns) is watched: when
///   it reaches `reader_count` the call returns the child's real status
///   promptly; if the readers are still blocked [`POST_EXIT_DRAIN_GRACE`] after
///   the child exited, the whole process group is reaped so the leaked
///   grandchild releases the pipe — and the child's real (success) status is
///   STILL returned, because the child itself succeeded; only a leaked
///   descendant was force-closed. Rewriting that into a timeout would re-publish
///   a succeeded one-way-door publisher on retry.
///
/// `child` is shared with the main thread (which performs the final reaping
/// `wait`) through a `Mutex`; the lock is held only for each non-blocking
/// `try_wait` / `kill`, never across a sleep, so the main thread can still
/// acquire it to drain the zombie after a kill.
fn wait_or_kill(
    child: &Mutex<Child>,
    readers_done: &AtomicUsize,
    reader_count: usize,
    timeout: Duration,
    tree: ChildTree,
) -> std::io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + timeout;
    let mut exited: Option<ExitStatus> = None;
    let mut drain_deadline: Option<Instant> = None;
    loop {
        if exited.is_none() {
            let mut guard = child.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(status) = guard.try_wait()? {
                exited = Some(status);
                drain_deadline = Some(Instant::now() + POST_EXIT_DRAIN_GRACE);
            } else if Instant::now() >= deadline {
                // Child itself outlived the timeout: reap the whole subtree (not
                // just the direct child) so a forked grandchild holding the
                // inherited pipe dies too and the readers can EOF.
                kill_child_tree(&mut guard, tree);
                return Ok(None);
            }
        }

        if let Some(status) = exited {
            // Child is done. Let the readers finish draining; return promptly
            // once they EOF.
            if readers_done.load(Ordering::Acquire) >= reader_count {
                return Ok(Some(status));
            }
            // Readers still blocked past the drain grace ⇒ a leaked grandchild
            // is holding the pipe. Reap the subtree to force EOF (on Windows via
            // the Job Object, which works even though the direct child has
            // already exited), but report the child's real (success) status — it
            // crossed its door; only the orphan was force-closed.
            if drain_deadline.is_some_and(|d| Instant::now() >= d) {
                let mut guard = child.lock().unwrap_or_else(|p| p.into_inner());
                kill_child_tree(&mut guard, tree);
                return Ok(Some(status));
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
///
/// Returns the raw captured [`Output`] regardless of exit status; the
/// success/failure decision (`check_output`) is left to the caller — `run_inner`
/// applies it, [`run_capture_timeout`] does not.
fn capture_inner(
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
    // With no stdin payload, detach the child's stdin — deliberately for EVERY
    // no-payload path, not just the former non-verbose `cmd.output()` sites
    // (which nulled stdin implicitly). The verbose and `*_timeout` paths used
    // to inherit the parent's stdin; that let a tool that reads fd 0 (a stray
    // `cargo login`, a git credential helper honoring piped input) block
    // forever on input that never arrives, and made stdin semantics differ by
    // verbosity. Tools that genuinely prompt interactively (gpg pinentry, ssh
    // passphrases) read `/dev/tty`, not fd 0, so they are unaffected. When
    // `stdin` is Some the caller already set `Stdio::piped()`.
    if stdin.is_none() {
        cmd.stdin(Stdio::null());
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn {label}"))?;

    // Windows: enclose the timeout-bounded child (and every process it spawns)
    // in a kill-on-close Job Object so the watchdog can reap the WHOLE subtree
    // via `TerminateJobObject` even after the direct child has exited — the
    // post-exit drain-reap case `taskkill /T` cannot serve (a terminated root is
    // absent from the snapshot its tree walk needs). Assigned immediately after
    // spawn; a grandchild forked in the microseconds before assignment escapes
    // the job, but the bounded tools do real work before forking. `None` (job
    // creation/assignment failed) falls back to the `taskkill` reap.
    #[cfg(windows)]
    let job = if timeout.is_some() {
        windows_job::enclose_child(&child)
    } else {
        None
    };

    // The per-platform reap target shared by the timeout watchdog and the
    // external-termination watcher (Unix pgid; Windows pid + Job Object handle).
    let tree = ChildTree {
        pid: child.id() as i32,
        #[cfg(windows)]
        job,
    };

    // Register the timeout-bounded child tree so an external SIGTERM/SIGINT
    // (CI cancel, runner job-timeout) reaches its whole subtree before anodizer
    // dies — otherwise a hung snapcraft/docker tree is orphaned and holds the
    // runner open. Only the timeout path has a reapable tree (the Unix process
    // group / Windows Job Object), so only it registers. The RAII guard
    // deregisters (and, on Windows, closes the job handle) on every exit edge
    // below — the pipe-take `?`s, the watchdog/stdin error returns, success, and
    // an unwinding panic — so a recycled pid can never be reaped by a later
    // external termination.
    let _registration = timeout.is_some().then(|| {
        register_child_tree(tree);
        TreeRegistration(tree)
    });

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

    // Shared with the watchdog (which reaps on the runtime deadline or at the
    // post-exit drain grace) and the post-scope reaping wait. Never held across a
    // sleep, so both sides keep making progress. The lock IS briefly held across
    // the reap: both kill edges in `wait_or_kill` call `kill_child_tree` under
    // this guard. On Windows that reap is `TerminateJobObject` — a fast,
    // non-blocking syscall — except in the rare fallback where the child could
    // not be assigned to a job, which spawns a blocking `taskkill`. Either way no
    // contender can be waiting: the only other acquirer is the main thread, and
    // whenever the watchdog reaches a reap the main thread is parked in
    // `join_capture` draining the readers, never reaching for this lock. (Unix
    // reaps are an async-signal-safe `libc::kill`, which never blocks.)
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
    // Counts the stdout + stderr reader threads that have reached EOF and
    // returned. The watchdog watches this so that, once the direct child has
    // exited, it can tell "readers drained, return promptly" from "readers still
    // blocked on a leaked grandchild's pipe, reap the group at the drain grace".
    let readers_done = AtomicUsize::new(0);
    let readers_done_ref = &readers_done;
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

        let out_handle = s.spawn(move || {
            let buf = tee_stream(child_stdout, log, false, verbose);
            readers_done_ref.fetch_add(1, Ordering::Release);
            buf
        });
        let err_handle = s.spawn(move || {
            let buf = tee_stream(child_stderr, log, true, verbose);
            readers_done_ref.fetch_add(1, Ordering::Release);
            buf
        });

        // Bounded-wait watchdog: kills the child (and, at the drain grace, a
        // leaked grandchild holding the inherited pipe) so the readers EOF and
        // this scope can exit. `reader_count` = 2 (stdout + stderr always piped).
        let watchdog =
            timeout.map(|t| s.spawn(move || wait_or_kill(child_ref, readers_done_ref, 2, t, tree)));

        // Heartbeat ticker: at default verbosity the tee prints nothing, so a
        // legitimately slow child (cargo publish, a large asset upload,
        // notarytool polling Apple) is indistinguishable from a hang. This
        // thread emits `still running <label> (<elapsed>)` every cadence until
        // the `stop` channel disconnects. The channel's Sender is held below and
        // dropped on EVERY scope exit edge — normal return, the watchdog/stdin
        // error `if let`s, AND a panic unwind — so `recv_timeout` returns
        // `Disconnected` and the ticker terminates promptly with no risk of
        // `thread::scope` hanging on a ticker that never stops. Off entirely
        // outside Normal verbosity (see `heartbeat_period`). The ticker
        // deliberately outlives the child's own exit into the pipe-drain window:
        // a drain that spans a cadence means output is still flowing (a leaked
        // grandchild, a large buffered tail), which IS the pipeline still
        // running from the operator's seat.
        let heartbeat_stop = crate::progress::heartbeat_period(log).map(|interval| {
            let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
            let start = Instant::now();
            let action = format!("running {label}");
            s.spawn(move || {
                crate::progress::run_ticker(&stop_rx, interval, || {
                    log.heartbeat(&crate::progress::heartbeat_message(&action, start));
                });
            });
            stop_tx
        });

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

        // Child has exited and its streams are drained: drop the heartbeat
        // Sender so its ticker observes `Disconnected` and stops, letting the
        // scope join return promptly instead of waiting out a full cadence.
        drop(heartbeat_stop);
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

    Ok(Output {
        status,
        stdout: out_buf,
        stderr: err_buf,
    })
}

/// Spawn `cmd` through [`capture_inner`] and apply the success/failure decision
/// via `check_output` — the shared core behind [`run_checked`],
/// [`run_checked_with_stdin`], and their timeout variants. A non-zero exit
/// becomes an `Err`; callers that must inspect a non-zero `Output` themselves
/// use [`run_capture_timeout`] instead.
fn run_inner(
    cmd: &mut Command,
    stdin: Option<&[u8]>,
    log: &StageLogger,
    label: &str,
    timeout: Option<Duration>,
) -> Result<Output> {
    let output = capture_inner(cmd, stdin, log, label, timeout)?;
    if log.is_verbose() {
        // The tee already printed both streams live; suppress check_output's
        // own re-emit so nothing prints twice, while keeping the bail! embed.
        log.check_output_streamed(output, label)
    } else {
        log.check_output(output, label)
    }
}

/// Bound `cmd` to `timeout` and return its raw captured [`Output`] **without**
/// treating a non-zero exit as an error: the caller inspects
/// `status`/`stdout`/`stderr` itself. The Snap Store publish path needs this —
/// a non-zero `snapcraft upload` may be a review-pending success or a retriable
/// 5xx that must be classified from the body, not pre-converted to a hard fail.
///
/// The child runs in its own process group; if it outlives `timeout` the whole
/// subtree is killed and a [`Retriable`]-wrapped timeout error is returned, so a
/// transient store/network stall retries within budget instead of hanging the
/// release indefinitely. Errors only on spawn failure, an OS-level wait
/// failure, or the deadline kill.
pub fn run_capture_timeout(
    cmd: &mut Command,
    log: &StageLogger,
    label: &str,
    timeout: Duration,
) -> Result<Output> {
    capture_impl(cmd, log, label, Some(timeout))
}

/// Capture `cmd`'s raw [`Output`] (both streams buffered) WITHOUT treating a
/// non-zero exit as an error — the caller classifies the result — and WITHOUT a
/// wall-clock timeout. The unbounded sibling of [`run_capture_timeout`]: for a
/// subprocess a caller must inspect (exit code + stdout/stderr) that has no
/// natural deadline yet can run long enough to look hung — notarytool polling
/// Apple can take many minutes — so routing it through the shared capture loop
/// earns the liveness heartbeat for free. Errors only on spawn / wait failure.
///
/// Verbose streaming, secret redaction, and heartbeat behavior are identical to
/// every other `run_*` entry point, since all share [`capture_inner`].
pub fn run_capture(cmd: &mut Command, log: &StageLogger, label: &str) -> Result<Output> {
    capture_impl(cmd, log, label, None)
}

/// Shared body of [`run_capture`] / [`run_capture_timeout`]: the piped-stdio
/// setup lives once so a future stdio default cannot diverge between the
/// bounded and unbounded entry points.
fn capture_impl(
    cmd: &mut Command,
    log: &StageLogger,
    label: &str,
    timeout: Option<Duration>,
) -> Result<Output> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    capture_inner(cmd, None, log, label, timeout)
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
