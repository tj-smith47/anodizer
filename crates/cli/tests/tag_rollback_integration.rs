//! Integration test for `anodize tag rollback`.
//!
//! Fixture: bare-bones repo with an initial commit + a "bump" commit
//! that lands a tag. Run `anodizer tag rollback --no-push` against
//! HEAD and assert:
//! - the tag at HEAD is gone,
//! - a revert commit replaces HEAD,
//! - the revert commit's subject carries the `chore(release): rollback`
//!   prefix and `[skip ci]` marker.

use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

/// Build an `anodizer` command wired through [`sandbox_env`]. Returns the
/// command paired with the scratch [`TempDir`] guard the env points at; the
/// caller must hold the guard until after the command runs so the redirected
/// HOME / XDG dir is not reclaimed mid-spawn.
fn anodizer() -> (Command, TempDir) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_anodizer"));
    let scratch = sandbox_env(&mut cmd);
    (cmd, scratch)
}

/// Redirect HOME / GIT_CONFIG_GLOBAL / GIT_CONFIG_SYSTEM at every git
/// child the test spawns so the host's global / system git config
/// (notably `commit.gpgsign = true`) cannot affect fixture-repo
/// behavior. Per-repo `git config user.email ...` writes already pin
/// the test repo, but `git commit` reads global settings on top of
/// that — without the redirection, a host with `commit.gpgsign`
/// globally enabled would try to sign with an unconfigured key and
/// the test would fail with a confusing "no secret key" error.
#[must_use = "hold the returned TempDir until after the command runs"]
fn sandbox_env(cmd: &mut Command) -> TempDir {
    // One scratch dir per spawn. Returned to the caller (rather than leaked)
    // so it is reclaimed at the end of the caller's scope, not the process —
    // it only needs to outlive the single command that reads HOME/XDG from it.
    let scratch = TempDir::new().expect("scratch tempdir for HOME redirect");
    cmd.env("HOME", scratch.path())
        .env("XDG_CONFIG_HOME", scratch.path())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    scratch
}

fn run_git(dir: &Path, args: &[&str]) {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    let _scratch = sandbox_env(&mut cmd);
    cmd.env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@test.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@test.com");
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("git {args:?} failed to spawn: {e}"));
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

/// Identity-less init variant: skips the per-repo `user.email` /
/// `user.name` writes so the test mirrors a CI host where
/// `actions/checkout@v6` cloned the repo without committer identity
/// configured. The rollback path must fall back to the committer-env
/// injection so the revert commit still lands.
///
/// The setup commits still need *some* identity (the repo itself
/// has none) — those are provided per-spawn via env. The eventual
/// `anodizer tag rollback` invocation is what runs without inherited
/// `GIT_AUTHOR_*` / `GIT_COMMITTER_*` and exercises the fallback.
fn git_init_no_identity(dir: &Path) {
    run_git(dir, &["init", "-q", "-b", "master"]);
    run_git(dir, &["config", "commit.gpgsign", "false"]);
}

/// Give the fixture a resolvable non-github.com origin: the
/// published-state guard's release probe is inapplicable there (warn +
/// proceed), so these offline `--no-push` / `--dry-run` tests exercise
/// their actual subject instead of the unresolvable-origin fail-closed
/// refusal. The URL is never contacted.
fn add_non_github_origin(dir: &Path) {
    run_git(
        dir,
        &["remote", "add", "origin", "https://gitlab.example/o/r.git"],
    );
}

fn git_tag_exists(dir: &Path, tag: &str) -> bool {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(["tag", "-l", tag]);
    let _scratch = sandbox_env(&mut cmd);
    let out = cmd.output().unwrap();
    !String::from_utf8_lossy(&out.stdout).trim().is_empty()
}

fn git_head_subject(dir: &Path) -> String {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .args(["log", "-1", "--format=%s", "HEAD"]);
    let _scratch = sandbox_env(&mut cmd);
    let out = cmd.output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn git_head_sha(dir: &Path) -> String {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(["rev-parse", "HEAD"]);
    let _scratch = sandbox_env(&mut cmd);
    let out = cmd.output().unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

#[test]
fn tag_rollback_local_deletes_tag_and_creates_revert_commit() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    git_init(dir);

    fs::write(dir.join("README.md"), "init\n").unwrap();
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-q", "-m", "initial"]);

    // Simulate an anodize-style bump commit + tag.
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-q", "-m", "chore(release): v1.0.0"]);
    run_git(dir, &["tag", "v1.0.0"]);
    add_non_github_origin(dir);

    assert!(
        git_tag_exists(dir, "v1.0.0"),
        "fixture setup: tag v1.0.0 should exist before rollback"
    );

    let (mut cmd, _scratch) = anodizer();
    let out = cmd
        .current_dir(dir)
        .args(["tag", "rollback", "--no-push"])
        .output()
        .expect("anodizer tag rollback should spawn");
    assert!(
        out.status.success(),
        "anodizer tag rollback exited with status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    assert!(
        !git_tag_exists(dir, "v1.0.0"),
        "expected v1.0.0 to be deleted after rollback"
    );

    let subj = git_head_subject(dir);
    assert!(
        subj.starts_with("chore(release): rollback v1.0.0"),
        "unexpected HEAD subject after rollback: {subj}"
    );
    assert!(
        subj.contains("[skip ci]"),
        "rollback commit subject should contain [skip ci]: {subj}"
    );
}

#[test]
fn tag_rollback_dry_run_makes_no_mutations() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    git_init(dir);

    fs::write(dir.join("README.md"), "init\n").unwrap();
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-q", "-m", "initial"]);

    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-q", "-m", "chore(release): v1.0.0"]);
    run_git(dir, &["tag", "v1.0.0"]);
    add_non_github_origin(dir);
    let head_before = git_head_sha(dir);

    let (mut cmd, _scratch) = anodizer();
    let out = cmd
        .current_dir(dir)
        .args(["tag", "rollback", "--dry-run", "--no-push"])
        .output()
        .expect("anodizer tag rollback --dry-run should spawn");
    assert!(out.status.success(), "dry-run should succeed");

    assert!(
        git_tag_exists(dir, "v1.0.0"),
        "dry-run must not delete tags"
    );
    let head_after = git_head_sha(dir);
    assert_eq!(head_before, head_after, "dry-run must not move HEAD");
}

/// B-R1: simulate a bare-CI host. The repo's git config has neither
/// `user.email` nor `user.name`, and the anodizer subprocess is
/// spawned without inherited `GIT_AUTHOR_*` / `GIT_COMMITTER_*` env.
/// `anodize tag rollback` must still land the revert commit by
/// injecting a synthetic identity for the spawn — without that
/// fallback the CI rollback step would die with "Author identity
/// unknown".
#[test]
fn tag_rollback_works_on_identity_less_host() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    git_init_no_identity(dir);

    fs::write(dir.join("README.md"), "init\n").unwrap();
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-q", "-m", "initial"]);
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-q", "-m", "chore(release): v1.0.0"]);
    run_git(dir, &["tag", "v1.0.0"]);
    add_non_github_origin(dir);

    // Spawn anodizer WITHOUT inheriting committer env. Manually
    // construct the command (the `anodizer()` helper would inherit
    // committer env if the test process had it, but we explicitly
    // clear it here so the fallback path is exercised).
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_anodizer"));
    cmd.current_dir(dir)
        .args(["tag", "rollback", "--no-push"])
        .env_remove("GIT_AUTHOR_NAME")
        .env_remove("GIT_AUTHOR_EMAIL")
        .env_remove("GIT_COMMITTER_NAME")
        .env_remove("GIT_COMMITTER_EMAIL");
    let _scratch = sandbox_env(&mut cmd);
    let out = cmd
        .output()
        .expect("anodizer tag rollback should spawn on identity-less host");
    assert!(
        out.status.success(),
        "rollback must succeed without configured/inherited committer identity; \
         stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        !git_tag_exists(dir, "v1.0.0"),
        "expected v1.0.0 to be deleted on identity-less rollback"
    );
    let subj = git_head_subject(dir);
    assert!(
        subj.starts_with("chore(release): rollback v1.0.0"),
        "revert must land even when user.email/name are unset: {subj}"
    );

    // Repo's LOCAL config must remain identity-less — the fix is
    // env-only, never `git config user.email ...`. (Use `--local` so
    // the host's global `~/.gitconfig` doesn't leak into the check.)
    let mut cfg_cmd = Command::new("git");
    cfg_cmd
        .current_dir(dir)
        .args(["config", "--local", "--get", "user.email"]);
    let _scratch = sandbox_env(&mut cfg_cmd);
    let cfg = cfg_cmd.output().unwrap();
    assert!(
        !cfg.status.success() || cfg.stdout.is_empty(),
        "rollback must not write user.email into repo's local config; got: {}",
        String::from_utf8_lossy(&cfg.stdout)
    );
}
