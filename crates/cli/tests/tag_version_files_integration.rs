//! Integration tests for `anodizer tag` `version_files` rewriting.
//!
//! Enrolled repo-committed files (Helm `Chart.yaml`, install docs, README
//! badges) have their embedded release version rewritten — bare and
//! `v`-prefixed forms, word-boundary anchored — in the same bump commit as
//! `Cargo.toml` / `Cargo.lock`, across all three config modes:
//!   1. single-crate (`--crate` + `version_sync`),
//!   2. workspace-lockstep (`[workspace.package].version`),
//!   3. workspace per-crate (flat `crates:` with independent versions).

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

/// The version_files staged into the bump commit must be committed, not left
/// as an unstaged working-tree edit. Returns the file's content at HEAD.
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
fn single_crate_rewrites_enrolled_version_files() {
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
    // Enrolled file carries both the bare and v-prefixed forms.
    fs::write(
        root.join("Chart.yaml"),
        "version: 0.1.0\nappVersion: v0.1.0\n",
    )
    .unwrap();
    fs::write(root.join("install.md"), "stays at 10.1.0 untouched\n").unwrap();
    fs::write(
        root.join(".anodizer.yaml"),
        r#"project_name: single
crates:
  - name: app
    path: crates/app
    tag_template: "v{{ .Version }}"
    version_sync:
      enabled: true
    version_files:
      - Chart.yaml
      - install.md
"#,
    )
    .unwrap();

    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "v0.1.0"]);
    fs::write(root.join("crates/app/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(root, "fix: a bug");

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--crate", "app"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");
    assert!(stdout.contains("new_tag=v0.1.1"), "stdout: {stdout}");

    // Both forms rewritten; the longer 10.1.0 stays put (word boundary).
    assert_eq!(
        read(root, "Chart.yaml"),
        "version: 0.1.1\nappVersion: v0.1.1\n"
    );
    assert_eq!(read(root, "install.md"), "stays at 10.1.0 untouched\n");
    // Rewritten file is in the bump commit, not just the working tree.
    assert_eq!(
        show_head(root, "Chart.yaml"),
        "version: 0.1.1\nappVersion: v0.1.1\n"
    );
}

// ---------------------------------------------------------------------------
// Mode 2: workspace-lockstep
// ---------------------------------------------------------------------------

#[test]
fn lockstep_rewrites_top_level_version_files() {
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
    fs::write(root.join("Chart.yaml"), "appVersion: v0.1.0\n").unwrap();
    fs::write(
        root.join(".anodizer.yaml"),
        "project_name: lockstep\nversion_files:\n  - Chart.yaml\n",
    )
    .unwrap();

    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "v0.1.0"]);
    fs::write(root.join("crates/a/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(root, "fix: a bug");

    let out = anodizer().current_dir(root).args(["tag"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");
    assert!(stdout.contains("new_tag=v0.1.1"), "stdout: {stdout}");

    assert_eq!(read(root, "Chart.yaml"), "appVersion: v0.1.1\n");
    assert_eq!(show_head(root, "Chart.yaml"), "appVersion: v0.1.1\n");
}

// ---------------------------------------------------------------------------
// Mode 3: workspace per-crate (flat crates: with independent versions)
// ---------------------------------------------------------------------------

#[test]
fn per_crate_rewrites_each_crates_own_version_files() {
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
    // Each crate enrolls its OWN doc; they bump from different old versions.
    fs::write(root.join("core-install.md"), "core is at v0.1.0\n").unwrap();
    fs::write(root.join("cli-install.md"), "cli is at 0.2.0\n").unwrap();
    fs::write(
        root.join(".anodizer.yaml"),
        r#"project_name: percrate
crates:
  - name: core
    path: crates/core
    tag_template: "core-v{{ .Version }}"
    version_sync:
      enabled: true
    version_files:
      - core-install.md
  - name: cli
    path: crates/cli
    tag_template: "cli-v{{ .Version }}"
    version_sync:
      enabled: true
    version_files:
      - cli-install.md
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

    // Each crate's enrolled file is rewritten with that crate's own old→new.
    // The default bump level is `minor`, so core 0.1.0 → 0.2.0 (its file
    // carries the v-prefixed form) and cli 0.2.0 → 0.3.0 (bare form).
    assert_eq!(read(root, "core-install.md"), "core is at v0.2.0\n");
    assert_eq!(read(root, "cli-install.md"), "cli is at 0.3.0\n");
    assert_eq!(show_head(root, "core-install.md"), "core is at v0.2.0\n");
    assert_eq!(show_head(root, "cli-install.md"), "cli is at 0.3.0\n");
}

/// Two per-crate crates bumped to DIFFERENT versions that enroll the SAME file
/// is a conflict — the tag run must bail naming the file.
#[test]
fn per_crate_conflicting_shared_file_bails() {
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
    fs::write(root.join("shared.md"), "core 0.1.0 and cli 0.2.0\n").unwrap();
    fs::write(
        root.join(".anodizer.yaml"),
        r#"project_name: conflict
crates:
  - name: core
    path: crates/core
    tag_template: "core-v{{ .Version }}"
    version_sync:
      enabled: true
    version_files:
      - shared.md
  - name: cli
    path: crates/cli
    tag_template: "cli-v{{ .Version }}"
    version_sync:
      enabled: true
    version_files:
      - shared.md
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
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "tag should have failed on the version_files conflict"
    );
    assert!(
        stderr.contains("version_files conflict") && stderr.contains("shared.md"),
        "expected a conflict error naming shared.md: {stderr}"
    );
}

/// Dry-run previews the rewrite but writes nothing.
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
    fs::write(root.join("Chart.yaml"), "appVersion: v0.1.0\n").unwrap();
    fs::write(
        root.join(".anodizer.yaml"),
        "project_name: lockstep\nversion_files:\n  - Chart.yaml\n",
    )
    .unwrap();

    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "v0.1.0"]);
    fs::write(root.join("crates/a/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(root, "fix: a bug");

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--dry-run"])
        .output()
        .unwrap();
    assert!(out.status.success());
    // Untouched on disk.
    assert_eq!(read(root, "Chart.yaml"), "appVersion: v0.1.0\n");
}
