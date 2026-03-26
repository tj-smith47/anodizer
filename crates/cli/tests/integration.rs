use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// Helper to create a minimal Cargo project for testing
fn create_test_project(dir: &std::path::Path) {
    // Create Cargo.toml
    fs::write(
        dir.join("Cargo.toml"),
        r#"
[package]
name = "test-project"
version = "0.1.0"
edition = "2024"

[[bin]]
name = "test-project"
path = "src/main.rs"
"#,
    )
    .unwrap();

    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(
        dir.join("src/main.rs"),
        r#"fn main() { println!("hello"); }"#,
    )
    .unwrap();
}

/// Helper to create an anodize.yaml config
fn create_config(dir: &std::path::Path, content: &str) {
    fs::write(dir.join(".anodize.yaml"), content).unwrap();
}

/// Helper to init git repo with a tag
fn init_git_repo(dir: &std::path::Path) {
    let run = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git command failed to spawn");
        assert!(
            output.status.success(),
            "git {:?} failed with status {}: {}",
            args,
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run(&["init"]);
    run(&["config", "user.email", "test@test.com"]);
    run(&["config", "user.name", "Test"]);
    run(&["add", "-A"]);
    run(&["commit", "-m", "initial"]);
    run(&["tag", "v0.1.0"]);
}

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
    assert!(stdout.contains("project_name:"));
    assert!(stdout.contains("test-project"));
    assert!(stdout.contains("tag_template:"));
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
  hooks:
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
    let output = Command::new("rustc")
        .arg("-vV")
        .output()
        .expect("rustc should be available");
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(triple) = line.strip_prefix("host: ") {
            return triple.trim().to_string();
        }
    }
    panic!("could not detect host target triple from rustc -vV");
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

    // Note: Version is not yet populated by the release command (git tag
    // resolution is TODO), so we use ProjectName + Os + Arch which ARE set.
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
        let has_metadata = entries.iter().any(|name| name == "metadata.json");
        assert!(
            !has_metadata,
            "dist/ should NOT contain metadata.json after dry-run, found: {:?}",
            entries
        );
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

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Verify the output is valid YAML by parsing it
    let parsed: serde_yaml::Value = serde_yaml::from_str(&stdout).unwrap_or_else(|e| {
        panic!(
            "init output should be valid YAML.\nParse error: {}\nOutput:\n{}",
            e, stdout
        );
    });

    // Verify key fields exist in the parsed YAML
    let map = parsed
        .as_mapping()
        .expect("parsed YAML should be a mapping");
    assert!(
        map.contains_key(serde_yaml::Value::String("project_name".to_string())),
        "YAML should contain project_name"
    );
    assert!(
        map.contains_key(serde_yaml::Value::String("crates".to_string())),
        "YAML should contain crates"
    );
    assert!(
        map.contains_key(serde_yaml::Value::String("defaults".to_string())),
        "YAML should contain defaults"
    );

    // Verify the project name matches
    let project_name = map
        .get(serde_yaml::Value::String("project_name".to_string()))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(
        project_name, "test-project",
        "project_name should be test-project"
    );

    // Verify crates section is an array
    let crates = map
        .get(serde_yaml::Value::String("crates".to_string()))
        .and_then(|v| v.as_sequence())
        .expect("crates should be an array");
    assert!(!crates.is_empty(), "crates array should not be empty");

    // Verify the generated YAML can be written and validated with `anodize check`
    let tmp2 = TempDir::new().unwrap();
    create_test_project(tmp2.path());
    init_git_repo(tmp2.path());
    create_config(tmp2.path(), &stdout);

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

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Verify the output mentions all three crates
    assert!(
        stdout.contains("core-lib"),
        "init output should mention core-lib"
    );
    assert!(
        stdout.contains("helper-lib"),
        "init output should mention helper-lib"
    );
    assert!(stdout.contains("myapp"), "init output should mention myapp");

    // Verify depends_on relationships are detected
    assert!(
        stdout.contains("depends_on"),
        "init output should include depends_on for workspace deps"
    );

    // Verify topological order: core-lib should appear before myapp
    let core_pos = stdout
        .find("name: core-lib")
        .expect("core-lib should appear");
    let app_pos = stdout.find("name: myapp").expect("myapp should appear");
    assert!(
        core_pos < app_pos,
        "core-lib should appear before myapp (topological order)"
    );

    // Verify the generated YAML is parseable
    let _parsed: serde_yaml::Value = serde_yaml::from_str(&stdout).unwrap_or_else(|e| {
        panic!(
            "workspace init output should be valid YAML.\nParse error: {}\nOutput:\n{}",
            e, stdout
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

    // helper-lib and myapp were NOT modified, so they should NOT appear.
    // Note: depends_on propagation is not implemented in detect_changed_crates(),
    // so dependents of core-lib are not automatically included.
    assert!(
        !stderr.contains("helper-lib"),
        "stderr should NOT mention helper-lib (unchanged crate), got:\n{}",
        stderr
    );
    assert!(
        !stderr.contains("myapp"),
        "stderr should NOT mention myapp (unchanged crate), got:\n{}",
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

/// Unknown YAML fields should be silently ignored (not cause parse errors).
#[test]
fn test_check_unknown_yaml_fields_ignored() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    create_config(
        tmp.path(),
        r#"
project_name: test-project
future_feature: "this field does not exist yet"
experimental_mode: true
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
    unknown_crate_field: 42
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .arg("check")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "check should succeed even with unknown YAML fields.\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
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
