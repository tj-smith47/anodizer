//! Integration tests for `anodizer tag --push` and its flag family.
//!
//! A tag run either pushes the version-sync bump commit and the tag(s)
//! together (`git push --atomic`) or pushes nothing; a tag pushed without its
//! bump commit (a remote orphan tag) is only producible via the explicit
//! `--push-tags-only` opt-in. These tests prove:
//!   * default (no `--push`): fully local — neither the tag nor the branch
//!     reaches the remote, and the run says so;
//!   * `--push`: remote branch HEAD == tag target == local HEAD;
//!   * `--push-tags-only`: the tag lands, the branch does not (the
//!     deferred-branch CI pattern);
//!   * a no-op run (no version change) creates no bump commit even with
//!     `--push`;
//!   * a non-fast-forward rejection leaves NEITHER an orphan branch tip NOR an
//!     orphan tag on the remote (atomic guarantee);
//!   * `--push-remote <name>` targets a second remote;
//!   * per-crate `--no-push` pushes nothing;
//!   * previous-tag resolution consults the remote's tag list, so a re-cut
//!     from a clone still holding a remotely-deleted tag re-mints the SAME
//!     version (with local fallback + warn when the remote is unreachable).

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

fn git_out(dir: &Path, args: &[&str]) -> String {
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
    let out = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.args(["--git-dir"])
                .arg(bare)
                .args(["rev-parse", &format!("refs/heads/{branch}")]);
            cmd
        },
        "git",
    );
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

/// Dereferenced SHA the remote tag points at, or `None` when absent.
fn remote_tag_target(bare: &Path, tag: &str) -> Option<String> {
    let out = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.args(["--git-dir"])
                .arg(bare)
                .args(["rev-parse", &format!("refs/tags/{tag}^{{}}")]);
            cmd
        },
        "git",
    );
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

/// Create a bare repo and return its path-holding TempDir.
fn make_bare() -> TempDir {
    let bare = TempDir::new().unwrap();
    let out = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.args(["init", "--bare", "-q", "-b", "master"])
                .arg(bare.path());
            cmd
        },
        "git",
    );
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
fn lockstep_default_pushes_nothing() {
    // A bare `anodizer tag` is fully local: the tag and the bump commit both
    // stay in the clone. Pushing only the tag (the historical default) left a
    // remote orphan tag whose bump commit no branch contained.
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

    // The tag exists locally, points at the bump commit…
    let local_tag = git_out(work.path(), &["rev-parse", "refs/tags/v0.1.1^{}"]);
    assert_eq!(
        local_tag, local_head,
        "local tag must target the bump commit"
    );
    // …but NOTHING reached the remote.
    assert_eq!(
        remote_tag_target(bare.path(), "v0.1.1"),
        None,
        "default run must NOT push the tag"
    );
    assert_eq!(
        remote_master_after, remote_master_before,
        "default run must NOT advance remote master"
    );
    // The run tells the user everything stayed local.
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("nothing was pushed"),
        "expected a local-only hint: {combined}"
    );
}

#[test]
fn git_api_tagging_bare_run_pushes_nothing() {
    // git_api_tagging drives tag creation through the GitHub API, which tags a
    // commit already on the remote. A bare `anodizer tag` (no push flag) must
    // still stay fully local: the API path must NOT fire and orphan a remote
    // tag whose bump commit no branch on the remote contains.
    let (work, bare) = lockstep_with_origin();
    fs::write(
        work.path().join(".anodizer.yaml"),
        "tag:\n  git_api_tagging: true\n",
    )
    .unwrap();
    git_add_commit(work.path(), "chore: enable git_api_tagging");
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
    let local_tag = git_out(work.path(), &["rev-parse", "refs/tags/v0.1.1^{}"]);
    assert_eq!(
        local_tag, local_head,
        "local tag must target the bump commit"
    );
    assert_eq!(
        remote_tag_target(bare.path(), "v0.1.1"),
        None,
        "git_api_tagging must NOT push the tag on a bare run"
    );
    assert_eq!(
        remote_branch_sha(bare.path(), "master").unwrap(),
        remote_master_before,
        "git_api_tagging must NOT advance remote master on a bare run"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("nothing was pushed"),
        "expected a local-only hint: {combined}"
    );
}

#[test]
#[cfg(unix)]
fn git_api_tagging_push_dry_run_previews_without_calling_the_api() {
    // The GitHub-API tagger creates the tag ref on the REMOTE, so it is a push
    // operation and must honour `--push-dry-run`. A preview must NOT invoke
    // `gh api` — doing so would create a real remote tag on a commit no pushed
    // branch contains (an orphan tag), the exact footgun push-preview avoids.
    use std::os::unix::fs::PermissionsExt;

    let work = TempDir::new().unwrap();
    let root = work.path();
    // A GitHub-looking origin so the API-tagging branch is taken; nothing is
    // ever contacted under a preview.
    run_git(root, &["init", "-q", "-b", "master"]);
    run_git(root, &["config", "user.email", "t@t.com"]);
    run_git(root, &["config", "user.name", "t"]);
    run_git(root, &["config", "commit.gpgsign", "false"]);
    run_git(
        root,
        &["remote", "add", "origin", "https://github.com/fake/repo"],
    );
    fs::create_dir_all(root.join("crates/a/src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/a\"]\nresolver = \"2\"\n\n[workspace.package]\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        root.join("crates/a/Cargo.toml"),
        "[package]\nname = \"a\"\nversion.workspace = true\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(root.join("crates/a/src/lib.rs"), "").unwrap();
    fs::write(
        root.join(".anodizer.yaml"),
        "tag:\n  git_api_tagging: true\n",
    )
    .unwrap();
    git_add_commit(root, "initial");
    run_git(root, &["tag", "v0.1.0"]);
    fs::write(root.join("crates/a/src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(root, "fix: a deref issue");

    // A `gh` stub on PATH that records every invocation. A preview must leave
    // this log empty.
    let stub_dir = root.join("ghbin");
    fs::create_dir_all(&stub_dir).unwrap();
    let call_log = root.join("gh_calls.log");
    fs::write(
        stub_dir.join("gh"),
        "#!/usr/bin/env bash\necho \"GH_CALLED: $*\" >> \"$GH_CALL_LOG\"\necho '{}'\n",
    )
    .unwrap();
    fs::set_permissions(stub_dir.join("gh"), fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        stub_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let out = anodizer()
        .current_dir(root)
        .args(["tag", "--push-dry-run"])
        .env("PATH", path)
        .env("GH_CALL_LOG", &call_log)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "push-dry-run must succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("would create tag v0.1.1 via GitHub API"),
        "the API tagging step must be previewed, not executed: {combined}"
    );
    let logged = fs::read_to_string(&call_log).unwrap_or_default();
    assert!(
        logged.trim().is_empty(),
        "a preview must not invoke `gh api`; recorded calls:\n{logged}"
    );
}

#[test]
fn lockstep_push_tags_only_pushes_tag_without_branch() {
    // The explicit deferred-branch CI pattern: the tag lands on the remote
    // (triggering tag-driven pipelines) while the bump commit stays local
    // until the caller fast-forwards the branch post-publish.
    let (work, bare) = lockstep_with_origin();
    let remote_master_before = remote_branch_sha(bare.path(), "master").unwrap();

    let out = anodizer()
        .current_dir(work.path())
        .args(["tag", "--push-tags-only"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tag --push-tags-only failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let local_head = head_sha(work.path());
    assert_eq!(
        remote_tag_target(bare.path(), "v0.1.1").as_deref(),
        Some(local_head.as_str()),
        "--push-tags-only must push the tag"
    );
    assert_eq!(
        remote_branch_sha(bare.path(), "master").unwrap(),
        remote_master_before,
        "--push-tags-only must NOT advance remote master"
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
fn per_crate_no_push_pushes_nothing() {
    // Per-crate is fully local by default; --no-push is the explicit,
    // redundant form of that same outcome — nothing reaches the remote.
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
    assert_eq!(
        remote_tag_target(bare.path(), "core-v0.1.1"),
        None,
        "--no-push must NOT push the tag"
    );
    // The tag still exists locally at the bump commit.
    let local_tag = git_out(work.path(), &["rev-parse", "refs/tags/core-v0.1.1^{}"]);
    assert_eq!(
        local_tag, local_head,
        "local tag must target the bump commit"
    );
}

#[test]
fn per_crate_push_tags_only_pushes_tags_without_branch() {
    // The deferred-branch pattern in per-crate dispatch: tags land, the bump
    // commit does not.
    let (work, bare) = per_crate_with_origin();
    let remote_master_before = remote_branch_sha(bare.path(), "master").unwrap();

    let out = anodizer()
        .current_dir(work.path())
        .args(["tag", "--push-tags-only"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "per-crate tag --push-tags-only failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let local_head = head_sha(work.path());
    assert_eq!(
        remote_tag_target(bare.path(), "core-v0.1.1").as_deref(),
        Some(local_head.as_str()),
        "--push-tags-only must push the tag"
    );
    assert_eq!(
        remote_branch_sha(bare.path(), "master").unwrap(),
        remote_master_before,
        "--push-tags-only must NOT advance remote master"
    );
}

#[test]
fn per_crate_default_is_fully_local() {
    // Every dispatch shape shares one default: a bare run pushes NOTHING.
    // Per-crate is no exception — the tag is created locally, the remote is
    // untouched, and reaching the remote requires an explicit --push /
    // --push-tags-only (mirroring `git tag`).
    let (work, bare) = per_crate_with_origin();
    let remote_master_before = remote_branch_sha(bare.path(), "master").unwrap();

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
        remote_branch_sha(bare.path(), "master").unwrap(),
        remote_master_before,
        "bare per-crate tag must NOT advance remote master"
    );
    assert_eq!(
        remote_tag_target(bare.path(), "core-v0.1.1"),
        None,
        "bare per-crate tag must NOT push the tag"
    );
    // The tag exists locally at the bump commit.
    let local_tag = git_out(work.path(), &["rev-parse", "refs/tags/core-v0.1.1^{}"]);
    assert_eq!(
        local_tag, local_head,
        "local per-crate tag must target the bump commit"
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
fn failed_version_sync_bump_commit_aborts_before_tagging() {
    // Safety guard: the single-crate version_sync path must propagate a bump
    // commit failure BEFORE creating the tag. A pre-commit hook that rejects
    // the `chore(release): bump …` commit must abort the whole `tag` run — otherwise
    // the tag would point at a commit whose Cargo.toml is NOT at the tagged
    // version (an orphan tag at a mismatched commit).
    let (work, bare) = per_crate_with_origin();

    // Install a pre-commit hook that always rejects, so stage_and_commit's
    // `git commit` fails. The tag must NOT be created locally or pushed.
    let hooks_dir = work.path().join(".git/hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let hook = hooks_dir.join("pre-commit");
    fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&hook).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&hook, perms).unwrap();
    }

    let out = anodizer()
        .current_dir(work.path())
        .args(["tag", "--crate", "core", "--push"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "tag must fail when the version_sync bump commit is rejected: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // No tag may exist locally...
    let local_tags = git_out(work.path(), &["tag", "--list", "core-v0.1.1"]);
    assert!(
        local_tags.is_empty(),
        "a failed bump must not leave a local tag: {local_tags:?}"
    );
    // ...nor on the remote.
    assert_eq!(
        remote_tag_target(bare.path(), "core-v0.1.1"),
        None,
        "a failed bump must not produce a pushed tag"
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
fn per_crate_push_dry_run_previews_atomic_push_without_pushing() {
    // Standalone --push-dry-run (no --push) must preview the atomic branch+tags
    // push in per-crate dispatch too, mirroring single/lockstep — not fall
    // through to the fully-local "nothing was pushed" path.
    let (work, bare) = per_crate_with_origin();
    let remote_master_before = remote_branch_sha(bare.path(), "master").unwrap();

    let out = anodizer()
        .current_dir(work.path())
        .args(["tag", "--push-dry-run"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "per-crate tag --push-dry-run must exit 0: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("(dry-run) would push branch"),
        "expected an atomic branch+tags push preview: {combined}"
    );

    // The remote must be untouched.
    assert_eq!(
        remote_branch_sha(bare.path(), "master").unwrap(),
        remote_master_before,
        "--push-dry-run must NOT advance remote master"
    );
    assert_eq!(
        remote_tag_target(bare.path(), "core-v0.1.1"),
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

/// Single-crate (non-workspace) fixture wired to a bare `origin`, baseline tag
/// `v0.1.0`, and one patch-worthy commit. Returns `(work, bare)`.
fn single_crate_with_origin() -> (TempDir, TempDir) {
    let work = TempDir::new().unwrap();
    let bare = make_bare();
    fs::write(
        work.path().join("Cargo.toml"),
        "[package]\nname = \"solo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::create_dir_all(work.path().join("src")).unwrap();
    fs::write(work.path().join("src/lib.rs"), "").unwrap();

    git_init(work.path());
    run_git(
        work.path(),
        &["remote", "add", "origin", bare.path().to_str().unwrap()],
    );
    git_add_commit(work.path(), "initial");
    run_git(work.path(), &["push", "origin", "master"]);
    run_git(work.path(), &["tag", "v0.1.0"]);
    run_git(work.path(), &["push", "origin", "v0.1.0"]);

    fs::write(work.path().join("src/lib.rs"), "// touched\n").unwrap();
    git_add_commit(work.path(), "fix: solo bug");

    (work, bare)
}

#[test]
fn single_crate_default_pushes_nothing() {
    // The single-crate path shares the fully-local default with lockstep.
    let (work, bare) = single_crate_with_origin();
    let remote_master_before = remote_branch_sha(bare.path(), "master").unwrap();

    let out = anodizer()
        .current_dir(work.path())
        .args(["tag"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "single-crate tag failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let local_tag = git_out(work.path(), &["rev-parse", "refs/tags/v0.1.1^{}"]);
    assert!(!local_tag.is_empty(), "tag must exist locally");
    assert_eq!(
        remote_tag_target(bare.path(), "v0.1.1"),
        None,
        "default run must NOT push the tag"
    );
    assert_eq!(
        remote_branch_sha(bare.path(), "master").unwrap(),
        remote_master_before,
        "default run must NOT advance remote master"
    );
}

/// Delete `tag` from the bare remote directly (the documented re-cut recipe's
/// `git push origin :refs/tags/<tag>` as seen from the server side).
fn delete_remote_tag(bare: &Path, tag: &str) {
    let out = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.args(["--git-dir"]).arg(bare).args([
                "update-ref",
                "-d",
                &format!("refs/tags/{tag}"),
            ]);
            cmd
        },
        "git",
    );
    assert!(
        out.status.success(),
        "remote tag delete failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn lockstep_recut_after_remote_tag_delete_remints_same_version() {
    // The documented re-cut recipe: delete the remote tag, push fixes, and the
    // next tag run must mint the SAME version — even from a clone that still
    // holds the deleted tag locally. Previous-tag resolution must follow the
    // REMOTE's tag list, not this clone's.
    let (work, bare) = lockstep_with_origin();

    // Cut and push v0.1.1 fully, then delete it on the remote only.
    let out = anodizer()
        .current_dir(work.path())
        .args(["tag", "--push"])
        .output()
        .unwrap();
    assert!(out.status.success(), "initial tag --push failed");
    assert!(remote_tag_target(bare.path(), "v0.1.1").is_some());
    delete_remote_tag(bare.path(), "v0.1.1");

    // A follow-up fix lands; the clone still holds the stale local v0.1.1.
    fs::write(work.path().join("crates/a/src/lib.rs"), "// fixed again\n").unwrap();
    git_add_commit(work.path(), "fix: the real fix");

    let out = anodizer()
        .current_dir(work.path())
        .args(["tag", "--dry-run"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "re-cut dry-run failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("new_tag=v0.1.1"),
        "re-cut must re-mint the SAME version v0.1.1 (remote is the source of truth): {stdout}"
    );
    assert!(
        stdout.contains("old_tag=v0.1.0"),
        "previous tag must be v0.1.0, not the remotely-deleted v0.1.1: {stdout}"
    );
}

#[test]
fn single_crate_recut_after_remote_tag_delete_remints_same_version() {
    let (work, bare) = single_crate_with_origin();

    let out = anodizer()
        .current_dir(work.path())
        .args(["tag", "--push"])
        .output()
        .unwrap();
    assert!(out.status.success(), "initial tag --push failed");
    assert!(remote_tag_target(bare.path(), "v0.1.1").is_some());
    delete_remote_tag(bare.path(), "v0.1.1");

    fs::write(work.path().join("src/lib.rs"), "// fixed again\n").unwrap();
    git_add_commit(work.path(), "fix: the real fix");

    let out = anodizer()
        .current_dir(work.path())
        .args(["tag", "--dry-run"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "re-cut dry-run failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("new_tag=v0.1.1") && stdout.contains("old_tag=v0.1.0"),
        "single-crate re-cut must re-mint v0.1.1 over v0.1.0: {stdout}"
    );
}

#[test]
fn per_crate_recut_after_remote_tag_delete_remints_same_version() {
    let (work, bare) = per_crate_with_origin();

    // Land core-v0.1.1 on the remote (explicit --push) so the re-cut path has a
    // remote tag to delete.
    let out = anodizer()
        .current_dir(work.path())
        .args(["tag", "--push"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "initial per-crate tag --push failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(remote_tag_target(bare.path(), "core-v0.1.1").is_some());
    delete_remote_tag(bare.path(), "core-v0.1.1");

    fs::write(
        work.path().join("crates/core/src/lib.rs"),
        "// fixed again\n",
    )
    .unwrap();
    git_add_commit(work.path(), "fix: core again");

    let out = anodizer()
        .current_dir(work.path())
        .args(["tag", "--dry-run"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "per-crate re-cut dry-run failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"core\":\"0.1.1\""),
        "per-crate re-cut must re-mint core 0.1.1 (stale local core-v0.1.1 must not count): {stdout}"
    );
}

#[test]
fn previous_tag_falls_back_to_local_when_remote_unreachable() {
    // An unreachable remote must not block tagging: previous-tag resolution
    // warns and falls back to the local tag list.
    let (work, _bare) = lockstep_with_origin();
    run_git(
        work.path(),
        &[
            "remote",
            "set-url",
            "origin",
            "/nonexistent/never-a-repo.git",
        ],
    );

    let out = anodizer()
        .current_dir(work.path())
        .args(["tag", "--dry-run"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tag must succeed offline: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("falling back to LOCAL tags"),
        "expected a remote-unreachable warning: {combined}"
    );
    assert!(
        combined.contains("new_tag=v0.1.1"),
        "local fallback must still derive v0.1.1: {combined}"
    );
}

/// Configure ephemeral SSH tag-signing on `dir` (no gpg-agent needed) and
/// return the key dir so the key file outlives the repo for the whole test.
fn configure_ssh_signing(dir: &Path) -> TempDir {
    let keydir = TempDir::new().unwrap();
    let key_path = keydir.path().join("sign_key");
    let keygen = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("ssh-keygen");
            cmd.args(["-t", "ed25519", "-N", "", "-C", "anodizer-test", "-f"])
                .arg(&key_path);
            cmd
        },
        "ssh-keygen",
    );
    assert!(
        keygen.status.success(),
        "ssh-keygen failed: {}",
        String::from_utf8_lossy(&keygen.stderr)
    );
    let pub_path = format!("{}.pub", key_path.display());
    run_git(dir, &["config", "gpg.format", "ssh"]);
    run_git(dir, &["config", "user.signingkey", &pub_path]);
    keydir
}

/// True when the local annotated tag object embeds an SSH signature block.
fn local_tag_is_signed(dir: &Path, tag: &str) -> bool {
    git_out(dir, &["cat-file", "tag", tag]).contains("-----BEGIN SSH SIGNATURE-----")
}

#[test]
fn tag_sign_config_creates_signed_local_tag() {
    // `tag.sign: true` in config makes a bare (local) `anodizer tag` cut a
    // signed annotated tag; the key/method come from the fixture's git config.
    let (work, _bare) = lockstep_with_origin();
    let _keydir = configure_ssh_signing(work.path());
    fs::write(work.path().join(".anodizer.yaml"), "tag:\n  sign: true\n").unwrap();
    git_add_commit(work.path(), "chore: enable signed tags");

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
    assert!(
        local_tag_is_signed(work.path(), "v0.1.1"),
        "tag.sign=true must produce a signed local tag"
    );
}

#[test]
fn tag_sign_cli_flag_creates_signed_local_tag() {
    // `--sign` opts into a signed tag even with no `tag.sign` in config, and
    // `--no-sign` overrides `tag.sign: true` back to an unsigned tag.
    let (work, _bare) = lockstep_with_origin();
    let _keydir = configure_ssh_signing(work.path());

    let out = anodizer()
        .current_dir(work.path())
        .args(["tag", "--sign"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tag --sign failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        local_tag_is_signed(work.path(), "v0.1.1"),
        "--sign must produce a signed local tag"
    );
}

#[test]
fn tag_no_sign_overrides_config_to_unsigned() {
    let (work, _bare) = lockstep_with_origin();
    let _keydir = configure_ssh_signing(work.path());
    fs::write(work.path().join(".anodizer.yaml"), "tag:\n  sign: true\n").unwrap();
    git_add_commit(work.path(), "chore: enable signed tags");

    let out = anodizer()
        .current_dir(work.path())
        .args(["tag", "--no-sign"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tag --no-sign failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !local_tag_is_signed(work.path(), "v0.1.1"),
        "--no-sign must override tag.sign=true and leave the tag unsigned"
    );
}

#[test]
fn tag_sign_with_git_api_tagging_pushed_is_rejected() {
    // A signed tag cannot be created via the GitHub API: the API mints the tag
    // object server-side, out of reach of the local signing key. Rather than
    // ship a silently-unsigned tag, the run must hard-error before creating any
    // tag. `--push-dry-run` enters push mode (push_mode=true), so the guard
    // fires without contacting a real remote.
    let (work, _bare) = lockstep_with_origin();
    let _keydir = configure_ssh_signing(work.path());
    fs::write(
        work.path().join(".anodizer.yaml"),
        "tag:\n  git_api_tagging: true\n",
    )
    .unwrap();
    git_add_commit(work.path(), "chore: enable git_api_tagging");

    let out = anodizer()
        .current_dir(work.path())
        .args(["tag", "--sign", "--push-dry-run"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "signed tag + git_api_tagging on a pushed tag must be rejected; stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("git_api_tagging"),
        "error must name git_api_tagging: {stderr}"
    );
    // No new tag was cut — only the fixture baseline remains.
    let tags = git_out(work.path(), &["tag", "--list"]);
    assert!(
        !tags.contains("v0.1.1"),
        "the rejected run must create no tag; tags present: {tags:?}"
    );
}
