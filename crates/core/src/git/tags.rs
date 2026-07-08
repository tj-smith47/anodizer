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

/// The tag-family prefix used for a crate: the prefix extracted from its
/// `tag_template`, falling back to the `<name>-v` convention when the
/// template is empty or carries no recognised version placeholder.
///
/// Every surface that scans or mints per-crate tags (`tag`, `bump` range
/// inference, `changelog` tag-owner resolution and crate selection) must
/// resolve the SAME family from the same inputs: a drifted fallback makes
/// the last-tag probe come up empty and silently widens the commit range
/// to full history.
pub fn per_crate_tag_prefix(name: &str, tag_template: &str) -> String {
    extract_tag_prefix(tag_template).unwrap_or_else(|| format!("{name}-v"))
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

/// Which form of a tag the `ignore_tags` / `ignore_tag_prefixes` filters match
/// against in a monorepo context.
#[derive(Clone, Copy, PartialEq, Eq)]
enum IgnoreMatchTarget {
    /// Match ignores against the monorepo-stripped tag (so user patterns like
    /// `v*-rc*` work without the prefix). Used by [`find_latest_tag_matching_in`].
    Stripped,
    /// Match ignores against the full tag name — identical to the legacy
    /// `git describe --exclude=<pat>` path regardless of `monorepo_prefix`.
    /// Used by [`smartsemver_previous_tag_in`].
    Full,
}

/// Parse `tags_output` lines into `(SemVer, tag)` pairs, applying the shared
/// monorepo-prefix membership filter, the `ignore_tags` glob filter, the
/// `ignore_tag_prefixes` starts-with filter, and SemVer parsing (stripping the
/// monorepo prefix before parsing). Unsorted — the caller picks ascending vs
/// descending and may layer additional per-site filters (regex match,
/// current-tag exclusion, prerelease skip).
///
/// `ignore_target` selects whether the ignore filters see the stripped or full
/// tag. `skip_empty_ignore_prefix` controls whether an empty rendered
/// `ignore_tag_prefixes` entry is ignored (`true`) or allowed to match every
/// tag (`false`) — preserving each call site's historical behavior.
fn semver_pairs_filtered(
    tags_output: &str,
    monorepo_prefix: Option<&str>,
    ignore_tag_globs: &[glob::Pattern],
    rendered_ignore_prefixes: &[String],
    ignore_target: IgnoreMatchTarget,
    skip_empty_ignore_prefix: bool,
) -> Vec<(SemVer, String)> {
    let ignore_view = |t: &str| -> String {
        match ignore_target {
            IgnoreMatchTarget::Stripped => monorepo_prefix
                .map(|pfx| strip_monorepo_prefix(t, pfx))
                .unwrap_or(t)
                .to_string(),
            IgnoreMatchTarget::Full => t.to_string(),
        }
    };
    tags_output
        .lines()
        .filter(|t| {
            monorepo_prefix
                .map(|pfx| t.starts_with(pfx))
                .unwrap_or(true)
        })
        .filter(|t| {
            let view = ignore_view(t);
            !ignore_tag_globs.iter().any(|g| g.matches(&view))
        })
        .filter(|t| {
            let view = ignore_view(t);
            !rendered_ignore_prefixes.iter().any(|p| {
                (!skip_empty_ignore_prefix || !p.is_empty()) && view.starts_with(p.as_str())
            })
        })
        .filter_map(|t| {
            let tag_for_parse = monorepo_prefix
                .map(|pfx| strip_monorepo_prefix(t, pfx))
                .unwrap_or(t);
            parse_semver_tag(tag_for_parse)
                .ok()
                .map(|v| (v, t.to_string()))
        })
        .collect()
}

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
///     applies to [`find_previous_tag_with_prefix`] only, where `current_tag`
///     determines whether prereleases should be skipped.
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
        monorepo_prefix,
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
            "(dry-run) would create tag {} (\"{}\")",
            tag, message
        ));
        return Ok(());
    }
    git_output_in(cwd, &["tag", "-a", tag, "-m", message])?;

    if super::has_remote_in(cwd, "origin") {
        git_output_in(cwd, &["push", "origin", tag])?;
    } else if strict {
        anyhow::bail!("no 'origin' remote found, cannot push tag (strict mode)");
    } else {
        log.warn("skipped push — no 'origin' remote found");
    }
    Ok(())
}

/// Create an annotated tag locally without pushing.
///
/// Writes `git tag -a <tag> -m <message>` in `cwd`. Does NOT push. The caller
/// is responsible for pushing all tags (typically atomically via
/// [`push_branch_and_tags_atomic_in`]).
pub fn create_tag_local_only(
    cwd: &Path,
    tag: &str,
    message: &str,
    dry_run: bool,
    log: &crate::log::StageLogger,
) -> Result<()> {
    if dry_run {
        log.status(&format!(
            "(dry-run) would create local tag {} (\"{}\")",
            tag, message
        ));
        return Ok(());
    }
    if let Err(e) = git_output_in(cwd, &["tag", "-a", tag, "-m", message]) {
        // A prior `tag` run that committed writeback and created the tag but
        // failed to push leaves this exact debris behind; a re-run must be
        // idempotent when the leftover tag already points at the commit we
        // would tag, and actionable (not raw git noise) when it does not.
        let tag_ref = format!("refs/tags/{}", tag);
        if git_output_in(cwd, &["rev-parse", "--verify", "--quiet", &tag_ref]).is_ok() {
            if tag_points_at_head_in(cwd, tag)? {
                log.status(&format!(
                    "tag {} already exists and points at HEAD; reusing it",
                    tag
                ));
                return Ok(());
            }
            anyhow::bail!(
                "tag {} already exists but points at a different commit than HEAD \
                 (likely left behind by a previous run); run `anodizer tag rollback` \
                 or delete the stale tag (`git tag -d {}`) and re-run",
                tag,
                tag
            );
        }
        return Err(e);
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
/// When `git_config.tag_sort == "smartsemver"`, the lookup switches to a
/// `git tag --list` + Rust-side SemVer sort path so prerelease tags can be
/// filtered out when the current run targets a non-prerelease version.
/// Without this, `git describe --abbrev=0` would return the literal previous
/// tag and an `v0.2.0` release would point its changelog at `v0.2.0-beta.3`.
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
///
/// **`semver` vs `smartsemver` topology:** The default and `semver` modes
/// walk commit ancestry via `git describe --abbrev=0 <tag>^`, so the result
/// reflects the nearest reachable ancestor tag. The `smartsemver` mode instead
/// picks the SemVer-second-highest tag from a flat `git tag --list`, ignoring
/// ancestry. In repos with branch-and-merge history the two paths can return
/// different tags even when prerelease filtering is disabled.
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
    let tag_sort = git_config.and_then(|gc| gc.tag_sort.as_deref());
    if tag_sort == Some("smartsemver") {
        return smartsemver_previous_tag_in(
            cwd,
            current_tag,
            git_config,
            template_vars,
            monorepo_prefix,
        );
    }

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

/// `smartsemver` previous-tag lookup: list all candidate tags, drop
/// `current_tag` itself, filter ignored entries, optionally drop prereleases
/// when the current version is non-prerelease, and return the SemVer-newest
/// remaining tag.
///
/// `current_tag` is removed regardless of how the SemVer comparison would
/// rank it so callers always get the *previous* tag, not the input one.
///
/// **Topology note:** Unlike the legacy `git describe --abbrev=0 <tag>^` path
/// (which walks commit ancestry), this path picks the SemVer-second-highest
/// tag from the flat tag list. In repos with branch-and-merge history the two
/// can differ even when `skip_prereleases` is false.
fn smartsemver_previous_tag_in(
    cwd: &Path,
    current_tag: &str,
    git_config: Option<&GitConfig>,
    template_vars: Option<&TemplateVars>,
    monorepo_prefix: Option<&str>,
) -> Result<Option<String>> {
    let tags_output = git_output_in(cwd, &["tag", "--list"])?;
    if tags_output.is_empty() {
        return Ok(None);
    }

    let (rendered_ignore_tags, rendered_ignore_prefixes) =
        render_ignore_patterns(git_config, template_vars);
    let ignore_tag_globs: Vec<glob::Pattern> = rendered_ignore_tags
        .iter()
        .filter_map(|pat| glob::Pattern::new(pat).ok())
        .collect();

    // Derive the prerelease-skip signal from current_tag itself: when the tag
    // we're releasing parses as a non-prerelease version, filter prereleases
    // from the candidate list so `v0.2.0` points its changelog at `v0.1.0`
    // rather than `v0.2.0-beta.3`.
    let skip_prereleases = {
        let tag_for_signal = monorepo_prefix
            .map(|pfx| strip_monorepo_prefix(current_tag, pfx))
            .unwrap_or(current_tag);
        parse_semver_tag(tag_for_signal)
            .map(|sv| !sv.is_prerelease())
            .unwrap_or(false)
    };

    // Shared monorepo-prefix + ignore-glob + ignore-prefix + SemVer-parse
    // pipeline. Matches ignores against the FULL tag (legacy `git describe
    // --exclude` parity) and skips empty ignore prefixes. The current-tag
    // exclusion and prerelease skip are conjunctive, so layering them on the
    // helper output leaves the final candidate set unchanged.
    let mut candidates: Vec<(SemVer, String)> = semver_pairs_filtered(
        &tags_output,
        monorepo_prefix,
        &ignore_tag_globs,
        &rendered_ignore_prefixes,
        IgnoreMatchTarget::Full,
        true,
    )
    .into_iter()
    .filter(|(_, t)| t != current_tag)
    .filter(|(sv, _)| !skip_prereleases || !sv.is_prerelease())
    .collect();

    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(candidates.into_iter().next().map(|(_, tag)| tag))
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
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
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

/// Return all tags that point at the current HEAD commit.
///
/// Runs `git tag --points-at HEAD`. An empty repository or a HEAD with no
/// tags returns `Ok(vec![])` rather than an error.
pub fn get_tags_at_head() -> Result<Vec<String>> {
    get_tags_at_head_in(&std::env::current_dir()?)
}

/// Path-taking sibling of [`get_tags_at_head`].
pub fn get_tags_at_head_in(cwd: &Path) -> Result<Vec<String>> {
    get_tags_at_sha_in(cwd, "HEAD")
}

/// Return all tags that point at the given commit (any revision spec).
///
/// Runs `git tag --points-at <sha>`. Failures (unknown sha, not a git
/// repo) return `Ok(vec![])` rather than an error so callers can treat
/// "no tags at that ref" as the empty case.
pub fn get_tags_at_sha_in(cwd: &Path, sha: &str) -> Result<Vec<String>> {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(["tag", "--points-at", sha])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| anyhow::anyhow!("failed to invoke git tag --points-at {sha}: {e}"))?;
    if !out.status.success() {
        // A real git failure (corrupt repo, bad sha that isn't merely
        // "unknown") must not masquerade as "no tags here". Warn with the
        // stderr so the empty result isn't silently misread as a clean
        // no-tags case.
        let stderr = String::from_utf8_lossy(&out.stderr);
        tracing::warn!(
            sha = sha,
            stderr = %stderr.trim(),
            "git tag --points-at exited non-zero; returning no tags"
        );
        return Ok(Vec::new());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

/// Delete a local tag (`git tag -d <tag>`). Returns `Ok(())` even when the
/// tag is missing so callers can run the delete idempotently.
///
/// `LC_ALL=C` is pinned on the spawn so the "tag not found" substring
/// match is locale-stable; a non-C locale would translate the message
/// and the idempotency check would silently degrade to bail-on-rerun.
pub fn delete_local_tag_in(cwd: &Path, tag: &str) -> Result<()> {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(["tag", "-d", tag])
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| anyhow::anyhow!("failed to invoke git tag -d {tag}: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        // "tag not found" is fine — caller wanted it gone.
        if stderr.contains("not found") {
            return Ok(());
        }
        anyhow::bail!("git tag -d {tag} failed: {}", stderr.trim());
    }
    Ok(())
}

/// Delete a tag on the `origin` remote (`git push origin :refs/tags/<tag>`).
///
/// Idempotent: when the remote tag is already absent, git exits non-zero
/// with `"remote ref does not exist"` on stderr — that case is treated as
/// success so a rollback re-run after a partially-completed previous pass
/// doesn't surface alarming WARN noise. Any other non-zero exit bubbles
/// up so callers (notably `tag rollback`) can warn-and-continue per tag
/// without aborting the whole pass.
///
/// `LC_ALL=C` is pinned on the spawn so the substring match is
/// locale-stable.
pub fn delete_remote_tag_in(cwd: &Path, tag: &str) -> Result<()> {
    let refspec = format!(":refs/tags/{}", tag);
    let out = Command::new("git")
        .current_dir(cwd)
        .args(["push", "origin", &refspec])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| anyhow::anyhow!("failed to invoke git push origin {refspec}: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        // Already-absent on the remote → treat as success. Covers both
        // `"remote ref does not exist"` (modern git) and the older
        // `"unable to delete '<refspec>': remote ref does not exist"`
        // wording — substring match catches both.
        if stderr.contains("remote ref does not exist") {
            tracing::warn!(
                "remote tag {tag} already absent on origin — treating as deleted (idempotent)"
            );
            return Ok(());
        }
        let raw = format!("git push origin {} failed: {}", refspec, stderr.trim());
        anyhow::bail!("{}", crate::redact::redact_process_env(&raw));
    }
    Ok(())
}

/// Inputs to [`push_branch_and_tags_atomic_in`].
///
/// Groups the push target (`remote` + optional `branch`), the `tags` to push,
/// and the `dry_run` / `strict` toggles so the public helper reads cleanly at
/// call sites instead of carrying a long positional argument list.
///
/// Ref combinations the helper accepts:
/// - `branch = Some` + non-empty `tags` → `git push --atomic <remote> HEAD:refs/heads/<branch> <tags…>`
/// - `branch = Some` + empty `tags` → `git push <remote> HEAD:refs/heads/<branch>`
/// - `branch = None` + non-empty `tags` → `git push --atomic <remote> <tags…>`
/// - `branch = None` + empty `tags` → no-op (logs a warning)
#[derive(Debug, Clone)]
pub struct AtomicPushSpec<'a> {
    /// Remote name to push to (e.g. `"origin"`).
    pub remote: &'a str,
    /// Branch to push HEAD to as `refs/heads/<branch>`, or `None` to push tags only.
    pub branch: Option<&'a str>,
    /// Tags to push.
    pub tags: &'a [String],
    /// When true, log the would-run push instead of executing it.
    pub dry_run: bool,
    /// When true, a missing remote is an error rather than a skipped no-op.
    pub strict: bool,
}

/// Push an optional `branch` and all `tags` to a `remote` atomically.
///
/// See [`AtomicPushSpec`] for the accepted ref combinations.
///
/// When `spec.dry_run` is true, logs what would happen without executing. When
/// the remote does not exist and `spec.strict` is true, returns an error;
/// otherwise logs a warning and returns `Ok(())`.
///
/// HEAD is pushed to `refs/heads/<branch>` (rather than `<branch>` alone) so
/// detached-HEAD checkouts (notably `actions/checkout@v4` with `ref: <sha>`)
/// work without a local branch ref.
///
/// A non-fast-forward rejection — the most likely failure when pushing a
/// version-sync bump commit — is rewrapped with an actionable message before
/// the raw (redacted) git output.
pub fn push_branch_and_tags_atomic_in(
    cwd: &Path,
    spec: &AtomicPushSpec<'_>,
    log: &crate::log::StageLogger,
) -> Result<()> {
    let AtomicPushSpec {
        remote,
        branch,
        tags,
        dry_run,
        strict,
    } = *spec;

    if dry_run {
        let tag_list = tags.join(", ");
        match branch {
            Some(b) => log.status(&format!(
                "(dry-run) would push branch '{}' + tags [{}] to '{}' atomically",
                b, tag_list, remote
            )),
            None => log.status(&format!(
                "(dry-run) would push tags [{}] to '{}' atomically",
                tag_list, remote
            )),
        }
        return Ok(());
    }

    if branch.is_none() && tags.is_empty() {
        log.warn("nothing to push (no branch, no tags)");
        return Ok(());
    }

    if !super::has_remote_in(cwd, remote) {
        if strict {
            anyhow::bail!("no '{remote}' remote found, cannot push (strict mode)");
        }
        log.warn(&format!("skipped push — no '{remote}' remote found"));
        return Ok(());
    }

    // Nothing to push atomically when the tags list is empty — fall back to a
    // plain branch push. --atomic with a single ref is valid git syntax but
    // misleading in log output and unnecessary for atomicity guarantees.
    if tags.is_empty() {
        let Some(b) = branch else {
            // branch=None + tags empty is rejected by the guard above.
            unreachable!("branch is Some whenever tags is empty (guarded above)")
        };
        log.verbose(&format!(
            "no tags to push; pushing branch '{}' to '{}' without --atomic",
            b, remote
        ));
        let head_refspec = format!("HEAD:refs/heads/{}", b);
        return push_with_ff_hint(cwd, &["push", remote, &head_refspec], remote, branch);
    }

    let head_refspec = branch.map(|b| format!("HEAD:refs/heads/{}", b));
    let mut args: Vec<&str> = vec!["push", "--atomic", remote];
    if let Some(ref rs) = head_refspec {
        args.push(rs.as_str());
    }
    for tag in tags {
        args.push(tag.as_str());
    }
    push_with_ff_hint(cwd, &args, remote, branch)
}

/// Run a `git push …` invocation and, on a non-fast-forward rejection, prepend
/// an actionable hint before the raw (already-redacted) git error.
///
/// `branch` names the release branch in the hint when known; falls back to a
/// generic ref message when pushing tags only.
fn push_with_ff_hint(cwd: &Path, args: &[&str], remote: &str, branch: Option<&str>) -> Result<()> {
    match git_output_in(cwd, args) {
        Ok(_) => Ok(()),
        Err(e) => {
            let raw = e.to_string();
            // `! [rejected]` / `non-fast-forward` are git's stable English
            // markers for a stale-ref rejection (`LC_ALL=C` is pinned on the
            // spawn, so the wording does not localize).
            if raw.contains("[rejected]") || raw.contains("non-fast-forward") {
                let target = match branch {
                    Some(b) => format!("{remote}/{b}"),
                    None => format!("a tag ref on '{remote}'"),
                };
                anyhow::bail!(
                    "push rejected (non-fast-forward): {target} moved since checkout. \
                     Pull/rebase the release branch and re-run, or drop --push to push \
                     the tag only.\n{raw}"
                );
            }
            Err(e)
        }
    }
}

#[cfg(test)]
mod delete_tag_tests {
    use super::*;

    /// Build a `<bare-repo>` + working clone pair so we can drive
    /// `delete_remote_tag_in` against a real "origin" without hitting the
    /// network. Returns `(bare, work)`; the working clone has `origin`
    /// pointing at the bare repo.
    fn init_clone_pair() -> (tempfile::TempDir, tempfile::TempDir) {
        let bare = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let run = |dir: &Path, args: &[&str]| {
            let out = anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(args)
                        .current_dir(dir)
                        .env("GIT_AUTHOR_NAME", "t")
                        .env("GIT_AUTHOR_EMAIL", "t@t.com")
                        .env("GIT_COMMITTER_NAME", "t")
                        .env("GIT_COMMITTER_EMAIL", "t@t.com");
                    cmd
                },
                "git",
            );
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(bare.path(), &["init", "--bare", "-b", "master"]);
        run(work.path(), &["init", "-b", "master"]);
        run(work.path(), &["config", "user.email", "t@t.com"]);
        run(work.path(), &["config", "user.name", "t"]);
        run(
            work.path(),
            &[
                "remote",
                "add",
                "origin",
                bare.path().to_str().expect("tempdir path utf-8"),
            ],
        );
        std::fs::write(work.path().join("a"), "0").unwrap();
        run(work.path(), &["add", "."]);
        run(work.path(), &["commit", "-m", "initial"]);
        run(work.path(), &["push", "origin", "master"]);
        (bare, work)
    }

    /// B-R3: deleting a remote tag that doesn't exist must succeed
    /// (idempotent). The git output for that case contains
    /// `"remote ref does not exist"`; the helper must absorb it.
    #[test]
    fn delete_remote_tag_in_is_idempotent_when_remote_tag_missing() {
        let (_bare, work) = init_clone_pair();
        // Tag was never created on the remote — first delete must succeed.
        delete_remote_tag_in(work.path(), "v0.0.0-never-existed")
            .expect("missing remote tag must be treated as already-deleted");
    }

    /// B-R3 follow-on: a real delete still works, and a second delete
    /// of the same tag remains idempotent.
    #[test]
    fn delete_remote_tag_in_succeeds_then_is_idempotent_on_second_call() {
        let (_bare, work) = init_clone_pair();
        let run = |args: &[&str]| {
            let out = anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(args)
                        .current_dir(work.path())
                        .env("GIT_AUTHOR_NAME", "t")
                        .env("GIT_AUTHOR_EMAIL", "t@t.com")
                        .env("GIT_COMMITTER_NAME", "t")
                        .env("GIT_COMMITTER_EMAIL", "t@t.com");
                    cmd
                },
                "git",
            );
            assert!(out.status.success(), "git {args:?} failed");
        };
        run(&["tag", "v1.2.3"]);
        run(&["push", "origin", "v1.2.3"]);
        delete_remote_tag_in(work.path(), "v1.2.3").expect("first remote delete must succeed");
        delete_remote_tag_in(work.path(), "v1.2.3")
            .expect("second remote delete must be a no-op (idempotent)");
    }
}

#[cfg(test)]
mod create_tag_local_only_tests {
    use super::*;

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let out = anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(args)
                        .current_dir(dir.path())
                        .env("GIT_AUTHOR_NAME", "t")
                        .env("GIT_AUTHOR_EMAIL", "t@t.com")
                        .env("GIT_COMMITTER_NAME", "t")
                        .env("GIT_COMMITTER_EMAIL", "t@t.com");
                    cmd
                },
                "git",
            );
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-b", "master"]);
        run(&["config", "user.email", "t@t.com"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.path().join("a"), "0").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "initial"]);
        dir
    }

    fn commit_change(dir: &Path) {
        let run = |args: &[&str]| {
            let out = anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(args)
                        .current_dir(dir)
                        .env("GIT_AUTHOR_NAME", "t")
                        .env("GIT_AUTHOR_EMAIL", "t@t.com")
                        .env("GIT_COMMITTER_NAME", "t")
                        .env("GIT_COMMITTER_EMAIL", "t@t.com");
                    cmd
                },
                "git",
            );
            assert!(out.status.success(), "git {args:?} failed");
        };
        std::fs::write(dir.join("a"), "1").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "next"]);
    }

    #[test]
    fn recreating_tag_at_same_head_is_idempotent() {
        let repo = init_repo();
        let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Quiet);
        create_tag_local_only(repo.path(), "v1.0.0", "Release v1.0.0", false, &log)
            .expect("first create must succeed");
        // Same tag, same HEAD — the leftover-from-failed-push case.
        create_tag_local_only(repo.path(), "v1.0.0", "Release v1.0.0", false, &log)
            .expect("re-creating a tag that already points at HEAD must be idempotent");
    }

    #[test]
    fn recreating_tag_at_different_commit_fails_actionably() {
        let repo = init_repo();
        let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Quiet);
        create_tag_local_only(repo.path(), "v1.0.0", "Release v1.0.0", false, &log)
            .expect("first create must succeed");
        commit_change(repo.path());
        let err = create_tag_local_only(repo.path(), "v1.0.0", "Release v1.0.0", false, &log)
            .expect_err("stale tag at a different commit must fail");
        let msg = err.to_string();
        assert!(msg.contains("v1.0.0"), "error must name the tag: {msg}");
        assert!(
            msg.contains("different commit"),
            "error must name the conflict: {msg}"
        );
        assert!(
            msg.contains("anodizer tag rollback") && msg.contains("git tag -d v1.0.0"),
            "error must suggest a remedy: {msg}"
        );
    }
}
