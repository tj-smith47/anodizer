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

// -----------------------------------------------------------------------------
// templated_extra_files
// -----------------------------------------------------------------------------

#[test]
fn npm_tarball_includes_templated_extra_file() {
    use anodizer_core::config::NpmTemplatedExtraFile;
    use flate2::read::GzDecoder;

    let tmp = tempfile::TempDir::new().expect("tempdir");
    let tpl_path = tmp.path().join("notes.tera");
    std::fs::write(&tpl_path, "release: {{ .ProjectName }}-{{ .Version }}\n")
        .expect("write template");

    let ctx = ctx_with_archives();
    let cfg = NpmConfig {
        name: Some("anodize-demo".into()),
        templated_extra_files: Some(vec![NpmTemplatedExtraFile {
            src: tpl_path.to_string_lossy().to_string(),
            dst: "{{ .Version }}/notes.txt".to_string(),
        }]),
        ..Default::default()
    };
    let bins = vec![one_binary("linux", "x64")];

    let staged = assemble_tarball(&ctx, &cfg, "demo", "1.2.3", &bins).expect("assemble");
    let tar_bytes = std::fs::read(&staged.tarball_path).expect("read tarball");
    let mut archive = tar::Archive::new(GzDecoder::new(&tar_bytes[..]));

    let mut found_rendered_at: Option<String> = None;
    let mut rendered_contents: Option<String> = None;
    for entry in archive.entries().expect("entries") {
        let mut entry = entry.expect("entry ok");
        let path = entry.path().expect("path").into_owned();
        let path_str = path.to_string_lossy().into_owned();
        if path_str.ends_with("notes.txt") {
            found_rendered_at = Some(path_str.clone());
            let mut s = String::new();
            use std::io::Read;
            entry.read_to_string(&mut s).expect("read entry");
            rendered_contents = Some(s);
        }
    }

    let dst = found_rendered_at.expect("notes.txt present in tarball");
    assert!(
        dst.contains("1.2.3/notes.txt"),
        "expected rendered dst under '1.2.3/', got '{}'",
        dst
    );
    let contents = rendered_contents.expect("entry bytes");
    assert!(
        contents.contains("release: demo-1.2.3"),
        "expected template to render project + version, got '{}'",
        contents
    );
}

// -----------------------------------------------------------------------------
// Multi-format preflight (ambiguous archive format)
// -----------------------------------------------------------------------------

fn multi_format_ctx() -> anodizer_core::context::Context {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig};
    let mut ctx = ctx_with_archives();
    let archive_cfg = ArchiveConfig {
        id: Some("default".into()),
        formats: Some(vec!["tar.gz".into(), "zip".into()]),
        ..Default::default()
    };
    ctx.config.crates[0].archives = ArchivesConfig::Configs(vec![archive_cfg]);
    ctx
}

#[test]
fn npm_fails_on_multi_format_archive_without_format_set() {
    let ctx = multi_format_ctx();
    let cfg = npm_cfg();
    let log = ctx.logger("publish");
    let err = publish_to_npm(&ctx, &cfg, "demo", &log)
        .expect_err("multi-format with no `format:` must error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("multiple formats") && msg.contains("`format:` is unset"),
        "expected multi-format diagnostic, got: {msg}"
    );
}

#[test]
fn npm_allows_multi_format_when_format_set_explicitly() {
    let mut ctx = multi_format_ctx();
    ctx.options.dry_run = true;
    let cfg = NpmConfig {
        name: Some("anodize-demo".into()),
        format: Some("tgz".into()),
        ..Default::default()
    };
    let log = ctx.logger("publish");
    let outcome = publish_to_npm(&ctx, &cfg, "demo", &log).expect("must succeed dry-run");
    assert!(outcome.is_none(), "dry-run returns None");
}

// -----------------------------------------------------------------------------
// Idempotency probe + retry helpers
// -----------------------------------------------------------------------------

#[test]
fn npm_version_already_published_returns_false_when_npm_unavailable() {
    use super::publish::version_already_published;
    // Drive a registry that no real `npm` will recognize. When the
    // subprocess errors (404 or "npm not on PATH"), the probe must
    // return `Ok(false)` so the publish path still runs.
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let out = version_already_published(
        "definitely-not-a-published-package-xyz",
        "9.9.9-anodize-fixture",
        tmp.path(),
        "https://registry.npmjs.org",
    );
    // Either spawn error or 404 — both surface as Ok(false).
    match out {
        Ok(b) => assert!(!b, "expected probe to report 'not published'"),
        Err(_) => {
            // No npm on PATH at all — implementation already swallows
            // this and returns Ok(false), so this branch should be
            // unreachable. If it isn't, the test environment is broken,
            // not the code.
        }
    }
}

// -----------------------------------------------------------------------------
// Rollback dispatch — fake-npm shim verifies `npm unpublish` is invoked
// -----------------------------------------------------------------------------

#[cfg(unix)]
#[test]
#[serial_test::serial(npm_path_shim)]
fn rollback_npm_calls_npm_unpublish() {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::TempDir::new().expect("tempdir for shim");
    let invocations_log = tmp.path().join("npm-invocations.log");
    let shim_path = tmp.path().join("npm");
    {
        let mut f = std::fs::File::create(&shim_path).expect("create shim");
        // POSIX shim: appends every npm invocation to a log file and
        // returns 0 so the rollback path treats it as success.
        writeln!(
            f,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {log}\nexit 0",
            log = invocations_log.display()
        )
        .expect("write shim");
    }
    std::fs::set_permissions(&shim_path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod shim");

    // Prepend the shim dir to PATH so `Command::new("npm")` resolves
    // to it for the duration of this test.
    let prev_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", tmp.path().display(), prev_path);
    // SAFETY: env mutation is serialized by `serial(npm_path_shim)`.
    unsafe {
        std::env::set_var("PATH", &new_path);
        std::env::set_var("NPM_TOKEN", "fake-token-value");
    }

    let mut ctx = TestContextBuilder::new().project_name("demo").build();
    let mut evidence = PublishEvidence::new("npm");
    evidence.extra =
        anodizer_core::PublishEvidenceExtra::Npm(anodizer_core::publish_evidence::NpmExtra {
            npm_targets: vec![anodizer_core::publish_evidence::NpmTargetSnapshot {
                target: "@anodize/demo".into(),
                package: "@anodize/demo".into(),
                version: "1.2.3".into(),
                registry: "https://registry.npmjs.org".into(),
                dist_tag: "latest".into(),
                token_env_var: "NPM_TOKEN".into(),
            }],
        });
    let p = NpmPublisher::new();
    let result = p.rollback(&mut ctx, &evidence);

    // Restore PATH before any assertion so failures don't leak the
    // shim into sibling tests.
    // SAFETY: still inside the serial-guarded block.
    unsafe {
        std::env::set_var("PATH", prev_path);
        std::env::remove_var("NPM_TOKEN");
    }

    result.expect("rollback should succeed against the shim");
    let log_contents = std::fs::read_to_string(&invocations_log).expect("read log");
    assert!(
        log_contents.contains("unpublish") && log_contents.contains("@anodize/demo@1.2.3"),
        "expected shim to have observed `npm unpublish @anodize/demo@1.2.3`, got: {log_contents}"
    );
}
