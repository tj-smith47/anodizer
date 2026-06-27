use anyhow::Result;
use std::path::Path;
use std::process::Command;

use super::git_output_in;
use crate::redact::redact_url_credentials;

/// Whether `remote` (e.g. `"origin"`) is configured in the git repo at `cwd`.
///
/// Probes `git remote get-url <remote>` and reports success. `GIT_TERMINAL_PROMPT=0`
/// prevents the call from blocking on a credential prompt; `LC_ALL=C` pins
/// machine-readable output. Any spawn or non-zero exit (no such remote) maps to
/// `false` so callers can branch on presence without surfacing an error.
pub fn has_remote_in(cwd: &Path, remote: &str) -> bool {
    Command::new("git")
        .current_dir(cwd)
        .args(["remote", "get-url", remote])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Parse owner and repo name from a GitHub remote URL.
/// Supports HTTPS (`https://github.com/owner/repo.git`) and SSH (`git@github.com:owner/repo.git`).
pub(crate) fn parse_github_remote(url: &str) -> Option<(String, String)> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }

    // Strip trailing ".git" if present
    let url = url.strip_suffix(".git").unwrap_or(url);

    // HTTPS: https://github.com/owner/repo
    if let Some(path) = url.strip_prefix("https://github.com/") {
        let parts: Vec<&str> = path.splitn(3, '/').collect();
        if parts.len() >= 2 && !parts[0].is_empty() && !parts[1].is_empty() {
            return Some((parts[0].to_string(), parts[1].to_string()));
        }
    }

    // SSH: git@github.com:owner/repo
    if let Some(path) = url.strip_prefix("git@github.com:") {
        let parts: Vec<&str> = path.splitn(3, '/').collect();
        if parts.len() >= 2 && !parts[0].is_empty() && !parts[1].is_empty() {
            return Some((parts[0].to_string(), parts[1].to_string()));
        }
    }

    None
}

/// Get the GitHub owner/name from the `origin` remote configured in `cwd`.
///
/// Runs `git remote get-url origin` with an explicit `current_dir` so callers
/// (including tests against a temporary fixture repo) don't have to
/// mutate the process-wide cwd.
pub(crate) fn detect_github_repo_in(cwd: &Path) -> Result<(String, String)> {
    let url = git_output_in(cwd, &["remote", "get-url", "origin"])?;
    parse_github_remote(&url).ok_or_else(|| {
        // Strip inline `<scheme>://<userinfo>@...` userinfo before surfacing
        // the URL in a user-visible error.
        let safe = redact_url_credentials(&url);
        anyhow::anyhow!(
            "could not parse GitHub owner/repo from remote URL: {}",
            safe
        )
    })
}

/// Parse owner and repo from any git remote URL, regardless of host.
///
/// Supports HTTPS (`https://host/owner/repo.git`) and SSH (`git@host:owner/repo.git`)
/// formats. Returns `(owner, repo)` with `.git` suffix stripped.
///
/// This is a host-agnostic version of [`parse_github_remote`], suitable for
/// GitLab, Gitea, and other SCM providers.
pub(crate) fn parse_remote_owner_repo(url: &str) -> Option<(String, String)> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }

    // Strip trailing ".git" if present
    let url = url.strip_suffix(".git").unwrap_or(url);

    // HTTPS: https://host/owner/repo or https://host/group/subgroup/repo
    if url.starts_with("https://") || url.starts_with("http://") {
        // Strip scheme and host
        let after_scheme = if let Some(rest) = url.strip_prefix("https://") {
            rest
        } else {
            url.strip_prefix("http://")?
        };
        // Strip any credentials (user:pass@host or user@host)
        let after_host = after_scheme.find('/').map(|i| &after_scheme[i + 1..])?;
        // For nested groups (e.g. group/subgroup/repo), the owner is everything
        // up to the last slash.
        let last_slash = after_host.rfind('/')?;
        let owner = &after_host[..last_slash];
        let repo = &after_host[last_slash + 1..];
        if !owner.is_empty() && !repo.is_empty() {
            return Some((owner.to_string(), repo.to_string()));
        }
    }

    // SSH: git@host:owner/repo or git@host:group/subgroup/repo
    if let Some(colon_pos) = url.find(':') {
        let before_colon = &url[..colon_pos];
        // Ensure it looks like an SSH URL (contains @, no //)
        if before_colon.contains('@') && !before_colon.contains("//") {
            let path = &url[colon_pos + 1..];
            let last_slash = path.rfind('/')?;
            let owner = &path[..last_slash];
            let repo = &path[last_slash + 1..];
            if !owner.is_empty() && !repo.is_empty() {
                return Some((owner.to_string(), repo.to_string()));
            }
        }
    }

    None
}

/// Convert a git remote URL into its web base (`https://host/owner/repo`),
/// regardless of SCM host.
///
/// Accepts HTTPS (`https://host/owner/repo.git`) and SSH
/// (`git@host:owner/repo.git`) forms, normalizes both to
/// `https://host/owner/repo` (no `.git` suffix), and preserves nested
/// groups (`group/subgroup/repo`). Returns `None` when the URL has no
/// recognizable host or path.
///
/// This is the host-preserving counterpart of [`parse_remote_owner_repo`]:
/// it keeps the host so callers (e.g. changelog compare-link footers) can
/// build links against a self-hosted GitLab/Gitea instead of assuming
/// `github.com`.
pub fn parse_remote_web_base(url: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    let url = url.strip_suffix(".git").unwrap_or(url);

    // HTTPS/HTTP: normalize the scheme to https and drop any userinfo.
    if let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    {
        // Split host[:port]/path; drop credentials in the host segment.
        let slash = rest.find('/')?;
        let host_seg = &rest[..slash];
        let path = &rest[slash + 1..];
        let host = host_seg.rsplit('@').next().unwrap_or(host_seg);
        if host.is_empty() || path.is_empty() {
            return None;
        }
        return Some(format!("https://{}/{}", host, path));
    }

    // SSH: git@host:owner/repo
    if let Some(colon_pos) = url.find(':') {
        let before_colon = &url[..colon_pos];
        if before_colon.contains('@') && !before_colon.contains("//") {
            let host = before_colon.rsplit('@').next().unwrap_or(before_colon);
            let path = &url[colon_pos + 1..];
            if !host.is_empty() && !path.is_empty() {
                return Some(format!("https://{}/{}", host, path));
            }
        }
    }

    None
}

/// Get the web base (`https://host/owner/repo`) for the `origin` remote
/// configured in `cwd`, regardless of SCM host.
///
/// Path-taking helper used to build host-correct compare links (changelog
/// footers) for self-hosted GitLab/Gitea as well as github.com.
pub fn detect_remote_web_base_in(cwd: &Path) -> Result<String> {
    let url = git_output_in(cwd, &["remote", "get-url", "origin"])?;
    parse_remote_web_base(&url).ok_or_else(|| {
        let safe = redact_url_credentials(&url);
        anyhow::anyhow!("could not parse web base from remote URL: {}", safe)
    })
}

/// Get the owner/repo from the `origin` remote configured in `cwd`,
/// regardless of SCM host.
///
/// Uses [`parse_remote_owner_repo`] which works with any git hosting provider
/// (GitHub, GitLab, Gitea, etc.).
pub(crate) fn detect_owner_repo_in(cwd: &Path) -> Result<(String, String)> {
    let url = git_output_in(cwd, &["remote", "get-url", "origin"])?;
    parse_remote_owner_repo(&url).ok_or_else(|| {
        // Strip inline userinfo before surfacing the URL.
        let safe = redact_url_credentials(&url);
        anyhow::anyhow!("could not parse owner/repo from remote URL: {}", safe)
    })
}
