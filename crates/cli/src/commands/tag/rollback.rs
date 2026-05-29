//! `anodize tag rollback` — delete anodize-managed tags at a SHA and
//! revert (or reset to) the bump commit they point at.
//!
//! Failure-recovery counterpart to `anodize tag`: when a downstream
//! `anodize release` poisons a tag (publish failure, mcp 422, etc.) the
//! operator is left with a tag pointing at a bumped-but-broken commit.
//! This subcommand deletes the tag locally + on origin, then either
//! `git revert`s the bump commit (default, history-preserving) or
//! `git reset --hard`s past it (opt-in, history-rewriting).
//!
//! Safety rails:
//! - Tag name regex filter — only anodize-shaped tags are touched
//!   (`vX.Y.Z[-pre][+build]` for lockstep, `<crate>-vX.Y.Z[...]` for
//!   per-crate). Non-matching tags are skipped with a reason printed.
//! - Hard-fail when non-anodize commits sit between the target SHA and
//!   HEAD in `--mode=revert` (protects against rolling back a bump
//!   after unrelated work landed on top). Use `--mode=reset` to force.

use anodizer_core::git;
use anodizer_core::log::{StageLogger, Verbosity};
use anyhow::{Result, bail};
use regex::Regex;
use std::path::PathBuf;

/// Scope filter for which tag shape(s) to operate on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Both lockstep (`vX.Y.Z`) and per-crate (`<crate>-vX.Y.Z`) tags.
    All,
    /// Only lockstep tags (`vX.Y.Z`).
    Lockstep,
    /// Only per-crate tags (`<crate>-vX.Y.Z`).
    PerCrate,
}

impl std::str::FromStr for Scope {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "all" => Ok(Scope::All),
            "lockstep" => Ok(Scope::Lockstep),
            "per-crate" | "percrate" => Ok(Scope::PerCrate),
            other => Err(format!(
                "invalid --scope value: {other:?} (expected all | lockstep | per-crate)"
            )),
        }
    }
}

/// Rollback strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `git revert --no-edit <sha>` — preserves history. Default.
    Revert,
    /// `git reset --hard <sha>~1` — rewrites history; requires
    /// `--force-with-lease` to push. Opt-in only.
    Reset,
}

impl std::str::FromStr for Mode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "revert" => Ok(Mode::Revert),
            "reset" => Ok(Mode::Reset),
            other => Err(format!(
                "invalid --mode value: {other:?} (expected revert | reset)"
            )),
        }
    }
}

pub struct RollbackOpts {
    /// Target SHA. `None` resolves to `HEAD`.
    pub sha: Option<String>,
    pub dry_run: bool,
    pub no_push: bool,
    pub scope: Scope,
    pub mode: Mode,
    pub verbose: bool,
    pub debug: bool,
    pub quiet: bool,
}

/// Strict semver-ish per-crate tag pattern: `<crate>-v<MAJOR>.<MINOR>.<PATCH>[-pre][+build]`.
/// The crate-name portion accepts ASCII letters, digits, `_` and `-`; we
/// then assert the suffix is anodize's `v<semver>` form so a tag like
/// `foo-bar` (no `-v` suffix) doesn't accidentally match.
fn per_crate_tag_re() -> Regex {
    Regex::new(
        r"^[A-Za-z0-9_][A-Za-z0-9_-]*-v\d+\.\d+\.\d+(?:-[A-Za-z0-9.-]+)?(?:\+[A-Za-z0-9.-]+)?$",
    )
    .expect("static regex compiles")
}

/// Lockstep tag pattern: `v<MAJOR>.<MINOR>.<PATCH>[-pre][+build]`.
fn lockstep_tag_re() -> Regex {
    Regex::new(r"^v\d+\.\d+\.\d+(?:-[A-Za-z0-9.-]+)?(?:\+[A-Za-z0-9.-]+)?$")
        .expect("static regex compiles")
}

/// Classification used to filter tags against the requested `--scope`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TagKind {
    Lockstep,
    PerCrate,
}

/// Classify a tag against anodize's naming conventions. Returns `None`
/// when the tag doesn't match either shape (in which case the rollback
/// command leaves it alone).
fn classify_tag(tag: &str) -> Option<TagKind> {
    // Lockstep first — `vX.Y.Z` would also fail the per-crate regex's
    // `<crate>-` prefix requirement, but the explicit ordering keeps the
    // intent obvious to a reader.
    if lockstep_tag_re().is_match(tag) {
        Some(TagKind::Lockstep)
    } else if per_crate_tag_re().is_match(tag) {
        Some(TagKind::PerCrate)
    } else {
        None
    }
}

/// Apply the `--scope` filter on top of the classification.
fn scope_includes(scope: Scope, kind: TagKind) -> bool {
    matches!(
        (scope, kind),
        (Scope::All, _)
            | (Scope::Lockstep, TagKind::Lockstep)
            | (Scope::PerCrate, TagKind::PerCrate)
    )
}

/// Build the rollback commit subject line. The deleted-tags list goes in
/// the body so a long per-crate batch doesn't blow past 72 chars.
fn build_revert_message(target_sha: &str, deleted_tags: &[String]) -> String {
    let primary = deleted_tags
        .iter()
        .find(|t| lockstep_tag_re().is_match(t))
        .cloned()
        .unwrap_or_else(|| {
            deleted_tags
                .first()
                .cloned()
                .unwrap_or_else(|| "release".to_string())
        });
    let short = if target_sha.len() > 7 {
        &target_sha[..7]
    } else {
        target_sha
    };
    let mut body = format!("chore(release): rollback {primary} [skip ci]\n\nReverts {short}.",);
    if !deleted_tags.is_empty() {
        body.push_str(&format!("\nTags deleted: {}", deleted_tags.join(", ")));
    }
    body
}

pub fn run(opts: RollbackOpts) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let log = StageLogger::new(
        "tag-rollback",
        Verbosity::from_flags(opts.quiet, opts.verbose, opts.debug),
    );

    // 1. Resolve target SHA (canonicalize to a full hash so log output
    //    is stable even when the user passed a short SHA / ref name).
    let raw_target = opts.sha.as_deref().unwrap_or("HEAD");
    let target_sha = git::rev_parse_in(&cwd, raw_target)?;
    log.status(&format!("target: {} ({})", raw_target, short(&target_sha)));

    // 2. Enumerate tags at that SHA.
    let all_tags_at_sha = git::get_tags_at_sha_in(&cwd, &target_sha)?;
    if all_tags_at_sha.is_empty() {
        log.warn(&format!("no tags found at {}", short(&target_sha)));
        // Nothing to delete; the revert step is still meaningful only if
        // there's a bump commit to undo. Without tags pointing at it the
        // operator is almost certainly running this against the wrong
        // SHA — bail rather than silently revert a random commit.
        bail!(
            "refusing to roll back: no tags point at {} — pass the bumped commit's SHA explicitly",
            short(&target_sha)
        );
    }

    // 3. Filter: anodize-shaped + matches --scope.
    let mut deletable: Vec<String> = Vec::new();
    for tag in &all_tags_at_sha {
        match classify_tag(tag) {
            None => log.status(&format!("skip (not anodize-shaped): {tag}")),
            Some(kind) if !scope_includes(opts.scope, kind) => log.status(&format!(
                "skip (scope filter --scope={:?}): {tag}",
                opts.scope
            )),
            Some(_) => deletable.push(tag.clone()),
        }
    }

    if deletable.is_empty() {
        log.warn(&format!(
            "no anodize-managed tags at {} match --scope={:?}",
            short(&target_sha),
            opts.scope
        ));
        return Ok(());
    }

    // 4. Safety check (--mode=revert only). Non-bump commits on top of
    //    the target SHA mean someone landed unrelated work since the
    //    bump; reverting blindly would lose it.
    if opts.mode == Mode::Revert {
        let intervening = git::commits_between_in(&cwd, &target_sha)?;
        // The bump commit IS the target SHA (revert of <sha> undoes it).
        // Anything strictly newer than <sha> is "on top of the bump".
        // Tolerate a single auto-generated `Revert "<sha>"` commit so
        // the operator can re-run the rollback idempotently.
        let mut suspicious: Vec<(String, String)> = Vec::new();
        for sha in &intervening {
            let subject = git::commit_subject_in(&cwd, sha).unwrap_or_default();
            if subject.starts_with("Revert ") || subject.starts_with("chore(release): rollback") {
                continue;
            }
            suspicious.push((sha.clone(), subject));
        }
        if !suspicious.is_empty() {
            let mut msg = format!(
                "cannot rollback — {} non-bump commit(s) sit between HEAD and {}:\n",
                suspicious.len(),
                short(&target_sha)
            );
            for (sha, subj) in &suspicious {
                msg.push_str(&format!("  {} {}\n", short(sha), subj));
            }
            msg.push_str("resolve manually, or use --mode=reset to force.");
            bail!("{msg}");
        }
    }

    // 5. Delete each anodize-shaped tag (best-effort: warn-and-continue
    //    per tag so a single remote-delete failure doesn't abandon the
    //    revert step).
    let mut deleted: Vec<String> = Vec::new();
    for tag in &deletable {
        if opts.dry_run {
            log.status(&format!("(dry-run) would delete tag: {tag} (remote+local)"));
            deleted.push(tag.clone());
            continue;
        }
        if !opts.no_push {
            match git::delete_remote_tag_in(&cwd, tag) {
                Ok(()) => log.status(&format!("deleted remote tag: {tag}")),
                Err(e) => log.warn(&format!(
                    "remote tag delete failed for {tag}: {e} (continuing)"
                )),
            }
        } else {
            log.status(&format!("--no-push: skipping remote delete for {tag}"));
        }
        match git::delete_local_tag_in(&cwd, tag) {
            Ok(()) => log.status(&format!("deleted local tag: {tag}")),
            Err(e) => log.warn(&format!(
                "local tag delete failed for {tag}: {e} (continuing)"
            )),
        }
        deleted.push(tag.clone());
    }

    // 6. Mode=reset short-circuits revert+push entirely. Print a loud
    //    warning so the operator knows they own the force-push.
    if opts.mode == Mode::Reset {
        let parent = format!("{}~1", target_sha);
        if opts.dry_run {
            log.status(&format!(
                "(dry-run) would: git reset --hard {} (parent of bump commit)",
                short(&target_sha)
            ));
        } else {
            git::reset_hard_in(&cwd, &parent)?;
            log.status(&format!(
                "reset HEAD to {} (parent of bump commit)",
                short(&target_sha)
            ));
        }
        log.warn(
            "--mode=reset rewrote local history. Push with \
             `git push --force-with-lease origin <branch>` when ready.",
        );
        return Ok(());
    }

    // 7. Mode=revert: create the revert commit (dry-run prints what
    //    would be committed without invoking git).
    let message = build_revert_message(&target_sha, &deleted);
    if opts.dry_run {
        log.status(&format!(
            "(dry-run) would: git revert --no-edit {} && git commit --amend -m {:?}",
            short(&target_sha),
            message
        ));
    } else {
        git::revert_commit_in(&cwd, &target_sha, Some(&message))?;
        log.status(&format!("created revert commit: {}", first_line(&message)));
    }

    // 8. Push the revert (skip on --no-push).
    if opts.no_push {
        log.status("--no-push: skipping branch push");
        return Ok(());
    }
    let branch = git::get_current_branch_in(&cwd)?;
    if opts.dry_run {
        log.status(&format!("(dry-run) would: git push origin {branch}"));
    } else {
        git::push_branch_in(&cwd, &branch)?;
        log.status(&format!("pushed revert to origin/{branch}"));
    }
    Ok(())
}

/// Trim a SHA to the canonical 7-char short form for log output.
fn short(sha: &str) -> &str {
    if sha.len() > 7 { &sha[..7] } else { sha }
}

/// First line of a multi-line commit message, for compact status lines.
fn first_line(msg: &str) -> &str {
    msg.lines().next().unwrap_or(msg)
}

// Re-export the path-bearing PathBuf import in this file so the clap
// surface in `lib.rs` can stay PathBuf-typed without adding a stray
// dependency.
#[allow(dead_code)]
fn _path_marker() -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

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
        );
        assert!(msg.starts_with("chore(release): rollback mycrate-v1.0.0 [skip ci]"));
    }

    // -----------------------------------------------------------------
    // Fixture-repo integration tests — exercise the safety-check path
    // and dry-run no-mutation guarantee against a real tempdir git repo.
    // -----------------------------------------------------------------

    use std::path::Path;
    use std::process::Command;

    fn run_git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .output()
            .expect("git invoke");
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
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(dir)
                .output()
                .unwrap()
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

    fn opts_for(dir: &Path, sha: Option<String>) -> RollbackOpts {
        let _ = dir; // cwd is process-global; the with-guard helpers below set it
        RollbackOpts {
            sha,
            dry_run: false,
            no_push: true,
            scope: Scope::All,
            mode: Mode::Revert,
            verbose: false,
            debug: false,
            quiet: true,
        }
    }

    /// Process-wide cwd swap. Marked `serial` upstream tests do the same;
    /// we re-use that pattern here.
    use serial_test::serial;

    #[test]
    #[serial]
    fn safety_check_fires_when_non_bump_commits_sit_on_top() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let bump_sha = init_bump_repo(dir, 2);

        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();

        let opts = opts_for(dir, Some(bump_sha));
        let err = run(opts).expect_err("safety check should fire");
        let msg = format!("{err}");
        assert!(msg.contains("cannot rollback"), "got: {msg}");
        assert!(
            msg.contains("non-bump commit"),
            "missing safety-check phrasing: {msg}"
        );

        std::env::set_current_dir(orig).unwrap();
    }

    #[test]
    #[serial]
    fn safety_check_passes_against_clean_head_at_bump_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let _bump_sha = init_bump_repo(dir, 0);

        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();

        // HEAD == bump_sha; safety check trivially passes (no commits
        // between HEAD and target).
        let mut opts = opts_for(dir, None);
        opts.dry_run = true; // don't mutate the fixture
        run(opts).expect("safety check should pass at HEAD == bump commit");

        // Tag still present (dry-run guarantee).
        let tags = git::get_tags_at_head_in(dir).unwrap();
        assert_eq!(tags, vec!["v1.0.0".to_string()]);

        std::env::set_current_dir(orig).unwrap();
    }

    #[test]
    #[serial]
    fn dry_run_makes_no_mutations() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let _bump_sha = init_bump_repo(dir, 0);
        let head_before = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(dir)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();

        let mut opts = opts_for(dir, None);
        opts.dry_run = true;
        run(opts).expect("dry-run should succeed");

        // Tag still present.
        let tags = git::get_tags_at_head_in(dir).unwrap();
        assert_eq!(tags, vec!["v1.0.0".to_string()]);
        // HEAD unchanged.
        let head_after = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(dir)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        assert_eq!(head_before, head_after);

        std::env::set_current_dir(orig).unwrap();
    }

    #[test]
    #[serial]
    fn no_push_skips_remote_ops_but_does_local_revert() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let bump_sha = init_bump_repo(dir, 0);
        // No 'origin' configured — push_branch_in would error otherwise.

        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();

        let opts = RollbackOpts {
            sha: None,
            dry_run: false,
            no_push: true,
            scope: Scope::All,
            mode: Mode::Revert,
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

        std::env::set_current_dir(orig).unwrap();
    }

    #[test]
    #[serial]
    fn skips_tags_not_matching_anodize_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let bump_sha = init_bump_repo(dir, 0);
        // Add a non-anodize tag at the same SHA.
        run_git(dir, &["tag", "internal-release"]);

        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();

        let opts = RollbackOpts {
            sha: None,
            dry_run: false,
            no_push: true,
            scope: Scope::All,
            mode: Mode::Revert,
            verbose: false,
            debug: false,
            quiet: true,
        };
        run(opts).expect("rollback should ignore non-anodize tag");

        // Non-anodize tag survived; anodize tag is gone.
        let surviving = git::get_tags_at_sha_in(dir, &bump_sha).unwrap();
        assert_eq!(surviving, vec!["internal-release".to_string()]);

        std::env::set_current_dir(orig).unwrap();
    }
}
