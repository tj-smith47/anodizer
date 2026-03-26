use anyhow::{Result, bail};
use regex::Regex;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct SemVer {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub prerelease: Option<String>,
}

impl SemVer {
    pub fn is_prerelease(&self) -> bool {
        self.prerelease.is_some()
    }
}

/// Parse a semver version from a tag string like "v1.2.3", "v1.0.0-rc.1", or "cfgd-core-v2.1.0"
pub fn parse_semver(tag: &str) -> Result<SemVer> {
    let re = Regex::new(r"v?(\d+)\.(\d+)\.(\d+)(?:-(.+))?$")?;
    let caps = re
        .captures(tag)
        .ok_or_else(|| anyhow::anyhow!("not a valid semver tag: {}", tag))?;
    Ok(SemVer {
        major: caps[1].parse()?,
        minor: caps[2].parse()?,
        patch: caps[3].parse()?,
        prerelease: caps.get(4).map(|m| m.as_str().to_string()),
    })
}

#[derive(Debug, Clone)]
pub struct GitInfo {
    pub tag: String,
    pub commit: String,
    pub short_commit: String,
    pub branch: String,
    pub dirty: bool,
    pub semver: SemVer,
    /// ISO 8601 author date of HEAD commit (from `git log -1 --format=%aI`)
    pub commit_date: String,
    /// Unix timestamp of HEAD commit (from `git log -1 --format=%at`)
    pub commit_timestamp: String,
    /// Previous tag matching the same pattern, if any.
    /// Populated externally by the release command once the tag_template is known.
    pub previous_tag: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Commit {
    pub hash: String,
    pub short_hash: String,
    pub message: String,
}

/// Run a git command and return stdout, trimmed.
fn git_output(args: &[&str]) -> Result<String> {
    let output = Command::new("git").args(args).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Detect git info for a given tag.
pub fn detect_git_info(tag: &str) -> Result<GitInfo> {
    let commit = git_output(&["rev-parse", "HEAD"])?;
    let short_commit = git_output(&["rev-parse", "--short", "HEAD"])?;
    let branch = git_output(&["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    let dirty = !git_output(&["status", "--porcelain"])?.is_empty();
    let commit_date = git_output(&["log", "-1", "--format=%aI"]).unwrap_or_default();
    let commit_timestamp = git_output(&["log", "-1", "--format=%at"]).unwrap_or_default();
    let semver = parse_semver(tag)?;
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
    })
}

/// Find the latest tag matching a template pattern.
/// E.g., tag_template "cfgd-core-v{{ .Version }}" → matches tags like "cfgd-core-v1.2.3"
pub fn find_latest_tag_matching(tag_template: &str) -> Result<Option<String>> {
    let pattern = tag_template
        .replace("{{ .Version }}", r"\d+\.\d+\.\d+(?:-.+)?")
        .replace("{{.Version}}", r"\d+\.\d+\.\d+(?:-.+)?")
        .replace("{{ Version }}", r"\d+\.\d+\.\d+(?:-.+)?")
        .replace("{{Version}}", r"\d+\.\d+\.\d+(?:-.+)?");
    let re = Regex::new(&format!("^{}$", pattern))?;

    let tags_output = git_output(&["tag", "--list"])?;
    if tags_output.is_empty() {
        return Ok(None);
    }

    let mut matching: Vec<(SemVer, String)> = tags_output
        .lines()
        .filter(|t| re.is_match(t))
        .filter_map(|t| parse_semver(t).ok().map(|v| (v, t.to_string())))
        .collect();

    matching.sort_by(|a, b| {
        a.0.major
            .cmp(&b.0.major)
            .then(a.0.minor.cmp(&b.0.minor))
            .then(a.0.patch.cmp(&b.0.patch))
            .then(match (&a.0.prerelease, &b.0.prerelease) {
                (Some(_), None) => std::cmp::Ordering::Less, // prerelease < release
                (None, Some(_)) => std::cmp::Ordering::Greater, // release > prerelease
                _ => std::cmp::Ordering::Equal,
            })
    });

    Ok(matching.last().map(|(_, tag)| tag.clone()))
}

/// Get commits between two refs, optionally filtered to a path.
pub fn get_commits_between(from: &str, to: &str, path_filter: Option<&str>) -> Result<Vec<Commit>> {
    let range = format!("{}..{}", from, to);
    let mut args = vec!["log", "--pretty=format:%H%n%h%n%s", &range];
    if let Some(path) = path_filter {
        args.push("--");
        args.push(path);
    }
    let output = git_output(&args)?;
    if output.is_empty() {
        return Ok(vec![]);
    }
    let lines: Vec<&str> = output.lines().collect();
    let mut commits = vec![];
    for chunk in lines.chunks(3) {
        if chunk.len() == 3 {
            commits.push(Commit {
                hash: chunk[0].to_string(),
                short_hash: chunk[1].to_string(),
                message: chunk[2].to_string(),
            });
        }
    }
    Ok(commits)
}

/// Get all commits reachable from HEAD, optionally filtered to a path.
/// Used for initial releases where there is no previous tag.
pub fn get_all_commits(path_filter: Option<&str>) -> Result<Vec<Commit>> {
    let mut args = vec!["log", "--pretty=format:%H%n%h%n%s", "HEAD"];
    if let Some(path) = path_filter {
        args.push("--");
        args.push(path);
    }
    let output = git_output(&args)?;
    if output.is_empty() {
        return Ok(vec![]);
    }
    let lines: Vec<&str> = output.lines().collect();
    let mut commits = vec![];
    for chunk in lines.chunks(3) {
        if chunk.len() == 3 {
            commits.push(Commit {
                hash: chunk[0].to_string(),
                short_hash: chunk[1].to_string(),
                message: chunk[2].to_string(),
            });
        }
    }
    Ok(commits)
}

/// Check if there are changes in a path since a given tag.
pub fn has_changes_since(tag: &str, path: &str) -> Result<bool> {
    let output = git_output(&["diff", "--name-only", &format!("{}..HEAD", tag), "--", path])?;
    Ok(!output.is_empty())
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
        anyhow::anyhow!("could not parse GitHub owner/repo from remote URL: {}", url)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_semver() {
        let v = parse_semver("v1.2.3").unwrap();
        assert_eq!(v.major, 1);
        assert_eq!(v.minor, 2);
        assert_eq!(v.patch, 3);
        assert_eq!(v.prerelease, None);
    }

    #[test]
    fn test_parse_semver_prerelease() {
        let v = parse_semver("v1.0.0-rc.1").unwrap();
        assert_eq!(v.major, 1);
        assert_eq!(v.prerelease, Some("rc.1".to_string()));
    }

    #[test]
    fn test_parse_semver_with_prefix() {
        let v = parse_semver("cfgd-core-v2.1.0").unwrap();
        assert_eq!(v.major, 2);
        assert_eq!(v.minor, 1);
    }

    #[test]
    fn test_is_prerelease() {
        assert!(parse_semver("v1.0.0-rc.1").unwrap().is_prerelease());
        assert!(!parse_semver("v1.0.0").unwrap().is_prerelease());
    }

    #[test]
    fn test_parse_github_remote_https() {
        let result = parse_github_remote("https://github.com/tj-smith47/anodize.git");
        assert_eq!(
            result,
            Some(("tj-smith47".to_string(), "anodize".to_string()))
        );
    }

    #[test]
    fn test_parse_github_remote_https_no_dotgit() {
        let result = parse_github_remote("https://github.com/owner/repo");
        assert_eq!(result, Some(("owner".to_string(), "repo".to_string())));
    }

    #[test]
    fn test_parse_github_remote_ssh() {
        let result = parse_github_remote("git@github.com:owner/repo.git");
        assert_eq!(result, Some(("owner".to_string(), "repo".to_string())));
    }

    #[test]
    fn test_parse_github_remote_ssh_no_dotgit() {
        let result = parse_github_remote("git@github.com:owner/repo");
        assert_eq!(result, Some(("owner".to_string(), "repo".to_string())));
    }

    #[test]
    fn test_parse_github_remote_invalid() {
        let result = parse_github_remote("https://gitlab.com/foo/bar.git");
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_github_remote_empty() {
        let result = parse_github_remote("");
        assert_eq!(result, None);
    }
}
