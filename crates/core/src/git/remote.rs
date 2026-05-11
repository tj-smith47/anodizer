use anyhow::Result;

use super::git_output;

/// Strip userinfo (credentials) from an HTTPS URL.
///
/// If the URL starts with `https://` and contains `@`, everything between
/// `://` and `@` is removed (e.g. `https://user:token@github.com/...` becomes
/// `https://github.com/...`). Non-HTTPS URLs are returned unchanged.
pub(super) fn strip_url_credentials(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("https://")
        && let Some(at_pos) = rest.find('@')
    {
        return format!("https://{}", &rest[at_pos + 1..]);
    }
    url.to_string()
}

/// Parse owner and repo name from a GitHub remote URL.
/// Supports HTTPS (`https://github.com/owner/repo.git`) and SSH (`git@github.com:owner/repo.git`).
pub fn parse_github_remote(url: &str) -> Option<(String, String)> {
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

/// Get the GitHub owner/name from the `origin` remote.
pub fn detect_github_repo() -> Result<(String, String)> {
    let url = git_output(&["remote", "get-url", "origin"])?;
    parse_github_remote(&url).ok_or_else(|| {
        // P7.4: strip inline `https://<token>@...` userinfo before surfacing
        // the URL in a user-visible error.
        let safe = strip_url_credentials(&url);
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
pub fn parse_remote_owner_repo(url: &str) -> Option<(String, String)> {
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

/// Get the owner/repo from the `origin` remote, regardless of SCM host.
///
/// Uses [`parse_remote_owner_repo`] which works with any git hosting provider
/// (GitHub, GitLab, Gitea, etc.).
pub fn detect_owner_repo() -> Result<(String, String)> {
    let url = git_output(&["remote", "get-url", "origin"])?;
    parse_remote_owner_repo(&url).ok_or_else(|| {
        // P7.4: strip inline userinfo before surfacing the URL.
        let safe = strip_url_credentials(&url);
        anyhow::anyhow!("could not parse owner/repo from remote URL: {}", safe)
    })
}
