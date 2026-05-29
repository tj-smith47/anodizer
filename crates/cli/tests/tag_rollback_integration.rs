//! Integration test for `anodize tag rollback`.
//!
//! Fixture: bare-bones repo with an initial commit + a "bump" commit
//! that lands a tag. Run `anodizer tag rollback --no-push` against
//! HEAD and assert:
//! - the tag at HEAD is gone,
//! - a revert commit replaces HEAD,
//! - the revert commit's subject carries the `chore(release): rollback`
//!   prefix and `[skip ci]` marker.

use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn anodizer() -> Command {
    Command::new(env!("CARGO_BIN_EXE_anodizer"))
}

fn run_git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@test.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@test.com")
        .output()
        .unwrap_or_else(|e| panic!("git {args:?} failed to spawn: {e}"));
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_init(dir: &Path) {
    run_git(dir, &["init", "-q", "-b", "master"]);
    run_git(dir, &["config", "user.email", "test@test.com"]);
    run_git(dir, &["config", "user.name", "Test"]);
    run_git(dir, &["config", "commit.gpgsign", "false"]);
}

fn git_tag_exists(dir: &Path, tag: &str) -> bool {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["tag", "-l", tag])
        .output()
        .unwrap();
    !String::from_utf8_lossy(&out.stdout).trim().is_empty()
}

fn git_head_subject(dir: &Path) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["log", "-1", "--format=%s", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn tag_rollback_local_deletes_tag_and_creates_revert_commit() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    git_init(dir);

    fs::write(dir.join("README.md"), "init\n").unwrap();
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-q", "-m", "initial"]);

    // Simulate an anodize-style bump commit + tag.
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-q", "-m", "chore(release): v1.0.0"]);
    run_git(dir, &["tag", "v1.0.0"]);

    assert!(
        git_tag_exists(dir, "v1.0.0"),
        "fixture setup: tag v1.0.0 should exist before rollback"
    );

    let out = anodizer()
        .current_dir(dir)
        .args(["tag", "rollback", "--no-push"])
        .output()
        .expect("anodizer tag rollback should spawn");
    assert!(
        out.status.success(),
        "anodizer tag rollback exited with status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    assert!(
        !git_tag_exists(dir, "v1.0.0"),
        "expected v1.0.0 to be deleted after rollback"
    );

    let subj = git_head_subject(dir);
    assert!(
        subj.starts_with("chore(release): rollback v1.0.0"),
        "unexpected HEAD subject after rollback: {subj}"
    );
    assert!(
        subj.contains("[skip ci]"),
        "rollback commit subject should contain [skip ci]: {subj}"
    );
}

#[test]
fn tag_rollback_dry_run_makes_no_mutations() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    git_init(dir);

    fs::write(dir.join("README.md"), "init\n").unwrap();
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-q", "-m", "initial"]);

    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-q", "-m", "chore(release): v1.0.0"]);
    run_git(dir, &["tag", "v1.0.0"]);
    let head_before = String::from_utf8(
        Command::new("git")
            .current_dir(dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();

    let out = anodizer()
        .current_dir(dir)
        .args(["tag", "rollback", "--dry-run", "--no-push"])
        .output()
        .expect("anodizer tag rollback --dry-run should spawn");
    assert!(out.status.success(), "dry-run should succeed");

    assert!(
        git_tag_exists(dir, "v1.0.0"),
        "dry-run must not delete tags"
    );
    let head_after = String::from_utf8(
        Command::new("git")
            .current_dir(dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    assert_eq!(head_before, head_after, "dry-run must not move HEAD");
}
