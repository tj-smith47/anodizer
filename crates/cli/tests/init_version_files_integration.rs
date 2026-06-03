//! Integration tests for `anodizer init --version-files`.
//!
//! The enrollment flow discovers TRACKED, text files that embed the current
//! version (bare or `v`-prefixed) and writes the user's selection into the
//! top-level `version_files:` block of an existing `.anodizer.yaml`, preserving
//! the file's comments and key order. Drives the non-interactive `-y` path
//! (the interactive multi-select cannot be driven from a test harness); the
//! discovery/filter/write logic is shared with the prompt.

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

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

fn read(root: &Path, rel: &str) -> String {
    fs::read_to_string(root.join(rel)).unwrap()
}

struct Run {
    success: bool,
    stdout: String,
    stderr: String,
}

fn run_enroll(root: &Path, extra: &[&str]) -> Run {
    let mut args = vec!["init", "--version-files"];
    args.extend_from_slice(extra);
    let out = anodizer().current_dir(root).args(&args).output().unwrap();
    Run {
        success: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// A single-crate workspace at version 0.1.0 with a `.anodizer.yaml` that
/// carries a leading comment (so preservation can be asserted), plus several
/// candidate / non-candidate files committed to git.
fn fixture(root: &Path) {
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/app\"]\nresolver = \"2\"\n\n[workspace.package]\nversion = \"0.1.0\"\n",
    );
    write(
        root,
        "crates/app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(root, "crates/app/src/lib.rs", "");
    // Candidates: contain the version, bare and v-prefixed.
    write(root, "Chart.yaml", "appVersion: v0.1.0\nversion: 0.1.0\n");
    write(root, "docs/install.md", "Install release 0.1.0 now.\n");
    // Not a candidate: no version string.
    write(root, "README.md", "Just a readme, no version.\n");
    // Auto-excluded: Cargo.toml/Cargo.lock embed the version but anodizer bumps them.
    write(root, "Cargo.lock", "# version 0.1.0 lockfile\n");
    // Auto-excluded: under dist/.
    write(root, "dist/output.txt", "built 0.1.0\n");
    write(
        root,
        ".anodizer.yaml",
        "# hand-maintained config — keep this comment\nproject_name: app\ndist: ./dist\n",
    );
    git_init(root);
    git_add_commit(root, "initial");
}

#[test]
fn yes_enrolls_all_candidates_and_excludes_auto() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fixture(root);

    let run = run_enroll(root, &["-y"]);
    assert!(run.success, "stderr: {}", run.stderr);

    let cfg = read(root, ".anodizer.yaml");
    // Pre-existing comment and keys are preserved.
    assert!(
        cfg.contains("# hand-maintained config — keep this comment"),
        "comment lost:\n{cfg}"
    );
    assert!(cfg.contains("project_name: app"), "key lost:\n{cfg}");
    // Candidates enrolled.
    assert!(cfg.contains("version_files:"), "no block:\n{cfg}");
    assert!(cfg.contains("- Chart.yaml"), "Chart.yaml missing:\n{cfg}");
    assert!(
        cfg.contains("- docs/install.md"),
        "docs/install.md missing:\n{cfg}"
    );
    // Non-version file not enrolled.
    assert!(!cfg.contains("- README.md"), "README enrolled:\n{cfg}");
    // Auto-excluded never enrolled.
    assert!(!cfg.contains("Cargo.lock"), "Cargo.lock enrolled:\n{cfg}");
    assert!(!cfg.contains("dist/output.txt"), "dist enrolled:\n{cfg}");
}

#[test]
fn exclude_glob_drops_matching_candidates() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fixture(root);

    let run = run_enroll(root, &["-y", "--exclude", "docs/**"]);
    assert!(run.success, "stderr: {}", run.stderr);

    let cfg = read(root, ".anodizer.yaml");
    assert!(cfg.contains("- Chart.yaml"), "Chart.yaml missing:\n{cfg}");
    assert!(
        !cfg.contains("docs/install.md"),
        "excluded file enrolled:\n{cfg}"
    );
}

#[test]
fn idempotent_second_run_adds_no_duplicates() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fixture(root);

    assert!(run_enroll(root, &["-y"]).success);
    let first = read(root, ".anodizer.yaml");

    let run2 = run_enroll(root, &["-y"]);
    assert!(run2.success, "stderr: {}", run2.stderr);
    let second = read(root, ".anodizer.yaml");

    // Already-enrolled candidates are the only candidates left, so the second
    // run is a no-op: content unchanged and exactly one occurrence of each.
    assert_eq!(first, second, "second run mutated config:\n{second}");
    assert_eq!(
        second.matches("- Chart.yaml").count(),
        1,
        "duplicate entry:\n{second}"
    );
    assert_eq!(
        second.matches("- docs/install.md").count(),
        1,
        "duplicate entry:\n{second}"
    );
}

#[test]
fn zero_candidates_exits_zero_with_message() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    // No file embeds the version other than the auto-excluded manifests.
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/app\"]\nresolver = \"2\"\n\n[workspace.package]\nversion = \"0.1.0\"\n",
    );
    write(
        root,
        "crates/app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(root, "crates/app/src/lib.rs", "");
    write(root, "README.md", "no version here\n");
    write(root, ".anodizer.yaml", "project_name: app\n");
    git_init(root);
    git_add_commit(root, "initial");

    let run = run_enroll(root, &["-y"]);
    assert!(run.success, "stderr: {}", run.stderr);
    let combined = format!("{}{}", run.stdout, run.stderr);
    assert!(
        combined.contains("nothing to enroll") || combined.contains("no un-enrolled"),
        "expected zero-candidate message, got:\nstdout={}\nstderr={}",
        run.stdout,
        run.stderr
    );
    // Config untouched: no version_files block added.
    let cfg = read(root, ".anodizer.yaml");
    assert!(!cfg.contains("version_files:"), "block added:\n{cfg}");
}

#[test]
fn existing_block_gets_new_items_appended_without_dupes() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fixture(root);
    // Pre-seed a config that already enrolls Chart.yaml under a top-level block.
    write(
        root,
        ".anodizer.yaml",
        "project_name: app\nversion_files:\n  - Chart.yaml\n",
    );
    // The seeded config must be tracked so the working tree matches; re-commit.
    git_add_commit(root, "seed config");

    let run = run_enroll(root, &["-y"]);
    assert!(run.success, "stderr: {}", run.stderr);

    let cfg = read(root, ".anodizer.yaml");
    // Chart.yaml stays once; docs/install.md joins the same block.
    assert_eq!(
        cfg.matches("- Chart.yaml").count(),
        1,
        "duplicate Chart.yaml:\n{cfg}"
    );
    assert!(
        cfg.contains("- docs/install.md"),
        "new item not added:\n{cfg}"
    );
}

#[test]
fn missing_config_errors() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(
        root,
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    git_init(root);
    git_add_commit(root, "initial");

    let run = run_enroll(root, &["-y"]);
    assert!(!run.success, "expected failure without .anodizer.yaml");
    assert!(
        run.stderr.contains("anodizer init") || run.stderr.contains("no '.anodizer.yaml'"),
        "stderr: {}",
        run.stderr
    );
}
