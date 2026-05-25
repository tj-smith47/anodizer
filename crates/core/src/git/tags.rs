use anyhow::Result;
use regex::Regex;
use std::path::Path;
use std::process::Command;

use crate::config::GitConfig;
use crate::template::TemplateVars;

use super::git_output_in;
use super::semver::{SemVer, parse_semver_tag};

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
/// # use anodizer_core::git::strip_monorepo_prefix;
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
    find_latest_tag_matching_in(
        &std::env::current_dir()?,
        tag_template,
        git_config,
        template_vars,
    )
}

/// Path-taking sibling of [`find_latest_tag_matching`].
pub fn find_latest_tag_matching_in(
    cwd: &Path,
    tag_template: &str,
    git_config: Option<&GitConfig>,
    template_vars: Option<&TemplateVars>,
) -> Result<Option<String>> {
    find_latest_tag_matching_with_prefix_in(cwd, tag_template, git_config, template_vars, None)
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
    find_latest_tag_matching_with_prefix_in(
        &std::env::current_dir()?,
        tag_template,
        git_config,
        template_vars,
        monorepo_prefix,
    )
}

/// Path-taking sibling of [`find_latest_tag_matching_with_prefix`].
pub fn find_latest_tag_matching_with_prefix_in(
    cwd: &Path,
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
        git_output_in(cwd, &args)?
    } else {
        git_output_in(cwd, &["tag", "--list"])?
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

/// Collect semver tags from the output of the given `git` arguments, filtered
/// by `prefix` and sorted descending by version. When `git_config` is
/// provided, applies `ignore_tags` (glob match) and `ignore_tag_prefixes`
/// (starts_with) filters; both lists are template-rendered when
/// `template_vars` is provided.
fn collect_semver_tags_in(
    cwd: &Path,
    git_args: &[&str],
    prefix: &str,
    git_config: Option<&GitConfig>,
    template_vars: Option<&TemplateVars>,
) -> Result<Vec<String>> {
    let tags_output = git_output_in(cwd, git_args)?;
    if tags_output.is_empty() {
        return Ok(vec![]);
    }

    let (rendered_ignore_tags, rendered_ignore_prefixes) =
        render_ignore_patterns(git_config, template_vars);
    let ignore_tag_globs: Vec<glob::Pattern> = rendered_ignore_tags
        .iter()
        .filter_map(|pat| glob::Pattern::new(pat).ok())
        .collect();

    let mut matching: Vec<(SemVer, String)> = tags_output
        .lines()
        .filter(|t| t.starts_with(prefix))
        .filter(|t| !ignore_tag_globs.iter().any(|g| g.matches(t)))
        .filter(|t| {
            !rendered_ignore_prefixes
                .iter()
                .any(|p| !p.is_empty() && t.starts_with(p))
        })
        .filter_map(|t| parse_semver_tag(t).ok().map(|v| (v, t.to_string())))
        .collect();
    matching.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(matching.into_iter().map(|(_, tag)| tag).collect())
}

/// Get all semver tags in the repo, sorted descending by version.
/// Prerelease tags sort after release tags of the same major.minor.patch.
///
/// When `git_config` is provided, applies `ignore_tags` (glob match) and
/// `ignore_tag_prefixes` (starts_with) filters. When `template_vars` is
/// provided, both lists are template-rendered first.
pub fn get_all_semver_tags(
    prefix: &str,
    git_config: Option<&GitConfig>,
    template_vars: Option<&TemplateVars>,
) -> Result<Vec<String>> {
    get_all_semver_tags_in(&std::env::current_dir()?, prefix, git_config, template_vars)
}

/// Path-taking sibling of [`get_all_semver_tags`].
pub fn get_all_semver_tags_in(
    cwd: &Path,
    prefix: &str,
    git_config: Option<&GitConfig>,
    template_vars: Option<&TemplateVars>,
) -> Result<Vec<String>> {
    collect_semver_tags_in(cwd, &["tag", "--list"], prefix, git_config, template_vars)
}

/// Get semver tags reachable from HEAD, sorted descending by version.
/// Prerelease tags sort after release tags of the same major.minor.patch.
///
/// Same filtering semantics as [`get_all_semver_tags`].
pub fn get_branch_semver_tags(
    prefix: &str,
    git_config: Option<&GitConfig>,
    template_vars: Option<&TemplateVars>,
) -> Result<Vec<String>> {
    get_branch_semver_tags_in(&std::env::current_dir()?, prefix, git_config, template_vars)
}

/// Path-taking sibling of [`get_branch_semver_tags`].
pub fn get_branch_semver_tags_in(
    cwd: &Path,
    prefix: &str,
    git_config: Option<&GitConfig>,
    template_vars: Option<&TemplateVars>,
) -> Result<Vec<String>> {
    collect_semver_tags_in(
        cwd,
        &["tag", "--merged", "HEAD", "--list"],
        prefix,
        git_config,
        template_vars,
    )
}

/// Create an annotated tag and push it if an `origin` remote exists.
pub fn create_and_push_tag(
    tag: &str,
    message: &str,
    dry_run: bool,
    log: &crate::log::StageLogger,
    strict: bool,
) -> Result<()> {
    create_and_push_tag_in(
        &std::env::current_dir()?,
        tag,
        message,
        dry_run,
        log,
        strict,
    )
}

/// Create an annotated tag in `cwd` and push it if an `origin` remote exists.
///
/// Path-taking sibling of [`create_and_push_tag`] so callers (notably the
/// GitHub-API tag fallback path and tests) can drive tagging against an
/// explicit repository without mutating the process cwd.
pub fn create_and_push_tag_in(
    cwd: &Path,
    tag: &str,
    message: &str,
    dry_run: bool,
    log: &crate::log::StageLogger,
    strict: bool,
) -> Result<()> {
    if dry_run {
        log.status(&format!(
            "(dry-run) would create tag: {} (\"{}\")",
            tag, message
        ));
        return Ok(());
    }
    git_output_in(cwd, &["tag", "-a", tag, "-m", message])?;

    let has_remote = std::process::Command::new("git")
        .current_dir(cwd)
        .args(["remote", "get-url", "origin"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if has_remote {
        git_output_in(cwd, &["push", "origin", tag])?;
    } else if strict {
        anyhow::bail!("no 'origin' remote found, cannot push tag (strict mode)");
    } else {
        log.warn("no 'origin' remote found, skipping push");
    }
    Ok(())
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
    find_previous_tag_in(
        &std::env::current_dir()?,
        current_tag,
        git_config,
        template_vars,
    )
}

/// Path-taking sibling of [`find_previous_tag`].
pub fn find_previous_tag_in(
    cwd: &Path,
    current_tag: &str,
    git_config: Option<&GitConfig>,
    template_vars: Option<&TemplateVars>,
) -> Result<Option<String>> {
    find_previous_tag_with_prefix_in(cwd, current_tag, git_config, template_vars, None)
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
    find_previous_tag_with_prefix_in(
        &std::env::current_dir()?,
        current_tag,
        git_config,
        template_vars,
        monorepo_prefix,
    )
}

/// Path-taking sibling of [`find_previous_tag_with_prefix`].
pub fn find_previous_tag_with_prefix_in(
    cwd: &Path,
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

    match git_output_in(cwd, &args) {
        Ok(tag) if !tag.is_empty() => Ok(Some(tag)),
        _ => Ok(None),
    }
}

/// Return the SHA of the very first commit in the repository.
///
/// Runs `git rev-list --max-parents=0 HEAD` and returns the first line
/// (repositories with multiple roots will return the oldest).
pub fn get_first_commit() -> Result<String> {
    get_first_commit_in(&std::env::current_dir()?)
}

/// Path-taking sibling of [`get_first_commit`].
pub fn get_first_commit_in(cwd: &Path) -> Result<String> {
    let output = git_output_in(cwd, &["rev-list", "--max-parents=0", "HEAD"])?;
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
    tag_points_at_head_in(&std::env::current_dir()?, tag)
}

/// Path-taking sibling of [`tag_points_at_head`].
pub fn tag_points_at_head_in(cwd: &Path, tag: &str) -> Result<bool> {
    let deref = format!("{}^{{}}", tag);
    let tag_sha = git_output_in(cwd, &["rev-parse", &deref])?;
    let head_sha = git_output_in(cwd, &["rev-parse", "HEAD"])?;
    Ok(tag_sha == head_sha)
}

/// Returns `true` when HEAD coincides with a tag.
///
/// HEAD-with-no-tag is the common case for development branches and
/// must not error; only inability to invoke git at all does.
pub fn head_is_at_tag(repo: &std::path::Path) -> Result<bool> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["describe", "--tags", "--exact-match", "HEAD"])
        .output()
        .map_err(|e| {
            anyhow::anyhow!("failed to invoke git describe --tags --exact-match HEAD: {e}")
        })?;
    Ok(out.status.success())
}

/// `git -C <workspace_root> tag --list --sort=-v:refname '<prefix>*'` —
/// return the list of refs whose name starts with `prefix`, ordered by
/// reverse semver. Returns `Ok(Vec::new())` when git fails (no repo,
/// no tags) so callers can treat absence as a non-error.
pub fn list_tags_with_prefix(
    workspace_root: &std::path::Path,
    prefix: &str,
) -> Result<Vec<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["tag", "--list", "--sort=-v:refname"])
        .arg(format!("{prefix}*"))
        .output()?;
    if !out.status.success() {
        return Ok(Vec::new());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}
