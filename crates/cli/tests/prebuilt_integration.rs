//! Integration coverage for `builder: prebuilt` — the import-pre-built-binary
//! builder that skips `cargo build` and stages an already-produced binary
//! into the release pipeline.
//!
//! Each test stages a fake binary outside `dist/` (per GoReleaser's warning
//! that the release pipeline removes `dist/` between runs), points the
//! config's `prebuilt.path` template at it, and asserts the artifact lands
//! with the expected metadata. Negative tests cover the four config-load
//! validations (missing path, missing targets, mutual-exclusion with
//! `cross_tool`, `cross:` crate-level strategy).

use std::fs;
use std::process::Command;
use tempfile::TempDir;

use anodizer_core::test_helpers::{create_config, create_test_project, init_git_repo};

/// Stage a fake binary at `output/<binary>_<target>` (the conventional
/// shape from GoReleaser's docs) so the `prebuilt.path` template
/// renders to it on every host. Returns the absolute path of the
/// staged file for assertion bookkeeping.
fn stage_fake_binary(tmp: &std::path::Path, binary: &str, target: &str) -> std::path::PathBuf {
    let outdir = tmp.join("output");
    fs::create_dir_all(&outdir).expect("create output dir");
    let path = outdir.join(format!("{binary}_{target}"));
    fs::write(&path, b"fake-binary-bytes").expect("write fake binary");
    path
}

#[test]
fn prebuilt_imports_binary_and_registers_artifact() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    let target = "x86_64-unknown-linux-gnu";
    let staged = stage_fake_binary(tmp.path(), "test-project", target);

    create_config(
        tmp.path(),
        r#"
project_name: test-project
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - id: prebuilt-foo
        binary: test-project
        builder: prebuilt
        prebuilt:
          path: "output/test-project_{{ .Target }}"
        targets:
          - x86_64-unknown-linux-gnu
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["build"])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "anodize build should succeed.\nstderr:\n{stderr}\nstdout:\n{stdout}"
    );

    let metadata_path = tmp.path().join("dist/metadata.json");
    let metadata: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&metadata_path).expect("read metadata.json"))
            .expect("parse metadata.json");

    let artifacts_path = tmp.path().join("dist/artifacts.json");
    let artifacts: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&artifacts_path).expect("read artifacts.json"))
            .expect("parse artifacts.json");

    let arr = artifacts.as_array().expect("artifacts.json is an array");
    let binary = arr
        .iter()
        .find(|a| a.get("kind").and_then(|v| v.as_str()) == Some("binary"))
        .unwrap_or_else(|| panic!("no binary artifact in {arr:?}"));

    assert_eq!(
        binary.get("target").and_then(|v| v.as_str()),
        Some(target),
        "binary artifact target mismatch"
    );
    let registered_path = binary
        .get("path")
        .and_then(|v| v.as_str())
        .expect("artifact path");
    assert!(
        registered_path.ends_with(&format!("output/test-project_{target}")),
        "expected staged path suffix, got {registered_path:?}"
    );

    let staged_bytes = fs::read(&staged).expect("read staged binary");
    assert_eq!(staged_bytes, b"fake-binary-bytes");

    // Project name carried through.
    assert_eq!(
        metadata.get("project_name").and_then(|v| v.as_str()),
        Some("test-project")
    );
}

#[test]
fn prebuilt_artifact_is_signable() {
    // Sign stage's `should_sign_artifact` accepts `ArtifactKind::Binary`
    // when the sign config matches `artifacts: binary`. A prebuilt
    // import registers exactly that kind, so the type-routing test below
    // guarantees prebuilt bytes flow into the existing sign matrix
    // without a separate code path. Asserting the artifact's `kind`
    // field is the contract bridge: anything that consumes `binary`
    // artifacts (sign, sbom, archive) treats the imported binary the
    // same as a `cargo build` output.
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    stage_fake_binary(tmp.path(), "test-project", "x86_64-unknown-linux-gnu");

    create_config(
        tmp.path(),
        r#"
project_name: test-project
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: test-project
        builder: prebuilt
        prebuilt:
          path: "output/test-project_{{ .Target }}"
        targets:
          - x86_64-unknown-linux-gnu
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["build"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "build failed");

    let artifacts: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(tmp.path().join("dist/artifacts.json")).expect("read artifacts"),
    )
    .expect("parse artifacts");
    let kinds: Vec<&str> = artifacts
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a.get("kind").and_then(|v| v.as_str()))
        .collect();
    assert!(
        kinds.contains(&"binary"),
        "imported prebuilt artifact must register as `binary` (the kind the sign stage matches on); got {kinds:?}"
    );
}

#[test]
fn prebuilt_missing_binary_fails_loudly() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    init_git_repo(tmp.path());
    // Intentionally DO NOT stage the binary.

    create_config(
        tmp.path(),
        r#"
project_name: test-project
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: test-project
        builder: prebuilt
        prebuilt:
          path: "output/test-project_{{ .Target }}"
        targets:
          - x86_64-unknown-linux-gnu
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["build"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "build should fail when prebuilt binary is missing"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("prebuilt: failed to stat"),
        "stderr should cite stat failure for missing prebuilt binary:\n{stderr}"
    );
    assert!(
        stderr.contains("output/test-project_x86_64-unknown-linux-gnu"),
        "stderr should cite the rendered path:\n{stderr}"
    );
    assert!(
        stderr.contains("x86_64-unknown-linux-gnu"),
        "stderr should cite the originating target triple:\n{stderr}"
    );
}

#[test]
fn prebuilt_without_targets_fails_at_config_load() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    create_config(
        tmp.path(),
        r#"
project_name: test-project
defaults:
  targets:
    - x86_64-unknown-linux-gnu
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: test-project
        builder: prebuilt
        prebuilt:
          path: "output/test-project_{{ .Target }}"
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "config"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "check config should reject prebuilt without explicit targets"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("`builder: prebuilt`") && stderr.contains("no explicit `targets:`"),
        "stderr should cite the targets-required rule:\n{stderr}"
    );
}

#[test]
fn prebuilt_with_cross_tool_fails_at_config_load() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    create_config(
        tmp.path(),
        r#"
project_name: test-project
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: test-project
        builder: prebuilt
        cross_tool: "/usr/local/bin/my-cross"
        prebuilt:
          path: "output/test-project_{{ .Target }}"
        targets:
          - x86_64-unknown-linux-gnu
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "config"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "check config should reject prebuilt + cross_tool together"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cross_tool") && stderr.contains("mutually exclusive"),
        "stderr should cite mutual-exclusion:\n{stderr}"
    );
}

#[test]
fn prebuilt_with_crate_level_cross_fails_at_config_load() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    create_config(
        tmp.path(),
        r#"
project_name: test-project
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
    cross: zigbuild
    builds:
      - binary: test-project
        builder: prebuilt
        prebuilt:
          path: "output/test-project_{{ .Target }}"
        targets:
          - x86_64-unknown-linux-gnu
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "config"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "check config should reject crate-level cross: + prebuilt build"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("crate-level `cross:` strategy") && stderr.contains("builder: prebuilt"),
        "stderr should cite the cross/prebuilt clash:\n{stderr}"
    );
}

#[test]
fn prebuilt_path_template_renders_os_arch_target_vars() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    let outdir = tmp.path().join("output");
    fs::create_dir_all(&outdir).expect("create output dir");
    // Stage at a path that exercises `Os` / `Arch` template vars:
    // `linux_amd64`, not the raw triple. Tests that Tera substitution
    // wires both vars through the prebuilt planner.
    let staged = outdir.join("myapp_linux_amd64");
    fs::write(&staged, b"fake-cross-binary").expect("write");

    create_config(
        tmp.path(),
        r#"
project_name: myapp
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: myapp
        builder: prebuilt
        prebuilt:
          path: "output/myapp_{{ .Os }}_{{ .Arch }}"
        targets:
          - x86_64-unknown-linux-gnu
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["build"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "build should succeed when `prebuilt.path` template renders correctly:\n{stderr}"
    );
}
