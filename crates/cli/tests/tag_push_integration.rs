//! Integration tests for `anodizer tag --push` and its flag family.
//!
//! `anodizer tag` historically pushed only the version-sync bump commit's
//! TAG to the remote, leaving the branch HEAD behind — orphaning the
//! `chore(release): bump …` commit on origin. `--push` makes the tag and the
//! bump commit land atomically (`git push --atomic`). These tests prove:
//!   * default (no `--push`): lockstep path leaves remote branch BEHIND the tag
//!     target (the load-bearing guard that proves the flag matters);
//!   * `--push`: remote branch HEAD == tag target == local HEAD;
//!   * a no-op run (no version change) creates no bump commit even with
//!     `--push`;
//!   * a non-fast-forward rejection leaves NEITHER an orphan branch tip NOR an
//!     orphan tag on the remote (atomic guarantee);
//!   * `--push-remote <name>` targets a second remote;
//!   * per-crate `--no-push` pushes the tags but not the branch.

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
        .unwrap_or_else(|e| panic!("git {:?} failed to spawn: {e}", args));
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {:?} failed to spawn: {e}", args));
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
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

fn head_sha(dir: &Path) -> String {
    git_out(dir, &["rev-parse", "HEAD"])
}

/// SHA the bare repo's `master` branch points at, or `None` when the branch
/// does not exist yet.
fn remote_branch_sha(bare: &Path, branch: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["--git-dir"])
        .arg(bare)
        .args(["rev-parse", &format!("refs/heads/{branch}")])
        .output()
        .unwrap();
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

/// Dereferenced SHA the remote tag points at, or `None` when absent.
fn remote_tag_target(bare: &Path, tag: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["--git-dir"])
        .arg(bare)
        .args(["rev-parse", &format!("refs/tags/{tag}^{{}}")])
        .output()
        .unwrap();
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

/// Create a bare repo and return its path-holding TempDir.
fn make_bare() -> TempDir {
    let bare = TempDir::new().unwrap();
    let out = Command::new("git")
        .args(["init", "--bare", "-q", "-b", "master"])
        .arg(bare.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "git init --bare failed");
    bare
}

/// Lockstep workspace fixture (inheriting `[workspace.package].version`) wired
/// to a bare `origin`, baseline tag `v0.1.0`, and one patch-worthy commit.
/// Returns `(work, bare)`.
fn lockstep_with_origin() -> (TempDir, TempDir) {
    let work = TempDir::new().unwrap();
    let bare = make_bare();
    fs::write(
        work.path().join("Cargo.toml"),
        r#"[workspace]
members = ["crates/a"]
resolver = "2"

[workspace.package]
version = "0.1.0"
"#,
    )
    .unwrap();
    fs::create_dir_all(work.path().join("crates/a/src")).unwrap();
    fs::write(
        work.path().join("crates/a/Cargo.toml"),
        r#"[package]
name = "a"
version.workspace = true
edition = "2024"
"#,
    )
    .unwrap();
    fs::write(work.path().join("crates/a/src/lib.rs"), "").unwrap();

    git_init(work.path());
    run_git(
        work.path(),
        &["remote", "add", "origin", bare.path().to_str().unwrap()],
    );
    git_add_commit(work.path(), "initial");
    run_git(work.path(), &["push", "origin", "master"]);
    run_git(work.path(), &["tag", "v0.1.0"]);
    run_git(work.path(), &["push", "origin", "v0.1.0"]);

    fs::write(work.path().join("crates/a/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(work.path(), "fix: a deref issue");

    (work, bare)
}

/// Flat two-crate per-crate workspace wired to a bare `origin`, baseline tags,
/// and one commit touching `core`. Returns `(work, bare)`.
fn per_crate_with_origin() -> (TempDir, TempDir) {
    let work = TempDir::new().unwrap();
    let bare = make_bare();
    fs::write(
        work.path().join("Cargo.toml"),
        r#"[workspace]
members = ["crates/core", "crates/cli"]
resolver = "2"
"#,
    )
    .unwrap();
    for (name, path) in [("core", "crates/core"), ("cli", "crates/cli")] {
        fs::create_dir_all(work.path().join(path).join("src")).unwrap();
        fs::write(
            work.path().join(path).join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
        )
        .unwrap();
        fs::write(work.path().join(path).join("src/lib.rs"), "").unwrap();
    }
    fs::write(
        work.path().join(".anodizer.yaml"),
        r#"project_name: myproj
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

    git_init(work.path());
    run_git(
        work.path(),
        &["remote", "add", "origin", bare.path().to_str().unwrap()],
    );
    git_add_commit(work.path(), "initial");
    run_git(work.path(), &["push", "origin", "master"]);
    run_git(work.path(), &["tag", "core-v0.1.0"]);
    run_git(work.path(), &["tag", "cli-v0.1.0"]);
    run_git(
        work.path(),
        &["push", "origin", "core-v0.1.0", "cli-v0.1.0"],
    );

    fs::write(work.path().join("crates/core/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(work.path(), "fix: core bug");

    (work, bare)
}

#[test]
fn lockstep_default_orphans_bump_commit_on_remote() {
    // Proves the flag is load-bearing: WITHOUT --push, the lockstep path pushes
    // only the tag, leaving remote master BEHIND the tag target.
    let (work, bare) = lockstep_with_origin();
    let remote_master_before = remote_branch_sha(bare.path(), "master").unwrap();

    let out = anodizer()
        .current_dir(work.path())
        .args(["tag"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tag failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let local_head = head_sha(work.path());
    let remote_master_after = remote_branch_sha(bare.path(), "master").unwrap();
    let tag_target = remote_tag_target(bare.path(), "v0.1.1").expect("tag pushed");

    // Tag landed and points at the local bump commit, but remote master did
    // NOT advance — the bump commit is orphaned on origin.
    assert_eq!(tag_target, local_head, "tag should target the bump commit");
    assert_eq!(
        remote_master_after, remote_master_before,
        "default run must NOT advance remote master"
    );
    assert_ne!(
        remote_master_after, local_head,
        "default run leaves remote master behind the tag target (orphan)"
    );
}

#[test]
fn lockstep_push_lands_bump_commit_atomically() {
    // The proof: with --push, remote master == tag target == local HEAD.
    let (work, bare) = lockstep_with_origin();

    let out = anodizer()
        .current_dir(work.path())
        .args(["tag", "--push"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tag --push failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let local_head = head_sha(work.path());
    let remote_master = remote_branch_sha(bare.path(), "master").expect("master on remote");
    let tag_target = remote_tag_target(bare.path(), "v0.1.1").expect("tag pushed");

    assert_eq!(
        remote_master, local_head,
        "remote master must reach the bump commit (no orphan)"
    );
    assert_eq!(
        tag_target, local_head,
        "remote tag must point at the bump commit"
    );
    assert_eq!(
        remote_master, tag_target,
        "remote branch and tag must agree (atomic push)"
    );
}

#[test]
fn push_noop_when_no_version_change_creates_no_bump_commit() {
    // A run with no version change (#none) must not create a bump commit and
    // must exit success, even with --push.
    let work = TempDir::new().unwrap();
    let bare = make_bare();
    fs::write(
        work.path().join("Cargo.toml"),
        r#"[workspace]
members = ["crates/a"]
resolver = "2"

[workspace.package]
version = "0.1.0"
"#,
    )
    .unwrap();
    fs::create_dir_all(work.path().join("crates/a/src")).unwrap();
    fs::write(
        work.path().join("crates/a/Cargo.toml"),
        "[package]\nname = \"a\"\nversion.workspace = true\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(work.path().join("crates/a/src/lib.rs"), "").unwrap();
    git_init(work.path());
    run_git(
        work.path(),
        &["remote", "add", "origin", bare.path().to_str().unwrap()],
    );
    git_add_commit(work.path(), "initial");
    run_git(work.path(), &["push", "origin", "master"]);
    run_git(work.path(), &["tag", "v0.1.0"]);
    run_git(work.path(), &["push", "origin", "v0.1.0"]);

    // A #none commit: no bump signal.
    fs::write(work.path().join("crates/a/src/lib.rs"), "// chore\n").unwrap();
    git_add_commit(work.path(), "chore: tidy #none");

    let head_before = head_sha(work.path());
    let out = anodizer()
        .current_dir(work.path())
        .args(["tag", "--push"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "no-op tag --push must exit 0: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let head_after = head_sha(work.path());
    assert_eq!(
        head_before, head_after,
        "no version change must NOT create a bump commit"
    );
    // No bump commit exists in history.
    let log = git_out(work.path(), &["log", "--format=%s", "-5"]);
    assert!(
        !log.contains("chore(release): bump"),
        "no `chore(release): bump` commit should be added: {log}"
    );
}

#[test]
fn push_non_fast_forward_leaves_no_orphan_branch_or_tag() {
    // Atomic guarantee: when the remote branch has advanced past local (the
    // push is a non-fast-forward), the whole push fails and NEITHER the branch
    // tip NOR the tag lands on the remote.
    let (work, bare) = lockstep_with_origin();

    // Advance the bare remote's master past the local tip via a second clone so
    // the upcoming --push is a non-fast-forward.
    let other = TempDir::new().unwrap();
    run_git(
        other.path(),
        &["clone", "-q", bare.path().to_str().unwrap(), "."],
    );
    run_git(other.path(), &["config", "user.email", "o@o.com"]);
    run_git(other.path(), &["config", "user.name", "Other"]);
    fs::write(other.path().join("UNRELATED"), "x\n").unwrap();
    git_add_commit(other.path(), "chore: advance remote");
    run_git(other.path(), &["push", "origin", "master"]);

    let remote_master_before = remote_branch_sha(bare.path(), "master").unwrap();

    let out = anodizer()
        .current_dir(work.path())
        .args(["tag", "--push"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "non-fast-forward push must fail: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // The error must be actionable, not a raw refspec dump.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("non-fast-forward") && stderr.contains("Pull/rebase"),
        "expected an actionable non-fast-forward hint: {stderr}"
    );

    let remote_master_after = remote_branch_sha(bare.path(), "master").unwrap();
    assert_eq!(
        remote_master_after, remote_master_before,
        "rejected push must NOT move remote master"
    );
    assert_eq!(
        remote_tag_target(bare.path(), "v0.1.1"),
        None,
        "rejected atomic push must NOT leave a dangling remote tag"
    );
}

#[test]
fn push_remote_targets_named_remote() {
    // --push-remote <name> should push to a second remote, not origin.
    let (work, _origin) = lockstep_with_origin();
    let upstream = make_bare();
    run_git(
        work.path(),
        &[
            "remote",
            "add",
            "upstream",
            upstream.path().to_str().unwrap(),
        ],
    );
    // Seed upstream master so the push is a fast-forward of an existing branch.
    run_git(work.path(), &["push", "upstream", "v0.1.0"]);

    let out = anodizer()
        .current_dir(work.path())
        .args(["tag", "--push", "--push-remote", "upstream"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tag --push --push-remote upstream failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let local_head = head_sha(work.path());
    assert_eq!(
        remote_branch_sha(upstream.path(), "master").as_deref(),
        Some(local_head.as_str()),
        "upstream master must reach the bump commit"
    );
    assert_eq!(
        remote_tag_target(upstream.path(), "v0.1.1").as_deref(),
        Some(local_head.as_str()),
        "upstream tag must point at the bump commit"
    );
}

#[test]
fn per_crate_no_push_pushes_tags_but_not_branch() {
    // Per-crate auto-dispatch defaults to pushing branch+tags. With --no-push,
    // the tags must land but the branch (bump commit) must NOT.
    let (work, bare) = per_crate_with_origin();
    let remote_master_before = remote_branch_sha(bare.path(), "master").unwrap();

    let out = anodizer()
        .current_dir(work.path())
        .args(["tag", "--no-push"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "per-crate tag --no-push failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let local_head = head_sha(work.path());
    let remote_master_after = remote_branch_sha(bare.path(), "master").unwrap();
    assert_eq!(
        remote_master_after, remote_master_before,
        "--no-push must NOT advance remote master"
    );
    assert_ne!(
        remote_master_after, local_head,
        "--no-push leaves remote master behind the bump commit"
    );
    // The tag still lands on the remote.
    assert_eq!(
        remote_tag_target(bare.path(), "core-v0.1.1").as_deref(),
        Some(local_head.as_str()),
        "--no-push must still push the tag"
    );
}

#[test]
fn per_crate_default_pushes_branch_and_tags_atomically() {
    // Guard the per-crate default (push=true) against regression: branch+tags
    // both reach the remote without any flag.
    let (work, bare) = per_crate_with_origin();

    let out = anodizer()
        .current_dir(work.path())
        .args(["tag"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "per-crate tag failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let local_head = head_sha(work.path());
    assert_eq!(
        remote_branch_sha(bare.path(), "master").as_deref(),
        Some(local_head.as_str()),
        "per-crate default must advance remote master to the bump commit"
    );
    assert_eq!(
        remote_tag_target(bare.path(), "core-v0.1.1").as_deref(),
        Some(local_head.as_str()),
        "per-crate default must push the tag"
    );
}

#[test]
fn crate_targeted_push_lands_bump_commit_atomically() {
    // The cfgd rollout path: `tag --crate <name> --push` drives the
    // single-crate fall-through (version_sync, not apply_workspace_bump) and
    // must push the bump commit + tag atomically.
    let (work, bare) = per_crate_with_origin();

    let out = anodizer()
        .current_dir(work.path())
        .args(["tag", "--crate", "core", "--push"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tag --crate core --push failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let local_head = head_sha(work.path());
    let remote_master = remote_branch_sha(bare.path(), "master").expect("master on remote");
    let tag_target = remote_tag_target(bare.path(), "core-v0.1.1").expect("tag pushed");

    assert_eq!(
        remote_master, local_head,
        "remote master must reach the bump commit (no orphan)"
    );
    assert_eq!(
        tag_target, local_head,
        "remote tag must point at the bump commit"
    );
    assert_eq!(
        remote_master, tag_target,
        "remote branch and tag must agree (atomic push)"
    );
    // The untargeted crate must NOT be tagged.
    assert_eq!(
        remote_tag_target(bare.path(), "cli-v0.1.1"),
        None,
        "cli must not be tagged when only core was targeted"
    );
}

#[test]
fn push_dry_run_creates_tag_locally_but_pushes_nothing() {
    // --push-dry-run still creates the tag + bump commit locally but only
    // PRINTS the git push commands; the remote must be untouched.
    let (work, bare) = lockstep_with_origin();
    let remote_master_before = remote_branch_sha(bare.path(), "master").unwrap();

    let out = anodizer()
        .current_dir(work.path())
        .args(["tag", "--push-dry-run"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tag --push-dry-run must exit 0: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // Tagging combined stdout+stderr should announce a dry-run push.
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("(dry-run) would push"),
        "expected a '(dry-run) would push' line: {combined}"
    );

    // The remote must be untouched: master unchanged, no new tag.
    let remote_master_after = remote_branch_sha(bare.path(), "master").unwrap();
    assert_eq!(
        remote_master_after, remote_master_before,
        "--push-dry-run must NOT advance remote master"
    );
    assert_eq!(
        remote_tag_target(bare.path(), "v0.1.1"),
        None,
        "--push-dry-run must NOT push the tag"
    );
}

#[test]
fn tag_rollback_rejects_push_flags() {
    // The --push family lives on the parent `tag` command, so `tag rollback
    // --push` parses but is meaningless — it must be rejected, not silently
    // ignored.
    let (work, _bare) = lockstep_with_origin();

    // `--push` placed BEFORE the subcommand binds to the parent `tag` command
    // and parses cleanly; the runtime guard must reject it.
    let out = anodizer()
        .current_dir(work.path())
        .args(["tag", "--push", "rollback", "--dry-run"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "tag --push rollback must be rejected: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--push family") && stderr.contains("tag rollback"),
        "expected a clear rollback rejection message: {stderr}"
    );
}

/// The newest `bump`-prefixed commit subject in HEAD's history.
fn latest_bump_subject(dir: &Path) -> String {
    let log = git_out(dir, &["log", "--format=%s", "-10"]);
    log.lines()
        .find(|s| s.contains("bump"))
        .unwrap_or("")
        .to_string()
}

#[test]
fn lockstep_bump_subject_omits_skip_ci_by_default() {
    // Default (no tag.skip_ci_on_bump): the bump commit subject must NOT carry
    // `[skip ci]`, so a tag-push-triggered release isn't silently suppressed.
    let (work, _bare) = lockstep_with_origin();

    let out = anodizer()
        .current_dir(work.path())
        .args(["tag", "--no-push"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tag failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let subject = latest_bump_subject(work.path());
    assert!(
        subject.contains("bump workspace"),
        "expected a workspace bump commit: {subject}"
    );
    assert!(
        !subject.contains("[skip ci]"),
        "default bump subject must NOT contain [skip ci]: {subject}"
    );
}

#[test]
fn lockstep_bump_subject_has_skip_ci_when_enabled() {
    // tag.skip_ci_on_bump: true → the bump commit subject carries `[skip ci]`.
    let (work, _bare) = lockstep_with_origin();
    fs::write(
        work.path().join(".anodizer.yaml"),
        "project_name: myproj\ntag:\n  skip_ci_on_bump: true\n",
    )
    .unwrap();
    // Stage the config into the same commit so change detection still fires on
    // the source edit already present in the fixture.
    git_add_commit(work.path(), "chore: enable skip_ci_on_bump");

    let out = anodizer()
        .current_dir(work.path())
        .args(["tag", "--no-push"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tag failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let subject = latest_bump_subject(work.path());
    assert!(
        subject.contains("bump workspace") && subject.contains("[skip ci]"),
        "enabled bump subject must contain [skip ci]: {subject}"
    );
}

#[test]
fn per_crate_bump_subject_omits_skip_ci_by_default() {
    // Per-crate dispatch path: default bump subject must NOT carry [skip ci].
    let (work, _bare) = per_crate_with_origin();

    let out = anodizer()
        .current_dir(work.path())
        .args(["tag", "--no-push"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tag failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let subject = latest_bump_subject(work.path());
    assert!(
        subject.contains("chore(release): bump"),
        "expected a per-crate bump commit: {subject}"
    );
    assert!(
        !subject.contains("[skip ci]"),
        "default per-crate bump subject must NOT contain [skip ci]: {subject}"
    );
}

#[test]
fn per_crate_bump_subject_has_skip_ci_when_enabled() {
    // Per-crate dispatch path with tag.skip_ci_on_bump: true → [skip ci] present.
    let (work, _bare) = per_crate_with_origin();
    // Re-write the config preserving the per-crate crates: block and adding the
    // top-level tag.skip_ci_on_bump toggle.
    fs::write(
        work.path().join(".anodizer.yaml"),
        r#"project_name: myproj
tag:
  skip_ci_on_bump: true
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
    git_add_commit(work.path(), "chore: enable skip_ci_on_bump");

    let out = anodizer()
        .current_dir(work.path())
        .args(["tag", "--no-push"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tag failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let subject = latest_bump_subject(work.path());
    assert!(
        subject.contains("chore(release): bump") && subject.contains("[skip ci]"),
        "enabled per-crate bump subject must contain [skip ci]: {subject}"
    );
}
