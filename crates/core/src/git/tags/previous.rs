//! Previous-tag resolution: the tag that bounds the START of a release range.
//!
//! Two topologies answer it. The default walks commit ancestry
//! (`git describe --tags --abbrev=0 <tag>^`); `tag_sort = "smartsemver"`
//! instead ranks a flat `git tag --list` by SemVer, drops prereleases when the
//! release being cut is not one, and demotes a candidate sitting on the same
//! commit as `current_tag`. Both narrow their candidates with the same
//! [`super::family`] scope the latest-tag probe in [`super::discover`] uses —
//! an unscoped previous-tag search is what ships an empty changelog.

use anyhow::Result;
use std::path::Path;

use crate::config::GitConfig;
use crate::git::semver::{SemVer, parse_semver_tag};
use crate::template::TemplateVars;

use super::family::{
    IgnoreMatchTarget, TagFamilyScope, nightly_exclude_describe_args, render_ignore_patterns,
    semver_pairs_filtered, tag_family_scope,
};
use super::git_output_in;
use super::position::rev_parse_verify_in;

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
    find_previous_tag_scoped_in(
        cwd,
        current_tag,
        git_config,
        template_vars,
        monorepo_prefix.map(TagFamilyScope::Prefix),
    )
}

/// Find the tag preceding `current_tag` **inside the tag family that
/// `tag_template` mints**.
///
/// This is the previous-tag counterpart of
/// [`find_latest_tag_matching_with_prefix`](crate::git::find_latest_tag_matching_with_prefix):
/// both probes must scope to the same family or the range they bound is not a
/// range at all. In a multi-track
/// workspace (`v`, `core-v`, `operator-v`, … all in one repository) an unscoped
/// search returns the nearest tag of ANY track — including a sibling tag on the
/// very commit being released, which collapses the range to nothing and ships
/// an empty changelog.
///
/// `monorepo_prefix` is the fallback family for templates with no version
/// placeholder, and the namespace for a bare `{{ Version }}` template.
pub fn find_previous_tag_in_family(
    current_tag: &str,
    tag_template: &str,
    git_config: Option<&GitConfig>,
    template_vars: Option<&TemplateVars>,
    monorepo_prefix: Option<&str>,
) -> Result<Option<String>> {
    find_previous_tag_in_family_in(
        &std::env::current_dir()?,
        current_tag,
        tag_template,
        git_config,
        template_vars,
        monorepo_prefix,
    )
}

/// Path-taking sibling of [`find_previous_tag_in_family`].
pub fn find_previous_tag_in_family_in(
    cwd: &Path,
    current_tag: &str,
    tag_template: &str,
    git_config: Option<&GitConfig>,
    template_vars: Option<&TemplateVars>,
    monorepo_prefix: Option<&str>,
) -> Result<Option<String>> {
    find_previous_tag_scoped_in(
        cwd,
        current_tag,
        git_config,
        template_vars,
        tag_family_scope(tag_template, monorepo_prefix),
    )
}

/// Shared implementation behind both previous-tag entry points: dispatches to
/// the `smartsemver` list path or the ancestry-walking `git describe` path,
/// with `scope` narrowing the candidate set in either.
fn find_previous_tag_scoped_in(
    cwd: &Path,
    current_tag: &str,
    git_config: Option<&GitConfig>,
    template_vars: Option<&TemplateVars>,
    scope: Option<TagFamilyScope<'_>>,
) -> Result<Option<String>> {
    let tag_sort = git_config.and_then(|gc| gc.tag_sort.as_deref());
    if tag_sort == Some("smartsemver") {
        return smartsemver_previous_tag_in(cwd, current_tag, git_config, template_vars, scope);
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
    // Unconditional nightly exclusion (see `is_nightly_tag`): git-side globs
    // so this path filters the same shapes the list-based paths do.
    exclude_args.extend(nightly_exclude_describe_args());

    // Constrain git describe to the tag family. Without this, git describe
    // returns the nearest reachable tag from ANY track or subproject.
    let match_arg;
    let mut args: Vec<&str> = vec!["describe", "--tags", "--abbrev=0"];
    if let Some(scope) = scope {
        match_arg = format!("--match={}", scope.describe_glob());
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
/// A candidate resolving to the SAME commit as `current_tag` is demoted below
/// every other candidate: the flat tag list has no ancestry to lean on, so a
/// co-located tag (a sibling track's release cut from the same commit, or a
/// re-tag) would otherwise outrank a genuine predecessor and bound a range
/// spanning zero commits. It is still returned when no other candidate
/// survives — that release truly has no new commits, and answering `None`
/// there would widen the range to the whole history.
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
    scope: Option<TagFamilyScope<'_>>,
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
        let tag_for_signal = scope.map(|s| s.strip(current_tag)).unwrap_or(current_tag);
        parse_semver_tag(tag_for_signal)
            .map(|sv| !sv.is_prerelease())
            .unwrap_or(false)
    };

    // Shared family + ignore-glob + ignore-prefix + SemVer-parse pipeline.
    // Matches ignores against the FULL tag (legacy `git describe --exclude`
    // parity) and skips empty ignore prefixes. The current-tag exclusion and
    // prerelease skip are conjunctive, so layering them on the helper output
    // leaves the final candidate set unchanged.
    let mut candidates: Vec<(SemVer, String)> = semver_pairs_filtered(
        &tags_output,
        scope,
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

    // Name inequality is not enough: a tag pointing at the SAME commit as
    // `current_tag` bounds a zero-commit range whatever it is called, so it
    // must never shadow a genuine predecessor further down the list. It is
    // still the right answer when nothing else is left — a release cut from an
    // already-tagged commit really has no new commits, and widening to `None`
    // there would dump the entire history into the release notes. Resolving
    // lazily keeps the common case at one extra `git rev-parse`: the highest
    // candidate is almost always on an older commit.
    let current_commit = tag_commit_in(cwd, current_tag);
    let mut co_located: Option<String> = None;
    for (_, tag) in candidates {
        if current_commit.is_some() && tag_commit_in(cwd, &tag) == current_commit {
            co_located.get_or_insert(tag);
            continue;
        }
        return Ok(Some(tag));
    }
    Ok(co_located)
}

/// The commit a tag resolves to (annotated tags dereferenced), or `None` when
/// the name does not resolve — a tag deleted between the `--list` and this
/// lookup is an ordinary answer, not a failure.
fn tag_commit_in(cwd: &Path, tag: &str) -> Option<String> {
    rev_parse_verify_in(cwd, &format!("{tag}^{{commit}}"))
        .ok()
        .flatten()
}
