//! Integration tests for the unified `anodizer changelog` command.
//!
//! `anodizer changelog` refreshes the pending `## [Unreleased]` section
//! (`--format keep-a-changelog`, default), emits GitHub-body notes
//! (`--format release-notes`), or a JSON array (`--format json`). The refresh
//! path must work across all three config modes:
//!   1. single-crate (`crates:` with one entry + `version_sync`),
//!   2. workspace-lockstep (`[workspace.package].version`),
//!   3. workspace per-crate (flat `crates:` with independent versions).
//!
//! Also covers the positional range parsing (omitted / `a..b` / single `<tag>`),
//! the `--write` + non-kac error, the preview-extracts-only-the-section
//! contract, and `--crate` filtering.

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
        .unwrap_or_else(|e| panic!("git {args:?} failed to spawn: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
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

fn read(dir: &Path, rel: &str) -> String {
    fs::read_to_string(dir.join(rel)).unwrap()
}

struct RunResult {
    stdout: String,
    stderr: String,
    success: bool,
}

fn changelog(dir: &Path, args: &[&str]) -> RunResult {
    let out = anodizer()
        .current_dir(dir)
        .arg("changelog")
        .args(args)
        .output()
        .unwrap();
    RunResult {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        success: out.status.success(),
    }
}

// ---------------------------------------------------------------------------
// Mode 1: single-crate refresh + write
// ---------------------------------------------------------------------------

fn single_crate_repo() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/app\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("crates/app/src")).unwrap();
    fs::write(
        root.join("crates/app/Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(root.join("crates/app/src/lib.rs"), "").unwrap();
    fs::write(
        root.join(".anodizer.yaml"),
        r#"project_name: single
changelog: {}
crates:
  - name: app
    path: crates/app
    tag_template: "v{{ .Version }}"
    version_sync:
      enabled: true
"#,
    )
    .unwrap();
    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "v0.1.0"]);
    fs::write(root.join("crates/app/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(root, "feat: add a thing");
    tmp
}

#[test]
fn single_crate_preview_shows_unreleased_only() {
    let tmp = single_crate_repo();
    let root = tmp.path();
    let r = changelog(root, &["-q"]);
    assert!(r.success, "preview failed: {}\n{}", r.stdout, r.stderr);
    assert!(
        r.stdout.contains("Unreleased"),
        "preview must show the [Unreleased] heading: {}",
        r.stdout
    );
    assert!(
        r.stdout.contains("add a thing"),
        "preview must show the new commit: {}",
        r.stdout
    );
    // Preview does not write the file.
    assert!(
        !root.join("CHANGELOG.md").exists(),
        "preview must not write CHANGELOG.md"
    );
}

#[test]
fn single_crate_write_refreshes_file() {
    let tmp = single_crate_repo();
    let root = tmp.path();
    let r = changelog(root, &["-q", "--write"]);
    assert!(r.success, "write failed: {}\n{}", r.stdout, r.stderr);
    // Bare `changelog: {}` routes to the workspace-root CHANGELOG.md.
    let cl = read(root, "CHANGELOG.md");
    assert!(cl.contains("Unreleased"), "expected [Unreleased]: {cl}");
    assert!(cl.contains("add a thing"), "expected the commit: {cl}");
    // No commit was made: the write is a working-tree edit only.
    let status = Command::new("git")
        .current_dir(root)
        .args(["status", "--porcelain", "CHANGELOG.md"])
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&status.stdout);
    assert!(
        out.contains("CHANGELOG.md"),
        "CHANGELOG.md must be an uncommitted working-tree edit, status: {out:?}"
    );
}

#[test]
fn single_crate_write_preserves_released_history() {
    let tmp = single_crate_repo();
    let root = tmp.path();
    // Seed a released section + footer that the refresh must preserve.
    fs::write(
        root.join("CHANGELOG.md"),
        "# Changelog\n\n## [Unreleased]\n\n## [0.1.0] - 2026-01-01\n\n- first release\n\n[Unreleased]: http://x/compare/v0.1.0...HEAD\n",
    )
    .unwrap();
    let r = changelog(root, &["-q", "--write"]);
    assert!(r.success, "write failed: {}\n{}", r.stdout, r.stderr);
    let cl = read(root, "CHANGELOG.md");
    assert!(cl.contains("## [0.1.0]"), "released history dropped: {cl}");
    assert!(cl.contains("first release"), "released body dropped: {cl}");
    assert!(cl.contains("add a thing"), "new commit missing: {cl}");
    assert!(cl.contains("compare/v0.1.0"), "footer dropped: {cl}");
}

// ---------------------------------------------------------------------------
// Mode 2: workspace-lockstep
// ---------------------------------------------------------------------------

#[test]
fn lockstep_write_refreshes_root_changelog() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/core\", \"crates/cli\"]\nresolver = \"2\"\n\n[workspace.package]\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    for name in ["core", "cli"] {
        fs::create_dir_all(root.join(format!("crates/{name}/src"))).unwrap();
        fs::write(
            root.join(format!("crates/{name}/Cargo.toml")),
            format!("[package]\nname = \"{name}\"\nversion.workspace = true\nedition = \"2024\"\n"),
        )
        .unwrap();
        fs::write(root.join(format!("crates/{name}/src/lib.rs")), "").unwrap();
    }
    fs::write(
        root.join(".anodizer.yaml"),
        "project_name: lockstep\nchangelog: {}\n",
    )
    .unwrap();
    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "v0.1.0"]);
    fs::write(root.join("crates/core/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(root, "feat: lockstep change");

    let r = changelog(root, &["-q", "--write"]);
    assert!(
        r.success,
        "lockstep write failed: {}\n{}",
        r.stdout, r.stderr
    );
    let cl = read(root, "CHANGELOG.md");
    assert!(cl.contains("Unreleased"), "expected [Unreleased]: {cl}");
    assert!(cl.contains("lockstep change"), "expected the commit: {cl}");
    // One aggregate root file; no per-crate files for a bare changelog config.
    assert!(
        !root.join("crates/core/CHANGELOG.md").exists(),
        "lockstep refresh must not write per-crate files"
    );
}

// ---------------------------------------------------------------------------
// Mode 3: workspace per-crate
// ---------------------------------------------------------------------------

fn per_crate_repo() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/core\", \"crates/cli\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    for (name, ver) in [("core", "0.1.0"), ("cli", "0.2.0")] {
        fs::create_dir_all(root.join(format!("crates/{name}/src"))).unwrap();
        fs::write(
            root.join(format!("crates/{name}/Cargo.toml")),
            format!("[package]\nname = \"{name}\"\nversion = \"{ver}\"\nedition = \"2024\"\n"),
        )
        .unwrap();
        fs::write(root.join(format!("crates/{name}/src/lib.rs")), "").unwrap();
    }
    fs::write(
        root.join(".anodizer.yaml"),
        r#"project_name: percrate
changelog:
  per_crate: true
crates:
  - name: core
    path: crates/core
    tag_template: "core-v{{ .Version }}"
    version_sync:
      enabled: true
  - name: cli
    path: crates/cli
    tag_template: "cli-v{{ .Version }}"
    version_sync:
      enabled: true
"#,
    )
    .unwrap();
    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "core-v0.1.0"]);
    run_git(root, &["tag", "cli-v0.2.0"]);
    fs::write(root.join("crates/core/src/lib.rs"), "// core touched\n").unwrap();
    git_add_commit(root, "feat: core change");
    fs::write(root.join("crates/cli/src/lib.rs"), "// cli touched\n").unwrap();
    git_add_commit(root, "fix: cli change");
    tmp
}

#[test]
fn per_crate_write_refreshes_each_crate_file() {
    let tmp = per_crate_repo();
    let root = tmp.path();
    let r = changelog(root, &["-q", "--write"]);
    assert!(
        r.success,
        "per-crate write failed: {}\n{}",
        r.stdout, r.stderr
    );
    let core = read(root, "crates/core/CHANGELOG.md");
    let cli = read(root, "crates/cli/CHANGELOG.md");
    assert!(core.contains("core change"), "core section missing: {core}");
    assert!(cli.contains("cli change"), "cli section missing: {cli}");
    // Each crate's range is bounded by ITS own tag, so the other crate's
    // commit must not bleed in.
    assert!(
        !core.contains("cli change"),
        "cli commit leaked into core: {core}"
    );
    assert!(
        !cli.contains("core change"),
        "core commit leaked into cli: {cli}"
    );
}

#[test]
fn per_crate_preview_separates_multiple_targets() {
    let tmp = per_crate_repo();
    let root = tmp.path();
    let r = changelog(root, &["-q"]);
    assert!(r.success, "preview failed: {}\n{}", r.stdout, r.stderr);
    // Two per-crate files → attributable `--- <path> ---` separators.
    assert!(
        r.stdout.contains("--- crates/core/CHANGELOG.md ---"),
        "missing core separator: {}",
        r.stdout
    );
    assert!(
        r.stdout.contains("--- crates/cli/CHANGELOG.md ---"),
        "missing cli separator: {}",
        r.stdout
    );
}

#[test]
fn per_crate_filter_restricts_to_one_crate() {
    let tmp = per_crate_repo();
    let root = tmp.path();
    let r = changelog(root, &["-q", "--write", "--crate", "core"]);
    assert!(
        r.success,
        "filtered write failed: {}\n{}",
        r.stdout, r.stderr
    );
    assert!(
        root.join("crates/core/CHANGELOG.md").exists(),
        "--crate core must refresh core"
    );
    assert!(
        !root.join("crates/cli/CHANGELOG.md").exists(),
        "--crate core must not touch cli"
    );
}

// ---------------------------------------------------------------------------
// Range parsing: single tag resolves crate + predecessor
// ---------------------------------------------------------------------------

#[test]
fn single_tag_resolves_owning_crate_and_predecessor() {
    let tmp = per_crate_repo();
    let root = tmp.path();
    // Add an older core tag so the predecessor of core-v0.3.0 is core-v0.2.0.
    run_git(root, &["tag", "core-v0.2.0"]);
    fs::write(root.join("crates/core/src/lib.rs"), "// core 0.3\n").unwrap();
    git_add_commit(root, "feat: core toward 0.3");
    run_git(root, &["tag", "core-v0.3.0"]);

    // `changelog core-v0.3.0 --format json` targets ONLY core, range
    // core-v0.2.0..core-v0.3.0.
    let r = changelog(root, &["-q", "core-v0.3.0", "--format", "json"]);
    assert!(
        r.success,
        "single-tag json failed: {}\n{}",
        r.stdout, r.stderr
    );
    let v: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 1, "single tag pins to one crate: {}", r.stdout);
    assert_eq!(arr[0]["crate"], "core");
    assert_eq!(
        arr[0]["from"], "core-v0.2.0",
        "predecessor wrong: {}",
        r.stdout
    );
    assert_eq!(arr[0]["to"], "core-v0.3.0");
}

// ---------------------------------------------------------------------------
// release-notes format (regression: grouped-bullet body)
// ---------------------------------------------------------------------------

#[test]
fn release_notes_format_emits_grouped_bullets() {
    let tmp = single_crate_repo();
    let root = tmp.path();
    // The release-notes path runs the changelog stage, which requires HEAD to
    // sit at the upper-bound tag (the release-time invariant). Tag the feat
    // commit as v0.2.0 and render the v0.1.0..v0.2.0 range via the single-tag
    // positional.
    run_git(root, &["tag", "v0.2.0"]);
    let r = changelog(root, &["-q", "v0.2.0", "--format", "release-notes"]);
    assert!(
        r.success,
        "release-notes failed: {}\n{}",
        r.stdout, r.stderr
    );
    assert!(
        r.stdout.contains("add a thing"),
        "release notes must list the commit: {}",
        r.stdout
    );
}

// ---------------------------------------------------------------------------
// json format shape
// ---------------------------------------------------------------------------

#[test]
fn json_format_emits_sorted_array_with_crate_field() {
    let tmp = per_crate_repo();
    let root = tmp.path();
    let r = changelog(root, &["-q", "--format", "json"]);
    assert!(r.success, "json failed: {}\n{}", r.stdout, r.stderr);
    let v: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let arr = v.as_array().expect("json output must be an array");
    assert_eq!(arr.len(), 2, "one element per crate: {}", r.stdout);
    // Sorted by crate name: cli before core.
    assert_eq!(arr[0]["crate"], "cli");
    assert_eq!(arr[1]["crate"], "core");
    // Each element carries the documented payload fields.
    for elem in arr {
        assert!(elem.get("crate").is_some());
        assert!(elem.get("to").is_some());
        assert!(elem.get("groups").is_some());
    }
}

// ---------------------------------------------------------------------------
// --write + non-kac format error (end-to-end through clap)
// ---------------------------------------------------------------------------

#[test]
fn write_with_release_notes_format_is_rejected() {
    let tmp = single_crate_repo();
    let root = tmp.path();
    let r = changelog(root, &["-q", "--write", "--format", "release-notes"]);
    assert!(!r.success, "--write + release-notes must fail");
    assert!(
        r.stderr.contains("--write is only valid"),
        "expected the write/format error: {}",
        r.stderr
    );
}

#[test]
fn explicit_range_overrides_auto_discovery() {
    let tmp = single_crate_repo();
    let root = tmp.path();
    // `changelog v0.1.0..HEAD --format json` feeds the exact range.
    let r = changelog(root, &["-q", "v0.1.0..HEAD", "--format", "json"]);
    assert!(r.success, "range json failed: {}\n{}", r.stdout, r.stderr);
    let v: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr[0]["from"], "v0.1.0");
    assert_eq!(arr[0]["to"], "HEAD");
}
