use super::*;
use std::path::Path;
use std::process::Command;

fn init_repo_with_commits(dir: &Path, files: &[&str]) {
    let run = |args: &[&str]| {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args)
                    .current_dir(dir)
                    .env("GIT_AUTHOR_NAME", "t")
                    .env("GIT_AUTHOR_EMAIL", "t@t.com")
                    .env("GIT_COMMITTER_NAME", "t")
                    .env("GIT_COMMITTER_EMAIL", "t@t.com");
                cmd
            },
            "git",
        );
        assert!(out.status.success(), "git {args:?} failed");
    };
    run(&["init"]);
    run(&["config", "user.email", "t@t.com"]);
    run(&["config", "user.name", "t"]);
    for (i, f) in files.iter().enumerate() {
        std::fs::write(dir.join(f), format!("c{i}")).unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", &format!("commit-{i}: {f}")]);
    }
}

#[test]
fn changelog_provenance_marker_binds_to_its_crate_and_version() {
    let tmp = tempfile::tempdir().unwrap();
    init_repo_with_commits(tmp.path(), &["a"]);
    std::fs::write(tmp.path().join("CHANGELOG.md"), "notes").unwrap();
    let msg = format!(
        "{}\n\n{}",
        release_bump_subject("core→0.12.0", ""),
        changelog_regenerated_marker("core", "0.12.0")
    );
    stage_and_commit_in(tmp.path(), &["CHANGELOG.md"], &msg).unwrap();
    let probe = |name: &str, ver: &str| {
        changelog_regenerated_recorded_in(tmp.path(), name, ver, "CHANGELOG.md").unwrap()
    };
    assert!(probe("core", "0.12.0"));
    // A sibling crate at the same version has no provenance of its own.
    assert!(!probe("cli", "0.12.0"));
    // A different version must not be vouched for, including the
    // substring-adjacent form (0.12.0 vs 0.12.01).
    assert!(!probe("core", "0.12.1"));
    assert!(!probe("core", "0.12.01"));
}

#[test]
fn changelog_provenance_scopes_to_the_probed_changelog_path() {
    let tmp = tempfile::tempdir().unwrap();
    init_repo_with_commits(tmp.path(), &["a"]);
    std::fs::create_dir_all(tmp.path().join("crates/core")).unwrap();
    std::fs::write(tmp.path().join("crates/core/CHANGELOG.md"), "notes").unwrap();
    let msg = format!(
        "{}\n\n{}",
        release_bump_subject("core→0.2.0", ""),
        changelog_regenerated_marker("core", "0.2.0")
    );
    stage_and_commit_in(tmp.path(), &["crates/core/CHANGELOG.md"], &msg).unwrap();
    assert!(
        changelog_regenerated_recorded_in(tmp.path(), "core", "0.2.0", "crates/core/CHANGELOG.md")
            .unwrap()
    );
    // The marker vouches only for the file the bump commit touched — a
    // never-committed root CHANGELOG.md has no provenance.
    assert!(
        !changelog_regenerated_recorded_in(tmp.path(), "core", "0.2.0", "CHANGELOG.md").unwrap()
    );
}

#[test]
fn changelog_provenance_superseded_by_a_later_hand_edit() {
    let tmp = tempfile::tempdir().unwrap();
    init_repo_with_commits(tmp.path(), &["a"]);
    std::fs::write(tmp.path().join("CHANGELOG.md"), "notes").unwrap();
    let msg = format!(
        "{}\n\n{}",
        release_bump_subject("core→0.2.0", ""),
        changelog_regenerated_marker("core", "0.2.0")
    );
    stage_and_commit_in(tmp.path(), &["CHANGELOG.md"], &msg).unwrap();
    assert!(
        changelog_regenerated_recorded_in(tmp.path(), "core", "0.2.0", "CHANGELOG.md").unwrap()
    );

    // A later commit touching an UNRELATED file keeps the provenance: the
    // marker commit is still the changelog's last toucher.
    std::fs::write(tmp.path().join("other.txt"), "x").unwrap();
    stage_and_commit_in(tmp.path(), &["other.txt"], "chore: unrelated").unwrap();
    assert!(
        changelog_regenerated_recorded_in(tmp.path(), "core", "0.2.0", "CHANGELOG.md").unwrap()
    );

    // A later hand-edit to the changelog makes the operator's commit the
    // last toucher — no marker there, so the provenance no longer holds.
    std::fs::write(tmp.path().join("CHANGELOG.md"), "notes\nedited").unwrap();
    stage_and_commit_in(tmp.path(), &["CHANGELOG.md"], "docs: tweak changelog").unwrap();
    assert!(
        !changelog_regenerated_recorded_in(tmp.path(), "core", "0.2.0", "CHANGELOG.md").unwrap()
    );
}

#[test]
fn changelog_provenance_absent_when_no_marker_committed() {
    let tmp = tempfile::tempdir().unwrap();
    init_repo_with_commits(tmp.path(), &["a"]);
    assert!(
        !changelog_regenerated_recorded_in(tmp.path(), "core", "0.2.0", "CHANGELOG.md").unwrap()
    );
}

#[test]
fn get_head_commit_in_returns_tempdirs_head_sha() {
    let tmp = tempfile::tempdir().unwrap();
    init_repo_with_commits(tmp.path(), &["a"]);
    let expected = String::from_utf8(
        anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(["rev-parse", "HEAD"]).current_dir(tmp.path());
                cmd
            },
            "git",
        )
        .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    let sha = get_head_commit_in(tmp.path()).unwrap();
    assert_eq!(sha, expected);
}

#[test]
fn get_short_commit_in_returns_tempdirs_short_sha() {
    let tmp = tempfile::tempdir().unwrap();
    init_repo_with_commits(tmp.path(), &["a"]);
    let expected = String::from_utf8(
        anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(["rev-parse", "--short", "HEAD"])
                    .current_dir(tmp.path());
                cmd
            },
            "git",
        )
        .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    let short = get_short_commit_in(tmp.path()).unwrap();
    assert_eq!(short, expected);
}

#[test]
fn has_commits_since_tag_in_returns_false_when_tag_is_head() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_commits(dir, &["a"]);
    let run = |args: &[&str]| {
        anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args)
                    .current_dir(dir)
                    .env("GIT_AUTHOR_NAME", "t")
                    .env("GIT_AUTHOR_EMAIL", "t@t.com")
                    .env("GIT_COMMITTER_NAME", "t")
                    .env("GIT_COMMITTER_EMAIL", "t@t.com");
                cmd
            },
            "git",
        );
    };
    run(&["tag", "v1.0.0"]);
    assert!(!has_commits_since_tag_in(dir, "v1.0.0").unwrap());
}

fn git_in(dir: &Path, args: &[&str]) {
    let out = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t.com");
            cmd
        },
        "git",
    );
    assert!(out.status.success(), "git {args:?} failed");
}

#[test]
fn count_commits_since_last_tag_counts_commits_after_tag() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // 2 commits, tag v1.0.0 at the 2nd, then 3 more commits.
    init_repo_with_commits(dir, &["a", "b"]);
    git_in(dir, &["tag", "v1.0.0"]);
    for f in ["c", "d", "e"] {
        std::fs::write(dir.join(f), "x").unwrap();
        git_in(dir, &["add", "."]);
        git_in(dir, &["commit", "-m", f]);
    }
    assert_eq!(count_commits_since_last_tag_in(dir, None).unwrap(), 3);
}

#[test]
fn count_commits_since_last_tag_resets_on_newer_tag() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_commits(dir, &["a"]);
    git_in(dir, &["tag", "v1.0.0"]);
    for f in ["b", "c"] {
        std::fs::write(dir.join(f), "x").unwrap();
        git_in(dir, &["add", "."]);
        git_in(dir, &["commit", "-m", f]);
    }
    assert_eq!(count_commits_since_last_tag_in(dir, None).unwrap(), 2);
    // A newer version tag lands -> counter resets to 0 at the tag.
    git_in(dir, &["tag", "v1.1.0"]);
    assert_eq!(count_commits_since_last_tag_in(dir, None).unwrap(), 0);
    std::fs::write(dir.join("d"), "x").unwrap();
    git_in(dir, &["add", "."]);
    git_in(dir, &["commit", "-m", "d"]);
    assert_eq!(count_commits_since_last_tag_in(dir, None).unwrap(), 1);
}

#[test]
fn count_commits_since_last_tag_counts_all_when_no_tag() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_commits(dir, &["a", "b", "c"]);
    // No tag at all -> count every commit on HEAD.
    assert_eq!(count_commits_since_last_tag_in(dir, None).unwrap(), 3);
}

#[test]
fn count_commits_since_last_tag_respects_monorepo_prefix() {
    // Per-crate workspace: tags for two subprojects interleave on one
    // branch. The `core/` count must be since the latest `core/*` tag,
    // NOT the nearer `api/*` tag from a different subproject.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_commits(dir, &["a"]);
    git_in(dir, &["tag", "core/v1.0.0"]); // matching-prefix tag (older)
    for f in ["b", "c"] {
        std::fs::write(dir.join(f), "x").unwrap();
        git_in(dir, &["add", "."]);
        git_in(dir, &["commit", "-m", f]);
    }
    git_in(dir, &["tag", "api/v2.0.0"]); // DIFFERENT prefix, NEARER to HEAD
    std::fs::write(dir.join("d"), "x").unwrap();
    git_in(dir, &["add", "."]);
    git_in(dir, &["commit", "-m", "d"]);

    // With prefix filtering: count since core/v1.0.0 = 3 commits (b, c, d).
    assert_eq!(
        count_commits_since_last_tag_in(dir, Some("core/")).unwrap(),
        3,
        "must count since the matching-prefix tag, ignoring api/v2.0.0",
    );
    // Without filtering (None): describe picks the nearer api/v2.0.0,
    // so the count is only 1 (d). This is the mutation-check baseline
    // proving the --match arg is load-bearing.
    assert_eq!(
        count_commits_since_last_tag_in(dir, None).unwrap(),
        1,
        "unfiltered count picks the nearest (wrong) subproject tag",
    );
}

#[test]
fn get_current_branch_in_returns_branch_name() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let run = |args: &[&str]| {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args)
                    .current_dir(dir)
                    .env("GIT_AUTHOR_NAME", "t")
                    .env("GIT_AUTHOR_EMAIL", "t@t.com")
                    .env("GIT_COMMITTER_NAME", "t")
                    .env("GIT_COMMITTER_EMAIL", "t@t.com");
                cmd
            },
            "git",
        );
        assert!(out.status.success(), "git {args:?} failed");
    };
    run(&["-c", "init.defaultBranch=t1-test-branch", "init"]);
    run(&["config", "user.email", "t@t.com"]);
    run(&["config", "user.name", "t"]);
    std::fs::write(dir.join("a"), "1").unwrap();
    run(&["add", "."]);
    run(&["commit", "-m", "c1"]);
    let branch = get_current_branch_in(dir).unwrap();
    assert_eq!(branch, "t1-test-branch");
}

#[test]
fn get_current_branch_in_resolves_detached_head_via_points_at() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let run = |args: &[&str]| {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args)
                    .current_dir(dir)
                    .env("GIT_AUTHOR_NAME", "t")
                    .env("GIT_AUTHOR_EMAIL", "t@t.com")
                    .env("GIT_COMMITTER_NAME", "t")
                    .env("GIT_COMMITTER_EMAIL", "t@t.com");
                cmd
            },
            "git",
        );
        assert!(out.status.success(), "git {args:?} failed");
    };
    run(&["-c", "init.defaultBranch=master", "init"]);
    run(&["config", "user.email", "t@t.com"]);
    run(&["config", "user.name", "t"]);
    std::fs::write(dir.join("a"), "1").unwrap();
    run(&["add", "."]);
    run(&["commit", "-m", "c1"]);
    let sha = get_head_commit_in(dir).unwrap();
    run(&["checkout", "--detach", &sha]);
    let branch = get_current_branch_in(dir).unwrap();
    assert_eq!(
        branch, "master",
        "detached HEAD pointing at master must resolve to master, not literal HEAD"
    );
}

#[test]
fn is_branchlike_rejects_lockstep_tag_shapes() {
    assert!(!is_branchlike("v0.4.5"));
    assert!(!is_branchlike("v1.2.3"));
    assert!(!is_branchlike("v10.20.30"));
    assert!(!is_branchlike("v1.2.3-rc.1"));
    assert!(!is_branchlike("v1.2.3+build.42"));
}

#[test]
fn is_branchlike_rejects_per_crate_tag_shapes() {
    assert!(!is_branchlike("mycrate-v1.2.3"));
    assert!(!is_branchlike("cfgd-operator-v0.4.0"));
    assert!(!is_branchlike("anodize-core-v1.2.3-rc.1"));
}

#[test]
fn is_branchlike_accepts_real_branch_names() {
    assert!(is_branchlike("master"));
    assert!(is_branchlike("main"));
    assert!(is_branchlike("publisher-required-config"));
    assert!(is_branchlike("release/v1.2.3-prep"));
    assert!(is_branchlike("dependabot/cargo/serde-1.0.200"));
}

#[test]
fn is_branchlike_accepts_slashed_branch_with_embedded_version() {
    // `feature/fix-v2.0.0` embeds `-v2.0.0` but is a branch, not a
    // per-crate tag: the unanchored `-v\d+\.\d+\.\d+` regex misclassified
    // it as a tag. The `^[^/]+-v` anchor keeps slashed branch names
    // branch-like.
    assert!(is_branchlike("feature/fix-v2.0.0"));
    assert!(is_branchlike("hotfix/release-v1.0.0-blocker"));
    assert!(is_branchlike("user/wip-v3.1.4"));
}

#[test]
fn get_current_branch_in_rejects_tag_shaped_github_ref_name() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let run = |args: &[&str]| {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args)
                    .current_dir(dir)
                    .env("GIT_AUTHOR_NAME", "t")
                    .env("GIT_AUTHOR_EMAIL", "t@t.com")
                    .env("GIT_COMMITTER_NAME", "t")
                    .env("GIT_COMMITTER_EMAIL", "t@t.com");
                cmd
            },
            "git",
        );
        assert!(out.status.success(), "git {args:?} failed");
    };
    // Build a repo whose HEAD is detached AND no local branch points
    // at it, so every fallback BEFORE GITHUB_REF_NAME fails. The only
    // way the fallback chain produces a value is via the env var.
    run(&["-c", "init.defaultBranch=master", "init"]);
    run(&["config", "user.email", "t@t.com"]);
    run(&["config", "user.name", "t"]);
    std::fs::write(dir.join("a"), "1").unwrap();
    run(&["add", "."]);
    run(&["commit", "-m", "c1"]);
    let sha = get_head_commit_in(dir).unwrap();
    // Move master forward so the detached HEAD has no branch
    // pointing at it; for-each-ref --points-at HEAD returns empty.
    std::fs::write(dir.join("a"), "2").unwrap();
    run(&["add", "."]);
    run(&["commit", "-m", "c2"]);
    run(&["checkout", "--detach", &sha]);

    // GITHUB_REF_NAME is injected via the env seam, so each branch of the
    // fallback is driven without mutating process-global env.

    // Tag-shaped: must NOT be accepted; bail surfaces.
    let env = crate::MapEnvSource::new().with("GITHUB_REF_NAME", "v0.4.5");
    let err = get_current_branch_in_with_env(dir, &env).unwrap_err();
    assert!(
        err.to_string().contains("could not resolve current branch"),
        "tag-shaped GITHUB_REF_NAME must trigger bail: {err}"
    );

    // Per-crate-shaped: must NOT be accepted either.
    let env = crate::MapEnvSource::new().with("GITHUB_REF_NAME", "mycrate-v1.2.3");
    let err = get_current_branch_in_with_env(dir, &env).unwrap_err();
    assert!(
        err.to_string().contains("could not resolve current branch"),
        "per-crate tag GITHUB_REF_NAME must trigger bail: {err}"
    );

    // Real branch name: accepted.
    let env = crate::MapEnvSource::new().with("GITHUB_REF_NAME", "master");
    let branch = get_current_branch_in_with_env(dir, &env).unwrap();
    assert_eq!(branch, "master");
}

#[test]
fn branches_containing_sha_in_returns_empty_without_remote() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let run = |args: &[&str]| {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args)
                    .current_dir(dir)
                    .env("GIT_AUTHOR_NAME", "t")
                    .env("GIT_AUTHOR_EMAIL", "t@t.com")
                    .env("GIT_COMMITTER_NAME", "t")
                    .env("GIT_COMMITTER_EMAIL", "t@t.com");
                cmd
            },
            "git",
        );
        assert!(out.status.success(), "git {args:?} failed");
    };
    run(&["-c", "init.defaultBranch=master", "init"]);
    run(&["config", "user.email", "t@t.com"]);
    run(&["config", "user.name", "t"]);
    std::fs::write(dir.join("a"), "1").unwrap();
    run(&["add", "."]);
    run(&["commit", "-m", "c1"]);
    let sha = get_head_commit_in(dir).unwrap();
    // No remote configured → `git branch -r --contains` returns
    // empty, which the helper surfaces as `Vec::new()` so the
    // caller can fall back to local branch resolution.
    let branches = branches_containing_sha_in(dir, &sha).unwrap();
    assert!(branches.is_empty(), "no remote → no remote branches");
}

#[test]
fn branches_containing_sha_in_finds_remote_branch_after_push() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let run_in = |cwd: &Path, args: &[&str]| {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args)
                    .current_dir(cwd)
                    .env("GIT_AUTHOR_NAME", "t")
                    .env("GIT_AUTHOR_EMAIL", "t@t.com")
                    .env("GIT_COMMITTER_NAME", "t")
                    .env("GIT_COMMITTER_EMAIL", "t@t.com");
                cmd
            },
            "git",
        );
        assert!(out.status.success(), "git {args:?} failed");
    };
    run_in(
        bare.path(),
        &["-c", "init.defaultBranch=master", "init", "--bare"],
    );
    run_in(dir, &["-c", "init.defaultBranch=master", "init"]);
    run_in(dir, &["config", "user.email", "t@t.com"]);
    run_in(dir, &["config", "user.name", "t"]);
    run_in(
        dir,
        &["remote", "add", "origin", bare.path().to_str().unwrap()],
    );
    std::fs::write(dir.join("a"), "1").unwrap();
    run_in(dir, &["add", "."]);
    run_in(dir, &["commit", "-m", "c1"]);
    let sha = get_head_commit_in(dir).unwrap();
    run_in(dir, &["push", "-u", "origin", "master"]);

    let branches = branches_containing_sha_in(dir, &sha).unwrap();
    assert_eq!(branches, vec!["master".to_string()]);
}

#[test]
fn stage_and_commit_in_returns_false_when_no_diff() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_commits(dir, &["a"]);
    // File is committed and unchanged — staging it should not produce
    // a diff, and stage_and_commit must report Ok(false) instead of
    // bailing on the "nothing to commit" path.
    let created = stage_and_commit_in(dir, &["a"], "chore: should be a no-op").unwrap();
    assert!(!created, "no diff → no commit should be created");
    let log = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.args(["log", "--oneline"]).current_dir(dir);
            cmd
        },
        "git",
    );
    let log_text = String::from_utf8_lossy(&log.stdout);
    assert!(
        !log_text.contains("should be a no-op"),
        "stage_and_commit_in must not create a commit when no diff: {log_text}"
    );
}

#[test]
fn stage_and_commit_in_returns_true_when_file_changed() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_commits(dir, &["a"]);
    std::fs::write(dir.join("a"), "changed").unwrap();
    let created = stage_and_commit_in(dir, &["a"], "chore: real change").unwrap();
    assert!(created, "real change → commit must be created");
    let log = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.args(["log", "-1", "--pretty=%s"]).current_dir(dir);
            cmd
        },
        "git",
    );
    let subject = String::from_utf8_lossy(&log.stdout).trim().to_string();
    assert_eq!(subject, "chore: real change");
}

#[test]
fn git_output_in_error_falls_back_to_stdout_when_stderr_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo_with_commits(dir, &["a"]);
    // `git commit -m ...` with an unchanged tree prints "nothing to
    // commit" to STDOUT (not stderr); the error message must surface
    // that detail instead of `failed: ` with nothing after.
    let err = git_output_in(dir, &["commit", "-m", "no-op"]).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("nothing to commit") || msg.contains("clean"),
        "error must include stdout detail when stderr is empty: {msg}"
    );
}

/// `CommitterIdentity::default_for_rollback` produces a populated
/// (name + email) identity. The exact host-derived suffix isn't
/// load-bearing — what matters is that both fields are present so
/// `apply_to` produces all four `GIT_AUTHOR_*` / `GIT_COMMITTER_*`
/// envs on the spawn.
#[test]
fn default_for_rollback_populates_both_name_and_email() {
    let id = CommitterIdentity::default_for_rollback();
    assert_eq!(id.name.as_deref(), Some("anodize-rollback"));
    let email = id.email.expect("email must be Some");
    assert!(
        email.starts_with("anodize-rollback@"),
        "email must use the anodize-rollback@<host> shape; got {email}"
    );
    assert!(!email.ends_with('@'), "host portion must not be empty");
}

/// `revert_commit_in` with an injected `CommitterIdentity` writes a
/// commit whose author/committer match the identity. Exercises the
/// env-injection path end-to-end against a real fixture repo whose
/// only configured identity is the override — so a future regression
/// that drops the env threading would show up as the commit
/// inheriting the host's `user.email` instead.
#[test]
fn revert_commit_in_uses_injected_identity_envs() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let run_env = |args: &[&str], extra: &[(&str, &str)]| {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args)
                    .current_dir(dir)
                    .env("GIT_AUTHOR_NAME", "bootstrap")
                    .env("GIT_AUTHOR_EMAIL", "bootstrap@b.com")
                    .env("GIT_COMMITTER_NAME", "bootstrap")
                    .env("GIT_COMMITTER_EMAIL", "bootstrap@b.com");
                for (k, v) in extra {
                    cmd.env(k, v);
                }
                cmd
            },
            "git",
        );
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run_env(&["init", "-b", "master"], &[]);
    std::fs::write(dir.join("a"), "0").unwrap();
    run_env(&["add", "."], &[]);
    run_env(&["commit", "-m", "initial"], &[]);
    std::fs::write(dir.join("a"), "1").unwrap();
    run_env(&["add", "."], &[]);
    run_env(&["commit", "-m", "chore(release): v1.0.0"], &[]);
    let bump_sha = get_head_commit_in(dir).unwrap();

    // Inject a distinct identity so the resulting revert commit can
    // be attributed unambiguously to the env path (the bootstrap
    // commits used a different identity above).
    let identity = CommitterIdentity {
        name: Some("rollback-bot".to_string()),
        email: Some("rollback-bot@anodize.test".to_string()),
    };
    revert_commit_in(dir, &bump_sha, Some("chore(release): rollback"), &identity)
        .expect("revert with injected identity must succeed");

    // The new HEAD commit's author email must be the injected one,
    // proving the env threading reached the git child.
    let out = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.current_dir(dir)
                .args(["log", "-1", "--format=%ae"])
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("LC_ALL", "C");
            cmd
        },
        "git",
    );
    let author_email = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(
        author_email, "rollback-bot@anodize.test",
        "revert commit must carry the injected committer identity"
    );

    // Repo config must remain unchanged — env-only fallback, no
    // `git config user.email ...` mutation.
    let cfg = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.current_dir(dir)
                .args(["config", "--local", "--get", "user.email"])
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("LC_ALL", "C");
            cmd
        },
        "git",
    );
    assert!(
        !cfg.status.success() || cfg.stdout.is_empty(),
        "revert must not write user.email into the repo's local config; got: {}",
        String::from_utf8_lossy(&cfg.stdout)
    );
}

/// B-R4: a revert that hits conflicts (because later commits overlap
/// with the bump) must run `git revert --abort`, restoring the working
/// tree so the operator isn't trapped by the dirty-tree guard on the
/// next attempt. Bail message must mention "aborted".
#[test]
fn revert_commit_in_aborts_on_conflict_and_leaves_tree_clean() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let run = |args: &[&str]| {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args)
                    .current_dir(dir)
                    .env("GIT_AUTHOR_NAME", "t")
                    .env("GIT_AUTHOR_EMAIL", "t@t.com")
                    .env("GIT_COMMITTER_NAME", "t")
                    .env("GIT_COMMITTER_EMAIL", "t@t.com");
                cmd
            },
            "git",
        );
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(&["init", "-b", "master"]);
    run(&["config", "user.email", "t@t.com"]);
    run(&["config", "user.name", "t"]);
    // Initial commit: file `x` with line "v1".
    std::fs::write(dir.join("x"), "v1\n").unwrap();
    run(&["add", "."]);
    run(&["commit", "-m", "initial"]);
    // "Bump" commit: change to "v2".
    std::fs::write(dir.join("x"), "v2\n").unwrap();
    run(&["add", "."]);
    run(&["commit", "-m", "chore(release): v2"]);
    let bump_sha = get_head_commit_in(dir).unwrap();
    // Later overlapping commit: change to "v3". A revert of the bump
    // would try to restore "v1" from a base of "v2", but HEAD is now
    // "v3" — that's the canonical revert-conflict shape.
    std::fs::write(dir.join("x"), "v3\n").unwrap();
    run(&["add", "."]);
    run(&["commit", "-m", "feat: overlap"]);

    let identity = CommitterIdentity::default();
    let err = revert_commit_in(dir, &bump_sha, None, &identity)
        .expect_err("revert against overlapping HEAD must conflict and bail");
    let msg = format!("{err}");
    assert!(
        msg.contains("aborted"),
        "bail message must mention abort recovery: {msg}"
    );

    // Working tree must be clean post-bail: no REVERT_HEAD, no
    // unmerged paths. The next rollback attempt must NOT hit the
    // dirty-tree guard.
    assert!(
        !dir.join(".git/REVERT_HEAD").exists(),
        ".git/REVERT_HEAD must be cleaned up after --abort"
    );
    let status_out = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.args(["status", "--porcelain"]).current_dir(dir);
            cmd
        },
        "git",
    );
    assert!(
        status_out.stdout.is_empty(),
        "working tree must be clean after revert --abort; got:\n{}",
        String::from_utf8_lossy(&status_out.stdout)
    );
}

/// S-R7: `commits_with_subjects_in` returns every (sha, subject)
/// pair in one git spawn. Asserts both correctness (matches per-commit
/// `commit_subject_in`) and that the range bound is exclusive on the
/// `<sha>` side.
#[test]
fn commits_with_subjects_in_returns_all_pairs_in_one_call() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let run = |args: &[&str]| {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args)
                    .current_dir(dir)
                    .env("GIT_AUTHOR_NAME", "t")
                    .env("GIT_AUTHOR_EMAIL", "t@t.com")
                    .env("GIT_COMMITTER_NAME", "t")
                    .env("GIT_COMMITTER_EMAIL", "t@t.com");
                cmd
            },
            "git",
        );
        assert!(out.status.success(), "git {args:?} failed");
    };
    run(&["init", "-b", "master"]);
    run(&["config", "user.email", "t@t.com"]);
    run(&["config", "user.name", "t"]);
    std::fs::write(dir.join("a"), "0").unwrap();
    run(&["add", "."]);
    run(&["commit", "-m", "initial"]);
    let base = get_head_commit_in(dir).unwrap();
    std::fs::write(dir.join("a"), "1").unwrap();
    run(&["add", "."]);
    run(&["commit", "-m", "feat: A with extra detail"]);
    std::fs::write(dir.join("a"), "2").unwrap();
    run(&["add", "."]);
    run(&["commit", "-m", "fix: B"]);

    let pairs = commits_with_subjects_in(dir, &base).unwrap();
    assert_eq!(pairs.len(), 2, "two commits sit on top of base");
    // Newest-first ordering (matches `git log` default).
    assert_eq!(pairs[0].1, "fix: B");
    assert_eq!(pairs[1].1, "feat: A with extra detail");

    // Empty range (sha IS HEAD) → empty vec.
    let head = get_head_commit_in(dir).unwrap();
    assert!(commits_with_subjects_in(dir, &head).unwrap().is_empty());
}

#[test]
fn parse_commit_output_with_files_pairs_each_commit_with_its_files() {
    // Two commits: newest first (git log order). Each metadata record is
    // `%H%x1f%h%x1f%s%x1f%an%x1f%ae%x1f%b%x1e`, then `--name-only` files.
    let raw = "h1\x1fs1\x1ffix: B\x1ft\x1ft@t\x1f\x1e\ncrates/cli/main.rs\n\nh0\x1fs0\x1ffeat: A\x1ft\x1ft@t\x1f\x1e\ncrates/core/lib.rs\nCargo.toml\n";
    let parsed = parse_commit_output_with_files(raw);
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].commit.message, "fix: B");
    assert_eq!(parsed[0].files, vec!["crates/cli/main.rs".to_string()]);
    assert_eq!(parsed[1].commit.message, "feat: A");
    assert_eq!(
        parsed[1].files,
        vec!["crates/core/lib.rs".to_string(), "Cargo.toml".to_string()]
    );
}

#[test]
fn parse_commit_output_with_files_preserves_multiline_body_at_idx_gt_0() {
    // A multi-line `%b` body for the SECOND commit (idx>0): the body spans
    // several newline-separated lines, and the parser must keep the full
    // record — not just its first line — so trailers like `Co-Authored-By:`
    // survive, matching the metadata-only `parse_git_log_records` path.
    let body0 = "detail line one\ndetail line two\n\nCo-Authored-By: Bob <bob@b.com>";
    let raw = format!(
        "h1\x1fs1\x1ffix: B\x1ft\x1ft@t\x1f\x1e\ncrates/cli/main.rs\n\n\
             h0\x1fs0\x1ffeat: A\x1ft\x1ft@t\x1f{body0}\x1e\ncrates/core/lib.rs\n"
    );
    let parsed = parse_commit_output_with_files(&raw);
    assert_eq!(parsed.len(), 2);
    // The idx>0 commit retains its FULL multi-line body and trailer.
    assert_eq!(parsed[1].commit.message, "feat: A");
    assert_eq!(parsed[1].commit.body, body0);
    assert!(
        parsed[1]
            .commit
            .body
            .contains("Co-Authored-By: Bob <bob@b.com>"),
        "multi-line body trailer dropped: {:?}",
        parsed[1].commit.body
    );
    assert_eq!(parsed[1].files, vec!["crates/core/lib.rs".to_string()]);
}

#[test]
fn get_commits_between_paths_with_files_in_reports_touched_files() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let run = |args: &[&str]| {
        assert!(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(args)
                        .current_dir(dir)
                        .env("GIT_AUTHOR_NAME", "t")
                        .env("GIT_AUTHOR_EMAIL", "t@t.com")
                        .env("GIT_COMMITTER_NAME", "t")
                        .env("GIT_COMMITTER_EMAIL", "t@t.com");
                    cmd
                },
                "git",
            )
            .status
            .success()
        );
    };
    run(&["init"]);
    run(&["config", "user.email", "t@t.com"]);
    run(&["config", "user.name", "t"]);
    std::fs::write(dir.join("base"), "0").unwrap();
    run(&["add", "."]);
    run(&["commit", "-m", "initial"]);
    let base = get_head_commit_in(dir).unwrap();
    std::fs::create_dir_all(dir.join("crates/core")).unwrap();
    std::fs::write(dir.join("crates/core/lib.rs"), "1").unwrap();
    run(&["add", "."]);
    run(&["commit", "-m", "feat: core"]);

    let pairs = get_commits_between_paths_with_files_in(dir, &base, "HEAD", &[]).unwrap();
    assert_eq!(pairs.len(), 1);
    assert_eq!(pairs[0].commit.message, "feat: core");
    assert_eq!(pairs[0].files, vec!["crates/core/lib.rs".to_string()]);
}

#[test]
fn get_commits_between_paths_with_files_in_preserves_multiline_body_for_later_commits() {
    // Real `git log --name-only` over TWO post-base commits, the OLDER one
    // (idx>0 in the newest-first output) carrying a multi-line body with a
    // `Co-Authored-By:` trailer. The full body must survive — proving the
    // narrowed fetch path agrees with the metadata-only path on body
    // content, not just the subject.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let run = |args: &[&str]| {
        assert!(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(args)
                        .current_dir(dir)
                        .env("GIT_AUTHOR_NAME", "t")
                        .env("GIT_AUTHOR_EMAIL", "t@t.com")
                        .env("GIT_COMMITTER_NAME", "t")
                        .env("GIT_COMMITTER_EMAIL", "t@t.com");
                    cmd
                },
                "git",
            )
            .status
            .success()
        );
    };
    run(&["init"]);
    run(&["config", "user.email", "t@t.com"]);
    run(&["config", "user.name", "t"]);
    std::fs::write(dir.join("base"), "0").unwrap();
    run(&["add", "."]);
    run(&["commit", "-m", "initial"]);
    let base = get_head_commit_in(dir).unwrap();

    // Older of the two reported commits — multi-line body + trailer.
    std::fs::write(dir.join("a.rs"), "1").unwrap();
    run(&["add", "."]);
    run(&[
        "commit",
        "-m",
        "feat: with body\n\nfirst body line\nsecond body line\n\nCo-Authored-By: Bob <bob@b.com>",
    ]);
    // Newer commit (idx 0 in newest-first output), single-line.
    std::fs::write(dir.join("b.rs"), "2").unwrap();
    run(&["add", "."]);
    run(&["commit", "-m", "fix: later"]);

    let pairs = get_commits_between_paths_with_files_in(dir, &base, "HEAD", &[]).unwrap();
    assert_eq!(pairs.len(), 2);
    // Newest-first: [0] = "fix: later", [1] = "feat: with body" (idx>0).
    assert_eq!(pairs[0].commit.message, "fix: later");
    let body = &pairs[1].commit.body;
    assert!(
        body.contains("first body line") && body.contains("second body line"),
        "multi-line body truncated for idx>0 commit: {body:?}"
    );
    assert!(
        body.contains("Co-Authored-By: Bob <bob@b.com>"),
        "Co-Authored-By trailer dropped for idx>0 commit: {body:?}"
    );
}

// ---- parse_commit_output: the single wire-format record decoder ----

#[test]
fn parse_commit_output_empty_input_yields_no_commits() {
    assert!(parse_commit_output("").is_empty());
}

#[test]
fn parse_commit_output_decodes_all_six_fields() {
    // %H%x1f%h%x1f%s%x1f%an%x1f%ae%x1f%b%x1e for a single commit.
    let raw = "abc123def\x1fabc123d\x1ffeat: add thing\x1fAlice\x1falice@x.com\x1fbody text\x1e";
    let commits = parse_commit_output(raw);
    assert_eq!(commits.len(), 1);
    let c = &commits[0];
    assert_eq!(c.hash, "abc123def");
    assert_eq!(c.short_hash, "abc123d");
    assert_eq!(c.message, "feat: add thing");
    assert_eq!(c.author_name, "Alice");
    assert_eq!(c.author_email, "alice@x.com");
    assert_eq!(c.body, "body text");
}

#[test]
fn parse_commit_output_trims_hash_and_body_but_keeps_inner_subject() {
    // Per the decoder: hash and body are trimmed; the subject (field 2)
    // is taken verbatim. A leading-newline body must come back trimmed.
    let raw = "  abc  \x1fabc\x1ffix: keep  spaces\x1ft\x1ft@t\x1f\n\nbody\n\x1e";
    let commits = parse_commit_output(raw);
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0].hash, "abc", "hash is trimmed");
    assert_eq!(commits[0].message, "fix: keep  spaces", "subject verbatim");
    assert_eq!(commits[0].body, "body", "body is trimmed");
}

#[test]
fn parse_commit_output_absent_body_field_defaults_to_empty() {
    // Exactly 5 fields (no %b segment) is still a valid record; body == "".
    let raw = "h\x1fh\x1fsubject\x1fname\x1fmail\x1e";
    let commits = parse_commit_output(raw);
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0].body, "");
    assert_eq!(commits[0].message, "subject");
}

#[test]
fn parse_commit_output_skips_records_with_too_few_fields() {
    // A record with <5 unit-separated fields is malformed and dropped,
    // while a well-formed sibling record in the same stream survives.
    let raw = "only\x1ftwo\x1e\
                   h\x1fh\x1fgood: subject\x1fn\x1fe\x1fbody\x1e";
    let commits = parse_commit_output(raw);
    assert_eq!(commits.len(), 1, "malformed record dropped, good one kept");
    assert_eq!(commits[0].message, "good: subject");
}

#[test]
fn parse_commit_output_multiline_body_survives_record_separator_split() {
    // Two commits separated by \x1e; the first body spans newlines and
    // carries a trailer — the \x1e (not \n) split keeps it intact.
    let raw = "h1\x1fh1\x1ffeat: A\x1fA\x1fa@x\x1fline one\nline two\n\nCo-Authored-By: B <b@x>\x1e\
                   h0\x1fh0\x1ffix: B\x1fB\x1fb@x\x1f\x1e";
    let commits = parse_commit_output(raw);
    assert_eq!(commits.len(), 2);
    assert_eq!(commits[0].message, "feat: A");
    assert!(commits[0].body.contains("line one\nline two"));
    assert!(commits[0].body.contains("Co-Authored-By: B <b@x>"));
    assert_eq!(commits[1].message, "fix: B");
    assert_eq!(commits[1].body, "");
}

// ---- short_commit_str: pure SHA truncation ----

#[test]
fn short_commit_str_truncates_long_sha_to_seven() {
    assert_eq!(short_commit_str("abcdef0123456789"), "abcdef0");
    assert_eq!(short_commit_str("abcdef0123456789").len(), SHORT_COMMIT_LEN);
}

#[test]
fn short_commit_str_returns_shorter_or_equal_input_unchanged() {
    assert_eq!(short_commit_str("abc"), "abc", "shorter than 7 unchanged");
    assert_eq!(
        short_commit_str("abcdefg"),
        "abcdefg",
        "exactly 7 unchanged"
    );
    assert_eq!(short_commit_str(""), "", "empty stays empty");
}

// ---- real-repo fixture helpers for the shelling functions ----

/// Run a git command in `dir` with a pinned identity, asserting success.
fn g(dir: &Path, args: &[&str]) {
    let out = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "Ada")
                .env("GIT_AUTHOR_EMAIL", "ada@x.com")
                .env("GIT_COMMITTER_NAME", "Ada")
                .env("GIT_COMMITTER_EMAIL", "ada@x.com")
                .env("GIT_AUTHOR_DATE", "1715000000 +0000")
                .env("GIT_COMMITTER_DATE", "1715000000 +0000");
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

/// `git init -b master` + identity config; no commits yet.
fn init_bare_repo(dir: &Path) {
    g(dir, &["init", "-b", "master"]);
    g(dir, &["config", "user.email", "ada@x.com"]);
    g(dir, &["config", "user.name", "Ada"]);
}

/// Write `path`=`content`, stage all, commit with `subject`.
fn commit_file(dir: &Path, path: &str, content: &str, subject: &str) {
    let full = dir.join(path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(full, content).unwrap();
    g(dir, &["add", "."]);
    g(dir, &["commit", "-m", subject]);
}

// ---- get_commits_between_in / paths variants ----

#[test]
fn get_commits_between_in_returns_only_post_base_commits() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_bare_repo(dir);
    commit_file(dir, "a", "0", "initial");
    let base = get_head_commit_in(dir).unwrap();
    commit_file(dir, "a", "1", "feat: one");
    commit_file(dir, "a", "2", "fix: two");

    let commits = get_commits_between_in(dir, &base, "HEAD", None).unwrap();
    assert_eq!(commits.len(), 2, "two commits sit above base");
    // git log default is newest-first.
    assert_eq!(commits[0].message, "fix: two");
    assert_eq!(commits[1].message, "feat: one");
    assert_eq!(commits[1].author_name, "Ada");
    assert_eq!(commits[1].author_email, "ada@x.com");
}

#[test]
fn get_commits_between_in_path_filter_excludes_untouched_files() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_bare_repo(dir);
    commit_file(dir, "base", "0", "initial");
    let base = get_head_commit_in(dir).unwrap();
    commit_file(dir, "src/lib.rs", "1", "feat: touch lib");
    commit_file(dir, "docs/readme", "2", "docs: touch docs only");

    // Filter to src/ — only the lib commit should be reported.
    let commits = get_commits_between_in(dir, &base, "HEAD", Some("src")).unwrap();
    assert_eq!(commits.len(), 1, "only the src-touching commit survives");
    assert_eq!(commits[0].message, "feat: touch lib");
}

#[test]
fn get_commits_between_paths_in_unions_multiple_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_bare_repo(dir);
    commit_file(dir, "base", "0", "initial");
    let base = get_head_commit_in(dir).unwrap();
    commit_file(dir, "a/x", "1", "feat: a");
    commit_file(dir, "b/y", "2", "feat: b");
    commit_file(dir, "c/z", "3", "feat: c");

    // Two paths -> union of commits touching either a/ or b/.
    let commits =
        get_commits_between_paths_in(dir, &base, "HEAD", &["a".into(), "b".into()]).unwrap();
    let subjects: Vec<&str> = commits.iter().map(|c| c.message.as_str()).collect();
    assert_eq!(
        commits.len(),
        2,
        "a and b touched, c excluded: {subjects:?}"
    );
    assert!(subjects.contains(&"feat: a"));
    assert!(subjects.contains(&"feat: b"));
    assert!(!subjects.contains(&"feat: c"));
}

// ---- get_all_commits_* ----

#[test]
fn get_all_commits_in_returns_every_commit_on_head() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_bare_repo(dir);
    commit_file(dir, "a", "0", "first");
    commit_file(dir, "a", "1", "second");
    commit_file(dir, "a", "2", "third");

    let commits = get_all_commits_in(dir, None).unwrap();
    assert_eq!(commits.len(), 3);
    assert_eq!(commits[0].message, "third", "newest-first");
    assert_eq!(commits[2].message, "first");
}

#[test]
fn get_all_commits_paths_in_filters_to_path() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_bare_repo(dir);
    commit_file(dir, "keep/x", "0", "feat: keep");
    commit_file(dir, "drop/y", "1", "feat: drop");

    let commits = get_all_commits_paths_in(dir, &["keep".into()]).unwrap();
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0].message, "feat: keep");
}

#[test]
fn get_all_commits_paths_with_files_in_pairs_files() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_bare_repo(dir);
    commit_file(dir, "crates/core/lib.rs", "0", "feat: core");

    let pairs = get_all_commits_paths_with_files_in(dir, &[]).unwrap();
    assert_eq!(pairs.len(), 1);
    assert_eq!(pairs[0].commit.message, "feat: core");
    assert_eq!(pairs[0].files, vec!["crates/core/lib.rs".to_string()]);
}

// ---- get_commits_reachable_paths_in: bound at an explicit ref ----

#[test]
fn get_commits_reachable_paths_in_stops_at_the_given_rev() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_bare_repo(dir);
    commit_file(dir, "a", "0", "first");
    commit_file(dir, "a", "1", "second");
    let mid = get_head_commit_in(dir).unwrap();
    commit_file(dir, "a", "2", "third-after-mid");

    // Reachable from `mid` excludes the commit made after it.
    let commits = get_commits_reachable_paths_in(dir, &mid, &[]).unwrap();
    let subjects: Vec<&str> = commits.iter().map(|c| c.message.as_str()).collect();
    assert_eq!(commits.len(), 2, "only ancestors of mid: {subjects:?}");
    assert!(subjects.contains(&"first"));
    assert!(subjects.contains(&"second"));
    assert!(!subjects.contains(&"third-after-mid"));
}

#[test]
fn get_commits_reachable_paths_with_files_in_pairs_touched_files() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_bare_repo(dir);
    commit_file(dir, "src/main.rs", "0", "feat: main");
    let head = get_head_commit_in(dir).unwrap();

    let pairs = get_commits_reachable_paths_with_files_in(dir, &head, &[]).unwrap();
    assert_eq!(pairs.len(), 1);
    assert_eq!(pairs[0].commit.message, "feat: main");
    assert_eq!(pairs[0].files, vec!["src/main.rs".to_string()]);
}

// ---- subject-only message helpers ----

#[test]
fn get_last_commit_messages_in_returns_n_subjects_newest_first() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_bare_repo(dir);
    commit_file(dir, "a", "0", "one");
    commit_file(dir, "a", "1", "two");
    commit_file(dir, "a", "2", "three");

    let msgs = get_last_commit_messages_in(dir, 2).unwrap();
    assert_eq!(msgs, vec!["three".to_string(), "two".to_string()]);
}

#[test]
fn get_commit_messages_between_in_lists_post_base_subjects() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_bare_repo(dir);
    commit_file(dir, "a", "0", "initial");
    let base = get_head_commit_in(dir).unwrap();
    commit_file(dir, "a", "1", "feat: x");
    commit_file(dir, "a", "2", "fix: y");

    let msgs = get_commit_messages_between_in(dir, &base, "HEAD").unwrap();
    assert_eq!(msgs, vec!["fix: y".to_string(), "feat: x".to_string()]);
}

#[test]
fn get_last_commit_messages_path_in_filters_to_path() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_bare_repo(dir);
    commit_file(dir, "keep/a", "0", "feat: keep");
    commit_file(dir, "other/b", "1", "feat: other");

    let msgs = get_last_commit_messages_path_in(dir, 10, "keep").unwrap();
    assert_eq!(msgs, vec!["feat: keep".to_string()]);
}

#[test]
fn get_commit_messages_between_path_in_filters_range_and_path() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_bare_repo(dir);
    commit_file(dir, "base", "0", "initial");
    let base = get_head_commit_in(dir).unwrap();
    commit_file(dir, "src/x", "1", "feat: src");
    commit_file(dir, "doc/y", "2", "docs: doc");

    let msgs = get_commit_messages_between_path_in(dir, &base, "HEAD", "src").unwrap();
    assert_eq!(msgs, vec!["feat: src".to_string()]);
}

// ---- diff / change-detection helpers ----

#[test]
fn has_changes_since_in_detects_path_touched_after_tag() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_bare_repo(dir);
    commit_file(dir, "watched", "0", "initial");
    g(dir, &["tag", "v1.0.0"]);
    // No change yet -> false.
    assert!(!has_changes_since_in(dir, "v1.0.0", "watched").unwrap());
    commit_file(dir, "watched", "1", "feat: change watched");
    // Now changed -> true.
    assert!(has_changes_since_in(dir, "v1.0.0", "watched").unwrap());
    // A different, untouched path -> false.
    assert!(!has_changes_since_in(dir, "v1.0.0", "unrelated").unwrap());
}

#[test]
fn paths_changed_since_tag_in_true_when_any_path_changed() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_bare_repo(dir);
    commit_file(dir, "a", "0", "initial");
    g(dir, &["tag", "v1.0.0"]);
    commit_file(dir, "b", "1", "feat: add b");

    // b changed; checking [a, b] -> true (b matched).
    assert!(paths_changed_since_tag_in(dir, "v1.0.0", &["a", "b"]).unwrap());
    // Only a (unchanged) -> false.
    assert!(!paths_changed_since_tag_in(dir, "v1.0.0", &["a"]).unwrap());
}

#[test]
fn paths_changed_since_tag_in_returns_false_when_git_fails() {
    // Non-existent tag makes `git diff` fail; the helper maps that to
    // Ok(false) rather than bubbling an error.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_bare_repo(dir);
    commit_file(dir, "a", "0", "initial");
    assert!(!paths_changed_since_tag_in(dir, "nope-no-such-tag", &["a"]).unwrap());
}

// ---- rev resolution helpers ----

#[test]
fn head_commit_hash_in_matches_rev_parse_head() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_bare_repo(dir);
    commit_file(dir, "a", "0", "initial");
    let expected = get_head_commit_in(dir).unwrap();
    assert_eq!(head_commit_hash_in(dir).unwrap(), expected);
}

#[test]
fn head_commit_hash_in_errors_on_non_repo() {
    let tmp = tempfile::tempdir().unwrap();
    // No git init -> rev-parse HEAD fails.
    assert!(head_commit_hash_in(tmp.path()).is_err());
}

#[test]
fn rev_parse_in_resolves_branch_to_full_sha() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_bare_repo(dir);
    commit_file(dir, "a", "0", "initial");
    let head = get_head_commit_in(dir).unwrap();
    assert_eq!(rev_parse_in(dir, "master").unwrap(), head);
}

#[test]
fn rev_verify_commit_in_accepts_commit_rejects_unknown() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_bare_repo(dir);
    commit_file(dir, "a", "0", "initial");
    let head = get_head_commit_in(dir).unwrap();
    assert_eq!(rev_verify_commit_in(dir, "HEAD").unwrap(), head);
    // A made-up ref must not verify.
    assert!(rev_verify_commit_in(dir, "deadbeefdeadbeef").is_err());
}

#[test]
fn commits_between_in_lists_shas_above_base_and_empty_at_head() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_bare_repo(dir);
    commit_file(dir, "a", "0", "initial");
    let base = get_head_commit_in(dir).unwrap();
    commit_file(dir, "a", "1", "second");
    let head = get_head_commit_in(dir).unwrap();

    let shas = commits_between_in(dir, &base).unwrap();
    assert_eq!(
        shas,
        vec![head.clone()],
        "exactly the one commit above base"
    );
    // sha IS HEAD -> empty range.
    assert!(commits_between_in(dir, &head).unwrap().is_empty());
}

#[test]
fn commit_subject_in_returns_single_commit_subject() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_bare_repo(dir);
    commit_file(dir, "a", "0", "feat: only-subject\n\nignored body");
    let head = get_head_commit_in(dir).unwrap();
    assert_eq!(commit_subject_in(dir, &head).unwrap(), "feat: only-subject");
}

#[test]
fn head_commit_timestamp_in_returns_pinned_committer_epoch() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_bare_repo(dir);
    // Pinned GIT_COMMITTER_DATE in `g` is 1715000000 +0000.
    commit_file(dir, "a", "0", "initial");
    assert_eq!(head_commit_timestamp_in(dir).unwrap(), 1_715_000_000);
}

// ---- log_subjects_for_range ----

#[test]
fn log_subjects_for_range_returns_full_bodies_for_path() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_bare_repo(dir);
    commit_file(dir, "watched", "0", "feat: A\n\nbody of A");
    commit_file(dir, "watched", "1", "fix: B");

    let bodies = log_subjects_for_range(dir, "HEAD", "watched").unwrap();
    assert_eq!(bodies.len(), 2);
    // %B is subject+body; newest-first.
    assert!(bodies[0].starts_with("fix: B"));
    assert!(bodies[1].contains("feat: A") && bodies[1].contains("body of A"));
}

#[test]
fn log_subjects_for_range_returns_empty_when_range_invalid() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_bare_repo(dir);
    commit_file(dir, "a", "0", "initial");
    // A range referencing a non-existent ref makes git fail; the helper
    // maps that to an empty Vec, not an error.
    let bodies = log_subjects_for_range(dir, "no-such-ref..HEAD", "a").unwrap();
    assert!(bodies.is_empty());
}

/// A pathspec fatal (empty pathspec) is a REAL failure, not an empty
/// history — collapsing it into `Ok(vec![])` made `bump` preview Skip
/// for root-level crates.
#[test]
fn log_subjects_for_range_propagates_pathspec_fatal() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_bare_repo(dir);
    commit_file(dir, "a", "0", "initial");
    let err = log_subjects_for_range(dir, "HEAD", "")
        .expect_err("empty pathspec must be an error, not empty success")
        .to_string();
    assert!(err.contains("git log HEAD failed"), "{err}");
}

// ---- add_path_in + commit_in ----

#[test]
fn add_path_in_then_commit_in_creates_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_bare_repo(dir);
    commit_file(dir, "seed", "0", "initial");
    std::fs::write(dir.join("new.txt"), "hello").unwrap();

    add_path_in(dir, std::path::Path::new("new.txt")).unwrap();
    commit_in(dir, "feat: add new.txt", false).unwrap();

    let subject = String::from_utf8(
        anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(["log", "-1", "--pretty=%s"]).current_dir(dir);
                cmd
            },
            "git",
        )
        .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    assert_eq!(subject, "feat: add new.txt");
}

#[test]
fn add_path_in_errors_on_missing_file() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_bare_repo(dir);
    let err = add_path_in(dir, std::path::Path::new("does-not-exist")).unwrap_err();
    assert!(
        err.to_string().contains("git add"),
        "error must name the failing git add: {err}"
    );
}

// ---- reset_hard_in ----

#[test]
fn reset_hard_in_moves_head_and_restores_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_bare_repo(dir);
    commit_file(dir, "a", "first", "first");
    let target = get_head_commit_in(dir).unwrap();
    commit_file(dir, "a", "second", "second");
    assert_ne!(get_head_commit_in(dir).unwrap(), target);

    reset_hard_in(dir, &target).unwrap();
    assert_eq!(get_head_commit_in(dir).unwrap(), target, "HEAD moved back");
    assert_eq!(
        std::fs::read_to_string(dir.join("a")).unwrap(),
        "first",
        "working tree restored to target content"
    );
}

// ---- push_branch_in error path (no remote) ----

#[test]
fn push_branch_in_bails_without_origin_remote() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_bare_repo(dir);
    commit_file(dir, "a", "0", "initial");
    let err = push_branch_in(dir, "master").unwrap_err();
    assert!(
        err.to_string().contains("no 'origin' remote"),
        "missing-remote bail must be explicit: {err}"
    );
}

// ---- resolve_rollback_identity / read_git_identity ----

#[test]
#[serial_test::serial(git_env)]
fn resolve_rollback_identity_inherits_when_repo_has_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_bare_repo(dir); // sets user.name + user.email

    // Clear any inherited GIT_AUTHOR_*/COMMITTER_* env so the resolver
    // falls through to reading the repo config (which IS configured).
    struct EnvGuard(Vec<(&'static str, Option<String>)>);
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.0 {
                match v {
                    // env-ok: restore/clear inside #[serial(git_env)] test; no concurrent reader
                    Some(val) => unsafe { std::env::set_var(k, val) },
                    // env-ok: restore/clear inside #[serial(git_env)] test; no concurrent reader
                    None => unsafe { std::env::remove_var(k) },
                }
            }
        }
    }
    let keys = [
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
    ];
    let _g = EnvGuard(keys.iter().map(|k| (*k, std::env::var(k).ok())).collect());
    for k in keys {
        // env-ok: restore/clear inside #[serial(git_env)] test; no concurrent reader
        unsafe { std::env::remove_var(k) };
    }

    // Repo has user.name + user.email -> inherit (empty identity).
    let id = resolve_rollback_identity(dir);
    assert!(
        id.name.is_none() && id.email.is_none(),
        "configured repo identity must be inherited, not overridden: {id:?}"
    );
}

#[test]
#[serial_test::serial(git_env)]
fn resolve_rollback_identity_synthesizes_when_no_identity_anywhere() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // init WITHOUT configuring user.name / user.email.
    g(dir, &["init", "-b", "master"]);

    struct EnvGuard(Vec<(&'static str, Option<String>)>);
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.0 {
                match v {
                    // env-ok: restore/clear inside #[serial(git_env)] test; no concurrent reader
                    Some(val) => unsafe { std::env::set_var(k, val) },
                    // env-ok: restore/clear inside #[serial(git_env)] test; no concurrent reader
                    None => unsafe { std::env::remove_var(k) },
                }
            }
        }
    }
    let keys = [
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
    ];
    let _g = EnvGuard(keys.iter().map(|k| (*k, std::env::var(k).ok())).collect());
    for k in keys {
        // env-ok: restore/clear inside #[serial(git_env)] test; no concurrent reader
        unsafe { std::env::remove_var(k) };
    }

    // Best-effort: global git config may still supply an identity on the
    // host. Only assert the synthetic path when the repo truly has none.
    let (n, e) = read_git_identity(dir);
    if n.is_none() || e.is_none() {
        let id = resolve_rollback_identity(dir);
        assert_eq!(id.name.as_deref(), Some("anodize-rollback"));
        assert!(
            id.email
                .as_deref()
                .unwrap_or("")
                .starts_with("anodize-rollback@"),
            "synthetic identity required when no config present: {id:?}"
        );
    }
}

#[test]
fn read_git_identity_reads_configured_values() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    g(dir, &["init", "-b", "master"]);
    g(dir, &["config", "user.name", "Configured Name"]);
    g(dir, &["config", "user.email", "configured@x.com"]);

    let (name, email) = read_git_identity(dir);
    assert_eq!(name.as_deref(), Some("Configured Name"));
    assert_eq!(email.as_deref(), Some("configured@x.com"));
}
