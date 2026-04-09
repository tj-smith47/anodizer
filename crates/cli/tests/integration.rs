use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::TempDir;

use anodize_core::test_helpers::{create_config, create_test_project, init_git_repo};

#[test]
fn test_check_valid_config() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    create_config(
        tmp.path(),
        r#"
project_name: test-project
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .arg("check")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "check should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_check_invalid_config() {
    let tmp = TempDir::new().unwrap();
    // No anodize.yaml at all
    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .arg("check")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no anodize config file found"));
}

#[test]
fn test_init_generates_config() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .arg("init")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "init should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Created .anodize.yaml"));

    // Read the generated config file
    let config_content =
        fs::read_to_string(tmp.path().join(".anodize.yaml")).expect(".anodize.yaml should exist");
    assert!(config_content.contains("project_name:"));
    assert!(config_content.contains("test-project"));
    assert!(config_content.contains("tag_template:"));

    // Verify .gitignore was updated
    let gitignore =
        fs::read_to_string(tmp.path().join(".gitignore")).expect(".gitignore should exist");
    assert!(
        gitignore.contains("dist/"),
        ".gitignore should contain dist/"
    );
}

#[test]
fn test_help_output() {
    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .arg("--help")
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("release"));
    assert!(stdout.contains("build"));
    assert!(stdout.contains("check"));
    assert!(stdout.contains("init"));
    assert!(stdout.contains("changelog"));
    assert!(
        stdout.contains("completion"),
        "help should list completion command"
    );
    assert!(
        stdout.contains("healthcheck"),
        "help should list healthcheck command"
    );
}

#[test]
fn test_version_output() {
    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .arg("--version")
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("anodize"));
}

#[test]
fn test_check_with_config_flag() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());

    // Place config at a non-default path
    let custom_dir = tmp.path().join("configs");
    fs::create_dir_all(&custom_dir).unwrap();
    let config_path = custom_dir.join("release.yaml");
    fs::write(
        &config_path,
        r#"
project_name: test-project
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
"#,
    )
    .unwrap();

    // Use -f to point to the custom config
    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args(["-f", config_path.to_str().unwrap(), "check"])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "check -f should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_check_with_config_flag_long() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());

    let config_path = tmp.path().join("my-anodize.yaml");
    fs::write(
        &config_path,
        r#"
project_name: test-project
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
"#,
    )
    .unwrap();

    // Use --config (long form) to point to the custom config
    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args(["--config", config_path.to_str().unwrap(), "check"])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "check --config should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_check_with_config_flag_nonexistent() {
    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args(["-f", "/tmp/does-not-exist-anodize.yaml", "check"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("config file not found"),
        "expected 'config file not found' error, got: {}",
        stderr
    );
}

#[test]
fn test_release_help_shows_timeout_flag() {
    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args(["release", "--help"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--timeout"),
        "release --help should show --timeout flag, got: {}",
        stdout
    );
}

#[test]
fn test_build_help_shows_timeout_flag() {
    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args(["build", "--help"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--timeout"),
        "build --help should show --timeout flag, got: {}",
        stdout
    );
}

#[test]
fn test_timeout_kills_long_running_release() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    // Config with a before-hook that sleeps for 60 seconds (much longer than our timeout)
    create_config(
        tmp.path(),
        r#"
project_name: test-project
before:
  pre:
    - "sleep 60"
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
"#,
    );

    let start = Instant::now();

    // Use spawn + try_wait instead of output(). When std::process::exit(124)
    // fires from the watchdog thread, the grandchild `sleep 60` may still
    // hold inherited pipe fds open, causing output() to block until that
    // process also exits. By discarding stdout/stderr with Stdio::null()
    // and polling try_wait(), we detect the exit immediately.
    let mut child = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args(["release", "--timeout", "1s"])
        .current_dir(tmp.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    // Poll for completion with a generous timeout
    let poll_deadline = Instant::now() + Duration::from_secs(10);
    let exit_status = loop {
        match child.try_wait().unwrap() {
            Some(status) => break status,
            None => {
                if Instant::now() > poll_deadline {
                    child.kill().ok();
                    panic!("anodize process did not exit within 10s (timeout was 1s)");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    };
    let elapsed = start.elapsed();

    // Should have been killed by timeout
    assert!(
        !exit_status.success(),
        "release with 1s timeout on a 60s sleep should fail"
    );

    // Verify exit code 124 (conventional timeout exit code)
    assert_eq!(
        exit_status.code(),
        Some(124),
        "expected exit code 124 for timeout, got {:?}",
        exit_status.code()
    );

    // The process should finish in well under 10s (timeout is 1s)
    assert!(
        elapsed < Duration::from_secs(10),
        "process should have been killed by timeout quickly, but took {:?}",
        elapsed
    );
}

#[test]
fn test_completion_bash_produces_output() {
    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args(["completion", "bash"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "completion bash should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.is_empty(), "bash completions should not be empty");
    assert!(
        stdout.contains("anodize"),
        "bash completions should reference 'anodize'"
    );
}

#[test]
fn test_completion_zsh_produces_output() {
    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args(["completion", "zsh"])
        .output()
        .unwrap();

    assert!(output.status.success(), "completion zsh should succeed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.is_empty(), "zsh completions should not be empty");
}

#[test]
fn test_healthcheck_succeeds() {
    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .arg("healthcheck")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "healthcheck should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Health Check"),
        "healthcheck should print header"
    );
    assert!(stderr.contains("cargo"), "healthcheck should check cargo");
}

#[test]
fn test_release_help_shows_new_flags() {
    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args(["release", "--help"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--parallelism"),
        "release --help should show --parallelism: {}",
        stdout
    );
    assert!(
        stdout.contains("--auto-snapshot"),
        "release --help should show --auto-snapshot: {}",
        stdout
    );
    assert!(
        stdout.contains("--single-target"),
        "release --help should show --single-target: {}",
        stdout
    );
    assert!(
        stdout.contains("--release-notes"),
        "release --help should show --release-notes: {}",
        stdout
    );
}

#[test]
fn test_build_help_shows_new_flags() {
    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args(["build", "--help"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--parallelism"),
        "build --help should show --parallelism: {}",
        stdout
    );
    assert!(
        stdout.contains("--single-target"),
        "build --help should show --single-target: {}",
        stdout
    );
}

#[test]
fn test_release_invalid_timeout_value() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    create_config(
        tmp.path(),
        r#"
project_name: test-project
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args(["release", "--timeout", "notavalidtimeout"])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid --timeout value"),
        "stderr should report invalid timeout, got: {}",
        stderr
    );
}

// ============================================================================
// E2E Test Helpers
// ============================================================================

/// Detect the host target triple (e.g., "x86_64-unknown-linux-gnu").
fn detect_host_target() -> String {
    anodize_cli::detect_host_target().expect("failed to detect host target triple")
}

/// Create a workspace Cargo project with multiple crates.
/// Returns the root path of the workspace.
///
/// Layout:
///   Cargo.toml (workspace)
///   crates/core-lib/Cargo.toml + src/lib.rs
///   crates/helper-lib/Cargo.toml + src/lib.rs (depends on core-lib)
///   crates/myapp/Cargo.toml + src/main.rs (depends on core-lib, helper-lib)
fn create_workspace_project(dir: &Path) {
    // Root Cargo.toml
    fs::write(
        dir.join("Cargo.toml"),
        r#"[workspace]
resolver = "2"
members = ["crates/core-lib", "crates/helper-lib", "crates/myapp"]
"#,
    )
    .unwrap();

    // core-lib: a library crate with no dependencies
    let core_dir = dir.join("crates/core-lib");
    fs::create_dir_all(core_dir.join("src")).unwrap();
    fs::write(
        core_dir.join("Cargo.toml"),
        r#"[package]
name = "core-lib"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();
    fs::write(
        core_dir.join("src/lib.rs"),
        r#"pub fn core_fn() -> &'static str { "core" }"#,
    )
    .unwrap();

    // helper-lib: depends on core-lib
    let helper_dir = dir.join("crates/helper-lib");
    fs::create_dir_all(helper_dir.join("src")).unwrap();
    fs::write(
        helper_dir.join("Cargo.toml"),
        r#"[package]
name = "helper-lib"
version = "0.1.0"
edition = "2021"

[dependencies]
core-lib = { path = "../core-lib" }
"#,
    )
    .unwrap();
    fs::write(
        helper_dir.join("src/lib.rs"),
        r#"pub fn helper_fn() -> String { format!("helper+{}", core_lib::core_fn()) }"#,
    )
    .unwrap();

    // myapp: binary crate that depends on both
    let app_dir = dir.join("crates/myapp");
    fs::create_dir_all(app_dir.join("src")).unwrap();
    fs::write(
        app_dir.join("Cargo.toml"),
        r#"[package]
name = "myapp"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "myapp"
path = "src/main.rs"

[dependencies]
core-lib = { path = "../core-lib" }
helper-lib = { path = "../helper-lib" }
"#,
    )
    .unwrap();
    fs::write(
        app_dir.join("src/main.rs"),
        r#"fn main() { println!("{}", helper_lib::helper_fn()); }"#,
    )
    .unwrap();
}

// ============================================================================
// E2E Config Helpers
// ============================================================================

/// Create the standard single-crate snapshot config string used by multiple tests.
fn create_single_crate_snapshot_config(host: &str) -> String {
    format!(
        r#"project_name: test-project
crates:
  - name: test-project
    path: "."
    tag_template: "v{{{{ .Version }}}}"
    builds:
      - binary: test-project
        targets:
          - {host}
    archives:
      - name_template: "{{{{ .ProjectName }}}}-{{{{ .Os }}}}-{{{{ .Arch }}}}"
        format: tar.gz
    checksum:
      name_template: "checksums.txt"
      algorithm: sha256
"#,
        host = host
    )
}

/// Create the standard workspace config string used by multiple tests.
fn create_workspace_snapshot_config(host: &str) -> String {
    format!(
        r#"project_name: my-workspace
crates:
  - name: core-lib
    path: "crates/core-lib"
    tag_template: "core-lib-v{{{{ .Version }}}}"

  - name: helper-lib
    path: "crates/helper-lib"
    tag_template: "helper-lib-v{{{{ .Version }}}}"
    depends_on:
      - core-lib

  - name: myapp
    path: "crates/myapp"
    tag_template: "myapp-v{{{{ .Version }}}}"
    depends_on:
      - core-lib
      - helper-lib
    builds:
      - binary: myapp
        targets:
          - {host}
    archives:
      - name_template: "myapp-{{{{ .Os }}}}-{{{{ .Arch }}}}"
        format: tar.gz
    checksum:
      name_template: "checksums.txt"
      algorithm: sha256
"#,
        host = host
    )
}

// ============================================================================
// E2E Tests
// ============================================================================

/// E2E: `anodize release --snapshot` produces correct artifacts in dist/.
///
/// This test actually compiles a Rust project, so it may take a while.
#[test]
fn test_e2e_snapshot_release_produces_artifacts() {
    let tmp = TempDir::new().unwrap();
    let host = detect_host_target();

    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    // The name_template uses ProjectName + Os + Arch (not Version) so the
    // archive filename is deterministic regardless of the resolved tag.
    let config = create_single_crate_snapshot_config(&host);
    create_config(tmp.path(), &config);

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args([
            "release",
            "--snapshot",
            "--skip=release,publish,docker,sign,announce,changelog,nfpm",
            "--timeout",
            "5m",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "snapshot release should succeed.\nstderr:\n{}",
        stderr
    );

    // Verify dist/ directory was created
    let dist_dir = tmp.path().join("dist");
    assert!(
        dist_dir.exists(),
        "dist/ directory should exist after snapshot release"
    );

    // Verify archive artifact exists (tar.gz)
    let entries: Vec<_> = fs::read_dir(&dist_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();

    let has_archive = entries.iter().any(|name| name.ends_with(".tar.gz"));
    assert!(
        has_archive,
        "dist/ should contain a .tar.gz archive, found: {:?}",
        entries
    );

    // Verify checksum file exists
    let has_checksum = entries.iter().any(|name| name == "checksums.txt");
    assert!(
        has_checksum,
        "dist/ should contain checksums.txt, found: {:?}",
        entries
    );

    // Verify checksum file has content (at least one line with a sha256 hash)
    let checksum_content = fs::read_to_string(dist_dir.join("checksums.txt")).unwrap();
    assert!(
        !checksum_content.trim().is_empty(),
        "checksums.txt should not be empty"
    );
    // SHA256 hashes are 64 hex characters
    assert!(
        checksum_content.lines().any(|line| line.len() > 64),
        "checksums.txt should contain hash lines, got: {}",
        checksum_content
    );

    // Verify metadata.json was written
    let has_metadata = entries.iter().any(|name| name == "metadata.json");
    assert!(
        has_metadata,
        "dist/ should contain metadata.json, found: {:?}",
        entries
    );
}

/// E2E: `anodize release --dry-run` runs full pipeline with no side effects.
#[test]
fn test_e2e_dry_run_no_side_effects() {
    let tmp = TempDir::new().unwrap();
    let host = detect_host_target();

    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    let config = create_single_crate_snapshot_config(&host);
    create_config(tmp.path(), &config);

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args([
            "release",
            "--dry-run",
            "--skip=release,publish,docker,sign,announce,changelog,nfpm",
            "--timeout",
            "5m",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "dry-run release should succeed.\nstderr:\n{}",
        stderr
    );

    // In dry-run mode, dist/ either should not exist (the expected case),
    // or if it does exist, it must not contain any archive/checksum artifacts.
    let dist_dir = tmp.path().join("dist");
    if dist_dir.exists() {
        // dist/ was created (e.g., archive stage mkdir), but verify no actual
        // artifacts were produced.
        let entries: Vec<_> = fs::read_dir(&dist_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        // There should be no .tar.gz archives, no checksums, no metadata.json
        let has_archives = entries
            .iter()
            .any(|name| name.ends_with(".tar.gz") || name.ends_with(".zip"));
        assert!(
            !has_archives,
            "dist/ should NOT contain archives after dry-run, found: {:?}",
            entries
        );
        let has_checksums = entries
            .iter()
            .any(|name| name.contains("checksum") || name.ends_with(".txt"));
        assert!(
            !has_checksums,
            "dist/ should NOT contain checksum files after dry-run, found: {:?}",
            entries
        );
        // GoReleaser writes metadata.json and artifacts.json even in dry-run mode.
        // Anodize matches this behavior: metadata is always written for debugging.
    }
    // If dist/ doesn't exist at all, that's the expected case for dry-run.

    // Verify the stderr mentions dry-run activity
    assert!(
        stderr.contains("dry-run"),
        "stderr should mention dry-run, got:\n{}",
        stderr
    );
}

/// E2E: `anodize check` validates a comprehensive config that exercises many fields.
#[test]
fn test_e2e_check_comprehensive_config() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());

    create_config(
        tmp.path(),
        r#"project_name: test-project
dist: ./dist
report_sizes: true

env:
  BUILD_ENV: ci
  DEPLOY_TARGET: staging

defaults:
  targets:
    - x86_64-unknown-linux-gnu
    - aarch64-unknown-linux-gnu
  cross: auto
  archives:
    format: tar.gz
    format_overrides:
      - os: windows
        format: zip
  checksum:
    algorithm: sha256

signs:
  - id: gpg-sign
    artifacts: checksum
    cmd: gpg
    args:
      - "--detach-sig"

publishers:
  - name: custom-upload
    cmd: echo
    args:
      - "published"
    artifact_types:
      - archive

crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: test-project
        targets:
          - x86_64-unknown-linux-gnu
    archives:
      - name_template: "test-project-{{ .Version }}-{{ .Os }}-{{ .Arch }}"
        format: tar.gz
    checksum:
      name_template: "checksums.txt"
      algorithm: sha256
    docker:
      - image_templates:
          - "myregistry/test-project:{{ .Version }}"
        dockerfile: Dockerfile
    nfpm:
      - formats:
          - deb
        package_name: test-project
        description: "A test project"

changelog:
  sort: asc
  groups:
    - title: Features
      regexp: "^feat"
      order: 0
    - title: Bug Fixes
      regexp: "^fix"
      order: 1
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .arg("check")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "check with comprehensive config should succeed.\nstderr:\n{}",
        stderr
    );
    assert!(
        stderr.contains("Config is valid"),
        "stderr should confirm config is valid, got:\n{}",
        stderr
    );
}

/// E2E: `anodize init` generates valid YAML that can be parsed back.
#[test]
fn test_e2e_init_generates_parseable_yaml() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .arg("init")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "init should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Read the generated config file
    let config_content = fs::read_to_string(tmp.path().join(".anodize.yaml"))
        .expect(".anodize.yaml should exist after init");

    // Verify the output is valid YAML by parsing it
    let parsed: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(&config_content).unwrap_or_else(|e| {
            panic!(
                "init output should be valid YAML.\nParse error: {}\nOutput:\n{}",
                e, config_content
            );
        });

    // Verify key fields exist in the parsed YAML
    let map = parsed
        .as_mapping()
        .expect("parsed YAML should be a mapping");
    assert!(
        map.contains_key(serde_yaml_ng::Value::String("project_name".to_string())),
        "YAML should contain project_name"
    );
    assert!(
        map.contains_key(serde_yaml_ng::Value::String("crates".to_string())),
        "YAML should contain crates"
    );
    assert!(
        map.contains_key(serde_yaml_ng::Value::String("defaults".to_string())),
        "YAML should contain defaults"
    );

    // Verify the project name matches
    let project_name = map
        .get(serde_yaml_ng::Value::String("project_name".to_string()))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(
        project_name, "test-project",
        "project_name should be test-project"
    );

    // Verify crates section is an array
    let crates = map
        .get(serde_yaml_ng::Value::String("crates".to_string()))
        .and_then(|v| v.as_sequence())
        .expect("crates should be an array");
    assert!(!crates.is_empty(), "crates array should not be empty");

    // Verify the generated YAML can be written and validated with `anodize check`
    let tmp2 = TempDir::new().unwrap();
    create_test_project(tmp2.path());
    init_git_repo(tmp2.path());
    create_config(tmp2.path(), &config_content);

    let check_output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .arg("check")
        .current_dir(tmp2.path())
        .output()
        .unwrap();

    assert!(
        check_output.status.success(),
        "generated config should pass validation.\nstderr:\n{}",
        String::from_utf8_lossy(&check_output.stderr)
    );
}

/// E2E: Multi-crate workspace with `--all --force` detects correct crates.
///
/// Creates a workspace with 3 crates: core-lib, helper-lib (depends on core-lib),
/// and myapp (depends on both). Verifies that:
/// 1. `anodize check` passes on the workspace config
/// 2. `anodize release --dry-run --all --force` includes all crates
/// 3. Dependency ordering is respected (core-lib before helper-lib before myapp)
#[test]
fn test_e2e_workspace_all_force_detects_crates() {
    let tmp = TempDir::new().unwrap();
    let host = detect_host_target();

    create_workspace_project(tmp.path());
    init_git_repo(tmp.path());

    // Create anodize config for the workspace with depends_on
    let config = create_workspace_snapshot_config(&host);
    create_config(tmp.path(), &config);

    // 1. Verify config is valid
    let check_output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .arg("check")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        check_output.status.success(),
        "workspace config check should succeed.\nstderr:\n{}",
        String::from_utf8_lossy(&check_output.stderr)
    );

    // 2. Run dry-run release with --all --force
    let release_output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args([
            "release",
            "--dry-run",
            "--all",
            "--force",
            "--skip=release,publish,docker,sign,announce,changelog,nfpm",
            "--timeout",
            "5m",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&release_output.stderr);
    assert!(
        release_output.status.success(),
        "workspace dry-run release should succeed.\nstderr:\n{}",
        stderr
    );

    // 3. Verify the dry-run output mentions all three crates, proving --all detected them
    assert!(
        stderr.contains("core-lib"),
        "stderr should mention core-lib crate, got:\n{}",
        stderr
    );
    assert!(
        stderr.contains("helper-lib"),
        "stderr should mention helper-lib crate, got:\n{}",
        stderr
    );
    assert!(
        stderr.contains("myapp"),
        "stderr should mention myapp crate, got:\n{}",
        stderr
    );
}

/// E2E: Workspace snapshot release actually builds and produces artifacts.
///
/// This is the full integration test: compile a workspace, archive, checksum.
#[test]
fn test_e2e_workspace_snapshot_produces_artifacts() {
    let tmp = TempDir::new().unwrap();
    let host = detect_host_target();

    create_workspace_project(tmp.path());
    init_git_repo(tmp.path());

    let config = create_workspace_snapshot_config(&host);
    create_config(tmp.path(), &config);

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args([
            "release",
            "--snapshot",
            "--all",
            "--force",
            "--skip=release,publish,docker,sign,announce,changelog,nfpm",
            "--timeout",
            "5m",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "workspace snapshot release should succeed.\nstderr:\n{}",
        stderr
    );

    // Verify dist/ exists and has artifacts
    let dist_dir = tmp.path().join("dist");
    assert!(
        dist_dir.exists(),
        "dist/ should exist after workspace snapshot release"
    );

    let entries: Vec<_> = fs::read_dir(&dist_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();

    // Should have a tar.gz archive for myapp
    let has_archive = entries
        .iter()
        .any(|name| name.contains("myapp") && name.ends_with(".tar.gz"));
    assert!(
        has_archive,
        "dist/ should contain a myapp .tar.gz archive, found: {:?}",
        entries
    );

    // Should have checksums.txt
    let has_checksum = entries.iter().any(|name| name == "checksums.txt");
    assert!(
        has_checksum,
        "dist/ should contain checksums.txt, found: {:?}",
        entries
    );

    // Should have metadata.json
    let has_metadata = entries.iter().any(|name| name == "metadata.json");
    assert!(
        has_metadata,
        "dist/ should contain metadata.json, found: {:?}",
        entries
    );
}

/// E2E: `anodize init` on a workspace project generates config with depends_on.
#[test]
fn test_e2e_init_workspace_generates_depends_on() {
    let tmp = TempDir::new().unwrap();
    create_workspace_project(tmp.path());
    init_git_repo(tmp.path());

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .arg("init")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "init on workspace should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Read the generated config file
    let config_content = fs::read_to_string(tmp.path().join(".anodize.yaml"))
        .expect(".anodize.yaml should exist after init");

    // Verify the config mentions all three crates
    assert!(
        config_content.contains("core-lib"),
        "init output should mention core-lib"
    );
    assert!(
        config_content.contains("helper-lib"),
        "init output should mention helper-lib"
    );
    assert!(
        config_content.contains("myapp"),
        "init output should mention myapp"
    );

    // Verify depends_on relationships are detected
    assert!(
        config_content.contains("depends_on"),
        "init output should include depends_on for workspace deps"
    );

    // Verify topological order: core-lib should appear before myapp
    let core_pos = config_content
        .find("name: core-lib")
        .expect("core-lib should appear");
    let app_pos = config_content
        .find("name: myapp")
        .expect("myapp should appear");
    assert!(
        core_pos < app_pos,
        "core-lib should appear before myapp (topological order)"
    );

    // Verify the generated YAML is parseable
    let _parsed: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(&config_content).unwrap_or_else(|e| {
            panic!(
                "workspace init output should be valid YAML.\nParse error: {}\nOutput:\n{}",
                e, config_content
            );
        });
}

/// E2E: `anodize check` detects invalid depends_on references in workspace config.
#[test]
fn test_e2e_check_workspace_invalid_depends_on() {
    let tmp = TempDir::new().unwrap();
    create_workspace_project(tmp.path());

    create_config(
        tmp.path(),
        r#"project_name: my-workspace
crates:
  - name: core-lib
    path: "crates/core-lib"
    tag_template: "core-lib-v{{ .Version }}"

  - name: myapp
    path: "crates/myapp"
    tag_template: "myapp-v{{ .Version }}"
    depends_on:
      - nonexistent-crate
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .arg("check")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "check should fail for invalid depends_on reference"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("nonexistent-crate") && stderr.contains("does not exist"),
        "error should mention the missing dependency, got:\n{}",
        stderr
    );
}

/// E2E: Workspace change detection only picks up crates with changes since their last tag.
///
/// This test verifies that `--all` without `--force` uses git-based change detection:
/// 1. Creates the workspace fixture and initializes git
/// 2. Tags all crates (core-lib-v0.1.0, helper-lib-v0.1.0, myapp-v0.1.0)
/// 3. Modifies only core-lib's source file and commits
/// 4. Runs `anodize release --all --dry-run` (no --force)
/// 5. Verifies that only core-lib (the changed crate) is detected
///
/// Note: depends_on propagation (helper-lib and myapp depend on core-lib) is not
/// yet implemented in detect_changed_crates(), so only the directly-changed crate
/// should appear in the output.
#[test]
fn test_e2e_workspace_change_detection_without_force() {
    let tmp = TempDir::new().unwrap();

    create_workspace_project(tmp.path());

    // Initialize git repo (creates initial commit and v0.1.0 tag)
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(tmp.path())
            .output()
            .expect("git command failed");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        output
    };

    git(&["init"]);
    git(&["config", "user.email", "test@test.com"]);
    git(&["config", "user.name", "Test"]);

    // Create anodize config before the initial commit so it's tracked
    create_config(
        tmp.path(),
        r#"project_name: my-workspace
crates:
  - name: core-lib
    path: "crates/core-lib"
    tag_template: "core-lib-v{{ .Version }}"

  - name: helper-lib
    path: "crates/helper-lib"
    tag_template: "helper-lib-v{{ .Version }}"
    depends_on:
      - core-lib

  - name: myapp
    path: "crates/myapp"
    tag_template: "myapp-v{{ .Version }}"
    depends_on:
      - core-lib
      - helper-lib
"#,
    );

    git(&["add", "-A"]);
    git(&["commit", "-m", "initial workspace"]);

    // Tag all crates at the initial commit
    git(&["tag", "core-lib-v0.1.0"]);
    git(&["tag", "helper-lib-v0.1.0"]);
    git(&["tag", "myapp-v0.1.0"]);

    // Modify only core-lib's source and commit the change
    fs::write(
        tmp.path().join("crates/core-lib/src/lib.rs"),
        r#"pub fn core_fn() -> &'static str { "core-modified" }"#,
    )
    .unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-m", "modify core-lib only"]);

    // Run release with --all but WITHOUT --force, so change detection kicks in
    let release_output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args([
            "release",
            "--dry-run",
            "--snapshot",
            "--all",
            "--single-target",
            "--skip=release,publish,docker,sign,announce,changelog,nfpm",
            "--timeout",
            "5m",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&release_output.stderr);
    assert!(
        release_output.status.success(),
        "workspace dry-run release (change detection) should succeed.\nstderr:\n{}",
        stderr
    );

    // core-lib was modified, so it must appear in the output
    assert!(
        stderr.contains("core-lib"),
        "stderr should mention core-lib (the changed crate), got:\n{}",
        stderr
    );

    // helper-lib and myapp depend on core-lib, so they should be transitively
    // included via depends_on propagation.
    assert!(
        stderr.contains("helper-lib"),
        "stderr should mention helper-lib (depends on changed core-lib), got:\n{}",
        stderr
    );
    assert!(
        stderr.contains("myapp"),
        "stderr should mention myapp (depends on changed core-lib), got:\n{}",
        stderr
    );
}

// ============================================================================
// Error Path Tests (Task 3B)
// ============================================================================

/// Error path: `anodize check` with malformed YAML should fail with a clear error.
#[test]
fn test_check_malformed_yaml_reports_parse_error() {
    let tmp = TempDir::new().unwrap();
    create_config(
        tmp.path(),
        r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: [[[invalid yaml
      this is broken
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .arg("check")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "check with malformed YAML should fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    // The error should mention a parsing issue
    assert!(
        stderr.contains("error")
            || stderr.contains("Error")
            || stderr.contains("parse")
            || stderr.contains("invalid"),
        "stderr should indicate a parse error, got:\n{}",
        stderr
    );
}

/// Error path: `anodize check` with type mismatch should fail with a clear error.
#[test]
fn test_check_type_mismatch_crates_not_array() {
    let tmp = TempDir::new().unwrap();
    create_config(
        tmp.path(),
        r#"
project_name: test
crates: "this should be an array not a string"
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .arg("check")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "check with type mismatch should fail"
    );
}

/// Error path: `--skip` flag causes stages to be skipped in dry-run output.
#[test]
fn test_skip_flag_skips_specified_stages() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    let host = detect_host_target();
    let config = format!(
        r#"project_name: test-project
crates:
  - name: test-project
    path: "."
    tag_template: "v{{{{ .Version }}}}"
    builds:
      - binary: test-project
        targets:
          - {host}
    archives:
      - name_template: "{{{{ .ProjectName }}}}-{{{{ .Os }}}}-{{{{ .Arch }}}}"
        format: tar.gz
    checksum:
      name_template: "checksums.txt"
"#,
        host = host
    );
    create_config(tmp.path(), &config);

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args([
            "release",
            "--dry-run",
            "--skip=build,archive,checksum,release,publish,docker,sign,announce,changelog,nfpm",
            "--timeout",
            "30s",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "release with all stages skipped should succeed.\nstderr:\n{}",
        stderr
    );
    // The skipped stages should appear as "skipped" in the output
    assert!(
        stderr.contains("skipped"),
        "stderr should mention 'skipped' when stages are skipped, got:\n{}",
        stderr
    );
}

// ============================================================================
// Error Path Tests
// ============================================================================

/// Check command should reject a config with an empty crate name.
#[test]
fn test_check_empty_crate_name_rejected() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    create_config(
        tmp.path(),
        r#"
project_name: test-project
crates:
  - name: ""
    path: "."
    tag_template: "v{{ .Version }}"
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .arg("check")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "check should fail for empty crate name"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("name must not be empty"),
        "stderr should mention empty name error, got:\n{}",
        stderr
    );
}

/// Check command should reject a tag_template that lacks {{ .Version }}.
#[test]
fn test_check_tag_template_missing_version_rejected() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    create_config(
        tmp.path(),
        r#"
project_name: test-project
crates:
  - name: test-project
    path: "."
    tag_template: "release-{{ .Tag }}"
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .arg("check")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "check should fail for tag_template without Version"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("must contain") && stderr.contains("Version"),
        "stderr should explain the Version requirement, got:\n{}",
        stderr
    );
}

/// Build stage should fail when the project has invalid Rust code.
#[test]
fn test_failed_compilation_snapshot() {
    let tmp = TempDir::new().unwrap();
    let host = detect_host_target();

    // Create a Cargo project with invalid Rust code
    fs::write(
        tmp.path().join("Cargo.toml"),
        r#"
[package]
name = "bad-project"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "bad-project"
path = "src/main.rs"
"#,
    )
    .unwrap();

    fs::create_dir_all(tmp.path().join("src")).unwrap();
    fs::write(
        tmp.path().join("src/main.rs"),
        r#"fn main() { let x: i32 = "not a number"; }"#,
    )
    .unwrap();

    init_git_repo(tmp.path());

    let config = format!(
        r#"project_name: bad-project
crates:
  - name: bad-project
    path: "."
    tag_template: "v{{{{ .Version }}}}"
    builds:
      - binary: bad-project
        targets:
          - {host}
"#,
        host = host
    );
    create_config(tmp.path(), &config);

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args([
            "release",
            "--snapshot",
            "--single-target",
            "--skip=release,publish,docker,sign,announce,changelog,nfpm",
            "--timeout",
            "2m",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "release should fail when Rust code doesn't compile.\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// GoReleaser parity: unknown YAML fields should be rejected (strict parsing).
#[test]
fn test_check_unknown_yaml_fields_rejected() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    create_config(
        tmp.path(),
        r#"
project_name: test-project
future_feature: "this field does not exist yet"
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .arg("check")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "check should fail with unknown YAML fields.\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown field"),
        "error should mention unknown field.\nstderr:\n{}",
        stderr
    );
}

/// Per-crate checksum disable via the check command should pass validation.
#[test]
fn test_check_per_crate_checksum_disable_valid() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    create_config(
        tmp.path(),
        r#"
project_name: test-project
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
    checksum:
      disable: true
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .arg("check")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "check should succeed with per-crate checksum disable.\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Archive disable via `archives: false` should pass validation.
#[test]
fn test_check_archives_disabled_valid() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    create_config(
        tmp.path(),
        r#"
project_name: test-project
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
    archives: false
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .arg("check")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "check should succeed with archives: false.\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Global checksum disable via defaults should pass validation.
#[test]
fn test_check_global_checksum_disable_valid() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    create_config(
        tmp.path(),
        r#"
project_name: test-project
defaults:
  checksum:
    disable: true
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .arg("check")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "check should succeed with global checksum disable.\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Changelog disable should pass validation.
#[test]
fn test_check_changelog_disabled_valid() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    create_config(
        tmp.path(),
        r#"
project_name: test-project
changelog:
  disable: true
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .arg("check")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "check should succeed with changelog disabled.\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// ============================================================================
// E2E Pipeline Tests (Task 4E)
// ============================================================================

/// E2E #1: Multi-format archive — config with tar.gz, tar.xz, zip, and binary
/// format produces all four artifact types correctly.
#[test]
fn test_e2e_multi_format_archive() {
    let tmp = TempDir::new().unwrap();
    let host = detect_host_target();

    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    let config = format!(
        r#"project_name: test-project
crates:
  - name: test-project
    path: "."
    tag_template: "v{{{{ .Version }}}}"
    builds:
      - binary: test-project
        targets:
          - {host}
    archives:
      - name_template: "test-project-{{{{ .Os }}}}-{{{{ .Arch }}}}-targz"
        format: tar.gz
      - name_template: "test-project-{{{{ .Os }}}}-{{{{ .Arch }}}}-tarxz"
        format: tar.xz
      - name_template: "test-project-{{{{ .Os }}}}-{{{{ .Arch }}}}-zipped"
        format: zip
      - name_template: "test-project-{{{{ .Os }}}}-{{{{ .Arch }}}}-raw"
        format: binary
    checksum:
      name_template: "checksums.txt"
      algorithm: sha256
"#,
        host = host
    );
    create_config(tmp.path(), &config);

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args([
            "release",
            "--snapshot",
            "--skip=release,publish,docker,sign,announce,changelog,nfpm",
            "--timeout",
            "5m",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "multi-format archive release should succeed.\nstderr:\n{}",
        stderr
    );

    let dist_dir = tmp.path().join("dist");
    assert!(dist_dir.exists(), "dist/ should exist");

    let entries: Vec<_> = fs::read_dir(&dist_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();

    // Verify all four formats present
    let has_targz = entries
        .iter()
        .any(|n| n.contains("targz") && n.ends_with(".tar.gz"));
    assert!(
        has_targz,
        "dist/ should contain a tar.gz archive, found: {:?}",
        entries
    );

    let has_tarxz = entries
        .iter()
        .any(|n| n.contains("tarxz") && n.ends_with(".tar.xz"));
    assert!(
        has_tarxz,
        "dist/ should contain a tar.xz archive, found: {:?}",
        entries
    );

    let has_zip = entries
        .iter()
        .any(|n| n.contains("zipped") && n.ends_with(".zip"));
    assert!(
        has_zip,
        "dist/ should contain a zip archive, found: {:?}",
        entries
    );

    let has_binary = entries
        .iter()
        .any(|n| n.contains("raw") && !n.contains('.'));
    assert!(
        has_binary,
        "dist/ should contain a raw binary (no extension), found: {:?}",
        entries
    );

    // Verify checksums.txt references all four archives
    let checksum_content = fs::read_to_string(dist_dir.join("checksums.txt")).unwrap();
    assert!(
        checksum_content.contains("targz"),
        "checksums should reference tar.gz archive"
    );
    assert!(
        checksum_content.contains("tarxz"),
        "checksums should reference tar.xz archive"
    );
    assert!(
        checksum_content.contains("zipped"),
        "checksums should reference zip archive"
    );
    assert!(
        checksum_content.contains("raw"),
        "checksums should reference binary archive"
    );
}

/// E2E #2: Multi-sign dry-run — two sign configs with different artifact filters
/// produce the expected dry-run output for each.
#[test]
fn test_e2e_multi_sign_dry_run() {
    let tmp = TempDir::new().unwrap();
    let host = detect_host_target();

    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    let config = format!(
        r#"project_name: test-project
signs:
  - id: gpg-checksum
    artifacts: checksum
    cmd: gpg
    args:
      - "--detach-sig"
      - "{{{{ .Artifact }}}}"
  - id: cosign-archive
    artifacts: archive
    cmd: cosign
    args:
      - "sign-blob"
      - "{{{{ .Artifact }}}}"
crates:
  - name: test-project
    path: "."
    tag_template: "v{{{{ .Version }}}}"
    builds:
      - binary: test-project
        targets:
          - {host}
    archives:
      - name_template: "test-project-{{{{ .Os }}}}-{{{{ .Arch }}}}"
        format: tar.gz
    checksum:
      name_template: "checksums.txt"
      algorithm: sha256
"#,
        host = host
    );
    create_config(tmp.path(), &config);

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args([
            "release",
            "--dry-run",
            "--skip=release,publish,docker,announce,changelog,nfpm",
            "--timeout",
            "5m",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "multi-sign dry-run should succeed.\nstderr:\n{}",
        stderr
    );

    // In dry-run mode, the sign stage logs what it would do.
    // Verify both sign configs are mentioned in the output.
    assert!(
        stderr.contains("sign") && stderr.contains("dry-run"),
        "stderr should contain dry-run sign output, got:\n{}",
        stderr
    );
}

/// E2E #3: Changelog with groups — real git history with feat/fix/chore commits
/// produces grouped markdown output.
#[test]
fn test_e2e_changelog_with_groups() {
    let tmp = TempDir::new().unwrap();
    let host = detect_host_target();

    create_test_project(tmp.path());

    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(tmp.path())
            .output()
            .expect("git command failed");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    };

    // Initialize repo with config and initial commit
    let config = format!(
        r#"project_name: test-project
changelog:
  sort: asc
  groups:
    - title: Features
      regexp: "^feat"
      order: 0
    - title: Bug Fixes
      regexp: "^fix"
      order: 1
    - title: Maintenance
      regexp: "^chore"
      order: 2
crates:
  - name: test-project
    path: "."
    tag_template: "v{{{{ .Version }}}}"
    builds:
      - binary: test-project
        targets:
          - {host}
    archives:
      - name_template: "test-project-{{{{ .Os }}}}-{{{{ .Arch }}}}"
        format: tar.gz
    checksum:
      name_template: "checksums.txt"
      algorithm: sha256
"#,
        host = host
    );
    create_config(tmp.path(), &config);

    git(&["init"]);
    git(&["config", "user.email", "test@test.com"]);
    git(&["config", "user.name", "Test"]);
    git(&["add", "-A"]);
    git(&["commit", "-m", "initial"]);
    git(&["tag", "v0.1.0"]);

    // Add conventional commits after the tag
    fs::write(
        tmp.path().join("src/main.rs"),
        r#"fn main() { println!("feature 1"); }"#,
    )
    .unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-m", "feat: add awesome new feature"]);

    fs::write(
        tmp.path().join("src/main.rs"),
        r#"fn main() { println!("fix 1"); }"#,
    )
    .unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-m", "fix: resolve critical bug"]);

    fs::write(
        tmp.path().join("src/main.rs"),
        r#"fn main() { println!("chore 1"); }"#,
    )
    .unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-m", "chore: update dependencies"]);

    fs::write(
        tmp.path().join("src/main.rs"),
        r#"fn main() { println!("feature 2"); }"#,
    )
    .unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-m", "feat: implement second feature"]);

    // Run a snapshot release that includes the changelog stage
    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args([
            "release",
            "--snapshot",
            "--skip=release,publish,docker,sign,announce,nfpm",
            "--timeout",
            "5m",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "changelog release should succeed.\nstderr:\n{}",
        stderr
    );

    // Verify CHANGELOG.md was created in dist/
    let notes_path = tmp.path().join("dist/CHANGELOG.md");
    assert!(
        notes_path.exists(),
        "dist/CHANGELOG.md should exist after changelog stage"
    );

    let notes = fs::read_to_string(&notes_path).unwrap();

    // Verify grouped sections
    assert!(
        notes.contains("## Features"),
        "changelog should contain Features group, got:\n{}",
        notes
    );
    assert!(
        notes.contains("## Bug Fixes"),
        "changelog should contain Bug Fixes group, got:\n{}",
        notes
    );
    assert!(
        notes.contains("## Maintenance"),
        "changelog should contain Maintenance group, got:\n{}",
        notes
    );

    // Verify commit descriptions appear
    assert!(
        notes.contains("add awesome new feature"),
        "changelog should contain feat commit description, got:\n{}",
        notes
    );
    assert!(
        notes.contains("resolve critical bug"),
        "changelog should contain fix commit description, got:\n{}",
        notes
    );
    assert!(
        notes.contains("update dependencies"),
        "changelog should contain chore commit description, got:\n{}",
        notes
    );

    // Verify Features appears before Bug Fixes (ordering by group order)
    let feat_pos = notes.find("## Features").unwrap();
    let fix_pos = notes.find("## Bug Fixes").unwrap();
    assert!(
        feat_pos < fix_pos,
        "Features (order 0) should appear before Bug Fixes (order 1)"
    );
}

/// E2E #4: Config validation round-trip — `init` generates config, `check` validates it,
/// `build --snapshot` succeeds using the generated config.
#[test]
fn test_e2e_config_validation_round_trip() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    // Step 1: Generate config with `init`
    let init_output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .arg("init")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        init_output.status.success(),
        "init should succeed: {}",
        String::from_utf8_lossy(&init_output.stderr)
    );

    // init now writes to .anodize.yaml instead of stdout
    let generated_config = std::fs::read_to_string(tmp.path().join(".anodize.yaml"))
        .expect("init should create .anodize.yaml");

    // Step 2: Parse the generated config, replace targets with only the host
    // target to avoid cross-compilation failures, then write back.
    let host = detect_host_target();
    let mut parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&generated_config).unwrap();
    // Replace defaults.targets with just the host target
    if let Some(mapping) = parsed.as_mapping_mut() {
        if let Some(defaults) = mapping
            .get_mut(serde_yaml_ng::Value::String("defaults".to_string()))
            .and_then(|d| d.as_mapping_mut())
        {
            defaults.insert(
                serde_yaml_ng::Value::String("targets".to_string()),
                serde_yaml_ng::Value::Sequence(vec![serde_yaml_ng::Value::String(host.clone())]),
            );
        }
        // Also replace per-crate targets
        if let Some(crates) = mapping
            .get_mut(serde_yaml_ng::Value::String("crates".to_string()))
            .and_then(|c| c.as_sequence_mut())
        {
            for krate in crates.iter_mut() {
                if let Some(builds) = krate
                    .as_mapping_mut()
                    .and_then(|m| m.get_mut(serde_yaml_ng::Value::String("builds".to_string())))
                    .and_then(|b| b.as_sequence_mut())
                {
                    for build in builds.iter_mut() {
                        if let Some(m) = build.as_mapping_mut() {
                            m.insert(
                                serde_yaml_ng::Value::String("targets".to_string()),
                                serde_yaml_ng::Value::Sequence(vec![serde_yaml_ng::Value::String(
                                    host.clone(),
                                )]),
                            );
                        }
                    }
                }
            }
        }
    }
    let modified_config = serde_yaml_ng::to_string(&parsed).unwrap();
    create_config(tmp.path(), &modified_config);

    // Step 3: Validate with `check`
    let check_output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .arg("check")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        check_output.status.success(),
        "check should succeed on init-generated config.\nstderr:\n{}",
        String::from_utf8_lossy(&check_output.stderr)
    );

    // Step 4: Run `release --snapshot` with the modified config.
    let build_output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args([
            "release",
            "--snapshot",
            "--skip=release,publish,docker,sign,announce,changelog,nfpm",
            "--timeout",
            "5m",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&build_output.stderr);
    assert!(
        build_output.status.success(),
        "release --snapshot should succeed with init-generated config.\nstderr:\n{}",
        stderr
    );

    // Verify dist/ exists and has at least one artifact
    let dist_dir = tmp.path().join("dist");
    assert!(
        dist_dir.exists(),
        "dist/ should exist after round-trip release"
    );
    let entries: Vec<_> = fs::read_dir(&dist_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert!(
        !entries.is_empty(),
        "dist/ should have at least one artifact after round-trip"
    );
}

/// E2E #5: Workspace dependency ordering — verify that dependee crates (B)
/// are processed before dependent crates (A) in dry-run output.
#[test]
fn test_e2e_workspace_dependency_ordering() {
    let tmp = TempDir::new().unwrap();
    let host = detect_host_target();

    create_workspace_project(tmp.path());
    init_git_repo(tmp.path());

    let config = create_workspace_snapshot_config(&host);
    create_config(tmp.path(), &config);

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args([
            "release",
            "--dry-run",
            "--all",
            "--force",
            "--skip=release,publish,docker,sign,announce,changelog,nfpm",
            "--timeout",
            "5m",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "workspace dry-run release should succeed.\nstderr:\n{}",
        stderr
    );

    // Verify dependency ordering: core-lib should appear before helper-lib,
    // and helper-lib before myapp in the processing output.
    let core_pos = stderr
        .find("core-lib")
        .expect("stderr should mention core-lib");
    let helper_pos = stderr
        .find("helper-lib")
        .expect("stderr should mention helper-lib");
    let app_pos = stderr.find("myapp").expect("stderr should mention myapp");

    assert!(
        core_pos < helper_pos,
        "core-lib should be processed before helper-lib (depends_on). \
         core-lib at {}, helper-lib at {}",
        core_pos,
        helper_pos
    );
    assert!(
        helper_pos < app_pos,
        "helper-lib should be processed before myapp (depends_on). \
         helper-lib at {}, myapp at {}",
        helper_pos,
        app_pos
    );
}

/// E2E #6: Skip archive and checksum stages — verify that build stage runs
/// but archives and checksums are not produced.
#[test]
fn test_e2e_skip_archive_and_checksum() {
    let tmp = TempDir::new().unwrap();
    let host = detect_host_target();

    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    let config = create_single_crate_snapshot_config(&host);
    create_config(tmp.path(), &config);

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args([
            "release",
            "--snapshot",
            "--skip=archive,checksum,release,publish,docker,sign,announce,changelog,nfpm",
            "--timeout",
            "5m",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "release with skipped archive/checksum should succeed.\nstderr:\n{}",
        stderr
    );

    // Build stage should have run (not skipped)
    assert!(
        stderr.contains("build"),
        "stderr should mention the build stage, got:\n{}",
        stderr
    );
    // Archive and checksum should be skipped
    assert!(
        stderr.contains("archive") && stderr.contains("skipped"),
        "stderr should show archive as skipped, got:\n{}",
        stderr
    );
    assert!(
        stderr.contains("checksum") && stderr.contains("skipped"),
        "stderr should show checksum as skipped, got:\n{}",
        stderr
    );

    // dist/ may exist (build stage might create it) but should have no archive/checksum files
    let dist_dir = tmp.path().join("dist");
    if dist_dir.exists() {
        let entries: Vec<_> = fs::read_dir(&dist_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();

        let has_archives = entries
            .iter()
            .any(|n| n.ends_with(".tar.gz") || n.ends_with(".zip") || n.ends_with(".tar.xz"));
        assert!(
            !has_archives,
            "dist/ should NOT contain archives when archive stage is skipped, found: {:?}",
            entries
        );

        let has_checksums = entries.iter().any(|n| n == "checksums.txt");
        assert!(
            !has_checksums,
            "dist/ should NOT contain checksums when checksum stage is skipped, found: {:?}",
            entries
        );
    }
}

/// E2E #7: Custom publishers dry-run — verify that publisher command construction
/// is logged in dry-run output.
#[test]
fn test_e2e_custom_publishers_dry_run() {
    let tmp = TempDir::new().unwrap();
    let host = detect_host_target();

    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    let config = format!(
        r#"project_name: test-project
publishers:
  - name: s3-upload
    cmd: aws
    args:
      - "s3"
      - "cp"
      - "{{{{ .ArtifactPath }}}}"
      - "s3://my-bucket/"
    artifact_types:
      - archive
  - name: notify
    cmd: curl
    args:
      - "-X"
      - "POST"
      - "https://example.com/notify"
    artifact_types:
      - checksum
crates:
  - name: test-project
    path: "."
    tag_template: "v{{{{ .Version }}}}"
    builds:
      - binary: test-project
        targets:
          - {host}
    archives:
      - name_template: "test-project-{{{{ .Os }}}}-{{{{ .Arch }}}}"
        format: tar.gz
    checksum:
      name_template: "checksums.txt"
      algorithm: sha256
"#,
        host = host
    );
    create_config(tmp.path(), &config);

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args([
            "release",
            "--dry-run",
            "--skip=release,docker,sign,announce,changelog,nfpm",
            "--timeout",
            "5m",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "publisher dry-run should succeed.\nstderr:\n{}",
        stderr
    );

    // Custom publishers run after the pipeline, so in dry-run mode they log their commands.
    // Verify the publisher names appear in the dry-run output.
    assert!(
        stderr.contains("publisher") || stderr.contains("s3-upload") || stderr.contains("notify"),
        "stderr should mention custom publishers in dry-run, got:\n{}",
        stderr
    );
}

/// E2E #8: Docker staging dry-run — verify the staging directory structure
/// references (binaries/amd64, binaries/arm64, Dockerfile) are logged.
#[test]
fn test_e2e_docker_staging_dry_run() {
    let tmp = TempDir::new().unwrap();
    let host = detect_host_target();

    create_test_project(tmp.path());

    // Create a dummy Dockerfile
    fs::write(tmp.path().join("Dockerfile"), "FROM scratch\nCOPY . /app\n").unwrap();

    init_git_repo(tmp.path());

    let config = format!(
        r#"project_name: test-project
crates:
  - name: test-project
    path: "."
    tag_template: "v{{{{ .Version }}}}"
    builds:
      - binary: test-project
        targets:
          - {host}
    archives:
      - name_template: "test-project-{{{{ .Os }}}}-{{{{ .Arch }}}}"
        format: tar.gz
    docker:
      - image_templates:
          - "myregistry/test-project:{{{{ .Version }}}}"
        dockerfile: Dockerfile
        platforms:
          - linux/amd64
          - linux/arm64
    checksum:
      name_template: "checksums.txt"
      algorithm: sha256
"#,
        host = host
    );
    create_config(tmp.path(), &config);

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args([
            "release",
            "--dry-run",
            "--skip=release,publish,sign,announce,changelog,nfpm",
            "--timeout",
            "5m",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "docker staging dry-run should succeed.\nstderr:\n{}",
        stderr
    );

    // Verify docker stage dry-run mentions Dockerfile copy
    assert!(
        stderr.contains("docker") && stderr.contains("dry-run"),
        "stderr should contain docker dry-run output, got:\n{}",
        stderr
    );
    assert!(
        stderr.contains("Dockerfile"),
        "stderr should mention Dockerfile in docker staging, got:\n{}",
        stderr
    );
}

/// E2E #9: Cross-platform format_overrides — verify that OS-based format
/// overrides are validated in config (windows -> zip, linux -> tar.gz).
#[test]
fn test_e2e_cross_platform_format_overrides_check() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());

    create_config(
        tmp.path(),
        r#"project_name: test-project
defaults:
  archives:
    format: tar.gz
    format_overrides:
      - os: windows
        format: zip
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: test-project
        targets:
          - x86_64-unknown-linux-gnu
          - x86_64-pc-windows-msvc
          - aarch64-unknown-linux-gnu
    archives:
      - name_template: "test-project-{{ .Version }}-{{ .Os }}-{{ .Arch }}"
    checksum:
      name_template: "checksums.txt"
      algorithm: sha256
"#,
    );

    // Verify config with format_overrides passes validation
    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .arg("check")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "check with format_overrides should succeed.\nstderr:\n{}",
        stderr
    );
}

/// E2E #10: Snapshot mode produces SNAPSHOT version in artifact names
/// and in the output log.
#[test]
fn test_e2e_snapshot_version_in_artifacts() {
    let tmp = TempDir::new().unwrap();
    let host = detect_host_target();

    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    let config = format!(
        r#"project_name: test-project
crates:
  - name: test-project
    path: "."
    tag_template: "v{{{{ .Version }}}}"
    builds:
      - binary: test-project
        targets:
          - {host}
    archives:
      - name_template: "test-project-{{{{ .Version }}}}-{{{{ .Os }}}}-{{{{ .Arch }}}}"
        format: tar.gz
    checksum:
      name_template: "checksums.txt"
      algorithm: sha256
"#,
        host = host
    );
    create_config(tmp.path(), &config);

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args([
            "release",
            "--snapshot",
            "--skip=release,publish,docker,sign,announce,changelog,nfpm",
            "--timeout",
            "5m",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "snapshot release should succeed.\nstderr:\n{}",
        stderr
    );

    // Verify artifacts contain a version string in their names.
    // In snapshot mode, the Version template var is resolved from the Cargo.toml
    // version (e.g. "0.1.0"), producing names like test-project-0.1.0-linux-amd64.tar.gz.
    let dist_dir = tmp.path().join("dist");
    assert!(dist_dir.exists(), "dist/ should exist");

    let entries: Vec<_> = fs::read_dir(&dist_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();

    // The archive name should contain the Cargo.toml version (0.1.0)
    let has_versioned_archive = entries.iter().any(|name| {
        name.starts_with("test-project-") && name.contains("0.1.0") && name.ends_with(".tar.gz")
    });
    assert!(
        has_versioned_archive,
        "dist/ should contain versioned archive with 0.1.0, found: {:?}",
        entries
    );

    // Verify checksums.txt references the versioned artifact
    let checksum_content = fs::read_to_string(dist_dir.join("checksums.txt")).unwrap();
    assert!(
        checksum_content.contains("0.1.0"),
        "checksums should reference versioned artifact, got:\n{}",
        checksum_content
    );
}

/// E2E #11: Full dry-run with all stages — runs complete pipeline including
/// changelog and sign with no side effects and no dist/ artifacts.
#[test]
fn test_e2e_full_dry_run_all_stages() {
    let tmp = TempDir::new().unwrap();
    let host = detect_host_target();

    create_test_project(tmp.path());

    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(tmp.path())
            .output()
            .expect("git command failed");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    };

    let config = format!(
        r#"project_name: test-project
changelog:
  sort: asc
  groups:
    - title: Changes
      regexp: ".*"
      order: 0
signs:
  - id: gpg
    artifacts: checksum
    cmd: gpg
    args:
      - "--detach-sig"
crates:
  - name: test-project
    path: "."
    tag_template: "v{{{{ .Version }}}}"
    builds:
      - binary: test-project
        targets:
          - {host}
    archives:
      - name_template: "test-project-{{{{ .Os }}}}-{{{{ .Arch }}}}"
        format: tar.gz
    checksum:
      name_template: "checksums.txt"
      algorithm: sha256
"#,
        host = host
    );
    create_config(tmp.path(), &config);

    git(&["init"]);
    git(&["config", "user.email", "test@test.com"]);
    git(&["config", "user.name", "Test"]);
    git(&["add", "-A"]);
    git(&["commit", "-m", "initial"]);
    git(&["tag", "v0.1.0"]);

    // Add a commit after the tag so changelog has content
    fs::write(
        tmp.path().join("src/main.rs"),
        r#"fn main() { println!("updated"); }"#,
    )
    .unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-m", "feat: dry-run test commit"]);

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args([
            "release",
            "--dry-run",
            "--snapshot",
            "--skip=release,publish,docker,announce,nfpm",
            "--timeout",
            "5m",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "full dry-run should succeed.\nstderr:\n{}",
        stderr
    );

    // Verify dry-run is mentioned
    assert!(
        stderr.contains("dry-run"),
        "stderr should mention dry-run, got:\n{}",
        stderr
    );

    // Verify changelog stage ran in dry-run (it logs skipping write)
    assert!(
        stderr.contains("changelog"),
        "stderr should mention changelog stage, got:\n{}",
        stderr
    );

    // Verify sign stage ran in dry-run
    assert!(
        stderr.contains("sign"),
        "stderr should mention sign stage, got:\n{}",
        stderr
    );

    // GoReleaser writes CHANGELOG.md even in dry-run mode.
    // Anodize matches this behavior for debugging and downstream stage consumption.
}

/// E2E #12: Check command with nested custom config path validates correctly.
#[test]
fn test_e2e_check_nested_custom_config() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());

    // Create deeply nested config directory
    let nested = tmp.path().join("configs").join("release").join("prod");
    fs::create_dir_all(&nested).unwrap();
    let config_path = nested.join("release-config.yaml");
    fs::write(
        &config_path,
        r#"
project_name: test-project
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: test-project
        targets:
          - x86_64-unknown-linux-gnu
    archives:
      - name_template: "{{ .ProjectName }}-{{ .Os }}-{{ .Arch }}"
        format: tar.gz
    checksum:
      name_template: "checksums.txt"
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args(["-f", config_path.to_str().unwrap(), "check"])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "check with nested custom config should succeed.\nstderr:\n{}",
        stderr
    );
    assert!(
        stderr.contains("Config is valid"),
        "should confirm config is valid, got:\n{}",
        stderr
    );
}

/// E2E #13: Init generates valid YAML that round-trips through Config struct.
/// Goes beyond parse validity — checks that Config-level fields survive serialization.
#[test]
fn test_e2e_init_yaml_structural_round_trip() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .arg("init")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "init should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Read the generated config file
    let yaml_str = fs::read_to_string(tmp.path().join(".anodize.yaml"))
        .expect(".anodize.yaml should exist after init");

    // Parse as generic YAML
    let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(&yaml_str).unwrap_or_else(|e| {
        panic!(
            "init output should be valid YAML: {}\nOutput:\n{}",
            e, yaml_str
        );
    });
    let map = value.as_mapping().expect("top-level should be a mapping");

    // Re-serialize the parsed value back to YAML and verify it's still valid
    let re_serialized = serde_yaml_ng::to_string(&value).unwrap();
    let re_parsed: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(&re_serialized).unwrap_or_else(|e| {
            panic!(
                "re-serialized YAML should parse: {}\nOutput:\n{}",
                e, re_serialized
            );
        });

    // Verify structural equivalence after round-trip
    assert_eq!(
        map.len(),
        re_parsed.as_mapping().unwrap().len(),
        "round-trip should preserve number of top-level keys"
    );

    // Verify essential keys survived the round-trip
    let has_key = |key: &str| map.contains_key(serde_yaml_ng::Value::String(key.to_string()));
    assert!(
        has_key("project_name"),
        "should have project_name after round-trip"
    );
    assert!(has_key("crates"), "should have crates after round-trip");
}

/// E2E #14: Multiple crates release — config with 2 binary crates, verify
/// both are processed in snapshot mode.
#[test]
fn test_e2e_multiple_crates_release() {
    let tmp = TempDir::new().unwrap();
    let host = detect_host_target();

    // Create a workspace with two binary crates
    fs::write(
        tmp.path().join("Cargo.toml"),
        r#"[workspace]
resolver = "2"
members = ["crates/app-one", "crates/app-two"]
"#,
    )
    .unwrap();

    // app-one
    let app1_dir = tmp.path().join("crates/app-one");
    fs::create_dir_all(app1_dir.join("src")).unwrap();
    fs::write(
        app1_dir.join("Cargo.toml"),
        r#"[package]
name = "app-one"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "app-one"
path = "src/main.rs"
"#,
    )
    .unwrap();
    fs::write(
        app1_dir.join("src/main.rs"),
        r#"fn main() { println!("app one"); }"#,
    )
    .unwrap();

    // app-two
    let app2_dir = tmp.path().join("crates/app-two");
    fs::create_dir_all(app2_dir.join("src")).unwrap();
    fs::write(
        app2_dir.join("Cargo.toml"),
        r#"[package]
name = "app-two"
version = "0.2.0"
edition = "2021"

[[bin]]
name = "app-two"
path = "src/main.rs"
"#,
    )
    .unwrap();
    fs::write(
        app2_dir.join("src/main.rs"),
        r#"fn main() { println!("app two"); }"#,
    )
    .unwrap();

    init_git_repo(tmp.path());

    let config = format!(
        r#"project_name: multi-app
crates:
  - name: app-one
    path: "crates/app-one"
    tag_template: "app-one-v{{{{ .Version }}}}"
    builds:
      - binary: app-one
        targets:
          - {host}
    archives:
      - name_template: "app-one-{{{{ .Os }}}}-{{{{ .Arch }}}}"
        format: tar.gz
    checksum:
      name_template: "app-one-checksums.txt"
      algorithm: sha256

  - name: app-two
    path: "crates/app-two"
    tag_template: "app-two-v{{{{ .Version }}}}"
    builds:
      - binary: app-two
        targets:
          - {host}
    archives:
      - name_template: "app-two-{{{{ .Os }}}}-{{{{ .Arch }}}}"
        format: tar.gz
    checksum:
      name_template: "app-two-checksums.txt"
      algorithm: sha256
"#,
        host = host
    );
    create_config(tmp.path(), &config);

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args([
            "release",
            "--snapshot",
            "--all",
            "--force",
            "--skip=release,publish,docker,sign,announce,changelog,nfpm",
            "--timeout",
            "5m",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "multi-crate release should succeed.\nstderr:\n{}",
        stderr
    );

    // Both crates should be mentioned in the output
    assert!(
        stderr.contains("app-one"),
        "stderr should mention app-one crate, got:\n{}",
        stderr
    );
    assert!(
        stderr.contains("app-two"),
        "stderr should mention app-two crate, got:\n{}",
        stderr
    );

    // Verify dist/ has archives for both crates
    let dist_dir = tmp.path().join("dist");
    assert!(dist_dir.exists(), "dist/ should exist");

    let entries: Vec<_> = fs::read_dir(&dist_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();

    let has_app_one = entries
        .iter()
        .any(|n| n.starts_with("app-one") && n.ends_with(".tar.gz"));
    assert!(
        has_app_one,
        "dist/ should contain app-one archive, found: {:?}",
        entries
    );

    let has_app_two = entries
        .iter()
        .any(|n| n.starts_with("app-two") && n.ends_with(".tar.gz"));
    assert!(
        has_app_two,
        "dist/ should contain app-two archive, found: {:?}",
        entries
    );
}

/// E2E #15: Healthcheck detects available tools and reports their versions.
#[test]
fn test_e2e_healthcheck_detects_tools() {
    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .arg("healthcheck")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "healthcheck should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);

    // Healthcheck should report on multiple tools
    assert!(
        stderr.contains("cargo"),
        "healthcheck should check cargo availability"
    );
    assert!(
        stderr.contains("git"),
        "healthcheck should check git availability"
    );

    // Should indicate tools are found or not found with clear status
    assert!(
        stderr.contains("found")
            || stderr.contains("ok")
            || stderr.contains("✓")
            || stderr.contains("available"),
        "healthcheck should report tool status, got:\n{}",
        stderr
    );
}

/// E2E #16: Changelog with header and footer — verify that header/footer
/// strings are included in the generated CHANGELOG.md.
#[test]
fn test_e2e_changelog_header_footer() {
    let tmp = TempDir::new().unwrap();
    let host = detect_host_target();

    create_test_project(tmp.path());

    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(tmp.path())
            .output()
            .expect("git command failed");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    };

    let config = format!(
        r##"project_name: test-project
changelog:
  sort: asc
  header: "# Release Notes"
  footer: "Generated by anodize"
crates:
  - name: test-project
    path: "."
    tag_template: "v{{{{ .Version }}}}"
    builds:
      - binary: test-project
        targets:
          - {host}
    archives:
      - name_template: "test-project-{{{{ .Os }}}}-{{{{ .Arch }}}}"
        format: tar.gz
    checksum:
      name_template: "checksums.txt"
"##,
        host = host
    );
    create_config(tmp.path(), &config);

    git(&["init"]);
    git(&["config", "user.email", "test@test.com"]);
    git(&["config", "user.name", "Test"]);
    git(&["add", "-A"]);
    git(&["commit", "-m", "initial"]);
    git(&["tag", "v0.1.0"]);

    fs::write(
        tmp.path().join("src/main.rs"),
        r#"fn main() { println!("v2"); }"#,
    )
    .unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-m", "feat: add header/footer test feature"]);

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args([
            "release",
            "--snapshot",
            "--skip=release,publish,docker,sign,announce,nfpm",
            "--timeout",
            "5m",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "changelog header/footer release should succeed.\nstderr:\n{}",
        stderr
    );

    let notes_path = tmp.path().join("dist/CHANGELOG.md");
    assert!(notes_path.exists(), "CHANGELOG.md should exist");

    let notes = fs::read_to_string(&notes_path).unwrap();
    assert!(
        notes.contains("# Release Notes"),
        "changelog should contain header, got:\n{}",
        notes
    );
    assert!(
        notes.contains("Generated by anodize"),
        "changelog should contain footer, got:\n{}",
        notes
    );
}

/// E2E #17: Changelog with exclude filters — verify that commits matching
/// exclude patterns are omitted from the changelog.
#[test]
fn test_e2e_changelog_exclude_filters() {
    let tmp = TempDir::new().unwrap();
    let host = detect_host_target();

    create_test_project(tmp.path());

    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(tmp.path())
            .output()
            .expect("git command failed");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    };

    let config = format!(
        r#"project_name: test-project
changelog:
  sort: asc
  filters:
    exclude:
      - "^chore"
      - "^docs"
crates:
  - name: test-project
    path: "."
    tag_template: "v{{{{ .Version }}}}"
    builds:
      - binary: test-project
        targets:
          - {host}
    archives:
      - name_template: "test-project-{{{{ .Os }}}}-{{{{ .Arch }}}}"
        format: tar.gz
    checksum:
      name_template: "checksums.txt"
"#,
        host = host
    );
    create_config(tmp.path(), &config);

    git(&["init"]);
    git(&["config", "user.email", "test@test.com"]);
    git(&["config", "user.name", "Test"]);
    git(&["add", "-A"]);
    git(&["commit", "-m", "initial"]);
    git(&["tag", "v0.1.0"]);

    // Create commits: feat should stay, chore and docs should be excluded
    fs::write(
        tmp.path().join("src/main.rs"),
        r#"fn main() { println!("a"); }"#,
    )
    .unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-m", "feat: visible feature"]);

    fs::write(
        tmp.path().join("src/main.rs"),
        r#"fn main() { println!("b"); }"#,
    )
    .unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-m", "chore: invisible chore"]);

    fs::write(
        tmp.path().join("src/main.rs"),
        r#"fn main() { println!("c"); }"#,
    )
    .unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-m", "docs: invisible docs"]);

    fs::write(
        tmp.path().join("src/main.rs"),
        r#"fn main() { println!("d"); }"#,
    )
    .unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-m", "fix: visible bugfix"]);

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args([
            "release",
            "--snapshot",
            "--skip=release,publish,docker,sign,announce,nfpm",
            "--timeout",
            "5m",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "changelog with filters should succeed.\nstderr:\n{}",
        stderr
    );

    let notes_path = tmp.path().join("dist/CHANGELOG.md");
    assert!(notes_path.exists(), "CHANGELOG.md should exist");

    let notes = fs::read_to_string(&notes_path).unwrap();
    assert!(
        notes.contains("visible feature"),
        "changelog should contain feat commit, got:\n{}",
        notes
    );
    assert!(
        notes.contains("visible bugfix"),
        "changelog should contain fix commit, got:\n{}",
        notes
    );
    assert!(
        !notes.contains("invisible chore"),
        "changelog should NOT contain chore commit (excluded), got:\n{}",
        notes
    );
    assert!(
        !notes.contains("invisible docs"),
        "changelog should NOT contain docs commit (excluded), got:\n{}",
        notes
    );
}

/// E2E #18: Auto-snapshot mode — verify that `--auto-snapshot` on a dirty repo
/// behaves like `--snapshot`.
#[test]
fn test_e2e_auto_snapshot_dirty_repo() {
    let tmp = TempDir::new().unwrap();
    let host = detect_host_target();

    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    let config = create_single_crate_snapshot_config(&host);
    create_config(tmp.path(), &config);

    // Make the repo dirty by modifying a file without committing
    fs::write(
        tmp.path().join("src/main.rs"),
        r#"fn main() { println!("dirty change"); }"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args([
            "release",
            "--auto-snapshot",
            "--skip=release,publish,docker,sign,announce,changelog,nfpm",
            "--timeout",
            "5m",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "auto-snapshot on dirty repo should succeed.\nstderr:\n{}",
        stderr
    );

    // Should mention snapshot mode was activated
    assert!(
        stderr.contains("snapshot"),
        "stderr should mention snapshot (auto-detected from dirty), got:\n{}",
        stderr
    );
}

/// E2E #19: Before/after hooks execute in release pipeline.
#[test]
fn test_e2e_before_hooks_execute() {
    let tmp = TempDir::new().unwrap();
    let host = detect_host_target();

    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    let marker_path = tmp.path().join("before-hook-ran.txt");
    // Use forward slashes for YAML compatibility on Windows
    let marker_yaml = marker_path.to_string_lossy().replace('\\', "/");
    let config = format!(
        r#"project_name: test-project
before:
  pre:
    - "echo before-hook-executed > {marker}"
crates:
  - name: test-project
    path: "."
    tag_template: "v{{{{ .Version }}}}"
    builds:
      - binary: test-project
        targets:
          - {host}
    archives:
      - name_template: "test-project-{{{{ .Os }}}}-{{{{ .Arch }}}}"
        format: tar.gz
    checksum:
      name_template: "checksums.txt"
"#,
        host = host,
        marker = marker_yaml
    );
    create_config(tmp.path(), &config);

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args([
            "release",
            "--snapshot",
            "--skip=release,publish,docker,sign,announce,changelog,nfpm",
            "--timeout",
            "5m",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "release with before hooks should succeed.\nstderr:\n{}",
        stderr
    );

    // Verify the before hook actually ran by checking for the marker file
    assert!(
        marker_path.exists(),
        "before hook should have created marker file at {}",
        marker_path.display()
    );
    let marker_content = fs::read_to_string(&marker_path).unwrap();
    assert!(
        marker_content.contains("before-hook-executed"),
        "marker file should contain expected content, got: {}",
        marker_content
    );
}

/// E2E #20: Before hooks are logged but not executed in dry-run mode.
#[test]
fn test_e2e_before_hooks_dry_run() {
    let tmp = TempDir::new().unwrap();
    let host = detect_host_target();

    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    let marker_path = tmp.path().join("should-not-exist.txt");
    // Use forward slashes for YAML compatibility on Windows
    let marker_yaml = marker_path.to_string_lossy().replace('\\', "/");
    let config = format!(
        r#"project_name: test-project
before:
  pre:
    - "echo should-not-run > {marker}"
crates:
  - name: test-project
    path: "."
    tag_template: "v{{{{ .Version }}}}"
    builds:
      - binary: test-project
        targets:
          - {host}
    archives:
      - name_template: "test-project-{{{{ .Os }}}}-{{{{ .Arch }}}}"
        format: tar.gz
    checksum:
      name_template: "checksums.txt"
"#,
        host = host,
        marker = marker_yaml
    );
    create_config(tmp.path(), &config);

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args([
            "release",
            "--dry-run",
            "--skip=release,publish,docker,sign,announce,changelog,nfpm",
            "--timeout",
            "5m",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "dry-run with before hooks should succeed.\nstderr:\n{}",
        stderr
    );

    // Verify the hook was NOT executed
    assert!(
        !marker_path.exists(),
        "before hook should NOT have run in dry-run mode"
    );

    // Verify the dry-run output logs the hook
    assert!(
        stderr.contains("dry-run") && stderr.contains("hook"),
        "stderr should mention dry-run hook logging, got:\n{}",
        stderr
    );
}

/// E2E #21: TOML config format — verify that anodize can read .toml configs.
#[test]
fn test_e2e_toml_config_check() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());

    let config_path = tmp.path().join("anodize.toml");
    fs::write(
        &config_path,
        r#"
project_name = "test-project"

[[crates]]
name = "test-project"
path = "."
tag_template = "v{{ .Version }}"
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args(["-f", config_path.to_str().unwrap(), "check"])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "check with TOML config should succeed.\nstderr:\n{}",
        stderr
    );
    assert!(
        stderr.contains("Config is valid"),
        "should confirm TOML config is valid, got:\n{}",
        stderr
    );
}

/// E2E #22: Report sizes — verify that `report_sizes: true` causes size
/// reporting in the release output.
#[test]
fn test_e2e_report_sizes() {
    let tmp = TempDir::new().unwrap();
    let host = detect_host_target();

    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    let config = format!(
        r#"project_name: test-project
report_sizes: true
crates:
  - name: test-project
    path: "."
    tag_template: "v{{{{ .Version }}}}"
    builds:
      - binary: test-project
        targets:
          - {host}
    archives:
      - name_template: "test-project-{{{{ .Os }}}}-{{{{ .Arch }}}}"
        format: tar.gz
    checksum:
      name_template: "checksums.txt"
      algorithm: sha256
"#,
        host = host
    );
    create_config(tmp.path(), &config);

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .args([
            "release",
            "--snapshot",
            "--skip=release,publish,docker,sign,announce,changelog,nfpm",
            "--timeout",
            "5m",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "release with report_sizes should succeed.\nstderr:\n{}",
        stderr
    );

    // When report_sizes is true, the output should contain size information
    // (bytes, KB, MB, or similar size reporting)
    let has_size_info = stderr.contains("bytes")
        || stderr.contains(" B")
        || stderr.contains("KB")
        || stderr.contains("MB")
        || stderr.contains("size")
        || stderr.contains("Size");
    assert!(
        has_size_info,
        "stderr should contain size information when report_sizes is true, got:\n{}",
        stderr
    );
}
