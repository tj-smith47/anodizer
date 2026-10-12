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

/// The names of every remote configured in the repository at `cwd`, in
/// `git remote` order; empty when there is none or git cannot be asked.
pub fn remote_names_in(cwd: &Path) -> Vec<String> {
    git_output_in(cwd, &["remote"])
        .map(|out| out.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Trim a remote URL down to the path a parser reads: surrounding
/// whitespace, any trailing `/` (git accepts `…/repo.git/` verbatim), then
/// the `.git` suffix. Returns `None` for an empty URL.
fn normalize_remote_url(url: &str) -> Option<&str> {
    let url = url.trim().trim_end_matches('/');
    if url.is_empty() {
        return None;
    }
    Some(url.strip_suffix(".git").unwrap_or(url))
}

/// The repository a remote URL names, as a key on which two spellings of one
/// remote compare equal: `host/path`, with the scheme, any userinfo, an SSH
/// port, the `.git` suffix and a trailing `/` dropped and the host
/// lower-cased. `ssh://aur@aur.archlinux.org/widget.git`,
/// `aur@aur.archlinux.org:widget.git`, `AUR.archlinux.org:widget.git/` and
/// `https://aur.archlinux.org/widget` are one key. A URL in no recognized
/// shape is returned trimmed, so two unparseable spellings still compare by
/// their text; an empty URL is the empty key.
pub fn remote_identity(url: &str) -> String {
    let Some(url) = normalize_remote_url(url) else {
        return String::new();
    };
    if let Some((host, path)) = split_scheme_url(url).or_else(|| split_scp_url(url)) {
        return format!("{}/{}", host.to_ascii_lowercase(), path.trim_matches('/'));
    }
    url.to_string()
}

/// The web host behind an SSH host name. GitHub and GitLab each serve SSH on
/// port 443 from a dedicated name (`ssh.github.com`, `altssh.gitlab.com`) for
/// networks that block port 22; the repository's pages live on the plain
/// host. Any other name is its own web host.
fn ssh_web_host(host: &str) -> &str {
    match host {
        "ssh.github.com" => "github.com",
        "altssh.gitlab.com" => "gitlab.com",
        other => other,
    }
}

/// Drop a trailing `:port` from a URL authority's host. A bracketed IPv6
/// literal (`[::1]:22`) keeps its brackets and the colons inside them.
fn strip_port(host: &str) -> &str {
    if host.starts_with('[') {
        return host.find(']').map_or(host, |end| &host[..=end]);
    }
    host.split(':').next().unwrap_or(host)
}

/// Split a URL into its scheme (`https://`, `http://`, `ssh://`) and the
/// text after it; `None` for a URL with no such scheme.
fn split_scheme(url: &str) -> Option<(&str, &str)> {
    ["https://", "http://", "ssh://"]
        .iter()
        .find_map(|scheme| url.strip_prefix(scheme).map(|rest| (*scheme, rest)))
}

/// The host of a URL's authority segment under `scheme`, any userinfo
/// dropped. An `ssh://` port is the SSH port and is dropped, and the host is
/// mapped through [`ssh_web_host`]; an `http(s)://` port is the web port and
/// stays.
fn authority_host<'a>(scheme: &str, authority: &'a str) -> &'a str {
    let host = authority.rsplit('@').next().unwrap_or(authority);
    if scheme == "ssh://" {
        ssh_web_host(strip_port(host))
    } else {
        host
    }
}

/// Split a URL with a scheme (`https://`, `http://`, `ssh://`) into
/// `(host, path)`, the host read by [`authority_host`]. `None` when the URL
/// has no such scheme, no host or no path.
fn split_scheme_url(url: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = split_scheme(url)?;
    let slash = rest.find('/')?;
    let host = authority_host(scheme, &rest[..slash]);
    let path = &rest[slash + 1..];
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some((host, path))
}

/// Split a scp-style `user@host:path` or `host:path` URL into `(host, path)`,
/// any userinfo dropped and the host mapped through [`ssh_web_host`]. The
/// host is the segment before the first colon when it holds no path
/// separator (which would make it a local path or a Windows drive) and names
/// a host (`@` or `.`); `None` otherwise, or with an empty path.
fn split_scp_url(url: &str) -> Option<(&str, &str)> {
    let (before, path) = url.split_once(':')?;
    if before.contains(['/', '\\'])
        || !(before.contains('@') || before.contains('.'))
        || path.is_empty()
    {
        return None;
    }
    Some((
        ssh_web_host(before.rsplit('@').next().unwrap_or(before)),
        path,
    ))
}

/// The host a remote or API URL names, lower-cased, with any userinfo and
/// any port dropped and the port-443 SSH names mapped through
/// [`ssh_web_host`], so a remote and an API base on one machine compare
/// equal whatever port each uses. The path is not required, so
/// `https://api.github.com/` names `api.github.com`. `None` for a URL with
/// no host: a local path, a Windows drive, an empty string.
pub fn remote_host(url: &str) -> Option<String> {
    let url = url.trim();
    let host = match split_scheme(url) {
        Some((scheme, rest)) => authority_host(scheme, rest.split('/').next().unwrap_or(rest)),
        None => split_scp_url(url)?.0,
    };
    let host = strip_port(host);
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// The host of the `origin` remote configured in `cwd`; `None` when there is
/// no such remote or its URL names no host.
pub fn detect_origin_remote_host_in(cwd: &Path) -> Option<String> {
    git_output_in(cwd, &["remote", "get-url", "origin"])
        .ok()
        .and_then(|url| remote_host(&url))
}

/// Parse owner and repo name from a GitHub remote URL.
/// Supports HTTPS (`https://github.com/owner/repo.git`) and both SSH
/// spellings (`git@github.com:owner/repo.git`,
/// `ssh://git@github.com/owner/repo.git`).
pub(crate) fn parse_github_remote(url: &str) -> Option<(String, String)> {
    let url = normalize_remote_url(url)?;

    // HTTPS / ssh://: scheme, github.com, owner/repo
    let path = match split_scheme_url(url) {
        Some(("github.com", path)) => Some(path),
        Some(_) => None,
        // scp-like SSH: git@github.com:owner/repo
        None => url
            .strip_prefix("git@github.com:")
            .or_else(|| url.strip_prefix("git@ssh.github.com:")),
    };
    let parts: Vec<&str> = path?.splitn(3, '/').collect();
    if parts.len() >= 2 && !parts[0].is_empty() && !parts[1].is_empty() {
        return Some((parts[0].to_string(), parts[1].to_string()));
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
/// Supports HTTPS (`https://host/owner/repo.git`) and both SSH spellings
/// (`git@host:owner/repo.git`, `ssh://git@host/owner/repo.git`). Returns
/// `(owner, repo)` with `.git` suffix stripped; nested groups keep every
/// segment but the last in `owner`.
///
/// This is a host-agnostic version of [`parse_github_remote`], suitable for
/// GitLab, Gitea, and other SCM providers.
pub(crate) fn parse_remote_owner_repo(url: &str) -> Option<(String, String)> {
    let url = normalize_remote_url(url)?;

    // https://host/owner/repo, https://host/group/subgroup/repo, ssh://git@host/owner/repo
    if let Some((_, path)) = split_scheme_url(url) {
        // For nested groups (e.g. group/subgroup/repo), the owner is everything
        // up to the last slash.
        let last_slash = path.rfind('/')?;
        let owner = &path[..last_slash];
        let repo = &path[last_slash + 1..];
        if !owner.is_empty() && !repo.is_empty() {
            return Some((owner.to_string(), repo.to_string()));
        }
        return None;
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
/// Accepts HTTPS (`https://host/owner/repo.git`) and both SSH spellings
/// (`git@host:owner/repo.git`, `ssh://git@host/owner/repo.git`), normalizes
/// each to `https://host/owner/repo` (no `.git` suffix), and preserves
/// nested groups (`group/subgroup/repo`). Returns `None` when the URL has no
/// recognizable host or path.
///
/// This is the host-preserving counterpart of `parse_remote_owner_repo`:
/// it keeps the host so callers (e.g. changelog compare-link footers) can
/// build links against a self-hosted GitLab/Gitea instead of assuming
/// `github.com`.
pub fn parse_remote_web_base(url: &str) -> Option<String> {
    let url = normalize_remote_url(url)?;

    // https://, http://, ssh://: the scheme becomes https and userinfo is
    // dropped. An ssh:// port is dropped with it; an http(s):// port is the
    // web port and stays in the base.
    if let Some((host, path)) = split_scheme_url(url) {
        return Some(format!("https://{}/{}", host, path));
    }

    // SSH: git@host:owner/repo
    if let Some(colon_pos) = url.find(':') {
        let before_colon = &url[..colon_pos];
        if before_colon.contains('@') && !before_colon.contains("//") {
            let host = ssh_web_host(before_colon.rsplit('@').next().unwrap_or(before_colon));
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
/// Uses `parse_remote_owner_repo` which works with any git hosting provider
/// (GitHub, GitLab, Gitea, etc.).
pub(crate) fn detect_owner_repo_in(cwd: &Path) -> Result<(String, String)> {
    let url = git_output_in(cwd, &["remote", "get-url", "origin"])?;
    parse_remote_owner_repo(&url).ok_or_else(|| {
        // Strip inline userinfo before surfacing the URL.
        let safe = redact_url_credentials(&url);
        anyhow::anyhow!("could not parse owner/repo from remote URL: {}", safe)
    })
}
