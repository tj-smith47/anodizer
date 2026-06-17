//! Static analysis of `.github/workflows/release.yml` and the reusable
//! `.github/workflows/determinism.yml` it calls, pinning the safety
//! properties of the release pipeline:
//!
//! - The release job runs only after a successful determinism check
//!   (so shipped bytes have always passed byte-stability verification).
//! - The redundant per-target build job does NOT exist (determinism IS
//!   the build).
//! - The release job consumes preserved-dist artifacts via
//!   `--publish-only`, not the legacy `--merge` flow.
//!
//! The sharded determinism gate (matrix, shard labels, per-shard
//! preserve-dist upload) lives in `determinism.yml` — a reusable
//! `workflow_call` workflow. `release.yml`'s `determinism-check` job is
//! the thin caller that wires `head_sha` into it; the link between the
//! two files is itself an asserted invariant so the extraction can't be
//! silently un-wired. Two further invariants close gaps the extraction
//! opened: `determinism.yml`'s checkout must consume that `head_sha`
//! (so the byte-verify runs against the shipped commit, not default-branch
//! HEAD), and its shard matrix must equal `release.yml`'s `expected=(...)`
//! partial-shard guard list (so the two hand-maintained shard sets cannot
//! drift).
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

/// Parsed `determinism.yml`, loaded once and shared across tests. This is
/// the reusable `workflow_call` workflow that `release.yml`'s
/// `determinism-check` job invokes; the sharded gate now lives here.
static DETERMINISM: LazyLock<Value> = LazyLock::new(|| {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../.github/workflows/determinism.yml"
    );
    let raw = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_yaml_ng::from_str(&raw).unwrap_or_else(|e| panic!("parse {path}: {e}"))
});

fn determinism() -> &'static Value {
    &DETERMINISM
}

/// Parsed `download-preserved-dist` composite action. The release/publish
/// jobs delegate dist-* artifact download to it, so the `pattern` +
/// `merge-multiple` invariant lives here rather than inline in `release.yml`.
static DOWNLOAD_PRESERVED_DIST: LazyLock<Value> = LazyLock::new(|| {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../.github/actions/download-preserved-dist/action.yml"
    );
    let raw = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_yaml_ng::from_str(&raw).unwrap_or_else(|e| panic!("parse {path}: {e}"))
});

fn download_preserved_dist() -> &'static Value {
    &DOWNLOAD_PRESERVED_DIST
}

fn jobs(wf: &Value) -> &serde_yaml_ng::Mapping {
    wf.get("jobs")
        .and_then(Value::as_mapping)
        .expect("missing `jobs:` mapping")
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
fn determinism_check_calls_reusable_determinism_workflow() {
    let wf = workflow();
    let det = jobs(wf)
        .get(Value::from("determinism-check"))
        .expect("release.yml: missing `determinism-check:` job");

    let uses = det
        .get("uses")
        .and_then(Value::as_str)
        .expect("release.yml: `determinism-check:` job must `uses:` a reusable workflow");
    assert_eq!(
        uses, "./.github/workflows/determinism.yml",
        "release.yml: `determinism-check:` must call the reusable \
         `./.github/workflows/determinism.yml` (the sharded gate lives there). \
         Got: {uses:?}"
    );

    let head_sha = det
        .get("with")
        .and_then(|w| w.get("head_sha"))
        .and_then(Value::as_str)
        .expect("release.yml: `determinism-check:` must pass `head_sha:` via `with:`");
    assert!(
        head_sha.contains("${{ needs.tag.outputs.sha }}"),
        "release.yml: `determinism-check:` must forward the tagged commit as \
         `head_sha: ${{{{ needs.tag.outputs.sha }}}}` so the byte-verify runs \
         against the exact bytes being shipped. Got: {head_sha:?}"
    );

    let needs = det
        .get("needs")
        .expect("release.yml: `determinism-check:` missing `needs:`");
    let depends_on_tag = match needs {
        Value::String(s) => s == "tag",
        Value::Sequence(seq) => seq.iter().any(|v| v.as_str() == Some("tag")),
        other => panic!("release.yml: unexpected `needs:` shape {other:?}"),
    };
    assert!(
        depends_on_tag,
        "release.yml: `determinism-check:` must `needs: tag` so the gate runs \
         against the freshly tagged commit. Got: {needs:?}"
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
fn determinism_shard_checks_out_passed_head_sha() {
    let wf = determinism();
    let det = jobs(wf)
        .get(Value::from("shard"))
        .expect("determinism.yml: missing `shard:` job");
    let steps = det
        .get("steps")
        .and_then(Value::as_sequence)
        .expect("determinism.yml: `shard:` job missing `steps:`");

    let checkout = steps
        .iter()
        .find(|s| {
            s.get("uses")
                .and_then(Value::as_str)
                .is_some_and(|u| u.starts_with("actions/checkout"))
        })
        .expect("determinism.yml: `shard:` job must check out the repo");

    let git_ref = checkout
        .get("with")
        .and_then(|w| w.get("ref"))
        .and_then(Value::as_str)
        .expect(
            "determinism.yml: checkout step must pin `with.ref` to the \
             passed-in commit — a missing `ref:` byte-verifies default-branch \
             HEAD, not the bytes being shipped",
        );
    assert!(
        git_ref.contains("${{ inputs.head_sha }}"),
        "determinism.yml: checkout `ref:` must consume `${{{{ inputs.head_sha }}}}` \
         (the SHA release.yml passes in) so the byte-verify runs against the \
         exact shipped commit. Got: {git_ref:?}"
    );
}

#[test]
fn determinism_check_uploads_dist_only_on_success() {
    let wf = determinism();
    let det = jobs(wf)
        .get(Value::from("shard"))
        .expect("determinism.yml: missing `shard:` job");
    let steps = det
        .get("steps")
        .and_then(Value::as_sequence)
        .expect("determinism.yml: `shard:` job missing `steps:`");

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
            "determinism.yml: `shard:` job must upload an artifact whose \
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

    // Dist-* artifact download is delegated to the `download-preserved-dist`
    // composite action (the inline `actions/download-artifact` step was hoisted
    // there in the CI-dedup refactor). The release job must still reference it.
    assert!(
        steps.iter().any(|s| {
            s.get("uses")
                .and_then(Value::as_str)
                .is_some_and(|u| u.starts_with("./.github/actions/download-preserved-dist"))
        }),
        "release.yml: `release:` job must download dist via the \
         `download-preserved-dist` composite action"
    );

    // The pattern + merge-multiple guarantee now lives inside that composite:
    // its `actions/download-artifact` step must collapse every per-shard
    // `dist-*` tree into a single dist/.
    let action_steps = download_preserved_dist()
        .get("runs")
        .and_then(|r| r.get("steps"))
        .and_then(Value::as_sequence)
        .expect("download-preserved-dist: missing `runs.steps:`");
    let download = action_steps
        .iter()
        .find(|s| {
            s.get("uses")
                .and_then(Value::as_str)
                .is_some_and(|u| u.starts_with("actions/download-artifact"))
        })
        .expect("download-preserved-dist: must use actions/download-artifact");

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
    let wf = determinism();
    let det = jobs(wf)
        .get(Value::from("shard"))
        .expect("determinism.yml: missing `shard:` job");
    let matrix = det
        .get("strategy")
        .and_then(|s| s.get("matrix"))
        .expect("`shard:` job must declare a matrix");
    let include = matrix
        .get("include")
        .and_then(Value::as_sequence)
        .expect("matrix must use `include:` form");

    let mut shards: Vec<String> = Vec::new();
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
        shards.push(shard.to_owned());
    }
    shards.sort();

    let mut expected: Vec<String> = [
        "macos-latest",
        "ubuntu-latest",
        "windows-aarch64",
        "windows-x86_64",
    ]
    .map(str::to_owned)
    .to_vec();
    expected.sort();
    assert_eq!(
        shards, expected,
        "determinism.yml: `shard:` matrix must cover EXACTLY the four \
         platform shards (both windows arches present) — a dropped shard \
         silently narrows reproducibility coverage. Got: {shards:?}"
    );
}

/// Extracts the bash `expected=(...)` shard tokens from the
/// `release.yml` partial-shard guard step. The guard list and the
/// `determinism.yml` matrix are hand-maintained independently; this
/// surfaces the tokens so a test can pin them equal.
fn release_guard_expected_shards() -> Vec<String> {
    let wf = workflow();
    let release = jobs(wf)
        .get(Value::from("release"))
        .expect("release.yml: missing `release:` job");
    let steps = release
        .get("steps")
        .and_then(Value::as_sequence)
        .expect("release.yml: `release:` job missing `steps:`");

    let run = steps
        .iter()
        .filter_map(|s| s.get("run").and_then(Value::as_str))
        .find(|r| r.contains("expected=("))
        .expect(
            "release.yml: partial-shard guard step must declare a bash \
             `expected=(...)` shard array",
        );

    let start = run.find("expected=(").expect("locate `expected=(`") + "expected=(".len();
    let rest = &run[start..];
    let end = rest
        .find(')')
        .expect("`expected=(...)` missing closing `)`");
    let mut shards: Vec<String> = rest[..end].split_whitespace().map(str::to_owned).collect();
    shards.sort();
    shards
}

#[test]
fn determinism_matrix_matches_release_guard_shard_list() {
    let wf = determinism();
    let det = jobs(wf)
        .get(Value::from("shard"))
        .expect("determinism.yml: missing `shard:` job");
    let include = det
        .get("strategy")
        .and_then(|s| s.get("matrix"))
        .and_then(|m| m.get("include"))
        .and_then(Value::as_sequence)
        .expect("determinism.yml: `shard:` job must declare a matrix `include:`");

    let mut matrix_shards: Vec<String> = include
        .iter()
        .filter_map(|e| e.get("shard").and_then(Value::as_str).map(str::to_owned))
        .collect();
    matrix_shards.sort();

    let guard_shards = release_guard_expected_shards();
    assert_eq!(
        matrix_shards, guard_shards,
        "determinism.yml matrix shard set must EQUAL release.yml's \
         `expected=(...)` partial-shard guard set — the two lists are \
         maintained independently and must not drift (a shard dropped \
         from the gate while the guard still passes would ship \
         unverified bytes). matrix: {matrix_shards:?} guard: {guard_shards:?}"
    );
}

#[test]
fn determinism_check_passes_shard_label_to_action() {
    let wf = determinism();
    let det = jobs(wf)
        .get(Value::from("shard"))
        .expect("determinism.yml: missing `shard:` job");
    let steps = det
        .get("steps")
        .and_then(Value::as_sequence)
        .expect("determinism.yml: `shard:` job missing `steps:`");

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
