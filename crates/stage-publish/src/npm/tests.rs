//! Tests for the NPM publisher.

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::{Config, CrateConfig, MetadataConfig, NpmConfig, StringOrBool};
use anodizer_core::test_helpers::TestContextBuilder;
use anodizer_core::{PreflightCheck, PublishEvidence, Publisher, PublisherGroup};

use super::manifest::{
    PlatformBinary, collect_platform_binaries, map_to_node, render_package_json,
    render_postinstall_js, resolve_access, resolve_extra_files, resolve_format, resolve_name,
    resolve_registry, resolve_tag,
};
use super::publish::{NpmTarget, assemble_tarball, publish_to_npm, resolve_token};
use super::publisher::NpmPublisher;

fn npm_cfg() -> NpmConfig {
    NpmConfig {
        name: Some("anodize-demo".into()),
        ..Default::default()
    }
}

fn scoped_cfg() -> NpmConfig {
    NpmConfig {
        name: Some("@anodize/demo".into()),
        access: Some("public".into()),
        ..Default::default()
    }
}

fn add_archive(
    ctx: &mut anodizer_core::context::Context,
    target: &str,
    sha: impl Into<String>,
    url: &str,
) {
    let sha = sha.into();
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        path: std::path::PathBuf::from(format!("/tmp/demo-{target}.tgz")),
        name: format!("demo-{target}.tgz"),
        target: Some(target.to_string()),
        crate_name: "demo".to_string(),
        metadata: {
            let mut m = std::collections::HashMap::new();
            m.insert("sha256".to_string(), sha.to_string());
            m.insert("url".to_string(), url.to_string());
            m
        },
        size: None,
    });
}

// -----------------------------------------------------------------------------
// Config parsing
// -----------------------------------------------------------------------------

#[test]
fn parse_minimal_npms_block() {
    let yaml = r#"
project_name: demo
crates:
  - name: demo
    path: .
    tag_template: "v{{ .Version }}"
npms:
  - name: "@anodize/demo"
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).expect("parse minimal npms");
    let entries = cfg.npms.expect("npms set");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name.as_deref(), Some("@anodize/demo"));
}

#[test]
fn parse_full_npms_block() {
    let yaml = r#"
project_name: demo
crates:
  - name: demo
    path: .
    tag_template: "v{{ .Version }}"
npms:
  - id: primary
    name: "@anodize/demo"
    description: "A demo"
    homepage: "https://example.com"
    license: MIT
    author: "Anodize"
    repository: "https://github.com/anodize/demo"
    bugs: "https://github.com/anodize/demo/issues"
    keywords: [cli, demo]
    access: public
    tag: next
    format: tgz
    registry: "https://npm.pkg.github.com"
    ids: [demo]
    skip: false
    disable: false
    required: true
    if: "{{ ne .Prerelease \"\" }}"
    extra:
      engines:
        node: ">=14"
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).expect("parse full npms");
    let entry = &cfg.npms.as_ref().unwrap()[0];
    assert_eq!(entry.id.as_deref(), Some("primary"));
    assert_eq!(entry.access.as_deref(), Some("public"));
    assert_eq!(entry.tag.as_deref(), Some("next"));
    assert_eq!(entry.format.as_deref(), Some("tgz"));
    assert_eq!(
        entry.registry.as_deref(),
        Some("https://npm.pkg.github.com")
    );
    assert!(matches!(entry.required, Some(true)));
    assert!(entry.if_condition.is_some());
    assert!(entry.extra.is_some());
}

#[test]
fn npm_config_defaults_resolve_correctly() {
    let cfg = NpmConfig::default();
    assert_eq!(resolve_tag(&cfg), "latest");
    assert_eq!(resolve_format(&cfg), "tgz");
    assert_eq!(resolve_registry(&cfg), "https://registry.npmjs.org");
    assert!(resolve_access(&cfg).is_none());
    assert_eq!(
        resolve_extra_files(&cfg),
        vec!["README*".to_string(), "LICENSE*".to_string()]
    );
}

#[test]
fn resolve_name_falls_back_to_crate_name() {
    let cfg = NpmConfig::default();
    assert_eq!(resolve_name(&cfg, "demo"), "demo");
}

#[test]
fn resolve_name_uses_configured_name_when_set() {
    let cfg = NpmConfig {
        name: Some("@scope/foo".into()),
        ..Default::default()
    };
    assert_eq!(resolve_name(&cfg, "demo"), "@scope/foo");
}

// -----------------------------------------------------------------------------
// OS / arch mapping
// -----------------------------------------------------------------------------

#[test]
fn map_to_node_maps_common_triples() {
    assert_eq!(map_to_node("linux", "amd64"), Some(("linux", "x64")));
    assert_eq!(map_to_node("linux", "arm64"), Some(("linux", "arm64")));
    assert_eq!(map_to_node("darwin", "amd64"), Some(("darwin", "x64")));
    assert_eq!(map_to_node("darwin", "arm64"), Some(("darwin", "arm64")));
    assert_eq!(map_to_node("windows", "amd64"), Some(("win32", "x64")));
    assert_eq!(map_to_node("windows", "386"), Some(("win32", "ia32")));
    assert_eq!(map_to_node("linux", "armv7"), Some(("linux", "arm")));
}

#[test]
fn map_to_node_rejects_unsupported() {
    assert_eq!(map_to_node("solaris", "amd64"), None);
    assert_eq!(map_to_node("linux", "mips"), None);
}

// -----------------------------------------------------------------------------
// Platform-binary collection
// -----------------------------------------------------------------------------

#[test]
fn collect_platform_binaries_maps_archive_artifacts() {
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .crates(vec![CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }])
        .build();
    add_archive(
        &mut ctx,
        "x86_64-unknown-linux-gnu",
        "11".repeat(32),
        "https://example.com/demo-linux-x64.tgz",
    );
    add_archive(
        &mut ctx,
        "aarch64-apple-darwin",
        "22".repeat(32),
        "https://example.com/demo-darwin-arm64.tgz",
    );

    let cfg = npm_cfg();
    let bins = collect_platform_binaries(&ctx, &cfg, "demo", "1.2.3").expect("collect");
    assert_eq!(bins.len(), 2);
    // Sorted alphabetically by os then cpu.
    assert_eq!(bins[0].os, "darwin");
    assert_eq!(bins[0].cpu, "arm64");
    assert_eq!(bins[1].os, "linux");
    assert_eq!(bins[1].cpu, "x64");
    assert_eq!(bins[1].url, "https://example.com/demo-linux-x64.tgz");
}

#[test]
fn collect_platform_binaries_skips_non_archive_artifacts() {
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .crates(vec![CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }])
        .build();
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        path: std::path::PathBuf::from("/tmp/demo"),
        name: "demo".to_string(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "demo".to_string(),
        metadata: std::collections::HashMap::new(),
        size: None,
    });
    let cfg = npm_cfg();
    let bins = collect_platform_binaries(&ctx, &cfg, "demo", "1.0.0").expect("collect");
    assert!(bins.is_empty());
}

// -----------------------------------------------------------------------------
// package.json generation
// -----------------------------------------------------------------------------

fn one_binary(os: &str, cpu: &str) -> PlatformBinary {
    PlatformBinary {
        os: os.to_string(),
        cpu: cpu.to_string(),
        url: format!("https://example.com/demo-{os}-{cpu}.tgz"),
        sha256: "a".repeat(64),
        format: "tgz".to_string(),
    }
}

#[test]
fn render_package_json_emits_canonical_fields() {
    let ctx = TestContextBuilder::new().project_name("demo").build();
    let cfg = npm_cfg();
    let bins = vec![one_binary("linux", "x64")];
    let body = render_package_json(&ctx, &cfg, "anodize-demo", "1.2.3", &bins).expect("render");
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid json");
    assert_eq!(parsed["name"], "anodize-demo");
    assert_eq!(parsed["version"], "1.2.3");
    assert_eq!(parsed["scripts"]["postinstall"], "node ./postinstall.js");
    assert_eq!(parsed["anodize"]["binaries"][0]["os"], "linux");
    assert_eq!(parsed["anodize"]["binaries"][0]["cpu"], "x64");
    assert_eq!(parsed["bin"]["anodize-demo"], "bin/anodize-demo.js");
}

#[test]
fn render_package_json_scoped_package_uses_basename_for_bin() {
    let ctx = TestContextBuilder::new().project_name("demo").build();
    let cfg = scoped_cfg();
    let bins = vec![one_binary("linux", "x64")];
    let body = render_package_json(&ctx, &cfg, "@anodize/demo", "1.0.0", &bins).expect("render");
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid json");
    assert_eq!(parsed["name"], "@anodize/demo");
    assert_eq!(parsed["bin"]["demo"], "bin/demo.js");
}

#[test]
fn render_package_json_metadata_fallback() {
    // No description/homepage/license set on the npm cfg, but the
    // project has metadata.
    let cfg_top = Config {
        project_name: "demo".to_string(),
        metadata: Some(MetadataConfig {
            description: Some("From metadata".to_string()),
            homepage: Some("https://meta.example.com".to_string()),
            license: Some("Apache-2.0".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let ctx = anodizer_core::context::Context::new(
        cfg_top,
        anodizer_core::context::ContextOptions::default(),
    );
    let cfg = NpmConfig {
        name: Some("demo".into()),
        ..Default::default()
    };
    let body = render_package_json(&ctx, &cfg, "demo", "1.0.0", &[]).expect("render");
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid json");
    assert_eq!(parsed["description"], "From metadata");
    assert_eq!(parsed["homepage"], "https://meta.example.com");
    assert_eq!(parsed["license"], "Apache-2.0");
}

#[test]
fn render_package_json_extra_shallow_merges() {
    let ctx = TestContextBuilder::new().project_name("demo").build();
    let mut extra = std::collections::HashMap::new();
    extra.insert("engines".to_string(), serde_json::json!({ "node": ">=18" }));
    let cfg = NpmConfig {
        name: Some("demo".into()),
        extra: Some(extra),
        ..Default::default()
    };
    let body = render_package_json(&ctx, &cfg, "demo", "1.0.0", &[]).expect("render");
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid json");
    assert_eq!(parsed["engines"]["node"], ">=18");
}

#[test]
fn render_package_json_extra_can_override_root_keys() {
    let ctx = TestContextBuilder::new().project_name("demo").build();
    let mut extra = std::collections::HashMap::new();
    // Operator overrides the generated `description`.
    extra.insert(
        "description".to_string(),
        serde_json::Value::String("override".to_string()),
    );
    let cfg = NpmConfig {
        name: Some("demo".into()),
        description: Some("original".into()),
        extra: Some(extra),
        ..Default::default()
    };
    let body = render_package_json(&ctx, &cfg, "demo", "1.0.0", &[]).expect("render");
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid json");
    assert_eq!(parsed["description"], "override");
}

// -----------------------------------------------------------------------------
// postinstall.js
// -----------------------------------------------------------------------------

#[test]
fn render_postinstall_js_includes_platform_check_and_sha256() {
    let body = render_postinstall_js("@anodize/demo");
    assert!(body.contains("process.platform"));
    assert!(body.contains("process.arch"));
    assert!(body.contains("sha256"));
    // unscoped basename used for the bin name
    assert!(body.contains("demo.exe") || body.contains("'demo'"));
}

// -----------------------------------------------------------------------------
// Tarball assembly
// -----------------------------------------------------------------------------

fn ctx_with_archives() -> anodizer_core::context::Context {
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
    add_archive(
        &mut ctx,
        "x86_64-unknown-linux-gnu",
        "11".repeat(32),
        "https://example.com/demo-linux-x64.tgz",
    );
    add_archive(
        &mut ctx,
        "aarch64-apple-darwin",
        "22".repeat(32),
        "https://example.com/demo-darwin-arm64.tgz",
    );
    ctx
}

#[test]
fn assemble_tarball_is_reproducible() {
    let ctx = ctx_with_archives();
    let cfg = npm_cfg();
    let bins = collect_platform_binaries(&ctx, &cfg, "anodize-demo", "1.2.3").expect("collect");

    let t1 = assemble_tarball(&ctx, &cfg, "demo", "1.2.3", &bins).expect("assemble 1");
    let t2 = assemble_tarball(&ctx, &cfg, "demo", "1.2.3", &bins).expect("assemble 2");

    let b1 = std::fs::read(&t1.tarball_path).expect("read 1");
    let b2 = std::fs::read(&t2.tarball_path).expect("read 2");
    assert_eq!(
        b1, b2,
        "two consecutive assemblies produced byte-different tarballs"
    );
}

#[test]
fn assemble_tarball_scoped_package_basename() {
    let ctx = ctx_with_archives();
    let cfg = scoped_cfg();
    let bins = vec![one_binary("linux", "x64")];
    let staged = assemble_tarball(&ctx, &cfg, "demo", "1.2.3", &bins).expect("assemble");
    let fname = staged
        .tarball_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    // scoped @anodize/demo -> anodize-demo-1.2.3.tgz
    assert_eq!(fname, "anodize-demo-1.2.3.tgz");
}

// -----------------------------------------------------------------------------
// Publisher contract
// -----------------------------------------------------------------------------

#[test]
fn npm_publisher_classification() {
    let p = NpmPublisher::new();
    assert_eq!(p.name(), "npm");
    assert_eq!(p.group(), PublisherGroup::Manager);
    assert!(p.required(), "npm publisher defaults to required=true");
    assert_eq!(p.rollback_scope_needed(), Some("NPM_TOKEN unpublish"));
}

#[test]
fn npm_publisher_preflight_passes() {
    let ctx = TestContextBuilder::new().build();
    let p = NpmPublisher::new();
    assert!(matches!(
        p.preflight(&ctx).expect("preflight ok"),
        PreflightCheck::Pass
    ));
}

#[test]
fn npm_publisher_run_with_no_npms_configured_is_noop() {
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .crates(vec![CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }])
        .build();
    let p = NpmPublisher::new();
    let evidence = p.run(&mut ctx).expect("run ok");
    assert_eq!(evidence.publisher, "npm");
    assert!(evidence.primary_ref.is_none());
}

// -----------------------------------------------------------------------------
// Dry-run + skip-paths
// -----------------------------------------------------------------------------

#[test]
fn publish_dry_run_returns_none_without_invoking_npm() {
    let mut ctx = ctx_with_archives();
    ctx.options.dry_run = true;
    let cfg = npm_cfg();
    let log = ctx.logger("publish");
    let outcome = publish_to_npm(&ctx, &cfg, "demo", &log).expect("publish");
    assert!(outcome.is_none(), "dry-run must return None");
}

#[test]
fn publish_skip_true_returns_none() {
    let ctx = ctx_with_archives();
    let cfg = NpmConfig {
        name: Some("demo".into()),
        skip: Some(StringOrBool::Bool(true)),
        ..Default::default()
    };
    let log = ctx.logger("publish");
    let outcome = publish_to_npm(&ctx, &cfg, "demo", &log).expect("publish");
    assert!(outcome.is_none());
}

#[test]
fn publish_disable_true_returns_none() {
    let ctx = ctx_with_archives();
    let cfg = NpmConfig {
        name: Some("demo".into()),
        disable: Some(StringOrBool::Bool(true)),
        ..Default::default()
    };
    let log = ctx.logger("publish");
    let outcome = publish_to_npm(&ctx, &cfg, "demo", &log).expect("publish");
    assert!(outcome.is_none());
}

#[test]
fn publish_if_condition_falsy_returns_none() {
    let ctx = ctx_with_archives();
    let cfg = NpmConfig {
        name: Some("demo".into()),
        if_condition: Some("false".into()),
        ..Default::default()
    };
    let log = ctx.logger("publish");
    let outcome = publish_to_npm(&ctx, &cfg, "demo", &log).expect("publish");
    assert!(outcome.is_none());
}

#[test]
fn publish_no_matching_binaries_warns_and_returns_none() {
    // No archive artifacts in the context — npm should warn + return
    // None (no panic, no Err).
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .crates(vec![CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }])
        .build();
    ctx.options.dry_run = true;
    let cfg = npm_cfg();
    let log = ctx.logger("publish");
    let outcome = publish_to_npm(&ctx, &cfg, "demo", &log).expect("publish");
    assert!(outcome.is_none());
}

// -----------------------------------------------------------------------------
// Auth
// -----------------------------------------------------------------------------

#[test]
fn resolve_token_falls_back_to_env_var() {
    let mut ctx = ctx_with_archives();
    let env = anodizer_core::MapEnvSource::new().with("NPM_TOKEN", "fake-token-value");
    ctx.set_env_source(env);
    let cfg = npm_cfg();
    let token = resolve_token(&ctx, &cfg).expect("token");
    assert_eq!(token, "fake-token-value");
}

#[test]
fn resolve_token_prefers_cfg_token() {
    let mut ctx = ctx_with_archives();
    let env = anodizer_core::MapEnvSource::new().with("NPM_TOKEN", "from-env");
    ctx.set_env_source(env);
    let cfg = NpmConfig {
        name: Some("demo".into()),
        token: Some("from-cfg".into()),
        ..Default::default()
    };
    let token = resolve_token(&ctx, &cfg).expect("token");
    assert_eq!(token, "from-cfg");
}

#[test]
fn publish_errors_when_token_missing_and_not_dry_run() {
    // Empty env, no cfg.token — publishing should bail with a clear
    // "NPM_TOKEN required" message.
    let ctx = ctx_with_archives();
    let cfg = npm_cfg();
    let log = ctx.logger("publish");
    let err = publish_to_npm(&ctx, &cfg, "demo", &log).expect_err("must err");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("NPM_TOKEN"),
        "missing NPM_TOKEN message: {msg}"
    );
}

// -----------------------------------------------------------------------------
// Rollback evidence shape
// -----------------------------------------------------------------------------

#[test]
fn npm_evidence_extra_carries_no_secret_material() {
    let mut e = PublishEvidence::new("npm");
    e.extra = anodizer_core::PublishEvidenceExtra::Npm(anodizer_core::publish_evidence::NpmExtra {
        npm_targets: vec![
            NpmTarget {
                package: "@anodize/demo".into(),
                version: "1.2.3".into(),
                registry: "https://registry.npmjs.org".into(),
                dist_tag: "latest".into(),
                token_env_var: "NPM_TOKEN".into(),
            }
            .into(),
        ],
    });
    let s = serde_json::to_string(&e).expect("serialize");
    assert!(!s.contains("\"token\":"), "{s}");
    assert!(!s.contains("\"_authToken\":"), "{s}");
    assert!(!s.contains("\"password\":"), "{s}");
    assert!(!s.contains("\"secret\":"), "{s}");
    // Positive shape: operator coordinates present.
    assert!(s.contains("\"package\":\"@anodize/demo\""), "{s}");
    assert!(s.contains("\"version\":\"1.2.3\""), "{s}");
    assert!(s.contains("\"token_env_var\":\"NPM_TOKEN\""), "{s}");
}

#[test]
fn npm_rollback_with_no_targets_emits_warn_not_err() {
    let mut ctx = TestContextBuilder::new().build();
    let evidence = PublishEvidence::new("npm");
    let p = NpmPublisher::new();
    assert!(p.rollback(&mut ctx, &evidence).is_ok());
}

// Helper to construct an NpmTarget for evidence tests.
impl From<NpmTarget> for anodizer_core::publish_evidence::NpmTargetSnapshot {
    fn from(t: NpmTarget) -> Self {
        Self {
            target: t.package.clone(),
            package: t.package,
            version: t.version,
            registry: t.registry,
            dist_tag: t.dist_tag,
            token_env_var: t.token_env_var,
        }
    }
}
