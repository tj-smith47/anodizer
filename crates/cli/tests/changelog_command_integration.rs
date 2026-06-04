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

/// Assert that the `## [Unreleased]` section of `text` contains NO `### `
/// heading other than the supplied `allowed_group_titles`. A flat aggregate
/// must never graft a `### <crate>` OR `### <project_name>` subsection — the
/// aggregate is keyed by `project_name`, so a brittle "no `### <crate>` name"
/// check misses a regressed render guard that grafts `### <project_name>`.
/// Restricting the scan to the `[Unreleased]` block keeps curated H3s in older
/// released sections from tripping the assertion.
fn assert_no_subsection_graft(text: &str, allowed_group_titles: &[&str]) {
    let mut in_unreleased = false;
    for line in text.lines() {
        if line.starts_with("## ") {
            in_unreleased = line.contains("Unreleased");
            continue;
        }
        if in_unreleased && line.starts_with("### ") {
            let title = line.trim_start_matches("### ").trim();
            assert!(
                allowed_group_titles.contains(&title),
                "unexpected `### {title}` subsection grafted into [Unreleased] \
                 (allowed group titles: {allowed_group_titles:?}):\n{text}"
            );
        }
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
    // The standalone `changelog` command is a LOCAL preview: it renders a tag's
    // window WITHOUT requiring HEAD to sit at that tag (no checkout). Tag the
    // feat commit as v0.2.0, then add a FURTHER commit so HEAD is BEHIND the
    // v0.2.0 tag's checkout state, proving the tag-at-HEAD guard is bypassed for
    // the preview.
    run_git(root, &["tag", "v0.2.0"]);
    fs::write(root.join("crates/app/src/lib.rs"), "// moved past v0.2.0\n").unwrap();
    git_add_commit(root, "chore: move HEAD past the tag");
    let r = changelog(root, &["-q", "v0.2.0", "--format", "release-notes"]);
    assert!(
        r.success,
        "release-notes failed: {}\n{}",
        r.stdout, r.stderr
    );
    assert!(
        !r.stderr.contains("does not point at HEAD"),
        "the standalone preview must NOT require HEAD at the tag: {}",
        r.stderr
    );
    assert!(
        r.stdout.contains("add a thing"),
        "release notes must list the commit: {}",
        r.stdout
    );
}

/// Bare `changelog --format release-notes` (no positional, no `--snapshot`) with
/// the last tag BEHIND HEAD must render the pending last-tag..HEAD window — the
/// same set kac/json show for the identical state — with NO release-time guards:
/// no tag-at-HEAD error, no `changelog skipped` line.
#[test]
fn bare_release_notes_renders_pending_window_no_guards() {
    let tmp = single_crate_repo();
    let root = tmp.path();
    // single_crate_repo leaves v0.1.0 tagged with one post-tag commit ("add a
    // thing") on HEAD — the pending window.
    let r = changelog(root, &["-q", "--format", "release-notes"]);
    assert!(
        r.success,
        "bare release-notes failed: {}\n{}",
        r.stdout, r.stderr
    );
    assert!(
        !r.stderr.contains("does not point at HEAD"),
        "bare preview must NOT require a checkout: {}",
        r.stderr
    );
    assert!(
        !r.stdout.contains("changelog skipped") && !r.stderr.contains("changelog skipped"),
        "bare preview must NOT hit the snapshot-skip gate: {}\n{}",
        r.stdout,
        r.stderr
    );
    assert!(
        r.stdout.contains("add a thing"),
        "bare preview must show the pending commit: {}",
        r.stdout
    );
}

/// `changelog --snapshot --format release-notes` against a fixture WITHOUT
/// `changelog.snapshot: true` must still render — the standalone command bypasses
/// the snapshot-skip config gate that the release pipeline honors.
#[test]
fn snapshot_release_notes_renders_without_config_opt_in() {
    let tmp = single_crate_repo();
    let root = tmp.path();
    // single_crate_repo's config is `changelog: {}` — snapshot opt-in is UNSET.
    let r = changelog(root, &["-q", "--snapshot", "--format", "release-notes"]);
    assert!(
        r.success,
        "snapshot release-notes failed: {}\n{}",
        r.stdout, r.stderr
    );
    assert!(
        !r.stdout.contains("changelog skipped") && !r.stderr.contains("changelog skipped"),
        "the standalone command must bypass the `changelog.snapshot` gate: {}\n{}",
        r.stdout,
        r.stderr
    );
    assert!(
        r.stdout.contains("add a thing"),
        "snapshot preview must show the pending commit: {}",
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
/// set — for the OMITTED (pending) range, full history (`..HEAD`), and an
/// explicit `v0.1.0..HEAD`.
#[test]
fn cross_format_commit_set_is_consistent() {
    let tmp = two_release_repo();
    let root = tmp.path();

    // OMITTED (pending): HEAD is one commit ahead of the latest tag (v0.2.0).
    // Both formats must bound at v0.2.0 — including the post-tag "late fix" and
    // EXCLUDING the pre-tag "early feature". This is the regression guard: a
    // prior build leaked full history through `--snapshot --format
    // release-notes` because `resolve_prev_tag` dropped the auto-discovered
    // previous tag when it equalled the snapshot's current `Tag`.
    let pending_json = json_summaries_for_range(root, None);
    let pending_notes = release_notes_for_range(root, None);
    assert!(
        pending_json.iter().any(|s| s.contains("late fix")),
        "json pending must include the post-tag commit: {pending_json:?}"
    );
    assert!(
        pending_json.iter().all(|s| !s.contains("early feature")),
        "json pending must EXCLUDE the pre-tag commit: {pending_json:?}"
    );
    assert!(
        pending_notes.contains("late fix"),
        "release-notes pending must include the post-tag commit:\n{pending_notes}"
    );
    assert!(
        !pending_notes.contains("early feature"),
        "release-notes pending must EXCLUDE the pre-tag commit (full-history leak):\n{pending_notes}"
    );

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

/// Single-crate, HEAD one commit ahead of the latest tag: the OMITTED pending
/// range through `--snapshot --format release-notes` must show exactly the
/// since-last-release set (the post-tag "late fix"), never the pre-tag "early
/// feature" — matching the json and keep-a-changelog pending output. The
/// snapshot `Tag` resolves to the latest existing tag (v0.2.0) here, the exact
/// condition under which release-notes previously leaked full history.
#[test]
fn snapshot_release_notes_pending_matches_engine_formats() {
    let tmp = two_release_repo();
    let root = tmp.path();

    let notes = release_notes_for_range(root, None);
    assert!(
        notes.contains("late fix"),
        "release-notes pending must include the post-tag commit:\n{notes}"
    );
    assert!(
        !notes.contains("early feature"),
        "release-notes pending leaked the pre-tag commit (full-history bug):\n{notes}"
    );

    // The same pending set must come out of json and keep-a-changelog.
    let json = json_summaries_for_range(root, None);
    assert!(json.iter().any(|s| s.contains("late fix")), "{json:?}");
    assert!(
        json.iter().all(|s| !s.contains("early feature")),
        "{json:?}"
    );

    let kac = changelog(root, &["-q"]);
    assert!(kac.success, "kac failed: {}\n{}", kac.stdout, kac.stderr);
    assert!(
        kac.stdout.contains("late fix") && !kac.stdout.contains("early feature"),
        "kac pending mismatch:\n{}",
        kac.stdout
    );
}

/// Workspace per-crate: each crate's pending release-notes window is bounded at
/// ITS OWN last tag, never full history and never the other crate's commits.
/// Both crates have HEAD ahead of their latest tag, so this exercises the
/// snapshot previous-tag resolution per crate (mode-agnostic regression).
fn per_crate_snapshot_repo() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/core\", \"crates/cli\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    for (name, ver) in [("core", "0.2.0"), ("cli", "0.3.0")] {
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
        r#"project_name: percrate-snap
changelog:
  snapshot: true
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
    // Each crate gets a pre-tag commit, then a tag, then a post-tag commit, so
    // "since last release" is a strict subset of full history for both.
    git_add_commit(root, "initial");
    run_git(root, &["tag", "core-v0.1.0"]);
    run_git(root, &["tag", "cli-v0.1.0"]);
    fs::write(root.join("crates/core/src/lib.rs"), "// core early\n").unwrap();
    git_add_commit(root, "feat: core early");
    run_git(root, &["tag", "core-v0.2.0"]);
    fs::write(root.join("crates/cli/src/lib.rs"), "// cli early\n").unwrap();
    git_add_commit(root, "feat: cli early");
    run_git(root, &["tag", "cli-v0.3.0"]);
    fs::write(root.join("crates/core/src/lib.rs"), "// core late\n").unwrap();
    git_add_commit(root, "fix: core late");
    fs::write(root.join("crates/cli/src/lib.rs"), "// cli late\n").unwrap();
    git_add_commit(root, "fix: cli late");
    tmp
}

#[test]
fn per_crate_snapshot_release_notes_bounds_at_each_crate_tag() {
    let tmp = per_crate_snapshot_repo();
    let root = tmp.path();

    // core's pending window is core-v0.2.0..HEAD: "core late", not "core early".
    let core = release_notes_for_crate(root, "core");
    assert!(
        core.contains("core late"),
        "core pending must include its post-tag commit:\n{core}"
    );
    assert!(
        !core.contains("core early"),
        "core pending leaked its pre-tag commit (full-history bug):\n{core}"
    );
    assert!(
        !core.contains("cli late") && !core.contains("cli early"),
        "cli commits leaked into core's notes:\n{core}"
    );

    // cli's pending window is cli-v0.3.0..HEAD: "cli late", not "cli early".
    let cli = release_notes_for_crate(root, "cli");
    assert!(
        cli.contains("cli late"),
        "cli pending must include its post-tag commit:\n{cli}"
    );
    assert!(
        !cli.contains("cli early"),
        "cli pending leaked its pre-tag commit (full-history bug):\n{cli}"
    );
    assert!(
        !cli.contains("core late") && !cli.contains("core early"),
        "core commits leaked into cli's notes:\n{cli}"
    );
}

/// Run `anodizer changelog --crate <name> --snapshot --format release-notes`
/// (omitted range = pending) and return stdout.
fn release_notes_for_crate(root: &Path, crate_name: &str) -> String {
    let r = changelog(
        root,
        &[
            "-q",
            "--crate",
            crate_name,
            "--snapshot",
            "--format",
            "release-notes",
        ],
    );
    assert!(
        r.success,
        "release-notes --crate {crate_name} failed: {}\n{}",
        r.stdout, r.stderr
    );
    r.stdout
}

// ---------------------------------------------------------------------------
// Same-prefix shared-root collapse: N crates all on `v{{ Version }}` routing to
// one shared root CHANGELOG.md are a SINGLE flat lockstep aggregate, not N
// multi-track `### <crate>` subsections.
// ---------------------------------------------------------------------------

/// A flat `crates:` workspace whose members ALL share `tag_template:
/// "v{{ Version }}"` and route to one shared root (no `per_crate`/`root`
/// config), with a curated flat `## [Unreleased]`, a `v0.1.0` tag, and post-tag
/// commits. The curated `### <Heading>` titles deliberately do NOT match the
/// configured `groups:` — the exact shape that tripped the multi-track
/// heuristic and grafted a spurious `### <crate>` subsection.
fn same_prefix_shared_root_repo() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/core\", \"crates/cli\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    for (name, ver) in [("core", "0.1.0"), ("cli", "0.1.0")] {
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
        r#"project_name: aggregate
changelog:
  groups:
    - title: Features
      regexp: "^feat"
crates:
  - name: core
    path: crates/core
    tag_template: "v{{ .Version }}"
  - name: cli
    path: crates/cli
    tag_template: "v{{ .Version }}"
"#,
    )
    .unwrap();
    // Curated flat [Unreleased] whose H3 titles (`### Docs`, `### Fixes`) are NOT
    // configured group titles — the multi-track-misread trap.
    fs::write(
        root.join("CHANGELOG.md"),
        "# Changelog\n\n## [Unreleased]\n\n### Docs\n\n- hand-written prose\n\n### Fixes\n\n- curated fix note\n",
    )
    .unwrap();
    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "v0.1.0"]);
    fs::write(root.join("crates/core/src/lib.rs"), "// core\n").unwrap();
    git_add_commit(root, "feat: aggregate change in core");
    fs::write(root.join("crates/cli/src/lib.rs"), "// cli\n").unwrap();
    git_add_commit(root, "feat: aggregate change in cli");
    tmp
}

#[test]
fn same_prefix_shared_root_collapses_to_one_flat_unreleased() {
    let tmp = same_prefix_shared_root_repo();
    let root = tmp.path();
    let r = changelog(root, &["-q"]);
    assert!(r.success, "preview failed: {}\n{}", r.stdout, r.stderr);

    // ONE flat [Unreleased] section, no `--- <path> ---` per-crate separators.
    assert_eq!(
        r.stdout.matches("## [Unreleased]").count(),
        1,
        "expected exactly one [Unreleased] section: {}",
        r.stdout
    );
    assert!(
        !r.stdout.contains("---"),
        "flat aggregate must not emit per-crate separators: {}",
        r.stdout
    );
    // No `### <crate>` NOR `### <project_name>` graft (the aggregate is keyed by
    // `aggregate`): reject any [Unreleased] H3 that isn't the configured group.
    assert!(
        !r.stdout.contains("### core") && !r.stdout.contains("### cli"),
        "flat aggregate must not graft a `### <crate>` subsection: {}",
        r.stdout
    );
    assert_no_subsection_graft(&r.stdout, &["Features"]);
    // The regenerated body reflects BOTH members' post-tag commits (whole-repo).
    assert!(
        r.stdout.contains("aggregate change in core")
            && r.stdout.contains("aggregate change in cli"),
        "regenerated flat body must aggregate every member's commits: {}",
        r.stdout
    );
}

#[test]
fn same_prefix_shared_root_write_is_flat() {
    let tmp = same_prefix_shared_root_repo();
    let root = tmp.path();
    let r = changelog(root, &["-q", "--write"]);
    assert!(r.success, "write failed: {}\n{}", r.stdout, r.stderr);
    let cl = read(root, "CHANGELOG.md");
    assert_eq!(
        cl.matches("## [Unreleased]").count(),
        1,
        "written file must keep a single [Unreleased]: {cl}"
    );
    assert!(
        !cl.contains("### core") && !cl.contains("### cli"),
        "written file must not graft a `### <crate>` subsection: {cl}"
    );
    assert_no_subsection_graft(&cl, &["Features"]);
    assert!(
        cl.contains("aggregate change in core") && cl.contains("aggregate change in cli"),
        "written flat body must aggregate every member's commits: {cl}"
    );
    // No per-crate files for a shared-root aggregate.
    assert!(
        !root.join("crates/core/CHANGELOG.md").exists()
            && !root.join("crates/cli/CHANGELOG.md").exists(),
        "flat aggregate must not write per-crate files"
    );
}

/// release-notes on a same-prefix shared-root repo collapses to ONE aggregate
/// body — no `### <crate>`/`### <project_name>` graft, no `--- <crate> ---`
/// per-crate separators, both members' commits in one block. HEAD is tagged
/// `v0.2.0` and the EXPLICIT `v0.1.0..v0.2.0` range drives a non-empty body
/// (the changelog stage requires HEAD at the upper tag). An explicit range
/// (unlike a single-tag positional, which pins to the tag's owning crate)
/// applies to every target, exercising the no-filter aggregate collapse.
#[test]
fn same_prefix_shared_root_release_notes_is_one_aggregate() {
    let tmp = same_prefix_shared_root_repo();
    let root = tmp.path();
    run_git(root, &["tag", "v0.2.0"]);
    let r = changelog(root, &["-q", "v0.1.0..v0.2.0", "--format", "release-notes"]);
    assert!(
        r.success,
        "release-notes failed: {}\n{}",
        r.stdout, r.stderr
    );
    let out = &r.stdout;
    // ONE aggregate body: no per-crate `--- <crate> ---` separators.
    assert!(
        !out.contains("---\ncore\n---") && !out.contains("---\ncli\n---"),
        "release-notes flat aggregate must not emit per-crate separators: {out}"
    );
    // No `### <crate>` NOR `### <project_name>` graft.
    for c in ["### core", "### cli", "### aggregate"] {
        assert!(
            !out.contains(c),
            "spurious `{c}` graft in release-notes: {out}"
        );
    }
    // Both members' commits land in the single aggregate body (whole-repo).
    assert!(
        out.contains("aggregate change in core") && out.contains("aggregate change in cli"),
        "release-notes aggregate body must span every member's commits: {out}"
    );
}

/// Contrast: a workspace with DISTINCT tag prefixes (`core-v*` + `cli-v*`)
/// curating a multi-track root must STILL refresh each crate's own
/// `### <crate>` subsection — the collapse must not regress genuine multi-track.
#[test]
fn distinct_prefix_multitrack_keeps_crate_subsections() {
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
        r#"project_name: multitrack
changelog:
  groups:
    - title: Features
      regexp: "^feat"
crates:
  - name: core
    path: crates/core
    tag_template: "core-v{{ .Version }}"
  - name: cli
    path: crates/cli
    tag_template: "cli-v{{ .Version }}"
"#,
    )
    .unwrap();
    // Curated multi-track root: a `### core` + `### cli` subsection each.
    fs::write(
        root.join("CHANGELOG.md"),
        "# Changelog\n\n## [Unreleased]\n\n### core\n\n- old core note\n\n### cli\n\n- old cli note\n",
    )
    .unwrap();
    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "core-v0.1.0"]);
    run_git(root, &["tag", "cli-v0.2.0"]);
    fs::write(root.join("crates/core/src/lib.rs"), "// core\n").unwrap();
    git_add_commit(root, "feat: distinct core change");
    fs::write(root.join("crates/cli/src/lib.rs"), "// cli\n").unwrap();
    git_add_commit(root, "feat: distinct cli change");

    let r = changelog(root, &["-q", "--write"]);
    assert!(
        r.success,
        "multitrack write failed: {}\n{}",
        r.stdout, r.stderr
    );
    let cl = read(root, "CHANGELOG.md");
    // Both crate subsections survive; each regenerated from its own track.
    assert!(
        cl.contains("### core") && cl.contains("### cli"),
        "genuine multi-track must keep both `### <crate>` subsections: {cl}"
    );
    assert!(
        cl.contains("distinct core change"),
        "core subsection must regenerate from core's commits: {cl}"
    );
    assert!(
        cl.contains("distinct cli change"),
        "cli subsection must regenerate from cli's commits: {cl}"
    );
}

/// The comprehensive dogfood regression mirroring anodizer's own
/// `.anodizer.yaml`: N crates all on `v{{ Version }}`, a `changelog:` block with
/// a commit `format` carrying `{{ .SHA }}` / `{{ .Message }}` /
/// `{{ .AuthorUsername }}` + groups, a shared root, a curated flat
/// `## [Unreleased]` whose H3 titles diverge from the configured groups, a `v*`
/// tag, and post-tag commits. The combined output must be CLEAN:
///   (a) ONE flat [Unreleased], no `### <crate>` graft;
///   (b) single `* ` bullets, no `* *`;
///   (c) authors render as NAMES, no empty `()`;
///   (d) generated bullets reflect the since-tag commits.
#[test]
fn dogfood_flat_aggregate_render_is_clean() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/core\", \"crates/cli\", \"crates/api\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    for name in ["core", "cli", "api"] {
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
        r#"project_name: dogfood
changelog:
  use: github-native
  format: "* {{ .SHA }} {{ .Message }} ({{ .AuthorUsername }})"
  abbrev: 12
  groups:
    - title: Features
      regexp: "^feat"
      order: 0
    - title: Bug Fixes
      regexp: "^fix"
      order: 1
crates:
  - name: core
    path: crates/core
    tag_template: "v{{ .Version }}"
  - name: cli
    path: crates/cli
    tag_template: "v{{ .Version }}"
  - name: api
    path: crates/api
    tag_template: "v{{ .Version }}"
"#,
    )
    .unwrap();
    // Curated flat [Unreleased] whose H3 titles diverge from the groups.
    fs::write(
        root.join("CHANGELOG.md"),
        "# Changelog\n\n## [Unreleased]\n\n### CI / Workflows\n\n- curated CI note\n\n### Docs\n\n- curated docs note\n",
    )
    .unwrap();
    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "v0.5.0"]);
    fs::write(root.join("crates/core/src/lib.rs"), "// core\n").unwrap();
    git_add_commit(root, "feat: dogfood core capability");
    fs::write(root.join("crates/cli/src/lib.rs"), "// cli\n").unwrap();
    git_add_commit(root, "fix: dogfood cli bug");

    // Default format (keep-a-changelog). github-native falls back to `git`
    // locally (no GitHub login), so author names come from the local commits.
    let r = changelog(root, &["-q"]);
    assert!(
        r.success,
        "dogfood preview failed: {}\n{}",
        r.stdout, r.stderr
    );
    let out = &r.stdout;

    // (a) one flat [Unreleased], no `### <crate>` NOR `### <project_name>`
    // graft. The aggregate is keyed by project_name (`dogfood`), so a regressed
    // render-side `single_track` guard would graft `### dogfood`, not
    // `### core`; assert against both, and robustly reject ANY `### ` heading in
    // the [Unreleased] block that isn't a configured group title.
    assert_eq!(
        out.matches("## [Unreleased]").count(),
        1,
        "expected one flat [Unreleased]: {out}"
    );
    for c in ["### core", "### cli", "### api", "### dogfood"] {
        assert!(!out.contains(c), "spurious `{c}` graft: {out}");
    }
    assert_no_subsection_graft(out, &["Features", "Bug Fixes"]);
    // (b) no `* *` double bullets.
    assert!(!out.contains("* *"), "double bullet emitted: {out}");
    // (c) author renders as a NAME, no empty `()`.
    assert!(
        out.contains("(Test)"),
        "author must render as the committer name: {out}"
    );
    assert!(!out.contains("()"), "empty author parens emitted: {out}");
    // (d) generated bullets reflect the since-tag commits.
    assert!(
        out.contains("dogfood core capability") && out.contains("dogfood cli bug"),
        "regenerated body must reflect since-tag commits: {out}"
    );
}

// ---------------------------------------------------------------------------
// github-native preview: the standalone command renders from LOCAL git instead
// of GitHub (whose body is generated at release time), requires no token, and
// never emits empty.
// ---------------------------------------------------------------------------

/// A single-crate repo configured `changelog.use: github-native` with a
/// `release.github` repo. The standalone `changelog --format release-notes`
/// must render LOCAL scm bullets (the pending window), emit the one-line
/// "previewing from local git" note, require NO token, and be NON-empty.
fn github_native_repo() -> TempDir {
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
        r#"project_name: gh-native
changelog:
  use: github-native
crates:
  - name: app
    path: crates/app
    tag_template: "v{{ .Version }}"
    release:
      github:
        owner: octocat
        name: app
"#,
    )
    .unwrap();
    git_init(root);
    git_add_commit(root, "initial");
    run_git(root, &["tag", "v0.1.0"]);
    fs::write(root.join("crates/app/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(root, "feat: github-native local preview");
    tmp
}

#[test]
fn github_native_release_notes_previews_from_local_git() {
    let tmp = github_native_repo();
    let root = tmp.path();
    // No token set in the environment: a real github-native release would
    // require one, but the local preview must not.
    let mut cmd = anodizer();
    cmd.current_dir(root)
        .arg("changelog")
        .args(["-q", "--format", "release-notes"])
        .env_remove("GITHUB_TOKEN")
        .env_remove("ANODIZER_GITHUB_TOKEN");
    let out = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "github-native preview failed: {stdout}\n{stderr}"
    );
    assert!(
        !stderr.contains("requires a GitHub token"),
        "github-native preview must NOT require a token: {stderr}"
    );
    assert!(
        stderr.contains("previewing from local git"),
        "github-native preview must emit the one-line fallback note: {stderr}"
    );
    assert!(
        stdout.contains("github-native local preview"),
        "github-native preview must render the local commit (non-empty): {stdout}"
    );
}

// ---------------------------------------------------------------------------
// Multi-track (distinct tag prefixes) release-notes preview: each crate's
// pending window renders without a checkout, bounded at its own tag.
// ---------------------------------------------------------------------------

/// Multi-track (distinct-prefix `core-v*` / `cli-v*`) repo with each crate's
/// last tag BEHIND HEAD. `changelog --crate <name> --format release-notes`
/// (NO `--snapshot`, no checkout) must render that crate's pending window
/// bounded at its own tag — proving the preview bypass works in the per-crate
/// mode too.
#[test]
fn multitrack_release_notes_preview_bounds_at_each_crate_tag() {
    let tmp = per_crate_repo();
    let root = tmp.path();
    // per_crate_repo: core-v0.1.0 + cli-v0.2.0 tagged, then "feat: core change"
    // and "fix: cli change" on HEAD (each the pending window for its crate).
    let core = changelog(
        root,
        &["-q", "--crate", "core", "--format", "release-notes"],
    );
    assert!(
        core.success,
        "core release-notes preview failed: {}\n{}",
        core.stdout, core.stderr
    );
    assert!(
        !core.stderr.contains("does not point at HEAD"),
        "multitrack preview must NOT require a checkout: {}",
        core.stderr
    );
    assert!(
        core.stdout.contains("core change"),
        "core preview must show its pending commit: {}",
        core.stdout
    );
    assert!(
        !core.stdout.contains("cli change"),
        "cli commit leaked into core's preview: {}",
        core.stdout
    );

    let cli = changelog(root, &["-q", "--crate", "cli", "--format", "release-notes"]);
    assert!(
        cli.success,
        "cli release-notes preview failed: {}\n{}",
        cli.stdout, cli.stderr
    );
    assert!(
        cli.stdout.contains("cli change"),
        "cli preview must show its pending commit: {}",
        cli.stdout
    );
    assert!(
        !cli.stdout.contains("core change"),
        "core commit leaked into cli's preview: {}",
        cli.stdout
    );
}

// ---------------------------------------------------------------------------
// Flat-aggregate coherence guard: members sharing one tag prefix must agree on
// `[package].version` (one tag can't carry two versions). The guard fires
// identically for changelog / tag / bump.
// ---------------------------------------------------------------------------

/// A flat `crates:` workspace whose members share `tag_template:
/// "v{{ Version }}"` but carry the supplied (possibly divergent) versions.
fn flat_aggregate_versions_repo(core_ver: &str, cli_ver: &str) -> TempDir {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/core\", \"crates/cli\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    for (name, ver) in [("core", core_ver), ("cli", cli_ver)] {
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
        r#"project_name: agg
changelog: {}
crates:
  - name: core
    path: crates/core
    tag_template: "v{{ .Version }}"
  - name: cli
    path: crates/cli
    tag_template: "v{{ .Version }}"
"#,
    )
    .unwrap();
    git_init(root);
    git_add_commit(root, "initial");
    tmp
}

fn assert_coherence_error(stderr: &str) {
    assert!(
        stderr.contains("core") && stderr.contains("cli"),
        "{stderr}"
    );
    assert!(
        stderr.contains("0.5.0") && stderr.contains("0.1.0"),
        "names differing versions: {stderr}"
    );
    assert!(
        stderr.contains("prefix 'v'"),
        "names shared prefix: {stderr}"
    );
    assert!(
        stderr.contains("[workspace.package].version"),
        "steers toward lockstep: {stderr}"
    );
    assert!(
        stderr.contains("distinct tag_template prefix"),
        "steers toward independent prefixes: {stderr}"
    );
}

#[test]
fn changelog_rejects_divergent_flat_aggregate_versions() {
    let tmp = flat_aggregate_versions_repo("0.5.0", "0.1.0");
    let r = changelog(tmp.path(), &["-q"]);
    assert!(
        !r.success,
        "divergent flat aggregate must error: {}",
        r.stdout
    );
    assert_coherence_error(&r.stderr);
}

#[test]
fn changelog_accepts_agreeing_flat_aggregate_versions() {
    let tmp = flat_aggregate_versions_repo("0.2.0", "0.2.0");
    let r = changelog(tmp.path(), &["-q"]);
    assert!(
        r.success,
        "all-agree flat aggregate must work: {}\n{}",
        r.stdout, r.stderr
    );
}

#[test]
fn tag_rejects_divergent_flat_aggregate_versions() {
    let tmp = flat_aggregate_versions_repo("0.5.0", "0.1.0");
    let out = anodizer()
        .current_dir(tmp.path())
        .args(["tag", "--dry-run", "-q"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "tag must error on divergent versions"
    );
    assert_coherence_error(&String::from_utf8_lossy(&out.stderr));
}

#[test]
fn bump_rejects_divergent_flat_aggregate_versions() {
    let tmp = flat_aggregate_versions_repo("0.5.0", "0.1.0");
    let out = anodizer()
        .current_dir(tmp.path())
        .args(["bump", "patch", "--workspace", "--dry-run", "-q"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "bump must error on divergent versions"
    );
    assert_coherence_error(&String::from_utf8_lossy(&out.stderr));
}
