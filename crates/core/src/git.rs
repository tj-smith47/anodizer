use anyhow::{Context as _, Result, bail};
use regex::Regex;
use std::process::Command;
use std::sync::LazyLock;

use crate::config::GitConfig;
use crate::template::TemplateVars;

/// Render ignore patterns (both `ignore_tags` and `ignore_tag_prefixes`) through
/// the template engine when `template_vars` is provided.
///
/// Returns two vecs: `(rendered_ignore_tags, rendered_ignore_tag_prefixes)`.
/// When `vars` is `None`, patterns are returned as-is (unrendered).
pub fn render_ignore_patterns(
    git_config: Option<&GitConfig>,
    vars: Option<&TemplateVars>,
) -> (Vec<String>, Vec<String>) {
    let rendered_tags: Vec<String> = git_config
        .and_then(|gc| gc.ignore_tags.as_ref())
        .map(|v| {
            v.iter()
                .map(|s| {
                    if let Some(tv) = vars {
                        crate::template::render(s, tv).unwrap_or_else(|_| s.clone())
                    } else {
                        s.clone()
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let rendered_prefixes: Vec<String> = git_config
        .and_then(|gc| gc.ignore_tag_prefixes.as_ref())
        .map(|v| {
            v.iter()
                .map(|s| {
                    if let Some(tv) = vars {
                        crate::template::render(s, tv).unwrap_or_else(|_| s.clone())
                    } else {
                        s.clone()
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    (rendered_tags, rendered_prefixes)
}

#[derive(Debug, Clone)]
pub struct SemVer {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub prerelease: Option<String>,
    pub build_metadata: Option<String>,
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
                (Some(a), Some(b)) => compare_prerelease(a, b),
                (None, None) => std::cmp::Ordering::Equal,
            })
    }
}

/// Compare two prerelease strings per SemVer 2.0.0 section 11.
///
/// Dot-separated identifiers are compared individually: numeric identifiers are
/// compared as integers, alphanumeric identifiers are compared lexicographically,
/// and numeric identifiers always have lower precedence than alphanumeric ones.
/// A shorter set of identifiers has lower precedence when all preceding
/// identifiers are equal.
fn compare_prerelease(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    let a_ids: Vec<&str> = a.split('.').collect();
    let b_ids: Vec<&str> = b.split('.').collect();

    for (ai, bi) in a_ids.iter().zip(b_ids.iter()) {
        let ord = match (ai.parse::<u64>(), bi.parse::<u64>()) {
            (Ok(an), Ok(bn)) => an.cmp(&bn), // both numeric: compare as integers
            (Ok(_), Err(_)) => Ordering::Less, // numeric < alphanumeric
            (Err(_), Ok(_)) => Ordering::Greater, // alphanumeric > numeric
            (Err(_), Err(_)) => ai.cmp(bi),  // both alpha: lexicographic
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    // Shorter set has lower precedence
    a_ids.len().cmp(&b_ids.len())
}

/// Compiled once and reused across all calls to [`parse_semver`].
///
/// Captures: 1=major, 2=minor, 3=patch, 4=prerelease (optional), 5=build metadata (optional).
/// Prerelease is after `-` but before `+`. Build metadata is after `+`.
static SEMVER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^v?(\d+)\.(\d+)\.(\d+)(?:-([^+]+))?(?:\+(.+))?$").unwrap());

/// Parse a strict semver version from a string like "v1.2.3", "1.2.3", "v1.0.0-rc.1",
/// "v1.0.0+build.42", or "v1.0.0-rc.1+build.42".
///
/// The string must start with an optional `v` prefix followed by the version.
/// For prefixed tags like "cfgd-core-v2.1.0", use [`parse_semver_tag`] instead.
pub fn parse_semver(tag: &str) -> Result<SemVer> {
    let caps = SEMVER_RE
        .captures(tag)
        .ok_or_else(|| anyhow::anyhow!("not a valid semver tag: {}", tag))?;
    Ok(SemVer {
        major: caps[1].parse()?,
        minor: caps[2].parse()?,
        patch: caps[3].parse()?,
        prerelease: caps.get(4).map(|m| m.as_str().to_string()),
        build_metadata: caps.get(5).map(|m| m.as_str().to_string()),
    })
}

/// Parse a semver version from a prefixed tag string.
///
/// Strips everything up to and including the last `-` or `_` before the version
/// portion, then delegates to [`parse_semver`]. Handles tags like
/// "cfgd-core-v2.1.0", "my_project-v1.0.0-rc.1", or plain "v1.2.3".
pub fn parse_semver_tag(tag: &str) -> Result<SemVer> {
    // Try strict parse first (handles "v1.2.3" and "1.2.3")
    if let Ok(sv) = parse_semver(tag) {
        return Ok(sv);
    }
    // Find the version portion: look for `v?\d+.\d+.\d+` after a separator
    static PREFIX_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[-_/](v?\d+\.\d+\.\d+(?:-[^+]+)?(?:\+.+)?)$").unwrap());
    if let Some(caps) = PREFIX_RE.captures(tag) {
        return parse_semver(&caps[1]);
    }
    anyhow::bail!("not a valid semver tag: {}", tag)
}

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

#[derive(Debug, Clone)]
pub struct Commit {
    pub hash: String,
    pub short_hash: String,
    pub message: String,
    pub author_name: String,
    pub author_email: String,
    /// Full commit message body (everything after the subject line).
    /// Contains trailers like `Co-Authored-By:`.
    pub body: String,
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

/// Strip userinfo (credentials) from an HTTPS URL.
///
/// If the URL starts with `https://` and contains `@`, everything between
/// `://` and `@` is removed (e.g. `https://user:token@github.com/...` becomes
/// `https://github.com/...`). Non-HTTPS URLs are returned unchanged.
fn strip_url_credentials(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("https://")
        && let Some(at_pos) = rest.find('@')
    {
        return format!("https://{}", &rest[at_pos + 1..]);
    }
    url.to_string()
}

/// Detect git info for a given tag.
///
/// When `skip_validate` is true and the tag is not valid semver, a warning is
/// logged and a default `SemVer { 0, 0, 0 }` is used instead of returning an error.
pub fn detect_git_info(tag: &str, skip_validate: bool) -> Result<GitInfo> {
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
    let remote_url_raw = git_output(&["ls-remote", "--get-url"]).unwrap_or_default();
    // Strip credentials from HTTPS URLs (e.g. https://user:token@github.com/... → https://github.com/...)
    let remote_url = strip_url_credentials(&remote_url_raw);
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
                eprintln!("WARNING: current tag is not semver, skipping validation");
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

/// Strip a monorepo tag prefix from a tag string.
///
/// If `tag` starts with `prefix`, returns the remainder; otherwise returns
/// the original tag unchanged.
///
/// # Examples
/// ```
/// # use anodize_core::git::strip_monorepo_prefix;
/// assert_eq!(strip_monorepo_prefix("subproject1/v1.2.3", "subproject1/"), "v1.2.3");
/// assert_eq!(strip_monorepo_prefix("v1.2.3", "subproject1/"), "v1.2.3");
/// ```
pub fn strip_monorepo_prefix<'a>(tag: &'a str, prefix: &str) -> &'a str {
    tag.strip_prefix(prefix).unwrap_or(tag)
}

/// Find the latest tag matching a template pattern.
/// E.g., tag_template "cfgd-core-v{{ .Version }}" → matches tags like "cfgd-core-v1.2.3"
///
/// When `git_config` is provided:
/// - `ignore_tags`: tags matching any entry (glob patterns) are excluded.
///   When `template_vars` is also provided, each entry is rendered through the
///   template engine first (matching GoReleaser's behavior).
/// - `ignore_tag_prefixes`: tags starting with any prefix are excluded.
///   Also template-rendered when `template_vars` is provided.
/// - `tag_sort` set to `"-version:creatordate"`: delegates ordering to git
///   instead of Rust-side SemVer sort (the default `"-version:refname"` is
///   equivalent to SemVer sort, so Rust-side sort is kept).
/// - `prerelease_suffix`: always passed as `-c versionsort.suffix=<suffix>` to
///   git, regardless of `tag_sort` value. When using the default refname sort
///   and `prerelease_suffix` is set, git-delegated sort with
///   `--sort=-version:refname` is used so the suffix takes effect.
pub fn find_latest_tag_matching(
    tag_template: &str,
    git_config: Option<&GitConfig>,
    template_vars: Option<&TemplateVars>,
) -> Result<Option<String>> {
    find_latest_tag_matching_with_prefix(tag_template, git_config, template_vars, None)
}

/// Like [`find_latest_tag_matching`], but with optional monorepo prefix filtering.
///
/// When `monorepo_prefix` is `Some`:
/// - Only tags starting with the prefix are considered.
/// - The prefix is stripped before SemVer parsing (so `subproject1/v1.2.3`
///   parses as `v1.2.3` for version comparison).
/// - The FULL tag (with prefix) is returned as the result.
pub fn find_latest_tag_matching_with_prefix(
    tag_template: &str,
    git_config: Option<&GitConfig>,
    template_vars: Option<&TemplateVars>,
    monorepo_prefix: Option<&str>,
) -> Result<Option<String>> {
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

    // Use the shared helper to render ignore_tags and ignore_tag_prefixes
    // through the template engine when vars are available.
    let (rendered_ignore_tags, rendered_ignore_prefixes) =
        render_ignore_patterns(git_config, template_vars);

    // Compile ignore_tags entries as glob patterns for consistent behavior
    // with `find_previous_tag` (which passes them to `git describe --exclude`
    // which interprets globs). This matches GoReleaser's behavior.
    let ignore_tag_globs: Vec<glob::Pattern> = rendered_ignore_tags
        .iter()
        .filter_map(|pat| glob::Pattern::new(pat).ok())
        .collect();

    let tag_sort = git_config
        .and_then(|gc| gc.tag_sort.as_deref())
        .unwrap_or("-version:refname");
    let prerelease_suffix = git_config.and_then(|gc| gc.prerelease_suffix.as_deref());

    // When prerelease_suffix is set, always use git-delegated sort so that
    // `-c versionsort.suffix=<suffix>` takes effect. This matches GoReleaser's
    // behavior of always passing the suffix regardless of sort mode.
    let use_git_sort = tag_sort == "-version:creatordate" || prerelease_suffix.is_some();

    let tags_output = if use_git_sort {
        // Build args with optional versionsort.suffix config.
        let suffix_cfg;
        let mut args: Vec<&str> = Vec::new();
        if let Some(suffix) = prerelease_suffix {
            suffix_cfg = format!("versionsort.suffix={}", suffix);
            args.extend_from_slice(&["-c", &suffix_cfg]);
        }
        args.extend_from_slice(&["tag", "--sort", tag_sort, "--list"]);
        git_output(&args)?
    } else {
        git_output(&["tag", "--list"])?
    };

    if tags_output.is_empty() {
        return Ok(None);
    }

    let mut matching: Vec<(SemVer, String)> = tags_output
        .lines()
        // When monorepo_prefix is set, only consider tags starting with it.
        .filter(|t| {
            monorepo_prefix
                .map(|pfx| t.starts_with(pfx))
                .unwrap_or(true)
        })
        // For regex matching: when monorepo_prefix is set, strip the prefix
        // before matching (the tag_template pattern matches the version portion).
        .filter(|t| {
            let tag_for_match = monorepo_prefix
                .map(|pfx| strip_monorepo_prefix(t, pfx))
                .unwrap_or(t);
            re.is_match(tag_for_match)
        })
        // Apply ignore_tags: exclude via glob matching (template-rendered).
        // In monorepo mode, match against the STRIPPED tag so that user-defined
        // patterns like "v*-rc*" work without needing the monorepo prefix.
        .filter(|t| {
            let tag_for_ignore = monorepo_prefix
                .map(|pfx| strip_monorepo_prefix(t, pfx))
                .unwrap_or(t);
            !ignore_tag_globs
                .iter()
                .any(|pat| pat.matches(tag_for_ignore))
        })
        // Apply ignore_tag_prefixes: exclude tags starting with any prefix
        // (template-rendered). In monorepo mode, match against stripped tag.
        .filter(|t| {
            let tag_for_ignore = monorepo_prefix
                .map(|pfx| strip_monorepo_prefix(t, pfx))
                .unwrap_or(t);
            !rendered_ignore_prefixes
                .iter()
                .any(|pfx| tag_for_ignore.starts_with(pfx.as_str()))
        })
        // For SemVer parsing: strip the monorepo prefix before parsing.
        .filter_map(|t| {
            let tag_for_parse = monorepo_prefix
                .map(|pfx| strip_monorepo_prefix(t, pfx))
                .unwrap_or(t);
            parse_semver_tag(tag_for_parse)
                .ok()
                .map(|v| (v, t.to_string()))
        })
        .collect();

    if use_git_sort {
        // Git already sorted; the first entry in --sort=-version:* output is
        // the newest, so take the first after filtering.
        Ok(matching.into_iter().next().map(|(_, tag)| tag))
    } else {
        // Rust-side SemVer sort (ascending), pick the last (highest).
        matching.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(matching.last().map(|(_, tag)| tag.clone()))
    }
}

/// Parse git log output (formatted as `%H%x1f%h%x1f%s%x1f%an%x1f%ae%x1f%b%x1e`)
/// into a vec of [`Commit`]s.
///
/// Uses ASCII record separator (0x1e) between commits and unit separator (0x1f)
/// between fields, so multi-line body text doesn't break parsing.
fn parse_commit_output(output: &str) -> Vec<Commit> {
    if output.is_empty() {
        return vec![];
    }
    output
        .split('\x1e')
        .filter(|record| !record.trim().is_empty())
        .filter_map(|record| {
            let fields: Vec<&str> = record.split('\x1f').collect();
            if fields.len() >= 5 {
                Some(Commit {
                    hash: fields[0].trim().to_string(),
                    short_hash: fields[1].to_string(),
                    message: fields[2].to_string(),
                    author_name: fields[3].to_string(),
                    author_email: fields[4].to_string(),
                    body: fields.get(5).unwrap_or(&"").trim().to_string(),
                })
            } else {
                None
            }
        })
        .collect()
}

/// Get commits between two refs, optionally filtered to a path.
pub fn get_commits_between(from: &str, to: &str, path_filter: Option<&str>) -> Result<Vec<Commit>> {
    get_commits_between_paths(
        from,
        to,
        &path_filter
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>(),
    )
}

/// Get commits between two refs, filtered to multiple paths (git log -- path1 path2 ...).
pub fn get_commits_between_paths(from: &str, to: &str, paths: &[String]) -> Result<Vec<Commit>> {
    let range = format!("{}..{}", from, to);
    let mut args = vec![
        "-c".to_string(),
        "log.showSignature=false".to_string(),
        "log".to_string(),
        "--pretty=format:%H%x1f%h%x1f%s%x1f%an%x1f%ae%x1f%b%x1e".to_string(),
        range,
    ];
    if !paths.is_empty() {
        args.push("--".to_string());
        for p in paths {
            args.push(p.clone());
        }
    }
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let output = git_output(&arg_refs)?;
    Ok(parse_commit_output(&output))
}

/// Get all commits reachable from HEAD, optionally filtered to a path.
/// Used for initial releases where there is no previous tag.
pub fn get_all_commits(path_filter: Option<&str>) -> Result<Vec<Commit>> {
    get_all_commits_paths(
        &path_filter
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>(),
    )
}

/// Get all commits reachable from HEAD, filtered to multiple paths.
pub fn get_all_commits_paths(paths: &[String]) -> Result<Vec<Commit>> {
    let mut args = vec![
        "-c".to_string(),
        "log.showSignature=false".to_string(),
        "log".to_string(),
        "--pretty=format:%H%x1f%h%x1f%s%x1f%an%x1f%ae%x1f%b%x1e".to_string(),
        "HEAD".to_string(),
    ];
    if !paths.is_empty() {
        args.push("--".to_string());
        for p in paths {
            args.push(p.clone());
        }
    }
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let output = git_output(&arg_refs)?;
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
        .filter_map(|t| parse_semver_tag(t).ok().map(|v| (v, t.to_string())))
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

/// GET a GitHub API endpoint via the `gh` CLI (single request, no pagination).
///
/// Returns the parsed JSON response. Useful for endpoints that return a single
/// object (e.g. the Compare API) rather than a paginated array.
pub fn gh_api_get(endpoint: &str, token: Option<&str>) -> Result<serde_json::Value> {
    let mut cmd = Command::new("gh");
    cmd.args(["api", endpoint]);
    if let Some(tok) = token {
        cmd.env("GITHUB_TOKEN", tok);
    }
    let output = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .context("failed to spawn gh CLI")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("gh api GET {} failed: {}", endpoint, stderr.trim());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout).context("failed to parse gh api response")
}

/// GET a GitHub API endpoint via the `gh` CLI, with pagination.
///
/// Returns a JSON array of all pages concatenated. The caller is responsible for
/// ensuring that `gh` is installed and authenticated.
pub fn gh_api_get_paginated(endpoint: &str, token: Option<&str>) -> Result<Vec<serde_json::Value>> {
    let mut cmd = Command::new("gh");
    cmd.args(["api", "--paginate", endpoint]);
    if let Some(tok) = token {
        cmd.env("GITHUB_TOKEN", tok);
    }
    let output = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .context("failed to spawn gh CLI")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("gh api GET {} failed: {}", endpoint, stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Try parsing the entire response first before falling back to splitting.
    // This avoids the split_inclusive(']') approach corrupting non-array responses.
    if let Ok(serde_json::Value::Array(arr)) = serde_json::from_str::<serde_json::Value>(&stdout) {
        return Ok(arr);
    }
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&stdout) {
        // Single object response (e.g. non-list endpoint) — wrap in a vec.
        return Ok(vec![val]);
    }

    // Whole-parse failed — gh --paginate may return multiple JSON arrays
    // concatenated (e.g. `[...][...]`). Split on `]` boundaries and parse each chunk.
    let mut all_items = Vec::new();
    for chunk in stdout.split_inclusive(']') {
        let trimmed = chunk.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(serde_json::Value::Array(arr)) =
            serde_json::from_str::<serde_json::Value>(trimmed)
        {
            all_items.extend(arr);
        } else if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
            all_items.push(val);
        } else {
            // Log unparseable chunks so corrupt data doesn't go unnoticed.
            eprintln!(
                "warning: gh_api_get_paginated: failed to parse JSON chunk (len={}): {:?}",
                trimmed.len(),
                &trimmed[..trimmed.len().min(200)]
            );
        }
    }
    Ok(all_items)
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
    let output = git_output(&[
        "-c",
        "log.showSignature=false",
        "log",
        &format!("-{count}"),
        "--pretty=format:%s",
    ])?;
    Ok(output.lines().map(str::to_string).collect())
}

/// Get commit subjects between two refs.
pub fn get_commit_messages_between(from: &str, to: &str) -> Result<Vec<String>> {
    let output = git_output(&[
        "-c",
        "log.showSignature=false",
        "log",
        "--pretty=format:%s",
        &format!("{from}..{to}"),
    ])?;
    Ok(output.lines().map(str::to_string).collect())
}

/// Get the current branch name.
pub fn get_current_branch() -> Result<String> {
    git_output(&["rev-parse", "--abbrev-ref", "HEAD"])
}

/// Check if there are any commits since a given tag.
pub fn has_commits_since_tag(tag: &str) -> Result<bool> {
    let range = format!("{}..HEAD", tag);
    let output = git_output(&["-c", "log.showSignature=false", "log", "--oneline", &range])?;
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

/// Get last N commit subjects that touched a specific path.
pub fn get_last_commit_messages_path(count: usize, path: &str) -> Result<Vec<String>> {
    let output = git_output(&[
        "-c",
        "log.showSignature=false",
        "log",
        &format!("-{count}"),
        "--pretty=format:%s",
        "--",
        path,
    ])?;
    Ok(output.lines().map(str::to_string).collect())
}

/// Get commit subjects between two refs that touched a specific path.
pub fn get_commit_messages_between_path(from: &str, to: &str, path: &str) -> Result<Vec<String>> {
    let output = git_output(&[
        "-c",
        "log.showSignature=false",
        "log",
        "--pretty=format:%s",
        &format!("{from}..{to}"),
        "--",
        path,
    ])?;
    Ok(output.lines().map(str::to_string).collect())
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
    parse_remote_owner_repo(&url)
        .ok_or_else(|| anyhow::anyhow!("could not parse owner/repo from remote URL: {}", url))
}

/// Find the tag immediately before `current_tag` in commit history.
///
/// Uses `git describe --tags --abbrev=0 {current_tag}^` to locate the previous
/// tag. When `git_config` is provided, applies `--exclude` flags for both
/// `ignore_tags` patterns and `ignore_tag_prefixes` (converted to `<prefix>*`
/// globs), so git handles all filtering natively in a single call.
///
/// Both `ignore_tags` and `ignore_tag_prefixes` are rendered through the
/// template engine when `template_vars` is provided.
///
/// If that fails (e.g. `current_tag` is the very first tag), falls back to
/// returning `None`.
///
/// **Note:** This variant is not monorepo-aware — in a monorepo, use
/// [`find_previous_tag_with_prefix`] to ensure only tags from the same
/// subproject are considered.
pub fn find_previous_tag(
    current_tag: &str,
    git_config: Option<&GitConfig>,
    template_vars: Option<&TemplateVars>,
) -> Result<Option<String>> {
    find_previous_tag_with_prefix(current_tag, git_config, template_vars, None)
}

/// Like [`find_previous_tag`], but with optional monorepo prefix filtering.
///
/// When `monorepo_prefix` is `Some`, adds `--match=<prefix>*` to the
/// `git describe` call so only tags from the same subproject are considered.
/// The full tag (with prefix) is returned.
pub fn find_previous_tag_with_prefix(
    current_tag: &str,
    git_config: Option<&GitConfig>,
    template_vars: Option<&TemplateVars>,
    monorepo_prefix: Option<&str>,
) -> Result<Option<String>> {
    let parent_ref = format!("{}^", current_tag);

    // Use the shared helper to render both ignore_tags and ignore_tag_prefixes.
    let (rendered_ignore_tags, rendered_ignore_prefixes) =
        render_ignore_patterns(git_config, template_vars);

    // Build args: `git describe --tags --abbrev=0 --exclude=<pattern> ... <parent_ref>`
    // Include both ignore_tags (as-is, they're glob patterns) and
    // ignore_tag_prefixes (converted to `<prefix>*` globs).
    let mut exclude_args: Vec<String> = rendered_ignore_tags
        .iter()
        .map(|t| format!("--exclude={}", t))
        .collect();
    for pfx in &rendered_ignore_prefixes {
        exclude_args.push(format!("--exclude={}*", pfx));
    }

    // When monorepo_prefix is set, constrain git describe to only consider
    // tags matching this prefix. Without this, git describe would return
    // the nearest reachable tag from ANY subproject.
    let match_arg;
    let mut args: Vec<&str> = vec!["describe", "--tags", "--abbrev=0"];
    if let Some(prefix) = monorepo_prefix {
        match_arg = format!("--match={}*", prefix);
        args.push(&match_arg);
    }
    for ea in &exclude_args {
        args.push(ea.as_str());
    }
    args.push(&parent_ref);

    match git_output(&args) {
        Ok(tag) if !tag.is_empty() => Ok(Some(tag)),
        _ => Ok(None),
    }
}

/// Return the SHA of the very first commit in the repository.
///
/// Runs `git rev-list --max-parents=0 HEAD` and returns the first line
/// (repositories with multiple roots will return the oldest).
pub fn get_first_commit() -> Result<String> {
    let output = git_output(&["rev-list", "--max-parents=0", "HEAD"])?;
    // In repos with multiple roots, take the last line (oldest commit).
    output
        .lines()
        .last()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("no commits found in repository"))
}

/// Check whether `tag` points at the current HEAD commit.
///
/// Compares the dereferenced tag object (`git rev-parse {tag}^{{}}`) with
/// `git rev-parse HEAD`. Returns `false` if either command fails.
///
/// Works with any tag name including monorepo-prefixed tags (e.g.
/// `subproject1/v1.2.3`), since `git rev-parse` resolves tag refs by
/// name regardless of slashes or prefixes.
pub fn tag_points_at_head(tag: &str) -> Result<bool> {
    let deref = format!("{}^{{}}", tag);
    let tag_sha = git_output(&["rev-parse", &deref])?;
    let head_sha = git_output(&["rev-parse", "HEAD"])?;
    Ok(tag_sha == head_sha)
}

/// Check whether `git` is available in PATH.
pub fn check_git_available() -> Result<()> {
    let output = Command::new("git").arg("--version").output();
    match output {
        Ok(o) if o.status.success() => Ok(()),
        _ => bail!("git is not installed or not in PATH. Install git and try again."),
    }
}

/// Check whether the current directory is inside a git repository.
pub fn is_git_repo() -> bool {
    git_output(&["rev-parse", "--git-dir"]).is_ok()
}

/// Return the `git status --porcelain` output showing dirty files.
pub fn git_status_porcelain() -> String {
    git_output(&["status", "--porcelain"]).unwrap_or_default()
}

/// Check whether the current repository is a shallow clone.
///
/// Returns `true` if the `.git/shallow` sentinel file exists, which git creates
/// when a repository was cloned with `--depth`.
pub fn is_shallow_clone() -> bool {
    // Use `git rev-parse --git-dir` to find the actual .git directory,
    // which handles worktrees and non-standard layouts.
    let git_dir = git_output(&["rev-parse", "--git-dir"]).unwrap_or_else(|_| ".git".to_string());
    std::path::Path::new(&git_dir).join("shallow").exists()
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
        assert_eq!(v.build_metadata, None);
    }

    #[test]
    fn test_parse_semver_prerelease() {
        let v = parse_semver("v1.0.0-rc.1").unwrap();
        assert_eq!(v.major, 1);
        assert_eq!(v.prerelease, Some("rc.1".to_string()));
        assert_eq!(v.build_metadata, None);
    }

    #[test]
    fn test_parse_semver_build_metadata() {
        let v = parse_semver("v1.0.0+build.42").unwrap();
        assert_eq!(v.major, 1);
        assert_eq!(v.minor, 0);
        assert_eq!(v.patch, 0);
        assert_eq!(v.prerelease, None);
        assert_eq!(v.build_metadata, Some("build.42".to_string()));
    }

    #[test]
    fn test_parse_semver_prerelease_and_build_metadata() {
        let v = parse_semver("v1.0.0-rc.1+build.42").unwrap();
        assert_eq!(v.major, 1);
        assert_eq!(v.prerelease, Some("rc.1".to_string()));
        assert_eq!(v.build_metadata, Some("build.42".to_string()));
    }

    #[test]
    fn test_parse_semver_rejects_prefix() {
        // Strict parse_semver rejects prefixed tags (use parse_semver_tag instead)
        assert!(parse_semver("cfgd-core-v2.1.0").is_err());
        assert!(parse_semver("release-notes-v1.2.3").is_err());
    }

    #[test]
    fn test_parse_semver_tag_with_prefix() {
        let v = parse_semver_tag("cfgd-core-v2.1.0").unwrap();
        assert_eq!(v.major, 2);
        assert_eq!(v.minor, 1);
        assert_eq!(v.patch, 0);
    }

    #[test]
    fn test_parse_semver_tag_plain() {
        // parse_semver_tag also handles plain versions
        let v = parse_semver_tag("v1.2.3").unwrap();
        assert_eq!(v.major, 1);
        assert_eq!(v.minor, 2);
        assert_eq!(v.patch, 3);
    }

    #[test]
    fn test_parse_semver_tag_with_prerelease_prefix() {
        let v = parse_semver_tag("my-project-v1.0.0-rc.1").unwrap();
        assert_eq!(v.major, 1);
        assert_eq!(v.prerelease, Some("rc.1".to_string()));
    }

    #[test]
    fn test_is_prerelease() {
        assert!(parse_semver("v1.0.0-rc.1").unwrap().is_prerelease());
        assert!(!parse_semver("v1.0.0").unwrap().is_prerelease());
        // Build metadata only is NOT a prerelease
        assert!(!parse_semver("v1.0.0+build.42").unwrap().is_prerelease());
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

    // -- parse_remote_owner_repo (generic) -----------------------------------

    #[test]
    fn test_parse_remote_github_https() {
        let result = parse_remote_owner_repo("https://github.com/owner/repo.git");
        assert_eq!(result, Some(("owner".to_string(), "repo".to_string())));
    }

    #[test]
    fn test_parse_remote_gitlab_https() {
        let result = parse_remote_owner_repo("https://gitlab.com/owner/repo.git");
        assert_eq!(result, Some(("owner".to_string(), "repo".to_string())));
    }

    #[test]
    fn test_parse_remote_gitea_https() {
        let result = parse_remote_owner_repo("https://gitea.example.com/myorg/myapp.git");
        assert_eq!(result, Some(("myorg".to_string(), "myapp".to_string())));
    }

    #[test]
    fn test_parse_remote_gitlab_nested_group() {
        let result = parse_remote_owner_repo("https://gitlab.com/group/subgroup/repo.git");
        assert_eq!(
            result,
            Some(("group/subgroup".to_string(), "repo".to_string()))
        );
    }

    #[test]
    fn test_parse_remote_ssh_gitlab() {
        let result = parse_remote_owner_repo("git@gitlab.com:owner/repo.git");
        assert_eq!(result, Some(("owner".to_string(), "repo".to_string())));
    }

    #[test]
    fn test_parse_remote_ssh_gitea() {
        let result = parse_remote_owner_repo("git@gitea.example.com:org/app.git");
        assert_eq!(result, Some(("org".to_string(), "app".to_string())));
    }

    #[test]
    fn test_parse_remote_ssh_nested_group() {
        let result = parse_remote_owner_repo("git@gitlab.com:group/subgroup/repo.git");
        assert_eq!(
            result,
            Some(("group/subgroup".to_string(), "repo".to_string()))
        );
    }

    #[test]
    fn test_parse_remote_no_dotgit() {
        let result = parse_remote_owner_repo("https://gitlab.com/owner/repo");
        assert_eq!(result, Some(("owner".to_string(), "repo".to_string())));
    }

    #[test]
    fn test_parse_remote_empty() {
        assert_eq!(parse_remote_owner_repo(""), None);
    }

    #[test]
    fn test_parse_remote_http() {
        let result = parse_remote_owner_repo("http://gitlab.local/team/project.git");
        assert_eq!(result, Some(("team".to_string(), "project".to_string())));
    }

    #[test]
    fn test_strip_url_credentials_with_userinfo() {
        assert_eq!(
            strip_url_credentials("https://user:token@github.com/owner/repo.git"),
            "https://github.com/owner/repo.git"
        );
    }

    #[test]
    fn test_strip_url_credentials_no_userinfo() {
        assert_eq!(
            strip_url_credentials("https://github.com/owner/repo.git"),
            "https://github.com/owner/repo.git"
        );
    }

    #[test]
    fn test_strip_url_credentials_ssh_unchanged() {
        assert_eq!(
            strip_url_credentials("git@github.com:owner/repo.git"),
            "git@github.com:owner/repo.git"
        );
    }

    #[test]
    fn test_strip_url_credentials_user_only() {
        assert_eq!(
            strip_url_credentials("https://user@github.com/owner/repo.git"),
            "https://github.com/owner/repo.git"
        );
    }

    #[test]
    fn test_compare_prerelease_numeric() {
        // rc.9 < rc.10 (numeric comparison, not lexicographic)
        assert_eq!(
            compare_prerelease("rc.9", "rc.10"),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_prerelease("rc.10", "rc.9"),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn test_compare_prerelease_numeric_less_than_alpha() {
        // Numeric identifiers always have lower precedence than alphanumeric
        assert_eq!(compare_prerelease("1", "alpha"), std::cmp::Ordering::Less);
        assert_eq!(
            compare_prerelease("alpha", "1"),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn test_compare_prerelease_alpha_lexicographic() {
        assert_eq!(
            compare_prerelease("alpha", "beta"),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn test_compare_prerelease_shorter_lower_precedence() {
        // alpha < alpha.1 (shorter set = lower precedence)
        assert_eq!(
            compare_prerelease("alpha", "alpha.1"),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn test_compare_prerelease_equal() {
        assert_eq!(
            compare_prerelease("rc.1", "rc.1"),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn test_semver_ord_prerelease_less_than_release() {
        let pre = parse_semver("v1.0.0-rc.1").unwrap();
        let rel = parse_semver("v1.0.0").unwrap();
        assert!(pre < rel);
    }

    #[test]
    fn test_semver_ord_prerelease_numeric_sorting() {
        // v1.0.0-rc.9 < v1.0.0-rc.10 (SemVer 2.0.0 compliant)
        let rc9 = parse_semver("v1.0.0-rc.9").unwrap();
        let rc10 = parse_semver("v1.0.0-rc.10").unwrap();
        assert!(rc9 < rc10);
    }

    // -----------------------------------------------------------------------
    // find_latest_tag_matching + GitConfig integration tests
    //
    // Each test creates a fresh temporary git repository with tags, then
    // verifies that GitConfig fields (ignore_tags, ignore_tag_prefixes, etc.)
    // are respected.
    // -----------------------------------------------------------------------

    use serial_test::serial;

    /// Create a bare-bones git repo in `dir` with an initial commit and the
    /// given list of lightweight tags.
    fn init_repo_with_tags(dir: &std::path::Path, tags: &[&str]) {
        use std::process::Command;

        let run = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@test.com")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@test.com")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };

        run(&["init"]);
        run(&["config", "user.email", "test@test.com"]);
        run(&["config", "user.name", "test"]);
        std::fs::write(dir.join("README"), "init").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "initial"]);

        for tag in tags {
            run(&["tag", tag]);
        }
    }

    #[test]
    #[serial]
    fn test_find_latest_tag_none_config_unchanged_behavior() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_tags(dir, &["v1.0.0", "v1.1.0", "v2.0.0"]);

        // Change to the temp repo so git commands work.
        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();

        let result = find_latest_tag_matching("v{{ .Version }}", None, None).unwrap();
        assert_eq!(result, Some("v2.0.0".to_string()));

        std::env::set_current_dir(orig).unwrap();
    }

    #[test]
    #[serial]
    fn test_find_latest_tag_ignore_tags_exact_match() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_tags(dir, &["v1.0.0", "v2.0.0", "v3.0.0"]);

        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();

        let gc = crate::config::GitConfig {
            ignore_tags: Some(vec!["v3.0.0".to_string()]),
            ..Default::default()
        };
        let result = find_latest_tag_matching("v{{ .Version }}", Some(&gc), None).unwrap();
        assert_eq!(result, Some("v2.0.0".to_string()));

        std::env::set_current_dir(orig).unwrap();
    }

    #[test]
    #[serial]
    fn test_find_latest_tag_ignore_tags_multiple() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_tags(dir, &["v1.0.0", "v2.0.0", "v3.0.0"]);

        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();

        let gc = crate::config::GitConfig {
            ignore_tags: Some(vec!["v3.0.0".to_string(), "v2.0.0".to_string()]),
            ..Default::default()
        };
        let result = find_latest_tag_matching("v{{ .Version }}", Some(&gc), None).unwrap();
        assert_eq!(result, Some("v1.0.0".to_string()));

        std::env::set_current_dir(orig).unwrap();
    }

    #[test]
    #[serial]
    fn test_find_latest_tag_ignore_tag_prefixes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_tags(
            dir,
            &["v1.0.0", "v2.0.0", "nightly-v3.0.0", "nightly-v4.0.0"],
        );

        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();

        // Without prefix filtering, the template "v{{ .Version }}" won't match
        // nightly-v* tags anyway (regex mismatch). So test with a broader template
        // or with nightly-prefixed tags that do match a nightly template.
        // Let's test: filter out "nightly-" prefix from "nightly-v{{ .Version }}"
        let gc = crate::config::GitConfig {
            ignore_tag_prefixes: Some(vec!["nightly-".to_string()]),
            ..Default::default()
        };
        // The "v{{ .Version }}" template only matches v1.0.0, v2.0.0.
        // Without filtering, nightly tags don't match anyway, so latest = v2.0.0.
        let result = find_latest_tag_matching("v{{ .Version }}", Some(&gc), None).unwrap();
        assert_eq!(result, Some("v2.0.0".to_string()));

        // Now test with a template that would match nightly tags too:
        // Use a nightly template. Without ignore_tag_prefixes, nightly-v4.0.0 wins.
        let result_nightly =
            find_latest_tag_matching("nightly-v{{ .Version }}", None, None).unwrap();
        assert_eq!(result_nightly, Some("nightly-v4.0.0".to_string()));

        // With ignore_tag_prefixes filtering out "nightly-", all nightly tags are excluded.
        let result_filtered =
            find_latest_tag_matching("nightly-v{{ .Version }}", Some(&gc), None).unwrap();
        assert_eq!(result_filtered, None);

        std::env::set_current_dir(orig).unwrap();
    }

    #[test]
    #[serial]
    fn test_find_latest_tag_ignore_all_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_tags(dir, &["v1.0.0", "v2.0.0"]);

        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();

        let gc = crate::config::GitConfig {
            ignore_tags: Some(vec!["v1.0.0".to_string(), "v2.0.0".to_string()]),
            ..Default::default()
        };
        let result = find_latest_tag_matching("v{{ .Version }}", Some(&gc), None).unwrap();
        assert_eq!(result, None);

        std::env::set_current_dir(orig).unwrap();
    }

    #[test]
    #[serial]
    fn test_find_latest_tag_ignore_tags_and_prefixes_combined() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_tags(dir, &["v1.0.0", "v2.0.0", "v3.0.0-beta.1"]);

        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();

        // ignore v2.0.0 by exact match, and anything starting with "v3" by prefix
        let gc = crate::config::GitConfig {
            ignore_tags: Some(vec!["v2.0.0".to_string()]),
            ignore_tag_prefixes: Some(vec!["v3".to_string()]),
            ..Default::default()
        };
        let result = find_latest_tag_matching("v{{ .Version }}", Some(&gc), None).unwrap();
        assert_eq!(result, Some("v1.0.0".to_string()));

        std::env::set_current_dir(orig).unwrap();
    }

    #[test]
    #[serial]
    fn test_find_latest_tag_with_prefixed_template() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_tags(
            dir,
            &[
                "myapp-v1.0.0",
                "myapp-v2.0.0",
                "myapp-v3.0.0",
                "other-v9.0.0",
            ],
        );

        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();

        // Ignore myapp-v3.0.0 specifically
        let gc = crate::config::GitConfig {
            ignore_tags: Some(vec!["myapp-v3.0.0".to_string()]),
            ..Default::default()
        };
        let result = find_latest_tag_matching("myapp-v{{ .Version }}", Some(&gc), None).unwrap();
        assert_eq!(result, Some("myapp-v2.0.0".to_string()));

        std::env::set_current_dir(orig).unwrap();
    }

    #[test]
    #[serial]
    fn test_find_latest_tag_default_git_config_same_as_none() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_tags(dir, &["v1.0.0", "v1.1.0", "v2.0.0"]);

        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();

        // Default GitConfig has all fields None — should behave identically to None
        let gc = crate::config::GitConfig::default();
        let with_default = find_latest_tag_matching("v{{ .Version }}", Some(&gc), None).unwrap();
        let with_none = find_latest_tag_matching("v{{ .Version }}", None, None).unwrap();
        assert_eq!(with_default, with_none);
        assert_eq!(with_default, Some("v2.0.0".to_string()));

        std::env::set_current_dir(orig).unwrap();
    }

    #[test]
    #[serial]
    fn test_find_latest_tag_prerelease_suffix_with_default_sort() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // Create tags: two releases and a prerelease with -rc suffix.
        // v1.1.1-rc.1 is semantically version 1.1.1 with a prerelease,
        // which is > 1.1.0 in both SemVer and git version sort.
        // versionsort.suffix only affects ordering relative to the same
        // base version (e.g. v1.1.1-rc.1 vs v1.1.1), not across different
        // patch levels.
        init_repo_with_tags(dir, &["v1.0.0", "v1.1.0", "v1.1.1-rc.1"]);

        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();

        // Without prerelease_suffix, using Rust-side SemVer sort:
        // v1.1.1-rc.1 is a prerelease of v1.1.1, which is > v1.1.0 but
        // SemVer says prereleases are < the release, so 1.1.1-rc.1 < 1.1.1.
        // But 1.1.1-rc.1 > 1.1.0 (different patch version), so it wins.
        let result_no_suffix = find_latest_tag_matching("v{{ .Version }}", None, None).unwrap();
        assert_eq!(
            result_no_suffix,
            Some("v1.1.1-rc.1".to_string()),
            "without prerelease_suffix, SemVer sort puts v1.1.1-rc.1 highest"
        );

        // With prerelease_suffix="-rc", git-delegated sort is activated
        // (use_git_sort=true). versionsort.suffix=-rc makes -rc tags sort
        // after their base version (so v1.1.1-rc.1 comes after v1.1.1),
        // but v1.1.1-rc.1 is still version 1.1.1 which is > 1.1.0.
        // Since we take the first (highest) from git's descending sort,
        // v1.1.1-rc.1 remains the latest.
        let gc = crate::config::GitConfig {
            prerelease_suffix: Some("-rc".to_string()),
            ..Default::default()
        };
        let result = find_latest_tag_matching("v{{ .Version }}", Some(&gc), None).unwrap();
        assert_eq!(
            result,
            Some("v1.1.1-rc.1".to_string()),
            "prerelease_suffix activates git-delegated sort; v1.1.1-rc.1 still highest"
        );

        // Now test the scenario where versionsort.suffix actually matters:
        // when the release version exists alongside the prerelease.
        // Add v1.1.1 — without suffix, git sorts rc before release (v1.1.1-rc.1 < v1.1.1);
        // with suffix, rc sorts *after* release but --sort=-version:refname
        // means descending, so release comes first.
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@test.com")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@test.com")
                .output()
                .unwrap();
            assert!(out.status.success());
        };
        run(&["tag", "v1.1.1"]);

        // With versionsort.suffix=-rc and both v1.1.1 and v1.1.1-rc.1 present,
        // the suffix causes -rc.1 to sort after v1.1.1 in ascending order,
        // meaning v1.1.1-rc.1 comes last. In descending sort (-version:refname),
        // v1.1.1-rc.1 would be first. But the key point is that git-delegated
        // sort IS being used (prerelease_suffix triggers it).
        let result_both = find_latest_tag_matching("v{{ .Version }}", Some(&gc), None).unwrap();
        assert!(
            result_both.is_some(),
            "should find a tag with both release and rc present"
        );

        std::env::set_current_dir(orig).unwrap();
    }

    #[test]
    #[serial]
    fn test_find_latest_tag_ignore_tags_template_rendered() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_tags(dir, &["v1.0.0", "v2.0.0", "v3.0.0"]);

        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();

        // Set up template vars with an env variable
        let mut vars = crate::template::TemplateVars::new();
        vars.set_env("IGNORE_TAG", "v3.0.0");

        // Use a template expression in ignore_tags
        let gc = crate::config::GitConfig {
            ignore_tags: Some(vec!["{{ .Env.IGNORE_TAG }}".to_string()]),
            ..Default::default()
        };

        // Without template_vars, the raw string "{{ .Env.IGNORE_TAG }}" won't
        // match any tag, so v3.0.0 is still included.
        let result_raw = find_latest_tag_matching("v{{ .Version }}", Some(&gc), None).unwrap();
        assert_eq!(result_raw, Some("v3.0.0".to_string()));

        // With template_vars, the template is rendered to "v3.0.0" which
        // matches and excludes that tag.
        let result_rendered =
            find_latest_tag_matching("v{{ .Version }}", Some(&gc), Some(&vars)).unwrap();
        assert_eq!(result_rendered, Some("v2.0.0".to_string()));

        std::env::set_current_dir(orig).unwrap();
    }

    /// Create a git repo in `dir` with separate commits for each tag
    /// (needed for `git describe --tags --abbrev=0` to work correctly).
    fn init_repo_with_tagged_commits(dir: &std::path::Path, tags: &[&str]) {
        use std::process::Command;

        let run = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@test.com")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@test.com")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };

        run(&["init"]);
        run(&["config", "user.email", "test@test.com"]);
        run(&["config", "user.name", "test"]);

        for (i, tag) in tags.iter().enumerate() {
            let filename = format!("file_{}", i);
            std::fs::write(dir.join(&filename), format!("content {}", i)).unwrap();
            run(&["add", "."]);
            run(&["commit", "-m", &format!("commit for {}", tag)]);
            run(&["tag", tag]);
        }
    }

    #[test]
    #[serial]
    fn test_find_previous_tag_with_ignore_tags() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // Create commits with tags: v1.0.0, v2.0.0, v3.0.0
        // Each tag on a separate commit so git describe can find them.
        init_repo_with_tagged_commits(dir, &["v1.0.0", "v2.0.0", "v3.0.0"]);

        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();

        // Without ignore_tags, previous tag of v3.0.0 should be v2.0.0
        let result = find_previous_tag("v3.0.0", None, None).unwrap();
        assert_eq!(result, Some("v2.0.0".to_string()));

        // With v2.0.0 in ignore_tags, it should be excluded via --exclude
        // and the previous tag should be v1.0.0
        let gc = crate::config::GitConfig {
            ignore_tags: Some(vec!["v2.0.0".to_string()]),
            ..Default::default()
        };
        let result_filtered = find_previous_tag("v3.0.0", Some(&gc), None).unwrap();
        assert_eq!(result_filtered, Some("v1.0.0".to_string()));

        std::env::set_current_dir(orig).unwrap();
    }

    #[test]
    #[serial]
    fn test_find_previous_tag_with_ignore_tag_prefixes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // Create tags where the previous tag has a prefix we want to ignore
        init_repo_with_tagged_commits(dir, &["v1.0.0", "nightly-v2.0.0", "v3.0.0"]);

        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();

        // Without filtering, previous tag of v3.0.0 is nightly-v2.0.0
        let result = find_previous_tag("v3.0.0", None, None).unwrap();
        assert_eq!(result, Some("nightly-v2.0.0".to_string()));

        // With ignore_tag_prefixes=["nightly-"], nightly-v2.0.0 is excluded
        // via --exclude=nightly-* and git describe skips it, returning v1.0.0
        let gc = crate::config::GitConfig {
            ignore_tag_prefixes: Some(vec!["nightly-".to_string()]),
            ..Default::default()
        };
        let result_filtered = find_previous_tag("v3.0.0", Some(&gc), None).unwrap();
        assert_eq!(result_filtered, Some("v1.0.0".to_string()));

        std::env::set_current_dir(orig).unwrap();
    }

    #[test]
    #[serial]
    fn test_find_previous_tag_no_config_unchanged_behavior() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_tagged_commits(dir, &["v1.0.0", "v2.0.0"]);

        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();

        let result = find_previous_tag("v2.0.0", None, None).unwrap();
        assert_eq!(result, Some("v1.0.0".to_string()));

        std::env::set_current_dir(orig).unwrap();
    }

    // -----------------------------------------------------------------------
    // strip_monorepo_prefix tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_strip_monorepo_prefix_with_match() {
        assert_eq!(
            strip_monorepo_prefix("subproject1/v1.2.3", "subproject1/"),
            "v1.2.3"
        );
    }

    #[test]
    fn test_strip_monorepo_prefix_no_match() {
        assert_eq!(strip_monorepo_prefix("v1.2.3", "subproject1/"), "v1.2.3");
    }

    #[test]
    fn test_strip_monorepo_prefix_empty_prefix() {
        assert_eq!(strip_monorepo_prefix("v1.2.3", ""), "v1.2.3");
    }

    #[test]
    fn test_strip_monorepo_prefix_partial_match() {
        // "sub" is a prefix of "subproject1/" but not the full prefix.
        assert_eq!(
            strip_monorepo_prefix("subproject1/v1.2.3", "sub"),
            "project1/v1.2.3"
        );
    }

    // -----------------------------------------------------------------------
    // find_latest_tag_matching_with_prefix (monorepo) tests
    // -----------------------------------------------------------------------

    #[test]
    #[serial]
    fn test_find_latest_tag_with_monorepo_prefix_filters_and_returns_full_tag() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_tags(
            dir,
            &[
                "v1.0.0",
                "subproject1/v1.0.0",
                "subproject1/v2.0.0",
                "subproject2/v3.0.0",
            ],
        );

        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();

        // With monorepo prefix "subproject1/", should only find subproject1 tags
        // and return the FULL tag (with prefix).
        let result = find_latest_tag_matching_with_prefix(
            "v{{ .Version }}",
            None,
            None,
            Some("subproject1/"),
        )
        .unwrap();
        assert_eq!(
            result,
            Some("subproject1/v2.0.0".to_string()),
            "should return the full tag with prefix"
        );

        std::env::set_current_dir(orig).unwrap();
    }

    #[test]
    #[serial]
    fn test_find_latest_tag_with_monorepo_prefix_semver_comparison_uses_stripped_tag() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // Versions should be compared using the stripped tag
        init_repo_with_tags(dir, &["myapp/v1.0.0", "myapp/v2.0.0", "myapp/v1.5.0"]);

        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();

        let result =
            find_latest_tag_matching_with_prefix("v{{ .Version }}", None, None, Some("myapp/"))
                .unwrap();
        assert_eq!(
            result,
            Some("myapp/v2.0.0".to_string()),
            "should pick the highest version based on stripped semver"
        );

        std::env::set_current_dir(orig).unwrap();
    }

    #[test]
    #[serial]
    fn test_find_latest_tag_with_monorepo_prefix_no_matching_tags() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_tags(dir, &["v1.0.0", "v2.0.0"]);

        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();

        // No tags start with "myapp/" so result should be None.
        let result =
            find_latest_tag_matching_with_prefix("v{{ .Version }}", None, None, Some("myapp/"))
                .unwrap();
        assert_eq!(result, None);

        std::env::set_current_dir(orig).unwrap();
    }

    #[test]
    #[serial]
    fn test_find_latest_tag_with_monorepo_prefix_none_behaves_like_original() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_tags(dir, &["v1.0.0", "v1.1.0", "v2.0.0"]);

        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();

        // Without monorepo prefix, should behave exactly like find_latest_tag_matching.
        let result_with_prefix =
            find_latest_tag_matching_with_prefix("v{{ .Version }}", None, None, None).unwrap();
        let result_original = find_latest_tag_matching("v{{ .Version }}", None, None).unwrap();
        assert_eq!(result_with_prefix, result_original);
        assert_eq!(result_with_prefix, Some("v2.0.0".to_string()));

        std::env::set_current_dir(orig).unwrap();
    }

    #[test]
    #[serial]
    fn test_find_latest_tag_with_monorepo_prefix_and_prerelease() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_tags(dir, &["svc/v1.0.0", "svc/v1.1.0-rc.1", "svc/v1.1.0"]);

        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();

        let result =
            find_latest_tag_matching_with_prefix("v{{ .Version }}", None, None, Some("svc/"))
                .unwrap();
        assert_eq!(
            result,
            Some("svc/v1.1.0".to_string()),
            "release v1.1.0 should win over v1.1.0-rc.1"
        );

        std::env::set_current_dir(orig).unwrap();
    }
}
