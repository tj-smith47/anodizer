//! Canonical repository-identity resolver.
//!
//! The GitHub/GitLab/Gitea owner + name pair (the repository "slug") is needed
//! by tagging, release, changelog, milestone, container-push, and several
//! publisher paths. Historically each site re-derived it from the git remote
//! with its own `git remote get-url origin` parse — so two sites could disagree
//! (a stale second remote, a detached checkout, a config that hard-codes the
//! repo in one place but not another).
//!
//! [`RepoSlug`] is the single source of truth. Its fields are private and it
//! can only be obtained from a `resolve_*` function, which applies one fixed
//! precedence:
//!
//! 1. an explicit config override (`release.<host>.owner` / `.name`, both
//!    non-empty), else
//! 2. derive once from the `origin` remote.
//!
//! No call site re-parses the remote independently; the [`super::remote`]
//! detectors are crate-private precisely so a future site cannot bypass this
//! resolver.

use anyhow::{Result, bail};
use std::path::Path;

use super::remote::{detect_github_repo_in, detect_owner_repo_in};

/// A validated repository identity: `owner` (user/org, or a GitLab nested
/// `group/subgroup` path) plus the repository `name`.
///
/// Construct only via [`resolve_github_slug_in`], [`resolve_github_slug`],
/// [`resolve_repo_slug_in`], or [`resolve_repo_slug`] — the private fields make
/// ad-hoc construction (and thus a second, divergent derivation of the repo
/// identity) impossible outside this module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoSlug {
    owner: String,
    name: String,
}

impl RepoSlug {
    /// Repository owner — a user/org login, or a GitLab nested group path
    /// (`group/subgroup`).
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// Repository name (the final path segment).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// `owner/name`, the form used in GitHub REST endpoints and clone URLs.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// Validated constructor — module-private so the only public path to a
    /// `RepoSlug` is a `resolve_*` function. Rejects an empty/whitespace owner
    /// or name (an unusable slug must surface as an error at resolution time,
    /// not as a silent `""/""` that 404s a downstream API call).
    fn validated(owner: String, name: String) -> Result<Self> {
        if owner.trim().is_empty() || name.trim().is_empty() {
            bail!(
                "repository slug requires a non-empty owner and name (got {:?}/{:?})",
                owner,
                name
            );
        }
        Ok(Self { owner, name })
    }

    /// Test-only escape hatch for unit tests that need a `RepoSlug` without a
    /// git fixture. Gated so production code cannot fabricate an unvalidated
    /// slug.
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn for_test(owner: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            owner: owner.into(),
            name: name.into(),
        }
    }
}

/// Treat an empty/whitespace override as absent so a blank config field falls
/// through to remote derivation rather than producing an invalid slug.
fn override_pair<'a>(owner: Option<&'a str>, name: Option<&'a str>) -> Option<(&'a str, &'a str)> {
    let owner = owner.filter(|v| !v.trim().is_empty())?;
    let name = name.filter(|v| !v.trim().is_empty())?;
    Some((owner, name))
}

/// Resolve the GitHub repository identity for the repo at `cwd`.
///
/// Precedence: a non-empty `(override_owner, override_name)` config override
/// wins; otherwise the `origin` remote is parsed once (github.com URLs only).
pub fn resolve_github_slug_in(
    override_owner: Option<&str>,
    override_name: Option<&str>,
    cwd: &Path,
) -> Result<RepoSlug> {
    if let Some((owner, name)) = override_pair(override_owner, override_name) {
        return RepoSlug::validated(owner.to_string(), name.to_string());
    }
    let (owner, name) = detect_github_repo_in(cwd)?;
    RepoSlug::validated(owner, name)
}

/// Process-cwd sibling of [`resolve_github_slug_in`].
pub fn resolve_github_slug(
    override_owner: Option<&str>,
    override_name: Option<&str>,
) -> Result<RepoSlug> {
    resolve_github_slug_in(override_owner, override_name, &std::env::current_dir()?)
}

/// Host-agnostic sibling of [`resolve_github_slug_in`] (GitHub, GitLab, Gitea,
/// self-hosted).
///
/// Precedence is identical, but remote derivation uses the host-agnostic parse
/// (so a GitLab nested `group/subgroup/repo` remote yields
/// `owner = "group/subgroup"`).
pub fn resolve_repo_slug_in(
    override_owner: Option<&str>,
    override_name: Option<&str>,
    cwd: &Path,
) -> Result<RepoSlug> {
    if let Some((owner, name)) = override_pair(override_owner, override_name) {
        return RepoSlug::validated(owner.to_string(), name.to_string());
    }
    let (owner, name) = detect_owner_repo_in(cwd)?;
    RepoSlug::validated(owner, name)
}

/// Process-cwd sibling of [`resolve_repo_slug_in`].
pub fn resolve_repo_slug(
    override_owner: Option<&str>,
    override_name: Option<&str>,
) -> Result<RepoSlug> {
    resolve_repo_slug_in(override_owner, override_name, &std::env::current_dir()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let out = crate::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args)
                    .current_dir(dir)
                    .env("GIT_TERMINAL_PROMPT", "0")
                    .env("LC_ALL", "C");
                cmd
            },
            "git",
        );
        assert!(out.status.success(), "git {args:?} failed");
    }

    fn repo_with_origin(url: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-q"]);
        git(tmp.path(), &["remote", "add", "origin", url]);
        tmp
    }

    #[test]
    fn override_wins_over_remote_without_touching_git() {
        // No git repo at all: an override must still resolve (proves the
        // override short-circuits remote derivation entirely).
        let tmp = tempfile::tempdir().unwrap();
        let slug = resolve_github_slug_in(Some("cfg-owner"), Some("cfg-name"), tmp.path()).unwrap();
        assert_eq!(slug.owner(), "cfg-owner");
        assert_eq!(slug.name(), "cfg-name");
        assert_eq!(slug.slug(), "cfg-owner/cfg-name");
    }

    #[test]
    fn empty_override_falls_through_to_remote() {
        let tmp = repo_with_origin("https://github.com/remote-owner/remote-repo.git");
        // Both empty -> remote.
        let slug = resolve_github_slug_in(Some(""), Some("  "), tmp.path()).unwrap();
        assert_eq!(slug.owner(), "remote-owner");
        assert_eq!(slug.name(), "remote-repo");
    }

    #[test]
    fn partial_override_falls_through_to_remote() {
        let tmp = repo_with_origin("https://github.com/remote-owner/remote-repo.git");
        // Only owner set -> not a complete override -> remote wins (no
        // half-config slug like `owner-only/`).
        let slug = resolve_github_slug_in(Some("cfg-owner"), None, tmp.path()).unwrap();
        assert_eq!(slug.owner(), "remote-owner");
        assert_eq!(slug.name(), "remote-repo");
    }

    #[test]
    fn github_remote_derivation() {
        let tmp = repo_with_origin("git@github.com:gh-owner/gh-repo.git");
        let slug = resolve_github_slug_in(None, None, tmp.path()).unwrap();
        assert_eq!(slug.slug(), "gh-owner/gh-repo");
    }

    #[test]
    fn host_agnostic_derivation_preserves_nested_groups() {
        let tmp = repo_with_origin("https://gitlab.com/group/subgroup/repo.git");
        // github-specific parse rejects a non-github host...
        assert!(resolve_github_slug_in(None, None, tmp.path()).is_err());
        // ...but the host-agnostic resolver keeps the nested owner path.
        let slug = resolve_repo_slug_in(None, None, tmp.path()).unwrap();
        assert_eq!(slug.owner(), "group/subgroup");
        assert_eq!(slug.name(), "repo");
    }

    #[test]
    fn missing_remote_is_an_error_not_an_empty_slug() {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-q"]);
        assert!(resolve_github_slug_in(None, None, tmp.path()).is_err());
        assert!(resolve_repo_slug_in(None, None, tmp.path()).is_err());
    }
}
