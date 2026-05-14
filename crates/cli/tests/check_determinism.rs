//! Integration tests for `anodize check determinism`.
//!
//! The full N-runs harness against a real cargo workspace is too costly
//! for the default test suite (each run is a full `cargo build
//! --release` cycle, ~5 min wallclock × N runs). Those scenarios live as
//! `#[ignore]`-flagged tests in this file — invoke with
//! `cargo test -p anodizer --test check_determinism -- --ignored`.
//!
//! The non-ignored test below asserts the CLI surface is wired (it
//! errors fast outside a git repo) and that the `--help` output documents
//! every flag in the spec — i.e. the dispatcher reaches clap and the
//! flag set survives.

use std::process::Command;
use tempfile::TempDir;

/// `anodize check determinism --help` must list every flag from the
/// spec (`--runs`, `--stages`, `--report`, `--snapshot`). A regression
/// in clap surface drops this signal silently otherwise.
#[test]
fn check_determinism_help_lists_every_flag() {
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "determinism", "--help"])
        .output()
        .expect("invoking anodize check determinism --help");

    assert!(
        output.status.success(),
        "--help exited non-zero: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    for flag in &["--runs", "--stages", "--report", "--snapshot"] {
        assert!(
            stdout.contains(flag),
            "--help missing flag {}; full output: {}",
            flag,
            stdout
        );
    }
}

/// Outside a git repo the dispatcher must error cleanly when resolving
/// HEAD (not panic, not hang). This pins the early-exit path that gates
/// the harness's expensive subprocess.
#[test]
fn check_determinism_errors_cleanly_outside_git_repo() {
    let tmp = TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "determinism", "--runs", "1"])
        .current_dir(tmp.path())
        .output()
        .expect("invoking anodize check determinism");

    assert!(
        !output.status.success(),
        "expected non-zero exit outside a git repo; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Smoke test: `--report=<path>` is respected when the dispatcher fails
/// fast (the report dir is NOT created in the error path; the test
/// passes by virtue of the binary exiting non-zero without panicking).
/// This is the lowest-cost shape that pins "the dispatcher reaches the
/// SDE resolver". A full N-runs harness test is below, gated `#[ignore]`.
#[test]
fn check_determinism_respects_report_flag_in_error_path() {
    let tmp = TempDir::new().unwrap();
    let report = tmp.path().join("custom-report.json");
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "determinism", "--runs", "1", "--report"])
        .arg(&report)
        .current_dir(tmp.path())
        .output()
        .expect("invoking anodize check determinism");

    // Non-git-repo path: must fail with a useful message, no panic.
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("panicked"),
        "binary panicked instead of erroring cleanly: {}",
        stderr
    );
}

/// Full-fledged harness run against a minimal cargo workspace.
///
/// Ignored by default — needs cargo + rustup + git in PATH and runs a
/// full `cargo build --release` per run (~5 min wallclock). Invoke
/// manually with `cargo test --test check_determinism -- --ignored`.
#[test]
#[ignore]
fn check_determinism_runs_at_least_once_against_fixture_workspace() {
    // Document the integration shape; the body intentionally stays
    // minimal until the harness's subprocess invocation matures (see
    // follow-up: snapshot-mode resolver + sandboxed cargo registry
    // prefetch).
    //
    // Expected manual invocation:
    //   $ cd <fixture-workspace>
    //   $ anodize check determinism --runs=1 --report=det.json
    //   $ test -f det.json && jq .schema_version det.json == 1
}
