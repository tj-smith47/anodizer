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

    let changelog = read(root, "crates/app/CHANGELOG.md");
    assert!(
        changelog.contains("## [0.2.0]"),
        "expected a 0.2.0 section: {changelog}"
    );
    assert!(
        changelog.contains("add a thing"),
        "expected the feat commit in the section: {changelog}"
    );
    // Refreshed file is in the bump commit, not just the working tree.
    let head = show_head(root, "crates/app/CHANGELOG.md");
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
