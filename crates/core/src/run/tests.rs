//! Tests for the subprocess exec API: capture and verbose tee behavior across
//! both verbosities, stdin piping, the bounded-wait watchdog and its
//! post-exit drain grace, and the live-subtree registry the external
//! termination handler drains.

use std::process::Command;
use std::time::{Duration, Instant};

use super::exec::*;
use super::process_tree::*;
#[cfg(unix)]
use crate::log::LogLevel;
use crate::log::{StageLogger, Verbosity};

/// `sh -c` wrapper so the tests run a portable shell snippet.
#[cfg(unix)]
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
#[cfg(unix)]
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

/// A `*_KEY` env value spanning three lines is not a substring of any single
/// line the child prints, so per-line redaction printed it verbatim. The
/// stream redacter holds the partial value until the whole of it has been
/// seen.
#[test]
#[cfg(unix)]
fn tee_stream_never_prints_a_secret_split_across_lines() {
    let pem =
        "-----BEGIN OPENSSH PRIVATE KEY-----\nAAAABBBBCCCC\n-----END OPENSSH PRIVATE KEY-----";
    let (log, cap) = StageLogger::with_capture("test", Verbosity::Verbose);
    let log = log.with_env(vec![("SSH_PRIVATE_KEY".to_string(), pem.to_string())]);
    let mut cmd = sh("printf '%s' \"$SSH_PRIVATE_KEY\" >&2");
    cmd.env("SSH_PRIVATE_KEY", pem);
    run_checked(&mut cmd, &log, "leaky").expect("the child must succeed");

    let recorded: String = cap
        .all_messages()
        .into_iter()
        .map(|(_, msg)| msg)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        recorded.contains("$SSH_PRIVATE_KEY"),
        "the streamed secret must be masked by name; got: {recorded:?}"
    );
    for line in pem.lines() {
        assert!(
            !recorded.contains(line),
            "no line of the key body may reach the log, including the header; \
             leaked {line:?} in {recorded:?}"
        );
    }
}

/// A secret whose value carries `\r\n` is masked exactly like its LF sibling.
///
/// The tee normalised the terminator before the redacter saw the bytes, so a
/// CRLF-valued key matched nothing the redacter held and every line of it
/// reached the log.
#[test]
#[cfg(unix)]
fn tee_stream_masks_a_crlf_valued_secret() {
    let pem =
        "-----BEGIN OPENSSH PRIVATE KEY-----\r\nAAAABBBBCCCC\r\n-----END OPENSSH PRIVATE KEY-----";
    let (log, cap) = StageLogger::with_capture("test", Verbosity::Verbose);
    let log = log.with_env(vec![("SSH_PRIVATE_KEY".to_string(), pem.to_string())]);
    let mut cmd = sh("printf '%s' \"$SSH_PRIVATE_KEY\" >&2");
    cmd.env("SSH_PRIVATE_KEY", pem);
    run_checked(&mut cmd, &log, "leaky").expect("the child must succeed");

    let recorded: String = cap
        .all_messages()
        .into_iter()
        .map(|(_, msg)| msg)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        recorded.contains("$SSH_PRIVATE_KEY"),
        "the streamed CRLF secret must be masked by name; got: {recorded:?}"
    );
    for line in pem.lines() {
        assert!(
            !recorded.contains(line),
            "no line of the CRLF key body may reach the log, including the header; \
             leaked {line:?} in {recorded:?}"
        );
    }
}

/// With no secrets attached the tee must emit exactly what it emitted before
/// the stream redacter existed: one record per line, terminator stripped.
#[test]
#[cfg(unix)]
fn tee_stream_line_output_is_unchanged_without_secrets() {
    let (log, cap) = StageLogger::with_capture("test", Verbosity::Verbose);
    run_checked(&mut sh("echo one; echo two"), &log, "echoes").expect("the child must succeed");
    let verbose: Vec<String> = cap
        .all_messages()
        .into_iter()
        .filter(|(lvl, _)| *lvl == LogLevel::Verbose)
        .map(|(_, msg)| msg)
        .collect();
    assert_eq!(verbose, vec!["one".to_string(), "two".to_string()]);
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
#[cfg(unix)]
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
    // `cat` echoes the full stdin to stdout, then 100k extra lines follow
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
/// beyond the run — the CI-cancel hang this fix targets). Unix-only: the
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
    {
        let _g = EnvGuard::set(crate::progress::HEARTBEAT_INTERVAL_ENV, "40");
        let (log, cap) = StageLogger::with_capture("test", Verbosity::Normal);
        let out = run_checked(&mut sh("sleep 0.35"), &log, "sleep").expect("sleep must succeed");
        assert!(out.status.success());
        assert!(
            cap.heartbeat_count() >= 1,
            "a slow silent child must emit at least one heartbeat; got {}",
            cap.heartbeat_count()
        );
    }

    // The instant half must not race the scheduler: on a loaded box even
    // `true` can take longer than 40ms to spawn and exit, so its cadence is
    // one no process start can reach.
    let _g = EnvGuard::set(crate::progress::HEARTBEAT_INTERVAL_ENV, "5000");
    let (log, cap) = StageLogger::with_capture("test", Verbosity::Normal);
    run_checked(&mut sh("true"), &log, "true").expect("true must succeed");
    assert_eq!(
        cap.heartbeat_count(),
        0,
        "an instant child must not emit a heartbeat"
    );
}

/// Every verbose record saying that reading a child's output was stopped.
#[cfg(unix)]
fn drain_cut_off_lines(cap: &crate::log::LogCapture) -> Vec<String> {
    cap.all_messages()
        .into_iter()
        .filter(|(lvl, msg)| *lvl == LogLevel::Verbose && msg.contains("stopped using the pipes"))
        .map(|(_, msg)| msg)
        .collect()
}

/// A child that exits 0 while a backgrounded grandchild still holds its
/// stdout and stderr must not keep the call blocked for the grandchild's whole
/// life. `run_capture` has no timeout, so no watchdog reaps the grandchild:
/// the reader threads are abandoned once `POST_EXIT_DRAIN_WINDOW` has passed
/// since the child exited, and the call returns the child's status with what
/// was captured, saying so at verbose.
#[test]
#[cfg(unix)]
fn a_grandchild_holding_the_pipes_ends_the_drain_after_the_window() {
    let (log, cap) = StageLogger::with_capture("test", Verbosity::Verbose);
    let start = Instant::now();
    let out = run_capture(
        &mut sh("sleep 30 & echo started; exit 0"),
        &log,
        "grandchild-holds-pipes",
    )
    .expect("a clean child exit must yield Ok(Output)");
    let elapsed = start.elapsed();
    assert!(
        elapsed >= POST_EXIT_DRAIN_WINDOW
            && elapsed < POST_EXIT_DRAIN_WINDOW + Duration::from_secs(5),
        "the drain must end one window after the child exited, not when the \
         grandchild does ({elapsed:?})"
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "started\n");
    assert_eq!(
        drain_cut_off_lines(&cap),
        vec![format!(
            "stopped using the pipes of grandchild-holds-pipes {}s after it exited: \
             a process it started still holds them",
            POST_EXIT_DRAIN_WINDOW.as_secs()
        )]
    );
}

/// A grandchild that releases the pipes inside the window is waited for: what
/// it wrote after the direct child exited is captured, and no cut-off line is
/// printed.
#[test]
#[cfg(unix)]
fn a_grandchild_that_exits_inside_the_window_keeps_all_output() {
    let (log, cap) = StageLogger::with_capture("test", Verbosity::Verbose);
    let out = run_checked(
        &mut sh("(sleep 1; echo late; echo late-err >&2) & echo early; exit 0"),
        &log,
        "short-grandchild",
    )
    .expect("the child must succeed");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "early\nlate\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "late-err\n");
    assert!(
        drain_cut_off_lines(&cap).is_empty(),
        "no cut-off line when the pipes closed inside the window"
    );
}

/// The stdin writer is bounded by the same window as the readers. A payload
/// larger than the pipe buffer, fed to a child that exits while a grandchild
/// holds stdin unread, blocks the writer; the call still returns one window
/// after the child exited, with the child's own status.
#[test]
#[cfg(unix)]
fn a_grandchild_holding_stdin_unread_ends_the_write_after_the_window() {
    let (log, cap) = StageLogger::with_capture("test", Verbosity::Verbose);
    let payload = vec![b'x'; 1024 * 1024];
    let start = Instant::now();
    let out = run_checked_with_stdin(
        &mut sh("sleep 30 & exit 0"),
        &payload,
        &log,
        "grandchild-holds-stdin",
    )
    .expect("a clean child exit must yield Ok(Output)");
    let elapsed = start.elapsed();
    assert!(
        elapsed >= POST_EXIT_DRAIN_WINDOW
            && elapsed < POST_EXIT_DRAIN_WINDOW + Duration::from_secs(5),
        "the write must end one window after the child exited ({elapsed:?})"
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(drain_cut_off_lines(&cap).len(), 1);
}

/// A child that reads all of a payload larger than the pipe buffer receives
/// every byte: the chunked writer loses nothing.
#[test]
#[cfg(unix)]
fn a_child_reading_a_large_stdin_receives_every_byte() {
    let (log, cap) = StageLogger::with_capture("test", Verbosity::Verbose);
    let payload = vec![b'x'; 1024 * 1024];
    let out =
        run_checked_with_stdin(&mut sh("wc -c"), &payload, &log, "count").expect("wc must succeed");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        payload.len().to_string()
    );
    assert!(drain_cut_off_lines(&cap).is_empty());
}

/// Wait up to `limit` for `path` to hold a full line, and return it trimmed.
#[cfg(unix)]
fn wait_for_line(path: &std::path::Path, limit: Duration) -> Option<String> {
    let start = Instant::now();
    while start.elapsed() < limit {
        if let Ok(text) = std::fs::read_to_string(path)
            && text.ends_with('\n')
        {
            return Some(text.trim().to_string());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

/// After a cut-off the reader threads stop waiting on the pipes and close
/// them promptly: a grandchild that writes one second after the call returned
/// gets a broken pipe on both stdout and stderr, and nothing it wrote is
/// captured or teed.
#[test]
#[cfg(unix)]
fn the_readers_close_the_pipes_promptly_after_a_cut_off() {
    let dir = tempfile::tempdir().unwrap();
    let status_file = dir.path().join("status");
    let (log, cap) = StageLogger::with_capture("test", Verbosity::Verbose);
    let script = format!(
        "(trap '' PIPE; sleep {}; echo late; a=$?; echo late-err >&2; b=$?; \
         echo \"$a $b\" > '{}') & echo started; exit 0",
        POST_EXIT_DRAIN_WINDOW.as_secs() + 1,
        status_file.display()
    );
    let out = run_capture(&mut sh(&script), &log, "background-writer").expect("a clean child exit");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "started\n");
    let statuses = wait_for_line(&status_file, Duration::from_secs(5))
        .expect("the grandchild must finish its writes");
    let codes: Vec<&str> = statuses.split(' ').collect();
    assert!(
        codes.len() == 2 && codes.iter().all(|c| *c != "0"),
        "both late writes must fail on a closed pipe, got exit codes {statuses:?}"
    );
    assert!(
        !cap.all_messages().iter().any(|(_, m)| m.contains("late")),
        "nothing written after the cut-off may be printed"
    );
}

/// How many bytes a fresh pipe accepts before a non-blocking write would block.
#[cfg(unix)]
fn pipe_capacity() -> usize {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is a two-element array as `pipe` requires; both ends are
    // closed below, and the writes only read from `chunk`.
    unsafe {
        assert_eq!(libc::pipe(fds.as_mut_ptr()), 0);
        let flags = libc::fcntl(fds[1], libc::F_GETFL);
        assert_eq!(
            libc::fcntl(fds[1], libc::F_SETFL, flags | libc::O_NONBLOCK),
            0
        );
        let chunk = [0u8; 1024];
        let mut total = 0usize;
        loop {
            let n = libc::write(fds[1], chunk.as_ptr().cast(), chunk.len());
            if n <= 0 {
                break;
            }
            total += n as usize;
        }
        libc::close(fds[0]);
        libc::close(fds[1]);
        total
    }
}

/// After a cut-off the stdin writer stops and closes the child's stdin
/// without waiting for the pipe to drain: a grandchild that holds stdin unread
/// and reads it one second after the call returned gets what the pipe had
/// buffered and then EOF. A writer still parked in a blocking write would
/// complete that write once the grandchild started reading, so the grandchild
/// would read more than one pipe's worth.
#[test]
#[cfg(unix)]
fn the_stdin_writer_closes_the_pipe_promptly_after_a_cut_off() {
    let dir = tempfile::tempdir().unwrap();
    let count_file = dir.path().join("count");
    let (log, cap) = StageLogger::with_capture("test", Verbosity::Verbose);
    let payload = vec![b'x'; 1024 * 1024];
    // A non-interactive shell points a background list's stdin at /dev/null,
    // so the child's stdin reaches it as fd 3; stdout and stderr go elsewhere
    // so only the writer is left waiting at the cut-off.
    let script = format!(
        "exec 3<&0; (sleep {}; wc -c <&3 > '{}') >/dev/null 2>&1 & exit 0",
        POST_EXIT_DRAIN_WINDOW.as_secs() + 1,
        count_file.display()
    );
    let out = run_checked_with_stdin(&mut sh(&script), &payload, &log, "stdin-holder")
        .expect("a clean child exit");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(drain_cut_off_lines(&cap).len(), 1);
    let count: usize = wait_for_line(&count_file, Duration::from_secs(5))
        .expect("the grandchild must reach EOF on stdin")
        .parse()
        .unwrap();
    let capacity = pipe_capacity();
    assert!(
        count <= capacity,
        "the writer must close stdin at the cut-off, yet the grandchild read \
         {count} bytes from a pipe that holds {capacity}"
    );
}

/// The Windows twin of
/// `a_grandchild_holding_the_pipes_ends_the_drain_after_the_window`: `start /b`
/// leaves a `ping` holding the child's stderr after `cmd` exits, and
/// `run_capture`, which has no timeout and so no Job Object reap, still
/// returns one window after the child exited.
#[test]
#[cfg(windows)]
fn a_grandchild_holding_the_pipes_ends_the_drain_after_the_window_windows() {
    let (log, cap) = StageLogger::with_capture("test", Verbosity::Verbose);
    let start = Instant::now();
    let out = run_capture(
        &mut cmd_c("start /b ping -n 30 127.0.0.1 >nul & echo started"),
        &log,
        "grandchild-holds-pipes",
    )
    .expect("a clean child exit must yield Ok(Output)");
    let elapsed = start.elapsed();
    assert!(
        elapsed >= POST_EXIT_DRAIN_WINDOW
            && elapsed < POST_EXIT_DRAIN_WINDOW + Duration::from_secs(5),
        "the drain must end one window after the child exited, not when the \
         grandchild does ({elapsed:?})"
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "started");
    let cut_off: Vec<String> = cap
        .all_messages()
        .into_iter()
        .filter(|(_, msg)| msg.contains("stopped using the pipes"))
        .map(|(_, msg)| msg)
        .collect();
    assert_eq!(
        cut_off,
        vec![format!(
            "stopped using the pipes of grandchild-holds-pipes {}s after it exited: \
             a process it started still holds them",
            POST_EXIT_DRAIN_WINDOW.as_secs()
        )]
    );
}

/// A reader that hands out `data` and then fails, as a pipe can mid-stream.
struct FailsAfter {
    data: &'static [u8],
}

impl std::io::Read for FailsAfter {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.data.is_empty() {
            return Err(std::io::Error::other("pipe broke"));
        }
        let n = self.data.len().min(buf.len());
        buf[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        Ok(n)
    }
}

/// A read error mid-line keeps and prints the partial line read before it,
/// alongside the complete lines, and the partial line still goes through the
/// redacter.
#[test]
fn tee_stream_keeps_the_partial_line_read_before_an_error() {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    let (log, cap) = StageLogger::with_capture("test", Verbosity::Verbose);
    let log = log.with_job_env([("API_TOKEN".to_string(), "longvalue1234".to_string())]);
    let capture = Mutex::new(Vec::new());
    tee_stream(
        FailsAfter {
            data: b"whole\npartial longvalue1234",
        },
        &log,
        false,
        true,
        &capture,
        &AtomicBool::new(false),
    );
    assert_eq!(
        capture.into_inner().unwrap(),
        b"whole\npartial longvalue1234"
    );
    let printed: String = cap
        .all_messages()
        .into_iter()
        .map(|(_, m)| m)
        .collect::<Vec<_>>()
        .join("|");
    assert!(
        printed.contains("partial $API_TOKEN"),
        "tee lost or did not mask the partial line: {printed}"
    );
    assert!(
        !printed.contains("longvalue1234"),
        "the secret in the partial line reached the log: {printed}"
    );
}

/// An unterminated last line the child wrote before exiting is captured even
/// when a grandchild keeps the pipe open past the drain window: the bytes were
/// read before the cut-off, so they are in the output the call returns.
#[test]
#[cfg(unix)]
fn the_unterminated_last_line_read_before_the_cut_off_is_captured() {
    let (log, cap) = StageLogger::with_capture("test", Verbosity::Verbose);
    let out = run_capture(
        &mut sh("printf 'tail-no-nl'; sleep 30 &"),
        &log,
        "grandchild-holds-pipes",
    )
    .expect("a clean child exit must yield Ok(Output)");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "tail-no-nl");
    assert_eq!(
        drain_cut_off_lines(&cap).len(),
        1,
        "the call must end at the cut-off, with the grandchild still holding the pipe"
    );
    let printed: String = cap
        .all_messages()
        .into_iter()
        .map(|(_, m)| m)
        .collect::<Vec<_>>()
        .join("|");
    assert!(
        !printed.contains("tail-no-nl"),
        "an unterminated line still held back at the cut-off must not be printed: {printed}"
    );
}

/// Hands out at most 1 KiB per read, the way a pipe often delivers a chatty
/// child's output, so a reader that rescans what it holds per read does so
/// thousands of times.
struct Trickle<'a> {
    data: &'a [u8],
}

impl std::io::Read for Trickle<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.data.len().min(buf.len()).min(1024);
        buf[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        Ok(n)
    }
}

/// A multi-megabyte line with no terminator (single-line JSON at `-v`) is
/// captured byte for byte and printed once, in time linear in its length.
/// The bound is loose so a slow machine passes; rescanning everything held
/// back on every read took far longer than it on this input.
#[test]
fn tee_stream_handles_a_long_unterminated_line_in_linear_time() {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    let line: String = (0..4 * 1024 * 1024)
        .map(|i| char::from(b'a' + (i % 26) as u8))
        .collect();
    let (log, cap) = StageLogger::with_capture("test", Verbosity::Verbose);
    let capture = Mutex::new(Vec::new());
    let started = Instant::now();
    tee_stream(
        Trickle {
            data: line.as_bytes(),
        },
        &log,
        false,
        true,
        &capture,
        &AtomicBool::new(false),
    );
    let elapsed = started.elapsed();
    assert_eq!(capture.into_inner().unwrap(), line.as_bytes());
    let printed = cap
        .all_messages()
        .into_iter()
        .filter(|(_, m)| m.contains(&line))
        .count();
    assert_eq!(printed, 1, "the line must be printed exactly once");
    assert!(
        elapsed < Duration::from_secs(5),
        "teeing one 4 MiB line took {elapsed:?}"
    );
}
