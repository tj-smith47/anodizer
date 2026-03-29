use anyhow::{Context as _, Result, bail};
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

impl PartialEq for SemVer {
    fn eq(&self, other: &Self) -> bool {
        self.major == other.major
            && self.minor == other.minor
            && self.patch == other.patch
            && self.prerelease == other.prerelease
    }
}

impl Eq for SemVer {}

impl PartialOrd for SemVer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SemVer {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.major
            .cmp(&other.major)
            .then(self.minor.cmp(&other.minor))
            .then(self.patch.cmp(&other.patch))
            .then(match (&self.prerelease, &other.prerelease) {
                (Some(_), None) => std::cmp::Ordering::Less, // prerelease < release
                (None, Some(_)) => std::cmp::Ordering::Greater, // release > prerelease
                (Some(a), Some(b)) => a.cmp(b),
                (None, None) => std::cmp::Ordering::Equal,
            })
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
}

#[derive(Debug, Clone)]
pub struct Commit {
    pub hash: String,
    pub short_hash: String,
    pub message: String,
    pub author_name: String,
    pub author_email: String,
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

/// Check whether the working tree has uncommitted changes.
pub fn is_git_dirty() -> bool {
    git_output(&["status", "--porcelain"])
        .map(|s| !s.is_empty())
        .unwrap_or(false)
}

/// Detect git info for a given tag.
pub fn detect_git_info(tag: &str) -> Result<GitInfo> {
    let commit = git_output(&["rev-parse", "HEAD"])?;
    let short_commit = git_output(&["rev-parse", "--short", "HEAD"])?;
    let branch = git_output(&["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    let dirty = is_git_dirty();
    let commit_date = git_output(&["log", "-1", "--format=%aI"]).unwrap_or_default();
    let commit_timestamp = git_output(&["log", "-1", "--format=%at"]).unwrap_or_default();
    let remote_url = git_output(&["remote", "get-url", "origin"]).unwrap_or_default();
    let summary = git_output(&["describe", "--tags", "--always"]).unwrap_or_default();

    // Try annotated tag message fields first; fall back to commit message fields.
    let tag_subject = git_output(&["tag", "-l", "--format=%(subject)", tag])
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| git_output(&["log", "-1", "--format=%s"]).unwrap_or_default());
    let tag_contents = git_output(&["tag", "-l", "--format=%(contents)", tag])
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| git_output(&["log", "-1", "--format=%B"]).unwrap_or_default());
    let tag_body = git_output(&["tag", "-l", "--format=%(body)", tag])
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| git_output(&["log", "-1", "--format=%b"]).unwrap_or_default());

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
        remote_url,
        summary,
        tag_subject,
        tag_contents,
        tag_body,
    })
}

/// The four accepted placeholder forms for the version variable in tag templates.
const VERSION_PLACEHOLDERS: &[&str] = &[
    "{{ .Version }}",
    "{{.Version}}",
    "{{ Version }}",
    "{{Version}}",
];

/// Check whether a tag template string contains any recognised version placeholder.
pub fn has_version_placeholder(template: &str) -> bool {
    VERSION_PLACEHOLDERS.iter().any(|p| template.contains(p))
}

/// Extract the prefix portion of a tag template by locating the version placeholder.
///
/// Returns the substring before the first recognised placeholder, or `None` if no
/// placeholder is found.
pub fn extract_tag_prefix(template: &str) -> Option<String> {
    for ph in VERSION_PLACEHOLDERS {
        if let Some(idx) = template.find(ph) {
            return Some(template[..idx].to_string());
        }
    }
    None
}

/// Find the latest tag matching a template pattern.
/// E.g., tag_template "cfgd-core-v{{ .Version }}" → matches tags like "cfgd-core-v1.2.3"
pub fn find_latest_tag_matching(tag_template: &str) -> Result<Option<String>> {
    // Replace version placeholders with a sentinel, regex-escape everything
    // else, then swap the sentinel back to the version regex pattern.
    // This prevents regex metacharacters in the prefix (e.g. dots in
    // project names) from being interpreted as regex operators.
    const SENTINEL: &str = "\x00VERSION_PLACEHOLDER\x00";
    let mut tmp = tag_template.to_string();
    for placeholder in VERSION_PLACEHOLDERS {
        tmp = tmp.replace(placeholder, SENTINEL);
    }
    let escaped = regex::escape(&tmp);
    let pattern = escaped.replace(SENTINEL, r"\d+\.\d+\.\d+(?:-.+)?");
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

    matching.sort_by(|a, b| a.0.cmp(&b.0));

    Ok(matching.last().map(|(_, tag)| tag.clone()))
}

/// Parse git log output (formatted as `%H%n%h%n%s%n%an%n%ae`) into a vec of [`Commit`]s.
fn parse_commit_output(output: &str) -> Vec<Commit> {
    if output.is_empty() {
        return vec![];
    }
    let lines: Vec<&str> = output.lines().collect();
    lines
        .chunks(5)
        .filter_map(|chunk| {
            if chunk.len() == 5 {
                Some(Commit {
                    hash: chunk[0].to_string(),
                    short_hash: chunk[1].to_string(),
                    message: chunk[2].to_string(),
                    author_name: chunk[3].to_string(),
                    author_email: chunk[4].to_string(),
                })
            } else {
                None
            }
        })
        .collect()
}

/// Get commits between two refs, optionally filtered to a path.
pub fn get_commits_between(from: &str, to: &str, path_filter: Option<&str>) -> Result<Vec<Commit>> {
    let range = format!("{}..{}", from, to);
    let mut args = vec!["log", "--pretty=format:%H%n%h%n%s%n%an%n%ae", &range];
    if let Some(path) = path_filter {
        args.push("--");
        args.push(path);
    }
    let output = git_output(&args)?;
    Ok(parse_commit_output(&output))
}

/// Get all commits reachable from HEAD, optionally filtered to a path.
/// Used for initial releases where there is no previous tag.
pub fn get_all_commits(path_filter: Option<&str>) -> Result<Vec<Commit>> {
    let mut args = vec!["log", "--pretty=format:%H%n%h%n%s%n%an%n%ae", "HEAD"];
    if let Some(path) = path_filter {
        args.push("--");
        args.push(path);
    }
    let output = git_output(&args)?;
    Ok(parse_commit_output(&output))
}

/// Collect semver tags from the output of the given `git` arguments, filtered
/// by `prefix` and sorted descending by version.
fn collect_semver_tags(git_args: &[&str], prefix: &str) -> Result<Vec<String>> {
    let tags_output = git_output(git_args)?;
    if tags_output.is_empty() {
        return Ok(vec![]);
    }
    let mut matching: Vec<(SemVer, String)> = tags_output
        .lines()
        .filter(|t| t.starts_with(prefix))
        .filter_map(|t| parse_semver(t).ok().map(|v| (v, t.to_string())))
        .collect();
    matching.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(matching.into_iter().map(|(_, tag)| tag).collect())
}

/// Get all semver tags in the repo, sorted descending by version.
/// Prerelease tags sort after release tags of the same major.minor.patch.
pub fn get_all_semver_tags(prefix: &str) -> Result<Vec<String>> {
    collect_semver_tags(&["tag", "--list"], prefix)
}

/// Get semver tags reachable from HEAD, sorted descending by version.
/// Prerelease tags sort after release tags of the same major.minor.patch.
pub fn get_branch_semver_tags(prefix: &str) -> Result<Vec<String>> {
    collect_semver_tags(&["tag", "--merged", "HEAD", "--list"], prefix)
}

/// Create an annotated tag and push it if an `origin` remote exists.
pub fn create_and_push_tag(
    tag: &str,
    message: &str,
    dry_run: bool,
    log: &crate::log::StageLogger,
) -> Result<()> {
    if dry_run {
        log.status(&format!(
            "(dry-run) would create tag: {} (\"{}\")",
            tag, message
        ));
        return Ok(());
    }
    git_output(&["tag", "-a", tag, "-m", message])?;

    let has_remote = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if has_remote {
        git_output(&["push", "origin", tag])?;
    } else {
        log.warn("no 'origin' remote found, skipping push");
    }
    Ok(())
}

/// POST a JSON body to a GitHub API endpoint via the `gh` CLI.
///
/// Returns the parsed JSON response on success. The caller is responsible for
/// ensuring that `gh` is installed and authenticated.
fn gh_api_post(endpoint: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
    use std::io::Write;

    let body_str = serde_json::to_string(body)?;

    let mut child = Command::new("gh")
        .args(["api", "--method", "POST", endpoint, "--input", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn gh CLI")?;

    if let Some(ref mut stdin) = child.stdin {
        stdin.write_all(body_str.as_bytes())?;
    }
    child.stdin.take(); // close stdin

    let output = child.wait_with_output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("gh api POST {} failed: {}", endpoint, stderr.trim());
    }

    let response: serde_json::Value = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("failed to parse GitHub API response from {}", endpoint))?;
    Ok(response)
}

/// Create a tag via the GitHub API (using the `gh` CLI).
///
/// This avoids the need for local git push access. Requires the `gh` CLI to be
/// installed and authenticated (`gh auth login`). The GitHub API creates a
/// lightweight tag object pointing at the HEAD commit on the default branch.
///
/// Falls back to [`create_and_push_tag`] if `gh` is not available.
pub fn create_tag_via_github_api(
    tag: &str,
    message: &str,
    dry_run: bool,
    log: &crate::log::StageLogger,
) -> Result<()> {
    if dry_run {
        log.status(&format!(
            "(dry-run) would create tag via GitHub API: {} (\"{}\")",
            tag, message
        ));
        return Ok(());
    }

    // Detect owner/repo from the origin remote.
    let (owner, repo) = detect_github_repo()?;

    // Get the current HEAD SHA to point the tag at.
    let sha = git_output(&["rev-parse", "HEAD"])?;

    // Step 1: Create the tag object
    let body = serde_json::json!({
        "tag": tag,
        "message": message,
        "object": sha,
        "type": "commit",
        "tagger": {
            "name": git_output(&["config", "user.name"]).unwrap_or_else(|_| "anodize".to_string()),
            "email": git_output(&["config", "user.email"]).unwrap_or_else(|_| "anodize@users.noreply.github.com".to_string()),
            "date": chrono::Utc::now().to_rfc3339(),
        }
    });

    let tag_endpoint = format!("/repos/{owner}/{repo}/git/tags");
    let response = match gh_api_post(&tag_endpoint, &body) {
        Ok(resp) => resp,
        Err(e) => {
            if e.to_string().contains("failed to spawn gh CLI") {
                log.warn("gh CLI not found, falling back to local git tag + push");
                return create_and_push_tag(tag, message, dry_run, log);
            }
            return Err(e);
        }
    };

    let tag_sha = response["sha"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("GitHub API response missing 'sha' field"))?;

    // Step 2: Create the ref pointing to the tag object
    let ref_body = serde_json::json!({
        "ref": format!("refs/tags/{}", tag),
        "sha": tag_sha,
    });

    let ref_endpoint = format!("/repos/{owner}/{repo}/git/refs");
    gh_api_post(&ref_endpoint, &ref_body)?;

    Ok(())
}

/// Get last N commit subjects.
pub fn get_last_commit_messages(count: usize) -> Result<Vec<String>> {
    let output = git_output(&["log", &format!("-{count}"), "--pretty=format:%s"])?;
    Ok(output.lines().map(str::to_string).collect())
}

/// Get commit subjects between two refs.
pub fn get_commit_messages_between(from: &str, to: &str) -> Result<Vec<String>> {
    let output = git_output(&["log", "--pretty=format:%s", &format!("{from}..{to}")])?;
    Ok(output.lines().map(str::to_string).collect())
}

/// Get the current branch name.
pub fn get_current_branch() -> Result<String> {
    git_output(&["rev-parse", "--abbrev-ref", "HEAD"])
}

/// Check if there are any commits since a given tag.
pub fn has_commits_since_tag(tag: &str) -> Result<bool> {
    let range = format!("{}..HEAD", tag);
    let output = git_output(&["log", "--oneline", &range])?;
    Ok(!output.is_empty())
}

/// Get the short commit hash of HEAD.
pub fn get_short_commit() -> Result<String> {
    git_output(&["rev-parse", "--short", "HEAD"])
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
