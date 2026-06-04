//! Integration tests for the unified `anodizer changelog` command.
//!
//! `anodizer changelog` refreshes the pending `## [Unreleased]` section
//! (`--format keep-a-changelog`, default), emits GitHub-body notes
//! (`--format release-notes`), or a JSON array (`--format json`). The refresh
//! path must work across all three config modes:
//!   1. single-crate (`crates:` with one entry + `version_sync`),
//!   2. workspace-lockstep (`[workspace.package].version`),
//!   3. workspace per-crate (flat `crates:` with independent versions).
//!
//! Also covers the positional range parsing (omitted / `a..b` / single `<tag>`),
//! the `--write` + non-kac error, the preview-extracts-only-the-section
//! contract, and `--crate` filtering.

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

struct RunResult {
    stdout: String,
    stderr: String,
    success: bool,
}

fn changelog(dir: &Path, args: &[&str]) -> RunResult {
    let out = anodizer()
        .current_dir(dir)
        .arg("changelog")
        .args(args)
        .output()
        .unwrap();
    RunResult {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        success: out.status.success(),
    }
}

// ---------------------------------------------------------------------------
// Mode 1: single-crate refresh + write
// ---------------------------------------------------------------------------

fn single_crate_repo() -> TempDir {
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
    tmp
}

#[test]
fn single_crate_preview_shows_unreleased_only() {
    let tmp = single_crate_repo();
    let root = tmp.path();
    let r = changelog(root, &["-q"]);
    assert!(r.success, "preview failed: {}\n{}", r.stdout, r.stderr);
    assert!(
        r.stdout.contains("Unreleased"),
        "preview must show the [Unreleased] heading: {}",
        r.stdout
    );
    assert!(
        r.stdout.contains("add a thing"),
        "preview must show the new commit: {}",
        r.stdout
    );
    // Preview does not write the file.
    assert!(
        !root.join("CHANGELOG.md").exists(),
        "preview must not write CHANGELOG.md"
    );
}

#[test]
fn single_crate_write_refreshes_file() {
    let tmp = single_crate_repo();
    let root = tmp.path();
    let r = changelog(root, &["-q", "--write"]);
    assert!(r.success, "write failed: {}\n{}", r.stdout, r.stderr);
    // Bare `changelog: {}` routes to the workspace-root CHANGELOG.md.
    let cl = read(root, "CHANGELOG.md");
    assert!(cl.contains("Unreleased"), "expected [Unreleased]: {cl}");
    assert!(cl.contains("add a thing"), "expected the commit: {cl}");
    // No commit was made: the write is a working-tree edit only.
    let status = Command::new("git")
        .current_dir(root)
        .args(["status", "--porcelain", "CHANGELOG.md"])
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&status.stdout);
    assert!(
        out.contains("CHANGELOG.md"),
        "CHANGELOG.md must be an uncommitted working-tree edit, status: {out:?}"
    );
}

#[test]
fn single_crate_write_preserves_released_history() {
    let tmp = single_crate_repo();
    let root = tmp.path();
    // Seed a released section + footer that the refresh must preserve.
    fs::write(
        root.join("CHANGELOG.md"),
        "# Changelog\n\n## [Unreleased]\n\n## [0.1.0] - 2026-01-01\n\n- first release\n\n[Unreleased]: http://x/compare/v0.1.0...HEAD\n",
    )
    .unwrap();
    let r = changelog(root, &["-q", "--write"]);
    assert!(r.success, "write failed: {}\n{}", r.stdout, r.stderr);
    let cl = read(root, "CHANGELOG.md");
    assert!(cl.contains("## [0.1.0]"), "released history dropped: {cl}");
    assert!(cl.contains("first release"), "released body dropped: {cl}");
    assert!(cl.contains("add a thing"), "new commit missing: {cl}");
    assert!(cl.contains("compare/v0.1.0"), "footer dropped: {cl}");
}

// ---------------------------------------------------------------------------
// Mode 2: workspace-lockstep
// ---------------------------------------------------------------------------

#[test]
fn lockstep_write_refreshes_root_changelog() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/core\", \"crates/cli\"]\nresolver = \"2\"\n\n[workspace.package]\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    for name in ["core", "cli"] {
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
    fs::write(root.join("crates/core/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(root, "feat: lockstep change");

    let r = changelog(root, &["-q", "--write"]);
    assert!(
        r.success,
        "lockstep write failed: {}\n{}",
        r.stdout, r.stderr
    );
    let cl = read(root, "CHANGELOG.md");
    assert!(cl.contains("Unreleased"), "expected [Unreleased]: {cl}");
    assert!(cl.contains("lockstep change"), "expected the commit: {cl}");
    // One aggregate root file; no per-crate files for a bare changelog config.
    assert!(
        !root.join("crates/core/CHANGELOG.md").exists(),
        "lockstep refresh must not write per-crate files"
    );
}

/// A lockstep repo whose `tag.tag_prefix` is `release-v` tags releases
/// `release-v0.1.0`. The refresh must bound the range from THAT tag — not a
/// hardcoded `v*` that misses it and degrades to full history (the recurring
/// 3-mode prefix-drift bug). Asserts only the post-tag commit appears.
#[test]
fn lockstep_honors_custom_tag_prefix_for_range() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/core\"]\nresolver = \"2\"\n\n[workspace.package]\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("crates/core/src")).unwrap();
    fs::write(
        root.join("crates/core/Cargo.toml"),
        "[package]\nname = \"core\"\nversion.workspace = true\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(root.join("crates/core/src/lib.rs"), "").unwrap();
    fs::write(
        root.join(".anodizer.yaml"),
        "project_name: lockstep\nchangelog: {}\ntag:\n  tag_prefix: \"release-v\"\n",
    )
    .unwrap();
    git_init(root);
    // A commit + the custom-prefixed tag, then a post-tag commit. Only the
    // post-tag commit may appear in the refreshed [Unreleased] range.
    fs::write(root.join("crates/core/src/lib.rs"), "// pre-tag\n").unwrap();
    git_add_commit(root, "feat: before the release tag");
    run_git(root, &["tag", "release-v0.1.0"]);
    fs::write(root.join("crates/core/src/lib.rs"), "// post-tag\n").unwrap();
    git_add_commit(root, "feat: after the release tag");

    let r = changelog(root, &["-q", "--write"]);
    assert!(
        r.success,
        "lockstep custom-prefix write failed: {}\n{}",
        r.stdout, r.stderr
    );
    let cl = read(root, "CHANGELOG.md");
    assert!(
        cl.contains("after the release tag"),
        "post-tag commit missing: {cl}"
    );
    assert!(
        !cl.contains("before the release tag"),
        "range degraded to full history — pre-tag commit leaked (prefix \"release-v\" was ignored): {cl}"
    );
}

// ---------------------------------------------------------------------------
// Mode 3: workspace per-crate
// ---------------------------------------------------------------------------

fn per_crate_repo() -> TempDir {
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
    git_add_commit(root, "feat: core change");
    fs::write(root.join("crates/cli/src/lib.rs"), "// cli touched\n").unwrap();
    git_add_commit(root, "fix: cli change");
    tmp
}

#[test]
fn per_crate_write_refreshes_each_crate_file() {
    let tmp = per_crate_repo();
    let root = tmp.path();
    let r = changelog(root, &["-q", "--write"]);
    assert!(
        r.success,
        "per-crate write failed: {}\n{}",
        r.stdout, r.stderr
    );
    let core = read(root, "crates/core/CHANGELOG.md");
    let cli = read(root, "crates/cli/CHANGELOG.md");
    assert!(core.contains("core change"), "core section missing: {core}");
    assert!(cli.contains("cli change"), "cli section missing: {cli}");
    // Each crate's range is bounded by ITS own tag, so the other crate's
    // commit must not bleed in.
    assert!(
        !core.contains("cli change"),
        "cli commit leaked into core: {core}"
    );
    assert!(
        !cli.contains("core change"),
        "core commit leaked into cli: {cli}"
    );
}

#[test]
fn per_crate_preview_separates_multiple_targets() {
    let tmp = per_crate_repo();
    let root = tmp.path();
    let r = changelog(root, &["-q"]);
    assert!(r.success, "preview failed: {}\n{}", r.stdout, r.stderr);
    // Two per-crate files → attributable `--- <path> ---` separators.
    assert!(
        r.stdout.contains("--- crates/core/CHANGELOG.md ---"),
        "missing core separator: {}",
        r.stdout
    );
    assert!(
        r.stdout.contains("--- crates/cli/CHANGELOG.md ---"),
        "missing cli separator: {}",
        r.stdout
    );
}

#[test]
fn per_crate_filter_restricts_to_one_crate() {
    let tmp = per_crate_repo();
    let root = tmp.path();
    let r = changelog(root, &["-q", "--write", "--crate", "core"]);
    assert!(
        r.success,
        "filtered write failed: {}\n{}",
        r.stdout, r.stderr
    );
    assert!(
        root.join("crates/core/CHANGELOG.md").exists(),
        "--crate core must refresh core"
    );
    assert!(
        !root.join("crates/cli/CHANGELOG.md").exists(),
        "--crate core must not touch cli"
    );
}

// ---------------------------------------------------------------------------
// Range parsing: single tag resolves crate + predecessor
// ---------------------------------------------------------------------------

#[test]
fn single_tag_resolves_owning_crate_and_predecessor() {
    let tmp = per_crate_repo();
    let root = tmp.path();
    // Add an older core tag so the predecessor of core-v0.3.0 is core-v0.2.0.
    run_git(root, &["tag", "core-v0.2.0"]);
    fs::write(root.join("crates/core/src/lib.rs"), "// core 0.3\n").unwrap();
    git_add_commit(root, "feat: core toward 0.3");
    run_git(root, &["tag", "core-v0.3.0"]);

    // `changelog core-v0.3.0 --format json` targets ONLY core, range
    // core-v0.2.0..core-v0.3.0.
    let r = changelog(root, &["-q", "core-v0.3.0", "--format", "json"]);
    assert!(
        r.success,
        "single-tag json failed: {}\n{}",
        r.stdout, r.stderr
    );
    let v: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 1, "single tag pins to one crate: {}", r.stdout);
    assert_eq!(arr[0]["crate"], "core");
    assert_eq!(
        arr[0]["from"], "core-v0.2.0",
        "predecessor wrong: {}",
        r.stdout
    );
    assert_eq!(arr[0]["to"], "core-v0.3.0");
}

// ---------------------------------------------------------------------------
// release-notes format (regression: grouped-bullet body)
// ---------------------------------------------------------------------------

#[test]
fn release_notes_format_emits_grouped_bullets() {
    let tmp = single_crate_repo();
    let root = tmp.path();
    // The release-notes path runs the changelog stage, which requires HEAD to
    // sit at the upper-bound tag (the release-time invariant). Tag the feat
    // commit as v0.2.0 and render the v0.1.0..v0.2.0 range via the single-tag
    // positional.
    run_git(root, &["tag", "v0.2.0"]);
    let r = changelog(root, &["-q", "v0.2.0", "--format", "release-notes"]);
    assert!(
        r.success,
        "release-notes failed: {}\n{}",
        r.stdout, r.stderr
    );
    assert!(
        r.stdout.contains("add a thing"),
        "release notes must list the commit: {}",
        r.stdout
    );
}

// ---------------------------------------------------------------------------
// json format shape
// ---------------------------------------------------------------------------

#[test]
fn json_format_emits_sorted_array_with_crate_field() {
    let tmp = per_crate_repo();
    let root = tmp.path();
    let r = changelog(root, &["-q", "--format", "json"]);
    assert!(r.success, "json failed: {}\n{}", r.stdout, r.stderr);
    let v: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let arr = v.as_array().expect("json output must be an array");
    assert_eq!(arr.len(), 2, "one element per crate: {}", r.stdout);
    // Sorted by crate name: cli before core.
    assert_eq!(arr[0]["crate"], "cli");
    assert_eq!(arr[1]["crate"], "core");
    // Each element carries the documented payload fields.
    for elem in arr {
        assert!(elem.get("crate").is_some());
        assert!(elem.get("to").is_some());
        assert!(elem.get("groups").is_some());
    }
}

// ---------------------------------------------------------------------------
// --write + non-kac format error (end-to-end through clap)
// ---------------------------------------------------------------------------

#[test]
fn write_with_release_notes_format_is_rejected() {
    let tmp = single_crate_repo();
    let root = tmp.path();
    let r = changelog(root, &["-q", "--write", "--format", "release-notes"]);
    assert!(!r.success, "--write + release-notes must fail");
    assert!(
        r.stderr.contains("--write is only valid"),
        "expected the write/format error: {}",
        r.stderr
    );
}

#[test]
fn explicit_range_overrides_auto_discovery() {
    let tmp = single_crate_repo();
    let root = tmp.path();
    // `changelog v0.1.0..HEAD --format json` feeds the exact range.
    let r = changelog(root, &["-q", "v0.1.0..HEAD", "--format", "json"]);
    assert!(r.success, "range json failed: {}\n{}", r.stdout, r.stderr);
    let v: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr[0]["from"], "v0.1.0");
    assert_eq!(arr[0]["to"], "HEAD");
}

// ---------------------------------------------------------------------------
// Range consistency: empty-from = full history, uniformly across formats
// ---------------------------------------------------------------------------

/// A single-crate repo with two tagged releases and a commit AFTER the latest
/// tag, so "since the last release" (v0.2.0..HEAD) is a strict subset of full
/// history. The pre-v0.2.0 `feat:` commit ("early feature") is the discriminator:
/// it appears under full history but not under the pending window.
fn two_release_repo() -> TempDir {
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
        "[package]\nname = \"app\"\nversion = \"0.2.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(root.join("crates/app/src/lib.rs"), "").unwrap();
    fs::write(
        root.join(".anodizer.yaml"),
        r#"project_name: two-release
changelog:
  snapshot: true
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
    fs::write(root.join("crates/app/src/lib.rs"), "// early\n").unwrap();
    git_add_commit(root, "feat: early feature");
    run_git(root, &["tag", "v0.2.0"]);
    fs::write(root.join("crates/app/src/lib.rs"), "// late\n").unwrap();
    git_add_commit(root, "fix: late fix");
    tmp
}

/// Collect every commit `summary` across all groups + nested subgroups of one
/// json changelog element.
fn json_summaries(elem: &serde_json::Value) -> Vec<String> {
    fn walk(group: &serde_json::Value, out: &mut Vec<String>) {
        if let Some(entries) = group.get("entries").and_then(|e| e.as_array()) {
            for e in entries {
                if let Some(s) = e.get("summary").and_then(|s| s.as_str()) {
                    out.push(s.to_string());
                }
            }
        }
        if let Some(subs) = group.get("subgroups").and_then(|g| g.as_array()) {
            for sub in subs {
                walk(sub, out);
            }
        }
    }
    let mut out = Vec::new();
    if let Some(groups) = elem.get("groups").and_then(|g| g.as_array()) {
        for g in groups {
            walk(g, &mut out);
        }
    }
    out
}

fn json_summaries_for_range(root: &Path, range: Option<&str>) -> Vec<String> {
    let mut args = vec!["-q"];
    if let Some(r) = range {
        args.push(r);
    }
    args.extend(["--format", "json"]);
    let r = changelog(root, &args);
    assert!(
        r.success,
        "json {range:?} failed: {}\n{}",
        r.stdout, r.stderr
    );
    let v: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 1, "single-crate repo yields one element");
    json_summaries(&arr[0])
}

fn release_notes_for_range(root: &Path, range: Option<&str>) -> String {
    let mut args = vec!["-q"];
    if let Some(r) = range {
        args.push(r);
    }
    args.extend(["--snapshot", "--format", "release-notes"]);
    let r = changelog(root, &args);
    assert!(
        r.success,
        "release-notes {range:?} failed: {}\n{}",
        r.stdout, r.stderr
    );
    r.stdout
}

/// `changelog ..` and `changelog ..HEAD` must converge: both are full history
/// and both include the pre-v0.2.0 commit, for json AND release-notes.
#[test]
fn full_history_dotdot_and_dotdot_head_converge() {
    let tmp = two_release_repo();
    let root = tmp.path();

    // json: identical commit sets, and both include the early (pre-latest-tag)
    // commit — proving empty-from = full history, not last-tag.
    let mut a = json_summaries_for_range(root, Some(".."));
    let mut b = json_summaries_for_range(root, Some("..HEAD"));
    a.sort();
    b.sort();
    assert_eq!(
        a, b,
        "`..` and `..HEAD` json must be identical: {a:?} vs {b:?}"
    );
    assert!(
        a.iter().any(|s| s.contains("early feature")),
        "full history must include the pre-v0.2.0 commit: {a:?}"
    );
    assert!(
        a.iter().any(|s| s.contains("late fix")),
        "full history must include the post-v0.2.0 commit: {a:?}"
    );

    // release-notes: both include the early commit (same full-history bound).
    for range in [Some(".."), Some("..HEAD")] {
        let notes = release_notes_for_range(root, range);
        assert!(
            notes.contains("early feature"),
            "release-notes {range:?} (full history) must include the early commit:\n{notes}"
        );
        assert!(
            notes.contains("late fix"),
            "release-notes {range:?} (full history) must include the late commit:\n{notes}"
        );
    }
}

/// The omitted form (pending / since-last-release) must NOT equal `..`: it
/// covers only commits since v0.2.0, excluding the early one.
#[test]
fn omitted_range_is_pending_not_full_history() {
    let tmp = two_release_repo();
    let root = tmp.path();

    let omitted = json_summaries_for_range(root, None);
    assert!(
        omitted.iter().all(|s| !s.contains("early feature")),
        "omitted (pending) range must exclude the pre-v0.2.0 commit: {omitted:?}"
    );
    assert!(
        omitted.iter().any(|s| s.contains("late fix")),
        "omitted (pending) range must include the post-v0.2.0 commit: {omitted:?}"
    );

    let full = json_summaries_for_range(root, Some(".."));
    assert!(
        full.len() > omitted.len(),
        "full history must cover strictly more than the pending window: full={full:?} pending={omitted:?}"
    );
}

/// For the SAME range arg, json and release-notes must surface the SAME commit
/// set — both for full history (`..HEAD`) and an explicit `v0.1.0..HEAD`.
#[test]
fn cross_format_commit_set_is_consistent() {
    let tmp = two_release_repo();
    let root = tmp.path();

    // Full history: early + late appear in BOTH formats.
    let full_json = json_summaries_for_range(root, Some("..HEAD"));
    let full_notes = release_notes_for_range(root, Some("..HEAD"));
    for needle in ["early feature", "late fix"] {
        assert!(
            full_json.iter().any(|s| s.contains(needle)),
            "json full history missing {needle:?}: {full_json:?}"
        );
        assert!(
            full_notes.contains(needle),
            "release-notes full history missing {needle:?}:\n{full_notes}"
        );
    }

    // Explicit `v0.1.0..HEAD`: starts at v0.1.0, so early + late both appear in
    // BOTH formats (the v0.1.0 "initial" commit is the lower bound, excluded).
    let exp_json = json_summaries_for_range(root, Some("v0.1.0..HEAD"));
    let exp_notes = release_notes_for_range(root, Some("v0.1.0..HEAD"));
    for needle in ["early feature", "late fix"] {
        assert!(
            exp_json.iter().any(|s| s.contains(needle)),
            "json v0.1.0..HEAD missing {needle:?}: {exp_json:?}"
        );
        assert!(
            exp_notes.contains(needle),
            "release-notes v0.1.0..HEAD missing {needle:?}:\n{exp_notes}"
        );
    }
}
