use super::*;
use anodizer_core::config::Chronology;

/// Minimal `CommitInfo` builder for the pure-helper tests below; only the
/// fields the JSON / author projections read are populated.
fn commit(description: &str, hash: &str, full_hash: &str) -> CommitInfo {
    CommitInfo {
        description: description.into(),
        hash: hash.into(),
        full_hash: full_hash.into(),
        ..Default::default()
    }
}

// ---- style_login_mentions ----

/// The sentinel-framed span the renderer substitutes for `AuthorUsername`.
fn span(login: &str) -> String {
    format!("{MENTION_SENTINEL}@{login}{MENTION_SENTINEL}")
}

/// Only the sentinel-framed span is styled; identical-looking free text
/// (a coincidental `@login` in the commit subject) is untouched in BOTH
/// styles, because it carries no sentinel.
#[test]
fn style_login_mentions_styles_only_the_sentinel_span() {
    let line = format!("remove @ada usage ({})", span("ada"));
    assert_eq!(
        style_login_mentions(&line, "ada", LoginStyle::Linked),
        "remove @ada usage ([@ada](https://github.com/ada))"
    );
    assert_eq!(
        style_login_mentions(&line, "ada", LoginStyle::Bare),
        "remove @ada usage (@ada)"
    );
}

/// A template-supplied literal `@` directly before the span is consumed
/// (the GR-ported `@{{ .AuthorUsername }}` shape), in both styles.
#[test]
fn style_login_mentions_collapses_template_at_before_span() {
    let line = format!("x (@{})", span("ada"));
    assert_eq!(
        style_login_mentions(&line, "ada", LoginStyle::Bare),
        "x (@ada)"
    );
    assert_eq!(
        style_login_mentions(&line, "ada", LoginStyle::Linked),
        "x ([@ada](https://github.com/ada))"
    );
}

/// A mangled span (a template filter ate one sentinel) degrades to its
/// unstyled text — the sentinel itself must never reach the output.
#[test]
fn style_login_mentions_never_leaks_sentinels() {
    let half = format!("{MENTION_SENTINEL}@ADA");
    let line = format!("x ({half})");
    for style in [LoginStyle::Bare, LoginStyle::Linked] {
        let out = style_login_mentions(&line, "ada", style);
        assert!(
            !out.contains(MENTION_SENTINEL),
            "sentinel leaked ({style:?}): {out:?}"
        );
        assert_eq!(out, "x (@ADA)");
    }
}

// ---- strip_mention_sentinels ----

/// Clean input borrows (no allocation, byte-identical); dirty input has
/// every sentinel removed.
#[test]
fn strip_mention_sentinels_borrows_clean_and_cleans_dirty() {
    assert!(matches!(
        strip_mention_sentinels("plain text"),
        std::borrow::Cow::Borrowed("plain text")
    ));
    let dirty = format!("a{MENTION_SENTINEL}b{MENTION_SENTINEL}c");
    assert_eq!(strip_mention_sentinels(&dirty), "abc");
}

// ---- collect_all_logins ----

/// `AllLogins` derives from per-commit logins when the caller supplies no
/// fetch-time login string (the enriched local-git path): unique, sorted,
/// comma-joined like the SCM fetchers produce.
#[test]
fn collect_all_logins_unique_sorted() {
    let mut a = commit("feat: a", "a1", "a1full");
    a.login = "zoe".into();
    let mut b = commit("fix: b", "b2", "b2full");
    b.login = "ada".into();
    let mut c = commit("fix: c", "c3", "c3full");
    c.login = "zoe".into();
    let grouped = vec![GroupedCommits {
        title: String::new(),
        commits: vec![a, b, c],
        subgroups: Vec::new(),
    }];
    assert_eq!(collect_all_logins(&grouped), "ada,zoe");
}

// ---- collect_all_authors ----

#[test]
fn collect_all_authors_dedupes_and_sorts_across_subgroups() {
    let mut c1 = commit("a", "h1", "h1");
    c1.author_name = "Charlie".into();
    c1.co_authors = vec!["Alice".into()];
    let mut c2 = commit("b", "h2", "h2");
    c2.author_name = "Bob".into();
    let mut c3 = commit("c", "h3", "h3");
    // Duplicate author + an empty entry that must be skipped.
    c3.author_name = "Alice".into();
    c3.co_authors = vec![String::new()];

    let parent = GroupedCommits {
        title: "Features".into(),
        commits: vec![c1, c2],
        subgroups: vec![GroupedCommits::new("Nested", vec![c3])],
    };
    // BTreeSet => alphabetical, deduped (Alice once).
    assert_eq!(collect_all_authors(&[parent]), "Alice, Bob, Charlie");
}

#[test]
fn collect_all_authors_empty_when_no_names() {
    let g = GroupedCommits::new("Features", vec![commit("a", "h", "h")]);
    assert_eq!(collect_all_authors(&[g]), "");
}

// ---- group_to_json ----

#[test]
fn group_to_json_projects_entries_authors_and_subgroups() {
    let mut c = commit("add thing", "abc1234", "abc1234deadbeef");
    c.author_name = "Author One".into();
    c.co_authors = vec!["Co Two".into(), String::new()];
    let child = GroupedCommits::new("Sub", vec![commit("nested", "def", "deffull")]);
    let group = GroupedCommits {
        title: "Features".into(),
        commits: vec![c],
        subgroups: vec![child],
    };

    let json = group_to_json(&group);
    let value = serde_json::to_value(&json).expect("serialize JsonGroup");
    assert_eq!(value["title"], "Features");
    assert_eq!(value["entries"][0]["summary"], "add thing");
    assert_eq!(value["entries"][0]["sha"], "abc1234");
    assert_eq!(value["entries"][0]["full_sha"], "abc1234deadbeef");
    // Primary author then non-empty co-author; the empty co-author is dropped.
    assert_eq!(
        value["entries"][0]["authors"],
        serde_json::json!(["Author One", "Co Two"])
    );
    assert_eq!(value["subgroups"][0]["title"], "Sub");
    assert_eq!(value["subgroups"][0]["entries"][0]["sha"], "def");
}

#[test]
fn group_to_json_entry_with_no_author_yields_empty_authors() {
    let group = GroupedCommits::new("X", vec![commit("only summary", "h", "h")]);
    let value = serde_json::to_value(group_to_json(&group)).expect("serialize");
    assert_eq!(value["entries"][0]["authors"], serde_json::json!([]));
}

// ---- H1 / skeleton / subsection shaping ----

#[test]
fn crate_h1_carries_crate_name() {
    assert_eq!(crate_h1("anodizer-core"), "# Changelog — anodizer-core");
}

#[test]
fn kac_skeleton_empty_body_collapses_section() {
    assert_eq!(
        kac_skeleton("# Changelog", ""),
        "# Changelog\n\n## [Unreleased]\n"
    );
}

#[test]
fn kac_skeleton_non_empty_body_fences_with_blanks() {
    assert_eq!(
        kac_skeleton("# Changelog — core", "### Features\n- feat: x"),
        "# Changelog — core\n\n## [Unreleased]\n\n### Features\n- feat: x\n"
    );
}

#[test]
fn wrap_subsection_empty_body_emits_nothing() {
    assert_eq!(wrap_subsection("cfgd", ""), "");
}

#[test]
fn wrap_subsection_wraps_body_under_crate_heading() {
    assert_eq!(
        wrap_subsection("cfgd", "#### Features\n- a"),
        "### cfgd\n\n#### Features\n- a"
    );
}

#[test]
fn demote_group_headings_relevels_only_exact_h3() {
    let body = "### Features\n- feat: a\n#### Already deep\n### Bug Fixes\n- fix: b";
    assert_eq!(
        demote_group_headings(body),
        "#### Features\n- feat: a\n#### Already deep\n#### Bug Fixes\n- fix: b"
    );
}

#[test]
fn build_promoted_section_shapes_dated_heading_and_trailing_blank() {
    let date = today_yyyy_mm_dd();
    let promoted = build_promoted_section("v0.7.0", "### Features\n- feat: x");
    assert_eq!(
        promoted,
        vec![
            format!("## [v0.7.0] - {date}"),
            "### Features".to_string(),
            "- feat: x".to_string(),
            String::new(),
        ]
    );
}

#[test]
fn today_yyyy_mm_dd_is_iso_shaped() {
    let date = today_yyyy_mm_dd();
    let parts: Vec<&str> = date.split('-').collect();
    assert_eq!(parts.len(), 3, "YYYY-MM-DD has three dash-joined parts");
    assert_eq!(parts[0].len(), 4);
    assert_eq!(parts[1].len(), 2);
    assert_eq!(parts[2].len(), 2);
    assert!(date.chars().all(|c| c.is_ascii_digit() || c == '-'));
}

// ---- heading / footer predicates ----

#[test]
fn is_code_fence_recognizes_backtick_tilde_and_indented() {
    assert!(is_code_fence("```rust"));
    assert!(is_code_fence("~~~"));
    assert!(is_code_fence("    ```"));
    assert!(!is_code_fence("not a fence"));
    assert!(!is_code_fence("`inline`"));
}

#[test]
fn strip_list_marker_handles_dash_star_and_continuation() {
    assert_eq!(strip_list_marker("- feat: x"), Some("feat: x"));
    assert_eq!(strip_list_marker("  * fix: y"), Some("fix: y"));
    // No marker => continuation line => None.
    assert_eq!(strip_list_marker("  wrapped text"), None);
    assert_eq!(strip_list_marker("plain"), None);
}

#[test]
fn section_heading_tag_extracts_bracketed_tag() {
    assert_eq!(
        section_heading_tag("## [v0.7.0] - 2026-05-28"),
        Some("v0.7.0")
    );
    assert_eq!(section_heading_tag("## [Unreleased]"), Some("Unreleased"));
    assert_eq!(section_heading_tag("## plain heading"), None);
    assert_eq!(section_heading_tag("### [v1.0.0]"), None);
}

#[test]
fn is_unreleased_heading_is_case_insensitive() {
    assert!(is_unreleased_heading("## [Unreleased]"));
    assert!(is_unreleased_heading("##   [UNRELEASED]   "));
    assert!(!is_unreleased_heading("## [v0.1.0]"));
    assert!(!is_unreleased_heading("### [Unreleased]"));
}

#[test]
fn is_section_heading_requires_h2_with_space() {
    assert!(is_section_heading("## [v0.1.0]"));
    assert!(!is_section_heading("### sub"));
    assert!(!is_section_heading("##no-space"));
    assert!(!is_section_heading("# H1"));
}

#[test]
fn is_version_heading_matches_exact_version_only() {
    assert!(is_version_heading("## [0.6.0] - 2026-01-01", "0.6.0"));
    assert!(is_version_heading("## [0.6.0]", "0.6.0"));
    assert!(!is_version_heading("## [0.6.0]", "0.6.1"));
    assert!(!is_version_heading("## [Unreleased]", "0.6.0"));
    assert!(!is_version_heading("plain", "0.6.0"));
}

#[test]
fn is_subsection_heading_matches_h3_name_not_h4() {
    assert_eq!(is_subsection_heading("### cfgd"), Some("cfgd"));
    assert_eq!(is_subsection_heading("### cfgd-core  "), Some("cfgd-core"));
    // Exactly three hashes: an H4 is not a crate subsection.
    assert_eq!(is_subsection_heading("#### Features"), None);
    assert_eq!(is_subsection_heading("### "), None);
    assert_eq!(is_subsection_heading("## [Unreleased]"), None);
}

#[test]
fn parse_unreleased_footer_returns_url_case_insensitive() {
    assert_eq!(
        parse_unreleased_footer("[Unreleased]: https://x/compare/v1...HEAD"),
        Some("https://x/compare/v1...HEAD")
    );
    assert_eq!(
        parse_unreleased_footer("  [unreleased]:  https://y  "),
        Some("https://y")
    );
    assert_eq!(parse_unreleased_footer("[v1.0.0]: https://z"), None);
    assert_eq!(parse_unreleased_footer("not a footer"), None);
}

#[test]
fn parse_compare_url_splits_base_and_anchor() {
    assert_eq!(
        parse_compare_url("https://github.com/o/r/compare/v0.6.0...HEAD"),
        Some(("https://github.com/o/r", "v0.6.0"))
    );
    // Missing `...HEAD` suffix.
    assert_eq!(
        parse_compare_url("https://github.com/o/r/compare/v0.6.0"),
        None
    );
    // Empty anchor.
    assert_eq!(parse_compare_url("https://x/compare/...HEAD"), None);
    // No `/compare/` segment.
    assert_eq!(parse_compare_url("https://x/releases/tag/v1"), None);
}

// ---- has_crate_subsections (classification) ----

#[test]
fn has_crate_subsections_true_only_for_known_crate_h3() {
    let names = vec!["cfgd".to_string(), "cfgd-core".to_string()];
    let multitrack =
        "# Changelog\n\n## [Unreleased]\n\n### cfgd\n- feat: a\n\n### cfgd-core\n- feat: b\n";
    assert!(has_crate_subsections(multitrack, &names));

    // Foreign curated H3 (### Added) is NOT a crate subsection.
    let flat = "# Changelog\n\n## [Unreleased]\n\n### Added\n- a thing\n";
    assert!(!has_crate_subsections(flat, &names));

    // No `[Unreleased]` heading at all.
    let no_unrel = "# Changelog\n\n## [v0.1.0]\n### cfgd\n- x\n";
    assert!(!has_crate_subsections(no_unrel, &names));
}

#[test]
fn has_crate_subsections_stops_at_section_boundary() {
    // The `### cfgd` lives in a released section, not under [Unreleased];
    // the scan must stop at the first `## ` boundary and return false.
    let names = vec!["cfgd".to_string()];
    let text = "# Changelog\n\n## [Unreleased]\n\n## [v0.1.0]\n### cfgd\n- old\n";
    assert!(!has_crate_subsections(text, &names));
}

// ---- bucket_curated_bullets ----

fn feat_fix() -> Vec<ChangelogGroup> {
    vec![
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
    ]
}

#[test]
fn bucket_curated_bullets_no_groups_joins_verbatim() {
    let curated = vec!["- feat: a", "- fix: b"];
    assert_eq!(
        bucket_curated_bullets(&curated, &[]).unwrap(),
        "- feat: a\n- fix: b"
    );
}

#[test]
fn bucket_curated_bullets_groups_in_order_with_headings() {
    let groups = feat_fix();
    let curated = vec!["- fix: env scope", "- feat: add man"];
    // Emitted in config `order`: Features (0) before Bug Fixes (1),
    // regardless of bullet order.
    assert_eq!(
        bucket_curated_bullets(&curated, &groups).unwrap(),
        "### Features\n- feat: add man\n### Bug Fixes\n- fix: env scope"
    );
}

#[test]
fn bucket_curated_bullets_continuation_stays_with_parent() {
    let groups = feat_fix();
    // The wrapped second line (no marker) belongs to the feat bullet.
    let curated = vec!["- feat: add man", "  with detail", "- fix: bug"];
    assert_eq!(
        bucket_curated_bullets(&curated, &groups).unwrap(),
        "### Features\n- feat: add man\n  with detail\n### Bug Fixes\n- fix: bug"
    );
}

#[test]
fn bucket_curated_bullets_unmatched_preserved_at_end_without_heading() {
    let groups = feat_fix();
    // `chore:` matches no group and there is no catch-all => preserved
    // at the end under no heading.
    let curated = vec!["- feat: a", "- chore: deps"];
    assert_eq!(
        bucket_curated_bullets(&curated, &groups).unwrap(),
        "### Features\n- feat: a\n- chore: deps"
    );
}

#[test]
fn bucket_curated_bullets_empty_regexp_is_catch_all_not_greedy_regex() {
    // A group with `regexp: ""` is the catch-all (equivalent to omitting
    // `regexp`): it collects only bullets that no specific group matched,
    // rather than greedily swallowing everything as a literal
    // `Regex::new("")` (which matches every string) would. With the
    // catch-all last, `feat:` stays in Features and only `chore:` falls
    // through to the empty-regexp group.
    let groups = vec![
        ChangelogGroup {
            title: "Features".into(),
            regexp: Some("^feat".into()),
            order: Some(0),
            groups: None,
        },
        ChangelogGroup {
            title: "Other".into(),
            regexp: Some(String::new()),
            order: Some(1),
            groups: None,
        },
    ];
    let curated = vec!["- feat: add man", "- chore: deps"];
    assert_eq!(
        bucket_curated_bullets(&curated, &groups).unwrap(),
        "### Features\n- feat: add man\n### Other\n- chore: deps"
    );
}

// ---- tag_insert_index (chronology slotting) ----

#[test]
fn tag_insert_index_same_prefix_inserts_before_lower_semver() {
    let sections = vec![
        "## [v0.6.0] - 2026-01-01",
        "- old",
        "## [v0.5.0]",
        "- older",
    ];
    let heading_idxs = vec![0usize, 2usize];
    // v0.7.0 > v0.6.0 => insert before index 0.
    assert_eq!(tag_insert_index(&sections, &heading_idxs, "v0.7.0"), 0);
    // v0.5.5 is between the two => insert before v0.5.0 (index 2).
    assert_eq!(tag_insert_index(&sections, &heading_idxs, "v0.5.5"), 2);
    // v0.1.0 is below all => append at end.
    assert_eq!(
        tag_insert_index(&sections, &heading_idxs, "v0.1.0"),
        sections.len()
    );
}

#[test]
fn tag_insert_index_clusters_by_prefix() {
    // Distinct prefixes cluster: `aaa-v` < `v` lexically, so an `aaa-v`
    // tag slots before a `v`-prefixed section.
    let sections = vec!["## [v0.2.0]", "- x"];
    let heading_idxs = vec![0usize];
    assert_eq!(tag_insert_index(&sections, &heading_idxs, "aaa-v0.3.0"), 0);
}

// ---- push_compare_footer / push_root_footer / synthesize_footer ----

#[test]
fn push_compare_footer_emits_two_link_lines() {
    let mut out: Vec<String> = Vec::new();
    push_compare_footer(&mut out, "https://x/o/r", "v0.5.0", "v0.6.0", "0.6.0");
    assert_eq!(
        out,
        vec![
            "[Unreleased]: https://x/o/r/compare/v0.6.0...HEAD".to_string(),
            "[0.6.0]: https://x/o/r/compare/v0.5.0...v0.6.0".to_string(),
        ]
    );
}

#[test]
fn push_root_footer_first_release_points_at_release_tag() {
    let mut out: Vec<String> = vec!["- body".to_string()];
    push_root_footer(&mut out, &[], "v0.1.0", None, Some("https://x/o/r"));
    assert_eq!(
        out,
        vec![
            "- body".to_string(),
            String::new(),
            "[Unreleased]: https://x/o/r/compare/v0.1.0...HEAD".to_string(),
            "[v0.1.0]: https://x/o/r/releases/tag/v0.1.0".to_string(),
        ]
    );
}

#[test]
fn push_root_footer_subsequent_release_uses_compare_and_drops_old_unreleased() {
    let mut out: Vec<String> = vec!["- body".to_string()];
    let footer = vec![
        "[Unreleased]: https://x/o/r/compare/v0.5.0...HEAD",
        "[v0.5.0]: https://x/o/r/compare/v0.4.0...v0.5.0",
    ];
    push_root_footer(
        &mut out,
        &footer,
        "v0.6.0",
        Some("v0.5.0"),
        Some("https://x/o/r"),
    );
    assert_eq!(
        out,
        vec![
            "- body".to_string(),
            String::new(),
            "[Unreleased]: https://x/o/r/compare/v0.6.0...HEAD".to_string(),
            "[v0.6.0]: https://x/o/r/compare/v0.5.0...v0.6.0".to_string(),
            "[v0.5.0]: https://x/o/r/compare/v0.4.0...v0.5.0".to_string(),
        ]
    );
}

#[test]
fn push_root_footer_without_base_keeps_existing_verbatim() {
    let mut out: Vec<String> = Vec::new();
    let footer = vec!["[Unreleased]: https://x/compare/v1...HEAD"];
    push_root_footer(&mut out, &footer, "v2", Some("v1"), None);
    assert_eq!(
        out,
        vec!["[Unreleased]: https://x/compare/v1...HEAD".to_string()]
    );
}

// ---- build_section_template_vars / render_field ----

#[test]
fn render_field_renders_section_vars() {
    let vars = build_section_template_vars(SectionVars {
        crate_name: "cfgd-core",
        version: "0.6.0",
        tag: "cfgd-core-v0.6.0",
    });
    let log = StageLogger::new("test", Verbosity::Normal);
    assert_eq!(
        render_field("{{ .Name }} {{ .Version }}", &vars, "group title", &log),
        "cfgd-core 0.6.0"
    );
    assert_eq!(
        render_field("{{ .Tag }}", &vars, "group title", &log),
        "cfgd-core-v0.6.0"
    );
    assert_eq!(
        render_field("{{ .ProjectName }}", &vars, "group title", &log),
        "cfgd-core"
    );
}

#[test]
fn render_field_falls_back_to_raw_and_warns_on_render_error() {
    let vars = build_section_template_vars(SectionVars {
        crate_name: "x",
        version: "1.0.0",
        tag: "v1.0.0",
    });
    let capture = anodizer_core::log::LogCapture::new();
    let log = StageLogger::new("test", Verbosity::Normal).with_capture_handle(capture.clone());

    // Malformed template => kept verbatim (non-strict write path), but the
    // fallback must NOT be silent: a warn naming the field + template proves
    // the failure surfaces instead of a literal `{{ … }}` shipping unnoticed.
    let raw = "{{ .Unterminated ";
    assert_eq!(render_field(raw, &vars, "group title", &log), raw);

    let warns = capture.warn_messages();
    assert_eq!(
        warns.len(),
        1,
        "malformed template must emit exactly one warn, got: {warns:?}"
    );
    assert!(
        warns[0].contains("group title") && warns[0].contains(".Unterminated"),
        "warn must name the field kind and the offending template, got: {warns:?}"
    );
}

// ---- render_groups depth cap ----

#[test]
fn render_groups_caps_heading_depth_at_six() {
    // A title at depth 7 must be silently dropped (Markdown max is `######`).
    let state = RenderGroupsState {
        abbrev: 7,
        tmpl: "{{ Message }}",
        logins: "",
        all_authors: "",
        divider: None,
        newline: "\n",
        login_style: LoginStyle::Bare,
    };
    let groups = vec![GroupedCommits::new(
        "TooDeep",
        vec![commit("desc", "h", "h")],
    )];
    let mut out = String::new();
    render_groups(&mut out, &groups, &state, 7).expect("render_groups ok");
    assert!(out.is_empty(), "depth > 6 renders nothing");
}

#[test]
fn render_groups_emits_heading_at_configured_depth() {
    let state = RenderGroupsState {
        abbrev: 7,
        tmpl: "{{ Message }}",
        logins: "",
        all_authors: "",
        divider: None,
        newline: "\n",
        login_style: LoginStyle::Bare,
    };
    let groups = vec![GroupedCommits::new(
        "Features",
        vec![commit("add x", "h", "h")],
    )];
    let mut out = String::new();
    render_groups(&mut out, &groups, &state, 3).expect("render_groups ok");
    assert_eq!(out, "### Features\n\n* add x\n\n");
}

// ---- splice_after_h1 (non-KAC merge path) ----

#[test]
fn splice_after_h1_inserts_section_below_existing_h1() {
    let existing = "# Changelog\n\n## [v0.1.0]\n- old\n";
    let out =
        splice_after_h1(existing, "## [v0.2.0]\n- new\n", "# Changelog\n\n").expect("splice ok");
    assert_eq!(
        out,
        "# Changelog\n\n## [v0.2.0]\n- new\n\n## [v0.1.0]\n- old\n"
    );
}

#[test]
fn splice_after_h1_synthesizes_h1_when_absent() {
    let existing = "- loose note\n";
    let out =
        splice_after_h1(existing, "## [v0.2.0]\n- new\n", "# Changelog\n\n").expect("splice ok");
    assert_eq!(out, "# Changelog\n\n## [v0.2.0]\n- new\n\n- loose note\n");
}

// ---- replace_unreleased_body (flat KAC refresh) ----

#[test]
fn replace_unreleased_body_absent_file_yields_skeleton() {
    let out = replace_unreleased_body(None, "# Changelog", "### Features\n- feat: x");
    assert_eq!(
        out,
        "# Changelog\n\n## [Unreleased]\n\n### Features\n- feat: x\n"
    );
}

#[test]
fn replace_unreleased_body_swaps_block_preserving_released_and_footer() {
    let existing = "# Changelog\n\n## [Unreleased]\n\n### Features\n- old body\n\n## [v0.1.0] - 2026-01-01\n- prior\n\n[Unreleased]: https://x/compare/v0.1.0...HEAD\n";
    let out = replace_unreleased_body(Some(existing), "# Changelog", "### Bug Fixes\n- fix: y");
    assert_eq!(
        out,
        "# Changelog\n\n## [Unreleased]\n\n### Bug Fixes\n- fix: y\n\n## [v0.1.0] - 2026-01-01\n- prior\n\n[Unreleased]: https://x/compare/v0.1.0...HEAD\n"
    );
}

// ---- finish (blank collapse, fence-aware) ----

#[test]
fn finish_collapses_blank_runs_and_trims_trailing() {
    let lines = vec![
        "# H".to_string(),
        String::new(),
        String::new(),
        String::new(),
        "body".to_string(),
        String::new(),
        String::new(),
    ];
    // 3 blanks collapse to 1; trailing blanks dropped; trailing newline restored.
    assert_eq!(finish(lines, true), "# H\n\nbody\n");
}

#[test]
fn finish_preserves_blank_lines_inside_code_fence() {
    let lines = vec![
        "```rust".to_string(),
        "let a = 1;".to_string(),
        String::new(),
        String::new(),
        "let b = 2;".to_string(),
        "```".to_string(),
    ];
    // Interior fence blanks are NOT collapsed.
    assert_eq!(
        finish(lines, false),
        "```rust\nlet a = 1;\n\n\nlet b = 2;\n```"
    );
}

// ---- slot_sections (chronology ordering) ----

#[test]
fn slot_sections_date_chronology_prepends_newest() {
    let sections = vec!["## [v0.5.0] - 2026-01-01", "- old"];
    let promoted = vec!["## [v0.6.0] - 2026-02-01".to_string(), "- new".to_string()];
    let out = slot_sections(&sections, &promoted, "v0.6.0", Chronology::Date);
    assert_eq!(
        out,
        vec![
            "## [v0.6.0] - 2026-02-01".to_string(),
            "- new".to_string(),
            "## [v0.5.0] - 2026-01-01".to_string(),
            "- old".to_string(),
        ]
    );
}
