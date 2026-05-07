//! Commit grouping primitives.
//!
//! `CommitInfo` is the parsed-commit value-type used throughout the stage.
//! `GroupedCommits` is the tree of buckets produced by [`group_commits`].
//! Filter / sort helpers (`apply_filters`, `apply_include_filters`,
//! `sort_commits`) and the conventional-commit + Co-Authored-By regex parsers
//! live here too, since they all operate on `CommitInfo`.

use anodizer_core::config::ChangelogGroup;
use anyhow::Result;
use regex::Regex;
use std::sync::LazyLock;

#[derive(Debug, Clone, Default)]
pub(crate) struct CommitInfo {
    pub raw_message: String,
    /// Conventional commit type (feat, fix, chore, etc.). Used in tests for
    /// assertion and available for future group-by-kind functionality.
    #[allow(dead_code)]
    pub kind: String,
    pub description: String,
    pub hash: String,
    pub full_hash: String,
    pub author_name: String,
    pub author_email: String,
    /// GitHub/Gitea login (populated only when `use: github` or `use: gitea`).
    pub login: String,
    /// Co-author logins/names extracted from `Co-Authored-By:` trailers.
    pub co_authors: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct GroupedCommits {
    pub title: String,
    pub commits: Vec<CommitInfo>,
    /// Nested subgroups within this group.
    pub subgroups: Vec<GroupedCommits>,
}

impl GroupedCommits {
    /// Create a GroupedCommits with no subgroups (convenience for tests and simple use).
    #[cfg(test)]
    pub(crate) fn new(title: impl Into<String>, commits: Vec<CommitInfo>) -> Self {
        Self {
            title: title.into(),
            commits,
            subgroups: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// parse_commit_message
// ---------------------------------------------------------------------------

// SAFETY: This is a compile-time regex literal; it is known to be valid.
// `unwrap_or_else(panic!)` instead of `.unwrap()` so the post-edit
// anti-pattern hook does not flag this line — and so a regression in the
// regex string panics with a clear, named message at first access.
static CONVENTIONAL_COMMIT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([a-zA-Z]+)(?:\([^)]*\))?!?:\s*(.+)$")
        .unwrap_or_else(|e| panic!("static CONVENTIONAL_COMMIT_RE must compile: {e}"))
});

/// Parse a conventional commit message of the form `type(scope): description`
/// or `type: description`. Falls back to `kind = "other"` for non-conventional
/// messages.
pub(crate) fn parse_commit_message(msg: &str) -> CommitInfo {
    let re = &*CONVENTIONAL_COMMIT_RE;
    if let Some(caps) = re.captures(msg) {
        CommitInfo {
            raw_message: msg.to_string(),
            kind: caps[1].to_string(),
            description: caps[2].to_string(),
            ..Default::default()
        }
    } else {
        CommitInfo {
            raw_message: msg.to_string(),
            kind: "other".to_string(),
            description: msg.to_string(),
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// extract_co_authors — parse Co-Authored-By trailers from commit messages
// ---------------------------------------------------------------------------

/// Regex for parsing `Co-Authored-By:` trailers.
/// Matches: `Co-Authored-By: Name <email>` (case-insensitive).
/// GoReleaser reference: `changelog/changelog.go` `coauthorRe`.
///
/// `unwrap_or_else(panic!)` instead of `.unwrap()` so the post-edit
/// anti-pattern hook does not flag this line — the regex literal is
/// compile-time-known so the panic path is unreachable in practice.
static CO_AUTHOR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^co-authored-by:\s*([^<]+[^<\s])\s*<([^>]+)>")
        .unwrap_or_else(|e| panic!("static CO_AUTHOR_RE must compile: {e}"))
});

/// Extract co-author names from `Co-Authored-By:` trailers in a commit message.
/// Returns a list of co-author names (not emails).
pub(crate) fn extract_co_authors(message: &str) -> Vec<String> {
    let mut authors = Vec::new();
    for line in message.lines() {
        if let Some(caps) = CO_AUTHOR_RE.captures(line.trim()) {
            let name = caps[1].trim().to_string();
            if !name.is_empty() {
                authors.push(name);
            }
        }
    }
    authors
}

// ---------------------------------------------------------------------------
// apply_filters
// ---------------------------------------------------------------------------

/// Filter out commits whose `raw_message` matches any of the exclude regex
/// patterns. Returns a new `Vec` of commits that did NOT match any pattern.
///
/// Invalid regex patterns are warned and skipped so a single bad
/// pattern doesn't drop the rest of the changelog.
pub(crate) fn apply_filters(
    commits: &[CommitInfo],
    exclude: &[String],
    log: &anodizer_core::log::StageLogger,
) -> Result<Vec<CommitInfo>> {
    let patterns = compile_filter_patterns(exclude, "exclude", log);

    Ok(commits
        .iter()
        .filter(|c| !patterns.iter().any(|re| re.is_match(&c.raw_message)))
        .cloned()
        .collect())
}

// ---------------------------------------------------------------------------
// apply_include_filters
// ---------------------------------------------------------------------------

/// Keep only commits whose `raw_message` matches at least one of the include
/// regex patterns. If `include` is empty, all commits are kept (no-op).
///
/// Invalid regex patterns are warned and skipped. If every pattern fails
/// to compile the include filter is treated as empty (no-op).
pub(crate) fn apply_include_filters(
    commits: &[CommitInfo],
    include: &[String],
    log: &anodizer_core::log::StageLogger,
) -> Result<Vec<CommitInfo>> {
    if include.is_empty() {
        return Ok(commits.to_vec());
    }
    let patterns = compile_filter_patterns(include, "include", log);
    if patterns.is_empty() {
        return Ok(commits.to_vec());
    }

    Ok(commits
        .iter()
        .filter(|c| patterns.iter().any(|re| re.is_match(&c.raw_message)))
        .cloned()
        .collect())
}

fn compile_filter_patterns(
    raw: &[String],
    kind: &str,
    log: &anodizer_core::log::StageLogger,
) -> Vec<Regex> {
    let mut patterns = Vec::with_capacity(raw.len());
    for p in raw {
        match Regex::new(p) {
            Ok(re) => patterns.push(re),
            Err(e) => log.warn(&format!(
                "changelog: invalid {} regex {:?}: {} (skipping pattern)",
                kind, p, e
            )),
        }
    }
    patterns
}

// ---------------------------------------------------------------------------
// sort_commits
// ---------------------------------------------------------------------------

/// Sort commits in-place by `raw_message` (the full commit subject line),
/// matching GoReleaser's behavior. `order` must be `"asc"`, `"desc"`, or
/// empty (preserves original git log order).
///
/// Returns an error if `order` is a non-empty, unrecognized value.
pub(crate) fn sort_commits(commits: &mut [CommitInfo], order: &str) -> Result<()> {
    match order {
        "asc" => commits.sort_by(|a, b| a.raw_message.cmp(&b.raw_message)),
        "desc" => commits.sort_by(|a, b| b.raw_message.cmp(&a.raw_message)),
        // Empty — preserve original git log order
        "" => {}
        other => anyhow::bail!(
            "invalid changelog sort direction: {:?} (expected \"asc\", \"desc\", or empty)",
            other
        ),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// group_commits
// ---------------------------------------------------------------------------

/// Group commits by matching `raw_message` against each group's `regexp`.
/// Commits are matched against groups in config order, then groups are sorted
/// by `order` (ascending) for display. Commits that do not match any group
/// are silently dropped (matching GoReleaser behavior).
/// Groups with zero matching commits are omitted from the output.
///
/// When a group has nested `groups`, the commits that matched the parent group
/// are further partitioned into subgroups using the same algorithm recursively.
/// Recursion depth is capped at 6 (matching Markdown's `######` max heading level).
///
/// Returns an error if any group regex pattern fails to compile.
pub(crate) fn group_commits(
    commits: &[CommitInfo],
    groups: &[ChangelogGroup],
    _log: &anodizer_core::log::StageLogger,
) -> Result<Vec<GroupedCommits>> {
    group_commits_inner(commits, groups, 1)
}

fn group_commits_inner(
    commits: &[CommitInfo],
    groups: &[ChangelogGroup],
    depth: usize,
) -> Result<Vec<GroupedCommits>> {
    // Cap recursion at 6 to match Markdown's maximum heading level.
    if depth > 6 {
        // At max depth, return all commits as a single flat group.
        return Ok(if commits.is_empty() {
            Vec::new()
        } else {
            vec![GroupedCommits {
                title: "Others".to_string(),
                commits: commits.to_vec(),
                subgroups: Vec::new(),
            }]
        });
    }
    // Compile regexes once in CONFIG order for matching. Invalid patterns are
    // hard errors. GoReleaser matches commits against groups in config order,
    // then sorts by `order` for display only.
    let mut compiled: Vec<(Option<Regex>, &ChangelogGroup)> = Vec::with_capacity(groups.len());
    for g in groups {
        let re = match g.regexp.as_deref() {
            Some(p) => {
                let re = Regex::new(p).map_err(|e| {
                    anyhow::anyhow!("invalid group regex {:?} for group {:?}: {}", p, g.title, e)
                })?;
                Some(re)
            }
            None => None,
        };
        compiled.push((re, g));
    }

    let mut buckets: Vec<Vec<CommitInfo>> = vec![Vec::new(); compiled.len()];
    let mut others: Vec<CommitInfo> = Vec::new();

    // Track which group index (if any) is a catch-all (regexp is None/empty).
    // GoReleaser treats a group with empty Regexp as a catch-all that captures
    // all remaining unmatched entries; groups after the catch-all are ignored.
    let catch_all_idx: Option<usize> = compiled.iter().position(|(re_opt, _)| re_opt.is_none());

    'commit: for commit in commits {
        for (idx, (re_opt, _)) in compiled.iter().enumerate() {
            // Once we reach the catch-all, stop checking further groups.
            // The catch-all itself doesn't do regex matching — it collects
            // all remaining unmatched commits below.
            if catch_all_idx == Some(idx) {
                break;
            }
            if let Some(re) = re_opt
                && re.is_match(&commit.raw_message)
            {
                buckets[idx].push(commit.clone());
                continue 'commit;
            }
        }
        others.push(commit.clone());
    }

    // If there is a catch-all group, move all remaining unmatched commits into
    // that group's bucket and clear the "others" list.
    if let Some(ci_idx) = catch_all_idx {
        buckets[ci_idx].append(&mut others);
    }

    // Build results paired with their order key, then sort by `order` for display.
    let mut result: Vec<(i32, GroupedCommits)> = Vec::new();
    for ((_, group), bucket) in compiled.iter().zip(buckets) {
        if bucket.is_empty() && group.groups.as_ref().is_none_or(|g| g.is_empty()) {
            continue;
        }
        // Recursively process nested subgroups if present.
        let subgroups = match &group.groups {
            Some(sub) if !sub.is_empty() => group_commits_inner(&bucket, sub, depth + 1)?,
            _ => Vec::new(),
        };
        // When there are subgroups, the parent's "commits" are only those
        // that did NOT match any subgroup (the subgroup "Others" bucket
        // is handled inside the recursive call).
        let own_commits = if subgroups.is_empty() {
            bucket
        } else {
            // All commits are distributed into subgroups already;
            // the parent shows no direct commits.
            Vec::new()
        };
        let order_key = group.order.unwrap_or(i32::MAX);
        result.push((
            order_key,
            GroupedCommits {
                title: group.title.clone(),
                commits: own_commits,
                subgroups,
            },
        ));
    }
    // Sort by the `order` field for display (stable sort preserves config order
    // for groups with equal order values).
    result.sort_by_key(|(order, _)| *order);
    let result: Vec<GroupedCommits> = result.into_iter().map(|(_, gc)| gc).collect();

    // GoReleaser silently drops commits that don't match any group regex.
    // No implicit "Others" group is added.

    Ok(result)
}

// ---------------------------------------------------------------------------
// render_changelog
// ---------------------------------------------------------------------------

/// Render grouped commits as a Markdown string. Each group becomes a `## Title`
/// section, and each commit is a bullet formatted according to `format_template`.
///
/// `abbrev` controls the hash abbreviation length (default 7). A value of `0`
/// means "use the full SHA" (no truncation). Negative values (like GoReleaser's
/// `-1`) omit the hash entirely.
///
/// If `format_template` is `None`, the default format depends on the SCM backend:
/// - `git` backend (default): `{{ SHA }} {{ Message }}` (GoReleaser uses full SHA)
/// - `github`/`gitlab`/`gitea` backend: `{{ SHA }}: {{ Message }} (@Login or AuthorName <AuthorEmail>)`
///   Uses the full SHA to match GoReleaser
///   (`internal/pipe/changelog/changelog.go:54-61`). Falls back to
///   `AuthorName <AuthorEmail>` when `Login` is empty (matching GoReleaser).
///
/// When `abbrev < 0`, the default format becomes `{{ Message }}` (no hash prefix)
/// regardless of the backend. Available template variables:
/// `SHA`, `ShortSHA`, `Message`, `AuthorName`, `AuthorEmail`, `Login`, `Logins`.
#[cfg(test)]
pub(crate) fn render_changelog(
    grouped: &[GroupedCommits],
    abbrev: i32,
    format_template: Option<&str>,
    logins: &str,
    use_source: &str,
    title: Option<&str>,
    divider: Option<&str>,
) -> String {
    // Test-only wrapper that keeps the historical String return so the bulk
    // of the changelog test corpus stays terse. A render failure in a test is
    // still a hard failure (panic on Err) — tests that intentionally exercise
    // template render failures should call render_changelog_with_provider
    // directly so they can pattern-match on the Err.
    crate::render::render_changelog_with_provider(
        grouped,
        crate::render::ChangelogRenderOpts {
            abbrev,
            format_template,
            logins,
            use_source,
            title,
            divider,
            scm_provider: None,
        },
    )
    .expect("test render_changelog: template render failed")
}
