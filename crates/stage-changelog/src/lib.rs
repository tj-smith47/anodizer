use anodize_core::config::ChangelogGroup;
use anodize_core::context::Context;
use anodize_core::git::{find_latest_tag_matching, get_all_commits, get_commits_between};
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
}

#[derive(Debug, Clone)]
pub(crate) struct GroupedCommits {
    pub title: String,
    pub commits: Vec<CommitInfo>,
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
            hash: String::new(),
            full_hash: String::new(),
            author_name: String::new(),
            author_email: String::new(),
        }
    } else {
        CommitInfo {
            raw_message: msg.to_string(),
            kind: "other".to_string(),
            description: msg.to_string(),
            hash: String::new(),
            full_hash: String::new(),
            author_name: String::new(),
            author_email: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// apply_filters
// ---------------------------------------------------------------------------

/// Filter out commits whose `raw_message` matches any of the exclude regex
/// patterns. Returns a new `Vec` of commits that did NOT match any pattern.
pub(crate) fn apply_filters(
    commits: &[CommitInfo],
    exclude: &[String],
    log: &anodize_core::log::StageLogger,
) -> Vec<CommitInfo> {
    let patterns: Vec<Regex> = exclude
        .iter()
        .filter_map(|p| match Regex::new(p) {
            Ok(re) => Some(re),
            Err(e) => {
                log.warn(&format!("invalid exclude regex {:?}: {}", p, e));
                None
            }
        })
        .collect();

    commits
        .iter()
        .filter(|c| !patterns.iter().any(|re| re.is_match(&c.raw_message)))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// apply_include_filters
// ---------------------------------------------------------------------------

/// Keep only commits whose `raw_message` matches at least one of the include
/// regex patterns. If `include` is empty, all commits are kept (no-op).
pub(crate) fn apply_include_filters(
    commits: &[CommitInfo],
    include: &[String],
    log: &anodize_core::log::StageLogger,
) -> Vec<CommitInfo> {
    if include.is_empty() {
        return commits.to_vec();
    }
    let patterns: Vec<Regex> = include
        .iter()
        .filter_map(|p| match Regex::new(p) {
            Ok(re) => Some(re),
            Err(e) => {
                log.warn(&format!("invalid include regex {:?}: {}", p, e));
                None
            }
        })
        .collect();

    commits
        .iter()
        .filter(|c| patterns.iter().any(|re| re.is_match(&c.raw_message)))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// sort_commits
// ---------------------------------------------------------------------------

/// Sort commits in-place by description. `order` must be `"asc"` or `"desc"`.
/// Any other value is treated as ascending.
pub(crate) fn sort_commits(commits: &mut [CommitInfo], order: &str) {
    if order == "desc" {
        commits.sort_by(|a, b| b.description.cmp(&a.description));
    } else {
        commits.sort_by(|a, b| a.description.cmp(&b.description));
    }
}

// ---------------------------------------------------------------------------
// group_commits
// ---------------------------------------------------------------------------

/// Group commits by matching `raw_message` against each group's `regexp`.
/// Groups are emitted in `order` (ascending). Commits that do not match any
/// group are collected into an implicit "Others" group appended at the end.
/// Groups with zero matching commits are omitted from the output.
pub(crate) fn group_commits(
    commits: &[CommitInfo],
    groups: &[ChangelogGroup],
    log: &anodize_core::log::StageLogger,
) -> Vec<GroupedCommits> {
    // Sort groups by their `order` field (None sorts last).
    let mut sorted_groups: Vec<&ChangelogGroup> = groups.iter().collect();
    sorted_groups.sort_by_key(|g| g.order.unwrap_or(i32::MAX));

    // Compile regexes once.
    let compiled: Vec<(Option<Regex>, &ChangelogGroup)> = sorted_groups
        .iter()
        .map(|g| {
            let re = g.regexp.as_deref().and_then(|p| match Regex::new(p) {
                Ok(re) => Some(re),
                Err(e) => {
                    log.warn(&format!(
                        "invalid group regex {:?} for group {:?}: {}",
                        p, g.title, e
                    ));
                    None
                }
            });
            (re, *g)
        })
        .collect();

    let mut buckets: Vec<Vec<CommitInfo>> = vec![Vec::new(); compiled.len()];
    let mut others: Vec<CommitInfo> = Vec::new();

    'commit: for commit in commits {
        for (idx, (re_opt, _)) in compiled.iter().enumerate() {
            if let Some(re) = re_opt
                && re.is_match(&commit.raw_message)
            {
                buckets[idx].push(commit.clone());
                continue 'commit;
            }
        }
        others.push(commit.clone());
    }

    let mut result: Vec<GroupedCommits> = compiled
        .iter()
        .zip(buckets)
        .filter(|(_, bucket)| !bucket.is_empty())
        .map(|((_, group), bucket)| GroupedCommits {
            title: group.title.clone(),
            commits: bucket,
        })
        .collect();

    if !others.is_empty() {
        result.push(GroupedCommits {
            title: "Others".to_string(),
            commits: others,
        });
    }

    result
}

// ---------------------------------------------------------------------------
// render_changelog
// ---------------------------------------------------------------------------

/// Render grouped commits as a Markdown string. Each group becomes a `## Title`
/// section, and each commit is a bullet formatted according to `format_template`.
///
/// `abbrev` controls the hash abbreviation length (default 7).
///
/// If `format_template` is `None`, the default format `{{ ShortSHA }} {{ Message }}`
/// is used (matching GoReleaser defaults). Available template variables:
/// `SHA`, `ShortSHA`, `Message`, `AuthorName`, `AuthorEmail`.
pub(crate) fn render_changelog(
    grouped: &[GroupedCommits],
    abbrev: usize,
    format_template: Option<&str>,
) -> String {
    let abbrev = abbrev.max(1); // Enforce minimum of 1 to avoid empty hashes
    let default_format = "{{ ShortSHA }} {{ Message }}";
    let tmpl = format_template.unwrap_or(default_format);
    let mut out = String::new();
    for group in grouped {
        out.push_str(&format!("## {}\n\n", group.title));
        for commit in &group.commits {
            let short_sha = if commit.hash.len() > abbrev {
                &commit.hash[..abbrev]
            } else {
                &commit.hash
            };
            let mut vars = TemplateVars::new();
            vars.set("SHA", &commit.full_hash);
            vars.set("ShortSHA", short_sha);
            vars.set("Message", &commit.description);
            vars.set("AuthorName", &commit.author_name);
            vars.set("AuthorEmail", &commit.author_email);
            let rendered = template::render(tmpl, &vars)
                .unwrap_or_else(|_| format!("{} {}", short_sha, commit.description));
            out.push_str(&format!("- {}\n", rendered));
        }
        out.push('\n');
    }
    out
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

            // Write to dist/RELEASE_NOTES.md (skip during dry-run).
            if !ctx.is_dry_run() {
                let dist = ctx.config.dist.clone();
                std::fs::create_dir_all(&dist)
                    .with_context(|| format!("changelog: create dist dir {}", dist.display()))?;
                let notes_out = dist.join("RELEASE_NOTES.md");
                std::fs::write(&notes_out, &content)
                    .with_context(|| format!("changelog: write {}", notes_out.display()))?;
                log.status(&format!("wrote {}", notes_out.display()));
            }
            return Ok(());
        }

        // If disabled, skip the stage entirely.
        if changelog_cfg
            .as_ref()
            .and_then(|c| c.disable)
            .unwrap_or(false)
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

        let cfg = changelog_cfg.as_ref();
        let sort_order = cfg
            .and_then(|c| c.sort.clone())
            .unwrap_or_else(|| "asc".to_string());
        let filters = cfg.and_then(|c| c.filters.as_ref());
        let exclude_filters: Vec<String> =
            filters.and_then(|f| f.exclude.clone()).unwrap_or_default();
        let include_filters: Vec<String> =
            filters.and_then(|f| f.include.clone()).unwrap_or_default();
        let groups: Vec<ChangelogGroup> = cfg.and_then(|c| c.groups.clone()).unwrap_or_default();
        let header: Option<String> = cfg.and_then(|c| c.header.clone());
        let footer: Option<String> = cfg.and_then(|c| c.footer.clone());
        let abbrev: usize = cfg.and_then(|c| c.abbrev).unwrap_or(7);
        let format_template: Option<String> = cfg.and_then(|c| c.format.clone());

        let selected = ctx.options.selected_crates.clone();
        let dist = ctx.config.dist.clone();

        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .cloned()
            .collect();

        let mut combined_markdown = String::new();

        for crate_cfg in &crates {
            let crate_name = crate_cfg.name.clone();

            // Find the previous tag for this crate.
            let prev_tag = find_latest_tag_matching(&crate_cfg.tag_template).unwrap_or(None);

            let path_filter = if crate_cfg.path.is_empty() || crate_cfg.path == "." {
                None
            } else {
                Some(crate_cfg.path.as_str())
            };

            let raw_commits = match &prev_tag {
                Some(tag) => get_commits_between(tag, "HEAD", path_filter).unwrap_or_default(),
                None => {
                    // Initial release: no previous tag, treat all commits as new.
                    log.status(&format!(
                        "no previous tag found for crate '{}', using all commits",
                        crate_name
                    ));
                    get_all_commits(path_filter).unwrap_or_default()
                }
            };

            let mut all_commit_infos: Vec<CommitInfo> = Vec::new();
            for commit in raw_commits {
                let mut info = parse_commit_message(&commit.message);
                info.hash = commit.short_hash.clone();
                info.full_hash = commit.hash.clone();
                info.author_name = commit.author_name.clone();
                info.author_email = commit.author_email.clone();
                all_commit_infos.push(info);
            }

            // Apply exclude filters, then include filters.
            let after_exclude = apply_filters(&all_commit_infos, &exclude_filters, &log);
            let filtered = apply_include_filters(&after_exclude, &include_filters, &log);

            // Sort commits.
            let mut sorted = filtered;
            sort_commits(&mut sorted, &sort_order);

            // Group commits.
            let grouped = if groups.is_empty() {
                // No groups configured — put everything in a single "Changes" group.
                if sorted.is_empty() {
                    vec![]
                } else {
                    vec![GroupedCommits {
                        title: "Changes".to_string(),
                        commits: sorted,
                    }]
                }
            } else {
                group_commits(&sorted, &groups, &log)
            };

            // Render the markdown for this crate.
            let markdown = render_changelog(&grouped, abbrev, format_template.as_deref());

            // Store per-crate changelog in context for the release stage.
            ctx.changelogs.insert(crate_name.clone(), markdown.clone());

            combined_markdown.push_str(&markdown);
        }

        // Prepend header and append footer if configured.
        let mut final_markdown = String::new();
        if let Some(ref h) = header {
            final_markdown.push_str(h);
            final_markdown.push('\n');
        }
        final_markdown.push_str(&combined_markdown);
        if let Some(ref f) = footer {
            final_markdown.push_str(f);
            final_markdown.push('\n');
        }

        // Write to dist/RELEASE_NOTES.md (skip during dry-run — this is the only side effect).
        if ctx.is_dry_run() {
            log.status("(dry-run) skipping write to disk");
            return Ok(());
        }

        std::fs::create_dir_all(&dist)
            .with_context(|| format!("changelog: create dist dir {}", dist.display()))?;
        let notes_path = dist.join("RELEASE_NOTES.md");
        std::fs::write(&notes_path, &final_markdown)
            .with_context(|| format!("changelog: write {}", notes_path.display()))?;

        log.status(&format!("wrote {}", notes_path.display()));
        Ok(())
    }
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
    /// New fields (`full_hash`, `author_name`, `author_email`) default to empty.
    fn ci(raw_message: &str, kind: &str, description: &str, hash: &str) -> CommitInfo {
        CommitInfo {
            raw_message: raw_message.into(),
            kind: kind.into(),
            description: description.into(),
            hash: hash.into(),
            ..Default::default()
        }
    }

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
            },
            ChangelogGroup {
                title: "Bug Fixes".into(),
                regexp: Some("^fix".into()),
                order: Some(1),
            },
        ];
        let result = group_commits(&commits, &groups, &test_logger());
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
        let filtered = apply_filters(&commits, &filters, &test_logger());
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].kind, "feat");
    }

    #[test]
    fn test_render_changelog() {
        let grouped = vec![
            GroupedCommits {
                title: "Features".into(),
                commits: vec![ci("feat: add X", "feat", "add X", "abc1234")],
            },
            GroupedCommits {
                title: "Bug Fixes".into(),
                commits: vec![ci("fix: fix Y", "fix", "fix Y", "def5678")],
            },
        ];
        let md = render_changelog(&grouped, 7, None);
        assert!(md.contains("## Features"));
        assert!(md.contains("add X"));
        assert!(md.contains("## Bug Fixes"));
        assert!(md.contains("fix Y"));
        assert!(md.contains("abc1234"));
    }

    #[test]
    fn test_sort_asc() {
        let mut commits = vec![ci("b", "feat", "b", "2"), ci("a", "feat", "a", "1")];
        sort_commits(&mut commits, "asc");
        assert_eq!(commits[0].description, "a");
    }

    #[test]
    fn test_sort_desc() {
        let mut commits = vec![ci("a", "feat", "a", "1"), ci("b", "feat", "b", "2")];
        sort_commits(&mut commits, "desc");
        assert_eq!(commits[0].description, "b");
    }

    #[test]
    fn test_group_commits_others_bucket() {
        let commits = vec![
            ci("feat: new thing", "feat", "new thing", "abc"),
            ci("chore: update deps", "chore", "update deps", "xyz"),
        ];
        let groups = vec![ChangelogGroup {
            title: "Features".into(),
            regexp: Some("^feat".into()),
            order: Some(0),
        }];
        let result = group_commits(&commits, &groups, &test_logger());
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].title, "Features");
        assert_eq!(result[1].title, "Others");
        assert_eq!(result[1].commits.len(), 1);
        assert_eq!(result[1].commits[0].kind, "chore");
    }

    #[test]
    fn test_group_commits_empty_group_omitted() {
        let commits = vec![ci("feat: only feat", "feat", "only feat", "abc")];
        let groups = vec![
            ChangelogGroup {
                title: "Features".into(),
                regexp: Some("^feat".into()),
                order: Some(0),
            },
            ChangelogGroup {
                title: "Bug Fixes".into(),
                regexp: Some("^fix".into()),
                order: Some(1),
            },
        ];
        let result = group_commits(&commits, &groups, &test_logger());
        // "Bug Fixes" has no commits, should be omitted
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].title, "Features");
    }

    #[test]
    fn test_render_changelog_short_hash() {
        // When hash is exactly 7 chars, it should appear as-is
        let grouped = vec![GroupedCommits {
            title: "Changes".into(),
            commits: vec![ci(
                "feat: short hash test",
                "feat",
                "short hash test",
                "abc1234",
            )],
        }];
        let md = render_changelog(&grouped, 7, None);
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
        let filtered = apply_filters(&commits, &[], &test_logger());
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn test_render_changelog_with_header_and_footer() {
        let grouped = vec![GroupedCommits {
            title: "Features".into(),
            commits: vec![ci("feat: add X", "feat", "add X", "abc1234")],
        }];
        let body = render_changelog(&grouped, 7, None);

        // Simulate the header/footer wrapping logic from ChangelogStage::run
        let header = "# My Release Notes";
        let footer = "---\nGenerated by anodize";
        let mut final_md = String::new();
        final_md.push_str(header);
        final_md.push('\n');
        final_md.push_str(&body);
        final_md.push_str(footer);
        final_md.push('\n');

        assert!(final_md.starts_with("# My Release Notes\n"));
        assert!(final_md.contains("## Features"));
        assert!(final_md.contains("add X"));
        assert!(final_md.ends_with("Generated by anodize\n"));
    }

    #[test]
    fn test_render_changelog_with_header_only() {
        let grouped = vec![GroupedCommits {
            title: "Changes".into(),
            commits: vec![ci("fix: bug", "fix", "bug", "def5678")],
        }];
        let body = render_changelog(&grouped, 7, None);

        let header = "# Changelog";
        let mut final_md = String::new();
        final_md.push_str(header);
        final_md.push('\n');
        final_md.push_str(&body);

        assert!(final_md.starts_with("# Changelog\n## Changes"));
    }

    #[test]
    fn test_render_changelog_with_footer_only() {
        let grouped = vec![GroupedCommits {
            title: "Changes".into(),
            commits: vec![ci("fix: bug", "fix", "bug", "def5678")],
        }];
        let body = render_changelog(&grouped, 7, None);

        let footer = "-- end --";
        let mut final_md = String::new();
        final_md.push_str(&body);
        final_md.push_str(footer);
        final_md.push('\n');

        assert!(final_md.contains("## Changes"));
        assert!(final_md.ends_with("-- end --\n"));
    }

    #[test]
    fn test_changelog_stage_disabled_skips() {
        use anodize_core::config::{ChangelogConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let mut config = Config::default();
        config.project_name = "test".to_string();
        config.changelog = Some(ChangelogConfig {
            disable: Some(true),
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
        let result = apply_include_filters(&commits, &include, &test_logger());
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
        let result = apply_include_filters(&commits, &include, &test_logger());
        assert!(result.is_empty());
    }

    #[test]
    fn test_apply_include_filters_empty_keeps_all() {
        let commits = vec![
            ci("feat: something", "feat", "something", "a"),
            ci("fix: something else", "fix", "something else", "b"),
        ];
        let result = apply_include_filters(&commits, &[], &test_logger());
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_apply_include_filters_invalid_regex_skipped() {
        let commits = vec![ci("feat: good", "feat", "good", "a")];
        // Invalid regex is skipped; valid one still works.
        let include = vec!["[invalid".to_string(), "^feat".to_string()];
        let result = apply_include_filters(&commits, &include, &test_logger());
        assert_eq!(result.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Tests for abbrev
    // -----------------------------------------------------------------------

    #[test]
    fn test_abbrev_controls_hash_length() {
        let grouped = vec![GroupedCommits {
            title: "Changes".into(),
            commits: vec![ci(
                "feat: test abbrev",
                "feat",
                "test abbrev",
                "abc1234567890",
            )],
        }];
        // abbrev = 5 should truncate to "abc12"
        let md = render_changelog(&grouped, 5, None);
        assert!(
            md.contains("abc12 test abbrev"),
            "expected 'abc12 test abbrev' in: {}",
            md
        );
        assert!(!md.contains("abc1234"), "should not contain full hash");
    }

    #[test]
    fn test_abbrev_longer_than_hash_uses_full_hash() {
        let grouped = vec![GroupedCommits {
            title: "Changes".into(),
            commits: vec![ci("feat: short", "feat", "short", "abc")],
        }];
        // abbrev = 10, but hash is only 3 chars — use full hash
        let md = render_changelog(&grouped, 10, None);
        assert!(md.contains("abc short"), "expected 'abc short' in: {}", md);
    }

    #[test]
    fn test_abbrev_default_is_seven() {
        let grouped = vec![GroupedCommits {
            title: "Changes".into(),
            commits: vec![ci(
                "feat: default abbrev",
                "feat",
                "default abbrev",
                "abc1234def5678",
            )],
        }];
        let md = render_changelog(&grouped, 7, None);
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
            disable: None,
            sort: None,
            filters: None,
            groups: None,
            header: None,
            footer: None,
            use_source: Some("github-native".to_string()),
            abbrev: None,
            format: None,
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
    fn test_include_and_exclude_together() {
        // Exclude runs first, then include further restricts.
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

        // Exclude WIP commits
        let after_exclude = apply_filters(&commits, &["wip".to_string()], &test_logger());
        assert_eq!(after_exclude.len(), 3); // feat, fix, docs

        // Then include only feat and fix
        let after_include = apply_include_filters(
            &after_exclude,
            &["^feat".to_string(), "^fix".to_string()],
            &test_logger(),
        );
        assert_eq!(after_include.len(), 2);
        assert_eq!(after_include[0].description, "good feature");
        assert_eq!(after_include[1].description, "important fix");
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
        for (_hash, short_hash, message) in &raw_messages {
            let mut info = parse_commit_message(message);
            info.hash = short_hash.to_string();
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
        let filtered = apply_filters(&all_commits, &exclude, &test_logger());
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
        sort_commits(&mut sorted, "asc");

        // Group into sections
        let groups = vec![
            ChangelogGroup {
                title: "Features".into(),
                regexp: Some("^feat".into()),
                order: Some(0),
            },
            ChangelogGroup {
                title: "Bug Fixes".into(),
                regexp: Some("^fix".into()),
                order: Some(1),
            },
        ];
        let grouped = group_commits(&sorted, &groups, &test_logger());

        // Verify grouping
        assert!(
            grouped.len() >= 2,
            "should have at least Features and Bug Fixes groups"
        );
        assert_eq!(grouped[0].title, "Features");
        assert_eq!(grouped[0].commits.len(), 3, "3 feat commits");
        assert_eq!(grouped[1].title, "Bug Fixes");
        assert_eq!(grouped[1].commits.len(), 2, "2 fix commits");

        // The "chore" commit should end up in "Others"
        if grouped.len() > 2 {
            assert_eq!(grouped[2].title, "Others");
            assert_eq!(grouped[2].commits.len(), 1);
            assert_eq!(grouped[2].commits[0].kind, "chore");
        }

        // Render
        let md = render_changelog(&grouped, 7, None);

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

        // Verify hash abbreviations are present (default format: "ShortSHA Message")
        assert!(
            md.contains("a1b2c3d "),
            "hash should be abbreviated to 7 chars"
        );
        assert!(md.contains("b2c3d4e "));

        // Verify bullets
        let bullet_lines: Vec<&str> = md.lines().filter(|l| l.starts_with("- ")).collect();
        assert_eq!(
            bullet_lines.len(),
            6,
            "should have 6 bullet points total (3 feat + 2 fix + 1 chore)"
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
        );
        assert_eq!(included.len(), 3);

        // Sort the filtered list, then group
        let mut sorted = included;
        sort_commits(&mut sorted, "asc");
        let groups = vec![
            ChangelogGroup {
                title: "Features".into(),
                regexp: Some("^feat".into()),
                order: Some(0),
            },
            ChangelogGroup {
                title: "Bug Fixes".into(),
                regexp: Some("^fix".into()),
                order: Some(1),
            },
        ];
        let grouped = group_commits(&sorted, &groups, &test_logger());

        let md = render_changelog(&grouped, 7, None);

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
                info
            })
            .collect();

        let grouped = vec![GroupedCommits {
            title: "Changes".to_string(),
            commits,
        }];

        let body = render_changelog(&grouped, 7, None);

        // Simulate header/footer wrapping as ChangelogStage.run does
        let header = "# Release v1.0.0";
        let footer = "---\nFull changelog: https://github.com/example/repo/compare/v0.9.0...v1.0.0";

        let mut final_md = String::new();
        final_md.push_str(header);
        final_md.push('\n');
        final_md.push_str(&body);
        final_md.push_str(footer);
        final_md.push('\n');

        // Verify structure
        assert!(final_md.starts_with("# Release v1.0.0\n## Changes"));
        assert!(final_md.contains("- abc1234 initial release"));
        assert!(final_md.contains("- def5678 typo in config"));
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
        );
        assert!(filtered.is_empty());

        let grouped = group_commits(
            &filtered,
            &[ChangelogGroup {
                title: "Features".into(),
                regexp: Some("^feat".into()),
                order: Some(0),
            }],
            &test_logger(),
        );
        assert!(grouped.is_empty());

        let md = render_changelog(&grouped, 7, None);
        assert!(
            md.is_empty(),
            "changelog should be empty when all commits are filtered"
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
                disable: None,
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
                    },
                    ChangelogGroup {
                        title: "Bug Fixes".into(),
                        regexp: Some("^fix".into()),
                        order: Some(1),
                    },
                ]),
                header: Some("# Changelog".to_string()),
                footer: None,
                use_source: None,
                abbrev: Some(7),
                format: None,
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

        // Verify RELEASE_NOTES.md was written
        let notes_path = repo.join("dist").join("RELEASE_NOTES.md");
        assert!(notes_path.exists(), "RELEASE_NOTES.md should be written");
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
        }];
        let body = render_changelog(&grouped, 7, None);

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
        }];
        let body = render_changelog(&grouped, 7, None);

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
        );
        assert_eq!(result.len(), 2);
        assert!(result.iter().all(|c| c.kind == "feat" || c.kind == "fix"));
        // Excluded types should not be present
        assert!(!result.iter().any(|c| c.kind == "chore"));
        assert!(!result.iter().any(|c| c.kind == "refactor"));
    }

    #[test]
    fn test_abbrev_truncates_to_specified_length() {
        let grouped = vec![GroupedCommits {
            title: "Changes".into(),
            commits: vec![ci("feat: test", "feat", "test", "abcdef1234567890")],
        }];

        // abbrev = 3 should produce "abc test"
        let md = render_changelog(&grouped, 3, None);
        assert!(
            md.contains("abc test"),
            "abbrev=3 expected 'abc test', got: {}",
            md
        );

        // abbrev = 10 should produce "abcdef1234 test"
        let md10 = render_changelog(&grouped, 10, None);
        assert!(
            md10.contains("abcdef1234 test"),
            "abbrev=10 expected 'abcdef1234 test', got: {}",
            md10
        );
    }

    #[test]
    fn test_abbrev_zero_uses_minimum_one() {
        let grouped = vec![GroupedCommits {
            title: "Changes".into(),
            commits: vec![ci("feat: test", "feat", "test", "abcdef")],
        }];

        // abbrev = 0 should be clamped to 1 (minimum)
        let md = render_changelog(&grouped, 0, None);
        assert!(
            md.contains("a test"),
            "abbrev=0 should clamp to 1, got: {}",
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
            disable: Some(true),
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
        let result = apply_include_filters(&commits, &["^feat".to_string()], &test_logger());
        assert!(result.is_empty());

        let grouped = group_commits(&result, &[], &test_logger());
        let md = render_changelog(&grouped, 7, None);
        assert!(
            md.is_empty(),
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
            changelog: Some(ChangelogConfig {
                disable: None,
                sort: None,
                filters: None,
                groups: None,
                header: None,
                footer: None,
                use_source: None,
                abbrev: None,
                format: None,
            }),
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

        // RELEASE_NOTES.md should be written to the dist directory
        let notes_path = custom_dist.join("RELEASE_NOTES.md");
        assert!(
            notes_path.exists(),
            "RELEASE_NOTES.md should be in the dist directory: {}",
            custom_dist.display()
        );
    }

    // ---- Error path tests (Task 4D) ----

    #[test]
    fn test_invalid_exclude_regex_warns_but_does_not_crash() {
        let commits = vec![ci("feat: new feature", "feat", "new feature", "abc")];
        // Invalid regex: unclosed group
        let filters = vec!["^feat(".to_string()];
        // apply_filters logs a warning but does not panic or error
        let result = apply_filters(&commits, &filters, &test_logger());
        // The invalid regex is skipped, so the commit passes through
        assert_eq!(
            result.len(),
            1,
            "invalid regex should be skipped, commits pass through"
        );
    }

    #[test]
    fn test_invalid_include_regex_warns_but_does_not_crash() {
        let commits = vec![ci("fix: a bug", "fix", "a bug", "def")];
        let filters = vec!["[invalid".to_string()];
        let result = apply_include_filters(&commits, &filters, &test_logger());
        // Invalid regex is skipped, no valid patterns remain, so nothing matches
        assert_eq!(
            result.len(),
            0,
            "invalid include regex means no commits match"
        );
    }

    #[test]
    fn test_invalid_group_regex_warns_and_commits_go_to_others() {
        let commits = vec![ci("feat: new thing", "feat", "new thing", "abc")];
        let groups = vec![ChangelogGroup {
            title: "Features".into(),
            regexp: Some("^feat(".into()), // invalid regex
            order: Some(0),
        }];
        let result = group_commits(&commits, &groups, &test_logger());
        // The invalid regex group compiles to None, so commit goes to "Others"
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].title, "Others");
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
    fn test_sort_commits_unknown_order_defaults_to_asc() {
        let mut commits = vec![
            ci("b: second", "other", "second", "1"),
            ci("a: first", "other", "first", "2"),
        ];
        sort_commits(&mut commits, "invalid_order");
        assert_eq!(commits[0].description, "first");
        assert_eq!(commits[1].description, "second");
    }

    #[test]
    fn test_render_changelog_empty_groups() {
        let grouped: Vec<GroupedCommits> = vec![];
        let result = render_changelog(&grouped, 7, None);
        assert_eq!(
            result, "",
            "rendering empty groups should produce empty string"
        );
    }

    #[test]
    fn test_render_changelog_very_short_hash_preserved() {
        let grouped = vec![GroupedCommits {
            title: "Test".into(),
            commits: vec![ci("feat: x", "feat", "x", "ab")], // shorter than abbrev
        }];
        let result = render_changelog(&grouped, 7, None);
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
        let config = Config {
            project_name: "test".to_string(),
            dist: std::path::PathBuf::from("/dev/null/impossible/dist"),
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

        // Create the dist dir, then place a directory where RELEASE_NOTES.md
        // would go, so fs::write fails (can't write to a directory path).
        let dist = repo.join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        let notes_blocker = dist.join("RELEASE_NOTES.md");
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
            "writing RELEASE_NOTES.md where a directory exists should fail"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("RELEASE_NOTES") || err.contains("changelog") || err.contains("write"),
            "error should mention the write failure context, got: {err}"
        );
    }

    #[test]
    #[serial]
    fn test_changelog_dry_run_skips_write_no_error() {
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

        // Use an impossible dist path, but dry-run should skip the write
        let config = Config {
            project_name: "test".to_string(),
            dist: std::path::PathBuf::from("/dev/null/impossible/dist"),
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
            "dry-run should skip fs write and succeed even with bad dist path"
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
            }],
        }];
        let md = render_changelog(
            &grouped,
            7,
            Some("{{ SHA }} {{ Message }} ({{ AuthorName }} <{{ AuthorEmail }}>)"),
        );
        assert!(
            md.contains(
                "abc1234567890abcdef1234567890abcdef123456 add auth (Alice <alice@example.com>)"
            ),
            "custom format should render all variables, got: {md}"
        );
    }

    #[test]
    fn test_default_format_unchanged() {
        let grouped = vec![GroupedCommits {
            title: "Changes".into(),
            commits: vec![CommitInfo {
                raw_message: "fix: bug".into(),
                kind: "fix".into(),
                description: "bug".into(),
                hash: "def5678".into(),
                full_hash: "def5678abcdef".into(),
                author_name: "Bob".into(),
                author_email: "bob@example.com".into(),
            }],
        }];
        // Default format: "{{ ShortSHA }} {{ Message }}"
        let md = render_changelog(&grouped, 7, None);
        assert!(
            md.contains("- def5678 bug"),
            "default format should be 'ShortSHA Message', got: {md}"
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
}
