use super::*;
use std::path::Path;
use std::process::Command;

mod delete_tag_tests {
    use super::*;

    /// Build a `<bare-repo>` + working clone pair so we can drive
    /// `delete_remote_tag_in` against a real "origin" without hitting the
    /// network. Returns `(bare, work)`; the working clone has `origin`
    /// pointing at the bare repo.
    fn init_clone_pair() -> (tempfile::TempDir, tempfile::TempDir) {
        let bare = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let run = |dir: &Path, args: &[&str]| {
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
        run(bare.path(), &["init", "--bare", "-b", "master"]);
        run(work.path(), &["init", "-b", "master"]);
        run(work.path(), &["config", "user.email", "t@t.com"]);
        run(work.path(), &["config", "user.name", "t"]);
        run(
            work.path(),
            &[
                "remote",
                "add",
                "origin",
                bare.path().to_str().expect("tempdir path utf-8"),
            ],
        );
        std::fs::write(work.path().join("a"), "0").unwrap();
        run(work.path(), &["add", "."]);
        run(work.path(), &["commit", "-m", "initial"]);
        run(work.path(), &["push", "origin", "master"]);
        (bare, work)
    }

    /// B-R3: deleting a remote tag that doesn't exist must succeed
    /// (idempotent). The git output for that case contains
    /// `"remote ref does not exist"`; the helper must absorb it.
    #[test]
    fn delete_remote_tag_in_is_idempotent_when_remote_tag_missing() {
        let (_bare, work) = init_clone_pair();
        // Tag was never created on the remote — first delete must succeed.
        delete_remote_tag_in(work.path(), "v0.0.0-never-existed")
            .expect("missing remote tag must be treated as already-deleted");
    }

    /// B-R3 follow-on: a real delete still works, and a second delete
    /// of the same tag remains idempotent.
    #[test]
    fn delete_remote_tag_in_succeeds_then_is_idempotent_on_second_call() {
        let (_bare, work) = init_clone_pair();
        let run = |args: &[&str]| {
            let out = anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(args)
                        .current_dir(work.path())
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
        run(&["tag", "v1.2.3"]);
        run(&["push", "origin", "v1.2.3"]);
        delete_remote_tag_in(work.path(), "v1.2.3").expect("first remote delete must succeed");
        delete_remote_tag_in(work.path(), "v1.2.3")
            .expect("second remote delete must be a no-op (idempotent)");
    }
}

mod create_tag_local_only_tests {
    use super::*;

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let out = anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(args)
                        .current_dir(dir.path())
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
        run(&["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.path().join("a"), "0").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "initial"]);
        dir
    }

    fn commit_change(dir: &Path) {
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
        std::fs::write(dir.join("a"), "1").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "next"]);
    }

    #[test]
    fn recreating_tag_at_same_head_is_idempotent() {
        let repo = init_repo();
        let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Quiet);
        create_tag_local_only(repo.path(), "v1.0.0", "Release v1.0.0", false, false, &log)
            .expect("first create must succeed");
        // Same tag, same HEAD — the leftover-from-failed-push case.
        create_tag_local_only(repo.path(), "v1.0.0", "Release v1.0.0", false, false, &log)
            .expect("re-creating a tag that already points at HEAD must be idempotent");
    }

    #[test]
    fn recreating_tag_at_different_commit_fails_actionably() {
        let repo = init_repo();
        let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Quiet);
        create_tag_local_only(repo.path(), "v1.0.0", "Release v1.0.0", false, false, &log)
            .expect("first create must succeed");
        commit_change(repo.path());
        let err =
            create_tag_local_only(repo.path(), "v1.0.0", "Release v1.0.0", false, false, &log)
                .expect_err("stale tag at a different commit must fail");
        let msg = err.to_string();
        assert!(msg.contains("v1.0.0"), "error must name the tag: {msg}");
        assert!(
            msg.contains("different commit"),
            "error must name the conflict: {msg}"
        );
        assert!(
            msg.contains("anodizer tag rollback") && msg.contains("git tag -d v1.0.0"),
            "error must suggest a remedy: {msg}"
        );
    }

    #[test]
    fn tag_create_flag_selects_signed_or_annotated() {
        assert_eq!(tag_create_flag(true), "-s");
        assert_eq!(tag_create_flag(false), "-a");
    }

    /// Run a git command against `dir`, asserting success.
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
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The raw annotated-tag object body for `tag` in `dir`.
    fn cat_file_tag(dir: &Path, tag: &str) -> String {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(["cat-file", "tag", tag]).current_dir(dir);
                cmd
            },
            "git",
        );
        assert!(out.status.success(), "git cat-file tag {tag} failed");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Configure ephemeral SSH tag-signing on `dir` and return the key dir so it
    /// outlives the repo. No gpg-agent involved: a throwaway ed25519 key drives
    /// `git tag -s` end to end.
    fn configure_ssh_signing(dir: &Path) -> tempfile::TempDir {
        let keydir = tempfile::tempdir().unwrap();
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
        git_in(dir, &["config", "gpg.format", "ssh"]);
        git_in(dir, &["config", "user.signingkey", &pub_path]);
        keydir
    }

    #[test]
    fn signed_tag_carries_ssh_signature_block() {
        let repo = init_repo();
        let _keydir = configure_ssh_signing(repo.path());
        let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Quiet);
        create_tag_local_only(repo.path(), "v2.0.0", "Release v2.0.0", false, true, &log)
            .expect("signed tag creation must succeed");
        let body = cat_file_tag(repo.path(), "v2.0.0");
        assert!(
            body.contains("-----BEGIN SSH SIGNATURE-----"),
            "signed tag must embed an SSH signature block, got:\n{body}"
        );
    }

    #[test]
    fn unsigned_tag_has_no_signature_block() {
        let repo = init_repo();
        // Signing is configured but NOT requested — the tag must stay unsigned.
        let _keydir = configure_ssh_signing(repo.path());
        let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Quiet);
        create_tag_local_only(repo.path(), "v2.0.0", "Release v2.0.0", false, false, &log)
            .expect("unsigned tag creation must succeed");
        let body = cat_file_tag(repo.path(), "v2.0.0");
        assert!(
            !body.contains("-----BEGIN SSH SIGNATURE-----"),
            "unsigned tag must NOT embed a signature block, got:\n{body}"
        );
    }
}
