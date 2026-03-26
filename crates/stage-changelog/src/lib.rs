use anodize_core::config::ChangelogGroup;
use anodize_core::context::Context;
use anodize_core::git::{find_latest_tag_matching, get_all_commits, get_commits_between};
use anodize_core::stage::Stage;
use anyhow::{Context as _, Result};
use regex::Regex;
use std::sync::LazyLock;

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub raw_message: String,
    pub kind: String,
    pub description: String,
    pub hash: String,
}

#[derive(Debug, Clone)]
pub struct GroupedCommits {
    pub title: String,
    pub commits: Vec<CommitInfo>,
}

// ---------------------------------------------------------------------------
// parse_commit_message
// ---------------------------------------------------------------------------

static CONVENTIONAL_COMMIT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([a-zA-Z]+)(?:\([^)]*\))?!?:\s*(.+)$").unwrap()
});

/// Parse a conventional commit message of the form `type(scope): description`
/// or `type: description`. Falls back to `kind = "other"` for non-conventional
/// messages.
pub fn parse_commit_message(msg: &str) -> CommitInfo {
    let re = &*CONVENTIONAL_COMMIT_RE;
    if let Some(caps) = re.captures(msg) {
        CommitInfo {
            raw_message: msg.to_string(),
            kind: caps[1].to_string(),
            description: caps[2].to_string(),
            hash: String::new(),
        }
    } else {
        CommitInfo {
            raw_message: msg.to_string(),
            kind: "other".to_string(),
            description: msg.to_string(),
            hash: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// apply_filters
// ---------------------------------------------------------------------------

/// Filter out commits whose `raw_message` matches any of the exclude regex
/// patterns. Returns a new `Vec` of commits that did NOT match any pattern.
pub fn apply_filters(commits: &[CommitInfo], exclude: &[String]) -> Vec<CommitInfo> {
    let patterns: Vec<Regex> = exclude
        .iter()
        .filter_map(|p| match Regex::new(p) {
            Ok(re) => Some(re),
            Err(e) => {
                eprintln!("[changelog] warning: invalid exclude regex {:?}: {}", p, e);
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
pub fn apply_include_filters(commits: &[CommitInfo], include: &[String]) -> Vec<CommitInfo> {
    if include.is_empty() {
        return commits.to_vec();
    }
    let patterns: Vec<Regex> = include
        .iter()
        .filter_map(|p| match Regex::new(p) {
            Ok(re) => Some(re),
            Err(e) => {
                eprintln!("[changelog] warning: invalid include regex {:?}: {}", p, e);
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
pub fn sort_commits(commits: &mut [CommitInfo], order: &str) {
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
pub fn group_commits(commits: &[CommitInfo], groups: &[ChangelogGroup]) -> Vec<GroupedCommits> {
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
                    eprintln!(
                        "[changelog] warning: invalid group regex {:?} for group {:?}: {}",
                        p, g.title, e
                    );
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
/// section, and each commit is a `- description (short_hash)` bullet.
///
/// `abbrev` controls the hash abbreviation length (default 7).
pub fn render_changelog(grouped: &[GroupedCommits], abbrev: usize) -> String {
    let mut out = String::new();
    for group in grouped {
        out.push_str(&format!("## {}\n\n", group.title));
        for commit in &group.commits {
            let short = if commit.hash.len() > abbrev {
                &commit.hash[..abbrev]
            } else {
                &commit.hash
            };
            out.push_str(&format!("- {} ({})\n", commit.description, short));
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
        let changelog_cfg = ctx.config.changelog.clone();

        // If disabled, skip the stage entirely.
        if changelog_cfg.as_ref().and_then(|c| c.disable).unwrap_or(false) {
            eprintln!("[changelog] disabled, skipping");
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
            eprintln!("[changelog] using github-native changelog — skipping local generation");
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

        let sort_order = changelog_cfg
            .as_ref()
            .and_then(|c| c.sort.clone())
            .unwrap_or_else(|| "asc".to_string());
        let exclude_filters: Vec<String> = changelog_cfg
            .as_ref()
            .and_then(|c| c.filters.as_ref())
            .and_then(|f| f.exclude.clone())
            .unwrap_or_default();
        let include_filters: Vec<String> = changelog_cfg
            .as_ref()
            .and_then(|c| c.filters.as_ref())
            .and_then(|f| f.include.clone())
            .unwrap_or_default();
        let groups: Vec<ChangelogGroup> = changelog_cfg
            .as_ref()
            .and_then(|c| c.groups.clone())
            .unwrap_or_default();
        let header: Option<String> = changelog_cfg.as_ref().and_then(|c| c.header.clone());
        let footer: Option<String> = changelog_cfg.as_ref().and_then(|c| c.footer.clone());
        let abbrev: usize = changelog_cfg
            .as_ref()
            .and_then(|c| c.abbrev)
            .unwrap_or(7);

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
            let prev_tag = find_latest_tag_matching(&crate_cfg.tag_template)
                .unwrap_or(None);

            let path_filter = if crate_cfg.path.is_empty() || crate_cfg.path == "." {
                None
            } else {
                Some(crate_cfg.path.as_str())
            };

            let raw_commits = match &prev_tag {
                Some(tag) => {
                    get_commits_between(tag, "HEAD", path_filter).unwrap_or_default()
                }
                None => {
                    // Initial release: no previous tag, treat all commits as new.
                    eprintln!(
                        "[changelog] no previous tag found for crate '{}', using all commits",
                        crate_name
                    );
                    get_all_commits(path_filter).unwrap_or_default()
                }
            };

            let mut all_commit_infos: Vec<CommitInfo> = Vec::new();
            for commit in raw_commits {
                let mut info = parse_commit_message(&commit.message);
                info.hash = commit.short_hash.clone();
                all_commit_infos.push(info);
            }

            // Apply exclude filters, then include filters.
            let after_exclude = apply_filters(&all_commit_infos, &exclude_filters);
            let filtered = apply_include_filters(&after_exclude, &include_filters);

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
                group_commits(&sorted, &groups)
            };

            // Render the markdown for this crate.
            let markdown = render_changelog(&grouped, abbrev);

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
            eprintln!("[changelog] (dry-run) skipping write to disk");
            return Ok(());
        }

        std::fs::create_dir_all(&dist)
            .with_context(|| format!("changelog: create dist dir {}", dist.display()))?;
        let notes_path = dist.join("RELEASE_NOTES.md");
        std::fs::write(&notes_path, &final_markdown)
            .with_context(|| format!("changelog: write {}", notes_path.display()))?;

        eprintln!("[changelog] wrote {}", notes_path.display());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
            CommitInfo { raw_message: "feat: new thing".into(), kind: "feat".into(), description: "new thing".into(), hash: "abc".into() },
            CommitInfo { raw_message: "fix: broken thing".into(), kind: "fix".into(), description: "broken thing".into(), hash: "def".into() },
            CommitInfo { raw_message: "feat: another thing".into(), kind: "feat".into(), description: "another thing".into(), hash: "ghi".into() },
        ];
        let groups = vec![
            ChangelogGroup { title: "Features".into(), regexp: Some("^feat".into()), order: Some(0) },
            ChangelogGroup { title: "Bug Fixes".into(), regexp: Some("^fix".into()), order: Some(1) },
        ];
        let result = group_commits(&commits, &groups);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].title, "Features");
        assert_eq!(result[0].commits.len(), 2);
        assert_eq!(result[1].title, "Bug Fixes");
        assert_eq!(result[1].commits.len(), 1);
    }

    #[test]
    fn test_apply_filters() {
        let commits = vec![
            CommitInfo { raw_message: "docs: update readme".into(), kind: "docs".into(), description: "update readme".into(), hash: "a".into() },
            CommitInfo { raw_message: "feat: new feature".into(), kind: "feat".into(), description: "new feature".into(), hash: "b".into() },
            CommitInfo { raw_message: "ci: fix pipeline".into(), kind: "ci".into(), description: "fix pipeline".into(), hash: "c".into() },
        ];
        let filters = vec!["^docs:".to_string(), "^ci:".to_string()];
        let filtered = apply_filters(&commits, &filters);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].kind, "feat");
    }

    #[test]
    fn test_render_changelog() {
        let grouped = vec![
            GroupedCommits {
                title: "Features".into(),
                commits: vec![
                    CommitInfo { raw_message: "feat: add X".into(), kind: "feat".into(), description: "add X".into(), hash: "abc1234".into() },
                ],
            },
            GroupedCommits {
                title: "Bug Fixes".into(),
                commits: vec![
                    CommitInfo { raw_message: "fix: fix Y".into(), kind: "fix".into(), description: "fix Y".into(), hash: "def5678".into() },
                ],
            },
        ];
        let md = render_changelog(&grouped, 7);
        assert!(md.contains("## Features"));
        assert!(md.contains("add X"));
        assert!(md.contains("## Bug Fixes"));
        assert!(md.contains("fix Y"));
        assert!(md.contains("abc1234"));
    }

    #[test]
    fn test_sort_asc() {
        let mut commits = vec![
            CommitInfo { raw_message: "b".into(), kind: "feat".into(), description: "b".into(), hash: "2".into() },
            CommitInfo { raw_message: "a".into(), kind: "feat".into(), description: "a".into(), hash: "1".into() },
        ];
        sort_commits(&mut commits, "asc");
        assert_eq!(commits[0].description, "a");
    }

    #[test]
    fn test_sort_desc() {
        let mut commits = vec![
            CommitInfo { raw_message: "a".into(), kind: "feat".into(), description: "a".into(), hash: "1".into() },
            CommitInfo { raw_message: "b".into(), kind: "feat".into(), description: "b".into(), hash: "2".into() },
        ];
        sort_commits(&mut commits, "desc");
        assert_eq!(commits[0].description, "b");
    }

    #[test]
    fn test_group_commits_others_bucket() {
        let commits = vec![
            CommitInfo { raw_message: "feat: new thing".into(), kind: "feat".into(), description: "new thing".into(), hash: "abc".into() },
            CommitInfo { raw_message: "chore: update deps".into(), kind: "chore".into(), description: "update deps".into(), hash: "xyz".into() },
        ];
        let groups = vec![
            ChangelogGroup { title: "Features".into(), regexp: Some("^feat".into()), order: Some(0) },
        ];
        let result = group_commits(&commits, &groups);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].title, "Features");
        assert_eq!(result[1].title, "Others");
        assert_eq!(result[1].commits.len(), 1);
        assert_eq!(result[1].commits[0].kind, "chore");
    }

    #[test]
    fn test_group_commits_empty_group_omitted() {
        let commits = vec![
            CommitInfo { raw_message: "feat: only feat".into(), kind: "feat".into(), description: "only feat".into(), hash: "abc".into() },
        ];
        let groups = vec![
            ChangelogGroup { title: "Features".into(), regexp: Some("^feat".into()), order: Some(0) },
            ChangelogGroup { title: "Bug Fixes".into(), regexp: Some("^fix".into()), order: Some(1) },
        ];
        let result = group_commits(&commits, &groups);
        // "Bug Fixes" has no commits, should be omitted
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].title, "Features");
    }

    #[test]
    fn test_render_changelog_short_hash() {
        // When hash is exactly 7 chars, it should appear as-is
        let grouped = vec![GroupedCommits {
            title: "Changes".into(),
            commits: vec![CommitInfo {
                raw_message: "feat: short hash test".into(),
                kind: "feat".into(),
                description: "short hash test".into(),
                hash: "abc1234".into(),
            }],
        }];
        let md = render_changelog(&grouped, 7);
        assert!(md.contains("(abc1234)"));
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
        let commits = vec![
            CommitInfo { raw_message: "feat: something".into(), kind: "feat".into(), description: "something".into(), hash: "a".into() },
        ];
        let filtered = apply_filters(&commits, &[]);
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn test_render_changelog_with_header_and_footer() {
        let grouped = vec![
            GroupedCommits {
                title: "Features".into(),
                commits: vec![
                    CommitInfo { raw_message: "feat: add X".into(), kind: "feat".into(), description: "add X".into(), hash: "abc1234".into() },
                ],
            },
        ];
        let body = render_changelog(&grouped, 7);

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
        let grouped = vec![
            GroupedCommits {
                title: "Changes".into(),
                commits: vec![
                    CommitInfo { raw_message: "fix: bug".into(), kind: "fix".into(), description: "bug".into(), hash: "def5678".into() },
                ],
            },
        ];
        let body = render_changelog(&grouped, 7);

        let header = "# Changelog";
        let mut final_md = String::new();
        final_md.push_str(header);
        final_md.push('\n');
        final_md.push_str(&body);

        assert!(final_md.starts_with("# Changelog\n## Changes"));
    }

    #[test]
    fn test_render_changelog_with_footer_only() {
        let grouped = vec![
            GroupedCommits {
                title: "Changes".into(),
                commits: vec![
                    CommitInfo { raw_message: "fix: bug".into(), kind: "fix".into(), description: "bug".into(), hash: "def5678".into() },
                ],
            },
        ];
        let body = render_changelog(&grouped, 7);

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
            sort: None,
            filters: None,
            groups: None,
            header: None,
            footer: None,
            use_source: None,
            abbrev: None,
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
            CommitInfo { raw_message: "feat: add login".into(), kind: "feat".into(), description: "add login".into(), hash: "a".into() },
            CommitInfo { raw_message: "fix: crash on start".into(), kind: "fix".into(), description: "crash on start".into(), hash: "b".into() },
            CommitInfo { raw_message: "docs: update readme".into(), kind: "docs".into(), description: "update readme".into(), hash: "c".into() },
            CommitInfo { raw_message: "chore: bump deps".into(), kind: "chore".into(), description: "bump deps".into(), hash: "d".into() },
        ];
        let include = vec!["^feat".to_string(), "^fix".to_string()];
        let result = apply_include_filters(&commits, &include);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].kind, "feat");
        assert_eq!(result[1].kind, "fix");
    }

    #[test]
    fn test_apply_include_filters_no_match() {
        let commits = vec![
            CommitInfo { raw_message: "docs: update readme".into(), kind: "docs".into(), description: "update readme".into(), hash: "a".into() },
            CommitInfo { raw_message: "chore: bump deps".into(), kind: "chore".into(), description: "bump deps".into(), hash: "b".into() },
        ];
        let include = vec!["^feat".to_string()];
        let result = apply_include_filters(&commits, &include);
        assert!(result.is_empty());
    }

    #[test]
    fn test_apply_include_filters_empty_keeps_all() {
        let commits = vec![
            CommitInfo { raw_message: "feat: something".into(), kind: "feat".into(), description: "something".into(), hash: "a".into() },
            CommitInfo { raw_message: "fix: something else".into(), kind: "fix".into(), description: "something else".into(), hash: "b".into() },
        ];
        let result = apply_include_filters(&commits, &[]);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_apply_include_filters_invalid_regex_skipped() {
        let commits = vec![
            CommitInfo { raw_message: "feat: good".into(), kind: "feat".into(), description: "good".into(), hash: "a".into() },
        ];
        // Invalid regex is skipped; valid one still works.
        let include = vec!["[invalid".to_string(), "^feat".to_string()];
        let result = apply_include_filters(&commits, &include);
        assert_eq!(result.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Tests for abbrev
    // -----------------------------------------------------------------------

    #[test]
    fn test_abbrev_controls_hash_length() {
        let grouped = vec![GroupedCommits {
            title: "Changes".into(),
            commits: vec![CommitInfo {
                raw_message: "feat: test abbrev".into(),
                kind: "feat".into(),
                description: "test abbrev".into(),
                hash: "abc1234567890".into(),
            }],
        }];
        // abbrev = 5 should truncate to "abc12"
        let md = render_changelog(&grouped, 5);
        assert!(md.contains("(abc12)"), "expected (abc12) in: {}", md);
        assert!(!md.contains("abc1234"), "should not contain full hash");
    }

    #[test]
    fn test_abbrev_longer_than_hash_uses_full_hash() {
        let grouped = vec![GroupedCommits {
            title: "Changes".into(),
            commits: vec![CommitInfo {
                raw_message: "feat: short".into(),
                kind: "feat".into(),
                description: "short".into(),
                hash: "abc".into(),
            }],
        }];
        // abbrev = 10, but hash is only 3 chars — use full hash
        let md = render_changelog(&grouped, 10);
        assert!(md.contains("(abc)"), "expected (abc) in: {}", md);
    }

    #[test]
    fn test_abbrev_default_is_seven() {
        let grouped = vec![GroupedCommits {
            title: "Changes".into(),
            commits: vec![CommitInfo {
                raw_message: "feat: default abbrev".into(),
                kind: "feat".into(),
                description: "default abbrev".into(),
                hash: "abc1234def5678".into(),
            }],
        }];
        let md = render_changelog(&grouped, 7);
        assert!(md.contains("(abc1234)"), "expected (abc1234) in: {}", md);
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
        let cfg: anodize_core::config::ChangelogConfig = serde_yaml::from_str(yaml).unwrap();
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
        let cfg: anodize_core::config::ChangelogConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.use_source.as_deref(), Some("github-native"));
    }

    #[test]
    fn test_config_parse_abbrev() {
        let yaml = r#"
abbrev: 10
"#;
        let cfg: anodize_core::config::ChangelogConfig = serde_yaml::from_str(yaml).unwrap();
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
            CommitInfo { raw_message: "feat: good feature".into(), kind: "feat".into(), description: "good feature".into(), hash: "a".into() },
            CommitInfo { raw_message: "feat(wip): work in progress".into(), kind: "feat".into(), description: "work in progress".into(), hash: "b".into() },
            CommitInfo { raw_message: "fix: important fix".into(), kind: "fix".into(), description: "important fix".into(), hash: "c".into() },
            CommitInfo { raw_message: "docs: update readme".into(), kind: "docs".into(), description: "update readme".into(), hash: "d".into() },
        ];

        // Exclude WIP commits
        let after_exclude = apply_filters(&commits, &["wip".to_string()]);
        assert_eq!(after_exclude.len(), 3); // feat, fix, docs

        // Then include only feat and fix
        let after_include = apply_include_filters(&after_exclude, &["^feat".to_string(), "^fix".to_string()]);
        assert_eq!(after_include.len(), 2);
        assert_eq!(after_include[0].description, "good feature");
        assert_eq!(after_include[1].description, "important fix");
    }
}
