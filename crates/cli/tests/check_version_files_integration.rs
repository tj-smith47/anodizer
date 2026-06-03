//! Integration tests for `anodizer check version-files`.
//!
//! The read-only drift guard reports any enrolled `version_files` whose embedded
//! version string has drifted from the crate's CURRENT declared version, so CI
//! can fail before a release. Exercised across all three config modes:
//!   1. single-crate (flat `crates:` with a literal `[package].version`),
//!   2. workspace-lockstep (top-level `version_files`, inherited
//!      `[workspace.package].version`),
//!   3. workspace per-crate (flat `crates:` with independent versions).

use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn anodizer() -> Command {
    Command::new(env!("CARGO_BIN_EXE_anodizer"))
}

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

struct Run {
    success: bool,
    stdout: String,
    stderr: String,
}

fn run_check(root: &Path) -> Run {
    let out = anodizer()
        .current_dir(root)
        .args(["check", "version-files"])
        .output()
        .unwrap();
    Run {
        success: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

// ---------------------------------------------------------------------------
// Mode 1: single-crate
// ---------------------------------------------------------------------------

fn single_crate_fixture(root: &Path, chart_version: &str) {
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/app\"]\nresolver = \"2\"\n",
    );
    write(
        root,
        "crates/app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(root, "crates/app/src/lib.rs", "");
    write(
        root,
        "Chart.yaml",
        &format!("appVersion: v{chart_version}\n"),
    );
    write(
        root,
        ".anodizer.yaml",
        r#"project_name: single
crates:
  - name: app
    path: crates/app
    tag_template: "v{{ .Version }}"
    version_files:
      - Chart.yaml
"#,
    );
}

#[test]
fn single_crate_fresh_exits_zero() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    // The enrolled file carries the crate's current version (0.1.0).
    single_crate_fixture(root, "0.1.0");

    let run = run_check(root);
    assert!(
        run.success,
        "fresh single-crate should pass: {}\n{}",
        run.stdout, run.stderr
    );
    assert!(
        run.stderr.contains("in sync") || run.stdout.contains("in sync"),
        "expected an in-sync line: {}\n{}",
        run.stdout,
        run.stderr
    );
}

#[test]
fn single_crate_stale_exits_nonzero() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    // Crate version is 0.1.0 but the enrolled file still says 0.0.9 → drift.
    single_crate_fixture(root, "0.0.9");

    let run = run_check(root);
    assert!(
        !run.success,
        "stale single-crate should fail: {}\n{}",
        run.stdout, run.stderr
    );
    assert!(
        run.stderr.contains("STALE: Chart.yaml") && run.stderr.contains("expected 0.1.0"),
        "expected a STALE finding naming Chart.yaml and the expected version: {}",
        run.stderr
    );
}

#[test]
fn missing_enrolled_file_exits_nonzero() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    single_crate_fixture(root, "0.1.0");
    // Remove the enrolled file so the guard hits an unreadable-file finding.
    fs::remove_file(root.join("Chart.yaml")).unwrap();

    let run = run_check(root);
    assert!(
        !run.success,
        "missing enrolled file should fail: {}\n{}",
        run.stdout, run.stderr
    );
    // The finding must name the path AND carry the unreadable-file wording, so
    // a missing-file finding stays shaped distinctly from a drift finding (which
    // also names the path but says "expected <version>, not found").
    assert!(
        run.stderr.contains("Chart.yaml"),
        "expected the missing path named in the finding: {}",
        run.stderr
    );
    assert!(
        run.stderr.contains("No such file") || run.stderr.contains("os error"),
        "expected the unreadable-file wording, not a drift message: {}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("expected 0.1.0, not found"),
        "a missing file must not be reported as a version drift: {}",
        run.stderr
    );
}

#[test]
fn no_version_files_configured_exits_zero() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/app\"]\nresolver = \"2\"\n",
    );
    write(
        root,
        "crates/app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(root, "crates/app/src/lib.rs", "");
    write(
        root,
        ".anodizer.yaml",
        "project_name: single\ncrates:\n  - name: app\n    path: crates/app\n    tag_template: \"v{{ .Version }}\"\n",
    );

    let run = run_check(root);
    assert!(
        run.success,
        "no version_files should exit 0: {}\n{}",
        run.stdout, run.stderr
    );
    assert!(
        run.stderr.contains("no version_files configured")
            || run.stdout.contains("no version_files configured"),
        "expected the no-version_files note: {}\n{}",
        run.stdout,
        run.stderr
    );
}

// ---------------------------------------------------------------------------
// Mode 2: workspace-lockstep (top-level version_files, inherited version)
// ---------------------------------------------------------------------------

fn lockstep_fixture(root: &Path, chart_version: &str) {
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/a\"]\nresolver = \"2\"\n\n[workspace.package]\nversion = \"0.4.0\"\n",
    );
    write(
        root,
        "crates/a/Cargo.toml",
        "[package]\nname = \"a\"\nversion.workspace = true\nedition = \"2024\"\n",
    );
    write(root, "crates/a/src/lib.rs", "");
    write(
        root,
        "Chart.yaml",
        &format!("appVersion: v{chart_version}\n"),
    );
    write(
        root,
        ".anodizer.yaml",
        "project_name: lockstep\nversion_files:\n  - Chart.yaml\n",
    );
}

#[test]
fn lockstep_stale_exits_nonzero() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    // Shared workspace version is 0.4.0 but the file still says 0.3.0 → drift.
    lockstep_fixture(root, "0.3.0");

    let run = run_check(root);
    assert!(
        !run.success,
        "stale lockstep should fail: {}\n{}",
        run.stdout, run.stderr
    );
    assert!(
        run.stderr.contains("STALE: Chart.yaml") && run.stderr.contains("expected 0.4.0"),
        "expected a STALE finding at the inherited workspace version: {}",
        run.stderr
    );
}

#[test]
fn lockstep_fresh_exits_zero() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    lockstep_fixture(root, "0.4.0");

    let run = run_check(root);
    assert!(
        run.success,
        "fresh lockstep should pass: {}\n{}",
        run.stdout, run.stderr
    );
}

// ---------------------------------------------------------------------------
// Mode 3: workspace per-crate (flat crates: with independent versions)
// ---------------------------------------------------------------------------

#[test]
fn per_crate_stale_exits_nonzero() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/core\", \"crates/cli\"]\nresolver = \"2\"\n",
    );
    for (name, ver) in [("core", "0.1.0"), ("cli", "0.2.0")] {
        write(
            root,
            &format!("crates/{name}/Cargo.toml"),
            &format!("[package]\nname = \"{name}\"\nversion = \"{ver}\"\nedition = \"2024\"\n"),
        );
        write(root, &format!("crates/{name}/src/lib.rs"), "");
    }
    // core's file is fresh (matches 0.1.0); cli's has drifted (says 0.1.9 not 0.2.0).
    write(root, "core-install.md", "core is at v0.1.0\n");
    write(root, "cli-install.md", "cli is at 0.1.9\n");
    write(
        root,
        ".anodizer.yaml",
        r#"project_name: percrate
crates:
  - name: core
    path: crates/core
    tag_template: "core-v{{ .Version }}"
    version_files:
      - core-install.md
  - name: cli
    path: crates/cli
    tag_template: "cli-v{{ .Version }}"
    version_files:
      - cli-install.md
"#,
    );

    let run = run_check(root);
    assert!(
        !run.success,
        "stale per-crate should fail: {}\n{}",
        run.stdout, run.stderr
    );
    // Only cli drifted; the finding must name cli's file at cli's own version.
    assert!(
        run.stderr.contains("STALE: cli-install.md") && run.stderr.contains("expected 0.2.0"),
        "expected a STALE finding for cli's file at its own version: {}",
        run.stderr
    );
    // core's fresh file must NOT be reported.
    assert!(
        !run.stderr.contains("STALE: core-install.md"),
        "core's file is fresh and must not be flagged: {}",
        run.stderr
    );
}

#[test]
fn per_crate_all_fresh_exits_zero() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/core\", \"crates/cli\"]\nresolver = \"2\"\n",
    );
    for (name, ver) in [("core", "0.1.0"), ("cli", "0.2.0")] {
        write(
            root,
            &format!("crates/{name}/Cargo.toml"),
            &format!("[package]\nname = \"{name}\"\nversion = \"{ver}\"\nedition = \"2024\"\n"),
        );
        write(root, &format!("crates/{name}/src/lib.rs"), "");
    }
    write(root, "core-install.md", "core is at v0.1.0\n");
    write(root, "cli-install.md", "cli is at 0.2.0\n");
    write(
        root,
        ".anodizer.yaml",
        r#"project_name: percrate
crates:
  - name: core
    path: crates/core
    tag_template: "core-v{{ .Version }}"
    version_files:
      - core-install.md
  - name: cli
    path: crates/cli
    tag_template: "cli-v{{ .Version }}"
    version_files:
      - cli-install.md
"#,
    );

    let run = run_check(root);
    assert!(
        run.success,
        "all-fresh per-crate should pass: {}\n{}",
        run.stdout, run.stderr
    );
}

// ---------------------------------------------------------------------------
// Non-member crate: a configured crate that is NOT a [workspace].members entry
// must resolve against ITS OWN literal version, never the workspace version.
// ---------------------------------------------------------------------------

/// The root workspace lists only `crates/member`; `crates/standalone` is
/// configured in `.anodizer.yaml` but excluded from the member globs. Its
/// enrolled file matches its OWN version (0.9.0), distinct from the workspace
/// package version (2.0.0) — proving the guard does not fall back to the
/// workspace version for a real, non-member crate.
fn non_member_fixture(root: &Path, standalone_doc_version: &str) {
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/member\"]\nresolver = \"2\"\n\n[workspace.package]\nversion = \"2.0.0\"\n",
    );
    write(
        root,
        "crates/member/Cargo.toml",
        "[package]\nname = \"member\"\nversion.workspace = true\nedition = \"2024\"\n",
    );
    write(root, "crates/member/src/lib.rs", "");
    // Standalone crate with its own literal version, outside the member globs.
    write(
        root,
        "crates/standalone/Cargo.toml",
        "[package]\nname = \"standalone\"\nversion = \"0.9.0\"\nedition = \"2024\"\n",
    );
    write(root, "crates/standalone/src/lib.rs", "");
    write(
        root,
        "standalone-install.md",
        &format!("standalone pinned at v{standalone_doc_version}\n"),
    );
    write(
        root,
        ".anodizer.yaml",
        r#"project_name: nonmember
crates:
  - name: standalone
    path: crates/standalone
    tag_template: "standalone-v{{ .Version }}"
    version_files:
      - standalone-install.md
"#,
    );
}

#[test]
fn non_member_crate_resolves_against_own_version_fresh() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    // Doc carries the standalone crate's OWN version (0.9.0), not 2.0.0.
    non_member_fixture(root, "0.9.0");

    let run = run_check(root);
    assert!(
        run.success,
        "non-member crate fresh at its own version should pass: {}\n{}",
        run.stdout, run.stderr
    );
    // Must NOT have resolved against the workspace version (2.0.0).
    assert!(
        !run.stderr.contains("expected 2.0.0"),
        "non-member crate must not resolve against the workspace version: {}",
        run.stderr
    );
}

#[test]
fn non_member_crate_resolves_against_own_version_stale() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    // Doc says 2.0.0 (the workspace version) — drift from the crate's own 0.9.0.
    // If the guard wrongly resolved against the workspace version this would
    // pass; it must instead report STALE expecting 0.9.0.
    non_member_fixture(root, "2.0.0");

    let run = run_check(root);
    assert!(
        !run.success,
        "non-member crate drifted from its own version should fail: {}\n{}",
        run.stdout, run.stderr
    );
    assert!(
        run.stderr.contains("STALE: standalone-install.md")
            && run.stderr.contains("expected 0.9.0"),
        "expected a STALE finding at the crate's own version (0.9.0): {}",
        run.stderr
    );
}

// ---------------------------------------------------------------------------
// Rootless layout: a single crate in a subdir with NO root Cargo.toml /
// [workspace] must still resolve (best-effort workspace load).
// ---------------------------------------------------------------------------

#[test]
fn rootless_single_crate_fresh_exits_zero() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    // No root Cargo.toml — the crate lives in a subdir and is its own root.
    write(
        root,
        "app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"1.4.2\"\nedition = \"2024\"\n",
    );
    write(root, "app/src/lib.rs", "");
    write(root, "app/README.md", "version 1.4.2\n");
    write(
        root,
        ".anodizer.yaml",
        r#"project_name: rootless
crates:
  - name: app
    path: app
    tag_template: "v{{ .Version }}"
    version_files:
      - app/README.md
"#,
    );

    let run = run_check(root);
    assert!(
        run.success,
        "rootless single-crate fresh should pass (best-effort workspace load): {}\n{}",
        run.stdout, run.stderr
    );
}

// ---------------------------------------------------------------------------
// Lockstep with a crates: block: one shared top-level file enrolled under N
// crates that all resolve to the same version collapses to a single check via
// the (path, version) dedup key.
// ---------------------------------------------------------------------------

#[test]
fn lockstep_shared_file_dedups_across_crates() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/a\", \"crates/b\"]\nresolver = \"2\"\n\n[workspace.package]\nversion = \"0.5.0\"\n",
    );
    for name in ["a", "b"] {
        write(
            root,
            &format!("crates/{name}/Cargo.toml"),
            &format!(
                "[package]\nname = \"{name}\"\nversion.workspace = true\nedition = \"2024\"\n"
            ),
        );
        write(root, &format!("crates/{name}/src/lib.rs"), "");
    }
    // ONE shared install doc carrying the shared lockstep version.
    write(root, "INSTALL.md", "install v0.5.0\n");
    // Both crates enroll the SAME file; both inherit the same 0.5.0 version, so
    // the (path, version) key collapses the two enrollments into one check.
    write(
        root,
        ".anodizer.yaml",
        r#"project_name: lockstepcrates
crates:
  - name: a
    path: crates/a
    tag_template: "v{{ .Version }}"
    version_files:
      - INSTALL.md
  - name: b
    path: crates/b
    tag_template: "v{{ .Version }}"
    version_files:
      - INSTALL.md
"#,
    );

    let run = run_check(root);
    assert!(
        run.success,
        "lockstep shared-file dedup should pass: {}\n{}",
        run.stdout, run.stderr
    );
    // The dedup collapses 2 enrollments of INSTALL.md@0.5.0 to a single check.
    assert!(
        run.stderr.contains("all 1 version_files are in sync")
            || run.stdout.contains("all 1 version_files are in sync"),
        "expected exactly 1 checked file after dedup: {}\n{}",
        run.stdout,
        run.stderr
    );
}
