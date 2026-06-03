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
//! Also covers the `--no-changelog` opt-out, `--dry-run`, and the
//! no-`changelog:` default-off case.

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
        .args(["tag", "--crate", "app"])
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

/// `--no-changelog` suppresses the refresh even though `changelog:` is set.
#[test]
fn single_crate_no_changelog_flag_suppresses() {
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
        .args(["tag", "--crate", "app", "--no-changelog"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");

    assert!(
        !root.join("crates/app/CHANGELOG.md").exists(),
        "--no-changelog must not write a CHANGELOG.md"
    );
}

/// `--dry-run` writes no CHANGELOG.md but still exits 0.
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
        .args(["tag", "--crate", "app", "--dry-run"])
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

/// No `changelog:` config → tag does not create a CHANGELOG.md.
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
        .args(["tag", "--crate", "app"])
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

/// A plain `changelog: { skip: true }` disables the refresh.
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
        .args(["tag", "--crate", "app"])
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

    let out = anodizer().current_dir(root).args(["tag"]).output().unwrap();
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

/// Lockstep `--dry-run` writes no CHANGELOG.md but still exits 0.
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
        .args(["tag", "--dry-run"])
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
        .args(["tag", "--no-push"])
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
/// per-crate site (proves the gate, not just single-crate).
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
        .args(["tag", "--no-push"])
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
        .args(["tag", "--crate", "app"])
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
        .args(["tag", "--crate", "app"])
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

/// Lockstep `--no-changelog` suppresses the refresh for every member (proves the
/// gate at the lockstep site, not just single-crate).
#[test]
fn lockstep_no_changelog_flag_suppresses() {
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

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--no-changelog"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");

    for name in ["a", "b"] {
        assert!(
            !root.join(format!("crates/{name}/CHANGELOG.md")).exists(),
            "--no-changelog must not write member {name}'s CHANGELOG.md"
        );
    }
}

/// Per-crate `--no-changelog` suppresses the refresh for every bumped crate, yet
/// the bump commit and tags still happen (proves the opt-out at the per-crate
/// site, mirroring `lockstep_no_changelog_flag_suppresses`).
#[test]
fn per_crate_no_changelog_flag_suppresses() {
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
        .args(["tag", "--no-changelog", "--no-push"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");

    for name in ["core", "cli"] {
        assert!(
            !root.join(format!("crates/{name}/CHANGELOG.md")).exists(),
            "--no-changelog must not write {name}'s CHANGELOG.md"
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

/// Per-crate `--dry-run` writes no CHANGELOG.md anywhere and exits 0 (mirrors
/// `lockstep_dry_run_writes_nothing`).
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
        .args(["tag", "--dry-run"])
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

    let out = anodizer().current_dir(root).args(["tag"]).output().unwrap();
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
