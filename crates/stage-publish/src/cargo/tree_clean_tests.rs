//! Clean-tree publish-guard tests: `ensure_publish_tree_clean` must exempt
//! exactly the paths anodizer itself recorded as its own in-run writes
//! (`Context::record_tree_mutation`) and stay loud on every other dirty path.
//!
//! The cross-iteration case this pins: per-crate `--publish-only` runs the
//! publish pipeline once per crate against one persistent context, so crate
//! A's binstall write to its own `Cargo.toml` is still on disk when crate B's
//! guard runs. That residue must not abort B's publish (cfgd v0.6.0 partial
//! release), while operator-authored dirt on any other path still must.

use super::*;
use anodizer_core::test_helpers::TestContextBuilder;
use std::path::Path;

/// `git init` + commit everything under `dir`, yielding a CLEAN working tree.
/// The guard fails CLOSED on a non-repo dir, so fixtures must be real repos.
fn init_clean_repo(dir: &Path) {
    let run = |args: &[&str]| {
        let ok = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = std::process::Command::new("git");
                cmd.current_dir(dir)
                    .args(args)
                    .env("GIT_AUTHOR_NAME", "t")
                    .env("GIT_AUTHOR_EMAIL", "t@example.com")
                    .env("GIT_COMMITTER_NAME", "t")
                    .env("GIT_COMMITTER_EMAIL", "t@example.com");
                cmd
            },
            "git",
        )
        .status
        .success();
        assert!(ok, "git {args:?} failed");
    };
    run(&["init", "-q"]);
    run(&["config", "user.email", "t@example.com"]);
    run(&["config", "user.name", "t"]);
    run(&["add", "-A"]);
    run(&["commit", "-qm", "fixture"]);
}

fn write_crate(root: &Path, dir: &str) {
    let d = root.join(dir);
    std::fs::create_dir_all(&d).expect("mkdir crate");
    std::fs::write(
        d.join("Cargo.toml"),
        format!("[package]\nname = \"{dir}\"\nversion = \"0.1.0\"\n"),
    )
    .expect("write Cargo.toml");
}

fn test_log() -> StageLogger {
    StageLogger::new("publish-test", anodizer_core::log::Verbosity::Normal)
}

#[test]
fn clean_tree_passes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_crate(tmp.path(), "a");
    init_clean_repo(tmp.path());
    let ctx = TestContextBuilder::new()
        .project_root(tmp.path().to_path_buf())
        .build();
    assert!(ensure_publish_tree_clean(&ctx, &test_log()).is_ok());
}

#[test]
fn recorded_self_write_is_exempted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_crate(tmp.path(), "a");
    write_crate(tmp.path(), "b");
    init_clean_repo(tmp.path());
    // Simulate crate a's binstall write leaking into crate b's iteration.
    std::fs::write(
        tmp.path().join("a/Cargo.toml"),
        "[package]\nname = \"a\"\nversion = \"0.1.0\"\n\n[package.metadata.binstall]\n",
    )
    .expect("dirty a");
    let mut ctx = TestContextBuilder::new()
        .project_root(tmp.path().to_path_buf())
        .build();
    ctx.record_tree_mutation("a/Cargo.toml");
    assert!(
        ensure_publish_tree_clean(&ctx, &test_log()).is_ok(),
        "anodizer's own recorded write must not trip the guard"
    );
}

#[test]
fn unrecorded_dirt_still_bails_and_names_only_residual_paths() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_crate(tmp.path(), "a");
    write_crate(tmp.path(), "b");
    init_clean_repo(tmp.path());
    std::fs::write(
        tmp.path().join("a/Cargo.toml"),
        "[package]\nname = \"a\"\nversion = \"0.1.0\"\n\n[package.metadata.binstall]\n",
    )
    .expect("dirty a");
    std::fs::write(tmp.path().join("b/src.rs"), "// operator edit\n").expect("dirty b");
    let mut ctx = TestContextBuilder::new()
        .project_root(tmp.path().to_path_buf())
        .build();
    ctx.record_tree_mutation("a/Cargo.toml");
    let err = ensure_publish_tree_clean(&ctx, &test_log())
        .expect_err("operator dirt must still bail")
        .to_string();
    assert!(err.contains("b/src.rs"), "residual path named: {err}");
    assert!(
        !err.contains("a/Cargo.toml"),
        "exempted path must not be listed as dirt: {err}"
    );
}

#[test]
fn dirt_with_nothing_recorded_bails() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_crate(tmp.path(), "a");
    init_clean_repo(tmp.path());
    std::fs::write(tmp.path().join("a/extra.txt"), "x").expect("dirty");
    let ctx = TestContextBuilder::new()
        .project_root(tmp.path().to_path_buf())
        .build();
    let err = ensure_publish_tree_clean(&ctx, &test_log())
        .expect_err("dirty tree must bail")
        .to_string();
    assert!(err.contains("working tree is DIRTY"), "{err}");
}
