//! Integration test for `--stages=cargo-package`.
//!
//! Drives the determinism harness against a minimal cargo crate fixture
//! to verify the `.crate` tarball produced by `cargo package` is
//! byte-stable across runs. The harness invokes `cargo package
//! --workspace --no-verify --allow-dirty --no-metadata` inside a fresh
//! `git worktree add --detach` per run; the resulting `.crate` files
//! are copied into `<worktree>/dist/cargo-package/` so the harness's
//! existing artifact-discovery walker picks them up.
//!
//! Skips cleanly when `cargo` or `git` are missing from PATH so the
//! suite stays green on minimal hosts (the documentation-build job,
//! containers without rustup, etc.).
//!
//! Known non-determinism the harness's env workarounds address:
//! - **File mtimes in the tar** — `SOURCE_DATE_EPOCH` is exported via
//!   `build_subprocess_env`; cargo (≥ 1.74) canonicalizes mtimes.
//! - **tar member ordering** — cargo sorts entries (≥ 1.74).
//! - **`.cargo_vcs_info.json` sha + dirty flag** — each run starts from
//!   `git worktree add --detach <tmp> <commit>`, so the recorded sha
//!   matches and the worktree is clean before `cargo package`.
//!
//! Any residual drift after these workarounds surfaces as
//! `drift_count` greater than zero and the test fails with the report
//! content attached. The test does not encode "known broken"
//! tolerance — that would defeat the point of the regression
//! detector.

use anodizer_core::DeterminismReport;
use std::fs;
use std::process::Command;
use tempfile::TempDir;

mod common;
use common::{bootstrap_minimal_cargo_repo, tool_on_path};

/// End-to-end byte-stability assertion for the `cargo-package` stage.
///
/// Bootstraps a minimal binary crate, drives `anodize check
/// determinism --runs=2 --stages=cargo-package` against it, and
/// asserts:
/// - The harness exit is zero (byte-stable across runs).
/// - The JSON report's `stages_under_test` lists `cargo-package`.
/// - At least one artifact row has stage `cargo-package` with
///   `deterministic = true`.
/// - The per-run hash is populated (`hash.is_some()`).
///
/// Skips when `cargo` or `git` are missing.
#[test]
fn cargo_package_stage_is_byte_stable_on_minimal_repo() {
    if !tool_on_path("cargo") || !tool_on_path("git") {
        eprintln!(
            "SKIP cargo_package_stage_is_byte_stable_on_minimal_repo: \
             cargo or git missing from PATH"
        );
        return;
    }

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    bootstrap_minimal_cargo_repo(repo, "anodize-cargo-package-fixture");

    let report_path = repo.join("det.json");
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "check",
            "determinism",
            "--runs",
            "2",
            "--stages",
            "cargo-package",
            "--report",
        ])
        .arg(&report_path)
        .current_dir(repo)
        .output()
        .expect("invoking anodize check determinism --stages=cargo-package");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        report_path.exists(),
        "report file missing at {}; stdout={} stderr={}",
        report_path.display(),
        stdout,
        stderr
    );
    let json = fs::read_to_string(&report_path).unwrap();
    let report: DeterminismReport =
        serde_json::from_str(&json).unwrap_or_else(|e| panic!("parsing report JSON: {e}\n{json}"));

    assert_eq!(report.schema_version, 1);
    assert_eq!(report.runs, 2, "harness ran exactly --runs=2 times");
    assert!(
        report
            .stages_under_test
            .iter()
            .any(|s| s == "cargo-package"),
        "stages_under_test must include `cargo-package`: {:?}",
        report.stages_under_test
    );

    let crate_rows: Vec<_> = report
        .artifacts
        .iter()
        .filter(|a| a.stage == "cargo-package")
        .collect();
    assert!(
        !crate_rows.is_empty(),
        "expected at least one cargo-package artifact row; report.artifacts={:?}",
        report
            .artifacts
            .iter()
            .map(|a| (&a.name, &a.stage))
            .collect::<Vec<_>>()
    );
    // Pin: every emitted `.crate` row must end in `.crate`.
    for row in &crate_rows {
        assert!(
            row.name.ends_with(".crate"),
            "cargo-package row name must end in `.crate`, got {:?}",
            row.name
        );
    }

    // Per-run byte-stability: the harness must exit zero AND every
    // cargo-package row must be marked deterministic with a populated
    // single-hash field. A drift in either dimension is a regression
    // surface this test exists to catch.
    assert!(
        output.status.success(),
        "harness exited non-zero (drift detected); stderr={}\nreport={}",
        stderr,
        json
    );
    assert_eq!(
        report.drift_count, 0,
        "drift detected in cargo-package output; drift rows: {:?}",
        report.drift
    );
    for row in &crate_rows {
        assert!(
            row.deterministic,
            "cargo-package row `{}` must be deterministic; hashes={:?}",
            row.name, row.hashes
        );
        assert!(
            row.hash.is_some(),
            "deterministic cargo-package row `{}` must carry a single hash, not per-run array",
            row.name
        );
    }
}
