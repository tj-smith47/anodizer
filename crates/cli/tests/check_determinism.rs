//! Integration tests for `anodize check determinism`.
//!
//! The fast tests below cover the CLI surface and the harness error
//! paths that don't require a real `cargo build`. The drift-injection
//! integration test (`inject_drift_archive_reports_drift_on_minimal_workspace`)
//! synthesizes a minimal cargo workspace and exercises the full
//! harness end-to-end; it is feature-gated on `cargo` being present on
//! `PATH` and skipped (with an eprintln) otherwise to keep the suite
//! green on hosts without a Rust toolchain.
//!
//! ## Manual integration runs (not driven by `cargo test`)
//!
//! Cases not covered automatically — kept here so an operator can
//! reproduce ad-hoc:
//!
//! ### Full N-runs harness against a fixture workspace
//!
//! ```text
//! cd <fixture-workspace>
//! anodize check determinism --runs=1 --report=det.json
//! test -f det.json && jq .schema_version det.json == 1
//! ```
//!
//! ### Drift-injection round-trip (production binary)
//!
//! ```text
//! ANODIZE_TEST_HARNESS=1 anodize check determinism \
//!   --runs=2 --inject-drift=archive
//! # Expected: exit code 1, report's drift_count > 0.
//! ```
//!
//! Both flows are covered automatically by the
//! `inject_drift_archive_reports_drift_on_minimal_workspace` test below
//! plus the unit tests in `crates/cli/src/determinism_harness.rs`. The
//! manual recipes survive here for operator debugging on hosts whose
//! `cargo`/`rustup` configuration differs from CI.

use anodizer_core::DeterminismReport;
use std::fs;
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
    for flag in &[
        "--runs",
        "--stages",
        "--report",
        "--snapshot",
        "--no-snapshot",
        "--preserve-dist",
    ] {
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
/// SDE resolver". A full N-runs harness test is below
/// (`inject_drift_archive_reports_drift_on_minimal_workspace`).
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

/// `--inject-drift=<stage>` is a hidden test-harness flag — it must be
/// rejected when `ANODIZE_TEST_HARNESS=1` is not set. This guards the
/// production-release surface: an operator who accidentally types the
/// flag gets a hard error rather than silent test-mode behaviour.
#[test]
fn inject_drift_rejected_without_test_harness_env() {
    let tmp = TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "check",
            "determinism",
            "--runs",
            "1",
            "--inject-drift",
            "archive",
        ])
        .current_dir(tmp.path())
        .env_remove("ANODIZE_TEST_HARNESS")
        .output()
        .expect("invoking anodize check determinism --inject-drift");

    assert!(
        !output.status.success(),
        "expected non-zero exit when --inject-drift is set without ANODIZE_TEST_HARNESS=1"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--inject-drift") && stderr.contains("ANODIZE_TEST_HARNESS"),
        "expected error citing both --inject-drift and ANODIZE_TEST_HARNESS; got: {}",
        stderr
    );
}

/// `--inject-drift=<stage>` is hidden from `--help` output. This
/// asserts the `hide = true` clap attribute is intact so a future
/// review can't accidentally promote the flag into the public surface.
#[test]
fn inject_drift_hidden_from_help() {
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "determinism", "--help"])
        .output()
        .expect("invoking anodize check determinism --help");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("--inject-drift"),
        "--inject-drift must not appear in --help output: {}",
        stdout
    );
}

// ── Drift-injection round-trip — fast (~1s with warm cache) ──────────────
//
// A fast integration test that synthesizes a tiny cargo workspace, runs
// the harness with `--runs=2 --inject-drift=archive`, and asserts the
// report shape + `drift_count > 0`. This test does that against a minimal
// no-deps `hello-world` binary crate.
//
// Cost: dominated by the harness's per-run `cargo build --release` (×2).
// A no-deps binary builds in ~0.2-0.5s warm, ~5-10s cold. Real
// measurements on this checkout: ~0.6s end-to-end (build + archive +
// sbom + sign + checksum × 2 runs + worktree setup + JSON serdes). Cold
// CI runs without a rustup toolchain cached will be slower but still
// well under a 30s "fast" budget.
//
// Skipped (with a `cargo test` warning line) when `cargo`/`git` aren't
// on PATH so the suite stays green on minimal hosts.

mod common;
use common::{bootstrap_minimal_cargo_repo, host_triple, run_git, tool_on_path};

/// End-to-end drift-injection integration test (I12). Synthesizes a
/// minimal cargo workspace, drives the harness with `--runs=2
/// --inject-drift=archive`, and asserts the JSON report records drift.
///
/// On hosts without `cargo` or `git` on PATH, prints a skip marker and
/// returns early so the suite stays green on minimal hosts.
#[test]
fn inject_drift_archive_reports_drift_on_minimal_workspace() {
    if !tool_on_path("cargo") || !tool_on_path("git") {
        eprintln!(
            "SKIP inject_drift_archive_reports_drift_on_minimal_workspace: \
             cargo or git missing from PATH"
        );
        return;
    }

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    bootstrap_minimal_cargo_repo(repo, "anodize-det-fixture");

    // RUSTUP_HOME / PATH propagation is the harness's responsibility —
    // `build_subprocess_env` defaults RUSTUP_HOME from the host's
    // HOME/USERPROFILE when unset, and `allow_listed_path` inherits the
    // host PATH verbatim. No per-test workaround needed.

    let report_path = repo.join("det.json");
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "check",
            "determinism",
            "--runs",
            "2",
            "--stages",
            "build,archive",
            "--inject-drift",
            "archive",
            "--report",
        ])
        .arg(&report_path)
        .current_dir(repo)
        .env("ANODIZE_TEST_HARNESS", "1")
        .output()
        .expect("invoking anodize check determinism");

    // Non-zero exit when drift is detected (the dispatcher calls
    // `process::exit(1)` after writing the report).
    assert!(
        !output.status.success(),
        "expected non-zero exit on drift; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        report_path.exists(),
        "report file missing at {}; stderr was: {}",
        report_path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    let json = fs::read_to_string(&report_path).unwrap();
    let report: DeterminismReport =
        serde_json::from_str(&json).unwrap_or_else(|e| panic!("parsing report JSON: {e}\n{json}"));

    assert_eq!(report.schema_version, 1, "schema_version pinned at 1");
    assert_eq!(report.runs, 2, "harness ran exactly --runs=2 times");
    assert!(
        report.drift_count > 0,
        "expected drift_count > 0 after --inject-drift=archive; report: {:?}\nstderr: {}",
        report,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !report.drift.is_empty(),
        "drift list non-empty alongside drift_count > 0"
    );

    // Sanity: at least one drift row is the archive itself (the target
    // of `--inject-drift=archive`). The other rows are transitive —
    // artifacts that record the archive's hash (e.g. metadata.json,
    // checksums.txt) propagate the byte-flip.
    assert!(
        report
            .drift
            .iter()
            .any(|d| d.artifact.ends_with(".tar.gz") || d.artifact.ends_with(".zip")),
        "at least one drift row should be an archive artifact; got: {:?}",
        report.drift.iter().map(|d| &d.artifact).collect::<Vec<_>>()
    );
}

/// Regression pin for the v0.9.0 release failure: the harness's replica
/// pipelines run on credential-less runners (the run paths skip what's
/// absent; nothing publishes), so the config-derived env preflight must be
/// disabled for the children by construction. The fixture mirrors the live
/// shape: HEAD at a tag (non-snapshot child), an nfpm apk signature whose
/// key env var is deliberately unset, and submitter publishers configured
/// `required: true`. Pre-fix this run aborted with
/// `Error: preflight: N check(s) failed` from determinism run 0.
///
/// Also pins the console contract:
/// - the `Checking determinism` header + parameter block print exactly once
///   (one formatter — the binary's; wrappers must not echo their own), and
/// - the static-config moderation-queue warnings print exactly once per
///   invocation (not once per config probe × replica build — they printed
///   4× in the live run).
#[test]
fn harness_skips_env_preflight_and_prints_header_and_config_warnings_once() {
    if !tool_on_path("cargo") || !tool_on_path("git") {
        eprintln!(
            "SKIP harness_skips_env_preflight_and_prints_header_and_config_warnings_once: \
             cargo or git missing from PATH"
        );
        return;
    }

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    bootstrap_minimal_cargo_repo(repo, "anodize-preflight-fixture");

    let host = host_triple();
    let yaml = format!(
        r#"crates:
  - name: anodize-preflight-fixture
    path: .
    tag_template: "v{{{{ Version }}}}"
    builds:
      - id: anodize-preflight-fixture
        binary: anodize-preflight-fixture
        targets:
          - {host}
    nfpms:
      - id: default
        formats:
          - apk
        maintainer: "Test <test@test.com>"
        description: "preflight fixture"
        apk:
          signature:
            key_file: "{{{{ .Env.APK_PRIVATE_KEY_PATH }}}}"
    publish:
      chocolatey:
        required: true
      winget:
        required: true
"#,
    );
    fs::write(repo.join(".anodizer.yaml"), yaml).unwrap();
    run_git(repo, &["add", "-A"]);
    run_git(repo, &["commit", "-q", "-m", "preflight fixture config"]);
    // HEAD at a tag → the harness auto-selects a NON-snapshot child (the
    // live tag-push shape), which is exactly the mode that ran the env
    // preflight pre-fix.
    run_git(repo, &["tag", "v0.1.0"]);

    let report_path = repo.join("det.json");
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "check",
            "determinism",
            "--runs",
            "2",
            "--stages",
            "build,archive,nfpm",
            "--report",
        ])
        .arg(&report_path)
        .current_dir(repo)
        // The unsatisfiable preflight requirement: the apk signature's
        // key env var is absent, as on a credential-less CI runner.
        .env_remove("APK_PRIVATE_KEY_PATH")
        .output()
        .expect("invoking anodize check determinism");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "harness must not preflight-fail in a credential-less env; \
         stdout={} stderr={stderr}",
        String::from_utf8_lossy(&output.stdout),
    );
    assert!(
        !stderr.contains("preflight:"),
        "no env-preflight failure may surface from replica children: {stderr}"
    );
    // Guard against a vacuous pass: the children must have actually driven
    // the build pipeline (one `Building binaries` section per run), not
    // short-circuited on the tags-at-HEAD no-op path.
    assert!(
        !stderr.contains("no release tags at HEAD"),
        "children must select the fixture crate from the tag, not no-op: {stderr}"
    );
    // (`running: cargo` rather than the `Building binaries` header — the
    // header's verb carries ANSI color codes between verb and message, so
    // the two words are not contiguous in raw stderr.)
    let builds = stderr.matches("running: cargo").count();
    assert_eq!(
        builds, 2,
        "expected one cargo build invocation per run (runs=2), got {builds}:\n{stderr}"
    );

    let header_count = stderr.matches("Checking determinism").count();
    assert_eq!(
        header_count, 1,
        "`Checking determinism` header must print exactly once, got {header_count}:\n{stderr}"
    );

    for publisher in ["chocolatey", "winget"] {
        let needle = format!("publisher '{publisher}' submits to an external moderation queue");
        let count = stderr.matches(needle.as_str()).count();
        assert_eq!(
            count, 1,
            "moderation-queue warning for {publisher} must print exactly once \
             per invocation, got {count}:\n{stderr}"
        );
    }
}
