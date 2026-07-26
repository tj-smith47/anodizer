//! Tag discovery: listing the repository's tags and picking the LATEST one.
//!
//! Every entry point here answers "which tags exist / which is newest" for a
//! family, applying [`super::family`]'s filters and either git's own
//! `--sort=-version:*` ordering or a Rust-side SemVer sort. The previous-tag
//! (range-start) question lives in [`super::previous`].

use anyhow::Result;
use regex::Regex;
use std::path::Path;
use std::process::Command;

use crate::config::GitConfig;
use crate::git::semver::{SemVer, parse_semver_tag};
use crate::template::TemplateVars;

use super::family::{
    IgnoreMatchTarget, TagFamilyScope, VERSION_PLACEHOLDERS, is_nightly_tag,
    render_ignore_patterns, semver_pairs_filtered, strip_monorepo_prefix,
};
use super::git_output_in;

/// Find the latest tag matching a template pattern.
/// E.g., tag_template "cfgd-core-v{{ .Version }}" → matches tags like "cfgd-core-v1.2.3"
///
/// When `git_config` is provided:
/// - `ignore_tags`: tags matching any entry (glob patterns) are excluded.
///   When `template_vars` is also provided, each entry is rendered through the
///   template engine first.
/// - `ignore_tag_prefixes`: tags starting with any prefix are excluded.
///   Also template-rendered when `template_vars` is provided.
/// - `tag_sort` controls ordering:
///   - `"-version:refname"` (default): Rust-side SemVer sort.
///   - `"-version:creatordate"`: git-delegated sort by tag creation date.
///   - `"semver"`: Rust-side strict SemVer 2.0.0 sort; bypasses git sort even
///     when `prerelease_suffix` is set.
///   - `"smartsemver"`: identical to `"semver"` for this function — pure SemVer
///     ordering with no prerelease filtering. The smartsemver prerelease filter
///     applies to
///     [`find_previous_tag_with_prefix`](crate::git::find_previous_tag_with_prefix)
///     only, where `current_tag` determines whether prereleases should be
///     skipped.
/// - `prerelease_suffix`: for the legacy `-version:*` modes, passed as
///   `-c versionsort.suffix=<suffix>` to git; setting it forces git-delegated
///   sort so the suffix takes effect.
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
    // which interprets globs).
    let ignore_tag_globs: Vec<glob::Pattern> = rendered_ignore_tags
        .iter()
        .filter_map(|pat| glob::Pattern::new(pat).ok())
        .collect();

    let tag_sort = git_config
        .and_then(|gc| gc.tag_sort.as_deref())
        .unwrap_or("-version:refname");
    let prerelease_suffix = git_config.and_then(|gc| gc.prerelease_suffix.as_deref());
    let is_rust_semver_mode = matches!(tag_sort, "semver" | "smartsemver");

    // For semver/smartsemver, prerelease detection is handled Rust-side via
    // SemVer parsing only; prerelease_suffix has no effect on these modes.
    let use_git_sort =
        !is_rust_semver_mode && (tag_sort == "-version:creatordate" || prerelease_suffix.is_some());

    let tags_output = if use_git_sort {
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

    // Shared monorepo-prefix + ignore-glob + ignore-prefix + SemVer-parse
    // pipeline. The tag_template regex is layered on top — it only narrows the
    // kept set (all filters are conjunctive), so applying it after the shared
    // helper leaves the final set and git-preserved order unchanged. Matches
    // ignores against the STRIPPED tag and does NOT skip empty ignore prefixes,
    // preserving this site's historical behavior.
    let mut matching: Vec<(SemVer, String)> = semver_pairs_filtered(
        &tags_output,
        monorepo_prefix.map(TagFamilyScope::Prefix),
        &ignore_tag_globs,
        &rendered_ignore_prefixes,
        IgnoreMatchTarget::Stripped,
        false,
    )
    .into_iter()
    .filter(|(_, t)| {
        let tag_for_match = monorepo_prefix
            .map(|pfx| strip_monorepo_prefix(t, pfx))
            .unwrap_or(t);
        re.is_match(tag_for_match)
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
        .filter(|t| !is_nightly_tag(t))
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

/// List tag names on `remote` via a single `git ls-remote --tags` call.
///
/// Annotated tags appear twice in `ls-remote` output — once as the tag object
/// (`refs/tags/<name>`) and once peeled to the commit (`refs/tags/<name>^{}`);
/// the peeled suffix is stripped and names are deduplicated, so each tag is
/// returned exactly once regardless of whether it is lightweight or annotated.
///
/// Errors (unreachable remote, auth failure, …) propagate so callers can
/// decide whether to fall back to the local tag list.
pub fn list_remote_tag_names_in(cwd: &Path, remote: &str) -> Result<Vec<String>> {
    let output = git_output_in(cwd, &["ls-remote", "--tags", remote])?;
    let mut seen = std::collections::HashSet::new();
    let mut names = Vec::new();
    for line in output.lines() {
        // Each line is `<sha>\trefs/tags/<name>[^{}]`.
        let Some(refname) = line.split('\t').nth(1) else {
            continue;
        };
        let Some(tag) = refname.strip_prefix("refs/tags/") else {
            continue;
        };
        let tag = tag.strip_suffix("^{}").unwrap_or(tag);
        if seen.insert(tag.to_string()) {
            names.push(tag.to_string());
        }
    }
    Ok(names)
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
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
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
