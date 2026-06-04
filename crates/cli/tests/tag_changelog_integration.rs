//! Integration tests for `anodizer tag` refreshing `CHANGELOG.md`.
//!
//! `tag` (the command release CI runs) renders each crate's
//! `## [<version>] - <date>` section from the conventional commits since the
//! crate's previous tag and folds the file into the same version-bump commit —
//! mirroring `bump --commit`. Covered across all three config modes:
//!   1. single-crate (`--crate` + `version_sync`),
//!   2. workspace-lockstep (`[workspace.package].version`),
//!   3. workspace per-crate (flat `crates:` with independent versions).
//!
//! The refresh is opt-in via `--changelog`; without it `tag` never touches a
//! `CHANGELOG.md` even with a `changelog:` block configured. Also covers
//! `--dry-run`, `changelog.skip: true`, and the no-`changelog:` config case.

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

/// The refreshed CHANGELOG.md must be committed, not left as an unstaged
/// working-tree edit. Returns the file content at HEAD.
fn show_head(dir: &Path, rel: &str) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["show", &format!("HEAD:{rel}")])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git show HEAD:{rel} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

// ---------------------------------------------------------------------------
// Mode 1: single-crate (--crate + version_sync)
// ---------------------------------------------------------------------------

#[test]
fn single_crate_refreshes_changelog() {
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

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--crate", "app", "--changelog"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");
    assert!(stdout.contains("new_tag=v0.2.0"), "stdout: {stdout}");

    // A bare `changelog: {}` resolves to the root destination (the deliberate
    // default), so the single-crate section lands in the workspace-root
    // CHANGELOG.md rather than a per-crate file.
    let changelog = read(root, "CHANGELOG.md");
    assert!(
        changelog.contains("## [0.2.0]"),
        "expected a 0.2.0 section: {changelog}"
    );
    assert!(
        changelog.contains("add a thing"),
        "expected the feat commit in the section: {changelog}"
    );
    // Refreshed file is in the bump commit, not just the working tree.
    let head = show_head(root, "CHANGELOG.md");
    assert!(
        head.contains("## [0.2.0]"),
        "expected the section committed: {head}"
    );
}

/// The new default: WITHOUT `--changelog`, `tag` writes no `CHANGELOG.md` even
/// though `changelog:` is configured (the refresh is opt-in).
#[test]
fn single_crate_default_no_flag_suppresses() {
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

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--crate", "app"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");

    assert!(
        !root.join("crates/app/CHANGELOG.md").exists(),
        "without --changelog, tag must not write a per-crate CHANGELOG.md"
    );
    assert!(
        !root.join("CHANGELOG.md").exists(),
        "without --changelog, tag must not write the root CHANGELOG.md"
    );
}

/// `--changelog --dry-run` writes no CHANGELOG.md but still exits 0 (dry-run
/// suppresses the write even with the refresh opted in).
#[test]
fn single_crate_dry_run_writes_nothing() {
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

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--crate", "app", "--changelog", "--dry-run"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");

    assert!(
        !root.join("crates/app/CHANGELOG.md").exists(),
        "--dry-run must not write a CHANGELOG.md"
    );
}

/// No `changelog:` config → even `--changelog` creates no CHANGELOG.md (the
/// flag opts in, but an absent config block has nothing to refresh).
#[test]
fn single_crate_no_changelog_config_is_default_off() {
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
    // Note: NO changelog: block.
    fs::write(
        root.join(".anodizer.yaml"),
        r#"project_name: single
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

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--crate", "app", "--changelog"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");

    assert!(
        !root.join("crates/app/CHANGELOG.md").exists(),
        "without changelog: config, tag must not create a CHANGELOG.md"
    );
}

/// A plain `changelog: { skip: true }` disables the refresh even when
/// `--changelog` opts in (skip wins over the opt-in flag).
#[test]
fn single_crate_changelog_skip_true_disables() {
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
changelog:
  skip: true
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

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--crate", "app", "--changelog"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");

    assert!(
        !root.join("crates/app/CHANGELOG.md").exists(),
        "changelog.skip: true must not write a CHANGELOG.md"
    );
}

// ---------------------------------------------------------------------------
// Mode 2: workspace-lockstep
// ---------------------------------------------------------------------------

#[test]
fn lockstep_refreshes_each_member_changelog() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("Cargo.toml"),
        r#"[workspace]
members = ["crates/a", "crates/b"]
resolver = "2"

[workspace.package]
version = "0.1.0"
"#,
    )
    .unwrap();
    for name in ["a", "b"] {
        fs::create_dir_all(root.join(format!("crates/{name}/src"))).unwrap();
        fs::write(
            root.join(format!("crates/{name}/Cargo.toml")),
            format!("[package]\nname = \"{name}\"\nversion.workspace = true\nedition = \"2024\"\n"),
        )
        .unwrap();
        fs::write(root.join(format!("crates/{name}/src/lib.rs")), "").unwrap();
    }
    // `per_crate: true` drives the per-member CHANGELOG.md files this test
    // asserts; a bare `changelog: {}` would aggregate into the root file.
    fs::write(
        root.join(".anodizer.yaml"),
        "project_name: lockstep\nchangelog:\n  per_crate: true\n",
    )
    .unwrap();

    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "v0.1.0"]);
    fs::write(root.join("crates/a/src/lib.rs"), "// touched a\n").unwrap();
    fs::write(root.join("crates/b/src/lib.rs"), "// touched b\n").unwrap();
    git_add_commit(root, "feat: shared change");

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--changelog"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");
    assert!(stdout.contains("new_tag=v0.2.0"), "stdout: {stdout}");

    for name in ["a", "b"] {
        let rel = format!("crates/{name}/CHANGELOG.md");
        let changelog = read(root, &rel);
        assert!(
            changelog.contains("## [0.2.0]"),
            "member {name}: expected a 0.2.0 section: {changelog}"
        );
        let head = show_head(root, &rel);
        assert!(
            head.contains("## [0.2.0]"),
            "member {name}: expected the section committed: {head}"
        );
    }
}

/// Lockstep `--changelog --dry-run` writes no CHANGELOG.md but still exits 0.
#[test]
fn lockstep_dry_run_writes_nothing() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("Cargo.toml"),
        r#"[workspace]
members = ["crates/a"]
resolver = "2"

[workspace.package]
version = "0.1.0"
"#,
    )
    .unwrap();
    fs::create_dir_all(root.join("crates/a/src")).unwrap();
    fs::write(
        root.join("crates/a/Cargo.toml"),
        "[package]\nname = \"a\"\nversion.workspace = true\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(root.join("crates/a/src/lib.rs"), "").unwrap();
    fs::write(
        root.join(".anodizer.yaml"),
        "project_name: lockstep\nchangelog: {}\n",
    )
    .unwrap();

    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "v0.1.0"]);
    fs::write(root.join("crates/a/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(root, "feat: a change");

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--changelog", "--dry-run"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        !root.join("crates/a/CHANGELOG.md").exists(),
        "--dry-run must not write a CHANGELOG.md"
    );
}

// ---------------------------------------------------------------------------
// Mode 3: workspace per-crate (flat crates: with independent versions)
// ---------------------------------------------------------------------------

#[test]
fn per_crate_refreshes_each_bumped_crate_changelog() {
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
    git_add_commit(root, "feat: core only change");

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--no-push", "--changelog"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");

    // Only core changed → core bumps minor 0.1.0 → 0.2.0 and gets a section
    // keyed to ITS new version; cli is untouched (no commits since its tag).
    let core_changelog = read(root, "crates/core/CHANGELOG.md");
    assert!(
        core_changelog.contains("## [0.2.0]"),
        "core: expected a 0.2.0 section: {core_changelog}"
    );
    assert!(
        core_changelog.contains("core only change"),
        "core: expected its commit in the section: {core_changelog}"
    );
    assert!(
        !root.join("crates/cli/CHANGELOG.md").exists(),
        "cli was not bumped — its CHANGELOG.md must be untouched"
    );
    assert!(
        show_head(root, "crates/core/CHANGELOG.md").contains("## [0.2.0]"),
        "core: expected the section committed"
    );
}

/// Per-crate `changelog: { skip: true }` suppresses the refresh at the
/// per-crate site even with `--changelog` (skip wins over the opt-in flag).
#[test]
fn per_crate_changelog_skip_true_disables() {
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
  skip: true
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
    git_add_commit(root, "feat: core only change");

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--no-push", "--changelog"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");

    assert!(
        !root.join("crates/core/CHANGELOG.md").exists(),
        "changelog.skip: true must not write a CHANGELOG.md at the per-crate site"
    );
}

// ---------------------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------------------

/// First tag (no prior tag → `from_tag == None`): the engine renders from full
/// HEAD history, so a section is still produced and committed. Exercises the
/// `old_tag_str.is_empty()` branch.
#[test]
fn first_tag_renders_from_full_history() {
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
    // No prior tag — the very first commit carries a feat so the initial bump
    // computes minor → 0.2.0 from the initial_version baseline.
    git_add_commit(root, "feat: initial feature");

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--crate", "app", "--changelog"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");

    // Bare `changelog: {}` writes the root CHANGELOG.md (the default destination).
    let changelog = read(root, "CHANGELOG.md");
    assert!(
        changelog.contains("initial feature"),
        "expected the full-history commit in the first section: {changelog}"
    );
    assert!(
        changelog.contains("## ["),
        "expected a version section heading: {changelog}"
    );
    // The section is part of the bump commit.
    assert!(
        show_head(root, "CHANGELOG.md").contains("initial feature"),
        "expected the section committed"
    );
}

/// An existing hand-written `CHANGELOG.md` with an H1 + an older section: the
/// new section is prepended and the H1 plus the old section survive
/// (Replace-mode merge).
#[test]
fn existing_changelog_h1_and_history_survive() {
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
    // Seed a hand-written root CHANGELOG with an H1 and an existing 0.1.0
    // section. A bare `changelog: {}` resolves to the root destination, so the
    // merge happens against this file.
    fs::write(
        root.join("CHANGELOG.md"),
        "# Changelog\n\n## [0.1.0] - 2026-01-01\n- old entry from an earlier release\n",
    )
    .unwrap();
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
    git_add_commit(root, "feat: brand new thing");

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--crate", "app", "--changelog"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");

    let changelog = read(root, "CHANGELOG.md");
    // The existing H1 survives.
    assert!(
        changelog.contains("# Changelog"),
        "the H1 must survive the merge: {changelog}"
    );
    // The new section is present...
    let new_idx = changelog
        .find("## [0.2.0]")
        .unwrap_or_else(|| panic!("expected the new 0.2.0 section: {changelog}"));
    // ...and the old section + its entry survive...
    let old_idx = changelog
        .find("## [0.1.0]")
        .unwrap_or_else(|| panic!("expected the old 0.1.0 section to survive: {changelog}"));
    assert!(
        changelog.contains("old entry from an earlier release"),
        "the old section content must survive: {changelog}"
    );
    // ...with the new section PREPENDED above the old one.
    assert!(
        new_idx < old_idx,
        "the new section must be prepended above the old one: {changelog}"
    );
}

/// Lockstep default (no `--changelog`) suppresses the refresh for every member
/// (proves the opt-in default at the lockstep site, not just single-crate).
#[test]
fn lockstep_default_no_flag_suppresses() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("Cargo.toml"),
        r#"[workspace]
members = ["crates/a", "crates/b"]
resolver = "2"

[workspace.package]
version = "0.1.0"
"#,
    )
    .unwrap();
    for name in ["a", "b"] {
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
    fs::write(root.join("crates/a/src/lib.rs"), "// touched a\n").unwrap();
    fs::write(root.join("crates/b/src/lib.rs"), "// touched b\n").unwrap();
    git_add_commit(root, "feat: shared change");

    let out = anodizer().current_dir(root).args(["tag"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");

    for name in ["a", "b"] {
        assert!(
            !root.join(format!("crates/{name}/CHANGELOG.md")).exists(),
            "without --changelog, tag must not write member {name}'s CHANGELOG.md"
        );
    }
    assert!(
        !root.join("CHANGELOG.md").exists(),
        "without --changelog, tag must not write the root CHANGELOG.md"
    );
}

/// Per-crate default (no `--changelog`) suppresses the refresh for every bumped
/// crate, yet the bump commit and tags still happen (proves the opt-in default
/// at the per-crate site, mirroring `lockstep_default_no_flag_suppresses`).
#[test]
fn per_crate_default_no_flag_suppresses() {
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
changelog: {}
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
    fs::write(root.join("crates/cli/src/lib.rs"), "// cli touched\n").unwrap();
    git_add_commit(root, "feat: both updated");

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--no-push"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");

    for name in ["core", "cli"] {
        assert!(
            !root.join(format!("crates/{name}/CHANGELOG.md")).exists(),
            "without --changelog, tag must not write {name}'s CHANGELOG.md"
        );
    }

    // The bump commit + tags still happened: both new per-crate tags exist.
    for tag in ["core-v0.2.0", "cli-v0.3.0"] {
        let out = Command::new("git")
            .current_dir(root)
            .args(["rev-parse", "--verify", &format!("refs/tags/{tag}")])
            .output()
            .unwrap();
        assert!(out.status.success(), "expected tag {tag} to be created");
    }
}

/// Per-crate `--changelog --dry-run` writes no CHANGELOG.md anywhere and exits 0
/// (mirrors `lockstep_dry_run_writes_nothing`).
#[test]
fn per_crate_dry_run_writes_nothing() {
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
changelog: {}
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
    fs::write(root.join("crates/cli/src/lib.rs"), "// cli touched\n").unwrap();
    git_add_commit(root, "feat: both updated");

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--changelog", "--dry-run"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");

    for name in ["core", "cli"] {
        assert!(
            !root.join(format!("crates/{name}/CHANGELOG.md")).exists(),
            "--dry-run must not write {name}'s CHANGELOG.md"
        );
    }
}

// ---------------------------------------------------------------------------
// version_files + changelog cohesion: both sibling features ride ONE commit
// ---------------------------------------------------------------------------

/// Enrolling BOTH a `version_files` path AND a `changelog:` block at the
/// lockstep site: a single `anodizer tag` must fold the rewritten version_files
/// file AND the new `## [<ver>]` changelog section into the SAME bump commit
/// (proves the two sibling features dedupe into one commit with no clobber).
#[test]
fn version_files_and_changelog_share_one_commit() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("Cargo.toml"),
        r#"[workspace]
members = ["crates/a"]
resolver = "2"

[workspace.package]
version = "0.1.0"
"#,
    )
    .unwrap();
    fs::create_dir_all(root.join("crates/a/src")).unwrap();
    fs::write(
        root.join("crates/a/Cargo.toml"),
        "[package]\nname = \"a\"\nversion.workspace = true\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(root.join("crates/a/src/lib.rs"), "").unwrap();
    // Enroll a version_files path AND a changelog block in one config.
    fs::write(root.join("Chart.yaml"), "appVersion: v0.1.0\n").unwrap();
    fs::write(
        root.join(".anodizer.yaml"),
        "project_name: lockstep\nversion_files:\n  - Chart.yaml\nchangelog: {}\n",
    )
    .unwrap();

    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "v0.1.0"]);
    fs::write(root.join("crates/a/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(root, "fix: a shared change");

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--changelog"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");
    assert!(stdout.contains("new_tag=v0.1.1"), "stdout: {stdout}");

    // The SAME HEAD commit carries both the rewritten version_files file...
    assert_eq!(
        show_head(root, "Chart.yaml"),
        "appVersion: v0.1.1\n",
        "version_files rewrite must be in the bump commit"
    );
    // ...and the new changelog section (bare config → root CHANGELOG.md).
    let head_changelog = show_head(root, "CHANGELOG.md");
    assert!(
        head_changelog.contains("## [0.1.1]"),
        "changelog section must be in the SAME bump commit: {head_changelog}"
    );

    // Both files are named in HEAD's tree diff — one commit, both features.
    let diff = Command::new("git")
        .current_dir(root)
        .args(["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"])
        .output()
        .unwrap();
    let names = String::from_utf8_lossy(&diff.stdout);
    assert!(
        names.lines().any(|l| l == "Chart.yaml"),
        "HEAD commit must touch Chart.yaml: {names}"
    );
    assert!(
        names.lines().any(|l| l == "CHANGELOG.md"),
        "HEAD commit must touch CHANGELOG.md: {names}"
    );
}

/// Write a two-member lockstep workspace, tag it, and return the root
/// `CHANGELOG.md` content at HEAD. `root_block` is spliced under `changelog:`
/// so callers can vary the `root.crates` filter; both members carry a distinct
/// commit so the aggregate section must span more than one crate.
fn lockstep_root_aggregate_fixture(root: &Path, root_block: &str) -> String {
    fs::write(
        root.join("Cargo.toml"),
        r#"[workspace]
members = ["crates/a", "crates/b"]
resolver = "2"

[workspace.package]
version = "0.1.0"
"#,
    )
    .unwrap();
    for name in ["a", "b"] {
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
        format!("project_name: lockstep\nchangelog:\n{root_block}"),
    )
    .unwrap();

    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "v0.1.0"]);
    // Distinct commit per member so the aggregate spans both crates.
    fs::write(root.join("crates/a/src/lib.rs"), "// touched a\n").unwrap();
    git_add_commit(root, "feat: change in crate a");
    fs::write(root.join("crates/b/src/lib.rs"), "// touched b\n").unwrap();
    git_add_commit(root, "fix: change in crate b");

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--changelog"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");
    assert!(stdout.contains("new_tag=v0.2.0"), "stdout: {stdout}");

    show_head(root, "CHANGELOG.md")
}

/// A bare lockstep `changelog: {}` writes ONE aggregate root `CHANGELOG.md`
/// spanning every member: a single `## [0.2.0]` section carrying both members'
/// commits (not a per-crate file each).
#[test]
fn lockstep_root_aggregate_spans_all_members() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let changelog = lockstep_root_aggregate_fixture(root, "  root: {}\n");

    assert!(
        changelog.matches("## [0.2.0]").count() == 1,
        "expected exactly one 0.2.0 section in the aggregate root: {changelog}"
    );
    assert!(
        changelog.contains("change in crate a"),
        "aggregate must include crate a's commit: {changelog}"
    );
    assert!(
        changelog.contains("change in crate b"),
        "aggregate must include crate b's commit: {changelog}"
    );
    // No per-crate files when the destination is root-only.
    assert!(
        !root.join("crates/a/CHANGELOG.md").exists()
            && !root.join("crates/b/CHANGELOG.md").exists(),
        "root-only destination must not write per-crate files"
    );
}

/// Regression guard: a `root.crates` filter naming a NON-first member must NOT
/// drop the lockstep aggregate. The aggregate is a flat whole-release section,
/// so the per-crate filter cannot gate it; before the fix, `root_crates`
/// excluding `members.first()` (`a`) silently dropped the entire root changelog.
#[test]
fn lockstep_root_aggregate_ignores_non_first_member_filter() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    // `b` is provably NOT members.first() (`a`); against the buggy code the
    // aggregate's synthetic crate_name (`a`) fails the `["b"]` filter and the
    // root file is never written.
    let changelog = lockstep_root_aggregate_fixture(root, "  root:\n    crates: [\"b\"]\n");

    assert!(
        root.join("CHANGELOG.md").is_file(),
        "filtered-but-non-subsection aggregate must still write the root file"
    );
    assert!(
        changelog.matches("## [0.2.0]").count() == 1,
        "aggregate section must survive the non-first-member root.crates filter: {changelog}"
    );
    assert!(
        changelog.contains("change in crate a") && changelog.contains("change in crate b"),
        "aggregate still spans every member regardless of root.crates: {changelog}"
    );
}

// ---------------------------------------------------------------------------
// Multi-track per-crate root: subsection-promote + chronology + both + filter
// ---------------------------------------------------------------------------

/// Seed a two-crate per-crate workspace (`core` on the `core-v*` track, `cli` on
/// `cli-v*`) whose root `CHANGELOG.md` carries a curated `### core` and `### cli`
/// subsection under `## [Unreleased]`, plus a `[Unreleased]:` compare footer so
/// the rolled footer base is resolvable without a git remote. `changelog_block`
/// is written verbatim under `.anodizer.yaml`'s top level (so callers vary the
/// `changelog:` destination). `extra_root` is spliced into the seeded root file
/// between the curated `[Unreleased]` block and its footer (used to seed an
/// existing released section for the `chronology: tag` ordering test); pass `""`
/// for none. Only `core` is touched with a commit, so a `core` tag promotes the
/// `### core` subsection and leaves `### cli` untouched.
fn multitrack_root_fixture(root: &Path, changelog_block: &str, extra_root: &str) {
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/core\", \"crates/cli\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    for name in ["core", "cli"] {
        fs::create_dir_all(root.join(format!("crates/{name}/src"))).unwrap();
        fs::write(
            root.join(format!("crates/{name}/Cargo.toml")),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
        )
        .unwrap();
        fs::write(root.join(format!("crates/{name}/src/lib.rs")), "").unwrap();
    }
    fs::write(
        root.join(".anodizer.yaml"),
        format!(
            r#"project_name: percrate
{changelog_block}crates:
  - name: core
    path: crates/core
    tag_template: "core-v{{{{ .Version }}}}"
    version_sync:
      enabled: true
  - name: cli
    path: crates/cli
    tag_template: "cli-v{{{{ .Version }}}}"
    version_sync:
      enabled: true
"#
        ),
    )
    .unwrap();

    // A multi-track root: each crate owns a `### <crate>` subsection under
    // `## [Unreleased]` with a curated bullet. The seeded `[Unreleased]:` footer
    // link supplies the compare base for the rolled footer.
    fs::write(
        root.join("CHANGELOG.md"),
        format!(
            "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### core\n\
- curated core entry\n\
\n\
### cli\n\
- curated cli entry\n\
\n\
{extra_root}\
[Unreleased]: https://github.com/acme/proj/compare/core-v0.1.0...HEAD\n"
        ),
    )
    .unwrap();

    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "core-v0.1.0"]);
    run_git(root, &["tag", "cli-v0.1.0"]);
}

/// Multi-track per-crate root, `chronology: date`: tagging the `core` track
/// promotes ONLY its `### core` subsection to `## [core-v0.2.0] - <date>`
/// (bucketed under `groups:` headings), retains `### cli` verbatim, slots the
/// section directly under `[Unreleased]`, and rolls the compare footer to this
/// track's tag.
#[test]
fn multitrack_root_date_promotes_only_tagged_track_subsection() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    // Explicit `root: {chronology: date}` (bare would resolve identically).
    multitrack_root_fixture(root, "changelog:\n  root:\n    chronology: date\n", "");

    // A feat on core only → core bumps 0.1.0 → 0.2.0 (tag core-v0.2.0).
    fs::write(root.join("crates/core/src/lib.rs"), "// core touched\n").unwrap();
    git_add_commit(root, "feat: core gains a thing");

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--no-push", "--changelog"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");

    let changelog = show_head(root, "CHANGELOG.md");

    // core's subsection promoted to a released section keyed to ITS full tag.
    assert!(
        changelog.contains("## [core-v0.2.0] - "),
        "expected core's subsection promoted to a dated release heading: {changelog}"
    );
    // Curated core bullet survives under a `groups:` heading (default Features).
    assert!(
        changelog.contains("curated core entry"),
        "promoted section must keep the curated core bullet: {changelog}"
    );
    // cli's subsection is retained verbatim under the (still-present) Unreleased.
    assert!(
        changelog.contains("### cli\n- curated cli entry"),
        "the untagged cli subsection must be retained verbatim: {changelog}"
    );
    // core's subsection is consumed out of Unreleased (no stray `### core` left
    // under the surviving `## [Unreleased]` block).
    let unreleased_idx = changelog.find("## [Unreleased]").unwrap();
    let promoted_idx = changelog.find("## [core-v0.2.0]").unwrap();
    let unreleased_block = &changelog[unreleased_idx..promoted_idx];
    assert!(
        !unreleased_block.contains("### core"),
        "core's subsection must be removed from Unreleased: {changelog}"
    );
    assert!(
        unreleased_block.contains("### cli"),
        "cli's subsection must remain under Unreleased: {changelog}"
    );
    // The promoted section sits directly under `[Unreleased]` (date → newest top).
    assert!(
        promoted_idx > unreleased_idx,
        "promoted section must follow the fresh Unreleased heading: {changelog}"
    );
    // The compare footer rolled to THIS track's tag, preserving the host.
    assert!(
        changelog.contains("[Unreleased]: https://github.com/acme/proj/compare/core-v0.2.0...HEAD"),
        "Unreleased footer must roll to core-v0.2.0...HEAD: {changelog}"
    );
    assert!(
        changelog.contains(
            "[core-v0.2.0]: https://github.com/acme/proj/compare/core-v0.1.0...core-v0.2.0"
        ),
        "a [core-v0.2.0] compare link must point from the prior core tag: {changelog}"
    );
}

/// Multi-track per-crate root, `chronology: tag`: with a seeded
/// newer-dated OTHER-track section present, the newly promoted `core` section
/// must land in its tag-prefix cluster (semver-desc) rather than on top — the
/// observable divergence from `date`, which would slot newest-by-date first.
#[test]
fn multitrack_root_tag_clusters_by_prefix_not_date() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    // Seed a NEWER-dated cli-track release below the curated Unreleased
    // block. Under `date`, today's core section would jump above it; under
    // `tag`, core-v* clusters before cli-v* (lexical prefix ascending), so the
    // new core section lands ABOVE the existing core release and the cli release
    // stays in its own cluster.
    let extra = "## [cli-v0.9.0] - 2099-01-01\n\
\n\
### Features\n\
- a far-future cli release\n\
\n\
## [core-v0.1.5] - 2025-01-01\n\
\n\
### Features\n\
- an older core release\n\
\n";
    multitrack_root_fixture(root, "changelog:\n  root:\n    chronology: tag\n", extra);

    fs::write(root.join("crates/core/src/lib.rs"), "// core touched\n").unwrap();
    git_add_commit(root, "feat: core gains a thing");

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--no-push", "--changelog"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");

    let changelog = show_head(root, "CHANGELOG.md");
    let new_idx = changelog
        .find("## [core-v0.2.0]")
        .unwrap_or_else(|| panic!("expected the promoted core-v0.2.0 section: {changelog}"));
    let old_core_idx = changelog
        .find("## [core-v0.1.5]")
        .unwrap_or_else(|| panic!("expected the seeded core-v0.1.5 section: {changelog}"));
    let cli_idx = changelog
        .find("## [cli-v0.9.0]")
        .unwrap_or_else(|| panic!("expected the seeded cli-v0.9.0 section: {changelog}"));

    // Tag clustering: the new core-v0.2.0 sits in the `core-v*` cluster, ABOVE
    // the older core-v0.1.5 (semver-desc within the cluster) and BEFORE the
    // `cli-v*` cluster (prefix `cli` < `core`, so cli sorts first).
    assert!(
        cli_idx < new_idx,
        "tag chronology must keep the cli-v* cluster before core-v*: {changelog}"
    );
    assert!(
        new_idx < old_core_idx,
        "new core-v0.2.0 must cluster above the older core-v0.1.5 (semver-desc): {changelog}"
    );
    // Divergence vs `date`: under `date` today's section would be the file's
    // newest and sit above the 2099 cli release; under `tag` the cli cluster
    // stays on top, so core-v0.2.0 lands between the cli cluster and the older
    // core release rather than at the top.
    assert!(
        new_idx > cli_idx && new_idx < old_core_idx,
        "tag chronology must slot core-v0.2.0 between the cli cluster ({cli_idx}) \
         and the older core release ({old_core_idx}), not at the top: {changelog}"
    );
}

/// The "both" destination (`per_crate: true` + bare `root: {}`): tagging writes
/// BOTH a per-crate `crates/core/CHANGELOG.md` AND the root `CHANGELOG.md`, and
/// BOTH ride the same bump commit.
#[test]
fn both_destination_writes_per_crate_and_root_in_one_commit() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    multitrack_root_fixture(root, "changelog:\n  per_crate: true\n  root: {}\n", "");

    fs::write(root.join("crates/core/src/lib.rs"), "// core touched\n").unwrap();
    git_add_commit(root, "feat: core gains a thing");

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--no-push", "--changelog"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");

    // Per-crate file: keyed to the plain version (per-crate sections are flat).
    let per_crate = show_head(root, "crates/core/CHANGELOG.md");
    assert!(
        per_crate.contains("## [0.2.0]"),
        "both: per-crate file must gain a 0.2.0 section: {per_crate}"
    );
    // Root file: the `### core` subsection promoted to its full tag.
    let root_cl = show_head(root, "CHANGELOG.md");
    assert!(
        root_cl.contains("## [core-v0.2.0] - "),
        "both: root file must gain the promoted core-v0.2.0 section: {root_cl}"
    );

    // BOTH files are named in HEAD's tree diff — one commit, both destinations.
    let diff = Command::new("git")
        .current_dir(root)
        .args(["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"])
        .output()
        .unwrap();
    let names = String::from_utf8_lossy(&diff.stdout);
    assert!(
        names.lines().any(|l| l == "CHANGELOG.md"),
        "HEAD commit must touch the root CHANGELOG.md: {names}"
    );
    assert!(
        names.lines().any(|l| l == "crates/core/CHANGELOG.md"),
        "HEAD commit must touch the per-crate CHANGELOG.md: {names}"
    );
}

/// `root.crates` subset filter on a multi-track root: with `crates: ["core"]`,
/// tagging the INCLUDED `core` track promotes its root section, but tagging the
/// EXCLUDED `cli` track adds NO new root section (its `### cli` subsection is
/// left untouched and no `## [cli-v...]` heading appears).
#[test]
fn root_crates_subset_filters_excluded_track_from_root() {
    // First: the INCLUDED crate (core) → its section lands in the root.
    let tmp_inc = TempDir::new().unwrap();
    let inc = tmp_inc.path();
    multitrack_root_fixture(inc, "changelog:\n  root:\n    crates: [\"core\"]\n", "");
    fs::write(inc.join("crates/core/src/lib.rs"), "// core touched\n").unwrap();
    git_add_commit(inc, "feat: core gains a thing");
    let out = anodizer()
        .current_dir(inc)
        .args(["tag", "--no-push", "--changelog"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tag (included) failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let inc_cl = show_head(inc, "CHANGELOG.md");
    assert!(
        inc_cl.contains("## [core-v0.2.0] - "),
        "included core track must gain a root section: {inc_cl}"
    );

    // Second: the EXCLUDED crate (cli) → NO new root section for it.
    let tmp_exc = TempDir::new().unwrap();
    let exc = tmp_exc.path();
    multitrack_root_fixture(exc, "changelog:\n  root:\n    crates: [\"core\"]\n", "");
    fs::write(exc.join("crates/cli/src/lib.rs"), "// cli touched\n").unwrap();
    git_add_commit(exc, "feat: cli gains a thing");
    let out = anodizer()
        .current_dir(exc)
        .args(["tag", "--no-push", "--changelog"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tag (excluded) failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let exc_cl = read(exc, "CHANGELOG.md");
    assert!(
        !exc_cl.contains("## [cli-v0.2.0]"),
        "excluded cli track must NOT gain a root section: {exc_cl}"
    );
    // The cli subsection is left intact under Unreleased (nothing promoted).
    assert!(
        exc_cl.contains("### cli\n- curated cli entry"),
        "excluded cli subsection must remain under Unreleased: {exc_cl}"
    );
}

// ---------------------------------------------------------------------------
// Cross-engine end-to-end: `changelog --write` (refresh) → hand-edit → `tag
// --changelog` (promote). The headline guarantee: an operator can preview /
// generate the pending section, curate it by hand, commit, then tag WITHOUT
// the promote step clobbering the hand edit. Two engine functions are spanned
// through the REAL CLI: `changelog --write` runs `refresh_*_unreleased`
// (regenerate `[Unreleased]`, no promote); `tag --changelog` runs
// `render_*_section` (promote `[Unreleased]` → `## [<version>] - <date>`,
// preserving a curated body verbatim).
// ---------------------------------------------------------------------------

/// Replace the substring `needle` in the file at `dir/rel` with `replacement`,
/// asserting `needle` was actually present (so a silent no-op edit can't pass
/// the test by accident).
fn sentinel_edit(dir: &Path, rel: &str, needle: &str, replacement: &str) {
    let before = read(dir, rel);
    assert!(
        before.contains(needle),
        "expected generated text {needle:?} in {rel} before the hand edit, got:\n{before}"
    );
    let after = before.replace(needle, replacement);
    fs::write(dir.join(rel), after).unwrap();
}

/// Single-crate (flat CHANGELOG.md): `changelog --write` fills `[Unreleased]`
/// from a `feat:` commit; the operator rewrites that bullet to a SENTINEL,
/// commits the file; `tag --changelog` promotes `[Unreleased]` to a dated
/// release heading carrying the SENTINEL verbatim — the generated text does not
/// reappear (the curated-body-wins branch of the Keep-a-Changelog roll).
#[test]
fn e2e_write_then_tag_preserves_hand_edited_single_crate() {
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
    git_add_commit(root, "feat: generated bullet text");

    // Engine A (refresh): generate the pending section into CHANGELOG.md.
    let write = anodizer()
        .current_dir(root)
        .args(["changelog", "--write", "-q"])
        .output()
        .unwrap();
    assert!(
        write.status.success(),
        "changelog --write failed: {}\n{}",
        String::from_utf8_lossy(&write.stdout),
        String::from_utf8_lossy(&write.stderr)
    );
    // Bare `changelog: {}` routes the single-crate section to the root file.
    let generated = read(root, "CHANGELOG.md");
    assert!(
        generated.contains("## [Unreleased]") && generated.contains("generated bullet text"),
        "refresh must seed [Unreleased] with the generated bullet: {generated}"
    );

    // The hand edit: rewrite the generated bullet to a unique sentinel, then
    // commit the curated file (the operator's curation lands in git).
    const SENTINEL: &str = "HAND CURATED SENTINEL 7f3a";
    sentinel_edit(root, "CHANGELOG.md", "generated bullet text", SENTINEL);
    git_add_commit(root, "docs: curate changelog");

    // Engine B (promote): tag with the refresh opted in.
    let tag = anodizer()
        .current_dir(root)
        .args(["tag", "--crate", "app", "--no-push", "--changelog"])
        .output()
        .unwrap();
    assert!(
        tag.status.success(),
        "tag --changelog failed: {}\n{}",
        String::from_utf8_lossy(&tag.stdout),
        String::from_utf8_lossy(&tag.stderr)
    );

    // The committed result (HEAD) promoted [Unreleased] to a dated heading,
    // carried the hand edit verbatim, and did NOT resurrect the generated text.
    let head = show_head(root, "CHANGELOG.md");
    assert!(
        head.contains("## [0.2.0] - "),
        "promote must produce a dated release heading: {head}"
    );
    assert!(
        head.contains(SENTINEL),
        "the hand-edited sentinel must survive promotion verbatim: {head}"
    );
    assert!(
        !head.contains("generated bullet text"),
        "the regenerated bullet must NOT reappear over the hand edit: {head}"
    );
}

/// Multi-track per-crate root: `changelog --write --crate core` refreshes ONLY
/// core's `### core` subsection under the shared root `[Unreleased]` from a
/// `feat:` commit; the operator rewrites that bullet to a SENTINEL, commits;
/// `tag --crate core --changelog` promotes the `### core` subsection to a dated
/// `## [core-v0.2.0]` heading carrying the SENTINEL verbatim, leaves `### cli`
/// untouched, and does not resurrect the generated text (curated-subsection-wins
/// branch of the multi-track promote).
#[test]
fn e2e_write_then_tag_preserves_hand_edited_multitrack_root() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    // `crates: ["core", "cli"]` per-crate config with a root destination; the
    // seeded root carries `### core`/`### cli` subsections so the refresh engages
    // the multi-track subsection path (not a flat roll).
    multitrack_root_fixture(root, "changelog:\n  root: {}\n", "");

    // A feat on core only → core is the bumped track (core-v0.1.0 → core-v0.2.0).
    fs::write(root.join("crates/core/src/lib.rs"), "// core touched\n").unwrap();
    git_add_commit(root, "feat: generated core text");

    // Engine A (refresh): regenerate ONLY core's `### core` subsection. The
    // seeded `- curated core entry` bullet is replaced by the generated bullet.
    let write = anodizer()
        .current_dir(root)
        .args(["changelog", "--write", "--crate", "core", "-q"])
        .output()
        .unwrap();
    assert!(
        write.status.success(),
        "changelog --write --crate core failed: {}\n{}",
        String::from_utf8_lossy(&write.stdout),
        String::from_utf8_lossy(&write.stderr)
    );
    let generated = read(root, "CHANGELOG.md");
    assert!(
        generated.contains("### core") && generated.contains("generated core text"),
        "refresh must regenerate the core subsection from the commit: {generated}"
    );
    assert!(
        generated.contains("### cli\n- curated cli entry"),
        "refresh of core must leave the cli subsection untouched: {generated}"
    );

    // The hand edit: rewrite core's generated bullet to a unique sentinel, commit.
    const SENTINEL: &str = "HAND CURATED CORE SENTINEL 91be";
    sentinel_edit(root, "CHANGELOG.md", "generated core text", SENTINEL);
    git_add_commit(root, "docs: curate core changelog");

    // Engine B (promote): tag the core track with the refresh opted in.
    let tag = anodizer()
        .current_dir(root)
        .args(["tag", "--crate", "core", "--no-push", "--changelog"])
        .output()
        .unwrap();
    assert!(
        tag.status.success(),
        "tag --crate core --changelog failed: {}\n{}",
        String::from_utf8_lossy(&tag.stdout),
        String::from_utf8_lossy(&tag.stderr)
    );

    let head = show_head(root, "CHANGELOG.md");
    assert!(
        head.contains("## [core-v0.2.0] - "),
        "promote must produce a dated core release heading: {head}"
    );
    assert!(
        head.contains(SENTINEL),
        "the hand-edited core sentinel must survive promotion verbatim: {head}"
    );
    assert!(
        !head.contains("generated core text"),
        "the regenerated core bullet must NOT reappear over the hand edit: {head}"
    );
    // The untagged cli subsection is preserved verbatim under Unreleased.
    assert!(
        head.contains("### cli\n- curated cli entry"),
        "the untagged cli subsection must survive the core promote: {head}"
    );
}

/// Build a flat `crates:` workspace whose members ALL share `tag_template:
/// "v{{ .Version }}"` and route to one shared root (no `per_crate`), with a
/// curated flat `## [Unreleased]` whose H3 (`### Docs`) is NOT a configured
/// group — the multi-track-misread trap. Both members start at `0.1.0` with a
/// shared `v0.1.0` tag, then both get a post-tag `feat:` commit.
fn same_prefix_flat_repo() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/core\", \"crates/cli\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    for name in ["core", "cli"] {
        fs::create_dir_all(root.join(format!("crates/{name}/src"))).unwrap();
        fs::write(
            root.join(format!("crates/{name}/Cargo.toml")),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
        )
        .unwrap();
        fs::write(root.join(format!("crates/{name}/src/lib.rs")), "").unwrap();
    }
    fs::write(
        root.join(".anodizer.yaml"),
        r#"project_name: aggregate
changelog:
  groups:
    - title: Features
      regexp: "^feat"
crates:
  - name: core
    path: crates/core
    tag_template: "v{{ .Version }}"
    version_sync:
      enabled: true
  - name: cli
    path: crates/cli
    tag_template: "v{{ .Version }}"
    version_sync:
      enabled: true
"#,
    )
    .unwrap();
    fs::write(
        root.join("CHANGELOG.md"),
        "# Changelog\n\n## [Unreleased]\n\n### Docs\n\n- curated docs note\n",
    )
    .unwrap();
    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "v0.1.0"]);
    fs::write(root.join("crates/core/src/lib.rs"), "// core\n").unwrap();
    git_add_commit(root, "feat: change in core");
    fs::write(root.join("crates/cli/src/lib.rs"), "// cli\n").unwrap();
    git_add_commit(root, "feat: change in cli");
    tmp
}

/// Assert that the `## [...]` section headed by `heading_substr` in `text`
/// carries NO `### ` subsection other than the supplied `allowed`. A flat
/// aggregate keyed by `project_name` must never graft `### <crate>` OR
/// `### <project_name>`; restricting the scan to the section keeps curated H3s
/// in OTHER sections from tripping the check.
fn assert_section_no_graft(text: &str, heading_substr: &str, allowed: &[&str]) {
    let mut in_section = false;
    for line in text.lines() {
        if line.starts_with("## ") {
            in_section = line.contains(heading_substr);
            continue;
        }
        if in_section && line.starts_with("### ") {
            let title = line.trim_start_matches("### ").trim();
            assert!(
                allowed.contains(&title),
                "unexpected `### {title}` grafted into `{heading_substr}` section \
                 (allowed: {allowed:?}):\n{text}"
            );
        }
    }
}

/// `tag --changelog` on a same-prefix flat-`crates:` repo (every member on
/// `v{{ Version }}`, shared root) is a single lockstep aggregate: both members
/// bump to one shared `v0.2.0` tag (created once), and the root gets ONE flat
/// `## [0.2.0]` section with NO `### <crate>`/`### <project_name>` graft.
#[test]
fn same_prefix_flat_tag_collapses_to_one_section() {
    let tmp = same_prefix_flat_repo();
    let root = tmp.path();
    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--no-push", "--changelog"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");

    // Exactly one shared tag created for both crates (no duplicate-tag failure).
    let tags = Command::new("git")
        .current_dir(root)
        .args(["tag"])
        .output()
        .unwrap();
    let tag_list = String::from_utf8_lossy(&tags.stdout);
    assert_eq!(
        tag_list.matches("v0.2.0").count(),
        1,
        "same-prefix crates must resolve to ONE shared v0.2.0 tag: {tag_list}"
    );

    let head = show_head(root, "CHANGELOG.md");
    // ONE flat released section; no `### <crate>`/`### <project_name>` graft.
    assert_eq!(
        head.matches("## [0.2.0]").count(),
        1,
        "expected one flat [0.2.0] section: {head}"
    );
    for c in ["### core", "### cli", "### aggregate"] {
        assert!(!head.contains(c), "spurious `{c}` graft: {head}");
    }
    assert_section_no_graft(&head, "[0.2.0]", &["Docs", "Features"]);
}

/// `bump --commit --changelog --workspace` on the same same-prefix flat repo
/// collapses identically: ONE flat `## [0.2.0]` section, no graft. (bump never
/// creates tags, so this covers the collapse without the shared-tag path.)
#[test]
fn same_prefix_flat_bump_collapses_to_one_section() {
    let tmp = same_prefix_flat_repo();
    let root = tmp.path();
    let out = anodizer()
        .current_dir(root)
        .args(["bump", "--commit", "--changelog", "--workspace", "minor"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "bump failed: {stdout}\n{stderr}");

    let head = show_head(root, "CHANGELOG.md");
    assert_eq!(
        head.matches("## [0.2.0]").count(),
        1,
        "expected one flat [0.2.0] section: {head}"
    );
    for c in ["### core", "### cli", "### aggregate"] {
        assert!(!head.contains(c), "spurious `{c}` graft: {head}");
    }
    assert_section_no_graft(&head, "[0.2.0]", &["Docs", "Features"]);
}
