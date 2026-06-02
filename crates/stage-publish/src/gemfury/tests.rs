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
            tag_template: "v{{ .Version }}".to_string(),
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
    // Legacy spelling pre-GR-v2.14 collapses to the same struct via
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

#[test]
fn push_and_api_token_env_var_defaults() {
    let cfg = GemFuryConfig::default();
    assert_eq!(push_token_env_var(&cfg), "FURY_TOKEN");
    assert_eq!(api_token_env_var(&cfg), "FURY_API_TOKEN");
}

#[test]
fn push_and_api_token_env_var_overrides() {
    let cfg = GemFuryConfig {
        secret_name: Some("MY_PUSH".into()),
        api_secret_name: Some("MY_API".into()),
        ..Default::default()
    };
    assert_eq!(push_token_env_var(&cfg), "MY_PUSH");
    assert_eq!(api_token_env_var(&cfg), "MY_API");
}

// -----------------------------------------------------------------------------
// Auth resolution
// -----------------------------------------------------------------------------

#[test]
fn resolve_push_token_falls_back_to_env_var() {
    let mut ctx = ctx_with_packages();
    let env = anodizer_core::MapEnvSource::new().with("FURY_TOKEN", "from-env");
    ctx.set_env_source(env);
    let cfg = basic_cfg();
    assert_eq!(resolve_push_token(&ctx, &cfg).expect("token"), "from-env");
}

#[test]
fn resolve_push_token_prefers_cfg_token() {
    let mut ctx = ctx_with_packages();
    let env = anodizer_core::MapEnvSource::new().with("FURY_TOKEN", "from-env");
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
        .with("FURY_TOKEN", "push-only")
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
    // Isolates from process FURY_TOKEN in case a future sibling sets it globally.
    ctx.set_env_source(anodizer_core::MapEnvSource::new());
    ctx.config.gemfury = Some(vec![basic_cfg()]);
    let log = ctx.logger("publish");
    let err = publish_to_gemfury(&ctx, &log).expect_err("missing token must err");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("push token is required") && msg.contains("FURY_TOKEN"),
        "expected token diagnostic, got: {msg}"
    );
}

#[test]
fn publish_errors_when_account_missing() {
    let mut ctx = ctx_with_packages();
    ctx.config.gemfury = Some(vec![GemFuryConfig::default()]);
    let log = ctx.logger("publish");
    let err = publish_to_gemfury(&ctx, &log).expect_err("missing account must err");
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
    let out = publish_to_gemfury(&ctx, &log).expect("dry-run");
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
    let out = publish_to_gemfury(&ctx, &log).expect("skip");
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
    let out = publish_to_gemfury(&ctx, &log).expect("disable alias");
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
    let out = publish_to_gemfury(&ctx, &log).expect("if falsy");
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
    let err = publish_to_gemfury(&ctx, &log).expect_err("multi-format overlap must err");
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
    publish_to_gemfury(&ctx, &log).expect("single-overlap dry-run ok");
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
            tag_template: "v{{ .Version }}".to_string(),
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
                push_token_env_var: "FURY_TOKEN".into(),
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
    assert!(s.contains("\"push_token_env_var\":\"FURY_TOKEN\""), "{s}");
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
                push_token_env_var: "FURY_TOKEN".into(),
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
#[serial_test::serial]
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

    unsafe {
        std::env::set_var("ANODIZE_GEMFURY_API_BASE", format!("http://{api_addr}"));
    }
    let result = version_already_published(
        &client,
        "acme",
        "demo",
        "1.2.3",
        "fake-push-token",
        &policy,
        &log,
    );
    unsafe {
        std::env::remove_var("ANODIZE_GEMFURY_API_BASE");
    }

    // A 404 is the documented "not present" response: the probe must
    // coerce it to Ok(false) so the publish path runs.
    assert!(
        matches!(result, Ok(false)),
        "404 probe must surface Ok(false), got {:?}",
        result
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

/// A re-run against an already-published version must succeed (idempotent):
/// the probe 404s (Fury's probe surface), then the push returns 409 Conflict
/// → treated as success with no rollback target recorded. A genuine failure
/// (400) still errors.
#[test]
#[serial_test::serial]
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
    ctx.set_env_source(anodizer_core::MapEnvSource::new().with("FURY_TOKEN", "fake-token"));
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::LinuxPackage,
        path: art_path.clone(),
        name: "demo_1.2.3_amd64.deb".to_string(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "demo".to_string(),
        metadata: std::collections::HashMap::new(),
        size: None,
    });

    unsafe {
        std::env::set_var("ANODIZE_GEMFURY_API_BASE", format!("http://{api_addr}"));
        std::env::set_var("ANODIZE_GEMFURY_PUSH_BASE", format!("http://{push_addr}"));
    }
    let log = StageLogger::new("gemfury", Verbosity::Quiet);
    let result = publish_to_gemfury(&ctx, &log);
    unsafe {
        std::env::remove_var("ANODIZE_GEMFURY_API_BASE");
        std::env::remove_var("ANODIZE_GEMFURY_PUSH_BASE");
    }

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
#[serial_test::serial]
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
    ctx.set_env_source(anodizer_core::MapEnvSource::new().with("FURY_TOKEN", "fake-token"));
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::LinuxPackage,
        path: art_path.clone(),
        name: "demo_1.2.3_amd64.deb".to_string(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "demo".to_string(),
        metadata: std::collections::HashMap::new(),
        size: None,
    });

    unsafe {
        std::env::set_var("ANODIZE_GEMFURY_API_BASE", format!("http://{api_addr}"));
        std::env::set_var("ANODIZE_GEMFURY_PUSH_BASE", format!("http://{push_addr}"));
    }
    let log = StageLogger::new("gemfury", Verbosity::Quiet);
    let result = publish_to_gemfury(&ctx, &log);
    unsafe {
        std::env::remove_var("ANODIZE_GEMFURY_API_BASE");
        std::env::remove_var("ANODIZE_GEMFURY_PUSH_BASE");
    }

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
#[serial_test::serial]
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
    ctx.set_env_source(anodizer_core::MapEnvSource::new().with("FURY_TOKEN", "fake-token"));
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::LinuxPackage,
        path: art_path.clone(),
        name: "demo_1.2.3_amd64.deb".to_string(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "demo".to_string(),
        metadata: std::collections::HashMap::new(),
        size: None,
    });

    unsafe {
        std::env::set_var("ANODIZE_GEMFURY_API_BASE", format!("http://{api_addr}"));
        std::env::set_var("ANODIZE_GEMFURY_PUSH_BASE", format!("http://{push_addr}"));
    }
    let log = StageLogger::new("gemfury", Verbosity::Quiet);
    let result = publish_to_gemfury(&ctx, &log);
    unsafe {
        std::env::remove_var("ANODIZE_GEMFURY_API_BASE");
        std::env::remove_var("ANODIZE_GEMFURY_PUSH_BASE");
    }

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
