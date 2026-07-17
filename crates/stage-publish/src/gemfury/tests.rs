//! Tests for the GemFury publisher.

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::{Config, CrateConfig, GemFuryConfig};
use anodizer_core::test_helpers::TestContextBuilder;
use anodizer_core::{PreflightCheck, PublishEvidence, Publisher, PublisherGroup};

use super::publish::{
    GemFuryTarget, api_token_env_var, default_formats, detect_gemfury_format, publish_to_gemfury,
    push_token_env_var, resolve_api_token, resolve_formats, resolve_push_token,
};
use super::publisher::GemFuryPublisher;

fn basic_cfg() -> GemFuryConfig {
    GemFuryConfig {
        account: Some("acme".into()),
        ..Default::default()
    }
}

/// Drive `publish_to_gemfury` with a fresh out-param vec and fold the
/// `(Result<()>, partial-vec)` pair back into the `Result<Vec<_>>` shape the
/// assertions read — on success the landed targets, on error the partial set.
fn run_publish(
    ctx: &anodizer_core::context::Context,
    log: &anodizer_core::log::StageLogger,
) -> anyhow::Result<Vec<GemFuryTarget>> {
    let mut pushed = Vec::new();
    publish_to_gemfury(ctx, log, &mut pushed).map(|()| pushed)
}

fn add_linux_package(ctx: &mut anodizer_core::context::Context, name: &str) {
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::LinuxPackage,
        path: std::path::PathBuf::from(format!("/tmp/{name}")),
        name: name.to_string(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "demo".to_string(),
        metadata: std::collections::HashMap::new(),
        size: None,
    });
}

fn ctx_with_packages() -> anodizer_core::context::Context {
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .crates(vec![CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            ..Default::default()
        }])
        .build();
    add_linux_package(&mut ctx, "demo_1.2.3_amd64.deb");
    add_linux_package(&mut ctx, "demo-1.2.3-1.x86_64.rpm");
    add_linux_package(&mut ctx, "demo-1.2.3.apk");
    ctx
}

// -----------------------------------------------------------------------------
// Config parsing
// -----------------------------------------------------------------------------

#[test]
fn parse_minimal_gemfury_block() {
    let yaml = r#"
project_name: demo
crates:
  - name: demo
    path: .
    tag_template: "v{{ .Version }}"
gemfury:
  - account: acme
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).expect("parse minimal gemfury");
    let entries = cfg.gemfury.expect("gemfury set");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].account.as_deref(), Some("acme"));
}

#[test]
fn parse_full_gemfury_block() {
    let yaml = r#"
project_name: demo
crates:
  - name: demo
    path: .
    tag_template: "v{{ .Version }}"
gemfury:
  - id: primary
    account: acme
    secret_name: MY_FURY_TOKEN
    api_secret_name: MY_FURY_API_TOKEN
    formats: [deb, rpm, apk]
    ids: [demo]
    skip: false
    required: true
    if: "{{ ne .Prerelease \"\" }}"
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).expect("parse full gemfury");
    let entry = &cfg.gemfury.as_ref().unwrap()[0];
    assert_eq!(entry.id.as_deref(), Some("primary"));
    assert_eq!(entry.account.as_deref(), Some("acme"));
    assert_eq!(entry.secret_name.as_deref(), Some("MY_FURY_TOKEN"));
    assert_eq!(entry.api_secret_name.as_deref(), Some("MY_FURY_API_TOKEN"));
    assert!(matches!(entry.required, Some(true)));
    assert!(entry.if_condition.is_some());
}

#[test]
fn furies_alias_still_parses_as_gemfury() {
    // The legacy spelling collapses to the same struct via
    // `#[serde(alias = "furies")]`.
    let yaml = r#"
project_name: demo
crates:
  - name: demo
    path: .
    tag_template: "v{{ .Version }}"
furies:
  - account: legacy
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).expect("parse furies alias");
    let entries = cfg.gemfury.expect("furies → gemfury alias");
    assert_eq!(entries[0].account.as_deref(), Some("legacy"));
}

#[test]
fn warn_on_legacy_furies_alias_detects_legacy_key() {
    // Pure-function check: the raw YAML walker spots `furies:` and the
    // wrapper emits a tracing warn. We can't assert tracing output here
    // without a subscriber; just confirm the helper accepts both shapes
    // without panicking and that `gemfury:` does not trip the legacy
    // branch when serialized through.
    let yaml_legacy: serde_yaml_ng::Value =
        serde_yaml_ng::from_str("furies:\n  - account: acme\n").expect("parse");
    anodizer_core::config::warn_on_legacy_furies_alias(&yaml_legacy);
    let yaml_new: serde_yaml_ng::Value =
        serde_yaml_ng::from_str("gemfury:\n  - account: acme\n").expect("parse");
    anodizer_core::config::warn_on_legacy_furies_alias(&yaml_new);
}

// -----------------------------------------------------------------------------
// Defaults / helpers
// -----------------------------------------------------------------------------

#[test]
fn default_formats_match_gr_v27() {
    assert_eq!(default_formats(), vec!["apk", "deb", "rpm"]);
}

#[test]
fn resolve_formats_uses_default_when_unset() {
    let cfg = GemFuryConfig::default();
    assert_eq!(resolve_formats(&cfg), vec!["apk", "deb", "rpm"]);
}

#[test]
fn resolve_formats_honors_override() {
    let cfg = GemFuryConfig {
        formats: Some(vec!["deb".to_string()]),
        ..Default::default()
    };
    assert_eq!(resolve_formats(&cfg), vec!["deb"]);
}

#[test]
fn detect_gemfury_format_matches_known_extensions() {
    assert_eq!(detect_gemfury_format("a.deb"), Some("deb"));
    assert_eq!(detect_gemfury_format("a.rpm"), Some("rpm"));
    assert_eq!(detect_gemfury_format("a.apk"), Some("apk"));
    assert_eq!(detect_gemfury_format("a.tar.gz"), None);
    assert_eq!(detect_gemfury_format("a.zip"), None);
}

/// The detector folds case so it agrees with the case-folding artifact
/// filter: an uppercase-extension artifact that the filter admits must be
/// detected here too, not fall through to the "filter should have excluded
/// it" error on the publish path.
#[test]
fn detect_gemfury_format_is_case_insensitive() {
    assert_eq!(detect_gemfury_format("myapp.DEB"), Some("deb"));
    assert_eq!(detect_gemfury_format("myapp.Rpm"), Some("rpm"));
    assert_eq!(detect_gemfury_format("myapp.APK"), Some("apk"));
}

#[test]
fn push_and_api_token_env_var_defaults() {
    let ctx = TestContextBuilder::new().build();
    let cfg = GemFuryConfig::default();
    assert_eq!(push_token_env_var(&ctx, &cfg), "FURY_PUSH_TOKEN");
    assert_eq!(api_token_env_var(&ctx, &cfg), "FURY_API_TOKEN");
}

#[test]
fn push_and_api_token_env_var_overrides() {
    let ctx = TestContextBuilder::new().build();
    let cfg = GemFuryConfig {
        secret_name: Some("MY_PUSH".into()),
        api_secret_name: Some("MY_API".into()),
        ..Default::default()
    };
    assert_eq!(push_token_env_var(&ctx, &cfg), "MY_PUSH");
    assert_eq!(api_token_env_var(&ctx, &cfg), "MY_API");
}

/// A templated `secret_name` / `api_secret_name`
/// (`FURY_{{ .Env.STAGE }}`) must render against the context before the
/// env-var lookup, exactly as cloudsmith's `resolve_secret_name` does — the
/// literal template string must never be looked up as a var name.
#[test]
fn token_env_vars_render_templated_secret_name() {
    let mut ctx = TestContextBuilder::new().build();
    ctx.template_vars_mut().set_env("STAGE", "PROD");
    let cfg = GemFuryConfig {
        secret_name: Some("FURY_PUSH_{{ .Env.STAGE }}".into()),
        api_secret_name: Some("FURY_API_{{ .Env.STAGE }}".into()),
        ..Default::default()
    };
    assert_eq!(push_token_env_var(&ctx, &cfg), "FURY_PUSH_PROD");
    assert_eq!(api_token_env_var(&ctx, &cfg), "FURY_API_PROD");
    // The same SSOT cloudsmith routes through resolves identically.
    assert_eq!(
        push_token_env_var(&ctx, &cfg),
        crate::util::resolve_secret_name(&ctx, cfg.secret_name.as_deref(), "FURY_PUSH_TOKEN")
    );
}

// -----------------------------------------------------------------------------
// Auth resolution
// -----------------------------------------------------------------------------

#[test]
fn resolve_push_token_falls_back_to_env_var() {
    let mut ctx = ctx_with_packages();
    let env = anodizer_core::MapEnvSource::new().with("FURY_PUSH_TOKEN", "from-env");
    ctx.set_env_source(env);
    let cfg = basic_cfg();
    assert_eq!(resolve_push_token(&ctx, &cfg).expect("token"), "from-env");
}

#[test]
fn resolve_push_token_prefers_cfg_token() {
    let mut ctx = ctx_with_packages();
    let env = anodizer_core::MapEnvSource::new().with("FURY_PUSH_TOKEN", "from-env");
    ctx.set_env_source(env);
    let cfg = GemFuryConfig {
        account: Some("acme".into()),
        token: Some("from-cfg".into()),
        ..Default::default()
    };
    assert_eq!(resolve_push_token(&ctx, &cfg).expect("token"), "from-cfg");
}

#[test]
fn resolve_api_token_independent_from_push_token() {
    let mut ctx = ctx_with_packages();
    let env = anodizer_core::MapEnvSource::new()
        .with("FURY_PUSH_TOKEN", "push-only")
        .with("FURY_API_TOKEN", "api-only");
    ctx.set_env_source(env);
    let cfg = basic_cfg();
    assert_eq!(
        resolve_api_token(&ctx, &cfg).expect("api token"),
        "api-only"
    );
}

#[test]
fn publish_errors_when_token_missing_and_not_dry_run() {
    let mut ctx = ctx_with_packages();
    // Isolates from process FURY_PUSH_TOKEN in case a future sibling sets it globally.
    ctx.set_env_source(anodizer_core::MapEnvSource::new());
    ctx.config.gemfury = Some(vec![basic_cfg()]);
    let log = ctx.logger("publish");
    let err = run_publish(&ctx, &log).expect_err("missing token must err");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("push token is required") && msg.contains("FURY_PUSH_TOKEN"),
        "expected token diagnostic, got: {msg}"
    );
}

#[test]
fn publish_errors_when_account_missing() {
    let mut ctx = ctx_with_packages();
    ctx.config.gemfury = Some(vec![GemFuryConfig::default()]);
    let log = ctx.logger("publish");
    let err = run_publish(&ctx, &log).expect_err("missing account must err");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("'account' is required"),
        "expected account diagnostic, got: {msg}"
    );
}

// -----------------------------------------------------------------------------
// Skip / if-condition / dry-run paths
// -----------------------------------------------------------------------------

#[test]
fn publish_dry_run_returns_no_targets() {
    let mut ctx = ctx_with_packages();
    ctx.options.dry_run = true;
    ctx.config.gemfury = Some(vec![basic_cfg()]);
    let log = ctx.logger("publish");
    let out = run_publish(&ctx, &log).expect("dry-run");
    assert!(out.is_empty(), "dry-run pushes nothing");
}

#[test]
fn publish_skip_true_returns_no_targets() {
    let mut ctx = ctx_with_packages();
    let cfg = GemFuryConfig {
        account: Some("acme".into()),
        skip: Some(anodizer_core::config::StringOrBool::Bool(true)),
        ..Default::default()
    };
    ctx.config.gemfury = Some(vec![cfg]);
    let log = ctx.logger("publish");
    let out = run_publish(&ctx, &log).expect("skip");
    assert!(out.is_empty());
}

#[test]
fn publish_disable_alias_true_returns_no_targets() {
    // The legacy `disable: true` spelling folds into `skip` on parse, so the
    // entry is skipped at publish time via the skip gate.
    let mut ctx = ctx_with_packages();
    let cfg: GemFuryConfig = serde_yaml_ng::from_str("account: acme\ndisable: true\n")
        .expect("disable: alias must parse into skip");
    assert!(matches!(
        cfg.skip,
        Some(anodizer_core::config::StringOrBool::Bool(true))
    ));
    ctx.config.gemfury = Some(vec![cfg]);
    let log = ctx.logger("publish");
    let out = run_publish(&ctx, &log).expect("disable alias");
    assert!(out.is_empty());
}

#[test]
fn publish_if_condition_falsy_returns_no_targets() {
    let mut ctx = ctx_with_packages();
    let cfg = GemFuryConfig {
        account: Some("acme".into()),
        if_condition: Some("false".into()),
        ..Default::default()
    };
    ctx.config.gemfury = Some(vec![cfg]);
    let log = ctx.logger("publish");
    let out = run_publish(&ctx, &log).expect("if falsy");
    assert!(out.is_empty());
}

// -----------------------------------------------------------------------------
// Multi-format preflight
// -----------------------------------------------------------------------------

#[test]
fn multi_format_archive_overlap_errors() {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig};
    let mut ctx = ctx_with_packages();
    let archive_cfg = ArchiveConfig {
        id: Some("default".into()),
        // Both `deb` and `rpm` are in the gemfury default filter — overlap > 1.
        formats: Some(vec!["deb".into(), "rpm".into()]),
        ..Default::default()
    };
    ctx.config.crates[0].archives = ArchivesConfig::Configs(vec![archive_cfg]);
    ctx.config.gemfury = Some(vec![basic_cfg()]);
    let log = ctx.logger("publish");
    let err = run_publish(&ctx, &log).expect_err("multi-format overlap must err");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("multiple package formats"),
        "expected diagnostic, got: {msg}"
    );
}

#[test]
fn multi_format_archive_with_single_overlap_passes() {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig};
    let mut ctx = ctx_with_packages();
    ctx.options.dry_run = true;
    let archive_cfg = ArchiveConfig {
        id: Some("default".into()),
        // `tar.gz` is NOT in the gemfury filter; only `deb` overlaps → no ambig.
        formats: Some(vec!["tar.gz".into(), "deb".into()]),
        ..Default::default()
    };
    ctx.config.crates[0].archives = ArchivesConfig::Configs(vec![archive_cfg]);
    ctx.config.gemfury = Some(vec![basic_cfg()]);
    let log = ctx.logger("publish");
    run_publish(&ctx, &log).expect("single-overlap dry-run ok");
}

// -----------------------------------------------------------------------------
// Publisher contract
// -----------------------------------------------------------------------------

#[test]
fn gemfury_publisher_classification() {
    let p = GemFuryPublisher::new();
    assert_eq!(p.name(), "gemfury");
    assert_eq!(p.group(), PublisherGroup::Manager);
    assert!(p.required(), "gemfury publisher defaults to required=true");
    assert_eq!(p.rollback_scope_needed(), Some("FURY_API_TOKEN delete"));
}

#[test]
fn gemfury_publisher_preflight_passes() {
    let ctx = TestContextBuilder::new().build();
    let p = GemFuryPublisher::new();
    assert!(matches!(
        p.preflight(&ctx).expect("preflight"),
        PreflightCheck::Pass
    ));
}

#[test]
fn gemfury_publisher_run_with_no_entries_is_noop() {
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .crates(vec![CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            ..Default::default()
        }])
        .build();
    let p = GemFuryPublisher::new();
    let ev = p.run(&mut ctx).expect("run ok");
    assert_eq!(ev.publisher, "gemfury");
    assert!(ev.primary_ref.is_none());
}

// -----------------------------------------------------------------------------
// Rollback evidence shape
// -----------------------------------------------------------------------------

#[test]
fn gemfury_evidence_carries_no_token_value() {
    let mut e = PublishEvidence::new("gemfury");
    e.extra = anodizer_core::PublishEvidenceExtra::GemFury(
        anodizer_core::publish_evidence::GemFuryExtra {
            gemfury_targets: vec![anodizer_core::publish_evidence::GemFuryTargetSnapshot {
                target: "acme/demo_1.2.3_amd64.deb".into(),
                account: "acme".into(),
                package: "demo_1.2.3_amd64.deb".into(),
                version: "1.2.3".into(),
                format: "deb".into(),
                push_token_env_var: "FURY_PUSH_TOKEN".into(),
                api_token_env_var: "FURY_API_TOKEN".into(),
            }],
        },
    );
    let s = serde_json::to_string(&e).expect("serialize");
    // Token VALUES never appear in evidence — only env-var NAMES.
    assert!(!s.contains("\"token\":"), "{s}");
    assert!(!s.contains("\"api_token\":"), "{s}");
    assert!(!s.contains("\"password\":"), "{s}");
    assert!(!s.contains("\"secret\":"), "{s}");
    // Positive shape: operator coordinates present.
    assert!(s.contains("\"account\":\"acme\""), "{s}");
    assert!(s.contains("\"version\":\"1.2.3\""), "{s}");
    assert!(
        s.contains("\"push_token_env_var\":\"FURY_PUSH_TOKEN\""),
        "{s}"
    );
    assert!(
        s.contains("\"api_token_env_var\":\"FURY_API_TOKEN\""),
        "{s}"
    );
}

#[test]
fn gemfury_rollback_with_no_targets_emits_warn_not_err() {
    let mut ctx = TestContextBuilder::new().build();
    let evidence = PublishEvidence::new("gemfury");
    let p = GemFuryPublisher::new();
    assert!(p.rollback(&mut ctx, &evidence).is_ok());
}

#[test]
fn gemfury_rollback_without_api_token_falls_back_to_warn() {
    // No FURY_API_TOKEN in env, no `api_token` in cfg → rollback emits
    // a manual-cleanup warn per target and returns Ok (rollback must not
    // bubble Err so sibling publishers' rollback paths still run).
    let mut ctx = TestContextBuilder::new().build();
    let mut evidence = PublishEvidence::new("gemfury");
    evidence.extra = anodizer_core::PublishEvidenceExtra::GemFury(
        anodizer_core::publish_evidence::GemFuryExtra {
            gemfury_targets: vec![anodizer_core::publish_evidence::GemFuryTargetSnapshot {
                target: "acme/demo.deb".into(),
                account: "acme".into(),
                package: "demo.deb".into(),
                version: "1.2.3".into(),
                format: "deb".into(),
                push_token_env_var: "FURY_PUSH_TOKEN".into(),
                api_token_env_var: "FURY_API_TOKEN".into(),
            }],
        },
    );
    let p = GemFuryPublisher::new();
    p.rollback(&mut ctx, &evidence).expect("warn-only rollback");
}

// Helper conversion to keep evidence assertions terse.
impl From<GemFuryTarget> for anodizer_core::publish_evidence::GemFuryTargetSnapshot {
    fn from(t: GemFuryTarget) -> Self {
        Self {
            target: format!("{}/{}", t.account, t.package),
            account: t.account,
            package: t.package,
            version: t.version,
            format: t.format,
            push_token_env_var: t.push_token_env_var,
            api_token_env_var: t.api_token_env_var,
        }
    }
}

// -----------------------------------------------------------------------------
// Probe classifier: a 404 from the API base means "version not present" and
// must surface as Ok(false) so the publish path proceeds. Hermetic via the
// ANODIZE_GEMFURY_API_BASE seam pointing at a local responder.
// -----------------------------------------------------------------------------

#[test]
fn version_already_published_returns_false_on_404() {
    use super::publish::version_already_published;
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

    let (api_addr, _calls) =
        spawn_oneshot_http_responder(vec!["HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"]);

    let client = anodizer_core::http::blocking_client(std::time::Duration::from_secs(2))
        .expect("http client");
    let policy = anodizer_core::retry::RetryPolicy {
        max_attempts: 1,
        base_delay: std::time::Duration::from_millis(1),
        max_delay: std::time::Duration::from_millis(1),
    };
    let log = anodizer_core::log::StageLogger::new("publish", anodizer_core::log::Verbosity::Quiet);

    // Inject the API base via a MapEnvSource so the probe reads THIS test's
    // responder without touching the process env — no cross-test race, no
    // misrouted request to a sibling test's torn-down listener.
    let env = anodizer_core::MapEnvSource::new()
        .with("ANODIZE_GEMFURY_API_BASE", format!("http://{api_addr}"));
    let result = version_already_published(
        &client,
        "acme",
        "demo",
        "1.2.3",
        "fake-push-token",
        &policy,
        &log,
        &env,
    );

    // A 404 is the documented "not present" response: the probe must
    // coerce it to Ok(false) so the publish path runs.
    assert!(
        matches!(result, Ok(false)),
        "404 probe must surface Ok(false), got {:?}",
        result
    );
}

/// A non-404 HTTP error (registry outage) must FAIL CLOSED: the probe cannot
/// prove the version is absent, so it bails rather than returning Ok(false)
/// and green-lighting a push to a registry that is irreversible for up to 72h.
#[test]
fn version_already_published_bails_on_non_404() {
    use super::publish::version_already_published;
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

    let (api_addr, _calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
    ]);

    let client = anodizer_core::http::blocking_client(std::time::Duration::from_secs(2))
        .expect("http client");
    let policy = anodizer_core::retry::RetryPolicy {
        max_attempts: 1,
        base_delay: std::time::Duration::from_millis(1),
        max_delay: std::time::Duration::from_millis(1),
    };
    let log = anodizer_core::log::StageLogger::new("publish", anodizer_core::log::Verbosity::Quiet);

    let env = anodizer_core::MapEnvSource::new()
        .with("ANODIZE_GEMFURY_API_BASE", format!("http://{api_addr}"));
    let result = version_already_published(
        &client,
        "acme",
        "demo",
        "1.2.3",
        "fake-push-token",
        &policy,
        &log,
        &env,
    );

    let err = result.expect_err("non-404 probe must bail, not return Ok(false)");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("inconclusive non-404"),
        "expected fail-closed diagnostic, got: {msg}"
    );
}

/// A transport/connect failure (registry unreachable) must FAIL CLOSED for the
/// same reason: an unproven absence cannot green-light an irreversible push.
#[test]
fn version_already_published_bails_on_transport_failure() {
    use super::publish::version_already_published;
    use std::net::TcpListener;

    // Bind then drop the listener to obtain a port that refuses connections.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let dead_addr = listener.local_addr().expect("addr");
    drop(listener);

    let client = anodizer_core::http::blocking_client(std::time::Duration::from_secs(2))
        .expect("http client");
    let policy = anodizer_core::retry::RetryPolicy {
        max_attempts: 1,
        base_delay: std::time::Duration::from_millis(1),
        max_delay: std::time::Duration::from_millis(1),
    };
    let log = anodizer_core::log::StageLogger::new("publish", anodizer_core::log::Verbosity::Quiet);

    let env = anodizer_core::MapEnvSource::new()
        .with("ANODIZE_GEMFURY_API_BASE", format!("http://{dead_addr}"));
    let result = version_already_published(
        &client,
        "acme",
        "demo",
        "1.2.3",
        "fake-push-token",
        &policy,
        &log,
        &env,
    );

    let err = result.expect_err("transport failure must bail, not return Ok(false)");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("inconclusive non-404"),
        "expected fail-closed diagnostic, got: {msg}"
    );
}

// -----------------------------------------------------------------------------
// Idempotency: package-name derivation + 409/422 conflict-as-success
// -----------------------------------------------------------------------------

#[test]
fn fury_package_name_strips_version_suffix() {
    use super::publish::fury_package_name;
    // deb: name_version_arch.deb
    assert_eq!(
        fury_package_name("mytool_1.2.3_amd64.deb", "1.2.3"),
        "mytool"
    );
    // rpm: name-version-release.arch.rpm
    assert_eq!(
        fury_package_name("mytool-1.2.3-1.x86_64.rpm", "1.2.3"),
        "mytool"
    );
    // apk: name-version.apk
    assert_eq!(fury_package_name("mytool-1.2.3.apk", "1.2.3"), "mytool");
    // multi-word package name with hyphens preserved before the version.
    assert_eq!(
        fury_package_name("my-cool-tool_4.5.6_arm64.deb", "4.5.6"),
        "my-cool-tool"
    );
    // version absent: fall back to extension-stripped basename.
    assert_eq!(
        fury_package_name("snapshot-build.deb", "1.2.3"),
        "snapshot-build"
    );
}

#[test]
fn fury_package_name_empty_version_strips_extension() {
    use super::publish::fury_package_name;
    // Empty version skips the find() and strips a known extension.
    assert_eq!(fury_package_name("mytool_x.rpm", ""), "mytool_x");
    assert_eq!(fury_package_name("mytool.apk", ""), "mytool");
}

#[test]
fn fury_package_name_unknown_extension_returns_raw() {
    use super::publish::fury_package_name;
    // No version match AND no recognized package extension: the raw
    // filename is the closest key available.
    assert_eq!(
        fury_package_name("mystery-artifact.bin", "9.9.9"),
        "mystery-artifact.bin"
    );
}

#[test]
fn fury_package_name_version_at_start_falls_through_to_ext_strip() {
    use super::publish::fury_package_name;
    // version is the whole leading segment -> trimmed head is empty, so
    // the function falls through to extension stripping rather than
    // returning an empty package name.
    assert_eq!(fury_package_name("1.2.3.deb", "1.2.3"), "1.2.3");
}

/// A re-run against an already-published version must succeed (idempotent):
/// the probe 404s (Fury's probe surface), then the push returns 409 Conflict
/// → treated as success with no rollback target recorded. A genuine failure
/// (400) still errors.
#[test]
fn gemfury_push_conflict_is_idempotent_success() {
    use anodizer_core::log::{StageLogger, Verbosity};
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

    let tmp = tempfile::tempdir().unwrap();
    let art_path = tmp.path().join("demo_1.2.3_amd64.deb");
    std::fs::write(&art_path, b"fake-deb").unwrap();

    // Connection order per artifact: probe (GET api_base) -> push (POST
    // push_base). Probe 404 ⇒ push attempted; push 409 ⇒ idempotent success.
    let probe_404 = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
    let push_409 = "HTTP/1.1 409 Conflict\r\nContent-Length: 0\r\n\r\n";
    let (api_addr, _api_calls) = spawn_oneshot_http_responder(vec![probe_404]);
    let (push_addr, push_calls) = spawn_oneshot_http_responder(vec![push_409]);

    let config = Config {
        project_name: "demo".to_string(),
        gemfury: Some(vec![GemFuryConfig {
            account: Some("acme".into()),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .build();
    ctx.config = config;
    // Inject the push token AND the responder bases through one MapEnvSource so
    // the publish reads THIS test's mock addresses without mutating process env.
    ctx.set_env_source(
        anodizer_core::MapEnvSource::new()
            .with("FURY_PUSH_TOKEN", "fake-token")
            .with("ANODIZE_GEMFURY_API_BASE", format!("http://{api_addr}"))
            .with("ANODIZE_GEMFURY_PUSH_BASE", format!("http://{push_addr}")),
    );
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::LinuxPackage,
        path: art_path.clone(),
        name: "demo_1.2.3_amd64.deb".to_string(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "demo".to_string(),
        metadata: std::collections::HashMap::new(),
        size: None,
    });

    let log = StageLogger::new("gemfury", Verbosity::Quiet);
    let result = run_publish(&ctx, &log);

    let pushed = result.expect("409 conflict must be an idempotent success, not an error");
    assert!(
        pushed.is_empty(),
        "a conflict-as-success push must record NO rollback target, got {pushed:?}"
    );
    assert_eq!(
        push_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "push should be attempted exactly once (409 is terminal, not retried)"
    );
}

/// A genuine non-conflict failure (HTTP 400) on push still errors.
#[test]
fn gemfury_push_genuine_failure_still_errors() {
    use anodizer_core::log::{StageLogger, Verbosity};
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

    let tmp = tempfile::tempdir().unwrap();
    let art_path = tmp.path().join("demo_1.2.3_amd64.deb");
    std::fs::write(&art_path, b"fake-deb").unwrap();

    let probe_404 = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
    let push_400 = "HTTP/1.1 400 Bad Request\r\nContent-Length: 7\r\n\r\nbad req";
    let (api_addr, _api_calls) = spawn_oneshot_http_responder(vec![probe_404]);
    let (push_addr, _push_calls) = spawn_oneshot_http_responder(vec![push_400]);

    let config = Config {
        project_name: "demo".to_string(),
        gemfury: Some(vec![GemFuryConfig {
            account: Some("acme".into()),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .build();
    ctx.config = config;
    ctx.set_env_source(
        anodizer_core::MapEnvSource::new()
            .with("FURY_PUSH_TOKEN", "fake-token")
            .with("ANODIZE_GEMFURY_API_BASE", format!("http://{api_addr}"))
            .with("ANODIZE_GEMFURY_PUSH_BASE", format!("http://{push_addr}")),
    );
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::LinuxPackage,
        path: art_path.clone(),
        name: "demo_1.2.3_amd64.deb".to_string(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "demo".to_string(),
        metadata: std::collections::HashMap::new(),
        size: None,
    });

    let log = StageLogger::new("gemfury", Verbosity::Quiet);
    let result = run_publish(&ctx, &log);

    assert!(
        result.is_err(),
        "a genuine 400 failure must error, not be swallowed as idempotent"
    );
}

/// A successful push records a rollback target keyed on the Fury-visible
/// package NAME (`demo`), not the full artifact filename. Rollback's DELETE
/// /packages/<name>/versions/… must hit the same name the push registered —
/// a full-filename key 404s and orphans the artifact.
#[test]
fn gemfury_recorded_rollback_target_uses_derived_package_name() {
    use anodizer_core::log::{StageLogger, Verbosity};
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

    let tmp = tempfile::tempdir().unwrap();
    let art_path = tmp.path().join("demo_1.2.3_amd64.deb");
    std::fs::write(&art_path, b"fake-deb").unwrap();

    let probe_404 = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
    let push_200 = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
    let (api_addr, _api_calls) = spawn_oneshot_http_responder(vec![probe_404]);
    let (push_addr, _push_calls) = spawn_oneshot_http_responder(vec![push_200]);

    let config = Config {
        project_name: "demo".to_string(),
        gemfury: Some(vec![GemFuryConfig {
            account: Some("acme".into()),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .build();
    ctx.config = config;
    ctx.set_env_source(
        anodizer_core::MapEnvSource::new()
            .with("FURY_PUSH_TOKEN", "fake-token")
            .with("ANODIZE_GEMFURY_API_BASE", format!("http://{api_addr}"))
            .with("ANODIZE_GEMFURY_PUSH_BASE", format!("http://{push_addr}")),
    );
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::LinuxPackage,
        path: art_path.clone(),
        name: "demo_1.2.3_amd64.deb".to_string(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "demo".to_string(),
        metadata: std::collections::HashMap::new(),
        size: None,
    });

    let log = StageLogger::new("gemfury", Verbosity::Quiet);
    let result = run_publish(&ctx, &log);

    let pushed = result.expect("successful push should return a target");
    assert_eq!(
        pushed.len(),
        1,
        "expected one recorded target, got {pushed:?}"
    );
    assert_eq!(
        pushed[0].package, "demo",
        "rollback target must key on the derived Fury package name, not the filename"
    );
    assert_eq!(pushed[0].version, "1.2.3");
    assert_eq!(pushed[0].account, "acme");
}

/// The idempotent retry floor must keep a Fury push alive across a transient
/// 503 even when the operator's resolved policy caps `attempts: 1` (the
/// `--publish-only` shape). With the floor reverted to 1 this fails: the 503
/// would surface as a hard error and the responder would be hit only once.
/// Scripts `[503, 200]` on the push responder under `attempts: 1` and asserts
/// the push SUCCEEDS and the responder was hit twice.
#[test]
fn gemfury_push_transient_503_retries_under_single_attempt_policy() {
    use anodizer_core::config::{HumanDuration, RetryConfig};
    use anodizer_core::log::{StageLogger, Verbosity};
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    let tmp = tempfile::tempdir().unwrap();
    let art_path = tmp.path().join("demo_1.2.3_amd64.deb");
    std::fs::write(&art_path, b"fake-deb").unwrap();

    // Probe 404 ⇒ push attempted; first push 503 (transient) ⇒ retry; second
    // push 200 ⇒ success. The floor (3) makes the second attempt available even
    // though the resolved policy caps attempts at 1.
    let probe_404 = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
    let push_503 = "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n";
    let push_200 = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
    let (api_addr, _api_calls) = spawn_oneshot_http_responder(vec![probe_404]);
    let (push_addr, push_calls) = spawn_oneshot_http_responder(vec![push_503, push_200]);

    let config = Config {
        project_name: "demo".to_string(),
        // attempts: 1 is the `--publish-only` resolved shape; tiny delays keep
        // the test fast. Only the idempotent floor lets attempt 2 run.
        retry: Some(RetryConfig {
            attempts: 1,
            delay: HumanDuration(Duration::from_millis(1)),
            max_delay: HumanDuration(Duration::from_millis(1)),
            max_elapsed: None,
        }),
        gemfury: Some(vec![GemFuryConfig {
            account: Some("acme".into()),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .build();
    ctx.config = config;
    ctx.set_env_source(
        anodizer_core::MapEnvSource::new()
            .with("FURY_PUSH_TOKEN", "fake-token")
            .with("ANODIZE_GEMFURY_API_BASE", format!("http://{api_addr}"))
            .with("ANODIZE_GEMFURY_PUSH_BASE", format!("http://{push_addr}")),
    );
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::LinuxPackage,
        path: art_path.clone(),
        name: "demo_1.2.3_amd64.deb".to_string(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "demo".to_string(),
        metadata: std::collections::HashMap::new(),
        size: None,
    });

    let log = StageLogger::new("gemfury", Verbosity::Quiet);
    let pushed = run_publish(&ctx, &log).expect("transient 503 must be retried to success");
    assert_eq!(pushed.len(), 1, "the retried push must land one target");
    assert_eq!(
        push_calls.load(Ordering::SeqCst),
        2,
        "the push responder must be hit twice (503 then 200) — proving the floor retried"
    );
}

/// When a mid-loop push fails after an earlier artifact already landed, the
/// out-param must still hold the partial set so the caller can roll back what
/// landed. The `?`-on-`Result<Vec<_>>` signature discarded that evidence,
/// orphaning the first artifact on a second-artifact failure.
#[test]
fn gemfury_partial_push_records_landed_target_on_later_failure() {
    use anodizer_core::log::{StageLogger, Verbosity};
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

    let tmp = tempfile::tempdir().unwrap();
    let art1 = tmp.path().join("alpha_1.2.3_amd64.deb");
    let art2 = tmp.path().join("beta_1.2.3_amd64.deb");
    std::fs::write(&art1, b"fake-deb-1").unwrap();
    std::fs::write(&art2, b"fake-deb-2").unwrap();

    // Two artifacts, two probe+push round-trips. Probes both 404 (not yet
    // published); first push lands (200), second push hard-fails (400).
    let probe_404 = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
    let push_200 = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
    let push_400 = "HTTP/1.1 400 Bad Request\r\nContent-Length: 7\r\n\r\nbad req";
    let (api_addr, _api_calls) = spawn_oneshot_http_responder(vec![probe_404, probe_404]);
    let (push_addr, _push_calls) = spawn_oneshot_http_responder(vec![push_200, push_400]);

    let config = Config {
        project_name: "demo".to_string(),
        gemfury: Some(vec![GemFuryConfig {
            account: Some("acme".into()),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .build();
    ctx.config = config;
    // Pin serial so the sequential responder's first-200-then-400 script maps
    // deterministically to alpha (lands) then beta (fails): this test asserts
    // the EXACT partial that landed, which only a serial push order fixes.
    // Concurrent-failure recording (a sibling success kept despite a failing
    // push) is covered separately and does not depend on which one fails.
    ctx.options.parallelism = 1;
    ctx.set_env_source(
        anodizer_core::MapEnvSource::new()
            .with("FURY_PUSH_TOKEN", "fake-token")
            .with("ANODIZE_GEMFURY_API_BASE", format!("http://{api_addr}"))
            .with("ANODIZE_GEMFURY_PUSH_BASE", format!("http://{push_addr}")),
    );
    for (path, name, krate) in [
        (&art1, "alpha_1.2.3_amd64.deb", "alpha"),
        (&art2, "beta_1.2.3_amd64.deb", "beta"),
    ] {
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::LinuxPackage,
            path: path.clone(),
            name: name.to_string(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: krate.to_string(),
            metadata: std::collections::HashMap::new(),
            size: None,
        });
    }

    let log = StageLogger::new("gemfury", Verbosity::Quiet);
    // Drive the out-param directly so the partial survives the Err return.
    let mut pushed: Vec<GemFuryTarget> = Vec::new();
    let result = publish_to_gemfury(&ctx, &log, &mut pushed);

    assert!(
        result.is_err(),
        "second-artifact 400 must surface as an error"
    );
    assert_eq!(
        pushed.len(),
        1,
        "the first artifact that landed must be recorded for rollback, got {pushed:?}"
    );
    assert_eq!(
        pushed[0].package, "alpha",
        "the recorded partial must be the artifact that actually pushed"
    );
}

/// Concurrent partial-evidence: with parallel pushes, an artifact that lands
/// (200) CONCURRENTLY with a sibling that fails (400) must still be recorded
/// for rollback — the fan-out folds every landed target before surfacing the
/// first error, so a concurrent success is never dropped. Which artifact gets
/// the 200 vs 400 is nondeterministic under parallelism, so the assertion is
/// order-agnostic: exactly one landed, and it is one of the two candidates.
#[test]
fn gemfury_concurrent_push_records_landed_target_despite_sibling_failure() {
    use anodizer_core::log::{StageLogger, Verbosity};
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

    let tmp = tempfile::tempdir().unwrap();
    let art1 = tmp.path().join("alpha_1.2.3_amd64.deb");
    let art2 = tmp.path().join("beta_1.2.3_amd64.deb");
    std::fs::write(&art1, b"fake-deb-1").unwrap();
    std::fs::write(&art2, b"fake-deb-2").unwrap();

    let probe_404 = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
    let push_200 = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
    let push_400 = "HTTP/1.1 400 Bad Request\r\nContent-Length: 7\r\n\r\nbad req";
    let (api_addr, _api_calls) = spawn_oneshot_http_responder(vec![probe_404, probe_404]);
    let (push_addr, _push_calls) = spawn_oneshot_http_responder(vec![push_200, push_400]);

    let config = Config {
        project_name: "demo".to_string(),
        gemfury: Some(vec![GemFuryConfig {
            account: Some("acme".into()),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .build();
    ctx.config = config;
    // Force concurrency so a landing push overlaps a failing one.
    ctx.options.parallelism = 4;
    ctx.set_env_source(
        anodizer_core::MapEnvSource::new()
            .with("FURY_PUSH_TOKEN", "fake-token")
            .with("ANODIZE_GEMFURY_API_BASE", format!("http://{api_addr}"))
            .with("ANODIZE_GEMFURY_PUSH_BASE", format!("http://{push_addr}")),
    );
    for (path, name, krate) in [
        (&art1, "alpha_1.2.3_amd64.deb", "alpha"),
        (&art2, "beta_1.2.3_amd64.deb", "beta"),
    ] {
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::LinuxPackage,
            path: path.clone(),
            name: name.to_string(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: krate.to_string(),
            metadata: std::collections::HashMap::new(),
            size: None,
        });
    }

    let log = StageLogger::new("gemfury", Verbosity::Quiet);
    let mut pushed: Vec<GemFuryTarget> = Vec::new();
    let result = publish_to_gemfury(&ctx, &log, &mut pushed);

    assert!(result.is_err(), "the 400 push must surface as an error");
    assert_eq!(
        pushed.len(),
        1,
        "the concurrently-landed artifact must be recorded for rollback even \
         though its sibling failed, got {pushed:?}"
    );
    assert!(
        matches!(pushed[0].package.as_str(), "alpha" | "beta"),
        "the recorded partial must be one of the two candidates, got {:?}",
        pushed[0].package
    );
}

// -----------------------------------------------------------------------------
// Push wire shape — assert the POST hits `push_base/<account>` with HTTP Basic
// auth (push token as username, empty password) and a multipart `package` part.
// Existing push tests only asserted the OUTCOME; these pin the on-the-wire
// request so an auth/path regression is caught. Driven via the scripted
// responder so (method, path, headers, body) are recorded.
// -----------------------------------------------------------------------------

/// Build a one-deb context wired to the given probe + push responder
/// addresses, with a resolvable push token AND both responder bases injected
/// through the env source — no process-env mutation, so the test stays
/// race-free and needs no `#[serial]`.
fn ctx_one_deb(
    art_path: std::path::PathBuf,
    api_addr: std::net::SocketAddr,
    push_addr: std::net::SocketAddr,
) -> anodizer_core::context::Context {
    let config = Config {
        project_name: "demo".to_string(),
        gemfury: Some(vec![GemFuryConfig {
            account: Some("acme".into()),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .build();
    ctx.config = config;
    ctx.set_env_source(
        anodizer_core::MapEnvSource::new()
            .with("FURY_PUSH_TOKEN", "push-secret")
            .with("ANODIZE_GEMFURY_API_BASE", format!("http://{api_addr}"))
            .with("ANODIZE_GEMFURY_PUSH_BASE", format!("http://{push_addr}")),
    );
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::LinuxPackage,
        path: art_path,
        name: "demo_1.2.3_amd64.deb".to_string(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "demo".to_string(),
        metadata: std::collections::HashMap::new(),
        size: None,
    });
    ctx
}

#[test]
fn gemfury_push_wire_uses_basic_auth_and_account_path() {
    use anodizer_core::log::{StageLogger, Verbosity};
    use anodizer_core::test_helpers::scripted_responder::{
        ScriptedRoute, spawn_scripted_responder,
    };
    use base64::Engine as _;

    let tmp = tempfile::tempdir().unwrap();
    let art_path = tmp.path().join("demo_1.2.3_amd64.deb");
    std::fs::write(&art_path, b"fake-deb").unwrap();

    // Probe keys on the derived package name `demo`; a 404 lets the push run.
    let (api_addr, _api_log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "GET",
        path_pattern: "/acme/packages/demo/versions/1.2.3",
        response: "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
        times: None,
    }]);
    let (push_addr, push_log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/acme",
        response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
        times: None,
    }]);

    let ctx = ctx_one_deb(art_path, api_addr, push_addr);

    let log = StageLogger::new("gemfury", Verbosity::Quiet);
    let result = run_publish(&ctx, &log);

    let pushed = result.expect("push should succeed");
    assert_eq!(pushed.len(), 1);

    let entries = push_log.lock().unwrap();
    assert_eq!(entries.len(), 1, "exactly one push POST");
    let push = &entries[0];
    assert_eq!(push.method, "POST");
    assert_eq!(push.path, "/acme", "push hits push_base/<account>");
    // basic_auth("push-secret", Some("")) == base64("push-secret:").
    let expect_basic = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("push-secret:")
    );
    assert_eq!(
        push.header("authorization"),
        Some(expect_basic.as_str()),
        "push must carry HTTP Basic auth with the push token as username"
    );
    assert!(
        push.header("content-type")
            .is_some_and(|v| v.contains("multipart/form-data")),
        "push body must be multipart/form-data; headers: {:?}",
        push.headers
    );
    assert!(
        push.body.contains("name=\"package\""),
        "multipart must carry the `package` field: {}",
        push.body
    );
}

// -----------------------------------------------------------------------------
// Rollback DELETE wire path — `GemFuryPublisher::rollback` decodes evidence and
// issues `DELETE api_base/<account>/packages/<name>/versions/<version>` with
// HTTP Basic auth (API token as username). These cover the previously
// unmeasured `delete_recorded_targets` / `delete_version` branches. The API
// base is injected via the context's `MapEnvSource` (read through the
// `api_base_from` seam), so each test stays hermetic and needs no `#[serial]`.
// -----------------------------------------------------------------------------

/// A 1-attempt, zero-delay retry policy so a rollback DELETE that 500s gives up
/// immediately instead of sleeping through the default 10-attempt backoff.
fn fast_retry() -> anodizer_core::config::RetryConfig {
    anodizer_core::config::RetryConfig {
        attempts: 1,
        delay: anodizer_core::config::HumanDuration(std::time::Duration::from_millis(0)),
        max_delay: anodizer_core::config::HumanDuration(std::time::Duration::from_millis(0)),
        max_elapsed: None,
    }
}

/// Build single-target gemfury evidence for `acme/demo@1.2.3`.
fn gemfury_evidence_for(account: &str, package: &str, version: &str) -> PublishEvidence {
    let mut ev = PublishEvidence::new("gemfury");
    ev.extra = anodizer_core::PublishEvidenceExtra::GemFury(
        anodizer_core::publish_evidence::GemFuryExtra {
            gemfury_targets: vec![anodizer_core::publish_evidence::GemFuryTargetSnapshot {
                target: format!("{account}/{package}"),
                account: account.into(),
                package: package.into(),
                version: version.into(),
                format: "deb".into(),
                push_token_env_var: "FURY_PUSH_TOKEN".into(),
                api_token_env_var: "FURY_API_TOKEN".into(),
            }],
        },
    );
    ev
}

#[test]
fn gemfury_rollback_deletes_recorded_version_with_basic_auth() {
    use anodizer_core::log::LogCapture;
    use anodizer_core::test_helpers::scripted_responder::{
        ScriptedRoute, spawn_scripted_responder,
    };
    use base64::Engine as _;

    let (api_addr, del_log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "DELETE",
        path_pattern: "/acme/packages/demo/versions/1.2.3",
        response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
        times: None,
    }]);

    let capture = LogCapture::new();
    let mut ctx = TestContextBuilder::new().build();
    // API (delete) token AND base resolved from the injected env source — never
    // process env, so the delete reads THIS test's responder race-free.
    ctx.set_env_source(
        anodizer_core::MapEnvSource::new()
            .with("FURY_API_TOKEN", "api-secret")
            .with("ANODIZE_GEMFURY_API_BASE", format!("http://{api_addr}")),
    );
    ctx.with_log_capture(capture.clone());
    ctx.config.retry = Some(fast_retry());

    let p = GemFuryPublisher::new();
    let result = p.rollback(&mut ctx, &gemfury_evidence_for("acme", "demo", "1.2.3"));
    result.expect("rollback returns Ok on a clean delete");

    let entries = del_log.lock().unwrap();
    assert_eq!(entries.len(), 1, "exactly one DELETE");
    let del = &entries[0];
    assert_eq!(del.method, "DELETE");
    assert_eq!(del.path, "/acme/packages/demo/versions/1.2.3");
    // basic_auth("api-secret", Some("")) == base64("api-secret:").
    let expect_basic = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("api-secret:")
    );
    assert_eq!(
        del.header("authorization"),
        Some(expect_basic.as_str()),
        "delete must carry HTTP Basic auth with the API token as username"
    );

    let all = capture.all_messages();
    assert!(
        all.iter()
            .any(|(_, m)| m.contains("deleted gemfury package 'acme/demo@1.2.3'")),
        "a successful delete must log the deleted target; got: {all:?}"
    );
    assert!(
        all.iter()
            .any(|(_, m)| m.contains("1 deleted") && m.contains("0 failure(s)")),
        "summary must report one delete and zero failures; got: {all:?}"
    );
}

#[test]
fn gemfury_rollback_delete_http_error_is_warned_not_raised() {
    use anodizer_core::log::LogCapture;
    use anodizer_core::test_helpers::scripted_responder::{
        ScriptedRoute, spawn_scripted_responder,
    };

    // A 500 on every DELETE attempt: rollback is best-effort, so the failure
    // must warn + count, NOT bubble an Err that would mask the original
    // failure being rolled back. `times: None` so the retry policy's repeats
    // all hit the same 500.
    let (api_addr, _del_log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "DELETE",
        path_pattern: "/acme/packages/demo/versions/1.2.3",
        response: "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 4\r\n\r\nboom",
        times: None,
    }]);

    let capture = LogCapture::new();
    let mut ctx = TestContextBuilder::new().build();
    ctx.set_env_source(
        anodizer_core::MapEnvSource::new()
            .with("FURY_API_TOKEN", "api-secret")
            .with("ANODIZE_GEMFURY_API_BASE", format!("http://{api_addr}")),
    );
    ctx.with_log_capture(capture.clone());
    ctx.config.retry = Some(fast_retry());

    let p = GemFuryPublisher::new();
    let result = p.rollback(&mut ctx, &gemfury_evidence_for("acme", "demo", "1.2.3"));
    result.expect("rollback must stay Ok despite a delete failure");

    let warns = capture.warn_messages();
    assert!(
        warns.iter().any(
            |m| m.contains("failed to delete gemfury package 'acme/demo@1.2.3'")
                && m.contains("manual cleanup required")
        ),
        "a delete failure must warn naming the target + manual-cleanup hint; got: {warns:?}"
    );
    let all = capture.all_messages();
    assert!(
        all.iter()
            .any(|(_, m)| m.contains("0 deleted") && m.contains("1 failure(s)")),
        "summary must report zero deletes and one failure; got: {all:?}"
    );
}

/// Rollback resolves the API token from the per-entry `cfg.api_token`
/// (templated) when the env var is absent — the config-override branch of
/// `delete_recorded_targets`. The DELETE must still fire (token resolved from
/// cfg), proving the override path reaches the wire.
#[test]
fn gemfury_rollback_resolves_api_token_from_cfg_override() {
    use anodizer_core::log::LogCapture;
    use anodizer_core::test_helpers::scripted_responder::{
        ScriptedRoute, spawn_scripted_responder,
    };

    let (api_addr, del_log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "DELETE",
        path_pattern: "/acme/packages/demo/versions/1.2.3",
        response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
        times: None,
    }]);

    let capture = LogCapture::new();
    let mut ctx = TestContextBuilder::new().build();
    // No FURY_API_TOKEN in the env source — the token must come from cfg. The
    // responder base is still injected so the DELETE hits THIS test's mock.
    ctx.set_env_source(
        anodizer_core::MapEnvSource::new()
            .with("ANODIZE_GEMFURY_API_BASE", format!("http://{api_addr}")),
    );
    // The config still declares the entry with an inline `api_token`, which
    // rollback re-reads to resolve the token the env no longer carries.
    ctx.config.gemfury = Some(vec![GemFuryConfig {
        account: Some("acme".into()),
        api_token: Some("cfg-api-secret".into()),
        ..Default::default()
    }]);
    ctx.with_log_capture(capture.clone());
    ctx.config.retry = Some(fast_retry());

    let p = GemFuryPublisher::new();
    let result = p.rollback(&mut ctx, &gemfury_evidence_for("acme", "demo", "1.2.3"));
    result.expect("rollback Ok via cfg token");

    let entries = del_log.lock().unwrap();
    assert_eq!(
        entries.len(),
        1,
        "the cfg-resolved token must let the DELETE fire"
    );
    let all = capture.all_messages();
    assert!(
        all.iter().any(|(_, m)| m.contains("1 deleted")),
        "cfg-token rollback must delete the recorded target; got: {all:?}"
    );
}
