//! Static analysis of `.github/workflows/release.yml` pinning the
//! safety properties of the release pipeline:
//!
//! - The release job runs only after a successful determinism check
//!   (so shipped bytes have always passed byte-stability verification).
//! - The redundant per-target build job does NOT exist (determinism IS
//!   the build).
//! - The release job consumes preserved-dist artifacts via
//!   `--publish-only`, not the legacy `--merge` flow.
//!
//! A YAML refactor that violates any of these invariants is a
//! release-pipeline regression; this test fails before the workflow
//! ever runs in CI.
//!
//! The test parses the workflow YAML in-process (no subprocess spawn)
//! and matches against the parsed structure, so renames that preserve
//! semantics (e.g. step name edits) don't trip false positives.

use serde_yaml_ng::Value;
use std::sync::LazyLock;

/// Parsed `release.yml`, loaded once and shared across tests.
static WORKFLOW: LazyLock<Value> = LazyLock::new(|| {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../.github/workflows/release.yml"
    );
    let raw = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_yaml_ng::from_str(&raw).unwrap_or_else(|e| panic!("parse {path}: {e}"))
});

fn workflow() -> &'static Value {
    &WORKFLOW
}

fn jobs(wf: &Value) -> &serde_yaml_ng::Mapping {
    wf.get("jobs")
        .and_then(Value::as_mapping)
        .expect("release.yml: missing `jobs:` mapping")
}

#[test]
fn no_redundant_build_job_exists() {
    let wf = workflow();
    let jobs = jobs(wf);
    assert!(
        !jobs.contains_key(Value::from("build")),
        "release.yml: a `build:` job exists. The harness produces the \
         shippable bytes directly — a separate recompile job is redundant \
         and re-introduces drift risk."
    );
}

#[test]
fn release_job_depends_on_determinism_check() {
    let wf = workflow();
    let release = jobs(wf)
        .get(Value::from("release"))
        .expect("release.yml: missing `release:` job");
    let needs = release
        .get("needs")
        .expect("release.yml: `release:` missing `needs:`");

    let mut deps: Vec<String> = match needs {
        Value::String(s) => vec![s.clone()],
        Value::Sequence(seq) => seq
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        other => panic!("release.yml: unexpected `needs:` shape {other:?}"),
    };
    deps.sort();
    assert!(
        deps.iter().any(|d| d == "determinism-check"),
        "release.yml: `release:` job must `needs: determinism-check`. \
         A green determinism check is the only thing that should gate the \
         release pipeline. Got: {deps:?}"
    );
    assert!(
        !deps.iter().any(|d| d == "build"),
        "release.yml: `release:` job must NOT depend on a `build:` job. \
         Got: {deps:?}"
    );
}

#[test]
fn release_job_runs_publish_only() {
    let wf = workflow();
    let release = jobs(wf)
        .get(Value::from("release"))
        .expect("release.yml: missing `release:` job");
    let steps = release
        .get("steps")
        .and_then(Value::as_sequence)
        .expect("release.yml: `release:` job missing `steps:`");

    let args = steps
        .iter()
        .filter_map(|step| step.get("with"))
        .filter_map(|with| with.get("args"))
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();

    let publish_only_present = args.iter().any(|a| a.contains("--publish-only"));
    let merge_present = args.iter().any(|a| a.contains("--merge"));

    assert!(
        publish_only_present,
        "release.yml: `release:` job must invoke `anodize release --publish-only`. \
         Found args: {args:?}"
    );
    assert!(
        !merge_present,
        "release.yml: `release:` job must NOT use the legacy `--merge` flow. \
         Found args: {args:?}"
    );
}

#[test]
fn determinism_check_uploads_dist_only_on_success() {
    let wf = workflow();
    let det = jobs(wf)
        .get(Value::from("determinism-check"))
        .expect("release.yml: missing `determinism-check:` job");
    let steps = det
        .get("steps")
        .and_then(Value::as_sequence)
        .expect("release.yml: `determinism-check:` job missing `steps:`");

    let upload = steps
        .iter()
        .find(|s| {
            s.get("uses")
                .and_then(Value::as_str)
                .is_some_and(|u| u.starts_with("actions/upload-artifact"))
                && s.get("with")
                    .and_then(|w| w.get("name"))
                    .and_then(Value::as_str)
                    .is_some_and(|n| n.starts_with("dist"))
        })
        .expect(
            "release.yml: `determinism-check:` job must upload an artifact whose \
             name starts with `dist` (consumed by `release: --publish-only`)",
        );

    let if_cond = upload
        .get("if")
        .and_then(Value::as_str)
        .expect("dist upload step must declare an `if:` condition");
    assert_eq!(
        if_cond.trim(),
        "success()",
        "dist upload step must be gated `if: success()` — a drift-failed \
         shard MUST NOT upload preserved bytes. Got: {if_cond:?}"
    );
}

#[test]
fn release_job_downloads_dist_artifacts_with_merge_multiple() {
    let wf = workflow();
    let release = jobs(wf)
        .get(Value::from("release"))
        .expect("release.yml: missing `release:` job");
    let steps = release
        .get("steps")
        .and_then(Value::as_sequence)
        .expect("release.yml: `release:` job missing `steps:`");

    let download = steps
        .iter()
        .find(|s| {
            s.get("uses")
                .and_then(Value::as_str)
                .is_some_and(|u| u.starts_with("actions/download-artifact"))
        })
        .expect("release.yml: `release:` job must download artifacts");

    let with = download.get("with").expect("download step missing `with:`");
    let pattern = with
        .get("pattern")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let merge_multiple = with
        .get("merge-multiple")
        .map(|v| v == &Value::Bool(true) || v.as_str() == Some("true"))
        .unwrap_or(false);

    assert!(
        pattern.starts_with("dist"),
        "download-artifact pattern must match `dist*`. Got: {pattern:?}"
    );
    assert!(
        merge_multiple,
        "download-artifact must use `merge-multiple: true` so per-shard \
         dist trees collapse into a single dist/."
    );
}

#[test]
fn determinism_check_matrix_declares_explicit_shard_labels() {
    let wf = workflow();
    let det = jobs(wf)
        .get(Value::from("determinism-check"))
        .expect("release.yml: missing `determinism-check:` job");
    let matrix = det
        .get("strategy")
        .and_then(|s| s.get("matrix"))
        .expect("`determinism-check:` job must declare a matrix");
    let include = matrix
        .get("include")
        .and_then(Value::as_sequence)
        .expect("matrix must use `include:` form");

    for entry in include {
        let m = entry
            .as_mapping()
            .expect("each matrix include must be a mapping");
        let shard = m
            .get(Value::from("shard"))
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(
            !shard.is_empty(),
            "every matrix entry must declare `shard:` explicitly. Entry: {entry:?}"
        );
    }
}

#[test]
fn determinism_check_passes_shard_label_to_action() {
    let wf = workflow();
    let det = jobs(wf)
        .get(Value::from("determinism-check"))
        .expect("release.yml: missing `determinism-check:` job");
    let steps = det
        .get("steps")
        .and_then(Value::as_sequence)
        .expect("release.yml: `determinism-check:` job missing `steps:`");

    let det_step = steps
        .iter()
        .find(|s| {
            s.get("with")
                .and_then(|w| w.get("preserve-dist"))
                .and_then(Value::as_str)
                .is_some_and(|v| v == "true")
        })
        .expect("a step with `preserve-dist: 'true'` must exist");

    let shard_label = det_step
        .get("with")
        .and_then(|w| w.get("shard-label"))
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        shard_label.contains("${{ matrix.shard }}"),
        "the preserve-dist step must pass `shard-label: ${{{{ matrix.shard }}}}`. \
         Got: {shard_label:?}"
    );
}

#[test]
fn release_workflow_does_not_carry_legacy_tag_bash() {
    let wf = workflow();
    let yaml_str = serde_yaml_ng::to_string(wf).expect("re-serialise");
    assert!(
        !yaml_str.contains("--no-snapshot"),
        "release.yml must NOT reference `--no-snapshot` literally — the harness \
         auto-detects tagged HEAD."
    );
}

#[test]
fn release_job_has_partial_shard_guard() {
    let wf = workflow();
    let release = jobs(wf)
        .get(Value::from("release"))
        .expect("release.yml: missing `release:` job");
    let steps = release
        .get("steps")
        .and_then(Value::as_sequence)
        .expect("release.yml: `release:` job missing `steps:`");

    let has_guard = steps.iter().any(|s| {
        s.get("name")
            .and_then(Value::as_str)
            .is_some_and(|n| n.contains("shards present") || n.contains("partial-shard"))
    });
    assert!(
        has_guard,
        "release.yml: `release:` job must include a partial-shard guard — \
         `fail-fast: false` permits incomplete dist trees otherwise."
    );
}
