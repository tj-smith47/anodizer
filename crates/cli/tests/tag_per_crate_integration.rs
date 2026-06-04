//! Integration tests for `anodizer tag` per-crate / hybrid-workspace mode.
//!
//! Exercises the `RepoShape::PerCrate` dispatch path: a multi-crate
//! anodizer config (flat `crates:` with >1 entry, or `workspaces:` with
//! multiple groups) with no `[workspace.package].version` → change
//! detection picks which crates to bump, one commit covers all bumps,
//! one tag per crate, push is atomic. Output: two `anodizer-output`
//! lines (`crates=[...]`, `versions={...}`).

use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

mod common;
use common::run_git;

fn anodizer() -> Command {
    Command::new(env!("CARGO_BIN_EXE_anodizer"))
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

fn git_tag_exists(dir: &Path, tag: &str) -> bool {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["tag", "-l", tag])
        .output()
        .unwrap();
    !String::from_utf8_lossy(&out.stdout).trim().is_empty()
}

fn git_head_sha(dir: &Path) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Two flat crates with per-crate `tag_template` — `crates:` style.
/// No `[workspace.package].version`; each crate carries its own version.
fn flat_two_crate_workspace(tmp: &Path) {
    fs::write(
        tmp.join("Cargo.toml"),
        r#"[workspace]
members = ["crates/core", "crates/cli"]
resolver = "2"
"#,
    )
    .unwrap();
    fs::create_dir_all(tmp.join("crates/core/src")).unwrap();
    fs::create_dir_all(tmp.join("crates/cli/src")).unwrap();
    fs::write(
        tmp.join("crates/core/Cargo.toml"),
        r#"[package]
name = "core"
version = "0.1.0"
edition = "2024"
"#,
    )
    .unwrap();
    fs::write(tmp.join("crates/core/src/lib.rs"), "").unwrap();
    fs::write(
        tmp.join("crates/cli/Cargo.toml"),
        r#"[package]
name = "cli"
version = "0.1.0"
edition = "2024"
"#,
    )
    .unwrap();
    fs::write(tmp.join("crates/cli/src/lib.rs"), "").unwrap();
    fs::write(
        tmp.join(".anodizer.yaml"),
        r#"project_name: myproj
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
}

/// Hybrid `workspaces:` config with two groups: a singleton (`group-a` →
/// `core`) and a lockstep pair (`group-b` → `bin-a` + `bin-b`).
fn hybrid_workspaces(tmp: &Path) {
    fs::write(
        tmp.join("Cargo.toml"),
        r#"[workspace]
members = ["crates/core", "crates/bin-a", "crates/bin-b"]
resolver = "2"
"#,
    )
    .unwrap();
    for (name, path) in [
        ("core", "crates/core"),
        ("bin-a", "crates/bin-a"),
        ("bin-b", "crates/bin-b"),
    ] {
        fs::create_dir_all(tmp.join(path).join("src")).unwrap();
        fs::write(
            tmp.join(path).join("Cargo.toml"),
            format!(
                "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
                name
            ),
        )
        .unwrap();
        fs::write(tmp.join(path).join("src/lib.rs"), "").unwrap();
    }
    fs::write(
        tmp.join(".anodizer.yaml"),
        r#"project_name: hybrid
workspaces:
  - name: group-a
    crates:
      - name: core
        path: crates/core
        tag_template: "core-v{{ .Version }}"
        version_sync:
          enabled: true
  - name: group-b
    crates:
      - name: bin-a
        path: crates/bin-a
        tag_template: "bin-a-v{{ .Version }}"
        version_sync:
          enabled: true
      - name: bin-b
        path: crates/bin-b
        tag_template: "bin-b-v{{ .Version }}"
        version_sync:
          enabled: true
"#,
    )
    .unwrap();
}

fn read_crate_version(root: &Path, crate_path: &str) -> String {
    let text = fs::read_to_string(root.join(crate_path).join("Cargo.toml")).unwrap();
    let doc = text.parse::<toml_edit::DocumentMut>().unwrap();
    doc.get("package")
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string()
}

#[test]
fn per_crate_single_crate_change_tags_one_crate_only() {
    // Flat two-crate config: a commit touching only crates/core should
    // produce one tag (core-v0.1.1) and emit `crates=["core"]`.
    let tmp = TempDir::new().unwrap();
    flat_two_crate_workspace(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");
    // Baseline tags for both crates so change detection has a tag to diff against.
    run_git(tmp.path(), &["tag", "core-v0.1.0"]);
    run_git(tmp.path(), &["tag", "cli-v0.1.0"]);

    // Touch only crates/core.
    fs::write(tmp.path().join("crates/core/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(tmp.path(), "fix: core bug");

    let out = anodizer()
        .current_dir(tmp.path())
        .args(["tag"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "tag failed: stdout={stdout} stderr={stderr}"
    );

    assert!(
        stdout.contains("anodizer-output crates=[\"core\"]"),
        "expected crates=[\"core\"] in stdout: {stdout}"
    );
    assert!(
        stdout.contains("anodizer-output versions={\"core\":\"0.1.1\"}"),
        "expected versions={{\"core\":\"0.1.1\"}} in stdout: {stdout}"
    );
    assert!(
        git_tag_exists(tmp.path(), "core-v0.1.1"),
        "core-v0.1.1 should be created"
    );
    assert!(
        !git_tag_exists(tmp.path(), "cli-v0.1.1"),
        "cli should NOT be tagged (no changes)"
    );
    // Cargo.toml of the bumped crate.
    assert_eq!(read_crate_version(tmp.path(), "crates/core"), "0.1.1");
    // Unbumped crate stays.
    assert_eq!(read_crate_version(tmp.path(), "crates/cli"), "0.1.0");
}

#[test]
fn per_crate_multi_crate_change_tags_all_changed() {
    // Both crates touched → both tagged, both in `crates=` and `versions=`.
    let tmp = TempDir::new().unwrap();
    flat_two_crate_workspace(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");
    run_git(tmp.path(), &["tag", "core-v0.1.0"]);
    run_git(tmp.path(), &["tag", "cli-v0.1.0"]);

    fs::write(
        tmp.path().join("crates/core/src/lib.rs"),
        "// core touched\n",
    )
    .unwrap();
    fs::write(tmp.path().join("crates/cli/src/lib.rs"), "// cli touched\n").unwrap();
    git_add_commit(tmp.path(), "feat: both crates updated");

    let out = anodizer()
        .current_dir(tmp.path())
        .args(["tag"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "tag failed: stdout={stdout} stderr={stderr}"
    );

    // crates= must include both (order is config order: core then cli).
    assert!(
        stdout.contains("anodizer-output crates="),
        "expected crates= line: {stdout}"
    );
    for name in &["\"core\"", "\"cli\""] {
        assert!(
            stdout.contains(name),
            "crates= should contain {name}: {stdout}"
        );
    }
    // versions= JSON must mention both crate→version arrows.
    assert!(
        stdout.contains("anodizer-output versions="),
        "expected versions= line: {stdout}"
    );
    for pair in &["\"core\":\"0.2.0\"", "\"cli\":\"0.2.0\""] {
        assert!(
            stdout.contains(pair),
            "versions= should contain {pair}: {stdout}"
        );
    }

    assert!(git_tag_exists(tmp.path(), "core-v0.2.0"));
    assert!(git_tag_exists(tmp.path(), "cli-v0.2.0"));
}

#[test]
fn per_crate_zero_change_emits_empty_and_no_commit_no_push_exit_zero() {
    // No commits since baseline tags → run_per_crate_tag's early-return:
    // `crates=[]`, `versions={}`, no new commit, exit 0.
    let tmp = TempDir::new().unwrap();
    flat_two_crate_workspace(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");
    run_git(tmp.path(), &["tag", "core-v0.1.0"]);
    run_git(tmp.path(), &["tag", "cli-v0.1.0"]);

    let head_before = git_head_sha(tmp.path());

    let out = anodizer()
        .current_dir(tmp.path())
        .args(["tag"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "zero-change tag must exit 0: stdout={stdout} stderr={stderr}"
    );
    assert!(
        stdout.contains("anodizer-output crates=[]"),
        "expected crates=[] in stdout: {stdout}"
    );
    assert!(
        stdout.contains("anodizer-output versions={}"),
        "expected versions={{}} in stdout: {stdout}"
    );

    let head_after = git_head_sha(tmp.path());
    assert_eq!(
        head_before, head_after,
        "zero-change run must NOT create a new commit"
    );
    // No new tags either.
    assert!(!git_tag_exists(tmp.path(), "core-v0.1.1"));
    assert!(!git_tag_exists(tmp.path(), "cli-v0.1.1"));
}

#[test]
fn per_crate_hybrid_workspaces_bumps_each_group_independently() {
    // Hybrid `workspaces:` layout: group-a is a singleton (core only),
    // group-b is a lockstep pair (bin-a + bin-b). Touching one crate
    // from group-b should bump BOTH bin-a and bin-b together (lockstep
    // group propagation), while core stays untouched.
    let tmp = TempDir::new().unwrap();
    hybrid_workspaces(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");
    run_git(tmp.path(), &["tag", "core-v0.1.0"]);
    run_git(tmp.path(), &["tag", "bin-a-v0.1.0"]);
    run_git(tmp.path(), &["tag", "bin-b-v0.1.0"]);

    fs::write(tmp.path().join("crates/bin-a/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(tmp.path(), "fix: bin-a regression");

    let out = anodizer()
        .current_dir(tmp.path())
        .args(["tag"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "hybrid tag failed: stdout={stdout} stderr={stderr}"
    );

    // Both bin-a AND bin-b must be tagged (lockstep group).
    assert!(git_tag_exists(tmp.path(), "bin-a-v0.1.1"));
    assert!(git_tag_exists(tmp.path(), "bin-b-v0.1.1"));
    // core stays untouched.
    assert!(!git_tag_exists(tmp.path(), "core-v0.1.1"));

    // Output lines should mention both bin-a + bin-b.
    assert!(
        stdout.contains("\"bin-a\""),
        "crates= missing bin-a: {stdout}"
    );
    assert!(
        stdout.contains("\"bin-b\""),
        "crates= missing bin-b: {stdout}"
    );
    assert!(
        !stdout.contains("\"core\""),
        "crates= should omit core: {stdout}"
    );
    assert!(
        stdout.contains("\"bin-a\":\"0.1.1\""),
        "versions= missing bin-a: {stdout}"
    );
    assert!(
        stdout.contains("\"bin-b\":\"0.1.1\""),
        "versions= missing bin-b: {stdout}"
    );
}

#[test]
fn per_crate_custom_tag_errors_in_per_crate_mode() {
    // --custom-tag at the workspace level with no --crate is incompatible
    // with per-crate dispatch — should error (not silently discard).
    let tmp = TempDir::new().unwrap();
    flat_two_crate_workspace(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");

    let out = anodizer()
        .current_dir(tmp.path())
        .args(["tag", "--custom-tag", "v9.9.9"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "expected error exit, got success: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--custom-tag") && stderr.contains("per-crate"),
        "expected per-crate custom-tag error in stderr: {stderr}"
    );
}

#[test]
fn per_crate_dry_run_emits_output_but_no_tags_no_commits() {
    // --dry-run on a real change must still emit the structured output
    // lines so CI can observe the would-be tag set, but NEVER touch the
    // repo (no new commits, no new tags).
    let tmp = TempDir::new().unwrap();
    flat_two_crate_workspace(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");
    run_git(tmp.path(), &["tag", "core-v0.1.0"]);
    run_git(tmp.path(), &["tag", "cli-v0.1.0"]);

    fs::write(tmp.path().join("crates/core/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(tmp.path(), "fix: core change");

    let head_before = git_head_sha(tmp.path());

    let out = anodizer()
        .current_dir(tmp.path())
        .args(["tag", "--dry-run"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    assert!(
        stdout.contains("anodizer-output crates=[\"core\"]"),
        "dry-run must still emit crates=: {stdout}"
    );
    assert!(
        stdout.contains("anodizer-output versions={\"core\":\"0.1.1\"}"),
        "dry-run must still emit versions=: {stdout}"
    );

    let head_after = git_head_sha(tmp.path());
    assert_eq!(head_before, head_after, "--dry-run must not create commits");
    assert!(
        !git_tag_exists(tmp.path(), "core-v0.1.1"),
        "--dry-run must not create tags"
    );
}

fn read_dep_version(root: &Path, manifest_rel: &str, dep_name: &str) -> String {
    let text = fs::read_to_string(root.join(manifest_rel)).unwrap();
    let doc = text.parse::<toml_edit::DocumentMut>().unwrap();
    doc.get("dependencies")
        .and_then(|d| d.get(dep_name))
        .and_then(|d| d.get("version"))
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| {
            panic!(
                "{}: [dependencies].{}.version not found",
                manifest_rel, dep_name
            )
        })
        .to_string()
}

/// A workspace member that pins a sibling via `{ path = "...", version = "X" }`
/// must have THAT version pin rewritten when the sibling is lockstep-bumped
/// during a per-crate tag run. Without this, `cargo publish -p <sibling>`
/// later fails with "failed to select a version for the requirement
/// <sibling> = ^<old>" because the workspace resolves against the
/// pre-bump pin while the sibling's `[package].version` is already at
/// the new value. Regression for cfgd's v0.4.0 ship failure.
#[test]
fn per_crate_lockstep_group_rewrites_intra_workspace_dep_pins() {
    let tmp = TempDir::new().unwrap();

    // Two-member workspace, lockstep group. `app` depends on `lib` via
    // `{ path = "../lib", version = "0.1.0" }`.
    fs::write(
        tmp.path().join("Cargo.toml"),
        r#"[workspace]
members = ["crates/lib", "crates/app"]
resolver = "2"
"#,
    )
    .unwrap();
    fs::create_dir_all(tmp.path().join("crates/lib/src")).unwrap();
    fs::create_dir_all(tmp.path().join("crates/app/src")).unwrap();
    fs::write(
        tmp.path().join("crates/lib/Cargo.toml"),
        r#"[package]
name = "lib"
version = "0.1.0"
edition = "2024"
"#,
    )
    .unwrap();
    fs::write(tmp.path().join("crates/lib/src/lib.rs"), "").unwrap();
    fs::write(
        tmp.path().join("crates/app/Cargo.toml"),
        r#"[package]
name = "app"
version = "0.1.0"
edition = "2024"

[dependencies]
lib = { path = "../lib", version = "0.1.0" }
"#,
    )
    .unwrap();
    fs::write(tmp.path().join("crates/app/src/lib.rs"), "").unwrap();
    fs::write(
        tmp.path().join(".anodizer.yaml"),
        r#"project_name: myproj
workspaces:
  - name: group-all
    crates:
      - name: lib
        path: crates/lib
        tag_template: "lib-v{{ .Version }}"
        version_sync:
          enabled: true
      - name: app
        path: crates/app
        tag_template: "app-v{{ .Version }}"
        version_sync:
          enabled: true
"#,
    )
    .unwrap();
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");
    run_git(tmp.path(), &["tag", "lib-v0.1.0"]);
    run_git(tmp.path(), &["tag", "app-v0.1.0"]);

    // Trigger a lockstep bump by touching the lib crate.
    fs::write(tmp.path().join("crates/lib/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(tmp.path(), "feat: lib feature");

    let out = anodizer()
        .current_dir(tmp.path())
        .args(["tag"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "tag failed: stdout={stdout} stderr={stderr}"
    );

    // Both members in the lockstep group were bumped to 0.2.0...
    assert_eq!(read_crate_version(tmp.path(), "crates/lib"), "0.2.0");
    assert_eq!(read_crate_version(tmp.path(), "crates/app"), "0.2.0");
    // ...and the intra-workspace pin in app's [dependencies] also moved.
    assert_eq!(
        read_dep_version(tmp.path(), "crates/app/Cargo.toml", "lib"),
        "0.2.0",
        "intra-workspace dep pin app→lib must be rewritten to the new version"
    );
}

#[test]
fn per_crate_no_output_when_push_fails() {
    // The `anodizer-output crates=…` / `versions=…` lines advertise a
    // successful tag+push to a downstream consumer. They must be emitted
    // only AFTER the atomic push succeeds — never before it. Point `origin`
    // at an unreachable URL and run under `--strict` so the atomic push hard-
    // fails; the command must exit non-zero AND emit no `anodizer-output`
    // line (a pre-push emission would advertise tags that never landed).
    let tmp = TempDir::new().unwrap();
    flat_two_crate_workspace(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");
    run_git(tmp.path(), &["tag", "core-v0.1.0"]);
    run_git(tmp.path(), &["tag", "cli-v0.1.0"]);
    // Unreachable remote so the push leg fails rather than being skipped:
    // no-remote is a soft skip, so a hard push failure requires a
    // configured-but-unreachable remote.
    run_git(
        tmp.path(),
        &[
            "remote",
            "add",
            "origin",
            "file:///nonexistent/anodizer-test-bare-repo.git",
        ],
    );

    // Touch only crates/core so there is exactly one crate to tag.
    fs::write(tmp.path().join("crates/core/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(tmp.path(), "fix: core bug");

    let out = anodizer()
        .current_dir(tmp.path())
        .args(["--strict", "tag"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        !out.status.success(),
        "tag must fail when the atomic push to a bad remote fails: \
         stdout={stdout} stderr={stderr}"
    );
    assert!(
        !stdout.contains("anodizer-output"),
        "no anodizer-output line may be emitted when the push fails \
         (would advertise tags that never landed): stdout={stdout}"
    );
}

/// Per-crate change detection must scope each crate's `git diff -- <path>`
/// against the workspace root, not the cwd. Invoked from a member directory
/// with the root config, a commit touching `crates/core` must still be
/// detected as a `core` change (and `cli` left untouched) — before the
/// root-aware fix, the `crates/core` pathspec was resolved relative to the
/// `crates/cli` cwd, found nothing, and reported zero changed crates.
#[test]
fn per_crate_change_detection_from_subdir_scopes_to_workspace_root() {
    let tmp = TempDir::new().unwrap();
    flat_two_crate_workspace(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");
    run_git(tmp.path(), &["tag", "core-v0.1.0"]);
    run_git(tmp.path(), &["tag", "cli-v0.1.0"]);
    // Touch only crates/core.
    fs::write(tmp.path().join("crates/core/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(tmp.path(), "fix: core bug");

    let config = tmp.path().join(".anodizer.yaml");
    let out = anodizer()
        .current_dir(tmp.path().join("crates/cli"))
        .args([
            "tag",
            "--dry-run",
            "--config",
            config.to_str().expect("utf8 config path"),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "tag --dry-run from subdir failed: stdout={stdout} stderr={stderr}"
    );
    assert!(
        stdout.contains("anodizer-output crates=[\"core\"]"),
        "from a subdir, the core change must still be detected \
         (pathspec scoped to the workspace root): stdout={stdout}"
    );
    assert!(
        stdout.contains("anodizer-output versions={\"core\":\"0.1.1\"}"),
        "expected versions={{\"core\":\"0.1.1\"}} from subdir: {stdout}"
    );
    // Dry-run must not create tags.
    assert!(
        !git_tag_exists(tmp.path(), "core-v0.1.1"),
        "dry-run must not create core-v0.1.1"
    );
}
