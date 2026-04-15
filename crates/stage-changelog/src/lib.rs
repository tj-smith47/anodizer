use anodize_core::config::ChangelogGroup;
use anodize_core::context::Context;
use anodize_core::git::{
    find_latest_tag_matching_with_prefix, get_all_commits_paths, get_commits_between_paths,
};
use anodize_core::stage::Stage;
use anodize_core::template::{self, TemplateVars};
use anyhow::{Context as _, Result};
use regex::Regex;
use std::sync::LazyLock;

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

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
    fn new(title: impl Into<String>, commits: Vec<CommitInfo>) -> Self {
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
static CONVENTIONAL_COMMIT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([a-zA-Z]+)(?:\([^)]*\))?!?:\s*(.+)$").unwrap());

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
static CO_AUTHOR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^co-authored-by:\s*([^<]+[^<\s])\s*<([^>]+)>").unwrap());

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
/// Returns an error if any regex pattern fails to compile.
pub(crate) fn apply_filters(
    commits: &[CommitInfo],
    exclude: &[String],
    _log: &anodize_core::log::StageLogger,
) -> Result<Vec<CommitInfo>> {
    let mut patterns: Vec<Regex> = Vec::with_capacity(exclude.len());
    for p in exclude {
        let re =
            Regex::new(p).map_err(|e| anyhow::anyhow!("invalid exclude regex {:?}: {}", p, e))?;
        patterns.push(re);
    }

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
/// Returns an error if any regex pattern fails to compile.
pub(crate) fn apply_include_filters(
    commits: &[CommitInfo],
    include: &[String],
    _log: &anodize_core::log::StageLogger,
) -> Result<Vec<CommitInfo>> {
    if include.is_empty() {
        return Ok(commits.to_vec());
    }
    let mut patterns: Vec<Regex> = Vec::with_capacity(include.len());
    for p in include {
        let re =
            Regex::new(p).map_err(|e| anyhow::anyhow!("invalid include regex {:?}: {}", p, e))?;
        patterns.push(re);
    }

    Ok(commits
        .iter()
        .filter(|c| patterns.iter().any(|re| re.is_match(&c.raw_message)))
        .cloned()
        .collect())
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
    _log: &anodize_core::log::StageLogger,
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
/// - `github`/`gitlab`/`gitea` backend: `{{ ShortSHA }}: {{ Message }} (@Login or AuthorName <AuthorEmail>)`
///   Falls back to `AuthorName <AuthorEmail>` when `Login` is empty (matching GoReleaser).
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
    render_changelog_with_provider(
        grouped,
        abbrev,
        format_template,
        logins,
        use_source,
        title,
        divider,
        None,
    )
}

/// Inner render function that accepts an optional SCM provider override for
/// newline handling. GoReleaser's `newLineFor()` checks `ctx.TokenType`, not
/// the changelog source. When `scm_provider` is set, it overrides `use_source`
/// for newline selection (but not for default format template selection).
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_changelog_with_provider(
    grouped: &[GroupedCommits],
    abbrev: i32,
    format_template: Option<&str>,
    logins: &str,
    use_source: &str,
    title: Option<&str>,
    divider: Option<&str>,
    scm_provider: Option<&str>,
) -> String {
    let default_format = if abbrev < 0 {
        "{{ Message }}"
    } else {
        match use_source {
            "github" | "gitlab" | "gitea" => {
                "{{ ShortSHA }}: {{ Message }} ({% if Login %}@{{ Login }}{% else %}{{ AuthorName }} <{{ AuthorEmail }}>{% endif %})"
            }
            // GoReleaser default: `{{ .SHA }} {{ .Message }}` (full hash).
            _ => "{{ SHA }} {{ Message }}",
        }
    };
    let tmpl = format_template.unwrap_or(default_format);
    // GitLab and Gitea need trailing spaces before newlines for markdown line breaks.
    // GoReleaser's newLineFor() checks ctx.TokenType, not the changelog source.
    // See https://docs.gitlab.com/ee/user/markdown.html#newlines
    let nl_source = scm_provider.unwrap_or(use_source);
    let newline = match nl_source {
        "gitlab" | "gitea" => "   \n",
        _ => "\n",
    };
    let mut out = String::new();
    // GoReleaser always emits a title heading (default "Changelog") when groups
    // are configured. When no groups are configured, the title is still emitted.
    let changelog_title = title.unwrap_or("Changelog");
    if !changelog_title.is_empty() {
        out.push_str(&format!("## {}\n\n", changelog_title));
    }
    render_groups(&mut out, grouped, abbrev, tmpl, logins, divider, newline, 3);
    out
}

/// Recursively render grouped commits at the given heading depth.
/// Depth is capped at 6 (matching Markdown's `######` max heading level).
#[allow(clippy::too_many_arguments)]
fn render_groups(
    out: &mut String,
    groups: &[GroupedCommits],
    abbrev: i32,
    tmpl: &str,
    logins: &str,
    divider: Option<&str>,
    newline: &str,
    depth: usize,
) {
    if depth > 6 {
        return;
    }
    let hashes = "#".repeat(depth);
    for (i, group) in groups.iter().enumerate() {
        // Insert divider between groups (not before the first one).
        if i > 0
            && let Some(div) = divider
        {
            out.push_str(div);
            out.push('\n');
        }
        // Only emit a heading when the group has a non-empty title.
        // When no changelog groups are configured, the default group has an
        // empty title so commits render as a plain bullet list without a
        // spurious heading — matching GoReleaser behaviour.
        if !group.title.is_empty() {
            out.push_str(&format!("{} {}\n\n", hashes, group.title));
        }
        for commit in &group.commits {
            render_commit_line(out, commit, abbrev, tmpl, logins, newline);
        }
        // Render nested subgroups one level deeper (no divider at subgroup level).
        if !group.subgroups.is_empty() {
            render_groups(
                out,
                &group.subgroups,
                abbrev,
                tmpl,
                logins,
                None,
                newline,
                depth + 1,
            );
        }
        // Add trailing newline after commits. Skip if this group has subgroups
        // (they add their own spacing) and no direct commits.
        if !group.commits.is_empty() || group.subgroups.is_empty() {
            out.push('\n');
        }
    }
}

/// Render a single commit as a bullet line.
///
/// Template variables available:
/// - `SHA` — full commit hash
/// - `ShortSHA` — abbreviated commit hash (controlled by `abbrev`)
/// - `Message` — commit subject / description
/// - `AuthorName` — commit author name
/// - `AuthorEmail` — commit author email
/// - `Login` — per-commit GitHub username (populated only with `github` backend)
/// - `Logins` — comma-separated list of all GitHub usernames in the release
fn render_commit_line(
    out: &mut String,
    commit: &CommitInfo,
    abbrev: i32,
    tmpl: &str,
    logins: &str,
    newline: &str,
) {
    let short_sha = if abbrev < 0 {
        // Negative abbrev (e.g. GoReleaser's -1) means omit hash entirely.
        String::new()
    } else if abbrev == 0 {
        // abbrev 0 means full SHA (no truncation).
        commit.full_hash.clone()
    } else {
        let a = abbrev as usize;
        if commit.hash.len() > a {
            commit.hash[..a].to_string()
        } else {
            commit.hash.clone()
        }
    };
    let mut vars = TemplateVars::new();
    // GoReleaser parity (changelog.go:262): SHA respects the `abbrev` config.
    // `short_sha` is already computed with abbrev applied above; use it here
    // so templates referencing {{ .SHA }} honor the user's abbreviation.
    vars.set("SHA", &short_sha);
    vars.set("ShortSHA", &short_sha);
    vars.set("Message", &commit.description);
    vars.set("AuthorName", &commit.author_name);
    vars.set("AuthorEmail", &commit.author_email);
    vars.set("Logins", logins);
    vars.set("Login", &commit.login);
    let rendered = template::render(tmpl, &vars).unwrap_or_else(|_| {
        if abbrev < 0 {
            commit.description.clone()
        } else {
            format!("{} {}", short_sha, commit.description)
        }
    });
    out.push_str(&format!("* {}{}", rendered, newline));
}

// ---------------------------------------------------------------------------
// ChangelogStage
// ---------------------------------------------------------------------------

pub struct ChangelogStage;

impl Stage for ChangelogStage {
    fn name(&self) -> &str {
        "changelog"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("changelog");

        // Note: GoReleaser skips changelog in snapshot mode (changelog.go:46-48),
        // but we intentionally generate it for testing/preview purposes.

        let changelog_cfg = ctx.config.changelog.clone();

        // If --release-notes was provided, read the file and use it directly,
        // bypassing all git-based changelog generation.
        if let Some(ref notes_path) = ctx.options.release_notes_path {
            let content = std::fs::read_to_string(notes_path).with_context(|| {
                format!(
                    "changelog: failed to read release notes file: {}",
                    notes_path.display()
                )
            })?;
            log.status(&format!(
                "using custom release notes from {}",
                notes_path.display()
            ));

            // Store the same content for every selected crate.
            let selected = ctx.options.selected_crates.clone();
            let crates: Vec<_> = ctx
                .config
                .crates
                .iter()
                .filter(|c| selected.is_empty() || selected.contains(&c.name))
                .cloned()
                .collect();
            for crate_cfg in &crates {
                ctx.changelogs
                    .insert(crate_cfg.name.clone(), content.clone());
            }

            // Write to dist/CHANGELOG.md (skip during dry-run).
            if !ctx.is_dry_run() {
                let dist = ctx.config.dist.clone();
                std::fs::create_dir_all(&dist)
                    .with_context(|| format!("changelog: create dist dir {}", dist.display()))?;
                let notes_out = dist.join("CHANGELOG.md");
                std::fs::write(&notes_out, &content)
                    .with_context(|| format!("changelog: write {}", notes_out.display()))?;
                log.status(&format!("wrote {}", notes_out.display()));
            }
            return Ok(());
        }

        // If disabled, skip the stage entirely (supports template-conditional disable).
        if let Some(d) = changelog_cfg.as_ref().and_then(|c| c.disable.as_ref())
            && d.is_disabled(|s| ctx.render_template(s))
        {
            log.status("disabled, skipping");
            return Ok(());
        }

        // If `use: github-native`, skip changelog generation and store empty
        // bodies so the release stage can delegate to GitHub's auto-generated
        // release notes.
        let use_source = changelog_cfg
            .as_ref()
            .and_then(|c| c.use_source.clone())
            .unwrap_or_else(|| "git".to_string());

        if use_source == "github-native" {
            log.status("using github-native changelog — skipping local generation");
            ctx.github_native_changelog = true;
            let selected = ctx.options.selected_crates.clone();
            let crates: Vec<_> = ctx
                .config
                .crates
                .iter()
                .filter(|c| selected.is_empty() || selected.contains(&c.name))
                .cloned()
                .collect();
            for crate_cfg in &crates {
                ctx.changelogs.insert(crate_cfg.name.clone(), String::new());
            }
            return Ok(());
        }

        // Validate the use source — only "git", "github", "gitlab", "gitea",
        // and "github-native" (already handled above) are supported.
        if !["git", "github", "gitlab", "gitea"].contains(&use_source.as_str()) {
            anyhow::bail!(
                "changelog: unsupported use source {:?} (expected \"git\", \"github\", \"gitlab\", \"gitea\", or \"github-native\")",
                use_source
            );
        }

        let cfg = changelog_cfg.as_ref();
        let sort_order = cfg.and_then(|c| c.sort.clone()).unwrap_or_default();
        let filters = cfg.and_then(|c| c.filters.as_ref());
        let exclude_filters: Vec<String> =
            filters.and_then(|f| f.exclude.clone()).unwrap_or_default();
        let include_filters: Vec<String> =
            filters.and_then(|f| f.include.clone()).unwrap_or_default();
        let groups: Vec<ChangelogGroup> = cfg.and_then(|c| c.groups.clone()).unwrap_or_default();
        let header: Option<String> = cfg.and_then(|c| c.header.clone());
        let footer: Option<String> = cfg.and_then(|c| c.footer.clone());
        let abbrev: i32 = cfg.and_then(|c| c.abbrev).unwrap_or(0);
        let format_template: Option<String> = cfg.and_then(|c| c.format.clone());
        let changelog_paths: Vec<String> = cfg.and_then(|c| c.paths.clone()).unwrap_or_default();
        let changelog_title: Option<String> = cfg.and_then(|c| c.title.clone());
        let changelog_divider: Option<String> = cfg.and_then(|c| c.divider.clone());

        // Render path templates if configured (paths support template variables).
        let changelog_paths: Vec<String> = changelog_paths
            .into_iter()
            .map(|p| ctx.render_template_strict(&p, "changelog path", &log))
            .collect::<Result<Vec<_>>>()?;

        // Render title template if configured.
        let changelog_title = changelog_title
            .map(|t| ctx.render_template_strict(&t, "changelog title", &log))
            .transpose()?;

        // Render divider template if configured.
        let changelog_divider = changelog_divider
            .map(|d| ctx.render_template_strict(&d, "changelog divider", &log))
            .transpose()?;

        let selected = ctx.options.selected_crates.clone();
        let dist = ctx.config.dist.clone();

        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .cloned()
            .collect();

        let use_github = use_source == "github";
        let use_gitlab = use_source == "gitlab";
        let use_gitea = use_source == "gitea";

        let mut combined_markdown = String::new();

        for crate_cfg in &crates {
            let crate_name = crate_cfg.name.clone();

            // Find the previous tag for this crate by searching for tags that
            // match the crate's tag_template pattern. We must exclude the
            // current tag — otherwise the "latest matching tag" IS the current
            // tag and `commits_between(current_tag, HEAD)` yields zero commits.
            let monorepo_prefix = ctx.config.monorepo_tag_prefix();
            let current_tag = ctx.template_vars().get("Tag").cloned();
            let prev_tag = find_latest_tag_matching_with_prefix(
                &crate_cfg.tag_template,
                ctx.config.git.as_ref(),
                Some(ctx.template_vars()),
                monorepo_prefix,
            )
            .unwrap_or(None)
            .filter(|t| current_tag.as_deref() != Some(t.as_str()));

            // Path filter: changelog-level `paths` takes precedence, then per-crate path,
            // then monorepo.dir as a fallback.
            // GoReleaser's changelog.paths filters git log to specific directories.
            // Only effective with `use: git`.
            let monorepo_dir = ctx.config.monorepo_dir();
            let paths: Vec<String> = if !changelog_paths.is_empty() {
                changelog_paths.clone()
            } else if !crate_cfg.path.is_empty() && crate_cfg.path != "." {
                vec![crate_cfg.path.clone()]
            } else if let Some(dir) = monorepo_dir {
                vec![dir.to_string()]
            } else {
                vec![]
            };

            // Warn when multiple paths are used with the GitHub backend, since
            // the GitHub API only supports filtering by a single path parameter.
            if use_github && paths.len() > 1 {
                log.warn(&format!(
                    "changelog: GitHub API only supports a single path filter; \
                     only the first of {} paths ('{}') will be used for API queries. \
                     Use `use: git` for accurate multi-path filtering.",
                    paths.len(),
                    paths[0]
                ));
            }

            // GitLab and Gitea compare APIs do not support path filtering.
            if (use_gitlab || use_gitea) && !paths.is_empty() {
                log.warn(&format!(
                    "changelog: {} API does not support path filtering; \
                     {} path(s) will be ignored. Use `use: git` for path-based filtering.",
                    if use_gitlab { "GitLab" } else { "Gitea" },
                    paths.len()
                ));
            }

            let (all_commit_infos, logins_str) = if use_github {
                // Fetch commits via the GitHub API for enriched author login info.
                match fetch_github_commits(ctx, &prev_tag, &paths, &log) {
                    Ok((infos, logins)) => (infos, logins),
                    Err(e) => {
                        ctx.strict_guard(
                            &log,
                            &format!(
                                "changelog: GitHub API fetch failed, falling back to git: {}",
                                e
                            ),
                        )?;
                        (
                            fetch_git_commits(&prev_tag, &paths, &crate_name, &log),
                            String::new(),
                        )
                    }
                }
            } else if use_gitlab {
                match fetch_gitlab_commits(ctx, &prev_tag, &log) {
                    Ok((infos, logins)) => (infos, logins),
                    Err(e) => {
                        ctx.strict_guard(
                            &log,
                            &format!(
                                "changelog: GitLab API fetch failed, falling back to git: {}",
                                e
                            ),
                        )?;
                        (
                            fetch_git_commits(&prev_tag, &paths, &crate_name, &log),
                            String::new(),
                        )
                    }
                }
            } else if use_gitea {
                match fetch_gitea_commits(ctx, &prev_tag, &log) {
                    Ok((infos, logins)) => (infos, logins),
                    Err(e) => {
                        ctx.strict_guard(
                            &log,
                            &format!(
                                "changelog: Gitea API fetch failed, falling back to git: {}",
                                e
                            ),
                        )?;
                        (
                            fetch_git_commits(&prev_tag, &paths, &crate_name, &log),
                            String::new(),
                        )
                    }
                }
            } else {
                (
                    fetch_git_commits(&prev_tag, &paths, &crate_name, &log),
                    String::new(),
                )
            };

            // GoReleaser treats include and exclude as mutually exclusive:
            // if include patterns are configured, exclude is completely ignored.
            let filtered = if !include_filters.is_empty() {
                apply_include_filters(&all_commit_infos, &include_filters, &log)?
            } else {
                apply_filters(&all_commit_infos, &exclude_filters, &log)?
            };

            // Sort commits.
            let mut sorted = filtered;
            sort_commits(&mut sorted, &sort_order)?;

            // Group commits.
            let grouped = if groups.is_empty() {
                // No groups configured — render commits as a flat list without
                // any group heading.  GoReleaser only emits a "## Changes"
                // heading when groups ARE configured (for the "others" bucket);
                // with no groups the changelog is a plain bullet list.
                if sorted.is_empty() {
                    vec![]
                } else {
                    vec![GroupedCommits {
                        title: String::new(),
                        commits: sorted,
                        subgroups: Vec::new(),
                    }]
                }
            } else {
                group_commits(&sorted, &groups, &log)?
            };

            // Render the markdown for this crate.
            let scm_provider = ctx.token_type.to_string();
            let markdown = render_changelog_with_provider(
                &grouped,
                abbrev,
                format_template.as_deref(),
                &logins_str,
                &use_source,
                changelog_title.as_deref(),
                changelog_divider.as_deref(),
                Some(&scm_provider),
            );

            // Store per-crate changelog in context for the release stage.
            ctx.changelogs.insert(crate_name.clone(), markdown.clone());

            combined_markdown.push_str(&markdown);
        }

        // Prepend header and append footer if configured, rendering through
        // the template engine so variables like {{ .ProjectName }} are expanded.
        //
        // NOTE: These changelog header/footer values only affect the disk file
        // (dist/CHANGELOG.md). They do NOT affect the GitHub release body.
        // The release stage has its own separate header/footer (in ReleaseConfig)
        // that wraps the per-crate changelog body for the GitHub release.
        let mut final_markdown = String::new();
        if let Some(ref h) = header {
            let rendered = ctx.render_template_strict(h, "changelog header", &log)?;
            final_markdown.push_str(&rendered);
            final_markdown.push_str("\n\n");
        }
        final_markdown.push_str(&combined_markdown);
        if let Some(ref f) = footer {
            let rendered = ctx.render_template_strict(f, "changelog footer", &log)?;
            final_markdown.push('\n');
            final_markdown.push_str(&rendered);
            final_markdown.push('\n');
        }

        // Write to dist/CHANGELOG.md (GoReleaser writes this even in dry-run mode).
        std::fs::create_dir_all(&dist)
            .with_context(|| format!("changelog: create dist dir {}", dist.display()))?;
        let notes_path = dist.join("CHANGELOG.md");
        std::fs::write(&notes_path, &final_markdown)
            .with_context(|| format!("changelog: write {}", notes_path.display()))?;

        log.status(&format!("wrote {}", notes_path.display()));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helper: fetch commits from local git
// ---------------------------------------------------------------------------

fn fetch_git_commits(
    prev_tag: &Option<String>,
    paths: &[String],
    crate_name: &str,
    log: &anodize_core::log::StageLogger,
) -> Vec<CommitInfo> {
    let raw_commits = match prev_tag {
        Some(tag) => get_commits_between_paths(tag, "HEAD", paths).unwrap_or_default(),
        None => {
            log.status(&format!(
                "no previous tag found for crate '{}', using all commits",
                crate_name
            ));
            get_all_commits_paths(paths).unwrap_or_default()
        }
    };

    let mut all_commit_infos = Vec::new();
    for commit in raw_commits {
        let mut info = parse_commit_message(&commit.message);
        info.hash = commit.short_hash.clone();
        info.full_hash = commit.hash.clone();
        info.author_name = commit.author_name.clone();
        info.author_email = commit.author_email.clone();
        // Extract co-authors from the commit body (trailers).
        info.co_authors = extract_co_authors(&commit.body);
        all_commit_infos.push(info);
    }
    all_commit_infos
}

// ---------------------------------------------------------------------------
// Helper: fetch commits from GitHub API (use: github)
// ---------------------------------------------------------------------------

/// Fetch commits via the GitHub API using the `gh` CLI.
/// Returns `(commits, logins_string)` where `logins_string` is a
/// comma-separated list of unique GitHub usernames.
///
/// When `path_filter` is set, commits are filtered to only those touching
/// files under the specified path (for monorepo support).
fn fetch_github_commits(
    ctx: &Context,
    prev_tag: &Option<String>,
    paths: &[String],
    log: &anodize_core::log::StageLogger,
) -> Result<(Vec<CommitInfo>, String)> {
    use anodize_core::git::{detect_github_repo, gh_api_get, gh_api_get_paginated};
    use std::collections::BTreeSet;

    let token = ctx.options.token.as_deref();
    let (owner, repo) = detect_github_repo()?;

    // Build the compare URL. If there is a previous tag, compare tag..HEAD;
    // otherwise list recent commits (first page).
    //
    // The Compare API returns a single JSON object (not a paginated array),
    // so we use `gh_api_get` instead of `gh_api_get_paginated` to avoid
    // corrupting the response by splitting on `]`.
    let (items, compare_files) = if let Some(tag) = prev_tag {
        let endpoint = format!("/repos/{owner}/{repo}/compare/{tag}...HEAD");
        let response = gh_api_get(&endpoint, token)?;
        // Extract the "commits" array from the single compare object.
        let commits_arr = response
            .get("commits")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        // Extract the "files" array for path filtering.
        let files_arr = response
            .get("files")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        (commits_arr, Some(files_arr))
    } else {
        log.status("no previous tag, fetching recent commits from GitHub API");
        // The /commits endpoint returns a paginated array and supports ?path= natively.
        // GitHub API only supports a single path parameter, so use the first one.
        let mut endpoint = format!("/repos/{owner}/{repo}/commits?per_page=100");
        if let Some(first_path) = paths.first() {
            // URL-encode the path to handle spaces, #, ?, & etc.
            let mut encoded = String::with_capacity(first_path.len());
            for b in first_path.bytes() {
                match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                        encoded.push(b as char)
                    }
                    _ => encoded.push_str(&format!("%{:02X}", b)),
                }
            }
            endpoint.push_str(&format!("&path={}", encoded));
        }
        (gh_api_get_paginated(&endpoint, token)?, None)
    };

    // When using the Compare API with a path filter, filter commits to only
    // those that touched files under the specified paths.
    //
    // LIMITATION: The Compare API returns a flat "files" list for the entire
    // diff, not per-commit file lists. We can only check whether *any* changed
    // file matches *any* path prefix. If a match is found, ALL commits pass
    // through (we cannot determine which specific commits touched which files).
    // If no files match any path prefix, all commits are excluded.
    //
    // This is a coarser filter than the `git log -- path1 path2` approach used
    // by the git backend, which filters at the per-commit level. For precise
    // multi-path filtering, users should prefer `use: git` over `use: github`.
    let filtered_shas: Option<std::collections::HashSet<String>> = if !paths.is_empty() {
        if let Some(ref files) = compare_files {
            let has_matching_files = files.iter().any(|f| {
                f.get("filename")
                    .and_then(|v| v.as_str())
                    .is_some_and(|name| paths.iter().any(|p| name.starts_with(p.as_str())))
            });
            if !has_matching_files {
                Some(std::collections::HashSet::new()) // empty set = filter out all
            } else {
                None // no filtering needed, all commits are relevant
            }
        } else {
            None
        }
    } else {
        None
    };

    let mut logins = BTreeSet::new();
    let mut all_commit_infos = Vec::new();

    for item in &items {
        let sha = item.get("sha").and_then(|v| v.as_str()).unwrap_or_default();

        // When path filtering is active via the Compare API, skip commits that
        // don't match (empty set means no files matched the path prefix).
        if let Some(ref allowed) = filtered_shas
            && !allowed.contains(sha)
        {
            continue;
        }

        let short_sha = if sha.len() >= 7 { &sha[..7] } else { sha };
        let message = item
            .pointer("/commit/message")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        // Use first line of the commit message as the subject.
        let subject = message.lines().next().unwrap_or(message);
        let author_name = item
            .pointer("/commit/author/name")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let author_email = item
            .pointer("/commit/author/email")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let login = item
            .pointer("/author/login")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        if !login.is_empty() {
            logins.insert(login.to_string());
        }

        // Extract co-authors from the full commit message body.
        let co_authors = extract_co_authors(message);
        for co_author in &co_authors {
            // Co-authors don't have GitHub logins in the trailer, just names.
            // We still add them for visibility in the Logins variable.
            logins.insert(co_author.clone());
        }

        let mut info = parse_commit_message(subject);
        info.hash = short_sha.to_string();
        info.full_hash = sha.to_string();
        info.author_name = author_name.to_string();
        info.author_email = author_email.to_string();
        info.login = login.to_string();
        info.co_authors = co_authors;
        all_commit_infos.push(info);
    }

    let logins_str = logins.into_iter().collect::<Vec<_>>().join(",");
    Ok((all_commit_infos, logins_str))
}

// ---------------------------------------------------------------------------
// Helper: fetch commits from GitLab API (use: gitlab)
// ---------------------------------------------------------------------------

/// Fetch commits via the GitLab Repository Compare API.
///
/// Uses `GET {api}/projects/{project_id}/repository/compare?from={prev}&to={current}`
/// to retrieve commits between tags. Authentication is via `PRIVATE-TOKEN` header
/// (or `JOB-TOKEN` when `use_job_token` is configured in `gitlab_urls`).
///
/// Falls back to an error (caller falls back to git) when no token is available
/// or the API call fails.
fn fetch_gitlab_commits(
    ctx: &Context,
    prev_tag: &Option<String>,
    log: &anodize_core::log::StageLogger,
) -> Result<(Vec<CommitInfo>, String)> {
    use anodize_core::git::detect_owner_repo;

    let token = ctx
        .options
        .token
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("gitlab changelog: no token available"))?;

    let gitlab_urls = ctx.config.gitlab_urls.clone().unwrap_or_default();
    let api_url = gitlab_urls
        .api
        .unwrap_or_else(|| "https://gitlab.com/api/v4".to_string());
    let api = api_url.trim_end_matches('/');
    let use_job_token = gitlab_urls.use_job_token.unwrap_or(false);
    let skip_tls = gitlab_urls.skip_tls_verify.unwrap_or(false);

    // Derive project ID from git remote (owner/repo), URL-encode slashes.
    let (owner, repo) = detect_owner_repo()?;
    let project_path = if owner.is_empty() {
        repo.clone()
    } else {
        format!("{}/{}", owner, repo)
    };
    // URL-encode the project path (slashes become %2F).
    let encoded_project = project_path.replace('/', "%2F");

    let auth_header = if use_job_token {
        "JOB-TOKEN"
    } else {
        "PRIVATE-TOKEN"
    };

    let from_ref = prev_tag.as_deref().unwrap_or("");
    let to_ref = "HEAD";

    let url = if from_ref.is_empty() {
        // No previous tag — list recent commits.
        format!(
            "{}/projects/{}/repository/commits?per_page=100&ref_name={}",
            api, encoded_project, to_ref
        )
    } else {
        format!(
            "{}/projects/{}/repository/compare?from={}&to={}",
            api, encoded_project, from_ref, to_ref
        )
    };

    log.status(&format!("fetching commits from GitLab API: {}", url));

    let client = reqwest::blocking::Client::builder()
        .danger_accept_invalid_certs(skip_tls)
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let response = client
        .get(&url)
        .header(auth_header, token)
        .send()
        .map_err(|e| anyhow::anyhow!("gitlab changelog: API request failed: {}", e))?;

    if !response.status().is_success() {
        anyhow::bail!(
            "gitlab changelog: API returned status {} for {}",
            response.status(),
            url
        );
    }

    let body: serde_json::Value = response
        .json()
        .map_err(|e| anyhow::anyhow!("gitlab changelog: failed to parse response: {}", e))?;

    // The compare endpoint returns { "commits": [...] }.
    // The commits listing endpoint returns [...] directly.
    let commits_arr = if let Some(arr) = body.get("commits").and_then(|v| v.as_array()) {
        arr.clone()
    } else if let Some(arr) = body.as_array() {
        arr.clone()
    } else {
        anyhow::bail!("gitlab changelog: unexpected response format");
    };

    let mut all_commit_infos = Vec::new();

    for item in &commits_arr {
        let sha = item.get("id").and_then(|v| v.as_str()).unwrap_or_default();
        let short_sha = if sha.len() >= 7 { &sha[..7] } else { sha };
        let message = item
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        // Use first line of the commit message as the subject.
        let subject = message.lines().next().unwrap_or(message);
        let author_name = item
            .get("author_name")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let author_email = item
            .get("author_email")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        let mut info = parse_commit_message(subject);
        info.hash = short_sha.to_string();
        info.full_hash = sha.to_string();
        info.author_name = author_name.to_string();
        info.author_email = author_email.to_string();
        // GitLab's compare API does not include login information,
        // but we can extract co-authors from commit message trailers.
        info.co_authors = extract_co_authors(message);
        all_commit_infos.push(info);
    }

    log.status(&format!(
        "fetched {} commits from GitLab API",
        all_commit_infos.len()
    ));

    // Aggregate co-author names into logins (GitLab has no username API).
    let mut logins = std::collections::BTreeSet::new();
    for info in &all_commit_infos {
        for co_author in &info.co_authors {
            logins.insert(co_author.clone());
        }
    }
    let logins_str = logins.into_iter().collect::<Vec<_>>().join(",");
    Ok((all_commit_infos, logins_str))
}

// ---------------------------------------------------------------------------
// Helper: fetch commits from Gitea API (use: gitea)
// ---------------------------------------------------------------------------

/// Fetch commits via the Gitea Compare API.
///
/// Uses `GET {api}/repos/{owner}/{repo}/compare/{prev}...{current}` to retrieve
/// commits between tags. Authentication is via `Authorization: token {value}`.
///
/// Falls back to an error (caller falls back to git) when no token is available
/// or the API call fails.
fn fetch_gitea_commits(
    ctx: &Context,
    prev_tag: &Option<String>,
    log: &anodize_core::log::StageLogger,
) -> Result<(Vec<CommitInfo>, String)> {
    use anodize_core::git::detect_owner_repo;
    use std::collections::BTreeSet;

    let token = ctx
        .options
        .token
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("gitea changelog: no token available"))?;

    let gitea_urls = ctx.config.gitea_urls.clone().unwrap_or_default();
    let api_url = gitea_urls
        .api
        .unwrap_or_else(|| "https://gitea.com/api/v1".to_string());
    let api = api_url.trim_end_matches('/');
    let skip_tls = gitea_urls.skip_tls_verify.unwrap_or(false);

    let (owner, repo) = detect_owner_repo()?;

    let url = if let Some(prev) = prev_tag {
        // Compare endpoint: GET /api/v1/repos/:owner/:repo/compare/:base...:head
        format!("{}/repos/{}/{}/compare/{}...HEAD", api, owner, repo, prev)
    } else {
        // No previous tag — list recent commits via the Commits API (not
        // /git/commits which returns a different JSON shape without the
        // top-level author object). This endpoint returns the same
        // GitHub-style commit objects as the compare endpoint.
        format!(
            "{}/repos/{}/{}/commits?sha=HEAD&limit=100",
            api, owner, repo
        )
    };

    log.status(&format!("fetching commits from Gitea API: {}", url));

    let client = reqwest::blocking::Client::builder()
        .danger_accept_invalid_certs(skip_tls)
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let response = client
        .get(&url)
        .header("Authorization", format!("token {}", token))
        .send()
        .map_err(|e| anyhow::anyhow!("gitea changelog: API request failed: {}", e))?;

    if !response.status().is_success() {
        anyhow::bail!(
            "gitea changelog: API returned status {} for {}",
            response.status(),
            url
        );
    }

    let body: serde_json::Value = response
        .json()
        .map_err(|e| anyhow::anyhow!("gitea changelog: failed to parse response: {}", e))?;

    // The compare endpoint returns { "commits": [...] }.
    // The commits listing endpoint returns [...] directly.
    let commits_arr = if let Some(arr) = body.get("commits").and_then(|v| v.as_array()) {
        arr.clone()
    } else if let Some(arr) = body.as_array() {
        arr.clone()
    } else {
        anyhow::bail!("gitea changelog: unexpected response format");
    };

    let mut logins = BTreeSet::new();
    let mut all_commit_infos = Vec::new();

    for item in &commits_arr {
        // Gitea compare response: commits have "sha", "commit.message",
        // "author.full_name", "author.email", "author.login".
        let sha = item.get("sha").and_then(|v| v.as_str()).unwrap_or_default();
        let short_sha = if sha.len() >= 7 { &sha[..7] } else { sha };
        let message = item
            .pointer("/commit/message")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let subject = message.lines().next().unwrap_or(message);

        // Author info: try top-level "author" object first (Gitea API user),
        // then fall back to commit-level author fields.
        let author_name = item
            .pointer("/author/full_name")
            .and_then(|v| v.as_str())
            .or_else(|| item.pointer("/commit/author/name").and_then(|v| v.as_str()))
            .unwrap_or_default();
        let author_email = item
            .pointer("/author/email")
            .and_then(|v| v.as_str())
            .or_else(|| {
                item.pointer("/commit/author/email")
                    .and_then(|v| v.as_str())
            })
            .unwrap_or_default();
        let login = item
            .pointer("/author/login")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        if !login.is_empty() {
            logins.insert(login.to_string());
        }

        let mut info = parse_commit_message(subject);
        info.hash = short_sha.to_string();
        info.full_hash = sha.to_string();
        info.author_name = author_name.to_string();
        info.author_email = author_email.to_string();
        info.login = login.to_string();

        // Extract co-authors from the full commit message body.
        let co_authors = extract_co_authors(message);
        for co_author in &co_authors {
            logins.insert(co_author.clone());
        }
        info.co_authors = co_authors;

        all_commit_infos.push(info);
    }

    log.status(&format!(
        "fetched {} commits from Gitea API",
        all_commit_infos.len()
    ));

    let logins_str = logins.into_iter().collect::<Vec<_>>().join(",");
    Ok((all_commit_infos, logins_str))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use anodize_core::log::{StageLogger, Verbosity};
    use serial_test::serial;

    fn test_logger() -> StageLogger {
        StageLogger::new("changelog", Verbosity::Normal)
    }

    /// Build a test `CommitInfo` with just the fields tests typically set.
    /// `full_hash` defaults to the same value as `hash` so that the default
    /// format template (`{{ SHA }} {{ Message }}`) produces meaningful output.
    fn ci(raw_message: &str, kind: &str, description: &str, hash: &str) -> CommitInfo {
        CommitInfo {
            raw_message: raw_message.into(),
            kind: kind.into(),
            description: description.into(),
            hash: hash.into(),
            full_hash: hash.into(),
            ..Default::default()
        }
    }

    // ---- extract_co_authors tests ----

    #[test]
    fn test_extract_co_authors_basic() {
        let msg =
            "feat: add feature\n\nSome details.\n\nCo-Authored-By: Alice Smith <alice@example.com>";
        let authors = extract_co_authors(msg);
        assert_eq!(authors, vec!["Alice Smith"]);
    }

    #[test]
    fn test_extract_co_authors_multiple() {
        let msg =
            "fix: bug\n\nCo-Authored-By: Alice <a@x.com>\nCo-Authored-By: Bob Jones <b@x.com>";
        let authors = extract_co_authors(msg);
        assert_eq!(authors, vec!["Alice", "Bob Jones"]);
    }

    #[test]
    fn test_extract_co_authors_case_insensitive() {
        let msg =
            "feat: thing\n\nco-authored-by: Jane <jane@x.com>\nCO-AUTHORED-BY: Joe <joe@x.com>";
        let authors = extract_co_authors(msg);
        assert_eq!(authors, vec!["Jane", "Joe"]);
    }

    #[test]
    fn test_extract_co_authors_empty_message() {
        assert!(extract_co_authors("").is_empty());
    }

    #[test]
    fn test_extract_co_authors_no_trailers() {
        assert!(extract_co_authors("feat: just a commit\n\nsome body").is_empty());
    }

    // ---- parse_commit_message tests ----

    #[test]
    fn test_parse_conventional_commit() {
        let info = parse_commit_message("feat: add new feature");
        assert_eq!(info.kind, "feat");
        assert_eq!(info.description, "add new feature");
    }

    #[test]
    fn test_parse_non_conventional() {
        let info = parse_commit_message("just a regular commit");
        assert_eq!(info.kind, "other");
        assert_eq!(info.description, "just a regular commit");
    }

    #[test]
    fn test_parse_scoped_commit() {
        let info = parse_commit_message("fix(core): resolve panic");
        assert_eq!(info.kind, "fix");
        assert_eq!(info.description, "resolve panic");
    }

    #[test]
    fn test_group_commits() {
        let commits = vec![
            ci("feat: new thing", "feat", "new thing", "abc"),
            ci("fix: broken thing", "fix", "broken thing", "def"),
            ci("feat: another thing", "feat", "another thing", "ghi"),
        ];
        let groups = vec![
            ChangelogGroup {
                title: "Features".into(),
                regexp: Some("^feat".into()),
                order: Some(0),
                groups: None,
            },
            ChangelogGroup {
                title: "Bug Fixes".into(),
                regexp: Some("^fix".into()),
                order: Some(1),
                groups: None,
            },
        ];
        let result = group_commits(&commits, &groups, &test_logger()).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].title, "Features");
        assert_eq!(result[0].commits.len(), 2);
        assert_eq!(result[1].title, "Bug Fixes");
        assert_eq!(result[1].commits.len(), 1);
    }

    #[test]
    fn test_apply_filters() {
        let commits = vec![
            ci("docs: update readme", "docs", "update readme", "a"),
            ci("feat: new feature", "feat", "new feature", "b"),
            ci("ci: fix pipeline", "ci", "fix pipeline", "c"),
        ];
        let filters = vec!["^docs:".to_string(), "^ci:".to_string()];
        let filtered = apply_filters(&commits, &filters, &test_logger()).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].kind, "feat");
    }

    #[test]
    fn test_render_changelog() {
        let grouped = vec![
            GroupedCommits {
                title: "Features".into(),
                commits: vec![ci("feat: add X", "feat", "add X", "abc1234")],
                subgroups: Vec::new(),
            },
            GroupedCommits {
                title: "Bug Fixes".into(),
                commits: vec![ci("fix: fix Y", "fix", "fix Y", "def5678")],
                subgroups: Vec::new(),
            },
        ];
        let md = render_changelog(&grouped, 7, None, "", "git", None, None);
        assert!(md.contains("## Features"));
        assert!(md.contains("add X"));
        assert!(md.contains("## Bug Fixes"));
        assert!(md.contains("fix Y"));
        assert!(md.contains("abc1234"));
    }

    #[test]
    fn test_sort_asc() {
        let mut commits = vec![ci("b", "feat", "b", "2"), ci("a", "feat", "a", "1")];
        sort_commits(&mut commits, "asc").unwrap();
        assert_eq!(commits[0].raw_message, "a");
    }

    #[test]
    fn test_sort_desc() {
        let mut commits = vec![ci("a", "feat", "a", "1"), ci("b", "feat", "b", "2")];
        sort_commits(&mut commits, "desc").unwrap();
        assert_eq!(commits[0].raw_message, "b");
    }

    #[test]
    fn test_group_commits_others_bucket() {
        // GoReleaser silently drops unmatched commits (no implicit "Others" group).
        let commits = vec![
            ci("feat: new thing", "feat", "new thing", "abc"),
            ci("chore: update deps", "chore", "update deps", "xyz"),
        ];
        let groups = vec![ChangelogGroup {
            title: "Features".into(),
            regexp: Some("^feat".into()),
            order: Some(0),
            groups: None,
        }];
        let result = group_commits(&commits, &groups, &test_logger()).unwrap();
        assert_eq!(result.len(), 1, "unmatched commits should be dropped");
        assert_eq!(result[0].title, "Features");
        assert_eq!(result[0].commits.len(), 1);
    }

    #[test]
    fn test_group_commits_empty_group_omitted() {
        let commits = vec![ci("feat: only feat", "feat", "only feat", "abc")];
        let groups = vec![
            ChangelogGroup {
                title: "Features".into(),
                regexp: Some("^feat".into()),
                order: Some(0),
                groups: None,
            },
            ChangelogGroup {
                title: "Bug Fixes".into(),
                regexp: Some("^fix".into()),
                order: Some(1),
                groups: None,
            },
        ];
        let result = group_commits(&commits, &groups, &test_logger()).unwrap();
        // "Bug Fixes" has no commits, should be omitted
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].title, "Features");
    }

    #[test]
    fn test_render_changelog_short_hash() {
        // When hash is exactly 7 chars, it should appear as-is
        let grouped = vec![GroupedCommits {
            title: String::new(),
            commits: vec![ci(
                "feat: short hash test",
                "feat",
                "short hash test",
                "abc1234",
            )],
            subgroups: Vec::new(),
        }];
        let md = render_changelog(&grouped, 7, None, "", "git", None, None);
        assert!(md.contains("abc1234 short hash test"));
    }

    #[test]
    fn test_parse_breaking_change() {
        // Conventional commit breaking change marker `!` before `:`
        let info = parse_commit_message("feat!: drop support for old API");
        assert_eq!(info.kind, "feat");
        assert_eq!(info.description, "drop support for old API");
    }

    #[test]
    fn test_apply_filters_empty_exclude() {
        let commits = vec![ci("feat: something", "feat", "something", "a")];
        let filtered = apply_filters(&commits, &[], &test_logger()).unwrap();
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn test_render_changelog_with_header_and_footer() {
        let grouped = vec![GroupedCommits {
            title: "Features".into(),
            commits: vec![ci("feat: add X", "feat", "add X", "abc1234")],
            subgroups: Vec::new(),
        }];
        let body = render_changelog(&grouped, 7, None, "", "git", None, None);

        // Simulate the header/footer wrapping logic from ChangelogStage::run
        // (uses double-newline separator matching GoReleaser)
        let header = "# My Release Notes";
        let footer = "---\nGenerated by anodize";
        let mut final_md = String::new();
        final_md.push_str(header);
        final_md.push_str("\n\n");
        final_md.push_str(&body);
        final_md.push('\n');
        final_md.push_str(footer);
        final_md.push('\n');

        assert!(final_md.starts_with("# My Release Notes\n\n"));
        assert!(final_md.contains("## Features"));
        assert!(final_md.contains("add X"));
        assert!(final_md.ends_with("Generated by anodize\n"));
    }

    #[test]
    fn test_render_changelog_with_header_only() {
        let grouped = vec![GroupedCommits {
            title: String::new(),
            commits: vec![ci("fix: bug", "fix", "bug", "def5678")],
            subgroups: Vec::new(),
        }];
        let body = render_changelog(&grouped, 7, None, "", "git", None, None);

        let header = "# Release Notes";
        let mut final_md = String::new();
        final_md.push_str(header);
        final_md.push_str("\n\n");
        final_md.push_str(&body);

        // Body now includes default "## Changelog" title (matching GoReleaser)
        assert!(final_md.starts_with("# Release Notes\n\n## Changelog\n\n* def5678 bug"));
    }

    #[test]
    fn test_render_changelog_with_footer_only() {
        let grouped = vec![GroupedCommits {
            title: String::new(),
            commits: vec![ci("fix: bug", "fix", "bug", "def5678")],
            subgroups: Vec::new(),
        }];
        let body = render_changelog(&grouped, 7, None, "", "git", None, None);

        let footer = "-- end --";
        let mut final_md = String::new();
        final_md.push_str(&body);
        final_md.push_str(footer);
        final_md.push('\n');

        // No groups configured => no "## Changes" heading
        assert!(!final_md.contains("## Changes"));
        assert!(final_md.contains("* def5678 bug"));
        assert!(final_md.ends_with("-- end --\n"));
    }

    #[test]
    fn test_changelog_stage_disabled_skips() {
        use anodize_core::config::{ChangelogConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let mut config = Config::default();
        config.project_name = "test".to_string();
        config.changelog = Some(ChangelogConfig {
            disable: Some(anodize_core::config::StringOrBool::Bool(true)),
            ..Default::default()
        });
        config.crates = vec![CrateConfig {
            name: "test".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }];

        let mut ctx = Context::new(config, ContextOptions::default());

        let stage = ChangelogStage;
        // Should succeed without errors (skips immediately).
        stage.run(&mut ctx).unwrap();

        // No changelogs should be generated.
        assert!(ctx.changelogs.is_empty());
    }

    // -----------------------------------------------------------------------
    // Tests for include filters
    // -----------------------------------------------------------------------

    #[test]
    fn test_apply_include_filters_matching() {
        let commits = vec![
            ci("feat: add login", "feat", "add login", "a"),
            ci("fix: crash on start", "fix", "crash on start", "b"),
            ci("docs: update readme", "docs", "update readme", "c"),
            ci("chore: bump deps", "chore", "bump deps", "d"),
        ];
        let include = vec!["^feat".to_string(), "^fix".to_string()];
        let result = apply_include_filters(&commits, &include, &test_logger()).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].kind, "feat");
        assert_eq!(result[1].kind, "fix");
    }

    #[test]
    fn test_apply_include_filters_no_match() {
        let commits = vec![
            ci("docs: update readme", "docs", "update readme", "a"),
            ci("chore: bump deps", "chore", "bump deps", "b"),
        ];
        let include = vec!["^feat".to_string()];
        let result = apply_include_filters(&commits, &include, &test_logger()).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_apply_include_filters_empty_keeps_all() {
        let commits = vec![
            ci("feat: something", "feat", "something", "a"),
            ci("fix: something else", "fix", "something else", "b"),
        ];
        let result = apply_include_filters(&commits, &[], &test_logger()).unwrap();
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_apply_include_filters_invalid_regex_is_error() {
        let commits = vec![ci("feat: good", "feat", "good", "a")];
        // Invalid regex should be a hard error.
        let include = vec!["[invalid".to_string(), "^feat".to_string()];
        let result = apply_include_filters(&commits, &include, &test_logger());
        assert!(
            result.is_err(),
            "invalid include regex should return an error"
        );
    }

    // -----------------------------------------------------------------------
    // Tests for abbrev
    // -----------------------------------------------------------------------

    #[test]
    fn test_abbrev_controls_hash_length() {
        let grouped = vec![GroupedCommits {
            title: String::new(),
            commits: vec![ci(
                "feat: test abbrev",
                "feat",
                "test abbrev",
                "abc1234567890",
            )],
            subgroups: Vec::new(),
        }];
        // GoReleaser parity: `{{ .SHA }}` respects abbrev. abbrev=5 → "abc12".
        let md = render_changelog(&grouped, 5, None, "", "git", None, None);
        assert!(
            md.contains("abc12 test abbrev"),
            "expected 'abc12 test abbrev' in: {}",
            md
        );
    }

    #[test]
    fn test_abbrev_longer_than_hash_uses_full_hash() {
        let grouped = vec![GroupedCommits {
            title: String::new(),
            commits: vec![ci("feat: short", "feat", "short", "abc")],
            subgroups: Vec::new(),
        }];
        // Default format uses `{{ SHA }}` (full hash), so abbrev is irrelevant.
        let md = render_changelog(&grouped, 10, None, "", "git", None, None);
        assert!(md.contains("abc short"), "expected 'abc short' in: {}", md);
    }

    #[test]
    fn test_abbrev_default_is_seven() {
        let grouped = vec![GroupedCommits {
            title: String::new(),
            commits: vec![ci(
                "feat: default abbrev",
                "feat",
                "default abbrev",
                "abc1234def5678",
            )],
            subgroups: Vec::new(),
        }];
        // GoReleaser parity: `{{ .SHA }}` respects abbrev (7) → short hash.
        let md = render_changelog(&grouped, 7, None, "", "git", None, None);
        assert!(
            md.contains("abc1234 default abbrev"),
            "expected 'abc1234 default abbrev' in: {}",
            md
        );
    }

    // -----------------------------------------------------------------------
    // Tests for config parsing (include, use_source, abbrev)
    // -----------------------------------------------------------------------

    #[test]
    fn test_config_parse_filters_include() {
        let yaml = r#"
sort: asc
filters:
  include:
    - "^feat"
    - "^fix"
  exclude:
    - "^chore"
"#;
        let cfg: anodize_core::config::ChangelogConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let include = cfg.filters.as_ref().unwrap().include.as_ref().unwrap();
        assert_eq!(include.len(), 2);
        assert_eq!(include[0], "^feat");
        assert_eq!(include[1], "^fix");
        let exclude = cfg.filters.as_ref().unwrap().exclude.as_ref().unwrap();
        assert_eq!(exclude.len(), 1);
    }

    #[test]
    fn test_config_parse_use_source_github_native() {
        let yaml = r#"
use: github-native
"#;
        let cfg: anodize_core::config::ChangelogConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.use_source.as_deref(), Some("github-native"));
    }

    #[test]
    fn test_config_parse_abbrev() {
        let yaml = r#"
abbrev: 10
"#;
        let cfg: anodize_core::config::ChangelogConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.abbrev, Some(10));
    }

    // -----------------------------------------------------------------------
    // Test github-native produces empty changelog
    // -----------------------------------------------------------------------

    #[test]
    fn test_changelog_stage_github_native_produces_empty() {
        use anodize_core::config::{ChangelogConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let mut config = Config::default();
        config.project_name = "test".to_string();
        config.changelog = Some(ChangelogConfig {
            use_source: Some("github-native".to_string()),
            ..Default::default()
        });
        config.crates = vec![CrateConfig {
            name: "mylib".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }];

        let mut ctx = Context::new(config, ContextOptions::default());

        let stage = ChangelogStage;
        stage.run(&mut ctx).unwrap();

        // github-native should produce an empty changelog body per crate.
        assert_eq!(ctx.changelogs.get("mylib"), Some(&String::new()));
    }

    // -----------------------------------------------------------------------
    // Test include + exclude together
    // -----------------------------------------------------------------------

    #[test]
    fn test_include_and_exclude_are_mutually_exclusive() {
        // GoReleaser treats include and exclude as mutually exclusive:
        // if include is configured, exclude is completely ignored.
        let commits = vec![
            ci("feat: good feature", "feat", "good feature", "a"),
            ci(
                "feat(wip): work in progress",
                "feat",
                "work in progress",
                "b",
            ),
            ci("fix: important fix", "fix", "important fix", "c"),
            ci("docs: update readme", "docs", "update readme", "d"),
        ];

        // When include is set, only include patterns matter (exclude ignored)
        let included = apply_include_filters(
            &commits,
            &["^feat".to_string(), "^fix".to_string()],
            &test_logger(),
        )
        .unwrap();
        assert_eq!(included.len(), 3); // both feat commits + fix
        assert_eq!(included[0].description, "good feature");
        assert_eq!(included[1].description, "work in progress");
        assert_eq!(included[2].description, "important fix");

        // When include is empty, exclude patterns apply
        let excluded = apply_filters(&commits, &["wip".to_string()], &test_logger()).unwrap();
        assert_eq!(excluded.len(), 3); // feat, fix, docs (WIP excluded)
    }

    // -----------------------------------------------------------------------
    // Deep integration tests: full pipeline with realistic commit data
    // -----------------------------------------------------------------------

    /// Simulate what ChangelogStage.run does internally: parse commits,
    /// apply filters, sort, group, and render, using realistic conventional
    /// commit messages with real-looking hashes.
    #[test]
    fn test_integration_full_changelog_pipeline_with_groups() {
        use anodize_core::config::ChangelogGroup;

        // Simulate raw git commits as the changelog stage would receive them
        let raw_messages = vec![
            (
                "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2",
                "a1b2c3d",
                "feat: add user authentication",
            ),
            (
                "b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3",
                "b2c3d4e",
                "fix: resolve login redirect loop",
            ),
            (
                "c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4",
                "c3d4e5f",
                "docs: update API reference",
            ),
            (
                "d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5",
                "d4e5f6a",
                "feat(core): add rate limiting",
            ),
            (
                "e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6",
                "e5f6a1b",
                "fix(auth): handle expired tokens",
            ),
            (
                "f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1",
                "f6a1b2c",
                "chore: update dependencies",
            ),
            (
                "a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6",
                "a7b8c9d",
                "ci: fix GitHub Actions workflow",
            ),
            (
                "b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7",
                "b8c9d0e",
                "feat!: drop support for v1 API",
            ),
        ];

        // Parse all commits
        let mut all_commits: Vec<CommitInfo> = Vec::new();
        for (full_hash, short_hash, message) in &raw_messages {
            let mut info = parse_commit_message(message);
            info.hash = short_hash.to_string();
            info.full_hash = full_hash.to_string();
            all_commits.push(info);
        }

        // Verify parsing was correct
        assert_eq!(all_commits[0].kind, "feat");
        assert_eq!(all_commits[0].description, "add user authentication");
        assert_eq!(all_commits[1].kind, "fix");
        assert_eq!(all_commits[2].kind, "docs");
        assert_eq!(all_commits[7].kind, "feat"); // breaking change still parsed as feat
        assert_eq!(all_commits[7].description, "drop support for v1 API");

        // Apply exclude filters (filter out docs and ci)
        let exclude = vec!["^docs:".to_string(), "^ci:".to_string()];
        let filtered = apply_filters(&all_commits, &exclude, &test_logger()).unwrap();
        assert_eq!(filtered.len(), 6, "docs and ci commits should be excluded");
        assert!(
            !filtered.iter().any(|c| c.kind == "docs"),
            "no docs commits after filter"
        );
        assert!(
            !filtered.iter().any(|c| c.kind == "ci"),
            "no ci commits after filter"
        );

        // Sort ascending
        let mut sorted = filtered;
        sort_commits(&mut sorted, "asc").unwrap();

        // Group into sections
        let groups = vec![
            ChangelogGroup {
                title: "Features".into(),
                regexp: Some("^feat".into()),
                order: Some(0),
                groups: None,
            },
            ChangelogGroup {
                title: "Bug Fixes".into(),
                regexp: Some("^fix".into()),
                order: Some(1),
                groups: None,
            },
        ];
        let grouped = group_commits(&sorted, &groups, &test_logger()).unwrap();

        // Verify grouping — unmatched "chore" commit is silently dropped (GoReleaser behavior)
        assert_eq!(
            grouped.len(),
            2,
            "should have Features and Bug Fixes groups only"
        );
        assert_eq!(grouped[0].title, "Features");
        assert_eq!(grouped[0].commits.len(), 3, "3 feat commits");
        assert_eq!(grouped[1].title, "Bug Fixes");
        assert_eq!(grouped[1].commits.len(), 2, "2 fix commits");

        // Render
        let md = render_changelog(&grouped, 7, None, "", "git", None, None);

        // Verify structural output
        assert!(
            md.contains("## Features\n"),
            "should have Features section header"
        );
        assert!(
            md.contains("## Bug Fixes\n"),
            "should have Bug Fixes section header"
        );

        // Verify commit messages appear in the output
        assert!(
            md.contains("add user authentication"),
            "feat commit should appear"
        );
        assert!(
            md.contains("add rate limiting"),
            "scoped feat commit should appear"
        );
        assert!(
            md.contains("drop support for v1 API"),
            "breaking feat should appear"
        );
        assert!(
            md.contains("resolve login redirect loop"),
            "fix commit should appear"
        );
        assert!(
            md.contains("handle expired tokens"),
            "scoped fix should appear"
        );

        // Verify excluded commits do NOT appear
        assert!(
            !md.contains("update API reference"),
            "docs commit should be filtered out"
        );
        assert!(
            !md.contains("fix GitHub Actions"),
            "ci commit should be filtered out"
        );

        // GoReleaser parity: default format "{{ SHA }} {{ Message }}" uses the
        // abbreviated SHA (abbrev=7 → 7-char prefixes).
        assert!(
            md.contains("a1b2c3d "),
            "abbreviated hash should appear in output, got:\n{md}"
        );
        assert!(md.contains("b2c3d4e "));

        // Verify bullets
        let bullet_lines: Vec<&str> = md.lines().filter(|l| l.starts_with("* ")).collect();
        assert_eq!(
            bullet_lines.len(),
            5,
            "should have 5 bullet points total (3 feat + 2 fix; chore dropped)"
        );
    }

    #[test]
    fn test_integration_changelog_with_include_filters() {
        use anodize_core::config::ChangelogGroup;

        // Simulate commits
        let messages = [
            ("abc1234", "feat: new dashboard"),
            ("def5678", "fix: crash on empty input"),
            ("ghi9012", "chore: bump version"),
            ("jkl3456", "refactor: simplify parser"),
            ("mno7890", "feat(api): add pagination"),
        ];

        let commits: Vec<CommitInfo> = messages
            .iter()
            .map(|(hash, msg)| {
                let mut info = parse_commit_message(msg);
                info.hash = hash.to_string();
                info
            })
            .collect();

        // Include only feat and fix
        let included = apply_include_filters(
            &commits,
            &["^feat".to_string(), "^fix".to_string()],
            &test_logger(),
        )
        .unwrap();
        assert_eq!(included.len(), 3);

        // Sort the filtered list, then group
        let mut sorted = included;
        sort_commits(&mut sorted, "asc").unwrap();
        let groups = vec![
            ChangelogGroup {
                title: "Features".into(),
                regexp: Some("^feat".into()),
                order: Some(0),
                groups: None,
            },
            ChangelogGroup {
                title: "Bug Fixes".into(),
                regexp: Some("^fix".into()),
                order: Some(1),
                groups: None,
            },
        ];
        let grouped = group_commits(&sorted, &groups, &test_logger()).unwrap();

        let md = render_changelog(&grouped, 7, None, "", "git", None, None);

        // Features and Bug Fixes should appear, but not chore or refactor
        assert!(md.contains("## Features"));
        assert!(md.contains("new dashboard"));
        assert!(md.contains("add pagination"));
        assert!(md.contains("## Bug Fixes"));
        assert!(md.contains("crash on empty input"));
        assert!(!md.contains("bump version"));
        assert!(!md.contains("simplify parser"));
    }

    #[test]
    fn test_integration_changelog_header_footer_assembly() {
        // Test the full assembly with header/footer like the stage does
        let messages = [
            ("abc1234", "feat: initial release"),
            ("def5678", "fix: typo in config"),
        ];

        let commits: Vec<CommitInfo> = messages
            .iter()
            .map(|(hash, msg)| {
                let mut info = parse_commit_message(msg);
                info.hash = hash.to_string();
                info.full_hash = hash.to_string();
                info
            })
            .collect();

        let grouped = vec![GroupedCommits {
            title: String::new(),
            commits,
            subgroups: Vec::new(),
        }];

        let body = render_changelog(&grouped, 7, None, "", "git", None, None);

        // Simulate header/footer wrapping as ChangelogStage.run does (double-newline separator)
        let header = "# Release v1.0.0";
        let footer = "---\nFull changelog: https://github.com/example/repo/compare/v0.9.0...v1.0.0";

        let mut final_md = String::new();
        final_md.push_str(header);
        final_md.push_str("\n\n");
        final_md.push_str(&body);
        final_md.push('\n');
        final_md.push_str(footer);
        final_md.push('\n');

        // Body includes default "## Changelog" title (matching GoReleaser).
        // Default format uses `{{ SHA }}` (full hash).
        assert!(
            final_md.starts_with("# Release v1.0.0\n\n## Changelog\n\n* abc1234 initial release")
        );
        assert!(final_md.contains("* abc1234 initial release"));
        assert!(final_md.contains("* def5678 typo in config"));
        assert!(final_md.ends_with("compare/v0.9.0...v1.0.0\n"));
    }

    #[test]
    fn test_integration_changelog_empty_after_filters() {
        // When all commits are filtered out, output should be empty
        let commits = vec![
            ci("ci: fix build", "ci", "fix build", "aaa"),
            ci("docs: update guide", "docs", "update guide", "bbb"),
        ];

        let filtered = apply_filters(
            &commits,
            &["^ci:".to_string(), "^docs:".to_string()],
            &test_logger(),
        )
        .unwrap();
        assert!(filtered.is_empty());

        let grouped = group_commits(
            &filtered,
            &[ChangelogGroup {
                title: "Features".into(),
                regexp: Some("^feat".into()),
                order: Some(0),
                groups: None,
            }],
            &test_logger(),
        )
        .unwrap();
        assert!(grouped.is_empty());

        let md = render_changelog(&grouped, 7, None, "", "git", None, None);
        // Default "## Changelog" title always emitted (matching GoReleaser)
        assert_eq!(
            md, "## Changelog\n\n",
            "changelog should only have title when all commits are filtered"
        );
    }

    // -----------------------------------------------------------------------
    // Integration test with real git history
    // -----------------------------------------------------------------------

    #[test]
    #[serial]
    fn test_integration_changelog_stage_with_real_git_repo() {
        use anodize_core::config::{
            ChangelogConfig, ChangelogFilters, ChangelogGroup, Config, CrateConfig,
        };
        use anodize_core::context::{Context, ContextOptions};
        use std::process::Command;

        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();

        // Helper to run git commands in the temp repo
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(repo)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
        };

        // Initialise a real git repo and make conventional commits
        git(&["init"]);
        // Create an initial file and commit
        std::fs::write(repo.join("file.txt"), b"v1").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "feat: add initial feature"]);

        std::fs::write(repo.join("bug.txt"), b"fix").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "fix: resolve startup crash"]);

        std::fs::write(repo.join("README.md"), b"# docs").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "docs: update readme"]);

        std::fs::write(repo.join("file.txt"), b"v2").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "feat(core): add rate limiting"]);

        // Build a config with changelog settings
        let config = Config {
            project_name: "test-project".to_string(),
            dist: repo.join("dist"),
            changelog: Some(ChangelogConfig {
                sort: Some("asc".to_string()),
                filters: Some(ChangelogFilters {
                    exclude: Some(vec!["^docs:".to_string()]),
                    include: None,
                }),
                groups: Some(vec![
                    ChangelogGroup {
                        title: "Features".into(),
                        regexp: Some("^feat".into()),
                        order: Some(0),
                        groups: None,
                    },
                    ChangelogGroup {
                        title: "Bug Fixes".into(),
                        regexp: Some("^fix".into()),
                        order: Some(1),
                        groups: None,
                    },
                ]),
                header: Some("# Changelog".to_string()),
                abbrev: Some(7),
                ..Default::default()
            }),
            crates: vec![CrateConfig {
                name: "test-project".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };

        let mut ctx = Context::new(config, ContextOptions::default());

        // Run the stage from within the temp repo so git commands target it
        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(repo).unwrap();
        let result = ChangelogStage.run(&mut ctx);
        std::env::set_current_dir(&original_dir).unwrap();

        result.unwrap();

        // Verify the per-crate changelog was populated
        let changelog = ctx
            .changelogs
            .get("test-project")
            .expect("changelog for test-project should exist");
        assert!(!changelog.is_empty(), "changelog should not be empty");

        // Verify expected sections exist
        assert!(
            changelog.contains("## Features"),
            "should have Features section"
        );
        assert!(
            changelog.contains("## Bug Fixes"),
            "should have Bug Fixes section"
        );

        // Verify expected commit messages appear
        assert!(
            changelog.contains("add initial feature"),
            "should contain feat commit"
        );
        assert!(
            changelog.contains("add rate limiting"),
            "should contain scoped feat commit"
        );
        assert!(
            changelog.contains("resolve startup crash"),
            "should contain fix commit"
        );

        // Verify excluded docs commit does NOT appear
        assert!(
            !changelog.contains("update readme"),
            "docs commit should be filtered out"
        );

        // Verify CHANGELOG.md was written
        let notes_path = repo.join("dist").join("CHANGELOG.md");
        assert!(notes_path.exists(), "CHANGELOG.md should be written");
        let notes_content = std::fs::read_to_string(&notes_path).unwrap();
        assert!(
            notes_content.starts_with("# Changelog\n"),
            "should start with header"
        );
        assert!(
            notes_content.contains("## Features"),
            "notes should contain Features"
        );
    }

    // -----------------------------------------------------------------------
    // Task 4C: Additional behavior tests — config fields actually do things
    // -----------------------------------------------------------------------

    #[test]
    fn test_header_appears_before_changelog_body() {
        let grouped = vec![GroupedCommits {
            title: "Features".into(),
            commits: vec![ci("feat: new feature", "feat", "new feature", "abc1234")],
            subgroups: Vec::new(),
        }];
        let body = render_changelog(&grouped, 7, None, "", "git", None, None);

        // Simulate the stage's header/footer assembly
        let mut final_md = String::new();
        final_md.push_str("# Release Header");
        final_md.push('\n');
        final_md.push_str(&body);

        // Header must appear first, before the changelog sections
        let header_pos = final_md.find("# Release Header").unwrap();
        let features_pos = final_md.find("## Features").unwrap();
        assert!(
            header_pos < features_pos,
            "header should appear before changelog body"
        );
    }

    #[test]
    fn test_footer_appears_after_changelog_body() {
        let grouped = vec![GroupedCommits {
            title: "Bug Fixes".into(),
            commits: vec![ci("fix: crash", "fix", "crash", "def5678")],
            subgroups: Vec::new(),
        }];
        let body = render_changelog(&grouped, 7, None, "", "git", None, None);

        let mut final_md = String::new();
        final_md.push_str(&body);
        final_md.push_str("--- Footer ---");
        final_md.push('\n');

        let fixes_pos = final_md.find("## Bug Fixes").unwrap();
        let footer_pos = final_md.find("--- Footer ---").unwrap();
        assert!(
            footer_pos > fixes_pos,
            "footer should appear after changelog body"
        );
    }

    #[test]
    fn test_include_filters_restrict_commits_to_matching_patterns() {
        // Only feat and fix should survive the include filter
        let commits = vec![
            ci("feat: add login", "feat", "add login", "a"),
            ci("fix: crash", "fix", "crash", "b"),
            ci("chore: deps", "chore", "deps", "c"),
            ci("refactor: cleanup", "refactor", "cleanup", "d"),
        ];

        let result = apply_include_filters(
            &commits,
            &["^feat".to_string(), "^fix".to_string()],
            &test_logger(),
        )
        .unwrap();
        assert_eq!(result.len(), 2);
        assert!(result.iter().all(|c| c.kind == "feat" || c.kind == "fix"));
        // Excluded types should not be present
        assert!(!result.iter().any(|c| c.kind == "chore"));
        assert!(!result.iter().any(|c| c.kind == "refactor"));
    }

    #[test]
    fn test_abbrev_truncates_to_specified_length() {
        let grouped = vec![GroupedCommits {
            title: String::new(),
            commits: vec![ci("feat: test", "feat", "test", "abcdef1234567890")],
            subgroups: Vec::new(),
        }];

        // GoReleaser parity (changelog.go:262): `{{ .SHA }}` respects the
        // `abbrev` setting — short SHA when abbrev > 0, full when abbrev == 0.
        let md = render_changelog(&grouped, 3, None, "", "git", None, None);
        assert!(
            md.contains("abc test"),
            "abbrev=3 expected short hash 'abc test', got: {}",
            md
        );

        let md10 = render_changelog(&grouped, 10, None, "", "git", None, None);
        assert!(
            md10.contains("abcdef1234 test"),
            "abbrev=10 expected short hash 'abcdef1234 test', got: {}",
            md10
        );
    }

    #[test]
    fn test_abbrev_zero_shows_full_sha() {
        let mut commit = ci("feat: test", "feat", "test", "abcdef");
        commit.full_hash = "abcdef1234567890abcdef1234567890abcdef12".to_string();
        let grouped = vec![GroupedCommits {
            title: String::new(),
            commits: vec![commit],
            subgroups: Vec::new(),
        }];

        // abbrev = 0 should show the full SHA (no truncation)
        let md = render_changelog(&grouped, 0, None, "", "git", None, None);
        assert!(
            md.contains("abcdef1234567890abcdef1234567890abcdef12 test"),
            "abbrev=0 should show full SHA, got: {}",
            md
        );
    }

    #[test]
    fn test_disable_skips_stage_entirely() {
        use anodize_core::config::{ChangelogConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let mut config = Config::default();
        config.project_name = "test".to_string();
        config.changelog = Some(ChangelogConfig {
            disable: Some(anodize_core::config::StringOrBool::Bool(true)),
            ..Default::default()
        });
        config.crates = vec![CrateConfig {
            name: "test".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }];

        let mut ctx = Context::new(config, ContextOptions::default());
        ChangelogStage.run(&mut ctx).unwrap();

        // No changelogs should be generated when disabled
        assert!(ctx.changelogs.is_empty());
        // The github_native_changelog flag should NOT be set
        assert!(!ctx.github_native_changelog);
    }

    #[test]
    fn test_empty_changelog_when_all_commits_filtered() {
        let commits = vec![ci("ci: pipeline fix", "ci", "pipeline fix", "a")];

        // Include filter that matches nothing
        let result =
            apply_include_filters(&commits, &["^feat".to_string()], &test_logger()).unwrap();
        assert!(result.is_empty());

        let grouped = group_commits(&result, &[], &test_logger()).unwrap();
        let md = render_changelog(&grouped, 7, None, "", "git", None, None);
        // With the default "## Changelog" title, empty groups still produce the title header
        assert_eq!(
            md, "## Changelog\n\n",
            "changelog should be empty when no commits match"
        );
    }

    #[test]
    #[serial]
    fn test_changelog_written_to_correct_output_location() {
        use anodize_core::config::{ChangelogConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};
        use std::process::Command;

        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();

        // Helper to run git commands
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(repo)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .output()
                .unwrap();
            assert!(output.status.success());
        };

        git(&["init"]);
        std::fs::write(repo.join("file.txt"), b"v1").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "feat: initial"]);

        let custom_dist = repo.join("custom-dist");

        let config = Config {
            project_name: "test".to_string(),
            dist: custom_dist.clone(),
            changelog: Some(ChangelogConfig::default()),
            crates: vec![CrateConfig {
                name: "test".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };

        let mut ctx = Context::new(config, ContextOptions::default());

        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(repo).unwrap();
        let result = ChangelogStage.run(&mut ctx);
        std::env::set_current_dir(&original_dir).unwrap();
        result.unwrap();

        // CHANGELOG.md should be written to the dist directory
        let notes_path = custom_dist.join("CHANGELOG.md");
        assert!(
            notes_path.exists(),
            "CHANGELOG.md should be in the dist directory: {}",
            custom_dist.display()
        );
    }

    // ---- Error path tests (Task 4D) ----

    #[test]
    fn test_invalid_exclude_regex_returns_error() {
        let commits = vec![ci("feat: new feature", "feat", "new feature", "abc")];
        // Invalid regex: unclosed group
        let filters = vec!["^feat(".to_string()];
        let result = apply_filters(&commits, &filters, &test_logger());
        assert!(
            result.is_err(),
            "invalid exclude regex should return an error"
        );
    }

    #[test]
    fn test_invalid_include_regex_returns_error() {
        let commits = vec![ci("fix: a bug", "fix", "a bug", "def")];
        let filters = vec!["[invalid".to_string()];
        let result = apply_include_filters(&commits, &filters, &test_logger());
        assert!(
            result.is_err(),
            "invalid include regex should return an error"
        );
    }

    #[test]
    fn test_invalid_group_regex_returns_error() {
        let commits = vec![ci("feat: new thing", "feat", "new thing", "abc")];
        let groups = vec![ChangelogGroup {
            title: "Features".into(),
            regexp: Some("^feat(".into()), // invalid regex
            order: Some(0),
            groups: None,
        }];
        let result = group_commits(&commits, &groups, &test_logger());
        assert!(
            result.is_err(),
            "invalid group regex should return an error"
        );
    }

    #[test]
    fn test_no_previous_tag_uses_all_commits() {
        // When there's no previous tag, the stage falls back to all commits
        // We test the underlying logic: if prev_tag is None, get_all_commits is used
        // This tests parse_commit_message on various edge cases
        let empty_msg = parse_commit_message("");
        assert_eq!(empty_msg.kind, "other");
        assert_eq!(empty_msg.description, "");

        let no_colon = parse_commit_message("no colon here");
        assert_eq!(no_colon.kind, "other");
    }

    #[test]
    fn test_sort_commits_unknown_order_returns_error() {
        let mut commits = vec![
            ci("b: second", "other", "second", "1"),
            ci("a: first", "other", "first", "2"),
        ];
        let result = sort_commits(&mut commits, "invalid_order");
        assert!(
            result.is_err(),
            "invalid sort direction should return an error"
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("invalid changelog sort direction"),
            "error message should mention the invalid direction"
        );
    }

    #[test]
    fn test_render_changelog_empty_groups() {
        let grouped: Vec<GroupedCommits> = vec![];
        let result = render_changelog(&grouped, 7, None, "", "git", None, None);
        // Default title "## Changelog" is always emitted (matching GoReleaser)
        assert_eq!(
            result, "## Changelog\n\n",
            "empty groups should produce only the title heading"
        );
    }

    #[test]
    fn test_render_changelog_very_short_hash_preserved() {
        let grouped = vec![GroupedCommits {
            title: "Test".into(),
            commits: vec![ci("feat: x", "feat", "x", "ab")], // shorter than abbrev
            subgroups: Vec::new(),
        }];
        let result = render_changelog(&grouped, 7, None, "", "git", None, None);
        // Short hash should be used as-is without truncation
        assert!(
            result.contains("ab x"),
            "short hash should be kept intact, got: {result}"
        );
    }

    #[test]
    #[serial]
    fn test_changelog_create_dist_dir_failure() {
        use anodize_core::config::{Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};
        use std::process::Command;

        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();

        // Set up a minimal git repo so the stage gets past git operations
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(repo)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .output()
                .unwrap();
            assert!(output.status.success());
        };

        git(&["init"]);
        std::fs::write(repo.join("file.txt"), b"content").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "feat: initial"]);

        // Use an impossible path for dist to trigger create_dir_all failure
        let impossible_dist = if cfg!(windows) {
            // NUL is the Windows equivalent of /dev/null — cannot be a directory
            std::path::PathBuf::from("NUL\\impossible\\dist")
        } else {
            std::path::PathBuf::from("/dev/null/impossible/dist")
        };
        let config = Config {
            project_name: "test".to_string(),
            dist: impossible_dist,
            crates: vec![CrateConfig {
                name: "test".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };

        let mut ctx = Context::new(config, ContextOptions::default());

        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(repo).unwrap();
        let result = ChangelogStage.run(&mut ctx);
        std::env::set_current_dir(&original_dir).unwrap();

        assert!(
            result.is_err(),
            "creating dist dir under /dev/null should fail"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("changelog") || err.contains("dist") || err.contains("create"),
            "error should mention directory creation context, got: {err}"
        );
    }

    #[test]
    #[serial]
    fn test_changelog_write_failure_on_readonly_path() {
        use anodize_core::config::{Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};
        use std::process::Command;

        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();

        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(repo)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .output()
                .unwrap();
            assert!(output.status.success());
        };

        git(&["init"]);
        std::fs::write(repo.join("file.txt"), b"content").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "feat: initial"]);

        // Create the dist dir, then place a directory where CHANGELOG.md
        // would go, so fs::write fails (can't write to a directory path).
        let dist = repo.join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        let notes_blocker = dist.join("CHANGELOG.md");
        std::fs::create_dir_all(&notes_blocker).unwrap();

        let config = Config {
            project_name: "test".to_string(),
            dist: dist.clone(),
            crates: vec![CrateConfig {
                name: "test".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };

        let mut ctx = Context::new(config, ContextOptions::default());

        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(repo).unwrap();
        let result = ChangelogStage.run(&mut ctx);
        std::env::set_current_dir(&original_dir).unwrap();

        assert!(
            result.is_err(),
            "writing CHANGELOG.md where a directory exists should fail"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("CHANGELOG") || err.contains("changelog") || err.contains("write"),
            "error should mention the write failure context, got: {err}"
        );
    }

    #[test]
    #[serial]
    fn test_changelog_dry_run_writes_file() {
        use anodize_core::config::{Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};
        use std::process::Command;

        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let dist = repo.join("dist");

        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(repo)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .output()
                .unwrap();
            assert!(output.status.success());
        };

        git(&["init"]);
        std::fs::write(repo.join("file.txt"), b"content").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "feat: initial"]);

        // GoReleaser writes CHANGELOG.md even in dry-run mode
        let config = Config {
            project_name: "test".to_string(),
            dist: dist.clone(),
            crates: vec![CrateConfig {
                name: "test".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(repo).unwrap();
        let result = ChangelogStage.run(&mut ctx);
        std::env::set_current_dir(&original_dir).unwrap();

        assert!(
            result.is_ok(),
            "dry-run should succeed and write CHANGELOG.md"
        );
        assert!(
            dist.join("CHANGELOG.md").exists(),
            "CHANGELOG.md should be written even in dry-run mode"
        );
    }

    // -----------------------------------------------------------------------
    // Tests for changelog format template
    // -----------------------------------------------------------------------

    #[test]
    fn test_custom_format_template_renders_correctly() {
        let grouped = vec![GroupedCommits {
            title: "Features".into(),
            commits: vec![CommitInfo {
                raw_message: "feat: add auth".into(),
                kind: "feat".into(),
                description: "add auth".into(),
                hash: "abc1234".into(),
                full_hash: "abc1234567890abcdef1234567890abcdef123456".into(),
                author_name: "Alice".into(),
                author_email: "alice@example.com".into(),
                login: String::new(),
                co_authors: Vec::new(),
            }],
            subgroups: Vec::new(),
        }];
        let md = render_changelog(
            &grouped,
            7,
            Some("{{ SHA }} {{ Message }} ({{ AuthorName }} <{{ AuthorEmail }}>)"),
            "",
            "git",
            None,
            None,
        );
        assert!(
            md.contains("abc1234 add auth (Alice <alice@example.com>)"),
            "custom format should render all variables, got: {md}"
        );
    }

    #[test]
    fn test_default_format_unchanged() {
        let grouped = vec![GroupedCommits {
            title: String::new(),
            commits: vec![CommitInfo {
                raw_message: "fix: bug".into(),
                kind: "fix".into(),
                description: "bug".into(),
                hash: "def5678".into(),
                full_hash: "def5678abcdef".into(),
                author_name: "Bob".into(),
                author_email: "bob@example.com".into(),
                login: String::new(),
                co_authors: Vec::new(),
            }],
            subgroups: Vec::new(),
        }];
        // GoReleaser parity: default format "{{ SHA }} {{ Message }}" uses the
        // abbreviated SHA. With abbrev=7 and hash="def5678", SHA == "def5678".
        let md = render_changelog(&grouped, 7, None, "", "git", None, None);
        assert!(
            md.contains("* def5678 bug"),
            "default format should be 'SHA Message' with abbrev-respecting SHA, got: {md}"
        );
    }

    #[test]
    fn test_config_parse_changelog_format() {
        let yaml = r#"
sort: asc
format: "{{ ShortSHA }} {{ Message }} by {{ AuthorName }}"
"#;
        let cfg: anodize_core::config::ChangelogConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(
            cfg.format.as_deref(),
            Some("{{ ShortSHA }} {{ Message }} by {{ AuthorName }}")
        );
    }

    // ---------------------------------------------------------------------------
    // Empty/none sort preserves original git log order
    // ---------------------------------------------------------------------------

    #[test]
    fn test_sort_empty_preserves_order() {
        let mut commits = vec![
            ci("c", "feat", "c", "3"),
            ci("a", "feat", "a", "1"),
            ci("b", "feat", "b", "2"),
        ];
        sort_commits(&mut commits, "").unwrap();
        // Original order should be preserved (c, a, b)
        assert_eq!(commits[0].raw_message, "c");
        assert_eq!(commits[1].raw_message, "a");
        assert_eq!(commits[2].raw_message, "b");
    }

    #[test]
    fn test_sort_none_is_invalid() {
        let mut commits = vec![
            ci("c", "feat", "c", "3"),
            ci("a", "feat", "a", "1"),
            ci("b", "feat", "b", "2"),
        ];
        let result = sort_commits(&mut commits, "none");
        assert!(result.is_err(), "\"none\" is not a valid sort direction");
    }

    #[test]
    fn test_sort_asc_still_works() {
        let mut commits = vec![
            ci("c", "feat", "c", "3"),
            ci("a", "feat", "a", "1"),
            ci("b", "feat", "b", "2"),
        ];
        sort_commits(&mut commits, "asc").unwrap();
        assert_eq!(commits[0].raw_message, "a");
        assert_eq!(commits[1].raw_message, "b");
        assert_eq!(commits[2].raw_message, "c");
    }

    #[test]
    fn test_sort_desc_still_works() {
        let mut commits = vec![
            ci("c", "feat", "c", "3"),
            ci("a", "feat", "a", "1"),
            ci("b", "feat", "b", "2"),
        ];
        sort_commits(&mut commits, "desc").unwrap();
        assert_eq!(commits[0].raw_message, "c");
        assert_eq!(commits[1].raw_message, "b");
        assert_eq!(commits[2].raw_message, "a");
    }

    // ---------------------------------------------------------------------------
    // Header/footer template rendering
    // ---------------------------------------------------------------------------

    #[test]
    fn test_header_footer_template_rendering() {
        // Simulate what ChangelogStage::run does: render header/footer through
        // ctx.render_template() before inserting them.
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "2.0.0");

        let header_tmpl = "# {{ .ProjectName }} v{{ .Version }} Release Notes";
        let footer_tmpl = "---\nGenerated for {{ .ProjectName }}";

        let rendered_header = ctx.render_template(header_tmpl).unwrap();
        let rendered_footer = ctx.render_template(footer_tmpl).unwrap();

        assert_eq!(rendered_header, "# myapp v2.0.0 Release Notes");
        assert_eq!(rendered_footer, "---\nGenerated for myapp");
    }

    #[test]
    fn test_header_with_plain_string_passes_through() {
        // A header without template variables should pass through unchanged.
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        let config = Config::default();
        let ctx = Context::new(config, ContextOptions::default());

        let header = "# Plain Header";
        let rendered = ctx.render_template(header).unwrap();
        assert_eq!(rendered, "# Plain Header");
    }

    // -----------------------------------------------------------------------
    // Parity tests: StringOrBool disable, abbrev -1, nested subgroups, Logins
    // -----------------------------------------------------------------------

    #[test]
    fn test_config_parse_disable_template_string() {
        let yaml = r#"
disable: "{{ if .IsSnapshot }}true{{ end }}"
sort: asc
"#;
        let cfg: anodize_core::config::ChangelogConfig = serde_yaml_ng::from_str(yaml).unwrap();
        match &cfg.disable {
            Some(anodize_core::config::StringOrBool::String(s)) => {
                assert!(s.contains("IsSnapshot"), "should contain template string");
            }
            other => panic!("expected StringOrBool::String, got {:?}", other),
        }
    }

    #[test]
    fn test_config_parse_disable_bool_true() {
        let yaml = r#"
disable: true
"#;
        let cfg: anodize_core::config::ChangelogConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(
            cfg.disable,
            Some(anodize_core::config::StringOrBool::Bool(true))
        );
    }

    #[test]
    fn test_config_parse_abbrev_negative_one() {
        let yaml = r#"
abbrev: -1
"#;
        let cfg: anodize_core::config::ChangelogConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.abbrev, Some(-1));
    }

    #[test]
    fn test_config_parse_nested_subgroups() {
        let yaml = r#"
sort: asc
groups:
  - title: "Features"
    regexp: "^feat"
    order: 0
    groups:
      - title: "Core Features"
        regexp: "^feat\\(core\\)"
        order: 0
      - title: "API Features"
        regexp: "^feat\\(api\\)"
        order: 1
  - title: "Bug Fixes"
    regexp: "^fix"
    order: 1
"#;
        let cfg: anodize_core::config::ChangelogConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let groups = cfg.groups.as_ref().unwrap();
        assert_eq!(groups.len(), 2);

        // First group should have nested subgroups
        let feat_group = &groups[0];
        assert_eq!(feat_group.title, "Features");
        let subgroups = feat_group.groups.as_ref().unwrap();
        assert_eq!(subgroups.len(), 2);
        assert_eq!(subgroups[0].title, "Core Features");
        assert_eq!(subgroups[1].title, "API Features");

        // Second group should have no subgroups
        let fix_group = &groups[1];
        assert_eq!(fix_group.title, "Bug Fixes");
        assert!(fix_group.groups.is_none() || fix_group.groups.as_ref().unwrap().is_empty());
    }

    #[test]
    fn test_config_parse_use_source_github() {
        let yaml = r#"
use: github
format: "{{ ShortSHA }} {{ Message }} @{{ Logins }}"
"#;
        let cfg: anodize_core::config::ChangelogConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.use_source.as_deref(), Some("github"));
        assert!(cfg.format.as_ref().unwrap().contains("Logins"));
    }

    #[test]
    fn test_render_abbrev_negative_one_omits_hash() {
        let grouped = vec![GroupedCommits::new(
            "",
            vec![ci(
                "feat: no hash test",
                "feat",
                "no hash test",
                "abc1234567890",
            )],
        )];
        let md = render_changelog(&grouped, -1, None, "", "git", None, None);
        // With abbrev=-1, the default format is "{{ Message }}" (no hash)
        assert!(
            md.contains("* no hash test"),
            "should contain commit message without hash, got: {md}"
        );
        assert!(
            !md.contains("abc"),
            "should NOT contain any part of the hash, got: {md}"
        );
    }

    #[test]
    fn test_render_abbrev_negative_one_custom_format_empty_short_sha() {
        let grouped = vec![GroupedCommits::new(
            "",
            vec![ci("feat: test", "feat", "test", "abc1234567890")],
        )];
        // Custom format referencing ShortSHA should get an empty string when abbrev=-1
        let md = render_changelog(
            &grouped,
            -1,
            Some("{{ ShortSHA }}|{{ Message }}"),
            "",
            "git",
            None,
            None,
        );
        assert!(
            md.contains("* |test"),
            "ShortSHA should be empty with abbrev=-1, got: {md}"
        );
    }

    #[test]
    fn test_render_nested_subgroups() {
        let grouped = vec![GroupedCommits {
            title: "Features".into(),
            commits: Vec::new(),
            subgroups: vec![
                GroupedCommits::new(
                    "Core Features",
                    vec![ci("feat(core): add auth", "feat", "add auth", "aaa1234")],
                ),
                GroupedCommits::new(
                    "API Features",
                    vec![ci(
                        "feat(api): add endpoint",
                        "feat",
                        "add endpoint",
                        "bbb5678",
                    )],
                ),
            ],
        }];
        let md = render_changelog(&grouped, 7, None, "", "git", None, None);
        // Parent group should be ## level
        assert!(
            md.contains("## Features"),
            "should have parent group heading, got: {md}"
        );
        // Subgroups should be ### level (one deeper)
        assert!(
            md.contains("### Core Features"),
            "should have Core Features sub-heading, got: {md}"
        );
        assert!(
            md.contains("### API Features"),
            "should have API Features sub-heading, got: {md}"
        );
        // Commits should appear under their subgroups
        assert!(md.contains("add auth"), "should contain core commit");
        assert!(md.contains("add endpoint"), "should contain api commit");
    }

    #[test]
    fn test_group_commits_with_nested_subgroups() {
        let commits = vec![
            ci("feat(core): add auth", "feat", "add auth", "aaa"),
            ci("feat(api): add endpoint", "feat", "add endpoint", "bbb"),
            ci("feat: generic feature", "feat", "generic feature", "ccc"),
            ci("fix: crash", "fix", "crash", "ddd"),
        ];
        let groups = vec![
            ChangelogGroup {
                title: "Features".into(),
                regexp: Some("^feat".into()),
                order: Some(0),
                groups: Some(vec![
                    ChangelogGroup {
                        title: "Core".into(),
                        regexp: Some(r"^feat\(core\)".into()),
                        order: Some(0),
                        groups: None,
                    },
                    ChangelogGroup {
                        title: "API".into(),
                        regexp: Some(r"^feat\(api\)".into()),
                        order: Some(1),
                        groups: None,
                    },
                ]),
            },
            ChangelogGroup {
                title: "Bug Fixes".into(),
                regexp: Some("^fix".into()),
                order: Some(1),
                groups: None,
            },
        ];
        let result = group_commits(&commits, &groups, &test_logger()).unwrap();

        assert_eq!(result.len(), 2, "should have Features and Bug Fixes");
        assert_eq!(result[0].title, "Features");
        // Features group distributes commits into subgroups.
        // GoReleaser drops unmatched commits silently, so "generic feature"
        // that matched the parent but not any subgroup is dropped.
        assert_eq!(
            result[0].subgroups.len(),
            2,
            "should have Core and API subgroups (no Others)"
        );
        assert_eq!(result[0].subgroups[0].title, "Core");
        assert_eq!(result[0].subgroups[0].commits.len(), 1);
        assert_eq!(result[0].subgroups[1].title, "API");
        assert_eq!(result[0].subgroups[1].commits.len(), 1);

        assert_eq!(result[1].title, "Bug Fixes");
        assert_eq!(result[1].commits.len(), 1);
    }

    #[test]
    fn test_gitlab_newline_handling() {
        let grouped = vec![GroupedCommits::new(
            "",
            vec![
                ci("feat: feature A", "feat", "feature A", "abc1234"),
                ci("fix: bug B", "fix", "bug B", "def5678"),
            ],
        )];
        // Use explicit format to keep assertions simple.
        let md = render_changelog(
            &grouped,
            7,
            Some("{{ ShortSHA }} {{ Message }}"),
            "",
            "gitlab",
            None,
            None,
        );
        // GitLab should use 3-space + newline for markdown line breaks.
        assert!(
            md.contains("* abc1234 feature A   \n"),
            "GitLab should use '   \\n' for line breaks, got: {md}"
        );
        assert!(
            md.contains("* def5678 bug B   \n"),
            "GitLab should use '   \\n' for line breaks, got: {md}"
        );
    }

    #[test]
    fn test_gitea_newline_handling() {
        let grouped = vec![GroupedCommits::new(
            "",
            vec![ci("feat: x", "feat", "x", "aaa1111")],
        )];
        let md = render_changelog(
            &grouped,
            7,
            Some("{{ ShortSHA }} {{ Message }}"),
            "",
            "gitea",
            None,
            None,
        );
        assert!(
            md.contains("* aaa1111 x   \n"),
            "Gitea should use '   \\n' for line breaks, got: {md}"
        );
    }

    #[test]
    fn test_github_newline_handling() {
        let grouped = vec![GroupedCommits::new(
            "",
            vec![ci("feat: y", "feat", "y", "bbb2222")],
        )];
        let md = render_changelog(
            &grouped,
            7,
            Some("{{ ShortSHA }} {{ Message }}"),
            "",
            "github",
            None,
            None,
        );
        // GitHub should NOT use 3-space newlines.
        assert!(
            md.contains("* bbb2222 y\n"),
            "GitHub should use plain newline, got: {md}"
        );
        assert!(
            !md.contains("   \n"),
            "GitHub should NOT use '   \\n', got: {md}"
        );
    }

    #[test]
    fn test_render_logins_variable_in_format() {
        let grouped = vec![GroupedCommits::new(
            "",
            vec![CommitInfo {
                raw_message: "feat: add feature".into(),
                kind: "feat".into(),
                description: "add feature".into(),
                hash: "abc1234".into(),
                full_hash: "abc1234567890".into(),
                author_name: "Alice".into(),
                author_email: "alice@example.com".into(),
                login: "alice".into(),
                co_authors: Vec::new(),
            }],
        )];
        let md = render_changelog(
            &grouped,
            7,
            Some("{{ ShortSHA }} {{ Message }} cc {{ Logins }}"),
            "alice,bob",
            "git",
            None,
            None,
        );
        assert!(
            md.contains("abc1234 add feature cc alice,bob"),
            "Logins variable should be rendered, got: {md}"
        );
    }

    #[test]
    fn test_grouped_commits_new_constructor() {
        // Verify the GroupedCommits::new constructor sets subgroups to empty vec
        let gc = GroupedCommits::new("Test Group", vec![]);
        assert_eq!(gc.title, "Test Group");
        assert!(gc.commits.is_empty());
        assert!(gc.subgroups.is_empty());
    }

    #[test]
    fn test_render_per_commit_login_variable() {
        let grouped = vec![GroupedCommits::new(
            "",
            vec![CommitInfo {
                raw_message: "feat: add feature".into(),
                kind: "feat".into(),
                description: "add feature".into(),
                hash: "abc1234".into(),
                full_hash: "abc1234567890".into(),
                author_name: "Octocat".into(),
                author_email: "octocat@github.com".into(),
                login: "octocat".into(),
                co_authors: Vec::new(),
            }],
        )];
        let md = render_changelog(
            &grouped,
            7,
            Some("{{ ShortSHA }} {{ Message }} (@{{ Login }})"),
            "",
            "git",
            None,
            None,
        );
        assert!(
            md.contains("abc1234 add feature (@octocat)"),
            "Login variable should render the per-commit GitHub username, got: {md}"
        );
    }

    #[test]
    fn test_abbrev_zero_custom_format_shows_full_sha() {
        let mut commit = ci("feat: test", "feat", "test", "abc1234567890");
        commit.full_hash = "abc1234567890def1234567890abc1234567890de".to_string();
        let grouped = vec![GroupedCommits::new("", vec![commit])];
        // Custom format referencing ShortSHA should get the full SHA when abbrev=0
        let md = render_changelog(
            &grouped,
            0,
            Some("{{ ShortSHA }}|{{ Message }}"),
            "",
            "git",
            None,
            None,
        );
        assert!(
            md.contains("* abc1234567890def1234567890abc1234567890de|test"),
            "ShortSHA should be full SHA with abbrev=0, got: {md}"
        );
    }

    // -----------------------------------------------------------------------
    // Title, Divider, Paths (Pro features)
    // -----------------------------------------------------------------------

    #[test]
    fn test_custom_title() {
        let grouped = vec![GroupedCommits::new(
            "Features",
            vec![ci("feat: add X", "feat", "add X", "abc1234")],
        )];
        let md = render_changelog(&grouped, 7, None, "", "git", Some("Release Notes"), None);
        assert!(
            md.starts_with("## Release Notes\n\n"),
            "custom title should replace default 'Changelog': {md}"
        );
        assert!(
            md.contains("### Features"),
            "groups should be at depth 3: {md}"
        );
    }

    #[test]
    fn test_empty_title_suppresses_heading() {
        let grouped = vec![GroupedCommits::new(
            "",
            vec![ci("feat: add X", "feat", "add X", "abc1234")],
        )];
        // Default format uses `{{ SHA }}` (full hash).
        let md = render_changelog(&grouped, 7, None, "", "git", Some(""), None);
        assert!(
            !md.contains("## "),
            "empty title should suppress title heading: {md}"
        );
        assert!(
            md.starts_with("* abc1234 add X"),
            "commits should start immediately: {md}"
        );
    }

    #[test]
    fn test_divider_between_groups() {
        let grouped = vec![
            GroupedCommits::new(
                "Features",
                vec![ci("feat: add X", "feat", "add X", "abc1234")],
            ),
            GroupedCommits::new(
                "Bug Fixes",
                vec![ci("fix: fix Y", "fix", "fix Y", "def5678")],
            ),
        ];
        let md = render_changelog(&grouped, 7, None, "", "git", None, Some("---"));
        assert!(
            md.contains("---\n### Bug Fixes"),
            "divider should appear between groups: {md}"
        );
        // Divider should NOT appear before the first group
        assert!(
            !md.starts_with("---"),
            "divider should not appear before first group: {md}"
        );
    }

    #[test]
    fn test_divider_not_emitted_with_single_group() {
        let grouped = vec![GroupedCommits::new(
            "Features",
            vec![ci("feat: add X", "feat", "add X", "abc1234")],
        )];
        let md = render_changelog(&grouped, 7, None, "", "git", None, Some("---"));
        assert!(
            !md.contains("---"),
            "divider should not appear with only one group: {md}"
        );
    }

    #[test]
    fn test_title_and_divider_combined() {
        let grouped = vec![
            GroupedCommits::new(
                "Features",
                vec![ci("feat: add X", "feat", "add X", "abc1234")],
            ),
            GroupedCommits::new(
                "Bug Fixes",
                vec![ci("fix: fix Y", "fix", "fix Y", "def5678")],
            ),
        ];
        let md = render_changelog(
            &grouped,
            7,
            None,
            "",
            "git",
            Some("What's Changed"),
            Some("---"),
        );
        assert!(
            md.starts_with("## What's Changed\n\n"),
            "custom title should be present: {md}"
        );
        assert!(
            md.contains("---\n### Bug Fixes"),
            "divider between groups: {md}"
        );
    }

    #[test]
    fn test_changelog_ai_config_deserializes() {
        use anodize_core::config::ChangelogConfig;
        let yaml = r#"
ai:
  use: anthropic
  model: claude-sonnet-4-20250514
  prompt: "Summarize these changes: {{ ReleaseNotes }}"
title: "Release Notes"
divider: "---"
paths:
  - src/
  - lib/
"#;
        let cfg: ChangelogConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.title.as_deref(), Some("Release Notes"));
        assert_eq!(cfg.divider.as_deref(), Some("---"));
        assert_eq!(
            cfg.paths.as_deref(),
            Some(&["src/".to_string(), "lib/".to_string()][..])
        );
        let ai = cfg.ai.unwrap();
        assert_eq!(ai.provider.as_deref(), Some("anthropic"));
        assert_eq!(ai.model.as_deref(), Some("claude-sonnet-4-20250514"));
    }

    #[test]
    fn test_changelog_ai_prompt_inline() {
        use anodize_core::config::{ChangelogAiConfig, ChangelogAiPrompt};
        let yaml = r#"
use: openai
model: gpt-4
prompt: "Summarize these release notes"
"#;
        let cfg: ChangelogAiConfig = serde_yaml_ng::from_str(yaml).unwrap();
        match cfg.prompt.unwrap() {
            ChangelogAiPrompt::Inline(s) => assert_eq!(s, "Summarize these release notes"),
            other => panic!("expected Inline prompt, got: {:?}", other),
        }
    }

    #[test]
    fn test_changelog_ai_prompt_from_file() {
        use anodize_core::config::{ChangelogAiConfig, ChangelogAiPrompt};
        let yaml = r#"
use: openai
prompt:
  from_file:
    path: ./prompt.md
"#;
        let cfg: ChangelogAiConfig = serde_yaml_ng::from_str(yaml).unwrap();
        match cfg.prompt.unwrap() {
            ChangelogAiPrompt::Source(src) => {
                assert_eq!(src.from_file.unwrap().path.as_deref(), Some("./prompt.md"));
            }
            other => panic!("expected Source prompt, got: {:?}", other),
        }
    }

    #[test]
    fn test_changelog_ai_prompt_from_url() {
        use anodize_core::config::{ChangelogAiConfig, ChangelogAiPrompt};
        let yaml = r#"
use: anthropic
prompt:
  from_url:
    url: https://example.com/prompt.txt
    headers:
      Authorization: "Bearer token123"
      Accept: text/plain
"#;
        let cfg: ChangelogAiConfig = serde_yaml_ng::from_str(yaml).unwrap();
        match cfg.prompt.unwrap() {
            ChangelogAiPrompt::Source(src) => {
                let from_url = src.from_url.unwrap();
                assert_eq!(
                    from_url.url.as_deref(),
                    Some("https://example.com/prompt.txt")
                );
                let headers = from_url.headers.unwrap();
                assert_eq!(
                    headers.get("Authorization").map(|s| s.as_str()),
                    Some("Bearer token123")
                );
                assert_eq!(
                    headers.get("Accept").map(|s| s.as_str()),
                    Some("text/plain")
                );
                assert!(src.from_file.is_none());
            }
            other => panic!("expected Source prompt, got: {:?}", other),
        }
    }

    #[test]
    fn test_resolve_prompt_source_file_overrides_url() {
        use anodize_core::config::{
            ChangelogAiPromptSource, ContentFromFile, ContentFromUrl, ResolvedPromptSource,
        };
        let src = ChangelogAiPromptSource {
            from_file: Some(ContentFromFile {
                path: Some("./prompt.md".to_string()),
            }),
            from_url: Some(ContentFromUrl {
                url: Some("https://example.com/p".to_string()),
                headers: None,
            }),
        };
        match src.resolve() {
            ResolvedPromptSource::File(p) => assert_eq!(p, "./prompt.md"),
            other => panic!("expected File, got: {:?}", other),
        }
    }

    #[test]
    fn test_resolve_prompt_source_url_only() {
        use anodize_core::config::{ChangelogAiPromptSource, ContentFromUrl, ResolvedPromptSource};
        let mut headers = std::collections::HashMap::new();
        headers.insert("Auth".to_string(), "Bearer x".to_string());
        let src = ChangelogAiPromptSource {
            from_file: None,
            from_url: Some(ContentFromUrl {
                url: Some("https://example.com/p".to_string()),
                headers: Some(headers),
            }),
        };
        match src.resolve() {
            ResolvedPromptSource::Url { url, headers } => {
                assert_eq!(url, "https://example.com/p");
                assert_eq!(
                    headers.unwrap().get("Auth").map(|s| s.as_str()),
                    Some("Bearer x")
                );
            }
            other => panic!("expected Url, got: {:?}", other),
        }
    }

    #[test]
    fn test_resolve_prompt_source_none() {
        use anodize_core::config::{ChangelogAiPromptSource, ResolvedPromptSource};
        let src = ChangelogAiPromptSource {
            from_file: None,
            from_url: None,
        };
        assert!(matches!(src.resolve(), ResolvedPromptSource::None));
    }

    #[test]
    fn test_resolve_prompt_source_file_with_empty_path_falls_through() {
        use anodize_core::config::{
            ChangelogAiPromptSource, ContentFromFile, ContentFromUrl, ResolvedPromptSource,
        };
        let src = ChangelogAiPromptSource {
            from_file: Some(ContentFromFile { path: None }),
            from_url: Some(ContentFromUrl {
                url: Some("https://fallback.com".to_string()),
                headers: None,
            }),
        };
        match src.resolve() {
            ResolvedPromptSource::Url { url, .. } => assert_eq!(url, "https://fallback.com"),
            other => panic!("expected Url fallback, got: {:?}", other),
        }
    }

    #[test]
    fn test_group_depth_with_title() {
        // With title present, groups should be at ### (depth 3) and subgroups at #### (depth 4)
        let grouped = vec![GroupedCommits {
            title: "Features".into(),
            commits: vec![ci("feat: add X", "feat", "add X", "abc1234")],
            subgroups: vec![GroupedCommits::new(
                "UI",
                vec![ci("feat: add button", "feat", "add button", "def5678")],
            )],
        }];
        let md = render_changelog(&grouped, 7, None, "", "git", None, None);
        assert!(md.contains("### Features"), "groups at depth 3: {md}");
        assert!(md.contains("#### UI"), "subgroups at depth 4: {md}");
    }

    // -----------------------------------------------------------------------
    // Tests for gitlab and gitea use sources
    // -----------------------------------------------------------------------

    #[test]
    fn test_config_parse_use_source_gitlab() {
        let yaml = r#"
use: gitlab
"#;
        let cfg: anodize_core::config::ChangelogConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.use_source.as_deref(), Some("gitlab"));
    }

    #[test]
    fn test_config_parse_use_source_gitea() {
        let yaml = r#"
use: gitea
"#;
        let cfg: anodize_core::config::ChangelogConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.use_source.as_deref(), Some("gitea"));
    }

    #[test]
    fn test_validation_rejects_unsupported_source() {
        // Exercise the actual production validation path — "bitbucket" should
        // cause the stage to bail with "unsupported use source".
        use anodize_core::config::{ChangelogConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let mut config = Config::default();
        config.project_name = "test".to_string();
        config.changelog = Some(ChangelogConfig {
            use_source: Some("bitbucket".to_string()),
            ..Default::default()
        });
        config.crates = vec![CrateConfig {
            name: "mylib".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }];

        let mut ctx = Context::new(config, ContextOptions::default());
        let stage = ChangelogStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("unsupported use source"), "got: {msg}");
    }

    #[test]
    fn test_changelog_stage_gitlab_falls_back_to_git_no_token() {
        // When use: gitlab but no token is available, should fall back to git
        // (which will also fail in a test environment, but the point is that
        // the stage doesn't bail on "unsupported use source").
        use anodize_core::config::{ChangelogConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.project_name = "test".to_string();
        config.dist = tmp.path().to_path_buf();
        config.changelog = Some(ChangelogConfig {
            use_source: Some("gitlab".to_string()),
            ..Default::default()
        });
        config.crates = vec![CrateConfig {
            name: "test".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }];

        let opts = ContextOptions {
            dry_run: true,
            // No token — should trigger fallback to git.
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);

        let stage = ChangelogStage;
        // Should not bail with "unsupported use source". It will either succeed
        // with git fallback or produce a git-based changelog.
        let result = stage.run(&mut ctx);
        // The stage should succeed (git fallback works in test git repo context).
        assert!(
            result.is_ok(),
            "gitlab with no token should fall back to git: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_changelog_stage_gitea_falls_back_to_git_no_token() {
        use anodize_core::config::{ChangelogConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.project_name = "test".to_string();
        config.dist = tmp.path().to_path_buf();
        config.changelog = Some(ChangelogConfig {
            use_source: Some("gitea".to_string()),
            ..Default::default()
        });
        config.crates = vec![CrateConfig {
            name: "test".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }];

        let opts = ContextOptions {
            dry_run: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);

        let stage = ChangelogStage;
        let result = stage.run(&mut ctx);
        assert!(
            result.is_ok(),
            "gitea with no token should fall back to git: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_changelog_stage_unsupported_source_bails() {
        use anodize_core::config::{ChangelogConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let mut config = Config::default();
        config.project_name = "test".to_string();
        config.changelog = Some(ChangelogConfig {
            use_source: Some("bitbucket".to_string()),
            ..Default::default()
        });
        config.crates = vec![CrateConfig {
            name: "test".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }];

        let mut ctx = Context::new(config, ContextOptions::default());

        let stage = ChangelogStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err(), "unsupported source should bail");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("unsupported use source"),
            "error should mention unsupported use source: {}",
            err_msg
        );
        assert!(
            err_msg.contains("gitlab") && err_msg.contains("gitea"),
            "error should list gitlab and gitea as valid options: {}",
            err_msg
        );
    }

    #[test]
    fn test_render_changelog_gitlab_default_format() {
        // When use source is "gitlab", the default format includes author info.
        let mut commit = ci("feat: add feature", "feat", "add feature", "abc1234");
        commit.author_name = "Jane Dev".to_string();
        commit.author_email = "jane@example.com".to_string();
        let grouped = vec![GroupedCommits {
            title: String::new(),
            commits: vec![commit],
            subgroups: Vec::new(),
        }];
        let md = render_changelog(&grouped, 7, None, "", "gitlab", None, None);
        // Default format for gitlab should include author info.
        assert!(
            md.contains("Jane Dev"),
            "gitlab format should include author name: {}",
            md
        );
        assert!(
            md.contains("jane@example.com"),
            "gitlab format should include author email: {}",
            md
        );
    }

    #[test]
    fn test_render_changelog_gitea_default_format_with_login() {
        // When use source is "gitea" and login is present, format includes @login.
        let mut commit = ci("feat: add feature", "feat", "add feature", "abc1234");
        commit.login = "janedev".to_string();
        let grouped = vec![GroupedCommits {
            title: String::new(),
            commits: vec![commit],
            subgroups: Vec::new(),
        }];
        let md = render_changelog(&grouped, 7, None, "", "gitea", None, None);
        assert!(
            md.contains("@janedev"),
            "gitea format should include @login: {}",
            md
        );
    }
}
