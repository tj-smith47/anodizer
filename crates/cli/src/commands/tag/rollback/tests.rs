use super::deletion::*;
use super::guard::*;
use super::registry_probe::*;
use super::release_probe::*;
use super::run::*;
use super::tags::*;
use super::types::*;
use anodizer_core::git;
use anodizer_core::log::{StageLogger, Verbosity};
use anyhow::Result;

#[test]
fn classifies_lockstep_release_tags() {
    assert_eq!(classify_tag("v1.2.3"), Some(TagKind::Lockstep));
    assert_eq!(classify_tag("v0.0.1"), Some(TagKind::Lockstep));
    assert_eq!(classify_tag("v10.20.30"), Some(TagKind::Lockstep));
}

#[test]
fn classifies_lockstep_prerelease_and_build_tags() {
    assert_eq!(classify_tag("v1.2.3-rc.1"), Some(TagKind::Lockstep));
    assert_eq!(classify_tag("v1.2.3-beta.10"), Some(TagKind::Lockstep));
    assert_eq!(classify_tag("v1.2.3+build.42"), Some(TagKind::Lockstep));
    assert_eq!(
        classify_tag("v1.2.3-rc.1+build.42"),
        Some(TagKind::Lockstep)
    );
}

#[test]
fn classifies_per_crate_tags() {
    assert_eq!(classify_tag("mycrate-v1.2.3"), Some(TagKind::PerCrate));
    assert_eq!(
        classify_tag("cfgd-operator-v0.4.0"),
        Some(TagKind::PerCrate)
    );
    assert_eq!(
        classify_tag("my_crate-v1.2.3-rc.1"),
        Some(TagKind::PerCrate)
    );
}

#[test]
fn rejects_non_anodize_shaped_tags() {
    assert_eq!(classify_tag("foo-bar"), None);
    assert_eq!(classify_tag("v1.2"), None);
    assert_eq!(classify_tag("v1"), None);
    assert_eq!(classify_tag("release-1.2.3"), None);
    assert_eq!(classify_tag("tag-without-version"), None);
    assert_eq!(classify_tag(""), None);
    assert_eq!(classify_tag("v1.2.3.4"), None);
}

#[test]
fn scope_lockstep_excludes_per_crate() {
    assert!(scope_includes(Scope::Lockstep, TagKind::Lockstep));
    assert!(!scope_includes(Scope::Lockstep, TagKind::PerCrate));
}

#[test]
fn scope_per_crate_excludes_lockstep() {
    assert!(scope_includes(Scope::PerCrate, TagKind::PerCrate));
    assert!(!scope_includes(Scope::PerCrate, TagKind::Lockstep));
}

#[test]
fn scope_all_accepts_both() {
    assert!(scope_includes(Scope::All, TagKind::Lockstep));
    assert!(scope_includes(Scope::All, TagKind::PerCrate));
}

#[test]
fn scope_parser_round_trip() {
    assert_eq!("all".parse::<Scope>().unwrap(), Scope::All);
    assert_eq!("lockstep".parse::<Scope>().unwrap(), Scope::Lockstep);
    assert_eq!("per-crate".parse::<Scope>().unwrap(), Scope::PerCrate);
    assert_eq!("percrate".parse::<Scope>().unwrap(), Scope::PerCrate);
    assert!("nope".parse::<Scope>().is_err());
}

#[test]
fn mode_parser_round_trip() {
    assert_eq!("revert".parse::<Mode>().unwrap(), Mode::Revert);
    assert_eq!("reset".parse::<Mode>().unwrap(), Mode::Reset);
    assert!("rewind".parse::<Mode>().is_err());
}

#[test]
fn revert_message_uses_lockstep_as_subject() {
    let msg = build_revert_message(
        "abcdef1234567890",
        &[
            "mycrate-v1.0.0".into(),
            "v1.0.0".into(),
            "other-v1.0.0".into(),
        ],
        false,
    );
    assert!(msg.starts_with("chore(release): rollback v1.0.0 [skip ci]"));
    assert!(msg.contains("Reverts abcdef1."));
    assert!(msg.contains("Tags deleted: mycrate-v1.0.0, v1.0.0, other-v1.0.0"));
}

#[test]
fn revert_message_falls_back_to_first_when_no_lockstep() {
    let msg = build_revert_message(
        "abcdef1234567890",
        &["mycrate-v1.0.0".into(), "other-v1.0.0".into()],
        false,
    );
    assert!(msg.starts_with("chore(release): rollback mycrate-v1.0.0 [skip ci]"));
}

#[test]
fn revert_message_dry_run_marks_pending_tag_deletion() {
    let msg = build_revert_message("abcdef1234567890", &["v1.0.0".into()], true);
    assert!(
        msg.contains("Tags that WOULD be deleted: v1.0.0"),
        "dry-run preview must distinguish pending deletion: {msg}"
    );
    assert!(
        !msg.contains("\nTags deleted:"),
        "dry-run preview must NOT emit the real-run label: {msg}"
    );
}

#[test]
fn per_crate_regex_rejects_leading_digit() {
    // Cargo crate names must start with a letter; the rollback
    // regex must not accept `9-foo-v1.2.3` as a per-crate tag.
    assert_eq!(classify_tag("9-foo-v1.2.3"), None);
    assert_eq!(classify_tag("0bad-v1.0.0"), None);
    // Underscore-leading is still accepted (matches cargo identifier rules).
    assert_eq!(classify_tag("_foo-v1.2.3"), Some(TagKind::PerCrate));
}

#[test]
fn safety_check_prefix_admits_anodize_revert_only() {
    // anodize's own prior revert subject — admissible.
    let anodize_subject = "Revert \"chore(release): rollback v1.2.3 [skip ci]\"";
    assert!(
        anodize_subject.starts_with(ANODIZE_REVERT_SUBJECT_PREFIX.as_str()),
        "anodize-generated revert must be recognised"
    );
    // GitHub's "Revert this PR" button subject — must NOT be admitted.
    let github_subject = "Revert \"feat: add new flag\"";
    assert!(
        !github_subject.starts_with(ANODIZE_REVERT_SUBJECT_PREFIX.as_str()),
        "unrelated revert PR subjects must NOT be admitted as anodize-shaped"
    );
}

// -----------------------------------------------------------------
// Fixture-repo integration tests — exercise the safety-check path
// and dry-run no-mutation guarantee against a real tempdir git repo.
// -----------------------------------------------------------------

use std::path::Path;
use std::process::Command;

fn run_git(dir: &Path, args: &[&str]) {
    let out = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@test.com")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@test.com");
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

/// Build a repo with: initial commit -> bump commit (tagged vX.Y.Z),
/// optionally followed by extra commits to exercise the safety check.
fn init_bump_repo(dir: &Path, extra_commits: usize) -> String {
    run_git(dir, &["init", "-b", "master"]);
    run_git(dir, &["config", "user.email", "test@test.com"]);
    run_git(dir, &["config", "user.name", "test"]);
    std::fs::write(dir.join("README"), "init").unwrap();
    run_git(dir, &["add", "."]);
    run_git(dir, &["commit", "-m", "initial"]);

    std::fs::write(dir.join("Cargo.toml"), "[package]\nversion = \"1.0.0\"\n").unwrap();
    run_git(dir, &["add", "."]);
    run_git(dir, &["commit", "-m", "chore(release): v1.0.0"]);
    run_git(dir, &["tag", "v1.0.0"]);

    let bump_sha = String::from_utf8(
        anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(["rev-parse", "HEAD"]).current_dir(dir);
                cmd
            },
            "git",
        )
        .stdout,
    )
    .unwrap()
    .trim()
    .to_string();

    for i in 0..extra_commits {
        let fname = format!("extra-{i}.txt");
        std::fs::write(dir.join(&fname), "x").unwrap();
        run_git(dir, &["add", "."]);
        run_git(dir, &["commit", "-m", &format!("feat: extra work {i}")]);
    }

    bump_sha
}

/// Give a fixture repo a resolvable non-github.com origin: the
/// published-state guard's probe is inapplicable there (warn +
/// proceed), letting tests exercise their actual subject without
/// tripping the unresolvable-origin fail-closed refusal. The URL is
/// never contacted — these tests run with `dry_run` / `no_push`.
fn add_non_github_origin(dir: &Path) {
    run_git(
        dir,
        &["remote", "add", "origin", "https://gitlab.example/o/r.git"],
    );
}

/// Write a config with no crates.io-targeting crate, satisfying the
/// guard's fail-closed config requirement without arming the index
/// probe — the run-path tests below exercise git mechanics, not the
/// probe.
fn write_minimal_config(dir: &Path) {
    std::fs::write(dir.join(".anodizer.yaml"), "project_name: fixture\n").unwrap();
}

fn opts_for(dir: &Path, sha: Option<String>) -> RollbackOpts {
    let _ = dir; // cwd is process-global; the with-guard helpers below set it
    RollbackOpts {
        sha,
        dry_run: false,
        no_push: true,
        force: false,
        scope: Scope::All,
        mode: Mode::Revert,
        branch: None,
        verbose: false,
        debug: false,
        quiet: true,
    }
}

/// Process-wide cwd swap. Marked `serial(cwd)` — the workspace-canonical
/// cwd serial group — so these swappers mutually exclude with every other
/// cwd-touching test in this binary (e.g. `helpers::resolve_git_context`).
use serial_test::serial;

#[test]
#[serial(cwd)]
fn safety_check_fires_when_non_bump_commits_sit_on_top() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let bump_sha = init_bump_repo(dir, 2);
    add_non_github_origin(dir);
    write_minimal_config(dir);

    let _cwd = anodizer_core::test_helpers::CwdGuard::new(dir).unwrap();

    let opts = opts_for(dir, Some(bump_sha));
    let err = run(opts).expect_err("safety check should fire");
    let msg = format!("{err}");
    assert!(msg.contains("cannot rollback"), "got: {msg}");
    assert!(
        msg.contains("non-bump commit"),
        "missing safety-check phrasing: {msg}"
    );
}

#[test]
#[serial(cwd)]
fn safety_check_passes_against_clean_head_at_bump_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let _bump_sha = init_bump_repo(dir, 0);
    add_non_github_origin(dir);
    write_minimal_config(dir);

    let _cwd = anodizer_core::test_helpers::CwdGuard::new(dir).unwrap();

    // HEAD == bump_sha; safety check trivially passes (no commits
    // between HEAD and target).
    let mut opts = opts_for(dir, None);
    opts.dry_run = true; // don't mutate the fixture
    run(opts).expect("safety check should pass at HEAD == bump commit");

    // Tag still present (dry-run guarantee).
    let tags = git::get_tags_at_head_in(dir).unwrap();
    assert_eq!(tags, vec!["v1.0.0".to_string()]);
}

#[test]
#[serial(cwd)]
fn dry_run_makes_no_mutations() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let _bump_sha = init_bump_repo(dir, 0);
    let head_before = String::from_utf8(
        anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(["rev-parse", "HEAD"]).current_dir(dir);
                cmd
            },
            "git",
        )
        .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    add_non_github_origin(dir);
    write_minimal_config(dir);

    let _cwd = anodizer_core::test_helpers::CwdGuard::new(dir).unwrap();

    let mut opts = opts_for(dir, None);
    opts.dry_run = true;
    run(opts).expect("dry-run should succeed");

    // Tag still present.
    let tags = git::get_tags_at_head_in(dir).unwrap();
    assert_eq!(tags, vec!["v1.0.0".to_string()]);
    // HEAD unchanged.
    let head_after = String::from_utf8(
        anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(["rev-parse", "HEAD"]).current_dir(dir);
                cmd
            },
            "git",
        )
        .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    assert_eq!(head_before, head_after);
}

#[test]
#[serial(cwd)]
fn no_push_skips_remote_ops_but_does_local_revert() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let bump_sha = init_bump_repo(dir, 0);
    // Non-github origin only; `no_push` keeps push_branch_in from contacting it.
    add_non_github_origin(dir);
    write_minimal_config(dir);

    let _cwd = anodizer_core::test_helpers::CwdGuard::new(dir).unwrap();

    let opts = RollbackOpts {
        sha: None,
        dry_run: false,
        no_push: true,
        force: false,
        scope: Scope::All,
        mode: Mode::Revert,
        branch: None,
        verbose: false,
        debug: false,
        quiet: true,
    };
    run(opts).expect("no-push rollback should succeed locally");

    // Local tag gone.
    let tags = git::get_tags_at_sha_in(dir, &bump_sha).unwrap();
    assert!(
        tags.is_empty(),
        "expected no tags at bump_sha; got {tags:?}"
    );

    // Revert commit landed on top of the bump.
    let subj = git::commit_subject_in(dir, "HEAD").unwrap();
    assert!(
        subj.starts_with("chore(release): rollback v1.0.0"),
        "unexpected HEAD subject: {subj}"
    );
}

#[test]
#[serial(cwd)]
fn skips_tags_not_matching_anodize_shape() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let bump_sha = init_bump_repo(dir, 0);
    add_non_github_origin(dir);
    write_minimal_config(dir);
    // Add a non-anodize tag at the same SHA.
    run_git(dir, &["tag", "internal-release"]);

    let _cwd = anodizer_core::test_helpers::CwdGuard::new(dir).unwrap();

    let opts = RollbackOpts {
        sha: None,
        dry_run: false,
        no_push: true,
        force: false,
        scope: Scope::All,
        mode: Mode::Revert,
        branch: None,
        verbose: false,
        debug: false,
        quiet: true,
    };
    run(opts).expect("rollback should ignore non-anodize tag");

    // Non-anodize tag survived; anodize tag is gone.
    let surviving = git::get_tags_at_sha_in(dir, &bump_sha).unwrap();
    assert_eq!(surviving, vec!["internal-release".to_string()]);
}

// -----------------------------------------------------------------
// --branch flag + detached-HEAD branch resolution.
// -----------------------------------------------------------------

#[test]
fn resolve_push_branch_honors_explicit_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // No git init required — explicit branch short-circuits before
    // hitting git_output_in.
    // Explicit short-circuits before any git query; SHA is irrelevant.
    let b = resolve_push_branch(
        dir,
        "0000000000000000000000000000000000000000",
        Some("release/v9.9.9-prep"),
    )
    .unwrap();
    assert_eq!(b, "release/v9.9.9-prep");
}

#[test]
fn resolve_push_branch_hard_fails_on_detached_head_without_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // Build a repo whose HEAD is detached AND no branch points at
    // it: commit twice on master, then `git checkout --detach` the
    // older sha — master now points past HEAD.
    run_git(dir, &["init", "-b", "master"]);
    run_git(dir, &["config", "user.email", "t@t.com"]);
    run_git(dir, &["config", "user.name", "t"]);
    std::fs::write(dir.join("a"), "1").unwrap();
    run_git(dir, &["add", "."]);
    run_git(dir, &["commit", "-m", "c1"]);
    let older_sha = String::from_utf8(
        anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(["rev-parse", "HEAD"]).current_dir(dir);
                cmd
            },
            "git",
        )
        .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    std::fs::write(dir.join("a"), "2").unwrap();
    run_git(dir, &["add", "."]);
    run_git(dir, &["commit", "-m", "c2"]);
    run_git(dir, &["checkout", "--detach", &older_sha]);

    // An empty env source means the `GITHUB_REF_NAME` fallback can't
    // supply a value, then verify the hard-fail surfaces the remediation.
    let env = anodizer_core::MapEnvSource::new();

    // No remote configured → SHA-derivation returns empty, falls
    // through to get_current_branch_in, which fails on detached
    // HEAD with no env fallback → operator-friendly hard-fail.
    let err = resolve_push_branch_with_env(dir, &older_sha, None, &env).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("cannot determine branch for revert push"),
        "missing hard-fail phrasing: {msg}"
    );
    assert!(
        msg.contains("--branch <name>"),
        "hard-fail must name the remediation flag: {msg}"
    );
}

#[test]
fn resolve_push_branch_hard_fails_when_github_ref_name_looks_like_tag() {
    // Same shape as above (detached HEAD with no pointing branch),
    // but GITHUB_REF_NAME is set to a tag-shaped value. The
    // is_branchlike guard in get_current_branch_in must reject it,
    // and resolve_push_branch must surface the operator-friendly
    // hard-fail (not silently push to a branch named after the tag).
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    run_git(dir, &["init", "-b", "master"]);
    run_git(dir, &["config", "user.email", "t@t.com"]);
    run_git(dir, &["config", "user.name", "t"]);
    std::fs::write(dir.join("a"), "1").unwrap();
    run_git(dir, &["add", "."]);
    run_git(dir, &["commit", "-m", "c1"]);
    let older_sha = String::from_utf8(
        anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(["rev-parse", "HEAD"]).current_dir(dir);
                cmd
            },
            "git",
        )
        .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    std::fs::write(dir.join("a"), "2").unwrap();
    run_git(dir, &["add", "."]);
    run_git(dir, &["commit", "-m", "c2"]);
    run_git(dir, &["checkout", "--detach", &older_sha]);

    let env = anodizer_core::MapEnvSource::new().with("GITHUB_REF_NAME", "v0.4.5");

    let err = resolve_push_branch_with_env(dir, &older_sha, None, &env).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("cannot determine branch for revert push"),
        "tag-shaped GITHUB_REF_NAME must trigger the operator-facing hard-fail: {msg}"
    );
}

#[test]
fn resolve_push_branch_explicit_branch_wins_over_detached_head() {
    // Even when auto-resolution would hard-fail, --branch wins.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    run_git(dir, &["init", "-b", "master"]);
    run_git(dir, &["config", "user.email", "t@t.com"]);
    run_git(dir, &["config", "user.name", "t"]);
    std::fs::write(dir.join("a"), "1").unwrap();
    run_git(dir, &["add", "."]);
    run_git(dir, &["commit", "-m", "c1"]);
    let older_sha = String::from_utf8(
        anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(["rev-parse", "HEAD"]).current_dir(dir);
                cmd
            },
            "git",
        )
        .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    std::fs::write(dir.join("a"), "2").unwrap();
    run_git(dir, &["add", "."]);
    run_git(dir, &["commit", "-m", "c2"]);
    run_git(dir, &["checkout", "--detach", &older_sha]);

    // --branch short-circuits before any env read, so an empty env source
    // proves the explicit flag wins regardless of `GITHUB_REF_NAME`.
    let env = anodizer_core::MapEnvSource::new();
    let b = resolve_push_branch_with_env(dir, &older_sha, Some("master"), &env).unwrap();
    assert_eq!(b, "master");
}

// -----------------------------------------------------------------
// Published-release guard. Drives `check_no_published_releases`
// with a stub `gh` script in a tempdir (no PATH mutation) against a
// fixture repo whose origin is GitHub-shaped (local config only —
// no network is touched; the stub answers the API call).
// -----------------------------------------------------------------

/// Write an executable stub standing in for the `gh` CLI.
#[cfg(unix)]
fn write_gh_stub(dir: &Path, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("gh-stub");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Fixture repo with a GitHub-shaped origin so
/// `resolve_repo_slug_in` resolves owner/repo without a network.
fn init_github_origin_repo(dir: &Path) {
    let _ = init_bump_repo(dir, 0);
    run_git(
        dir,
        &["remote", "add", "origin", "https://github.com/o/r.git"],
    );
}

fn quiet_log() -> StageLogger {
    StageLogger::new("test", Verbosity::Quiet)
}

/// crates.io index probe stub that must never be consulted — used by
/// tests whose fixtures carry no repo config (no tag→crate mapping
/// exists), pinning that the probe layer stays quiet on that path.
fn probe_untouched(_: &str, _: &str) -> Result<bool> {
    panic!("crates.io index probe must not be consulted on this path")
}

/// Moderated-registry probe stub reporting "never submitted", so tests
/// targeting the crates.io layer (or the summary/release layers)
/// exercise their subject in isolation.
fn moderated_probe_clear(_: &str, _: &str) -> Result<Option<String>> {
    Ok(None)
}

/// Winget sibling of [`moderated_probe_clear`].
fn winget_probe_clear(_: &WingetProbeSpec) -> Result<Option<String>> {
    Ok(None)
}

/// npm/pypi immutable-registry probe stub reporting "not published", so
/// tests targeting other layers (crates.io, summary, release) exercise
/// their subject in isolation.
fn immutable_probe_clear(_: &str, _: &str, _: &str) -> Result<bool> {
    Ok(false)
}

/// npm/pypi probe stub that must never be consulted — pins that a path
/// leaves the immutable-registry probe layer quiet (no npm/pypi config, or
/// the tag doesn't version the entry).
fn immutable_probe_untouched(_: &str, _: &str, _: &str) -> Result<bool> {
    panic!("npm/pypi burn probe must not be consulted on this path")
}

/// Wrap a crates.io index probe into the full [`BurnProbes`] seam with
/// clear npm/pypi + moderated-registry probes.
fn probes_with_crates_io(index: &(dyn Fn(&str, &str) -> Result<bool> + Sync)) -> BurnProbes<'_> {
    BurnProbes {
        crates_io: index,
        npm: &immutable_probe_clear,
        pypi: &immutable_probe_clear,
        chocolatey: &moderated_probe_clear,
        winget: &winget_probe_clear,
    }
}

/// Wrap an npm + a pypi immutable-registry probe into the full
/// [`BurnProbes`] seam with a clear (never-burned) crates.io probe and
/// clear moderated-registry probes.
fn probes_with_npm_pypi<'a>(
    npm: &'a (dyn Fn(&str, &str, &str) -> Result<bool> + Sync),
    pypi: &'a (dyn Fn(&str, &str, &str) -> Result<bool> + Sync),
) -> BurnProbes<'a> {
    BurnProbes {
        crates_io: &crates_io_probe_clear,
        npm,
        pypi,
        chocolatey: &moderated_probe_clear,
        winget: &winget_probe_clear,
    }
}

/// crates.io index probe stub reporting "not published" — used by the
/// npm/pypi tests so the crates.io layer clears and their subject (the
/// immutable-registry probe) is exercised in isolation.
fn crates_io_probe_clear(_: &str, _: &str) -> Result<bool> {
    Ok(false)
}

/// One crates.io-targeting crate plus a top-level `pypis:`/`npms:` entry
/// versioned by that crate's tag family, for the npm/pypi burn-probe tests.
fn config_with_pypi(
    crate_name: &str,
    tag_tmpl: &str,
    entry: anodizer_core::config::PypiConfig,
) -> anodizer_core::config::Config {
    let mut config = single_crate_config(crate_name, tag_tmpl);
    config.pypis = Some(vec![entry]);
    config
}

fn config_with_npm(
    crate_name: &str,
    tag_tmpl: &str,
    entry: anodizer_core::config::NpmConfig,
) -> anodizer_core::config::Config {
    let mut config = single_crate_config(crate_name, tag_tmpl);
    config.npms = Some(vec![entry]);
    config
}

fn single_crate_config(crate_name: &str, tag_tmpl: &str) -> anodizer_core::config::Config {
    let mut config = anodizer_core::config::Config::default();
    config.crates = vec![anodizer_core::config::CrateConfig {
        name: crate_name.to_string(),
        tag_template: Some(tag_tmpl.to_string()),
        ..Default::default()
    }];
    config
}

/// Config with no crates.io-targeting crate: layer 2 has nothing to
/// probe, so layer-1/3 tests exercise their subject in isolation.
/// Named (rather than inlining `Config::default()` at call sites) so
/// the six guard tests state the fixture's INTENT — "no cargo crate"
/// is the property under test, not an incidental default.
fn no_cargo_config() -> anodizer_core::config::Config {
    anodizer_core::config::Config::default()
}

/// Minimal in-memory repo config: one crates.io-targeting cargo crate
/// per `(name, tag_template)` pair.
fn config_with_cargo_crates(crates: &[(&str, &str)]) -> anodizer_core::config::Config {
    let mut config = anodizer_core::config::Config::default();
    config.crates = crates
        .iter()
        .map(|(name, tmpl)| anodizer_core::config::CrateConfig {
            name: name.to_string(),
            tag_template: Some(tmpl.to_string()),
            publish: Some(anodizer_core::config::PublishConfig {
                cargo: Some(anodizer_core::config::CargoPublishConfig::default()),
                ..Default::default()
            }),
            ..Default::default()
        })
        .collect();
    config
}

#[test]
#[cfg(unix)]
fn guard_refuses_when_release_is_published() {
    let tmp = tempfile::tempdir().unwrap();
    init_github_origin_repo(tmp.path());
    let gh = write_gh_stub(tmp.path(), r#"echo '{"id": 1, "draft": false}'"#);

    let err =
        check_no_published_releases(tmp.path(), &gh, &["v1.0.0".to_string()], &quiet_log(), &[])
            .expect_err("published release must block rollback");
    let msg = err.to_string();
    assert!(msg.contains("refusing to roll back"), "got: {msg}");
    assert!(msg.contains("v1.0.0"), "must name the blocking tag: {msg}");
    assert!(
        msg.contains("--force"),
        "must name the override flag: {msg}"
    );
    assert!(
        err.downcast_ref::<RollbackRefusal>().is_some(),
        "a published-release refusal must be typed for the failure policy"
    );
    assert!(
        msg.contains("ORPHAN"),
        "must warn the release may be an orphan of a pre-cleanup rollback: {msg}"
    );
}

#[test]
#[cfg(unix)]
fn guard_allows_when_release_is_draft() {
    let tmp = tempfile::tempdir().unwrap();
    init_github_origin_repo(tmp.path());
    let gh = write_gh_stub(tmp.path(), r#"echo '{"id": 1, "draft": true}'"#);

    check_no_published_releases(tmp.path(), &gh, &["v1.0.0".to_string()], &quiet_log(), &[])
        .expect("draft release is reversible; rollback may proceed");
}

#[test]
#[cfg(unix)]
fn guard_treats_missing_draft_field_as_published() {
    let tmp = tempfile::tempdir().unwrap();
    init_github_origin_repo(tmp.path());
    let gh = write_gh_stub(tmp.path(), r#"echo '{"id": 1}'"#);

    let err =
        check_no_published_releases(tmp.path(), &gh, &["v1.0.0".to_string()], &quiet_log(), &[])
            .expect_err("a release whose draft state is unknown must block");
    assert!(err.to_string().contains("refusing to roll back"));
}

#[test]
#[cfg(unix)]
fn guard_allows_when_no_release_exists() {
    let tmp = tempfile::tempdir().unwrap();
    init_github_origin_repo(tmp.path());
    let gh = write_gh_stub(
        tmp.path(),
        r#"echo 'gh: HTTP 404: Not Found (https://api.github.com/...)' >&2; exit 1"#,
    );

    check_no_published_releases(tmp.path(), &gh, &["v1.0.0".to_string()], &quiet_log(), &[])
        .expect("404 means no release; rollback may proceed");
}

#[test]
#[cfg(unix)]
fn guard_fails_closed_on_indeterminate_probe() {
    // gh binary missing entirely — with a GitHub-shaped origin and
    // no summary, an unanswerable probe means zero evidence the
    // version is safe to destroy: refuse and point at --force.
    let tmp = tempfile::tempdir().unwrap();
    init_github_origin_repo(tmp.path());
    let missing = tmp.path().join("nonexistent-gh");

    let err = check_no_published_releases(
        tmp.path(),
        &missing,
        &["v1.0.0".to_string()],
        &quiet_log(),
        &[],
    )
    .expect_err("indeterminate probe must fail closed");
    let msg = err.to_string();
    assert!(msg.contains("could not determine"), "got: {msg}");
    assert!(msg.contains("v1.0.0"), "must name the tag: {msg}");
    assert!(msg.contains("--force"), "must name the escape hatch: {msg}");
    assert!(
        err.downcast_ref::<RollbackRefusal>().is_none(),
        "an indeterminate (transient) fail-closed is mechanical, not a \
             by-design refusal — it must NOT be typed as RollbackRefusal"
    );
}

#[test]
fn guard_fails_closed_when_origin_unresolvable() {
    // No 'origin' remote at all — zero evidence either way, so the
    // guard must refuse, not warn-and-proceed.
    let tmp = tempfile::tempdir().unwrap();
    let _ = init_bump_repo(tmp.path(), 0);
    let gh = tmp.path().join("gh-never-spawned");

    let err =
        check_no_published_releases(tmp.path(), &gh, &["v1.0.0".to_string()], &quiet_log(), &[])
            .expect_err("unresolvable origin must fail closed");
    let msg = err.to_string();
    assert!(msg.contains("refusing to roll back"), "got: {msg}");
    assert!(msg.contains("'origin'"), "must name the remote: {msg}");
    assert!(msg.contains("--force"), "must name the escape hatch: {msg}");
}

#[test]
fn guard_proceeds_for_resolvable_non_github_origin() {
    // Origin resolves but is not github.com-shaped — the one
    // genuinely-inapplicable case: no github.com release can exist,
    // so the guard warns and proceeds without spawning the probe.
    let tmp = tempfile::tempdir().unwrap();
    let _ = init_bump_repo(tmp.path(), 0);
    run_git(
        tmp.path(),
        &["remote", "add", "origin", "https://gitlab.com/o/r.git"],
    );
    let gh = tmp.path().join("gh-never-spawned");

    check_no_published_releases(tmp.path(), &gh, &["v1.0.0".to_string()], &quiet_log(), &[])
        .expect("non-github.com origin carries no probe signal; rollback may proceed");
}

#[test]
#[cfg(unix)]
fn guard_fails_closed_on_gh_auth_error() {
    // gh present but erroring (auth/network) — same fail-closed
    // ruling as a missing gh, with the probe error surfaced.
    let tmp = tempfile::tempdir().unwrap();
    init_github_origin_repo(tmp.path());
    let gh = write_gh_stub(
        tmp.path(),
        r#"echo 'gh: HTTP 401: Bad credentials' >&2; exit 1"#,
    );

    let err =
        check_no_published_releases(tmp.path(), &gh, &["v1.0.0".to_string()], &quiet_log(), &[])
            .expect_err("auth-failed probe must fail closed");
    assert!(
        err.to_string().contains("401"),
        "must carry the probe error"
    );
}

#[test]
#[serial(cwd)]
#[cfg(unix)]
fn run_refuses_rollback_when_release_is_published() {
    // End-to-end through `run_with_gh`: the stub `gh` reports a
    // published release for v1.0.0 → rollback must refuse before
    // any mutation (tag intact, HEAD untouched).
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_github_origin_repo(dir);
    let gh = write_gh_stub(dir, r#"echo '{"id": 1, "draft": false}'"#);

    let _cwd = anodizer_core::test_helpers::CwdGuard::new(dir).unwrap();

    let err =
        run_with_gh(opts_for(dir, None), &gh).expect_err("published release must refuse rollback");
    assert!(err.to_string().contains("refusing to roll back"));

    let tags = git::get_tags_at_head_in(dir).unwrap();
    assert!(
        tags.contains(&"v1.0.0".to_string()),
        "tag must survive a refused rollback; got {tags:?}"
    );
}

#[test]
#[serial(cwd)]
#[cfg(unix)]
fn run_force_bypasses_published_release_guard() {
    // Same fixture, but --force: the guard is skipped (the stub gh
    // would refuse) and the local rollback completes. The stub
    // lives OUTSIDE the repo so the revert's dirty-tree check
    // doesn't trip on an untracked file.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_github_origin_repo(dir);
    let stub_dir = tempfile::tempdir().unwrap();
    let _gh = write_gh_stub(stub_dir.path(), r#"echo '{"id": 1, "draft": false}'"#);

    let _cwd = anodizer_core::test_helpers::CwdGuard::new(dir).unwrap();

    let mut opts = opts_for(dir, None);
    opts.force = true;
    run(opts).expect("--force rollback must proceed without the guard");
    let tags = git::get_tags_at_head_in(dir).unwrap();
    assert!(
        !tags.contains(&"v1.0.0".to_string()),
        "tag must be deleted under --force"
    );
}

// -----------------------------------------------------------------
// GitHub release cleanup: a rolled-back tag's release belongs to the
// aborted attempt and is deleted alongside the tag (matched by tag).
// -----------------------------------------------------------------

/// gh stub that records every invocation's args to `record` and
/// answers GETs with a release object (id 7) while accepting DELETEs.
#[cfg(unix)]
fn write_recording_gh_stub(dir: &Path, record: &Path) -> std::path::PathBuf {
    write_gh_stub(
        dir,
        &format!(
            "echo \"$@\" >> {record}\n\
                 case \"$*\" in *DELETE*) exit 0;; *) echo '{{\"id\": 7, \"draft\": true}}';; esac",
            record = record.display()
        ),
    )
}

#[test]
#[cfg(unix)]
fn release_cleanup_deletes_release_matched_by_tag() {
    let tmp = tempfile::tempdir().unwrap();
    init_github_origin_repo(tmp.path());
    let record = tmp.path().join("gh-calls.log");
    let gh = write_recording_gh_stub(tmp.path(), &record);

    delete_release_at_tag(tmp.path(), &gh, "v1.0.0", true, &quiet_log());

    let calls = std::fs::read_to_string(&record).expect("gh must have been consulted");
    assert!(
        calls.contains("/repos/o/r/releases/tags/v1.0.0"),
        "lookup must match by THIS tag only: {calls}"
    );
    assert!(
        calls.contains("-X DELETE /repos/o/r/releases/7"),
        "must delete the release id the tag lookup returned: {calls}"
    );
}

#[test]
#[cfg(unix)]
fn release_cleanup_noop_when_no_release_exists() {
    let tmp = tempfile::tempdir().unwrap();
    init_github_origin_repo(tmp.path());
    let record = tmp.path().join("gh-calls.log");
    let gh = write_gh_stub(
        tmp.path(),
        &format!(
            "echo \"$@\" >> {}\necho 'gh: HTTP 404: Not Found' >&2; exit 1",
            record.display()
        ),
    );

    delete_release_at_tag(tmp.path(), &gh, "v1.0.0", true, &quiet_log());

    let calls = std::fs::read_to_string(&record).expect("lookup must have run");
    assert!(
        !calls.contains("DELETE"),
        "no release means no DELETE call: {calls}"
    );
}

#[test]
#[cfg(unix)]
fn release_cleanup_skipped_for_non_github_origin() {
    let tmp = tempfile::tempdir().unwrap();
    let _ = init_bump_repo(tmp.path(), 0);
    run_git(
        tmp.path(),
        &["remote", "add", "origin", "https://gitlab.com/o/r.git"],
    );
    let record = tmp.path().join("gh-calls.log");
    let gh = write_recording_gh_stub(tmp.path(), &record);

    delete_release_at_tag(tmp.path(), &gh, "v1.0.0", true, &quiet_log());

    assert!(
        !record.exists(),
        "gh must never be spawned for a non-github.com origin"
    );
}

/// A tag with NO run summary is not attributed to this rollback: any
/// GitHub release it carries (a human's draft notes, a prior reversible
/// release) must be LEFT IN PLACE, never deleted — even though the tag
/// itself is removed.
#[test]
#[cfg(unix)]
fn release_cleanup_preserves_unattributed_release() {
    let tmp = tempfile::tempdir().unwrap();
    init_github_origin_repo(tmp.path());
    let record = tmp.path().join("gh-calls.log");
    let gh = write_recording_gh_stub(tmp.path(), &record);

    let outcome = delete_release_at_tag(tmp.path(), &gh, "v1.0.0", false, &quiet_log());

    assert!(matches!(outcome, ReleaseCleanup::Cleared));
    let calls = std::fs::read_to_string(&record).expect("lookup must have run");
    assert!(
        !calls.contains("DELETE"),
        "an unattributed release must never be deleted: {calls}"
    );
}

/// When an OWNED release lookup succeeds but the DELETE fails, the tag is
/// RETAINED so the rollback stays retryable, never orphaning the release
/// under a deleted tag.
#[test]
#[cfg(unix)]
fn release_cleanup_retains_tag_when_release_delete_fails() {
    let tmp = tempfile::tempdir().unwrap();
    init_github_origin_repo(tmp.path());
    let record = tmp.path().join("gh-calls.log");
    let gh = write_gh_stub(
        tmp.path(),
        &format!(
            "echo \"$@\" >> {record}\n\
                 case \"$*\" in *DELETE*) echo 'gh: HTTP 500' >&2; exit 1;; \
                 *) echo '{{\"id\": 7}}';; esac",
            record = record.display()
        ),
    );

    let outcome = delete_release_at_tag(tmp.path(), &gh, "v1.0.0", true, &quiet_log());

    assert!(
        matches!(outcome, ReleaseCleanup::Retained),
        "a failed owned-release delete must retain the tag for retry"
    );
}

// -----------------------------------------------------------------
// Summary-based published-state guard: the run summary on disk is
// the primary evidence; the gh probe is consulted only for tags
// with no summary. Proven with gh stubs whose answer CONTRADICTS
// the summary, so the assertion pins which source decided.
// -----------------------------------------------------------------

/// Write a run summary for `tag` under the repo's dist tree.
/// `rel` is the run-dir path relative to dist (e.g. "run-v1.0.0"
/// or "mycrate/run-mycrate-v1.0.0"), `results` the per-publisher
/// rows. The top-level flags are computed the way the producer
/// computes them (via the public types), so these fixtures cannot
/// drift from the real writer's shape.
fn write_summary(
    repo: &Path,
    rel: &str,
    tag: &str,
    irreversibly_published: bool,
    results: Vec<anodizer_stage_publish::run_summary::RunSummaryResult>,
) {
    use anodizer_stage_publish::run_summary::{
        DeterminismAllowlist, RunSummary, write_summary_json,
    };
    let summary = RunSummary {
        schema_version: RunSummary::CURRENT_SCHEMA_VERSION,
        anodize_version: "0.0.0-test".to_string(),
        tag: tag.to_string(),
        submitter_gated: false,
        announce_gated: false,
        publishers_succeeded: 0,
        publishers_failed: 0,
        irreversibly_published,
        failure_policy: None,
        verify_release: None,
        retry_backoff_secs: 0.0,
        retry_by_scope: vec![],
        results,
        determinism_allowlist: DeterminismAllowlist::default(),
    };
    write_summary_json(&summary, &repo.join("dist").join(rel).join("summary.json"))
        .expect("write summary fixture");
}

fn summary_result(
    name: &str,
    group: anodizer_core::publish_report::PublisherGroup,
    status: &str,
) -> anodizer_stage_publish::run_summary::RunSummaryResult {
    anodizer_stage_publish::run_summary::RunSummaryResult {
        name: name.to_string(),
        group,
        required: true,
        status: status.to_string(),
        evidence: None,
    }
}

#[test]
#[cfg(unix)]
fn guard_refuses_when_summary_shows_irreversible_publish() {
    use anodizer_core::publish_report::PublisherGroup;
    // The gh stub answers 404 (no release — would PERMIT), so the
    // refusal can only come from the summary: the summary is the
    // primary evidence and must win.
    let tmp = tempfile::tempdir().unwrap();
    init_github_origin_repo(tmp.path());
    let gh = write_gh_stub(tmp.path(), r#"echo 'gh: HTTP 404: Not Found' >&2; exit 1"#);
    write_summary(
        tmp.path(),
        "run-v1.0.0",
        "v1.0.0",
        true,
        vec![
            summary_result("cargo", PublisherGroup::Submitter, "succeeded"),
            summary_result(
                "chocolatey",
                PublisherGroup::Submitter,
                "pending-moderation",
            ),
            summary_result("github-release", PublisherGroup::Assets, "succeeded"),
        ],
    );

    let err = check_not_irreversibly_published(
        tmp.path(),
        &gh,
        &["v1.0.0".to_string()],
        &no_cargo_config(),
        &probes_with_crates_io(&probe_untouched),
        &quiet_log(),
    )
    .expect_err("irreversible publish in the summary must block rollback");
    let msg = err.to_string();
    assert!(
        msg.contains("version burned at cargo, chocolatey"),
        "got: {msg}"
    );
    assert!(
        !msg.contains("github-release"),
        "reversible publishers must not be blamed: {msg}"
    );
    assert!(
        msg.contains("--force"),
        "must name the override flag: {msg}"
    );
    assert!(
        msg.contains("cut the NEXT version"),
        "must suggest fix-forward: {msg}"
    );
    assert!(
        err.downcast_ref::<RollbackRefusal>().is_some(),
        "a burn-evidence refusal must be typed so the failure policy \
             renders it as protection, not breakage"
    );
}

#[test]
#[cfg(unix)]
fn guard_permits_when_summary_shows_only_reversible_publishers() {
    use anodizer_core::publish_report::PublisherGroup;
    // The gh stub reports a published release (would REFUSE), but
    // the summary proves only reversible publishers landed — a
    // same-version re-cut is still possible, so rollback proceeds
    // and the probe is never consulted for this tag.
    let tmp = tempfile::tempdir().unwrap();
    init_github_origin_repo(tmp.path());
    let gh = write_gh_stub(tmp.path(), r#"echo '{"id": 1, "draft": false}'"#);
    write_summary(
        tmp.path(),
        "run-v1.0.0",
        "v1.0.0",
        false,
        vec![
            summary_result("github-release", PublisherGroup::Assets, "succeeded"),
            summary_result("homebrew", PublisherGroup::Manager, "succeeded"),
            summary_result(
                "cargo",
                PublisherGroup::Submitter,
                "skipped-submitter-gated",
            ),
        ],
    );

    check_not_irreversibly_published(
        tmp.path(),
        &gh,
        &["v1.0.0".to_string()],
        &no_cargo_config(),
        &probes_with_crates_io(&probe_untouched),
        &quiet_log(),
    )
    .expect("reversible-only summary must permit rollback without probing GitHub");
}

#[test]
#[cfg(unix)]
fn guard_refuses_on_legacy_summary_without_the_flag() {
    // A summary written before `irreversibly_published` existed
    // (raw JSON, field absent) still blocks via the per-result
    // group/status rows.
    let tmp = tempfile::tempdir().unwrap();
    init_github_origin_repo(tmp.path());
    let gh = write_gh_stub(tmp.path(), r#"echo 'gh: HTTP 404: Not Found' >&2; exit 1"#);
    let dir = tmp.path().join("dist").join("run-v1.0.0");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("summary.json"),
        r#"{
                "schema_version": 1,
                "anodize_version": "0.7.0",
                "tag": "v1.0.0",
                "submitter_gated": false,
                "announce_gated": false,
                "results": [{
                    "name": "cargo",
                    "group": "Submitter",
                    "required": true,
                    "status": "succeeded",
                    "evidence": null
                }],
                "determinism_allowlist": {"compile_time": [], "runtime": []}
            }"#,
    )
    .unwrap();

    let err = check_not_irreversibly_published(
        tmp.path(),
        &gh,
        &["v1.0.0".to_string()],
        &no_cargo_config(),
        &probes_with_crates_io(&probe_untouched),
        &quiet_log(),
    )
    .expect_err("legacy summary with a landed Submitter must block");
    assert!(err.to_string().contains("version burned at cargo"));
}

#[test]
#[cfg(unix)]
fn guard_falls_back_to_release_probe_when_no_summary_matches_the_tag() {
    use anodizer_core::publish_report::PublisherGroup;
    // A summary exists but for a DIFFERENT tag: the guarded tag has
    // no summary evidence, so the gh probe decides — and it reports
    // a published release, so rollback refuses.
    let tmp = tempfile::tempdir().unwrap();
    init_github_origin_repo(tmp.path());
    let gh = write_gh_stub(tmp.path(), r#"echo '{"id": 1, "draft": false}'"#);
    write_summary(
        tmp.path(),
        "run-v0.9.0",
        "v0.9.0",
        false,
        vec![summary_result(
            "github-release",
            PublisherGroup::Assets,
            "succeeded",
        )],
    );

    let err = check_not_irreversibly_published(
        tmp.path(),
        &gh,
        &["v1.0.0".to_string()],
        &no_cargo_config(),
        &probes_with_crates_io(&probe_untouched),
        &quiet_log(),
    )
    .expect_err("unsummarized tag must fall back to the release probe");
    assert!(
        err.to_string()
            .contains("published GitHub release(s) exist")
    );
}

#[test]
#[cfg(unix)]
fn guard_reads_per_crate_summary_layout() {
    use anodizer_core::publish_report::PublisherGroup;
    let tmp = tempfile::tempdir().unwrap();
    init_github_origin_repo(tmp.path());
    let gh = write_gh_stub(tmp.path(), r#"echo 'gh: HTTP 404: Not Found' >&2; exit 1"#);
    write_summary(
        tmp.path(),
        "mycrate/run-mycrate-v1.0.0",
        "mycrate-v1.0.0",
        true,
        vec![summary_result(
            "cargo",
            PublisherGroup::Submitter,
            "succeeded",
        )],
    );

    let err = check_not_irreversibly_published(
        tmp.path(),
        &gh,
        &["mycrate-v1.0.0".to_string()],
        &no_cargo_config(),
        &probes_with_crates_io(&probe_untouched),
        &quiet_log(),
    )
    .expect_err("per-crate summary must be found and must block");
    assert!(err.to_string().contains("version burned at cargo"));
}

#[test]
#[cfg(unix)]
fn guard_ignores_malformed_summary_and_falls_back_to_probe() {
    // Unparseable summary carries no evidence: warn, then let the
    // probe decide (404 here → rollback permitted).
    let tmp = tempfile::tempdir().unwrap();
    init_github_origin_repo(tmp.path());
    let gh = write_gh_stub(tmp.path(), r#"echo 'gh: HTTP 404: Not Found' >&2; exit 1"#);
    let dir = tmp.path().join("dist").join("run-v1.0.0");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("summary.json"), "not json {").unwrap();

    check_not_irreversibly_published(
        tmp.path(),
        &gh,
        &["v1.0.0".to_string()],
        &no_cargo_config(),
        &probes_with_crates_io(&probe_untouched),
        &quiet_log(),
    )
    .expect("malformed summary + 404 probe must permit rollback");
}

// -----------------------------------------------------------------
// Global crates.io index probe (layer 2): the run summary answers a
// per-run question, but whether a version is burned on crates.io is
// GLOBAL state — a PRIOR run may have published it, and that run's
// summary lives on another runner's disk.
// -----------------------------------------------------------------

#[test]
fn crates_io_versions_for_tag_maps_tag_families_to_crates() {
    // The tag family prefix comes from the crate's tag_template, NOT
    // the crate name: cfgd's `crd-v...` tags belong to `cfgd-crd`.
    let config = config_with_cargo_crates(&[
        ("cfgd-crd", "crd-v{{ Version }}"),
        ("cfgd", "v{{ Version }}"),
    ]);
    assert_eq!(
        crates_io_versions_for_tag(&config, "crd-v0.5.0").probes,
        vec![("cfgd-crd".to_string(), "0.5.0".to_string())]
    );
    assert_eq!(
        crates_io_versions_for_tag(&config, "v0.5.0").probes,
        vec![("cfgd".to_string(), "0.5.0".to_string())]
    );
    let unmapped = crates_io_versions_for_tag(&config, "other-v1.0.0");
    assert!(
        unmapped.probes.is_empty() && unmapped.matched_non_crates_io == 0,
        "a tag outside every configured family maps to nothing"
    );
}

#[test]
fn crates_io_versions_for_tag_lockstep_maps_every_sharing_crate() {
    // Lockstep workspaces share one `v...` family across all crates —
    // a lockstep tag must probe every crates.io-targeting crate.
    let config = config_with_cargo_crates(&[("core", "v{{ Version }}"), ("cli", "v{{ Version }}")]);
    assert_eq!(
        crates_io_versions_for_tag(&config, "v1.2.3").probes,
        vec![
            ("core".to_string(), "1.2.3".to_string()),
            ("cli".to_string(), "1.2.3".to_string()),
        ]
    );
}

#[test]
fn crates_io_versions_for_tag_excludes_custom_registry_crates() {
    // A custom `registry:` points at a different index; the crates.io
    // probe carries no signal for it (same scoping judgment the
    // publisher's guard applies).
    let mut config = config_with_cargo_crates(&[("corp-crate", "v{{ Version }}")]);
    config.crates[0]
        .publish
        .as_mut()
        .expect("fixture publish block")
        .cargo
        .as_mut()
        .expect("fixture cargo block")
        .registry = Some("corp".to_string());
    let mapping = crates_io_versions_for_tag(&config, "v1.0.0");
    assert!(mapping.probes.is_empty());
    assert_eq!(
        mapping.matched_non_crates_io, 1,
        "the family matched — the crate just probes a different index"
    );
}

#[test]
#[cfg(unix)]
fn crates_io_probe_refuses_burned_version_despite_clean_summary() {
    use anodizer_core::publish_report::PublisherGroup;
    // The v0.5.0 attempt-#5 regression: this run's summary for
    // crd-v0.5.0 shows only reversible publishers (clean), but
    // cfgd-crd@0.5.0 is live on crates.io from a PRIOR run — the
    // per-run summary must not permit deleting a tag whose version is
    // globally burned.
    let tmp = tempfile::tempdir().unwrap();
    init_github_origin_repo(tmp.path());
    let gh = write_gh_stub(tmp.path(), r#"echo 'gh: HTTP 404: Not Found' >&2; exit 1"#);
    write_summary(
        tmp.path(),
        "cfgd-crd/run-crd-v0.5.0",
        "crd-v0.5.0",
        false,
        vec![summary_result(
            "github-release",
            PublisherGroup::Assets,
            "succeeded",
        )],
    );
    let config = config_with_cargo_crates(&[("cfgd-crd", "crd-v{{ Version }}")]);
    let probe = |name: &str, version: &str| -> Result<bool> {
        assert_eq!(
            (name, version),
            ("cfgd-crd", "0.5.0"),
            "probe must target the crate name + version the tag stamps on crates.io"
        );
        Ok(true)
    };

    let err = check_not_irreversibly_published(
        tmp.path(),
        &gh,
        &["crd-v0.5.0".to_string()],
        &config,
        &probes_with_crates_io(&probe),
        &quiet_log(),
    )
    .expect_err("a version live on the crates.io index must refuse rollback");
    let msg = err.to_string();
    assert!(
        msg.contains("live on the crates.io index"),
        "must name the global registry state: {msg}"
    );
    assert!(
        msg.contains("cfgd-crd@0.5.0"),
        "must name the burned crate@version: {msg}"
    );
    assert!(
        msg.contains("prior attempt"),
        "must explain the source: {msg}"
    );
    assert!(
        msg.contains("cut the NEXT version"),
        "must suggest fix-forward: {msg}"
    );
    assert!(msg.contains("--force"), "must name the escape hatch: {msg}");
    assert!(
        err.downcast_ref::<RollbackRefusal>().is_some(),
        "an index-burn refusal must be typed for the failure policy"
    );
    assert!(
        !msg.contains("No local run summary corroborates"),
        "a summarized tag's index burn is corroborated — no ownership caveat: {msg}"
    );
}

/// Index-only burn evidence (no run summary for the tag at all):
/// existence on crates.io proves publication, not ownership. The refusal
/// notes the absence of a corroborating summary — leading with the likely
/// own-prior-run explanation and pointing at the crates.io page so the
/// rarer foreign-ownership case can be ruled out.
#[test]
#[cfg(unix)]
fn crates_io_refusal_notes_possible_squatting_without_summary() {
    let tmp = tempfile::tempdir().unwrap();
    init_github_origin_repo(tmp.path());
    let gh = write_gh_stub(tmp.path(), r#"echo 'gh: HTTP 404: Not Found' >&2; exit 1"#);
    let config = config_with_cargo_crates(&[("test-project", "v{{ Version }}")]);
    let probe = |_: &str, _: &str| -> Result<bool> { Ok(true) };

    let err = check_not_irreversibly_published(
        tmp.path(),
        &gh,
        &["v0.1.0".to_string()],
        &config,
        &probes_with_crates_io(&probe),
        &quiet_log(),
    )
    .expect_err("index-live version must refuse rollback");
    let msg = err.to_string();
    assert!(
        msg.contains("No local run summary corroborates"),
        "uncorroborated index evidence must raise the ownership caveat: {msg}"
    );
    assert!(
        msg.contains("most likely a prior run of yours"),
        "the caveat must lead with the likely own-publish explanation, not squatting: {msg}"
    );
    assert!(
        msg.contains("https://crates.io/crates/test-project"),
        "must link the crates.io page to verify ownership: {msg}"
    );
    assert!(
        err.downcast_ref::<RollbackRefusal>().is_some(),
        "still a typed refusal"
    );
}

#[test]
#[cfg(unix)]
fn crates_io_probe_permits_absent_version_with_clean_summary() {
    use anodizer_core::publish_report::PublisherGroup;
    // Clean summary AND the version positively absent from the index:
    // nothing irreversible anywhere ⇒ rollback permitted.
    let tmp = tempfile::tempdir().unwrap();
    init_github_origin_repo(tmp.path());
    let gh = write_gh_stub(tmp.path(), r#"echo 'gh: HTTP 404: Not Found' >&2; exit 1"#);
    write_summary(
        tmp.path(),
        "run-v1.0.0",
        "v1.0.0",
        false,
        vec![summary_result(
            "github-release",
            PublisherGroup::Assets,
            "succeeded",
        )],
    );
    let config = config_with_cargo_crates(&[("mycrate", "v{{ Version }}")]);
    let probe = |_: &str, _: &str| -> Result<bool> { Ok(false) };

    check_not_irreversibly_published(
        tmp.path(),
        &gh,
        &["v1.0.0".to_string()],
        &config,
        &probes_with_crates_io(&probe),
        &quiet_log(),
    )
    .expect("clean summary + version absent from the index must permit rollback");
}

#[test]
#[cfg(unix)]
fn crates_io_probe_unreachable_index_fails_closed() {
    use anodizer_core::publish_report::PublisherGroup;
    // The index cannot be consulted: publication state is unverifiable,
    // so the guard must refuse (fail closed) rather than gamble a
    // destructive tag delete on a transient outage.
    let tmp = tempfile::tempdir().unwrap();
    init_github_origin_repo(tmp.path());
    let gh = write_gh_stub(tmp.path(), r#"echo 'gh: HTTP 404: Not Found' >&2; exit 1"#);
    write_summary(
        tmp.path(),
        "run-v1.0.0",
        "v1.0.0",
        false,
        vec![summary_result(
            "github-release",
            PublisherGroup::Assets,
            "succeeded",
        )],
    );
    let config = config_with_cargo_crates(&[("mycrate", "v{{ Version }}")]);
    let probe = |_: &str, _: &str| -> Result<bool> { Err(anyhow::anyhow!("connection refused")) };

    let err = check_not_irreversibly_published(
        tmp.path(),
        &gh,
        &["v1.0.0".to_string()],
        &config,
        &probes_with_crates_io(&probe),
        &quiet_log(),
    )
    .expect_err("an unreachable index must fail closed");
    let msg = err.to_string();
    assert!(
        msg.contains("could not be reached"),
        "must explain the index is unreachable: {msg}"
    );
    assert!(
        msg.contains("no proof the version(s) are safe to destroy"),
        "must explain publication state is unverifiable: {msg}"
    );
    assert!(msg.contains("--force"), "must name the escape hatch: {msg}");
}

#[test]
#[cfg(unix)]
fn crates_io_probe_bails_when_tag_maps_to_no_crate() {
    // The config publishes to crates.io, but the guarded tag matches no
    // crate's tag family: the probe is blind for that tag and must fail
    // closed instead of silently narrowing itself to zero crates.
    let tmp = tempfile::tempdir().unwrap();
    init_github_origin_repo(tmp.path());
    let gh = write_gh_stub(tmp.path(), r#"echo 'gh: HTTP 404: Not Found' >&2; exit 1"#);
    let config = config_with_cargo_crates(&[("myapp", "app-v{{ Version }}")]);
    let probe = |_: &str, _: &str| -> Result<bool> {
        panic!("an unmapped tag must never reach the index probe")
    };

    let err = check_not_irreversibly_published(
        tmp.path(),
        &gh,
        &["v1.0.0".to_string()],
        &config,
        &probes_with_crates_io(&probe),
        &quiet_log(),
    )
    .expect_err("an unmappable tag must fail closed");
    let msg = err.to_string();
    assert!(
        msg.contains("could not map these tag(s) to any crate"),
        "must name the mapping failure: {msg}"
    );
    assert!(msg.contains("v1.0.0"), "must name the tag: {msg}");
    assert!(
        msg.contains("tag_template"),
        "must point at the family mapping to fix: {msg}"
    );
    assert!(msg.contains("--force"), "must name the escape hatch: {msg}");
}

#[test]
#[cfg(unix)]
fn crates_io_probe_proceeds_when_mapped_crates_skip_crates_io() {
    // The tag maps to a crate, but that crate publishes to a custom
    // registry: no crates.io one-way door exists for it, so the guard
    // proceeds without probing (distinct from the unmapped-tag bail).
    let tmp = tempfile::tempdir().unwrap();
    init_github_origin_repo(tmp.path());
    let gh = write_gh_stub(tmp.path(), r#"echo 'gh: HTTP 404: Not Found' >&2; exit 1"#);
    let mut config = config_with_cargo_crates(&[
        ("corp-crate", "corp-v{{ Version }}"),
        ("public-crate", "pub-v{{ Version }}"),
    ]);
    config.crates[0]
        .publish
        .as_mut()
        .expect("fixture publish block")
        .cargo
        .as_mut()
        .expect("fixture cargo block")
        .registry = Some("corp".to_string());
    let probe = |_: &str, _: &str| -> Result<bool> {
        panic!("a custom-registry crate must never reach the crates.io probe")
    };

    check_not_irreversibly_published(
        tmp.path(),
        &gh,
        &["corp-v1.0.0".to_string()],
        &config,
        &probes_with_crates_io(&probe),
        &quiet_log(),
    )
    .expect("a mapped crate outside crates.io carries no cargo one-way door");
}

#[test]
#[cfg(unix)]
fn crates_io_probe_dedups_repeated_crate_version_probes() {
    // Under Scope::All a monorepo-prefixed and a bare tag can resolve to
    // the same crate@version; the index must be consulted once per pair.
    let tmp = tempfile::tempdir().unwrap();
    init_github_origin_repo(tmp.path());
    let gh = write_gh_stub(tmp.path(), r#"echo 'gh: HTTP 404: Not Found' >&2; exit 1"#);
    let mut config = config_with_cargo_crates(&[("mycrate", "v{{ Version }}")]);
    config.monorepo = Some(anodizer_core::config::MonorepoConfig {
        tag_prefix: Some("sub/".to_string()),
        ..Default::default()
    });
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let probe = |name: &str, version: &str| -> Result<bool> {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert_eq!((name, version), ("mycrate", "1.0.0"));
        Ok(false)
    };

    check_not_irreversibly_published(
        tmp.path(),
        &gh,
        &["v1.0.0".to_string(), "sub/v1.0.0".to_string()],
        &config,
        &probes_with_crates_io(&probe),
        &quiet_log(),
    )
    .expect("version absent from the index must permit rollback");
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the duplicate crate@version pair must be probed exactly once"
    );
}

#[test]
fn crates_io_probes_run_concurrently() {
    // Two crates share the bare `v...` family (lockstep): both probes
    // rendezvous on a barrier, which only resolves when they execute on
    // different workers at the same time — a serialized probe loop would
    // deadlock here (and trip the barrier's wait), never pass.
    let config = config_with_cargo_crates(&[("a", "v{{ Version }}"), ("b", "v{{ Version }}")]);
    let barrier = std::sync::Barrier::new(2);
    let probe = |_: &str, _: &str| -> Result<bool> {
        barrier.wait();
        Ok(false)
    };
    check_not_burned_on_crates_io(&["v1.0.0".to_string()], &[], &config, &probe, &quiet_log())
        .expect("absent versions must permit rollback");
}

// -----------------------------------------------------------------
// Immutable-registry (npm / pypi) burn probe: the cross-runner layer
// that catches a version a PRIOR run burned on npm/pypi with no local
// summary — the second half of the same fail-closed treatment crates.io
// gets, since both are one-way doors (immutable npm version, permanent
// PyPI filename).
// -----------------------------------------------------------------

#[test]
#[cfg(unix)]
fn npm_pypi_pypi_live_hit_refuses() {
    // The gh stub answers 404 (would PERMIT) and there is no summary, so
    // the refusal can only come from the live pypi probe.
    let tmp = tempfile::tempdir().unwrap();
    init_github_origin_repo(tmp.path());
    let gh = write_gh_stub(tmp.path(), r#"echo 'gh: HTTP 404: Not Found' >&2; exit 1"#);
    let config = config_with_pypi(
        "mytool",
        "v{{ Version }}",
        anodizer_core::config::PypiConfig::default(),
    );
    let pypi = |repository: &str, project: &str, version: &str| -> Result<bool> {
        assert_eq!(
            (project, version),
            ("mytool", "1.0.0"),
            "probe must target the pypi project + version the tag stamps"
        );
        assert!(
            repository.contains("pypi.org"),
            "an unset repository defaults to production PyPI: {repository}"
        );
        Ok(true)
    };

    let err = check_not_irreversibly_published(
        tmp.path(),
        &gh,
        &["v1.0.0".to_string()],
        &config,
        &probes_with_npm_pypi(&immutable_probe_untouched, &pypi),
        &quiet_log(),
    )
    .expect_err("a version live on PyPI must refuse rollback");
    let msg = err.to_string();
    assert!(
        msg.contains("live on an immutable registry"),
        "must name the global registry state: {msg}"
    );
    assert!(
        msg.contains("mytool==1.0.0"),
        "must name the burned project==version: {msg}"
    );
    assert!(msg.contains("cut the NEXT version"), "fix-forward: {msg}");
    assert!(msg.contains("--force"), "must name the escape hatch: {msg}");
    assert!(
        err.downcast_ref::<RollbackRefusal>().is_some(),
        "an immutable-registry burn must be typed for the failure policy"
    );
}

#[test]
#[cfg(unix)]
fn npm_pypi_npm_live_hit_refuses() {
    let tmp = tempfile::tempdir().unwrap();
    init_github_origin_repo(tmp.path());
    let gh = write_gh_stub(tmp.path(), r#"echo 'gh: HTTP 404: Not Found' >&2; exit 1"#);
    let config = config_with_npm(
        "mytool",
        "v{{ Version }}",
        anodizer_core::config::NpmConfig::default(),
    );
    let npm = |registry: &str, package: &str, version: &str| -> Result<bool> {
        assert_eq!(
            (package, version),
            ("mytool", "1.0.0"),
            "probe must target the metapackage + version the tag stamps"
        );
        assert!(
            registry.contains("registry.npmjs.org"),
            "an unset registry defaults to the public npm registry: {registry}"
        );
        Ok(true)
    };

    let err = check_not_irreversibly_published(
        tmp.path(),
        &gh,
        &["v1.0.0".to_string()],
        &config,
        &probes_with_npm_pypi(&npm, &immutable_probe_untouched),
        &quiet_log(),
    )
    .expect_err("a version live on npm must refuse rollback");
    let msg = err.to_string();
    assert!(
        msg.contains("live on an immutable registry"),
        "must name the global registry state: {msg}"
    );
    assert!(
        msg.contains("mytool@1.0.0"),
        "must name the burned package@version: {msg}"
    );
    assert!(
        err.downcast_ref::<RollbackRefusal>().is_some(),
        "an immutable-registry burn must be typed for the failure policy"
    );
}

#[test]
#[cfg(unix)]
fn npm_pypi_probes_summarized_tag_catching_recorded_failed_landing() {
    use anodizer_core::publish_report::PublisherGroup;
    // The immutable-door verification race: this run's summary for v1.0.0
    // records only a reversible publisher — npm is ABSENT because the
    // publish landed at the registry but a read-timeout after the 201 made
    // anodizer record it as failed. Layer 1 sees no burned Submitter and
    // would permit the rollback; only the live npm probe proves the version
    // is burned. A summarized tag must therefore STILL be probed (parity
    // with the crates.io sibling, which probes every tag) — the regression
    // this guards is the old `unsummarized`-only skip that let this class of
    // burn through into a poisoning same-version re-cut.
    let tmp = tempfile::tempdir().unwrap();
    init_github_origin_repo(tmp.path());
    let gh = write_gh_stub(tmp.path(), r#"echo 'gh: HTTP 404: Not Found' >&2; exit 1"#);
    write_summary(
        tmp.path(),
        "run-v1.0.0",
        "v1.0.0",
        false,
        vec![summary_result(
            "github-release",
            PublisherGroup::Assets,
            "succeeded",
        )],
    );
    let config = config_with_npm(
        "mytool",
        "v{{ Version }}",
        anodizer_core::config::NpmConfig::default(),
    );
    let npm = |_: &str, _: &str, _: &str| -> Result<bool> { Ok(true) };

    let err = check_not_irreversibly_published(
        tmp.path(),
        &gh,
        &["v1.0.0".to_string()],
        &config,
        &probes_with_npm_pypi(&npm, &immutable_probe_untouched),
        &quiet_log(),
    )
    .expect_err("a summarized tag whose npm version is live must still refuse");
    let msg = err.to_string();
    assert!(
        msg.contains("live on an immutable registry"),
        "the live probe must fire even for a summarized tag: {msg}"
    );
    assert!(
        msg.contains("mytool@1.0.0"),
        "must name the burned package@version: {msg}"
    );
    assert!(
        err.downcast_ref::<RollbackRefusal>().is_some(),
        "an immutable-registry burn must be typed for the failure policy"
    );
}

#[test]
#[cfg(unix)]
fn npm_pypi_unreachable_registry_fails_closed() {
    // The registry cannot be consulted: publication state is unverifiable,
    // so the guard fails closed (a mechanical bail, NOT a typed refusal).
    let tmp = tempfile::tempdir().unwrap();
    init_github_origin_repo(tmp.path());
    let gh = write_gh_stub(tmp.path(), r#"echo 'gh: HTTP 404: Not Found' >&2; exit 1"#);
    let config = config_with_pypi(
        "mytool",
        "v{{ Version }}",
        anodizer_core::config::PypiConfig::default(),
    );
    let pypi =
        |_: &str, _: &str, _: &str| -> Result<bool> { Err(anyhow::anyhow!("connection refused")) };

    let err = check_not_irreversibly_published(
        tmp.path(),
        &gh,
        &["v1.0.0".to_string()],
        &config,
        &probes_with_npm_pypi(&immutable_probe_untouched, &pypi),
        &quiet_log(),
    )
    .expect_err("an unreachable immutable registry must fail closed");
    let msg = err.to_string();
    assert!(
        msg.contains("could not be reached"),
        "must explain the registry is unreachable: {msg}"
    );
    assert!(
        msg.contains("no proof the version(s) are safe to destroy"),
        "must explain publication state is unverifiable: {msg}"
    );
    assert!(msg.contains("--force"), "must name the escape hatch: {msg}");
    assert!(
        err.downcast_ref::<RollbackRefusal>().is_none(),
        "a transient unreachable-registry fail-closed is mechanical, not a by-design refusal"
    );
}

#[test]
#[cfg(unix)]
fn npm_pypi_templated_name_fails_closed() {
    // The pypi project name is a template expression that cannot be
    // resolved outside a release run — the guard cannot name the immutable
    // project it would orphan, so it fails closed (never probes).
    let tmp = tempfile::tempdir().unwrap();
    init_github_origin_repo(tmp.path());
    let gh = write_gh_stub(tmp.path(), r#"echo 'gh: HTTP 404: Not Found' >&2; exit 1"#);
    let config = config_with_pypi(
        "mytool",
        "v{{ Version }}",
        anodizer_core::config::PypiConfig {
            name: Some("{{ .ProjectName }}-tool".to_string()),
            ..Default::default()
        },
    );

    let err = check_not_irreversibly_published(
        tmp.path(),
        &gh,
        &["v1.0.0".to_string()],
        &config,
        &probes_with_npm_pypi(&immutable_probe_untouched, &immutable_probe_untouched),
        &quiet_log(),
    )
    .expect_err("an unresolvable templated package name must fail closed");
    let msg = err.to_string();
    assert!(
        msg.contains("could not resolve the npm/pypi package name"),
        "must name the resolution failure: {msg}"
    );
    assert!(msg.contains("v1.0.0"), "must name the tag: {msg}");
    assert!(msg.contains("--force"), "must name the escape hatch: {msg}");
}

#[test]
#[cfg(unix)]
fn npm_pypi_unconfigured_config_proceeds() {
    // No npm/pypi publisher in the config: no immutable one-way door to
    // probe, so the guard proceeds without consulting either registry.
    let tmp = tempfile::tempdir().unwrap();
    init_github_origin_repo(tmp.path());
    let gh = write_gh_stub(tmp.path(), r#"echo 'gh: HTTP 404: Not Found' >&2; exit 1"#);
    let config = single_crate_config("mytool", "v{{ Version }}");

    check_not_irreversibly_published(
        tmp.path(),
        &gh,
        &["v1.0.0".to_string()],
        &config,
        &probes_with_npm_pypi(&immutable_probe_untouched, &immutable_probe_untouched),
        &quiet_log(),
    )
    .expect("a config with no npm/pypi door must permit rollback without probing");
}

#[test]
#[cfg(unix)]
fn npm_pypi_entry_not_versioned_by_tag_proceeds() {
    // The npm entry maps to crate 'mytool' (family `v...`), but the
    // rolled-back tag belongs to a different family: the entry is not
    // versioned by the tag, so no probe fires.
    let tmp = tempfile::tempdir().unwrap();
    init_github_origin_repo(tmp.path());
    let gh = write_gh_stub(tmp.path(), r#"echo 'gh: HTTP 404: Not Found' >&2; exit 1"#);
    let config = config_with_npm(
        "mytool",
        "v{{ Version }}",
        anodizer_core::config::NpmConfig::default(),
    );

    check_not_irreversibly_published(
        tmp.path(),
        &gh,
        &["app-v1.0.0".to_string()],
        &config,
        &probes_with_npm_pypi(&immutable_probe_untouched, &immutable_probe_untouched),
        &quiet_log(),
    )
    .expect("a tag that versions no npm/pypi entry must permit rollback");
}

#[test]
#[cfg(unix)]
fn npm_pypi_summarized_clean_tag_probed_and_permitted() {
    use anodizer_core::publish_report::PublisherGroup;
    // A tag WITH a clean run summary is STILL probed live (parity with the
    // crates.io sibling — the summary alone cannot rule out the
    // recorded-failed-but-actually-landed immutable-door race). When the
    // live probe confirms the version is NOT on the registry, rollback is
    // permitted. The probe returning Ok(false) here (rather than being
    // untouched) is the point: it fires, and its negative answer is what
    // clears the tag.
    let tmp = tempfile::tempdir().unwrap();
    init_github_origin_repo(tmp.path());
    let gh = write_gh_stub(tmp.path(), r#"echo 'gh: HTTP 404: Not Found' >&2; exit 1"#);
    write_summary(
        tmp.path(),
        "run-v1.0.0",
        "v1.0.0",
        false,
        vec![summary_result(
            "github-release",
            PublisherGroup::Assets,
            "succeeded",
        )],
    );
    let config = config_with_npm(
        "mytool",
        "v{{ Version }}",
        anodizer_core::config::NpmConfig::default(),
    );
    let not_live = |_: &str, _: &str, _: &str| -> Result<bool> { Ok(false) };

    check_not_irreversibly_published(
        tmp.path(),
        &gh,
        &["v1.0.0".to_string()],
        &config,
        &probes_with_npm_pypi(&not_live, &not_live),
        &quiet_log(),
    )
    .expect("a summarized tag the live probe confirms un-burned permits rollback");
}

/// Moderated-registry probe stub that must never be consulted — for
/// fixtures with no chocolatey/winget publisher configured.
fn moderated_probe_untouched(_: &str, _: &str) -> Result<Option<String>> {
    panic!("moderated-registry probe must not be consulted on this path")
}

/// Winget sibling of [`moderated_probe_untouched`].
fn winget_probe_untouched(_: &WingetProbeSpec) -> Result<Option<String>> {
    panic!("winget probe must not be consulted on this path")
}

fn config_with_choco_crate(choco_name: Option<&str>) -> anodizer_core::config::Config {
    let mut config = anodizer_core::config::Config::default();
    config.crates = vec![anodizer_core::config::CrateConfig {
        name: "mytool".to_string(),
        tag_template: Some("v{{ Version }}".to_string()),
        publish: Some(anodizer_core::config::PublishConfig {
            chocolatey: Some(anodizer_core::config::ChocolateyConfig {
                name: choco_name.map(str::to_string),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }];
    config
}

fn config_with_winget_crate(package_identifier: &str) -> anodizer_core::config::Config {
    let mut config = anodizer_core::config::Config::default();
    config.crates = vec![anodizer_core::config::CrateConfig {
        name: "mytool".to_string(),
        tag_template: Some("v{{ Version }}".to_string()),
        publish: Some(anodizer_core::config::PublishConfig {
            winget: Some(anodizer_core::config::WingetConfig {
                package_identifier: Some(package_identifier.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }];
    config
}

#[test]
fn moderated_registries_choco_pending_submission_refuses() {
    let config = config_with_choco_crate(None);
    let choco = |id: &str, version: &str| -> Result<Option<String>> {
        assert_eq!((id, version), ("mytool", "1.0.0"));
        Ok(Some(
            "submitted, currently: awaiting moderation".to_string(),
        ))
    };
    let probes = BurnProbes {
        crates_io: &probe_untouched,
        npm: &immutable_probe_untouched,
        pypi: &immutable_probe_untouched,
        chocolatey: &choco,
        winget: &winget_probe_untouched,
    };
    let err = check_not_burned_on_moderated_registries(
        &["v1.0.0".to_string()],
        &config,
        &probes,
        &quiet_log(),
    )
    .expect_err("a pending chocolatey submission consumes the version");
    let msg = err.to_string();
    assert!(
        msg.contains("chocolatey package 'mytool@1.0.0'"),
        "must name the burned package: {msg}"
    );
    assert!(msg.contains("awaiting moderation"), "got: {msg}");
    assert!(
        err.downcast_ref::<RollbackRefusal>().is_some(),
        "a moderated-registry burn must be a typed refusal"
    );
}

#[test]
fn moderated_registries_choco_name_override_is_probed() {
    let config = config_with_choco_crate(Some("renamed-pkg"));
    let choco = |id: &str, _: &str| -> Result<Option<String>> {
        assert_eq!(id, "renamed-pkg");
        Ok(None)
    };
    let probes = BurnProbes {
        crates_io: &probe_untouched,
        npm: &immutable_probe_untouched,
        pypi: &immutable_probe_untouched,
        chocolatey: &choco,
        winget: &winget_probe_untouched,
    };
    check_not_burned_on_moderated_registries(
        &["v1.0.0".to_string()],
        &config,
        &probes,
        &quiet_log(),
    )
    .expect("a never-submitted version must permit rollback");
}

#[test]
fn moderated_registries_winget_open_pr_refuses() {
    let config = config_with_winget_crate("Acme.MyTool");
    let winget = |spec: &WingetProbeSpec| -> Result<Option<String>> {
        assert_eq!(
            spec,
            &WingetProbeSpec {
                upstream: "microsoft/winget-pkgs".to_string(),
                package_id: "Acme.MyTool".to_string(),
                version: "2.0.0".to_string(),
                search_in_title: true,
            }
        );
        Ok(Some("an open manifest PR is pending".to_string()))
    };
    let probes = BurnProbes {
        crates_io: &probe_untouched,
        npm: &immutable_probe_untouched,
        pypi: &immutable_probe_untouched,
        chocolatey: &moderated_probe_untouched,
        winget: &winget,
    };
    let err = check_not_burned_on_moderated_registries(
        &["v2.0.0".to_string()],
        &config,
        &probes,
        &quiet_log(),
    )
    .expect_err("an open winget manifest PR consumes the version");
    let msg = err.to_string();
    assert!(
        msg.contains("winget package 'Acme.MyTool' at 2.0.0"),
        "must name the burned package: {msg}"
    );
    assert!(msg.contains("open manifest PR"), "got: {msg}");
    assert!(err.downcast_ref::<RollbackRefusal>().is_some());
}

#[test]
fn moderated_registries_probe_error_fails_open() {
    // Unlike the crates.io index probe (fail closed), the moderated
    // registries are advisory evidence: a probe failure warns and
    // proceeds so rate-limited/flaky endpoints cannot dead-end recovery.
    let config = config_with_choco_crate(None);
    let failing =
        |_: &str, _: &str| -> Result<Option<String>> { anyhow::bail!("connection reset by peer") };
    let probes = BurnProbes {
        crates_io: &probe_untouched,
        npm: &immutable_probe_untouched,
        pypi: &immutable_probe_untouched,
        chocolatey: &failing,
        winget: &winget_probe_untouched,
    };
    check_not_burned_on_moderated_registries(
        &["v1.0.0".to_string()],
        &config,
        &probes,
        &quiet_log(),
    )
    .expect("an unreachable moderated registry must warn and proceed");
}

#[test]
fn moderated_registries_skipped_when_publisher_not_configured() {
    // Cargo-only config: neither moderated-registry probe may run.
    let config = config_with_cargo_crates(&[("mycrate", "v{{ Version }}")]);
    let probes = BurnProbes {
        crates_io: &probe_untouched,
        npm: &immutable_probe_untouched,
        pypi: &immutable_probe_untouched,
        chocolatey: &moderated_probe_untouched,
        winget: &winget_probe_untouched,
    };
    check_not_burned_on_moderated_registries(
        &["v1.0.0".to_string()],
        &config,
        &probes,
        &quiet_log(),
    )
    .expect("no moderated publisher configured — nothing to probe");
}

#[test]
fn moderated_registries_templated_package_id_skips_probe() {
    // A template override cannot be resolved without a release context;
    // the probe must skip (warn) rather than probe a guessed id.
    let config = config_with_choco_crate(Some("{{ .ProjectName }}"));
    let probes = BurnProbes {
        crates_io: &probe_untouched,
        npm: &immutable_probe_untouched,
        pypi: &immutable_probe_untouched,
        chocolatey: &moderated_probe_untouched,
        winget: &winget_probe_untouched,
    };
    check_not_burned_on_moderated_registries(
        &["v1.0.0".to_string()],
        &config,
        &probes,
        &quiet_log(),
    )
    .expect("an unresolvable package id skips the advisory probe");
}

#[test]
fn moderated_registries_non_community_choco_feed_skips_probe() {
    // Only the community gallery has a moderation queue; a private feed
    // target must not be probed against community.chocolatey.org.
    let mut config = config_with_choco_crate(None);
    config.crates[0]
        .publish
        .as_mut()
        .unwrap()
        .chocolatey
        .as_mut()
        .unwrap()
        .source_repo = Some("https://nuget.internal.example/v2/".to_string());
    let probes = BurnProbes {
        crates_io: &probe_untouched,
        npm: &immutable_probe_untouched,
        pypi: &immutable_probe_untouched,
        chocolatey: &moderated_probe_untouched,
        winget: &winget_probe_untouched,
    };
    check_not_burned_on_moderated_registries(
        &["v1.0.0".to_string()],
        &config,
        &probes,
        &quiet_log(),
    )
    .expect("a non-community feed has no moderation queue — nothing to probe");
}

#[test]
fn moderated_registries_community_choco_feed_spelled_explicitly_is_probed() {
    // An explicit source_repo equal to the community push endpoint
    // (trailing-slash / case variance included) must still probe.
    let mut config = config_with_choco_crate(None);
    config.crates[0]
        .publish
        .as_mut()
        .unwrap()
        .chocolatey
        .as_mut()
        .unwrap()
        .source_repo = Some("HTTPS://PUSH.CHOCOLATEY.ORG".to_string());
    let choco = |id: &str, version: &str| -> Result<Option<String>> {
        assert_eq!((id, version), ("mytool", "1.0.0"));
        Ok(None)
    };
    let probes = BurnProbes {
        crates_io: &probe_untouched,
        npm: &immutable_probe_untouched,
        pypi: &immutable_probe_untouched,
        chocolatey: &choco,
        winget: &winget_probe_untouched,
    };
    check_not_burned_on_moderated_registries(
        &["v1.0.0".to_string()],
        &config,
        &probes,
        &quiet_log(),
    )
    .expect("the community feed spelled explicitly must still be probed");
}

#[test]
fn moderated_registries_winget_probe_uses_configured_upstream() {
    // The probe must search the same upstream the publisher would
    // submit to (repository.pull_request.base), not a hardcoded
    // microsoft/winget-pkgs.
    let mut config = config_with_winget_crate("Acme.MyTool");
    config.crates[0]
        .publish
        .as_mut()
        .unwrap()
        .winget
        .as_mut()
        .unwrap()
        .repository = Some(anodizer_core::config::RepositoryConfig {
        pull_request: Some(anodizer_core::config::PullRequestConfig {
            base: Some(anodizer_core::config::PullRequestBaseConfig {
                owner: Some("acme".to_string()),
                name: Some("winget-fork".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    });
    let winget = |spec: &WingetProbeSpec| -> Result<Option<String>> {
        assert_eq!(spec.upstream, "acme/winget-fork");
        Ok(None)
    };
    let probes = BurnProbes {
        crates_io: &probe_untouched,
        npm: &immutable_probe_untouched,
        pypi: &immutable_probe_untouched,
        chocolatey: &moderated_probe_untouched,
        winget: &winget,
    };
    check_not_burned_on_moderated_registries(
        &["v2.0.0".to_string()],
        &config,
        &probes,
        &quiet_log(),
    )
    .expect("clear upstream must permit rollback");
}

#[test]
fn moderated_registries_custom_commit_template_widens_winget_search() {
    // A custom commit_msg_template makes the PR title unpredictable, so
    // the probe must drop the in:title qualifier.
    let mut config = config_with_winget_crate("Acme.MyTool");
    config.crates[0]
        .publish
        .as_mut()
        .unwrap()
        .winget
        .as_mut()
        .unwrap()
        .commit_msg_template = Some("chore: bump {{ .Version }}".to_string());
    let winget = |spec: &WingetProbeSpec| -> Result<Option<String>> {
        assert!(
            !spec.search_in_title,
            "a custom PR-title template must widen the search to title+body"
        );
        Ok(None)
    };
    let probes = BurnProbes {
        crates_io: &probe_untouched,
        npm: &immutable_probe_untouched,
        pypi: &immutable_probe_untouched,
        chocolatey: &moderated_probe_untouched,
        winget: &winget,
    };
    check_not_burned_on_moderated_registries(
        &["v2.0.0".to_string()],
        &config,
        &probes,
        &quiet_log(),
    )
    .expect("clear search must permit rollback");
}

#[test]
fn winget_probe_token_prefers_github_token_via_env_seam() {
    let env = anodizer_core::MapEnvSource::new()
        .with("GITHUB_TOKEN", "gh-primary")
        .with("GH_TOKEN", "gh-fallback");
    assert_eq!(winget_probe_token(&env).as_deref(), Some("gh-primary"));
    let fallback_only = anodizer_core::MapEnvSource::new().with("GH_TOKEN", "gh-fallback");
    assert_eq!(
        winget_probe_token(&fallback_only).as_deref(),
        Some("gh-fallback")
    );
    let empty = anodizer_core::MapEnvSource::new();
    assert_eq!(winget_probe_token(&empty), None);
}

#[test]
#[serial(cwd)]
fn run_without_config_fails_closed() {
    // Unparseable config: the guard cannot map tags to crates, so a
    // non-forced rollback must refuse instead of silently skipping the
    // crates.io probe (the pre-fix fail-open).
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let _bump_sha = init_bump_repo(dir, 0);
    std::fs::write(dir.join(".anodizer.yaml"), "::: not yaml {").unwrap();
    add_non_github_origin(dir);

    let _cwd = anodizer_core::test_helpers::CwdGuard::new(dir).unwrap();

    let err = run(opts_for(dir, None)).expect_err("missing config must fail closed");
    let msg = format!("{err}");
    assert!(
        msg.contains("could not load the anodizer config"),
        "must name the config failure: {msg}"
    );
    assert!(msg.contains("--force"), "must name the escape hatch: {msg}");
    // Nothing was mutated: the tag survives the refusal.
    let tags = git::get_tags_at_head_in(dir).unwrap();
    assert_eq!(tags, vec!["v1.0.0".to_string()]);
}

#[test]
#[serial(cwd)]
#[cfg(unix)]
fn run_force_bypasses_crates_io_probe() {
    // --force skips the whole published-state guard, index probe
    // included: with a committed config whose crate family matches the
    // tag (the probe WOULD map v1.0.0 → mycrate@1.0.0), the rollback
    // still completes without consulting any registry. Companion to
    // `run_force_bypasses_published_release_guard`, which pins the same
    // bypass for the GitHub-release layer.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(
            dir.join(".anodizer.yaml"),
            "crates:\n  - name: mycrate\n    path: .\n    tag_template: \"v{{ Version }}\"\n    publish:\n      cargo: {}\n",
        )
        .unwrap();
    init_github_origin_repo(dir);

    let _cwd = anodizer_core::test_helpers::CwdGuard::new(dir).unwrap();

    let mut opts = opts_for(dir, None);
    opts.force = true;
    run(opts).expect("--force rollback must proceed without the crates.io probe");
    let tags = git::get_tags_at_head_in(dir).unwrap();
    assert!(
        !tags.contains(&"v1.0.0".to_string()),
        "tag must be deleted under --force"
    );
}
