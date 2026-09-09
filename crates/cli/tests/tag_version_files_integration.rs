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
    let out = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.current_dir(dir).args(args);
            cmd
        },
        "git",
    );
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
    let out = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.current_dir(dir).args(["show", &format!("HEAD:{rel}")]);
            cmd
        },
        "git",
    );
    assert!(
        out.status.success(),
        "git show HEAD:{rel} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

// ---------------------------------------------------------------------------
// Shared per-crate fixtures
// ---------------------------------------------------------------------------

/// A two-crate per-crate workspace whose crates BOTH enroll the bare path
/// `shared.md`, each tagged at its own `(name, version)` and bumped by one
/// `feat:` commit touching both crate directories.
fn shared_file_fixture(root: &Path, crates: &[(&str, &str)], shared: &str) {
    let members: Vec<String> = crates
        .iter()
        .map(|(name, _)| format!("\"crates/{name}\""))
        .collect();
    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[workspace]\nmembers = [{}]\nresolver = \"2\"\n",
            members.join(", ")
        ),
    )
    .unwrap();
    let mut yaml = String::from("project_name: shared\ncrates:\n");
    for (name, version) in crates {
        fs::create_dir_all(root.join(format!("crates/{name}/src"))).unwrap();
        fs::write(
            root.join(format!("crates/{name}/Cargo.toml")),
            format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\nedition = \"2024\"\n"),
        )
        .unwrap();
        fs::write(root.join(format!("crates/{name}/src/lib.rs")), "").unwrap();
        yaml.push_str(&format!(
            "  - name: {name}\n    path: crates/{name}\n    tag_template: \"{name}-v{{{{ .Version }}}}\"\n    version_sync:\n      enabled: true\n    version_files:\n      - shared.md\n"
        ));
    }
    fs::write(root.join("shared.md"), shared).unwrap();
    fs::write(root.join(".anodizer.yaml"), yaml).unwrap();

    git_init(root);
    git_add_commit(root, "initial");
    for (name, version) in crates {
        run_git(root, &["tag", &format!("{name}-v{version}")]);
    }
    for (name, _) in crates {
        fs::write(
            root.join(format!("crates/{name}/src/lib.rs")),
            format!("// {name} touched\n"),
        )
        .unwrap();
    }
    git_add_commit(root, "feat: both updated");
}

/// A two-crate per-crate workspace (`operator`, `csi`) BOTH at `version`, with
/// a `feat:` commit scoped to the operator crate and a `fix:` commit scoped to
/// the csi crate — so the two bump FROM the same old version TO different new
/// ones. `files` are written before the initial commit; `enroll_operator` /
/// `enroll_csi` are the raw YAML item lines under each crate's
/// `version_files:`.
fn split_bump_fixture(
    root: &Path,
    version: &str,
    files: &[(&str, &str)],
    enroll_operator: &str,
    enroll_csi: &str,
) {
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/operator\", \"crates/csi\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    for name in ["operator", "csi"] {
        fs::create_dir_all(root.join(format!("crates/{name}/src"))).unwrap();
        fs::write(
            root.join(format!("crates/{name}/Cargo.toml")),
            format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\nedition = \"2024\"\n"),
        )
        .unwrap();
        fs::write(root.join(format!("crates/{name}/src/lib.rs")), "").unwrap();
    }
    for (rel, body) in files {
        fs::write(root.join(rel), body).unwrap();
    }
    fs::write(
        root.join(".anodizer.yaml"),
        format!(
            r#"project_name: split
crates:
  - name: operator
    path: crates/operator
    tag_template: "operator-v{{{{ .Version }}}}"
    version_sync:
      enabled: true
    version_files:
{enroll_operator}
  - name: csi
    path: crates/csi
    tag_template: "csi-v{{{{ .Version }}}}"
    version_sync:
      enabled: true
    version_files:
{enroll_csi}
"#
        ),
    )
    .unwrap();

    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", &format!("operator-v{version}")]);
    run_git(root, &["tag", &format!("csi-v{version}")]);
    fs::write(
        root.join("crates/operator/src/lib.rs"),
        "// operator touched\n",
    )
    .unwrap();
    git_add_commit(root, "feat: operator gains a knob");
    fs::write(root.join("crates/csi/src/lib.rs"), "// csi touched\n").unwrap();
    git_add_commit(root, "fix: csi mount race");
}

/// A single-crate (`--crate app`) workspace at 0.1.0 whose `Chart.yaml` holds
/// `chart` and whose enrollment is the raw YAML item lines `enroll`.
fn single_crate_fixture(root: &Path, chart: &str, enroll: &str) {
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
    fs::write(root.join("Chart.yaml"), chart).unwrap();
    fs::write(
        root.join(".anodizer.yaml"),
        format!(
            r#"project_name: single
crates:
  - name: app
    path: crates/app
    tag_template: "v{{{{ .Version }}}}"
    version_sync:
      enabled: true
    version_files:
{enroll}
"#
        ),
    )
    .unwrap();

    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "v0.1.0"]);
    fs::write(root.join("crates/app/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(root, "fix: a bug");
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

/// Single-crate `--dry-run` previews the version_files rewrite (logging the
/// per-file replacement count) but writes nothing — matching the lockstep and
/// per-crate dry-run behaviour.
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
    fs::write(root.join("Chart.yaml"), "appVersion: v0.1.0\n").unwrap();
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
        .args(["tag", "--crate", "app", "--dry-run"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");

    // The rewrite was previewed (count logged) but nothing was written.
    assert!(
        stderr.contains("rewrote 1 occurrence(s) of 0.1.0 → 0.1.1 in Chart.yaml"),
        "expected a dry-run version_files preview line: {stderr}"
    );
    assert_eq!(read(root, "Chart.yaml"), "appVersion: v0.1.0\n");
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

/// Two per-crate crates bumping FROM DIFFERENT old versions may share one
/// enrolled file: each pair rewrites only its own literal, so both land.
#[test]
fn per_crate_distinct_old_versions_both_rewrite() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    shared_file_fixture(
        root,
        &[("core", "0.9.0"), ("cli", "0.7.0")],
        "core 0.9.0 and cli 0.7.0\n",
    );

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--no-push"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");

    assert_eq!(read(root, "shared.md"), "core 0.10.0 and cli 0.8.0\n");
    assert_eq!(show_head(root, "shared.md"), "core 0.10.0 and cli 0.8.0\n");
}

/// The same shape under `--dry-run`: both rewrites are previewed and nothing
/// is written, so the preview matches the real run.
#[test]
fn per_crate_distinct_old_versions_both_rewrite_in_dry_run() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    shared_file_fixture(
        root,
        &[("core", "0.9.0"), ("cli", "0.7.0")],
        "core 0.9.0 and cli 0.7.0\n",
    );

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--dry-run"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "dry-run tag failed: {stdout}\n{stderr}"
    );
    assert!(
        stderr.contains("rewrote 1 occurrence(s) of 0.9.0 → 0.10.0 in shared.md"),
        "expected the core preview line: {stderr}"
    );
    assert!(
        stderr.contains("rewrote 1 occurrence(s) of 0.7.0 → 0.8.0 in shared.md"),
        "expected the cli preview line: {stderr}"
    );
    assert_eq!(read(root, "shared.md"), "core 0.9.0 and cli 0.7.0\n");
}

/// Two crates bumping FROM THE SAME old version TO DIFFERENT new versions
/// cannot share one bare enrollment: the file would have to hold two new
/// versions for one old one.
#[test]
fn per_crate_shared_file_same_old_different_new_bails() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    split_bump_fixture(
        root,
        "0.7.0",
        &[("shared.md", "operator 0.7.0 and csi 0.7.0\n")],
        "      - shared.md",
        "      - shared.md",
    );

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--no-push"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "tag should have failed on the same-old/different-new conflict"
    );
    assert!(
        stderr.contains("bumping FROM the same version") && stderr.contains("shared.md"),
        "expected a same-old conflict naming shared.md: {stderr}"
    );
}

/// Two crates whose bumps CHAIN (core 0.1.0 → 0.2.0 beside cli 0.2.0 → 0.3.0)
/// cannot share one enrollment: the second rewrite would consume the first's
/// output, and no apply order fixes it.
#[test]
fn per_crate_shared_file_chain_bails() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    shared_file_fixture(
        root,
        &[("core", "0.1.0"), ("cli", "0.2.0")],
        "core 0.1.0 and cli 0.2.0\n",
    );

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--no-push"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "tag should have failed on the version_files chain"
    );
    assert!(
        stderr.contains("chain") && stderr.contains("shared.md"),
        "expected a chain conflict naming shared.md: {stderr}"
    );
}

/// The chain check also runs under `--dry-run`, writing nothing — so the
/// preview never green-lights a config the real run would reject.
#[test]
fn per_crate_shared_file_chain_bails_in_dry_run() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    shared_file_fixture(
        root,
        &[("core", "0.1.0"), ("cli", "0.2.0")],
        "core 0.1.0 and cli 0.2.0\n",
    );

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--dry-run"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "dry-run tag should ALSO fail on the version_files chain"
    );
    assert!(
        stderr.contains("chain") && stderr.contains("shared.md"),
        "expected a chain conflict naming shared.md in dry-run: {stderr}"
    );
    assert_eq!(read(root, "shared.md"), "core 0.1.0 and cli 0.2.0\n");
}

/// Two crates bumped to the SAME new version from DIFFERENT old versions
/// (core 0.1.0 → 0.2.0, cli 0.1.5 → 0.2.0) share one file safely: each pair
/// rewrites only the literal it owns.
#[test]
fn per_crate_shared_file_different_old_versions_both_rewrite() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    shared_file_fixture(
        root,
        &[("core", "0.1.0"), ("cli", "0.1.5")],
        "core 0.1.0 and cli 0.1.5\n",
    );

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--no-push"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");

    assert_eq!(read(root, "shared.md"), "core 0.2.0 and cli 0.2.0\n");
    assert_eq!(show_head(root, "shared.md"), "core 0.2.0 and cli 0.2.0\n");
}

/// The cfgd case: two crates at the SAME version share one file AND one
/// literal, each scoped to its own occurrence by a `match` anchor.
#[test]
fn per_crate_anchored_shared_literal_rewrites_own_pin() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    split_bump_fixture(
        root,
        "0.7.0",
        &[(
            "values.yaml",
            "operator:\n  image: ghcr.io/x/operator:v0.7.0\ncsi:\n  image: ghcr.io/x/csi:v0.7.0\n",
        )],
        "      - path: values.yaml\n        match: 'operator:\\n  image: .*:v{version}'",
        "      - path: values.yaml\n        match: 'csi:\\n  image: .*:v{version}'",
    );

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--no-push"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");

    let expected =
        "operator:\n  image: ghcr.io/x/operator:v0.8.0\ncsi:\n  image: ghcr.io/x/csi:v0.7.1\n";
    assert_eq!(read(root, "values.yaml"), expected);
    assert_eq!(show_head(root, "values.yaml"), expected);
}

/// An anchor that selects nothing fails the tag before any file is written —
/// a silent no-op is exactly the failure the anchor exists to prevent.
#[test]
fn anchored_unmatched_anchor_fails_the_tag() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    single_crate_fixture(
        root,
        "appVersion: v0.1.0\n",
        "      - path: Chart.yaml\n        match: 'nope: v{version}'",
    );

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--crate", "app", "--no-push"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "an unmatched anchor must fail the tag"
    );
    assert!(
        stderr.contains("crate 'app'")
            && stderr.contains("Chart.yaml")
            && stderr.contains("nope: v{version}")
            && stderr.contains("matched nothing"),
        "error must name the crate, file and anchor: {stderr}"
    );
    // Nothing written: the engine validates every entry before any IO.
    assert_eq!(read(root, "Chart.yaml"), "appVersion: v0.1.0\n");
}

/// A lockstep workspace's top-level anchored entry rewrites inside its anchor
/// and leaves the rest of the file alone.
#[test]
fn lockstep_anchored_entry_rewrites() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/a\"]\nresolver = \"2\"\n\n[workspace.package]\nversion = \"0.1.0\"\n",
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
        root.join("Chart.yaml"),
        "appVersion: v0.1.0\nunrelated: 0.1.0\n",
    )
    .unwrap();
    fs::write(
        root.join(".anodizer.yaml"),
        "project_name: lockstep\nversion_files:\n  - path: Chart.yaml\n    match: 'appVersion: v{version}'\n",
    )
    .unwrap();

    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "v0.1.0"]);
    fs::write(root.join("crates/a/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(root, "fix: a bug");

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--no-push"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");

    let expected = "appVersion: v0.1.1\nunrelated: 0.1.0\n";
    assert_eq!(read(root, "Chart.yaml"), expected);
    assert_eq!(show_head(root, "Chart.yaml"), expected);
}

/// A single-crate (`--crate`) anchored entry rewrites inside its anchor.
#[test]
fn single_crate_anchored_entry_rewrites() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    single_crate_fixture(
        root,
        "appVersion: v0.1.0\nunrelated: 0.1.0\n",
        "      - path: Chart.yaml\n        match: 'appVersion: v{version}'",
    );

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--crate", "app", "--no-push"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");

    let expected = "appVersion: v0.1.1\nunrelated: 0.1.0\n";
    assert_eq!(read(root, "Chart.yaml"), expected);
    assert_eq!(show_head(root, "Chart.yaml"), expected);
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

// ---------------------------------------------------------------------------
// Invoked from a SUBDIRECTORY of the workspace
//
// `tag` discovers the workspace root and runs every git op there. The enrolled
// version_files (and the manifest/lockfile IO) must resolve against that same
// root, not the process cwd — otherwise a top-level `Chart.yaml` misresolves
// when `tag` is run from a crate subdirectory. One test per config mode.
// ---------------------------------------------------------------------------

#[test]
fn single_crate_from_subdir_rewrites_top_level_version_files() {
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
        root.join("Chart.yaml"),
        "version: 0.1.0\nappVersion: v0.1.0\n",
    )
    .unwrap();
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
"#,
    )
    .unwrap();

    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "v0.1.0"]);
    fs::write(root.join("crates/app/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(root, "fix: a bug");

    // Invoke from the crate subdirectory, NOT the workspace root. The explicit
    // `--config` (root `.anodizer.yaml`) anchors the workspace-root discovery
    // (walk up from the config's parent), matching the established subdir
    // contract; the fix is that the version_files IO then resolves against that
    // root rather than the cwd.
    let cfg = root.join(".anodizer.yaml");
    let out = anodizer()
        .current_dir(root.join("crates/app"))
        .args([
            "tag",
            "--crate",
            "app",
            "--no-push",
            "--config",
            cfg.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");
    assert!(stdout.contains("new_tag=v0.1.1"), "stdout: {stdout}");

    // The TOP-LEVEL Chart.yaml is rewritten + committed (resolved against the
    // workspace root, not the crates/app cwd).
    assert_eq!(
        read(root, "Chart.yaml"),
        "version: 0.1.1\nappVersion: v0.1.1\n"
    );
    assert_eq!(
        show_head(root, "Chart.yaml"),
        "version: 0.1.1\nappVersion: v0.1.1\n"
    );
}

#[test]
fn lockstep_from_subdir_rewrites_top_level_version_files() {
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

    let cfg = root.join(".anodizer.yaml");
    let out = anodizer()
        .current_dir(root.join("crates/a"))
        .args(["tag", "--no-push", "--config", cfg.to_str().unwrap()])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");
    assert!(stdout.contains("new_tag=v0.1.1"), "stdout: {stdout}");

    assert_eq!(read(root, "Chart.yaml"), "appVersion: v0.1.1\n");
    assert_eq!(show_head(root, "Chart.yaml"), "appVersion: v0.1.1\n");
}

#[test]
fn per_crate_from_subdir_rewrites_top_level_version_files() {
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
    // Top-level (repo-root-relative) enrolled files, one per crate.
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

    // Invoke from a crate subdirectory; the per-crate engine still resolves
    // every top-level enrolled file against the workspace root.
    let cfg = root.join(".anodizer.yaml");
    let out = anodizer()
        .current_dir(root.join("crates/core"))
        .args(["tag", "--no-push", "--config", cfg.to_str().unwrap()])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "tag failed: {stdout}\n{stderr}");

    assert_eq!(read(root, "core-install.md"), "core is at v0.2.0\n");
    assert_eq!(read(root, "cli-install.md"), "cli is at 0.3.0\n");
    assert_eq!(show_head(root, "core-install.md"), "core is at v0.2.0\n");
    assert_eq!(show_head(root, "cli-install.md"), "cli is at 0.3.0\n");
}

/// A config with no `crates:` block: `tag` must rewrite its top-level
/// `version_files`, bare and anchored alike. `check version-files` validates
/// the same list, so an entry `tag` skipped would be reported stale forever.
#[test]
fn no_crates_block_rewrites_top_level_version_files() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"1.2.3\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(root.join("README.md"), "Install app 1.2.3 today.\n").unwrap();
    fs::write(
        root.join("chart.yaml"),
        "app:\n  image: ghcr.io/x/app:v1.2.3\ndocs: see 1.2.3 for details\n",
    )
    .unwrap();
    fs::write(
        root.join(".anodizer.yaml"),
        "project_name: app\nversion_files:\n  - README.md\n  - path: chart.yaml\n    match: 'app:\\n  image: .*:v{version}'\n",
    )
    .unwrap();

    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "v1.2.3"]);
    fs::write(root.join("src/main.rs"), "fn main() {}\n// touched\n").unwrap();
    git_add_commit(root, "feat: a thing");

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--dry-run"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{stdout}{stderr}");
    assert!(out.status.success(), "tag failed: {combined}");
    assert!(
        combined.contains("rewrote 1 occurrence(s) of 1.2.3 → 1.3.0 in README.md"),
        "bare top-level entry not planned: {combined}"
    );
    assert!(
        combined.contains("rewrote 1 occurrence(s) of 1.2.3 → 1.3.0 in chart.yaml (match"),
        "anchored top-level entry not planned: {combined}"
    );
    // Dry-run previews only.
    assert_eq!(read(root, "README.md"), "Install app 1.2.3 today.\n");

    // The real run writes both entries and commits them before the tag: the
    // anchored one rewrites its image pin and leaves the unrelated literal.
    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--no-push"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tag failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let chart = "app:\n  image: ghcr.io/x/app:v1.3.0\ndocs: see 1.2.3 for details\n";
    assert_eq!(read(root, "README.md"), "Install app 1.3.0 today.\n");
    assert_eq!(read(root, "chart.yaml"), chart);
    assert_eq!(show_head(root, "README.md"), "Install app 1.3.0 today.\n");
    assert_eq!(show_head(root, "chart.yaml"), chart);
}

/// A prerelease target still matches its own old version, so a bare entry and
/// an anchored entry on one file would rewrite the same bytes twice. The guard
/// runs in single-crate mode, not only per-crate.
#[test]
fn single_crate_bare_plus_anchored_prerelease_bails() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"1.2.3\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(root.join("chart.yaml"), "pin: v1.2.3\nother: 1.2.3\n").unwrap();
    fs::write(
        root.join(".anodizer.yaml"),
        "project_name: app\nversion_files:\n  - chart.yaml\n  - path: chart.yaml\n    match: 'pin: v{version}'\n",
    )
    .unwrap();
    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "v1.2.3"]);
    fs::write(root.join("src/main.rs"), "fn main() {}\n// touched\n").unwrap();
    git_add_commit(root, "fix: a bug");

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--version", "1.2.3-rc1", "--dry-run"])
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "the double rewrite must bail: {combined}"
    );
    assert!(
        combined.contains("bumps chain (app 1.2.3 → 1.2.3-rc1 then app 1.2.3 → 1.2.3-rc1)"),
        "chain refusal missing: {combined}"
    );
    assert_eq!(read(root, "chart.yaml"), "pin: v1.2.3\nother: 1.2.3\n");
}

/// The same guard in lockstep mode, where the top-level enrollment is shared by
/// every workspace crate under one version.
#[test]
fn lockstep_bare_plus_anchored_prerelease_bails() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/a\"]\nresolver = \"2\"\n\n[workspace.package]\nversion = \"2.0.0\"\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("crates/a/src")).unwrap();
    fs::write(
        root.join("crates/a/Cargo.toml"),
        "[package]\nname = \"a\"\nversion.workspace = true\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(root.join("crates/a/src/lib.rs"), "").unwrap();
    fs::write(root.join("chart.yaml"), "pin: v2.0.0\nother: 2.0.0\n").unwrap();
    fs::write(
        root.join(".anodizer.yaml"),
        "project_name: suite\ncrates:\n  - name: a\n    path: crates/a\nversion_files:\n  - chart.yaml\n  - path: chart.yaml\n    match: 'pin: v{version}'\n",
    )
    .unwrap();
    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "v2.0.0"]);
    fs::write(root.join("crates/a/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(root, "fix: a bug");

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--version", "2.0.0-rc1", "--dry-run"])
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "the double rewrite must bail: {combined}"
    );
    assert!(
        combined.contains("bumps chain (suite 2.0.0 → 2.0.0-rc1 then suite 2.0.0 → 2.0.0-rc1)"),
        "chain refusal missing: {combined}"
    );
    assert_eq!(read(root, "chart.yaml"), "pin: v2.0.0\nother: 2.0.0\n");
}
