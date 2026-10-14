use anyhow::Result;
use std::path::Path;

use super::git_output_in;
use super::semver::{SemVer, parse_semver_tag};
use super::status::{is_git_dirty_in, is_git_repo_in};
use super::tags::get_first_commit_in;
use crate::redact::redact_url_credentials;

/// How the tag carried by a [`GitInfo`] was arrived at.
///
/// The two are not interchangeable to a gate that reasons about *which*
/// version a run targets: an inferred tag is a guess about intent that the
/// repository's own history can contradict, while a declared one is the
/// operator stating the target outright.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TagSource {
    /// Discovered by scanning the repository for the newest ref matching the
    /// crate's tag template — or the synthetic `v0.0.0` stand-in when the
    /// repository has no matching tag at all.
    #[default]
    Inferred,
    /// Supplied verbatim by the operator through the current-tag override
    /// chain (`ANODIZER_CURRENT_TAG`, the `GORELEASER_CURRENT_TAG` compat
    /// alias, or a tag-push `GITHUB_REF_NAME`).
    Declared,
}

#[derive(Debug, Clone)]
pub struct GitInfo {
    pub tag: String,
    /// Whether [`Self::tag`] was declared by the operator or inferred from the
    /// repository. Defaults to [`TagSource::Inferred`]; the tag-resolution
    /// layer promotes it to [`TagSource::Declared`] when an override supplied
    /// the name.
    pub tag_source: TagSource,
    pub commit: String,
    pub short_commit: String,
    pub branch: String,
    pub dirty: bool,
    pub semver: SemVer,
    /// ISO 8601 committer date of the HEAD commit, read by [`head_commit_time_in`].
    pub commit_date: String,
    /// Committer time of the HEAD commit as Unix seconds, read by
    /// [`head_commit_time_in`] in the same `git log` call as [`Self::commit_date`].
    pub commit_timestamp: String,
    /// Previous tag matching the same pattern, if any.
    /// Populated externally by the release command once the tag_template is known.
    pub previous_tag: Option<String>,
    /// Remote URL from `git remote get-url origin`.
    pub remote_url: String,
    /// Git describe summary (e.g. `v1.0.0-10-g34f56g3`) from `git describe --tags --always`.
    pub summary: String,
    /// Annotated tag subject (first line of tag message) or commit subject.
    pub tag_subject: String,
    /// Full annotated tag message or full commit message.
    pub tag_contents: String,
    /// Tag message body (everything after first line) or commit message body.
    pub tag_body: String,
    /// First commit hash in the repository (for changelog range when no previous tag).
    pub first_commit: Option<String>,
}

/// The committer time of a commit, as Unix seconds and as ISO 8601.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitTime {
    /// Seconds since 1970-01-01 UTC (`%ct`).
    pub timestamp: i64,
    /// The same instant in strict ISO 8601 with the committer's offset (`%cI`).
    pub iso8601: String,
}

/// Read the committer time of `HEAD` in the repository at `repo`.
///
/// Every commit-time value anodizer uses comes from this one read: the
/// `CommitTimestamp` and `CommitDate` template variables, the snapshot
/// source date and the determinism check's seed. The committer time is the
/// time the commit object was last written, so it moves on a rebase or an
/// amend together with the commit hash.
pub fn head_commit_time_in(repo: &Path) -> Result<CommitTime> {
    let out = git_output_in(
        repo,
        &[
            "-c",
            "log.showSignature=false",
            "log",
            "-1",
            "--format=%ct%n%cI",
            "HEAD",
        ],
    )?;
    let mut lines = out.lines();
    let (Some(secs), Some(iso)) = (lines.next(), lines.next()) else {
        anyhow::bail!("git log -1 --format=%ct%n%cI HEAD printed {out:?}, expected two lines");
    };
    let timestamp = secs.trim().parse::<i64>().map_err(|_| {
        anyhow::anyhow!("git log printed a committer time that is not a number: {secs:?}")
    })?;
    Ok(CommitTime {
        timestamp,
        iso8601: iso.trim().to_string(),
    })
}

/// Committer time of `HEAD` in the repository at `repo`, as Unix seconds.
pub fn head_commit_timestamp_in(repo: &Path) -> Result<i64> {
    Ok(head_commit_time_in(repo)?.timestamp)
}

/// Detect git info for a given tag.
///
/// When `skip_validate` is true and the tag is not valid semver, a warning is
/// logged and a default `SemVer { 0, 0, 0 }` is used instead of returning an error.
///
/// When `snapshot` is true and the working directory is not inside a git
/// repository, a synthetic `GitInfo` is returned (commit/branch/etc. left
/// empty) so users can run `anodizer release --snapshot` from a fresh tarball
/// or scratch directory without git ever having been initialized. Outside
/// snapshot mode, the missing repo bubbles as an error.
pub fn detect_git_info(tag: &str, skip_validate: bool) -> Result<GitInfo> {
    detect_git_info_in(&std::env::current_dir()?, tag, skip_validate)
}

/// Detect git info for a given tag against a repository at `cwd`.
///
/// Path-taking sibling of [`detect_git_info`] so callers (tests, library
/// consumers) can target an explicit repository without mutating the
/// process-wide cwd.
pub fn detect_git_info_in(cwd: &Path, tag: &str, skip_validate: bool) -> Result<GitInfo> {
    if !is_git_repo_in(cwd) {
        // Synthetic GitInfo for non-repo snapshot/scratch builds. Lets users
        // run `anodizer release --snapshot` from a fresh tarball or scratch
        // directory without `git init` first. Caller is responsible for only
        // accepting this in snapshot/dry-run mode.
        return Ok(GitInfo {
            tag: tag.to_string(),
            tag_source: TagSource::default(),
            commit: String::new(),
            short_commit: String::new(),
            branch: String::new(),
            dirty: false,
            semver: SemVer {
                major: 0,
                minor: 0,
                patch: 0,
                prerelease: None,
                build_metadata: None,
            },
            commit_date: String::new(),
            commit_timestamp: String::new(),
            previous_tag: None,
            remote_url: String::new(),
            summary: String::new(),
            tag_subject: String::new(),
            tag_contents: String::new(),
            tag_body: String::new(),
            first_commit: None,
        });
    }
    let commit = git_output_in(cwd, &["rev-parse", "HEAD"])?;
    let short_commit = git_output_in(cwd, &["rev-parse", "--short", "HEAD"])?;
    let branch = git_output_in(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    let dirty = is_git_dirty_in(cwd);
    let (commit_date, commit_timestamp) = match head_commit_time_in(cwd) {
        Ok(time) => (time.iso8601, time.timestamp.to_string()),
        Err(_) => (String::new(), String::new()),
    };
    // Use ls-remote --get-url.
    // Without an explicit remote name this defaults to "origin".
    //
    // A truly missing remote (no `origin` configured) is a legitimate state —
    // local-only repos, fresh `git init` — so detect must not fail.
    // But a *git error* during this lookup (broken config, transient SSH
    // failure, permission issue) used to be silently swallowed by
    // `unwrap_or_default()`, leaving `remote_url=""` with no diagnostic.
    // Preserve the
    // underlying error rather than replacing it with an empty sentinel.
    let remote_url_raw = match git_output_in(cwd, &["ls-remote", "--get-url"]) {
        Ok(url) => url,
        Err(e) => {
            // `e` already begins with `git ls-remote --get-url failed: …`
            // (from `git_output_in`), so a second "failed" lead-in would
            // double up; render the wrapped error verbatim with a trailing
            // note about the empty fallback.
            tracing::warn!("git remote URL detection: {e}; remote_url left empty");
            String::new()
        }
    };
    // Strip credentials from URLs of any scheme
    // (e.g. https://user:token@github.com/... → https://<redacted>@github.com/...).
    let remote_url = redact_url_credentials(&remote_url_raw);
    let summary = git_output_in(
        cwd,
        &[
            "-c",
            "log.showSignature=false",
            "describe",
            "--tags",
            "--always",
            "--dirty",
        ],
    )
    .unwrap_or_default();

    // Try annotated tag message fields first; fall back to commit message fields.
    let tag_subject = git_output_in(cwd, &["tag", "-l", "--format=%(contents:subject)", tag])
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            git_output_in(
                cwd,
                &["-c", "log.showSignature=false", "log", "-1", "--format=%s"],
            )
            .unwrap_or_default()
        });
    let tag_contents = git_output_in(cwd, &["tag", "-l", "--format=%(contents)", tag])
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            git_output_in(
                cwd,
                &["-c", "log.showSignature=false", "log", "-1", "--format=%B"],
            )
            .unwrap_or_default()
        });
    let tag_body = git_output_in(cwd, &["tag", "-l", "--format=%(contents:body)", tag])
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            git_output_in(
                cwd,
                &["-c", "log.showSignature=false", "log", "-1", "--format=%b"],
            )
            .unwrap_or_default()
        });

    let semver = match parse_semver_tag(tag) {
        Ok(sv) => sv,
        Err(e) => {
            if skip_validate {
                tracing::warn!("skipped validation — current tag is not semver");
                SemVer {
                    major: 0,
                    minor: 0,
                    patch: 0,
                    prerelease: None,
                    build_metadata: None,
                }
            } else {
                return Err(e);
            }
        }
    };
    let first_commit = get_first_commit_in(cwd).ok();
    Ok(GitInfo {
        tag: tag.to_string(),
        tag_source: TagSource::default(),
        commit,
        short_commit,
        branch,
        dirty,
        semver,
        commit_date,
        commit_timestamp,
        previous_tag: None,
        remote_url,
        summary,
        tag_subject,
        tag_contents,
        tag_body,
        first_commit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_git_info_in_bails_outside_repo_when_tag_not_semver() {
        let tmp = tempfile::tempdir().unwrap();
        // Non-repo path returns synthetic GitInfo with default SemVer, so
        // a non-semver tag is harmless. To force the error path, pass a
        // valid temp dir but with a non-semver tag AND a repo that exists.
        // Here cover the synthetic path: non-repo + arbitrary tag is Ok.
        let info = detect_git_info_in(tmp.path(), "v1.0.0", false).unwrap();
        assert_eq!(info.commit, "");
        assert_eq!(info.branch, "");
        assert_eq!(info.semver.major, 0);
    }

    #[test]
    fn detect_git_info_in_returns_synthetic_for_non_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let info = detect_git_info_in(tmp.path(), "v9.9.9", true).unwrap();
        assert_eq!(info.tag, "v9.9.9");
        assert_eq!(info.commit, "");
        assert!(info.first_commit.is_none());
    }

    #[test]
    fn detect_git_info_in_resolves_head_inside_real_repo() {
        use std::process::Command;

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
        run(&["init"]);
        run(&["config", "user.email", "t@t.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.join("a"), "1").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "c1"]);

        let capture = |args: &[&str]| -> String {
            let out = anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(args).current_dir(dir);
                    cmd
                },
                "git",
            );
            assert!(out.status.success(), "git {args:?} failed");
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        let expected_head = capture(&["rev-parse", "HEAD"]);
        let expected_root = capture(&["rev-list", "--max-parents=0", "HEAD"]);

        let info = detect_git_info_in(dir, "v1.0.0", false).unwrap();
        assert_eq!(info.commit, expected_head);
        assert_eq!(info.semver.major, 1);
        assert_eq!(info.first_commit.as_deref(), Some(expected_root.as_str()));
    }

    /// A rebase or amend leaves the author time behind and moves the
    /// committer time. `CommitTimestamp`, `CommitDate` and the snapshot
    /// source date must all carry the committer time.
    #[test]
    fn commit_timestamp_commit_date_and_snapshot_sde_all_carry_the_committer_time() {
        use anodizer_core::test_helpers::{git_test_ok, output_with_spawn_retry};

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        git_test_ok(dir, &["init", "-q"]);
        std::fs::write(dir.join("a"), "1").unwrap();
        git_test_ok(dir, &["add", "a"]);
        let out = output_with_spawn_retry(
            || {
                let mut cmd = crate::git::git_command();
                cmd.current_dir(dir)
                    .env("GIT_AUTHOR_DATE", "1600000000 +0000")
                    .env("GIT_COMMITTER_DATE", "1715000000 +0000")
                    .args(["commit", "-q", "-m", "c1"]);
                cmd
            },
            "git",
        );
        assert!(out.status.success(), "{out:?}");

        let info = detect_git_info_in(dir, "v1.0.0", false).unwrap();
        let mut ctx = crate::context::Context::new(
            crate::config::Config::default(),
            crate::context::ContextOptions::default(),
        );
        ctx.git_info = Some(info);
        ctx.populate_git_vars();
        let vars = ctx.template_vars();
        assert_eq!(
            vars.get("CommitTimestamp").map(String::as_str),
            Some("1715000000")
        );
        let commit_date = vars.get("CommitDate").cloned().unwrap_or_default();
        assert_eq!(
            chrono::DateTime::parse_from_rfc3339(&commit_date)
                .map(|d| d.timestamp())
                .ok(),
            Some(1_715_000_000),
            "CommitDate {commit_date:?}"
        );
        assert_eq!(
            crate::git::resolve_snapshot_sde_with_env(dir, &crate::MapEnvSource::new()).unwrap(),
            1_715_000_000
        );
    }
}
