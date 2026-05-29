//! Integration tests for `anodize check determinism --preserve-dist=<path>`.
//!
//! The harness must, on a green run, copy `<worktree>/dist/**` from
//! run-0 to the operator-supplied destination and emit a `context.json`
//! manifest describing the artifact set. These tests synthesize a
//! minimal cargo workspace, drive the harness end-to-end with
//! `--preserve-dist=<tmp>`, and assert:
//!
//! 1. The dist tree was copied to <tmp>.
//! 2. `<tmp>/context.json` is present and round-trips through serde.
//! 3. Each file in `<tmp>/` has a SHA256 that matches the corresponding
//!    entry in `determinism.json:artifacts[].hash` — the load-bearing
//!    "preserved bytes match the determinism check" safety property.
//!
//! On hosts without `cargo` or `git` on PATH, these tests print a SKIP
//! marker and return early — same convention as `check_determinism.rs`.

use anodizer_core::DeterminismReport;
use std::fs;
use std::process::Command;
use tempfile::TempDir;

mod common;
use common::{bootstrap_minimal_cargo_repo, sha256_file, tool_on_path, walk_files};

/// Crate name for the preserve_dist tests' fixture workspace —
/// distinct from check_determinism's so the per-test cargo target dirs
/// don't share lock state.
const FIXTURE_CRATE_NAME: &str = "anodize-det-fixture-preserve";

/// `--preserve-dist=<tmp>` copies the dist tree AND emits a
/// `context.json` that round-trips through serde.
#[test]
fn preserve_dist_copies_dist_tree_and_emits_context_json() {
    if !tool_on_path("cargo") || !tool_on_path("git") {
        eprintln!(
            "SKIP preserve_dist_copies_dist_tree_and_emits_context_json: \
             cargo or git missing from PATH"
        );
        return;
    }

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    bootstrap_minimal_cargo_repo(repo, FIXTURE_CRATE_NAME);

    let preserved = repo.join("preserved-dist");
    let report_path = repo.join("det.json");
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "check",
            "determinism",
            "--runs",
            "2",
            "--stages",
            "build,archive",
            "--report",
        ])
        .arg(&report_path)
        .args(["--preserve-dist"])
        .arg(&preserved)
        .current_dir(repo)
        .output()
        .expect("invoking anodize check determinism --preserve-dist");

    assert!(
        output.status.success(),
        "expected zero exit on green run; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // The preserved-dist directory must exist and be non-empty.
    assert!(
        preserved.exists(),
        "preserved-dist missing at {}; stderr was: {}",
        preserved.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    let preserved_files = walk_files(&preserved);
    assert!(
        !preserved_files.is_empty(),
        "preserved-dist directory empty at {}",
        preserved.display()
    );

    // The fixture exercises `build,archive` so we expect at least one
    // archive file in the preserved tree (the harness's run-0 output
    // for a successful build).
    assert!(
        preserved_files
            .iter()
            .any(|(rel, _)| rel.ends_with(".tar.gz")
                || rel.ends_with(".zip")
                || rel.ends_with(".tar.xz")
                || rel.ends_with(".tar.zst")),
        "expected at least one archive in preserved-dist; files: {:?}",
        preserved_files.iter().map(|(r, _)| r).collect::<Vec<_>>()
    );

    // context.json must be present + parse through serde.
    let context_path = preserved.join("context.json");
    assert!(
        context_path.exists(),
        "context.json missing under preserved-dist at {}",
        context_path.display()
    );
    let context_bytes = fs::read(&context_path).expect("reading context.json");
    let context: serde_json::Value = serde_json::from_slice(&context_bytes).unwrap_or_else(|e| {
        panic!(
            "parsing context.json: {e}\n{}",
            String::from_utf8_lossy(&context_bytes)
        )
    });

    // Schema fields: artifacts, targets, version, commit.
    let artifacts = context
        .get("artifacts")
        .and_then(|v| v.as_array())
        .expect("context.json missing `artifacts` array");
    assert!(
        !artifacts.is_empty(),
        "context.json `artifacts` array is empty; full doc: {}",
        String::from_utf8_lossy(&context_bytes)
    );
    // Each entry has name / path / sha256 / size.
    for (i, entry) in artifacts.iter().enumerate() {
        for field in ["name", "path", "sha256", "size"] {
            assert!(
                entry.get(field).is_some(),
                "artifacts[{i}] missing `{field}`: {entry:?}"
            );
        }
        let sha = entry.get("sha256").and_then(|v| v.as_str()).unwrap();
        assert!(
            sha.starts_with("sha256:"),
            "sha256 field must carry the `sha256:` prefix: {sha}"
        );
    }
    assert!(
        context.get("targets").and_then(|v| v.as_array()).is_some(),
        "context.json missing `targets` array"
    );
    assert!(
        context.get("version").and_then(|v| v.as_str()).is_some(),
        "context.json missing `version` string"
    );
    let commit = context
        .get("commit")
        .and_then(|v| v.as_str())
        .expect("context.json missing `commit` string");
    assert!(
        !commit.is_empty(),
        "context.json `commit` must be the harness's commit SHA, got empty"
    );

    // Round-trip: re-serialize the parsed value and reparse — pins the
    // shape stable under serde even if a future contributor swaps the
    // serializer.
    let reserialized = serde_json::to_string_pretty(&context).expect("reserializing context.json");
    let reparsed: serde_json::Value =
        serde_json::from_str(&reserialized).expect("reparsing context.json");
    assert_eq!(
        context, reparsed,
        "context.json must round-trip through serde"
    );
}

/// Each file in `<preserved-dist>/` has a SHA256 that matches the
/// corresponding `determinism.json:artifacts[].hash` entry.
///
/// This pins the load-bearing safety property: the bytes the publish-
/// only flow ships are byte-identical to the bytes the determinism
/// check verified. A divergence here means the preservation copy lost
/// or corrupted a file.
#[test]
fn preserve_dist_bytes_match_determinism_report_hashes() {
    if !tool_on_path("cargo") || !tool_on_path("git") {
        eprintln!(
            "SKIP preserve_dist_bytes_match_determinism_report_hashes: \
             cargo or git missing from PATH"
        );
        return;
    }

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    bootstrap_minimal_cargo_repo(repo, FIXTURE_CRATE_NAME);

    let preserved = repo.join("preserved-dist");
    let report_path = repo.join("det.json");
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "check",
            "determinism",
            "--runs",
            "2",
            "--stages",
            "build,archive",
            "--report",
        ])
        .arg(&report_path)
        .args(["--preserve-dist"])
        .arg(&preserved)
        .current_dir(repo)
        .output()
        .expect("invoking anodize check determinism --preserve-dist");

    assert!(
        output.status.success(),
        "expected zero exit on green run; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Load determinism.json — every deterministic artifact has a `hash`
    // field. We index by basename (the harness's map key convention).
    let report_json = fs::read_to_string(&report_path).expect("reading determinism.json");
    let report: DeterminismReport =
        serde_json::from_str(&report_json).expect("parsing determinism.json");
    assert_eq!(report.drift_count, 0, "harness must green for this test");

    let by_name: std::collections::HashMap<&str, &str> = report
        .artifacts
        .iter()
        .filter_map(|a| a.hash.as_deref().map(|h| (a.name.as_str(), h)))
        .collect();
    assert!(
        !by_name.is_empty(),
        "determinism.json has no deterministic artifacts to verify against; \
         report: {report:?}"
    );

    // Walk preserved-dist and confirm each file's SHA matches the
    // report's recorded hash. The expected-not-in-report set is
    // bounded — only the manifest sidecars below are valid misses;
    // any other miss indicates a regression in the harness's
    // discover/hash walk and MUST fail the test.
    //
    // - `context.json` is written by the preserve module POST-loop,
    //   after the determinism check finished, so it can't appear in
    //   the report's artifacts array.
    // - `metadata.json` / `artifacts.json` are written by the
    //   release pipeline's `run_post_pipeline`. They post-date the
    //   harness's `discover_artifacts` walk (which runs immediately
    //   after `run_build_pipeline` returns, but BEFORE post-pipeline
    //   bookkeeping fires inside the child), so they don't appear
    //   in `determinism.json:artifacts`.
    const EXPECTED_MISSES: &[&str] = &["context.json", "metadata.json", "artifacts.json"];

    let mut matched = 0usize;
    let mut expected_misses = 0usize;
    for (rel, abs) in walk_files(&preserved) {
        let basename = abs
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if EXPECTED_MISSES.contains(&basename.as_str()) {
            expected_misses += 1;
            continue;
        }
        // Files copied by preserve_raw_binaries land at
        // `_preserved-bin/<triple>/<basename>`, but determinism.json records
        // them under the cargo `target/<triple>/release/<basename>` shape.
        // Map back so the hash lookup hits.
        let lookup_key: String = if let Some(rest) = rel.strip_prefix("_preserved-bin/") {
            match rest.split_once('/') {
                Some((triple, name)) => format!("target/{triple}/release/{name}"),
                None => basename.clone(),
            }
        } else {
            basename.clone()
        };
        let expected = by_name.get(lookup_key.as_str()).unwrap_or_else(|| {
            panic!(
                "preserved file {} (basename {}, lookup_key {}) has no matching entry in \
                 determinism.json; expected one of the bounded EXPECTED_MISSES set or a \
                 hashed artifact. Preserved tree: {:?}\nReport artifacts: {:?}",
                rel,
                basename,
                lookup_key,
                walk_files(&preserved)
                    .iter()
                    .map(|(r, _)| r)
                    .collect::<Vec<_>>(),
                report.artifacts.iter().map(|a| &a.name).collect::<Vec<_>>(),
            )
        });
        let actual = sha256_file(&abs);
        assert_eq!(
            &actual, expected,
            "preserved file {} (basename {}) SHA mismatch:\n  preserved: {}\n  report:    {}",
            rel, basename, actual, expected
        );
        matched += 1;
    }

    // Tight floor: matched + expected_misses MUST account for every
    // file in the preserved tree. A regression where a file silently
    // stops appearing in the report (e.g. the discover walk drops
    // `.sha256` sidecars) would shift it out of `matched` AND out of
    // `EXPECTED_MISSES`, tripping the panic above. This assertion is
    // the belt to that suspender — it locks the count.
    let total_files = walk_files(&preserved).len();
    assert_eq!(
        matched + expected_misses,
        total_files,
        "matched ({matched}) + expected_misses ({expected_misses}) must equal \
         total preserved files ({total_files}); a file is escaping both buckets"
    );
    assert!(
        matched > 0,
        "no preserved files matched determinism.json hashes; preserved tree: {:?}; \
         report artifacts: {:?}",
        walk_files(&preserved)
            .iter()
            .map(|(r, _)| r)
            .collect::<Vec<_>>(),
        report.artifacts.iter().map(|a| &a.name).collect::<Vec<_>>()
    );
}

/// On drift detection, the preserved-dist directory must NOT survive —
/// shippable bytes cannot escape a failed determinism check. Spec
/// safety property: "The harness only succeeds (exit zero) when
/// drift_count == 0 ... shipped bytes are EXACTLY the bytes the
/// harness compared."
///
/// Drives the harness with `--inject-drift=archive` so it deliberately
/// drifts, then asserts the preserved-dist tree was removed.
#[test]
fn preserve_dist_removed_on_drift_detection() {
    if !tool_on_path("cargo") || !tool_on_path("git") {
        eprintln!(
            "SKIP preserve_dist_removed_on_drift_detection: \
             cargo or git missing from PATH"
        );
        return;
    }

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    bootstrap_minimal_cargo_repo(repo, FIXTURE_CRATE_NAME);

    let preserved = repo.join("preserved-dist");
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
        .args(["--preserve-dist"])
        .arg(&preserved)
        .current_dir(repo)
        .env("ANODIZE_TEST_HARNESS", "1")
        .output()
        .expect("invoking anodize check determinism --preserve-dist --inject-drift");

    // Drift → non-zero exit.
    assert!(
        !output.status.success(),
        "expected non-zero exit on injected drift; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // The preserved-dist directory MUST be gone (or never have
    // received a context.json) — the harness removes it post-drift so
    // a downstream consumer can't accidentally publish bytes from a
    // failed determinism check.
    assert!(
        !preserved.exists(),
        "preserved-dist must not survive a drifted run; remaining tree: {:?}",
        walk_files(&preserved)
            .iter()
            .map(|(r, _)| r)
            .collect::<Vec<_>>()
    );
}

/// FIX #2 regression test: the production end-to-end harness run must
/// emit a `context.json` with NON-EMPTY `targets` and `version`
/// fields. The release pipeline's `run_post_pipeline` writes
/// `dist/metadata.json` (`version`) and `dist/artifacts.json`
/// (`target` per entry) even when the `release` stage is skipped, so
/// both fields are available in the preserved tree. This test pins
/// that contract so a future contributor refactoring the post-
/// pipeline write site can't silently regress to empty manifest
/// fields (which would force Phase-2 consumers to fall back to
/// guess-and-pray on the version string).
#[test]
fn preserve_dist_context_json_has_non_empty_targets_and_version() {
    if !tool_on_path("cargo") || !tool_on_path("git") {
        eprintln!(
            "SKIP preserve_dist_context_json_has_non_empty_targets_and_version: \
             cargo or git missing from PATH"
        );
        return;
    }

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    bootstrap_minimal_cargo_repo(repo, FIXTURE_CRATE_NAME);

    let preserved = repo.join("preserved-dist");
    let report_path = repo.join("det.json");
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "check",
            "determinism",
            "--runs",
            "2",
            "--stages",
            "build,archive",
            "--report",
        ])
        .arg(&report_path)
        .args(["--preserve-dist"])
        .arg(&preserved)
        .current_dir(repo)
        .output()
        .expect("invoking anodize check determinism --preserve-dist");

    assert!(
        output.status.success(),
        "expected zero exit on green run; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let context_bytes = fs::read(preserved.join("context.json")).expect("reading context.json");
    let context: serde_json::Value =
        serde_json::from_slice(&context_bytes).expect("parsing context.json");

    let targets = context
        .get("targets")
        .and_then(|v| v.as_array())
        .expect("context.json missing `targets` array");
    assert!(
        !targets.is_empty(),
        "context.json `targets` MUST be non-empty in production runs \
         (release pipeline writes target metadata via run_post_pipeline). \
         Full context: {}",
        String::from_utf8_lossy(&context_bytes)
    );

    let version = context
        .get("version")
        .and_then(|v| v.as_str())
        .expect("context.json missing `version` string");
    assert!(
        !version.is_empty(),
        "context.json `version` MUST be non-empty in production runs \
         (release pipeline writes metadata.json:version via run_post_pipeline, \
         and the harness threads CARGO_PKG_VERSION as fallback). \
         Full context: {}",
        String::from_utf8_lossy(&context_bytes)
    );
}
