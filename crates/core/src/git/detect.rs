use anyhow::Result;

use super::git_output;
use super::semver::{SemVer, parse_semver_tag};
use super::status::{is_git_dirty, is_git_repo};
use super::tags::get_first_commit;
use crate::redact::redact_url_credentials;

#[derive(Debug, Clone)]
pub struct GitInfo {
    pub tag: String,
    pub commit: String,
    pub short_commit: String,
    pub branch: String,
    pub dirty: bool,
    pub semver: SemVer,
    /// ISO 8601 committer date of HEAD commit (from `git log -1 --format=%cI`)
    pub commit_date: String,
    /// Unix timestamp of HEAD commit (from `git log -1 --format=%at`)
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
    if !is_git_repo() {
        // Synthetic GitInfo for non-repo snapshot/scratch builds. Lets users
        // run `anodizer release --snapshot` from a fresh tarball or scratch
        // directory without `git init` first. Caller is responsible for only
        // accepting this in snapshot/dry-run mode.
        return Ok(GitInfo {
            tag: tag.to_string(),
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
    let commit = git_output(&["rev-parse", "HEAD"])?;
    let short_commit = git_output(&["rev-parse", "--short", "HEAD"])?;
    let branch = git_output(&["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    let dirty = is_git_dirty();
    let commit_date = git_output(&["-c", "log.showSignature=false", "log", "-1", "--format=%cI"])
        .unwrap_or_default();
    let commit_timestamp =
        git_output(&["-c", "log.showSignature=false", "log", "-1", "--format=%at"])
            .unwrap_or_default();
    // Use ls-remote --get-url (matches GoReleaser git.go:355).
    // Without an explicit remote name this defaults to "origin".
    //
    // A truly missing remote (no `origin` configured) is a legitimate state —
    // local-only repos, fresh `git init` — so we don't want to fail detect.
    // But a *git error* during this lookup (broken config, transient SSH
    // failure, permission issue) used to be silently swallowed by
    // `unwrap_or_default()`, leaving `remote_url=""` with no diagnostic.
    // Mirrors the spirit of GoReleaser commit 5042b84 (Q12): preserve the
    // underlying error rather than replacing it with an empty sentinel.
    let remote_url_raw = match git_output(&["ls-remote", "--get-url"]) {
        Ok(url) => url,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "git ls-remote --get-url failed; remote_url left empty"
            );
            String::new()
        }
    };
    // Strip credentials from URLs of any scheme
    // (e.g. https://user:token@github.com/... → https://<redacted>@github.com/...).
    let remote_url = redact_url_credentials(&remote_url_raw);
    let summary = git_output(&[
        "-c",
        "log.showSignature=false",
        "describe",
        "--tags",
        "--always",
        "--dirty",
    ])
    .unwrap_or_default();

    // Try annotated tag message fields first; fall back to commit message fields.
    let tag_subject = git_output(&["tag", "-l", "--format=%(contents:subject)", tag])
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            git_output(&["-c", "log.showSignature=false", "log", "-1", "--format=%s"])
                .unwrap_or_default()
        });
    let tag_contents = git_output(&["tag", "-l", "--format=%(contents)", tag])
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            git_output(&["-c", "log.showSignature=false", "log", "-1", "--format=%B"])
                .unwrap_or_default()
        });
    let tag_body = git_output(&["tag", "-l", "--format=%(contents:body)", tag])
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            git_output(&["-c", "log.showSignature=false", "log", "-1", "--format=%b"])
                .unwrap_or_default()
        });

    let semver = match parse_semver_tag(tag) {
        Ok(sv) => sv,
        Err(e) => {
            if skip_validate {
                tracing::warn!("current tag is not semver, skipping validation");
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
    let first_commit = get_first_commit().ok();
    Ok(GitInfo {
        tag: tag.to_string(),
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
