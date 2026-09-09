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

/// A two-crate per-crate workspace whose crates all enroll `shared.md`, each
/// tagged at its own `(name, version)` and bumped by one `feat:` commit
/// touching every crate directory. `enroll` is the raw YAML item lines under
/// each crate's `version_files:`, indexed alongside `crates`.
fn shared_file_fixture_enrolled(
    root: &Path,
    crates: &[(&str, &str)],
    shared: &str,
    enroll: &[&str],
) {
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
    for (i, (name, version)) in crates.iter().enumerate() {
        fs::create_dir_all(root.join(format!("crates/{name}/src"))).unwrap();
        fs::write(
            root.join(format!("crates/{name}/Cargo.toml")),
            format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\nedition = \"2024\"\n"),
        )
        .unwrap();
        fs::write(root.join(format!("crates/{name}/src/lib.rs")), "").unwrap();
        let items = enroll[i];
        yaml.push_str(&format!(
            "  - name: {name}\n    path: crates/{name}\n    tag_template: \"{name}-v{{{{ .Version }}}}\"\n    version_sync:\n      enabled: true\n    version_files:\n{items}"
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

/// [`shared_file_fixture_enrolled`] with every crate enrolling the bare path.
fn shared_file_fixture(root: &Path, crates: &[(&str, &str)], shared: &str) {
    let enroll = vec!["      - shared.md\n"; crates.len()];
    shared_file_fixture_enrolled(root, crates, shared, &enroll);
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

/// Every `version_files` message names the path the user enrolled, in the one
/// repo-relative spelling `check version-files` prints — including the engine's
/// own unmatched-anchor error, which used to render the resolved absolute path
/// beside siblings that printed `STALE: values.yaml`. Run from a SUBDIRECTORY,
/// where the two spellings differ the most.
#[test]
fn unmatched_anchor_names_the_repo_relative_path() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    single_crate_fixture(
        root,
        "appVersion: v0.1.0\n",
        "      - path: charts/values.yaml\n        match: 'nope: v{version}'",
    );
    fs::create_dir_all(root.join("charts")).unwrap();
    fs::write(root.join("charts/values.yaml"), "image: app:v0.1.0\n").unwrap();

    let config = root.join(".anodizer.yaml");
    let out = anodizer()
        .current_dir(root.join("crates/app"))
        .args([
            "tag",
            "--config",
            config.to_str().unwrap(),
            "--crate",
            "app",
            "--no-push",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "an unmatched anchor must fail the tag"
    );
    let message = stderr
        .lines()
        .find(|l| l.contains("matched nothing"))
        .unwrap_or_else(|| panic!("no unmatched-anchor error: {stderr}"));
    assert!(
        message.contains("enrolled charts/values.yaml with match"),
        "error must name the enrolled repo-relative path: {message}"
    );
    assert!(
        !message.contains(&root.to_string_lossy().into_owned()),
        "error leaked the resolved absolute path: {message}"
    );
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

/// A no-`crates:` repo at `1.2.3` enrolling one `chart.yaml` twice — once bare,
/// once anchored on its `pin:` line — with one `fix:` commit after `v1.2.3`.
fn bare_and_anchored_single_crate_fixture(root: &Path) {
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
}

/// Single-crate, one file enrolled bare AND anchored under one ordinary bump:
/// the anchored entry claims its `pin:` line, the bare sweep takes the rest,
/// and neither byte is rewritten twice.
#[test]
fn single_crate_bare_and_anchored_share_one_file() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    bare_and_anchored_single_crate_fixture(root);

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--no-push"])
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "tag failed: {combined}");
    let expected = "pin: v1.2.4\nother: 1.2.4\n";
    assert_eq!(read(root, "chart.yaml"), expected);
    assert_eq!(show_head(root, "chart.yaml"), expected);
}

/// A prerelease target still matches its own old version — `1.2.3` fires inside
/// `1.2.3-rc1` — but the two entries express ONE owner's ONE bump, so they are
/// one rewrite: the anchored entry claims its `pin:` line first and the bare
/// sweep takes the rest, each byte rewritten exactly once.
#[test]
fn single_crate_bare_plus_anchored_prerelease_rewrites_both() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    bare_and_anchored_single_crate_fixture(root);

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--version", "1.2.3-rc1", "--no-push"])
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "tag failed: {combined}");
    let expected = "pin: v1.2.3-rc1\nother: 1.2.3-rc1\n";
    assert_eq!(read(root, "chart.yaml"), expected);
    assert_eq!(show_head(root, "chart.yaml"), expected);
}

/// The same shape in lockstep mode, where the top-level enrollment is shared by
/// every workspace crate under one version: still one owner, one pair, one
/// rewrite per occurrence.
#[test]
fn lockstep_bare_plus_anchored_prerelease_rewrites_both() {
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
        .args(["tag", "--version", "2.0.0-rc1", "--no-push"])
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "tag failed: {combined}");
    let expected = "pin: v2.0.0-rc1\nother: 2.0.0-rc1\n";
    assert_eq!(read(root, "chart.yaml"), expected);
    assert_eq!(show_head(root, "chart.yaml"), expected);
}

/// Per-crate, two crates on the same bump, one enrolling the shared file bare
/// and one with an anchor. The guard passes (same pair, no chain), so the apply
/// must too: selecting the anchor against a partially rewritten copy made the
/// bare sweep consume the anchored region and the tag failed with "matched
/// nothing".
#[test]
fn per_crate_bare_and_anchored_share_one_file() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    shared_file_fixture_enrolled(
        root,
        &[("core", "0.9.0"), ("cli", "0.9.0")],
        "pin: v0.9.0\nother: 0.9.0\n",
        &[
            "      - shared.md\n",
            "      - path: shared.md\n        match: 'pin: v{version}'\n",
        ],
    );

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--no-push"])
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "tag failed: {combined}");
    let expected = "pin: v0.10.0\nother: 0.10.0\n";
    assert_eq!(read(root, "shared.md"), expected);
    assert_eq!(show_head(root, "shared.md"), expected);
}

// ---------------------------------------------------------------------------
// One declared crate, tagged without `--crate`
// ---------------------------------------------------------------------------

/// A workspace declaring exactly ONE crate that enrolls nothing of its own,
/// with a TOP-LEVEL `version_files` list, at 0.1.0 with a `fix:` commit after
/// `v0.1.0`. `check version-files` resolves the top-level list as that crate's
/// list, so the repo-level tag path must rewrite it.
fn one_declared_crate_fixture(root: &Path) {
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
    fs::write(root.join("README.md"), "app is at 0.1.0\n").unwrap();
    fs::write(
        root.join(".anodizer.yaml"),
        r#"project_name: app
crates:
  - name: app
    path: crates/app
    tag_template: "v{{ .Version }}"
    version_sync:
      enabled: true
version_files:
  - README.md
"#,
    )
    .unwrap();

    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "v0.1.0"]);
    fs::write(root.join("crates/app/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(root, "fix: a bug");
}

/// The single declared crate's bump must carry the top-level enrollment. It
/// used to be dropped — the repo-level path planned the top-level list only
/// when `crates:` was absent entirely — so `check version-files` validated a
/// file `tag` never rewrote and the repo went stale on its own release.
#[test]
fn one_declared_crate_rewrites_top_level_version_files() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    one_declared_crate_fixture(root);

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--dry-run"])
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "tag failed: {combined}");
    assert!(
        combined.contains("rewrote 1 occurrence(s) of 0.1.0 → 0.1.1 in README.md"),
        "top-level entry not planned for the one declared crate: {combined}"
    );
    assert_eq!(read(root, "README.md"), "app is at 0.1.0\n");

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
    assert_eq!(read(root, "README.md"), "app is at 0.1.1\n");
    assert_eq!(show_head(root, "README.md"), "app is at 0.1.1\n");
}

/// The `(file, version)` pairs `check version-files` actually validated, taken
/// from its own `-v` report rather than a hand-written list, plus a cross-check
/// that the count it reports matches the number of lines it printed.
fn check_validated(root: &Path, mode: &str) -> Vec<(String, String)> {
    let out = anodizer()
        .current_dir(root)
        .args(["check", "version-files", "--verbose"])
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{mode}: check failed: {combined}");

    let mut pairs: Vec<(String, String)> = combined
        .lines()
        .filter_map(|line| {
            let (file, rest) = line.split_once(" contains ")?;
            let file = file.split_whitespace().last()?;
            let version = rest.split_whitespace().next()?;
            Some((file.to_string(), version.to_string()))
        })
        .collect();
    let reported: usize = combined
        .lines()
        .find_map(|l| {
            l.split_once("all ")?
                .1
                .split_once(" version_files are in sync")
        })
        .and_then(|(n, _)| n.trim().parse().ok())
        .unwrap_or_else(|| panic!("{mode}: check reported no in-sync count: {combined}"));
    assert_eq!(
        reported,
        pairs.len(),
        "{mode}: check counted {reported} files but reported {} of them: {combined}",
        pairs.len()
    );
    pairs.sort();
    pairs
}

/// The `(file, old_version)` pairs a `tag` run planned to rewrite, taken from
/// its own rewrite lines.
fn tag_rewrote(root: &Path, mode: &str, tag_args: &[&str]) -> Vec<(String, String)> {
    let out = anodizer()
        .current_dir(root)
        .args(tag_args)
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{mode}: tag failed: {combined}");

    let mut pairs: Vec<(String, String)> = combined
        .lines()
        .filter_map(|line| {
            let rest = line.split_once(" occurrence(s) of ")?.1;
            let (old, rest) = rest.split_once(' ')?;
            let file = rest.split_once(" in ")?.1;
            Some((file.trim().to_string(), old.to_string()))
        })
        .collect();
    pairs.sort();
    pairs
}

/// Both commands resolve the same enrollment, so the files `check` validates
/// and the files `tag` rewrites are the same set at the same versions — the
/// pin derives BOTH lists from the commands' own output, so a resolver that
/// drifts on one side fails here without anyone updating a fixture list.
fn assert_tag_covers_what_check_validates(root: &Path, mode: &str, tag_args: &[&str]) {
    let checked = check_validated(root, mode);
    assert!(
        !checked.is_empty(),
        "{mode}: check validated no enrolled file"
    );
    let rewritten = tag_rewrote(root, mode, tag_args);
    assert_eq!(
        checked, rewritten,
        "{mode}: check validates one set of version_files and tag rewrites another"
    );
}

/// The tag/check agreement across every config mode: single-crate, lockstep
/// with and without a `crates:` block, per-crate, and the one-declared-crate
/// shape whose enrollment lives at the top level. Both divergences found here
/// were resolver splits — `check` walked one list and `tag` another.
#[test]
fn tag_and_check_agree_in_every_config_mode() {
    let single = TempDir::new().unwrap();
    single_crate_fixture(single.path(), "appVersion: v0.1.0\n", "      - Chart.yaml");
    assert_tag_covers_what_check_validates(
        single.path(),
        "single-crate",
        &["tag", "--crate", "app", "--dry-run"],
    );

    let lockstep = TempDir::new().unwrap();
    lockstep_fixture(lockstep.path());
    assert_tag_covers_what_check_validates(lockstep.path(), "lockstep", &["tag", "--dry-run"]);

    let lockstep_crates = TempDir::new().unwrap();
    lockstep_with_crates_fixture(lockstep_crates.path());
    assert_tag_covers_what_check_validates(
        lockstep_crates.path(),
        "lockstep with crates:",
        &["tag", "--dry-run"],
    );

    let per_crate = TempDir::new().unwrap();
    shared_file_fixture(
        per_crate.path(),
        &[("core", "0.1.0"), ("cli", "0.1.0")],
        "both at 0.1.0\n",
    );
    assert_tag_covers_what_check_validates(per_crate.path(), "per-crate", &["tag", "--dry-run"]);

    let one_crate = TempDir::new().unwrap();
    one_declared_crate_fixture(one_crate.path());
    assert_tag_covers_what_check_validates(
        one_crate.path(),
        "one declared crate",
        &["tag", "--dry-run"],
    );
}

/// A lockstep workspace that ALSO declares its crate under `crates:`, where the
/// crate enrolls its own file and a stale top-level list enrolls another. Only
/// the crate's own list is the enrollment — the top-level list is the fallback
/// for crates that declare none — so both commands must land on `OWN.md` and
/// leave `TOP.md` alone.
fn lockstep_with_crates_fixture(root: &Path) {
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
    fs::write(root.join("OWN.md"), "a is at 0.1.0\n").unwrap();
    fs::write(root.join("TOP.md"), "top says 0.1.0\n").unwrap();
    fs::write(
        root.join(".anodizer.yaml"),
        r#"project_name: lockstep
crates:
  - name: a
    path: crates/a
    version_files:
      - OWN.md
version_files:
  - TOP.md
"#,
    )
    .unwrap();
    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "v0.1.0"]);
    fs::write(root.join("crates/a/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(root, "fix: a bug");
}

/// A lockstep workspace (`[workspace.package].version = "0.1.0"`, no `crates:`
/// block) enrolling one top-level `Chart.yaml`, with a `fix:` commit after
/// `v0.1.0`.
fn lockstep_fixture(root: &Path) {
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
}

/// A flat repo with no `crates:` block: root `[package].version` at 1.2.3, one
/// enrolled README, one `fix:` commit after `v1.2.3`.
fn no_crates_block_fixture(root: &Path) {
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"1.2.3\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(root.join("README.md"), "Install app 1.2.3 today.\n").unwrap();
    fs::write(
        root.join(".anodizer.yaml"),
        "project_name: app\nversion_files:\n  - README.md\n",
    )
    .unwrap();

    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "v1.2.3"]);
    fs::write(root.join("src/main.rs"), "fn main() {}\n// touched\n").unwrap();
    git_add_commit(root, "fix: a bug");
}

/// Runs a real bump, then `check version-files`: the bump moves the manifest
/// and the enrolled files together, so a repo that was in sync before it is in
/// sync after it. A path that rewrote the files without writing the manifest
/// leaves a drift report no bump can clear.
fn assert_bump_leaves_check_in_sync(root: &Path, mode: &str, tag_args: &[&str]) {
    let out = anodizer()
        .current_dir(root)
        .args(tag_args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{mode}: tag failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let out = anodizer()
        .current_dir(root)
        .args(["check", "version-files"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{mode}: check went stale after its own bump: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Every shape a bump can take leaves `check version-files` green, because
/// every shape writes the new version into the manifest `check` reads.
#[test]
fn every_bump_leaves_check_version_files_in_sync() {
    let no_crates = TempDir::new().unwrap();
    no_crates_block_fixture(no_crates.path());
    assert_bump_leaves_check_in_sync(no_crates.path(), "no crates:", &["tag", "--no-push"]);

    let one_crate = TempDir::new().unwrap();
    one_declared_crate_fixture(one_crate.path());
    assert_bump_leaves_check_in_sync(
        one_crate.path(),
        "one declared crate",
        &["tag", "--no-push"],
    );

    let lockstep = TempDir::new().unwrap();
    lockstep_fixture(lockstep.path());
    assert_bump_leaves_check_in_sync(lockstep.path(), "lockstep", &["tag", "--no-push"]);

    let per_crate = TempDir::new().unwrap();
    shared_file_fixture(
        per_crate.path(),
        &[("core", "0.1.0"), ("cli", "0.1.0")],
        "both at 0.1.0\n",
    );
    assert_bump_leaves_check_in_sync(per_crate.path(), "per-crate", &["tag", "--no-push"]);
}

/// The repo-level bump writes no manifest when it owns none: several declared
/// crates dispatch elsewhere, and a lone declared crate that never opted into
/// `version_sync` keeps its manifest (and, with it, its enrolled files) untouched.
#[test]
fn repo_level_manifest_is_the_one_check_reads() {
    let one_crate = TempDir::new().unwrap();
    one_declared_crate_fixture(one_crate.path());
    let out = anodizer()
        .current_dir(one_crate.path())
        .args(["tag", "--dry-run"])
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "tag failed: {combined}");
    assert!(
        combined.contains("would sync version in") && combined.contains("crates/app"),
        "the declared crate's manifest is not in the planned writes: {combined}"
    );
}

// ---------------------------------------------------------------------------
// One occurrence matcher for both commands
// ---------------------------------------------------------------------------

/// A flat repo at 1.2.3 whose enrolled file mentions only NEIGHBOURING version
/// strings: `1.2.3` appears as a substring of both, but never as its own word.
fn near_miss_fixture(root: &Path) {
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"1.2.3\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(root.join("README.md"), "pinned 11.2.3 and 1.2.34\n").unwrap();
    fs::write(
        root.join(".anodizer.yaml"),
        "project_name: app\nversion_files:\n  - README.md\n",
    )
    .unwrap();

    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "v1.2.3"]);
    fs::write(root.join("src/main.rs"), "fn main() {}\n// touched\n").unwrap();
    git_add_commit(root, "fix: a bug");
}

/// Presence is decided by ONE matcher for both commands, and it is word-bounded:
/// `1.2.3` inside `11.2.3` or `1.2.34` is a different version. `check` must
/// call the file stale and `tag` must rewrite none of it — a substring test on
/// either side would silently corrupt the neighbours.
#[test]
fn version_occurrences_are_word_bounded_on_both_commands() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    near_miss_fixture(root);

    let out = anodizer()
        .current_dir(root)
        .args(["check", "version-files"])
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "check treated a neighbouring version as present: {combined}"
    );
    assert!(
        combined.contains("STALE: README.md (expected 1.2.3, not found)"),
        "check did not name the drifted file: {combined}"
    );

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--no-push"])
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "tag failed: {combined}");
    assert_eq!(
        read(root, "README.md"),
        "pinned 11.2.3 and 1.2.34\n",
        "tag rewrote a neighbouring version"
    );
}
