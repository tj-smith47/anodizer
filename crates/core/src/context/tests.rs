use super::skip::NON_PUBLISHER_RELEASE_SKIPS;
use super::*;
use crate::config::Config;
use crate::git::{GitInfo, SemVer};
use crate::test_helpers::env::env_mutex;
use std::collections::BTreeSet;

/// A `StageLogger` built via `Context::logger` before a secret is minted
/// into `env_source` (e.g. crates.io Trusted Publishing overlaying
/// `CARGO_REGISTRY_TOKEN` mid-run via `begin_cargo_trusted_publishing`)
/// must still redact that secret: the logger holds a live handle to the
/// context's redaction table, not a frozen construction-time snapshot.
#[test]
fn stage_logger_redacts_secret_minted_after_construction() {
    let mut ctx = Context::new(Config::default(), ContextOptions::default());
    ctx.set_env_source(crate::MapEnvSource::new());
    let log = ctx.logger("cargo");

    ctx.set_env_source(crate::MapEnvSource::new().with("SOMETHING_TOKEN", "supersecret123"));

    let redacted = log.redact("publish failed: token=supersecret123 rejected");
    assert!(
        !redacted.contains("supersecret123"),
        "logger built before the mint must still redact a secret added afterward: {redacted}"
    );
    assert!(
        redacted.contains("$SOMETHING_TOKEN"),
        "redacted output should substitute the env-var name: {redacted}"
    );
}

/// `env_for_redact` must honor an injected/sealed `env_source` instead of
/// unconditionally reading `std::env::vars()` — otherwise a hermetic test
/// that seals its env can still leak an unrelated real ambient
/// secret-suffixed var into a `StageLogger`'s redaction table (silently
/// masking substrings of literal test fixture text that happen to
/// collide with the ambient value).
#[test]
fn env_for_redact_honors_injected_env_source_not_real_process_env() {
    let _g = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let key = "ANODIZER_T3_ENV_REDACT_FIXTURE_TOKEN";
    // SAFETY: serialised by env_mutex; cleaned up before guard drop.
    // env-ok: contract test for env_for_redact source routing; unique key.
    unsafe { std::env::set_var(key, "should-not-leak") };

    let mut ctx = Context::new(Config::default(), ContextOptions::default());
    ctx.set_env_source(crate::MapEnvSource::new());
    let log = ctx.logger("test");
    let redacted = log.redact("value=should-not-leak");

    // SAFETY: serialised by env_mutex.
    // env-ok: contract test for env_for_redact source routing; unique key.
    unsafe { std::env::remove_var(key) };

    assert_eq!(
        redacted, "value=should-not-leak",
        "a sealed env_source must not let the real ambient var mask this literal value"
    );
}

/// `VALID_RELEASE_SKIPS` MUST recognize every publisher token. Driven off
/// [`PublisherKind::iter`] so a newly added publisher that is not folded
/// into the `--skip` vocabulary trips immediately. Pins the nine tokens
/// that had silently dropped out of the former hand-maintained literal.
#[test]
fn valid_release_skips_is_superset_of_every_publisher_token() {
    let skips: BTreeSet<&str> = VALID_RELEASE_SKIPS.iter().copied().collect();
    for k in PublisherKind::iter() {
        assert!(
            skips.contains(k.token()),
            "VALID_RELEASE_SKIPS missing publisher token `{}` — `--skip={}` would be \
             silently rejected",
            k.token(),
            k.token(),
        );
    }
    for previously_missing in [
        "npm",
        "gemfury",
        "cloudsmith",
        "artifactory",
        "uploads",
        "dockerhub",
        "mcp",
        "schemastore",
        "upstream-aur",
    ] {
        assert!(
            skips.contains(previously_missing),
            "publisher token `{previously_missing}` (one of the nine that had dropped out \
             of the old literal) is still not a recognized --skip value"
        );
    }
}

/// The non-publisher half of the vocabulary must stay disjoint from the
/// publisher tokens, so the union has a single, unambiguous owner per
/// token. (`snapcraft`/`snapcraft-publish` and `release`/`github-release`
/// are the deliberately-distinct stage-vs-publisher pairs.)
#[test]
fn non_publisher_release_skips_disjoint_from_publisher_tokens() {
    let publisher_tokens: BTreeSet<&str> =
        PublisherKind::iter().map(PublisherKind::token).collect();
    for stage in NON_PUBLISHER_RELEASE_SKIPS {
        assert!(
            !publisher_tokens.contains(stage),
            "`{stage}` is listed in NON_PUBLISHER_RELEASE_SKIPS but is also a publisher token"
        );
    }
}

/// By construction: the token set `anodizer vocabulary` emits equals
/// [`VALID_RELEASE_SKIPS`] exactly — same members, no duplicates. Both are
/// derived from the same SSOT ([`NON_PUBLISHER_RELEASE_SKIPS`] ∪
/// [`PublisherKind::iter`]), so a newly added publisher or stage token
/// flows into both at once; this pins that they can never diverge.
#[test]
fn release_skip_vocabulary_token_set_equals_valid_release_skips() {
    let vocab = release_skip_vocabulary();
    let emitted: BTreeSet<&str> = vocab.iter().map(|t| t.token).collect();
    let valid: BTreeSet<&str> = VALID_RELEASE_SKIPS.iter().copied().collect();
    assert_eq!(
        emitted, valid,
        "`anodizer vocabulary` token set drifted from VALID_RELEASE_SKIPS"
    );
    assert_eq!(
        vocab.len(),
        emitted.len(),
        "release_skip_vocabulary emitted a duplicate token"
    );
}

/// Each vocabulary entry classifies itself consistently with the SSOT:
/// publisher entries carry [`PublisherKind::is_publish_stage`]; the
/// non-publisher stage tokens are never marked as publishers or publish
/// stages.
#[test]
fn release_skip_vocabulary_flags_match_publisher_kind() {
    let vocab = release_skip_vocabulary();
    for entry in &vocab {
        if entry.is_publisher {
            let kind = PublisherKind::iter()
                .find(|k| k.token() == entry.token)
                .unwrap_or_else(|| panic!("publisher entry `{}` has no kind", entry.token));
            assert_eq!(
                entry.is_publish_stage,
                kind.is_publish_stage(),
                "is_publish_stage for `{}` drifted from PublisherKind",
                entry.token
            );
        } else {
            assert!(
                !entry.is_publish_stage,
                "non-publisher token `{}` must not be a publish stage",
                entry.token
            );
            assert!(
                NON_PUBLISHER_RELEASE_SKIPS.contains(&entry.token),
                "non-publisher entry `{}` is not in NON_PUBLISHER_RELEASE_SKIPS",
                entry.token
            );
        }
    }
}

fn make_git_info(dirty: bool, prerelease: Option<&str>) -> GitInfo {
    let tag = match prerelease {
        Some(pre) => format!("v1.2.3-{pre}"),
        None => "v1.2.3".to_string(),
    };
    GitInfo {
        tag,
        commit: "abc123def456abc123def456abc123def456abc1".to_string(),
        short_commit: "abc123d".to_string(),
        branch: "main".to_string(),
        dirty,
        semver: SemVer {
            major: 1,
            minor: 2,
            patch: 3,
            prerelease: prerelease.map(|s| s.to_string()),
            build_metadata: None,
        },
        commit_date: "2026-03-25T10:30:00+00:00".to_string(),
        commit_timestamp: "1774463400".to_string(),
        previous_tag: Some("v1.2.2".to_string()),
        remote_url: "https://github.com/test/repo.git".to_string(),
        summary: "v1.2.3-0-gabc123d".to_string(),
        tag_subject: "Release v1.2.3".to_string(),
        tag_contents: "Release v1.2.3\n\nFull release notes here.".to_string(),
        tag_body: "Full release notes here.".to_string(),
        first_commit: None,
    }
}

#[test]
fn test_context_template_vars() {
    let mut config = Config::default();
    config.project_name = "test-project".to_string();
    let ctx = Context::new(config, ContextOptions::default());
    assert_eq!(
        ctx.template_vars().get("ProjectName"),
        Some(&"test-project".to_string())
    );
}

#[test]
fn validate_skip_values_hint_dedups_overlapping_vocabulary() {
    // The release skip vocabulary is `VALID_RELEASE_SKIPS ++ publisher
    // names`, which legitimately overlap. A bad token must surface a hint
    // listing each valid option exactly ONCE, in first-seen order — not the
    // doubled list a raw `valid.join(", ")` produces.
    let valid = ["homebrew", "cargo", "npm", "homebrew", "cargo", "uploads"];
    let err = validate_skip_values(&["bogus".to_string()], &valid).unwrap_err();
    let opts = err
        .split("Valid options: ")
        .nth(1)
        .expect("hint must carry a Valid options list");
    assert_eq!(
        opts, "homebrew, cargo, npm, uploads",
        "valid options must be de-duplicated in first-seen order"
    );
}

#[test]
fn validate_skip_values_dedups_repeated_invalid_tokens() {
    // The token must not be a substring of any valid option, or `matches`
    // would count the valid-options hint too (`uploads` contains `upload`).
    let err = validate_skip_values(
        &["bogusxyz".to_string(), "bogusxyz".to_string()],
        &VALID_RELEASE_SKIPS,
    )
    .unwrap_err();
    assert_eq!(
        err.matches("bogusxyz").count(),
        1,
        "a repeated invalid token must be reported once: {err}"
    );
}

#[test]
fn test_context_should_skip() {
    let config = Config::default();
    let opts = ContextOptions {
        skip_stages: vec!["publish".to_string(), "announce".to_string()],
        ..Default::default()
    };
    let ctx = Context::new(config, opts);
    assert!(ctx.should_skip("publish"));
    assert!(ctx.should_skip("announce"));
    assert!(!ctx.should_skip("build"));
}

#[test]
fn publisher_deselected_empty_selectors_runs_everything() {
    let ctx = Context::new(Config::default(), ContextOptions::default());
    assert!(!ctx.publisher_deselected("npm"));
    assert!(!ctx.publisher_deselected("cargo"));
    assert!(!ctx.publisher_deselected("anything"));
}

#[test]
fn publisher_deselected_skip_denylists() {
    let opts = ContextOptions {
        skip_stages: vec!["npm".to_string()],
        ..Default::default()
    };
    let ctx = Context::new(Config::default(), opts);
    assert!(ctx.publisher_deselected("npm"));
    assert!(!ctx.publisher_deselected("cargo"));
}

#[test]
fn publisher_deselected_allowlist_excludes_unlisted() {
    let opts = ContextOptions {
        publisher_allowlist: vec!["cargo".to_string()],
        ..Default::default()
    };
    let ctx = Context::new(Config::default(), opts);
    assert!(!ctx.publisher_deselected("cargo"));
    assert!(ctx.publisher_deselected("npm"));
}

#[test]
fn publisher_deselected_skip_wins_over_allowlist() {
    let opts = ContextOptions {
        skip_stages: vec!["cargo".to_string()],
        publisher_allowlist: vec!["cargo".to_string()],
        ..Default::default()
    };
    let ctx = Context::new(Config::default(), opts);
    assert!(ctx.publisher_deselected("cargo"));
}

#[test]
fn any_publisher_selected_matches_deselection_dual() {
    let opts = ContextOptions {
        publisher_allowlist: vec!["cargo".to_string()],
        ..Default::default()
    };
    let ctx = Context::new(Config::default(), opts);
    assert!(ctx.any_publisher_selected(&["npm", "cargo"]));
    assert!(!ctx.any_publisher_selected(&["npm", "blob"]));
    assert!(!ctx.any_publisher_selected(&[]));
}

#[test]
fn test_context_render_template() {
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    let ctx = Context::new(config, ContextOptions::default());
    let result = ctx.render_template("{{ .ProjectName }}-release").unwrap();
    assert_eq!(result, "myapp-release");
}

#[test]
fn test_populate_git_vars_sets_all_expected_vars() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.git_info = Some(make_git_info(false, None));
    ctx.populate_git_vars();

    let v = ctx.template_vars();
    assert_eq!(v.get("Tag"), Some(&"v1.2.3".to_string()));
    assert_eq!(v.get("Version"), Some(&"1.2.3".to_string()));
    assert_eq!(v.get("RawVersion"), Some(&"1.2.3".to_string()));
    assert_eq!(v.get("Major"), Some(&"1".to_string()));
    assert_eq!(v.get("Minor"), Some(&"2".to_string()));
    assert_eq!(v.get("Patch"), Some(&"3".to_string()));
    assert_eq!(v.get("Prerelease"), Some(&"".to_string()));
    assert_eq!(
        v.get("FullCommit"),
        Some(&"abc123def456abc123def456abc123def456abc1".to_string())
    );
    assert_eq!(v.get("ShortCommit"), Some(&"abc123d".to_string()));
    assert_eq!(v.get("Branch"), Some(&"main".to_string()));
    assert_eq!(
        v.get("CommitDate"),
        Some(&"2026-03-25T10:30:00+00:00".to_string())
    );
    assert_eq!(v.get("CommitTimestamp"), Some(&"1774463400".to_string()));
    assert_eq!(v.get("PreviousTag"), Some(&"v1.2.2".to_string()));
    // Base mirrors the numeric base semver, set before any
    // snapshot/nightly version templating overwrites Version.
    assert_eq!(v.get("Base"), Some(&"1.2.3".to_string()));
}

#[test]
fn test_nightly_build_defaults_to_zero_without_git_info() {
    // No git_info (synthetic snapshot/scratch build): NightlyBuild must
    // render as "0" so version_templates referencing it never fail.
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.git_info = None;
    ctx.populate_git_vars();
    assert_eq!(
        ctx.template_vars().get_structured("NightlyBuild"),
        Some(&serde_json::Value::from(0u64))
    );
}

#[test]
fn test_commit_is_alias_for_full_commit() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.git_info = Some(make_git_info(false, None));
    ctx.populate_git_vars();

    let v = ctx.template_vars();
    assert_eq!(v.get("Commit"), v.get("FullCommit"));
}

#[test]
fn test_populate_git_vars_prerelease() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.git_info = Some(make_git_info(false, Some("rc.1")));
    ctx.populate_git_vars();

    let v = ctx.template_vars();
    assert_eq!(v.get("Version"), Some(&"1.2.3-rc.1".to_string()));
    assert_eq!(v.get("RawVersion"), Some(&"1.2.3".to_string()));
    assert_eq!(v.get("Prerelease"), Some(&"rc.1".to_string()));
}

#[test]
fn test_build_metadata_template_var() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    let mut info = make_git_info(false, None);
    info.tag = "v1.2.3+build.42".to_string();
    info.semver.build_metadata = Some("build.42".to_string());
    ctx.git_info = Some(info);
    ctx.populate_git_vars();

    let v = ctx.template_vars();
    assert_eq!(v.get("BuildMetadata"), Some(&"build.42".to_string()));
    // Version should include build metadata (strip v prefix only)
    assert_eq!(v.get("Version"), Some(&"1.2.3+build.42".to_string()));
}

#[test]
fn test_build_metadata_empty_when_none() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.git_info = Some(make_git_info(false, None));
    ctx.populate_git_vars();

    assert_eq!(
        ctx.template_vars().get("BuildMetadata"),
        Some(&"".to_string())
    );
}

#[test]
fn test_populate_git_vars_monorepo_prefixed_tag() {
    // Workspace tags like "core-v0.3.2" should produce Version="0.3.2",
    // not "core-v0.3.2" (which breaks RPM Version fields and templates).
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    let mut info = make_git_info(false, None);
    info.tag = "core-v0.3.2".to_string();
    info.semver = SemVer {
        major: 0,
        minor: 3,
        patch: 2,
        prerelease: None,
        build_metadata: None,
    };
    ctx.git_info = Some(info);
    ctx.populate_git_vars();

    let v = ctx.template_vars();
    assert_eq!(v.get("Tag"), Some(&"core-v0.3.2".to_string()));
    assert_eq!(v.get("Version"), Some(&"0.3.2".to_string()));
    assert_eq!(v.get("RawVersion"), Some(&"0.3.2".to_string()));
    assert_eq!(v.get("Major"), Some(&"0".to_string()));
    assert_eq!(v.get("Minor"), Some(&"3".to_string()));
    assert_eq!(v.get("Patch"), Some(&"2".to_string()));
}

#[test]
fn test_populate_git_vars_monorepo_prefixed_tag_with_prerelease() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    let mut info = make_git_info(false, None);
    info.tag = "operator-v1.0.0-rc.1".to_string();
    info.semver = SemVer {
        major: 1,
        minor: 0,
        patch: 0,
        prerelease: Some("rc.1".to_string()),
        build_metadata: None,
    };
    ctx.git_info = Some(info);
    ctx.populate_git_vars();

    let v = ctx.template_vars();
    assert_eq!(v.get("Tag"), Some(&"operator-v1.0.0-rc.1".to_string()));
    assert_eq!(v.get("Version"), Some(&"1.0.0-rc.1".to_string()));
    assert_eq!(v.get("RawVersion"), Some(&"1.0.0".to_string()));
}

#[test]
fn test_git_tree_state_clean() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.git_info = Some(make_git_info(false, None));
    ctx.populate_git_vars();

    let v = ctx.template_vars();
    assert_eq!(
        v.get_structured("IsGitDirty"),
        Some(&serde_json::Value::Bool(false))
    );
    assert_eq!(v.get("GitTreeState"), Some(&"clean".to_string()));
}

#[test]
fn test_git_tree_state_dirty() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.git_info = Some(make_git_info(true, None));
    ctx.populate_git_vars();

    let v = ctx.template_vars();
    assert_eq!(
        v.get_structured("IsGitDirty"),
        Some(&serde_json::Value::Bool(true))
    );
    assert_eq!(v.get("GitTreeState"), Some(&"dirty".to_string()));
}

#[test]
fn test_is_snapshot_reflects_context_options() {
    let config = Config::default();
    let opts = ContextOptions {
        snapshot: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    ctx.git_info = Some(make_git_info(false, None));
    ctx.populate_git_vars();

    assert_eq!(
        ctx.template_vars().get_structured("IsSnapshot"),
        Some(&serde_json::Value::Bool(true))
    );

    // Non-snapshot
    let config2 = Config::default();
    let opts2 = ContextOptions {
        snapshot: false,
        ..Default::default()
    };
    let mut ctx2 = Context::new(config2, opts2);
    ctx2.git_info = Some(make_git_info(false, None));
    ctx2.populate_git_vars();

    assert_eq!(
        ctx2.template_vars().get_structured("IsSnapshot"),
        Some(&serde_json::Value::Bool(false))
    );
}

#[test]
fn test_is_draft_defaults_to_false() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.git_info = Some(make_git_info(false, None));
    ctx.populate_git_vars();

    assert_eq!(
        ctx.template_vars().get_structured("IsDraft"),
        Some(&serde_json::Value::Bool(false))
    );
}

#[test]
fn test_previous_tag_empty_when_none() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    let mut info = make_git_info(false, None);
    info.previous_tag = None;
    ctx.git_info = Some(info);
    ctx.populate_git_vars();

    assert_eq!(
        ctx.template_vars().get("PreviousTag"),
        Some(&"".to_string())
    );
}

/// Regression: `populate_time_vars` MUST derive `Date` / `Timestamp` /
/// `Now` (and the calendar fields) from `SOURCE_DATE_EPOCH` when the
/// env var is set — the standard reproducible-build contract the
/// determinism harness depends on. Two from-clean runs of the same
/// commit otherwise emit `dist/metadata.json` files that differ in
/// the embedded `date` field, drifting `metadata.json` AND its
/// `.sha256` sidecar across runs. CI run 25975073213 surfaced this
/// drift on every platform shard before the fix landed.
#[test]
fn populate_time_vars_uses_source_date_epoch_when_set() {
    // 1_715_000_000 = 2024-05-06T12:53:20+00:00 — picked to be safely
    // earlier than wall-clock so a wall-clock-derived assertion would
    // visibly fail.
    let env = crate::MapEnvSource::new().with("SOURCE_DATE_EPOCH", "1715000000");
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.set_env_source(env);
    ctx.populate_time_vars();

    let v = ctx.template_vars();
    assert_eq!(
        v.get("Timestamp"),
        Some(&"1715000000".to_string()),
        "Timestamp must equal SOURCE_DATE_EPOCH seconds"
    );
    assert_eq!(
        v.get("Date"),
        Some(&"2024-05-06T12:53:20+00:00".to_string()),
        "Date must be RFC 3339 derived from SDE"
    );
    assert_eq!(v.get("Year"), Some(&"2024".to_string()));
    assert_eq!(v.get("Month"), Some(&"05".to_string()));
    assert_eq!(v.get("Day"), Some(&"06".to_string()));
}

#[test]
fn test_populate_time_vars() {
    // Wall-clock fallback path: empty MapEnvSource has no
    // SOURCE_DATE_EPOCH, so we exercise the chrono::Utc::now() branch.
    let env = crate::MapEnvSource::new();
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.set_env_source(env);
    ctx.populate_time_vars();

    let v = ctx.template_vars();

    // Date should be RFC 3339 format (e.g. 2026-03-30T12:00:00+00:00)
    let date = v
        .get("Date")
        .unwrap_or_else(|| panic!("Date should be set"));
    assert!(
        date.contains('T') && date.len() > 10,
        "Date should be RFC 3339, got: {date}"
    );

    // Timestamp should be numeric
    let ts = v
        .get("Timestamp")
        .unwrap_or_else(|| panic!("Timestamp should be set"));
    assert!(
        ts.parse::<i64>().is_ok(),
        "Timestamp should be a numeric string, got: {ts}"
    );

    // Now should be ISO 8601
    let now = v.get("Now").unwrap_or_else(|| panic!("Now should be set"));
    assert!(now.contains('T'), "Now should be ISO 8601, got: {now}");
}

#[test]
fn test_env_vars_accessible_in_templates() {
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set_env("MY_VAR", "hello-world");
    ctx.template_vars_mut().set_env("DEPLOY_ENV", "staging");

    let result = ctx
        .render_template("{{ .Env.MY_VAR }}-{{ .Env.DEPLOY_ENV }}")
        .unwrap();
    assert_eq!(result, "hello-world-staging");
}

#[test]
fn test_populate_git_vars_without_git_info_still_sets_snapshot() {
    let config = Config::default();
    let opts = ContextOptions {
        snapshot: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    // Don't set git_info — populate_git_vars should still set IsSnapshot/IsDraft
    ctx.populate_git_vars();

    assert_eq!(
        ctx.template_vars().get_structured("IsSnapshot"),
        Some(&serde_json::Value::Bool(true))
    );
    assert_eq!(
        ctx.template_vars().get_structured("IsDraft"),
        Some(&serde_json::Value::Bool(false))
    );
    // Git-specific vars should NOT be set
    assert_eq!(ctx.template_vars().get("Tag"), None);
}

#[test]
fn test_is_nightly_set_when_nightly_mode_active() {
    let config = Config::default();
    let opts = ContextOptions {
        nightly: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    ctx.git_info = Some(make_git_info(false, None));
    ctx.populate_git_vars();

    assert_eq!(
        ctx.template_vars().get_structured("IsNightly"),
        Some(&serde_json::Value::Bool(true)),
        "IsNightly should be 'true' when nightly mode is active"
    );
    assert!(ctx.is_nightly(), "is_nightly() should return true");
}

#[test]
fn test_is_nightly_false_by_default() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.git_info = Some(make_git_info(false, None));
    ctx.populate_git_vars();

    assert_eq!(
        ctx.template_vars().get_structured("IsNightly"),
        Some(&serde_json::Value::Bool(false)),
        "IsNightly should default to 'false'"
    );
    assert!(
        !ctx.is_nightly(),
        "is_nightly() should return false by default"
    );
}

#[test]
fn test_version_returns_populated_value() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.git_info = Some(make_git_info(false, None));
    ctx.populate_git_vars();

    assert_eq!(ctx.version(), "1.2.3");
}

#[test]
fn test_version_returns_empty_when_not_set() {
    let config = Config::default();
    let ctx = Context::new(config, ContextOptions::default());
    assert_eq!(ctx.version(), "");
}

#[test]
fn test_is_nightly_without_git_info() {
    let config = Config::default();
    let opts = ContextOptions {
        nightly: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    // No git_info set — populate_git_vars still sets IsNightly
    ctx.populate_git_vars();

    assert_eq!(
        ctx.template_vars().get_structured("IsNightly"),
        Some(&serde_json::Value::Bool(true)),
        "IsNightly should be set even without git info"
    );
}

#[test]
fn test_is_git_clean_when_not_dirty() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.git_info = Some(make_git_info(false, None));
    ctx.populate_git_vars();

    assert_eq!(
        ctx.template_vars().get_structured("IsGitClean"),
        Some(&serde_json::Value::Bool(true))
    );
}

#[test]
fn test_is_git_clean_when_dirty() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.git_info = Some(make_git_info(true, None));
    ctx.populate_git_vars();

    assert_eq!(
        ctx.template_vars().get_structured("IsGitClean"),
        Some(&serde_json::Value::Bool(false))
    );
}

#[test]
fn test_git_url_set_from_git_info() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.git_info = Some(make_git_info(false, None));
    ctx.populate_git_vars();

    assert_eq!(
        ctx.template_vars().get("GitURL"),
        Some(&"https://github.com/test/repo.git".to_string())
    );
}

#[test]
fn test_summary_set_from_git_info() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.git_info = Some(make_git_info(false, None));
    ctx.populate_git_vars();

    assert_eq!(
        ctx.template_vars().get("Summary"),
        Some(&"v1.2.3-0-gabc123d".to_string())
    );
}

#[test]
fn test_tag_subject_set_from_git_info() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.git_info = Some(make_git_info(false, None));
    ctx.populate_git_vars();

    assert_eq!(
        ctx.template_vars().get("TagSubject"),
        Some(&"Release v1.2.3".to_string())
    );
}

#[test]
fn test_tag_contents_set_from_git_info() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.git_info = Some(make_git_info(false, None));
    ctx.populate_git_vars();

    assert_eq!(
        ctx.template_vars().get("TagContents"),
        Some(&"Release v1.2.3\n\nFull release notes here.".to_string())
    );
}

#[test]
fn test_tag_body_set_from_git_info() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.git_info = Some(make_git_info(false, None));
    ctx.populate_git_vars();

    assert_eq!(
        ctx.template_vars().get("TagBody"),
        Some(&"Full release notes here.".to_string())
    );
}

#[test]
fn test_is_single_target_false_by_default() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.git_info = Some(make_git_info(false, None));
    ctx.populate_git_vars();

    assert_eq!(
        ctx.template_vars().get_structured("IsSingleTarget"),
        Some(&serde_json::Value::Bool(false))
    );
}

#[test]
fn test_is_single_target_true_when_set() {
    let config = Config::default();
    let opts = ContextOptions {
        single_target: Some("x86_64-unknown-linux-gnu".to_string()),
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    ctx.git_info = Some(make_git_info(false, None));
    ctx.populate_git_vars();

    assert_eq!(
        ctx.template_vars().get_structured("IsSingleTarget"),
        Some(&serde_json::Value::Bool(true))
    );
}

#[test]
#[serial_test::serial]
fn test_populate_runtime_vars() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.populate_runtime_vars();

    let v = ctx.template_vars();

    let goos = v
        .get("RuntimeGoos")
        .unwrap_or_else(|| panic!("RuntimeGoos should be set"));
    assert!(
        !goos.is_empty(),
        "RuntimeGoos should not be empty, got: {goos}"
    );
    // RuntimeGoos uses Go naming (e.g. "darwin" not "macos")
    assert_eq!(goos, map_os_to_goos(std::env::consts::OS));

    let goarch = v
        .get("RuntimeGoarch")
        .unwrap_or_else(|| panic!("RuntimeGoarch should be set"));
    assert!(
        !goarch.is_empty(),
        "RuntimeGoarch should not be empty, got: {goarch}"
    );
    // RuntimeGoarch uses Go naming (e.g. "amd64" not "x86_64")
    assert_eq!(goarch, map_arch_to_goarch(std::env::consts::ARCH));
}

#[test]
fn test_map_arch_to_goarch_matches_shared_table() {
    // Host template vars and triple-derived asset tokens share one table:
    // loongarch64 must reach "loong64" (the former private copy passed it
    // through verbatim, so host renders never matched asset names) and the
    // endian-ambiguous hosts resolve by this build's endianness.
    assert_eq!(map_arch_to_goarch("x86_64"), "amd64");
    assert_eq!(map_arch_to_goarch("aarch64"), "arm64");
    assert_eq!(map_arch_to_goarch("x86"), "386");
    assert_eq!(map_arch_to_goarch("loongarch64"), "loong64");
    assert_eq!(map_arch_to_goarch("sparc64"), "sparc64");
    assert_eq!(
        map_arch_to_goarch("powerpc64"),
        crate::target::rust_arch_to_goarch("powerpc64", cfg!(target_endian = "little")).unwrap()
    );
    // GOARCH for 32-bit ARM really is "arm" — passthrough, not a mapping gap.
    assert_eq!(map_arch_to_goarch("arm"), "arm");
}

#[test]
fn test_populate_release_notes_var_with_changelogs() {
    let mut config = Config::default();
    config.crates.push(crate::config::CrateConfig {
        name: "my-crate".to_string(),
        ..Default::default()
    });
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.stage_outputs
        .changelogs
        .insert("my-crate".to_string(), "## Changes\n- fix bug".to_string());
    ctx.populate_release_notes_var();

    assert_eq!(
        ctx.template_vars().get("ReleaseNotes"),
        Some(&"## Changes\n- fix bug".to_string())
    );
}

#[test]
fn test_populate_release_notes_var_empty_when_no_changelogs() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.populate_release_notes_var();

    assert_eq!(
        ctx.template_vars().get("ReleaseNotes"),
        Some(&"".to_string())
    );
}

#[test]
fn test_populate_release_notes_var_deterministic_with_multiple_crates() {
    let mut config = Config::default();
    config.crates.push(crate::config::CrateConfig {
        name: "crate-a".to_string(),
        ..Default::default()
    });
    config.crates.push(crate::config::CrateConfig {
        name: "crate-b".to_string(),
        ..Default::default()
    });
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.stage_outputs
        .changelogs
        .insert("crate-a".to_string(), "notes-a".to_string());
    ctx.stage_outputs
        .changelogs
        .insert("crate-b".to_string(), "notes-b".to_string());
    ctx.populate_release_notes_var();

    // Should always pick the first crate in config order, not arbitrary HashMap order
    assert_eq!(
        ctx.template_vars().get("ReleaseNotes"),
        Some(&"notes-a".to_string())
    );
}

#[test]
fn test_populate_release_notes_var_sees_workspace_only_crates() {
    // Pure-`workspaces:` config: the crates carrying the changelogs never
    // appear in the top-level `crates:` list, so the lookup must walk the
    // crate universe or `ReleaseNotes` renders empty.
    let config = Config {
        workspaces: Some(vec![crate::config::WorkspaceConfig {
            name: "grp".to_string(),
            crates: vec![crate::config::CrateConfig {
                name: "member".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        }]),
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.stage_outputs
        .changelogs
        .insert("member".to_string(), "## member notes".to_string());
    ctx.populate_release_notes_var();

    assert_eq!(
        ctx.template_vars().get("ReleaseNotes"),
        Some(&"## member notes".to_string()),
        "a workspace-only crate's changelog must populate ReleaseNotes"
    );
}

#[test]
fn test_outputs_accessible_in_templates() {
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set_output("build_id", "abc123");
    ctx.template_vars_mut()
        .set_output("deploy_url", "https://example.com");

    let result = ctx
        .render_template("{{ .Outputs.build_id }}-{{ .Outputs.deploy_url }}")
        .unwrap();
    assert_eq!(result, "abc123-https://example.com");
}

#[test]
fn test_artifact_ext_and_target_template_vars() {
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("ArtifactName", "myapp.tar.gz");
    ctx.template_vars_mut().set("ArtifactExt", ".tar.gz");
    ctx.template_vars_mut()
        .set("Target", "x86_64-unknown-linux-gnu");

    let result = ctx
        .render_template("{{ .ArtifactExt }}_{{ .Target }}")
        .unwrap();
    assert_eq!(result, ".tar.gz_x86_64-unknown-linux-gnu");
}

#[test]
fn test_checksums_template_var() {
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    let mut ctx = Context::new(config, ContextOptions::default());
    let checksum_text = "abc123  myapp.tar.gz\ndef456  myapp.zip\n";
    ctx.template_vars_mut().set("Checksums", checksum_text);

    let result = ctx.render_template("{{ .Checksums }}").unwrap();
    assert_eq!(result, checksum_text);
}

// --- Pro template variable tests ---

#[test]
fn test_prefixed_tag_with_tag_prefix() {
    let mut config = Config::default();
    config.tag = Some(crate::config::TagConfig {
        tag_prefix: Some("api/".to_string()),
        ..Default::default()
    });
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.git_info = Some(make_git_info(false, None));
    ctx.populate_git_vars();

    assert_eq!(
        ctx.template_vars().get("PrefixedTag"),
        Some(&"api/v1.2.3".to_string())
    );
}

#[test]
fn test_prefixed_tag_without_tag_prefix() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.git_info = Some(make_git_info(false, None));
    ctx.populate_git_vars();

    // No tag_prefix configured — PrefixedTag should equal Tag
    assert_eq!(
        ctx.template_vars().get("PrefixedTag"),
        Some(&"v1.2.3".to_string())
    );
}

#[test]
fn test_prefixed_previous_tag_with_tag_prefix() {
    let mut config = Config::default();
    config.tag = Some(crate::config::TagConfig {
        tag_prefix: Some("api/".to_string()),
        ..Default::default()
    });
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.git_info = Some(make_git_info(false, None));
    ctx.populate_git_vars();

    assert_eq!(
        ctx.template_vars().get("PrefixedPreviousTag"),
        Some(&"api/v1.2.2".to_string())
    );
}

#[test]
fn test_prefixed_previous_tag_empty_when_no_previous() {
    let mut config = Config::default();
    config.tag = Some(crate::config::TagConfig {
        tag_prefix: Some("api/".to_string()),
        ..Default::default()
    });
    let mut ctx = Context::new(config, ContextOptions::default());
    let mut info = make_git_info(false, None);
    info.previous_tag = None;
    ctx.git_info = Some(info);
    ctx.populate_git_vars();

    // When there is no previous tag, PrefixedPreviousTag should be empty
    // (not just the prefix).
    assert_eq!(
        ctx.template_vars().get("PrefixedPreviousTag"),
        Some(&"".to_string())
    );
}

#[test]
fn test_prefixed_summary_with_tag_prefix() {
    let mut config = Config::default();
    config.tag = Some(crate::config::TagConfig {
        tag_prefix: Some("api/".to_string()),
        ..Default::default()
    });
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.git_info = Some(make_git_info(false, None));
    ctx.populate_git_vars();

    assert_eq!(
        ctx.template_vars().get("PrefixedSummary"),
        Some(&"api/v1.2.3-0-gabc123d".to_string())
    );
}

#[test]
fn test_is_release_true_for_normal_release() {
    let config = Config::default();
    let opts = ContextOptions {
        snapshot: false,
        nightly: false,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    ctx.git_info = Some(make_git_info(false, None));
    ctx.populate_git_vars();

    assert_eq!(
        ctx.template_vars().get_structured("IsRelease"),
        Some(&serde_json::Value::Bool(true))
    );
}

#[test]
fn test_is_release_false_for_snapshot() {
    let config = Config::default();
    let opts = ContextOptions {
        snapshot: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    ctx.git_info = Some(make_git_info(false, None));
    ctx.populate_git_vars();

    assert_eq!(
        ctx.template_vars().get_structured("IsRelease"),
        Some(&serde_json::Value::Bool(false))
    );
}

#[test]
fn test_is_release_false_for_nightly() {
    let config = Config::default();
    let opts = ContextOptions {
        nightly: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    ctx.git_info = Some(make_git_info(false, None));
    ctx.populate_git_vars();

    assert_eq!(
        ctx.template_vars().get_structured("IsRelease"),
        Some(&serde_json::Value::Bool(false))
    );
}

#[test]
fn test_is_merging_true_when_merge_flag_set() {
    let config = Config::default();
    let opts = ContextOptions {
        merge: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    ctx.git_info = Some(make_git_info(false, None));
    ctx.populate_git_vars();

    assert_eq!(
        ctx.template_vars().get_structured("IsMerging"),
        Some(&serde_json::Value::Bool(true))
    );
}

#[test]
fn test_is_merging_false_by_default() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.git_info = Some(make_git_info(false, None));
    ctx.populate_git_vars();

    assert_eq!(
        ctx.template_vars().get_structured("IsMerging"),
        Some(&serde_json::Value::Bool(false))
    );
}

#[test]
fn test_refresh_artifacts_var_empty() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.refresh_artifacts_var();

    // Should render as an empty array
    let result = ctx
        .render_template("{% for a in Artifacts %}{{ a.name }}{% endfor %}")
        .unwrap();
    assert_eq!(result, "");
}

#[test]
fn test_refresh_artifacts_var_with_artifacts() {
    use crate::artifact::{Artifact, ArtifactKind};
    use std::collections::HashMap;
    use std::path::PathBuf;

    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    // Artifacts are created with empty `name` — ArtifactRegistry::add()
    // auto-derives the name from the path's filename component when name
    // is empty (see artifact.rs add() implementation).
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: PathBuf::from("dist/myapp-1.0.0-linux-amd64.tar.gz"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("format".to_string(), "tar.gz".to_string())]),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("dist/myapp"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });
    ctx.refresh_artifacts_var();

    // Iterate over artifacts and collect names
    let result = ctx
        .render_template("{% for a in Artifacts %}{{ a.name }},{% endfor %}")
        .unwrap();
    assert!(result.contains("myapp-1.0.0-linux-amd64.tar.gz"));
    assert!(result.contains("myapp"));

    // Check kind field
    let result_kinds = ctx
        .render_template("{% for a in Artifacts %}{{ a.kind }},{% endfor %}")
        .unwrap();
    assert!(result_kinds.contains("archive"));
    assert!(result_kinds.contains("binary"));
}

#[test]
fn test_populate_metadata_var_with_mod_timestamp() {
    let mut config = Config::default();
    config.metadata = Some(crate::config::MetadataConfig {
        mod_timestamp: Some("{{ .CommitTimestamp }}".to_string()),
        ..Default::default()
    });
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.populate_metadata_var().unwrap();

    // Metadata should be accessible as a nested map with PascalCase keys
    let result = ctx.render_template("{{ Metadata.ModTimestamp }}").unwrap();
    assert_eq!(result, "{{ .CommitTimestamp }}");
}

#[test]
fn test_populate_metadata_var_empty_when_no_config() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.populate_metadata_var().unwrap();

    // Should render empty strings for missing fields (PascalCase keys)
    let result = ctx.render_template("{{ Metadata.Description }}").unwrap();
    assert_eq!(result, "");
}

#[test]
fn test_populate_metadata_var_reads_from_config() {
    let mut config = Config::default();
    config.metadata = Some(crate::config::MetadataConfig {
        description: Some("A test project".to_string()),
        homepage: Some("https://example.com".to_string()),
        documentation: Some("https://docs.example.com".to_string()),
        license: Some("MIT".to_string()),
        repository: Some("https://github.com/example/test".to_string()),
        maintainers: Some(vec!["Alice".to_string(), "Bob".to_string()]),
        mod_timestamp: Some("1234567890".to_string()),
        ..Default::default()
    });
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.populate_metadata_var().unwrap();

    let desc = ctx.render_template("{{ Metadata.Description }}").unwrap();
    assert_eq!(desc, "A test project");

    let home = ctx.render_template("{{ Metadata.Homepage }}").unwrap();
    assert_eq!(home, "https://example.com");

    let repo = ctx.render_template("{{ Metadata.Repository }}").unwrap();
    assert_eq!(repo, "https://github.com/example/test");

    let docs = ctx.render_template("{{ Metadata.Documentation }}").unwrap();
    assert_eq!(docs, "https://docs.example.com");

    let lic = ctx.render_template("{{ Metadata.License }}").unwrap();
    assert_eq!(lic, "MIT");

    let ts = ctx.render_template("{{ Metadata.ModTimestamp }}").unwrap();
    assert_eq!(ts, "1234567890");
}

#[test]
fn test_populate_metadata_var_license_falls_back_to_derived() {
    // No top-level `metadata.license`: the var must derive from the
    // primary crate's Cargo.toml-derived license (here, a dual SPDX
    // expression), not render empty.
    let mut config = Config::default();
    config.crates = vec![crate::config::CrateConfig {
        name: "anodizer".to_string(),
        ..Default::default()
    }];
    config.derived_metadata.insert(
        "anodizer".to_string(),
        crate::config::MetadataConfig {
            description: Some("Derived desc".to_string()),
            homepage: Some("https://derived.example".to_string()),
            documentation: Some("https://derived.docs".to_string()),
            license: Some("MIT OR Apache-2.0".to_string()),
            ..Default::default()
        },
    );
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.populate_metadata_var().unwrap();

    assert_eq!(
        ctx.render_template("{{ Metadata.License }}").unwrap(),
        "MIT OR Apache-2.0"
    );
    assert_eq!(
        ctx.render_template("{{ Metadata.Description }}").unwrap(),
        "Derived desc"
    );
    assert_eq!(
        ctx.render_template("{{ Metadata.Homepage }}").unwrap(),
        "https://derived.example"
    );
    assert_eq!(
        ctx.render_template("{{ Metadata.Documentation }}").unwrap(),
        "https://derived.docs"
    );
}

#[test]
fn test_populate_metadata_var_top_level_license_wins_over_derived() {
    // Explicit top-level `metadata.license` still wins over the derived
    // Cargo.toml value.
    let mut config = Config::default();
    config.crates = vec![crate::config::CrateConfig {
        name: "anodizer".to_string(),
        ..Default::default()
    }];
    config.derived_metadata.insert(
        "anodizer".to_string(),
        crate::config::MetadataConfig {
            license: Some("MIT OR Apache-2.0".to_string()),
            ..Default::default()
        },
    );
    config.metadata = Some(crate::config::MetadataConfig {
        license: Some("GPL-3.0".to_string()),
        ..Default::default()
    });
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.populate_metadata_var().unwrap();

    assert_eq!(
        ctx.render_template("{{ Metadata.License }}").unwrap(),
        "GPL-3.0"
    );
}

#[test]
fn test_populate_metadata_var_documentation_renders() {
    let mut config = Config::default();
    config.metadata = Some(crate::config::MetadataConfig {
        documentation: Some("https://docs.rs/anodizer".to_string()),
        ..Default::default()
    });
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.populate_metadata_var().unwrap();

    let docs = ctx.render_template("{{ Metadata.Documentation }}").unwrap();
    assert_eq!(docs, "https://docs.rs/anodizer");
}

#[test]
fn test_populate_metadata_var_documentation_empty_when_unset() {
    let mut ctx = Context::new(Config::default(), ContextOptions::default());
    ctx.populate_metadata_var().unwrap();

    let docs = ctx.render_template("{{ Metadata.Documentation }}").unwrap();
    assert_eq!(docs, "");
}

#[test]
fn test_populate_metadata_var_full_description_inline() {
    use crate::config::ContentSource;
    let mut config = Config::default();
    config.metadata = Some(crate::config::MetadataConfig {
        full_description: Some(ContentSource::Inline(
            "A long-form description of the project.".to_string(),
        )),
        ..Default::default()
    });
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.populate_metadata_var().unwrap();
    let rendered = ctx
        .render_template("{{ Metadata.FullDescription }}")
        .unwrap();
    assert_eq!(rendered, "A long-form description of the project.");
}

#[test]
fn test_populate_metadata_var_full_description_from_file() {
    use crate::config::ContentSource;
    let tmp = tempfile::tempdir().unwrap();
    let desc_path = tmp.path().join("DESCRIPTION.md");
    std::fs::write(&desc_path, "read from disk").unwrap();
    let mut config = Config::default();
    config.metadata = Some(crate::config::MetadataConfig {
        full_description: Some(ContentSource::FromFile {
            from_file: desc_path.to_string_lossy().into_owned(),
        }),
        ..Default::default()
    });
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.populate_metadata_var().unwrap();
    let rendered = ctx
        .render_template("{{ Metadata.FullDescription }}")
        .unwrap();
    assert_eq!(rendered, "read from disk");
}

#[test]
fn test_populate_metadata_var_full_description_from_url_resolves() {
    // `from_url` routes through the shared `content_source::resolve`
    // helper. We stand up a oneshot HTTP responder so the test is
    // hermetic (no real network) and verify the body lands in the
    // rendered Metadata.FullDescription variable.
    use crate::config::ContentSource;
    use crate::test_helpers::responder::spawn_oneshot_http_responder;

    let body = "long form description body";
    let body_len = body.len();
    let response: &'static str = Box::leak(
        format!("HTTP/1.1 200 OK\r\nContent-Length: {body_len}\r\n\r\n{body}").into_boxed_str(),
    );
    let (addr, _calls) = spawn_oneshot_http_responder(vec![response]);

    let mut config = Config::default();
    config.metadata = Some(crate::config::MetadataConfig {
        full_description: Some(ContentSource::FromUrl {
            from_url: format!("http://{addr}/description.md"),
            headers: None,
        }),
        ..Default::default()
    });
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.populate_metadata_var()
        .expect("from_url should resolve through content_source");
    let rendered = ctx
        .render_template("{{ Metadata.FullDescription }}")
        .unwrap();
    assert_eq!(rendered, body);
}

#[test]
fn test_populate_metadata_var_commit_author() {
    use crate::config::CommitAuthorConfig;
    let mut config = Config::default();
    config.metadata = Some(crate::config::MetadataConfig {
        commit_author: Some(CommitAuthorConfig {
            name: Some("Alice Developer".to_string()),
            email: Some("alice@example.com".to_string()),
            signing: None,
            use_github_app_token: false,
        }),
        ..Default::default()
    });
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.populate_metadata_var().unwrap();
    let name = ctx
        .render_template("{{ Metadata.CommitAuthor.Name }}")
        .unwrap();
    assert_eq!(name, "Alice Developer");
    let email = ctx
        .render_template("{{ Metadata.CommitAuthor.Email }}")
        .unwrap();
    assert_eq!(email, "alice@example.com");
}

#[test]
fn test_artifact_id_template_var() {
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("ArtifactID", "default");

    let result = ctx.render_template("{{ .ArtifactID }}").unwrap();
    assert_eq!(result, "default");
}

#[test]
fn test_artifact_id_empty_when_not_set() {
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("ArtifactID", "");

    let result = ctx.render_template("{{ .ArtifactID }}").unwrap();
    assert_eq!(result, "");
}

#[test]
fn test_pro_vars_rendered_in_templates() {
    // Test that all Pro vars can be used in templates together
    let mut config = Config::default();
    config.tag = Some(crate::config::TagConfig {
        tag_prefix: Some("api/".to_string()),
        ..Default::default()
    });
    let opts = ContextOptions {
        snapshot: false,
        nightly: false,
        merge: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    ctx.git_info = Some(make_git_info(false, None));
    ctx.populate_git_vars();

    let result = ctx
        .render_template(
            "{% if IsRelease %}release{% endif %}-{% if IsMerging %}merge{% endif %}-{{ .PrefixedTag }}",
        )
        .unwrap();
    assert_eq!(result, "release-merge-api/v1.2.3");
}

#[test]
fn test_is_release_without_git_info() {
    // IsRelease should still be set even without git info
    let config = Config::default();
    let opts = ContextOptions {
        snapshot: false,
        nightly: false,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    ctx.populate_git_vars();

    assert_eq!(
        ctx.template_vars().get_structured("IsRelease"),
        Some(&serde_json::Value::Bool(true))
    );
}

#[test]
fn test_is_merging_without_git_info() {
    // IsMerging should still be set even without git info
    let config = Config::default();
    let opts = ContextOptions {
        merge: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    ctx.populate_git_vars();

    assert_eq!(
        ctx.template_vars().get_structured("IsMerging"),
        Some(&serde_json::Value::Bool(true))
    );
}

// -----------------------------------------------------------------------
// Monorepo template variable tests
// -----------------------------------------------------------------------

/// Parity proof: in monorepo mode `populate_git_vars` derives `Version`
/// from the shared `SemVer::version_string()` helper — the SAME source the
/// build stage's per-crate `crate_template_overrides` uses — so the two
/// can't drift. Exercised with a prerelease + build-metadata tag, the case
/// where the old raw string-strip and the struct derivation could diverge.
#[test]
fn test_monorepo_version_matches_shared_semver_helper() {
    let mut config = Config::default();
    config.monorepo = Some(crate::config::MonorepoConfig {
        tag_prefix: Some("core/".to_string()),
        dir: None,
    });
    let mut ctx = Context::new(config, ContextOptions::default());

    let semver = SemVer {
        major: 2,
        minor: 1,
        patch: 0,
        prerelease: Some("rc.1".to_string()),
        build_metadata: Some("build.7".to_string()),
    };
    let mut info = make_git_info(false, None);
    info.tag = "core/v2.1.0-rc.1+build.7".to_string();
    info.semver = semver.clone();
    ctx.git_info = Some(info);
    ctx.populate_git_vars();

    let v = ctx.template_vars();
    // populate_git_vars (monorepo path) and the build stage's per-crate
    // derivation both route through SemVer::version_string().
    assert_eq!(v.get("Version"), Some(&semver.version_string()));
    assert_eq!(v.get("Version"), Some(&"2.1.0-rc.1+build.7".to_string()));
    assert_eq!(v.get("RawVersion"), Some(&semver.raw_version_string()));
    assert_eq!(v.get("RawVersion"), Some(&"2.1.0".to_string()));
    // Tag is still the monorepo-stripped value.
    assert_eq!(v.get("Tag"), Some(&"v2.1.0-rc.1+build.7".to_string()));
}

#[test]
fn test_monorepo_tag_prefix_strips_tag_for_template_var() {
    let mut config = Config::default();
    config.monorepo = Some(crate::config::MonorepoConfig {
        tag_prefix: Some("subproject1/".to_string()),
        dir: None,
    });
    let mut ctx = Context::new(config, ContextOptions::default());

    // Simulate a monorepo tag: the full prefixed tag is stored in git_info.
    let mut info = make_git_info(false, None);
    info.tag = "subproject1/v1.2.3".to_string();
    info.previous_tag = Some("subproject1/v1.2.2".to_string());
    info.summary = "subproject1/v1.2.3-0-gabc123d".to_string();
    ctx.git_info = Some(info);
    ctx.populate_git_vars();

    let v = ctx.template_vars();
    // Tag should have the prefix stripped.
    assert_eq!(v.get("Tag"), Some(&"v1.2.3".to_string()));
    // Version should derive from stripped tag.
    assert_eq!(v.get("Version"), Some(&"1.2.3".to_string()));
    // PrefixedTag should retain the full tag.
    assert_eq!(
        v.get("PrefixedTag"),
        Some(&"subproject1/v1.2.3".to_string())
    );
    // PreviousTag should be stripped (consistent with Tag).
    assert_eq!(v.get("PreviousTag"), Some(&"v1.2.2".to_string()));
    // PrefixedPreviousTag should retain the full tag.
    assert_eq!(
        v.get("PrefixedPreviousTag"),
        Some(&"subproject1/v1.2.2".to_string())
    );
    // Summary should be stripped.
    assert_eq!(v.get("Summary"), Some(&"v1.2.3-0-gabc123d".to_string()));
    // PrefixedSummary should retain the full summary.
    assert_eq!(
        v.get("PrefixedSummary"),
        Some(&"subproject1/v1.2.3-0-gabc123d".to_string())
    );
}

#[test]
fn test_monorepo_prefixed_previous_tag() {
    let mut config = Config::default();
    config.monorepo = Some(crate::config::MonorepoConfig {
        tag_prefix: Some("svc/".to_string()),
        dir: None,
    });
    let mut ctx = Context::new(config, ContextOptions::default());

    let mut info = make_git_info(false, None);
    info.tag = "svc/v2.0.0".to_string();
    info.previous_tag = Some("svc/v1.9.0".to_string());
    ctx.git_info = Some(info);
    ctx.populate_git_vars();

    let v = ctx.template_vars();
    // PrefixedPreviousTag should be the full previous tag.
    assert_eq!(
        v.get("PrefixedPreviousTag"),
        Some(&"svc/v1.9.0".to_string())
    );
    // PreviousTag should be stripped (prefix removed), consistent with Tag.
    assert_eq!(v.get("PreviousTag"), Some(&"v1.9.0".to_string()));
}

#[test]
fn test_no_monorepo_falls_back_to_tag_prefix() {
    // When monorepo is not set, PrefixedTag should use tag.tag_prefix.
    let mut config = Config::default();
    config.tag = Some(crate::config::TagConfig {
        tag_prefix: Some("release/".to_string()),
        ..Default::default()
    });
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.git_info = Some(make_git_info(false, None));
    ctx.populate_git_vars();

    let v = ctx.template_vars();
    // Tag is plain "v1.2.3" (not stripped because no monorepo).
    assert_eq!(v.get("Tag"), Some(&"v1.2.3".to_string()));
    // PrefixedTag should prepend tag_prefix.
    assert_eq!(v.get("PrefixedTag"), Some(&"release/v1.2.3".to_string()));
    assert_eq!(
        v.get("PrefixedPreviousTag"),
        Some(&"release/v1.2.2".to_string())
    );
}

#[test]
fn test_monorepo_overrides_tag_prefix_for_prefixed_vars() {
    // When both monorepo.tag_prefix and tag.tag_prefix are set,
    // monorepo should take precedence for PrefixedTag.
    let mut config = Config::default();
    config.tag = Some(crate::config::TagConfig {
        tag_prefix: Some("release/".to_string()),
        ..Default::default()
    });
    config.monorepo = Some(crate::config::MonorepoConfig {
        tag_prefix: Some("svc/".to_string()),
        dir: None,
    });
    let mut ctx = Context::new(config, ContextOptions::default());

    let mut info = make_git_info(false, None);
    info.tag = "svc/v1.2.3".to_string();
    info.previous_tag = Some("svc/v1.2.2".to_string());
    ctx.git_info = Some(info);
    ctx.populate_git_vars();

    let v = ctx.template_vars();
    // Monorepo takes precedence: Tag is stripped.
    assert_eq!(v.get("Tag"), Some(&"v1.2.3".to_string()));
    // PrefixedTag is the full monorepo tag, NOT tag_prefix-prepended.
    assert_eq!(v.get("PrefixedTag"), Some(&"svc/v1.2.3".to_string()));
}

#[test]
fn test_monorepo_prefixed_summary() {
    let mut config = Config::default();
    config.monorepo = Some(crate::config::MonorepoConfig {
        tag_prefix: Some("pkg/".to_string()),
        dir: None,
    });
    let mut ctx = Context::new(config, ContextOptions::default());

    let mut info = make_git_info(false, None);
    info.tag = "pkg/v1.2.3".to_string();
    // In a real monorepo, `git describe` already includes the prefix in the summary.
    info.summary = "pkg/v1.2.3-0-gabc123d".to_string();
    ctx.git_info = Some(info);
    ctx.populate_git_vars();

    // PrefixedSummary is info.summary as-is (already contains prefix).
    assert_eq!(
        ctx.template_vars().get("PrefixedSummary"),
        Some(&"pkg/v1.2.3-0-gabc123d".to_string())
    );
    // Summary should have the prefix stripped.
    assert_eq!(
        ctx.template_vars().get("Summary"),
        Some(&"v1.2.3-0-gabc123d".to_string())
    );
}

#[test]
fn test_monorepo_no_previous_tag() {
    let mut config = Config::default();
    config.monorepo = Some(crate::config::MonorepoConfig {
        tag_prefix: Some("svc/".to_string()),
        dir: None,
    });
    let mut ctx = Context::new(config, ContextOptions::default());

    let mut info = make_git_info(false, None);
    info.tag = "svc/v1.0.0".to_string();
    info.previous_tag = None;
    ctx.git_info = Some(info);
    ctx.populate_git_vars();

    let v = ctx.template_vars();
    assert_eq!(v.get("PrefixedPreviousTag"), Some(&"".to_string()));
    // PreviousTag should also be empty when no previous tag exists.
    assert_eq!(v.get("PreviousTag"), Some(&"".to_string()));
}

// -----------------------------------------------------------------------
// Integration test: full monorepo flow
// -----------------------------------------------------------------------

#[test]
fn test_monorepo_full_flow_all_vars() {
    // End-to-end test: config with monorepo.tag_prefix + dir
    // → context creation → populate_git_vars → verify ALL template vars.
    let mut config = Config::default();
    config.project_name = "mymonorepo".to_string();
    config.monorepo = Some(crate::config::MonorepoConfig {
        tag_prefix: Some("services/api/".to_string()),
        dir: Some("services/api".to_string()),
    });

    // Verify Config helper methods work
    assert_eq!(config.monorepo_tag_prefix(), Some("services/api/"));
    assert_eq!(config.monorepo_dir(), Some("services/api"));

    let mut ctx = Context::new(config, ContextOptions::default());

    // Simulate git info as it would appear in a monorepo:
    // tag and summary already contain the prefix from git.
    let mut info = make_git_info(false, None);
    info.tag = "services/api/v2.1.0".to_string();
    info.previous_tag = Some("services/api/v2.0.5".to_string());
    info.summary = "services/api/v2.1.0-0-gabc123d".to_string();
    info.semver = crate::git::SemVer {
        major: 2,
        minor: 1,
        patch: 0,
        prerelease: None,
        build_metadata: None,
    };
    ctx.git_info = Some(info);
    ctx.populate_git_vars();

    let v = ctx.template_vars();

    // Base vars should have the prefix STRIPPED.
    assert_eq!(v.get("Tag"), Some(&"v2.1.0".to_string()));
    assert_eq!(v.get("Version"), Some(&"2.1.0".to_string()));
    assert_eq!(v.get("RawVersion"), Some(&"2.1.0".to_string()));
    assert_eq!(v.get("Major"), Some(&"2".to_string()));
    assert_eq!(v.get("Minor"), Some(&"1".to_string()));
    assert_eq!(v.get("Patch"), Some(&"0".to_string()));
    assert_eq!(v.get("PreviousTag"), Some(&"v2.0.5".to_string()));
    assert_eq!(v.get("Summary"), Some(&"v2.1.0-0-gabc123d".to_string()));

    // Prefixed vars should retain the FULL prefix.
    assert_eq!(
        v.get("PrefixedTag"),
        Some(&"services/api/v2.1.0".to_string())
    );
    assert_eq!(
        v.get("PrefixedPreviousTag"),
        Some(&"services/api/v2.0.5".to_string())
    );
    assert_eq!(
        v.get("PrefixedSummary"),
        Some(&"services/api/v2.1.0-0-gabc123d".to_string())
    );

    // Project name should be available.
    assert_eq!(v.get("ProjectName"), Some(&"mymonorepo".to_string()));
}

#[test]
fn render_template_for_version_blanks_semver_parts_on_non_semver() {
    // Context version is 2.0.0 — its Major/Minor/Patch must NOT leak into a
    // non-semver `--version` render.
    let mut ctx = Context::new(Config::default(), ContextOptions::default());
    let vars = ctx.template_vars_mut();
    vars.set("Version", "2.0.0");
    vars.set("Major", "2");
    vars.set("Minor", "0");
    vars.set("Patch", "0");

    let rendered = ctx
        .render_template_for_version(
            "{{ .Major }}.{{ .Minor }}.{{ .Patch }}",
            "not-a-semver",
            "not-a-semver",
        )
        .expect("render");
    // The context version (parts) must not leak — semver-part vars are blanked.
    assert!(
        !rendered.contains('2') && !rendered.contains("2.0.0"),
        "context version leaked into non-semver render: {rendered:?}"
    );

    // The raw `Version` var still resolves to the supplied non-semver string.
    let version_only = ctx
        .render_template_for_version("{{ .Version }}", "not-a-semver", "vX")
        .expect("render");
    assert_eq!(version_only, "not-a-semver");
}

#[test]
fn context_env_var_defaults_to_process_env_source() {
    let ctx = Context::new(Config::default(), ContextOptions::default());
    // A deliberately weird name no real shell will ever export.
    assert_eq!(ctx.env_var("ANODIZER_T3_UNSET_VAR"), None);
}

#[test]
fn context_env_var_routes_to_injected_source() {
    let mut ctx = Context::new(Config::default(), ContextOptions::default());
    ctx.set_env_source(crate::MapEnvSource::new().with("INJECTED", "yes"));
    assert_eq!(ctx.env_var("INJECTED"), Some("yes".to_string()));
    // The injected source REPLACES the process source — `PATH` is set
    // in every realistic execution environment, but the map does not
    // know about it, so the read must return `None`.
    assert_eq!(ctx.env_var("PATH"), None);
}

#[test]
fn retry_deadline_is_some_when_config_sets_max_elapsed() {
    let mut config = Config::default();
    config.retry = Some(crate::config::RetryConfig {
        max_elapsed: Some(crate::config::HumanDuration(
            std::time::Duration::from_secs(15 * 60),
        )),
        ..Default::default()
    });
    let ctx = Context::new(config, ContextOptions::default());
    assert!(
        ctx.retry_deadline().is_some(),
        "retry.max_elapsed: 15m must resolve to a wall-clock deadline"
    );
}

#[test]
fn retry_deadline_defaults_to_the_built_in_budget_when_config_omits_retry() {
    let ctx = Context::new(Config::default(), ContextOptions::default());
    let before = std::time::Instant::now() + crate::retry::DEFAULT_MAX_ELAPSED;
    let deadline = ctx
        .retry_deadline()
        .expect("an omitted retry config must still yield the default budget");
    let after = std::time::Instant::now() + crate::retry::DEFAULT_MAX_ELAPSED;
    // The deadline anchors at call time + the 15m default, so it lands within
    // the [before, after] window bracketing this call.
    assert!(deadline >= before && deadline <= after);
}

#[test]
#[serial_test::serial]
fn populate_runtime_vars_sets_rustc_version() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    // RustcVersion is folded into populate_runtime_vars — exercising the
    // public entry point proves the delegation wires the var through.
    ctx.populate_runtime_vars();

    let ver = ctx
        .template_vars()
        .get("RustcVersion")
        .expect("RustcVersion should be set after populate_runtime_vars");
    // On a host with rustc on PATH the var must be non-empty and start
    // with a digit (e.g. "1.96.0").  On a host without rustc the var is
    // empty but must still be present (no missing-key footgun).
    if !ver.is_empty() {
        assert!(
            ver.chars().next().is_some_and(|c| c.is_ascii_digit()),
            "RustcVersion should start with a digit: {ver}"
        );
    }
}
