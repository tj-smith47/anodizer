//! Exit-code classification contract: deterministic config/usage errors
//! exit 2 and stamp the `anodizer-error-class: deterministic` stderr
//! marker; everything else keeps exit 1. Retry wrappers key off both
//! signals, so each pinned row here is consumer-facing surface.

use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

use anodizer_core::error_class::{CLASS_MARKER, EXIT_DETERMINISTIC};
use anodizer_core::test_helpers::{create_config, create_test_project, init_git_repo};

fn run_anodizer(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(args)
        .current_dir(dir)
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_TOKEN")
        .env_remove("ANODIZER_GITHUB_TOKEN")
        .output()
        .expect("invoke anodizer")
}

fn assert_deterministic(out: &std::process::Output, label: &str) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(EXIT_DETERMINISTIC),
        "{label}: expected exit {EXIT_DETERMINISTIC}; got {:?}\nstderr:\n{stderr}",
        out.status.code()
    );
    assert!(
        stderr.contains(CLASS_MARKER),
        "{label}: stderr must carry the classification marker; got:\n{stderr}"
    );
}

fn host_target() -> String {
    let output = Command::new("rustc")
        .arg("-vV")
        .output()
        .expect("rustc -vV");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("host: ").map(str::to_string))
        .expect("host triple")
}

fn minimal_fixture(tmp: &Path) {
    create_test_project(tmp);
    init_git_repo(tmp);
    create_config(
        tmp,
        &format!(
            r#"project_name: test-project
crates:
  - name: test-project
    path: "."
    tag_template: "v{{{{ .Version }}}}"
    builds:
      - binary: test-project
        targets:
          - {}
"#,
            host_target()
        ),
    );
}

#[test]
fn unknown_flag_exits_two_with_marker() {
    let tmp = TempDir::new().unwrap();
    let out = run_anodizer(tmp.path(), &["release", "--definitely-not-a-flag"]);
    assert_deterministic(&out, "unknown flag");
}

#[test]
fn unknown_publisher_exits_two_with_marker() {
    let tmp = TempDir::new().unwrap();
    minimal_fixture(tmp.path());
    let out = run_anodizer(
        tmp.path(),
        &["publish", "--dry-run", "--publishers", "not-a-publisher"],
    );
    assert_deterministic(&out, "unknown publisher");
}

#[test]
fn unknown_skip_token_exits_two_with_marker() {
    let tmp = TempDir::new().unwrap();
    minimal_fixture(tmp.path());
    let out = run_anodizer(
        tmp.path(),
        &["release", "--dry-run", "--skip", "not-a-stage"],
    );
    assert_deterministic(&out, "unknown skip token");
}

#[test]
fn invalid_timeout_value_exits_two_with_marker() {
    let tmp = TempDir::new().unwrap();
    minimal_fixture(tmp.path());
    let out = run_anodizer(tmp.path(), &["release", "--dry-run", "--timeout", "banana"]);
    assert_deterministic(&out, "invalid --timeout");
}

#[test]
fn unparseable_config_exits_two_with_marker() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    init_git_repo(tmp.path());
    fs::write(
        tmp.path().join(".anodizer.yaml"),
        "version: 2\nbuilds: [unclosed\n",
    )
    .unwrap();
    let out = run_anodizer(tmp.path(), &["check", "config"]);
    assert_deterministic(&out, "unparseable config");
}

#[test]
fn unknown_config_field_exits_two_with_marker() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    init_git_repo(tmp.path());
    fs::write(
        tmp.path().join(".anodizer.yaml"),
        "version: 2\nno_such_top_level_field: true\n",
    )
    .unwrap();
    let out = run_anodizer(tmp.path(), &["check", "config"]);
    assert_deterministic(&out, "unknown config field");
}

#[test]
fn missing_explicit_config_exits_two_with_marker() {
    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    init_git_repo(tmp.path());
    let out = run_anodizer(
        tmp.path(),
        &["check", "config", "--config", "does-not-exist.yaml"],
    );
    assert_deterministic(&out, "missing --config path");
}

#[test]
fn dist_not_empty_exits_two_with_marker() {
    let tmp = TempDir::new().unwrap();
    minimal_fixture(tmp.path());
    fs::create_dir_all(tmp.path().join("dist")).unwrap();
    fs::write(tmp.path().join("dist/leftover.txt"), "stale").unwrap();
    let out = run_anodizer(
        tmp.path(),
        &["release", "--dry-run", "--timeout", "2m", "--snapshot"],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not empty"),
        "expected the dist-not-empty guard to fire; got:\n{stderr}"
    );
    assert_deterministic(&out, "dist not empty");
}

#[test]
fn tag_rollback_parent_only_flag_exits_two_with_marker() {
    // `tag rollback` rejects the parent `tag` command's flags (--push family,
    // --changelog, --version). These rejections are argv-determined, so they
    // must carry the deterministic exit code + marker like every other usage
    // error — not a bare exit 1 that a retry wrapper would re-attempt.
    let tmp = TempDir::new().unwrap();
    minimal_fixture(tmp.path());
    let out = run_anodizer(tmp.path(), &["tag", "--changelog", "rollback"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("applies to `anodizer tag`, not `tag rollback`"),
        "must reject via the rollback usage guard, not clap: {stderr}"
    );
    assert_deterministic(&out, "tag rollback --changelog");
}

#[test]
fn help_exits_zero_without_marker() {
    let tmp = TempDir::new().unwrap();
    let out = run_anodizer(tmp.path(), &["--help"]);
    assert_eq!(out.status.code(), Some(0), "--help must exit 0");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains(CLASS_MARKER),
        "--help must not emit the classification marker"
    );
}

#[test]
fn nondeterministic_failure_keeps_exit_one() {
    // `continue` with an empty dist fails on missing preserved state — a
    // run-environment condition, deliberately outside the deterministic
    // allowlist — so it must keep the generic failure exit.
    let tmp = TempDir::new().unwrap();
    minimal_fixture(tmp.path());
    fs::create_dir_all(tmp.path().join("dist")).unwrap();
    let out = run_anodizer(tmp.path(), &["release", "--publish-only", "--dry-run"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(1),
        "missing preserved dist must keep exit 1; stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains(CLASS_MARKER),
        "unclassified failure must not emit the marker; got:\n{stderr}"
    );
}
