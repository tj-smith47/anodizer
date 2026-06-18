//! Integration tests for `anodizer preflight` (the config-derived
//! environment preflight command).
//!
//! Drives the real binary against a synthesized fixture repo whose config
//! demands things the host cannot satisfy, asserting:
//!   - collect-all: failures from independent surfaces (publisher SSH key,
//!     sbom tool, cosign key material) all appear in ONE run;
//!   - the exit code is non-zero when anything is missing;
//!   - secret VALUES never appear in the output — only env-var names;
//!   - `--json` emits a machine-readable report with the same failures.
//!
//! Skips cleanly on hosts without git (fixture bootstrap needs it), same
//! convention as `publish_only.rs`.

use std::process::Command;
use tempfile::TempDir;

mod common;
use common::{bootstrap_minimal_cargo_repo, tool_on_path};

const FIXTURE_CRATE_NAME: &str = "anodizer-preflight-fixture";

/// Sentinel that must NEVER appear in preflight output: it is the VALUE of
/// an env var the config requires (a malformed cosign key, so the check
/// fails and the failure message is exercised, not just the happy path).
const SECRET_SENTINEL: &str = "SUPERSECRET-PREFLIGHT-SENTINEL-VALUE";

fn write_fixture_config(dir: &std::path::Path) {
    // Four independent failure surfaces:
    //   publish.aur  -> PF_MISSING_AUR_KEY unset        (env-missing)
    //   publish.npm  -> NPM_TOKEN unset                 (env-missing)
    //   sboms.cmd    -> tool that cannot exist on PATH  (tool-missing)
    //   signs env:// -> PF_COSIGN_KEY set but malformed (bad key material)
    //
    // aur and npm are BOTH publishers, so a `--publishers` allowlist can
    // select one and deselect the other in a single run — the allowlist test
    // asserts the selected publisher's requirement survives while the
    // deselected one drops.
    let yaml = format!(
        r#"project_name: {FIXTURE_CRATE_NAME}
crates:
  - name: {FIXTURE_CRATE_NAME}
    path: .
    publish:
      aur:
        private_key: "{{{{ .Env.PF_MISSING_AUR_KEY }}}}"
npms:
  - scope: "@pf"
uploads:
  - name: mirror
    target: "https://uploads.example/{{{{ .ProjectName }}}}/{{{{ .ArtifactName }}}}"
    signature: true
signs:
  - artifacts: checksum
    cmd: cosign
    args: ["sign-blob", "--key", "env://PF_COSIGN_KEY", "{{{{ .Artifact }}}}"]
binary_signs:
  - cmd: cosign
    args: ["sign-blob", "--key", "env://PF_BINARY_COSIGN_KEY", "{{{{ .Artifact }}}}"]
sboms:
  - cmd: pf-definitely-not-a-real-tool-9z
"#
    );
    std::fs::write(dir.join(".anodizer.yaml"), yaml).unwrap();
}

fn run_preflight(dir: &std::path::Path, extra_args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .current_dir(dir)
        .arg("preflight")
        .args(extra_args)
        // The malformed-but-SET secrets: their NAMEs may appear, VALUEs must not.
        // PF_COSIGN_KEY backs the `signs:` slice; PF_BINARY_COSIGN_KEY backs the
        // `binary_signs:` slice — distinct env vars so each slice's gate is
        // asserted independently.
        .env("PF_COSIGN_KEY", SECRET_SENTINEL)
        .env("PF_BINARY_COSIGN_KEY", SECRET_SENTINEL)
        .env_remove("PF_MISSING_AUR_KEY")
        // Unset on the child only (never the test process) so the npm
        // publisher's token requirement deterministically reads as missing;
        // the allowlist test asserts this surfaces for the SELECTED npm
        // publisher. Per-child env keeps the test-isolation guard satisfied.
        .env_remove("NPM_TOKEN")
        .output()
        .expect("spawn anodizer preflight")
}

#[test]
fn preflight_collects_all_failures_and_exits_nonzero() {
    if !tool_on_path("git") {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let tmp = TempDir::new().unwrap();
    bootstrap_minimal_cargo_repo(tmp.path(), FIXTURE_CRATE_NAME);
    write_fixture_config(tmp.path());

    let out = run_preflight(tmp.path(), &[]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        !out.status.success(),
        "preflight must exit non-zero on failures; output:\n{combined}"
    );
    // Collect-all: every independent failure surface present in ONE run.
    assert!(
        combined.contains("PF_MISSING_AUR_KEY"),
        "missing publisher SSH key env var not reported:\n{combined}"
    );
    assert!(
        combined.contains("pf-definitely-not-a-real-tool-9z"),
        "missing sbom tool not reported:\n{combined}"
    );
    assert!(
        combined.contains("PF_COSIGN_KEY"),
        "malformed cosign key env var not reported:\n{combined}"
    );
    // Secret hygiene: the VALUE of the set-but-invalid key never leaks.
    assert!(
        !combined.contains(SECRET_SENTINEL),
        "preflight output echoed a secret value:\n{combined}"
    );
}

#[test]
fn preflight_json_reports_same_failures() {
    if !tool_on_path("git") {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let tmp = TempDir::new().unwrap();
    bootstrap_minimal_cargo_repo(tmp.path(), FIXTURE_CRATE_NAME);
    write_fixture_config(tmp.path());

    let out = run_preflight(tmp.path(), &["--json"]);
    assert!(!out.status.success(), "non-zero exit expected");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json_start = stdout.find('{').expect("JSON object in stdout");
    let report: serde_json::Value =
        serde_json::from_str(stdout[json_start..].trim()).expect("valid JSON report");
    let failures = report["failures"].as_array().expect("failures array");
    assert!(
        failures.len() >= 3,
        "expected at least 3 failures, got: {failures:?}"
    );
    let kinds: Vec<&str> = failures.iter().filter_map(|f| f["kind"].as_str()).collect();
    assert!(kinds.contains(&"missing_env"), "kinds: {kinds:?}");
    assert!(kinds.contains(&"missing_tool"), "kinds: {kinds:?}");
    assert!(kinds.contains(&"bad_key_material"), "kinds: {kinds:?}");
    assert!(
        !stdout.contains(SECRET_SENTINEL),
        "JSON output echoed a secret value"
    );
}

#[test]
fn preflight_skip_drops_stage_requirements() {
    if !tool_on_path("git") {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let tmp = TempDir::new().unwrap();
    bootstrap_minimal_cargo_repo(tmp.path(), FIXTURE_CRATE_NAME);
    write_fixture_config(tmp.path());

    let out = run_preflight(tmp.path(), &["--skip=sign,sbom,publish"]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !combined.contains("pf-definitely-not-a-real-tool-9z"),
        "skipped sbom stage still contributed requirements:\n{combined}"
    );
    assert!(
        !combined.contains("PF_COSIGN_KEY"),
        "skipped sign stage still contributed signs requirements:\n{combined}"
    );
    assert!(
        !combined.contains("PF_BINARY_COSIGN_KEY"),
        "skipped sign stage still contributed binary_signs requirements:\n{combined}"
    );
    assert!(
        !combined.contains("PF_MISSING_AUR_KEY"),
        "skipped publish still contributed requirements:\n{combined}"
    );
}

/// `preflight --publishers <name>` mirrors `release --publishers`: a
/// non-empty allowlist SELECTS the named publisher and DESELECTS every other
/// one, in a single pass. The fixture configures two publishers — `npm` and
/// `aur` — so one run proves BOTH directions of the allowlist:
///   - SELECTED (`npm`): its `NPM_TOKEN` requirement SURVIVES the allowlist
///     and surfaces in the report as `[needed by: publish:npm]`;
///   - DESELECTED (`aur`): its `PF_MISSING_AUR_KEY` requirement is DROPPED.
///
/// Crucially, NO `--skip` is passed: the `--publishers npm` allowlist ALONE
/// must auto-deselect the `signs:` surface (its only consumers —
/// github-release / blob / artifactory — are all deselected), so the malformed
/// `PF_COSIGN_KEY` cosign demand vanishes without a hand-skip. This is exactly
/// the surface the npm-provenance job validates with
/// `preflight --publish-only --publishers npm` and zero `--skip`.
#[test]
fn preflight_publishers_allowlist_keeps_selected_drops_deselected_publisher() {
    if !tool_on_path("git") {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let tmp = TempDir::new().unwrap();
    bootstrap_minimal_cargo_repo(tmp.path(), FIXTURE_CRATE_NAME);
    write_fixture_config(tmp.path());

    // Allowlist `npm` with NO `--skip`: npm is SELECTED (its token requirement
    // must survive), the configured-but-unselected `aur` publisher is
    // DESELECTED (its key requirement must vanish), and the `signs:` slice
    // self-deselects because every signature consumer is deselected.
    let out = run_preflight(tmp.path(), &["--publish-only", "--publishers=npm"]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // SELECTED publisher: requirement survives the allowlist AND is attributed
    // to the npm publisher source, not merely present by coincidence.
    assert!(
        combined.contains("NPM_TOKEN"),
        "allowlist-selected npm publisher's token requirement was dropped:\n{combined}"
    );
    assert!(
        combined.contains("publish:npm"),
        "npm token requirement not attributed to the selected npm publisher:\n{combined}"
    );
    // DESELECTED publisher: its requirement is gone.
    assert!(
        !combined.contains("PF_MISSING_AUR_KEY"),
        "allowlist-deselected aur publisher still demanded its key:\n{combined}"
    );
    assert!(
        !combined.contains("publish:aur"),
        "deselected aur publisher still attributed a requirement source:\n{combined}"
    );
    // The signs slice self-deselects (no consumer selected) WITHOUT a
    // hand-skip, so its cosign key demand is gone.
    assert!(
        !combined.contains("PF_COSIGN_KEY"),
        "--publishers npm must auto-deselect the signs surface (no --skip):\n{combined}"
    );
    // The binary_signs slice self-skips in --publish-only (its output has no
    // publish-time consumer), so its DISTINCT cosign key demand is ALSO gone
    // WITHOUT a hand-skip — this is the second half of the npm-clean invariant.
    assert!(
        !combined.contains("PF_BINARY_COSIGN_KEY"),
        "--publish-only must auto-skip the binary_signs surface (no --skip):\n{combined}"
    );
    // Neither sign slice contributes anything: with both signs: (deselected
    // consumers) and binary_signs: (publish-only) skipped, no `stage:sign`
    // requirement may appear at all.
    assert!(
        !combined.contains("stage:sign"),
        "the sign slices must contribute nothing under --publish-only --publishers npm:\n{combined}"
    );
    assert!(
        !combined.contains("stage:release") && !combined.contains("publish:github-release"),
        "github-release must auto-deselect under --publishers npm (no --skip):\n{combined}"
    );
}

/// Under `--publish-only` with an EMPTY `--publishers` allowlist the `signs:`
/// surface must SURVIVE (`publisher_deselected` short-circuits to the denylist,
/// which never names a signs consumer) while the `binary_signs:` surface is
/// SKIPPED (publish-only mode — its output has no publish-time consumer).
/// Guards both directions: the signs gate must not over-fire and silently ship
/// an unsigned release; the binary_signs gate must fire on publish-only
/// regardless of allowlist.
#[test]
fn preflight_publish_only_empty_allowlist_keeps_signs_skips_binary_signs() {
    if !tool_on_path("git") {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let tmp = TempDir::new().unwrap();
    bootstrap_minimal_cargo_repo(tmp.path(), FIXTURE_CRATE_NAME);
    write_fixture_config(tmp.path());

    // No allowlist, no skip, publish-only: the signs slice runs (its consumers
    // are not deselected), so its cosign key demand (malformed PF_COSIGN_KEY)
    // still surfaces; the binary_signs slice is publish-only-skipped, so its
    // distinct PF_BINARY_COSIGN_KEY demand is gone.
    let out = run_preflight(tmp.path(), &["--publish-only"]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("PF_COSIGN_KEY"),
        "publish-only empty allowlist must keep the signs surface:\n{combined}"
    );
    assert!(
        !combined.contains("PF_BINARY_COSIGN_KEY"),
        "publish-only must skip the binary_signs surface regardless of allowlist:\n{combined}"
    );
}

/// The MAIN-job invariant under the REAL binary: the full release pipeline
/// (no `--publish-only`; the main job runs `release --skip=npm`, i.e. the FULL
/// scope with an empty allowlist) must KEEP BOTH sign surfaces — `signs:` AND
/// `binary_signs:` — so the binaries that ship are still signed. Proves the
/// binary_signs publish-only gate does not weaken the main release.
#[test]
fn preflight_full_scope_keeps_both_sign_surfaces() {
    if !tool_on_path("git") {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let tmp = TempDir::new().unwrap();
    bootstrap_minimal_cargo_repo(tmp.path(), FIXTURE_CRATE_NAME);
    write_fixture_config(tmp.path());

    // FULL scope (no --publish-only), empty allowlist: BOTH sign slices run.
    let out = run_preflight(tmp.path(), &[]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("PF_COSIGN_KEY"),
        "full-scope run must keep the signs surface:\n{combined}"
    );
    assert!(
        combined.contains("PF_BINARY_COSIGN_KEY"),
        "full-scope run must keep the binary_signs surface (main-job binary signing preserved):\n{combined}"
    );
}

/// The `uploads` publisher consumes the `signs:` sidecars when an entry sets
/// `signature: true`, so it is a member of `signs_consumers()`. Selecting it
/// ALONE (every OTHER consumer deselected) must KEEP the `signs:` surface — its
/// cosign key demand must survive — proving preflight stays in lockstep with
/// the fixed runtime. Before the fix, `uploads` was absent from the hard-coded
/// three-consumer conjunction, so `--publishers uploads` falsely dropped
/// `stage:sign` and the selected uploads publisher would mirror an unsigned set.
#[test]
fn preflight_publishers_uploads_keeps_signs_surface() {
    if !tool_on_path("git") {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let tmp = TempDir::new().unwrap();
    bootstrap_minimal_cargo_repo(tmp.path(), FIXTURE_CRATE_NAME);
    write_fixture_config(tmp.path());

    let out = run_preflight(tmp.path(), &["--publish-only", "--publishers=uploads"]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // The selected `uploads` publisher reads the signs sidecars, so the signs
    // surface (and its cosign key demand) must NOT be deselected.
    assert!(
        combined.contains("PF_COSIGN_KEY"),
        "--publishers uploads must keep the signs surface (uploads consumes the sidecars):\n{combined}"
    );
    assert!(
        combined.contains("stage:sign"),
        "the signs slice must contribute its requirements under --publishers uploads:\n{combined}"
    );
}
