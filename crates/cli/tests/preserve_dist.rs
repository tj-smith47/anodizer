//! Integration tests for `anodize check determinism --preserve-dist=<path>`.
//!
//! Phase 1 of `.claude/specs/2026-05-19-determinism-produces-shippable.md`:
//! the harness must, on a green run, copy `<worktree>/dist/**` from
//! run-0 to the operator-supplied destination and emit a `context.json`
//! manifest describing the artifact set.
//!
//! These tests synthesize a minimal cargo workspace (matching the
//! existing `check_determinism.rs` fixture pattern), drive the harness
//! end-to-end with `--preserve-dist=<tmp>`, and assert:
//!
//!   1. The dist tree was copied to <tmp>.
//!   2. `<tmp>/context.json` is present and round-trips through serde.
//!   3. Each file in `<tmp>/` has a SHA256 that matches the
//!      corresponding entry in `determinism.json:artifacts[].hash` (the
//!      load-bearing "preserved bytes match the determinism check"
//!      property the spec's Safety Property depends on).
//!
//! On hosts without `cargo` or `git` on PATH, these tests print a SKIP
//! marker and return early — same convention as the existing harness
//! integration test in `check_determinism.rs`.

use anodizer_core::DeterminismReport;
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn tool_on_path(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn run_git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {:?} failed to spawn: {e}", args));
    assert!(
        out.status.success(),
        "git {:?} failed: stdout={} stderr={}",
        args,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn host_triple() -> String {
    let out = Command::new("rustc")
        .args(["-vV"])
        .output()
        .expect("rustc -vV must succeed (cargo is on PATH; rustc is sibling)");
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        if let Some(host) = line.strip_prefix("host: ") {
            return host.trim().to_string();
        }
    }
    panic!("no `host:` line in `rustc -vV` output:\n{}", stdout);
}

/// Bootstrap a minimal cargo workspace at `dir`, init it as a git
/// repo, commit. Same shape as `check_determinism::bootstrap_minimal_cargo_repo`
/// but inlined here so the two test files stay independent.
fn bootstrap_minimal_cargo_repo(dir: &Path) {
    fs::write(
        dir.join("Cargo.toml"),
        r#"[package]
name = "anodize-det-fixture-preserve"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "anodize-det-fixture-preserve"
path = "src/main.rs"
"#,
    )
    .unwrap();
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();

    let host = host_triple();
    let yaml = format!(
        r#"crates:
  - name: anodize-det-fixture-preserve
    path: .
    builds:
      - id: anodize-det-fixture-preserve
        binary: anodize-det-fixture-preserve
        targets:
          - {host}
"#,
    );
    fs::write(dir.join(".anodizer.yaml"), yaml).unwrap();

    run_git(dir, &["init", "-q", "-b", "master"]);
    run_git(dir, &["config", "user.email", "test@test.com"]);
    run_git(dir, &["config", "user.name", "Test"]);
    run_git(dir, &["config", "commit.gpgsign", "false"]);
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-q", "-m", "init"]);
}

/// SHA256 a file the way the harness does — `sha256:<hex>` prefix.
fn sha256_file(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let bytes = fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    format!("sha256:{:x}", hasher.finalize())
}

/// Walk `<dir>` recursively and return a sorted list of `(relpath, abspath)`
/// for every regular file. Used to spot-check that the preserved tree
/// matches the worktree dist tree.
fn walk_files(dir: &Path) -> Vec<(String, std::path::PathBuf)> {
    fn inner(root: &Path, dir: &Path, out: &mut Vec<(String, std::path::PathBuf)>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                inner(root, &path, out);
            } else if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((rel, path));
            }
        }
    }
    let mut out = Vec::new();
    inner(dir, dir, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Test #1 from the spec's Test Plan section (Phase 1, item 1):
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
    bootstrap_minimal_cargo_repo(repo);

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

    // Schema fields (spec section A.3): artifacts, targets, version, commit.
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

/// Test #2 from the spec's Test Plan section (Phase 1, item 2):
/// each file in `<preserved-dist>/` has a SHA256 that matches the
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
    bootstrap_minimal_cargo_repo(repo);

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
    // report's recorded hash. Files NOT in the report (e.g. metadata.json,
    // artifacts.json, context.json itself) are tolerated — the harness's
    // discover walk filters to artifacts that pass `infer_stage_from_path`
    // logic, and freshly-written metadata may post-date the walk.
    let mut matched = 0usize;
    for (rel, abs) in walk_files(&preserved) {
        // context.json is written AFTER the determinism check completes,
        // so it doesn't appear in the report's artifacts array — skip.
        if rel == "context.json" {
            continue;
        }
        let basename = abs
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Some(expected) = by_name.get(basename.as_str()) else {
            // File present in preserved tree but not in the report's
            // hashed set — tolerated (see comment above).
            continue;
        };
        let actual = sha256_file(&abs);
        assert_eq!(
            &actual, expected,
            "preserved file {} (basename {}) SHA mismatch:\n  preserved: {}\n  report:    {}",
            rel, basename, actual, expected
        );
        matched += 1;
    }
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
    bootstrap_minimal_cargo_repo(repo);

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
