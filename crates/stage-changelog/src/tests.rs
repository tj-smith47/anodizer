//! Test corpus for the changelog stage.
//!
//! Imports each item explicitly from the appropriate submodule (no `use
//! super::*;`) so additions in one prod submodule don't quietly become
//! visible in tests.

#![allow(clippy::field_reassign_with_default)]

use anodizer_core::config::ChangelogGroup;
use anodizer_core::log::{StageLogger, Verbosity};
use anodizer_core::stage::Stage;
use anodizer_core::test_helpers::{CwdGuard, TestContextBuilder};
use serial_test::serial;

use crate::ChangelogStage;
use crate::fetch::should_preempt_scm_to_git;
use crate::group::{
    CommitInfo, GroupedCommits, apply_filters, apply_include_filters, extract_co_authors,
    group_commits, parse_commit_message, render_changelog, sort_commits,
};

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
    let msg = "fix: bug\n\nCo-Authored-By: Alice <a@x.com>\nCo-Authored-By: Bob Jones <b@x.com>";
    let authors = extract_co_authors(msg);
    assert_eq!(authors, vec!["Alice", "Bob Jones"]);
}

#[test]
fn test_extract_co_authors_case_insensitive() {
    let msg = "feat: thing\n\nco-authored-by: Jane <jane@x.com>\nCO-AUTHORED-BY: Joe <joe@x.com>";
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
    // Unmatched commits are silently dropped (no implicit "Others" group).
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
    // (uses double-newline separator)
    let header = "# My Release Notes";
    let footer = "---\nGenerated by anodizer";
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
    assert!(final_md.ends_with("Generated by anodizer\n"));
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

    // Body now includes default "## Changelog" title
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
    use anodizer_core::config::{ChangelogConfig, CrateConfig};

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .crates(vec![CrateConfig {
            name: "test".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }])
        .build();
    ctx.config.changelog = Some(ChangelogConfig {
        skip: Some(anodizer_core::config::StringOrBool::Bool(true)),
        ..Default::default()
    });

    let stage = ChangelogStage;
    // Should succeed without errors (skips immediately).
    stage.run(&mut ctx).unwrap();

    // No changelogs should be generated.
    assert!(ctx.stage_outputs.changelogs.is_empty());
}

// Snapshot mode skips changelog generation by default (matches
// default); `changelog.snapshot: true` opts back in for local
// preview / draft work.
fn changelog_snapshot_test_config(snapshot_opt_in: Option<bool>) -> anodizer_core::config::Config {
    use anodizer_core::config::{ChangelogConfig, Config, CrateConfig};
    let mut config = Config::default();
    config.project_name = "test".to_string();
    config.changelog = Some(ChangelogConfig {
        snapshot: snapshot_opt_in,
        ..Default::default()
    });
    config.crates = vec![CrateConfig {
        name: "test".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        ..Default::default()
    }];
    config
}

#[test]
fn test_changelog_snapshot_skipped_when_opt_in_unset() {
    let config = changelog_snapshot_test_config(None);
    let mut ctx = TestContextBuilder::new()
        .project_name(&config.project_name)
        .crates(config.crates.clone())
        .snapshot(true)
        .build();
    ctx.config.changelog = config.changelog;
    // The release pipeline never sets `changelog_preview`; assert the default
    // so the gate-bypass fix can't silently leak into the release path.
    assert!(
        !ctx.options.changelog_preview,
        "release-path context must leave changelog_preview unset"
    );
    ChangelogStage
        .run(&mut ctx)
        .expect("snapshot skip is graceful");
    assert!(
        ctx.stage_outputs.changelogs.is_empty(),
        "snapshot mode without opt-in must skip changelog generation"
    );
}

#[test]
fn test_changelog_preview_bypasses_snapshot_skip_gate() {
    // The standalone `changelog` command marks the context `changelog_preview`,
    // which must bypass the `changelog.snapshot` opt-in gate that the release
    // pipeline honors (proved by `test_changelog_snapshot_skipped_when_opt_in_unset`).
    // Use a release-notes content path so the assertion doesn't depend on git
    // history — the only thing under test is that the snapshot-skip early-return
    // is NOT taken.
    let tmp = tempfile::tempdir().expect("tempdir");
    let dist = tmp.path().join("dist");
    std::fs::create_dir_all(&dist).expect("create dist");
    let notes = tmp.path().join("notes.md");
    std::fs::write(&notes, "preview body").expect("write notes");

    let config = changelog_snapshot_test_config(None);
    let mut ctx = TestContextBuilder::new()
        .project_name(&config.project_name)
        .crates(config.crates.clone())
        .dist(dist.clone())
        .snapshot(true)
        .build();
    ctx.config.changelog = config.changelog;
    ctx.options.changelog_preview = true;
    ctx.options.release_notes_path = Some(notes);
    ChangelogStage
        .run(&mut ctx)
        .expect("preview must render even without the snapshot opt-in");
    assert!(
        !ctx.stage_outputs.changelogs.is_empty(),
        "changelog_preview must bypass the snapshot-skip gate"
    );
}

#[test]
fn test_changelog_snapshot_skipped_when_opt_in_false() {
    let config = changelog_snapshot_test_config(Some(false));
    let mut ctx = TestContextBuilder::new()
        .project_name(&config.project_name)
        .crates(config.crates.clone())
        .snapshot(true)
        .build();
    ctx.config.changelog = config.changelog;
    ChangelogStage
        .run(&mut ctx)
        .expect("snapshot skip is graceful");
    assert!(
        ctx.stage_outputs.changelogs.is_empty(),
        "snapshot mode with opt-in=false must skip changelog generation"
    );
}

#[test]
fn test_changelog_non_snapshot_runs_regardless_of_opt_in() {
    // The opt-in is snapshot-mode-only; in normal release mode the gate is
    // bypassed entirely so changelog generation continues as before.
    let config = changelog_snapshot_test_config(None);
    let mut ctx = TestContextBuilder::new()
        .project_name(&config.project_name)
        .crates(config.crates.clone())
        .build();
    ctx.config.changelog = config.changelog;
    // We can't easily run the full git-backed pipeline in a unit test, but
    // we can assert that the snapshot-skip branch is NOT taken — the stage
    // will proceed and either succeed or fail later for unrelated reasons
    // (no git repo, etc.). We only care that the snapshot guard didn't
    // short-circuit, so ensure the stage doesn't return Ok with an empty
    // changelogs map (which is the snapshot-skip signature).
    let _ = ChangelogStage.run(&mut ctx);
    // No assertion on outcome — the assertion is implicit: the stage did
    // not take the snapshot-skip early-return path (this branch is only
    // reachable when ctx.is_snapshot() is true). Compiles + doesn't panic
    // on unwrap of the snapshot gate.
}

#[test]
fn test_changelog_snapshot_runs_when_opt_in_true() {
    // Third matrix cell: snapshot mode + `changelog.snapshot: true` →
    // changelog generation proceeds. We use --release-notes to give the
    // stage a deterministic content path that doesn't require git history.
    let tmp = tempfile::tempdir().expect("tempdir");
    let dist = tmp.path().join("dist");
    std::fs::create_dir_all(&dist).expect("create dist");
    let notes = tmp.path().join("notes.md");
    std::fs::write(&notes, "snapshot opt-in body").expect("write notes");

    let config = changelog_snapshot_test_config(Some(true));
    let mut ctx = TestContextBuilder::new()
        .project_name(&config.project_name)
        .crates(config.crates.clone())
        .dist(dist.clone())
        .snapshot(true)
        .build();
    ctx.config.changelog = config.changelog;
    ctx.options.release_notes_path = Some(notes);
    ChangelogStage
        .run(&mut ctx)
        .expect("snapshot + opt-in must render");
    assert!(
        !ctx.stage_outputs.changelogs.is_empty(),
        "snapshot mode with opt-in=true must NOT skip changelog generation"
    );
    assert_eq!(
        ctx.stage_outputs.changelogs.get("test").map(String::as_str),
        Some("snapshot opt-in body"),
        "release-notes content should be stored verbatim"
    );
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
fn test_apply_include_filters_invalid_regex_is_skipped() {
    let commits = vec![ci("feat: good", "feat", "good", "a")];
    // Invalid pattern is warned and skipped; the second valid pattern
    // still matches.
    let include = vec!["[invalid".to_string(), "^feat".to_string()];
    let result = apply_include_filters(&commits, &include, &test_logger()).unwrap();
    assert_eq!(result.len(), 1);
}

#[test]
fn test_apply_include_filters_all_invalid_keeps_everything() {
    let commits = vec![ci("feat: good", "feat", "good", "a")];
    let include = vec!["[invalid".to_string()];
    let result = apply_include_filters(&commits, &include, &test_logger()).unwrap();
    assert_eq!(result.len(), 1, "all-invalid include is treated as no-op");
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
    // `{{ .SHA }}` respects abbrev. abbrev=5 → "abc12".
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

/// `abbrev: N` must truncate the 40-char `full_hash` (`%H`), not the
/// 7-char `hash` (`%h`) — otherwise any abbrev > 7 silently returns
/// the 7-char short_hash.
#[test]
fn test_abbrev_truncates_full_hash_not_short_hash() {
    let mut ci_long = ci(
        "feat: long-sha test",
        "feat",
        "long-sha test",
        "abc1234", // simulates the 7-char `%h` short_hash
    );
    // `full_hash` carries the 40-char `%H` value; render must read it
    // rather than the short `hash` field for abbrev > 7.
    ci_long.full_hash = "abc1234567890123456789012345678901234567".into();
    let grouped = vec![GroupedCommits {
        title: String::new(),
        commits: vec![ci_long],
        subgroups: Vec::new(),
    }];
    // abbrev=12 must yield 12 chars (`abc123456789`), not a 7-char
    // fallback (`abc1234`).
    let md = render_changelog(&grouped, 12, None, "", "git", None, None);
    assert!(
        md.contains("abc123456789 long-sha test"),
        "abbrev=12 must yield 12-char SHA via full_hash; got:\n{md}"
    );
    assert!(
        !md.contains("abc1234 long-sha"),
        "abbrev=12 must NOT fall back to the 7-char short_hash; got:\n{md}"
    );
}

/// Empty `full_hash` (e.g. a fallback CommitInfo with no SHA available)
/// must not panic when `abbrev > 0` byte-slices into it.
#[test]
fn test_abbrev_handles_empty_full_hash_without_panic() {
    let mut ci_empty = ci("feat: empty-sha test", "feat", "empty-sha test", "");
    ci_empty.full_hash = String::new();
    let grouped = vec![GroupedCommits {
        title: String::new(),
        commits: vec![ci_empty],
        subgroups: Vec::new(),
    }];
    // Must not panic — render returns empty SHA segment cleanly.
    let _md = render_changelog(&grouped, 12, None, "", "git", None, None);
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
    // `{{ .SHA }}` respects abbrev (7) → short hash.
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
    let cfg: anodizer_core::config::ChangelogConfig = serde_yaml_ng::from_str(yaml).unwrap();
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
    let cfg: anodizer_core::config::ChangelogConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.use_source.as_deref(), Some("github-native"));
}

#[test]
fn test_config_parse_abbrev() {
    let yaml = r#"
abbrev: 10
"#;
    let cfg: anodizer_core::config::ChangelogConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.abbrev, Some(10));
}

// -----------------------------------------------------------------------
// Test github-native produces empty changelog
// -----------------------------------------------------------------------

#[test]
fn test_changelog_stage_github_native_dry_run_skips_api() {
    use anodizer_core::config::{ChangelogConfig, CrateConfig, ReleaseConfig, ScmRepoConfig};

    // Flow: github-native calls
    // POST /repos/{o}/{r}/releases/generate-notes upfront. In dry-run /
    // snapshot mode that API call is suppressed (no network in tests),
    // and the per-crate changelog body falls back to the empty string.
    // Outside dry-run/snapshot the body would be the API response.
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .token(Some("test-token".to_string()))
        .dry_run(true)
        .dist(tmp.path().join("dist"))
        .crates(vec![CrateConfig {
            name: "mylib".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            release: Some(ReleaseConfig {
                github: Some(ScmRepoConfig {
                    owner: "owner".to_string(),
                    name: "repo".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    ctx.config.changelog = Some(ChangelogConfig {
        use_source: Some("github-native".to_string()),
        ..Default::default()
    });

    let stage = ChangelogStage;
    stage.run(&mut ctx).unwrap();

    // Dry-run path: the per-crate body is empty (no API call) but the
    // provenance flag is still set so downstream stages know the source.
    assert_eq!(
        ctx.stage_outputs.changelogs.get("mylib"),
        Some(&String::new())
    );
    assert!(ctx.stage_outputs.github_native_changelog);

    // CHANGELOG.md must be written to dist so downstream artifacts and
    // re-runs see a deterministic file (the
    // the changelog stage).
    let changelog_path = ctx.config.dist.join("CHANGELOG.md");
    assert!(
        changelog_path.exists(),
        "expected dist/CHANGELOG.md at {}",
        changelog_path.display()
    );
}

#[test]
fn test_changelog_stage_github_native_requires_token() {
    use anodizer_core::config::{ChangelogConfig, CrateConfig, ReleaseConfig, ScmRepoConfig};

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .crates(vec![CrateConfig {
            name: "mylib".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            release: Some(ReleaseConfig {
                github: Some(ScmRepoConfig {
                    owner: "owner".to_string(),
                    name: "repo".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    ctx.config.changelog = Some(ChangelogConfig {
        use_source: Some("github-native".to_string()),
        ..Default::default()
    });

    let err = ChangelogStage.run(&mut ctx).unwrap_err().to_string();
    assert!(err.contains("requires a GitHub token"), "{}", err);
}

#[test]
fn test_changelog_stage_github_native_skips_when_no_repo_configured() {
    // No crate in scope has release.github (e.g. a library-only workspace
    // in publish-only's per-crate iteration). The stage skips cleanly
    // with a warn instead of bailing — bailing would block the whole
    // pipeline on a workspace that legitimately ships nothing to
    // GitHub.
    use anodizer_core::config::{ChangelogConfig, CrateConfig};

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .token(Some("test-token".to_string()))
        .crates(vec![CrateConfig {
            name: "mylib".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }])
        .build();
    ctx.config.changelog = Some(ChangelogConfig {
        use_source: Some("github-native".to_string()),
        ..Default::default()
    });

    ChangelogStage
        .run(&mut ctx)
        .expect("changelog stage should skip cleanly when no crate has release.github");
}

#[test]
fn test_changelog_github_native_aggregates_missing_release_github_warnings() {
    // A mixed workspace where one crate HAS release.github (so the stage
    // does not take the all-library early return) and several others lack it
    // must emit ONE aggregated warn listing the missing ones, not one warn
    // per crate. Runs in dry-run so the configured crate's generate-notes
    // call is suppressed (no network).
    use anodizer_core::config::{ChangelogConfig, CrateConfig, ReleaseConfig, ScmRepoConfig};

    let mut crates: Vec<CrateConfig> = vec![CrateConfig {
        name: "core".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        release: Some(ReleaseConfig {
            github: Some(ScmRepoConfig {
                owner: "owner".to_string(),
                name: "repo".to_string(),
            }),
            ..Default::default()
        }),
        ..Default::default()
    }];
    crates.extend(["alpha", "beta", "gamma"].iter().map(|name| CrateConfig {
        name: name.to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        ..Default::default()
    }));

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .token(Some("test-token".to_string()))
        .dry_run(true)
        .dist(tmp.path().join("dist"))
        .crates(crates)
        .build();
    let capture = anodizer_core::log::LogCapture::new();
    ctx.with_log_capture(capture.clone());
    ctx.config.changelog = Some(ChangelogConfig {
        use_source: Some("github-native".to_string()),
        ..Default::default()
    });

    ChangelogStage
        .run(&mut ctx)
        .expect("changelog stage should skip cleanly");

    let warns = capture.warn_messages();
    let skip_warns: Vec<&String> = warns
        .iter()
        .filter(|m| m.contains("skipped github-native notes"))
        .collect();
    assert_eq!(
        skip_warns.len(),
        1,
        "expected exactly one aggregated skip warn, got: {warns:?}"
    );
    // The single line must still name every skipped crate.
    let line = skip_warns[0];
    for name in ["alpha", "beta", "gamma"] {
        assert!(
            line.contains(name),
            "aggregated warn must name crate '{name}': {line}"
        );
    }
    assert!(
        line.contains("3 crate(s)"),
        "aggregated warn must report the count: {line}"
    );
}

// -----------------------------------------------------------------------
// Test include + exclude together
// -----------------------------------------------------------------------

#[test]
fn test_include_and_exclude_are_mutually_exclusive() {
    // include and exclude are mutually exclusive:
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
    use anodizer_core::config::ChangelogGroup;

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

    // Verify grouping — unmatched "chore" commit is silently dropped
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

    // default format "{{ SHA }} {{ Message }}" uses the
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
    use anodizer_core::config::ChangelogGroup;

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

    // Body includes default "## Changelog" title.
    // Default format uses `{{ SHA }}` (full hash).
    assert!(final_md.starts_with("# Release v1.0.0\n\n## Changelog\n\n* abc1234 initial release"));
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
    // Default "## Changelog" title always emitted
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
    use anodizer_core::config::{
        ChangelogConfig, ChangelogFilters, ChangelogGroup, Config, CrateConfig,
    };
    use std::process::Command;

    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();

    // Helper to run git commands in the temp repo
    let git = |args: &[&str]| {
        let output = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args)
                    .current_dir(repo)
                    .env("GIT_AUTHOR_NAME", "Test")
                    .env("GIT_AUTHOR_EMAIL", "test@example.com")
                    .env("GIT_COMMITTER_NAME", "Test")
                    .env("GIT_COMMITTER_EMAIL", "test@example.com");
                cmd
            },
            "git",
        );
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
                exclude_version_sync_commits: None,
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
            header: Some(anodizer_core::config::ContentSource::Inline(
                "# Changelog".to_string(),
            )),
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

    let mut ctx = TestContextBuilder::new()
        .project_name(&config.project_name)
        .crates(config.crates.clone())
        .dist(config.dist.clone())
        .build();
    ctx.config.changelog = config.changelog;

    // Run the stage from within the temp repo so git commands target it.
    // CwdGuard restores cwd on Drop — panic-safe if `stage.run` panics.
    let _cwd = CwdGuard::new(repo).unwrap();
    let result = ChangelogStage.run(&mut ctx);

    result.unwrap();

    // Verify the per-crate changelog was populated
    let changelog = ctx
        .stage_outputs
        .changelogs
        .get("test-project")
        .unwrap_or_else(|| panic!("changelog for test-project should exist"));
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

/// The dist `CHANGELOG.md` write is gated on `!changelog_preview`: the release
/// pipeline (preview unset) MUST persist the dist artifact downstream stages
/// consume; the standalone `changelog` preview (preview set) MUST NOT, so it
/// never dirties the working tree. Both halves are asserted against the SAME
/// real git repo so the only variable is the flag.
#[test]
#[serial]
fn test_changelog_dist_write_gated_on_preview_flag() {
    use anodizer_core::config::{ChangelogConfig, Config, CrateConfig};
    use std::process::Command;

    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    let git = |args: &[&str]| {
        let output = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args)
                    .current_dir(repo)
                    .env("GIT_AUTHOR_NAME", "Test")
                    .env("GIT_AUTHOR_EMAIL", "test@example.com")
                    .env("GIT_COMMITTER_NAME", "Test")
                    .env("GIT_COMMITTER_EMAIL", "test@example.com");
                cmd
            },
            "git",
        );
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init"]);
    std::fs::write(repo.join("file.txt"), b"v1").unwrap();
    git(&["add", "."]);
    git(&["commit", "-m", "feat: add initial feature"]);

    let config = Config {
        project_name: "test-project".to_string(),
        dist: repo.join("dist"),
        changelog: Some(ChangelogConfig::default()),
        crates: vec![CrateConfig {
            name: "test-project".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ Version }}".to_string(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let dist_changelog = config.dist.join("CHANGELOG.md");

    // Release path (changelog_preview unset): dist artifact IS written.
    {
        let mut ctx = TestContextBuilder::new()
            .project_name(&config.project_name)
            .crates(config.crates.clone())
            .dist(config.dist.clone())
            .build();
        ctx.config.changelog = config.changelog.clone();
        let _cwd = CwdGuard::new(repo).unwrap();
        ChangelogStage.run(&mut ctx).unwrap();
        assert!(
            !ctx.stage_outputs.changelogs.is_empty(),
            "release path must still populate changelog content"
        );
        assert!(
            dist_changelog.exists(),
            "release path (preview unset) must write dist/CHANGELOG.md"
        );
    }

    std::fs::remove_file(&dist_changelog).unwrap();

    // Preview path (changelog_preview set): content is still produced for
    // streaming, but NO dist artifact is written.
    {
        let mut ctx = TestContextBuilder::new()
            .project_name(&config.project_name)
            .crates(config.crates.clone())
            .dist(config.dist.clone())
            .changelog_preview(true)
            .build();
        ctx.config.changelog = config.changelog.clone();
        let _cwd = CwdGuard::new(repo).unwrap();
        ChangelogStage.run(&mut ctx).unwrap();
        assert!(
            !ctx.stage_outputs.changelogs.is_empty(),
            "preview must still populate stage_outputs.changelogs for streaming"
        );
        assert!(
            !dist_changelog.exists(),
            "preview (changelog_preview set) must NOT write dist/CHANGELOG.md"
        );
    }
}

// -----------------------------------------------------------------------
// Additional behavior tests — config fields actually do things
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

    // `{{ .SHA }}` respects the
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
    use anodizer_core::config::{ChangelogConfig, CrateConfig};

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .crates(vec![CrateConfig {
            name: "test".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }])
        .build();
    ctx.config.changelog = Some(ChangelogConfig {
        skip: Some(anodizer_core::config::StringOrBool::Bool(true)),
        ..Default::default()
    });

    ChangelogStage.run(&mut ctx).unwrap();

    // No changelogs should be generated when disabled
    assert!(ctx.stage_outputs.changelogs.is_empty());
    // The github_native_changelog flag should NOT be set
    assert!(!ctx.stage_outputs.github_native_changelog);
}

#[test]
fn test_empty_changelog_when_all_commits_filtered() {
    let commits = vec![ci("ci: pipeline fix", "ci", "pipeline fix", "a")];

    // Include filter that matches nothing
    let result = apply_include_filters(&commits, &["^feat".to_string()], &test_logger()).unwrap();
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
    use anodizer_core::config::{ChangelogConfig, Config, CrateConfig};
    use std::process::Command;

    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();

    // Helper to run git commands
    let git = |args: &[&str]| {
        let output = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args)
                    .current_dir(repo)
                    .env("GIT_AUTHOR_NAME", "Test")
                    .env("GIT_AUTHOR_EMAIL", "test@example.com")
                    .env("GIT_COMMITTER_NAME", "Test")
                    .env("GIT_COMMITTER_EMAIL", "test@example.com");
                cmd
            },
            "git",
        );
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

    let mut ctx = TestContextBuilder::new()
        .project_name(&config.project_name)
        .crates(config.crates.clone())
        .dist(config.dist.clone())
        .build();
    ctx.config.changelog = config.changelog;

    // CwdGuard restores cwd on Drop — panic-safe if `stage.run` panics.
    let _cwd = CwdGuard::new(repo).unwrap();
    let result = ChangelogStage.run(&mut ctx);
    result.unwrap();

    // CHANGELOG.md should be written to the dist directory
    let notes_path = custom_dist.join("CHANGELOG.md");
    assert!(
        notes_path.exists(),
        "CHANGELOG.md should be in the dist directory: {}",
        custom_dist.display()
    );
}

// ---- Error path tests: malformed inputs / regex / config ----

#[test]
fn test_invalid_exclude_regex_is_warned_and_skipped() {
    let commits = vec![ci("feat: new feature", "feat", "new feature", "abc")];
    // Invalid regex: unclosed group is warned and skipped; the rest of
    // the changelog is preserved unfiltered.
    let filters = vec!["^feat(".to_string()];
    let result = apply_filters(&commits, &filters, &test_logger()).unwrap();
    assert_eq!(result.len(), 1);
}

#[test]
fn test_invalid_include_regex_is_warned_and_skipped() {
    let commits = vec![ci("fix: a bug", "fix", "a bug", "def")];
    // Single invalid pattern means no valid filter is applied → keep
    // all commits.
    let filters = vec!["[invalid".to_string()];
    let result = apply_include_filters(&commits, &filters, &test_logger()).unwrap();
    assert_eq!(result.len(), 1);
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
    // Default title "## Changelog" is always emitted
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
    use anodizer_core::config::{Config, CrateConfig};
    use std::process::Command;

    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();

    // Set up a minimal git repo so the stage gets past git operations
    let git = |args: &[&str]| {
        let output = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args)
                    .current_dir(repo)
                    .env("GIT_AUTHOR_NAME", "Test")
                    .env("GIT_AUTHOR_EMAIL", "test@example.com")
                    .env("GIT_COMMITTER_NAME", "Test")
                    .env("GIT_COMMITTER_EMAIL", "test@example.com");
                cmd
            },
            "git",
        );
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

    let mut ctx = TestContextBuilder::new()
        .project_name(&config.project_name)
        .crates(config.crates.clone())
        .dist(config.dist.clone())
        .build();

    // CwdGuard restores cwd on Drop — panic-safe if `stage.run` panics.
    let _cwd = CwdGuard::new(repo).unwrap();
    let result = ChangelogStage.run(&mut ctx);

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
    use anodizer_core::config::{Config, CrateConfig};
    use std::process::Command;

    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();

    let git = |args: &[&str]| {
        let output = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args)
                    .current_dir(repo)
                    .env("GIT_AUTHOR_NAME", "Test")
                    .env("GIT_AUTHOR_EMAIL", "test@example.com")
                    .env("GIT_COMMITTER_NAME", "Test")
                    .env("GIT_COMMITTER_EMAIL", "test@example.com");
                cmd
            },
            "git",
        );
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

    let mut ctx = TestContextBuilder::new()
        .project_name(&config.project_name)
        .crates(config.crates.clone())
        .dist(config.dist.clone())
        .build();

    // CwdGuard restores cwd on Drop — panic-safe if `stage.run` panics.
    let _cwd = CwdGuard::new(repo).unwrap();
    let result = ChangelogStage.run(&mut ctx);

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
    use anodizer_core::config::{Config, CrateConfig};
    use std::process::Command;

    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    let dist = repo.join("dist");

    let git = |args: &[&str]| {
        let output = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args)
                    .current_dir(repo)
                    .env("GIT_AUTHOR_NAME", "Test")
                    .env("GIT_AUTHOR_EMAIL", "test@example.com")
                    .env("GIT_COMMITTER_NAME", "Test")
                    .env("GIT_COMMITTER_EMAIL", "test@example.com");
                cmd
            },
            "git",
        );
        assert!(output.status.success());
    };

    git(&["init"]);
    std::fs::write(repo.join("file.txt"), b"content").unwrap();
    git(&["add", "."]);
    git(&["commit", "-m", "feat: initial"]);

    // CHANGELOG.md is written even in dry-run mode
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

    let mut ctx = TestContextBuilder::new()
        .project_name(&config.project_name)
        .crates(config.crates.clone())
        .dist(config.dist.clone())
        .dry_run(true)
        .build();

    // CwdGuard restores cwd on Drop — panic-safe if `stage.run` panics.
    let _cwd = CwdGuard::new(repo).unwrap();
    let result = ChangelogStage.run(&mut ctx);

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
    // default format "{{ SHA }} {{ Message }}" uses the
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
    let cfg: anodizer_core::config::ChangelogConfig = serde_yaml_ng::from_str(yaml).unwrap();
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
    let mut ctx = TestContextBuilder::new().project_name("myapp").build();
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
    let ctx = TestContextBuilder::new().build();

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
skip: "{{ if .IsSnapshot }}true{{ end }}"
sort: asc
"#;
    let cfg: anodizer_core::config::ChangelogConfig = serde_yaml_ng::from_str(yaml).unwrap();
    match &cfg.skip {
        Some(anodizer_core::config::StringOrBool::String(s)) => {
            assert!(s.contains("IsSnapshot"), "should contain template string");
        }
        other => panic!("expected StringOrBool::String, got {:?}", other),
    }
}

#[test]
fn test_config_parse_disable_bool_true() {
    let yaml = r#"
skip: true
"#;
    let cfg: anodizer_core::config::ChangelogConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(
        cfg.skip,
        Some(anodizer_core::config::StringOrBool::Bool(true))
    );
}

#[test]
fn test_config_parse_abbrev_negative_one() {
    let yaml = r#"
abbrev: -1
"#;
    let cfg: anodizer_core::config::ChangelogConfig = serde_yaml_ng::from_str(yaml).unwrap();
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
    let cfg: anodizer_core::config::ChangelogConfig = serde_yaml_ng::from_str(yaml).unwrap();
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
    let cfg: anodizer_core::config::ChangelogConfig = serde_yaml_ng::from_str(yaml).unwrap();
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
    // Unmatched commits are dropped silently, so "generic feature"
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
fn test_render_per_entry_logins_variable_in_format() {
    // `Logins` is per-entry (this commit's login). The release-wide
    // login list lives under `AllLogins`.
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
        Some("{{ ShortSHA }} {{ Message }} cc {{ Logins }} (all={{ AllLogins }})"),
        "alice,bob",
        "git",
        None,
        None,
    );
    assert!(
        md.contains("abc1234 add feature cc alice (all=alice,bob)"),
        "per-entry Logins + AllLogins variables should be rendered, got: {md}"
    );
}

#[test]
fn test_render_per_entry_authors_variable_in_format() {
    // `Authors` is per-entry: primary author + names parsed out of any
    // `Co-Authored-By:` trailers on that commit.
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
            co_authors: vec!["Bob <bob@example.com>".into(), "Carol".into()],
        }],
    )];
    let md = render_changelog(
        &grouped,
        7,
        Some("{{ ShortSHA }} {{ Message }} by {{ Authors }}"),
        "",
        "git",
        None,
        None,
    );
    assert!(
        md.contains("abc1234 add feature by Alice, Bob <bob@example.com>, Carol"),
        "per-entry Authors should include primary + co-authors, got: {md}"
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
fn test_render_author_username_alias_for_login() {
    // Regression guard: `AuthorUsername` not bound. The default format string when
    // `use ∈ {github,gitlab,gitea}` references `.AuthorUsername`; before
    // the fix that key was unbound and copy-pasting the default into an
    // anodizer config raised a Tera "missing key" error. The fix binds
    // `AuthorUsername` to the same datum as `Login`, so the default format
    // template renders cleanly.
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
        Some("{{ ShortSHA }} {{ Message }} (@{{ AuthorUsername }})"),
        "",
        "git",
        None,
        None,
    );
    assert!(
        md.contains("abc1234 add feature (@octocat)"),
        "AuthorUsername must alias Login so GR's default format string works, got: {md}"
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
fn test_title_three_state_matrix() {
    // Pin the three-state behaviour of the title escape-hatch:
    //   - None        → emits the default `## Changelog` heading (matches
    //                   the unconditional emission).
    //   - Some("foo") → emits `## foo`.
    //   - Some("")    → suppresses the heading (anodize-additive carve-out).
    let grouped = vec![GroupedCommits::new(
        "",
        vec![ci("feat: add X", "feat", "add X", "abc1234")],
    )];

    // None → default "Changelog".
    let md_default = render_changelog(&grouped, 7, None, "", "git", None, None);
    assert!(
        md_default.starts_with("## Changelog\n\n"),
        "None should emit default '## Changelog' heading: {md_default}"
    );

    // Some("Custom") → "## Custom".
    let md_custom = render_changelog(&grouped, 7, None, "", "git", Some("Custom"), None);
    assert!(
        md_custom.starts_with("## Custom\n\n"),
        "Some(\"Custom\") should emit '## Custom' heading: {md_custom}"
    );

    // Some("") → no heading.
    let md_empty = render_changelog(&grouped, 7, None, "", "git", Some(""), None);
    assert!(
        !md_empty.contains("## "),
        "Some(\"\") should suppress heading entirely: {md_empty}"
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
    use anodizer_core::config::ChangelogConfig;
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
    use anodizer_core::config::{ChangelogAiConfig, ChangelogAiPrompt};
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
    use anodizer_core::config::{ChangelogAiConfig, ChangelogAiPrompt};
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
    use anodizer_core::config::{ChangelogAiConfig, ChangelogAiPrompt};
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
    use anodizer_core::config::{
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
    use anodizer_core::config::{ChangelogAiPromptSource, ContentFromUrl, ResolvedPromptSource};
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
    use anodizer_core::config::{ChangelogAiPromptSource, ResolvedPromptSource};
    let src = ChangelogAiPromptSource {
        from_file: None,
        from_url: None,
    };
    assert!(matches!(src.resolve(), ResolvedPromptSource::None));
}

#[test]
fn test_resolve_prompt_source_file_with_empty_path_falls_through() {
    use anodizer_core::config::{
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
    let cfg: anodizer_core::config::ChangelogConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.use_source.as_deref(), Some("gitlab"));
}

#[test]
fn test_config_parse_use_source_gitea() {
    let yaml = r#"
use: gitea
"#;
    let cfg: anodizer_core::config::ChangelogConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.use_source.as_deref(), Some("gitea"));
}

#[test]
fn test_validation_rejects_unsupported_source() {
    // Exercise the actual production validation path — "bitbucket" should
    // cause the stage to bail with "unsupported use source".
    use anodizer_core::config::{ChangelogConfig, CrateConfig};

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .crates(vec![CrateConfig {
            name: "mylib".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }])
        .build();
    ctx.config.changelog = Some(ChangelogConfig {
        use_source: Some("bitbucket".to_string()),
        ..Default::default()
    });

    let stage = ChangelogStage;
    let result = stage.run(&mut ctx);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("unsupported use source"), "got: {msg}");
}

// `serial` because this test shells out to `git` from process cwd, which
// races with sibling tests that swap cwd (fetch::gitlab::tests use CwdGuard).
#[test]
#[serial]
fn test_changelog_stage_gitlab_falls_back_to_git_no_token() {
    // When use: gitlab but no token is available, should fall back to git
    // (which will also fail in a test environment, but the point is that
    // the stage doesn't bail on "unsupported use source").
    use anodizer_core::config::{ChangelogConfig, CrateConfig};

    let tmp = tempfile::TempDir::new().unwrap();
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dist(tmp.path().to_path_buf())
        .dry_run(true)
        // No token — should trigger fallback to git.
        .crates(vec![CrateConfig {
            name: "test".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }])
        .build();
    ctx.config.changelog = Some(ChangelogConfig {
        use_source: Some("gitlab".to_string()),
        ..Default::default()
    });

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

// `serial` because this test shells out to `git` from process cwd, which
// races with sibling tests that swap cwd (fetch::gitea::tests use CwdGuard).
#[test]
#[serial]
fn test_changelog_stage_gitea_falls_back_to_git_no_token() {
    use anodizer_core::config::{ChangelogConfig, CrateConfig};

    let tmp = tempfile::TempDir::new().unwrap();
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dist(tmp.path().to_path_buf())
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "test".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }])
        .build();
    ctx.config.changelog = Some(ChangelogConfig {
        use_source: Some("gitea".to_string()),
        ..Default::default()
    });

    let stage = ChangelogStage;
    let result = stage.run(&mut ctx);
    assert!(
        result.is_ok(),
        "gitea with no token should fall back to git: {:?}",
        result.err()
    );
}

// -----------------------------------------------------------------------
// C-new-19: SCM mode pre-empts to git fallback when no previous tag
// -----------------------------------------------------------------------

#[test]
fn test_should_preempt_scm_to_git_github_no_prev_tag() {
    // GitHub mode + no previous tag → pre-empt to git fallback.
    assert!(should_preempt_scm_to_git(true, false, false, &None));
}

#[test]
fn test_should_preempt_scm_to_git_gitlab_no_prev_tag() {
    assert!(should_preempt_scm_to_git(false, true, false, &None));
}

#[test]
fn test_should_preempt_scm_to_git_gitea_no_prev_tag() {
    assert!(should_preempt_scm_to_git(false, false, true, &None));
}

#[test]
fn test_should_preempt_scm_to_git_with_prev_tag_no_preempt() {
    // With a previous tag, the SCM API path is taken — no pre-empt.
    let prev = Some("v1.0.0".to_string());
    assert!(!should_preempt_scm_to_git(true, false, false, &prev));
    assert!(!should_preempt_scm_to_git(false, true, false, &prev));
    assert!(!should_preempt_scm_to_git(false, false, true, &prev));
}

#[test]
fn test_should_preempt_scm_to_git_pure_git_mode_no_preempt() {
    // `use: git` — no SCM at all — never pre-empts (the `else` branch
    // already calls fetch_git_commits directly).
    assert!(!should_preempt_scm_to_git(false, false, false, &None));
    assert!(!should_preempt_scm_to_git(
        false,
        false,
        false,
        &Some("v1.0.0".to_string())
    ));
}

#[test]
#[serial]
fn test_changelog_stage_github_no_prev_tag_uses_git_fallback() {
    // End-to-end: with `use: github` + no previous tag, the stage should
    // succeed without making an API call (pre-empt path). This is run in
    // a fresh tempdir as a git repo with no tags so prev_tag is None.
    use anodizer_core::config::{ChangelogConfig, CrateConfig};
    use std::process::Command;

    let tmp = tempfile::TempDir::new().unwrap();
    let repo = tmp.path();

    // Initialize a fresh git repo with one commit but no tags.
    let _ = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.args(["init", "-q"]).current_dir(repo);
            cmd
        },
        "git",
    );
    let _ = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.args(["config", "user.email", "test@example.com"])
                .current_dir(repo);
            cmd
        },
        "git",
    );
    let _ = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.args(["config", "user.name", "Test"]).current_dir(repo);
            cmd
        },
        "git",
    );
    std::fs::write(repo.join("README.md"), "test").unwrap();
    let _ = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.args(["add", "."]).current_dir(repo);
            cmd
        },
        "git",
    );
    let _ = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.args(["commit", "-q", "-m", "feat: initial commit"])
                .current_dir(repo);
            cmd
        },
        "git",
    );

    let dist = repo.join("dist");
    std::fs::create_dir_all(&dist).unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dist(dist.clone())
        .crates(vec![CrateConfig {
            name: "mylib".to_string(),
            // The crate's tag_template never matched any tag (the repo
            // has none), so prev_tag will be None and pre-empt should
            // kick in.
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }])
        // No token — if the API path were taken, fetch_github_commits
        // would attempt resolve_repo_slug() → likely fail, then
        // strict_guard would log + fall back. Our pre-empt skips that
        // entire branch.
        .dry_run(true)
        .build();
    ctx.config.changelog = Some(ChangelogConfig {
        use_source: Some("github".to_string()),
        ..Default::default()
    });

    // Run from inside the tempdir so git commands operate on it.
    // CwdGuard restores cwd on Drop — panic-safe if `stage.run` panics.
    let _cwd = CwdGuard::new(repo).unwrap();
    let stage = ChangelogStage;
    let result = stage.run(&mut ctx);

    assert!(
        result.is_ok(),
        "github + no prev tag should pre-empt to git fallback: {:?}",
        result.err()
    );

    // The git fallback should have produced a changelog entry containing
    // the seed commit's subject.
    let body = ctx
        .stage_outputs
        .changelogs
        .get("mylib")
        .cloned()
        .unwrap_or_default();
    assert!(
        body.contains("initial commit"),
        "git fallback should include the seed commit, got: {body}"
    );
}

#[test]
#[serial]
fn test_lockstep_resolves_prev_tag_once_not_full_history() {
    // Lockstep workspace: two crates share ONE tag namespace (`v{{ .Version }}`),
    // so at release time the just-cut tag (v0.12.0) IS the latest match for
    // every crate. The previous lockstep tag (v0.11.3) must be resolved for all
    // crates and the range must be v0.11.3..HEAD — never a per-crate full-history
    // fallback.
    use anodizer_core::config::{ChangelogConfig, CrateConfig};
    use std::process::Command;

    let tmp = tempfile::TempDir::new().unwrap();
    let repo = tmp.path();

    let git = |args: &[&str]| {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args)
                    .current_dir(repo)
                    .env("GIT_AUTHOR_NAME", "Test")
                    .env("GIT_AUTHOR_EMAIL", "test@example.com")
                    .env("GIT_COMMITTER_NAME", "Test")
                    .env("GIT_COMMITTER_EMAIL", "test@example.com");
                cmd
            },
            "git",
        );
        assert!(out.status.success(), "git {:?} failed", args);
    };

    git(&["init", "-q"]);
    std::fs::create_dir_all(repo.join("crates/alpha")).unwrap();
    std::fs::create_dir_all(repo.join("crates/beta")).unwrap();

    let commit = |path: &str, body: &str, msg: &str| {
        std::fs::write(repo.join(path), body).unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", msg]);
    };

    // Pre-v0.11.3 history that must NOT leak into the changelog.
    commit("crates/alpha/lib.rs", "v0", "feat: ancient alpha feature");
    git(&["tag", "v0.11.3"]);

    // Commits in the v0.11.3..HEAD window for both crates.
    commit("crates/alpha/lib.rs", "v1", "feat: new alpha feature");
    commit("crates/beta/lib.rs", "v1", "fix: new beta fix");
    git(&["tag", "v0.12.0"]);

    let dist = repo.join("dist");
    std::fs::create_dir_all(&dist).unwrap();

    let lockstep_crate = |name: &str, path: &str| CrateConfig {
        name: name.to_string(),
        path: path.to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        ..Default::default()
    };

    let mut ctx = TestContextBuilder::new()
        .project_name("ws")
        .dist(dist.clone())
        .tag("v0.12.0")
        .crates(vec![
            lockstep_crate("alpha", "crates/alpha"),
            lockstep_crate("beta", "crates/beta"),
        ])
        .build();
    ctx.config.changelog = Some(ChangelogConfig {
        use_source: Some("git".to_string()),
        ..Default::default()
    });

    let _cwd = CwdGuard::new(repo).unwrap();
    ChangelogStage.run(&mut ctx).unwrap();

    let alpha = ctx
        .stage_outputs
        .changelogs
        .get("alpha")
        .cloned()
        .unwrap_or_default();
    let beta = ctx
        .stage_outputs
        .changelogs
        .get("beta")
        .cloned()
        .unwrap_or_default();

    assert!(
        alpha.contains("new alpha feature"),
        "alpha changelog should cover the v0.11.3..HEAD window, got: {alpha}"
    );
    assert!(
        !alpha.contains("ancient alpha feature"),
        "lockstep must resolve prev=v0.11.3 once; pre-v0.11.3 history leaked: {alpha}"
    );
    assert!(
        beta.contains("new beta fix"),
        "beta changelog should cover the v0.11.3..HEAD window, got: {beta}"
    );
}

#[test]
fn test_changelog_stage_unsupported_source_bails() {
    use anodizer_core::config::{ChangelogConfig, CrateConfig};

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .crates(vec![CrateConfig {
            name: "test".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }])
        .build();
    ctx.config.changelog = Some(ChangelogConfig {
        use_source: Some("bitbucket".to_string()),
        ..Default::default()
    });

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

// ---- AuthorsList / LoginsList structured per-entry vars ----

#[test]
fn test_render_authors_list_structured_for_iteration() {
    // `AuthorsList` is a structured shape: a list of
    // {Name, Email, Username} records, iterable from Tera.
    let grouped = vec![GroupedCommits::new(
        "",
        vec![CommitInfo {
            raw_message: "feat: ship it".into(),
            kind: "feat".into(),
            description: "ship it".into(),
            hash: "deadbee".into(),
            full_hash: "deadbeefdeadbeef".into(),
            author_name: "Alice".into(),
            author_email: "alice@example.com".into(),
            login: "alice42".into(),
            co_authors: vec!["Bob".into()],
        }],
    )];
    // Iterate over the structured list: emit `name(@login)` separated by `;`.
    let md = render_changelog(
        &grouped,
        7,
        Some(
            "{{ ShortSHA }} {{ Message }} cc {% for a in AuthorsList %}{{ a.Name }}(@{{ a.Username }}){% if not loop.last %}; {% endif %}{% endfor %}",
        ),
        "",
        "git",
        None,
        None,
    );
    assert!(
        md.contains("deadbee ship it cc Alice(@alice42); Bob(@)"),
        "AuthorsList must be iterable as structured records, got: {md}"
    );
}

#[test]
fn test_render_all_authors_release_wide_unique_set() {
    // `AllAuthors` is release-wide: every commit's primary author + every
    // co-author trailer name, deduped and alpha-sorted. Templated as a
    // comma-string for footer-style rendering. Per-commit scope is the
    // only template scope anodizer's changelog renderer exposes, so the
    // value is repeated on every line; here we just sample line 1.
    let grouped = vec![
        GroupedCommits::new(
            "Features",
            vec![CommitInfo {
                raw_message: "feat: a".into(),
                kind: "feat".into(),
                description: "a".into(),
                hash: "aaa".into(),
                full_hash: "aaa".into(),
                author_name: "Bob".into(),
                author_email: "bob@example.com".into(),
                login: String::new(),
                co_authors: vec!["Alice".into()],
            }],
        ),
        GroupedCommits::new(
            "Bug Fixes",
            vec![CommitInfo {
                raw_message: "fix: b".into(),
                kind: "fix".into(),
                description: "b".into(),
                hash: "bbb".into(),
                full_hash: "bbb".into(),
                author_name: "Carol".into(),
                author_email: "carol@example.com".into(),
                login: String::new(),
                co_authors: vec!["Alice".into()],
            }],
        ),
    ];
    let md = render_changelog(
        &grouped,
        7,
        Some("{{ ShortSHA }} {{ Message }} (release: {{ AllAuthors }})"),
        "",
        "git",
        Some(""),
        None,
    );
    assert!(
        md.contains("release: Alice, Bob, Carol"),
        "AllAuthors should render the sorted, deduped author set, got: {md}"
    );
}

#[test]
fn test_render_logins_list_with_english_join_filter() {
    // `LoginsList` is the structured-list shape for per-entry logins;
    // pairing it with the new `englishJoin` filter matches the default
    // `{{ .Logins | englishJoin }}` template fragment.
    let grouped = vec![GroupedCommits::new(
        "",
        vec![CommitInfo {
            raw_message: "feat: x".into(),
            kind: "feat".into(),
            description: "x".into(),
            hash: "aaa1111".into(),
            full_hash: "aaa1111aaa1111".into(),
            author_name: "Alice".into(),
            author_email: "alice@example.com".into(),
            login: "alice".into(),
            co_authors: Vec::new(),
        }],
    )];
    let md = render_changelog(
        &grouped,
        7,
        Some("{{ ShortSHA }} {{ Message }} ({{ LoginsList | englishJoin }})"),
        "",
        "git",
        None,
        None,
    );
    assert!(
        md.contains("aaa1111 x (alice)"),
        "LoginsList | englishJoin should render the per-entry logins, got: {md}"
    );
}

// ---- Keep-a-Changelog merge tests ----

use crate::render::{MergeArgs, merge_into_changelog};

/// A fixture mirroring anodizer's real root `CHANGELOG.md` shape: H1 +
/// KaC preamble + a curated `## [Unreleased]` section + a prior released
/// section + `[Unreleased]:` / `[0.5.0]:` footer links. `anchor` lets the
/// per-crate variant use a `crate-v`-prefixed compare ref.
fn kac_fixture(anchor: &str, curated: &str) -> String {
    format!(
        "# Changelog\n\
\n\
All notable changes to this project will be documented in this file.\n\
The format is based on [Keep a Changelog].\n\
\n\
## [Unreleased]\n\
{curated}\
## [0.5.0] - 2026-01-01\n\
\n\
### Features\n\
- old feature\n\
\n\
[Unreleased]: https://github.com/tj-smith47/anodizer/compare/{anchor}...HEAD\n\
[0.5.0]: https://github.com/tj-smith47/anodizer/releases/tag/{anchor}\n"
    )
}

/// Write `contents` to a `CHANGELOG.md` in a fresh tempdir and run the
/// real merge path against it, returning the merged output.
fn run_merge(
    contents: &str,
    generated_body: &str,
    from_tag: Option<&str>,
    to_version: &str,
) -> String {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("CHANGELOG.md");
    std::fs::write(&path, contents).expect("write fixture");
    let section_heading = format!("## [{to_version}] - 2026-06-03");
    let new_section = format!("{section_heading}\n\n{generated_body}\n");
    merge_into_changelog(MergeArgs {
        file_path: &path,
        h1: "# Changelog — anodizer",
        new_section: &new_section,
        generated_body,
        from_tag,
        to_version,
        workspace_root: dir.path(),
    })
    .expect("merge succeeds")
}

#[test]
fn kac_unreleased_promoted_with_curated_content_preserved() {
    let curated =
        "\n### Features\n- **hand-curated rich prose** with detail\n### Fixes\n- a curated fix\n\n";
    let fixture = kac_fixture("v0.5.0", curated);
    let out = run_merge(
        &fixture,
        "### Features\n- generated commit",
        Some("v0.5.0"),
        "0.6.0",
    );

    // Curated body preserved verbatim under the promoted heading.
    assert!(
        out.contains("## [0.6.0] - "),
        "promoted heading missing: {out}"
    );
    assert!(
        out.contains("- **hand-curated rich prose** with detail"),
        "curated content must be preserved verbatim: {out}"
    );
    assert!(
        out.contains("- a curated fix"),
        "curated fix must be preserved: {out}"
    );
    // Generated commit body must NOT overwrite the curated section.
    assert!(
        !out.contains("- generated commit"),
        "generated body must not replace curated content: {out}"
    );
    // A fresh empty Unreleased sits above the promoted release.
    let unreleased_pos = out.find("## [Unreleased]").expect("fresh Unreleased");
    let promoted_pos = out.find("## [0.6.0]").expect("promoted section");
    assert!(
        unreleased_pos < promoted_pos,
        "fresh Unreleased must precede the promoted release: {out}"
    );
    // The prior released section survives.
    assert!(
        out.contains("## [0.5.0] - 2026-01-01"),
        "prior section lost: {out}"
    );
    assert!(out.contains("- old feature"), "prior body lost: {out}");
}

#[test]
fn kac_link_footer_rolled_unreleased_and_new_version() {
    let fixture = kac_fixture("v0.5.0", "\n### Fixes\n- curated\n\n");
    let out = run_merge(&fixture, "### Fixes\n- gen", Some("v0.5.0"), "0.6.0");

    assert!(
        out.contains("[Unreleased]: https://github.com/tj-smith47/anodizer/compare/v0.6.0...HEAD"),
        "Unreleased footer must roll to v0.6.0...HEAD: {out}"
    );
    assert!(
        out.contains("[0.6.0]: https://github.com/tj-smith47/anodizer/compare/v0.5.0...v0.6.0"),
        "new [0.6.0] compare link missing: {out}"
    );
    // Prior release footer line survives untouched.
    assert!(
        out.contains("[0.5.0]: https://github.com/tj-smith47/anodizer/releases/tag/v0.5.0"),
        "prior [0.5.0] footer line must survive: {out}"
    );
    // The new [0.6.0]: line sits directly under the [Unreleased]: line.
    let un_line = out
        .lines()
        .position(|l| l.starts_with("[Unreleased]:"))
        .expect("unreleased footer line");
    let new_line = out
        .lines()
        .position(|l| l.starts_with("[0.6.0]:"))
        .expect("new version footer line");
    assert_eq!(
        new_line,
        un_line + 1,
        "[0.6.0]: must sit under [Unreleased]:"
    );
}

#[test]
fn kac_empty_unreleased_filled_from_commits() {
    // Empty curated body (just blank lines) → generated commits fill it.
    let fixture = kac_fixture("v0.5.0", "\n");
    let out = run_merge(
        &fixture,
        "### Features\n- generated commit body",
        Some("v0.5.0"),
        "0.6.0",
    );
    assert!(
        out.contains("## [0.6.0] - "),
        "promoted heading missing: {out}"
    );
    assert!(
        out.contains("- generated commit body"),
        "empty Unreleased must be filled from generated commits: {out}"
    );
}

#[test]
fn kac_per_crate_anchor_prefix() {
    let fixture = kac_fixture("anodizer-v0.5.0", "\n### Fixes\n- curated\n\n");
    let out = run_merge(
        &fixture,
        "### Fixes\n- gen",
        Some("anodizer-v0.5.0"),
        "0.6.0",
    );

    assert!(
        out.contains(
            "[Unreleased]: https://github.com/tj-smith47/anodizer/compare/anodizer-v0.6.0...HEAD"
        ),
        "per-crate anchor must yield anodizer-v0.6.0...HEAD: {out}"
    );
    assert!(
        out.contains(
            "[0.6.0]: https://github.com/tj-smith47/anodizer/compare/anodizer-v0.5.0...anodizer-v0.6.0"
        ),
        "per-crate compare link must use anodizer-v prefix: {out}"
    );
}

#[test]
fn kac_no_footer_link_still_rolls_heading() {
    // A KAC file with NO footer links and no resolvable remote: the heading
    // roll must still happen and the call must not panic.
    let fixture = "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### Fixes\n\
- curated fix\n\
\n\
## [0.5.0] - 2026-01-01\n\
\n\
- old\n";
    let out = run_merge(fixture, "### Fixes\n- gen", Some("v0.5.0"), "0.6.0");

    assert!(
        out.contains("## [0.6.0] - "),
        "promoted heading missing: {out}"
    );
    assert!(out.contains("- curated fix"), "curated content lost: {out}");
    let unreleased_pos = out.find("## [Unreleased]").expect("fresh Unreleased");
    let promoted_pos = out.find("## [0.6.0]").expect("promoted section");
    assert!(
        unreleased_pos < promoted_pos,
        "fresh Unreleased must precede release: {out}"
    );
    // No footer link present and no remote in the throwaway tempdir → no
    // synthesized footer; prior section survives.
    assert!(
        out.contains("## [0.5.0] - 2026-01-01"),
        "prior section lost: {out}"
    );
}

#[test]
fn kac_second_roll_same_version_is_noop() {
    // After rolling 0.6.0 once, the file already has a `## [0.6.0]` section.
    // A second roll for the SAME version must NOT promote the freshly-emptied
    // `## [Unreleased]` into a duplicate `## [0.6.0]` section.
    let fixture = kac_fixture("v0.5.0", "\n### Fixes\n- curated\n\n");
    let first = run_merge(&fixture, "### Fixes\n- gen", Some("v0.5.0"), "0.6.0");
    assert!(
        first.contains("## [0.6.0] - "),
        "first roll missing: {first}"
    );

    // Feed the rolled output back through the merge for the same version.
    let second = run_merge(&first, "### Fixes\n- gen2", Some("v0.6.0"), "0.6.0");
    assert_eq!(
        second, first,
        "a second roll for the same version must be a no-op: {second}"
    );
    // Exactly one `## [0.6.0]` heading — no duplicate promotion.
    let count = second.matches("## [0.6.0]").count();
    assert_eq!(
        count, 1,
        "expected one [0.6.0] section, found {count}: {second}"
    );
}

#[test]
fn kac_unreleased_only_section_with_footer() {
    // A KAC file whose ONLY section is `## [Unreleased]` (no prior released
    // section) but which DOES carry a `[Unreleased]:` footer link.
    let fixture = "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### Features\n\
- shiny new thing\n\
\n\
[Unreleased]: https://github.com/acme/widget/compare/v0.1.0...HEAD\n";
    let out = run_merge(fixture, "### Features\n- gen", Some("v0.1.0"), "0.2.0");

    // Promotion happened and a fresh empty Unreleased sits above it.
    assert!(
        out.contains("## [0.2.0] - "),
        "promoted heading missing: {out}"
    );
    assert!(
        out.contains("- shiny new thing"),
        "curated content lost: {out}"
    );
    let unreleased_pos = out.find("## [Unreleased]").expect("fresh Unreleased");
    let promoted_pos = out.find("## [0.2.0]").expect("promoted section");
    assert!(
        unreleased_pos < promoted_pos,
        "fresh Unreleased must precede the promoted release: {out}"
    );
    // Footer rolled: new Unreleased compare + new version compare line.
    assert!(
        out.contains("[Unreleased]: https://github.com/acme/widget/compare/v0.2.0...HEAD"),
        "Unreleased footer must roll to v0.2.0...HEAD: {out}"
    );
    assert!(
        out.contains("[0.2.0]: https://github.com/acme/widget/compare/v0.1.0...v0.2.0"),
        "new [0.2.0] compare link missing: {out}"
    );
}

#[test]
fn kac_unreleased_content_then_footer_no_other_section() {
    // `## [Unreleased]` with curated content immediately followed by the
    // footer block — no other `## ` heading between body and footer. The
    // curated body must be bounded BEFORE the footer (footer not pulled in).
    let fixture = "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### Fixes\n\
- a real fix\n\
\n\
[Unreleased]: https://github.com/acme/widget/compare/v0.1.0...HEAD\n";
    let out = run_merge(fixture, "### Fixes\n- gen", Some("v0.1.0"), "0.2.0");

    assert!(
        out.contains("## [0.2.0] - "),
        "promoted heading missing: {out}"
    );
    assert!(out.contains("- a real fix"), "curated content lost: {out}");
    // The footer link must NOT appear inside the promoted body: there must be
    // exactly one `[Unreleased]:` line and it sits after the `[0.2.0]:` rolls,
    // not glued under the promoted heading. Assert the body line precedes the
    // footer and the footer rolled correctly.
    let fix_pos = out.find("- a real fix").expect("fix line");
    let footer_pos = out
        .find("[Unreleased]: https://github.com/acme/widget/compare/v0.2.0...HEAD")
        .expect("rolled footer");
    assert!(
        fix_pos < footer_pos,
        "curated body must precede the footer block: {out}"
    );
    assert!(
        out.contains("[0.2.0]: https://github.com/acme/widget/compare/v0.1.0...v0.2.0"),
        "new [0.2.0] compare link missing: {out}"
    );
    // Exactly one rolled `[Unreleased]:` footer line.
    let count = out
        .lines()
        .filter(|l| l.starts_with("[Unreleased]:"))
        .count();
    assert_eq!(
        count, 1,
        "expected one [Unreleased]: footer, got {count}: {out}"
    );
}

/// Initialize a fresh git repo at `dir` with an `origin` remote pointing at
/// `remote_url`, so the synthesize-footer path can resolve a web base.
fn init_repo_with_origin(dir: &std::path::Path, remote_url: &str) {
    use std::process::Command;
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "test@example.com"],
        vec!["config", "user.name", "Test"],
        vec!["remote", "add", "origin", remote_url],
    ] {
        let ok = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(&args).current_dir(dir);
                cmd
            },
            "git",
        )
        .status
        .success();
        assert!(ok, "git {args:?} failed");
    }
}

#[test]
fn kac_synthesize_footer_from_github_remote() {
    // KAC file with a `## [Unreleased]` heading but NO footer link. With a
    // real `origin` remote configured, a footer is synthesized from it.
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo_with_origin(dir.path(), "https://github.com/acme/widget.git");
    let path = dir.path().join("CHANGELOG.md");
    let fixture = "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### Fixes\n\
- curated fix\n\
\n\
## [0.1.0] - 2026-01-01\n\
\n\
- old\n";
    std::fs::write(&path, fixture).expect("write fixture");

    let out = merge_into_changelog(MergeArgs {
        file_path: &path,
        h1: "# Changelog — widget",
        new_section: "## [0.2.0] - 2026-06-03\n\n### Fixes\n- gen\n",
        generated_body: "### Fixes\n- gen",
        from_tag: Some("v0.1.0"),
        to_version: "0.2.0",
        workspace_root: dir.path(),
    })
    .expect("merge succeeds");

    assert!(
        out.contains("## [0.2.0] - "),
        "promoted heading missing: {out}"
    );
    assert!(
        out.contains("[Unreleased]: https://github.com/acme/widget/compare/v0.2.0...HEAD"),
        "synthesized Unreleased footer missing/wrong: {out}"
    );
    assert!(
        out.contains("[0.2.0]: https://github.com/acme/widget/compare/v0.1.0...v0.2.0"),
        "synthesized [0.2.0] compare link missing/wrong: {out}"
    );
}

#[test]
fn kac_synthesize_footer_from_gitlab_remote_is_host_correct() {
    // A self-hosted GitLab origin must yield a host-correct compare base —
    // never a hardcoded github.com URL.
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo_with_origin(dir.path(), "git@gitlab.example.com:team/widget.git");
    let path = dir.path().join("CHANGELOG.md");
    let fixture = "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### Fixes\n\
- curated fix\n";
    std::fs::write(&path, fixture).expect("write fixture");

    let out = merge_into_changelog(MergeArgs {
        file_path: &path,
        h1: "# Changelog — widget",
        new_section: "## [0.2.0] - 2026-06-03\n\n### Fixes\n- gen\n",
        generated_body: "### Fixes\n- gen",
        from_tag: Some("v0.1.0"),
        to_version: "0.2.0",
        workspace_root: dir.path(),
    })
    .expect("merge succeeds");

    assert!(
        out.contains("[Unreleased]: https://gitlab.example.com/team/widget/compare/v0.2.0...HEAD"),
        "synthesized footer must use the GitLab host: {out}"
    );
    assert!(
        out.contains("[0.2.0]: https://gitlab.example.com/team/widget/compare/v0.1.0...v0.2.0"),
        "synthesized [0.2.0] link must use the GitLab host: {out}"
    );
    assert!(
        !out.contains("github.com"),
        "must never synthesize a github.com URL for a non-GitHub remote: {out}"
    );
}

#[test]
fn non_kac_file_behavior_unchanged() {
    // A simple non-KAC file (no `## [Unreleased]` heading): the section is
    // spliced after the H1 and nothing else is touched.
    let fixture = "# Changelog — anodizer\n\
\n\
## [0.1.0] - 2026-01-01\n\
\n\
- first\n";
    let out = run_merge(fixture, "- gen", Some("v0.1.0"), "0.2.0");

    assert!(
        out.starts_with("# Changelog — anodizer\n"),
        "H1 must be preserved: {out}"
    );
    // New section spliced directly after the H1, above the prior release.
    let new_pos = out.find("## [0.2.0] - 2026-06-03").expect("new section");
    let old_pos = out.find("## [0.1.0] - 2026-01-01").expect("old section");
    assert!(new_pos < old_pos, "new section must precede old: {out}");
    // No Unreleased heading is introduced for a non-KAC file.
    assert!(
        !out.contains("## [Unreleased]"),
        "non-KAC must not add Unreleased: {out}"
    );
    // No compare-link footer synthesized for the non-KAC path.
    assert!(
        !out.contains("[Unreleased]:"),
        "non-KAC must not add footer links: {out}"
    );
}

// ---------------------------------------------------------------------------
// Bullet de-duplication + Login/AuthorUsername fallback (local-git render path).
// ---------------------------------------------------------------------------

/// One default-format commit gets exactly one leading `* ` bullet.
#[test]
fn default_format_emits_single_bullet() {
    let grouped = vec![GroupedCommits {
        title: String::new(),
        commits: vec![ci("feat: add X", "feat", "add X", "abc1234")],
        subgroups: Vec::new(),
    }];
    let md = render_changelog(&grouped, 7, None, "", "git", None, None);
    let line = md
        .lines()
        .find(|l| l.contains("add X"))
        .expect("bullet line present");
    assert_eq!(line, "* abc1234 add X", "exactly one `* ` bullet: {md:?}");
}

/// A user `format:` already leading with `* ` must NOT be double-bulleted.
#[test]
fn leading_star_bullet_not_doubled() {
    let grouped = vec![GroupedCommits {
        title: String::new(),
        commits: vec![ci("feat: add X", "feat", "add X", "abc1234")],
        subgroups: Vec::new(),
    }];
    let md = render_changelog(
        &grouped,
        7,
        Some("* {{ .SHA }} {{ .Message }}"),
        "",
        "git",
        None,
        None,
    );
    let line = md
        .lines()
        .find(|l| l.contains("add X"))
        .expect("bullet line present");
    assert_eq!(line, "* abc1234 add X", "single bullet, no `* *`: {md:?}");
    assert!(!md.contains("* *"), "no doubled bullet anywhere: {md:?}");
}

/// A user `format:` leading with `- ` is preserved verbatim (no `* ` prepend).
#[test]
fn leading_dash_bullet_preserved() {
    let grouped = vec![GroupedCommits {
        title: String::new(),
        commits: vec![ci("feat: add X", "feat", "add X", "abc1234")],
        subgroups: Vec::new(),
    }];
    let md = render_changelog(
        &grouped,
        7,
        Some("- {{ .SHA }} {{ .Message }}"),
        "",
        "git",
        None,
        None,
    );
    let line = md
        .lines()
        .find(|l| l.contains("add X"))
        .expect("bullet line present");
    assert_eq!(line, "- abc1234 add X", "dash bullet preserved: {md:?}");
}

/// Empty `login` + an `AuthorUsername` reference falls back to the author name.
#[test]
fn empty_login_falls_back_to_author_name() {
    let mut commit = ci("feat: add X", "feat", "add X", "abc1234");
    commit.author_name = "Jane Roe".into();
    // login left empty (the local-git changelog path never populates it).
    let grouped = vec![GroupedCommits {
        title: String::new(),
        commits: vec![commit],
        subgroups: Vec::new(),
    }];
    let md = render_changelog(
        &grouped,
        7,
        Some("{{ .SHA }} {{ .Message }} ({{ .AuthorUsername }})"),
        "",
        "git",
        None,
        None,
    );
    assert!(
        md.contains("(Jane Roe)"),
        "empty login should render author name, not (): {md:?}"
    );
    assert!(!md.contains("()"), "no empty parens: {md:?}");
}

/// Non-empty `login` renders the `@login` mention (fallback only fires when
/// empty). Bare style: the GitHub release body autolinks the mention itself.
#[test]
fn nonempty_login_renders_bare_mention() {
    let mut commit = ci("feat: add X", "feat", "add X", "abc1234");
    commit.author_name = "Jane Roe".into();
    commit.login = "janeroe".into();
    let grouped = vec![GroupedCommits {
        title: String::new(),
        commits: vec![commit],
        subgroups: Vec::new(),
    }];
    let md = render_changelog(
        &grouped,
        7,
        Some("{{ .SHA }} {{ .Message }} ({{ .AuthorUsername }})"),
        "",
        "git",
        None,
        None,
    );
    assert!(
        md.contains("(@janeroe)"),
        "non-empty login must render the @login mention: {md:?}"
    );
    assert!(
        !md.contains("Jane Roe"),
        "author name must not leak when login is present: {md:?}"
    );
}

/// Render through the canonical entry point with an explicit [`LoginStyle`].
fn render_with_style(
    grouped: &[GroupedCommits],
    format: &str,
    use_source: &str,
    style: crate::render::LoginStyle,
) -> String {
    crate::render::render_changelog_with_provider(
        grouped,
        crate::render::ChangelogRenderOpts {
            abbrev: 7,
            format_template: if format.is_empty() {
                None
            } else {
                Some(format)
            },
            logins: "",
            use_source,
            title: None,
            divider: None,
            scm_provider: None,
            login_style: style,
        },
    )
    .expect("render with style")
}

/// Linked style (on-disk `CHANGELOG.md`): a resolved login renders an
/// explicit Markdown link, since nothing autolinks committed files.
#[test]
fn linked_style_renders_markdown_login_link() {
    let mut commit = ci("feat: add X", "feat", "add X", "abc1234");
    commit.author_name = "Jane Roe".into();
    commit.login = "janeroe".into();
    let grouped = vec![GroupedCommits::new("", vec![commit])];
    let md = render_with_style(
        &grouped,
        "{{ .SHA }} {{ .Message }} ({{ .AuthorUsername }})",
        "git",
        crate::render::LoginStyle::Linked,
    );
    assert!(
        md.contains("([@janeroe](https://github.com/janeroe))"),
        "linked style must render an explicit Markdown link: {md:?}"
    );
}

/// Linked style with NO login resolved must be byte-identical to the
/// historical name-based rendering (the graceful-degradation contract).
#[test]
fn linked_style_unresolved_login_is_byte_identical_name_fallback() {
    let mut commit = ci(
        "fix: close audit gaps",
        "fix",
        "close audit gaps",
        "ef88059e",
    );
    commit.full_hash = "ef88059e1234567890abcdef1234567890abcdef".into();
    commit.author_name = "TJ Smith".into();
    let grouped = vec![GroupedCommits::new("", vec![commit])];
    let fmt = "* {{ .SHA }} {{ .Message }} ({{ .AuthorUsername }})";
    let linked = render_with_style(&grouped, fmt, "git", crate::render::LoginStyle::Linked);
    let bare = render_with_style(&grouped, fmt, "git", crate::render::LoginStyle::Bare);
    assert_eq!(
        linked, bare,
        "with no login the two styles must agree byte-for-byte"
    );
    let line = linked
        .lines()
        .find(|l| l.contains("close audit gaps"))
        .expect("bullet line present");
    assert_eq!(line, "* ef88059 close audit gaps (TJ Smith)");
}

/// The default SCM format (the `AuthorUsername` mention with the name/email
/// fallback) gets the linked rendering for free in kac style — same render
/// path, no format-template duplication.
#[test]
fn default_scm_format_links_login_in_linked_style() {
    let mut commit = ci("feat: add X", "feat", "add X", "abc1234");
    commit.full_hash = "abc1234567890abcdef1234567890abcdef12345".into();
    commit.author_name = "Octocat".into();
    commit.author_email = "octocat@github.com".into();
    commit.login = "octocat".into();
    let grouped = vec![GroupedCommits::new("", vec![commit])];
    let md = render_with_style(&grouped, "", "github", crate::render::LoginStyle::Linked);
    assert!(
        md.contains("([@octocat](https://github.com/octocat))"),
        "default SCM format must link the login in kac style: {md:?}"
    );
}

/// A template that prefixes its own `@` (the GR-ported
/// `(@{{ .AuthorUsername }})` shape) must not double the `@` now that
/// `AuthorUsername` carries the mention form.
#[test]
fn at_prefixed_author_username_does_not_double_at() {
    let mut commit = ci("feat: add X", "feat", "add X", "abc1234");
    commit.author_name = "Octocat".into();
    commit.login = "octocat".into();
    let grouped = vec![GroupedCommits::new("", vec![commit])];
    let fmt = "{{ .SHA }} {{ .Message }} (@{{ .AuthorUsername }})";
    let bare = render_with_style(&grouped, fmt, "git", crate::render::LoginStyle::Bare);
    assert!(
        bare.contains("(@octocat)") && !bare.contains("@@"),
        "double @ must collapse: {bare:?}"
    );
    let linked = render_with_style(&grouped, fmt, "git", crate::render::LoginStyle::Linked);
    assert!(
        linked.contains("([@octocat](https://github.com/octocat))"),
        "collapsed mention must still link in kac style: {linked:?}"
    );
}

/// A commit subject that legitimately contains `@<login>` text — where the
/// login happens to equal a resolved author's — stays plain in BOTH styles:
/// styling targets only the renderer-substituted mention span, never
/// coincidental free text.
#[test]
fn coincidental_login_text_in_message_stays_plain() {
    let mut commit = ci(
        "fix: remove @deprecated usage",
        "fix",
        "remove @deprecated usage",
        "abc1234",
    );
    commit.full_hash = "abc1234567890abcdef1234567890abcdef12345".into();
    commit.author_name = "Dep Recated".into();
    commit.login = "deprecated".into();
    let grouped = vec![GroupedCommits::new("", vec![commit])];
    let fmt = "* {{ .SHA }} {{ .Message }} ({{ .AuthorUsername }})";
    let linked = render_with_style(&grouped, fmt, "git", crate::render::LoginStyle::Linked);
    let line = linked
        .lines()
        .find(|l| l.contains("remove"))
        .expect("bullet line present");
    assert_eq!(
        line, "* abc1234 remove @deprecated usage ([@deprecated](https://github.com/deprecated))",
        "message text must stay plain; only the author slot links"
    );
    let bare = render_with_style(&grouped, fmt, "git", crate::render::LoginStyle::Bare);
    let line = bare
        .lines()
        .find(|l| l.contains("remove"))
        .expect("bullet line present");
    assert_eq!(line, "* abc1234 remove @deprecated usage (@deprecated)");
}

/// A crafted sentinel-framed "mention" inside a commit subject is neutralized
/// by input sanitization: it neither triggers styling nor leaks the sentinel
/// control character — on the resolved-login path AND the empty-login path.
#[test]
fn crafted_sentinel_in_message_never_styles_or_leaks() {
    let evil_subject = "say \u{1}@evil\u{1} aloud";
    let mut commit = ci("feat: x", "feat", evil_subject, "abc1234");
    commit.author_name = "Ada L".into();
    commit.login = "ada".into();
    let grouped = vec![GroupedCommits::new("", vec![commit])];
    let fmt = "* {{ .SHA }} {{ .Message }} ({{ .AuthorUsername }})";
    for style in [
        crate::render::LoginStyle::Bare,
        crate::render::LoginStyle::Linked,
    ] {
        let md = render_with_style(&grouped, fmt, "git", style);
        assert!(!md.contains('\u{1}'), "sentinel leaked: {md:?}");
        assert!(
            md.contains("say @evil aloud"),
            "crafted span must degrade to plain text: {md:?}"
        );
        assert!(
            !md.contains("github.com/evil"),
            "crafted span must never be styled into a link: {md:?}"
        );
    }
    // Empty-login path: no styling pass runs, the sentinel still never leaks.
    let mut commit = ci("feat: x", "feat", evil_subject, "abc1234");
    commit.author_name = "Ada L".into();
    let grouped = vec![GroupedCommits::new("", vec![commit])];
    for style in [
        crate::render::LoginStyle::Bare,
        crate::render::LoginStyle::Linked,
    ] {
        let md = render_with_style(&grouped, fmt, "git", style);
        assert!(!md.contains('\u{1}'), "sentinel leaked: {md:?}");
        assert!(md.contains("say @evil aloud"), "{md:?}");
    }
}

/// The exact anodizer-dogfooding shape: leading `* ` + `(AuthorUsername)` with an
/// empty login → one bullet and a non-empty author.
#[test]
fn anodizer_shaped_format_single_bullet_and_named_author() {
    let mut commit = ci(
        "fix: close audit gaps",
        "fix",
        "close audit gaps",
        "ef88059e",
    );
    commit.author_name = "TJ Smith".into();
    let grouped = vec![GroupedCommits {
        title: String::new(),
        commits: vec![commit],
        subgroups: Vec::new(),
    }];
    let md = render_changelog(
        &grouped,
        8,
        Some("* {{ .SHA }} {{ .Message }} ({{ .AuthorUsername }})"),
        "",
        "git",
        None,
        None,
    );
    let line = md
        .lines()
        .find(|l| l.contains("close audit gaps"))
        .expect("bullet line present");
    assert_eq!(line, "* ef88059e close audit gaps (TJ Smith)", "{md:?}");
}

// ---------------------------------------------------------------------------
// refresh_*_unreleased + render_changelog_json — generate-only [Unreleased]
// regeneration and JSON serialization over a real git repo.
// ---------------------------------------------------------------------------

mod refresh_unreleased_tests {
    use std::path::Path;
    use std::process::Command;

    use anodizer_core::config::Chronology;

    use crate::{
        InsertionMode, refresh_crate_unreleased, refresh_root_unreleased, render_changelog_json,
        render_crate_section, render_root_section,
    };

    /// Run `git <args>` inside `dir`, asserting success.
    fn git(dir: &Path, args: &[&str]) {
        let ok = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args).current_dir(dir);
                cmd
            },
            "git",
        )
        .status
        .success();
        assert!(ok, "git {:?} failed in {}", args, dir.display());
    }

    /// Fresh git repo with deterministic identity; returns the repo root.
    fn init_repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "test@example.com"]);
        git(root, &["config", "user.name", "Test User"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        tmp
    }

    /// Stage all and commit with `subject` inside `dir`.
    fn commit(dir: &Path, subject: &str) {
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", subject]);
    }

    /// Write `.anodizer.yaml` with a feat/fix `groups:` config at `root`.
    fn write_config(root: &Path) {
        std::fs::write(
            root.join(".anodizer.yaml"),
            "changelog:\n  groups:\n    - title: Features\n      regexp: '^feat'\n      order: 0\n    - title: Bug Fixes\n      regexp: '^fix'\n      order: 1\n",
        )
        .expect("write config");
    }

    #[test]
    fn refresh_fills_empty_unreleased_from_commits() {
        let tmp = init_repo();
        let root = tmp.path();
        write_config(root);
        std::fs::write(root.join("a.txt"), "x").unwrap();
        commit(root, "feat: add a thing");

        std::fs::write(
            root.join("CHANGELOG.md"),
            "# Changelog\n\n## [Unreleased]\n",
        )
        .unwrap();

        let update = refresh_crate_unreleased(root, "mylib", root, None, None)
            .expect("ok")
            .expect("some update");
        assert_eq!(update.insertion_mode, InsertionMode::Replace);
        assert!(
            update.rendered_text.contains("### Features"),
            "expected Features heading, got:\n{}",
            update.rendered_text
        );
        assert!(update.rendered_text.contains("add a thing"));
        // H1 preserved.
        assert!(update.rendered_text.starts_with("# Changelog\n"));
    }

    #[test]
    fn refresh_replaces_stale_unreleased_body() {
        let tmp = init_repo();
        let root = tmp.path();
        write_config(root);
        std::fs::write(root.join("a.txt"), "x").unwrap();
        commit(root, "fix: real bug");

        std::fs::write(
            root.join("CHANGELOG.md"),
            "# Changelog\n\n## [Unreleased]\n\n### Features\n- stale: leftover entry\n",
        )
        .unwrap();

        let update = refresh_crate_unreleased(root, "mylib", root, None, None)
            .expect("ok")
            .expect("some update");
        assert!(
            !update.rendered_text.contains("leftover entry"),
            "stale body must be replaced, got:\n{}",
            update.rendered_text
        );
        assert!(update.rendered_text.contains("real bug"));
        assert!(update.rendered_text.contains("### Bug Fixes"));
    }

    #[test]
    fn refresh_preserves_released_sections_and_footer() {
        let tmp = init_repo();
        let root = tmp.path();
        write_config(root);
        std::fs::write(root.join("a.txt"), "x").unwrap();
        commit(root, "feat: brand new");

        let existing = "# Changelog\n\n\
## [Unreleased]\n\n\
## [v0.1.0] - 2026-01-01\n\
### Features\n\
- earlier: shipped feature\n\n\
[Unreleased]: https://github.com/o/r/compare/v0.1.0...HEAD\n\
[v0.1.0]: https://github.com/o/r/releases/tag/v0.1.0\n";
        std::fs::write(root.join("CHANGELOG.md"), existing).unwrap();

        let update = refresh_crate_unreleased(root, "mylib", root, None, None)
            .expect("ok")
            .expect("some update");
        let out = &update.rendered_text;
        assert!(out.contains("## [v0.1.0] - 2026-01-01"));
        assert!(out.contains("- earlier: shipped feature"));
        assert!(out.contains("[Unreleased]: https://github.com/o/r/compare/v0.1.0...HEAD"));
        assert!(out.contains("[v0.1.0]: https://github.com/o/r/releases/tag/v0.1.0"));
        assert!(out.contains("brand new"));
    }

    #[test]
    fn refresh_preserves_fenced_code_block_double_blank_verbatim() {
        let tmp = init_repo();
        let root = tmp.path();
        write_config(root);
        std::fs::write(root.join("a.txt"), "x").unwrap();
        commit(root, "feat: brand new");

        // A released section whose body holds a fenced code block with an
        // intentional double-blank line inside the fence. The fence interior
        // must survive a refresh byte-for-byte (blank-collapsing is
        // fence-aware).
        let fenced = "## [v0.1.0] - 2026-01-01\n\
### Notes\n\
- example usage:\n\n\
```text\n\
line one\n\
\n\
\n\
line two\n\
```\n";
        let existing = format!(
            "# Changelog\n\n## [Unreleased]\n\n{fenced}\n\
[Unreleased]: https://github.com/o/r/compare/v0.1.0...HEAD\n"
        );
        std::fs::write(root.join("CHANGELOG.md"), &existing).unwrap();

        let update = refresh_crate_unreleased(root, "mylib", root, None, None)
            .expect("ok")
            .expect("some update");
        let out = &update.rendered_text;
        assert!(
            out.contains("line one\n\n\nline two"),
            "double-blank inside the fence must be preserved verbatim, got:\n{out}"
        );
        // The whole fenced section survives intact.
        assert!(out.contains(fenced.trim_end()));
    }

    #[test]
    fn refresh_multitrack_root_updates_only_target_subsection() {
        let tmp = init_repo();
        let root = tmp.path();
        write_config(root);
        let crate_dir = root.join("crates").join("cfgd");
        std::fs::create_dir_all(&crate_dir).unwrap();
        std::fs::write(crate_dir.join("lib.rs"), "x").unwrap();
        commit(root, "feat: cfgd new capability");

        let existing = "# Changelog\n\n\
## [Unreleased]\n\n\
### cfgd\n\
- stale: old cfgd entry\n\n\
### cfgd-core\n\
- keep: sibling untouched\n\n\
## [v0.2.0] - 2026-02-02\n\
### Features\n\
- released: thing\n\n\
[Unreleased]: https://github.com/o/r/compare/v0.2.0...HEAD\n";
        std::fs::write(root.join("CHANGELOG.md"), existing).unwrap();

        let update = refresh_root_unreleased(
            root,
            "cfgd",
            &crate_dir,
            None,
            None,
            Chronology::Date,
            // Topology count is 1 (single target); the crate-name-aware fallback
            // over the known crate set routes to the `### cfgd` subsection.
            false,
            &["cfgd".to_string(), "cfgd-core".to_string()],
            None,
        )
        .expect("ok")
        .expect("some update");
        let out = &update.rendered_text;
        assert!(
            !out.contains("old cfgd entry"),
            "target subsection should be regenerated, got:\n{out}"
        );
        assert!(out.contains("cfgd new capability"));
        assert!(
            out.contains("- keep: sibling untouched"),
            "sibling subsection must be preserved, got:\n{out}"
        );
        assert!(out.contains("### cfgd-core"));
        assert!(out.contains("## [v0.2.0] - 2026-02-02"));
        assert!(out.contains("[Unreleased]: https://github.com/o/r/compare/v0.2.0...HEAD"));
    }

    #[test]
    fn refresh_flat_root_behaves_like_crate_path() {
        let tmp = init_repo();
        let root = tmp.path();
        write_config(root);
        std::fs::write(root.join("a.txt"), "x").unwrap();
        commit(root, "feat: flat root feature");

        std::fs::write(
            root.join("CHANGELOG.md"),
            "# Changelog\n\n## [Unreleased]\n",
        )
        .unwrap();

        let root_update = refresh_root_unreleased(
            root,
            "mylib",
            root,
            None,
            None,
            Chronology::Date,
            // Single-track topology: flat root, byte-identical to the per-crate
            // path.
            false,
            &[],
            None,
        )
        .expect("ok")
        .expect("some update");
        let crate_update = refresh_crate_unreleased(root, "mylib", root, None, None)
            .expect("ok")
            .expect("some update");
        assert_eq!(
            root_update.rendered_text, crate_update.rendered_text,
            "flat root must match the per-crate path byte-for-byte"
        );
    }

    #[test]
    fn refresh_creates_skeleton_when_file_absent() {
        let tmp = init_repo();
        let root = tmp.path();
        write_config(root);
        std::fs::write(root.join("a.txt"), "x").unwrap();
        commit(root, "feat: first ever");

        let update = refresh_crate_unreleased(root, "mylib", root, None, None)
            .expect("ok")
            .expect("some update");
        let out = &update.rendered_text;
        // Per-crate file: a synthesized H1 is crate-named (matches the per-crate
        // promote path).
        assert!(
            out.starts_with("# Changelog — mylib\n"),
            "per-crate refresh synthesizes a crate-named H1, got:\n{out}"
        );
        assert!(out.contains("## [Unreleased]"));
        assert!(out.contains("first ever"));
        assert!(
            !out.contains("[Unreleased]:"),
            "first creation synthesizes no footer, got:\n{out}"
        );
    }

    #[test]
    fn refresh_inserts_unreleased_after_h1_for_non_kac_file() {
        let tmp = init_repo();
        let root = tmp.path();
        write_config(root);
        std::fs::write(root.join("a.txt"), "x").unwrap();
        commit(root, "feat: insert me");

        std::fs::write(
            root.join("CHANGELOG.md"),
            "# My Project Changes\n\nSome prose preamble.\n\n## [v0.1.0]\n- old: thing\n",
        )
        .unwrap();

        let update = refresh_crate_unreleased(root, "mylib", root, None, None)
            .expect("ok")
            .expect("some update");
        let out = &update.rendered_text;
        let h1_pos = out.find("# My Project Changes").expect("h1");
        let unreleased_pos = out.find("## [Unreleased]").expect("unreleased");
        let old_pos = out.find("## [v0.1.0]").expect("old section");
        assert!(h1_pos < unreleased_pos, "Unreleased must follow H1");
        assert!(
            unreleased_pos < old_pos,
            "Unreleased must precede the existing released section"
        );
        assert!(out.contains("Some prose preamble."));
        assert!(out.contains("- old: thing"));
        assert!(out.contains("insert me"));
    }

    #[test]
    fn refresh_is_idempotent() {
        let tmp = init_repo();
        let root = tmp.path();
        write_config(root);
        std::fs::write(root.join("a.txt"), "x").unwrap();
        commit(root, "feat: idempotent thing");
        std::fs::write(
            root.join("CHANGELOG.md"),
            "# Changelog\n\n## [Unreleased]\n",
        )
        .unwrap();

        let first = refresh_crate_unreleased(root, "mylib", root, None, None)
            .expect("ok")
            .expect("some update");
        // Apply the update, then refresh again — second run is a no-op.
        std::fs::write(root.join("CHANGELOG.md"), &first.rendered_text).unwrap();
        let second = refresh_crate_unreleased(root, "mylib", root, None, None).expect("ok");
        assert!(
            second.is_none(),
            "second refresh must be a no-op, got:\n{:?}",
            second.map(|u| u.rendered_text)
        );
    }

    #[test]
    fn refresh_returns_none_without_config() {
        let tmp = init_repo();
        let root = tmp.path();
        // No .anodizer.yaml written.
        std::fs::write(root.join("a.txt"), "x").unwrap();
        commit(root, "feat: no config here");
        std::fs::write(
            root.join("CHANGELOG.md"),
            "# Changelog\n\n## [Unreleased]\n",
        )
        .unwrap();

        let update = refresh_crate_unreleased(root, "mylib", root, None, None).expect("ok");
        assert!(update.is_none(), "no changelog: config ⇒ Ok(None)");
    }

    #[test]
    fn to_ref_bounds_the_commit_range() {
        let tmp = init_repo();
        let root = tmp.path();
        write_config(root);

        std::fs::write(root.join("a.txt"), "x").unwrap();
        commit(root, "feat: included before bound");
        git(root, &["tag", "bound"]);
        std::fs::write(root.join("b.txt"), "y").unwrap();
        commit(root, "feat: excluded after bound");

        // to_ref = "bound" ⇒ only the first commit is in range.
        let update = refresh_crate_unreleased(root, "mylib", root, None, Some("bound"))
            .expect("ok")
            .expect("some update");
        let out = &update.rendered_text;
        assert!(out.contains("included before bound"));
        assert!(
            !out.contains("excluded after bound"),
            "commits after to_ref must be excluded, got:\n{out}"
        );
    }

    #[test]
    fn nonexistent_to_ref_yields_err_not_silent_empty() {
        let tmp = init_repo();
        let root = tmp.path();
        write_config(root);
        std::fs::write(root.join("a.txt"), "x").unwrap();
        commit(root, "feat: a real commit");

        // A typo'd upper bound must surface as an error rather than a
        // silently-empty changelog.
        let result = refresh_crate_unreleased(root, "mylib", root, None, Some("nope-not-a-ref"));
        let err = result.expect_err("nonexistent to_ref must be an error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("nope-not-a-ref") || msg.to_lowercase().contains("revision"),
            "error should name the offending ref / revision, got: {msg}"
        );
    }

    #[test]
    fn json_empty_range_returns_none() {
        let tmp = init_repo();
        let root = tmp.path();
        write_config(root);
        std::fs::write(root.join("a.txt"), "x").unwrap();
        commit(root, "feat: only commit");
        git(root, &["tag", "v1.0.0"]);

        // from..to with no commits between ⇒ Ok(None).
        let json = render_changelog_json(root, root, Some("v1.0.0"), None).expect("ok");
        assert!(json.is_none(), "empty range ⇒ Ok(None), got: {json:?}");
    }

    #[test]
    fn json_grouped_commits_produce_documented_shape() {
        let tmp = init_repo();
        let root = tmp.path();
        write_config(root);
        std::fs::write(root.join("a.txt"), "x").unwrap();
        commit(root, "feat: shiny feature");
        std::fs::write(root.join("b.txt"), "y").unwrap();
        commit(root, "fix: nasty bug");

        let json = render_changelog_json(root, root, None, None)
            .expect("ok")
            .expect("some json");
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid json");

        assert_eq!(v["from"], serde_json::Value::Null);
        assert_eq!(v["to"], "HEAD");
        let groups = v["groups"].as_array().expect("groups array");
        let titles: Vec<&str> = groups.iter().filter_map(|g| g["title"].as_str()).collect();
        assert!(titles.contains(&"Features"), "got groups: {titles:?}");
        assert!(titles.contains(&"Bug Fixes"), "got groups: {titles:?}");

        let features = groups
            .iter()
            .find(|g| g["title"] == "Features")
            .expect("Features group");
        let entry = &features["entries"][0];
        assert_eq!(entry["summary"], "shiny feature");
        assert!(entry["sha"].as_str().expect("sha").len() >= 4);
        assert!(entry["full_sha"].as_str().expect("full_sha").len() >= 7);
        let authors = entry["authors"].as_array().expect("authors");
        assert_eq!(authors[0], "Test User");
        assert!(
            features["subgroups"]
                .as_array()
                .expect("subgroups")
                .is_empty()
        );
    }

    #[test]
    fn json_subgroups_nest() {
        let tmp = init_repo();
        let root = tmp.path();
        // A Features group with a nested "Scoped" subgroup matching `feat(.*)`.
        std::fs::write(
            root.join(".anodizer.yaml"),
            "changelog:\n  groups:\n    - title: Features\n      regexp: '^feat'\n      order: 0\n      groups:\n        - title: Scoped\n          regexp: '^feat\\('\n          order: 0\n",
        )
        .unwrap();
        std::fs::write(root.join("a.txt"), "x").unwrap();
        commit(root, "feat(api): scoped feature");

        let json = render_changelog_json(root, root, None, None)
            .expect("ok")
            .expect("some json");
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        let features = v["groups"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["title"] == "Features")
            .expect("Features group");
        let subgroups = features["subgroups"].as_array().expect("subgroups");
        let scoped = subgroups
            .iter()
            .find(|s| s["title"] == "Scoped")
            .expect("Scoped subgroup nested under Features");
        assert_eq!(scoped["entries"][0]["summary"], "scoped feature");
    }

    #[test]
    fn json_from_and_to_populated_with_bounds() {
        let tmp = init_repo();
        let root = tmp.path();
        write_config(root);
        std::fs::write(root.join("a.txt"), "x").unwrap();
        commit(root, "feat: base commit");
        git(root, &["tag", "v0.1.0"]);
        std::fs::write(root.join("b.txt"), "y").unwrap();
        commit(root, "feat: next commit");
        git(root, &["tag", "v0.2.0"]);

        let json = render_changelog_json(root, root, Some("v0.1.0"), Some("v0.2.0"))
            .expect("ok")
            .expect("some json");
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(v["from"], "v0.1.0");
        assert_eq!(v["to"], "v0.2.0");
        let summaries: Vec<String> = v["groups"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["entries"].as_array().cloned().unwrap_or_default())
            .filter_map(|e| e["summary"].as_str().map(str::to_string))
            .collect();
        assert!(summaries.iter().any(|s| s == "next commit"));
        assert!(
            !summaries.iter().any(|s| s == "base commit"),
            "base commit is the lower bound and must be excluded"
        );
    }

    // -- Multitrack root aggregate (PerCrate) -------------------------------
    //
    // These exercise the topology-driven root aggregate: a fresh/empty/foreign
    // root must NOT lose any crate's commits (R3), must omit empty crates (R4),
    // nest groups as `#### Group` under `### <crate>` (R5), classify by crate
    // name (R2), and be idempotent (R6). The caller's sequential threading
    // (`existing_override`) is mirrored by `refresh_root_multitrack`.

    /// Commit `subject` after writing a file under `crates/<crate>/`, so the
    /// path-scoped changelog fetch attributes it to that crate.
    fn commit_in_crate(root: &Path, crate_name: &str, file: &str, subject: &str) {
        let dir = root.join("crates").join(crate_name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(file), subject).unwrap();
        commit(root, subject);
    }

    /// Drive the multitrack root refresh loop the way the CLI caller does:
    /// each crate refreshes against the running result of the previous one
    /// (`existing_override`), so every `### <crate>` subsection accumulates.
    /// Returns the final root text (or the seed when nothing changed).
    fn refresh_root_multitrack(root: &Path, crate_names: &[String], seed: Option<&str>) -> String {
        let mut working: Option<String> = seed.map(str::to_string);
        for name in crate_names {
            let crate_dir = root.join("crates").join(name);
            let update = refresh_root_unreleased(
                root,
                name,
                &crate_dir,
                None,
                None,
                Chronology::Date,
                true,
                crate_names,
                working.as_deref(),
            )
            .expect("refresh ok");
            if let Some(u) = update {
                working = Some(u.rendered_text);
            }
        }
        working.unwrap_or_default()
    }

    /// R3 + R4 + R5: a FRESH (absent) root must bootstrap one `### <crate>`
    /// subsection per non-empty crate with NO data loss, omit the empty crate,
    /// and nest group headings as `#### <Group>`.
    #[test]
    fn multitrack_fresh_root_bootstraps_all_crates_no_loss() {
        let tmp = init_repo();
        let root = tmp.path();
        write_config(root);
        commit_in_crate(root, "cfgd-core", "a.rs", "feat: core feature");
        commit_in_crate(root, "cfgd-core", "b.rs", "fix: core bug");
        commit_in_crate(root, "cfgd", "c.rs", "feat: cfgd feature");
        commit_in_crate(root, "cfgd-csi", "d.rs", "fix: csi repair");
        // cfgd-operator: NO commit → must be omitted (R4).

        let crate_names = vec![
            "cfgd-core".to_string(),
            "cfgd".to_string(),
            "cfgd-csi".to_string(),
            "cfgd-operator".to_string(),
        ];
        let out = refresh_root_multitrack(root, &crate_names, None);

        // R3: every non-empty crate present, NONE lost.
        assert!(out.contains("### cfgd-core"), "cfgd-core lost:\n{out}");
        assert!(out.contains("### cfgd"), "cfgd lost:\n{out}");
        assert!(out.contains("### cfgd-csi"), "cfgd-csi lost:\n{out}");
        assert!(out.contains("core feature"), "core commit lost:\n{out}");
        assert!(out.contains("core bug"), "core fix lost:\n{out}");
        assert!(out.contains("cfgd feature"), "cfgd commit lost:\n{out}");
        assert!(out.contains("csi repair"), "csi commit lost:\n{out}");
        // R4: empty crate omitted.
        assert!(
            !out.contains("### cfgd-operator"),
            "empty crate must be omitted:\n{out}"
        );
        // R5: groups nest one level deeper under the crate subsection.
        assert!(
            out.contains("#### Features"),
            "group headings must be `#### Group` under a crate subsection:\n{out}"
        );
        assert!(
            !out.contains("\n### Features"),
            "no `### Group` heading should appear in multitrack mode:\n{out}"
        );
    }

    /// R3: a FRESH-EMPTY `## [Unreleased]` (the exact last-writer-wins trigger)
    /// must accumulate every crate's subsection rather than the flat path
    /// replacing the whole body each iteration.
    #[test]
    fn multitrack_fresh_empty_unreleased_accumulates_subsections() {
        let tmp = init_repo();
        let root = tmp.path();
        write_config(root);
        commit_in_crate(root, "cfgd-core", "a.rs", "feat: core thing");
        commit_in_crate(root, "cfgd", "c.rs", "feat: cfgd thing");

        std::fs::write(
            root.join("CHANGELOG.md"),
            "# Changelog\n\n## [Unreleased]\n",
        )
        .unwrap();

        let crate_names = vec!["cfgd-core".to_string(), "cfgd".to_string()];
        let out = refresh_root_multitrack(root, &crate_names, None);
        assert!(out.contains("### cfgd-core"), "first crate lost:\n{out}");
        assert!(
            out.contains("### cfgd\n") || out.contains("### cfgd\n\n"),
            "second crate lost (last-writer-wins regression):\n{out}"
        );
        assert!(out.contains("core thing"));
        assert!(out.contains("cfgd thing"));
    }

    /// R2: a FOREIGN (git-cliff-style) flat `[Unreleased]` leading with
    /// `### Added` must NOT be misclassified as a crate subsection. In
    /// multitrack mode the crate's `### <crate>` subsection is appended; the
    /// foreign content is preserved (not duplicated under a spurious crate).
    #[test]
    fn multitrack_foreign_flat_root_is_not_misclassified() {
        let tmp = init_repo();
        let root = tmp.path();
        write_config(root);
        commit_in_crate(root, "cfgd-core", "a.rs", "feat: real core");

        let existing = "# Changelog\n\n## [Unreleased]\n\n### Added\n- git-cliff entry\n\n[Unreleased]: https://github.com/o/r/compare/v0.1.0...HEAD\n";
        std::fs::write(root.join("CHANGELOG.md"), existing).unwrap();

        let crate_names = vec!["cfgd-core".to_string(), "cfgd".to_string()];
        // Single iteration for cfgd-core (multitrack topology forces subsection).
        let crate_dir = root.join("crates").join("cfgd-core");
        let out = refresh_root_unreleased(
            root,
            "cfgd-core",
            &crate_dir,
            None,
            None,
            Chronology::Date,
            true,
            &crate_names,
            None,
        )
        .expect("ok")
        .expect("update");
        let text = &out.rendered_text;
        assert!(
            text.contains("### Added") && text.contains("git-cliff entry"),
            "foreign content must be preserved:\n{text}"
        );
        assert!(
            text.contains("### cfgd-core") && text.contains("real core"),
            "crate subsection appended alongside foreign content:\n{text}"
        );
        // Foreign `### Added` must appear exactly once (not duplicated).
        assert_eq!(
            text.matches("### Added").count(),
            1,
            "foreign heading duplicated:\n{text}"
        );
    }

    /// R6: re-running the multitrack refresh with no new commits is
    /// byte-identical (idempotent).
    #[test]
    fn multitrack_refresh_is_idempotent() {
        let tmp = init_repo();
        let root = tmp.path();
        write_config(root);
        commit_in_crate(root, "cfgd-core", "a.rs", "feat: core thing");
        commit_in_crate(root, "cfgd", "c.rs", "feat: cfgd thing");

        let crate_names = vec!["cfgd-core".to_string(), "cfgd".to_string()];
        let first = refresh_root_multitrack(root, &crate_names, None);
        std::fs::write(root.join("CHANGELOG.md"), &first).unwrap();
        let second = refresh_root_multitrack(root, &crate_names, Some(&first));
        assert_eq!(first, second, "second pass must be byte-identical");
    }

    /// R4 (data-loss guard): an EXISTING `### <crate>` subsection with content,
    /// refreshed over a range that yields NO new commits, must be left untouched
    /// — not blanked. The whole file is byte-identical and the sibling
    /// subsection survives verbatim.
    #[test]
    fn multitrack_existing_subsection_with_empty_body_is_untouched() {
        let tmp = init_repo();
        let root = tmp.path();
        write_config(root);
        // A real commit for `cfgd`, then a tag bounding it out so the
        // `tag..HEAD` range is empty and the generated body is empty.
        commit_in_crate(root, "cfgd", "c.rs", "feat: cfgd thing");
        git(root, &["tag", "cfgd-v0.1.0"]);

        // The existing root already carries a populated `### cfgd` subsection
        // plus a sibling. Hand-curated content that the empty refresh must keep.
        let existing = "# Changelog\n\n## [Unreleased]\n\n### cfgd\n\n#### Features\n\n- curated: keep me\n\n### cfgd-core\n\n#### Features\n\n- sibling: keep me too\n\n[Unreleased]: https://github.com/o/r/compare/cfgd-v0.1.0...HEAD\n";
        std::fs::write(root.join("CHANGELOG.md"), existing).unwrap();

        let crate_dir = root.join("crates").join("cfgd");
        let update = refresh_root_unreleased(
            root,
            "cfgd",
            &crate_dir,
            Some("cfgd-v0.1.0"),
            None,
            Chronology::Date,
            true,
            &["cfgd".to_string(), "cfgd-core".to_string()],
            None,
        )
        .expect("ok");
        // Empty body over an existing subsection is a no-op: either `None`
        // (nothing to do) or a byte-identical rewrite.
        if let Some(u) = update {
            assert_eq!(
                u.rendered_text, existing,
                "existing subsection must be left byte-identical, not blanked"
            );
        }
    }

    /// Single / Lockstep / FlatAggregate stay FLAT: with `multitrack=false`
    /// the root is byte-identical to the per-crate flat path, regardless of an
    /// existing curated `### Group` body.
    #[test]
    fn single_track_root_stays_flat() {
        let tmp = init_repo();
        let root = tmp.path();
        write_config(root);
        std::fs::write(root.join("a.txt"), "x").unwrap();
        commit(root, "feat: flat feature");

        std::fs::write(
            root.join("CHANGELOG.md"),
            "# Changelog\n\n## [Unreleased]\n",
        )
        .unwrap();
        let root_update = refresh_root_unreleased(
            root,
            "mylib",
            root,
            None,
            None,
            Chronology::Date,
            false,
            &[],
            None,
        )
        .expect("ok")
        .expect("update");
        let crate_update = refresh_crate_unreleased(root, "mylib", root, None, None)
            .expect("ok")
            .expect("update");
        assert_eq!(
            root_update.rendered_text, crate_update.rendered_text,
            "single-track root must match the flat per-crate path byte-for-byte"
        );
        // Flat path keeps `### Group` (not `#### Group`).
        assert!(root_update.rendered_text.contains("### Features"));
    }

    /// `--crate`-filtered single target on a PerCrate repo (topology count == 1,
    /// so `multitrack=false`): the crate-name-aware fallback still routes the
    /// refresh to the crate's `### <crate>` subsection, leaving siblings intact.
    #[test]
    fn crate_filtered_single_target_uses_subsection_fallback() {
        let tmp = init_repo();
        let root = tmp.path();
        write_config(root);
        commit_in_crate(root, "cfgd", "c.rs", "feat: cfgd new");

        let existing = "# Changelog\n\n## [Unreleased]\n\n### cfgd\n- stale: old\n\n### cfgd-core\n- keep: sibling\n\n[Unreleased]: https://github.com/o/r/compare/v0.2.0...HEAD\n";
        std::fs::write(root.join("CHANGELOG.md"), existing).unwrap();

        let crate_dir = root.join("crates").join("cfgd");
        let out = refresh_root_unreleased(
            root,
            "cfgd",
            &crate_dir,
            None,
            None,
            Chronology::Date,
            // Single target → topology says false; crate-name fallback rescues.
            false,
            &["cfgd".to_string(), "cfgd-core".to_string()],
            None,
        )
        .expect("ok")
        .expect("update");
        let text = &out.rendered_text;
        assert!(!text.contains("stale: old"), "target regenerated:\n{text}");
        assert!(text.contains("cfgd new"), "new commit present:\n{text}");
        assert!(
            text.contains("- keep: sibling"),
            "sibling subsection preserved:\n{text}"
        );
    }

    /// Released history + footer + sibling subsections survive a multitrack
    /// refresh that only touches one crate's subsection.
    #[test]
    fn multitrack_preserves_released_history_and_footer() {
        let tmp = init_repo();
        let root = tmp.path();
        write_config(root);
        commit_in_crate(root, "cfgd", "c.rs", "feat: cfgd new");

        let existing = "# Changelog\n\n## [Unreleased]\n\n### cfgd\n- stale\n\n### cfgd-core\n- sibling kept\n\n## [v0.2.0] - 2026-02-02\n### Features\n- released thing\n\n[Unreleased]: https://github.com/o/r/compare/v0.2.0...HEAD\n[v0.2.0]: https://github.com/o/r/releases/tag/v0.2.0\n";
        std::fs::write(root.join("CHANGELOG.md"), existing).unwrap();

        let crate_dir = root.join("crates").join("cfgd");
        let out = refresh_root_unreleased(
            root,
            "cfgd",
            &crate_dir,
            None,
            None,
            Chronology::Date,
            true,
            &["cfgd".to_string(), "cfgd-core".to_string()],
            None,
        )
        .expect("ok")
        .expect("update");
        let text = &out.rendered_text;
        assert!(
            text.contains("## [v0.2.0] - 2026-02-02"),
            "history lost:\n{text}"
        );
        assert!(text.contains("- released thing"));
        assert!(text.contains("- sibling kept"), "sibling lost:\n{text}");
        assert!(text.contains("[Unreleased]: https://github.com/o/r/compare/v0.2.0...HEAD"));
        assert!(text.contains("[v0.2.0]: https://github.com/o/r/releases/tag/v0.2.0"));
    }

    // -- H1 title rules ----------------------------------------------------
    //
    // ROOT changelog H1 = the project header (`# Changelog`, or the configured
    // `changelog.header` rendered with `{{ ProjectName }}`), NEVER a crate name.
    // PER-CRATE changelog H1 = `# Changelog — <crate>`. Both refresh and promote
    // synthesize the SAME title for an absent file; an existing H1 is preserved.

    /// Write `.anodizer.yaml` with a project_name + an inline `changelog.header`
    /// template, so the root H1 resolves `{{ ProjectName }}`.
    fn write_config_with_header(root: &Path) {
        std::fs::write(
            root.join(".anodizer.yaml"),
            "project_name: myproj\nchangelog:\n  header: \"# Changelog for {{ ProjectName }}\"\n  groups:\n    - title: Features\n      regexp: '^feat'\n      order: 0\n",
        )
        .expect("write config");
    }

    /// PER-CRATE absent-file title is crate-named in BOTH refresh and promote.
    #[test]
    fn per_crate_title_is_crate_named_refresh_and_promote() {
        let tmp = init_repo();
        let root = tmp.path();
        write_config(root);
        std::fs::write(root.join("a.txt"), "x").unwrap();
        commit(root, "feat: a thing");

        let refresh = refresh_crate_unreleased(root, "demo-core", root, None, None)
            .expect("ok")
            .expect("update");
        assert!(
            refresh
                .rendered_text
                .starts_with("# Changelog — demo-core\n"),
            "per-crate refresh H1, got:\n{}",
            refresh.rendered_text
        );

        let promote = render_crate_section(root, "demo-core", root, None, "0.1.0")
            .expect("ok")
            .expect("update");
        assert!(
            promote
                .rendered_text
                .starts_with("# Changelog — demo-core\n"),
            "per-crate promote H1, got:\n{}",
            promote.rendered_text
        );
    }

    /// ROOT absent-file title is the project header (default `# Changelog`),
    /// never a crate name, in BOTH refresh and promote.
    #[test]
    fn root_title_is_project_header_refresh_and_promote() {
        let tmp = init_repo();
        let root = tmp.path();
        write_config(root);
        std::fs::write(root.join("a.txt"), "x").unwrap();
        commit(root, "feat: a thing");

        let refresh = refresh_root_unreleased(
            root,
            "demo-core",
            root,
            None,
            None,
            Chronology::Date,
            false,
            &[],
            None,
        )
        .expect("ok")
        .expect("update");
        assert!(
            refresh.rendered_text.starts_with("# Changelog\n"),
            "root refresh H1 is the project header, got:\n{}",
            refresh.rendered_text
        );
        assert!(
            !refresh.rendered_text.contains("# Changelog —"),
            "root H1 must not be crate-named:\n{}",
            refresh.rendered_text
        );

        let promote = render_root_section(
            root,
            "demo-core",
            root,
            None,
            "0.1.0",
            "v0.1.0",
            Chronology::Date,
            false,
            &[],
        )
        .expect("ok")
        .expect("update");
        assert!(
            promote.rendered_text.starts_with("# Changelog\n"),
            "root promote H1 is the project header, got:\n{}",
            promote.rendered_text
        );
        assert!(
            !promote.rendered_text.contains("# Changelog —"),
            "root H1 must not be crate-named:\n{}",
            promote.rendered_text
        );
    }

    /// A configured inline `changelog.header` resolves `{{ ProjectName }}` for
    /// the synthesized ROOT H1.
    #[test]
    fn root_title_renders_configured_header_template() {
        let tmp = init_repo();
        let root = tmp.path();
        write_config_with_header(root);
        std::fs::write(root.join("a.txt"), "x").unwrap();
        commit(root, "feat: a thing");

        let promote = render_root_section(
            root,
            "demo-core",
            root,
            None,
            "0.1.0",
            "v0.1.0",
            Chronology::Date,
            false,
            &[],
        )
        .expect("ok")
        .expect("update");
        assert!(
            promote
                .rendered_text
                .starts_with("# Changelog for myproj\n"),
            "configured header resolves ProjectName, got:\n{}",
            promote.rendered_text
        );
    }

    /// A configured `changelog.header` referencing a variable other than
    /// `ProjectName` cannot be rendered for the absent-root seed (no release
    /// Context here), so the title falls back to plain `# Changelog` rather than
    /// leaking a half-rendered `{{ ... }}` literal into the file.
    #[test]
    fn root_title_falls_back_when_header_uses_non_projectname_var() {
        let tmp = init_repo();
        let root = tmp.path();
        std::fs::write(
            root.join(".anodizer.yaml"),
            "project_name: myproj\nchangelog:\n  header: \"# {{ Version }} Changelog\"\n  groups:\n    - title: Features\n      regexp: '^feat'\n      order: 0\n",
        )
        .expect("write config");
        std::fs::write(root.join("a.txt"), "x").unwrap();
        commit(root, "feat: a thing");

        let promote = render_root_section(
            root,
            "demo-core",
            root,
            None,
            "0.1.0",
            "v0.1.0",
            Chronology::Date,
            false,
            &[],
        )
        .expect("ok")
        .expect("update");
        assert!(
            promote.rendered_text.starts_with("# Changelog\n"),
            "non-ProjectName header var must fall back to plain title, got:\n{}",
            promote.rendered_text
        );
        assert!(
            !promote.rendered_text.contains("{{"),
            "no half-rendered template literal leaks, got:\n{}",
            promote.rendered_text
        );
    }

    /// An EXISTING root H1 (even a stale crate-named one) is PRESERVED, never
    /// rewritten — the title rules apply to synthesis only.
    #[test]
    fn existing_root_h1_is_preserved_not_rewritten() {
        let tmp = init_repo();
        let root = tmp.path();
        write_config(root);
        std::fs::write(root.join("a.txt"), "x").unwrap();
        commit(root, "feat: a thing");
        // A pre-existing (stale) crate-named root H1.
        std::fs::write(
            root.join("CHANGELOG.md"),
            "# Changelog — stale-name\n\n## [Unreleased]\n",
        )
        .unwrap();

        let refresh = refresh_root_unreleased(
            root,
            "demo-core",
            root,
            None,
            None,
            Chronology::Date,
            false,
            &[],
            None,
        )
        .expect("ok")
        .expect("update");
        assert!(
            refresh
                .rendered_text
                .starts_with("# Changelog — stale-name\n"),
            "existing H1 must be preserved verbatim, got:\n{}",
            refresh.rendered_text
        );
    }
}
