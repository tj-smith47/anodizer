//! Tests for the NPM publisher (restored + realigned to optional-deps).

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::{
    Config, CrateConfig, MetadataConfig, NpmConfig, NpmMode, StringOrBool,
};
use anodizer_core::test_helpers::TestContextBuilder;
use anodizer_core::{PreflightCheck, Publisher, PublisherGroup};

use super::manifest::{
    PlatformBinary, collect_platform_binaries, npm_triple, render_package_json,
    render_postinstall_js, resolve_access, resolve_extra_files, resolve_format, resolve_name,
    resolve_registry, resolve_tag,
};
use super::optional_deps::generate_layout;
use super::publish::{assemble_postinstall_tarball, publish_to_npm};
use super::publisher::NpmPublisher;

fn demo_crate() -> CrateConfig {
    CrateConfig {
        name: "demo".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        ..Default::default()
    }
}

fn npm_cfg() -> NpmConfig {
    NpmConfig {
        mode: NpmMode::Postinstall,
        name: Some("anodize-demo".into()),
        ..Default::default()
    }
}

fn scoped_cfg() -> NpmConfig {
    NpmConfig {
        mode: NpmMode::Postinstall,
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

/// Add an `UploadableBinary` artifact backed by a real on-disk file (so the
/// publish path can read its bytes). Returns the binary basename.
fn add_binary(
    ctx: &mut anodizer_core::context::Context,
    dir: &std::path::Path,
    target: &str,
    basename: &str,
) {
    let path = dir.join(format!("{basename}-{target}"));
    std::fs::write(&path, format!("ELF-{target}").as_bytes()).expect("write fake binary");
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::UploadableBinary,
        path,
        name: basename.to_string(),
        target: Some(target.to_string()),
        crate_name: "demo".to_string(),
        metadata: std::collections::HashMap::new(),
        size: None,
    });
}

// -----------------------------------------------------------------------------
// Config parsing
// -----------------------------------------------------------------------------

#[test]
fn parse_minimal_npms_block_defaults_to_optional_deps() {
    let yaml = r#"
project_name: demo
crates:
  - name: demo
    path: .
    tag_template: "v{{ .Version }}"
npms:
  - scope: "@anodize"
    metapackage: demo
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).expect("parse minimal npms");
    let entries = cfg.npms.expect("npms set");
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].mode,
        NpmMode::OptionalDeps,
        "default is optional-deps"
    );
    assert!(entries[0].libc_aware, "libc_aware defaults true");
    assert_eq!(entries[0].scope.as_deref(), Some("@anodize"));
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
    mode: postinstall
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
    libc_aware: false
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
    assert_eq!(entry.mode, NpmMode::Postinstall);
    assert!(!entry.libc_aware);
    assert_eq!(entry.access.as_deref(), Some("public"));
    assert_eq!(entry.tag.as_deref(), Some("next"));
    assert!(matches!(entry.required, Some(true)));
    assert!(entry.if_condition.is_some());
    assert!(entry.extra.is_some());
}

#[test]
fn npm_config_defaults_resolve_correctly() {
    let cfg = NpmConfig::default();
    assert_eq!(cfg.mode, NpmMode::OptionalDeps);
    assert!(cfg.libc_aware);
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
// target -> npm triple derivation (rule #11 — derived from real targets)
// -----------------------------------------------------------------------------

#[test]
fn npm_triple_derives_os_cpu_libc_from_target() {
    let t = npm_triple("x86_64-unknown-linux-musl").expect("linux musl");
    assert_eq!(t.os, "linux");
    assert_eq!(t.cpu, "x64");
    assert_eq!(t.libc, "musl");

    let t = npm_triple("x86_64-unknown-linux-gnu").expect("linux gnu");
    assert_eq!(t.os, "linux");
    assert_eq!(t.cpu, "x64");
    assert_eq!(t.libc, "glibc", "gnu maps to npm's glibc");

    let t = npm_triple("aarch64-apple-darwin").expect("darwin arm64");
    assert_eq!(t.os, "darwin");
    assert_eq!(t.cpu, "arm64");
    assert_eq!(t.libc, "", "darwin has no libc selector");

    let t = npm_triple("x86_64-pc-windows-msvc").expect("win x64");
    assert_eq!(t.os, "win32");
    assert_eq!(t.cpu, "x64");
    assert_eq!(t.libc, "");

    let t = npm_triple("i686-pc-windows-msvc").expect("win ia32");
    assert_eq!(t.cpu, "ia32");
}

#[test]
fn npm_triple_rejects_unsupported() {
    assert!(npm_triple("sparc64-unknown-linux-gnu").is_none());
    assert!(npm_triple("x86_64-unknown-haiku").is_none());
}

// -----------------------------------------------------------------------------
// Platform-binary collection (postinstall mode)
// -----------------------------------------------------------------------------

#[test]
fn collect_platform_binaries_maps_archive_artifacts() {
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .crates(vec![demo_crate()])
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
    assert_eq!(bins[0].os, "darwin");
    assert_eq!(bins[0].cpu, "arm64");
    assert_eq!(bins[1].os, "linux");
    assert_eq!(bins[1].cpu, "x64");
    assert_eq!(bins[1].url, "https://example.com/demo-linux-x64.tgz");
}

// -----------------------------------------------------------------------------
// package.json generation (postinstall mode)
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
    assert_eq!(parsed["bin"]["anodize-demo"], "bin/anodize-demo.js");
}

#[test]
fn render_package_json_metadata_fallback() {
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
        mode: NpmMode::Postinstall,
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
fn render_package_json_extra_can_override_root_keys() {
    let ctx = TestContextBuilder::new().project_name("demo").build();
    let mut extra = std::collections::HashMap::new();
    extra.insert(
        "description".to_string(),
        serde_json::Value::String("override".to_string()),
    );
    let cfg = NpmConfig {
        mode: NpmMode::Postinstall,
        name: Some("demo".into()),
        description: Some("original".into()),
        extra: Some(extra),
        ..Default::default()
    };
    let body = render_package_json(&ctx, &cfg, "demo", "1.0.0", &[]).expect("render");
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid json");
    assert_eq!(parsed["description"], "override");
}

#[test]
fn render_postinstall_js_includes_platform_check_and_sha256() {
    let body = render_postinstall_js("@anodize/demo");
    assert!(body.contains("process.platform"));
    assert!(body.contains("process.arch"));
    assert!(body.contains("sha256"));
    assert!(body.contains("demo.exe") || body.contains("'demo'"));
}

// -----------------------------------------------------------------------------
// Tarball assembly (postinstall mode)
// -----------------------------------------------------------------------------

fn ctx_with_archives() -> anodizer_core::context::Context {
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .crates(vec![demo_crate()])
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
fn assemble_postinstall_tarball_is_reproducible() {
    let ctx = ctx_with_archives();
    let cfg = npm_cfg();
    let bins = collect_platform_binaries(&ctx, &cfg, "anodize-demo", "1.2.3").expect("collect");
    let t1 = assemble_postinstall_tarball(&ctx, &cfg, "demo", "1.2.3", &bins).expect("assemble 1");
    let t2 = assemble_postinstall_tarball(&ctx, &cfg, "demo", "1.2.3", &bins).expect("assemble 2");
    let b1 = std::fs::read(&t1.tarball_path).expect("read 1");
    let b2 = std::fs::read(&t2.tarball_path).expect("read 2");
    assert_eq!(
        b1, b2,
        "two consecutive assemblies produced byte-different tarballs"
    );
}

#[test]
fn assemble_postinstall_tarball_scoped_package_basename() {
    let ctx = ctx_with_archives();
    let cfg = scoped_cfg();
    let bins = vec![one_binary("linux", "x64")];
    let staged =
        assemble_postinstall_tarball(&ctx, &cfg, "demo", "1.2.3", &bins).expect("assemble");
    let fname = staged
        .tarball_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    assert_eq!(fname, "anodize-demo-1.2.3.tgz");
}

// -----------------------------------------------------------------------------
// optional-deps layout generation (Part B — the realign)
// -----------------------------------------------------------------------------

fn optional_deps_ctx() -> (tempfile::TempDir, anodizer_core::context::Context) {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .crates(vec![demo_crate()])
        .build();
    add_binary(&mut ctx, tmp.path(), "x86_64-unknown-linux-musl", "demo");
    add_binary(&mut ctx, tmp.path(), "x86_64-unknown-linux-gnu", "demo");
    add_binary(&mut ctx, tmp.path(), "x86_64-apple-darwin", "demo");
    add_binary(&mut ctx, tmp.path(), "x86_64-pc-windows-msvc", "demo.exe");
    (tmp, ctx)
}

fn opt_cfg() -> NpmConfig {
    NpmConfig {
        scope: Some("@anodize".into()),
        metapackage: Some("demo".into()),
        bin: Some("demo".into()),
        ..Default::default()
    }
}

#[test]
fn optional_deps_emits_per_platform_packages_with_derived_triples() {
    let (_tmp, ctx) = optional_deps_ctx();
    let layout = generate_layout(&ctx, &opt_cfg(), "demo", "1.2.3").expect("layout");

    // 4 distinct platform packages (musl + gnu are separate under libc_aware).
    let names: Vec<&str> = layout.platforms.iter().map(|p| p.name.as_str()).collect();
    assert!(names.contains(&"@anodize/demo-linux-x64-musl"), "{names:?}");
    assert!(
        names.contains(&"@anodize/demo-linux-x64-glibc"),
        "{names:?}"
    );
    assert!(names.contains(&"@anodize/demo-darwin-x64"), "{names:?}");
    assert!(names.contains(&"@anodize/demo-win32-x64"), "{names:?}");

    // linux-x64-musl package.json carries os/cpu/libc selectors.
    let musl = layout
        .platforms
        .iter()
        .find(|p| p.name == "@anodize/demo-linux-x64-musl")
        .expect("musl pkg");
    let j: serde_json::Value = serde_json::from_str(&musl.package_json).expect("json");
    assert_eq!(j["os"], serde_json::json!(["linux"]));
    assert_eq!(j["cpu"], serde_json::json!(["x64"]));
    assert_eq!(j["libc"], serde_json::json!(["musl"]));

    // darwin package has NO libc selector.
    let darwin = layout
        .platforms
        .iter()
        .find(|p| p.name == "@anodize/demo-darwin-x64")
        .expect("darwin pkg");
    let j: serde_json::Value = serde_json::from_str(&darwin.package_json).expect("json");
    assert_eq!(j["os"], serde_json::json!(["darwin"]));
    assert!(j.get("libc").is_none(), "darwin must not carry libc");

    // gnu -> glibc in the libc field.
    let gnu = layout
        .platforms
        .iter()
        .find(|p| p.name == "@anodize/demo-linux-x64-glibc")
        .expect("glibc pkg");
    let j: serde_json::Value = serde_json::from_str(&gnu.package_json).expect("json");
    assert_eq!(j["libc"], serde_json::json!(["glibc"]));
}

#[test]
fn optional_deps_metapackage_lists_all_platform_deps_and_shim() {
    let (_tmp, ctx) = optional_deps_ctx();
    let layout = generate_layout(&ctx, &opt_cfg(), "demo", "1.2.3").expect("layout");

    let meta: serde_json::Value = serde_json::from_str(&layout.metapackage_json).expect("json");
    assert_eq!(meta["name"], "demo");
    assert_eq!(meta["bin"]["demo"], "shim.js");
    let opt = meta["optionalDependencies"].as_object().expect("opt deps");
    assert_eq!(opt.len(), 4, "all four platform pkgs listed");
    for p in &layout.platforms {
        assert_eq!(opt[&p.name], "1.2.3", "{} listed at version", p.name);
    }

    // shim resolves via require.resolve and detects musl.
    assert!(layout.shim_js.contains("require.resolve"));
    assert!(layout.shim_js.contains("BINARY_OVERRIDE"));
    assert!(layout.shim_js.contains("musl"));
    assert!(layout.shim_js.contains("@anodize/demo-linux-x64-musl"));
}

#[test]
fn optional_deps_libc_aware_false_collapses_linux() {
    let (_tmp, ctx) = optional_deps_ctx();
    let cfg = NpmConfig {
        libc_aware: false,
        ..opt_cfg()
    };
    let layout = generate_layout(&ctx, &cfg, "demo", "1.2.3").expect("layout");
    let names: Vec<&str> = layout.platforms.iter().map(|p| p.name.as_str()).collect();
    // musl + gnu collapse to one linux-x64 package (no libc suffix).
    assert!(names.contains(&"@anodize/demo-linux-x64"), "{names:?}");
    assert!(!names.iter().any(|n| n.contains("musl")), "{names:?}");
    assert!(!names.iter().any(|n| n.contains("glibc")), "{names:?}");
    // The collapsed linux package emits no libc selector.
    let linux = layout
        .platforms
        .iter()
        .find(|p| p.name == "@anodize/demo-linux-x64")
        .expect("linux pkg");
    let j: serde_json::Value = serde_json::from_str(&linux.package_json).expect("json");
    assert!(j.get("libc").is_none());
}

#[test]
fn optional_deps_requires_scope() {
    let (_tmp, ctx) = optional_deps_ctx();
    let cfg = NpmConfig {
        scope: None,
        metapackage: Some("demo".into()),
        ..Default::default()
    };
    let err = generate_layout(&ctx, &cfg, "demo", "1.2.3").expect_err("must require scope");
    assert!(err.to_string().contains("scope:"), "{err}");
}

#[test]
fn optional_deps_no_binaries_errors() {
    let ctx = TestContextBuilder::new()
        .project_name("demo")
        .crates(vec![demo_crate()])
        .build();
    let err = generate_layout(&ctx, &opt_cfg(), "demo", "1.2.3").expect_err("no binaries");
    assert!(err.to_string().contains("no binary artifacts"), "{err}");
}

#[test]
fn optional_deps_layout_is_deterministic() {
    let (_tmp, ctx) = optional_deps_ctx();
    let cfg = opt_cfg();
    let l1 = generate_layout(&ctx, &cfg, "demo", "1.2.3").expect("l1");
    let l2 = generate_layout(&ctx, &cfg, "demo", "1.2.3").expect("l2");
    assert_eq!(l1.metapackage_json, l2.metapackage_json);
    assert_eq!(l1.shim_js, l2.shim_js);
    let n1: Vec<&str> = l1.platforms.iter().map(|p| p.name.as_str()).collect();
    let n2: Vec<&str> = l2.platforms.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(n1, n2);
}

// -----------------------------------------------------------------------------
// Workspace per-crate config mode
// -----------------------------------------------------------------------------

#[test]
fn optional_deps_filters_by_ids_for_workspace_per_crate() {
    // Two crates' binaries in the artifact set; `ids: [demo]` selects only the
    // demo crate's binaries so a per-crate npms entry stays scoped.
    let tmp = tempfile::TempDir::new().expect("tmp");
    let mut ctx = TestContextBuilder::new()
        .project_name("ws")
        .tag("v1.2.3")
        .crates(vec![
            demo_crate(),
            CrateConfig {
                name: "other".to_string(),
                path: "other".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                ..Default::default()
            },
        ])
        .build();
    add_binary(&mut ctx, tmp.path(), "x86_64-unknown-linux-musl", "demo");
    // An `other`-crate binary that must NOT appear in the demo metapackage.
    let other_path = tmp.path().join("other-linux");
    std::fs::write(&other_path, b"ELF-other").expect("write");
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::UploadableBinary,
        path: other_path,
        name: "other".to_string(),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "other".to_string(),
        metadata: std::collections::HashMap::new(),
        size: None,
    });

    let cfg = NpmConfig {
        ids: Some(vec!["demo".into()]),
        ..opt_cfg()
    };
    let layout = generate_layout(&ctx, &cfg, "demo", "1.2.3").expect("layout");
    assert_eq!(
        layout.platforms.len(),
        1,
        "only the demo binary is selected"
    );
    assert_eq!(layout.platforms[0].name, "@anodize/demo-linux-x64-musl");
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
fn npm_publisher_required_override_honored() {
    let p = NpmPublisher::with_required(Some(false));
    assert!(!p.required(), "required: false override must win");
    let p = NpmPublisher::with_required(None);
    assert!(p.required(), "None falls through to the built-in default");
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
        .crates(vec![demo_crate()])
        .build();
    let p = NpmPublisher::new();
    let evidence = p.run(&mut ctx).expect("run ok");
    assert_eq!(evidence.publisher, "npm");
    assert!(evidence.primary_ref.is_none());
}

// -----------------------------------------------------------------------------
// Dry-run + skip-paths (both modes)
// -----------------------------------------------------------------------------

#[test]
fn publish_postinstall_dry_run_returns_empty_without_invoking_npm() {
    let mut ctx = ctx_with_archives();
    ctx.options.dry_run = true;
    let cfg = npm_cfg();
    let log = ctx.logger("publish");
    let outcome = publish_to_npm(&ctx, &cfg, "demo", &log).expect("publish");
    assert!(outcome.is_empty(), "dry-run must return no targets");
}

#[test]
fn publish_optional_deps_dry_run_returns_empty() {
    let (_tmp, mut ctx) = optional_deps_ctx();
    ctx.options.dry_run = true;
    let cfg = opt_cfg();
    let log = ctx.logger("publish");
    let outcome = publish_to_npm(&ctx, &cfg, "demo", &log).expect("publish");
    assert!(outcome.is_empty(), "dry-run must return no targets");
}

#[test]
fn publish_skip_true_returns_empty() {
    let ctx = ctx_with_archives();
    let cfg = NpmConfig {
        mode: NpmMode::Postinstall,
        name: Some("demo".into()),
        skip: Some(StringOrBool::Bool(true)),
        ..Default::default()
    };
    let log = ctx.logger("publish");
    let outcome = publish_to_npm(&ctx, &cfg, "demo", &log).expect("publish");
    assert!(outcome.is_empty());
}

#[test]
fn publish_disable_true_returns_empty() {
    let ctx = ctx_with_archives();
    let cfg = NpmConfig {
        mode: NpmMode::Postinstall,
        name: Some("demo".into()),
        disable: Some(StringOrBool::Bool(true)),
        ..Default::default()
    };
    let log = ctx.logger("publish");
    let outcome = publish_to_npm(&ctx, &cfg, "demo", &log).expect("publish");
    assert!(outcome.is_empty());
}

#[test]
fn publish_if_condition_falsy_returns_empty() {
    let ctx = ctx_with_archives();
    let cfg = NpmConfig {
        mode: NpmMode::Postinstall,
        name: Some("demo".into()),
        if_condition: Some("false".into()),
        ..Default::default()
    };
    let log = ctx.logger("publish");
    let outcome = publish_to_npm(&ctx, &cfg, "demo", &log).expect("publish");
    assert!(outcome.is_empty());
}

#[test]
fn publish_postinstall_no_matching_binaries_warns_and_returns_empty() {
    let ctx = TestContextBuilder::new()
        .project_name("demo")
        .crates(vec![demo_crate()])
        .build();
    let cfg = npm_cfg();
    let log = ctx.logger("publish");
    let outcome = publish_to_npm(&ctx, &cfg, "demo", &log).expect("publish");
    assert!(outcome.is_empty());
}
