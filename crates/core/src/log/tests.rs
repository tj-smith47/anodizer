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
    // own depth. (`flush_pending` writes the header to stderr, so the
    // assertion is on the state transition, not the uncapturable eprintln.)
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
/// whose output could end up in the capture: itself and its `stderr_fd`
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
    // gutter labels with NO trailing colon, and their message must end up in
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
        // message ends up in the same column as the "Building" header's,
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

/// The two child-stream tee helpers have no production caller today — every
/// live path goes through `stream_child_chunk` — so nothing observed that they
/// still redact, still terminate the line, and still record at their own
/// levels. They are `pub` on a published crate, so this is the pin.
#[test]
fn stream_child_helpers_redact_and_record_at_their_own_level() {
    let _guard = SECTION_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (log, cap) = StageLogger::with_capture("build", Verbosity::Normal);
    let log = log.with_env(vec![(
        "API_TOKEN".to_string(),
        "ghp_streamsecret".to_string(),
    )]);

    log.stream_child_stdout("out ghp_streamsecret at https://u:p@example.com/x");
    log.stream_child_stderr("err ghp_streamsecret");

    assert_eq!(
        cap.all_messages(),
        vec![
            (
                LogLevel::Verbose,
                "out $API_TOKEN at https://<redacted>@example.com/x".to_string()
            ),
            (LogLevel::Error, "err $API_TOKEN".to_string()),
        ]
    );
}

/// The line tee and the chunk tee mask one text identically: the env-value half
/// first, the URL half exactly once. A secret whose value IS a credential URL
/// is the case that separates them — stripping the credentials first leaves the
/// env half nothing to recognise, so the variable name never appears.
#[test]
fn the_line_tee_and_the_chunk_tee_mask_a_credential_url_secret_alike() {
    let _guard = SECTION_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (log, cap) = StageLogger::with_capture("build", Verbosity::Normal);
    let log = log.with_env(vec![(
        "REPO_TOKEN".to_string(),
        "https://bot:s3cret@example.com".to_string(),
    )]);

    log.stream_child_stdout("clone https://bot:s3cret@example.com");
    let mut redacter = log.stream_redacter();
    let mut chunk = redacter.push("clone https://bot:s3cret@example.com");
    chunk.push_str(&redacter.flush());
    log.stream_child_chunk(&chunk, false);

    assert_eq!(
        cap.all_messages(),
        vec![
            (LogLevel::Verbose, "clone $REPO_TOKEN".to_string()),
            (LogLevel::Verbose, "clone $REPO_TOKEN".to_string()),
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

/// Every production function that spawns a `Command` carrying an env of its
/// own AND hands that child's output to a logger or an error must log
/// through [`StageLogger::with_child_env`]: the logger's own table was built
/// from the process env, so a secret a rendered `env:` plants on the child
/// reaches the live tee, the failure embed and the `running …` echo unmasked.
///
/// Trigger, per function body, after the shared scanner has split the
/// workspace's production sources into bodies:
///
/// - the body sets an env on a `Command`: `.envs(`, `.env_clear(`, or an
///   `.env(` whose VALUE argument is not a string literal. `::env` (as in
///   `std::env::var`) is excluded by the character before the dot; `.env.`
///   and `.env_var(` never match the spellings asked for. A literal value
///   (`.env("LC_ALL", "C")`, `.env("GIT_TERMINAL_PROMPT", "0")`) is no
///   secret, and a body that sets only those is outside the class — that
///   is what keeps the thirty-odd git helpers out of the walk. Known
///   negative: a literal value that is a secret written in source, which
///   this workspace does not do and a review catches.
/// - or the body calls a BUILDER: a function whose signature returns a
///   `Command` and whose own body sets such an env. The command a builder
///   hands back carries the env just the same, and a spawner that never
///   spells `.env(` itself would otherwise be invisible. The builders are
///   found by the walk and their names pinned.
/// - and the body reads the child's output somewhere a line can be printed:
///   any run helper of `crate::run` (`RUN_HELPERS`), `check_output(`,
///   `.redact(` or a `.stderr)` read (`from_utf8_lossy(&output.stderr)`).
///   Known positive: a `.stderr)` read that is compared and never printed;
///   none exists today, and such a body is listed as an exemption rather
///   than relaxing the trigger.
///
/// A body in that population is CONVERTED when it mentions `with_child_env`
/// AND routes output through something that applies the table: a run
/// helper, `check_output(` or `.redact(`. Naming the child logger is not
/// enough — `status`, `verbose`, `warn` and `error` print their message as
/// given, so `log.with_child_env(&cmd).warn(&stderr)` masks nothing.
///
/// A body that is not converted fails, unless it is listed in `EXEMPT` with
/// the reason it needs no child table: either it has no logger and scrubs
/// its own error embed, or the only env it sets is a git author identity
/// read from config.
#[test]
fn every_spawn_with_its_own_env_logs_through_the_child_env_redactor() {
    use crate::test_helpers::test_sources::{
        function_bodies, production_half, workspace_production_sources,
    };

    // Functions returning a `Command` that already carries a non-literal env.
    const BUILDERS: [&str; 5] = [
        "build_fetch_command",
        "build_npm_publish_command",
        "build_subprocess_command",
        "whitelisted",
        "whitelisted_with_env",
    ];
    // (file suffix, fn name, why no child table is needed)
    const EXEMPT: [(&str, &str, &str); 8] = [
        (
            "core/src/hooks.rs",
            "run_hooks_inner",
            "composes a superset table with `with_env(effective_env)` before the Command exists, because the dry-run line needs it",
        ),
        (
            "core/src/git/github_api.rs",
            "gh_api_get_with_binary_with_env",
            "no logger; scrubs the token out of its bail! with `redact_gh_stderr_with_env`",
        ),
        (
            "core/src/git/github_api.rs",
            "gh_api_delete_with_binary",
            "no logger; scrubs the token out of its bail! with `redact_gh_stderr_with_env`",
        ),
        (
            "core/src/git/github_api.rs",
            "gh_api_get_paginated_with_binary",
            "no logger; scrubs the token out of its bail! with `redact_gh_stderr_with_env`",
        ),
        (
            "stage-docker/src/run/manifest.rs",
            "process_docker_manifest",
            "the one command it spawns, `manifest rm`, has its output discarded; its `.redact(` is the dry-run echo, masked through `with_job_env`",
        ),
        (
            "stage-publish/src/util/cmd.rs",
            "run_cmd_in_envs",
            "no logger; its one env-carrying caller passes a git author identity from config, never a secret",
        ),
        (
            "stage-publish/src/util/git_revert.rs",
            "revert_commit_in",
            "no logger; sets only the git author identity from config",
        ),
        (
            "stage-sign/src/verify.rs",
            "derive_cosign_public_key",
            "no logger; scrubs its bail! with `redact::string` over the job env plus the process env",
        ),
    ];

    let exec = include_str!("../run/exec.rs");
    let declared: Vec<String> = exec
        .lines()
        .filter_map(|l| l.strip_prefix("pub fn "))
        .map(|rest| {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            format!("{name}(")
        })
        .collect();
    let mut expected: Vec<String> = RUN_HELPERS.iter().map(|s| s.to_string()).collect();
    let mut found = declared.clone();
    expected.sort();
    found.sort();
    assert_eq!(
        found, expected,
        "RUN_HELPERS must list every pub fn of crate::run's exec API"
    );

    let mut bodies: Vec<(String, String, String)> = Vec::new();
    for source in workspace_production_sources() {
        let text = std::fs::read_to_string(&source).expect("read source");
        let path = source.to_string_lossy().replace('\\', "/");
        for body in function_bodies(production_half(&text)) {
            bodies.push((path.clone(), fn_name(&body), body));
        }
    }

    // Transitive: a function that returns the command another builder built
    // hands on the same env.
    let mut builders: Vec<&str> = Vec::new();
    loop {
        let before = builders.len();
        for (_, name, body) in &bodies {
            if !builders.contains(&name.as_str())
                && returns_a_command(body)
                && (sets_command_env(body) || builders.iter().any(|b| calls(body, b)))
            {
                builders.push(name);
            }
        }
        if builders.len() == before {
            break;
        }
    }
    builders.sort_unstable();
    assert_eq!(
        builders, BUILDERS,
        "the functions returning a Command that carries its own env moved; \
         every spawner of one is in this pin's population"
    );

    let reads_output = |body: &str| {
        RUN_HELPERS
            .iter()
            .chain(&REDACTING)
            .chain(&[".stderr)"])
            .any(|s| body.contains(s))
    };
    let mut checked = 0usize;
    let mut converted = 0usize;
    let mut exempt_seen = Vec::new();
    let mut offenders = Vec::new();
    let mut traced = Vec::new();
    for (path, name, body) in &bodies {
        let spawns_a_built_command = BUILDERS.iter().any(|b| b != name && calls(body, b));
        if !(sets_command_env(body) || spawns_a_built_command) || !reads_output(body) {
            continue;
        }
        checked += 1;
        // A tracing event prints its fields as given: no redaction table
        // reaches it, so a rendered value in one is printed unmasked.
        if TRACING_EVENTS.iter().any(|m| body.contains(m))
            && !TRACING_ALLOWED
                .iter()
                .any(|(suffix, fn_name, _)| path.ends_with(suffix) && fn_name == name)
        {
            traced.push(format!("{path}: fn {name}"));
        }
        if is_converted(body) {
            converted += 1;
            continue;
        }
        if let Some(entry) = EXEMPT
            .iter()
            .find(|(suffix, fn_name, _)| path.ends_with(suffix) && fn_name == name)
        {
            exempt_seen.push(*entry);
            continue;
        }
        offenders.push(format!("{path}: fn {name}"));
    }
    assert!(
        offenders.is_empty(),
        "these functions spawn a Command with its own env and print its output \
         without routing it through `StageLogger::with_child_env` and a \
         redacting call (a run helper, `check_output` or `.redact(`):\n{}",
        offenders.join("\n")
    );
    assert!(
        traced.is_empty(),
        "these functions spawn a Command with its own env and emit a tracing \
         event, which no redaction table masks; print through the job or \
         child logger's `.redact(` instead:\n{}",
        traced.join("\n")
    );
    assert_eq!(
        exempt_seen.len(),
        EXEMPT.len(),
        "every exemption must still name a live site; stale: {:?}",
        EXEMPT
            .iter()
            .filter(|e| !exempt_seen.contains(e))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        checked,
        converted + EXEMPT.len(),
        "the population is every converted site plus every exemption"
    );
    assert_eq!(
        converted, 25,
        "the count of sites logging through with_child_env moved; update \
         .claude/rules/child-env-redaction.md's population table"
    );
}

/// Every `pub fn` of `crate::run`'s exec API; the pin holds the list to the
/// module's own source.
const RUN_HELPERS: [&str; 6] = [
    "run_checked(",
    "run_checked_with_stdin(",
    "run_checked_with_stdin_timeout(",
    "run_checked_timeout(",
    "run_capture_timeout(",
    "run_capture(",
];

/// The tracing event macros, which print their fields with no redaction.
const TRACING_EVENTS: [&str; 5] = [
    "tracing::trace!(",
    "tracing::debug!(",
    "tracing::info!(",
    "tracing::warn!(",
    "tracing::error!(",
];

/// (file suffix, fn name, why its tracing events carry no rendered value)
const TRACING_ALLOWED: [(&str, &str, &str); 2] = [
    (
        "core/src/hooks.rs",
        "run_hooks_inner",
        "its two debug events carry the hook's `cmd:` as written in the config, before any render",
    ),
    (
        "core/src/git/github_api.rs",
        "gh_api_get_paginated_with_binary",
        "no logger; its one warn event prints a body snippet scrubbed with `redact_process_env`",
    ),
];

/// The logger calls that apply the redaction table to what they are handed.
const REDACTING: [&str; 2] = ["check_output(", ".redact("];

/// Whether `body` both names the child logger and routes output through
/// something that applies its table. Naming the logger alone is not enough.
fn is_converted(body: &str) -> bool {
    body.contains("with_child_env")
        && RUN_HELPERS
            .iter()
            .chain(&REDACTING)
            .any(|s| body.contains(s))
}

/// Whether `body` calls the function `name`, as a whole word.
fn calls(body: &str, name: &str) -> bool {
    let needle = format!("{name}(");
    body.match_indices(&needle).any(|(at, _)| {
        !body[..at]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
    })
}

/// Whether the signature of a body the shared scanner produced returns a
/// `Command`, bare or inside a `Result`.
fn returns_a_command(body: &str) -> bool {
    let signature = body.split_once('{').map(|(sig, _)| sig).unwrap_or(body);
    signature
        .split_once("->")
        .is_some_and(|(_, ret)| ret.contains("Command"))
}

/// A body that names the child logger and only prints through it is not
/// converted: `warn` prints its message as given.
#[test]
fn naming_the_child_logger_without_a_redacting_route_is_not_a_conversion() {
    assert!(!is_converted(
        "fn f() { log.with_child_env(&cmd).warn(&String::from_utf8_lossy(&o.stderr)); }"
    ));
    assert!(is_converted(
        "fn f() { let s = log.with_child_env(&cmd).redact(&String::from_utf8_lossy(&o.stderr)); log.warn(&s); }"
    ));
    assert!(is_converted(
        "fn f() { let log = log.with_child_env(&cmd); run_checked_with_stdin(&mut cmd, b, &log, l)?; }"
    ));
    assert!(returns_a_command("fn b(a: &str) -> Command {\n    x\n}"));
    assert!(returns_a_command(
        "fn b() -> Result<std::process::Command> {\n}"
    ));
    assert!(!returns_a_command(
        "fn b(cmd: &mut Command) -> Result<()> {\n}"
    ));
    assert!(calls("let c = whitelisted(&args)?;", "whitelisted"));
    assert!(!calls("if is_whitelisted(&args) {", "whitelisted"));
}

/// The first `fn` name in a body the shared scanner produced.
fn fn_name(body: &str) -> String {
    let after = body.split_once("fn ").map(|(_, rest)| rest).unwrap_or("");
    after
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect()
}

/// Whether `body` sets an env on a `Command` with a value that is not a
/// string literal (see the doc comment on the pin above).
fn sets_command_env(body: &str) -> bool {
    let bytes = body.as_bytes();
    let mut from = 0;
    while let Some(at) = body[from..].find(".env") {
        let start = from + at;
        from = start + 4;
        if start > 0 && bytes[start - 1] == b':' {
            continue;
        }
        let rest = &body[start + 4..];
        if rest.starts_with("s(") || rest.starts_with("_clear(") {
            return true;
        }
        let Some(args) = rest.strip_prefix('(') else {
            continue;
        };
        // The value is whatever follows the first top-level comma.
        let mut depth = 0usize;
        let mut value = None;
        for (i, c) in args.char_indices() {
            match c {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' if depth == 0 => break,
                ')' | ']' | '}' => depth -= 1,
                ',' if depth == 0 => {
                    value = Some(args[i + 1..].trim_start());
                    break;
                }
                _ => {}
            }
        }
        if let Some(value) = value
            && !value.starts_with('"')
        {
            return true;
        }
    }
    false
}

#[test]
fn the_command_env_trigger_tells_a_literal_value_from_a_variable() {
    assert!(!sets_command_env(
        r#"cmd.env("LC_ALL", "C").env("GIT_TERMINAL_PROMPT", "0")"#
    ));
    assert!(!sets_command_env("std::env::var(\"X\")"));
    assert!(!sets_command_env("ctx.env_var(\"X\")"));
    assert!(!sets_command_env("ctx.env.get(\"X\")"));
    assert!(sets_command_env("cmd.env(k, v)"));
    assert!(sets_command_env("cmd.env(\"TOKEN\", &tok)"));
    assert!(sets_command_env("cmd.env(format!(\"{k}\"), v)"));
    assert!(sets_command_env("cmd.envs(pairs)"));
    assert!(sets_command_env("cmd.env_clear()"));
}
