//! Integration tests for `anodizer tag` workspace mode.
//!
//! When no `--crate` is given and the root Cargo.toml has
//! `[workspace.package].version`, the tag command treats the whole Cargo
//! workspace as a single versioned unit: rewrites the workspace package
//! version, every member's own version (for non-inheriting members), every
//! `[workspace.dependencies]` pin, and every sibling `[dependencies]` pin;
//! commits the edits; then creates the tag.

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
        "git {:?} failed: {}",
        args,
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

/// Write a two-member inheriting workspace at version 0.1.0 plus a
/// `[workspace.dependencies]` table that pins both members.
fn inheriting_workspace_with_deps(tmp: &Path) {
    fs::write(
        tmp.join("Cargo.toml"),
        r#"[workspace]
members = ["crates/a", "crates/b"]
resolver = "2"

[workspace.package]
version = "0.1.0"

[workspace.dependencies]
a = { path = "crates/a", version = "0.1.0" }
b = { path = "crates/b", version = "0.1.0" }
"#,
    )
    .unwrap();
    fs::create_dir_all(tmp.join("crates/a/src")).unwrap();
    fs::create_dir_all(tmp.join("crates/b/src")).unwrap();
    fs::write(
        tmp.join("crates/a/Cargo.toml"),
        r#"[package]
name = "a"
version.workspace = true
edition = "2024"
"#,
    )
    .unwrap();
    fs::write(tmp.join("crates/a/src/lib.rs"), "").unwrap();
    fs::write(
        tmp.join("crates/b/Cargo.toml"),
        r#"[package]
name = "b"
version.workspace = true
edition = "2024"

[dependencies]
a = { workspace = true }
"#,
    )
    .unwrap();
    fs::write(tmp.join("crates/b/src/lib.rs"), "").unwrap();
}

fn read_workspace_package_version(root: &Path) -> String {
    let text = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let doc = text.parse::<toml_edit::DocumentMut>().unwrap();
    doc.get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string()
}

fn read_workspace_dep_version(root: &Path, name: &str) -> Option<String> {
    let text = fs::read_to_string(root.join("Cargo.toml")).ok()?;
    let doc = text.parse::<toml_edit::DocumentMut>().ok()?;
    let dep = doc.get("workspace")?.get("dependencies")?.get(name)?;
    if let Some(s) = dep.as_str() {
        return Some(s.to_string());
    }
    if let Some(t) = dep.as_inline_table() {
        return t.get("version").and_then(|v| v.as_str()).map(String::from);
    }
    if let Some(t) = dep.as_table() {
        return t.get("version").and_then(|v| v.as_str()).map(String::from);
    }
    None
}

fn git_tag_exists(dir: &Path, tag: &str) -> bool {
    let out = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.current_dir(dir).args(["tag", "-l", tag]);
            cmd
        },
        "git",
    );
    !String::from_utf8_lossy(&out.stdout).trim().is_empty()
}

#[test]
fn workspace_mode_bumps_inheriting_members_and_dep_pins() {
    let tmp = TempDir::new().unwrap();
    inheriting_workspace_with_deps(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");
    // v0.1.0 baseline tag exists, then a patch-worthy commit.
    run_git(tmp.path(), &["tag", "v0.1.0"]);
    fs::write(tmp.path().join("crates/a/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(tmp.path(), "fix: a deref issue");

    let out = anodizer()
        .current_dir(tmp.path())
        .args(["tag"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tag failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("new_tag=v0.1.1"),
        "expected new_tag=v0.1.1 in stdout: {stdout}"
    );

    assert_eq!(read_workspace_package_version(tmp.path()), "0.1.1");
    assert_eq!(
        read_workspace_dep_version(tmp.path(), "a").as_deref(),
        Some("0.1.1")
    );
    assert_eq!(
        read_workspace_dep_version(tmp.path(), "b").as_deref(),
        Some("0.1.1")
    );
    assert!(
        git_tag_exists(tmp.path(), "v0.1.1"),
        "v0.1.1 should be created"
    );
}

#[test]
fn workspace_mode_dry_run_touches_nothing() {
    let tmp = TempDir::new().unwrap();
    inheriting_workspace_with_deps(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");
    run_git(tmp.path(), &["tag", "v0.1.0"]);
    fs::write(tmp.path().join("crates/a/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(tmp.path(), "feat: shiny new thing");

    let before_root = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
    let out = anodizer()
        .current_dir(tmp.path())
        .args(["tag", "--dry-run"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tag --dry-run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let after_root = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
    assert_eq!(before_root, after_root, "--dry-run must not edit manifests");
    assert!(
        !git_tag_exists(tmp.path(), "v0.2.0"),
        "--dry-run must not create a tag"
    );
}

#[test]
fn workspace_mode_skipped_when_crate_flag_used() {
    // With --crate naming a crate that no configuration defines (the
    // Cargo.toml fallback config has an empty crate universe), tag must
    // hard-error with remediation rather than silently fall through to a
    // repo-wide tag the user explicitly scoped away from — and the
    // workspace must be untouched.
    let tmp = TempDir::new().unwrap();
    inheriting_workspace_with_deps(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");
    run_git(tmp.path(), &["tag", "a-v0.1.0"]);
    fs::write(tmp.path().join("crates/a/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(tmp.path(), "fix: a thing");

    let out = anodizer()
        .current_dir(tmp.path())
        .args(["tag", "--crate", "a"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "--crate with no configured crates must be a hard error"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("defines no crates") && stderr.contains("drop --crate"),
        "the empty-universe error must carry remediation: {stderr}"
    );
    assert_eq!(read_workspace_package_version(tmp.path()), "0.1.0");
    assert_eq!(
        read_workspace_dep_version(tmp.path(), "a").as_deref(),
        Some("0.1.0")
    );
}

#[test]
fn workspace_mode_skips_when_already_at_target() {
    // Manually-bumped Cargo.toml already at the next version: tag must still
    // create the tag but not create a redundant bump commit.
    let tmp = TempDir::new().unwrap();
    inheriting_workspace_with_deps(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");
    run_git(tmp.path(), &["tag", "v0.1.0"]);

    // Hand-bump root Cargo.toml to 0.1.1 and commit. Use `#patch` so the
    // detected bump matches the manual value; otherwise the default_bump
    // fallback would minor-bump past 0.1.1.
    let mut root = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
    root = root.replace("version = \"0.1.0\"", "version = \"0.1.1\"");
    fs::write(tmp.path().join("Cargo.toml"), &root).unwrap();
    git_add_commit(tmp.path(), "chore: bump workspace manually #patch");

    let head_before = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.current_dir(tmp.path()).args(["rev-parse", "HEAD"]);
            cmd
        },
        "git",
    );
    let head_before = String::from_utf8_lossy(&head_before.stdout)
        .trim()
        .to_string();

    let out = anodizer()
        .current_dir(tmp.path())
        .args(["tag"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tag failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("new_tag=v0.1.1"),
        "expected new_tag=v0.1.1: {stdout}"
    );

    let head_after = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.current_dir(tmp.path()).args(["rev-parse", "HEAD"]);
            cmd
        },
        "git",
    );
    let head_after = String::from_utf8_lossy(&head_after.stdout)
        .trim()
        .to_string();

    // No redundant bump commit should be created — workspace already at target.
    assert_eq!(
        head_before, head_after,
        "workspace already at target version should not add a bump commit"
    );
    assert!(git_tag_exists(tmp.path(), "v0.1.1"));
}

// ---------------------------------------------------------------------------
// Subdirectory invocation: `tag --dry-run` must resolve the workspace root
// from the config (not the cwd), so a run from `crates/<x>` previews the same
// tag as a run from the repo root — across single, lockstep, and per-crate
// modes. Regression guard for the cwd-as-root bug (known-bugs #4): before the
// fix, `discover_workspace_root` fell back to the cwd ancestor, so from a
// member directory it resolved the member's own `Cargo.toml` as the root and
// mis-detected the repo shape.
// ---------------------------------------------------------------------------

struct DryRunResult {
    stdout: String,
    success: bool,
}

/// Run `tag --dry-run -q` against an explicit `--config` (the root
/// `.anodizer.yaml`) from `run_dir`. The explicit config isolates the
/// workspace-root concern: the fix walks up from the config's parent to the
/// root, so the same config resolves the same root from any subdirectory.
fn tag_dry_run_from(run_dir: &Path, config: &Path) -> DryRunResult {
    let out = anodizer()
        .current_dir(run_dir)
        .args([
            "tag",
            "--dry-run",
            "-q",
            "--config",
            config.to_str().expect("utf8 config path"),
        ])
        .output()
        .unwrap();
    DryRunResult {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr),
        success: out.status.success(),
    }
}

/// Normalize the `anodizer-output versions={...}` line so two runs compare
/// equal regardless of the map's hash-iteration order: sort the comma-separated
/// entries inside the braces. (The `crates=[...]` list is order-stable.)
fn normalize_versions(out: &str) -> String {
    out.lines()
        .map(
            |line| match line.strip_prefix("anodizer-output versions={") {
                Some(rest) => {
                    let body = rest.strip_suffix('}').unwrap_or(rest);
                    let mut entries: Vec<&str> =
                        body.split(',').filter(|s| !s.is_empty()).collect();
                    entries.sort_unstable();
                    format!("anodizer-output versions={{{}}}", entries.join(","))
                }
                None => line.to_string(),
            },
        )
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_tag_subdir_matches_root(root: &Path, subdir: &str) {
    let config = root.join(".anodizer.yaml");
    let from_root = tag_dry_run_from(root, &config);
    assert!(
        from_root.success,
        "root tag --dry-run failed: {}",
        from_root.stdout
    );
    let from_subdir = tag_dry_run_from(&root.join(subdir), &config);
    assert!(
        from_subdir.success,
        "subdir tag --dry-run failed: {}",
        from_subdir.stdout
    );
    assert_eq!(
        normalize_versions(&from_subdir.stdout),
        normalize_versions(&from_root.stdout),
        "tag --dry-run from {subdir} must match the repo-root preview \
         (workspace root resolved against cwd instead of the config)"
    );
}

#[test]
fn lockstep_tag_dry_run_from_subdir_matches_root() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    inheriting_workspace_with_deps(root);
    fs::write(root.join(".anodizer.yaml"), "project_name: ws\n").unwrap();
    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "v0.1.0"]);
    fs::write(root.join("crates/a/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(root, "fix: a deref issue");

    assert_tag_subdir_matches_root(root, "crates/a");
}

/// On a lockstep repo, `--crate <project_name>` names the shared-root
/// aggregate itself: it must behave byte-identically to a bare `tag`
/// (repo-level tagging), never fail crate-universe validation — the
/// aggregate's name is not a universe crate, but it IS the one selectable
/// spelling for the whole release unit.
#[test]
fn lockstep_tag_crate_project_name_matches_bare_tag() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    inheriting_workspace_with_deps(root);
    fs::write(root.join(".anodizer.yaml"), "project_name: ws\n").unwrap();
    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "v0.1.0"]);
    fs::write(root.join("crates/a/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(root, "fix: a deref issue");

    let config = root.join(".anodizer.yaml");
    let bare = tag_dry_run_from(root, &config);
    assert!(bare.success, "bare tag --dry-run failed: {}", bare.stdout);

    let out = anodizer()
        .current_dir(root)
        .args([
            "tag",
            "--crate",
            "ws",
            "--dry-run",
            "-q",
            "--config",
            config.to_str().expect("utf8 config path"),
        ])
        .output()
        .unwrap();
    let by_name =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "tag --crate ws --dry-run failed: {by_name}"
    );
    assert_eq!(
        normalize_versions(&by_name),
        normalize_versions(&bare.stdout),
        "tag --crate <project_name> must match the bare tag preview on a lockstep repo"
    );

    // An unknown name still hard-errors — the carve-out is exactly the
    // aggregate's own name, not a general fall-through.
    let out = anodizer()
        .current_dir(root)
        .args([
            "tag",
            "--crate",
            "nope",
            "--dry-run",
            "--config",
            config.to_str().expect("utf8 config path"),
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "an unknown --crate name must remain a hard error"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("nope"),
        "error must name the unknown crate: {stderr}"
    );
}

#[test]
fn single_crate_tag_dry_run_from_subdir_matches_root() {
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
        "project_name: single\ncrates:\n  - name: app\n    path: crates/app\n    tag_template: \"v{{ .Version }}\"\n    version_sync:\n      enabled: true\n",
    )
    .unwrap();
    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "v0.1.0"]);
    fs::write(root.join("crates/app/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(root, "feat: add a thing");

    assert_tag_subdir_matches_root(root, "crates/app");
}

#[test]
fn per_crate_tag_dry_run_from_subdir_matches_root() {
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
        "project_name: percrate\ncrates:\n  - name: core\n    path: crates/core\n    tag_template: \"core-v{{ .Version }}\"\n  - name: cli\n    path: crates/cli\n    tag_template: \"cli-v{{ .Version }}\"\n",
    )
    .unwrap();
    git_init(root);
    git_add_commit(root, "feat: initial release");
    // No per-crate tags: both crates read as "changed" without a per-crate
    // `git log -- <path>` pathspec, so the preview's selected-crate set is
    // independent of the change-detection cwd axis (a separate concern) and
    // isolates the workspace-root resolution this guard targets.

    assert_tag_subdir_matches_root(root, "crates/cli");
}

/// Per-crate workspace mode: `bump_minor_pre_major` demotes a conventional
/// breaking change to a minor for every crate still in 0.x. Proves the
/// demotion is wired through the per-crate tagging path end-to-end, not just
/// the lockstep helper.
#[test]
fn per_crate_bump_minor_pre_major_demotes_breaking_to_minor() {
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
            format!("[package]\nname = \"{name}\"\nversion = \"0.5.0\"\nedition = \"2024\"\n"),
        )
        .unwrap();
        fs::write(root.join(format!("crates/{name}/src/lib.rs")), "").unwrap();
    }
    fs::write(
        root.join(".anodizer.yaml"),
        "project_name: percrate\ntag:\n  bump_minor_pre_major: true\ncrates:\n  - name: core\n    path: crates/core\n    tag_template: \"core-v{{ .Version }}\"\n  - name: cli\n    path: crates/cli\n    tag_template: \"cli-v{{ .Version }}\"\n",
    )
    .unwrap();
    git_init(root);
    git_add_commit(root, "chore: initial");
    run_git(root, &["tag", "core-v0.5.0"]);
    run_git(root, &["tag", "cli-v0.5.0"]);
    // A breaking change touching both crate paths.
    fs::write(root.join("crates/core/src/lib.rs"), "// break\n").unwrap();
    fs::write(root.join("crates/cli/src/lib.rs"), "// break\n").unwrap();
    git_add_commit(root, "feat!: redo the api");

    let config = root.join(".anodizer.yaml");
    let res = tag_dry_run_from(root, &config);
    assert!(
        res.success,
        "per-crate tag --dry-run failed: {}",
        res.stdout
    );
    // Pre-1.0 breaking demotes to minor: 0.5.0 -> 0.6.0, never 1.0.0.
    assert!(
        res.stdout.contains("0.6.0"),
        "expected demoted 0.6.0 in per-crate preview, got:\n{}",
        res.stdout
    );
    assert!(
        !res.stdout.contains("1.0.0"),
        "breaking change must not force 1.0.0 under bump_minor_pre_major; got:\n{}",
        res.stdout
    );
}

// --- `--version` explicit override -----------------------------------------

/// Read a member crate's own `[package].version`.
fn read_member_version(root: &Path, member: &str) -> String {
    let text = fs::read_to_string(root.join(member).join("Cargo.toml")).unwrap();
    let doc = text.parse::<toml_edit::DocumentMut>().unwrap();
    doc.get("package")
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string()
}

/// Lockstep workspace where `--version` (in both `1.2.3` and `v1.2.3` forms)
/// pins the workspace version verbatim, bypassing autotag derivation.
fn lockstep_version_override(arg: &str, expected_tag: &str) {
    let tmp = TempDir::new().unwrap();
    inheriting_workspace_with_deps(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");
    run_git(tmp.path(), &["tag", "v0.1.0"]);
    fs::write(tmp.path().join("crates/a/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(tmp.path(), "fix: a deref issue");

    let out = anodizer()
        .current_dir(tmp.path())
        .args(["tag", "--version", arg])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tag --version {arg} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&format!("new_tag={expected_tag}")),
        "expected new_tag={expected_tag} in stdout: {stdout}"
    );
    // The explicit version (not the derived v0.1.1 patch) is synced into the
    // workspace manifest + dep pins before the tag.
    let bare = expected_tag.trim_start_matches('v');
    assert_eq!(read_workspace_package_version(tmp.path()), bare);
    assert_eq!(
        read_workspace_dep_version(tmp.path(), "a").as_deref(),
        Some(bare)
    );
    assert!(
        git_tag_exists(tmp.path(), expected_tag),
        "{expected_tag} should be created"
    );
    assert!(
        !git_tag_exists(tmp.path(), "v0.1.1"),
        "derived patch tag v0.1.1 must NOT be created when --version pins"
    );
}

#[test]
fn version_override_lockstep_bare_form() {
    lockstep_version_override("1.2.3", "v1.2.3");
}

#[test]
fn version_override_lockstep_v_prefixed_form() {
    lockstep_version_override("v1.2.3", "v1.2.3");
}

/// Lockstep workspace where `--version` overrides the Cargo.toml-ahead guard.
/// The workspace manifest sits at 0.4.0 (ahead of the v0.2.0 tag, so autotag's
/// guard would lift the derived patch 0.2.1 to 0.4.0), yet `--version 0.3.0`
/// wins and a warning naming the derived 0.4.0 is emitted.
#[test]
fn version_override_beats_cargo_ahead_guard_and_warns() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    // A lockstep workspace whose [workspace.package].version is manually ahead
    // of the previous tag, the exact condition the Cargo.toml-ahead guard fires
    // on.
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/a\"]\nresolver = \"2\"\n\n[workspace.package]\nversion = \"0.4.0\"\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("crates/a/src")).unwrap();
    fs::write(
        root.join("crates/a/Cargo.toml"),
        "[package]\nname = \"a\"\nversion.workspace = true\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(root.join("crates/a/src/lib.rs"), "").unwrap();
    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "v0.2.0"]);
    fs::write(root.join("crates/a/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(root, "fix: something");

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--version", "0.3.0"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tag --version failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let combined =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(
        combined.contains("new_tag=v0.3.0"),
        "explicit v0.3.0 must win over Cargo-ahead 0.4.0: {combined}"
    );
    // The warning names the explicit version and the fully-derived version
    // (0.4.0 — the value the Cargo.toml-ahead guard would have produced).
    assert!(
        combined.contains("--version 0.3.0 overrides the derived version 0.4.0"),
        "expected a warning naming explicit 0.3.0 and derived 0.4.0: {combined}"
    );
    assert_eq!(read_workspace_package_version(root), "0.3.0");
    assert!(git_tag_exists(root, "v0.3.0"));
    assert!(!git_tag_exists(root, "v0.4.0"));
}

/// An ill-formed `--version` value fails cleanly with a non-zero exit before
/// any tag is created.
#[test]
fn version_override_invalid_errors() {
    let tmp = TempDir::new().unwrap();
    inheriting_workspace_with_deps(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");

    let out = anodizer()
        .current_dir(tmp.path())
        .args(["tag", "--version", "not-a-version"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "invalid --version must exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not a valid semver"),
        "expected a semver-validation error: {stderr}"
    );
}

// --- dependency-floor healing ----------------------------------------------

/// Read a member manifest's `[dependencies].<dep>.version`.
fn read_dep_version(root: &Path, manifest_rel: &str, dep_name: &str) -> String {
    let text = fs::read_to_string(root.join(manifest_rel)).unwrap();
    let doc = text.parse::<toml_edit::DocumentMut>().unwrap();
    let dep = doc
        .get("dependencies")
        .and_then(|d| d.get(dep_name))
        .unwrap_or_else(|| panic!("{manifest_rel}: [dependencies].{dep_name} not found"));
    if let Some(s) = dep.as_str() {
        return s.to_string();
    }
    dep.get("version")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("{manifest_rel}: [dependencies].{dep_name}.version not found"))
        .to_string()
}

/// Lockstep workspace whose member `c` already carries the version the next tag
/// resolves to (so its plan row is `Skip`), while `b` still floors it at a
/// pre-bump `0.0.1`.
fn lockstep_workspace_with_skipped_member(tmp: &Path) {
    fs::write(
        tmp.join("Cargo.toml"),
        r#"[workspace]
members = ["crates/a", "crates/b", "crates/c"]
resolver = "2"

[workspace.package]
version = "0.1.0"

[workspace.dependencies]
a = { path = "crates/a", version = "0.1.0" }
"#,
    )
    .unwrap();
    for name in ["a", "b", "c"] {
        fs::create_dir_all(tmp.join(format!("crates/{name}/src"))).unwrap();
        fs::write(tmp.join(format!("crates/{name}/src/lib.rs")), "").unwrap();
    }
    fs::write(
        tmp.join("crates/a/Cargo.toml"),
        "[package]\nname = \"a\"\nversion.workspace = true\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(
        tmp.join("crates/b/Cargo.toml"),
        r#"[package]
name = "b"
version.workspace = true
edition = "2024"

[dependencies]
a = { workspace = true }
c = { path = "../c", version = "0.0.1" }
"#,
    )
    .unwrap();
    fs::write(
        tmp.join("crates/c/Cargo.toml"),
        "[package]\nname = \"c\"\nversion = \"0.2.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
}

/// A lockstep member already at the target version is skipped by the plan, so
/// the same-run propagation never looks at floors that reference it. The bump
/// commit must still raise them.
#[test]
fn lockstep_bump_heals_stale_floor_on_skipped_member() {
    let tmp = TempDir::new().unwrap();
    lockstep_workspace_with_skipped_member(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");
    run_git(tmp.path(), &["tag", "v0.1.0"]);
    fs::write(tmp.path().join("crates/a/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(tmp.path(), "feat: a shiny thing");

    let out = anodizer()
        .current_dir(tmp.path())
        .args(["tag"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tag failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(read_workspace_package_version(tmp.path()), "0.2.0");
    // c was never bumped (it was already at 0.2.0), yet b's floor on it heals.
    assert_eq!(read_member_version(tmp.path(), "crates/c"), "0.2.0");
    assert_eq!(
        read_dep_version(tmp.path(), "crates/b/Cargo.toml", "c"),
        "0.2.0"
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("healed dep floor c 0.0.1 → 0.2.0"),
        "the heal must be reported: {stderr}"
    );
    assert!(
        !stderr.contains("healed dep floor a "),
        "a floor the same-run propagation owns must not be reported as a heal: {stderr}"
    );

    let show = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.current_dir(tmp.path()).args([
                "show",
                "--stat",
                "--name-only",
                "--format=",
                "HEAD",
            ]);
            cmd
        },
        "git",
    );
    let files = String::from_utf8_lossy(&show.stdout);
    assert!(
        files.contains("crates/b/Cargo.toml"),
        "the healed manifest must be inside the bump commit: {files}"
    );
}

/// The same heal, previewed: `--dry-run` names the floor it would raise and
/// leaves the manifest byte-for-byte alone.
#[test]
fn lockstep_dry_run_previews_heal_and_writes_nothing() {
    let tmp = TempDir::new().unwrap();
    lockstep_workspace_with_skipped_member(tmp.path());
    git_init(tmp.path());
    git_add_commit(tmp.path(), "initial");
    run_git(tmp.path(), &["tag", "v0.1.0"]);
    fs::write(tmp.path().join("crates/a/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(tmp.path(), "feat: a shiny thing");

    let before = fs::read_to_string(tmp.path().join("crates/b/Cargo.toml")).unwrap();
    let out = anodizer()
        .current_dir(tmp.path())
        .args(["tag", "--dry-run"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tag --dry-run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("(dry-run) would heal dep floor c 0.0.1 → 0.2.0"),
        "dry-run must preview the heal: {stderr}"
    );
    // `a` is bumped by this run, so its floor belongs to the same-run
    // propagation; previewing it as a heal would announce an edit the real run
    // attributes elsewhere.
    assert!(
        !stderr.contains("would heal dep floor a "),
        "a floor the same-run propagation owns must not be previewed as a heal: {stderr}"
    );
    assert_eq!(
        fs::read_to_string(tmp.path().join("crates/b/Cargo.toml")).unwrap(),
        before,
        "--dry-run must not edit manifests"
    );
}

/// Single-crate mode: the configured crate's own bump propagates its own
/// version, but a floor on an unreleased sibling is outside that propagation.
/// The bump commit heals it against the sibling's manifest version.
#[test]
fn single_crate_bump_heals_floor_on_unreleased_sibling() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/app\", \"crates/util\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    for (name, ver) in [("app", "0.1.0"), ("util", "0.5.0")] {
        fs::create_dir_all(root.join(format!("crates/{name}/src"))).unwrap();
        fs::write(root.join(format!("crates/{name}/src/lib.rs")), "").unwrap();
        fs::write(
            root.join(format!("crates/{name}/Cargo.toml")),
            format!("[package]\nname = \"{name}\"\nversion = \"{ver}\"\nedition = \"2024\"\n"),
        )
        .unwrap();
    }
    fs::write(
        root.join("crates/app/Cargo.toml"),
        r#"[package]
name = "app"
version = "0.1.0"
edition = "2024"

[dependencies]
util = { path = "../util", version = "0.1.0" }
"#,
    )
    .unwrap();
    fs::write(
        root.join(".anodizer.yaml"),
        "project_name: single\ncrates:\n  - name: app\n    path: crates/app\n    tag_template: \"v{{ .Version }}\"\n    version_sync:\n      enabled: true\n",
    )
    .unwrap();
    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "v0.1.0"]);
    fs::write(root.join("crates/app/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(root, "feat: app change");

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--crate", "app"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tag failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(read_member_version(root, "crates/app"), "0.2.0");
    assert_eq!(read_member_version(root, "crates/util"), "0.5.0");
    assert_eq!(
        read_dep_version(root, "crates/app/Cargo.toml", "util"),
        "0.5.0"
    );
}

/// The stranded-bump topology: a bump commit lands its tag but never reaches
/// the branch, so the branch keeps a floor that references a sibling at a
/// version the sibling's manifest has already moved past. The next run's bump
/// commit must heal it even though that sibling is not part of the run.
#[test]
fn stranded_bump_master_behind_latest_tag_heals() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/core\", \"crates/cli\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    for name in ["core", "cli"] {
        fs::create_dir_all(root.join(format!("crates/{name}/src"))).unwrap();
        fs::write(root.join(format!("crates/{name}/src/lib.rs")), "").unwrap();
    }
    fs::write(
        root.join("crates/core/Cargo.toml"),
        "[package]\nname = \"core\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(
        root.join("crates/cli/Cargo.toml"),
        r#"[package]
name = "cli"
version = "0.1.0"
edition = "2024"

[dependencies]
core = { path = "../core", version = "0.1.0" }
"#,
    )
    .unwrap();
    fs::write(
        root.join(".anodizer.yaml"),
        "project_name: stranded\ncrates:\n  - name: core\n    path: crates/core\n    tag_template: \"core-v{{ .Version }}\"\n    version_sync:\n      enabled: true\n  - name: cli\n    path: crates/cli\n    tag_template: \"cli-v{{ .Version }}\"\n    version_sync:\n      enabled: true\n",
    )
    .unwrap();
    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "core-v0.1.0"]);
    run_git(root, &["tag", "cli-v0.1.0"]);

    // Release core: its bump commit carries core 0.2.0 AND cli's 0.2.0 floor.
    fs::write(root.join("crates/core/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(root, "feat: core change");
    let out = anodizer().current_dir(root).args(["tag"]).output().unwrap();
    assert!(
        out.status.success(),
        "first tag failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(git_tag_exists(root, "core-v0.2.0"));
    assert_eq!(
        read_dep_version(root, "crates/cli/Cargo.toml", "core"),
        "0.2.0"
    );

    // The branch never absorbs that bump commit; the tag keeps pointing at it.
    run_git(root, &["reset", "--hard", "HEAD~1"]);
    // Only core's own version is restored onto the branch — the floor drift the
    // stranded commit was carrying stays behind.
    fs::write(
        root.join("crates/core/Cargo.toml"),
        "[package]\nname = \"core\"\nversion = \"0.2.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    git_add_commit(root, "chore: restore the version the stranded bump carried");
    assert_eq!(
        read_dep_version(root, "crates/cli/Cargo.toml", "core"),
        "0.1.0",
        "the branch must start this run with the drifted floor"
    );

    // Release cli only. core is not in this run, so nothing propagates its
    // version — the heal is the only thing that can fix cli's floor.
    fs::write(root.join("crates/cli/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(root, "feat: cli change");
    let out = anodizer().current_dir(root).args(["tag"]).output().unwrap();
    assert!(
        out.status.success(),
        "second tag failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(git_tag_exists(root, "cli-v0.2.0"));
    assert_eq!(read_member_version(root, "crates/core"), "0.2.0");
    assert_eq!(
        read_dep_version(root, "crates/cli/Cargo.toml", "core"),
        "0.2.0",
        "the stranded floor must heal against core's manifest version"
    );
}

/// Build a per-crate (independently versioned) workspace with two distinctly
/// prefixed crates and a baseline tag for each.
fn per_crate_workspace(root: &Path) {
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
        "project_name: percrate\ncrates:\n  - name: core\n    path: crates/core\n    tag_template: \"core-v{{ .Version }}\"\n    version_sync:\n      enabled: true\n  - name: cli\n    path: crates/cli\n    tag_template: \"cli-v{{ .Version }}\"\n    version_sync:\n      enabled: true\n",
    )
    .unwrap();
    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "core-v0.1.0"]);
    run_git(root, &["tag", "cli-v0.2.0"]);
}

/// A bare `--version` in per-crate mode is rejected — one version across
/// independently versioned crates would corrupt their cadences — with guidance
/// pointing at the `--crate` selector.
#[test]
fn version_override_rejected_in_per_crate_mode() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    per_crate_workspace(root);
    fs::write(root.join("crates/core/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(root, "feat: core change");

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--version", "5.0.0"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "--version without --crate must be rejected in per-crate mode"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("per-crate") && stderr.contains("--crate"),
        "rejection must point at the --crate alternative: {stderr}"
    );
    assert!(!git_tag_exists(root, "core-v5.0.0"));
    assert!(!git_tag_exists(root, "cli-v5.0.0"));
}

/// The submitter moderation-queue advisory is verbose-only: hidden at the
/// default log level, and printed exactly once per invocation under `--verbose`
/// on BOTH tag paths. The `--crate` path used to re-load the config inside the
/// crate lookup, which would double the advisory if it re-emitted per load.
#[test]
fn tag_crate_path_emits_static_config_warnings_once() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    per_crate_workspace(root);
    fs::write(
        root.join(".anodizer.yaml"),
        "project_name: percrate\ncrates:\n  - name: core\n    path: crates/core\n    tag_template: \"core-v{{ .Version }}\"\n    version_sync:\n      enabled: true\n    publish:\n      chocolatey:\n        required: true\n  - name: cli\n    path: crates/cli\n    tag_template: \"cli-v{{ .Version }}\"\n    version_sync:\n      enabled: true\n",
    )
    .unwrap();
    git_add_commit(root, "chore: require chocolatey on core");
    fs::write(root.join("crates/core/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(root, "feat: core change");

    for base in [
        vec!["tag", "--dry-run"],
        vec!["tag", "--crate", "core", "--dry-run"],
    ] {
        // Default log level: the advisory is hidden.
        let out = anodizer().current_dir(root).args(&base).output().unwrap();
        assert!(
            out.status.success(),
            "{base:?} failed: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            stderr.matches("publisher 'chocolatey'").count(),
            0,
            "moderation-queue advisory must be hidden at the default level for {base:?}: {stderr}"
        );

        // `--verbose`: the advisory surfaces exactly once (no per-load doubling).
        let mut verbose_args = base.clone();
        verbose_args.push("--verbose");
        let out = anodizer()
            .current_dir(root)
            .args(&verbose_args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{verbose_args:?} failed: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            stderr.matches("publisher 'chocolatey'").count(),
            1,
            "moderation-queue advisory must print exactly once under --verbose for {verbose_args:?}: {stderr}"
        );
    }
}

/// `--version` WITH `--crate` in per-crate mode pins exactly that one crate's
/// version, leaving its siblings untouched.
#[test]
fn version_override_with_crate_in_per_crate_mode() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    per_crate_workspace(root);
    fs::write(root.join("crates/core/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(root, "feat: core change");

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--crate", "core", "--version", "5.0.0"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tag --crate core --version 5.0.0 failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("new_tag=core-v5.0.0"),
        "expected new_tag=core-v5.0.0: {stdout}"
    );
    assert!(git_tag_exists(root, "core-v5.0.0"));
    assert_eq!(read_member_version(root, "crates/core"), "5.0.0");
    // The sibling crate is untouched: no cli tag, manifest stays at 0.2.0.
    assert!(!git_tag_exists(root, "cli-v5.0.0"));
    assert_eq!(read_member_version(root, "crates/cli"), "0.2.0");
}

// ---------------------------------------------------------------------------
// Mixed configs: top-level `crates:` alongside `workspaces:`
// ---------------------------------------------------------------------------

/// Mixed shape, shared-prefix top-levels: `alpha` + `beta` (both
/// `v{{ Version }}`) next to a workspace crate. A feat on alpha and a fix on
/// beta must cut ONE shared v0.2.0 — never divergent v0.2.0 AND v0.1.1 tags
/// into the same `v*` namespace (which would cross-contaminate every member's
/// next change detection).
#[test]
fn mixed_shared_prefix_top_levels_cut_one_shared_tag() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/alpha\", \"crates/beta\", \"tools/gamma\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    for (name, dir) in [
        ("alpha", "crates/alpha"),
        ("beta", "crates/beta"),
        ("gamma", "tools/gamma"),
    ] {
        fs::create_dir_all(root.join(dir).join("src")).unwrap();
        fs::write(
            root.join(dir).join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
        )
        .unwrap();
        fs::write(root.join(dir).join("src/lib.rs"), "").unwrap();
    }
    fs::write(
        root.join(".anodizer.yaml"),
        r#"project_name: mixed
crates:
  - name: alpha
    path: crates/alpha
    tag_template: "v{{ .Version }}"
  - name: beta
    path: crates/beta
    tag_template: "v{{ .Version }}"
workspaces:
  - name: tools
    crates:
      - name: gamma
        path: tools/gamma
        tag_template: "gamma-v{{ .Version }}"
"#,
    )
    .unwrap();
    git_init(root);
    git_add_commit(root, "chore: initial");
    run_git(root, &["tag", "v0.1.0"]);
    run_git(root, &["tag", "gamma-v0.1.0"]);

    fs::write(root.join("crates/alpha/src/lib.rs"), "// feature\n").unwrap();
    git_add_commit(root, "feat: alpha grows a feature");
    fs::write(root.join("crates/beta/src/lib.rs"), "// fix\n").unwrap();
    git_add_commit(root, "fix: beta stops crashing");

    let config = root.join(".anodizer.yaml");
    let res = tag_dry_run_from(root, &config);
    assert!(res.success, "mixed tag --dry-run failed: {}", res.stdout);
    // One aggregate group: both members land on the SAME 0.2.0 (feat wins).
    assert!(
        res.stdout.contains("\"alpha\":\"0.2.0\"") && res.stdout.contains("\"beta\":\"0.2.0\""),
        "alpha and beta must share one 0.2.0 version, got:\n{}",
        res.stdout
    );
    assert!(
        !res.stdout.contains("0.1.1"),
        "no divergent patch tag may be cut into the shared v* namespace, got:\n{}",
        res.stdout
    );
    // The untouched workspace crate stays quiet.
    assert!(
        !res.stdout.contains("\"gamma\""),
        "gamma had no changes and must not be tagged, got:\n{}",
        res.stdout
    );
}

/// A per-crate track's FIRST tag starts from the crate's own
/// `[package].version` (the Cargo-ahead guard, mirroring lockstep), not from
/// the `initial_version` 0.0.0 sentinel.
#[test]
fn per_crate_first_tag_starts_from_cargo_version() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/core\", \"crates/cli\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    for (name, ver) in [("core", "0.1.0"), ("cli", "0.3.0")] {
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
        "project_name: percrate\ncrates:\n  - name: core\n    path: crates/core\n    tag_template: \"core-v{{ .Version }}\"\n  - name: cli\n    path: crates/cli\n    tag_template: \"cli-v{{ .Version }}\"\n",
    )
    .unwrap();
    git_init(root);
    git_add_commit(root, "feat: initial release");

    let config = root.join(".anodizer.yaml");
    let res = tag_dry_run_from(root, &config);
    assert!(res.success, "first-tag dry-run failed: {}", res.stdout);
    assert!(
        res.stdout.contains("\"core\":\"0.1.0\"") && res.stdout.contains("\"cli\":\"0.3.0\""),
        "each first tag must start from that crate's [package].version, got:\n{}",
        res.stdout
    );
    assert!(
        !res.stdout.contains("0.0.0"),
        "the initial_version sentinel must not leak into a first tag when the manifest \
         carries a real version, got:\n{}",
        res.stdout
    );
}

/// `tag --crate <unknown>` is a hard error naming the known crates — never a
/// silent fall-through to REPO-LEVEL tagging (default `v` prefix, no path
/// scoping), which could cut and push a wrong repo-wide tag from a typo.
#[test]
fn tag_unknown_crate_hard_errors_naming_known_crates() {
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
        "project_name: percrate\ncrates:\n  - name: core\n    path: crates/core\n    tag_template: \"core-v{{ .Version }}\"\n  - name: cli\n    path: crates/cli\n    tag_template: \"cli-v{{ .Version }}\"\n",
    )
    .unwrap();
    git_init(root);
    git_add_commit(root, "feat: initial release");

    let out = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = anodizer();
            cmd.current_dir(root).args(["tag", "--crate", "nope"]);
            cmd
        },
        "anodizer",
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "tag --crate nope must fail, got stdout:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        stderr.contains("nope") && stderr.contains("core") && stderr.contains("cli"),
        "error must name the unknown crate and the known ones, got:\n{stderr}"
    );
    assert!(
        !git_tag_exists(root, "v0.1.0") && !git_tag_exists(root, "v0.2.0"),
        "no repo-level tag may be created from an unknown --crate"
    );
}

/// A one-crate `crates:` repo with no explicit `tag.tag_prefix` must tag in
/// that crate's own family (`app-v0.1.1`), not in the `v` family the
/// repo-level default would otherwise mint — every other surface (bump,
/// changelog, `--crate app`, the release stage) resolves the crate's
/// `tag_family_template()`.
#[test]
fn repo_level_tag_uses_the_crate_tag_family() {
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
        "project_name: single\ncrates:\n  - name: app\n    path: crates/app\n",
    )
    .unwrap();
    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "app-v0.1.0"]);
    fs::write(root.join("crates/app/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(root, "fix: a deref issue");

    let out = tag_dry_run_from(root, &root.join(".anodizer.yaml"));
    assert!(out.success, "tag --dry-run failed: {}", out.stdout);
    assert!(
        out.stdout.contains("new_tag=app-v0.1.1"),
        "expected new_tag=app-v0.1.1: {}",
        out.stdout
    );
}
