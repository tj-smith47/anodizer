use std::process::Command;
use tempfile::TempDir;
use std::fs;

/// Helper to create a minimal Cargo project for testing
fn create_test_project(dir: &std::path::Path) {
    // Create Cargo.toml
    fs::write(dir.join("Cargo.toml"), r#"
[package]
name = "test-project"
version = "0.1.0"
edition = "2024"

[[bin]]
name = "test-project"
path = "src/main.rs"
"#).unwrap();

    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join("src/main.rs"), r#"fn main() { println!("hello"); }"#).unwrap();
}

/// Helper to create an anodize.yaml config
fn create_config(dir: &std::path::Path, content: &str) {
    fs::write(dir.join(".anodize.yaml"), content).unwrap();
}

/// Helper to init git repo with a tag
fn init_git_repo(dir: &std::path::Path) {
    let run = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git command failed");
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
    create_config(tmp.path(), r#"
project_name: test-project
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
"#);

    let output = Command::new(env!("CARGO_BIN_EXE_anodize"))
        .arg("check")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "check should succeed: {}", String::from_utf8_lossy(&output.stderr));
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

    assert!(output.status.success(), "init should succeed: {}", String::from_utf8_lossy(&output.stderr));
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
    fs::write(&config_path, r#"
project_name: test-project
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
"#).unwrap();

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
    fs::write(&config_path, r#"
project_name: test-project
crates:
  - name: test-project
    path: "."
    tag_template: "v{{ .Version }}"
"#).unwrap();

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
