//! End-to-end tests for `anodize bump`.
//!
//! Each test builds a minimal cargo workspace inside a `TempDir`, initializes
//! a git repo, and shells out to the compiled `anodize` binary via
//! `CARGO_BIN_EXE_anodize`. Workspaces only contain manifests (no sources) —
//! `bump` never invokes cargo, it only reads/writes `Cargo.toml`.

use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn anodize() -> Command {
    Command::new(env!("CARGO_BIN_EXE_anodize"))
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
    run_git(dir, &["init", "-q"]);
    run_git(dir, &["config", "user.email", "test@test.com"]);
    run_git(dir, &["config", "user.name", "Test"]);
    run_git(dir, &["config", "commit.gpgsign", "false"]);
}

fn git_add_commit(dir: &Path, message: &str) {
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-q", "-m", message]);
}

fn git_commit_empty_on_path(dir: &Path, relpath: &str, content: &str, message: &str) {
    let full = dir.join(relpath);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&full, content).unwrap();
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-q", "-m", message]);
}

// ---------------------------------------------------------------------------
// Workspace builders
// ---------------------------------------------------------------------------

/// Single-crate workspace at `tmp/Cargo.toml` with literal version "0.1.0".
fn single_crate_workspace(tmp: &Path) {
    fs::write(
        tmp.join("Cargo.toml"),
        r#"[package]
name = "demo"
version = "0.1.0"
edition = "2024"
"#,
    )
    .unwrap();
}

/// Two-member workspace: `core` (0.1.0) and `cli` (0.1.0), cli depends on core.
fn two_crate_workspace(tmp: &Path) {
    fs::write(
        tmp.join("Cargo.toml"),
        r#"[workspace]
members = ["crates/core", "crates/cli"]
resolver = "2"
"#,
    )
    .unwrap();
    fs::create_dir_all(tmp.join("crates/core")).unwrap();
    fs::create_dir_all(tmp.join("crates/cli")).unwrap();
    fs::write(
        tmp.join("crates/core/Cargo.toml"),
        r#"[package]
name = "core"
version = "0.1.0"
edition = "2024"
"#,
    )
    .unwrap();
    fs::write(
        tmp.join("crates/cli/Cargo.toml"),
        r#"[package]
name = "cli"
version = "0.1.0"
edition = "2024"

[dependencies]
core = { path = "../core", version = "0.1.0" }
"#,
    )
    .unwrap();
}

/// Workspace with `version.workspace = true` inheritance on both members.
fn inheriting_workspace(tmp: &Path) {
    fs::write(
        tmp.join("Cargo.toml"),
        r#"[workspace]
members = ["crates/a", "crates/b"]
resolver = "2"

[workspace.package]
version = "0.3.0"
"#,
    )
    .unwrap();
    fs::create_dir_all(tmp.join("crates/a")).unwrap();
    fs::create_dir_all(tmp.join("crates/b")).unwrap();
    fs::write(
        tmp.join("crates/a/Cargo.toml"),
        r#"[package]
name = "a"
version.workspace = true
edition = "2024"
"#,
    )
    .unwrap();
    fs::write(
        tmp.join("crates/b/Cargo.toml"),
        r#"[package]
name = "b"
version.workspace = true
edition = "2024"
"#,
    )
    .unwrap();
}

/// Workspace where one member is `publish = false`.
fn workspace_with_private_member(tmp: &Path) {
    fs::write(
        tmp.join("Cargo.toml"),
        r#"[workspace]
members = ["crates/pub", "crates/priv"]
resolver = "2"
"#,
    )
    .unwrap();
    fs::create_dir_all(tmp.join("crates/pub")).unwrap();
    fs::create_dir_all(tmp.join("crates/priv")).unwrap();
    fs::write(
        tmp.join("crates/pub/Cargo.toml"),
        r#"[package]
name = "pub"
version = "0.1.0"
edition = "2024"
"#,
    )
    .unwrap();
    fs::write(
        tmp.join("crates/priv/Cargo.toml"),
        r#"[package]
name = "priv"
version = "0.1.0"
edition = "2024"
publish = false
"#,
    )
    .unwrap();
}

fn read_version(manifest: &Path) -> String {
    let text = fs::read_to_string(manifest).unwrap();
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("version")
            && let Some(eq) = rest.find('=')
        {
            let raw = rest[eq + 1..].trim();
            // strip quotes if present
            if let Some(s) = raw
                .trim_start_matches('"')
                .split('"')
                .next()
                .filter(|s| !s.is_empty() && s.chars().next().unwrap().is_ascii_digit())
            {
                return s.to_string();
            }
        }
    }
    panic!("no version in {}", manifest.display());
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn patch_bumps_single_crate() {
    let tmp = TempDir::new().unwrap();
    single_crate_workspace(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");

    let out = anodize()
        .current_dir(tmp.path())
        .args(["bump", "patch", "-y"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "bump failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(read_version(&tmp.path().join("Cargo.toml")), "0.1.1");
}

#[test]
fn minor_explicit_then_major() {
    let tmp = TempDir::new().unwrap();
    single_crate_workspace(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");

    anodize()
        .current_dir(tmp.path())
        .args(["bump", "minor", "-y"])
        .status()
        .unwrap();
    assert_eq!(read_version(&tmp.path().join("Cargo.toml")), "0.2.0");
    // Tree is now dirty from the first bump; second bump must pass --allow-dirty.
    anodize()
        .current_dir(tmp.path())
        .args(["bump", "major", "-y", "--allow-dirty"])
        .status()
        .unwrap();
    assert_eq!(read_version(&tmp.path().join("Cargo.toml")), "1.0.0");
}

#[test]
fn dry_run_writes_nothing() {
    let tmp = TempDir::new().unwrap();
    single_crate_workspace(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");

    let before = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
    let out = anodize()
        .current_dir(tmp.path())
        .args(["bump", "minor", "--dry-run"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The plan table should mention the old→new transition.
    assert!(stdout.contains("0.1.0"), "stdout missing 0.1.0: {stdout}");
    assert!(stdout.contains("0.2.0"), "stdout missing 0.2.0: {stdout}");
    let after = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
    assert_eq!(before, after, "--dry-run should not touch the manifest");
}

#[test]
fn dry_run_json_is_parseable() {
    let tmp = TempDir::new().unwrap();
    single_crate_workspace(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");

    let out = anodize()
        .current_dir(tmp.path())
        .args(["bump", "minor", "--dry-run", "--output", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "json dry-run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stdout must be JSON");
    let arr = v.as_array().expect("json root must be an array");
    assert_eq!(arr.len(), 1);
    let row = &arr[0];
    assert_eq!(row["crate"], "demo");
    assert_eq!(row["current"], "0.1.0");
    assert_eq!(row["next"], "0.2.0");
    assert_eq!(row["level"], "minor");
}

#[test]
fn dirty_tree_refused_without_allow_dirty() {
    let tmp = TempDir::new().unwrap();
    single_crate_workspace(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");
    // Introduce a dirty change.
    fs::write(tmp.path().join("dirty.txt"), "hello").unwrap();

    let out = anodize()
        .current_dir(tmp.path())
        .args(["bump", "patch", "-y"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "bump should refuse a dirty tree; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("uncommitted") || err.contains("dirty"),
        "error should mention uncommitted changes: {err}"
    );
}

#[test]
fn publish_false_skipped_from_workspace() {
    let tmp = TempDir::new().unwrap();
    workspace_with_private_member(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");

    let out = anodize()
        .current_dir(tmp.path())
        .args(["bump", "patch", "--workspace", "-y"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "bump --workspace failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        read_version(&tmp.path().join("crates/pub/Cargo.toml")),
        "0.1.1"
    );
    // publish = false crate should be untouched.
    assert_eq!(
        read_version(&tmp.path().join("crates/priv/Cargo.toml")),
        "0.1.0"
    );
}

#[test]
fn workspace_package_inheritance_bumps_root_only() {
    let tmp = TempDir::new().unwrap();
    inheriting_workspace(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");

    let out = anodize()
        .current_dir(tmp.path())
        .args(["bump", "minor", "--workspace", "-y"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "bump --workspace failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Root [workspace.package].version → 0.4.0.
    let root = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
    assert!(
        root.contains("version = \"0.4.0\""),
        "root should be bumped to 0.4.0: {root}"
    );
    // Members must still say version.workspace — we don't rewrite them.
    let a = fs::read_to_string(tmp.path().join("crates/a/Cargo.toml")).unwrap();
    assert!(a.contains("version.workspace = true"), "member a: {a}");
}

#[test]
fn exact_skips_dep_propagation() {
    let tmp = TempDir::new().unwrap();
    two_crate_workspace(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");

    let out = anodize()
        .current_dir(tmp.path())
        .args(["bump", "patch", "-p", "core", "--exact", "-y"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    // core bumped.
    assert_eq!(
        read_version(&tmp.path().join("crates/core/Cargo.toml")),
        "0.1.1"
    );
    // cli's [dependencies] core = "0.1.0" left untouched.
    let cli = fs::read_to_string(tmp.path().join("crates/cli/Cargo.toml")).unwrap();
    assert!(
        cli.contains("version = \"0.1.0\""),
        "cli dep on core should NOT be rewritten under --exact: {cli}"
    );
}

#[test]
fn propagation_rewrites_sibling_dep() {
    let tmp = TempDir::new().unwrap();
    two_crate_workspace(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");

    let out = anodize()
        .current_dir(tmp.path())
        .args(["bump", "patch", "-p", "core", "-y"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // core bumped.
    assert_eq!(
        read_version(&tmp.path().join("crates/core/Cargo.toml")),
        "0.1.1"
    );
    // cli's own version is untouched.
    assert_eq!(
        read_version(&tmp.path().join("crates/cli/Cargo.toml")),
        "0.1.0"
    );
    // cli's [dependencies] core version is rewritten.
    let cli = fs::read_to_string(tmp.path().join("crates/cli/Cargo.toml")).unwrap();
    assert!(
        cli.contains("version = \"0.1.1\""),
        "cli dep on core should be rewritten: {cli}"
    );
}

#[test]
fn commit_flag_creates_single_commit() {
    let tmp = TempDir::new().unwrap();
    single_crate_workspace(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");

    let before = Command::new("git")
        .current_dir(tmp.path())
        .args(["rev-list", "--count", "HEAD"])
        .output()
        .unwrap();
    let before_n: u32 = String::from_utf8_lossy(&before.stdout)
        .trim()
        .parse()
        .unwrap();

    let out = anodize()
        .current_dir(tmp.path())
        .args(["bump", "patch", "--commit", "-y"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "bump --commit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let after = Command::new("git")
        .current_dir(tmp.path())
        .args(["rev-list", "--count", "HEAD"])
        .output()
        .unwrap();
    let after_n: u32 = String::from_utf8_lossy(&after.stdout)
        .trim()
        .parse()
        .unwrap();
    assert_eq!(after_n, before_n + 1, "exactly one new commit expected");

    // Commit message should include the new version.
    let msg = Command::new("git")
        .current_dir(tmp.path())
        .args(["log", "-1", "--pretty=%B"])
        .output()
        .unwrap();
    let msg = String::from_utf8_lossy(&msg.stdout);
    assert!(
        msg.contains("0.1.1"),
        "commit message missing version: {msg}"
    );
    assert!(
        msg.contains("demo"),
        "commit message missing crate name: {msg}"
    );
}

#[test]
fn infer_picks_per_crate_level_from_commits() {
    let tmp = TempDir::new().unwrap();
    two_crate_workspace(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");
    // Seed a tag for each crate so the inference range starts there.
    run_git(tmp.path(), &["tag", "core-v0.1.0"]);
    run_git(tmp.path(), &["tag", "cli-v0.1.0"]);

    // Commit that only touches core (a feat) → minor bump for core.
    git_commit_empty_on_path(
        tmp.path(),
        "crates/core/new_feature.rs",
        "pub fn f() {}",
        "feat(core): add new feature",
    );
    // Commit that only touches cli (a fix) → patch bump for cli.
    git_commit_empty_on_path(
        tmp.path(),
        "crates/cli/bugfix.rs",
        "pub fn g() {}",
        "fix(cli): correct bug",
    );

    let out = anodize()
        .current_dir(tmp.path())
        .args(["bump", "--workspace", "--dry-run", "--output", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "infer dry-run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    let rows = v.as_array().unwrap();
    let by_name: std::collections::HashMap<&str, &serde_json::Value> = rows
        .iter()
        .map(|r| (r["crate"].as_str().unwrap(), r))
        .collect();
    assert_eq!(by_name["core"]["level"], "minor");
    assert_eq!(by_name["core"]["next"], "0.2.0");
    assert_eq!(by_name["cli"]["level"], "patch");
    assert_eq!(by_name["cli"]["next"], "0.1.1");
}

#[test]
fn multi_crate_without_selection_errors() {
    let tmp = TempDir::new().unwrap();
    two_crate_workspace(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");

    let out = anodize()
        .current_dir(tmp.path())
        .args(["bump", "patch", "-y"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "multi-crate bump without selection should error"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("-p") || err.contains("--workspace"),
        "error should suggest -p or --workspace: {err}"
    );
}

#[test]
fn release_strips_prerelease() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("Cargo.toml"),
        r#"[package]
name = "demo"
version = "1.0.0-rc.1"
edition = "2024"
"#,
    )
    .unwrap();
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");

    let out = anodize()
        .current_dir(tmp.path())
        .args(["bump", "release", "-y"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "release bump failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(read_version(&tmp.path().join("Cargo.toml")), "1.0.0");
}

#[test]
fn pre_appends_prerelease() {
    let tmp = TempDir::new().unwrap();
    single_crate_workspace(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");

    let out = anodize()
        .current_dir(tmp.path())
        .args(["bump", "minor", "--pre", "rc.1", "-y"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "pre bump failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
    assert!(
        text.contains("version = \"0.2.0-rc.1\""),
        "expected 0.2.0-rc.1: {text}"
    );
}

// ---------------------------------------------------------------------------
// Bundled changelog (via stage-changelog::render_crate_section)
// ---------------------------------------------------------------------------

#[test]
fn commit_bundles_changelog_when_configured() {
    let tmp = TempDir::new().unwrap();
    single_crate_workspace(tmp.path());
    fs::write(
        tmp.path().join(".anodize.yaml"),
        r#"version: 2
project_name: demo
crates:
  - name: demo
    path: .
    tag_template: "v{{ Version }}"
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
    )
    .unwrap();
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");
    // Tag the initial state so the changelog inference range starts there.
    run_git(tmp.path(), &["tag", "demo-v0.1.0"]);
    // A feat commit since the tag — this is what the rendered section must
    // include.
    git_commit_empty_on_path(
        tmp.path(),
        "src/feature.rs",
        "pub fn f() {}",
        "feat: add a sparkly new feature",
    );

    let before = Command::new("git")
        .current_dir(tmp.path())
        .args(["rev-list", "--count", "HEAD"])
        .output()
        .unwrap();
    let before_n: u32 = String::from_utf8_lossy(&before.stdout)
        .trim()
        .parse()
        .unwrap();

    let out = anodize()
        .current_dir(tmp.path())
        .args(["bump", "patch", "--commit", "-y"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "bump --commit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let after = Command::new("git")
        .current_dir(tmp.path())
        .args(["rev-list", "--count", "HEAD"])
        .output()
        .unwrap();
    let after_n: u32 = String::from_utf8_lossy(&after.stdout)
        .trim()
        .parse()
        .unwrap();
    assert_eq!(
        after_n,
        before_n + 1,
        "exactly one new commit must include the bundled changelog"
    );

    // Diff of the new commit must touch BOTH Cargo.toml and CHANGELOG.md.
    let diff = Command::new("git")
        .current_dir(tmp.path())
        .args(["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"])
        .output()
        .unwrap();
    let names = String::from_utf8_lossy(&diff.stdout);
    assert!(
        names.lines().any(|l| l == "Cargo.toml"),
        "commit must touch Cargo.toml: {names}"
    );
    assert!(
        names.lines().any(|l| l == "CHANGELOG.md"),
        "commit must touch CHANGELOG.md: {names}"
    );

    // Sanity: the new section is present on disk.
    let cl = fs::read_to_string(tmp.path().join("CHANGELOG.md")).unwrap();
    assert!(cl.contains("[0.1.1]"), "changelog missing version: {cl}");
    assert!(
        cl.contains("sparkly new feature"),
        "changelog missing feat description: {cl}"
    );
}

// ---------------------------------------------------------------------------
// Inference must honor each crate's `tag_template` (cfgd-style monorepos
// have crates whose tag is bare `v{{ Version }}` — not `<name>-v…`).
// ---------------------------------------------------------------------------

#[test]
fn inference_respects_tag_template_from_anodize_yaml() {
    let tmp = TempDir::new().unwrap();
    two_crate_workspace(tmp.path());
    // .anodize.yaml gives `core` a custom `core-v` prefix and `cli` the bare
    // `v` prefix (the cfgd top-crate convention). Without the tag-template
    // wiring, bump's inference would fall back to `core-v` and `cli-v` and
    // miss the actual tags below.
    fs::write(
        tmp.path().join(".anodize.yaml"),
        r#"version: 2
project_name: tag-template-fixture
crates:
  - name: core
    path: crates/core
    tag_template: "core-v{{ Version }}"
  - name: cli
    path: crates/cli
    tag_template: "v{{ Version }}"
"#,
    )
    .unwrap();
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");
    // Tag with the templates' actual prefixes — `core-v` and bare `v`.
    run_git(tmp.path(), &["tag", "core-v0.1.0"]);
    run_git(tmp.path(), &["tag", "v0.1.0"]);
    // After-tag commits: core gets a feat (→ minor), cli gets a chore (→ skip).
    git_commit_empty_on_path(
        tmp.path(),
        "crates/core/feature.rs",
        "pub fn f() {}",
        "feat(core): add feature",
    );
    git_commit_empty_on_path(
        tmp.path(),
        "crates/cli/notes.rs",
        "// notes",
        "chore(cli): housekeeping",
    );

    let out = anodize()
        .current_dir(tmp.path())
        .args(["bump", "--workspace", "--dry-run", "--output", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "infer dry-run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    let by_name: std::collections::HashMap<&str, &serde_json::Value> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["crate"].as_str().unwrap(), r))
        .collect();
    // `cli`'s tag prefix is bare `v` per .anodize.yaml; the v0.1.0 tag must
    // be discovered and the only post-tag commit is a chore → skip. With
    // the fallback `<name>-v` prefix this row would scan all-of-history,
    // see the chore, classify it as skip ANYWAY, but the reason string would
    // mention "no commits since cli-v" instead of `since v0.1.0`. Pin to
    // the reason text so the regression cannot hide.
    assert_eq!(
        by_name["cli"]["level"], "skip",
        "cli should skip — no feat/fix since v0.1.0"
    );
    let cli_reason = by_name["cli"]["reason"].as_str().unwrap();
    assert!(
        cli_reason.contains("since v0.1.0"),
        "cli reason must reference the actual v0.1.0 tag, not <name>-v fallback: {cli_reason}"
    );
    // core uses `core-v` template; the post-tag feat → minor, reason cites
    // core-v0.1.0 specifically.
    assert_eq!(by_name["core"]["level"], "minor");
    assert_eq!(by_name["core"]["next"], "0.2.0");
    let core_reason = by_name["core"]["reason"].as_str().unwrap();
    assert!(
        core_reason.contains("since core-v0.1.0"),
        "core reason must reference core-v0.1.0: {core_reason}"
    );
}

// ---------------------------------------------------------------------------
// Strict-mode version-pin enforcement
// ---------------------------------------------------------------------------

/// Two-crate workspace with `core` pinned to `0.1.0` in `.anodize.yaml`. The
/// `cli` crate is unpinned.
fn pinned_two_crate_workspace(tmp: &Path, pin_core: bool) {
    two_crate_workspace(tmp);
    let core_pin = if pin_core {
        "    version: \"0.1.0\"\n"
    } else {
        ""
    };
    let yaml = format!(
        r#"version: 2
project_name: pinned
crates:
  - name: core
    path: crates/core
    tag_template: "v{{{{ Version }}}}"
{core_pin}  - name: cli
    path: crates/cli
    tag_template: "v{{{{ Version }}}}"
"#,
        core_pin = core_pin,
    );
    fs::write(tmp.join(".anodize.yaml"), yaml).unwrap();
}

#[test]
fn strict_refuses_bump_when_version_pinned() {
    let tmp = TempDir::new().unwrap();
    pinned_two_crate_workspace(tmp.path(), true);
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");

    let out = anodize()
        .current_dir(tmp.path())
        .args(["--strict", "bump", "minor", "-p", "core", "-y"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "strict bump should fail when version is pinned; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("core"), "error must mention crate name: {err}");
    assert!(
        err.contains("0.1.0"),
        "error must mention pinned version: {err}"
    );
    assert!(
        err.contains("0.2.0"),
        "error must mention proposed version: {err}"
    );
    // Manifest must NOT have been touched.
    assert_eq!(
        read_version(&tmp.path().join("crates/core/Cargo.toml")),
        "0.1.0"
    );
}

#[test]
fn strict_allows_bump_when_no_pin() {
    let tmp = TempDir::new().unwrap();
    pinned_two_crate_workspace(tmp.path(), false);
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");

    let out = anodize()
        .current_dir(tmp.path())
        .args(["--strict", "bump", "minor", "-p", "core", "-y"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "strict bump should succeed without a pin; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        read_version(&tmp.path().join("crates/core/Cargo.toml")),
        "0.2.0"
    );
}

#[test]
fn non_strict_warns_but_proceeds() {
    let tmp = TempDir::new().unwrap();
    pinned_two_crate_workspace(tmp.path(), true);
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");

    let out = anodize()
        .current_dir(tmp.path())
        .args(["bump", "minor", "-p", "core", "-y"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "non-strict bump should proceed despite pin; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.to_lowercase().contains("warn"),
        "stderr must include a warning when bumping a pinned crate: {err}"
    );
    assert_eq!(
        read_version(&tmp.path().join("crates/core/Cargo.toml")),
        "0.2.0"
    );
}
