#![allow(clippy::field_reassign_with_default)]

use super::*;
use anodizer_core::artifact::Artifact;
use anodizer_core::config::{CloudSmithConfig, Config, StringOrBool};
use anodizer_core::context::{Context, ContextOptions};
use std::path::PathBuf;

fn dry_run_ctx(config: Config) -> Context {
    Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    )
}

/// The per-entry summary reports the uploaded artifact count and the
/// already-present skip count taken from the loop's own tallies, so an
/// entry that uploaded 4 and skipped 1 renders `uploaded 4 …, skipped 1`.
/// CloudSmith requires a distribution for deb/alpine; when the user
/// configures none, the accept-all catch-all keeps the package
/// installable. rpm/srpm/raw need no distribution and return None.
#[test]
fn cloudsmith_default_distribution_per_format() {
    assert_eq!(
        cloudsmith_default_distribution("deb"),
        Some("any-distro/any-version")
    );
    assert_eq!(
        cloudsmith_default_distribution("alpine"),
        Some("alpine/any-version")
    );
    assert_eq!(cloudsmith_default_distribution("rpm"), None);
    assert_eq!(cloudsmith_default_distribution("srpm"), None);
    assert_eq!(cloudsmith_default_distribution("raw"), None);
}

#[test]
fn upload_summary_reflects_uploaded_and_skipped_counts() {
    let line = cloudsmith_upload_summary(4, 1, "acme", "stable");
    assert_eq!(
        line,
        "uploaded 4 artifact(s), skipped 1 (already present) → acme/stable"
    );
}

/// A fully-idempotent re-run (nothing newly uploaded) still renders a
/// factual summary with a zero upload count rather than suppressing it.
#[test]
fn upload_summary_handles_zero_uploads() {
    let line = cloudsmith_upload_summary(0, 3, "acme", "edge");
    assert_eq!(
        line,
        "uploaded 0 artifact(s), skipped 3 (already present) → acme/edge"
    );
}

fn entry(slug: &str, version: &str, uploaded_at: &str) -> CloudsmithVersionEntry {
    CloudsmithVersionEntry {
        slug: slug.to_string(),
        version: version.to_string(),
        uploaded_at: uploaded_at.to_string(),
    }
}

// keep_versions=2 over 4 versions: the 2 oldest are deleted, the 2
// newest (incl. the current upload) are kept.
#[test]
fn prune_keep_2_of_4_deletes_two_oldest() {
    let entries = vec![
        entry("s-070", "0.7.0", "2026-06-13T00:00:00Z"),
        entry("s-061", "0.6.1", "2026-05-13T00:00:00Z"),
        entry("s-060", "0.6.0", "2026-04-13T00:00:00Z"),
        entry("s-050", "0.5.0", "2026-03-13T00:00:00Z"),
    ];
    let mut to_delete = select_versions_to_prune(&entries, 2, "0.7.0");
    to_delete.sort();
    assert_eq!(to_delete, vec!["s-050".to_string(), "s-060".to_string()]);
}

// keep_versions never deletes the current upload even when ranking would
// otherwise rank it out of the top-N (e.g. a hotfix re-cut of an older
// line, or skewed timestamps).
#[test]
fn prune_never_deletes_current_version() {
    let entries = vec![
        entry("s-090", "0.9.0", "2026-06-13T00:00:00Z"),
        entry("s-081", "0.8.1", "2026-06-12T00:00:00Z"),
        entry("s-080", "0.8.0", "2026-06-11T00:00:00Z"),
    ];
    // keep=1 would normally keep only 0.9.0, but current is 0.8.0.
    let to_delete = select_versions_to_prune(&entries, 1, "0.8.0");
    assert!(
        !to_delete.contains(&"s-080".to_string()),
        "current version must never be pruned: {to_delete:?}"
    );
    // 0.9.0 (top-1) and 0.8.0 (current) kept; 0.8.1 pruned.
    assert_eq!(to_delete, vec!["s-081".to_string()]);
}

// All formats of one release (deb epoch `1:0.9.1-1`, apk `0.9.1-r1`, rpm
// `0.9.1-1`, bare `0.9.1`) normalize to `0.9.1` and rank as ONE version.
#[test]
fn prune_normalizes_epoch_and_revision_into_one_version() {
    let entries = vec![
        entry("deb-091", "1:0.9.1-1", "2026-06-13T00:00:00Z"),
        entry("apk-091", "0.9.1-r1", "2026-06-13T00:00:00Z"),
        entry("rpm-091", "0.9.1-1", "2026-06-13T00:00:00Z"),
        entry("deb-090", "1:0.9.0-1", "2026-05-13T00:00:00Z"),
        entry("apk-090", "0.9.0-r1", "2026-05-13T00:00:00Z"),
    ];
    // keep=1, current 0.9.1 → keep all three 0.9.1 artifacts, prune both
    // 0.9.0 artifacts.
    let mut to_delete = select_versions_to_prune(&entries, 1, "0.9.1");
    to_delete.sort();
    assert_eq!(
        to_delete,
        vec!["apk-090".to_string(), "deb-090".to_string()]
    );
}

#[test]
fn normalize_strips_epoch_and_revision() {
    assert_eq!(normalize_cloudsmith_version("1:0.9.1-1"), "0.9.1");
    assert_eq!(normalize_cloudsmith_version("0.9.1-r1"), "0.9.1");
    assert_eq!(normalize_cloudsmith_version("0.9.1-1"), "0.9.1");
    assert_eq!(normalize_cloudsmith_version("0.9.1"), "0.9.1");
    assert_eq!(normalize_cloudsmith_version("2:1.2.3-5"), "1.2.3");
    // SemVer prerelease tails survive (not a packaging revision).
    assert_eq!(normalize_cloudsmith_version("0.9.1-rc.1"), "0.9.1-rc.1");
    assert_eq!(normalize_cloudsmith_version("0.9.1-alpha"), "0.9.1-alpha");
    // A non-numeric prerelease tail is NOT a packaging revision and
    // survives intact (head `0.9.1` is bare SemVer but tail `beta` isn't
    // `r?<digits>`).
    assert_eq!(normalize_cloudsmith_version("0.9.1-beta"), "0.9.1-beta");
    // A deb revision ON a prerelease strips only the trailing revision,
    // keeping the prerelease: head `1.0.0-rc.1` parses as SemVer, tail `1`
    // is a numeric revision. `1.0.0-rc-1` likewise → `1.0.0-rc`.
    assert_eq!(normalize_cloudsmith_version("1.0.0-rc.1-1"), "1.0.0-rc.1");
    assert_eq!(normalize_cloudsmith_version("1.0.0-rc-1"), "1.0.0-rc");
    // A tail that isn't `r?<digits>` is never stripped even with a SemVer
    // head, so a true single-segment prerelease is safe.
    assert_eq!(normalize_cloudsmith_version("1.0.0-rc"), "1.0.0-rc");
}

// The operator "kept …" summary must name exactly the versions that
// survive deletion — both go through the same comparator.
#[test]
fn retained_summary_matches_selection() {
    let entries = vec![
        entry("s-100", "1.0.0", "2026-06-13T00:00:00Z"),
        entry("s-091", "0.9.1", "2026-05-13T00:00:00Z"),
        entry("s-090", "0.9.0", "2026-04-13T00:00:00Z"),
    ];
    let to_delete = select_versions_to_prune(&entries, 2, "1.0.0");
    // 1.0.0 + 0.9.1 kept, 0.9.0 pruned.
    assert_eq!(to_delete, vec!["s-090".to_string()]);
    let summary = retained_version_summary(&entries, 2, "1.0.0");
    assert_eq!(summary, "1.0.0, 0.9.1");
}

// keep=0 refuses to prune anything (belt-and-braces; caller rejects 0).
#[test]
fn prune_keep_zero_deletes_nothing() {
    let entries = vec![
        entry("s-090", "0.9.0", "2026-06-13T00:00:00Z"),
        entry("s-080", "0.8.0", "2026-06-12T00:00:00Z"),
    ];
    assert!(select_versions_to_prune(&entries, 0, "0.9.0").is_empty());
}

// Versions that won't parse as SemVer fall back to uploaded_at ordering
// and rank below any parseable version.
#[test]
fn prune_unparseable_versions_fall_back_to_timestamp() {
    let entries = vec![
        entry("s-good", "1.0.0", "2026-01-01T00:00:00Z"),
        entry("s-new", "nightly-xyz", "2026-06-13T00:00:00Z"),
        entry("s-old", "nightly-abc", "2026-05-13T00:00:00Z"),
    ];
    // keep=2, current 1.0.0: parseable 1.0.0 ranks first and is kept;
    // among the two unparseable, the newer (s-new) takes the 2nd slot,
    // so s-old is pruned.
    let to_delete = select_versions_to_prune(&entries, 2, "1.0.0");
    assert_eq!(to_delete, vec!["s-old".to_string()]);
}

#[test]
fn test_cloudsmith_skips_when_no_config() {
    let config = Config::default();
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("cloudsmith");
    assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
}

#[test]
fn test_cloudsmith_skips_when_empty_vec() {
    let mut config = Config::default();
    config.cloudsmiths = Some(vec![]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("cloudsmith");
    assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
}

#[test]
fn test_cloudsmith_skips_when_skipped() {
    let mut config = Config::default();
    config.cloudsmiths = Some(vec![CloudSmithConfig {
        skip: Some(StringOrBool::Bool(true)),
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("cloudsmith");
    assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
}

#[test]
fn test_cloudsmith_skips_when_skip_string_true() {
    let mut config = Config::default();
    config.cloudsmiths = Some(vec![CloudSmithConfig {
        skip: Some(StringOrBool::String("true".to_string())),
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("cloudsmith");
    assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
}

#[test]
fn test_cloudsmith_requires_organization() {
    let mut config = Config::default();
    config.cloudsmiths = Some(vec![CloudSmithConfig {
        organization: None,
        repository: Some("myrepo".to_string()),
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("cloudsmith");
    let err = publish_to_cloudsmith(&ctx, &log).unwrap_err();
    assert!(
        err.to_string().contains("'organization' is required"),
        "unexpected error: {}",
        err
    );
}

#[test]
fn test_cloudsmith_requires_organization_nonempty() {
    let mut config = Config::default();
    config.cloudsmiths = Some(vec![CloudSmithConfig {
        organization: Some(String::new()),
        repository: Some("myrepo".to_string()),
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("cloudsmith");
    let err = publish_to_cloudsmith(&ctx, &log).unwrap_err();
    assert!(
        err.to_string().contains("'organization' is required"),
        "unexpected error: {}",
        err
    );
}

#[test]
fn test_cloudsmith_requires_repository() {
    let mut config = Config::default();
    config.cloudsmiths = Some(vec![CloudSmithConfig {
        organization: Some("myorg".to_string()),
        repository: None,
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("cloudsmith");
    let err = publish_to_cloudsmith(&ctx, &log).unwrap_err();
    assert!(
        err.to_string().contains("'repository' is required"),
        "unexpected error: {}",
        err
    );
}

#[test]
fn test_cloudsmith_requires_repository_nonempty() {
    let mut config = Config::default();
    config.cloudsmiths = Some(vec![CloudSmithConfig {
        organization: Some("myorg".to_string()),
        repository: Some(String::new()),
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("cloudsmith");
    let err = publish_to_cloudsmith(&ctx, &log).unwrap_err();
    assert!(
        err.to_string().contains("'repository' is required"),
        "unexpected error: {}",
        err
    );
}

#[test]
fn test_cloudsmith_upload_url() {
    // Display-only helper (dry-run logs). Live code uses the 3-step API
    // flow against api.cloudsmith.io, not a single upload URL.
    let url = cloudsmith_upload_url("myorg", "myrepo", "deb", "ubuntu/focal");
    assert_eq!(
        url,
        format!(
            "{}/packages/myorg/myrepo/upload/deb/ (distribution=ubuntu/focal)",
            CLOUDSMITH_API_BASE
        )
    );
}

#[test]
fn test_cloudsmith_default_formats() {
    let defaults = cloudsmith_default_formats();
    assert_eq!(defaults, vec!["apk", "deb", "rpm"]);
}

#[test]
fn test_cloudsmith_dry_run() {
    let mut config = Config::default();
    config.cloudsmiths = Some(vec![CloudSmithConfig {
        organization: Some("myorg".to_string()),
        repository: Some("myrepo".to_string()),
        formats: Some(vec!["deb".to_string()]),
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("cloudsmith");
    assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
}

#[test]
fn test_cloudsmith_dry_run_default_formats() {
    let mut config = Config::default();
    config.cloudsmiths = Some(vec![CloudSmithConfig {
        organization: Some("myorg".to_string()),
        repository: Some("myrepo".to_string()),
        formats: None,
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("cloudsmith");
    assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
}

#[test]
fn test_cloudsmith_dry_run_with_ids_filter() {
    let mut config = Config::default();
    config.cloudsmiths = Some(vec![CloudSmithConfig {
        organization: Some("myorg".to_string()),
        repository: Some("myrepo".to_string()),
        ids: Some(vec!["build1".to_string(), "build2".to_string()]),
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("cloudsmith");
    assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
}

#[test]
fn test_cloudsmith_dry_run_with_distributions() {
    use anodizer_core::config::CloudSmithDistributions;

    let mut distributions = HashMap::new();
    distributions.insert(
        "deb".to_string(),
        CloudSmithDistributions::Single("ubuntu/focal".to_string()),
    );

    let mut config = Config::default();
    config.cloudsmiths = Some(vec![CloudSmithConfig {
        organization: Some("myorg".to_string()),
        repository: Some("myrepo".to_string()),
        distributions: Some(distributions),
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("cloudsmith");
    assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
}

/// YAML array form (`deb: ["ubuntu/focal", "ubuntu/jammy"]`) parses
/// into [`CloudSmithDistributions::Multiple`].
#[test]
fn distributions_array_form_parses() {
    use anodizer_core::config::CloudSmithDistributions;
    let yaml = "deb:\n  - ubuntu/focal\n  - ubuntu/jammy\n";
    let parsed: HashMap<String, CloudSmithDistributions> = serde_yaml_ng::from_str(yaml).unwrap();
    match parsed.get("deb").unwrap() {
        CloudSmithDistributions::Multiple(v) => {
            assert_eq!(
                v,
                &vec!["ubuntu/focal".to_string(), "ubuntu/jammy".to_string()]
            );
        }
        other => panic!("expected Multiple, got {:?}", other),
    }
}

/// `.src.rpm` files map to the `srpm` format slug (NOT `rpm`).
#[test]
fn detect_format_distinguishes_src_rpm() {
    assert_eq!(detect_format("pkg-1.0-1.src.rpm"), "srpm");
    assert_eq!(detect_format("pkg-1.0-1.x86_64.rpm"), "rpm");
    assert_eq!(
        detect_format("pkg-1.0-1.SRC.rpm"),
        "srpm",
        "case-insensitive"
    );
}

/// `cloudsmith_format_matches` accepts both `apk` (user-facing) and
/// `alpine` (API-side) spellings.
#[test]
fn format_matches_apk_and_alpine_aliases() {
    assert!(cloudsmith_format_matches("pkg.apk", &["apk".to_string()]));
    assert!(cloudsmith_format_matches(
        "pkg.apk",
        &["alpine".to_string()]
    ));
}

/// `cloudsmith_format_matches` recognises both `srpm` and `src.rpm`
/// filter slugs against a `.src.rpm` file.
#[test]
fn format_matches_srpm_aliases() {
    assert!(cloudsmith_format_matches(
        "pkg-1.0-1.src.rpm",
        &["srpm".to_string()]
    ));
    assert!(cloudsmith_format_matches(
        "pkg-1.0-1.src.rpm",
        &["src.rpm".to_string()]
    ));
}

#[test]
fn test_cloudsmith_dry_run_with_component() {
    let mut config = Config::default();
    config.cloudsmiths = Some(vec![CloudSmithConfig {
        organization: Some("myorg".to_string()),
        repository: Some("myrepo".to_string()),
        component: Some("main".to_string()),
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("cloudsmith");
    assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
}

#[test]
fn test_cloudsmith_dry_run_with_republish() {
    let mut config = Config::default();
    config.cloudsmiths = Some(vec![CloudSmithConfig {
        organization: Some("myorg".to_string()),
        repository: Some("myrepo".to_string()),
        republish: Some(StringOrBool::Bool(true)),
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("cloudsmith");
    assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
}

#[test]
fn test_cloudsmith_dry_run_default_secret_name() {
    let mut config = Config::default();
    config.cloudsmiths = Some(vec![CloudSmithConfig {
        organization: Some("myorg".to_string()),
        repository: Some("myrepo".to_string()),
        secret_name: None,
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("cloudsmith");
    assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
}

#[test]
fn test_cloudsmith_multiple_entries() {
    let mut config = Config::default();
    config.cloudsmiths = Some(vec![
        CloudSmithConfig {
            organization: Some("org1".to_string()),
            repository: Some("repo1".to_string()),
            ..Default::default()
        },
        CloudSmithConfig {
            organization: Some("org2".to_string()),
            repository: Some("repo2".to_string()),
            skip: Some(StringOrBool::Bool(true)),
            ..Default::default()
        },
    ]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("cloudsmith");
    assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
}

#[test]
fn test_cloudsmith_live_mode_errors_without_token() {
    let mut config = Config::default();
    config.cloudsmiths = Some(vec![CloudSmithConfig {
        organization: Some("myorg".to_string()),
        repository: Some("myrepo".to_string()),
        secret_name: Some("CLOUDSMITH_TEST_NONEXISTENT_TOKEN_12345".to_string()),
        ..Default::default()
    }]);
    let ctx = Context::new(
        config,
        ContextOptions {
            dry_run: false,
            ..Default::default()
        },
    );
    let log = ctx.logger("cloudsmith");
    let result = publish_to_cloudsmith(&ctx, &log);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("CLOUDSMITH_TEST_NONEXISTENT_TOKEN_12345"),
        "error should mention the secret env var name, got: {}",
        msg
    );
}

#[test]
fn test_cloudsmith_format_matches() {
    let formats = vec!["deb".to_string(), "rpm".to_string()];
    assert!(cloudsmith_format_matches("myapp_1.0.0_amd64.deb", &formats));
    assert!(cloudsmith_format_matches(
        "myapp-1.0.0.x86_64.rpm",
        &formats
    ));
    assert!(!cloudsmith_format_matches("myapp-1.0.0.tar.gz", &formats));
}

#[test]
fn test_cloudsmith_format_matches_apk() {
    let formats = vec!["apk".to_string()];
    assert!(cloudsmith_format_matches("myapp-1.0.0.apk", &formats));
    assert!(!cloudsmith_format_matches("myapp-1.0.0.deb", &formats));
}

#[test]
fn test_cloudsmith_format_matches_empty_formats() {
    let formats: Vec<String> = vec![];
    assert!(!cloudsmith_format_matches("myapp.deb", &formats));
}

#[test]
fn test_detect_format() {
    assert_eq!(detect_format("app.deb"), "deb");
    assert_eq!(detect_format("app.rpm"), "rpm");
    assert_eq!(detect_format("app.apk"), "alpine");
    assert_eq!(detect_format("app.tar.gz"), "raw");
}

#[test]
fn test_cloudsmith_dry_run_lists_matching_artifacts() {
    let mut config = Config::default();
    config.project_name = "testapp".to_string();
    config.cloudsmiths = Some(vec![CloudSmithConfig {
        organization: Some("myorg".to_string()),
        repository: Some("myrepo".to_string()),
        ..Default::default()
    }]);
    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::LinuxPackage,
        name: "testapp_1.0.0_amd64.deb".to_string(),
        path: PathBuf::from("dist/testapp_1.0.0_amd64.deb"),
        target: None,
        crate_name: "testapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::LinuxPackage,
        name: "testapp-1.0.0.x86_64.rpm".to_string(),
        path: PathBuf::from("dist/testapp-1.0.0.x86_64.rpm"),
        target: None,
        crate_name: "testapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    let log = ctx.logger("cloudsmith");
    assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
}

/// Defense-in-depth: a Cloudsmith API error response that echoes our
/// `Authorization: Bearer <PAT>` header back must not leak the token
/// into the user-visible error chain. Exercises the `retry_request`
/// helper's error-message closure via a one-shot TCP responder.
#[test]
fn retry_request_redacts_bearer_in_error_body() {
    use anodizer_core::log::{StageLogger, Verbosity};
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
    use std::time::Duration;

    let leaky = "Authorization: Bearer ghp_FAKETOKEN1234567890abcdefg";
    let body_len = leaky.len();
    let resp: &'static str = Box::leak(
        format!("HTTP/1.1 500 Internal Server Error\r\nContent-Length: {body_len}\r\n\r\n{leaky}")
            .into_boxed_str(),
    );

    // Serve up to 3 identical attempts (matches fast_policy max_attempts).
    let (addr, _calls) = spawn_oneshot_http_responder(vec![resp; 3]);

    let policy = RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    };
    let log = StageLogger::new("cloudsmith", Verbosity::Normal);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");
    let url = format!("http://{addr}/files/");
    let err = retry_request("upload", "test.deb", &policy, None, &log, || {
        client.post(&url).send()
    })
    .expect_err("500 must exhaust + error");
    let chain = format!("{err:#}");
    assert!(
        !chain.contains("ghp_FAKETOKEN1234567890abcdefg"),
        "bearer token leaked into error chain: {chain}"
    );
    assert!(
        chain.contains("<redacted>"),
        "expected `<redacted>` marker in error chain: {chain}"
    );
}

/// Multi-distribution upload must stage a fresh files/create slot +
/// presigned upload PER distribution: a Cloudsmith identifier is consumed
/// by a single package-create, so reusing one across distributions makes
/// the 2nd+ package-create 4xx.
///
/// Two distributions ⇒ each needs its own (files/create + presigned +
/// package-create) = 6 served connections. The bug (file stage hoisted
/// out of the loop) would serve only 4 (1 files/create + 1 presigned +
/// 2 package-creates). The connection count is the load-bearing assertion.
#[test]
fn cloudsmith_multi_distribution_stages_one_file_per_distro() {
    use anodizer_core::MapEnvSource;
    use anodizer_core::config::CloudSmithDistributions;
    use anodizer_core::log::{StageLogger, Verbosity};
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder_with;
    use std::sync::atomic::Ordering;

    let tmp = tempfile::tempdir().unwrap();
    let art_path = tmp.path().join("app_1.0.0_amd64.deb");
    std::fs::write(&art_path, b"fake-deb-bytes").unwrap();

    let http_json = |body: String| -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
    };
    let presigned_ok = || "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n".to_string();

    // Build the response queue AFTER the responder binds so the
    // files/create `upload_url` can point the presigned upload (step 2)
    // back at this same responder. Served one-per-connection, in order;
    // per distribution the client opens three connections in sequence:
    //   files/create -> presigned upload -> packages/upload.
    let (addr, calls) = spawn_oneshot_http_responder_with(|addr| {
        let base = format!("http://{addr}");
        let files_create = |id: &str| {
            http_json(format!(
                r#"{{"identifier":"{id}","upload_url":"{base}/s3-presigned/","upload_fields":{{"key":"v"}}}}"#
            ))
        };
        vec![
            files_create("id-distro-1"),
            presigned_ok(),
            http_json(r#"{"slug_perm":"slug-1"}"#.to_string()),
            files_create("id-distro-2"),
            presigned_ok(),
            http_json(r#"{"slug_perm":"slug-2"}"#.to_string()),
        ]
    });
    let base = format!("http://{addr}");

    let mut distros: HashMap<String, CloudSmithDistributions> = HashMap::new();
    distros.insert(
        "deb".to_string(),
        CloudSmithDistributions::Multiple(vec![
            "ubuntu/focal".to_string(),
            "ubuntu/jammy".to_string(),
        ]),
    );

    let mut config = Config::default();
    config.project_name = "app".to_string();
    config.cloudsmiths = Some(vec![CloudSmithConfig {
        organization: Some("myorg".to_string()),
        repository: Some("myrepo".to_string()),
        distributions: Some(distros),
        // republish=true skips the pre-check packages-list query so the
        // response queue stays exactly the 3-per-distro upload sequence.
        republish: Some(StringOrBool::Bool(true)),
        ..Default::default()
    }]);

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.set_env_source(
        MapEnvSource::new()
            .with("CLOUDSMITH_TOKEN", "fake-token")
            .with("ANODIZE_CLOUDSMITH_API_BASE", &base),
    );

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::LinuxPackage,
        name: "app_1.0.0_amd64.deb".to_string(),
        path: art_path.clone(),
        target: None,
        crate_name: "app".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    let log = StageLogger::new("cloudsmith", Verbosity::Quiet);
    let result = publish_to_cloudsmith(&ctx, &log);

    let uploaded = result.expect("multi-distribution upload should succeed");
    // One CloudsmithTarget recorded per distribution package-create.
    assert_eq!(
        uploaded.len(),
        2,
        "expected one recorded target per distribution, got {uploaded:?}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        6,
        "two distributions must each stage their own file (3 connections \
             each: files/create + presigned + package-create); a hoisted file \
             stage would serve only 4"
    );
}

// ---- classify_cloudsmith_package_response ----------------------------
//
// Pure-function tests for the packages-list response classifier. The
// network-bound `check_cloudsmith_package_exists` is exercised
// indirectly via the same retry helper as `retry_request` (already
// covered above); these tests pin the JSON decision rule.

#[test]
fn cloudsmith_classify_not_found_when_empty_array() {
    let result =
        classify_cloudsmith_package_response("[]", "app_1.0.0_amd64.deb", "deadbeef").unwrap();
    assert_eq!(result, CloudsmithPackageState::NotFound);
}

#[test]
fn cloudsmith_classify_not_found_when_no_matching_filename() {
    let body = r#"[{"filename":"other.deb","checksum_md5":"abcd"}]"#;
    let result =
        classify_cloudsmith_package_response(body, "app_1.0.0_amd64.deb", "deadbeef").unwrap();
    assert_eq!(result, CloudsmithPackageState::NotFound);
}

#[test]
fn cloudsmith_classify_skip_when_md5_matches() {
    let body = r#"[{"filename":"app_1.0.0_amd64.deb","checksum_md5":"deadbeef"}]"#;
    let result =
        classify_cloudsmith_package_response(body, "app_1.0.0_amd64.deb", "deadbeef").unwrap();
    assert_eq!(result, CloudsmithPackageState::SkipIdempotent);
}

#[test]
fn cloudsmith_classify_skip_when_md5_matches_case_insensitive() {
    // Cloudsmith may return uppercase hex; our local computation is
    // lowercase. The comparator must normalize.
    let body = r#"[{"filename":"app_1.0.0_amd64.deb","checksum_md5":"DEADBEEF"}]"#;
    let result =
        classify_cloudsmith_package_response(body, "app_1.0.0_amd64.deb", "deadbeef").unwrap();
    assert_eq!(result, CloudsmithPackageState::SkipIdempotent);
}

#[test]
fn cloudsmith_classify_unverifiable_when_md5_field_absent() {
    // Filename match but no checksum_md5 in the response: presence alone
    // does NOT prove the remote bytes match the local ones, so the
    // classifier must NOT skip-and-claim-match. Upload and let the 409
    // path resolve a real duplicate.
    let body = r#"[{"filename":"app_1.0.0_amd64.deb"}]"#;
    let result =
        classify_cloudsmith_package_response(body, "app_1.0.0_amd64.deb", "deadbeef").unwrap();
    assert_eq!(result, CloudsmithPackageState::Unverifiable);
}

#[test]
fn cloudsmith_classify_unverifiable_when_md5_empty_string() {
    // A partial / still-syncing prior upload reports an empty checksum.
    // Treating that as a match would ship unverified/stale content while
    // claiming "matching md5"; it must classify as Unverifiable (upload).
    let body = r#"[{"filename":"app_1.0.0_amd64.deb","checksum_md5":""}]"#;
    let result =
        classify_cloudsmith_package_response(body, "app_1.0.0_amd64.deb", "deadbeef").unwrap();
    assert_eq!(result, CloudsmithPackageState::Unverifiable);
}

#[test]
fn cloudsmith_classify_bails_when_md5_differs() {
    // The scenario the pre-check guards: a previous run uploaded with
    // one md5, the retry's re-packaged artifact has a different md5.
    // Bail loudly instead of creating a conflicting duplicate.
    let body = r#"[{"filename":"app_1.0.0_amd64.deb","checksum_md5":"aaaa1111"}]"#;
    let result =
        classify_cloudsmith_package_response(body, "app_1.0.0_amd64.deb", "deadbeef").unwrap();
    assert_eq!(
        result,
        CloudsmithPackageState::Md5Mismatch {
            remote: "aaaa1111".to_string()
        }
    );
}

#[test]
fn cloudsmith_classify_handles_non_array_body() {
    // An error envelope or unexpected shape: treat as NotFound rather
    // than blow up, since we can't fix the mismatch anyway and a false
    // upload-attempt is recoverable while a false bail is not.
    let body = r#"{"detail":"not authorized"}"#;
    let result =
        classify_cloudsmith_package_response(body, "app_1.0.0_amd64.deb", "deadbeef").unwrap();
    assert_eq!(result, CloudsmithPackageState::NotFound);
}

#[test]
fn cloudsmith_classify_picks_first_matching_filename() {
    // Defensive: if Cloudsmith returns multiple entries (e.g. across
    // distributions), the classifier picks the first match. Both
    // entries have the same md5 here, mirroring real-world behavior
    // where the same filename is shared across distros.
    let body = r#"[
            {"filename":"other.deb","checksum_md5":"abcd"},
            {"filename":"app_1.0.0_amd64.deb","checksum_md5":"deadbeef"},
            {"filename":"app_1.0.0_amd64.deb","checksum_md5":"deadbeef"}
        ]"#;
    let result =
        classify_cloudsmith_package_response(body, "app_1.0.0_amd64.deb", "deadbeef").unwrap();
    assert_eq!(result, CloudsmithPackageState::SkipIdempotent);
}

// ---- live 3-step upload path (scripted_responder) --------------------
//
// These tests redirect ALL Cloudsmith API traffic to an in-process TCP
// responder via the `ANODIZE_CLOUDSMITH_API_BASE` env seam, then drive a
// real `publish_to_cloudsmith` and assert on the recorded request log
// (method / path / body). The base override is injected per-test through
// each Context's `MapEnvSource` (`cloudsmith_api_base_from` reads it via
// `ctx.env_source()`), so nothing touches the process env and the tests
// run concurrently — no `#[serial_test::serial]`, no shared `env_mutex`.

use anodizer_core::log::{StageLogger, Verbosity};
use anodizer_core::test_helpers::scripted_responder::{
    RequestLog, ScriptedRoute, spawn_scripted_responder_with,
};
use anodizer_core::{MapEnvSource, config::RetryConfig};

/// A `retry:` block with millisecond delays so a 5xx-then-success test
/// retries without the default 10s base sleep stretching CI.
fn fast_retry_config() -> RetryConfig {
    use anodizer_core::config::HumanDuration;
    RetryConfig {
        attempts: 3,
        delay: HumanDuration(std::time::Duration::from_millis(1)),
        max_delay: HumanDuration(std::time::Duration::from_millis(5)),
        max_elapsed: None,
    }
}

/// `HTTP/1.1 200` envelope wrapping a JSON body.
fn http_json(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    )
}

/// Static `204 No Content` for the S3 presigned upload (step 2).
const PRESIGNED_204: &str = "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n";

/// md5 the bytes the same way production does, so a pre-check response
/// can be made to match (or deliberately not match) the local digest.
fn md5_hex_of(bytes: &[u8]) -> String {
    use md5::Digest as _;
    let mut h = md5::Md5::new();
    h.update(bytes);
    anodizer_core::hashing::hex_lower(&h.finalize())
}

/// Build a single-artifact Context whose token resolves from an injected
/// env source (no process-env mutation needed for the token — only the
/// API base override touches the process env, handled per-test).
fn ctx_with_one_artifact(
    cfg: CloudSmithConfig,
    base: &str,
    kind: ArtifactKind,
    art_name: &str,
    path: PathBuf,
    retry: Option<RetryConfig>,
) -> Context {
    let mut config = Config::default();
    config.project_name = "app".to_string();
    config.retry = retry;
    config.cloudsmiths = Some(vec![cfg]);
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.set_env_source(
        MapEnvSource::new()
            .with("CLOUDSMITH_TOKEN", "fake-token")
            .with("ANODIZE_CLOUDSMITH_API_BASE", base),
    );
    ctx.artifacts.add(Artifact {
        kind,
        name: art_name.to_string(),
        path,
        target: None,
        crate_name: "app".to_string(),
        metadata: HashMap::new(),
        size: None,
    });
    ctx
}

/// Convenience: count log entries whose `(method, path)` exactly match.
fn count_calls(log: &[RequestLog], method: &str, path: &str) -> usize {
    log.iter()
        .filter(|e| e.method == method && e.path == path)
        .count()
}

/// A files/create JSON response pointing step 2 back at this responder.
fn files_create_ok(base: &str, id: &str) -> String {
    http_json(&format!(
        r#"{{"identifier":"{id}","upload_url":"{base}/s3-presigned/","upload_fields":{{"key":"v"}}}}"#
    ))
}

/// End-to-end happy path for a `.deb`: files/create -> presigned ->
/// packages/upload/deb/. Asserts the step-3 URL routes to `/upload/deb/`,
/// the step-1 body carries the md5 + filename, the step-3 body carries
/// the files/create `identifier` and the configured `distribution`, and
/// the returned target captures the response `slug_perm`.
#[test]
fn live_deb_full_three_step_records_slug_and_routes_deb() {
    use anodizer_core::config::CloudSmithDistributions;

    let tmp = tempfile::tempdir().unwrap();
    let art = tmp.path().join("app_1.0.0_amd64.deb");
    std::fs::write(&art, b"deb-bytes").unwrap();
    let md5 = md5_hex_of(b"deb-bytes");

    let (addr, log) = spawn_scripted_responder_with(move |addr| {
        let base = format!("http://{addr}");
        vec![
            ScriptedRoute {
                method: "POST",
                path_pattern: "/files/myorg/myrepo/",
                response: Box::leak(files_create_ok(&base, "id-1").into_boxed_str()),
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/s3-presigned/",
                response: PRESIGNED_204,
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/packages/myorg/myrepo/upload/deb/",
                response: Box::leak(http_json(r#"{"slug_perm":"deb-slug"}"#).into_boxed_str()),
                times: None,
            },
        ]
    });
    let base = format!("http://{addr}");

    let mut distros: HashMap<String, CloudSmithDistributions> = HashMap::new();
    distros.insert(
        "deb".to_string(),
        CloudSmithDistributions::Single("ubuntu/focal".to_string()),
    );
    let cfg = CloudSmithConfig {
        organization: Some("myorg".to_string()),
        repository: Some("myrepo".to_string()),
        distributions: Some(distros),
        republish: Some(StringOrBool::Bool(true)),
        ..Default::default()
    };
    let ctx = ctx_with_one_artifact(
        cfg,
        &base,
        ArtifactKind::LinuxPackage,
        "app_1.0.0_amd64.deb",
        art.clone(),
        None,
    );

    let result = publish_to_cloudsmith(&ctx, &StageLogger::new("cloudsmith", Verbosity::Quiet));

    let uploaded = result.expect("happy-path upload");
    assert_eq!(uploaded.len(), 1);
    assert_eq!(uploaded[0].slug.as_deref(), Some("deb-slug"));
    assert_eq!(uploaded[0].filename, "app_1.0.0_amd64.deb");

    let entries = log.lock().unwrap();
    assert_eq!(
        count_calls(&entries, "POST", "/files/myorg/myrepo/"),
        1,
        "exactly one files/create"
    );
    assert_eq!(
        count_calls(&entries, "POST", "/packages/myorg/myrepo/upload/deb/"),
        1,
        "step 3 routed to /upload/deb/"
    );
    let create = entries
        .iter()
        .find(|e| e.path == "/files/myorg/myrepo/")
        .unwrap();
    assert!(
        create.body.contains(&format!("\"md5_checksum\":\"{md5}\"")),
        "files/create body carries local md5: {}",
        create.body
    );
    assert!(
        create.body.contains("\"filename\":\"app_1.0.0_amd64.deb\""),
        "files/create body carries filename: {}",
        create.body
    );
    let step3 = entries
        .iter()
        .find(|e| e.path == "/packages/myorg/myrepo/upload/deb/")
        .unwrap();
    assert!(
        step3.body.contains("\"package_file\":\"id-1\""),
        "step-3 body threads the files/create identifier: {}",
        step3.body
    );
    assert!(
        step3.body.contains("\"distribution\":\"ubuntu/focal\""),
        "step-3 body carries the configured distribution: {}",
        step3.body
    );
}

/// Drive one artifact of `kind`/`art_name` through the 3-step flow with a
/// responder whose step-3 route is `expected_step3_path`. Returns the
/// captured request log so per-format tests can assert routing + body.
/// `republish=true` so the pre-check packages-list query is skipped and
/// the route table is exactly the 3 upload calls.
fn run_one_format(
    art_name: &'static str,
    kind: ArtifactKind,
    expected_step3_path: &'static str,
    extra_cfg: impl FnOnce(&mut CloudSmithConfig),
) -> Vec<RequestLog> {
    let tmp = tempfile::tempdir().unwrap();
    let art = tmp.path().join(art_name);
    std::fs::write(&art, b"bytes").unwrap();

    let (addr, log) = spawn_scripted_responder_with(move |addr| {
        let base = format!("http://{addr}");
        vec![
            ScriptedRoute {
                method: "POST",
                path_pattern: "/files/myorg/myrepo/",
                response: Box::leak(files_create_ok(&base, "id-f").into_boxed_str()),
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/s3-presigned/",
                response: PRESIGNED_204,
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: expected_step3_path,
                response: Box::leak(http_json(r#"{"slug_perm":"s"}"#).into_boxed_str()),
                times: None,
            },
        ]
    });
    let base = format!("http://{addr}");

    let mut cfg = CloudSmithConfig {
        organization: Some("myorg".to_string()),
        repository: Some("myrepo".to_string()),
        // Match every format so the single artifact always passes the
        // filter. `zip` is included so a non-package Archive (which
        // `detect_format` slugs as `raw`) still clears the extension
        // filter — there is no literal `.raw` extension to match on.
        formats: Some(vec![
            "deb".to_string(),
            "rpm".to_string(),
            "srpm".to_string(),
            "apk".to_string(),
            "zip".to_string(),
        ]),
        republish: Some(StringOrBool::Bool(true)),
        ..Default::default()
    };
    extra_cfg(&mut cfg);
    let ctx = ctx_with_one_artifact(cfg, &base, kind, art_name, art.clone(), None);

    let result = publish_to_cloudsmith(&ctx, &StageLogger::new("cloudsmith", Verbosity::Quiet));
    result.expect("upload should succeed");
    let entries = log.lock().unwrap();
    entries.clone()
}

/// `.rpm` routes step 3 to `/upload/rpm/`.
#[test]
fn live_rpm_routes_to_upload_rpm() {
    let log = run_one_format(
        "app-1.0.0.x86_64.rpm",
        ArtifactKind::LinuxPackage,
        "/packages/myorg/myrepo/upload/rpm/",
        |_| {},
    );
    assert_eq!(
        count_calls(&log, "POST", "/packages/myorg/myrepo/upload/rpm/"),
        1,
    );
}

/// `.src.rpm` is a distinct format slug: step 3 routes to `/upload/srpm/`,
/// NOT `/upload/rpm/` (the suffix overlap is resolved in `detect_format`).
#[test]
fn live_src_rpm_routes_to_upload_srpm() {
    let log = run_one_format(
        "app-1.0.0.src.rpm",
        ArtifactKind::LinuxPackage,
        "/packages/myorg/myrepo/upload/srpm/",
        |_| {},
    );
    assert_eq!(
        count_calls(&log, "POST", "/packages/myorg/myrepo/upload/srpm/"),
        1,
    );
    assert_eq!(
        count_calls(&log, "POST", "/packages/myorg/myrepo/upload/rpm/"),
        0,
        "src.rpm must not route to the rpm slug",
    );
}

/// `.apk` maps to the API-side `alpine` slug: step 3 -> `/upload/alpine/`.
#[test]
fn live_apk_routes_to_upload_alpine() {
    let log = run_one_format(
        "app-1.0.0.apk",
        ArtifactKind::LinuxPackage,
        "/packages/myorg/myrepo/upload/alpine/",
        |_| {},
    );
    assert_eq!(
        count_calls(&log, "POST", "/packages/myorg/myrepo/upload/alpine/"),
        1,
    );
}

/// A non-package Archive (`.zip`) detects as the `raw` format and routes
/// step 3 to `/upload/raw/` (the `detect_format` fallback slug).
#[test]
fn live_zip_archive_routes_to_upload_raw() {
    let log = run_one_format(
        "app-1.0.0.zip",
        ArtifactKind::Archive,
        "/packages/myorg/myrepo/upload/raw/",
        |_| {},
    );
    assert_eq!(
        count_calls(&log, "POST", "/packages/myorg/myrepo/upload/raw/"),
        1,
    );
}

/// `component:` is included in the step-3 body for `deb` (a
/// component-bearing format).
#[test]
fn live_deb_includes_component_in_body() {
    let log = run_one_format(
        "app_1.0.0_amd64.deb",
        ArtifactKind::LinuxPackage,
        "/packages/myorg/myrepo/upload/deb/",
        |cfg| cfg.component = Some("contrib".to_string()),
    );
    let step3 = log
        .iter()
        .find(|e| e.path == "/packages/myorg/myrepo/upload/deb/")
        .unwrap();
    assert!(
        step3.body.contains("\"component\":\"contrib\""),
        "deb step-3 body carries component: {}",
        step3.body
    );
}

/// `component:` is DROPPED from the step-3 body for `rpm` (rpm is not in
/// `COMPONENT_BEARING_FORMATS`); the upload still succeeds.
#[test]
fn live_rpm_drops_component_from_body() {
    let log = run_one_format(
        "app-1.0.0.x86_64.rpm",
        ArtifactKind::LinuxPackage,
        "/packages/myorg/myrepo/upload/rpm/",
        |cfg| cfg.component = Some("contrib".to_string()),
    );
    let step3 = log
        .iter()
        .find(|e| e.path == "/packages/myorg/myrepo/upload/rpm/")
        .unwrap();
    assert!(
        !step3.body.contains("component"),
        "rpm step-3 body must not carry a component: {}",
        step3.body
    );
}

/// `republish: true` puts `"republish": true` into the step-3 body so
/// Cloudsmith overwrites an existing package rather than 409ing.
#[test]
fn live_republish_sets_republish_flag_in_body() {
    let log = run_one_format(
        "app_1.0.0_amd64.deb",
        ArtifactKind::LinuxPackage,
        "/packages/myorg/myrepo/upload/deb/",
        |_| {},
    );
    let step3 = log
        .iter()
        .find(|e| e.path == "/packages/myorg/myrepo/upload/deb/")
        .unwrap();
    assert!(
        step3.body.contains("\"republish\":true"),
        "step-3 body carries republish flag: {}",
        step3.body
    );
}

/// Run a publish for a single `.deb` artifact against a caller-supplied
/// route table (built once the responder addr is known), returning the
/// publish `Result` and the request log. `cfg_mut` customizes the entry;
/// `retry` lets retry-path tests inject a fast policy.
#[allow(clippy::type_complexity)]
fn run_deb_with_routes<R>(
    routes_fn: R,
    cfg_mut: impl FnOnce(&mut CloudSmithConfig),
    retry: Option<RetryConfig>,
) -> (Result<Vec<CloudsmithTarget>>, Vec<RequestLog>)
where
    R: FnOnce(&str) -> Vec<ScriptedRoute> + Send + 'static,
{
    let tmp = tempfile::tempdir().unwrap();
    let art = tmp.path().join("app_1.0.0_amd64.deb");
    std::fs::write(&art, b"deb-bytes").unwrap();

    let (addr, log) = spawn_scripted_responder_with(move |addr| {
        let base = format!("http://{addr}");
        routes_fn(&base)
    });
    let base = format!("http://{addr}");

    let mut cfg = CloudSmithConfig {
        organization: Some("myorg".to_string()),
        repository: Some("myrepo".to_string()),
        formats: Some(vec!["deb".to_string()]),
        ..Default::default()
    };
    cfg_mut(&mut cfg);
    let ctx = ctx_with_one_artifact(
        cfg,
        &base,
        ArtifactKind::LinuxPackage,
        "app_1.0.0_amd64.deb",
        art.clone(),
        retry,
    );

    let result = publish_to_cloudsmith(&ctx, &StageLogger::new("cloudsmith", Verbosity::Quiet));
    let entries = log.lock().unwrap().clone();
    (result, entries)
}

/// The exact pre-check packages-list path reqwest produces for
/// `filename:app_1.0.0_amd64.deb` (the colon percent-encodes to `%3A`;
/// dots/underscores are unreserved). Routing the GET here lets the
/// republish=false pre-check be driven without a real Cloudsmith.
const PRECHECK_PATH: &str =
    "/packages/myorg/myrepo/?query=filename%3Aapp_1.0.0_amd64.deb&page_size=100";

fn precheck_route(body: &'static str) -> ScriptedRoute {
    ScriptedRoute {
        method: "GET",
        path_pattern: PRECHECK_PATH,
        response: Box::leak(http_json(body).into_boxed_str()),
        times: None,
    }
}

/// republish=false + a pre-check that reports the same md5 ⇒ the upload
/// is skipped (idempotent): no files/create, no step-3, empty targets.
#[test]
fn live_precheck_skip_idempotent_when_md5_matches() {
    let md5 = md5_hex_of(b"deb-bytes");
    let body: &'static str = Box::leak(
        format!(r#"[{{"filename":"app_1.0.0_amd64.deb","checksum_md5":"{md5}"}}]"#)
            .into_boxed_str(),
    );
    let (result, log) = run_deb_with_routes(move |_base| vec![precheck_route(body)], |_| {}, None);
    let uploaded = result.expect("idempotent skip is success");
    assert!(uploaded.is_empty(), "skip records no target: {uploaded:?}");
    assert_eq!(
        count_calls(&log, "POST", "/files/myorg/myrepo/"),
        0,
        "idempotent skip must not stage a file"
    );
    assert_eq!(count_calls(&log, "GET", PRECHECK_PATH), 1);
}

/// republish=false + a pre-check reporting a DIFFERENT md5 ⇒ bail with a
/// conflict error naming both md5s; nothing is uploaded.
#[test]
fn live_precheck_bails_on_md5_mismatch() {
    let (result, log) = run_deb_with_routes(
        move |_base| {
            vec![precheck_route(
                r#"[{"filename":"app_1.0.0_amd64.deb","checksum_md5":"00bad00remote"}]"#,
            )]
        },
        |_| {},
        None,
    );
    let err = result.expect_err("md5 mismatch must bail").to_string();
    assert!(err.contains("different md5"), "conflict error: {err}");
    assert!(err.contains("00bad00remote"), "names remote md5: {err}");
    assert_eq!(
        count_calls(&log, "POST", "/files/myorg/myrepo/"),
        0,
        "mismatch must not stage a file"
    );
}

/// A files/create that 4xxs surfaces the HTTP status and the response
/// body in the error chain (and does not retry — 4xx fast-fails).
#[test]
fn live_files_create_4xx_surfaces_body() {
    let (result, log) = run_deb_with_routes(
        move |_base| {
            vec![ScriptedRoute {
                method: "POST",
                path_pattern: "/files/myorg/myrepo/",
                response: "HTTP/1.1 422 Unprocessable Entity\r\nContent-Length: 24\r\n\r\n{\"detail\":\"bad md5 sum\"}",
                times: None,
            }]
        },
        |cfg| cfg.republish = Some(StringOrBool::Bool(true)),
        None,
    );
    let err = format!("{:#}", result.expect_err("4xx must error"));
    assert!(err.contains("422"), "status in error: {err}");
    assert!(err.contains("bad md5 sum"), "body in error: {err}");
    assert_eq!(
        count_calls(&log, "POST", "/files/myorg/myrepo/"),
        1,
        "4xx must NOT retry"
    );
}

/// A files/create 200 whose JSON lacks `identifier` is a contract
/// violation: bail with a message naming the missing field + artifact.
#[test]
fn live_files_create_missing_identifier_errors() {
    let (result, _log) = run_deb_with_routes(
        move |base| {
            vec![ScriptedRoute {
                method: "POST",
                path_pattern: "/files/myorg/myrepo/",
                response: Box::leak(
                    http_json(&format!(
                        r#"{{"upload_url":"{base}/s3/","upload_fields":{{}}}}"#
                    ))
                    .into_boxed_str(),
                ),
                times: None,
            }]
        },
        |cfg| cfg.republish = Some(StringOrBool::Bool(true)),
        None,
    );
    let err = result
        .expect_err("missing identifier must bail")
        .to_string();
    assert!(err.contains("identifier"), "names missing field: {err}");
    assert!(err.contains("app_1.0.0_amd64.deb"), "names artifact: {err}");
}

/// A files/create 200 missing `upload_url` bails naming that field.
#[test]
fn live_files_create_missing_upload_url_errors() {
    let (result, _log) = run_deb_with_routes(
        move |_base| {
            vec![ScriptedRoute {
                method: "POST",
                path_pattern: "/files/myorg/myrepo/",
                response: Box::leak(
                    http_json(r#"{"identifier":"id-1","upload_fields":{}}"#).into_boxed_str(),
                ),
                times: None,
            }]
        },
        |cfg| cfg.republish = Some(StringOrBool::Bool(true)),
        None,
    );
    let err = result
        .expect_err("missing upload_url must bail")
        .to_string();
    assert!(err.contains("upload_url"), "names missing field: {err}");
}

/// A files/create that returns a 200 with a non-JSON body bails with the
/// "non-JSON body" diagnostic (the parse-context branch).
#[test]
fn live_files_create_non_json_errors() {
    let (result, _log) = run_deb_with_routes(
        move |_base| {
            vec![ScriptedRoute {
                method: "POST",
                path_pattern: "/files/myorg/myrepo/",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\nnot-jsn",
                times: None,
            }]
        },
        |cfg| cfg.republish = Some(StringOrBool::Bool(true)),
        None,
    );
    let err = result.expect_err("non-JSON must bail").to_string();
    assert!(err.contains("non-JSON body"), "diagnostic: {err}");
}

/// A 5xx on files/create retries (fast policy, 2 attempts allowed) and
/// then succeeds on the 2nd attempt — the `times`-capped 500 route is
/// exhausted, so the unlimited 200 route serves attempt 2. Two recorded
/// files/create calls prove the retry actually happened.
#[test]
fn live_files_create_5xx_then_success_retries() {
    let (result, log) = run_deb_with_routes(
        move |base| {
            let base = base.to_string();
            vec![
                // First attempt: 500 (capped to one hit).
                ScriptedRoute {
                    method: "POST",
                    path_pattern: "/files/myorg/myrepo/",
                    response: "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 3\r\n\r\nerr",
                    times: Some(1),
                },
                // Second attempt: the 500 route is spent, so this matches.
                ScriptedRoute {
                    method: "POST",
                    path_pattern: "/files/myorg/myrepo/",
                    response: Box::leak(files_create_ok(&base, "id-r").into_boxed_str()),
                    times: None,
                },
                ScriptedRoute {
                    method: "POST",
                    path_pattern: "/s3-presigned/",
                    response: PRESIGNED_204,
                    times: None,
                },
                ScriptedRoute {
                    method: "POST",
                    path_pattern: "/packages/myorg/myrepo/upload/deb/",
                    response: Box::leak(http_json(r#"{"slug_perm":"s"}"#).into_boxed_str()),
                    times: None,
                },
            ]
        },
        |cfg| cfg.republish = Some(StringOrBool::Bool(true)),
        Some(fast_retry_config()),
    );
    let uploaded = result.expect("retry then success");
    assert_eq!(uploaded.len(), 1);
    assert_eq!(
        count_calls(&log, "POST", "/files/myorg/myrepo/"),
        2,
        "one 5xx + one success = two files/create attempts (retry fired)"
    );
}

/// A missing artifact file bails before any HTTP call.
#[test]
fn live_missing_artifact_file_bails() {
    let (addr, log) = spawn_scripted_responder_with(|_| Vec::new());
    let base = format!("http://{addr}");
    let cfg = CloudSmithConfig {
        organization: Some("myorg".to_string()),
        repository: Some("myrepo".to_string()),
        formats: Some(vec!["deb".to_string()]),
        republish: Some(StringOrBool::Bool(true)),
        ..Default::default()
    };
    // Point the artifact at a path that does not exist.
    let ctx = ctx_with_one_artifact(
        cfg,
        &base,
        ArtifactKind::LinuxPackage,
        "app_1.0.0_amd64.deb",
        PathBuf::from("/nonexistent/app_1.0.0_amd64.deb"),
        None,
    );
    let result = publish_to_cloudsmith(&ctx, &StageLogger::new("cloudsmith", Verbosity::Quiet));
    let err = result.expect_err("missing file must bail").to_string();
    assert!(err.contains("artifact file not found"), "{err}");
    assert!(
        log.lock().unwrap().is_empty(),
        "no HTTP call before the file check"
    );
}

/// When no artifact matches the format filter, the publisher reports the
/// no-match status and returns an empty target list (no HTTP traffic).
#[test]
fn live_no_matching_artifacts_is_noop() {
    // A `.rpm` artifact but a `deb`-only filter ⇒ zero matches.
    let log = {
        let (addr, log) = spawn_scripted_responder_with(|_| Vec::new());
        let base = format!("http://{addr}");
        let cfg = CloudSmithConfig {
            organization: Some("myorg".to_string()),
            repository: Some("myrepo".to_string()),
            formats: Some(vec!["deb".to_string()]),
            ..Default::default()
        };
        let tmp = tempfile::tempdir().unwrap();
        let art = tmp.path().join("app-1.0.0.x86_64.rpm");
        std::fs::write(&art, b"x").unwrap();
        let ctx = ctx_with_one_artifact(
            cfg,
            &base,
            ArtifactKind::LinuxPackage,
            "app-1.0.0.x86_64.rpm",
            art.clone(),
            None,
        );
        let result = publish_to_cloudsmith(&ctx, &StageLogger::new("cloudsmith", Verbosity::Quiet));
        assert!(result.expect("no-match is ok").is_empty());
        log
    };
    assert!(log.lock().unwrap().is_empty(), "no upload attempted");
}

/// step-3 response carrying only `slug` (not `slug_perm`) still captures
/// the slug into the recorded target (the `or_else` fallback key).
#[test]
fn live_step3_slug_fallback_key() {
    let (result, _log) = run_deb_with_routes(
        move |base| {
            let base = base.to_string();
            vec![
                ScriptedRoute {
                    method: "POST",
                    path_pattern: "/files/myorg/myrepo/",
                    response: Box::leak(files_create_ok(&base, "id-1").into_boxed_str()),
                    times: None,
                },
                ScriptedRoute {
                    method: "POST",
                    path_pattern: "/s3-presigned/",
                    response: PRESIGNED_204,
                    times: None,
                },
                ScriptedRoute {
                    method: "POST",
                    path_pattern: "/packages/myorg/myrepo/upload/deb/",
                    response: Box::leak(http_json(r#"{"slug":"plain-slug"}"#).into_boxed_str()),
                    times: None,
                },
            ]
        },
        |cfg| cfg.republish = Some(StringOrBool::Bool(true)),
        None,
    );
    let uploaded = result.expect("upload ok");
    assert_eq!(uploaded[0].slug.as_deref(), Some("plain-slug"));
}

/// step-3 response with no recognizable slug field still records the
/// target (slug = None) so the upload is counted; rollback degrades to
/// the warn-only path for it.
#[test]
fn live_step3_no_slug_records_target_without_slug() {
    let (result, _log) = run_deb_with_routes(
        move |base| {
            let base = base.to_string();
            vec![
                ScriptedRoute {
                    method: "POST",
                    path_pattern: "/files/myorg/myrepo/",
                    response: Box::leak(files_create_ok(&base, "id-1").into_boxed_str()),
                    times: None,
                },
                ScriptedRoute {
                    method: "POST",
                    path_pattern: "/s3-presigned/",
                    response: PRESIGNED_204,
                    times: None,
                },
                ScriptedRoute {
                    method: "POST",
                    path_pattern: "/packages/myorg/myrepo/upload/deb/",
                    response: Box::leak(http_json(r#"{"ok":true}"#).into_boxed_str()),
                    times: None,
                },
            ]
        },
        |cfg| cfg.republish = Some(StringOrBool::Bool(true)),
        None,
    );
    let uploaded = result.expect("upload ok");
    assert_eq!(uploaded.len(), 1);
    assert!(uploaded[0].slug.is_none(), "no slug field ⇒ None");
    assert_eq!(uploaded[0].filename, "app_1.0.0_amd64.deb");
}

/// Build the (files/create, presigned, conflicting-step3) routes plus two
/// sequenced pre-check GET routes: the FIRST GET (pre-check) returns `[]`
/// (NotFound → proceed to upload); the SECOND GET (post-409 re-query)
/// returns `recheck_body`. The step-3 route always 409s.
fn conflict_recovery_routes(base: &str, recheck_body: &'static str) -> Vec<ScriptedRoute> {
    let base = base.to_string();
    vec![
        // Pre-check (republish=false): NotFound so the upload proceeds.
        ScriptedRoute {
            method: "GET",
            path_pattern: PRECHECK_PATH,
            response: Box::leak(http_json("[]").into_boxed_str()),
            times: Some(1),
        },
        // Post-409 re-query: the recovery verdict.
        ScriptedRoute {
            method: "GET",
            path_pattern: PRECHECK_PATH,
            response: Box::leak(http_json(recheck_body).into_boxed_str()),
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/files/myorg/myrepo/",
            response: Box::leak(files_create_ok(&base, "id-1").into_boxed_str()),
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/s3-presigned/",
            response: PRESIGNED_204,
            times: None,
        },
        // step-3 always conflicts (409). 4xx fast-fails (no retry), so a
        // single capped response is enough.
        ScriptedRoute {
            method: "POST",
            path_pattern: "/packages/myorg/myrepo/upload/deb/",
            response: "HTTP/1.1 409 Conflict\r\nContent-Length: 13\r\n\r\nalready there",
            times: None,
        },
    ]
}

/// step-3 409 + a re-query showing the same md5 already landed (a
/// concurrent uploader won the race) ⇒ idempotent skip: Ok, no target
/// recorded, and the recovery re-query actually fired (2 GETs).
#[test]
fn live_step3_409_recovers_as_idempotent_skip() {
    let md5 = md5_hex_of(b"deb-bytes");
    let recheck: &'static str = Box::leak(
        format!(r#"[{{"filename":"app_1.0.0_amd64.deb","checksum_md5":"{md5}"}}]"#)
            .into_boxed_str(),
    );
    let (result, log) = run_deb_with_routes(
        move |base| conflict_recovery_routes(base, recheck),
        |_| {},
        Some(fast_retry_config()),
    );
    let uploaded = result.expect("409 with matching remote md5 ⇒ idempotent skip");
    assert!(uploaded.is_empty(), "skip records no target: {uploaded:?}");
    assert_eq!(
        count_calls(&log, "GET", PRECHECK_PATH),
        2,
        "pre-check + post-409 recovery re-query"
    );
}

/// step-3 409 + a re-query showing a DIFFERENT md5 ⇒ surface the conflict
/// (a concurrent uploader landed different bytes under our name).
#[test]
fn live_step3_409_recovery_bails_on_md5_mismatch() {
    let (result, _log) = run_deb_with_routes(
        move |base| {
            conflict_recovery_routes(
                base,
                r#"[{"filename":"app_1.0.0_amd64.deb","checksum_md5":"00different00"}]"#,
            )
        },
        |_| {},
        Some(fast_retry_config()),
    );
    let err = result
        .expect_err("409 + different remote md5 must bail")
        .to_string();
    assert!(
        err.contains("step-3 conflict"),
        "names step-3 conflict: {err}"
    );
    assert!(err.contains("00different00"), "names remote md5: {err}");
}

/// step-3 409 + a re-query showing the package is NOT present (the 409 was
/// not a same-name race) ⇒ the original step-3 error re-propagates instead
/// of being silently swallowed.
#[test]
fn live_step3_409_recovery_repropagates_when_not_found() {
    let (result, _log) = run_deb_with_routes(
        move |base| conflict_recovery_routes(base, "[]"),
        |_| {},
        Some(fast_retry_config()),
    );
    let err = format!("{:#}", result.expect_err("409 + still-absent must error"));
    assert!(err.contains("409"), "original 409 status propagates: {err}");
}

// ---- live keep_versions retention pruning (list + DELETE) ------------
//
// The prune path (`prune_cloudsmith_versions` → `list_cloudsmith_package_versions`
// → DELETE) was previously only exercised through the pure selector
// (`select_versions_to_prune`); these drive the real HTTP list+delete
// against the scripted responder. The package name pruning scopes to is
// captured from the step-3 response `name` field, so the upload routes
// must return a `name` for the prune to fire.

/// The exact list path reqwest produces for the prune `name:` query of
/// package `app` on page 1. `name:` → `name%3A`; `page`/`page_size` are
/// appended in builder order.
const PRUNE_LIST_PATH: &str = "/packages/myorg/myrepo/?query=name%3Aapp&page=1&page_size=100";

/// The three upload routes for a single `.deb` whose step-3 response
/// carries `slug_perm` + `name:"app"` so `keep_versions` pruning can scope
/// to that package. `republish=true` keeps the pre-check off the route
/// table.
fn upload_routes_with_name(base: &str, slug: &str, name: &str) -> Vec<ScriptedRoute> {
    let body: &'static str = Box::leak(
        http_json(&format!(r#"{{"slug_perm":"{slug}","name":"{name}"}}"#)).into_boxed_str(),
    );
    vec![
        ScriptedRoute {
            method: "POST",
            path_pattern: "/files/myorg/myrepo/",
            response: Box::leak(files_create_ok(base, "id-u").into_boxed_str()),
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/s3-presigned/",
            response: PRESIGNED_204,
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/packages/myorg/myrepo/upload/deb/",
            response: body,
            times: None,
        },
    ]
}

/// A 204 No Content for a prune DELETE.
const DELETE_204: &str = "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n";

/// Run a `.deb` publish with `keep_versions: keep`, a current `Version`,
/// and a caller-supplied route table (list + DELETE routes layered on the
/// upload routes). Returns the request log.
fn run_prune(
    keep: u32,
    version: &str,
    extra_routes: impl FnOnce(&str) -> Vec<ScriptedRoute> + Send + 'static,
) -> Vec<RequestLog> {
    let tmp = tempfile::tempdir().unwrap();
    let art = tmp.path().join("app_1.0.0_amd64.deb");
    std::fs::write(&art, b"deb-bytes").unwrap();

    let (addr, log) = spawn_scripted_responder_with(move |addr| {
        let base = format!("http://{addr}");
        let mut routes = upload_routes_with_name(&base, "current-slug", "app");
        routes.extend(extra_routes(&base));
        routes
    });
    let base = format!("http://{addr}");

    let cfg = CloudSmithConfig {
        organization: Some("myorg".to_string()),
        repository: Some("myrepo".to_string()),
        formats: Some(vec!["deb".to_string()]),
        republish: Some(StringOrBool::Bool(true)),
        keep_versions: Some(keep),
        ..Default::default()
    };
    let mut ctx = ctx_with_one_artifact(
        cfg,
        &base,
        ArtifactKind::LinuxPackage,
        "app_1.0.0_amd64.deb",
        art.clone(),
        Some(fast_retry_config()),
    );
    // The prune is gated on a known current version (an empty version
    // disables it to protect the just-uploaded release).
    ctx.template_vars_mut().set("Version", version);

    let result = publish_to_cloudsmith(&ctx, &StageLogger::new("cloudsmith", Verbosity::Quiet));
    result.expect("upload + prune should succeed");
    let entries = log.lock().unwrap();
    entries.clone()
}

/// keep_versions=2 over a list of 3 distinct versions ⇒ the single oldest
/// version's slug is DELETEd; the list query + delete are real HTTP, and
/// the upload's own version is always retained.
#[test]
fn live_prune_lists_and_deletes_oldest_version() {
    let list_body: &'static str = Box::leak(
            http_json(
                r#"[
                    {"name":"app","slug_perm":"current-slug","version":"0.9.1","uploaded_at":"2026-06-14T00:00:00Z"},
                    {"name":"app","slug_perm":"s-090","version":"0.9.0","uploaded_at":"2026-06-01T00:00:00Z"},
                    {"name":"app","slug_perm":"s-080","version":"0.8.0","uploaded_at":"2026-05-01T00:00:00Z"}
                ]"#,
            )
            .into_boxed_str(),
        );
    let log = run_prune(2, "0.9.1", move |_base| {
        vec![
            ScriptedRoute {
                method: "GET",
                path_pattern: PRUNE_LIST_PATH,
                response: list_body,
                times: None,
            },
            ScriptedRoute {
                method: "DELETE",
                path_pattern: "/packages/myorg/myrepo/s-080/",
                response: DELETE_204,
                times: None,
            },
        ]
    });
    // The list query fired exactly once (single short page).
    assert_eq!(
        count_calls(&log, "GET", PRUNE_LIST_PATH),
        1,
        "prune lists the package's versions: {log:?}"
    );
    // Only the oldest (0.8.0) is deleted; 0.9.1 (current) + 0.9.0 kept.
    assert_eq!(
        count_calls(&log, "DELETE", "/packages/myorg/myrepo/s-080/"),
        1,
        "oldest version's slug is DELETEd: {log:?}"
    );
    assert_eq!(
        count_calls(&log, "DELETE", "/packages/myorg/myrepo/s-090/"),
        0,
        "second-newest is retained (within keep=2)"
    );
    assert_eq!(
        count_calls(&log, "DELETE", "/packages/myorg/myrepo/current-slug/"),
        0,
        "the just-uploaded current version is never pruned"
    );
    // The DELETE carries the `Authorization: token <secret>` header that
    // only header capture can observe.
    let del = log
        .iter()
        .find(|e| e.method == "DELETE")
        .expect("delete request recorded");
    assert_eq!(
        del.header("Authorization"),
        Some("token fake-token"),
        "prune DELETE carries the token auth header: {:?}",
        del.headers
    );
}

/// keep_versions pages through a >100-entry list: a full first page
/// (100 entries, all the current version) then a short second page
/// carrying the prunable old version. Two GETs prove pagination fired.
#[test]
fn live_prune_paginates_until_short_page() {
    // Page 1: exactly PAGE_SIZE (100) entries, all version 0.9.1
    // (current), each a distinct slug. A full page forces a page-2 fetch.
    let mut page1 = String::from("[");
    for i in 0..100 {
        if i > 0 {
            page1.push(',');
        }
        page1.push_str(&format!(
                r#"{{"name":"app","slug_perm":"cur-{i}","version":"0.9.1","uploaded_at":"2026-06-14T00:00:00Z"}}"#
            ));
    }
    page1.push(']');
    let page1_resp: &'static str = Box::leak(http_json(&page1).into_boxed_str());
    let page2_resp: &'static str = Box::leak(
            http_json(
                r#"[{"name":"app","slug_perm":"old-1","version":"0.5.0","uploaded_at":"2026-01-01T00:00:00Z"}]"#,
            )
            .into_boxed_str(),
        );

    let log = run_prune(1, "0.9.1", move |_base| {
        vec![
            ScriptedRoute {
                method: "GET",
                path_pattern: "/packages/myorg/myrepo/?query=name%3Aapp&page=1&page_size=100",
                response: page1_resp,
                times: None,
            },
            ScriptedRoute {
                method: "GET",
                path_pattern: "/packages/myorg/myrepo/?query=name%3Aapp&page=2&page_size=100",
                response: page2_resp,
                times: None,
            },
            ScriptedRoute {
                method: "DELETE",
                path_pattern: "/packages/myorg/myrepo/old-1/",
                response: DELETE_204,
                times: None,
            },
        ]
    });
    assert_eq!(
        count_calls(
            &log,
            "GET",
            "/packages/myorg/myrepo/?query=name%3Aapp&page=1&page_size=100"
        ),
        1,
        "page 1 fetched"
    );
    assert_eq!(
        count_calls(
            &log,
            "GET",
            "/packages/myorg/myrepo/?query=name%3Aapp&page=2&page_size=100"
        ),
        1,
        "full first page forces a page-2 fetch (pagination): {log:?}"
    );
    assert_eq!(
        count_calls(&log, "DELETE", "/packages/myorg/myrepo/old-1/"),
        1,
        "the old version found on page 2 is pruned"
    );
}

/// A prune-list 4xx is non-fatal by contract: the upload already
/// succeeded, so the publish still returns Ok and NO DELETE is issued
/// (the warn-and-continue branch).
#[test]
fn live_prune_list_4xx_is_nonfatal_and_deletes_nothing() {
    let log = run_prune(2, "0.9.1", move |_base| {
        vec![ScriptedRoute {
            method: "GET",
            path_pattern: PRUNE_LIST_PATH,
            response: "HTTP/1.1 403 Forbidden\r\nContent-Length: 11\r\n\r\nno read acl",
            times: None,
        }]
    });
    // The upload itself still landed (run_prune asserts Ok); the prune
    // list failed → nothing deleted.
    assert_eq!(count_calls(&log, "GET", PRUNE_LIST_PATH), 1);
    assert_eq!(
        log.iter().filter(|e| e.method == "DELETE").count(),
        0,
        "a failed list must not delete anything: {log:?}"
    );
}

/// A prune DELETE 4xx is counted as a failure but is STILL non-fatal:
/// the publish returns Ok (the upload succeeded) while the delete failure
/// only warns. Proves the destructive follow-up can never fail the stage.
#[test]
fn live_prune_delete_4xx_is_nonfatal() {
    let list_body: &'static str = Box::leak(
            http_json(
                r#"[
                    {"name":"app","slug_perm":"current-slug","version":"0.9.1","uploaded_at":"2026-06-14T00:00:00Z"},
                    {"name":"app","slug_perm":"s-070","version":"0.7.0","uploaded_at":"2026-04-01T00:00:00Z"}
                ]"#,
            )
            .into_boxed_str(),
        );
    let log = run_prune(1, "0.9.1", move |_base| {
        vec![
            ScriptedRoute {
                method: "GET",
                path_pattern: PRUNE_LIST_PATH,
                response: list_body,
                times: None,
            },
            ScriptedRoute {
                method: "DELETE",
                path_pattern: "/packages/myorg/myrepo/s-070/",
                response: "HTTP/1.1 403 Forbidden\r\nContent-Length: 6\r\n\r\ndenied",
                times: None,
            },
        ]
    });
    // run_prune already asserted the publish returned Ok despite the 403.
    assert_eq!(
        count_calls(&log, "DELETE", "/packages/myorg/myrepo/s-070/"),
        1,
        "the delete was attempted (and failed non-fatally): {log:?}"
    );
}

/// Templated `organization` / `repository` (the workspace-style path
/// where org/repo come from context vars rather than literals) render
/// before any URL is built: `{{ .ProjectName }}` resolves to the
/// project name so the upload routes to `/files/app-org/app-repo/`.
/// Top-level publishers like cloudsmith don't resolve per-crate config,
/// but they DO render their config values against the active context —
/// this pins that the rendered values reach the wire.
#[test]
fn live_templated_org_repo_render_into_request_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let art = tmp.path().join("app_1.0.0_amd64.deb");
    std::fs::write(&art, b"deb-bytes").unwrap();

    let (addr, log) = spawn_scripted_responder_with(move |addr| {
        let base = format!("http://{addr}");
        vec![
            ScriptedRoute {
                method: "POST",
                path_pattern: "/files/app-org/app-repo/",
                response: Box::leak(files_create_ok(&base, "id-t").into_boxed_str()),
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/s3-presigned/",
                response: PRESIGNED_204,
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/packages/app-org/app-repo/upload/deb/",
                response: Box::leak(http_json(r#"{"slug_perm":"s"}"#).into_boxed_str()),
                times: None,
            },
        ]
    });
    let base = format!("http://{addr}");

    let cfg = CloudSmithConfig {
        // `app` is the project name seeded by ctx_with_one_artifact.
        organization: Some("{{ .ProjectName }}-org".to_string()),
        repository: Some("{{ .ProjectName }}-repo".to_string()),
        formats: Some(vec!["deb".to_string()]),
        republish: Some(StringOrBool::Bool(true)),
        ..Default::default()
    };
    let ctx = ctx_with_one_artifact(
        cfg,
        &base,
        ArtifactKind::LinuxPackage,
        "app_1.0.0_amd64.deb",
        art.clone(),
        None,
    );

    let result = publish_to_cloudsmith(&ctx, &StageLogger::new("cloudsmith", Verbosity::Quiet));

    let uploaded = result.expect("templated org/repo upload ok");
    assert_eq!(uploaded.len(), 1);
    assert_eq!(uploaded[0].org, "app-org", "org rendered from template");
    assert_eq!(uploaded[0].repo, "app-repo", "repo rendered from template");

    let entries = log.lock().unwrap();
    assert_eq!(
        count_calls(&entries, "POST", "/files/app-org/app-repo/"),
        1,
        "files/create routed to the rendered org/repo: {entries:?}"
    );
    assert_eq!(
        count_calls(&entries, "POST", "/packages/app-org/app-repo/upload/deb/"),
        1,
        "step-3 routed to the rendered org/repo"
    );
}

/// The step-1 files/create, the step-3 packages/upload, and the pre-check
/// list all carry the `Authorization: token <secret>` header — asserted
/// on the wire via the responder's header capture (previously
/// unobservable). Drives republish=false so the pre-check GET is on the
/// route table too.
#[test]
fn live_auth_header_present_on_every_cloudsmith_call() {
    let (result, log) = run_deb_with_routes(
        move |base| {
            let base = base.to_string();
            vec![
                precheck_route("[]"),
                ScriptedRoute {
                    method: "POST",
                    path_pattern: "/files/myorg/myrepo/",
                    response: Box::leak(files_create_ok(&base, "id-a").into_boxed_str()),
                    times: None,
                },
                ScriptedRoute {
                    method: "POST",
                    path_pattern: "/s3-presigned/",
                    response: PRESIGNED_204,
                    times: None,
                },
                ScriptedRoute {
                    method: "POST",
                    path_pattern: "/packages/myorg/myrepo/upload/deb/",
                    response: Box::leak(http_json(r#"{"slug_perm":"s"}"#).into_boxed_str()),
                    times: None,
                },
            ]
        },
        |_| {},
        None,
    );
    result.expect("upload ok");
    // The pre-check, files/create, and step-3 each go to the Cloudsmith
    // API and must carry the token auth header. The S3 presigned POST
    // must NOT (it's an unauthenticated AWS form post).
    for path in [
        PRECHECK_PATH,
        "/files/myorg/myrepo/",
        "/packages/myorg/myrepo/upload/deb/",
    ] {
        let req = log
            .iter()
            .find(|e| e.path == path)
            .unwrap_or_else(|| panic!("request to {path} recorded: {log:?}"));
        assert_eq!(
            req.header("Authorization"),
            Some("token fake-token"),
            "{path} must carry the cloudsmith token: {:?}",
            req.headers
        );
    }
    let s3 = log
        .iter()
        .find(|e| e.path == "/s3-presigned/")
        .expect("presigned post recorded");
    assert!(
        s3.header("Authorization").is_none(),
        "S3 presigned upload must NOT carry a cloudsmith auth header: {:?}",
        s3.headers
    );
}
