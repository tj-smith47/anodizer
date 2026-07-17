use super::*;
use anodizer_core::config::{ChangelogGroup, Chronology};

#[test]
fn tag_prefix_handles_common_and_embedded_digit_schemes() {
    // Common: `v`-led and bare semver.
    assert_eq!(tag_prefix("v0.5.0"), "v");
    assert_eq!(tag_prefix("0.5.0"), "");
    // Monorepo prefix with a separator.
    assert_eq!(tag_prefix("anodizer-v0.5.0"), "anodizer-v");
    // n6: a digit *inside* the prefix must not truncate the prefix
    // (first-digit scan wrongly yielded `py`).
    assert_eq!(tag_prefix("py3-v1.2.3"), "py3-v");
    // n6: a leading-digit calendar version yields an empty prefix (all
    // calver tags cluster together).
    assert_eq!(tag_prefix("2024.01.0"), "");
    assert_eq!(tag_prefix("release-2024.01.0"), "release-");
    // Pre-release / build metadata stays out of the prefix.
    assert_eq!(tag_prefix("v1.2.3-rc.1"), "v");
}

/// Features (`^feat`) / Bug Fixes (`^fix`) groups, mirroring a typical
/// `groups:` config for the curated-bucketing tests.
fn feat_fix_groups() -> Vec<ChangelogGroup> {
    vec![
        ChangelogGroup {
            title: "Features".to_string(),
            regexp: Some("^feat".to_string()),
            order: Some(0),
            groups: None,
        },
        ChangelogGroup {
            title: "Bug Fixes".to_string(),
            regexp: Some("^fix".to_string()),
            order: Some(1),
            groups: None,
        },
    ]
}

/// Drive the pure subsection-promote transform with a fixed compare base.
fn promote(
    existing: &str,
    crate_name: &str,
    tag: &str,
    from_tag: Option<&str>,
    chronology: Chronology,
    groups: &[ChangelogGroup],
    generated_body: &str,
) -> Option<String> {
    promote_subsection(PromoteArgs {
        existing,
        crate_name,
        tag,
        from_tag,
        chronology,
        groups,
        generated_body,
        base: Some("https://github.com/tj-smith47/cfgd"),
    })
    .expect("promote_subsection succeeds")
}

const TWO_TRACK_FIXTURE: &str = "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### cfgd\n\
- feat: add `cfgd man`\n\
- fix: env scope\n\
\n\
### cfgd-core\n\
- feat: broaden spec.env\n\
\n\
## [v0.6.0] - 2026-05-28\n\
### Features\n\
- prior cfgd thing\n\
\n\
[Unreleased]: https://github.com/tj-smith47/cfgd/compare/v0.6.0...HEAD\n\
[v0.6.0]: https://github.com/tj-smith47/cfgd/compare/v0.5.0...v0.6.0\n";

#[test]
fn single_subsection_promote_curated_regrouped_date() {
    let groups = feat_fix_groups();
    let out = promote(
        TWO_TRACK_FIXTURE,
        "cfgd",
        "v0.7.0",
        Some("v0.6.0"),
        Chronology::Date,
        &groups,
        "",
    )
    .expect("some output");

    let date = today_yyyy_mm_dd();
    let expected = format!(
        "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### cfgd-core\n\
- feat: broaden spec.env\n\
\n\
## [v0.7.0] - {date}\n\
### Features\n\
- feat: add `cfgd man`\n\
### Bug Fixes\n\
- fix: env scope\n\
\n\
## [v0.6.0] - 2026-05-28\n\
### Features\n\
- prior cfgd thing\n\
\n\
[Unreleased]: https://github.com/tj-smith47/cfgd/compare/v0.7.0...HEAD\n\
[v0.7.0]: https://github.com/tj-smith47/cfgd/compare/v0.6.0...v0.7.0\n\
[v0.6.0]: https://github.com/tj-smith47/cfgd/compare/v0.5.0...v0.6.0\n"
    );
    assert_eq!(out, expected, "exact root promote output");
}

#[test]
fn other_subsections_retained_byte_identical() {
    let groups = feat_fix_groups();
    let out = promote(
        TWO_TRACK_FIXTURE,
        "cfgd",
        "v0.7.0",
        Some("v0.6.0"),
        Chronology::Date,
        &groups,
        "",
    )
    .expect("some output");

    // The non-promoted crate's subsection survives verbatim.
    assert!(
        out.contains("### cfgd-core\n- feat: broaden spec.env"),
        "cfgd-core subsection must be retained verbatim: {out}"
    );
    // The promoted crate's subsection is gone from Unreleased.
    let unreleased = out
        .split("## [v0.7.0]")
        .next()
        .expect("text before promoted section");
    assert!(
        !unreleased.contains("### cfgd\n"),
        "promoted ### cfgd subsection must be removed from Unreleased: {unreleased}"
    );
}

#[test]
fn date_slots_new_section_directly_under_unreleased() {
    let groups = feat_fix_groups();
    let out = promote(
        TWO_TRACK_FIXTURE,
        "cfgd",
        "v0.7.0",
        Some("v0.6.0"),
        Chronology::Date,
        &groups,
        "",
    )
    .expect("some output");

    let promoted = out.find("## [v0.7.0]").expect("promoted heading");
    let prior = out.find("## [v0.6.0]").expect("prior heading");
    assert!(
        promoted < prior,
        "date chronology puts today's section above older releases: {out}"
    );
}

/// Five-release reference timeline shared by the Tag/Date ordering tests.
/// Two `### crate` subsections under Unreleased so a promote keeps the
/// multi-track shape, plus four prior released sections in mixed order.
fn five_release_fixture() -> String {
    "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### cfgd\n\
- feat: new cfgd\n\
\n\
### cfgd-core\n\
- feat: new core\n\
\n\
## [core-v0.5.0] - 2026-05-20\n\
### Features\n\
- core 0.5.0\n\
\n\
## [v0.6.0] - 2026-05-10\n\
### Features\n\
- cfgd 0.6.0\n\
\n\
## [core-v0.4.0] - 2026-05-01\n\
### Features\n\
- core 0.4.0\n\
\n\
[Unreleased]: https://github.com/tj-smith47/cfgd/compare/core-v0.5.0...HEAD\n\
[core-v0.5.0]: https://github.com/tj-smith47/cfgd/compare/core-v0.4.0...core-v0.5.0\n\
[v0.6.0]: https://github.com/tj-smith47/cfgd/compare/v0.5.0...v0.6.0\n\
[core-v0.4.0]: https://github.com/tj-smith47/cfgd/releases/tag/core-v0.4.0\n"
        .to_string()
}

/// Same release set as [`five_release_fixture`], but with the existing
/// released sections already in valid `Tag` order (prefix-clustered,
/// semver-descending): `core-v0.5.0, core-v0.4.0, v0.6.0`. A `Tag`-mode
/// promote must keep that invariant after inserting the new section.
fn five_release_tag_ordered_fixture() -> String {
    "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### cfgd\n\
- feat: new cfgd\n\
\n\
### cfgd-core\n\
- feat: new core\n\
\n\
## [core-v0.5.0] - 2026-05-20\n\
### Features\n\
- core 0.5.0\n\
\n\
## [core-v0.4.0] - 2026-05-01\n\
### Features\n\
- core 0.4.0\n\
\n\
## [v0.6.0] - 2026-05-10\n\
### Features\n\
- cfgd 0.6.0\n\
\n\
[Unreleased]: https://github.com/tj-smith47/cfgd/compare/core-v0.5.0...HEAD\n\
[core-v0.5.0]: https://github.com/tj-smith47/cfgd/compare/core-v0.4.0...core-v0.5.0\n\
[core-v0.4.0]: https://github.com/tj-smith47/cfgd/releases/tag/core-v0.4.0\n\
[v0.6.0]: https://github.com/tj-smith47/cfgd/compare/v0.5.0...v0.6.0\n"
        .to_string()
}

/// Collect the ordered list of `## [<tag>]` section tags from a rendered
/// changelog (excluding `[Unreleased]`).
fn section_order(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(section_heading_tag)
        .filter(|t| !t.eq_ignore_ascii_case("unreleased"))
        .map(|s| s.to_string())
        .collect()
}

#[test]
fn tag_clusters_by_prefix_then_semver_desc() {
    let groups = feat_fix_groups();
    // Tag v0.7.0 into a Tag-ordered timeline (core-v0.5.0, core-v0.4.0,
    // v0.6.0). Tag ordering clusters `core-` (asc lexical) before `v`,
    // semver-desc within each cluster, so v0.7.0 lands at the head of the
    // `v` cluster (before v0.6.0) and after the whole `core-` cluster.
    let out = promote(
        &five_release_tag_ordered_fixture(),
        "cfgd",
        "v0.7.0",
        Some("v0.6.0"),
        Chronology::Tag,
        &groups,
        "",
    )
    .expect("some output");

    assert_eq!(
        section_order(&out),
        vec!["core-v0.5.0", "core-v0.4.0", "v0.7.0", "v0.6.0"],
        "tag chronology clusters by prefix then semver-desc: {out}"
    );
}

#[test]
fn date_orders_newest_first_distinct_from_tag() {
    let groups = feat_fix_groups();
    let out = promote(
        &five_release_fixture(),
        "cfgd",
        "v0.7.0",
        Some("v0.6.0"),
        Chronology::Date,
        &groups,
        "",
    )
    .expect("some output");

    // Date inserts today's section at the very top of the version list,
    // leaving all existing sections in their file order.
    assert_eq!(
        section_order(&out),
        vec!["v0.7.0", "core-v0.5.0", "v0.6.0", "core-v0.4.0"],
        "date chronology keeps today on top, others unchanged: {out}"
    );
}

#[test]
fn multitrack_footer_derives_tag_from_own_from_tag_not_unreleased_anchor() {
    // The shared `[Unreleased]:` anchor belongs to the `core-` track
    // (core-v0.5.0), but we tag the `v` track. The new tag and compare
    // lower-bound MUST come from this track's from_tag (v0.6.0), not the
    // anchor.
    let groups = feat_fix_groups();
    let out = promote(
        &five_release_fixture(),
        "cfgd",
        "v0.7.0",
        Some("v0.6.0"),
        Chronology::Date,
        &groups,
        "",
    )
    .expect("some output");

    assert!(
        out.contains("[Unreleased]: https://github.com/tj-smith47/cfgd/compare/v0.7.0...HEAD"),
        "Unreleased anchor must roll to this track's new tag: {out}"
    );
    assert!(
        out.contains("[v0.7.0]: https://github.com/tj-smith47/cfgd/compare/v0.6.0...v0.7.0"),
        "new compare link must use this track's from_tag, not the anchor: {out}"
    );
    assert!(
        !out.contains("core-v0.7.0"),
        "must NOT synthesize a core-prefixed tag from the shared anchor: {out}"
    );
    // Pre-existing footer links survive.
    assert!(
        out.contains("[v0.6.0]: https://github.com/tj-smith47/cfgd/compare/v0.5.0...v0.6.0"),
        "prior footer links preserved: {out}"
    );
}

#[test]
fn generated_fill_when_subsection_absent() {
    // A root with `### other` subsections under Unreleased but NO `### cfgd`
    // subsection: cfgd still gets a section from the generated body.
    let existing = "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### cfgd-core\n\
- feat: core work\n\
\n\
[Unreleased]: https://github.com/tj-smith47/cfgd/compare/v0.6.0...HEAD\n";
    let groups = feat_fix_groups();
    let generated = "### Features\n- feat: generated cfgd commit";
    let out = promote(
        existing,
        "cfgd",
        "v0.7.0",
        Some("v0.6.0"),
        Chronology::Date,
        &groups,
        generated,
    )
    .expect("some output");

    assert!(
        out.contains("## [v0.7.0] - "),
        "generated section heading present: {out}"
    );
    assert!(
        out.contains("- feat: generated cfgd commit"),
        "generated body fills the section: {out}"
    );
    // The unrelated subsection is untouched.
    assert!(
        out.contains("### cfgd-core\n- feat: core work"),
        "other subsection retained: {out}"
    );
}

#[test]
fn returns_none_when_no_subsection_and_no_commits() {
    let existing = "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### cfgd-core\n\
- feat: core work\n\
\n\
[Unreleased]: https://github.com/tj-smith47/cfgd/compare/v0.6.0...HEAD\n";
    let groups = feat_fix_groups();
    let out = promote(
        existing,
        "cfgd",
        "v0.7.0",
        Some("v0.6.0"),
        Chronology::Date,
        &groups,
        "",
    );
    assert!(
        out.is_none(),
        "no curated subsection and no generated commits → nothing to release"
    );
}

#[test]
fn idempotent_second_promote_is_noop() {
    let groups = feat_fix_groups();
    let first = promote(
        TWO_TRACK_FIXTURE,
        "cfgd",
        "v0.7.0",
        Some("v0.6.0"),
        Chronology::Date,
        &groups,
        "",
    )
    .expect("first promote");

    // Re-running with the same tag must be a no-op: the `## [v0.7.0]`
    // section already exists.
    let second = promote(
        &first,
        "cfgd",
        "v0.7.0",
        Some("v0.6.0"),
        Chronology::Date,
        &groups,
        "",
    )
    .expect("second promote");
    assert_eq!(first, second, "second promote with same tag is a no-op");
}

#[test]
fn curated_bullet_with_no_group_and_no_catchall_is_preserved() {
    let existing = "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### cfgd\n\
- feat: a feature\n\
- docs: update readme\n\
\n\
### cfgd-core\n\
- feat: core\n\
\n\
[Unreleased]: https://github.com/tj-smith47/cfgd/compare/v0.6.0...HEAD\n";
    // Only Features (^feat) configured; no catch-all. The `docs:` bullet
    // matches no group and must NOT be dropped.
    let groups = vec![ChangelogGroup {
        title: "Features".to_string(),
        regexp: Some("^feat".to_string()),
        order: Some(0),
        groups: None,
    }];
    let out = promote(
        existing,
        "cfgd",
        "v0.7.0",
        Some("v0.6.0"),
        Chronology::Date,
        &groups,
        "",
    )
    .expect("some output");

    assert!(
        out.contains("### Features\n- feat: a feature"),
        "feat bullet bucketed under Features: {out}"
    );
    assert!(
        out.contains("- docs: update readme"),
        "unmatched curated bullet must be preserved, not dropped: {out}"
    );
}

#[test]
fn wrapped_curated_bullet_continuation_stays_with_parent() {
    // A curated bullet wrapped across two lines: the continuation (indented,
    // no list marker) must follow its `feat:` parent under Features — not be
    // re-classified to the catch-all / unmatched tail under another heading.
    // A raw string keeps the 2-space continuation indent intact (a
    // `\`-continued literal would strip it).
    let existing = r#"# Changelog

## [Unreleased]

### cfgd
- feat: add a long thing
  that wraps to a second line
- fix: a quick fix

### cfgd-core
- feat: core

[Unreleased]: https://github.com/tj-smith47/cfgd/compare/v0.6.0...HEAD
"#;
    let groups = feat_fix_groups();
    let out = promote(
        existing,
        "cfgd",
        "v0.7.0",
        Some("v0.6.0"),
        Chronology::Date,
        &groups,
        "",
    )
    .expect("some output");

    // The continuation sits directly under its parent, both inside Features,
    // and the `fix:` bullet is not interleaved between them.
    assert!(
        out.contains("### Features\n- feat: add a long thing\n  that wraps to a second line\n"),
        "wrapped continuation must stay with its parent bullet under Features: {out}"
    );
    assert!(
        out.contains("### Bug Fixes\n- fix: a quick fix"),
        "the fix bullet still buckets under Bug Fixes: {out}"
    );
    // The continuation text must NOT leak to the end as an unmatched tail.
    assert!(
        !out.ends_with("  that wraps to a second line\n"),
        "continuation must not be re-classified to the unmatched tail: {out}"
    );
}

#[test]
fn no_groups_emits_curated_bullets_flat() {
    let existing = "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### cfgd\n\
- feat: a feature\n\
- fix: a fix\n\
\n\
### cfgd-core\n\
- feat: core\n\
\n\
[Unreleased]: https://github.com/tj-smith47/cfgd/compare/v0.6.0...HEAD\n";
    let out = promote(
        existing,
        "cfgd",
        "v0.7.0",
        Some("v0.6.0"),
        Chronology::Date,
        &[],
        "",
    )
    .expect("some output");

    let date = today_yyyy_mm_dd();
    // No group headings — bullets stay flat under the version heading.
    assert!(
        out.contains(&format!(
            "## [v0.7.0] - {date}\n- feat: a feature\n- fix: a fix\n"
        )),
        "no groups → flat bullets, no ### headings: {out}"
    );
}

#[test]
fn degenerate_flat_root_uses_bare_version_heading() {
    // A flat `[Unreleased]` with NO `### crate` subsections takes the flat
    // KaC roll path (bare `## [<version>]` heading, not the full tag).
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("CHANGELOG.md");
    // A genuinely flat single-track `[Unreleased]`: bare bullets, no `###`
    // group/crate subsections. This is the degenerate N=1 shape.
    std::fs::write(
        &path,
        "# Changelog\n\
\n\
## [Unreleased]\n\
- a feature\n\
\n\
[Unreleased]: https://github.com/tj-smith47/cfgd/compare/v0.6.0...HEAD\n",
    )
    .expect("write fixture");

    // No `### crate` subsection → has_crate_subsections == false.
    let existing = std::fs::read_to_string(&path).expect("read");
    assert!(
        !has_crate_subsections(&existing, &[]),
        "flat Unreleased has no crate subsections"
    );

    // The flat path emits a bare-version heading. Drive it through the
    // same merge the degenerate branch of render_root_section uses.
    let date = today_yyyy_mm_dd();
    let new_section = format!("## [0.7.0] - {date}\n\n- a feature\n");
    let merged = merge_into_changelog(MergeArgs {
        file_path: &path,
        h1: "# Changelog",
        new_section: &new_section,
        generated_body: "- a feature",
        from_tag: Some("v0.6.0"),
        to_version: "0.7.0",
        workspace_root: dir.path(),
    })
    .expect("flat merge");

    assert!(
        merged.contains(&format!("## [0.7.0] - {date}")),
        "degenerate flat root uses a bare-version heading: {merged}"
    );
    assert!(
        !merged.contains("## [v0.7.0]"),
        "flat path must NOT use the full tag in the heading: {merged}"
    );
}

#[test]
fn group_headings_under_flat_unreleased_are_not_crate_subsections() {
    // Crate-name-aware classification: a `### X` is a crate subsection IFF
    // `X` is a KNOWN crate name. A flat curated body's `### Features` /
    // `### Bug Fixes` group headings — and any foreign heading — must NEVER
    // be mistaken for a crate subsection.
    let existing = "# Changelog\n\
\n\
## [Unreleased]\n\
### Features\n\
- a feature\n\
### Bug Fixes\n\
- a fix\n\
\n\
[Unreleased]: https://github.com/tj-smith47/cfgd/compare/v0.6.0...HEAD\n";
    let crate_names = vec!["cfgd".to_string(), "cfgd-core".to_string()];
    assert!(
        !has_crate_subsections(existing, &crate_names),
        "group headings are not crate subsections (no group title is a crate name)"
    );
    // An empty crate-name set can never see a crate subsection.
    assert!(
        !has_crate_subsections(existing, &[]),
        "no known crate names → no crate subsection"
    );

    // A `### cfgd` heading IS a crate subsection when `cfgd` is a known crate.
    let multi = "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### cfgd\n\
- a thing\n\
\n\
[Unreleased]: https://github.com/tj-smith47/cfgd/compare/v0.6.0...HEAD\n";
    assert!(
        has_crate_subsections(multi, &crate_names),
        "a heading matching a known crate name is a crate subsection"
    );
}

/// R2: a foreign git-cliff/towncrier `### Added` heading under
/// `[Unreleased]` must NEVER be misclassified as a crate subsection — only a
/// KNOWN crate name qualifies.
#[test]
fn foreign_added_heading_is_not_a_crate_subsection() {
    let existing = "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### Added\n\
- some git-cliff entry\n\
\n\
### CI/CD\n\
- a pipeline change\n\
\n\
[Unreleased]: https://github.com/o/r/compare/v0.1.0...HEAD\n";
    let crate_names = vec!["cfgd".to_string(), "cfgd-core".to_string()];
    assert!(
        !has_crate_subsections(existing, &crate_names),
        "foreign `### Added`/`### CI/CD` must not read as crate subsections"
    );
}

#[test]
fn first_release_footer_points_at_release_tag_not_compare() {
    // First release of a track: `from_tag=None`. The new `[<tag>]:` link
    // must point at the release page (no 404 compare range), while the
    // rolled `[Unreleased]:` anchor still advances to this release's tag.
    let existing = "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### cfgd\n\
- feat: first ever\n\
\n\
### cfgd-core\n\
- feat: core work\n";
    let groups = feat_fix_groups();
    let out = promote(
        existing,
        "cfgd",
        "v0.7.0",
        None,
        Chronology::Date,
        &groups,
        "",
    )
    .expect("some output");

    assert!(
        out.contains("[v0.7.0]: https://github.com/tj-smith47/cfgd/releases/tag/v0.7.0"),
        "first release must link the release page, not a compare range: {out}"
    );
    assert!(
        !out.contains("compare/...v0.7.0") && !out.contains("/compare/None"),
        "first release must NOT synthesize a compare lower-bound: {out}"
    );
    assert!(
        out.contains("[Unreleased]: https://github.com/tj-smith47/cfgd/compare/v0.7.0...HEAD"),
        "Unreleased anchor must still roll to this release's tag: {out}"
    );
}

/// Initialize a fresh git repo with `user`/`email` configured and a single
/// `feat:` commit, so the commit-driven `render_root_section` branch has
/// real history. Mirrors the repo setup the existing stage tests use.
fn init_repo_with_commit(dir: &std::path::Path) {
    use std::process::Command;
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "test@example.com"],
        vec!["config", "user.name", "Test"],
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
    std::fs::write(dir.join("README.md"), "seed").expect("write seed file");
    for args in [
        vec!["add", "."],
        vec!["commit", "-q", "-m", "feat: initial work"],
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

/// Tag the current HEAD with `tag`, then add `file_change` as a fresh
/// commit, so `<tag>..HEAD` resolves to a non-empty range.
fn tag_and_commit(dir: &std::path::Path, tag: &str, message: &str) {
    use std::process::Command;
    let run = |args: &[&str]| {
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
        assert!(ok, "git {args:?} failed");
    };
    run(&["tag", tag]);
    std::fs::write(dir.join("post.txt"), "post-tag").expect("write post-tag file");
    run(&["add", "."]);
    run(&["commit", "-q", "-m", message]);
}

/// Write a minimal `.anodizer.yaml` carrying a `changelog:` block with
/// Features/Bug Fixes groups so config-load + grouping resolve.
fn write_anodizer_yaml(dir: &std::path::Path) {
    // A raw string keeps the YAML block's leading indentation intact (a
    // `\`-continued string literal would strip it and break the parse).
    let yaml = r#"changelog:
  groups:
    - title: Features
      regexp: '^feat'
      order: 0
    - title: Bug Fixes
      regexp: '^fix'
      order: 1
"#;
    std::fs::write(dir.join(".anodizer.yaml"), yaml).expect("write .anodizer.yaml");
}

/// The changelog engine's config discovery must honor the full shared
/// candidate list, not just `.anodizer.yaml` — a repo configured via
/// `anodizer.yaml` previously got a silently-degraded changelog
/// (default groups, no filters) while every CLI command honored it.
#[test]
fn changelog_config_discovered_from_non_dot_anodizer_yaml() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let yaml = r#"changelog:
  groups:
    - title: Features
      regexp: '^feat'
      order: 0
"#;
    std::fs::write(root.join("anodizer.yaml"), yaml).expect("write anodizer.yaml");

    let cfg = load_changelog_config(root)
        .expect("discovery succeeds")
        .expect("changelog block found via the shared candidate list");
    let groups = cfg.groups.expect("groups parsed");
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].title, "Features");
}

/// Write a `changelog:` block whose group title is a `{{ .ProjectName }}`
/// template, exercising the write-time template-rendering path.
fn write_anodizer_yaml_templated_group(dir: &std::path::Path) {
    let yaml = r#"changelog:
  groups:
    - title: "{{ .ProjectName }} features"
      regexp: '^feat'
      order: 0
"#;
    std::fs::write(dir.join(".anodizer.yaml"), yaml).expect("write .anodizer.yaml");
}

#[test]
fn render_crate_section_renders_templated_group_title() {
    // M1: a templated group title (`{{ .ProjectName }} features`) must be
    // rendered through the per-crate template context on the write path,
    // not shipped as a literal `{{ ... }}` into the committed CHANGELOG.md.
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo_with_commit(root);
    write_anodizer_yaml_templated_group(root);

    let update = render_crate_section(root, "myapp", root, None, "0.7.0")
        .expect("render_crate_section succeeds")
        .expect("commits present → an update is produced");

    let text = &update.rendered_text;
    assert!(
        text.contains("### myapp features"),
        "templated group title is rendered with the crate's ProjectName: {text}"
    );
    assert!(
        !text.contains("{{"),
        "no literal template braces leak into the changelog: {text}"
    );
}

#[test]
fn render_root_section_renders_templated_group_title() {
    // M1 (root write path): the per-crate context must drive the templated
    // group title on the shared root file too.
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo_with_commit(root);
    write_anodizer_yaml_templated_group(root);

    let update = render_root_section(
        root,
        "myapp",
        root,
        None,
        "0.7.0",
        "v0.7.0",
        Chronology::Date,
        false,
        &[],
    )
    .expect("render_root_section succeeds")
    .expect("commits present → an update is produced");

    let text = &update.rendered_text;
    assert!(
        text.contains("### myapp features"),
        "root templated group title is rendered with ProjectName: {text}"
    );
    assert!(
        !text.contains("{{"),
        "no literal template braces leak into the root changelog: {text}"
    );
}

#[test]
fn render_root_section_absent_file_creates_initial_root() {
    // IO branch (a): no root CHANGELOG.md yet, but the crate has commits →
    // synthesize the initial root file with a bare `## [<to_version>]`
    // first-write section.
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo_with_commit(root);
    write_anodizer_yaml(root);

    let update = render_root_section(
        root,
        "cfgd",
        root,
        None,
        "0.7.0",
        "v0.7.0",
        Chronology::Date,
        false,
        &[],
    )
    .expect("render_root_section succeeds")
    .expect("commits present → an update is produced");

    assert_eq!(
        update.file_path,
        root.join("CHANGELOG.md"),
        "writes the root file, not a per-crate file"
    );
    let text = &update.rendered_text;
    let date = today_yyyy_mm_dd();
    assert!(
        text.contains(&format!("## [0.7.0] - {date}")),
        "initial root carries a bare-version first-write heading: {text}"
    );
    // The `feat: initial work` commit is grouped under Features; the
    // default git format renders it as `<sha> initial work` (the
    // conventional-commit prefix is consumed by the group match).
    assert!(
        text.contains("### Features"),
        "the commit is grouped under its configured heading: {text}"
    );
    assert!(
        text.contains("initial work"),
        "the commit feeds the generated body: {text}"
    );
}

#[test]
fn render_root_section_degenerate_flat_uses_bare_version_heading() {
    // IO branch (b): a flat `[Unreleased]` with NO `### crate` subsections
    // (only group headings) → flat roll, bare `## [<to_version>]` heading.
    // The flat roll is commit-gated (same `render_section_body` gate as
    // `render_crate_section`), so a real commit must be present for it to
    // fire; the curated `[Unreleased]` body is then promoted verbatim.
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    // Commit, tag v0.6.0, then a NEW commit so `v0.6.0..HEAD` is non-empty
    // and the commit-gated flat roll fires.
    init_repo_with_commit(root);
    tag_and_commit(root, "v0.6.0", "feat: post-tag work");
    write_anodizer_yaml(root);
    let existing = "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### Features\n\
- a curated feature\n\
\n\
## [v0.6.0] - 2026-05-28\n\
### Features\n\
- prior thing\n\
\n\
[Unreleased]: https://github.com/tj-smith47/cfgd/compare/v0.6.0...HEAD\n\
[v0.6.0]: https://github.com/tj-smith47/cfgd/compare/v0.5.0...v0.6.0\n";
    std::fs::write(root.join("CHANGELOG.md"), existing).expect("write root");

    let update = render_root_section(
        root,
        "cfgd",
        root,
        Some("v0.6.0"),
        "0.7.0",
        "v0.7.0",
        Chronology::Date,
        false,
        &[],
    )
    .expect("render_root_section succeeds")
    .expect("curated flat Unreleased → an update is produced");

    let text = &update.rendered_text;
    let date = today_yyyy_mm_dd();
    assert!(
        text.contains(&format!("## [0.7.0] - {date}")),
        "degenerate flat root uses the bare-version heading: {text}"
    );
    assert!(
        !text.contains("## [v0.7.0]"),
        "flat path must NOT use the full tag in the heading: {text}"
    );
    // Curated content is promoted verbatim by the flat roll.
    assert!(
        text.contains("- a curated feature"),
        "curated body promoted verbatim: {text}"
    );
}

#[test]
fn render_root_section_subsection_promote_uses_full_tag_heading() {
    // IO branch (c): a real `### cfgd` subsection under `[Unreleased]` →
    // full subsection promote, `## [<tag>]` heading, footer base parsed
    // from the existing compare link (resolve_compare_base, not remote).
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    write_anodizer_yaml(root);
    // The locked two-track fixture shape: cfgd + cfgd-core subsections.
    let existing = "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### cfgd\n\
- feat: add `cfgd man`\n\
- fix: env scope\n\
\n\
### cfgd-core\n\
- feat: broaden spec.env\n\
\n\
## [v0.6.0] - 2026-05-28\n\
### Features\n\
- prior cfgd thing\n\
\n\
[Unreleased]: https://github.com/tj-smith47/cfgd/compare/v0.6.0...HEAD\n\
[v0.6.0]: https://github.com/tj-smith47/cfgd/compare/v0.5.0...v0.6.0\n";
    std::fs::write(root.join("CHANGELOG.md"), existing).expect("write root");

    let update = render_root_section(
        root,
        "cfgd",
        root,
        Some("v0.6.0"),
        "0.7.0",
        "v0.7.0",
        Chronology::Date,
        // Topology count is 1 here (single promote target); the
        // crate-name-aware fallback over the known crate set still routes to
        // the `### cfgd` subsection promote.
        false,
        &["cfgd".to_string(), "cfgd-core".to_string()],
    )
    .expect("render_root_section succeeds")
    .expect("curated ### cfgd subsection → an update is produced");

    let text = &update.rendered_text;
    let date = today_yyyy_mm_dd();
    assert_eq!(
        update.file_path,
        root.join("CHANGELOG.md"),
        "promotes into the root file"
    );
    assert!(
        text.contains(&format!("## [v0.7.0] - {date}")),
        "subsection promote uses the FULL tag heading: {text}"
    );
    // Curated bullets bucketed under Features/Bug Fixes, verbatim.
    assert!(
        text.contains("### Features\n- feat: add `cfgd man`"),
        "feat bullet bucketed under Features: {text}"
    );
    assert!(
        text.contains("### Bug Fixes\n- fix: env scope"),
        "fix bullet bucketed under Bug Fixes: {text}"
    );
    // The other crate's subsection is retained verbatim under Unreleased.
    assert!(
        text.contains("### cfgd-core\n- feat: broaden spec.env"),
        "sibling subsection retained: {text}"
    );
    // resolve_compare_base parsed the existing footer's host (no remote).
    assert!(
        text.contains("[Unreleased]: https://github.com/tj-smith47/cfgd/compare/v0.7.0...HEAD"),
        "footer base parsed from the existing compare link: {text}"
    );
    assert!(
        text.contains("[v0.7.0]: https://github.com/tj-smith47/cfgd/compare/v0.6.0...v0.7.0"),
        "new compare link derives from this track's from_tag: {text}"
    );
}

/// R1 + R5: the TOPOLOGY `multitrack=true` flag (not text inference) drives
/// the subsection-promote path even with an EMPTY crate-name list, and the
/// promoted dated section is flat (`### <Group>`, not `#### <Group>`).
#[test]
fn render_root_section_multitrack_flag_promotes_to_flat_dated_section() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    write_anodizer_yaml(root);
    let existing = "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### cfgd\n\
- feat: add thing\n\
\n\
### cfgd-core\n\
- feat: sibling thing\n\
\n\
[Unreleased]: https://github.com/o/r/compare/v0.6.0...HEAD\n";
    std::fs::write(root.join("CHANGELOG.md"), existing).expect("write root");

    let update = render_root_section(
        root,
        "cfgd",
        root,
        Some("v0.6.0"),
        "0.7.0",
        "v0.7.0",
        Chronology::Date,
        // Topology says multitrack; crate-name list intentionally empty to
        // prove the FLAG (not text inference) drives the decision.
        true,
        &[],
    )
    .expect("render_root_section succeeds")
    .expect("multitrack flag → subsection promote");

    let text = &update.rendered_text;
    let date = today_yyyy_mm_dd();
    assert!(
        text.contains(&format!("## [v0.7.0] - {date}")),
        "multitrack flag promotes to the full-tag dated heading: {text}"
    );
    // Promoted dated section is FLAT `### Features` (re-leveled), not `####`.
    assert!(
        text.contains("### Features\n- feat: add thing"),
        "promoted section is flat `### Group`: {text}"
    );
    assert!(
        !text.contains("#### Features"),
        "promoted dated section must not carry `#### Group`: {text}"
    );
    // The sibling subsection is retained under `[Unreleased]`.
    assert!(
        text.contains("### cfgd-core\n- feat: sibling thing"),
        "sibling subsection retained: {text}"
    );
}

/// R1+R2+R3+R5: a multitrack tag-promote on a root with NO `### <crate>`
/// subsection (the release-time workflow, no refresh first) must still emit a
/// tag-PREFIXED `## [<tag>]` dated section built from this crate's commits,
/// ACCUMULATE across successive promotes (both sections survive), keep a
/// `# Changelog` project title, and NOT collide two same-version tracks.
#[test]
fn render_root_section_multitrack_promote_without_subsection_accumulates() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo_with_commit(root);
    write_anodizer_yaml(root);
    // First track promote: no root file yet.
    let first = render_root_section(
        root,
        "demo-core",
        root,
        None,
        "0.2.0",
        "aaa-v0.2.0",
        Chronology::Date,
        true,
        &["demo-core".to_string(), "demo-app".to_string()],
    )
    .expect("ok")
    .expect("first promote produces a section");
    std::fs::write(root.join("CHANGELOG.md"), &first.rendered_text).expect("write root");

    let t1 = &first.rendered_text;
    assert!(
        t1.starts_with("# Changelog\n"),
        "project H1, not a crate: {t1}"
    );
    assert!(
        !t1.contains("# Changelog —"),
        "title must not carry a crate name: {t1}"
    );
    assert!(
        t1.contains("## [aaa-v0.2.0] -"),
        "tag-prefixed heading, not bare version: {t1}"
    );
    assert!(!t1.contains("## [0.2.0]"), "no bare-version heading: {t1}");

    // Second track at the SAME version (different prefix) must NOT collide.
    let second = render_root_section(
        root,
        "demo-app",
        root,
        None,
        "0.2.0",
        "v0.2.0",
        Chronology::Date,
        true,
        &["demo-core".to_string(), "demo-app".to_string()],
    )
    .expect("ok")
    .expect("second promote produces a section");
    let t2 = &second.rendered_text;
    // R3: both sections accumulate; R1: tag-prefixed, no collision.
    assert!(
        t2.contains("## [aaa-v0.2.0] -"),
        "first section survives: {t2}"
    );
    assert!(
        t2.contains("## [v0.2.0] -"),
        "second section accumulates: {t2}"
    );
    // Date chronology: newest promote on top.
    let pos_app = t2.find("## [v0.2.0]").expect("app section present");
    let pos_core = t2.find("## [aaa-v0.2.0]").expect("core section present");
    assert!(pos_app < pos_core, "date: newest (app) on top: {t2}");
}

/// R4: `tag` chronology clusters by prefix (`aaa-` before `v`), so the same
/// two same-version promotes order differently than `date` chronology.
#[test]
fn render_root_section_multitrack_tag_chronology_clusters_by_prefix() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo_with_commit(root);
    write_anodizer_yaml(root);
    let names = &["demo-core".to_string(), "demo-app".to_string()];

    let first = render_root_section(
        root,
        "demo-core",
        root,
        None,
        "0.2.0",
        "aaa-v0.2.0",
        Chronology::Tag,
        true,
        names,
    )
    .expect("ok")
    .expect("first");
    std::fs::write(root.join("CHANGELOG.md"), &first.rendered_text).expect("write");
    let second = render_root_section(
        root,
        "demo-app",
        root,
        None,
        "0.2.0",
        "v0.2.0",
        Chronology::Tag,
        true,
        names,
    )
    .expect("ok")
    .expect("second");
    let t = &second.rendered_text;
    let pos_core = t.find("## [aaa-v0.2.0]").expect("core present");
    let pos_app = t.find("## [v0.2.0]").expect("app present");
    // Tag: `aaa-` cluster sorts before the `v` cluster.
    assert!(
        pos_core < pos_app,
        "tag: aaa- cluster before v cluster: {t}"
    );
}

/// R6: re-promoting a tag whose `## [<tag>]` already exists is a no-op.
#[test]
fn render_root_section_multitrack_promote_is_idempotent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo_with_commit(root);
    write_anodizer_yaml(root);
    let names = &["demo-core".to_string(), "demo-app".to_string()];

    let first = render_root_section(
        root,
        "demo-core",
        root,
        None,
        "0.2.0",
        "aaa-v0.2.0",
        Chronology::Date,
        true,
        names,
    )
    .expect("ok")
    .expect("first");
    std::fs::write(root.join("CHANGELOG.md"), &first.rendered_text).expect("write");
    let again = render_root_section(
        root,
        "demo-core",
        root,
        None,
        "0.2.0",
        "aaa-v0.2.0",
        Chronology::Date,
        true,
        names,
    )
    .expect("ok");
    // Either no update, or a byte-identical one.
    if let Some(u) = again {
        assert_eq!(
            u.rendered_text, first.rendered_text,
            "re-promote of an existing tag must be a no-op"
        );
    }
}

/// Write `file` under `crates/<crate>/`, commit it with `subject`, so a
/// path-scoped changelog fetch attributes the commit to that crate only.
fn commit_in_crate(root: &std::path::Path, crate_dir: &str, file: &str, subject: &str) {
    use std::process::Command;
    let dir = root.join("crates").join(crate_dir);
    std::fs::create_dir_all(&dir).expect("mkdir crate dir");
    std::fs::write(dir.join(file), subject).expect("write crate file");
    for args in [vec!["add", "."], vec!["commit", "-q", "-m", subject]] {
        let ok = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(&args).current_dir(root);
                cmd
            },
            "git",
        )
        .status
        .success();
        assert!(ok, "git {args:?} failed");
    }
}

/// R2 (body provenance / no cross-track leakage — the headline live bug):
/// each promoted section must carry ONLY its own track's commit. Distinct
/// per-crate commit ranges (core gets `alpha`, app gets `beta`) prove the
/// section body is path-scoped to the crate, not shared across tracks.
#[test]
fn render_root_section_multitrack_body_is_per_crate_no_leakage() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo_with_commit(root);
    write_anodizer_yaml(root);
    let core_dir = root.join("crates").join("core");
    let app_dir = root.join("crates").join("app");
    let names = &["demo-core".to_string(), "demo-app".to_string()];

    // Crate-distinct commits, each touching only its own crate path.
    commit_in_crate(root, "core", "lib.rs", "feat(core): alpha");
    commit_in_crate(root, "app", "main.rs", "feat(app): beta");

    // Promote core from its OWN path: body must hold `alpha`, never `beta`.
    let core = render_root_section(
        root,
        "demo-core",
        &core_dir,
        None,
        "0.2.0",
        "aaa-v0.2.0",
        Chronology::Date,
        true,
        names,
    )
    .expect("ok")
    .expect("core promote");
    std::fs::write(root.join("CHANGELOG.md"), &core.rendered_text).expect("write");

    // Promote app from its OWN path: body must hold `beta`, never `alpha`.
    let app = render_root_section(
        root,
        "demo-app",
        &app_dir,
        None,
        "0.2.0",
        "v0.2.0",
        Chronology::Date,
        true,
        names,
    )
    .expect("ok")
    .expect("app promote");
    let text = &app.rendered_text;

    // Isolate each dated section's body to assert provenance per-section.
    let core_section = section_body(text, "## [aaa-v0.2.0]");
    let app_section = section_body(text, "## [v0.2.0]");
    assert!(
        core_section.contains("alpha") && !core_section.contains("beta"),
        "core section must hold ONLY alpha (no cross-track leakage):\n{core_section}"
    );
    assert!(
        app_section.contains("beta") && !app_section.contains("alpha"),
        "app section must hold ONLY beta (no cross-track leakage):\n{app_section}"
    );
}

/// Slice the body of the dated section opened by `heading` up to the next
/// `## ` heading (or end), so a per-section provenance assertion can't be
/// satisfied by a sibling section's bullet.
fn section_body(text: &str, heading: &str) -> String {
    let start = text.find(heading).expect("section heading present");
    let after = &text[start + heading.len()..];
    let end = after.find("\n## ").map(|i| i + 1).unwrap_or(after.len());
    after[..end].to_string()
}
