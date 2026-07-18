use super::*;

// ---- Push resolution tests ----

/// Build a `TagOpts` carrying only the two push toggles under test;
/// everything else is left at its inert default.
fn push_opts(push: bool, no_push: bool) -> TagOpts {
    TagOpts {
        dry_run: false,
        custom_tag: None,
        version_override: None,
        default_bump: None,
        crate_name: None,
        push,
        no_push,
        push_tags_only: false,
        sign: false,
        no_sign: false,
        push_remote: None,
        push_dry_run: false,
        changelog: false,
        config_override: None,
        verbose: false,
        debug: false,
        quiet: false,
        strict: false,
    }
}

#[test]
fn resolve_effective_push_matrix() {
    // (push, no_push, config_push) -> expected. Every dispatch shape shares
    // this one resolution: fully local unless a push is explicitly asked for.
    let cases: &[(bool, bool, Option<bool>, bool)] = &[
        // --no-push wins over everything.
        (false, true, Some(true), false),
        (true, true, Some(true), false), // (clap forbids push+no_push, but the resolver must still be safe)
        (false, true, None, false),
        // --push forces a branch push.
        (true, false, None, true),
        // config push=true forces a branch push.
        (false, false, Some(true), true),
        // config push=false is inert; the local default stands.
        (false, false, Some(false), false),
        // No signal: fully local.
        (false, false, None, false),
    ];
    for &(push, no_push, config_push, expected) in cases {
        let opts = push_opts(push, no_push);
        assert_eq!(
            resolve_effective_push(&opts, config_push),
            expected,
            "push={push} no_push={no_push} config_push={config_push:?}"
        );
    }
}

#[test]
fn resolve_effective_sign_matrix() {
    // (sign, no_sign, config_sign) -> expected. Signing is opt-in and
    // workspace-global: --no-sign always wins, then --sign or
    // tag.sign=true selects a signed tag; otherwise the tag is unsigned.
    let cases: &[(bool, bool, Option<bool>, bool)] = &[
        // --no-sign wins over everything.
        (true, true, Some(true), false),
        (false, true, Some(true), false),
        (false, true, None, false),
        // --sign forces a signed tag.
        (true, false, None, true),
        // config sign=true forces a signed tag.
        (false, false, Some(true), true),
        // config sign=false is inert; the unsigned default stands.
        (false, false, Some(false), false),
        // No signal: unsigned.
        (false, false, None, false),
    ];
    for &(sign, no_sign, config_sign, expected) in cases {
        let mut opts = push_opts(false, false);
        opts.sign = sign;
        opts.no_sign = no_sign;
        assert_eq!(
            resolve_effective_sign(&opts, config_sign),
            expected,
            "sign={sign} no_sign={no_sign} config_sign={config_sign:?}"
        );
    }
}

// ---- Bump detection tests ----

#[test]
fn test_detect_bump_major_takes_precedence() {
    let messages = vec![
        "fix: something #patch".to_string(),
        "feat: big change #major".to_string(),
        "feat: small change #minor".to_string(),
    ];
    let result = detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "minor");
    assert_eq!(result, BumpKind::Major);
}

#[test]
fn test_detect_bump_minor_over_patch() {
    let messages = vec![
        "fix: something #patch".to_string(),
        "feat: new feature #minor".to_string(),
    ];
    let result = detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "patch");
    assert_eq!(result, BumpKind::Minor);
}

#[test]
fn test_detect_bump_patch_only() {
    let messages = vec!["fix: a bug #patch".to_string()];
    let result = detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "minor");
    assert_eq!(result, BumpKind::Patch);
}

#[test]
fn test_detect_bump_none_token_loses_to_explicit_major() {
    // `#none` is a veto over the default_bump fallback, NOT over explicit
    // release signals. If any commit in the range explicitly asks for a
    // bump, that wins regardless of a sibling `#none`.
    let messages = vec![
        "chore: update deps #none".to_string(),
        "feat: something #major".to_string(),
    ];
    let result = detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "minor");
    assert_eq!(result, BumpKind::Major);
}

#[test]
fn test_detect_bump_none_suppresses_default_fallback() {
    // No explicit token, no conventional marker, but `#none` present →
    // range is intentionally non-release-worthy. Skip regardless of
    // default_bump.
    let messages = vec!["chore: prep #none".to_string()];
    let result = detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "minor");
    assert_eq!(result, BumpKind::None);
}

#[test]
fn test_detect_bump_none_loses_to_conventional_fix() {
    // A legit `fix:` in the range is a release signal. A `#none` on a
    // sibling cleanup commit must not mask it.
    let messages = vec![
        "fix: deref bug".to_string(),
        "chore: revert local-only churn #none".to_string(),
    ];
    let result = detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "none");
    assert_eq!(result, BumpKind::Patch);
}

#[test]
fn test_detect_bump_default_when_no_tokens() {
    // Messages carry no explicit token and no release-worthy conventional
    // marker (docs: doesn't bump), so the default_bump fallback takes effect.
    let messages = vec![
        "unstructured message".to_string(),
        "docs: update readme".to_string(),
    ];
    let result = detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "minor");
    assert_eq!(result, BumpKind::Minor);
}

#[test]
fn test_detect_bump_default_patch() {
    let messages = vec!["chore: deps bump".to_string()];
    let result = detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "patch");
    assert_eq!(result, BumpKind::Patch);
}

#[test]
fn test_detect_bump_default_major() {
    let messages = vec!["chore: deps bump".to_string()];
    let result = detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "major");
    assert_eq!(result, BumpKind::Major);
}

#[test]
fn test_detect_bump_default_none() {
    let messages = vec!["chore: deps bump".to_string()];
    let result = detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "none");
    assert_eq!(result, BumpKind::None);
}

// ------------------------------------------------------------------
// Conventional-commit layer tests
// ------------------------------------------------------------------

#[test]
fn test_conventional_fix_triggers_patch() {
    let messages = vec!["fix: null deref in parser".to_string()];
    let result = detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "none");
    assert_eq!(result, BumpKind::Patch);
}

#[test]
fn test_conventional_feat_triggers_minor() {
    let messages = vec!["feat(api): add pagination".to_string()];
    let result = detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "none");
    assert_eq!(result, BumpKind::Minor);
}

#[test]
fn test_conventional_perf_triggers_patch() {
    let messages = vec!["perf: skip redundant clone".to_string()];
    let result = detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "none");
    assert_eq!(result, BumpKind::Patch);
}

#[test]
fn test_conventional_breaking_change_footer_triggers_major() {
    let messages =
        vec!["feat: rename flags\n\nBREAKING CHANGE: --dry replaced with --dry-run".to_string()];
    let result = detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "none");
    assert_eq!(result, BumpKind::Major);
}

#[test]
fn test_conventional_breaking_shorthand_triggers_major() {
    let messages = vec!["feat!: rewrite config layer".to_string()];
    let result = detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "none");
    assert_eq!(result, BumpKind::Major);
}

#[test]
fn test_conventional_scoped_breaking_shorthand_triggers_major() {
    let messages = vec!["fix(config)!: rename layer field".to_string()];
    let result = detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "none");
    assert_eq!(result, BumpKind::Major);
}

#[test]
fn test_conventional_chore_only_range_noops_with_none_default() {
    // This is the cfgd dogfood scenario: a stable lib crate gets a test/chore
    // touch but no release-worthy commit. default_bump=none means autotag
    // should NOT mint a new tag — matches the intent.
    let messages = vec![
        "chore: bump dep".to_string(),
        "test: new harness".to_string(),
        "refactor: cleaner helper".to_string(),
    ];
    let result = detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "none");
    assert_eq!(result, BumpKind::None);
}

#[test]
fn test_conventional_ignored_when_explicit_token_present() {
    // `#major` wins over `feat:` — explicit intent overrides the
    // conventional-commit layer.
    let messages = vec!["feat: add thing\n\n#major".to_string()];
    let result = detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "none");
    assert_eq!(result, BumpKind::Major);
}

#[test]
fn test_detect_bump_empty_messages_uses_default() {
    let result = detect_bump_from_tokens(&[], "#major", "#minor", "#patch", "#none", "patch");
    assert_eq!(result, BumpKind::Patch);
}

#[test]
fn test_detect_bump_custom_tokens() {
    let messages = vec!["BREAKING CHANGE: rewrite".to_string()];
    let result = detect_bump_from_tokens(
        &messages,
        "BREAKING CHANGE",
        "feat:",
        "fix:",
        "skip:",
        "patch",
    );
    assert_eq!(result, BumpKind::Major);
}

// ---- Apply bump tests ----

#[test]
fn test_apply_bump_major() {
    assert_eq!(apply_bump(1, 2, 3, &BumpKind::Major), (2, 0, 0));
}

#[test]
fn test_apply_bump_minor() {
    assert_eq!(apply_bump(1, 2, 3, &BumpKind::Minor), (1, 3, 0));
}

#[test]
fn test_apply_bump_patch() {
    assert_eq!(apply_bump(1, 2, 3, &BumpKind::Patch), (1, 2, 4));
}

#[test]
fn test_apply_bump_none() {
    assert_eq!(apply_bump(1, 2, 3, &BumpKind::None), (1, 2, 3));
}

#[test]
fn test_apply_bump_from_zero() {
    assert_eq!(apply_bump(0, 0, 0, &BumpKind::Patch), (0, 0, 1));
    assert_eq!(apply_bump(0, 0, 0, &BumpKind::Minor), (0, 1, 0));
    assert_eq!(apply_bump(0, 0, 0, &BumpKind::Major), (1, 0, 0));
}

// ---- pre-major demotion tests ----

/// `demote_pre_major` only touches an inferred Major/Minor while the
/// governing major is `0`, and the two axes never cascade.
#[test]
fn demote_pre_major_axes() {
    // major == 0: each flag governs its own axis.
    assert_eq!(
        demote_pre_major(BumpKind::Major, 0, true, false),
        BumpKind::Minor
    );
    assert_eq!(
        demote_pre_major(BumpKind::Major, 0, false, false),
        BumpKind::Major
    );
    assert_eq!(
        demote_pre_major(BumpKind::Minor, 0, false, true),
        BumpKind::Patch
    );
    assert_eq!(
        demote_pre_major(BumpKind::Minor, 0, false, false),
        BumpKind::Minor
    );
    // Both flags on: breaking → minor (NOT cascaded to patch), feat → patch.
    assert_eq!(
        demote_pre_major(BumpKind::Major, 0, true, true),
        BumpKind::Minor
    );
    assert_eq!(
        demote_pre_major(BumpKind::Minor, 0, true, true),
        BumpKind::Patch
    );
    // Patch / None are never demoted.
    assert_eq!(
        demote_pre_major(BumpKind::Patch, 0, true, true),
        BumpKind::Patch
    );
    assert_eq!(
        demote_pre_major(BumpKind::None, 0, true, true),
        BumpKind::None
    );
}

/// Once a real tag reaches `1.x`, both toggles are inert.
#[test]
fn demote_pre_major_inert_at_one() {
    assert_eq!(
        demote_pre_major(BumpKind::Major, 1, true, true),
        BumpKind::Major
    );
    assert_eq!(
        demote_pre_major(BumpKind::Minor, 1, true, true),
        BumpKind::Minor
    );
    assert_eq!(
        demote_pre_major(BumpKind::Major, 2, true, false),
        BumpKind::Major
    );
}

fn cfg_with_pre_major(minor_pre_major: bool, patch_for_minor: bool) -> ResolvedConfig {
    let tag_cfg = TagConfig {
        bump_minor_pre_major: Some(minor_pre_major),
        bump_patch_for_minor_pre_major: Some(patch_for_minor),
        ..Default::default()
    };
    ResolvedConfig::from_tag_config(&tag_cfg, &push_opts(false, false))
}

#[test]
fn has_explicit_bump_token_whole_word_only() {
    let cfg = cfg_with_pre_major(true, false);
    assert!(has_explicit_bump_token(
        &["chore: x #minor".to_string()],
        &cfg
    ));
    assert!(has_explicit_bump_token(
        &["release #major".to_string()],
        &cfg
    ));
    // Conventional-only ranges carry no token.
    assert!(!has_explicit_bump_token(
        &["feat!: break".to_string()],
        &cfg
    ));
    // A token embedded in a larger word is not a token.
    assert!(!has_explicit_bump_token(
        &["fix #minorbug".to_string()],
        &cfg
    ));
}

/// End-to-end precedence: an explicit token always wins; an inferred
/// breaking change demotes only while pre-1.0 and only when the flag is on.
#[test]
fn detect_bump_demoted_precedence() {
    // feat! with bump_minor_pre_major on, base 0.x → Minor.
    assert_eq!(
        detect_bump_demoted(
            &["feat!: break".to_string()],
            &cfg_with_pre_major(true, false),
            Some("v0.5.0")
        ),
        BumpKind::Minor
    );
    // Same input, flag off → Major (consensus default).
    assert_eq!(
        detect_bump_demoted(
            &["feat!: break".to_string()],
            &cfg_with_pre_major(false, false),
            Some("v0.5.0")
        ),
        BumpKind::Major
    );
    // Explicit #major token wins over demotion even with the flag on.
    assert_eq!(
        detect_bump_demoted(
            &["feat!: break".to_string(), "stabilize #major".to_string()],
            &cfg_with_pre_major(true, false),
            Some("v0.5.0"),
        ),
        BumpKind::Major
    );
    // Inert once the base tag is 1.x.
    assert_eq!(
        detect_bump_demoted(
            &["feat!: break".to_string()],
            &cfg_with_pre_major(true, false),
            Some("v1.2.0")
        ),
        BumpKind::Major
    );
    // No prior tag is treated as pre-major (base major 0).
    assert_eq!(
        detect_bump_demoted(
            &["feat!: break".to_string()],
            &cfg_with_pre_major(true, false),
            None
        ),
        BumpKind::Minor
    );
    // bump_patch_for_minor_pre_major: a plain feat demotes to patch pre-1.0.
    assert_eq!(
        detect_bump_demoted(
            &["feat: thing".to_string()],
            &cfg_with_pre_major(false, true),
            Some("v0.5.0")
        ),
        BumpKind::Patch
    );
}

/// A `#none` token is overridden by a conventional marker in the same range,
/// so it must NOT suppress that breaking change's pre-major demotion.
#[test]
fn detect_bump_demoted_none_token_does_not_block_demotion() {
    // #none loses to feat!: -> the breaking change still demotes to Minor.
    assert_eq!(
        detect_bump_demoted(
            &["feat!: break #none".to_string()],
            &cfg_with_pre_major(true, false),
            Some("v0.5.0")
        ),
        BumpKind::Minor
    );
    // A standalone #none (no conventional marker) still skips the bump.
    assert_eq!(
        detect_bump_demoted(
            &["chore: housekeeping #none".to_string()],
            &cfg_with_pre_major(true, false),
            Some("v0.5.0")
        ),
        BumpKind::None
    );
}

/// `has_explicit_bump_token` resolves the SAME configurable tokens as
/// `detect_bump_from_tokens`, so a custom major token still wins over
/// demotion under non-default token config.
#[test]
fn detect_bump_demoted_honors_custom_tokens() {
    let tag_cfg = TagConfig {
        major_string_token: Some("#breaking".to_string()),
        bump_minor_pre_major: Some(true),
        ..Default::default()
    };
    let cfg = ResolvedConfig::from_tag_config(&tag_cfg, &push_opts(false, false));
    // Custom #breaking token drives Major and is not demoted.
    assert_eq!(
        detect_bump_demoted(&["rework #breaking".to_string()], &cfg, Some("v0.5.0")),
        BumpKind::Major
    );
    // A conventional feat!: (no custom token) still demotes.
    assert_eq!(
        detect_bump_demoted(&["feat!: rework".to_string()], &cfg, Some("v0.5.0")),
        BumpKind::Minor
    );
}

// ---- branch_matches tests ----

#[test]
fn test_branch_matches_exact() {
    assert!(branch_matches("main", &["main".to_string()]));
    assert!(branch_matches("master", &["master".to_string()]));
}

#[test]
fn test_branch_matches_regex() {
    assert!(branch_matches("release/1.0", &["release/.*".to_string()]));
}

#[test]
fn test_branch_no_match() {
    assert!(!branch_matches(
        "feature/foo",
        &["main".to_string(), "master".to_string()]
    ));
}

#[test]
fn test_branch_matches_empty_patterns() {
    assert!(!branch_matches("main", &[]));
}

// ---- Prerelease suffix tests ----

#[test]
fn test_prerelease_suffix_application() {
    // Simulate the prerelease logic
    let version = "1.2.0";
    let suffix = "beta";
    let result = format!("{}-{}", version, suffix);
    assert_eq!(result, "1.2.0-beta");
}

#[test]
fn test_prerelease_suffix_custom() {
    let version = "2.0.0";
    let suffix = "rc.1";
    let result = format!("{}-{}", version, suffix);
    assert_eq!(result, "2.0.0-rc.1");
}

// ---- Custom tag override tests ----

#[test]
fn test_custom_tag_with_prefix() {
    // If custom tag already has prefix, don't duplicate
    let custom = "v5.0.0";
    let prefix = "v";
    let tag = if custom.starts_with(prefix) {
        custom.to_string()
    } else {
        format!("{}{}", prefix, custom)
    };
    assert_eq!(tag, "v5.0.0");
}

#[test]
fn test_custom_tag_without_prefix() {
    let custom = "5.0.0";
    let prefix = "v";
    let tag = if custom.starts_with(prefix) {
        custom.to_string()
    } else {
        format!("{}{}", prefix, custom)
    };
    assert_eq!(tag, "v5.0.0");
}

// ---- Config resolution tests ----

#[test]
fn test_resolved_config_defaults() {
    let cfg = TagConfig::default();
    let opts = TagOpts {
        dry_run: false,
        custom_tag: None,
        version_override: None,
        default_bump: None,
        crate_name: None,
        push: false,
        no_push: false,
        push_tags_only: false,
        sign: false,
        no_sign: false,
        push_remote: None,
        push_dry_run: false,
        changelog: false,
        config_override: None,
        verbose: false,
        debug: false,
        quiet: false,
        strict: false,
    };
    let resolved = ResolvedConfig::from_tag_config(&cfg, &opts);
    assert_eq!(resolved.default_bump, "none");
    assert_eq!(resolved.tag_prefix, "v");
    assert_eq!(resolved.tag_context, "repo");
    assert_eq!(resolved.branch_history, "compare");
    assert_eq!(resolved.initial_version, "0.0.0");
    assert!(!resolved.prerelease);
    assert_eq!(resolved.prerelease_suffix, "beta");
    assert!(!resolved.force_without_changes);
    assert!(!resolved.force_without_changes_pre);
    assert_eq!(resolved.major_string_token, "#major");
    assert_eq!(resolved.minor_string_token, "#minor");
    assert_eq!(resolved.patch_string_token, "#patch");
    assert_eq!(resolved.none_string_token, "#none");
}

#[test]
fn test_resolved_config_cli_overrides() {
    let cfg = TagConfig {
        default_bump: Some("minor".to_string()),
        ..Default::default()
    };
    let opts = TagOpts {
        dry_run: false,
        custom_tag: Some("v9.9.9".to_string()),
        version_override: None,
        default_bump: Some("major".to_string()),
        crate_name: None,
        push: false,
        no_push: false,
        push_tags_only: false,
        sign: false,
        no_sign: false,
        push_remote: None,
        push_dry_run: false,
        changelog: false,
        config_override: None,
        verbose: false,
        debug: false,
        quiet: false,
        strict: false,
    };
    let resolved = ResolvedConfig::from_tag_config(&cfg, &opts);
    assert_eq!(resolved.default_bump, "major");
    assert_eq!(resolved.custom_tag, Some("v9.9.9".to_string()));
}

#[test]
fn test_resolved_config_full_config() {
    let cfg = TagConfig {
        default_bump: Some("patch".to_string()),
        bump_minor_pre_major: None,
        bump_patch_for_minor_pre_major: None,
        tag_prefix: Some("release-v".to_string()),
        release_branches: Some(vec!["main".to_string(), "release/.*".to_string()]),
        custom_tag: None,
        tag_context: Some("branch".to_string()),
        branch_history: Some("last".to_string()),
        initial_version: Some("1.0.0".to_string()),
        prerelease: Some(true),
        prerelease_suffix: Some("alpha".to_string()),
        force_without_changes: Some(true),
        force_without_changes_pre: Some(true),
        major_string_token: Some("BREAKING".to_string()),
        minor_string_token: Some("feat:".to_string()),
        patch_string_token: Some("fix:".to_string()),
        none_string_token: Some("skip".to_string()),
        git_api_tagging: Some(false),
        sign: None,
        push: None,
        skip_ci_on_bump: None,
        verbose: Some(false),
        tag_pre_hooks: None,
        tag_post_hooks: None,
    };
    let opts = TagOpts {
        dry_run: false,
        custom_tag: None,
        version_override: None,
        default_bump: None,
        crate_name: None,
        push: false,
        no_push: false,
        push_tags_only: false,
        sign: false,
        no_sign: false,
        push_remote: None,
        push_dry_run: false,
        changelog: false,
        config_override: None,
        verbose: false,
        debug: false,
        quiet: false,
        strict: false,
    };
    let resolved = ResolvedConfig::from_tag_config(&cfg, &opts);
    assert_eq!(resolved.default_bump, "patch");
    assert_eq!(resolved.tag_prefix, "release-v");
    assert_eq!(resolved.release_branches.len(), 2);
    assert_eq!(resolved.tag_context, "branch");
    assert_eq!(resolved.branch_history, "last");
    assert_eq!(resolved.initial_version, "1.0.0");
    assert!(resolved.prerelease);
    assert_eq!(resolved.prerelease_suffix, "alpha");
    assert!(resolved.force_without_changes);
    assert!(resolved.force_without_changes_pre);
    assert_eq!(resolved.major_string_token, "BREAKING");
    assert_eq!(resolved.minor_string_token, "feat:");
    assert_eq!(resolved.patch_string_token, "fix:");
    assert_eq!(resolved.none_string_token, "skip");
}

// ---- Config parsing from YAML tests ----

#[test]
fn test_tag_config_from_yaml_full() {
    let yaml = r##"
default_bump: patch
tag_prefix: "v"
release_branches:
  - main
  - "release/.*"
tag_context: branch
branch_history: last
initial_version: "1.0.0"
prerelease: true
prerelease_suffix: rc
force_without_changes: true
force_without_changes_pre: false
major_string_token: "#major"
minor_string_token: "#minor"
patch_string_token: "#patch"
none_string_token: "#none"
git_api_tagging: true
verbose: false
"##;
    let cfg: TagConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.default_bump, Some("patch".to_string()));
    assert_eq!(cfg.tag_prefix, Some("v".to_string()));
    assert_eq!(
        cfg.release_branches,
        Some(vec!["main".to_string(), "release/.*".to_string()])
    );
    assert_eq!(cfg.tag_context, Some("branch".to_string()));
    assert_eq!(cfg.branch_history, Some("last".to_string()));
    assert_eq!(cfg.initial_version, Some("1.0.0".to_string()));
    assert_eq!(cfg.prerelease, Some(true));
    assert_eq!(cfg.prerelease_suffix, Some("rc".to_string()));
    assert_eq!(cfg.force_without_changes, Some(true));
    assert_eq!(cfg.force_without_changes_pre, Some(false));
    assert_eq!(cfg.git_api_tagging, Some(true));
    assert_eq!(cfg.verbose, Some(false));
}

#[test]
fn test_tag_config_from_yaml_minimal() {
    let yaml = "{}";
    let cfg: TagConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.default_bump, None);
    assert_eq!(cfg.tag_prefix, None);
    assert_eq!(cfg.release_branches, None);
}

#[test]
fn test_tag_config_from_yaml_defaults() {
    let yaml = "default_bump: major";
    let cfg: TagConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.default_bump, Some("major".to_string()));
    assert_eq!(cfg.tag_prefix, None); // not set, will use default when resolved
}

#[test]
fn test_top_level_config_with_tag_section() {
    let yaml = r#"
project_name: myproject
crates:
  - name: myproject
    path: "."
    tag_template: "v{{ .Version }}"
tag:
  default_bump: patch
  tag_prefix: "v"
  branch_history: last
"#;
    let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
    let tag = config.tag.unwrap();
    assert_eq!(tag.default_bump, Some("patch".to_string()));
    assert_eq!(tag.branch_history, Some("last".to_string()));
}

#[test]
fn test_tag_pre_post_hooks_yaml_roundtrip() {
    // Both simple-string and structured hook forms must parse; the
    // structured form carries `cmd` / `dir` / `env` so an update-lockfile
    // hook can run inside a workspace subdirectory with its own env.
    let yaml = r#"
tag_pre_hooks:
  - "cargo update --workspace"
  - cmd: "scripts/pre-tag.sh {{ .Tag }}"
    dir: "."
tag_post_hooks:
  - "git push --follow-tags"
"#;
    let cfg: TagConfig = serde_yaml_ng::from_str(yaml).unwrap();
    let pre = cfg.tag_pre_hooks.as_ref().unwrap();
    assert_eq!(pre.len(), 2);
    assert!(matches!(
        pre[0],
        anodizer_core::config::HookEntry::Simple(ref s) if s == "cargo update --workspace"
    ));
    let post = cfg.tag_post_hooks.as_ref().unwrap();
    assert_eq!(post.len(), 1);
    assert!(matches!(
        post[0],
        anodizer_core::config::HookEntry::Simple(ref s) if s == "git push --follow-tags"
    ));
}

#[test]
fn test_tag_hooks_default_none() {
    // Absent in YAML means Option::None — the `create_tag` closure treats
    // this as "no hooks" and skips invocation.
    let cfg: TagConfig = serde_yaml_ng::from_str("default_bump: minor").unwrap();
    assert!(cfg.tag_pre_hooks.is_none());
    assert!(cfg.tag_post_hooks.is_none());
}

// ---- Integration-style bump logic tests ----

#[test]
fn test_full_bump_flow_major() {
    let messages = vec!["feat: breaking change #major".to_string()];
    let bump = detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "patch");
    assert_eq!(bump, BumpKind::Major);
    let (maj, min, pat) = apply_bump(1, 5, 3, &bump);
    assert_eq!((maj, min, pat), (2, 0, 0));
    let new_tag = format!("v{}.{}.{}", maj, min, pat);
    assert_eq!(new_tag, "v2.0.0");
}

#[test]
fn test_full_bump_flow_minor_default() {
    let messages = vec!["docs: update readme".to_string()];
    let bump = detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "minor");
    assert_eq!(bump, BumpKind::Minor);
    let (maj, min, pat) = apply_bump(1, 2, 3, &bump);
    assert_eq!((maj, min, pat), (1, 3, 0));
}

#[test]
fn test_full_bump_flow_prerelease() {
    let messages = vec!["feat: new thing #minor".to_string()];
    let bump = detect_bump_from_tokens(&messages, "#major", "#minor", "#patch", "#none", "patch");
    assert_eq!(bump, BumpKind::Minor);
    let (maj, min, pat) = apply_bump(1, 2, 3, &bump);
    let version = format!("{}.{}.{}-beta", maj, min, pat);
    assert_eq!(version, "1.3.0-beta");
}

// ---- detect_repo_shape unit tests ----

fn crate_cfg(name: &str, path: &str, template: &str) -> CrateConfig {
    CrateConfig {
        name: name.to_string(),
        path: path.to_string(),
        tag_template: Some(template.to_string()),
        ..Default::default()
    }
}

/// A workspace root with no `Cargo.toml`, so `load_workspace` returns `Err`
/// and the Cargo lockstep signal stays absent. Pinning the root explicitly
/// (instead of `detect_repo_shape` reading the runner's cwd) keeps each
/// shape assertion hermetic — run from the anodizer workspace root it would
/// otherwise flip to `Lockstep` off the real `[workspace.package].version`.
fn empty_root() -> tempfile::TempDir {
    tempfile::tempdir().expect("create temp workspace root")
}

#[test]
fn detect_repo_shape_no_config_no_workspace_returns_single() {
    // Bare repo: no anodizer config, no Cargo workspace info → Single.
    let root = empty_root();
    let shape = detect_repo_shape(root.path(), None, None);
    assert!(matches!(shape, RepoShape::Single));
}

#[test]
fn detect_repo_shape_single_crate_config_returns_single() {
    let config = anodizer_core::config::Config {
        project_name: "app".to_string(),
        crates: vec![crate_cfg("app", ".", "v{{ .Version }}")],
        ..Default::default()
    };
    let root = empty_root();
    let shape = detect_repo_shape(root.path(), Some(&config), None);
    assert!(matches!(shape, RepoShape::Single));
}

#[test]
fn detect_repo_shape_lockstep_workspace_wins_over_per_crate_config() {
    // [workspace.package].version is authoritative — even when the
    // anodizer config has multiple flat crates, a lockstep workspace
    // returns Lockstep so the operator's Cargo-level intent wins.
    let config = anodizer_core::config::Config {
        project_name: "ws".to_string(),
        crates: vec![
            crate_cfg("a", "crates/a", "a-v{{ .Version }}"),
            crate_cfg("b", "crates/b", "b-v{{ .Version }}"),
        ],
        ..Default::default()
    };
    let ws = WorkspaceInfo {
        workspace_package_version: Some("0.1.0".to_string()),
        members: vec![],
    };
    let root = empty_root();
    let shape = detect_repo_shape(root.path(), Some(&config), Some(&ws));
    assert!(matches!(shape, RepoShape::Lockstep));
}

#[test]
fn shared_root_aggregate_name_lockstep_is_project_name() {
    let config = anodizer_core::config::Config {
        project_name: "ws".to_string(),
        crates: vec![
            crate_cfg("a", "crates/a", "v{{ .Version }}"),
            crate_cfg("b", "crates/b", "v{{ .Version }}"),
        ],
        ..Default::default()
    };
    let ws = WorkspaceInfo {
        workspace_package_version: Some("0.1.0".to_string()),
        members: vec![],
    };
    let root = empty_root();
    assert_eq!(
        shared_root_aggregate_name(root.path(), &config, Some(&ws)),
        Some("ws")
    );
}

#[test]
fn shared_root_aggregate_name_flat_aggregate_is_project_name() {
    // Same explicit prefix on every flat crate → FlatAggregate → the
    // project name selects the shared unit.
    let config = anodizer_core::config::Config {
        project_name: "agg".to_string(),
        crates: vec![
            crate_cfg("a", "crates/a", "v{{ .Version }}"),
            crate_cfg("b", "crates/b", "v{{ .Version }}"),
        ],
        ..Default::default()
    };
    let root = empty_root();
    assert_eq!(
        shared_root_aggregate_name(root.path(), &config, None),
        Some("agg")
    );
}

#[test]
fn shared_root_aggregate_name_empty_universe_single_is_project_name() {
    // Config-less repos (Cargo.toml fallback) have no crate universe at
    // all; the project name is the only spelling that can select the
    // repo-level unit.
    let config = anodizer_core::config::Config {
        project_name: "solo".to_string(),
        ..Default::default()
    };
    let root = empty_root();
    assert_eq!(
        shared_root_aggregate_name(root.path(), &config, None),
        Some("solo")
    );
}

#[test]
fn shared_root_aggregate_name_none_on_per_crate_and_named_single() {
    // Per-crate: every selectable name is a universe crate — the project
    // name selects nothing.
    let per_crate = anodizer_core::config::Config {
        project_name: "ws".to_string(),
        crates: vec![
            crate_cfg("a", "crates/a", "a-v{{ .Version }}"),
            crate_cfg("b", "crates/b", "b-v{{ .Version }}"),
        ],
        ..Default::default()
    };
    let root = empty_root();
    assert_eq!(
        shared_root_aggregate_name(root.path(), &per_crate, None),
        None
    );
    // Single WITH a configured crate: that crate's own name is the
    // selectable spelling, so the project name stays invalid.
    let single = anodizer_core::config::Config {
        project_name: "proj".to_string(),
        crates: vec![crate_cfg("app", ".", "v{{ .Version }}")],
        ..Default::default()
    };
    assert_eq!(shared_root_aggregate_name(root.path(), &single, None), None);
}

#[test]
fn detect_repo_shape_mixed_config_keeps_top_level_crates_as_tracks() {
    // Top-level `crates:` alongside `workspaces:`: the workspace group
    // stays intact and each top-level crate not in any group becomes
    // its own singleton track — never silently dropped from tag
    // dispatch. A top-level duplicate of a group member stays with its
    // group (no double dispatch).
    let config = anodizer_core::config::Config {
        project_name: "ws".to_string(),
        crates: vec![
            crate_cfg("root", ".", "root-v{{ .Version }}"),
            crate_cfg("member", "crates/member", "member-v{{ .Version }}"),
        ],
        workspaces: Some(vec![anodizer_core::config::WorkspaceConfig {
            name: "grp".to_string(),
            crates: vec![
                crate_cfg("member", "crates/member", "member-v{{ .Version }}"),
                crate_cfg("sibling", "crates/sibling", "sibling-v{{ .Version }}"),
            ],
            ..Default::default()
        }]),
        ..Default::default()
    };
    let root = empty_root();
    let shape = detect_repo_shape(root.path(), Some(&config), None);
    match shape {
        RepoShape::PerCrate(groups) => {
            let names: Vec<Vec<&str>> = groups
                .iter()
                .map(|g| g.iter().map(|c| c.name.as_str()).collect())
                .collect();
            assert_eq!(names, vec![vec!["member", "sibling"], vec!["root"]]);
        }
        other => panic!(
            "expected PerCrate, got {:?}",
            std::mem::discriminant(&other)
        ),
    }
}

#[test]
fn detect_repo_shape_flat_multi_crate_returns_per_crate() {
    let config = anodizer_core::config::Config {
        project_name: "ws".to_string(),
        crates: vec![
            crate_cfg("core", "crates/core", "core-v{{ .Version }}"),
            crate_cfg("cli", "crates/cli", "v{{ .Version }}"),
        ],
        ..Default::default()
    };
    let root = empty_root();
    let shape = detect_repo_shape(root.path(), Some(&config), None);
    match shape {
        RepoShape::PerCrate(groups) => {
            assert_eq!(groups.len(), 2);
            // Flat layout: each crate is its own singleton group.
            assert_eq!(groups[0][0].name, "core");
            assert_eq!(groups[1][0].name, "cli");
        }
        other => panic!(
            "expected PerCrate, got {:?}",
            std::mem::discriminant(&other)
        ),
    }
}

#[test]
fn detect_repo_shape_hybrid_workspaces_returns_per_crate_groups() {
    // workspaces: with two groups (one singleton, one lockstep pair) →
    // PerCrate, preserving group boundaries so each group bumps as a unit.
    let ws1 = anodizer_core::config::WorkspaceConfig {
        name: "group-a".to_string(),
        crates: vec![crate_cfg("core", "crates/core", "core-v{{ .Version }}")],
        ..Default::default()
    };
    let ws2 = anodizer_core::config::WorkspaceConfig {
        name: "group-b".to_string(),
        crates: vec![
            crate_cfg("bin-a", "crates/bin-a", "bin-a-v{{ .Version }}"),
            crate_cfg("bin-b", "crates/bin-b", "bin-b-v{{ .Version }}"),
        ],
        ..Default::default()
    };
    let config = anodizer_core::config::Config {
        project_name: "myproj".to_string(),
        workspaces: Some(vec![ws1, ws2]),
        ..Default::default()
    };
    let root = empty_root();
    let shape = detect_repo_shape(root.path(), Some(&config), None);
    match shape {
        RepoShape::PerCrate(groups) => {
            assert_eq!(groups.len(), 2);
            assert_eq!(groups[0].len(), 1);
            assert_eq!(groups[0][0].name, "core");
            assert_eq!(groups[1].len(), 2);
            assert_eq!(groups[1][0].name, "bin-a");
            assert_eq!(groups[1][1].name, "bin-b");
        }
        other => panic!(
            "expected PerCrate, got {:?}",
            std::mem::discriminant(&other)
        ),
    }
}

#[test]
fn detect_repo_shape_workspaces_block_wins_over_workspace_package_version() {
    let ws1 = anodizer_core::config::WorkspaceConfig {
        name: "group".to_string(),
        crates: vec![
            crate_cfg("a", "crates/a", "a-v{{ .Version }}"),
            crate_cfg("b", "crates/b", "b-v{{ .Version }}"),
        ],
        ..Default::default()
    };
    let config = anodizer_core::config::Config {
        project_name: "p".to_string(),
        workspaces: Some(vec![ws1]),
        ..Default::default()
    };
    let ws = WorkspaceInfo {
        workspace_package_version: Some("0.2.0".to_string()),
        members: vec![],
    };
    let root = empty_root();
    let shape = detect_repo_shape(root.path(), Some(&config), Some(&ws));
    match shape {
        RepoShape::PerCrate(groups) => {
            assert_eq!(groups.len(), 1);
            assert_eq!(groups[0].len(), 2);
        }
        other => panic!(
            "expected PerCrate (workspaces: declaration wins over [workspace.package].version), got {:?}",
            std::mem::discriminant(&other)
        ),
    }
}

#[test]
fn detect_repo_shape_single_flat_crate_returns_single() {
    // A flat config with exactly one crate is NOT per-crate (no group
    // routing needed); it falls through to the single-crate path.
    let config = anodizer_core::config::Config {
        project_name: "solo".to_string(),
        crates: vec![crate_cfg("solo", ".", "v{{ .Version }}")],
        ..Default::default()
    };
    let root = empty_root();
    let shape = detect_repo_shape(root.path(), Some(&config), None);
    assert!(matches!(shape, RepoShape::Single));
}

/// A `WorkspaceInfo` with no `[workspace.package].version`, so the Cargo
/// signal does NOT force `Lockstep` — the prefix axis decides. Passed
/// explicitly so the result is hermetic regardless of the test's cwd.
fn ws_no_lockstep() -> WorkspaceInfo {
    WorkspaceInfo {
        workspace_package_version: None,
        members: vec![],
    }
}

#[test]
fn detect_repo_shape_same_prefix_flat_crates_returns_flat_aggregate() {
    // ≥2 flat crates all on `v{{ Version }}` with no workspace version:
    // one shared tag prefix is one shared tag namespace, so they release in
    // lockstep — `v0.2.0` cannot be two crates' independent tag — but each
    // carries its own `[package].version`, so the shape is `FlatAggregate`
    // (bumped by N per-crate manifests), not genuine `Lockstep`.
    let config = anodizer_core::config::Config {
        project_name: "ws".to_string(),
        crates: vec![
            crate_cfg("core", "crates/core", "v{{ .Version }}"),
            crate_cfg("cli", "crates/cli", "v{{ .Version }}"),
        ],
        ..Default::default()
    };
    let root = empty_root();
    let shape = detect_repo_shape(root.path(), Some(&config), Some(&ws_no_lockstep()));
    match shape {
        RepoShape::FlatAggregate(crates) => {
            assert_eq!(crates.len(), 2, "carries the flat crate list");
            assert_eq!(crates[0].name, "core");
            assert_eq!(crates[1].name, "cli");
        }
        other => panic!(
            "same-prefix flat crates must classify as FlatAggregate, got {:?}",
            std::mem::discriminant(&other)
        ),
    }
}

#[test]
fn detect_repo_shape_distinct_prefix_flat_crates_returns_per_crate() {
    // Distinct prefixes (`core-v*` + `v*`) are independent tracks → PerCrate.
    let config = anodizer_core::config::Config {
        project_name: "ws".to_string(),
        crates: vec![
            crate_cfg("core", "crates/core", "core-v{{ .Version }}"),
            crate_cfg("cli", "crates/cli", "v{{ .Version }}"),
        ],
        ..Default::default()
    };
    let root = empty_root();
    let shape = detect_repo_shape(root.path(), Some(&config), Some(&ws_no_lockstep()));
    match shape {
        RepoShape::PerCrate(groups) => assert_eq!(groups.len(), 2),
        other => panic!(
            "expected PerCrate for distinct prefixes, got {:?}",
            std::mem::discriminant(&other)
        ),
    }
}

#[test]
fn detect_repo_shape_no_tag_template_flat_crates_returns_per_crate() {
    // No `tag_template` → no extractable shared prefix (each would fall back
    // to a per-crate `{crate}-v`), so the crates stay distinct → PerCrate.
    let config = anodizer_core::config::Config {
        project_name: "ws".to_string(),
        crates: vec![
            crate_cfg("core", "crates/core", ""),
            crate_cfg("cli", "crates/cli", ""),
        ],
        ..Default::default()
    };
    let root = empty_root();
    let shape = detect_repo_shape(root.path(), Some(&config), Some(&ws_no_lockstep()));
    match shape {
        RepoShape::PerCrate(groups) => assert_eq!(groups.len(), 2),
        other => panic!(
            "expected PerCrate when no tag_template yields a shared prefix, got {:?}",
            std::mem::discriminant(&other)
        ),
    }
}

#[test]
fn detect_repo_shape_explicit_workspaces_shared_prefix_still_per_crate() {
    // An explicit `workspaces:` block is operator intent and wins at step 1,
    // even when its crates coincidentally share one tag prefix — the
    // same-prefix → FlatAggregate collapse applies ONLY to inferred flat
    // `crates:`.
    let ws1 = anodizer_core::config::WorkspaceConfig {
        name: "group-a".to_string(),
        crates: vec![crate_cfg("a", "crates/a", "v{{ .Version }}")],
        ..Default::default()
    };
    let ws2 = anodizer_core::config::WorkspaceConfig {
        name: "group-b".to_string(),
        crates: vec![crate_cfg("b", "crates/b", "v{{ .Version }}")],
        ..Default::default()
    };
    let config = anodizer_core::config::Config {
        project_name: "p".to_string(),
        workspaces: Some(vec![ws1, ws2]),
        ..Default::default()
    };
    let root = empty_root();
    let shape = detect_repo_shape(root.path(), Some(&config), Some(&ws_no_lockstep()));
    match shape {
        RepoShape::PerCrate(groups) => assert_eq!(groups.len(), 2),
        other => panic!(
            "explicit workspaces: must stay PerCrate despite a shared prefix, got {:?}",
            std::mem::discriminant(&other)
        ),
    }
}

// ---- flat-aggregate coherence guard tests ----

/// Write a two-crate flat workspace whose members share `v{{ Version }}` but
/// carry the supplied `[package].version` values, returning the root dir.
fn flat_aggregate_versions_fixture(
    core_ver: &str,
    cli_ver: &str,
) -> (tempfile::TempDir, anodizer_core::config::Config) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    for (name, ver) in [("core", core_ver), ("cli", cli_ver)] {
        let dir = root.join(format!("crates/{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"{ver}\"\n"),
        )
        .unwrap();
    }
    let config = anodizer_core::config::Config {
        project_name: "agg".to_string(),
        crates: vec![
            crate_cfg("core", "crates/core", "v{{ .Version }}"),
            crate_cfg("cli", "crates/cli", "v{{ .Version }}"),
        ],
        ..Default::default()
    };
    (tmp, config)
}

#[test]
fn coherence_guard_passes_when_versions_agree() {
    let (tmp, config) = flat_aggregate_versions_fixture("0.2.0", "0.2.0");
    let res = guard_flat_aggregate_coherence(Some(&config), Some(&ws_no_lockstep()), tmp.path());
    assert!(res.is_ok(), "all-agree flat aggregate must pass: {res:?}");
}

#[test]
fn coherence_guard_rejects_divergent_versions() {
    let (tmp, config) = flat_aggregate_versions_fixture("0.5.0", "0.1.0");
    let err = guard_flat_aggregate_coherence(Some(&config), Some(&ws_no_lockstep()), tmp.path())
        .unwrap_err()
        .to_string();
    assert!(err.contains("core"), "names conflicting crate core: {err}");
    assert!(err.contains("cli"), "names conflicting crate cli: {err}");
    assert!(err.contains("0.5.0") && err.contains("0.1.0"), "{err}");
    assert!(err.contains("prefix 'v'"), "names the shared prefix: {err}");
    assert!(
        err.contains("[workspace.package].version"),
        "steers toward lockstep: {err}"
    );
    assert!(
        err.contains("distinct tag_template prefix"),
        "steers toward independent prefixes: {err}"
    );
}

/// Mixed shape: top-level crates sharing one extractable prefix alongside
/// `workspaces:` join as ONE aggregate group — separate singleton groups
/// would cut divergent tags (v0.2.0 AND v0.1.1) into one `v*` namespace.
#[test]
fn detect_repo_shape_mixed_shared_prefix_leftovers_join_one_group() {
    let config = anodizer_core::config::Config {
        project_name: "mixed".to_string(),
        crates: vec![
            crate_cfg("alpha", "crates/alpha", "v{{ .Version }}"),
            crate_cfg("beta", "crates/beta", "v{{ .Version }}"),
        ],
        workspaces: Some(vec![anodizer_core::config::WorkspaceConfig {
            name: "grp".to_string(),
            crates: vec![crate_cfg("gamma", "tools/gamma", "gamma-v{{ .Version }}")],
            ..Default::default()
        }]),
        ..Default::default()
    };
    let root = empty_root();
    let shape = detect_repo_shape(root.path(), Some(&config), None);
    match shape {
        RepoShape::PerCrate(groups) => {
            let names: Vec<Vec<&str>> = groups
                .iter()
                .map(|g| g.iter().map(|c| c.name.as_str()).collect())
                .collect();
            assert_eq!(
                names,
                vec![vec!["gamma"], vec!["alpha", "beta"]],
                "shared-prefix leftovers must join as one aggregate group"
            );
        }
        other => panic!(
            "expected PerCrate, got {:?}",
            std::mem::discriminant(&other)
        ),
    }
}

/// Mixed shape: leftovers with DISTINCT prefixes stay independent
/// singleton groups — no shared namespace, no aggregation.
#[test]
fn detect_repo_shape_mixed_distinct_prefix_leftovers_stay_singletons() {
    let config = anodizer_core::config::Config {
        project_name: "mixed".to_string(),
        crates: vec![
            crate_cfg("alpha", "crates/alpha", "alpha-v{{ .Version }}"),
            crate_cfg("beta", "crates/beta", "beta-v{{ .Version }}"),
        ],
        workspaces: Some(vec![anodizer_core::config::WorkspaceConfig {
            name: "grp".to_string(),
            crates: vec![crate_cfg("gamma", "tools/gamma", "gamma-v{{ .Version }}")],
            ..Default::default()
        }]),
        ..Default::default()
    };
    let root = empty_root();
    let shape = detect_repo_shape(root.path(), Some(&config), None);
    match shape {
        RepoShape::PerCrate(groups) => {
            let names: Vec<Vec<&str>> = groups
                .iter()
                .map(|g| g.iter().map(|c| c.name.as_str()).collect())
                .collect();
            assert_eq!(names, vec![vec!["gamma"], vec!["alpha"], vec!["beta"]]);
        }
        other => panic!(
            "expected PerCrate, got {:?}",
            std::mem::discriminant(&other)
        ),
    }
}

/// The mixed shape's leftover aggregate gets the SAME coherence check as
/// a flat aggregate: divergent `[package].version` under one shared
/// prefix is an impossible config and must error before tagging.
#[test]
fn coherence_guard_rejects_divergent_mixed_leftovers() {
    let (tmp, mut config) = flat_aggregate_versions_fixture("0.5.0", "0.1.0");
    config.workspaces = Some(vec![anodizer_core::config::WorkspaceConfig {
        name: "grp".to_string(),
        crates: vec![crate_cfg("gamma", "tools/gamma", "gamma-v{{ .Version }}")],
        ..Default::default()
    }]);
    let err = guard_flat_aggregate_coherence(Some(&config), Some(&ws_no_lockstep()), tmp.path())
        .unwrap_err()
        .to_string();
    assert!(err.contains("core") && err.contains("cli"), "{err}");
    assert!(err.contains("0.5.0") && err.contains("0.1.0"), "{err}");
}

/// Agreeing mixed leftovers pass the guard.
#[test]
fn coherence_guard_passes_agreeing_mixed_leftovers() {
    let (tmp, mut config) = flat_aggregate_versions_fixture("0.2.0", "0.2.0");
    config.workspaces = Some(vec![anodizer_core::config::WorkspaceConfig {
        name: "grp".to_string(),
        crates: vec![crate_cfg("gamma", "tools/gamma", "gamma-v{{ .Version }}")],
        ..Default::default()
    }]);
    let res = guard_flat_aggregate_coherence(Some(&config), Some(&ws_no_lockstep()), tmp.path());
    assert!(res.is_ok(), "agreeing mixed leftovers must pass: {res:?}");
}

/// A missing member manifest is skipped, not errored: the guard fires only
/// on versions it can actually read.
#[test]
fn coherence_guard_skips_missing_manifests() {
    let tmp = tempfile::tempdir().unwrap();
    let config = anodizer_core::config::Config {
        project_name: "agg".to_string(),
        crates: vec![
            crate_cfg("core", "crates/core", "v{{ .Version }}"),
            crate_cfg("cli", "crates/cli", "v{{ .Version }}"),
        ],
        ..Default::default()
    };
    // No Cargo.toml on disk → every member skipped → no versions to compare.
    let res = guard_flat_aggregate_coherence(Some(&config), Some(&ws_no_lockstep()), tmp.path());
    assert!(res.is_ok(), "missing manifests must be skipped: {res:?}");
}

/// Non-flat-aggregate shapes (here a distinct-prefix `PerCrate`) are a no-op
/// even with divergent versions — one tag never spans both crates.
#[test]
fn coherence_guard_noop_for_non_flat_aggregate() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    for (name, ver) in [("core", "0.5.0"), ("cli", "0.1.0")] {
        let dir = root.join(format!("crates/{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"{ver}\"\n"),
        )
        .unwrap();
    }
    let config = anodizer_core::config::Config {
        project_name: "p".to_string(),
        crates: vec![
            crate_cfg("core", "crates/core", "core-v{{ .Version }}"),
            crate_cfg("cli", "crates/cli", "cli-v{{ .Version }}"),
        ],
        ..Default::default()
    };
    let res = guard_flat_aggregate_coherence(Some(&config), Some(&ws_no_lockstep()), root);
    assert!(
        res.is_ok(),
        "distinct-prefix PerCrate is not guarded: {res:?}"
    );
}

/// A member whose manifest is PRESENT but carries no literal
/// `[package].version` (a virtual / workspace-inheriting manifest) must be
/// skipped, not compared as a `0.0.0` sentinel: it neither trips the guard
/// against a real sibling nor masks a real divergence.
#[test]
fn coherence_guard_skips_versionless_member() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    // `core` carries a real version; `cli` declares no `[package].version`.
    std::fs::create_dir_all(root.join("crates/core")).unwrap();
    std::fs::write(
        root.join("crates/core/Cargo.toml"),
        "[package]\nname = \"core\"\nversion = \"0.2.0\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("crates/cli")).unwrap();
    std::fs::write(
        root.join("crates/cli/Cargo.toml"),
        "[package]\nname = \"cli\"\nversion.workspace = true\n",
    )
    .unwrap();
    let config = anodizer_core::config::Config {
        project_name: "agg".to_string(),
        crates: vec![
            crate_cfg("core", "crates/core", "v{{ .Version }}"),
            crate_cfg("cli", "crates/cli", "v{{ .Version }}"),
        ],
        ..Default::default()
    };
    let res = guard_flat_aggregate_coherence(Some(&config), Some(&ws_no_lockstep()), root);
    assert!(
        res.is_ok(),
        "versionless member must be skipped, not compared as 0.0.0: {res:?}"
    );
}

/// A 3-way divergence names EVERY member (not just the first conflicting
/// pair), so a `[0.2.0, 0.2.0, 0.5.0]` split is fully visible.
#[test]
fn coherence_guard_lists_all_members_on_n_way_divergence() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    for (name, ver) in [("a", "0.2.0"), ("b", "0.2.0"), ("c", "0.5.0")] {
        let dir = root.join(format!("crates/{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"{ver}\"\n"),
        )
        .unwrap();
    }
    let config = anodizer_core::config::Config {
        project_name: "agg".to_string(),
        crates: vec![
            crate_cfg("a", "crates/a", "v{{ .Version }}"),
            crate_cfg("b", "crates/b", "v{{ .Version }}"),
            crate_cfg("c", "crates/c", "v{{ .Version }}"),
        ],
        ..Default::default()
    };
    let err = guard_flat_aggregate_coherence(Some(&config), Some(&ws_no_lockstep()), root)
        .unwrap_err()
        .to_string();
    // All three members appear with their versions — including the two that
    // agree (`a`/`b`), which a first-pair-only message would have dropped.
    assert!(err.contains("'a' (0.2.0)"), "lists member a: {err}");
    assert!(err.contains("'b' (0.2.0)"), "lists member b: {err}");
    assert!(err.contains("'c' (0.5.0)"), "lists member c: {err}");
}

/// A strict SUBSET of a flat `crates:` list sharing one prefix
/// aggregates BY prefix: `alpha`+`beta` (both `v*`) form one group,
/// `gamma` (`gamma-v*`) stays independent — never three singletons
/// cutting divergent tags into the shared `v*` namespace, and never a
/// whole-list collapse swallowing `gamma`.
#[test]
fn detect_repo_shape_flat_prefix_subset_groups_by_prefix() {
    let config = anodizer_core::config::Config {
        project_name: "p".to_string(),
        crates: vec![
            crate_cfg("alpha", "crates/alpha", "v{{ .Version }}"),
            crate_cfg("beta", "crates/beta", "v{{ .Version }}"),
            crate_cfg("gamma", "crates/gamma", "gamma-v{{ .Version }}"),
        ],
        ..Default::default()
    };
    let root = empty_root();
    let shape = detect_repo_shape(root.path(), Some(&config), None);
    match shape {
        RepoShape::PerCrate(groups) => {
            let names: Vec<Vec<&str>> = groups
                .iter()
                .map(|g| g.iter().map(|c| c.name.as_str()).collect())
                .collect();
            assert_eq!(
                names,
                vec![vec!["alpha", "beta"], vec!["gamma"]],
                "shared-prefix subset must aggregate; unique prefix stays singleton"
            );
        }
        other => panic!(
            "expected PerCrate, got {:?}",
            std::mem::discriminant(&other)
        ),
    }
}

/// The SAME subset rule in the mixed shape: leftovers `alpha`+`beta`
/// (both `v*`) aggregate while leftover `delta` (`delta-v*`) stays a
/// singleton track alongside the workspace group.
#[test]
fn detect_repo_shape_mixed_leftover_prefix_subset_groups_by_prefix() {
    let config = anodizer_core::config::Config {
        project_name: "mixed".to_string(),
        crates: vec![
            crate_cfg("alpha", "crates/alpha", "v{{ .Version }}"),
            crate_cfg("beta", "crates/beta", "v{{ .Version }}"),
            crate_cfg("delta", "crates/delta", "delta-v{{ .Version }}"),
        ],
        workspaces: Some(vec![anodizer_core::config::WorkspaceConfig {
            name: "grp".to_string(),
            crates: vec![crate_cfg("gamma", "tools/gamma", "gamma-v{{ .Version }}")],
            ..Default::default()
        }]),
        ..Default::default()
    };
    let root = empty_root();
    let shape = detect_repo_shape(root.path(), Some(&config), None);
    match shape {
        RepoShape::PerCrate(groups) => {
            let names: Vec<Vec<&str>> = groups
                .iter()
                .map(|g| g.iter().map(|c| c.name.as_str()).collect())
                .collect();
            assert_eq!(
                names,
                vec![vec!["gamma"], vec!["alpha", "beta"], vec!["delta"]],
                "leftover shared-prefix subset must aggregate; unique prefix stays singleton"
            );
        }
        other => panic!(
            "expected PerCrate, got {:?}",
            std::mem::discriminant(&other)
        ),
    }
}

/// The coherence guard fires on a divergent shared-prefix SUBSET of a
/// flat list — the shape is `PerCrate` (gamma keeps it from collapsing to
/// `FlatAggregate`), but alpha/beta still share the `v*` namespace and
/// must agree on `[package].version`.
#[test]
fn coherence_guard_rejects_divergent_flat_prefix_subset() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    for (name, ver) in [("alpha", "0.5.0"), ("beta", "0.1.0"), ("gamma", "0.9.0")] {
        let dir = root.join(format!("crates/{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"{ver}\"\n"),
        )
        .unwrap();
    }
    let config = anodizer_core::config::Config {
        project_name: "p".to_string(),
        crates: vec![
            crate_cfg("alpha", "crates/alpha", "v{{ .Version }}"),
            crate_cfg("beta", "crates/beta", "v{{ .Version }}"),
            crate_cfg("gamma", "crates/gamma", "gamma-v{{ .Version }}"),
        ],
        ..Default::default()
    };
    let err = guard_flat_aggregate_coherence(Some(&config), Some(&ws_no_lockstep()), root)
        .unwrap_err()
        .to_string();
    assert!(err.contains("alpha") && err.contains("beta"), "{err}");
    assert!(err.contains("0.5.0") && err.contains("0.1.0"), "{err}");
    assert!(
        !err.contains("gamma"),
        "gamma has its own namespace and must not be blamed: {err}"
    );
}

/// An agreeing shared-prefix subset passes even when the independent
/// crate's version differs — `gamma` mints tags into its OWN namespace,
/// so its version never conflicts with the `v*` aggregate.
#[test]
fn coherence_guard_passes_agreeing_flat_subset_with_divergent_singleton() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    for (name, ver) in [("alpha", "0.2.0"), ("beta", "0.2.0"), ("gamma", "0.9.0")] {
        let dir = root.join(format!("crates/{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"{ver}\"\n"),
        )
        .unwrap();
    }
    let config = anodizer_core::config::Config {
        project_name: "p".to_string(),
        crates: vec![
            crate_cfg("alpha", "crates/alpha", "v{{ .Version }}"),
            crate_cfg("beta", "crates/beta", "v{{ .Version }}"),
            crate_cfg("gamma", "crates/gamma", "gamma-v{{ .Version }}"),
        ],
        ..Default::default()
    };
    let res = guard_flat_aggregate_coherence(Some(&config), Some(&ws_no_lockstep()), root);
    assert!(res.is_ok(), "independent singleton must not trip: {res:?}");
}

/// The divergent-subset rule in the MIXED shape: leftovers alpha/beta
/// share `v*` with divergent versions alongside a `workspaces:` group →
/// error; the singleton leftover stays out of it.
#[test]
fn coherence_guard_rejects_divergent_mixed_leftover_subset() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    for (name, ver) in [("alpha", "0.5.0"), ("beta", "0.1.0"), ("delta", "0.9.0")] {
        let dir = root.join(format!("crates/{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"{ver}\"\n"),
        )
        .unwrap();
    }
    let config = anodizer_core::config::Config {
        project_name: "mixed".to_string(),
        crates: vec![
            crate_cfg("alpha", "crates/alpha", "v{{ .Version }}"),
            crate_cfg("beta", "crates/beta", "v{{ .Version }}"),
            crate_cfg("delta", "crates/delta", "delta-v{{ .Version }}"),
        ],
        workspaces: Some(vec![anodizer_core::config::WorkspaceConfig {
            name: "grp".to_string(),
            crates: vec![crate_cfg("gamma", "tools/gamma", "gamma-v{{ .Version }}")],
            ..Default::default()
        }]),
        ..Default::default()
    };
    let err = guard_flat_aggregate_coherence(Some(&config), Some(&ws_no_lockstep()), root)
        .unwrap_err()
        .to_string();
    assert!(err.contains("alpha") && err.contains("beta"), "{err}");
    assert!(
        !err.contains("delta"),
        "delta has its own namespace and must not be blamed: {err}"
    );
}

// ---- anodizer-output line format tests ----

#[test]
fn anodizer_output_format_empty() {
    let crates: Vec<String> = vec![];
    let json = serde_json::to_string(&crates).unwrap();
    assert_eq!(json, "[]");
    let line = format!("anodizer-output crates={}", json);
    assert_eq!(line, "anodizer-output crates=[]");
}

#[test]
fn anodizer_output_format_single_crate() {
    let crates = vec!["myproj-core".to_string()];
    let json = serde_json::to_string(&crates).unwrap();
    let line = format!("anodizer-output crates={}", json);
    assert_eq!(line, "anodizer-output crates=[\"myproj-core\"]");
}

#[test]
fn anodizer_output_format_multi_crate() {
    let crates = vec!["core".to_string(), "bin-a".to_string(), "bin-b".to_string()];
    let json = serde_json::to_string(&crates).unwrap();
    let line = format!("anodizer-output crates={}", json);
    assert_eq!(
        line,
        "anodizer-output crates=[\"core\",\"bin-a\",\"bin-b\"]"
    );
}

#[test]
fn anodizer_output_versions_format_empty() {
    // Zero-change push must emit a stable `versions={}` literal so
    // downstream `fromJson()` parsers always see a valid empty object.
    let versions: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let json = serde_json::to_string(&versions).unwrap();
    assert_eq!(json, "{}");
}

#[test]
fn anodizer_output_versions_format_single_crate() {
    let mut versions = std::collections::HashMap::new();
    versions.insert("cfgd-core".to_string(), "0.4.0".to_string());
    let json = serde_json::to_string(&versions).unwrap();
    // serde_json::to_string for a single-entry map is deterministic.
    assert_eq!(json, "{\"cfgd-core\":\"0.4.0\"}");
}

// -----------------------------------------------------------------------
// skip_ci_suffix
// -----------------------------------------------------------------------

#[test]
fn skip_ci_suffix_on_appends_marker_with_leading_space() {
    assert_eq!(skip_ci_suffix(true), " [skip ci]");
}

#[test]
fn skip_ci_suffix_off_is_empty() {
    assert_eq!(skip_ci_suffix(false), "");
}

// -----------------------------------------------------------------------
// shared_tag_prefix
// -----------------------------------------------------------------------

#[test]
fn shared_tag_prefix_uniform_prefix_returns_it() {
    let crates = vec![
        crate_cfg("a", "crates/a", "v{{ .Version }}"),
        crate_cfg("b", "crates/b", "v{{ .Version }}"),
    ];
    assert_eq!(shared_tag_prefix(&crates), Some("v".to_string()));
}

#[test]
fn shared_tag_prefix_divergent_prefixes_returns_none() {
    let crates = vec![
        crate_cfg("a", "crates/a", "a-v{{ .Version }}"),
        crate_cfg("b", "crates/b", "b-v{{ .Version }}"),
    ];
    assert_eq!(shared_tag_prefix(&crates), None);
}

#[test]
fn shared_tag_prefix_single_crate_returns_its_prefix() {
    let crates = vec![crate_cfg("core", "crates/core", "core-v{{ .Version }}")];
    assert_eq!(shared_tag_prefix(&crates), Some("core-v".to_string()));
}

#[test]
fn shared_tag_prefix_empty_slice_returns_none() {
    assert_eq!(shared_tag_prefix(&[]), None);
}

// -----------------------------------------------------------------------
// message_has_token (whole-word, not substring)
// -----------------------------------------------------------------------

#[test]
fn message_has_token_matches_standalone_word() {
    assert!(message_has_token("fix: a bug #patch", "#patch"));
}

#[test]
fn message_has_token_rejects_substring_within_word() {
    assert!(!message_has_token("this is #handsome", "#hand"));
    assert!(!message_has_token("#patches galore", "#patch"));
}

#[test]
fn message_has_token_matches_token_anywhere_in_whitespace_split() {
    assert!(message_has_token(
        "subject\nbody line #major footer",
        "#major"
    ));
}

// -----------------------------------------------------------------------
// detect_conventional_bump
// -----------------------------------------------------------------------

#[test]
fn detect_conventional_bump_feat_is_minor() {
    let msgs = vec!["feat: add thing".to_string()];
    assert_eq!(detect_conventional_bump(&msgs), Some(BumpKind::Minor));
}

#[test]
fn detect_conventional_bump_fix_is_patch() {
    let msgs = vec!["fix(core): correct it".to_string()];
    assert_eq!(detect_conventional_bump(&msgs), Some(BumpKind::Patch));
}

#[test]
fn detect_conventional_bump_breaking_shorthand_is_major() {
    let msgs = vec!["feat!: drop old API".to_string()];
    assert_eq!(detect_conventional_bump(&msgs), Some(BumpKind::Major));
}

#[test]
fn detect_conventional_bump_chore_only_is_none() {
    let msgs = vec!["chore: bump deps".to_string(), "docs: tweak".to_string()];
    assert_eq!(detect_conventional_bump(&msgs), None);
}

#[test]
fn detect_conventional_bump_major_wins_over_minor_and_patch() {
    let msgs = vec![
        "fix: x".to_string(),
        "feat: y".to_string(),
        "refactor!: z".to_string(),
    ];
    assert_eq!(detect_conventional_bump(&msgs), Some(BumpKind::Major));
}

/// The same commit corpus must classify identically through the `tag`
/// consumer (`detect_conventional_bump`, feeding the auto-tag precedence
/// layers) and the `bump` consumer (`inference::classify`, feeding the
/// dry-run plan) — the two commands previewing/cutting different releases
/// from the same range is the drift this pins against.
#[test]
fn conventional_classification_is_lockstep_between_tag_and_bump() {
    use crate::commands::bump::inference;
    use crate::commands::bump::plan::BumpLevel;

    let corpus: &[(&[&str], Option<BumpKind>)] = &[
        (&["revert: undo broken feature"], Some(BumpKind::Patch)),
        (
            &["feat: x\n\nBREAKING CHANGE removed the old endpoint"],
            Some(BumpKind::Major),
        ),
        (
            &["fix: y\n\nBREAKING-CHANGE: dropped the flag"],
            Some(BumpKind::Major),
        ),
        (&["feat!: drop legacy auth"], Some(BumpKind::Major)),
        (&["feat(core)!: rewrite pipeline"], Some(BumpKind::Major)),
        (&["refactor!: drop the shim"], Some(BumpKind::Major)),
        (&["feat: new stage"], Some(BumpKind::Minor)),
        (&["feat(build): add cache key"], Some(BumpKind::Minor)),
        (&["fix: race"], Some(BumpKind::Patch)),
        (&["perf: faster loop"], Some(BumpKind::Patch)),
        (&["feat(broken: unclosed scope"], Some(BumpKind::Minor)),
        (&["chore: deps", "docs: tweak"], None),
        (&["random subject"], None),
        (&["fix: a", "feat: b", "chore: c"], Some(BumpKind::Minor)),
        (&["fix: a", "feat!: b"], Some(BumpKind::Major)),
    ];

    for (msgs, expected_tag) in corpus {
        let msgs: Vec<String> = msgs.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            detect_conventional_bump(&msgs),
            *expected_tag,
            "tag-side classification for {msgs:?}"
        );
        let expected_bump = match expected_tag {
            Some(BumpKind::Major) => BumpLevel::Major,
            Some(BumpKind::Minor) => BumpLevel::Minor,
            Some(BumpKind::Patch) => BumpLevel::Patch,
            Some(BumpKind::None) | None => BumpLevel::Skip,
        };
        let (level, _) = inference::classify(&msgs);
        assert_eq!(
            level, expected_bump,
            "bump-side classification for {msgs:?}"
        );
    }
}

// -----------------------------------------------------------------------
// plan_changelog_targets / collapse_targets_to_flat_aggregate /
// plan_version_files_rewrites — small fixture builder for GroupTagResult.
// -----------------------------------------------------------------------

fn group_result(
    crate_names: &[&str],
    new_tags: &[(&str, &str)],
    version_updates: &[(&str, &str)],
    old_version: Option<&str>,
    prev_tag: Option<&str>,
    crate_version_files: Vec<Vec<String>>,
) -> GroupTagResult {
    GroupTagResult {
        crate_names: crate_names.iter().map(|s| s.to_string()).collect(),
        new_tags: new_tags
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect(),
        version_updates: version_updates
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect(),
        old_version: old_version.map(str::to_string),
        prev_tag: prev_tag.map(str::to_string),
        crate_version_files,
    }
}

#[test]
fn plan_changelog_targets_one_target_per_bumped_crate() {
    let root = Path::new("/ws");
    let groups = vec![
        group_result(
            &["core"],
            &[("core-v0.2.0", "msg")],
            &[("crates/core", "0.2.0")],
            Some("0.1.0"),
            Some("core-v0.1.0"),
            vec![vec![]],
        ),
        group_result(
            &["cli"],
            &[("cli-v1.0.0", "msg")],
            &[("crates/cli", "1.0.0")],
            None,
            None,
            vec![vec![]],
        ),
    ];
    let targets = plan_changelog_targets(root, &groups);
    assert_eq!(targets.len(), 2);
    assert_eq!(targets[0].crate_name, "core");
    assert_eq!(targets[0].crate_dir, root.join("crates/core"));
    assert_eq!(targets[0].from_tag.as_deref(), Some("core-v0.1.0"));
    assert_eq!(targets[0].to_version, "0.2.0");
    assert_eq!(targets[0].full_tag, "core-v0.2.0");
    assert_eq!(targets[1].crate_name, "cli");
    assert_eq!(targets[1].from_tag, None);
    assert_eq!(targets[1].full_tag, "cli-v1.0.0");
}

#[test]
fn collapse_targets_to_flat_aggregate_collapses_lockstep_set() {
    let root = Path::new("/ws");
    let groups = vec![group_result(
        &["a", "b"],
        &[("v0.5.0", "m"), ("v0.5.0", "m")],
        &[("crates/a", "0.5.0"), ("crates/b", "0.5.0")],
        Some("0.4.0"),
        Some("v0.4.0"),
        vec![vec![], vec![]],
    )];
    let mut targets = plan_changelog_targets(root, &groups);
    assert_eq!(targets.len(), 2, "precondition: two per-crate targets");
    let config = anodizer_core::config::Config {
        project_name: "myproj".to_string(),
        ..Default::default()
    };
    let collapsed = collapse_targets_to_flat_aggregate(&mut targets, root, Some(&config), true);
    assert!(collapsed);
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].crate_name, "myproj");
    assert_eq!(targets[0].crate_dir, root.to_path_buf());
    assert_eq!(targets[0].from_tag.as_deref(), Some("v0.4.0"));
    assert_eq!(targets[0].to_version, "0.5.0");
}

#[test]
fn collapse_targets_to_flat_aggregate_noop_when_collapse_false() {
    let root = Path::new("/ws");
    let mut targets = plan_changelog_targets(
        root,
        &[group_result(
            &["a", "b"],
            &[("v1.0.0", "m"), ("v1.0.0", "m")],
            &[("crates/a", "1.0.0"), ("crates/b", "1.0.0")],
            Some("0.9.0"),
            Some("v0.9.0"),
            vec![vec![], vec![]],
        )],
    );
    let config = anodizer_core::config::Config::default();
    let collapsed = collapse_targets_to_flat_aggregate(&mut targets, root, Some(&config), false);
    assert!(!collapsed);
    assert_eq!(targets.len(), 2, "targets must be left untouched");
}

#[test]
fn collapse_targets_to_flat_aggregate_noop_for_single_target() {
    let root = Path::new("/ws");
    let mut targets = plan_changelog_targets(
        root,
        &[group_result(
            &["solo"],
            &[("v1.0.0", "m")],
            &[("crates/solo", "1.0.0")],
            Some("0.9.0"),
            Some("v0.9.0"),
            vec![vec![]],
        )],
    );
    let config = anodizer_core::config::Config::default();
    assert!(!collapse_targets_to_flat_aggregate(
        &mut targets,
        root,
        Some(&config),
        true
    ));
    assert_eq!(targets.len(), 1);
}

#[test]
fn plan_version_files_rewrites_dedupes_identical_lockstep_pair() {
    // Two crates in one group enroll the same file with the same (old,new):
    // a lockstep set dedupes to a single rewrite.
    let groups = vec![group_result(
        &["a", "b"],
        &[("v0.2.0", "m"), ("v0.2.0", "m")],
        &[("crates/a", "0.2.0"), ("crates/b", "0.2.0")],
        Some("0.1.0"),
        Some("v0.1.0"),
        vec![vec!["README.md".to_string()], vec!["README.md".to_string()]],
    )];
    let plan = plan_version_files_rewrites(&groups).unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].file, "README.md");
    assert_eq!(plan[0].old, "0.1.0");
    assert_eq!(plan[0].new, "0.2.0");
}

#[test]
fn plan_version_files_rewrites_conflicting_old_versions_bail() {
    // Two crates enroll the SAME file but bump from different old versions:
    // a file cannot hold two source versions in one tag run.
    let groups = vec![
        group_result(
            &["a"],
            &[("a-v0.2.0", "m")],
            &[("crates/a", "0.2.0")],
            Some("0.1.0"),
            Some("a-v0.1.0"),
            vec![vec!["shared.txt".to_string()]],
        ),
        group_result(
            &["b"],
            &[("b-v0.2.0", "m")],
            &[("crates/b", "0.2.0")],
            Some("0.1.5"),
            Some("b-v0.1.5"),
            vec![vec!["shared.txt".to_string()]],
        ),
    ];
    let err = plan_version_files_rewrites(&groups)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("version_files conflict") && err.contains("shared.txt"),
        "conflict must name the file, got: {err}"
    );
}

#[test]
fn plan_version_files_rewrites_skips_group_with_no_old_version() {
    // A first-tag group (old_version=None) has nothing to rewrite from.
    let groups = vec![group_result(
        &["new"],
        &[("new-v0.1.0", "m")],
        &[("crates/new", "0.1.0")],
        None,
        None,
        vec![vec!["VERSION".to_string()]],
    )];
    let plan = plan_version_files_rewrites(&groups).unwrap();
    assert!(plan.is_empty());
}
