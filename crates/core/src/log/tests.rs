//! Tests for the log register: the `status`/`verbose`/`warn`/`error` vocabulary
//! and its gutter format, section nesting depth and the RAII guards that move
//! it, deferred section headers that only print once their section produces
//! output, secret redaction, and the `test-helpers` capture surface.

use std::sync::Mutex;
use std::sync::atomic::Ordering;

use super::capture::*;
use super::depth::*;
use super::render::*;
use super::stage_logger::*;
use super::verbosity::*;
use super::*;

/// Serializes the section-depth tests: `SECTION_DEPTH` is a
/// process-global atomic, so two grouping tests running on parallel
/// threads would observe each other's increments.
static SECTION_TEST_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn test_group_guard_balances_depth_locally() {
    // `group()` increments depth on open and the guard decrements on
    // drop, so nested sections always balance back to the start depth.
    let _guard = SECTION_TEST_LOCK.lock().unwrap();
    let log = StageLogger::new("build", Verbosity::Normal);
    let start = SECTION_DEPTH.load(Ordering::Relaxed);
    {
        let _outer = log.group("build");
        assert_eq!(SECTION_DEPTH.load(Ordering::Relaxed), start + 1);
        {
            let _inner = log.group("sign");
            assert_eq!(SECTION_DEPTH.load(Ordering::Relaxed), start + 2);
        }
        assert_eq!(SECTION_DEPTH.load(Ordering::Relaxed), start + 1);
    }
    assert_eq!(SECTION_DEPTH.load(Ordering::Relaxed), start);
}

#[test]
fn test_group_quiet_still_tracks_local_depth() {
    // Even at Quiet verbosity the indent depth must stay balanced so
    // any status lines that DO print (errors) indent correctly and the
    // guard's decrement has a matching increment.
    let _guard = SECTION_TEST_LOCK.lock().unwrap();
    let log = StageLogger::new("build", Verbosity::Quiet);
    let start = SECTION_DEPTH.load(Ordering::Relaxed);
    {
        let _s = log.group("build");
        assert_eq!(SECTION_DEPTH.load(Ordering::Relaxed), start + 1);
    }
    assert_eq!(SECTION_DEPTH.load(Ordering::Relaxed), start);
}

#[test]
fn test_group_with_body_flushes_header_once() {
    // A section that emits a real body line flushes its deferred header:
    // the pending entry is marked `flushed` exactly once and stays at its
    // own depth. (`flush_pending` writes the header to stderr; we assert
    // the state transition rather than capture the uncapturable eprintln.)
    let _guard = SECTION_TEST_LOCK.lock().unwrap();
    let log = StageLogger::new("build", Verbosity::Normal);
    {
        let _section = log.group("build");
        // Header is pending, not yet printed.
        assert!(!PENDING.lock().unwrap().last().unwrap().flushed);
        log.status("compiling x86_64-unknown-linux-gnu");
        // The body line flushed the header.
        let pending = PENDING.lock().unwrap();
        let entry = pending.last().unwrap();
        assert!(entry.flushed, "body line must flush the header");
        assert_eq!(entry.verb, "Building");
        assert_eq!(entry.msg, "binaries");
    }
    // Guard drop popped the (flushed) entry.
    assert!(PENDING.lock().unwrap().is_empty());
}

#[test]
fn test_noop_group_prints_no_header() {
    // A section that emits NOTHING leaves its pending entry unflushed, and
    // the guard drop pops it without ever printing — a no-op stage shows
    // no bare header (the GoReleaser behavior). A blank `status("")` spacer
    // is NOT a real body line, so it does not flush either.
    let _guard = SECTION_TEST_LOCK.lock().unwrap();
    let log = StageLogger::new("verify-release", Verbosity::Normal);
    {
        let _section = log.group("verify-release");
        log.status(""); // blank spacer — must not flush
        assert!(
            !PENDING.lock().unwrap().last().unwrap().flushed,
            "a no-op section's header must stay unflushed"
        );
    }
    assert!(PENDING.lock().unwrap().is_empty());
}

#[test]
fn test_nested_groups_flush_in_ancestor_order() {
    // A body line in a nested section flushes BOTH the ancestor and the
    // nested header (each at its own stored depth), so the deferred
    // headers appear in correct order above the first line.
    let _guard = SECTION_TEST_LOCK.lock().unwrap();
    let log = StageLogger::new("publish", Verbosity::Normal);
    let start = SECTION_DEPTH.load(Ordering::Relaxed);
    {
        let _outer = log.group("publish");
        {
            let _inner = log.group("blob");
            log.status("uploading blob");
            let pending = PENDING.lock().unwrap();
            assert_eq!(pending.len(), 2);
            assert!(pending[0].flushed, "ancestor header must flush");
            assert!(pending[1].flushed, "nested header must flush");
            assert_eq!(pending[0].depth, start);
            assert_eq!(pending[1].depth, start + 1);
        }
    }
    assert!(PENDING.lock().unwrap().is_empty());
}

/// Run `f` with the process stderr fd (2) redirected to a temp file, then
/// restore it and return everything that reached fd 2 as a string, or
/// `None` if `eprintln!` output is being intercepted before fd 2.
///
/// `eprintln!` writes through libtest's macro path, which — under a plain
/// in-process `cargo test` — diverts output to a thread-local capture sink
/// BEFORE it reaches fd 2, so an fd swap observes nothing. Under
/// `cargo nextest` (the CI test runner) each test is its own process with a
/// real stderr pipe, so the swap captures the true bytes. A sentinel probe
/// distinguishes the two: if the sentinel does not survive the round-trip,
/// fd 2 is not the real emit target and the caller must fall back.
///
/// `f` must emit its header as the FIRST line it writes — callers slice the
/// header off with `.lines().next()`, which is only correct because the
/// caller's `group()` defers the header and `flush_pending` writes it ahead
/// of any body line. A change that made `f` emit anything before its header
/// would silently grab the wrong line.
///
/// Unix-only. The fd-2 swap is process-global across the WHOLE
/// `anodizer-core` test binary, so a caller must exclude every other test
/// whose output could land in the capture: itself and its `stderr_fd`
/// peers, plus the two groups in this binary whose tests spawn
/// subprocesses that inherit fd 2 — hence
/// `#[serial_test::serial(cwd, path_env, stderr_fd)]`.
/// `SECTION_TEST_LOCK` only orders the in-file `PENDING`/`SECTION_DEPTH`
/// state these callers also touch.
///
/// Under `cargo nextest` (the gate) each test is its own process, so fd 2
/// is private and the keys are belt-and-braces; they carry the weight only
/// under an in-process `cargo test`, where an unkeyed `#[serial]` would
/// have excluded nothing keyed at all.
#[cfg(unix)]
fn capture_stderr(f: impl FnOnce()) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::unix::io::AsRawFd;

    /// Restores fd 2 from the saved dup on EVERY exit path, including a
    /// panic in `f` or the probe between the swap and the read. Without
    /// this, an unwind would leave fd 2 pointed at the dropped tempfile, so
    /// every later test's panic/`eprintln!` diagnostics in this shared
    /// process would write to a dangling fd and vanish.
    struct StderrRestore(libc::c_int);
    impl Drop for StderrRestore {
        fn drop(&mut self) {
            // SAFETY: self.0 is the dup of the original stderr taken before
            // the swap; restoring it on every exit path (including unwind)
            // guarantees fd 2 is never left dangling at the tempfile.
            unsafe {
                libc::dup2(self.0, libc::STDERR_FILENO);
                libc::close(self.0);
            }
        }
    }

    let mut file = tempfile::tempfile().expect("tempfile for stderr capture");
    std::io::stderr().flush().ok();
    // SAFETY: dup/dup2 on the live stderr fd; the saved fd is owned by the
    // StderrRestore guard below, which restores fd 2 and closes the dup on
    // every exit path (panic-safe). The whole swap is serialized by the
    // caller's `#[serial(cwd, path_env, stderr_fd)]` keys.
    let saved = unsafe { libc::dup(libc::STDERR_FILENO) };
    assert!(saved >= 0, "dup(stderr) failed");
    unsafe {
        assert!(
            libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO) >= 0,
            "dup2(tempfile, stderr) failed"
        );
    }
    // Owns `saved` from here on; its Drop restores fd 2 even if `f` panics.
    let _restore = StderrRestore(saved);

    const SENTINEL: &str = "__anodizer_capture_probe__";
    eprintln!("{SENTINEL}");
    f();
    std::io::stderr().flush().ok();

    file.seek(SeekFrom::Start(0)).expect("rewind capture file");
    let mut out = String::new();
    file.read_to_string(&mut out).expect("read capture file");
    // The sentinel survives only when fd 2 is the real emit target (nextest
    // / `--nocapture`); under in-process `cargo test` libtest swallowed it
    // (and `f`'s output), so the fd capture cannot prove anything.
    let body = out.strip_prefix(SENTINEL)?.trim_start_matches('\n');
    Some(body.to_string())
}

#[test]
#[cfg(unix)]
#[serial_test::serial(cwd, path_env, stderr_fd)]
fn test_header_paths_emit_identical_bytes() {
    // Regression guard for the v0.9.1 drift where stage headers rendered
    // with 2/3/4/5 leading spaces depending on which path printed them.
    //
    // This drives the TWO REAL emitting paths — the deferred-section header
    // in `flush_pending` and the direct `step` — and asserts they write
    // byte-identical headers at the same depth. It compares ACTUAL stderr
    // bytes (not `render_header`'s return value), so it FAILS the moment
    // either path open-codes its own indent/spacing instead of delegating
    // to `render_header`. Under `cargo nextest` (the CI gate) the fd capture
    // sees real output; under a bare in-process `cargo test` libtest
    // intercepts `eprintln!` and `capture_stderr` returns None, so the body
    // falls back to re-checking the shared helper rather than failing
    // spuriously. The named serial keys keep the subprocess-spawning and
    // fd-swapping tests out of the capture; SECTION_TEST_LOCK only orders
    // the in-file PENDING/SECTION_DEPTH state.
    let _guard = SECTION_TEST_LOCK.lock().unwrap();
    let log = StageLogger::new("sign", Verbosity::Normal);
    // Absolute depth both paths must render at (includes any inherited base
    // from ANODIZER_LOG_DEPTH, so the anchor below shifts with it).
    let depth = current_depth();

    // flush_pending path: open a section (pending header pushed at `depth`),
    // then a body line triggers `flush_pending`, which prints the header.
    // The header is the FIRST captured line; the body line follows it.
    let flushed = capture_stderr(|| {
        let _section = log.group("sign");
        assert_eq!(
            PENDING.lock().unwrap().last().unwrap().depth,
            depth,
            "pending header must sit at the pre-increment depth"
        );
        log.status("byte-equality probe"); // forces flush_pending
    });
    assert!(
        PENDING.lock().unwrap().is_empty(),
        "guard must pop the entry"
    );

    // step path: the section is closed, so `current_depth()` is back to
    // `depth` — the same depth the pending header rendered at. `step` emits
    // exactly the header line, nothing else.
    assert_eq!(current_depth(), depth, "depth must return to start");
    let stepped = capture_stderr(|| log.step("Signing", "artifacts"));

    let prefix = "  ".repeat(depth);
    let expected = format!("{prefix}{:>VERB_COLUMN$} artifacts", "Signing");

    match (flushed, stepped) {
        (Some(flushed), Some(stepped)) => {
            let flush_header = strip_ansi(
                flushed
                    .lines()
                    .next()
                    .expect("flush_pending must emit a header line"),
            );
            let step_header = strip_ansi(stepped.trim_end_matches('\n'));
            // The whole point: both REAL paths produce the same header
            // bytes. If a future edit makes one open-code a different
            // indent, these diverge and the test fails.
            assert_eq!(
                flush_header, step_header,
                "flush_pending and step must emit byte-identical headers \
                 (flush={flush_header:?} step={step_header:?})"
            );
            // Anchor the shared bytes so a regression that drifts BOTH paths
            // in lockstep (still equal to each other) is also caught.
            assert_eq!(
                step_header, expected,
                "header must be indent + gutter verb + space + message"
            );
        }
        // In-process `cargo test`: BOTH swaps were intercepted, so the real
        // paths are unobservable here. Re-assert the shared helper so the
        // test is not a silent no-op; nextest exercises the real bytes.
        (None, None) => {
            assert_eq!(
                strip_ansi(&render_header(depth, "Signing", "artifacts")),
                expected
            );
        }
        // The sentinel survived one swap but not the other — a real capture
        // anomaly (a flaky/half-redirected environment), not the documented
        // all-or-nothing fallback. Surface it loudly instead of silently
        // running the weaker check.
        (flushed, stepped) => panic!(
            "inconsistent stderr capture: flush={} step={}",
            flushed.is_some(),
            stepped.is_some()
        ),
    }
}

#[test]
#[cfg(unix)]
#[serial_test::serial(cwd, path_env, stderr_fd)]
fn test_single_word_header_emits_no_trailing_space() {
    // A single-word phrase (empty message) renders the bare gutter verb
    // with NO trailing space on the REAL `step` path — a stray space here
    // would leave invisible whitespace at the end of every `Publishing`
    // header line. Drives `step` directly and inspects the emitted bytes.
    let _guard = SECTION_TEST_LOCK.lock().unwrap();
    let log = StageLogger::new("publish", Verbosity::Normal);
    let depth = current_depth();
    let prefix = "  ".repeat(depth);

    match capture_stderr(|| log.step("Publishing", "")) {
        Some(stepped) => {
            let header = strip_ansi(stepped.trim_end_matches('\n'));
            assert_eq!(header, format!("{prefix}{:>VERB_COLUMN$}", "Publishing"));
            assert!(
                !header.ends_with(' '),
                "single-word header must not carry a trailing space: {header:?}"
            );
        }
        // In-process `cargo test` intercepts `eprintln!`; re-assert the
        // shared helper so the invariant still has a floor under nextest.
        None => {
            let header = strip_ansi(&render_header(depth, "Publishing", ""));
            assert_eq!(header, format!("{prefix}{:>VERB_COLUMN$}", "Publishing"));
            assert!(!header.ends_with(' '));
        }
    }
}

#[test]
fn test_status_labels_gutter_aligned_without_colon() {
    // Regression guard: Warning/Error/Note must render as right-aligned
    // gutter labels with NO trailing colon, and their message must land in
    // the same column as a section header's message (both follow the
    // VERB_COLUMN gutter + one space). The old format open-coded
    // "Warning:" at BODY_INDENT, which faked Cargo alignment with an
    // anti-Cargo colon.
    let header = strip_ansi(&render_header(0, "Building", "binaries"));
    let header_msg_col = header.find("binaries");
    for (rendered, label, msg) in [
        (render_warning("oops"), "Warning", "oops"),
        (render_error("boom"), "Error", "boom"),
        (render_note("fyi"), "Note", "fyi"),
    ] {
        let line = strip_ansi(&rendered);
        assert!(
            !line.contains(':'),
            "status label must not carry a colon: {line:?}"
        );
        // Label lines go through the SAME gutter renderer as section
        // headers: stripped of color, a Warning/Error/Note line is
        // byte-identical to a header whose verb is that label. Deriving the
        // expectation from `render_header` (not a hand-written format)
        // proves the shared renderer rather than re-stating its shape.
        assert_eq!(line, strip_ansi(&render_header(0, label, msg)));
        // Column-invariance across differing label widths: every label's
        // message lands in the same column as the "Building" header's,
        // regardless of how long the verb is.
        assert_eq!(
            line.find(msg),
            header_msg_col,
            "status-label message must align with the header message column"
        );
    }
}

#[test]
#[serial_test::serial(stderr_fd)]
fn test_status_label_aligns_with_enclosing_header_not_body_depth() {
    // Regression: a Warning/Error/Note fired INSIDE a section must align
    // with that section's HEADER (label in the verb column, message in the
    // header message column), NOT one level deeper at the body-bullet
    // depth. The body-depth variant pushed the gutter-aligned label two
    // columns past both the sibling headers and the `•` bullets, leaving it
    // floating on its own.
    let _guard = SECTION_TEST_LOCK.lock().unwrap();
    let log = StageLogger::new("build", Verbosity::Normal);
    let base = base_depth();
    // Open one section: its header renders at depth `base`; body lines
    // (and the bullets) render one level deeper at `base + 1`.
    let _outer = log.group("preparing release");
    let warn = strip_ansi(&render_warning("preflight skipped"));
    // Aligns with the enclosing header (depth `base`) ...
    assert_eq!(
        warn,
        strip_ansi(&render_header(base, "Warning", "preflight skipped")),
        "in-section label must align with its enclosing header"
    );
    // ... and NOT with the deeper body depth (`base + 1`) it used before.
    assert_ne!(
        warn,
        strip_ansi(&render_header(base + 1, "Warning", "preflight skipped")),
        "in-section label must not float at the deeper body indent"
    );
}

#[test]
#[cfg(unix)]
#[serial_test::serial(cwd, path_env, stderr_fd)]
fn test_capture_stderr_restores_fd_on_panic() {
    // The fd-restore must run on the unwind path: a panic inside `f`
    // (the asserts in the real callers are a reachable panic path) must
    // not leave fd 2 dangling at the dropped tempfile, which would make
    // every later test's stderr vanish in the shared process.
    let _guard = SECTION_TEST_LOCK.lock().unwrap();
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        capture_stderr(|| panic!("boom inside capture"));
    }));
    assert!(panicked.is_err(), "the injected panic must propagate");

    // fd 2 is usable again: a fresh capture round-trips its sentinel. (Under
    // in-process `cargo test` the sentinel is swallowed and the result is
    // None — still a successful, non-dangling write; only a leaked fd 2
    // would corrupt this follow-up capture.)
    let after = capture_stderr(|| eprintln!("after panic"));
    if let Some(body) = after {
        assert!(
            body.contains("after panic"),
            "stderr must work after a mid-capture panic: {body:?}"
        );
    }
}

#[test]
fn test_indent_reflects_section_depth() {
    // Indentation tracks the open-section depth (2 spaces per level)
    // identically everywhere — anodizer streams one continuous log, so
    // indentation (not a collapsible `::group::` block) conveys nesting.
    let _guard = SECTION_TEST_LOCK.lock().unwrap();
    let log = StageLogger::new("build", Verbosity::Normal);
    // Relative to the inherited base so an exported ANODIZER_LOG_DEPTH
    // in the test environment shifts every expectation uniformly.
    let base = "  ".repeat(base_depth());
    assert_eq!(indent(), base);
    {
        let _outer = log.group("build");
        assert_eq!(indent(), format!("{base}  "));
        {
            let _inner = log.group("sign");
            assert_eq!(indent(), format!("{base}    "));
        }
        assert_eq!(indent(), format!("{base}  "));
    }
    assert_eq!(indent(), base);
}

#[test]
fn test_indent_one_level_adds_depth_without_pending_header() {
    // The header-less guard must deepen the indent (so the row aligns
    // with sibling sections' body bullets) without registering a
    // pending header that a later body line could spuriously flush.
    let _guard = SECTION_TEST_LOCK.lock().unwrap();
    let start = SECTION_DEPTH.load(Ordering::Relaxed);
    let pending_before = PENDING.lock().unwrap().len();
    {
        let _indent = indent_one_level();
        assert_eq!(SECTION_DEPTH.load(Ordering::Relaxed), start + 1);
        assert_eq!(
            PENDING.lock().unwrap().len(),
            pending_before,
            "indent_one_level must not push a pending header"
        );
        assert_eq!(indent(), "  ".repeat(current_depth()));
    }
    assert_eq!(SECTION_DEPTH.load(Ordering::Relaxed), start);
}

#[test]
fn test_parse_base_depth_accepts_valid_and_degrades_invalid() {
    // A subprocess child inherits a numeric depth; anything else
    // (absent, junk, negative) degrades to the standalone default 0 —
    // indentation must never abort a run.
    assert_eq!(parse_base_depth(Some("3")), 3);
    assert_eq!(parse_base_depth(Some(" 2 ")), 2);
    assert_eq!(parse_base_depth(Some("0")), 0);
    assert_eq!(parse_base_depth(Some("-1")), 0);
    assert_eq!(parse_base_depth(Some("abc")), 0);
    assert_eq!(parse_base_depth(Some("")), 0);
    assert_eq!(parse_base_depth(None), 0);
}

#[test]
fn test_current_depth_tracks_sections() {
    // `current_depth` = inherited base (0 in tests — the env var is
    // not set under cargo test) + open sections; it is the value a
    // parent exports to children via LOG_DEPTH_ENV.
    let _guard = SECTION_TEST_LOCK.lock().unwrap();
    let log = StageLogger::new("build", Verbosity::Normal);
    let start = current_depth();
    {
        let _outer = log.group("build");
        assert_eq!(current_depth(), start + 1);
    }
    assert_eq!(current_depth(), start);
}

#[test]
fn test_stage_header_splits_into_verb_and_message() {
    // A multi-word phrase splits on the FIRST space: the verb feeds the
    // right-aligned gutter, the remainder is the section message.
    let log = StageLogger::new("build", Verbosity::Normal);
    assert_eq!(log.split_header("build"), ("Building", "binaries"));
    assert_eq!(log.split_header("sign"), ("Signing", "artifacts"));
    assert_eq!(log.split_header("source"), ("Archiving", "source"));
}

#[test]
fn test_stage_header_single_word_renders_verb_only() {
    // A known single-word phrase ("Publishing") renders just the gutter
    // verb with an empty message — no stage-name echo.
    let log = StageLogger::new("publish", Verbosity::Normal);
    assert_eq!(log.split_header("publish"), ("Publishing", ""));
}

#[test]
fn test_stage_header_unknown_stage_uses_running_plus_name() {
    // An unknown stage falls back to "Running" + the stage name, so it
    // still renders in the system vocabulary (`   Running myfancystage`).
    let log = StageLogger::new("x", Verbosity::Normal);
    assert_eq!(
        log.split_header("myfancystage"),
        ("Running", "myfancystage")
    );
}

#[test]
fn test_verbosity_from_flags_default() {
    assert_eq!(
        Verbosity::from_flags(false, false, false),
        Verbosity::Normal
    );
}

#[test]
fn test_verbosity_from_flags_quiet() {
    assert_eq!(Verbosity::from_flags(true, false, false), Verbosity::Quiet);
}

#[test]
fn test_verbosity_from_flags_verbose() {
    assert_eq!(
        Verbosity::from_flags(false, true, false),
        Verbosity::Verbose
    );
}

#[test]
fn test_verbosity_from_flags_debug() {
    assert_eq!(Verbosity::from_flags(false, false, true), Verbosity::Debug);
}

#[test]
fn test_verbosity_from_flags_debug_wins_over_verbose() {
    assert_eq!(Verbosity::from_flags(false, true, true), Verbosity::Debug);
}

#[test]
fn test_verbosity_from_flags_debug_wins_over_quiet() {
    assert_eq!(Verbosity::from_flags(true, false, true), Verbosity::Debug);
}

#[test]
fn test_verbosity_from_flags_quiet_overrides_verbose() {
    assert_eq!(Verbosity::from_flags(true, true, false), Verbosity::Quiet);
}

#[test]
fn test_verbosity_ordering() {
    assert!(Verbosity::Quiet < Verbosity::Normal);
    assert!(Verbosity::Normal < Verbosity::Verbose);
    assert!(Verbosity::Verbose < Verbosity::Debug);
}

#[test]
fn test_stage_logger_is_verbose() {
    let log = StageLogger::new("test", Verbosity::Verbose);
    assert!(log.is_verbose());
    assert!(!log.is_debug());
}

#[test]
fn test_stage_logger_is_debug() {
    let log = StageLogger::new("test", Verbosity::Debug);
    assert!(log.is_verbose());
    assert!(log.is_debug());
}

#[test]
fn test_stage_logger_normal_not_verbose() {
    let log = StageLogger::new("test", Verbosity::Normal);
    assert!(!log.is_verbose());
    assert!(!log.is_debug());
}

#[test]
fn test_default_verbosity_is_normal() {
    assert_eq!(Verbosity::default(), Verbosity::Normal);
}

// -----------------------------------------------------------------
// Redaction inside check_output
// -----------------------------------------------------------------

#[cfg(unix)]
fn fake_output(stdout: &[u8], stderr: &[u8], code: i32) -> std::process::Output {
    use std::os::unix::process::ExitStatusExt;
    std::process::Output {
        status: std::process::ExitStatus::from_raw(code << 8),
        stdout: stdout.to_vec(),
        stderr: stderr.to_vec(),
    }
}

#[test]
fn test_redact_uses_attached_env() {
    // A logger built via `with_env` must scrub configured secrets.
    let log = StageLogger::new("test", Verbosity::Normal).with_env(vec![(
        "GITHUB_TOKEN".to_string(),
        "ghp_real_secret_token".to_string(),
    )]);
    let out = log.redact("auth header: ghp_real_secret_token");
    assert_eq!(out, "auth header: $GITHUB_TOKEN");
    assert!(!out.contains("ghp_real_secret_token"));
}

#[test]
fn test_redact_without_env_only_scrubs_inline_urls() {
    // A logger constructed without `with_env` still scrubs inline URL
    // credentials, even if the bare token is not in env (the env-pair
    // list is empty).
    let log = StageLogger::new("test", Verbosity::Normal);
    let out = log.redact("fetched from https://user:tok@example.com/path");
    assert_eq!(out, "fetched from https://<redacted>@example.com/path");
}

#[test]
fn test_redact_combines_env_and_url_credentials() {
    let log = StageLogger::new("test", Verbosity::Normal)
        .with_env(vec![("API_TOKEN".to_string(), "ghp_tok123".to_string())]);
    // Both the env-value token AND the inline URL credential should be
    // scrubbed in a single call.
    let out = log.redact("remote: https://ghp_tok123@github.com/x/y");
    // URL credential strip runs first, so the `ghp_tok123` between
    // `://` and `@` becomes `<redacted>`. The path / host text never
    // contains `ghp_tok123`, so the env-value pass is a no-op here.
    assert_eq!(out, "remote: https://<redacted>@github.com/x/y");
    assert!(!out.contains("ghp_tok123"));
}

#[cfg(unix)]
#[test]
fn test_check_output_redacts_stderr_on_failure() {
    // Stderr from a failing subprocess must be redacted before
    // the logger surfaces it, so secrets present in `output.stderr`
    // never reach the eprintln sink (or any future log appender).
    let log = StageLogger::new("test", Verbosity::Normal).with_env(vec![(
        "REGISTRY_PASSWORD".to_string(),
        "supersecret_pw_123".to_string(),
    )]);
    let output = fake_output(
        b"",
        b"docker login failed: invalid password 'supersecret_pw_123'",
        1,
    );
    let (stderr_line, _) = log.format_output_lines(&output, "docker login");
    let line = stderr_line.expect("stderr should be present on failure");
    assert!(
        !line.contains("supersecret_pw_123"),
        "stderr must be redacted: {line}"
    );
    assert!(line.contains("$REGISTRY_PASSWORD"));
}

#[cfg(unix)]
#[test]
fn test_check_output_redacts_stdout_on_failure() {
    // Stdout on the failure path must be redacted alongside
    // stderr. Some tools dump credentials onto stdout (e.g. helm
    // login prints a warning to stdout, not stderr).
    let log = StageLogger::new("test", Verbosity::Normal).with_env(vec![(
        "DOCKER_PASSWORD".to_string(),
        "tok_dckr_abc".to_string(),
    )]);
    let output = fake_output(b"echoed config: DOCKER_PASSWORD=tok_dckr_abc\n", b"", 2);
    let (_, stdout_line) = log.format_output_lines(&output, "docker");
    let line = stdout_line.expect("stdout should be present on failure");
    assert!(!line.contains("tok_dckr_abc"));
    assert!(line.contains("$DOCKER_PASSWORD"));
}

#[cfg(unix)]
#[test]
fn test_check_output_redacts_stdout_on_verbose_success() {
    // At verbose level, successful subprocess stdout is logged
    // too; it must also be redacted.
    let log = StageLogger::new("test", Verbosity::Verbose).with_env(vec![(
        "MY_API_KEY".to_string(),
        "key-abcdef-123".to_string(),
    )]);
    let output = fake_output(b"echo: key-abcdef-123 OK\n", b"", 0);
    let (_, stdout_line) = log.format_output_lines(&output, "echo");
    let line = stdout_line.expect("stdout should be present on success");
    assert!(!line.contains("key-abcdef-123"));
    assert!(line.contains("$MY_API_KEY"));
}

#[cfg(unix)]
#[test]
fn test_check_output_strips_inline_url_credentials_without_env() {
    // A logger built without env still strips URL credentials,
    // so even when the user did not export a matching env var, an
    // inline `https://<user>:<pw>@host` in stderr is scrubbed.
    let log = StageLogger::new("test", Verbosity::Normal);
    let output = fake_output(
        b"",
        b"fatal: cannot read https://user:p4ssw0rd@example.com/repo.git\n",
        128,
    );
    let (stderr_line, _) = log.format_output_lines(&output, "git fetch");
    let line = stderr_line.expect("stderr should be present on failure");
    assert!(
        !line.contains("p4ssw0rd"),
        "userinfo must be redacted: {line}"
    );
    assert!(line.contains("<redacted>@example.com"));
}

#[cfg(unix)]
#[test]
fn test_check_output_bail_message_excludes_raw_secret() {
    // The bail message embeds the (truncated, redacted) stderr tail
    // so an operator reading the bubbled anyhow chain sees something
    // more actionable than the bare exit code. That redaction must
    // still strip env-resolved secrets — otherwise the new tail
    // would leak whatever stderr the subprocess emitted.
    let log = StageLogger::new("test", Verbosity::Normal).with_env(vec![(
        "AUTH_TOKEN".to_string(),
        "secret_zzz_yyy".to_string(),
    )]);
    let output = fake_output(b"", b"401 Unauthorized: secret_zzz_yyy\n", 1);
    let err = log
        .check_output(output, "curl")
        .expect_err("non-zero exit should bail");
    let msg = format!("{err:#}");
    assert!(
        !msg.contains("secret_zzz_yyy"),
        "bail message leaks secret: {msg}"
    );
    assert!(
        msg.contains("stderr:") && msg.contains("401 Unauthorized"),
        "bail message should embed redacted stderr tail: {msg}"
    );
}

#[cfg(unix)]
#[test]
fn test_check_output_bail_message_strips_ansi_color_codes() {
    // Color is forced on for child processes so the live CI log stays
    // colorized; cargo (and friends) then emit SGR escapes around versions,
    // paths, and numbers. The bubbled tail flows into failure-notification
    // emails and the on_error hook's $ANODIZER_ERROR, which render raw ANSI
    // as garbage — so the persisted error must carry plain text only.
    let log = StageLogger::new("test", Verbosity::Normal);
    // cargo-shaped colorized stderr: bold version, dimmed path, red error.
    let colorized =
        b"\x1b[1mPackaging\x1b[0m foo \x1b[2mv\x1b[1m0.11.3\x1b[0m\n\x1b[31merror\x1b[0m: exit \x1b[33m101\x1b[0m\n";
    let output = fake_output(b"", colorized, 101);
    let err = log
        .check_output(output, "cargo publish")
        .expect_err("non-zero exit should bail");
    let msg = format!("{err:#}");
    assert!(
        !msg.contains('\u{1b}'),
        "bail message must contain no ANSI escape bytes: {msg:?}"
    );
    assert!(
        msg.contains("Packaging") && msg.contains("0.11.3") && msg.contains("101"),
        "plain-text content must survive ANSI stripping: {msg}"
    );
}

#[cfg(unix)]
#[test]
fn test_check_output_bail_includes_no_stderr_marker_when_empty() {
    // Subprocess failed with empty stderr — the bail still wants
    // SOMETHING after `stderr:` so a grep on operator logs sees a
    // deterministic marker rather than blank text.
    let log = StageLogger::new("test", Verbosity::Normal);
    let output = fake_output(b"", b"", 7);
    let err = log
        .check_output(output, "tool")
        .expect_err("non-zero exit should bail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("stderr: <no stderr>"),
        "expected explicit <no stderr> marker: {msg}"
    );
}

#[cfg(unix)]
#[test]
fn test_check_output_bail_truncates_long_stderr() {
    // Stderr larger than the 2 KiB cap is truncated with an ellipsis
    // so the operator's error chain remains scannable.
    let log = StageLogger::new("test", Verbosity::Normal);
    // 3 KiB of stderr.
    let big = vec![b'x'; 3072];
    let output = fake_output(b"", &big, 1);
    let err = log
        .check_output(output, "tool")
        .expect_err("non-zero exit should bail");
    let msg = format!("{err:#}");
    assert!(
        msg.ends_with('…'),
        "expected ellipsis on truncated stderr: {msg}"
    );
    // Truncation must keep the surface manageable — well under
    // 3 KiB of raw stderr should make it into the bail.
    assert!(
        msg.len() < 2500,
        "bail message too long: {} bytes",
        msg.len()
    );
}

#[test]
fn test_with_env_is_arc_shared() {
    // Cloning a logger should share the env cell via Arc, not deep-copy.
    // Verified by `Arc::ptr_eq` on the shared `Arc<Mutex<Vec<_>>>` cell.
    let env = vec![("K".to_string(), "v_long_enough_to_be_a_token".to_string())];
    let a = StageLogger::new("a", Verbosity::Normal).with_env(env);
    let b = a.clone();
    assert!(Arc::ptr_eq(
        a.env.as_ref().unwrap(),
        b.env.as_ref().unwrap()
    ));
}

#[test]
fn test_with_stage_rebinds_stage_field() {
    // The per-line `[stage]` tag is gone from rendered output, but
    // `with_stage` still rebinds the `stage` field a logger carries (it
    // drives redaction env inheritance, not line formatting now).
    let log = StageLogger::new("release", Verbosity::Normal);
    assert_eq!(log.stage, "release");
    assert_eq!(log.with_stage("finalize").stage, "finalize");
}

#[test]
fn test_body_markers_render_at_body_indent() {
    // Body lines sit at the 3-space body indent (top level: no section
    // nesting) behind a colored marker glyph. ANSI codes are stripped
    // for the assertion so the test pins the visible shape, not palette.
    let _guard = SECTION_TEST_LOCK.lock().unwrap();
    let strip = |s: String| {
        // Drop CSI sequences so the assertion is palette-independent.
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' {
                for n in chars.by_ref() {
                    if n == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    };
    // Relative to the live indent so an exported ANODIZER_LOG_DEPTH
    // (or a section left open by a parallel test) cannot skew the
    // absolute column.
    let prefix = indent();
    assert_eq!(
        strip(StageLogger::render_body(MARKER_DETAIL, "x")),
        format!("{prefix}   • x")
    );
    assert_eq!(
        strip(StageLogger::render_body(MARKER_SUCCESS, "ok")),
        format!("{prefix}   ✓ ok")
    );
    assert_eq!(
        strip(StageLogger::render_body(MARKER_FAILURE, "bad")),
        format!("{prefix}   ✗ bad")
    );
}

#[test]
fn test_kv_pads_plain_key_so_values_align() {
    // The padded key width counts the PLAIN key, not the ANSI-dimmed
    // bytes, so a short key and a long key share the same value column.
    // Emitting a body line drains the process-global PENDING stack via
    // `flush_pending`, so serialize against the section-depth tests that
    // assert on that stack.
    let _guard = SECTION_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (log, cap) = StageLogger::with_capture("check", Verbosity::Normal);
    let w = ["targets", "runs"].iter().map(|k| k.len()).max().unwrap();
    log.kv("targets", "aarch64", w);
    log.kv("runs", "2", w);
    // The capture stores a normalized `key = value` form regardless of
    // the rendered padding/palette.
    assert_eq!(
        cap.all_messages(),
        vec![
            (LogLevel::Status, "targets = aarch64".to_string()),
            (LogLevel::Status, "runs = 2".to_string()),
        ]
    );
}

#[test]
fn test_retag_helpers_record_under_shared_capture() {
    // The retagged clone shares the capture sink, and the plain
    // delegations still record at the right level — locking the plumbing
    // independent of the rendered tag (which the capture does not store).
    // Emitting body lines drains the global PENDING stack via
    // `flush_pending`; serialize against the section-depth tests.
    let _guard = SECTION_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (log, cap) = StageLogger::with_capture("release", Verbosity::Normal);

    log.with_stage("finalize").status("x");
    log.error("y");
    log.status("own-status");
    log.error("own-error");

    assert_eq!(
        cap.all_messages(),
        vec![
            (LogLevel::Status, "x".to_string()),
            (LogLevel::Error, "y".to_string()),
            (LogLevel::Status, "own-status".to_string()),
            (LogLevel::Error, "own-error".to_string()),
        ]
    );
}

#[test]
fn skip_line_records_debug_when_not_shown() {
    // The default (show=false) routes a per-crate "no config block" skip to
    // debug() so it stays invisible at Normal/Verbose and only surfaces at
    // --debug — the fix for the 300+-line workspace skip-noise problem.
    // skip_line emits a body line that drains the global PENDING stack;
    // serialize against the section-depth tests.
    let _guard = SECTION_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (log, cap) = StageLogger::with_capture("homebrew", Verbosity::Normal);
    log.skip_line(
        false,
        "skipped homebrew for crate 'demo' — no homebrew config block",
    );
    assert_eq!(cap.debug_count(), 1);
    assert_eq!(cap.status_count(), 0);
    assert_eq!(
        cap.all_messages(),
        vec![(
            LogLevel::Debug,
            "skipped homebrew for crate 'demo' — no homebrew config block".to_string()
        )]
    );
}

#[test]
fn skip_line_records_status_when_shown() {
    // --show-skipped (show=true) forces the skip line back to status so the
    // operator can diagnose why a publisher didn't run for a given crate.
    // skip_line emits a body line that drains the global PENDING stack;
    // serialize against the section-depth tests.
    let _guard = SECTION_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (log, cap) = StageLogger::with_capture("homebrew", Verbosity::Normal);
    log.skip_line(
        true,
        "skipped homebrew for crate 'demo' — no homebrew config block",
    );
    assert_eq!(cap.status_count(), 1);
    assert_eq!(cap.debug_count(), 0);
}
