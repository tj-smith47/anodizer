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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};

use crate::log::StageLogger;
use crate::retry::Retriable;

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

/// Kill the process subtree rooted at `pid` (its own process group on Unix,
/// the `/T` tree walk on Windows), so a forked grandchild holding an inherited
/// pipe dies too. Signal-safe ONLY on Unix (a bare `libc::kill`); the Windows
/// arm spawns `taskkill` and must therefore never run from a signal/console
/// handler — only from a normal watcher thread. Best-effort: an already-reaped
/// tree yields a benign error.
///
/// `signal` selects the disposition: the timeout watchdog passes `SIGKILL`
/// (unconditional reap); the external-termination watcher passes `SIGTERM` so a
/// well-behaved child (snapcraft, docker, git) gets a chance to clean up before
/// anodizer itself re-raises and dies. The Windows arm is always `/F` (no
/// graceful equivalent for an opaque subtree).
fn kill_process_tree(pid: i32, signal: i32) {
    #[cfg(unix)]
    {
        // Negative pid targets the process GROUP (pgid == child pid, set at
        // spawn via `set_own_process_group`). Signal the whole group so no
        // descendant survives holding our pipe ends.
        //
        // SAFETY: `kill(2)` with a negative pid is async-signal-safe and has no
        // memory effects; an already-reaped group yields ESRCH, which is
        // ignored. Callable from a signal handler.
        unsafe {
            libc::kill(-pid, signal);
        }
    }
    #[cfg(windows)]
    {
        let _ = signal; // no graceful disposition for an opaque Windows subtree
        // `child.kill()` (TerminateProcess) reaps ONLY the direct child;
        // CREATE_NEW_PROCESS_GROUP does not extend termination to descendants.
        // A forked grandchild (the `sh -c <tool>` wrapper shape) would survive
        // holding the inherited stdout/stderr pipe, leaving the reader threads
        // blocked until it exits on its own — so the timeout would not actually
        // bound the call. `taskkill /T` walks the process tree (by PPID linkage)
        // and terminates every descendant present at snapshot time, closing
        // those pipes. Resolved by absolute path (System32) so a sanitized PATH
        // can't strip the tool and silently drop us back to the 30s-hang bug.
        // Best-effort — an already-exited tree yields a non-zero status we
        // ignore. NOT signal-safe (spawns a subprocess); only the watcher
        // thread calls this, never a console handler.
        let taskkill = std::env::var_os("SystemRoot")
            .map(|root| {
                std::path::Path::new(&root)
                    .join("System32")
                    .join("taskkill.exe")
            })
            .unwrap_or_else(|| std::path::PathBuf::from("taskkill.exe"));
        let _ = std::process::Command::new(taskkill)
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Kill `child` and its entire process subtree, so a forked grandchild holding
/// an inherited pipe dies too (otherwise the reader threads never EOF and the
/// timeout fails to bound the call). The timeout path is unconditional, so it
/// uses `SIGKILL`. Best-effort: a child that already exited yields a benign
/// error.
fn kill_child_tree(child: &mut Child) {
    #[cfg(unix)]
    kill_process_tree(child.id() as i32, libc::SIGKILL);
    #[cfg(windows)]
    {
        // The /T tree walk MUST run before `child.kill()` below: Windows never
        // reparents orphans, so once the parent is killed its PID can be
        // recycled by an unrelated process — the grandchild's now-stale PPID
        // would then point the /T walk at the wrong subtree (or miss it
        // entirely). Keeping the parent alive holds the tree linkage valid.
        kill_process_tree(child.id() as i32, 0);
    }
    // The direct child kill is the portable fallback (and the only path on a
    // platform without group/tree semantics): it still reaps the immediate
    // child when the subtree kill above was a no-op or unavailable.
    let _ = child.kill();
}

/// Process-global registry of live, group-isolated child subtrees, keyed by
/// the value that targets the whole tree on each platform (Unix: the child's
/// pgid, equal to its pid; Windows: the child's pid for the `taskkill /T`
/// walk). Populated only for children spawned in their own process group (the
/// timeout-bounded path — the long-running snapcraft / docker / git subtrees
/// that survive a cancel), so the external-termination watcher can group-kill
/// every one of them before anodizer itself dies.
///
/// A plain `Mutex` is safe here because it is locked ONLY from normal threads
/// (`capture_inner` on spawn/reap, the watcher thread on signal) — never from
/// the async-signal-safe handler, which touches only the self-pipe.
static LIVE_CHILD_TREES: OnceLock<Mutex<std::collections::HashSet<i32>>> = OnceLock::new();

fn live_child_trees() -> &'static Mutex<std::collections::HashSet<i32>> {
    LIVE_CHILD_TREES.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// Record a spawned, group-isolated child tree so the external-termination
/// watcher can reach it. Paired with [`deregister_child_tree`] on reap.
fn register_child_tree(id: i32) {
    live_child_trees()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(id);
}

/// Drop a reaped child tree from the registry so a recycled pid is never
/// signalled by a later termination.
fn deregister_child_tree(id: i32) {
    live_child_trees()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&id);
}

/// RAII guard that deregisters a registered child tree on every exit edge of
/// `capture_inner` — the pipe-take `?`s, the watchdog/stdin error returns,
/// the success return, and an unwinding panic. A manual deregister could only
/// cover the edges before it and would leak the pid past any earlier `?` or
/// `thread::scope` panic, after which an OS pid-recycle would let an external
/// termination signal an unrelated process group.
struct TreeRegistration(i32);

impl Drop for TreeRegistration {
    fn drop(&mut self) {
        deregister_child_tree(self.0);
    }
}

/// SIGTERM every registered child subtree. Run by the watcher thread (NOT a
/// signal handler), so locking the registry and issuing the kills is safe.
/// Returns the number of trees signalled. On Windows the trees are killed via
/// `taskkill /T /F`; there is no graceful disposition for an opaque subtree.
fn terminate_all_child_trees() -> usize {
    let ids: Vec<i32> = {
        let guard = live_child_trees().lock().unwrap_or_else(|p| p.into_inner());
        guard.iter().copied().collect()
    };
    for id in &ids {
        #[cfg(unix)]
        kill_process_tree(*id, libc::SIGTERM);
        #[cfg(windows)]
        kill_process_tree(*id, 0);
    }
    ids.len()
}

/// Install a one-shot handler so an EXTERNAL SIGTERM/SIGINT (a GitHub Actions
/// job cancel, a runner job-timeout, an operator `Ctrl-C`) propagates to every
/// group-isolated child subtree before anodizer exits — instead of orphaning a
/// hung snapcraft/docker subtree that then holds the CI runner open long after
/// anodizer is gone.
///
/// Idempotent and infallible from the caller's view: call once, early, before
/// the pipeline runs. A second call (or a platform without the primitive) is a
/// silent no-op. On the unsupported-platform fallback the process keeps its
/// default signal disposition (terminate), so behavior is unchanged there.
///
/// # Mechanism (async-signal-safety)
///
/// Unix uses the classic **self-pipe**: the installed `sigaction` handler does
/// nothing but `write(2)` a single byte to a pipe — the one syscall guaranteed
/// async-signal-safe — and a normal watcher thread blocked on `read(2)` does
/// the actual work (lock the registry, group-`SIGTERM` each child tree, then
/// reset the signal to its default disposition and re-raise so anodizer dies
/// WITH the right signal exit code, AFTER its children got the signal). The
/// handler never locks, allocates, or logs.
pub fn install_termination_handler() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    if INSTALLED.set(()).is_err() {
        return; // already installed
    }
    #[cfg(unix)]
    unix_termination::install();
    #[cfg(windows)]
    windows_termination::install();
}

#[cfg(unix)]
mod unix_termination {
    use super::terminate_all_child_trees;
    use std::os::unix::io::RawFd;
    use std::sync::atomic::{AtomicI32, Ordering};

    /// Write end of the self-pipe, set BEFORE the handler is armed so a signal
    /// can never observe an uninitialized fd. The handler reads it relaxed and
    /// writes one byte; that is the only work it does.
    static WAKE_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

    /// Carries which signal fired from the handler to the watcher (for the
    /// re-raise), so anodizer exits with the same signal that hit it.
    static FIRED_SIGNAL: AtomicI32 = AtomicI32::new(0);

    /// The `sigaction` handler: async-signal-safe by construction — it records
    /// the signal number and writes ONE byte to the self-pipe, nothing else.
    /// No lock, no allocation, no logging.
    extern "C" fn on_signal(sig: libc::c_int) {
        FIRED_SIGNAL.store(sig, Ordering::SeqCst);
        let fd = WAKE_WRITE_FD.load(Ordering::SeqCst);
        if fd >= 0 {
            let byte: u8 = 1;
            // SAFETY: `write(2)` is async-signal-safe; a single-byte write to a
            // valid pipe fd has no memory effects. A short/failed write (EINTR,
            // full pipe) is ignored — one queued byte already wakes the watcher.
            unsafe {
                let _ = libc::write(fd, &byte as *const u8 as *const libc::c_void, 1);
            }
        }
    }

    pub fn install() {
        let mut fds: [RawFd; 2] = [-1, -1];
        // SAFETY: `pipe(2)` fills the two-element array with valid fds or
        // returns non-zero; on failure the handler is never armed.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return;
        }
        let (read_fd, write_fd) = (fds[0], fds[1]);
        // Publish the write fd BEFORE arming the handler so no early signal can
        // race a -1 fd.
        WAKE_WRITE_FD.store(write_fd, Ordering::SeqCst);

        // SAFETY: zeroed `sigaction` is a valid empty struct; we then set the
        // handler and an empty mask. `sigaction(2)` itself is the documented
        // installation API.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_signal as *const () as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            sa.sa_flags = 0;
            libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
            libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
        }

        std::thread::Builder::new()
            .name("anodizer-sigwatch".into())
            .spawn(move || watcher(read_fd))
            .ok();
    }

    /// Normal watcher thread: blocks on the self-pipe, then group-`SIGTERM`s
    /// every live child tree and re-raises the original signal so anodizer dies
    /// WITH its children (correct signal exit code), not before them.
    fn watcher(read_fd: RawFd) -> ! {
        let mut byte = [0u8; 1];
        // SAFETY: a blocking `read(2)` of one byte from the read end of our own
        // pipe; the buffer outlives the call. EINTR is treated as "woken".
        loop {
            let n = unsafe { libc::read(read_fd, byte.as_mut_ptr() as *mut libc::c_void, 1) };
            if n != 0 {
                break; // a byte (signal) arrived, or EINTR — either way, act
            }
        }

        terminate_all_child_trees();

        let sig = FIRED_SIGNAL.load(Ordering::SeqCst);
        let sig = if sig == 0 { libc::SIGTERM } else { sig };
        // Reset to default disposition and re-raise so the process terminates
        // with the SAME signal that hit it (right exit code for CI), now that
        // its children already received SIGTERM.
        // SAFETY: restoring SIG_DFL and `raise`ing are async-signal-safe and
        // have no memory effects.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = libc::SIG_DFL;
            libc::sigemptyset(&mut sa.sa_mask);
            sa.sa_flags = 0;
            libc::sigaction(sig, &sa, std::ptr::null_mut());
            libc::raise(sig);
        }
        // `raise` of a default-disposition terminating signal does not return;
        // the explicit exit is an unreachable belt-and-suspenders.
        std::process::exit(128 + sig);
    }
}

#[cfg(windows)]
mod windows_termination {
    use super::terminate_all_child_trees;
    use std::sync::atomic::{AtomicBool, Ordering};

    type Bool = i32;
    type Dword = u32;

    const TRUE: Bool = 1;
    const CTRL_C_EVENT: Dword = 0;
    const CTRL_BREAK_EVENT: Dword = 1;
    const CTRL_CLOSE_EVENT: Dword = 2;
    const CTRL_LOGOFF_EVENT: Dword = 5;
    const CTRL_SHUTDOWN_EVENT: Dword = 6;

    static FIRED: AtomicBool = AtomicBool::new(false);

    unsafe extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<HandlerRoutine>, add: Bool) -> Bool;
    }

    type HandlerRoutine = unsafe extern "system" fn(ctrl_type: Dword) -> Bool;

    /// Console control handler: Windows runs it on a dedicated thread (NOT a
    /// Unix-style async-signal context), so locking the registry and spawning
    /// `taskkill /T /F` from here is safe. Kills every live child tree, then
    /// returns FALSE so the default handler runs and terminates anodizer —
    /// children gone first, anodizer second.
    unsafe extern "system" fn on_ctrl(ctrl_type: Dword) -> Bool {
        match ctrl_type {
            CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT
            | CTRL_SHUTDOWN_EVENT => {
                FIRED.store(true, Ordering::SeqCst);
                terminate_all_child_trees();
                // FALSE → fall through to the default handler, which terminates
                // the process now that its child trees are killed.
                0
            }
            _ => 0,
        }
    }

    pub fn install() {
        // SAFETY: registering a console control handler; the function pointer
        // is a valid `extern "system"` routine for the lifetime of the process.
        unsafe {
            SetConsoleCtrlHandler(Some(on_ctrl), TRUE);
        }
    }
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
                // Child itself outlived the timeout: kill the whole subtree (not
                // just the direct child) so a forked grandchild holding the
                // inherited pipe dies too and the readers can EOF.
                kill_child_tree(&mut guard);
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
            // is holding the pipe. Reap the group to force EOF, but report the
            // child's real (success) status — it crossed its door; only the
            // orphan was force-closed.
            if drain_deadline.is_some_and(|d| Instant::now() >= d) {
                let mut guard = child.lock().unwrap_or_else(|p| p.into_inner());
                kill_child_tree(&mut guard);
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
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn {label}"))?;

    // Register the group-isolated child tree so an external SIGTERM/SIGINT
    // (CI cancel, runner job-timeout) reaches its whole subtree before anodizer
    // dies — otherwise a hung snapcraft/docker tree is orphaned and holds the
    // runner open. Only the timeout path isolates the child into its own group,
    // so only it has a tree the watcher can target. The RAII guard deregisters
    // on every exit edge below (the pipe-take `?`s, the watchdog/stdin error
    // returns, success, and an unwinding panic), so a recycled pid can never be
    // signalled by a later external termination.
    let _registration = timeout.is_some().then(|| {
        let id = child.id() as i32;
        register_child_tree(id);
        TreeRegistration(id)
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
            timeout.map(|t| s.spawn(move || wait_or_kill(child_ref, readers_done_ref, 2, t)));

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
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    capture_inner(cmd, None, log, label, Some(timeout))
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
    // Serialized against the watcher broadcast test: that test SIGTERMs every
    // registered tree, and the timeout path registers this child.
    #[serial_test::serial(child_tree_registry)]
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
    #[serial_test::serial(child_tree_registry)]
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

    /// `run_capture_timeout` must hand back a NON-zero exit as `Ok(Output)` —
    /// not pre-convert it to an `Err` the way `run_checked` does. The snapcraft
    /// publish path relies on this to classify a failed `snapcraft upload` as a
    /// review-pending success vs. a retriable 5xx from the captured body.
    #[serial_test::serial(child_tree_registry)]
    #[test]
    fn run_capture_timeout_returns_nonzero_exit_as_ok_output() {
        let (log, _cap) = StageLogger::with_capture("test", Verbosity::Normal);
        let out = run_capture_timeout(
            &mut sh("echo to-stdout; echo to-stderr >&2; exit 7"),
            &log,
            "classify-me",
            Duration::from_secs(30),
        )
        .expect("a non-zero exit must be Ok(Output), not Err");
        assert_eq!(
            out.status.code(),
            Some(7),
            "the caller must see the real non-zero exit code"
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("to-stdout"),
            "stdout must be captured for body classification"
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("to-stderr"),
            "stderr must be captured for body classification"
        );
    }

    /// A hung child (the Snap Store-stall analogue) must be killed at the
    /// deadline and surface a retriable timeout — never block forever. This is
    /// the regression guard for the publish-stage hang.
    #[serial_test::serial(child_tree_registry)]
    #[test]
    fn run_capture_timeout_kills_hung_child() {
        let (log, _cap) = StageLogger::with_capture("test", Verbosity::Normal);
        let start = Instant::now();
        let err = run_capture_timeout(
            &mut sh("sleep 30"),
            &log,
            "hung-upload",
            Duration::from_millis(200),
        )
        .expect_err("a child outliving the timeout must surface as Err");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "timeout must return promptly (killed the child), took {elapsed:?}"
        );
        assert!(
            crate::retry::is_retriable(err.as_ref()),
            "a deadline kill must classify as retriable so the upload retries within budget; got: {err:#}"
        );
    }

    /// A child that exits cleanly but leaves a backgrounded grandchild holding
    /// the inherited stdout/stderr pipe (the snapcraft → snapd / background
    /// uploader shape) must STILL honour the deadline. The direct child's
    /// `try_wait` succeeds immediately, but the reader threads can only EOF once
    /// the leaked grandchild releases the pipe — without a drain bound they block
    /// for the grandchild's full lifetime, blowing past the timeout and only
    /// surfacing at the global 1h pipeline watchdog (RED past every one-way
    /// door). The deadline must reap the whole process group so the readers EOF
    /// and the call returns promptly with the child's real (success) status.
    #[serial_test::serial(child_tree_registry)]
    #[test]
    #[cfg(unix)]
    fn run_capture_timeout_reaps_grandchild_holding_pipe() {
        let (log, _cap) = StageLogger::with_capture("test", Verbosity::Normal);
        let start = Instant::now();
        // `sleep 60 &` inherits sh's stdout/stderr; sh exits 0 immediately while
        // the backgrounded sleep keeps the pipe write-end open for 60s.
        let out = run_capture_timeout(
            &mut sh("sleep 60 & echo started; exit 0"),
            &log,
            "grandchild-holds-pipe",
            Duration::from_millis(300),
        )
        .expect("a clean child exit must yield Ok(Output), even with a leaked grandchild");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(20),
            "the drain must be bounded — the leaked grandchild's pipe was reaped \
             at the deadline, not waited out ({elapsed:?})"
        );
        assert_eq!(
            out.status.code(),
            Some(0),
            "the direct child exited 0; reaping a leaked grandchild must not \
             rewrite that into a failure (which would re-publish on retry)"
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("started"),
            "output the child wrote before exiting must still be captured"
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

    /// The live-child-tree registry add/remove is a plain set: register makes a
    /// pgid visible to the external-termination watcher; deregister removes it so
    /// a recycled pid is never signalled later. Uses a sentinel pgid that no real
    /// child would own.
    #[test]
    fn child_tree_registry_add_and_remove() {
        let sentinel = -424_242; // never a real pgid; distinct from any test child
        register_child_tree(sentinel);
        assert!(
            live_child_trees()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .contains(&sentinel),
            "register must make the tree visible to the watcher"
        );
        deregister_child_tree(sentinel);
        assert!(
            !live_child_trees()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .contains(&sentinel),
            "deregister must drop the tree so a recycled pid is never signalled"
        );
    }

    /// A `capture_inner` that returns `Err` (here: a child that outlives its
    /// timeout, taking the watchdog-kill error edge) must still leave the
    /// registry at its pre-call baseline. This pins the RAII guard's coverage
    /// of the error path — the leak window a manual deregister statement (placed
    /// after the reap, before the error returns) would miss. Unix-only: the
    /// timeout path relies on process-group kill semantics. Serialized against
    /// the watcher test, which broadcasts to every registered tree.
    #[cfg(unix)]
    #[serial_test::serial(child_tree_registry)]
    #[test]
    fn err_path_does_not_leak_registered_child_tree() {
        let baseline = live_child_trees()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len();
        let (log, _cap) = StageLogger::with_capture("test", Verbosity::Normal);
        let err = run_capture_timeout(
            &mut sh("sleep 30"),
            &log,
            "leak-probe",
            Duration::from_millis(50),
        )
        .expect_err("a child that outlives its timeout must surface an Err");
        assert!(
            format!("{err:#}").contains("did not exit within"),
            "the error must be the watchdog deadline kill; got: {err:#}"
        );
        assert_eq!(
            live_child_trees()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .len(),
            baseline,
            "the RAII guard must deregister the child tree even on the Err path"
        );
    }

    /// The external-termination watcher's kill routine
    /// ([`terminate_all_child_trees`]) must reach a registered, group-isolated
    /// child: spawn a real long-lived `sleep` in its own process group, register
    /// its pgid, fire the routine, and assert the child is reaped (not orphaned
    /// to outlive us — the CI-cancel hang this fix targets). Unix-only: the
    /// assertion uses `waitpid`/`kill` semantics.
    ///
    /// Serialized against the timeout tests: `terminate_all_child_trees`
    /// broadcasts to EVERY registered tree process-wide (correct production
    /// semantics — a real signal kills all children), so it must not run while
    /// another test has its own timeout child registered.
    #[cfg(unix)]
    #[serial_test::serial(child_tree_registry)]
    #[test]
    fn watcher_kill_reaps_registered_child_tree() {
        // Kill-on-drop guard so an assertion failure before the explicit
        // `wait()` below cannot orphan the 5-minute sleep for the rest of the
        // test session. The explicit reap takes the child back out of the guard.
        struct KillOnDrop(Option<std::process::Child>);
        impl Drop for KillOnDrop {
            fn drop(&mut self) {
                if let Some(mut c) = self.0.take() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
            }
        }

        let mut cmd = Command::new("sleep");
        cmd.arg("300");
        set_own_process_group(&mut cmd); // pgid == child pid
        let child = cmd.spawn().expect("spawn sleep child");
        let pid = child.id() as i32;
        let mut guard = KillOnDrop(Some(child));
        register_child_tree(pid);

        // Fire the watcher's actual kill routine (the same one the signal-watcher
        // thread runs). It group-SIGTERMs every registered tree.
        let killed = terminate_all_child_trees();
        assert!(killed >= 1, "watcher must report it signalled ≥1 tree");

        // Reap so no zombie leaks and confirm the child actually terminated by
        // signal — proving the SIGTERM reached the group, not that the child
        // exited on its own.
        let status = guard
            .0
            .take()
            .expect("child taken once")
            .wait()
            .expect("reap killed child");
        deregister_child_tree(pid);
        use std::os::unix::process::ExitStatusExt as _;
        assert_eq!(
            status.signal(),
            Some(libc::SIGTERM),
            "the registered child must die from the watcher's group SIGTERM, not outlive us; got {status:?}"
        );
    }
}
