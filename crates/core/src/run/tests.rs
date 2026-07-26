//! Tests for the subprocess exec API: capture and verbose tee behavior across
//! both verbosities, stdin piping, the bounded-wait watchdog and its
//! post-exit drain grace, and the live-subtree registry the external
//! termination handler drains.

use std::process::Command;
use std::time::{Duration, Instant};

use super::exec::*;
use super::process_tree::*;
use crate::log::{LogLevel, StageLogger, Verbosity};

/// `sh -c` wrapper so the tests run a portable shell snippet.
fn sh(script: &str) -> Command {
    let mut c = Command::new("sh");
    c.arg("-c").arg(script);
    c
}

/// `cmd /c` wrapper — the Windows analogue of [`sh`] for the Windows-only
/// tests, which need batch syntax (`start /b`, `&`) rather than a POSIX
/// shell snippet.
#[cfg(windows)]
fn cmd_c(script: &str) -> Command {
    let mut c = Command::new("cmd");
    c.arg("/c").arg(script);
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
#[cfg(unix)]
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
#[cfg(unix)]
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
#[cfg(unix)]
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
#[cfg(unix)]
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
#[cfg(unix)]
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
#[cfg(unix)]
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
#[cfg(unix)]
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
#[cfg(unix)]
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
#[cfg(unix)]
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
#[cfg(unix)]
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
#[cfg(unix)]
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

/// Windows analogue of `run_capture_timeout_reaps_grandchild_holding_pipe`.
/// A `cmd /c` that exits 0 immediately but leaves a backgrounded `ping` (the
/// Windows sleep idiom) holding the inherited stdout pipe — the
/// snapcraft → snapd / background-uploader shape on Windows. The direct
/// child's `try_wait` succeeds at once, but the reader threads can only EOF
/// once the leaked grandchild releases the pipe; without the drain bound they
/// block for ping's full ~60s lifetime, blowing past the timeout. On expiry
/// of `POST_EXIT_DRAIN_GRACE`, `wait_or_kill` calls `kill_child_tree`, which
/// on Windows reaps the Job Object the child was enclosed in via
/// `TerminateJobObject` — killing the leaked grandchild by job membership
/// even though the direct child already exited (which `taskkill /T` cannot
/// do: a terminated root is absent from the snapshot its tree walk needs).
/// That forces EOF so the call returns promptly with the child's real
/// (success) status. `ping` writes a line per second, so its presence in the
/// captured stdout also proves the grandchild genuinely inherited the pipe
/// (otherwise the pipe would close on the child's own exit and the drain reap
/// would never be exercised).
///
/// This is the regression guard for the dead-root `taskkill /T` bug: before
/// the Job Object fix this test waited out the full ~59s.
#[serial_test::serial(child_tree_registry)]
#[test]
#[cfg(windows)]
fn run_capture_timeout_reaps_grandchild_holding_pipe_windows() {
    let (log, _cap) = StageLogger::with_capture("test", Verbosity::Normal);
    let start = Instant::now();
    // `start /b` launches ping as a background grandchild that inherits cmd's
    // stdout handle; cmd exits 0 immediately while ping keeps the pipe
    // write-end open (`ping -n 60 127.0.0.1` ≈ 59s of 1s waits).
    let out = run_capture_timeout(
        &mut cmd_c("start /b ping -n 60 127.0.0.1 & echo started & exit 0"),
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
    let captured = String::from_utf8_lossy(&out.stdout);
    assert!(
        captured.contains("started"),
        "output the child wrote before exiting must still be captured"
    );
    assert!(
        captured.to_ascii_lowercase().contains("ping")
            || captured.to_ascii_lowercase().contains("pinging"),
        "the backgrounded grandchild must genuinely hold the inherited pipe \
         (its ping output should land in our capture); got: {captured:?}"
    );
}

/// The same large-in/large-out child at VERBOSE: the tee path must also
/// drain concurrently with the stdin write (no deadlock) and still capture
/// both streams whole.
#[test]
#[cfg(unix)]
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

/// The live-child-tree registry add/remove is a plain map: register makes a
/// pid visible to the external-termination watcher; deregister removes it so
/// a recycled pid is never signalled later. Uses a sentinel pid that no real
/// child would own. Serialized against the other registry tests: it mutates
/// the shared registry, so it must not run while a baseline-sensitive test
/// (`err_path_does_not_leak_registered_child_tree`) is sampling the length.
#[serial_test::serial(child_tree_registry)]
#[test]
fn child_tree_registry_add_and_remove() {
    let sentinel = -424_242; // never a real pgid; distinct from any test child
    register_child_tree(ChildTree {
        pid: sentinel,
        #[cfg(windows)]
        job: None,
    });
    assert!(
        live_child_trees()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(&sentinel),
        "register must make the tree visible to the watcher"
    );
    deregister_child_tree(sentinel);
    assert!(
        !live_child_trees()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(&sentinel),
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
    register_child_tree(ChildTree { pid });

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

#[test]
#[cfg(unix)]
fn slow_subprocess_heartbeats_fast_subprocess_does_not() {
    use crate::test_helpers::env::{EnvGuard, env_mutex};
    let _lock = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    // A 40ms cadence over a ~350ms child yields several heartbeats; assert
    // ≥1 so the test is robust to scheduler jitter. The RAII guard restores
    // the prior env even if an assertion below panics.
    let _g = EnvGuard::set(crate::progress::HEARTBEAT_INTERVAL_ENV, "40");

    let (log, cap) = StageLogger::with_capture("test", Verbosity::Normal);
    let out = run_checked(&mut sh("sleep 0.35"), &log, "sleep").expect("sleep must succeed");
    assert!(out.status.success());
    assert!(
        cap.heartbeat_count() >= 1,
        "a slow silent child must emit at least one heartbeat; got {}",
        cap.heartbeat_count()
    );

    let (log, cap) = StageLogger::with_capture("test", Verbosity::Normal);
    run_checked(&mut sh("true"), &log, "true").expect("true must succeed");
    assert_eq!(
        cap.heartbeat_count(),
        0,
        "an instant child must not emit a heartbeat"
    );
}
