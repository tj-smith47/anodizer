//! Integration tests for `anodizer tag` workspace mode.
//!
//! When no `--crate` is given and the root Cargo.toml has
//! `[workspace.package].version`, the tag command treats the whole Cargo
//! workspace as a single versioned unit: rewrites the workspace package
//! version, every member's own version (for non-inheriting members), every
//! `[workspace.dependencies]` pin, and every sibling `[dependencies]` pin;
//! commits the edits; then creates the tag.

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
        .output()
        .unwrap_or_else(|e| panic!("git {:?} failed to spawn: {e}", args));
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

fn git_add_commit(dir: &Path, message: &str) {
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-q", "-m", message]);
}

/// Write a two-member inheriting workspace at version 0.1.0 plus a
/// `[workspace.dependencies]` table that pins both members.
fn inheriting_workspace_with_deps(tmp: &Path) {
    fs::write(
        tmp.join("Cargo.toml"),
        r#"[workspace]
members = ["crates/a", "crates/b"]
resolver = "2"

[workspace.package]
version = "0.1.0"

[workspace.dependencies]
a = { path = "crates/a", version = "0.1.0" }
b = { path = "crates/b", version = "0.1.0" }
"#,
    )
    .unwrap();
    fs::create_dir_all(tmp.join("crates/a/src")).unwrap();
    fs::create_dir_all(tmp.join("crates/b/src")).unwrap();
    fs::write(
        tmp.join("crates/a/Cargo.toml"),
        r#"[package]
name = "a"
version.workspace = true
edition = "2024"
"#,
    )
    .unwrap();
    fs::write(tmp.join("crates/a/src/lib.rs"), "").unwrap();
    fs::write(
        tmp.join("crates/b/Cargo.toml"),
        r#"[package]
name = "b"
version.workspace = true
edition = "2024"

[dependencies]
a = { workspace = true }
"#,
    )
    .unwrap();
    fs::write(tmp.join("crates/b/src/lib.rs"), "").unwrap();
}

fn read_workspace_package_version(root: &Path) -> String {
    let text = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let doc = text.parse::<toml_edit::DocumentMut>().unwrap();
    doc.get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string()
}

fn read_workspace_dep_version(root: &Path, name: &str) -> Option<String> {
    let text = fs::read_to_string(root.join("Cargo.toml")).ok()?;
    let doc = text.parse::<toml_edit::DocumentMut>().ok()?;
    let dep = doc.get("workspace")?.get("dependencies")?.get(name)?;
    if let Some(s) = dep.as_str() {
        return Some(s.to_string());
    }
    if let Some(t) = dep.as_inline_table() {
        return t.get("version").and_then(|v| v.as_str()).map(String::from);
    }
    if let Some(t) = dep.as_table() {
        return t.get("version").and_then(|v| v.as_str()).map(String::from);
    }
    None
}

fn git_tag_exists(dir: &Path, tag: &str) -> bool {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["tag", "-l", tag])
        .output()
        .unwrap();
    !String::from_utf8_lossy(&out.stdout).trim().is_empty()
}

#[test]
fn workspace_mode_bumps_inheriting_members_and_dep_pins() {
    let tmp = TempDir::new().unwrap();
    inheriting_workspace_with_deps(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");
    // v0.1.0 baseline tag exists, then a patch-worthy commit.
    run_git(tmp.path(), &["tag", "v0.1.0"]);
    fs::write(tmp.path().join("crates/a/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(tmp.path(), "fix: a deref issue");

    let out = anodizer()
        .current_dir(tmp.path())
        .args(["tag"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tag failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("new_tag=v0.1.1"),
        "expected new_tag=v0.1.1 in stdout: {stdout}"
    );

    assert_eq!(read_workspace_package_version(tmp.path()), "0.1.1");
    assert_eq!(
        read_workspace_dep_version(tmp.path(), "a").as_deref(),
        Some("0.1.1")
    );
    assert_eq!(
        read_workspace_dep_version(tmp.path(), "b").as_deref(),
        Some("0.1.1")
    );
    assert!(
        git_tag_exists(tmp.path(), "v0.1.1"),
        "v0.1.1 should be created"
    );
}

#[test]
fn workspace_mode_dry_run_touches_nothing() {
    let tmp = TempDir::new().unwrap();
    inheriting_workspace_with_deps(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");
    run_git(tmp.path(), &["tag", "v0.1.0"]);
    fs::write(tmp.path().join("crates/a/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(tmp.path(), "feat: shiny new thing");

    let before_root = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
    let out = anodizer()
        .current_dir(tmp.path())
        .args(["tag", "--dry-run"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tag --dry-run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let after_root = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
    assert_eq!(before_root, after_root, "--dry-run must not edit manifests");
    assert!(
        !git_tag_exists(tmp.path(), "v0.2.0"),
        "--dry-run must not create a tag"
    );
}

#[test]
fn workspace_mode_skipped_when_crate_flag_used() {
    // With --crate pointing at a single crate and no .anodizer.yaml config for
    // it, tag falls through to its non-workspace branch. The important
    // behavior here: workspace-mode must NOT silently overwrite the
    // user-chosen single-crate flow.
    let tmp = TempDir::new().unwrap();
    inheriting_workspace_with_deps(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");
    run_git(tmp.path(), &["tag", "a-v0.1.0"]);
    fs::write(tmp.path().join("crates/a/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(tmp.path(), "fix: a thing");

    let out = anodizer()
        .current_dir(tmp.path())
        .args(["tag", "--crate", "a"])
        .output()
        .unwrap();
    // With no .anodizer.yaml, --crate silently no-ops on config lookup and
    // falls through to the base non-version-sync flow. The root Cargo.toml
    // must remain at 0.1.0 — workspace mode must NOT fire when --crate is
    // given.
    let _ = out; // just verify the workspace wasn't touched
    assert_eq!(read_workspace_package_version(tmp.path()), "0.1.0");
    assert_eq!(
        read_workspace_dep_version(tmp.path(), "a").as_deref(),
        Some("0.1.0")
    );
}

#[test]
fn workspace_mode_skips_when_already_at_target() {
    // Manually-bumped Cargo.toml already at the next version: tag must still
    // create the tag but not create a redundant bump commit.
    let tmp = TempDir::new().unwrap();
    inheriting_workspace_with_deps(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");
    run_git(tmp.path(), &["tag", "v0.1.0"]);

    // Hand-bump root Cargo.toml to 0.1.1 and commit. Use `#patch` so the
    // detected bump matches the manual value; otherwise the default_bump
    // fallback would minor-bump past 0.1.1.
    let mut root = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
    root = root.replace("version = \"0.1.0\"", "version = \"0.1.1\"");
    fs::write(tmp.path().join("Cargo.toml"), &root).unwrap();
    git_add_commit(tmp.path(), "chore: bump workspace manually #patch");

    let head_before = Command::new("git")
        .current_dir(tmp.path())
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let head_before = String::from_utf8_lossy(&head_before.stdout)
        .trim()
        .to_string();

    let out = anodizer()
        .current_dir(tmp.path())
        .args(["tag"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tag failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("new_tag=v0.1.1"),
        "expected new_tag=v0.1.1: {stdout}"
    );

    let head_after = Command::new("git")
        .current_dir(tmp.path())
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let head_after = String::from_utf8_lossy(&head_after.stdout)
        .trim()
        .to_string();

    // No redundant bump commit should be created — workspace already at target.
    assert_eq!(
        head_before, head_after,
        "workspace already at target version should not add a bump commit"
    );
    assert!(git_tag_exists(tmp.path(), "v0.1.1"));
}
