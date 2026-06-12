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
    // Three independent failure surfaces:
    //   publish.aur  -> PF_MISSING_AUR_KEY unset        (env-missing)
    //   sboms.cmd    -> tool that cannot exist on PATH  (tool-missing)
    //   signs env:// -> PF_COSIGN_KEY set but malformed (bad key material)
    let yaml = format!(
        r#"project_name: {FIXTURE_CRATE_NAME}
crates:
  - name: {FIXTURE_CRATE_NAME}
    path: .
    publish:
      aur:
        private_key: "{{{{ .Env.PF_MISSING_AUR_KEY }}}}"
signs:
  - artifacts: checksum
    cmd: cosign
    args: ["sign-blob", "--key", "env://PF_COSIGN_KEY", "{{{{ .Artifact }}}}"]
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
        // The malformed-but-SET secret: its NAME may appear, its VALUE must not.
        .env("PF_COSIGN_KEY", SECRET_SENTINEL)
        .env_remove("PF_MISSING_AUR_KEY")
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
        "skipped sign stage still contributed requirements:\n{combined}"
    );
    assert!(
        !combined.contains("PF_MISSING_AUR_KEY"),
        "skipped publish still contributed requirements:\n{combined}"
    );
}
