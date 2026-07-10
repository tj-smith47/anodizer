use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::TempDir;

use anodizer_core::test_helpers::{create_config, create_test_project, init_git_repo};

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

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "config"])
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
    // No anodizer.yaml at all
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "config"])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no anodizer config file found"));
}

/// A config that loads but fails to deserialize (unknown field under
/// `deny_unknown_fields`) is a FATAL config error: `check config` must exit
/// non-zero so CI catches the broken config. Regression guard — a printed-
/// but-swallowed error would let a typo'd field pass CI silently.
#[test]
fn test_check_config_unknown_field_fails_nonzero() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    create_config(
        tmp.path(),
        r#"
project_name: test-project
totally_unknown_key: oops
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "config"])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "check config must exit non-zero on a deserialize failure; got success.\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown field") && stderr.contains("totally_unknown_key"),
        "expected a deserialize error naming the unknown field, got:\n{stderr}"
    );
}

/// An unknown field nested inside a `deny_unknown_fields` sub-struct (here
/// `signs[]`) is equally fatal — the failure must not be confined to the
/// top level.
#[test]
fn test_check_config_unknown_nested_field_fails_nonzero() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    create_config(
        tmp.path(),
        r#"
project_name: test-project
signs:
  - artifacts: sbom
    bogus_nested_key: x
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "config"])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "check config must exit non-zero on a nested deserialize failure.\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown field") && stderr.contains("bogus_nested_key"),
        "expected a deserialize error naming the nested unknown field, got:\n{stderr}"
    );
}

/// A valid `signs.artifacts: sbom` config (and the other previously-stale
/// filters) must pass `check config` with exit 0 AND emit no "unrecognized
/// artifact filter" warning — the runtime sign stage honors these values, so
/// check-time validation must too (Bug 1 regression, end-to-end).
#[test]
fn test_check_config_sbom_sign_filter_no_warning() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    create_config(
        tmp.path(),
        r#"
project_name: test-project
signs:
  - artifacts: sbom
  - artifacts: any
  - artifacts: installer
  - artifacts: diskimage
  - artifacts: snap
  - artifacts: macos_package
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "config"])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "check config should succeed for runtime-valid sign filters.\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("unrecognized signs artifacts filter"),
        "no spurious unrecognized-filter warning expected, got:\n{stderr}"
    );
}

#[test]
fn test_init_generates_config() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .arg("init")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "init should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // `init` routes status through StageLogger, which writes to stderr.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Created .anodizer.yaml"),
        "stderr: {stderr}"
    );

    // Read the generated config file
    let config_content =
        fs::read_to_string(tmp.path().join(".anodizer.yaml")).expect(".anodizer.yaml should exist");
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
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .arg("--version")
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("anodizer"));
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
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["-f", config_path.to_str().unwrap(), "check", "config"])
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

    let config_path = tmp.path().join("my-anodizer.yaml");
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
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["--config", config_path.to_str().unwrap(), "check", "config"])
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
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["-f", "/tmp/does-not-exist-anodizer.yaml", "check", "config"])
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
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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

    // Config with a before-hook that sleeps for 60 seconds (much longer than our timeout).
    // The git pipe (including the dirty-repo gate) runs BEFORE the before-hooks,
    // so the config file must be committed — otherwise the dirty-repo check aborts
    // with exit 1 before the hook gets a chance to hit the timeout.
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
    // Commit the freshly-written config so the repo is clean.
    let _ = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = std::process::Command::new("git");
            cmd.args(["add", "-A"]).current_dir(tmp.path());
            cmd
        },
        "git",
    );
    let _ = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = std::process::Command::new("git");
            cmd.args(["commit", "--amend", "--no-edit"])
                .current_dir(tmp.path());
            cmd
        },
        "git",
    );
    // Re-tag HEAD so tag_points_at_head still succeeds after amending.
    let _ = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = std::process::Command::new("git");
            cmd.args(["tag", "-f", "v0.1.0"]).current_dir(tmp.path());
            cmd
        },
        "git",
    );

    let start = Instant::now();

    // Use spawn + try_wait instead of output(). When std::process::exit(124)
    // fires from the watchdog thread, the grandchild `sleep 60` may still
    // hold inherited pipe fds open, causing output() to block until that
    // process also exits. By discarding stdout/stderr with Stdio::null()
    // and polling try_wait(), we detect the exit immediately.
    let mut child = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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
                    panic!("anodizer process did not exit within 10s (timeout was 1s)");
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
fn test_failing_before_hook_stderr_surfaces_once_when_verbose() {
    // A failing `before:` hook routes through the consolidated run helper
    // (anodizer_core::run::run_checked). At --verbose the helper tees the
    // hook's stderr live AND captures it; the streaming-aware check_output
    // variant must then SUPPRESS its own stderr re-emit so the captured
    // output is not printed a second time.
    //
    // The token is produced at runtime (`printf` building the string from
    // fragments) so it never appears in the literal hook command text — only
    // genuine *stderr surfacing* of the token is counted, isolating the
    // double-emit guard from the command-echo lines.
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    create_config(
        tmp.path(),
        r#"
project_name: test-project
before:
  hooks:
    - "printf '%s%s\n' HOOKFAIL TOKEN42 >&2; exit 7"
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
"#,
    );
    // Commit the freshly-written config so the dirty-repo gate (which runs
    // before the before-hooks) does not abort first.
    let _ = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.args(["add", "-A"]).current_dir(tmp.path());
            cmd
        },
        "git",
    );
    let _ = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.args(["commit", "--amend", "--no-edit"])
                .current_dir(tmp.path());
            cmd
        },
        "git",
    );
    let _ = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.args(["tag", "-f", "v0.1.0"]).current_dir(tmp.path());
            cmd
        },
        "git",
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["release", "--snapshot", "--verbose"])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "release with a failing before-hook must exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    // The assembled token "HOOKFAILTOKEN42" surfaces once from the live tee.
    // The bail! embed at the end of the run carries it a second time inside
    // the propagated error chain (defense-in-depth, not the log re-emit the
    // streaming guard suppresses), so the live-stream surfacing is bounded at
    // one and the total stays at the documented two.
    let hits = stderr.matches("HOOKFAILTOKEN42").count();
    assert!(
        hits <= 2,
        "failing before-hook stderr must not be triple-printed at --verbose \
         (live tee once + bail embed once); got {hits} in stderr:\n{stderr}"
    );
    assert!(
        hits >= 1,
        "failing before-hook stderr must surface at least once; \
         got {hits} in stderr:\n{stderr}"
    );
}

#[test]
fn test_man_renders_roff() {
    // Smoke test for `anodizer man`: the parser test in main.rs asserts the
    // subcommand dispatches; this asserts the rendering path actually emits
    // valid roff via clap_mangen, so a regression in clap/clap_mangen wiring
    // (not just parsing) is caught.
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .arg("man")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "man should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.is_empty(), "man output should not be empty");
    // clap_mangen emits a `.ie ` macro on the first line followed by `.TH`
    // for the program. Either anchor proves we got roff and not help text.
    assert!(
        stdout.starts_with(".ie ") || stdout.contains(".TH anodizer"),
        "man output should be roff (start with .ie or contain .TH anodizer), got first 200 bytes: {}",
        stdout.chars().take(200).collect::<String>()
    );
    assert!(
        stdout.contains("anodizer"),
        "man output should reference the program name 'anodizer'"
    );
    // The SUBCOMMANDS section should reference at least one known subcommand.
    // clap_mangen escapes the dash, so the name appears as `anodizer\-release`.
    assert!(
        stdout.contains("release"),
        "man output should reference the 'release' subcommand"
    );
}

#[test]
fn test_completion_bash_produces_output() {
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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
        stdout.contains("anodizer"),
        "bash completions should reference 'anodizer'"
    );
}

#[test]
fn test_completion_zsh_produces_output() {
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["completion", "zsh"])
        .output()
        .unwrap();

    assert!(output.status.success(), "completion zsh should succeed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.is_empty(), "zsh completions should not be empty");
}

#[test]
fn test_healthcheck_succeeds() {
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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
    anodizer_cli::detect_host_target().expect("failed to detect host target triple")
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
        formats: [tar.gz]
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
        formats: [tar.gz]
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

/// E2E: `anodizer release --snapshot` produces correct artifacts in dist/.
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

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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

    // Snapshot with `--skip=release` never derives a release URL; the key
    // must still be present (action-side `jq '.release_url // empty'`
    // contract) and empty, matching the sibling keys' absent-value shape.
    let metadata: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dist_dir.join("metadata.json")).unwrap()).unwrap();
    assert_eq!(
        metadata["release_url"], "",
        "snapshot metadata.json must carry an empty release_url when no \
         release was created"
    );
}

/// E2E: a real archive build records the bundled non-binary in-archive paths
/// (LICENSE / README) under `metadata.archive_files`, so the krew publisher can
/// emit a `files:` extraction list gated on the archive's actual contents. This
/// proves the stage-archive → artifacts.json wiring end-to-end, not just the
/// publisher's consumption of the metadata.
#[test]
fn test_e2e_archive_records_bundled_license_readme_in_metadata() {
    let tmp = TempDir::new().unwrap();
    let host = detect_host_target();

    create_test_project(tmp.path());
    init_git_repo(tmp.path());
    // The default extra-files glob bundles LICENSE / README; write both so the
    // archive carries them and the metadata names them.
    fs::write(tmp.path().join("LICENSE"), "MIT\n").unwrap();
    fs::write(tmp.path().join("README.md"), "# test-project\n").unwrap();

    let config = create_single_crate_snapshot_config(&host);
    create_config(tmp.path(), &config);

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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

    let artifacts: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(tmp.path().join("dist/artifacts.json")).unwrap())
            .unwrap();
    let archive = artifacts
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["kind"] == "archive")
        .expect("an archive artifact must exist");
    let files = archive["metadata"]["archive_files"]
        .as_str()
        .expect("archive metadata must carry archive_files");
    assert!(
        files.contains("LICENSE"),
        "archive_files must name the bundled LICENSE, got: {files}"
    );
    assert!(
        files.contains("README.md"),
        "archive_files must name the bundled README, got: {files}"
    );
}

/// E2E: `anodizer release --prepare` produces the same skip-stage behaviour
/// as an explicit `--skip=<UPSTREAM_STAGES>` — the shared upstream-touching
/// classification, not a hand list, so this test cannot drift from what
/// `--prepare` actually derives its skip set from.
///
/// Locks in the `--prepare` contract end-to-end. The unit
/// tests for `apply_prepare_mode_to_skip()` cover the helper's input/output;
/// this asserts the helper is actually wired into `release::run()` and that
/// the augmented skip list reaches the pipeline so `release`, `publish`, and
/// `announce` are reported as skipped in the run output.
#[test]
fn test_release_prepare_matches_explicit_skip() {
    let host = detect_host_target();

    // Extracts the set of stages the pipeline reported as skipped.
    //
    // Pipeline-level skips are consolidated into kv rows: consecutive
    // skipped stages buffer up and flush as `   • skipped  a, b, c`
    // (operator/mode `--skip`) or `   • skipped  a, b, c (no binaries)`
    // (binary-dependent stages with no binaries) — both emitted by
    // `Pipeline::run` via `kv`. The value is a comma-separated stage
    // list, split into names so the assertions below can match
    // `release` / `publish` / `announce` directly.
    //
    // Per-crate / per-config body notes (e.g. `no gitlab config ...,
    // skipping`) are progress lines inside a running stage, not a stage
    // skip; anchoring on the kv key pad (`skipped` + at least two spaces)
    // keeps them out — notes like `skipped (snapshot mode)` have a single
    // space and don't match.
    fn extract_skipped_stages(stderr: &str) -> std::collections::BTreeSet<String> {
        stderr
            .lines()
            .filter_map(|line| {
                let line = strip_ansi(line);
                let body = line.trim_start().strip_prefix("• ")?;
                let names = body.strip_prefix("skipped  ")?.trim_start();
                let names = names.strip_suffix(" (no binaries)").unwrap_or(names);
                Some(
                    names
                        .split(", ")
                        .map(|stage| stage.to_string())
                        .collect::<Vec<_>>(),
                )
            })
            .flatten()
            .collect()
    }

    fn run_release(tmp: &Path, extra_args: &[&str]) -> std::process::Output {
        let mut args: Vec<&str> = vec![
            "release",
            "--snapshot",
            "--dry-run",
            // Surface the consolidated pipeline skip row at default
            // verbosity so `extract_skipped_stages` can read it; both
            // invocations share these base args, so the comparison stays
            // symmetric.
            "--show-skipped",
            // Skip everything heavy so the test stays fast — these stages
            // are skipped by both invocations identically, so they cancel
            // out of the comparison and only the prepare-injected stages
            // (release/publish/announce) differentiate.
            "--skip=build,archive,checksum,docker,sign,nfpm,changelog,sbom",
            "--timeout",
            "2m",
        ];
        args.extend_from_slice(extra_args);

        Command::new(env!("CARGO_BIN_EXE_anodizer"))
            .args(&args)
            .current_dir(tmp)
            .output()
            .unwrap()
    }

    fn setup_fixture(tmp: &Path, host: &str) {
        create_test_project(tmp);
        init_git_repo(tmp);
        let config = create_single_crate_snapshot_config(host);
        create_config(tmp, &config);
    }

    // Run 1: --prepare (relies on apply_prepare_mode_to_skip injecting
    // every UPSTREAM_STAGES entry into the skip list).
    let tmp_prepare = TempDir::new().unwrap();
    setup_fixture(tmp_prepare.path(), &host);
    let out_prepare = run_release(tmp_prepare.path(), &["--prepare"]);
    assert!(
        out_prepare.status.success(),
        "release --prepare should succeed.\nstderr:\n{}",
        String::from_utf8_lossy(&out_prepare.stderr)
    );
    let stderr_prepare = String::from_utf8_lossy(&out_prepare.stderr).into_owned();
    let skipped_prepare = extract_skipped_stages(&stderr_prepare);

    // Run 2: explicit --skip=<UPSTREAM_STAGES> (additive) — the same
    // classification --prepare derives from.
    let explicit_skip = format!(
        "--skip={}",
        anodizer_core::stages::UPSTREAM_STAGES.join(",")
    );
    let tmp_explicit = TempDir::new().unwrap();
    setup_fixture(tmp_explicit.path(), &host);
    let out_explicit = run_release(tmp_explicit.path(), &[explicit_skip.as_str()]);
    assert!(
        out_explicit.status.success(),
        "release --skip=release,publish,announce should succeed.\nstderr:\n{}",
        String::from_utf8_lossy(&out_explicit.stderr)
    );
    let stderr_explicit = String::from_utf8_lossy(&out_explicit.stderr).into_owned();
    let skipped_explicit = extract_skipped_stages(&stderr_explicit);

    // Sanity: each invocation must report the three pro-prepare stages
    // as skipped — otherwise the assertion below could pass vacuously
    // (e.g. if the extractor matched nothing and both sets were empty).
    for stage in ["release", "publish", "announce"] {
        assert!(
            skipped_prepare.contains(stage),
            "--prepare run should report '{stage} skipped' in stderr, got skipped set {:?}\nfull stderr:\n{}",
            skipped_prepare,
            stderr_prepare
        );
        assert!(
            skipped_explicit.contains(stage),
            "explicit --skip run should report '{stage} skipped' in stderr, got skipped set {:?}\nfull stderr:\n{}",
            skipped_explicit,
            stderr_explicit
        );
    }

    // Contract: the two invocations must produce the same set of skipped
    // stages. If --prepare ever drifts from the shared upstream-touching
    // classification — by adding extra stages, missing one, or reordering
    // the helper — this assertion catches it.
    assert_eq!(
        skipped_prepare, skipped_explicit,
        "release --prepare must yield the same skip-stage set as \
         --skip=<UPSTREAM_STAGES>\n\
         --prepare skipped: {:?}\n--skip skipped: {:?}",
        skipped_prepare, skipped_explicit
    );
}

/// Strip ANSI escape sequences (CSI: ESC `[ ... <final-byte>`). Tiny
/// inline implementation so this test file doesn't add a dependency.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next(); // consume '['
            // CSI parameter/intermediate bytes are 0x20..=0x3F; final
            // byte is 0x40..=0x7E.
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// E2E: `anodizer release --dry-run` runs full pipeline with no side effects.
#[test]
fn test_e2e_dry_run_no_side_effects() {
    let tmp = TempDir::new().unwrap();
    let host = detect_host_target();

    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    let config = create_single_crate_snapshot_config(&host);
    create_config(tmp.path(), &config);

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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
        // metadata.json and artifacts.json are written even in dry-run mode.
        // Anodizer matches this behavior: metadata is always written for debugging.
    }
    // If dist/ doesn't exist at all, that's the expected case for dry-run.

    // Verify the stderr mentions dry-run activity
    assert!(
        stderr.contains("dry-run"),
        "stderr should mention dry-run, got:\n{}",
        stderr
    );
}

/// E2E: `anodizer check` validates a comprehensive config that exercises many fields.
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
  - BUILD_ENV=ci
  - DEPLOY_TARGET=staging

defaults:
  targets:
    - x86_64-unknown-linux-gnu
    - aarch64-unknown-linux-gnu
  cross: auto
  archives:
    formats: [tar.gz]
    format_overrides:
      - os: windows
        formats: [zip]
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
        formats: [tar.gz]
    checksum:
      name_template: "checksums.txt"
      algorithm: sha256
    dockers_v2:
      - images:
          - "myregistry/test-project"
        tags:
          - "{{ .Version }}"
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

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "config"])
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

/// E2E: `anodizer init` generates valid YAML that can be parsed back.
#[test]
fn test_e2e_init_generates_parseable_yaml() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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
    let config_content = fs::read_to_string(tmp.path().join(".anodizer.yaml"))
        .expect(".anodizer.yaml should exist after init");

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

    // Verify the generated YAML can be written and validated with `anodizer check`
    let tmp2 = TempDir::new().unwrap();
    create_test_project(tmp2.path());
    init_git_repo(tmp2.path());
    create_config(tmp2.path(), &config_content);

    let check_output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "config"])
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
/// 1. `anodizer check` passes on the workspace config
/// 2. `anodizer release --dry-run --all --force` includes all crates
/// 3. Dependency ordering is respected (core-lib before helper-lib before myapp)
#[test]
fn test_e2e_workspace_all_force_detects_crates() {
    let tmp = TempDir::new().unwrap();
    let host = detect_host_target();

    create_workspace_project(tmp.path());
    init_git_repo(tmp.path());

    // Create anodizer config for the workspace with depends_on
    let config = create_workspace_snapshot_config(&host);
    create_config(tmp.path(), &config);

    // 1. Verify config is valid
    let check_output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "config"])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        check_output.status.success(),
        "workspace config check should succeed.\nstderr:\n{}",
        String::from_utf8_lossy(&check_output.stderr)
    );

    // 2. Run dry-run release with --all --force
    let release_output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "release",
            "--dry-run",
            "--all",
            "--force",
            // Surface the per-crate skip detail at default verbosity so the
            // crate-name assertions below can read it.
            "--show-skipped",
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

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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

/// E2E: `anodizer init` on a workspace project generates config with depends_on.
#[test]
fn test_e2e_init_workspace_generates_depends_on() {
    let tmp = TempDir::new().unwrap();
    create_workspace_project(tmp.path());
    init_git_repo(tmp.path());

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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
    let config_content = fs::read_to_string(tmp.path().join(".anodizer.yaml"))
        .expect(".anodizer.yaml should exist after init");

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

/// E2E: `anodizer check` detects invalid depends_on references in workspace config.
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

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "config"])
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
/// 4. Runs `anodizer release --all --dry-run` (no --force)
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
        let output = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args).current_dir(tmp.path());
                cmd
            },
            "git",
        );
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

    // Create anodizer config before the initial commit so it's tracked
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
    let release_output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "release",
            "--dry-run",
            "--snapshot",
            "--all",
            "--single-target",
            // Surface the per-crate skip detail at default verbosity so the
            // crate-name assertions below can read it.
            "--show-skipped",
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

/// Regression: `release --all` change detection must resolve its pathspecs
/// against the discovered workspace root, NOT the process CWD, so it selects
/// the same crates whether invoked from the root or a subdirectory.
///
/// The discriminating case is the workspace-level check
/// (`check_workspace_files_changed`): editing a *per-crate* manifest
/// (`crates/myapp/Cargo.toml`) leaves the root `Cargo.toml`/`Cargo.lock`
/// untouched, so only `myapp` should be selected. Run from the `crates/myapp`
/// subdirectory, the old CWD-relative pathspec resolved `Cargo.toml` to the
/// subdir's own (changed) manifest and false-promoted the *entire* workspace.
///
/// This asserts on the build stage's per-crate lines, which enumerate exactly
/// the selected set — and crucially that the unrelated `solo-lib`/`core-lib`/
/// `helper-lib` are NOT selected (the `--all` "empty set means all crates"
/// collapse would otherwise mask under-detection).
#[test]
fn test_e2e_release_change_detection_from_subdir() {
    let tmp = TempDir::new().unwrap();

    create_workspace_project(tmp.path());

    // Add a fourth, fully independent crate so a strict-subset selection is
    // observable (the existing fixture's crates all relate to core-lib).
    let solo_dir = tmp.path().join("crates/solo-lib");
    fs::create_dir_all(solo_dir.join("src")).unwrap();
    fs::write(
        solo_dir.join("Cargo.toml"),
        "[package]\nname = \"solo-lib\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(solo_dir.join("src/lib.rs"), "pub fn solo() {}").unwrap();
    fs::write(
        tmp.path().join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"crates/core-lib\", \"crates/helper-lib\", \"crates/myapp\", \"crates/solo-lib\"]\n",
    )
    .unwrap();

    let git = |args: &[&str]| {
        let output = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args).current_dir(tmp.path());
                cmd
            },
            "git",
        );
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

  - name: solo-lib
    path: "crates/solo-lib"
    tag_template: "solo-lib-v{{ .Version }}"
"#,
    );

    git(&["add", "-A"]);
    git(&["commit", "-m", "initial workspace"]);

    git(&["tag", "core-lib-v0.1.0"]);
    git(&["tag", "helper-lib-v0.1.0"]);
    git(&["tag", "myapp-v0.1.0"]);
    git(&["tag", "solo-lib-v0.1.0"]);

    // Edit ONLY a per-crate manifest. The root manifests are untouched, so the
    // workspace-level check must NOT fire; only `myapp` (which nothing depends
    // on) should be selected.
    let myapp_manifest = tmp.path().join("crates/myapp/Cargo.toml");
    let manifest = fs::read_to_string(&myapp_manifest).unwrap();
    fs::write(
        &myapp_manifest,
        format!("{manifest}# touched per-crate manifest\n"),
    )
    .unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-m", "bump myapp manifest only"]);

    // Invoke from the crate subdirectory with an explicit (absolute) --config
    // pointing at the root config, so workspace-root discovery is driven by the
    // config override rather than the CWD. Build is intentionally NOT skipped so
    // the per-crate `[build] ... crate '<name>'` lines enumerate the selection.
    let subdir = tmp.path().join("crates/myapp");
    let config_path = tmp.path().join(".anodizer.yaml");
    let release_output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "release",
            "--dry-run",
            "--snapshot",
            "--all",
            "--single-target",
            // Surface the per-crate build-skip detail (these libs have no
            // binary target) at default verbosity so the `crate '<name>'`
            // selection assertions below can read it.
            "--show-skipped",
            "--skip=release,publish,docker,sign,announce,changelog,nfpm",
            "--timeout",
            "5m",
            "--config",
        ])
        .arg(&config_path)
        .current_dir(&subdir)
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&release_output.stderr);
    assert!(
        release_output.status.success(),
        "subdir change detection should succeed.\nstderr:\n{}",
        stderr
    );

    // Only `myapp` changed; it must be selected.
    assert!(
        stderr.contains("crate 'myapp'"),
        "myapp (changed manifest) must be selected, got:\n{stderr}",
    );
    // The unchanged, independent crates must be excluded. Under the CWD bug the
    // subdir's own Cargo.toml false-promoted the whole workspace, pulling these
    // in.
    for unchanged in ["crate 'solo-lib'", "crate 'core-lib'", "crate 'helper-lib'"] {
        assert!(
            !stderr.contains(unchanged),
            "{unchanged} must NOT be selected (subdir pathspec must resolve at the \
             workspace root), got:\n{stderr}",
        );
    }
}

// ============================================================================
// Error Path Tests
// ============================================================================

/// Error path: `anodizer check` with malformed YAML should fail with a clear error.
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

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "config"])
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

/// Error path: `anodizer check` with type mismatch should fail with a clear error.
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

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "config"])
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
        formats: [tar.gz]
    checksum:
      name_template: "checksums.txt"
"#,
        host = host
    );
    create_config(tmp.path(), &config);

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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
// Publisher skip-name acceptance / rejection tests
// ============================================================================

/// `--skip=brew` is REJECTED — the short alias is gone; the canonical
/// publisher skip token is the GoReleaser-aligned `homebrew`.
#[test]
fn test_skip_brew_rejected() {
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
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "release",
            "--skip=brew",
            "--dry-run",
            "--snapshot",
            "--single-target",
            "--clean",
            "--timeout",
            "30s",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "--skip=brew should be rejected (use --skip=homebrew); command unexpectedly succeeded"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    // The validator emits a single, well-known phrase on rejection (see
    // `registry::validate_publisher_selection`). Substring-matching on
    // "invalid" / "unknown" is too loose because the dry-run pipeline
    // legitimately prints target triples like `x86_64-unknown-linux-gnu` to
    // stderr and would false-positive on this Linux host.
    assert!(
        stderr.contains("invalid --skip value"),
        "--skip=brew rejection should mention invalidity, got:\n{}",
        stderr
    );
}

/// `--skip=choco` is REJECTED — the short alias is gone; the canonical
/// publisher skip token is the GoReleaser-aligned `chocolatey`.
#[test]
fn test_skip_choco_rejected() {
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
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "release",
            "--skip=choco",
            "--dry-run",
            "--snapshot",
            "--single-target",
            "--clean",
            "--timeout",
            "30s",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "--skip=choco should be rejected (use --skip=chocolatey); command unexpectedly succeeded"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    // See note on `test_skip_brew_rejected` for why this checks the exact
    // validator phrase rather than substring-matching "invalid"/"unknown".
    assert!(
        stderr.contains("invalid --skip value"),
        "--skip=choco rejection should mention invalidity, got:\n{}",
        stderr
    );
}

/// `--skip=cargo` is accepted (crates.io publisher skip).
#[test]
fn test_skip_cargo_accepted() {
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
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "release",
            "--skip=cargo",
            "--dry-run",
            "--snapshot",
            "--single-target",
            "--clean",
            "--timeout",
            "30s",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    // See note on `test_skip_brew_rejected` for why this checks the exact
    // validator phrase rather than substring-matching "invalid"/"unknown".
    assert!(
        !stderr.contains("invalid --skip value"),
        "--skip=cargo should be accepted, got:\n{}",
        stderr
    );
    // Non-vacuous: prove the run progressed PAST the validation gate. The
    // pipeline prints "Preparing release" only after selection validation
    // passes, so a pre-gate abort would fail this assertion.
    assert!(
        stderr.contains("Preparing release"),
        "expected post-gate marker, got:\n{}",
        stderr
    );
}

/// `--skip=krew` is accepted (krew publisher skip).
#[test]
fn test_skip_krew_accepted() {
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
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "release",
            "--skip=krew",
            "--dry-run",
            "--snapshot",
            "--single-target",
            "--clean",
            "--timeout",
            "30s",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    // See note on `test_skip_brew_rejected` for why this checks the exact
    // validator phrase rather than substring-matching "invalid"/"unknown".
    assert!(
        !stderr.contains("invalid --skip value"),
        "--skip=krew should be accepted, got:\n{}",
        stderr
    );
    assert!(
        stderr.contains("Preparing release"),
        "expected post-gate marker, got:\n{}",
        stderr
    );
}

/// `--skip=homebrew` is ACCEPTED — `homebrew` is the canonical, GoReleaser-
/// aligned publisher skip token (the short `brew` alias was removed).
#[test]
fn test_skip_homebrew_accepted() {
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
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "release",
            "--skip=homebrew",
            "--dry-run",
            "--snapshot",
            "--single-target",
            "--clean",
            "--timeout",
            "30s",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    // Validation passing is the assertion: the well-known rejection phrase
    // must be absent. The command may still fail LATER (dry-run snapshot has
    // nothing to publish) — that is unrelated to skip-token validation. The
    // looser substring search on "invalid"/"unknown" false-positives because
    // dry-run output contains target triples like `x86_64-unknown-linux-gnu`.
    assert!(
        !stderr.contains("invalid --skip value"),
        "--skip=homebrew should be accepted, got:\n{}",
        stderr
    );
    assert!(
        stderr.contains("Preparing release"),
        "expected post-gate marker, got:\n{}",
        stderr
    );
}

/// `--skip=chocolatey` is ACCEPTED — `chocolatey` is the canonical,
/// GoReleaser-aligned publisher skip token (the short `choco` alias was
/// removed).
#[test]
fn test_skip_chocolatey_accepted() {
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
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "release",
            "--skip=chocolatey",
            "--dry-run",
            "--snapshot",
            "--single-target",
            "--clean",
            "--timeout",
            "30s",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    // Validation passing is the assertion (see note on
    // `test_skip_homebrew_accepted`).
    assert!(
        !stderr.contains("invalid --skip value"),
        "--skip=chocolatey should be accepted, got:\n{}",
        stderr
    );
    assert!(
        stderr.contains("Preparing release"),
        "expected post-gate marker, got:\n{}",
        stderr
    );
}

/// `--skip=crates` is REJECTED — the canonical skip stage is `cargo`.
#[test]
fn test_skip_crates_alias_rejected() {
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
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "release",
            "--skip=crates",
            "--dry-run",
            "--snapshot",
            "--single-target",
            "--clean",
            "--timeout",
            "30s",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "--skip=crates should be rejected (use --skip=cargo); command unexpectedly succeeded"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Match the validator's exact phrase (see note on
    // `test_skip_brew_rejected`).
    assert!(
        stderr.contains("invalid --skip value"),
        "--skip=crates rejection should mention invalidity, got:\n{}",
        stderr
    );
}

/// `release --publishers cargo,homebrew` parses a 2-element allowlist of
/// known publisher names and passes validation (the command may fail later
/// for unrelated reasons; only skip/publisher validation is asserted here).
#[test]
fn test_publishers_allowlist_accepted() {
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
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "release",
            "--publishers=cargo,homebrew",
            "--dry-run",
            "--snapshot",
            "--single-target",
            "--clean",
            "--timeout",
            "30s",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("invalid --publishers value"),
        "--publishers=cargo,homebrew should be accepted, got:\n{}",
        stderr
    );
    assert!(
        stderr.contains("Preparing release"),
        "expected post-gate marker, got:\n{}",
        stderr
    );
}

/// `release --publishers bogusname` is REJECTED with a loud error naming the
/// valid publishers and a nonzero exit.
#[test]
fn test_publishers_typo_rejected() {
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
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "release",
            "--publishers=bogusname",
            "--dry-run",
            "--snapshot",
            "--single-target",
            "--clean",
            "--timeout",
            "30s",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "--publishers=bogusname should be rejected; command unexpectedly succeeded"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid --publishers value") && stderr.contains("Valid publishers:"),
        "--publishers typo should name the valid publishers, got:\n{}",
        stderr
    );
}

/// `release --skip bogusname` is REJECTED — `bogusname` is neither a stage
/// token nor a publisher name. Loud error, nonzero exit.
#[test]
fn test_skip_publisher_typo_rejected() {
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
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "release",
            "--skip=bogusname",
            "--dry-run",
            "--snapshot",
            "--single-target",
            "--clean",
            "--timeout",
            "30s",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "--skip=bogusname should be rejected; command unexpectedly succeeded"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid --skip value"),
        "--skip typo should be flagged invalid, got:\n{}",
        stderr
    );
}

/// `release --skip homebrew` flows to validation cleanly under the unified
/// denylist now that `homebrew` is the GoReleaser-canonical skip token.
/// (Companion to `test_skip_homebrew_accepted` — asserts the same token via
/// the GR-parity framing.)
#[test]
fn test_skip_homebrew_canonical_accepted() {
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
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "release",
            "--skip=homebrew",
            "--dry-run",
            "--snapshot",
            "--single-target",
            "--clean",
            "--timeout",
            "30s",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("invalid --skip value"),
        "--skip=homebrew (GR-canonical) should validate, got:\n{}",
        stderr
    );
    assert!(
        stderr.contains("Preparing release"),
        "expected post-gate marker, got:\n{}",
        stderr
    );
}

/// `publish --publishers npm` accepts the selector against the known set (the
/// publish gate is a vocabulary check), while `check config --publishers` is
/// tightened to the CONFIGURED set: a configured publisher passes, a known-but-
/// unconfigured publisher is rejected with a "not configured" error, and a typo
/// keeps the loud invalid-value rejection.
#[test]
fn test_publish_and_check_accept_publishers_flag() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    init_git_repo(tmp.path());
    // `publish.cargo` configures the cargo publisher so `check config
    // --publishers=cargo` resolves against a real publish block; `npm` is
    // deliberately left unconfigured to exercise the not-configured path.
    create_config(
        tmp.path(),
        r#"
project_name: test-project
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      cargo: {}
"#,
    );

    // `publish` validates the selector against the KNOWN set (vocabulary
    // check), then reaches dist loading and fails for lack of a manifest.
    // Assert both: the selector is not flagged invalid AND the run reached
    // the post-gate dist stage (non-vacuous — a pre-gate abort would not
    // print the manifest error).
    let publish_out = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["publish", "--publishers=npm", "--dry-run"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let publish_err = String::from_utf8_lossy(&publish_out.stderr);
    assert!(
        !publish_err.contains("invalid --publishers value"),
        "publish --publishers=npm should be accepted, got:\n{}",
        publish_err
    );
    assert!(
        publish_err.contains("no artifacts manifest found"),
        "expected post-gate dist marker, got:\n{}",
        publish_err
    );

    // `check config --publishers=cargo` names a CONFIGURED publisher → passes
    // and reaches config validation (non-vacuous: "Config is valid." prints
    // only after the publisher check passes).
    let check_ok = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "config", "--publishers=cargo"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let check_ok_err = String::from_utf8_lossy(&check_ok.stderr);
    assert!(
        check_ok.status.success(),
        "check config --publishers=cargo should pass, got:\n{}",
        check_ok_err
    );
    assert!(
        check_ok_err.contains("Config is valid"),
        "expected post-validation marker, got:\n{}",
        check_ok_err
    );

    // `check config --publishers=npm` names a KNOWN but UNCONFIGURED publisher
    // → rejected with the not-configured error (not the typo phrase).
    let check_unconfigured = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "config", "--publishers=npm"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(
        !check_unconfigured.status.success(),
        "check config --publishers=npm (unconfigured) should exit nonzero"
    );
    let check_unconfigured_err = String::from_utf8_lossy(&check_unconfigured.stderr);
    assert!(
        check_unconfigured_err.contains("not configured") && check_unconfigured_err.contains("npm"),
        "unconfigured publisher should yield a not-configured error, got:\n{}",
        check_unconfigured_err
    );

    // A check-config typo keeps the loud invalid-value rejection + nonzero exit.
    let check_typo = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "config", "--publishers=bogusname"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(
        !check_typo.status.success(),
        "check config --publishers=bogusname should exit nonzero"
    );
    let check_typo_err = String::from_utf8_lossy(&check_typo.stderr);
    assert!(
        check_typo_err.contains("invalid --publishers value"),
        "check config publisher typo should be flagged, got:\n{}",
        check_typo_err
    );
}

/// `continue` runs the same publish pipeline as `publish` and dispatches the
/// same irreversible publishers, so its `--publishers` / `--skip` selectors
/// MUST be validated before dispatch. A typo must error loudly with a nonzero
/// exit, BEFORE the command does any work — the one-way-door guard.
#[test]
fn test_continue_publishers_typo_rejected_before_dispatch() {
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
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["continue", "--publishers=bogusname", "--dry-run"])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "continue --publishers=bogusname must be rejected; got success"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid --publishers value") && stderr.contains("Valid publishers:"),
        "continue publisher typo must name valid publishers, got:\n{}",
        stderr
    );
}

/// `continue --skip=nmp` (a typo for `npm`) must be rejected before dispatch —
/// previously `continue` wired `--skip` straight into `skip_stages` with no
/// validation, so a typo silently failed to deselect a publisher.
#[test]
fn test_continue_skip_typo_rejected_before_dispatch() {
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
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["continue", "--skip=nmp", "--dry-run"])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "continue --skip=nmp (typo) must be rejected; got success"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid --skip value"),
        "continue skip typo must be flagged invalid, got:\n{}",
        stderr
    );
}

/// `continue --skip=npm` (valid publisher) passes validation. The command may
/// later fail loading a populated dist, but it must get PAST the selector
/// gate (no "invalid" rejection).
#[test]
fn test_continue_skip_publisher_accepted() {
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
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["continue", "--skip=npm", "--dry-run"])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("invalid --skip value") && !stderr.contains("invalid --publishers value"),
        "continue --skip=npm must pass the selector gate, got:\n{}",
        stderr
    );
}

/// The out-of-dispatch publish stages — `blob`, `snapcraft-publish`, `docker`,
/// `docker-sign` — must be ACCEPTED as `--publishers` allowlist entries (they
/// perform irreversible publishes and are now governed by the allowlist). A
/// release naming only them must clear the selector gate.
#[test]
fn test_publishers_allowlist_accepts_publish_stages() {
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
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "release",
            "--publishers=blob,snapcraft-publish,docker,docker-sign,announce",
            "--dry-run",
            "--snapshot",
            "--single-target",
            "--clean",
            "--timeout",
            "30s",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("invalid --publishers value"),
        "--publishers=blob,snapcraft-publish,docker,docker-sign,announce must be accepted, got:\n{}",
        stderr
    );
    assert!(
        stderr.contains("Preparing release"),
        "expected post-gate marker, got:\n{}",
        stderr
    );
}

/// `--skip=blob` / `--skip=snapcraft-publish` must STILL validate after the
/// publish stages became publisher tokens — the denylist must not regress.
#[test]
fn test_skip_publish_stage_still_accepted() {
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
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "release",
            "--skip=blob,snapcraft-publish",
            "--dry-run",
            "--snapshot",
            "--single-target",
            "--clean",
            "--timeout",
            "30s",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("invalid --skip value"),
        "--skip=blob,snapcraft-publish must still be accepted, got:\n{}",
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

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "config"])
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

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "config"])
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

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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

/// unknown YAML fields should be rejected (strict parsing).
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

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "config"])
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
      skip: true
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "config"])
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

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "config"])
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
    skip: true
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "config"])
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
  skip: true
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "config"])
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
// E2E Pipeline Tests
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
      # Q-arch2: each entry needs a unique id (matches GoReleaser's
      # ids.New("archives").Validate() requirement). Default-id collision
      # would otherwise be caught at config load.
      - id: targz
        name_template: "test-project-{{{{ .Os }}}}-{{{{ .Arch }}}}-targz"
        formats: [tar.gz]
      - id: tarxz
        name_template: "test-project-{{{{ .Os }}}}-{{{{ .Arch }}}}-tarxz"
        formats: [tar.xz]
      - id: zipped
        name_template: "test-project-{{{{ .Os }}}}-{{{{ .Arch }}}}-zipped"
        formats: [zip]
      - id: raw
        name_template: "test-project-{{{{ .Os }}}}-{{{{ .Arch }}}}-raw"
        formats: [binary]
    checksum:
      name_template: "checksums.txt"
      algorithm: sha256
"#,
        host = host
    );
    create_config(tmp.path(), &config);

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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

    // Linux/macOS: bare binary (no extension). Windows: `.exe` is appended
    // by the binary-format archiver.
    let has_binary = if cfg!(windows) {
        entries
            .iter()
            .any(|n| n.contains("raw") && n.ends_with(".exe"))
    } else {
        entries
            .iter()
            .any(|n| n.contains("raw") && !n.contains('.'))
    };
    assert!(
        has_binary,
        "dist/ should contain a raw binary (no extension on unix; .exe on windows), found: {:?}",
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
        formats: [tar.gz]
    checksum:
      name_template: "checksums.txt"
      algorithm: sha256
"#,
        host = host
    );
    create_config(tmp.path(), &config);

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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
        let output = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args).current_dir(tmp.path());
                cmd
            },
            "git",
        );
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
  snapshot: true
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
        formats: [tar.gz]
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
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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

/// A positional `<from>..<to>` range must use `<from>` as the range START,
/// overriding the auto-discovered latest matching tag (which the bare command
/// uses). Regression guard: the stage previously recomputed the previous tag
/// unconditionally and ignored the supplied range start.
#[test]
fn changelog_range_start_overrides_auto_discovered_previous_tag() {
    let tmp = TempDir::new().unwrap();

    create_test_project(tmp.path());

    let git = |args: &[&str]| {
        let output = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args).current_dir(tmp.path());
                cmd
            },
            "git",
        );
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    };

    let config = r#"project_name: test-project
changelog:
  snapshot: true
  sort: asc
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    create_config(tmp.path(), config);

    git(&["init"]);
    git(&["config", "user.email", "test@test.com"]);
    git(&["config", "user.name", "Test"]);
    git(&["config", "commit.gpgsign", "false"]);
    git(&["add", "-A"]);
    git(&["commit", "-m", "initial"]);
    git(&["tag", "v0.1.0"]);

    // Commit that lives BETWEEN v0.1.0 and v0.2.0. It is in range only when
    // the range starts at v0.1.0 — auto-discovery (latest tag = v0.2.0) drops it.
    fs::write(tmp.path().join("src/main.rs"), "fn main() { /* a */ }").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-m", "feat: between-tags feature"]);
    git(&["tag", "v0.2.0"]);

    // Commit AFTER v0.2.0 — always in range regardless of the range start.
    fs::write(tmp.path().join("src/main.rs"), "fn main() { /* b */ }").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-m", "fix: after-latest-tag fix"]);

    // The auto-discovered range start (the latest tag, v0.2.0) is asserted via
    // `--format json`: json's pending lower bound is `find_last_tag` (v0.2.0)
    // with HEAD as the upper bound, so v0.2.0..HEAD excludes the between-tags
    // commit. (Release-notes under `--snapshot` cannot demonstrate the same
    // baseline: snapshot resolves the current `Tag` to the latest tag itself,
    // and `resolve_prev_tag` drops a previous-tag equal to the current tag, so
    // the auto baseline degrades to full history there — a property of snapshot
    // tag resolution, independent of the range model.)
    let omitted_json = run_changelog_json(tmp.path(), None);
    assert!(
        !omitted_json.contains("between-tags feature"),
        "omitted range (pending) must auto-discover the start at v0.2.0 and exclude the between-tags commit, got:\n{omitted_json}"
    );
    assert!(
        omitted_json.contains("after-latest-tag fix"),
        "omitted range (pending) must include the post-v0.2.0 commit, got:\n{omitted_json}"
    );

    // The START override is exercised through `--format release-notes` (the
    // grouped-bullet body is directly assertable). `v0.1.0..HEAD` starts at
    // v0.1.0, so BOTH commits appear — overriding the auto-discovered v0.2.0
    // start that the pending baseline above used.
    let with_range = run_changelog_release_notes(tmp.path(), "v0.1.0..HEAD");
    assert!(
        with_range.contains("between-tags feature"),
        "v0.1.0..HEAD must include the between-tags commit (range start override), got:\n{with_range}"
    );
    assert!(
        with_range.contains("after-latest-tag fix"),
        "v0.1.0..HEAD must still include the post-v0.2.0 commit, got:\n{with_range}"
    );
}

/// Run `anodizer changelog [<range>] --format json` and return stdout.
fn run_changelog_json(dir: &std::path::Path, range: Option<&str>) -> String {
    let mut args = vec!["changelog"];
    if let Some(r) = range {
        args.push(r);
    }
    args.extend(["--format", "json"]);
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(&args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "changelog {:?} should succeed.\nstderr:\n{}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// Run `anodizer changelog <range> --snapshot --format release-notes` and
/// return stdout.
fn run_changelog_release_notes(dir: &std::path::Path, range: &str) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "changelog",
            range,
            "--snapshot",
            "--format",
            "release-notes",
        ])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "changelog {range:?} should succeed.\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// A positional range whose start names a nonexistent ref must ERROR (naming
/// the bad ref), not silently degrade to an empty changelog. `git log` returns
/// a non-zero exit on a bad range, so without the `is_bad_revision` guard a
/// typo'd ref would ship a blank changelog.
///
/// The default keep-a-changelog format drives `refresh_*_unreleased` →
/// `fetch_git_commits_in_paths`, whose `is_bad_revision` check bails naming the full
/// `<from>..<to>` range. (The release-notes path validates only the range START
/// up-front via `rev_verify_commit_in`; keep-a-changelog is used here because it
/// surfaces the engine-level bad-ref error for either endpoint.)
#[test]
fn changelog_nonexistent_range_ref_errors() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());

    let git = |args: &[&str]| {
        let output = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args).current_dir(tmp.path());
                cmd
            },
            "git",
        );
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    };

    let config = r#"project_name: test-project
changelog:
  snapshot: true
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    create_config(tmp.path(), config);

    git(&["init"]);
    git(&["config", "user.email", "test@test.com"]);
    git(&["config", "user.name", "Test"]);
    git(&["config", "commit.gpgsign", "false"]);
    git(&["add", "-A"]);
    git(&["commit", "-m", "initial"]);

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["changelog", "v9.9.9-does-not-exist..HEAD"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "changelog with a nonexistent range start must fail, not emit an empty changelog.\nstdout:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("v9.9.9-does-not-exist"),
        "error must name the bad ref, got:\n{stderr}"
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
    let init_output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .arg("init")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        init_output.status.success(),
        "init should succeed: {}",
        String::from_utf8_lossy(&init_output.stderr)
    );

    // init now writes to .anodizer.yaml instead of stdout
    let generated_config = std::fs::read_to_string(tmp.path().join(".anodizer.yaml"))
        .expect("init should create .anodizer.yaml");

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
    let check_output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "config"])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        check_output.status.success(),
        "check should succeed on init-generated config.\nstderr:\n{}",
        String::from_utf8_lossy(&check_output.stderr)
    );

    // Step 4: Run `release --snapshot` with the modified config.
    let build_output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "release",
            "--dry-run",
            "--all",
            "--force",
            // Surface the per-crate skip detail at default verbosity so the
            // crate-ordering assertions below can read it.
            "--show-skipped",
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

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "release",
            "--snapshot",
            // Surface the consolidated pipeline skip row at default verbosity
            // so the archive/checksum skip assertions below can read it.
            "--show-skipped",
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

    // Build stage should have run (not skipped). Anchor on the section
    // header (the `running cargo …` command echo is verbose-only now).
    assert!(
        stderr.contains("Building binaries"),
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
        formats: [tar.gz]
    checksum:
      name_template: "checksums.txt"
      algorithm: sha256
"#,
        host = host
    );
    create_config(tmp.path(), &config);

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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
        formats: [tar.gz]
    # Uses the `docker_v2:` back-compat alias (canonical is `dockers_v2:`) to
    # prove the serde alias resolves through the real CLI config parse.
    docker_v2:
      - images:
          - "myregistry/test-project"
        tags:
          - "{{{{ .Version }}}}"
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

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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
    formats: [tar.gz]
    format_overrides:
      - os: windows
        formats: [zip]
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
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "config"])
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
        formats: [tar.gz]
    checksum:
      name_template: "checksums.txt"
      algorithm: sha256
"#,
        host = host
    );
    create_config(tmp.path(), &config);

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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
        let output = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args).current_dir(tmp.path());
                cmd
            },
            "git",
        );
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
        formats: [tar.gz]
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

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "release",
            "--dry-run",
            "--snapshot",
            // Surface the changelog skip detail at default verbosity so the
            // changelog-stage assertion below can read it.
            "--show-skipped",
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

    // Verify sign stage ran in dry-run. The section header reads
    // "Signing artifacts" (a readable phrase, not a stage-name echo).
    assert!(
        stderr.contains("Signing"),
        "stderr should mention the sign stage header, got:\n{}",
        stderr
    );

    // CHANGELOG.md is written even in dry-run mode.
    // Anodizer matches this behavior for debugging and downstream stage consumption.
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
        formats: [tar.gz]
    checksum:
      name_template: "checksums.txt"
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["-f", config_path.to_str().unwrap(), "check", "config"])
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

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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
    let yaml_str = fs::read_to_string(tmp.path().join(".anodizer.yaml"))
        .expect(".anodizer.yaml should exist after init");

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
        formats: [tar.gz]
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
        formats: [tar.gz]
    checksum:
      name_template: "app-two-checksums.txt"
      algorithm: sha256
"#,
        host = host
    );
    create_config(tmp.path(), &config);

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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
        let output = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args).current_dir(tmp.path());
                cmd
            },
            "git",
        );
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
  snapshot: true
  sort: asc
  header: "# Release Notes"
  footer: "Generated by anodizer"
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
        formats: [tar.gz]
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

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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
        notes.contains("Generated by anodizer"),
        "changelog should contain footer, got:\n{}",
        notes
    );
}

/// E2E: cross-axis strict-mode smoke test — exercises three orthogonal
/// pipeline streams in one binary invocation:
///
///   * milestone pre-flight — `milestones[*].close: true` triggers the
///     validate-time pre-flight; the resolved-target log line is asserted
///     on stderr.
///   * SBOM → checksum cross-link — the SBOM stage emits an `Sbom`
///     artifact; the checksum stage's source-list (cross-linked to
///     `release_uploadable_kinds()`) must include it in `checksums.txt`.
///   * Changelog header propagation — `release.changelog.header`
///     propagates into `dist/CHANGELOG.md` (the rendered release-notes
///     body).
///
/// Runs under `--strict --snapshot` so any silently-skipped resolution would
/// fail loudly. A fourth axis (a synthetic DiskImage / Signature artifact
/// fed directly into the source-list) lives at the unit level in
/// `stage-checksum::test_checksum_source_list_cross_links_release_uploadable_kinds`
/// because registering bare artifact metadata is impractical from a
/// black-box binary test; the SBOM artifact path here exercises the same
/// cross-link without needing docker / cosign / hdiutil.
#[test]
fn test_strict_mode_cross_axis_smoke() {
    let tmp = TempDir::new().unwrap();
    let host = detect_host_target();

    create_test_project(tmp.path());

    let git = |args: &[&str]| {
        let output = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args).current_dir(tmp.path());
                cmd
            },
            "git",
        );
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    };

    // changelog.header set so the rendered body must contain it.
    // milestones[].close=true with an explicit repo so pre-flight can
    // resolve owner/name without a token or git remote.
    // A single sbom block triggers the builtin Cargo.lock-based SBOM
    // emitter (no syft dependency) so the Sbom artifact lands in the
    // checksum source-list.
    let config = format!(
        r##"project_name: test-project
changelog:
  snapshot: true
  sort: asc
  header: "# Cross-Axis Header"
sboms:
  - id: archive
    documents: ["{{{{ .ArtifactName }}}}.cdx.json"]
milestones:
  - close: true
    name_template: "{{{{ Tag }}}}"
    repo:
      owner: cross-axis-owner
      name: cross-axis-repo
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
        formats: [tar.gz]
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
        r#"fn main() { println!("cross-axis"); }"#,
    )
    .unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-m", "feat: cross-axis smoke"]);

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "--strict",
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
        "strict + snapshot release should succeed.\nstderr:\n{}",
        stderr
    );

    // milestone pre-flight emits the resolved target on stderr.
    assert!(
        stderr.contains("will close milestone")
            && stderr.contains("cross-axis-owner/cross-axis-repo"),
        "stderr should log the milestone pre-flight target, got:\n{}",
        stderr
    );

    // The rendered release notes body must contain the configured
    // changelog.header verbatim.
    let dist = tmp.path().join("dist");
    let notes = fs::read_to_string(dist.join("CHANGELOG.md"))
        .expect("dist/CHANGELOG.md should exist after release");
    assert!(
        notes.contains("# Cross-Axis Header"),
        "release notes should contain changelog.header, got:\n{}",
        notes
    );

    // The SBOM artifact must land in the checksums source-list. The
    // checksum filename is parsed structurally to avoid substring
    // false-positives between e.g. `foo.cdx.json` and `foo.cdx.json.sig`.
    let checksums = fs::read_to_string(dist.join("checksums.txt"))
        .expect("dist/checksums.txt should exist after release");
    let filenames: std::collections::HashSet<&str> = checksums
        .lines()
        .filter_map(|l| l.split_once("  ").map(|(_, name)| name))
        .collect();
    assert!(
        filenames.iter().any(|n| n.ends_with(".cdx.json")),
        "checksums.txt should list the SBOM artifact (cross-link), got: {:?}",
        filenames
    );

    // Strict-mode sentinel: no `strict_guard` rejection slipped through. A
    // strict-mode escalation would have failed the run; we assert the
    // stderr is free of the canonical strict bail-out so a future
    // regression that demotes a strict error to a warn still trips.
    assert!(
        !stderr.contains("strict mode: refusing"),
        "strict mode should not reject a well-formed config, got:\n{}",
        stderr
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
        let output = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args).current_dir(tmp.path());
                cmd
            },
            "git",
        );
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
  snapshot: true
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
        formats: [tar.gz]
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

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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
  hooks:
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
        formats: [tar.gz]
    checksum:
      name_template: "checksums.txt"
"#,
        host = host,
        marker = marker_yaml
    );
    create_config(tmp.path(), &config);

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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
  hooks:
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
        formats: [tar.gz]
    checksum:
      name_template: "checksums.txt"
"#,
        host = host,
        marker = marker_yaml
    );
    create_config(tmp.path(), &config);

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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

/// E2E #21: TOML config format — verify that anodizer can read .toml configs.
#[test]
fn test_e2e_toml_config_check() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());

    let config_path = tmp.path().join("anodizer.toml");
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

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["-f", config_path.to_str().unwrap(), "check", "config"])
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
        formats: [tar.gz]
    checksum:
      name_template: "checksums.txt"
      algorithm: sha256
"#,
        host = host
    );
    create_config(tmp.path(), &config);

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
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

/// `anodizer build` must produce the same per-stage outputs as
/// The build-command pipeline: before-hook marker file,
/// effective `dist/config.yaml`, `dist/metadata.json`,
/// `dist/artifacts.json`, and a size-report line when
/// `report_sizes: true`.
#[test]
fn test_e2e_build_command_matches_goreleaser_pipeline_outputs() {
    let tmp = TempDir::new().unwrap();
    let host = detect_host_target();

    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    // Use a before-hook that creates a sentinel file; `anodizer build`
    // must execute the hook (the build-command pipeline includes
    // before.Pipe). Cross-platform marker write uses sh-style on unix
    // and powershell on Windows so the test runs on every CI runner.
    let before_marker = tmp.path().join("before-ran");
    let before_marker_str = before_marker.to_string_lossy().replace('\\', "\\\\");
    let hook_cmd = if cfg!(windows) {
        // PowerShell New-Item is the analog of `touch` in PS 5+/Core.
        // `\"` escapes produce literal `\"` so the outer YAML double-quoted
        // string keeps the inner quotes as part of the PowerShell -Command
        // payload instead of terminating the YAML scalar early.
        format!(
            "powershell -NoProfile -Command \\\"New-Item -ItemType File -Force -Path '{}'\\\"",
            before_marker_str
        )
    } else {
        format!("touch {}", before_marker_str)
    };

    let config = format!(
        r#"project_name: test-project
report_sizes: true
before:
  hooks:
    - "{hook}"
crates:
  - name: test-project
    path: "."
    tag_template: "v{{{{ .Version }}}}"
    builds:
      - binary: test-project
        targets:
          - {host}
"#,
        host = host,
        hook = hook_cmd,
    );
    create_config(tmp.path(), &config);

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["build", "--timeout", "5m"])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "anodizer build should succeed.\nstderr:\n{}",
        stderr
    );

    // 1. before hooks ran
    assert!(
        before_marker.exists(),
        "before hooks should have run and created marker file\nstderr:\n{}",
        stderr
    );

    let dist = tmp.path().join("dist");

    // 2. effectiveconfig.yaml (anodizer writes it as config.yaml)
    assert!(
        dist.join("config.yaml").exists(),
        "dist/config.yaml should exist after build (effective config dump)"
    );

    // 3. metadata.json
    let metadata = dist.join("metadata.json");
    assert!(
        metadata.exists(),
        "dist/metadata.json should exist after build"
    );
    let metadata_text = fs::read_to_string(&metadata).unwrap();
    // Project name must round-trip through the metadata file.
    assert!(
        metadata_text.contains("test-project"),
        "metadata.json should contain project_name, got: {}",
        metadata_text
    );

    // 4. artifacts.json
    let artifacts = dist.join("artifacts.json");
    assert!(
        artifacts.exists(),
        "dist/artifacts.json should exist after build"
    );
    let artifacts_text = fs::read_to_string(&artifacts).unwrap();
    // Must list at least one binary artifact for the built crate.
    // anodizer serializes ArtifactKind as lowercase snake_case.
    assert!(
        artifacts_text.contains("\"binary\""),
        "artifacts.json should contain the built binary, got: {}",
        artifacts_text
    );

    // 5. reportsizes — size report line emitted to stderr
    let has_size_info = stderr.contains("bytes")
        || stderr.contains(" B")
        || stderr.contains("KB")
        || stderr.contains("MB")
        || stderr.contains("size")
        || stderr.contains("Size");
    assert!(
        has_size_info,
        "build should print size report when report_sizes is true, got:\n{}",
        stderr
    );
}

// ---- Release-resilience CLI flag runtime behaviour ----

/// `--rollback-only --from-run X` short-circuits the pipeline and replays
/// rollback against the prior run's `report.json`. When no such report
/// exists on disk the command surfaces a clear error referencing the
/// missing path (no other stages run).
#[test]
fn release_rollback_only_bails_when_prior_report_missing() {
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
"#,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["release", "--rollback-only", "--from-run", "abc123"])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "release --rollback-only with no prior report must exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("failed to read prior report") && stderr.contains("run-abc123"),
        "stderr should reference the missing prior report path, got: {}",
        stderr
    );
}

/// `--rollback-only --from-run X` reads `<dist>/run-<id>/report.json`,
/// invokes Publisher rollback for every Succeeded / RollbackFailed entry,
/// and writes the updated state to `<dist>/run-<id>/rollback.json`.
/// Submitter entries are not in the registry under this minimal config,
/// so the test fixture uses a publisher name that maps to nothing; the
/// outcome flips to `RollbackFailed("publisher not found...")` which is
/// the documented diagnostic when the registry no longer carries the
/// publisher recorded in the prior report.
#[test]
fn release_rollback_only_invokes_replay_from_disk() {
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
"#,
    );

    // Seed dist/run-fixt/report.json with a Succeeded Manager entry
    // whose name does not match any publisher in this minimal config.
    // Real publisher dispatch is not needed — only that the replay
    // path runs end-to-end and writes rollback.json.
    let run_dir = tmp.path().join("dist").join("run-fixt");
    fs::create_dir_all(&run_dir).unwrap();
    let report_json = r#"{
  "results": [
    {
      "name": "orphan-mgr",
      "group": "Manager",
      "required": true,
      "outcome": "Succeeded",
      "evidence": {
        "schema_version": 1,
        "publisher": "orphan-mgr",
        "primary_ref": null,
        "artifact_paths": [],
        "nondeterministic": null,
        "extra": {}
      }
    }
  ],
  "submitter_gated": false,
  "announce_gated": false
}"#;
    fs::write(run_dir.join("report.json"), report_json).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["release", "--rollback-only", "--from-run", "fixt"])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "release --rollback-only must succeed even when a recorded publisher is missing from the current registry (it surfaces as RollbackFailed in rollback.json). stdout:\n{}\nstderr:\n{}",
        stdout,
        stderr,
    );

    let rollback_path = run_dir.join("rollback.json");
    assert!(
        rollback_path.exists(),
        "rollback.json must be written to {}",
        rollback_path.display(),
    );
    let rollback_text = fs::read_to_string(&rollback_path).unwrap();
    assert!(
        rollback_text.contains("RollbackFailed")
            && rollback_text.contains("not found in current registry"),
        "rollback.json must carry diagnostic for missing publisher, got:\n{}",
        rollback_text,
    );
}

/// `--from-run=<id>` joins into a filesystem path; clap's value_parser
/// must reject path-traversal at the binary surface (not just in
/// in-process try_parse_from tests). Validates the whole-binary
/// happy/sad path so a regression in the value_parser wiring (e.g. a
/// future refactor that drops it) shows up here.
#[test]
fn release_from_run_rejects_path_traversal_at_binary_surface() {
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
"#,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "release",
            "--rollback-only",
            "--from-run",
            "../../etc/passwd",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "release --from-run=../../etc/passwd must exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--from-run") || stderr.contains("invalid"),
        "stderr should explain the rejection, got: {}",
        stderr
    );
    // Critically: the traversed path must NOT have been touched. The
    // poisoned destination `<dist>/run-../../etc/passwd/` resolves
    // (via Path::join) up out of the tempdir, so we cannot positively
    // assert "no file written" without scanning the real FS — but we
    // CAN assert the local <dist>/ stays free of any run-<id> dir
    // because the parser bailed before any stage ran. No `if exists()`
    // guard: dist/ not existing AND dist/ existing-with-no-run-* are
    // both passing states, and a future regression that lets parsing
    // succeed and `dist/run-../../etc/passwd` get mkdir'd before the
    // stage fails must trip this assertion loudly.
    let dist = tmp.path().join("dist");
    let has_traversal_leak = dist.exists()
        && std::fs::read_dir(&dist)
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().starts_with("run-"));
    assert!(
        !has_traversal_leak,
        "no run-* directory should leak into dist/ when parsing fails"
    );
}

/// Invalid `--rollback` values are caught at the translation site before any
/// pipeline work runs.
#[test]
fn release_rejects_invalid_rollback_value() {
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
"#,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["release", "--rollback", "wat"])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "release --rollback=wat must exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid --rollback value")
            && stderr.contains("none")
            && stderr.contains("best-effort"),
        "stderr should explain valid set, got: {}",
        stderr
    );
}

/// `--simulate-failure` without `ANODIZE_TEST_HARNESS=1` must be rejected
/// hard so production releases cannot weaponize the test harness.
#[test]
fn release_simulate_failure_gated_by_env() {
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
"#,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["release", "--simulate-failure", "cargo"])
        .env_remove("ANODIZE_TEST_HARNESS")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "release --simulate-failure without ANODIZE_TEST_HARNESS=1 must exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--simulate-failure") && stderr.contains("ANODIZE_TEST_HARNESS"),
        "stderr should explain the env gate, got: {}",
        stderr
    );
}

/// E2E regression: a required-publisher failure in real-release mode
/// (not snapshot, not dry-run) must surface as a non-zero exit. Two
/// gates defend this, in layers:
///
/// 1. The publish stage itself errors (`bail_on_required_failures` in
///    `crates/stage-publish/src/lib.rs`), after dispatch, rollback
///    bookkeeping, and report/summary persistence complete — so the
///    pipeline body returns `Err` and this is the gate that normally
///    fires.
/// 2. `gate_required_failures(&ctx)?` in
///    `crates/cli/src/commands/release/mod.rs` remains as the outer
///    defense: if the stage-level gate were removed, it would still
///    convert the recorded failure to a non-zero exit. Both gates must
///    be dropped before the simulated cargo failure rides through to
///    exit 0 and this test fails.
///
/// Both gates phrase the bail as `"... {N} required publisher(s)
/// failed: {names}. ..."` — so this test asserts both `"required
/// publisher"` and `"cargo"` appear in stderr without pinning which
/// layer fired. It also confirms `report.json` was persisted *before*
/// the gate fired so operators can replay rollback via
/// `--rollback-only --from-run=<id>`.
#[test]
fn release_required_publisher_failure_gates_exit_code() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    // Write config BEFORE `init_git_repo` so the initial commit
    // captures `.anodizer.yaml` — otherwise the validate stage trips
    // the "git is in a dirty state" check and we never reach the
    // publish gate.
    create_config(
        tmp.path(),
        r#"
project_name: test-project
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      cargo: {}
"#,
    );
    init_git_repo(tmp.path());

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "release",
            "--no-preflight",
            "--simulate-failure",
            "cargo",
            "--skip=build,upx,appbundle,dmg,msi,pkg,nsis,notarize,changelog,archive,source,nfpm,srpm,makeself,snapcraft,flatpak,sbom,templatefiles,checksum,sign,release,docker,docker-sign,blob,snapcraft-publish,announce",
        ])
        .env("ANODIZE_TEST_HARNESS", "1")
        .env_remove("CARGO_REGISTRY_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_TOKEN")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "release with required-publisher failure must exit non-zero; stderr: {stderr}"
    );
    assert!(
        stderr.contains("required publisher") && stderr.contains("cargo"),
        "stderr must carry the gate bail message; got: {stderr}"
    );
    // Sanity: report.json was written before the gate fired, so
    // `--rollback-only --from-run=v0.1.0` has something to replay.
    // `init_git_repo` tags the initial commit `v0.1.0`, which becomes
    // the run-id via `derive_run_id` in `crates/stage-publish/src/lib.rs`.
    let report = tmp
        .path()
        .join("dist")
        .join("run-v0.1.0")
        .join("report.json");
    assert!(
        report.exists(),
        "report.json must persist before gate fires; expected at {}",
        report.display()
    );
}

/// Companion to `release_required_publisher_failure_gates_exit_code`:
/// proves `gate_required_failures` does NOT false-positive when the
/// failing publisher is non-required. Uses `aur_source` — a non-binary,
/// source-distributing publisher (`required` defaults to false) — so the
/// binary-presence guard does not pre-empt the non-gating path under
/// `--skip=build`: a binary-requiring publisher (scoop, homebrew, ...)
/// would hard-bail in the guard before this gate is ever reached, since
/// no binary artifact exists. The publisher's runtime name is
/// `upstream-aur`, which is what `--simulate-failure` must target.
///
/// Together, these two tests pin both directions of the gate wiring:
/// - required + failed → non-zero exit (the regression class)
/// - non-required + failed → zero exit (no false-positive gate)
#[test]
fn release_non_required_publisher_failure_does_not_gate() {
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
    publish:
      aur_source: {}
"#,
    );
    init_git_repo(tmp.path());

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "release",
            "--no-preflight",
            "--simulate-failure",
            "upstream-aur",
            "--skip=build,upx,appbundle,dmg,msi,pkg,nsis,notarize,changelog,archive,source,nfpm,srpm,makeself,snapcraft,flatpak,sbom,templatefiles,checksum,sign,release,docker,docker-sign,blob,snapcraft-publish,announce",
        ])
        .env("ANODIZE_TEST_HARNESS", "1")
        .env_remove("CARGO_REGISTRY_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_TOKEN")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "release with only non-required publisher failure must exit 0;\nstderr: {stderr}\nstdout: {stdout}"
    );
    // The gate's bail message must NOT appear — that string is unique
    // to `gate_required_failures` and would only show if the gate
    // mistakenly fired for a non-required publisher.
    assert!(
        !stderr.contains("required publisher(s) failed"),
        "gate bail message must NOT appear for non-required failure; got: {stderr}"
    );
}

/// The environment preflight must abort `anodizer release` BEFORE any
/// stage runs: a configured cargo publisher demands CARGO_REGISTRY_TOKEN,
/// which is removed from the environment here, so the release must exit
/// non-zero with the preflight bail on stderr and leave no `dist/run-*`
/// directory behind — the publish stage persisting one would prove a
/// stage ran past the failed check.
#[test]
fn release_env_preflight_failure_aborts_before_any_stage() {
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
    publish:
      cargo: {}
"#,
    );
    init_git_repo(tmp.path());

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["release"])
        .env_remove("CARGO_REGISTRY_TOKEN")
        .env_remove("ANODIZER_GITHUB_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_TOKEN")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "release with an unsatisfiable preflight requirement must exit non-zero; stderr: {stderr}"
    );
    assert!(
        stderr.contains("environment failure(s)"),
        "stderr must carry the preflight bail; got: {stderr}"
    );
    assert!(
        stderr.contains("CARGO_REGISTRY_TOKEN"),
        "the failure report must name the missing env var; got: {stderr}"
    );
    // No stage ran: the publish stage writes `dist/run-<id>/` very early
    // in its execution, so any run dir means the abort came too late.
    let dist = tmp.path().join("dist");
    if dist.exists() {
        let run_dirs: Vec<String> = fs::read_dir(&dist)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("run-"))
            .collect();
        assert!(
            run_dirs.is_empty(),
            "preflight abort must precede every stage; found run dir(s): {run_dirs:?}\nstderr: {stderr}"
        );
    }
}

/// `--allow-nondeterministic foo` (no `=`) errors at the translation site.
#[test]
fn release_allow_nondeterministic_rejects_no_eq() {
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
"#,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["release", "--allow-nondeterministic", "barevalue"])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "release --allow-nondeterministic barevalue must exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("NAME=REASON"),
        "stderr should require NAME=REASON, got: {}",
        stderr
    );
}

/// `--allow-nondeterministic foo=` with empty reason is rejected so the
/// summary always carries a human-readable justification.
#[test]
fn release_allow_nondeterministic_rejects_empty_reason() {
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
"#,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["release", "--allow-nondeterministic", "foo="])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "release --allow-nondeterministic foo= must exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("reason cannot be empty"),
        "stderr should reject empty reason, got: {}",
        stderr
    );
}

/// `--strict` is mutually exclusive with `--allow-nondeterministic`. clap
/// can't express this across the global/subcommand boundary, so the runtime
/// check in `release::run` handles it.
#[test]
fn release_strict_conflicts_with_allow_nondeterministic() {
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
"#,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "--strict",
            "release",
            "--allow-nondeterministic",
            "foo.rpm=tool-bug",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "release --strict --allow-nondeterministic=... must exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--strict") && stderr.contains("--allow-nondeterministic"),
        "stderr should name both conflicting flags, got: {}",
        stderr
    );
}

/// E2E: `anodizer release --skip=announce --summary-json=<path>` writes
/// the per-publisher summary even when the announce stage is operator-
/// skipped. Binary-surface coverage for B6 finding I1; the unit-level
/// equivalent lives at `pipeline/builders.rs::tests::pipeline_emits_summary_when_announce_is_skipped_via_skip_flag`.
///
/// Snapshot + dry-run mode keep the test self-contained (no network,
/// no git tag required). We skip the heavy stages so the test runs
/// quickly; the only thing that matters for the assertion is that
/// the pipeline reaches `emit_summary` regardless of `--skip=announce`.
#[test]
fn test_release_skip_announce_still_writes_summary_json() {
    let host = detect_host_target();
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    init_git_repo(tmp.path());
    let config = create_single_crate_snapshot_config(&host);
    create_config(tmp.path(), &config);

    let summary_path = tmp.path().join("summary.json");

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "release",
            "--snapshot",
            "--dry-run",
            "--skip=build,archive,checksum,docker,sign,nfpm,changelog,sbom,release,publish,announce",
            "--summary-json",
            summary_path.to_str().unwrap(),
            "--timeout",
            "2m",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "release with --skip=announce + --summary-json must succeed.\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The audit-trail contract: summary.json lands on disk even though
    // announce was operator-skipped. Pre-I1 this assertion would fail
    // because emit_summary lived inside AnnounceStage::run.
    assert!(
        summary_path.exists(),
        "summary.json must be written even when announce is operator-skipped.\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
    let summary_text = std::fs::read_to_string(&summary_path)
        .expect("read summary.json that the binary just wrote");
    // Parse via the canonical struct to confirm the file is valid JSON
    // with the expected schema, not garbage / a previous file we forgot
    // to delete.
    let parsed: anodizer_stage_publish::run_summary::RunSummary =
        serde_json::from_str(&summary_text).expect("summary.json must parse as RunSummary");
    assert_eq!(
        parsed.schema_version,
        anodizer_stage_publish::run_summary::RunSummary::CURRENT_SCHEMA_VERSION,
        "schema_version field must round-trip",
    );
}

/// E2E: `--allow-rerun` and `--rollback-only` are mutually exclusive
/// at the clap layer (B6 review I-2). The combination must be
/// rejected at parse time with a clear conflicts error, NOT silently
/// no-op'd. We don't need a real release pipeline for this test —
/// clap rejects the args before any pipeline code runs.
#[test]
fn test_release_allow_rerun_conflicts_with_rollback_only() {
    let tmp = TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "release",
            "--rollback-only",
            "--from-run=v0.0.0-test",
            "--allow-rerun",
        ])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "release --rollback-only --allow-rerun must be rejected by clap",
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--allow-rerun") && stderr.contains("--rollback-only"),
        "stderr must name both conflicting flags, got: {}",
        stderr,
    );
}

// ============================================================================
// --workspace scoping: the overlay must scope the crate universe
// ============================================================================

/// Two-workspace fixture: each workspace holds one tiny binary crate, so a
/// scoped run's stage output names exactly one of them.
fn create_two_workspace_project(dir: &Path, host: &str) {
    fs::write(
        dir.join("Cargo.toml"),
        r#"[workspace]
resolver = "2"
members = ["crates/wa-app", "crates/wb-app"]
"#,
    )
    .unwrap();
    for name in ["wa-app", "wb-app"] {
        let d = dir.join(format!("crates/{name}"));
        fs::create_dir_all(d.join("src")).unwrap();
        fs::write(
            d.join("Cargo.toml"),
            format!(
                "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[[bin]]\nname = \"{name}\"\npath = \"src/main.rs\"\n"
            ),
        )
        .unwrap();
        fs::write(d.join("src/main.rs"), "fn main() {}\n").unwrap();
    }
    let config = format!(
        r#"project_name: two-ws
workspaces:
  - name: ws-a
    crates:
      - name: wa-app
        path: "crates/wa-app"
        tag_template: "wa-app-v{{{{ .Version }}}}"
        builds:
          - binary: wa-app
            targets:
              - {host}
  - name: ws-b
    crates:
      - name: wb-app
        path: "crates/wb-app"
        tag_template: "wb-app-v{{{{ .Version }}}}"
        builds:
          - binary: wb-app
            targets:
              - {host}
"#
    );
    create_config(dir, &config);
}

/// `release --workspace ws-a --dry-run` plans ONLY ws-a's crates. Before the
/// overlay cleared `config.workspaces`, dry-run's "empty selection = all"
/// walked the whole universe and planned the SIBLING workspace's builds and
/// archives under ws-a's overlay.
#[test]
fn test_release_workspace_dry_run_scopes_to_workspace() {
    let tmp = TempDir::new().unwrap();
    let host = detect_host_target();
    create_two_workspace_project(tmp.path(), &host);
    init_git_repo(tmp.path());

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "release",
            "--workspace",
            "ws-a",
            "--dry-run",
            "--show-skipped",
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
        "workspace-scoped dry-run should succeed.\nstderr:\n{}",
        stderr
    );
    assert!(
        stderr.contains("wa-app"),
        "stderr should mention ws-a's crate, got:\n{}",
        stderr
    );
    assert!(
        !stderr.contains("wb-app"),
        "a ws-a-scoped dry-run must NOT plan the sibling workspace's crate, got:\n{}",
        stderr
    );
}

/// `release --workspace ws-a --snapshot` builds ONLY ws-a's crates — a real
/// (non-dry-run) mode, proving the sibling's artifacts are never produced.
#[test]
fn test_release_workspace_snapshot_scopes_to_workspace() {
    let tmp = TempDir::new().unwrap();
    let host = detect_host_target();
    create_two_workspace_project(tmp.path(), &host);
    init_git_repo(tmp.path());

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args([
            "release",
            "--workspace",
            "ws-a",
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
        "workspace-scoped snapshot should succeed.\nstderr:\n{}",
        stderr
    );
    assert!(
        !stderr.contains("wb-app"),
        "a ws-a-scoped snapshot must NOT touch the sibling workspace's crate, got:\n{}",
        stderr
    );
    assert!(
        stderr.contains("wa-app"),
        "the scoped snapshot should build ws-a's crate, got:\n{}",
        stderr
    );
    // dist/ must hold exactly ONE archive (the scoped run counts a single
    // crate, so the single-crate ProjectName naming applies) and none of the
    // sibling's artifacts.
    let dist = tmp.path().join("dist");
    assert!(dist.exists(), "dist/ should exist after snapshot");
    let mut names: Vec<String> = Vec::new();
    let mut stack = vec![dist];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).unwrap() {
            let entry = entry.unwrap();
            if entry.path().is_dir() {
                stack.push(entry.path());
            }
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    let archives: Vec<&String> = names.iter().filter(|n| n.ends_with(".tar.gz")).collect();
    assert_eq!(
        archives.len(),
        1,
        "a ws-a-scoped snapshot must produce exactly one archive, found: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains("wb-app")),
        "dist/ must NOT contain the sibling workspace's artifacts, found: {names:?}"
    );
}

/// A `--crate` selection spanning a workspace and a top-level crate is a hard
/// error in BOTH orderings — the overlay decision reads the whole selection
/// set, and the loser of the old first()-based inference would otherwise be
/// silently dropped (or released under the wrong workspace's settings).
#[test]
fn test_release_mixed_scope_crate_selection_errors_both_orderings() {
    let tmp = TempDir::new().unwrap();
    let host = detect_host_target();
    create_two_workspace_project(tmp.path(), &host);
    // Add a top-level crate next to the two workspaces.
    let config = format!(
        r#"project_name: two-ws
crates:
  - name: top-app
    path: "crates/wb-app"
    tag_template: "top-app-v{{{{ .Version }}}}"
workspaces:
  - name: ws-a
    crates:
      - name: wa-app
        path: "crates/wa-app"
        tag_template: "wa-app-v{{{{ .Version }}}}"
        builds:
          - binary: wa-app
            targets:
              - {host}
"#
    );
    create_config(tmp.path(), &config);
    init_git_repo(tmp.path());

    for order in [["wa-app", "top-app"], ["top-app", "wa-app"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
            .args([
                "release",
                "--dry-run",
                "--crate",
                order[0],
                "--crate",
                order[1],
            ])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "mixed-scope selection ({order:?}) must fail"
        );
        assert!(
            stderr.contains("wa-app") && stderr.contains("top-app"),
            "error must name both crates for ordering {order:?}, got:\n{}",
            stderr
        );
    }
}

// ---------------------------------------------------------------------------
// build --crate: unknown-name validation + workspace inference
// ---------------------------------------------------------------------------

/// One-member `workspaces:` fixture with a buildable bin crate and a
/// workspace-level `env:` sentinel only the overlay can inject.
fn create_workspaces_member_build_project(dir: &Path) {
    let host = detect_host_target();
    fs::write(
        dir.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"tools/member\"]\n",
    )
    .unwrap();
    let member = dir.join("tools/member");
    fs::create_dir_all(member.join("src")).unwrap();
    fs::write(
        member.join("Cargo.toml"),
        "[package]\nname = \"member\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        member.join("src/main.rs"),
        "fn main() { println!(\"member\"); }\n",
    )
    .unwrap();
    let config = format!(
        r#"project_name: mono
workspaces:
  - name: tools
    env:
      - MEMBER_WS_SENTINEL=overlay-applied
    crates:
      - name: member
        path: tools/member
        tag_template: "member-v{{{{ .Version }}}}"
        builds:
          - binary: member
            targets:
              - {host}
"#
    );
    create_config(dir, &config);
    init_git_repo(dir);
}

/// `build --crate <unknown>` is a hard error naming the known crates — every
/// stage filters unknown names to an empty set, so before this validation a
/// typo produced a silent no-op "build complete".
#[test]
fn test_build_unknown_crate_hard_errors_naming_known_crates() {
    let tmp = TempDir::new().unwrap();
    create_workspaces_member_build_project(tmp.path());

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["build", "--crate", "nope"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "build --crate nope must fail, got stdout:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        stderr.contains("nope") && stderr.contains("member"),
        "error must name the unknown crate and the known ones, got:\n{stderr}"
    );
}

/// `build --crate <ws-member>` (no `--workspace`) infers the member's
/// workspace and applies its overlay — the same inference the release path
/// uses — so the member builds under its workspace env. The effective config
/// dump (written post-overlay) is the observable: the workspace `env:`
/// sentinel must be merged in and the sibling `workspaces:` block cleared.
#[test]
fn test_build_ws_member_crate_applies_workspace_inference() {
    let tmp = TempDir::new().unwrap();
    create_workspaces_member_build_project(tmp.path());

    let output = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["build", "--crate", "member"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "build --crate member must succeed.\nstderr:\n{stderr}"
    );

    let effective = fs::read_to_string(tmp.path().join("dist/config.yaml"))
        .expect("dist/config.yaml effective dump must exist");
    assert!(
        effective.contains("MEMBER_WS_SENTINEL=overlay-applied"),
        "workspace env must be merged by the inferred overlay, got:\n{effective}"
    );
    assert!(
        effective.contains("member"),
        "post-overlay universe must carry the member crate, got:\n{effective}"
    );
    assert!(
        !effective.contains("workspaces:")
            || effective.contains("workspaces: null")
            || effective.contains("workspaces: ~"),
        "the overlay must clear the workspaces block, got:\n{effective}"
    );
}
