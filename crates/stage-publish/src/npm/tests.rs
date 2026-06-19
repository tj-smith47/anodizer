//! Tests for the NPM publisher (restored + realigned to optional-deps).

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::{
    Config, CrateConfig, MetadataConfig, NpmAuthMode, NpmConfig, NpmMode, StringOrBool,
};
use anodizer_core::test_helpers::TestContextBuilder;
use anodizer_core::{PreflightCheck, Publisher, PublisherGroup};
// Inspected only by unix-gated tests here; the gate must match or the import
// reads as unused on a Windows build.
#[cfg(unix)]
use anodizer_core::PublisherOutcome;

use super::manifest::{
    PlatformBinary, collect_platform_binaries, effective_provenance_override, npm_triple,
    render_package_json, render_postinstall_js, resolve_access, resolve_extra_files,
    resolve_format, resolve_name, resolve_registry, resolve_tag, runner_supports_npm_provenance,
};
use super::optional_deps::generate_layout;
use super::publish::{
    AuthDecision, NpmAuth, PackageExistence, assemble_optional_deps_tarball,
    assemble_postinstall_tarball, build_npm_publish_command, decide_auth, encode_package_path,
    probe_package_existence, publish_to_npm, publish_with_oidc_fallback, resolve_auth_for_package,
    write_npmrc,
};
use super::publisher::NpmPublisher;
use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
use std::sync::atomic::Ordering;

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
    required: true
    if: "{{ ne .Prerelease \"\" }}"
    engines:
      node: ">=14"
    files: [shim.js, README.md]
    provenance: false
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
    // First-class engines / files / provenance round-trip (deny_unknown_fields).
    assert_eq!(
        entry
            .engines
            .as_ref()
            .and_then(|e| e.get("node"))
            .map(String::as_str),
        Some(">=14")
    );
    assert_eq!(
        entry.files.as_deref(),
        Some(&["shim.js".to_string(), "README.md".to_string()][..])
    );
    assert_eq!(entry.provenance, Some(false));
}

#[test]
fn npm_config_defaults_resolve_correctly() {
    let cfg = NpmConfig::default();
    assert_eq!(cfg.mode, NpmMode::OptionalDeps);
    assert!(cfg.libc_aware);
    let ctx = anodizer_core::test_helpers::TestContextBuilder::new().build();
    assert_eq!(resolve_tag(&ctx, &cfg).unwrap(), "latest");
    assert_eq!(resolve_format(&cfg), "tgz");
    assert_eq!(
        resolve_registry(&ctx, &cfg).unwrap(),
        "https://registry.npmjs.org"
    );
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
    let bins = collect_platform_binaries(&ctx, &cfg, "demo", "1.2.3", &ctx.logger("publish"))
        .expect("collect");
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
    let body = render_package_json(&ctx, &cfg, "anodize-demo", "demo", "1.2.3", &bins, None)
        .expect("render");
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
    let body = render_package_json(&ctx, &cfg, "demo", "demo", "1.0.0", &[], None).expect("render");
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid json");
    assert_eq!(parsed["description"], "From metadata");
    assert_eq!(parsed["homepage"], "https://meta.example.com");
    assert_eq!(parsed["license"], "Apache-2.0");
}

#[test]
fn compound_spdx_license_emitted_verbatim() {
    // npm passes the SPDX license through unchanged: a dual `MIT OR Apache-2.0`
    // expression derived from project metadata must land in package.json's
    // `license` field as the exact string, not split or reshaped.
    let cfg_top = Config {
        project_name: "demo".to_string(),
        metadata: Some(MetadataConfig {
            license: Some("MIT OR Apache-2.0".to_string()),
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
    let body = render_package_json(&ctx, &cfg, "demo", "demo", "1.0.0", &[], None).expect("render");
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid json");
    assert_eq!(parsed["license"], "MIT OR Apache-2.0");
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
    let body = render_package_json(&ctx, &cfg, "demo", "demo", "1.0.0", &[], None).expect("render");
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
    // Every format branch is present, including uncompressed `tar` (-xf, no -z)
    // distinct from gzip'd `tgz`/`tar.gz` (-xzf), zip, and raw binary.
    assert!(body.contains("'binary'"));
    assert!(body.contains("unzip -o"));
    assert!(body.contains("=== 'tar'"));
    assert!(body.contains("tar -xf "));
    assert!(body.contains("tar -xzf "));
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
    let bins =
        collect_platform_binaries(&ctx, &cfg, "anodize-demo", "1.2.3", &ctx.logger("publish"))
            .expect("collect");
    let t1 =
        assemble_postinstall_tarball(&ctx, &cfg, "demo", "1.2.3", &bins, None).expect("assemble 1");
    let t2 =
        assemble_postinstall_tarball(&ctx, &cfg, "demo", "1.2.3", &bins, None).expect("assemble 2");
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
        assemble_postinstall_tarball(&ctx, &cfg, "demo", "1.2.3", &bins, None).expect("assemble");
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
    let layout = generate_layout(
        &ctx,
        &opt_cfg(),
        "demo",
        "1.2.3",
        None,
        &ctx.logger("publish"),
    )
    .expect("layout");

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
    let layout = generate_layout(
        &ctx,
        &opt_cfg(),
        "demo",
        "1.2.3",
        None,
        &ctx.logger("publish"),
    )
    .expect("layout");

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
    let layout =
        generate_layout(&ctx, &cfg, "demo", "1.2.3", None, &ctx.logger("publish")).expect("layout");
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
fn optional_deps_libc_aware_false_collapse_keeps_glibc_deterministically() {
    // Insertion order favors musl (added first), yet the not-libc-aware
    // collapse must deterministically retain the GLIBC binary — the winner is
    // defined by libc rank, not artifact-insertion order. On the pre-fix code
    // (stable sort + dedup-keep-first) the musl binary would survive here.
    let tmp = tempfile::TempDir::new().expect("tmp");
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .crates(vec![demo_crate()])
        .build();
    add_binary(&mut ctx, tmp.path(), "x86_64-unknown-linux-musl", "demo");
    add_binary(&mut ctx, tmp.path(), "x86_64-unknown-linux-gnu", "demo");

    let cfg = NpmConfig {
        libc_aware: false,
        ..opt_cfg()
    };
    let layout =
        generate_layout(&ctx, &cfg, "demo", "1.2.3", None, &ctx.logger("publish")).expect("layout");

    let linux = layout
        .platforms
        .iter()
        .find(|p| p.name == "@anodize/demo-linux-x64")
        .expect("collapsed linux pkg");
    let src = linux.binary_src.to_string_lossy();
    assert!(
        src.ends_with("x86_64-unknown-linux-gnu"),
        "collapse must keep the glibc binary deterministically, got {src}"
    );
    assert_eq!(
        linux.triple.libc, "glibc",
        "retained triple must be glibc, got {:?}",
        linux.triple.libc
    );
}

#[test]
fn optional_deps_requires_scope() {
    let (_tmp, ctx) = optional_deps_ctx();
    let cfg = NpmConfig {
        scope: None,
        metapackage: Some("demo".into()),
        ..Default::default()
    };
    let err = generate_layout(&ctx, &cfg, "demo", "1.2.3", None, &ctx.logger("publish"))
        .expect_err("must require scope");
    assert!(err.to_string().contains("scope:"), "{err}");
}

#[test]
fn optional_deps_no_binaries_errors() {
    let ctx = TestContextBuilder::new()
        .project_name("demo")
        .crates(vec![demo_crate()])
        .build();
    let err = generate_layout(
        &ctx,
        &opt_cfg(),
        "demo",
        "1.2.3",
        None,
        &ctx.logger("publish"),
    )
    .expect_err("no binaries");
    assert!(err.to_string().contains("no binary artifacts"), "{err}");
}

#[test]
fn optional_deps_layout_is_deterministic() {
    let (_tmp, ctx) = optional_deps_ctx();
    let cfg = opt_cfg();
    let l1 =
        generate_layout(&ctx, &cfg, "demo", "1.2.3", None, &ctx.logger("publish")).expect("l1");
    let l2 =
        generate_layout(&ctx, &cfg, "demo", "1.2.3", None, &ctx.logger("publish")).expect("l2");
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
    let layout =
        generate_layout(&ctx, &cfg, "demo", "1.2.3", None, &ctx.logger("publish")).expect("layout");
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
    let p = NpmPublisher::with_overrides(Some(false), None);
    assert!(!p.required(), "required: false override must win");
    let p = NpmPublisher::with_overrides(None, None);
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
    let mut targets = Vec::new();
    publish_to_npm(&ctx, &cfg, "demo", &log, &mut targets).expect("publish");
    assert!(targets.is_empty(), "dry-run must return no targets");
}

#[test]
fn publish_optional_deps_dry_run_returns_empty() {
    let (_tmp, mut ctx) = optional_deps_ctx();
    ctx.options.dry_run = true;
    let cfg = opt_cfg();
    let log = ctx.logger("publish");
    let mut targets = Vec::new();
    publish_to_npm(&ctx, &cfg, "demo", &log, &mut targets).expect("publish");
    assert!(targets.is_empty(), "dry-run must return no targets");
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
    let mut targets = Vec::new();
    publish_to_npm(&ctx, &cfg, "demo", &log, &mut targets).expect("publish");
    assert!(targets.is_empty());
}

#[test]
fn publish_disable_alias_true_returns_empty() {
    // The legacy `disable: true` spelling folds into `skip` on parse, so the
    // entry is skipped at publish time via the skip gate.
    let ctx = ctx_with_archives();
    let cfg: NpmConfig = serde_yaml_ng::from_str("disable: true\nname: demo\n")
        .expect("disable: alias must parse into skip");
    assert!(matches!(cfg.skip, Some(StringOrBool::Bool(true))));
    let log = ctx.logger("publish");
    let mut targets = Vec::new();
    publish_to_npm(&ctx, &cfg, "demo", &log, &mut targets).expect("publish");
    assert!(targets.is_empty());
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
    let mut targets = Vec::new();
    publish_to_npm(&ctx, &cfg, "demo", &log, &mut targets).expect("publish");
    assert!(targets.is_empty());
}

#[test]
fn publish_postinstall_no_matching_binaries_warns_and_returns_empty() {
    let ctx = TestContextBuilder::new()
        .project_name("demo")
        .crates(vec![demo_crate()])
        .build();
    let cfg = npm_cfg();
    let log = ctx.logger("publish");
    let mut targets = Vec::new();
    publish_to_npm(&ctx, &cfg, "demo", &log, &mut targets).expect("publish");
    assert!(targets.is_empty());
}

// -----------------------------------------------------------------------------
// Auth resolution — token vs Trusted Publishing (OIDC) vs neither
// -----------------------------------------------------------------------------

const OIDC_URL_VAR: &str = "ACTIONS_ID_TOKEN_REQUEST_URL";
const OIDC_TOKEN_VAR: &str = "ACTIONS_ID_TOKEN_REQUEST_TOKEN";

/// Read the `.npmrc` body written by [`write_npmrc`] for the resolved auth.
fn npmrc_body(auth: &NpmAuth) -> String {
    let dir = tempfile::TempDir::new().expect("tmp");
    let path = write_npmrc(
        dir.path(),
        "https://registry.npmjs.org",
        auth,
        Some("public"),
    )
    .expect("npmrc");
    std::fs::read_to_string(path).expect("read npmrc")
}

/// An `opt_cfg` with a forced auth mode (avoids the `auto`-mode network probe
/// in resolution unit tests).
fn opt_cfg_auth(mode: NpmAuthMode) -> NpmConfig {
    NpmConfig {
        auth: mode,
        ..opt_cfg()
    }
}

#[test]
fn auth_token_mode_writes_authtoken_line() {
    // `auth: token` + NPM_TOKEN set → `_authToken` in the .npmrc, no OIDC, no
    // network probe.
    let ctx = TestContextBuilder::new()
        .project_name("demo")
        .env("NPM_TOKEN", "npm_secretvalue")
        .build();
    let cfg = opt_cfg_auth(NpmAuthMode::Token);
    let (auth, _token) = resolve_auth_for_package(
        &ctx,
        &cfg,
        "https://registry.npmjs.org",
        "demo",
        &ctx.logger("p"),
    )
    .expect("auth resolves to token");
    assert_eq!(auth, NpmAuth::Token("npm_secretvalue".to_string()));

    let body = npmrc_body(&auth);
    assert!(
        body.contains("_authToken=npm_secretvalue"),
        "token .npmrc must carry _authToken: {body:?}"
    );
}

#[test]
fn auth_oidc_mode_writes_no_token_and_threads_env() {
    // `auth: oidc` + both OIDC request vars set → tokenless Trusted Publishing:
    // the .npmrc carries NO _authToken, and the publish command's env carries
    // the OIDC request vars so the npm CLI performs the exchange. No probe.
    let ctx = TestContextBuilder::new()
        .project_name("demo")
        .env(OIDC_URL_VAR, "https://token.actions.example/req")
        .env(OIDC_TOKEN_VAR, "oidc-request-jwt")
        .build();
    let cfg = opt_cfg_auth(NpmAuthMode::Oidc);
    let (auth, _token) = resolve_auth_for_package(
        &ctx,
        &cfg,
        "https://registry.npmjs.org",
        "demo",
        &ctx.logger("p"),
    )
    .expect("auth resolves to oidc");
    match &auth {
        NpmAuth::Oidc(pairs) => {
            assert!(
                pairs
                    .iter()
                    .any(|(k, v)| k == OIDC_URL_VAR && v == "https://token.actions.example/req")
            );
            assert!(
                pairs
                    .iter()
                    .any(|(k, v)| k == OIDC_TOKEN_VAR && v == "oidc-request-jwt")
            );
        }
        other => panic!("expected OIDC auth, got {other:?}"),
    }

    let body = npmrc_body(&auth);
    assert!(
        !body.contains("_authToken"),
        "OIDC .npmrc must NOT carry _authToken: {body:?}"
    );

    // The publish subprocess must inherit the OIDC request vars.
    let dir = tempfile::TempDir::new().expect("tmp");
    let cmd = build_npm_publish_command(
        std::path::Path::new("/tmp/demo-1.0.0.tgz"),
        dir.path(),
        "https://registry.npmjs.org",
        "latest",
        Some("public"),
        &auth,
    );
    let envs: std::collections::HashMap<String, Option<String>> = cmd
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect();
    assert_eq!(
        envs.get(OIDC_URL_VAR),
        Some(&Some("https://token.actions.example/req".to_string())),
        "publish command must thread the OIDC URL var"
    );
    assert_eq!(
        envs.get(OIDC_TOKEN_VAR),
        Some(&Some("oidc-request-jwt".to_string())),
        "publish command must thread the OIDC token var"
    );
}

#[test]
fn auth_no_token_no_oidc_is_clear_error_not_panic_or_skip() {
    // Neither credential present (auto mode) → hard error, never anonymous
    // publish, never silent skip, and no network probe (the verdict is
    // ErrorNoAuth regardless of existence). A sealed (closed, empty) env
    // ensures the dev/CI host's real NPM_TOKEN / OIDC vars cannot leak in.
    let ctx = TestContextBuilder::new()
        .project_name("demo")
        .sealed_env()
        .build();
    let cfg = opt_cfg();
    let err = resolve_auth_for_package(
        &ctx,
        &cfg,
        "https://registry.npmjs.org",
        "demo",
        &ctx.logger("p"),
    )
    .expect_err("must error with no credential");
    let msg = err.to_string();
    assert!(
        msg.contains("cannot authenticate"),
        "error must name the auth failure: {msg}"
    );
    assert!(
        msg.contains("NPM_TOKEN") && msg.contains("OIDC"),
        "error must point at both credential paths: {msg}"
    );
}

#[test]
fn auth_oidc_mode_requires_both_vars_present() {
    // Only the URL var set (no request token) is NOT an OIDC context — `oidc`
    // mode must error, not authorize a partial/anonymous publish.
    let ctx = TestContextBuilder::new()
        .project_name("demo")
        .env(OIDC_URL_VAR, "https://token.actions.example/req")
        .build();
    let cfg = opt_cfg_auth(NpmAuthMode::Oidc);
    assert!(
        resolve_auth_for_package(
            &ctx,
            &cfg,
            "https://registry.npmjs.org",
            "demo",
            &ctx.logger("p")
        )
        .is_err(),
        "a single OIDC var must not authorize a publish"
    );
}

// -----------------------------------------------------------------------------
// Per-package auth decision matrix (pure function — no I/O, no secrets)
// -----------------------------------------------------------------------------

#[test]
fn decide_auth_matrix_auto() {
    use AuthDecision::*;
    use PackageExistence::*;
    let m = NpmAuthMode::Auto;

    // new package
    assert_eq!(decide_auth(m, New, false, true), Token); // new, no oidc, token
    assert_eq!(decide_auth(m, New, true, true), Token); // new, oidc, token → token (TP can't create)
    assert_eq!(decide_auth(m, New, true, false), FailNewNeedsToken); // new, oidc-only
    assert_eq!(decide_auth(m, New, false, false), ErrorNoAuth);

    // existing package
    assert_eq!(decide_auth(m, Exists, true, true), Oidc); // exists, oidc preferred over token
    assert_eq!(decide_auth(m, Exists, true, false), Oidc);
    assert_eq!(decide_auth(m, Exists, false, true), Token);
    assert_eq!(decide_auth(m, Exists, false, false), ErrorNoAuth);

    // unknown (probe failed)
    assert_eq!(decide_auth(m, Unknown, true, true), Token); // safe path
    assert_eq!(decide_auth(m, Unknown, false, true), Token);
    assert_eq!(decide_auth(m, Unknown, true, false), Oidc); // best effort
    assert_eq!(decide_auth(m, Unknown, false, false), ErrorNoAuth);
}

#[test]
fn decide_auth_matrix_forced_token() {
    use AuthDecision::*;
    use PackageExistence::*;
    let m = NpmAuthMode::Token;
    // Token mode forces token regardless of existence / oidc; errors if no token.
    for ex in [New, Exists, Unknown] {
        assert_eq!(decide_auth(m, ex, true, true), Token);
        assert_eq!(decide_auth(m, ex, false, true), Token);
        assert_eq!(decide_auth(m, ex, true, false), ErrorNoAuth);
        assert_eq!(decide_auth(m, ex, false, false), ErrorNoAuth);
    }
}

#[test]
fn decide_auth_matrix_forced_oidc() {
    use AuthDecision::*;
    use PackageExistence::*;
    let m = NpmAuthMode::Oidc;
    // Oidc mode forces oidc regardless of existence / token; errors if no oidc.
    // Strict: never falls back to a token.
    for ex in [New, Exists, Unknown] {
        assert_eq!(decide_auth(m, ex, true, true), Oidc);
        assert_eq!(decide_auth(m, ex, true, false), Oidc);
        assert_eq!(decide_auth(m, ex, false, true), ErrorNoAuth);
        assert_eq!(decide_auth(m, ex, false, false), ErrorNoAuth);
    }
}

#[test]
fn encode_package_path_scoped_and_unscoped() {
    // A scoped name's single `/` is percent-encoded for the registry metadata GET.
    assert_eq!(
        encode_package_path("@anodizer/cli-linux-x64"),
        "@anodizer%2Fcli-linux-x64"
    );
    // An unscoped name has no `/` and is returned unchanged.
    assert_eq!(encode_package_path("anodizer"), "anodizer");
}

// -----------------------------------------------------------------------------
// Existence probe — real GET + HTTP-status → PackageExistence mapping
// (in-process responder; no real network beyond loopback)
// -----------------------------------------------------------------------------

#[test]
fn probe_existence_maps_200_to_exists() {
    let (addr, calls) =
        spawn_oneshot_http_responder(vec!["HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}"]);
    let registry = format!("http://{addr}");
    let ctx = TestContextBuilder::new().project_name("demo").build();
    let got = probe_package_existence(&registry, "demo", &ctx.logger("p"));
    assert_eq!(got, PackageExistence::Exists, "200 → Exists");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "probe must hit the registry"
    );
}

#[test]
fn probe_existence_maps_404_to_new() {
    let (addr, calls) =
        spawn_oneshot_http_responder(vec!["HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"]);
    let registry = format!("http://{addr}");
    let ctx = TestContextBuilder::new().project_name("demo").build();
    let got = probe_package_existence(&registry, "demo", &ctx.logger("p"));
    assert_eq!(got, PackageExistence::New, "404 → New");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "probe must hit the registry"
    );
}

#[test]
fn probe_existence_maps_5xx_to_unknown() {
    let (addr, calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
    ]);
    let registry = format!("http://{addr}");
    let ctx = TestContextBuilder::new().project_name("demo").build();
    let got = probe_package_existence(&registry, "demo", &ctx.logger("p"));
    assert_eq!(
        got,
        PackageExistence::Unknown,
        "non-200/404 status is inconclusive → Unknown"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "probe must hit the registry"
    );
}

#[test]
fn probe_existence_transport_error_is_unknown() {
    // Bind an ephemeral port, capture its address, then drop the listener so
    // the port is closed: the probe's GET hits connection-refused (a transport
    // error, not an HTTP status) and must degrade to Unknown rather than panic.
    let addr = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr")
        // listener dropped here → port closed
    };
    let registry = format!("http://{addr}");
    let ctx = TestContextBuilder::new().project_name("demo").build();
    let got = probe_package_existence(&registry, "demo", &ctx.logger("p"));
    assert_eq!(
        got,
        PackageExistence::Unknown,
        "a transport error must map to Unknown, never a false Exists/New"
    );
}

#[test]
fn probe_existence_url_encodes_scoped_name_on_the_wire() {
    // A scoped name's `/` must be `%2F` in the live URL path. The capturing
    // responder records the request line so we can assert `encode_package_path`
    // is wired into the GET, not just unit-tested in isolation.
    let (addr, captured) =
        anodizer_core::test_helpers::responder::spawn_request_capturing_responder(
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}",
        );
    let registry = format!("http://{addr}");
    let ctx = TestContextBuilder::new().project_name("demo").build();
    let got = probe_package_existence(&registry, "@scope/name", &ctx.logger("p"));
    assert_eq!(got, PackageExistence::Exists);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let request = loop {
        let s = captured.lock().unwrap().clone();
        if !s.is_empty() || std::time::Instant::now() >= deadline {
            break s;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert!(
        request.contains("/@scope%2Fname "),
        "scoped name must be percent-encoded in the live URL path, got request line:\n{request}"
    );
}

// -----------------------------------------------------------------------------
// resolve_auth_for_package end-to-end — probe drives the auto-mode decision
// -----------------------------------------------------------------------------

#[test]
fn resolve_auth_auto_exists_prefers_oidc_over_token() {
    // auto mode, package Exists (200 from the mock), BOTH a token and an OIDC
    // context available → the existence-aware decision prefers OIDC (Trusted
    // Publishing) for an already-published package.
    let (addr, calls) =
        spawn_oneshot_http_responder(vec!["HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}"]);
    let registry = format!("http://{addr}");
    let ctx = TestContextBuilder::new()
        .project_name("demo")
        .env("NPM_TOKEN", "npm_secretvalue")
        .env(OIDC_URL_VAR, "https://token.actions.example/req")
        .env(OIDC_TOKEN_VAR, "oidc-request-jwt")
        .build();
    let cfg = opt_cfg_auth(NpmAuthMode::Auto);
    let (auth, token) = resolve_auth_for_package(&ctx, &cfg, &registry, "demo", &ctx.logger("p"))
        .expect("auto+exists+oidc resolves");
    assert!(
        matches!(auth, NpmAuth::Oidc(_)),
        "existing package with an OIDC context must resolve to OIDC, got {auth:?}"
    );
    // The token is still returned alongside (for the OIDC→token fallback).
    assert_eq!(token, "npm_secretvalue");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "auto mode with a credential must probe the registry"
    );
}

#[test]
fn resolve_auth_auto_exists_token_only_resolves_token() {
    // auto mode, package Exists (200), only a token (no OIDC context) → Token.
    let (addr, calls) =
        spawn_oneshot_http_responder(vec!["HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}"]);
    let registry = format!("http://{addr}");
    let ctx = TestContextBuilder::new()
        .project_name("demo")
        .env("NPM_TOKEN", "npm_secretvalue")
        .build();
    let cfg = opt_cfg_auth(NpmAuthMode::Auto);
    let (auth, _token) = resolve_auth_for_package(&ctx, &cfg, &registry, "demo", &ctx.logger("p"))
        .expect("auto+exists+token resolves");
    assert_eq!(
        auth,
        NpmAuth::Token("npm_secretvalue".to_string()),
        "existing package with only a token must resolve to Token"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "auto mode with a credential must probe the registry"
    );
}

#[test]
fn resolve_auth_auto_new_token_only_resolves_token() {
    // auto mode, package New (404), only a token → Token (the initial publish
    // that Trusted Publishing cannot perform). Drives the probe → decide → New
    // branch live.
    let (addr, calls) =
        spawn_oneshot_http_responder(vec!["HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"]);
    let registry = format!("http://{addr}");
    let ctx = TestContextBuilder::new()
        .project_name("demo")
        .env("NPM_TOKEN", "npm_secretvalue")
        .build();
    let cfg = opt_cfg_auth(NpmAuthMode::Auto);
    let (auth, _token) = resolve_auth_for_package(&ctx, &cfg, &registry, "demo", &ctx.logger("p"))
        .expect("auto+new+token resolves");
    assert_eq!(
        auth,
        NpmAuth::Token("npm_secretvalue".to_string()),
        "new package with a token must resolve to Token for the initial publish"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "auto mode must probe the registry"
    );
}

#[test]
fn resolve_auth_auto_renders_cfg_token_template() {
    // auto mode, package Exists (200), the token comes from a templated
    // `cfg.token` (not NPM_TOKEN) → the template is rendered and the resolved
    // auth carries the rendered value. Covers the `cfg.token` template branch
    // of `resolve_token` end-to-end through the probe→decide→Token path.
    let (addr, calls) =
        spawn_oneshot_http_responder(vec!["HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}"]);
    let registry = format!("http://{addr}");
    let ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.0.0")
        // Seal the env so a host/runner that exports the OIDC request vars
        // (ACTIONS_ID_TOKEN_REQUEST_*) cannot flip the auto verdict from Token
        // to Oidc — this asserts the cfg.token path, not the ambient env.
        .sealed_env()
        .build();
    let cfg = NpmConfig {
        // A `{{ .Version }}` token proves the template render fires rather than
        // the raw string passing through.
        token: Some("tok-{{ .Version }}".into()),
        ..opt_cfg_auth(NpmAuthMode::Auto)
    };
    let (auth, token) = resolve_auth_for_package(&ctx, &cfg, &registry, "demo", &ctx.logger("p"))
        .expect("templated cfg.token resolves");
    assert_eq!(
        auth,
        NpmAuth::Token("tok-1.0.0".to_string()),
        "cfg.token template must be rendered into the resolved auth"
    );
    assert_eq!(token, "tok-1.0.0");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "auto mode must probe the registry"
    );
}

#[test]
fn resolve_auth_auto_new_oidc_only_fails_needs_token_live_probe() {
    // auto mode, package New (404 from the mock), an OIDC context but NO token:
    // Trusted Publishing cannot create a non-existent package. The live probe →
    // decide → FailNewNeedsToken bail must fire with the package-naming, fixable
    // error — covering the probe-driven terminal branch end-to-end.
    let (addr, calls) =
        spawn_oneshot_http_responder(vec!["HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"]);
    let registry = format!("http://{addr}");
    let ctx = TestContextBuilder::new()
        .project_name("demo")
        .env(OIDC_URL_VAR, "https://token.actions.example/req")
        .env(OIDC_TOKEN_VAR, "oidc-request-jwt")
        .build();
    let cfg = opt_cfg_auth(NpmAuthMode::Auto);
    let err = resolve_auth_for_package(&ctx, &cfg, &registry, "demo", &ctx.logger("p"))
        .expect_err("new package + oidc-only must fail needing a token");
    let msg = err.to_string();
    assert!(
        msg.contains("does not exist") && msg.contains("Trusted Publishing"),
        "error must explain TP cannot create a new package: {msg}"
    );
    assert!(
        msg.contains("demo"),
        "error must name the offending package: {msg}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "auto mode must probe the registry"
    );
}

#[test]
fn auth_auto_new_package_oidc_only_fails_with_specific_error() {
    // auto + OIDC context but NO token + the registry says the package is new:
    // Trusted Publishing cannot create it. The decision is FailNewNeedsToken;
    // the materialized error must name the package and tell the operator to set
    // NPM_TOKEN for the initial publish.
    let d = decide_auth(
        NpmAuthMode::Auto,
        PackageExistence::New,
        /* oidc */ true,
        /* token */ false,
    );
    assert_eq!(d, AuthDecision::FailNewNeedsToken);
}

#[test]
fn auth_token_mode_no_token_errors_naming_mode() {
    // `auth: token` with no token → specific error naming the mode, no probe.
    let ctx = TestContextBuilder::new()
        .project_name("demo")
        .sealed_env()
        .build();
    let cfg = opt_cfg_auth(NpmAuthMode::Token);
    let err = resolve_auth_for_package(
        &ctx,
        &cfg,
        "https://registry.npmjs.org",
        "demo",
        &ctx.logger("p"),
    )
    .expect_err("token mode with no token must error");
    let msg = err.to_string();
    assert!(
        msg.contains("token") && msg.contains("NPM_TOKEN"),
        "error must name the token mode + NPM_TOKEN: {msg}"
    );
}

#[test]
fn auth_oidc_mode_no_fallback_to_token_present() {
    // `auth: oidc` but only a token is present (no OIDC) → strict: must error,
    // never silently use the token.
    let ctx = TestContextBuilder::new()
        .project_name("demo")
        .env("NPM_TOKEN", "npm_secretvalue")
        .build();
    let cfg = opt_cfg_auth(NpmAuthMode::Oidc);
    let err = resolve_auth_for_package(
        &ctx,
        &cfg,
        "https://registry.npmjs.org",
        "demo",
        &ctx.logger("p"),
    )
    .expect_err("oidc mode with no OIDC must error even with a token present");
    assert!(
        err.to_string().contains("oidc"),
        "error must name the oidc mode: {err}"
    );
}

// -----------------------------------------------------------------------------
// OIDC → token fallback (auto mode only): a failed Trusted Publishing publish
// retries with the token and warns loudly that TP was not exercised.
// -----------------------------------------------------------------------------

#[test]
fn oidc_failure_falls_back_to_token_with_loud_warning_in_auto() {
    let dir = tempfile::TempDir::new().expect("tmp");
    let oidc = NpmAuth::Oidc(vec![(OIDC_URL_VAR.to_string(), "u".to_string())]);
    let (log, cap) = anodizer_core::log::StageLogger::with_capture(
        "publish",
        anodizer_core::log::Verbosity::Normal,
    );

    // Record every (attempt, auth) pair; the OIDC attempt fails, the token
    // retry succeeds.
    let mut attempts: Vec<NpmAuth> = Vec::new();
    let res = publish_with_oidc_fallback(
        "demo-linux-x64",
        NpmAuthMode::Auto,
        &oidc,
        Some("npm_secretvalue".to_string()),
        dir.path(),
        "https://registry.npmjs.org",
        Some("public"),
        &log,
        &mut |npmrc_dir, auth| {
            attempts.push(auth.clone());
            if matches!(auth, NpmAuth::Oidc(_)) {
                Err(anyhow::anyhow!("npm publish failed: 401 trusted publisher"))
            } else {
                // The retry must see a token-bearing .npmrc.
                let body = std::fs::read_to_string(npmrc_dir.join(".npmrc")).expect("npmrc");
                assert!(
                    body.contains("_authToken=npm_secretvalue"),
                    "token retry must rewrite .npmrc with _authToken: {body:?}"
                );
                Ok(())
            }
        },
    );

    assert!(res.is_ok(), "fallback retry should succeed: {res:?}");
    assert_eq!(
        attempts.len(),
        2,
        "exactly one OIDC attempt + one token retry"
    );
    assert!(matches!(attempts[0], NpmAuth::Oidc(_)));
    assert_eq!(attempts[1], NpmAuth::Token("npm_secretvalue".to_string()));

    let warns = cap.warn_messages().join("\n");
    assert!(
        warns.contains("demo-linux-x64")
            && warns.contains("NOT exercised")
            && warns.contains("NPM_TOKEN"),
        "must warn loudly, naming the package + the TP gap: {warns}"
    );
}

#[test]
fn oidc_failure_in_oidc_mode_does_not_fall_back() {
    // `auth: oidc` (strict): a failed OIDC publish must NOT retry with a token,
    // even if a token is available — fail loud.
    let dir = tempfile::TempDir::new().expect("tmp");
    let oidc = NpmAuth::Oidc(vec![(OIDC_URL_VAR.to_string(), "u".to_string())]);
    let (log, _cap) = anodizer_core::log::StageLogger::with_capture(
        "publish",
        anodizer_core::log::Verbosity::Normal,
    );

    let mut attempts = 0usize;
    let res = publish_with_oidc_fallback(
        "demo",
        NpmAuthMode::Oidc,
        &oidc,
        Some("npm_secretvalue".to_string()),
        dir.path(),
        "https://registry.npmjs.org",
        Some("public"),
        &log,
        &mut |_dir, _auth| {
            attempts += 1;
            Err(anyhow::anyhow!("npm publish failed"))
        },
    );
    assert!(res.is_err(), "strict oidc must propagate the failure");
    assert_eq!(attempts, 1, "no token retry in oidc mode");
}

#[test]
fn oidc_failure_no_token_available_does_not_fall_back() {
    // auto mode, OIDC chosen, publish fails, but NO token → no retry possible;
    // the failure propagates.
    let dir = tempfile::TempDir::new().expect("tmp");
    let oidc = NpmAuth::Oidc(vec![(OIDC_URL_VAR.to_string(), "u".to_string())]);
    let (log, _cap) = anodizer_core::log::StageLogger::with_capture(
        "publish",
        anodizer_core::log::Verbosity::Normal,
    );

    let mut attempts = 0usize;
    let res = publish_with_oidc_fallback(
        "demo",
        NpmAuthMode::Auto,
        &oidc,
        None,
        dir.path(),
        "https://registry.npmjs.org",
        Some("public"),
        &log,
        &mut |_dir, _auth| {
            attempts += 1;
            Err(anyhow::anyhow!("npm publish failed"))
        },
    );
    assert!(res.is_err(), "no token → failure propagates");
    assert_eq!(attempts, 1, "no retry without a token");
}

#[test]
fn token_publish_success_does_not_trigger_fallback() {
    // A successful first publish (token auth) returns Ok with a single attempt;
    // the fallback branch is never reached.
    let dir = tempfile::TempDir::new().expect("tmp");
    let token = NpmAuth::Token("npm_secretvalue".to_string());
    let (log, _cap) = anodizer_core::log::StageLogger::with_capture(
        "publish",
        anodizer_core::log::Verbosity::Normal,
    );

    let mut attempts = 0usize;
    let res = publish_with_oidc_fallback(
        "demo",
        NpmAuthMode::Auto,
        &token,
        Some("npm_secretvalue".to_string()),
        dir.path(),
        "https://registry.npmjs.org",
        Some("public"),
        &log,
        &mut |_dir, _auth| {
            attempts += 1;
            Ok(())
        },
    );
    assert!(res.is_ok());
    assert_eq!(attempts, 1);
}

// -----------------------------------------------------------------------------
// Config parse: `auth:` round-trips; absent → auto.
// -----------------------------------------------------------------------------

#[test]
fn config_auth_field_parses_and_defaults_to_auto() {
    let absent: NpmConfig = serde_yaml_ng::from_str("name: demo\n").expect("parse");
    assert_eq!(absent.auth, NpmAuthMode::Auto, "absent auth → auto");

    let auto: NpmConfig = serde_yaml_ng::from_str("name: demo\nauth: auto\n").expect("parse");
    assert_eq!(auto.auth, NpmAuthMode::Auto);

    let token: NpmConfig = serde_yaml_ng::from_str("name: demo\nauth: token\n").expect("parse");
    assert_eq!(token.auth, NpmAuthMode::Token);

    let oidc: NpmConfig = serde_yaml_ng::from_str("name: demo\nauth: oidc\n").expect("parse");
    assert_eq!(oidc.auth, NpmAuthMode::Oidc);

    // Round-trip: serialize then re-parse.
    let yaml = serde_yaml_ng::to_string(&token).expect("serialize");
    let back: NpmConfig = serde_yaml_ng::from_str(&yaml).expect("reparse");
    assert_eq!(back.auth, NpmAuthMode::Token);
}

// -----------------------------------------------------------------------------
// Excluded-target warning (#2 — no silent platform-coverage gaps)
// -----------------------------------------------------------------------------

#[test]
fn optional_deps_warns_on_unsupported_target_and_skips_it() {
    // A supported binary plus a darwin-universal binary (arch "all" → npm has
    // no universal selector). The universal one must be excluded WITH a warning
    // rather than silently dropped.
    let tmp = tempfile::TempDir::new().expect("tmp");
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .crates(vec![demo_crate()])
        .build();
    add_binary(&mut ctx, tmp.path(), "x86_64-unknown-linux-musl", "demo");
    add_binary(&mut ctx, tmp.path(), "darwin-universal", "demo");

    let (log, cap) = anodizer_core::log::StageLogger::with_capture(
        "publish",
        anodizer_core::log::Verbosity::Normal,
    );
    let layout = generate_layout(&ctx, &opt_cfg(), "demo", "1.2.3", None, &log).expect("layout");
    // Only the linux package is emitted; the universal target is excluded.
    assert_eq!(layout.platforms.len(), 1);
    assert_eq!(layout.platforms[0].name, "@anodize/demo-linux-x64-musl");
    assert!(
        cap.warn_count() >= 1,
        "excluded target must produce a warning"
    );
}

#[test]
fn collect_platform_binaries_warns_on_unsupported_archive_target() {
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
        "sparc64-unknown-linux-gnu",
        "22".repeat(32),
        "https://example.com/demo-sparc64.tgz",
    );
    let cfg = npm_cfg();
    let (log, cap) = anodizer_core::log::StageLogger::with_capture(
        "publish",
        anodizer_core::log::Verbosity::Normal,
    );
    let bins = collect_platform_binaries(&ctx, &cfg, "demo", "1.2.3", &log).expect("collect");
    assert_eq!(bins.len(), 1, "only the supported target is collected");
    assert_eq!(bins[0].os, "linux");
    assert!(cap.warn_count() >= 1, "sparc64 exclusion must warn");
}

// -----------------------------------------------------------------------------
// optional-deps tarball reproducibility + binary mode (#3 — default-mode path)
// -----------------------------------------------------------------------------

#[test]
fn assemble_optional_deps_tarball_is_reproducible_and_binary_is_0o755() {
    let ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .crates(vec![demo_crate()])
        .build();
    let cfg = opt_cfg();
    let pkg_json = r#"{"name":"@anodize/demo-linux-x64-musl","version":"1.2.3","os":["linux"],"cpu":["x64"],"libc":["musl"]}"#;
    let binary = b"ELF-fake-binary".to_vec();
    let embedded = vec![("demo".to_string(), binary, 0o755u32)];

    let t1 = assemble_optional_deps_tarball(
        &ctx,
        &cfg,
        "@anodize/demo-linux-x64-musl",
        "1.2.3",
        pkg_json,
        &embedded,
    )
    .expect("assemble 1");
    let t2 = assemble_optional_deps_tarball(
        &ctx,
        &cfg,
        "@anodize/demo-linux-x64-musl",
        "1.2.3",
        pkg_json,
        &embedded,
    )
    .expect("assemble 2");

    let b1 = std::fs::read(&t1.tarball_path).expect("read 1");
    let b2 = std::fs::read(&t2.tarball_path).expect("read 2");
    assert_eq!(b1, b2, "optional-deps tarball must be byte-reproducible");

    // The embedded binary must land inside the tarball at mode 0o755.
    let f = std::fs::File::open(&t1.tarball_path).expect("open tgz");
    let gz = flate2::read::GzDecoder::new(f);
    let mut ar = tar::Archive::new(gz);
    let mut saw_binary_0o755 = false;
    for entry in ar.entries().expect("entries") {
        let entry = entry.expect("entry");
        let path = entry.path().expect("path").into_owned();
        if path.ends_with("package/demo") {
            let mode = entry.header().mode().expect("mode");
            assert_eq!(mode & 0o777, 0o755, "embedded binary must be 0o755");
            saw_binary_0o755 = true;
        }
    }
    assert!(
        saw_binary_0o755,
        "binary entry must be present in the tarball"
    );
    drop((t1, t2));
}

// -----------------------------------------------------------------------------
// Partial-failure rollback-evidence survival (#1 — irreversible publisher)
// -----------------------------------------------------------------------------

/// End-to-end proof that a mid-sequence `npm publish` failure does NOT lose the
/// rollback coordinates of packages already live on the registry. A fake `npm`
/// on PATH succeeds for the first `publish` and fails for the second; the
/// publisher must still return `Ok(evidence)` carrying the first package as an
/// `npm_targets` entry and record a `Failed` outcome (so dispatch keeps the
/// evidence instead of dropping it to `None`).
#[cfg(unix)]
#[test]
#[serial_test::serial(npm_counter)]
fn partial_publish_failure_preserves_rollback_evidence() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::TempDir::new().expect("tmp");
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).expect("bin dir");
    let counter = tmp.path().join("publish_count");

    // Fake `npm`: `view` always reports E404 (never-published, so the
    // idempotency probe proceeds to publish); `publish` succeeds on attempt 1
    // and fails on attempt 2+.
    let npm = bin_dir.join("npm");
    std::fs::write(
        &npm,
        r#"#!/bin/sh
case "$1" in
  view)
    echo "npm error code E404" 1>&2
    exit 1
    ;;
  publish)
    n=0
    if [ -f "$NPM_PUBLISH_COUNTER" ]; then n=$(cat "$NPM_PUBLISH_COUNTER"); fi
    n=$((n + 1))
    echo "$n" > "$NPM_PUBLISH_COUNTER"
    if [ "$n" -ge 2 ]; then
      echo "npm error 403 second publish boom" 1>&2
      exit 1
    fi
    exit 0
    ;;
  *)
    exit 0
    ;;
esac
"#,
    )
    .expect("write fake npm");
    std::fs::set_permissions(&npm, std::fs::Permissions::from_mode(0o755)).expect("chmod npm");

    // Two platform binaries → two per-platform publishes before the metapackage.
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .env("NPM_TOKEN", "fake-token")
        .crates(vec![demo_crate()])
        .build();
    add_binary(&mut ctx, tmp.path(), "x86_64-unknown-linux-musl", "demo");
    add_binary(&mut ctx, tmp.path(), "aarch64-apple-darwin", "demo");
    ctx.config.npms = Some(vec![opt_cfg()]);

    let _g = anodizer_core::test_helpers::env::env_mutex()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let orig_path = std::env::var("PATH").unwrap_or_default();
    // SAFETY: serialised by `#[serial(npm_counter)]` plus the crate-wide
    // env_mutex (the shared PATH coordinator); paired set/restore below.
    unsafe {
        std::env::set_var("PATH", format!("{}:{}", bin_dir.display(), orig_path));
        std::env::set_var("NPM_PUBLISH_COUNTER", counter.display().to_string());
    }

    let p = NpmPublisher::new();
    let evidence = p
        .run(&mut ctx)
        .expect("run must NOT bubble Err — evidence must survive");

    // SAFETY: serialised by `#[serial(npm_counter)]` plus the crate-wide
    // env_mutex (the shared PATH coordinator); paired with the set above.
    unsafe {
        std::env::set_var("PATH", orig_path);
        std::env::remove_var("NPM_PUBLISH_COUNTER");
    }

    // The first package published successfully and MUST be recorded for
    // rollback even though a later publish failed.
    let targets = match &evidence.extra {
        anodizer_core::PublishEvidenceExtra::Npm(n) => &n.npm_targets,
        other => panic!("expected Npm evidence, got {other:?}"),
    };
    assert_eq!(
        targets.len(),
        1,
        "exactly the one already-live package must survive in evidence"
    );
    assert!(
        targets[0].package.starts_with("@anodize/demo-"),
        "recorded target is a per-platform package: {}",
        targets[0].package
    );
    assert!(
        evidence.primary_ref.is_some(),
        "primary_ref set from survivor"
    );

    // The publisher recorded a Failed outcome (so dispatch maps it Failed but
    // keeps the evidence) rather than bubbling Err (which would drop it).
    let outcome = ctx
        .take_pending_outcome()
        .expect("a Failed outcome override must be recorded");
    assert!(
        matches!(outcome, PublisherOutcome::Failed(_)),
        "outcome must be Failed, got {outcome:?}"
    );
}

/// M4: a missing platform binary must abort the optional-deps publish with
/// NOTHING published — the staging pass reads every binary BEFORE the first
/// `npm publish`, so a binary that is not on disk fails fast and no irreversible
/// publish fires. A fake `npm` records every `publish` invocation; the counter
/// must stay at zero.
#[cfg(unix)]
#[test]
#[serial_test::serial(npm_counter)]
fn missing_platform_binary_publishes_nothing() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::TempDir::new().expect("tmp");
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).expect("bin dir");
    let counter = tmp.path().join("publish_count");

    // Fake `npm`: `view` reports E404 (never-published); `publish` increments a
    // counter so the test can prove zero publishes occurred.
    let npm = bin_dir.join("npm");
    std::fs::write(
        &npm,
        r#"#!/bin/sh
case "$1" in
  view)
    echo "npm error code E404" 1>&2
    exit 1
    ;;
  publish)
    n=0
    if [ -f "$NPM_PUBLISH_COUNTER" ]; then n=$(cat "$NPM_PUBLISH_COUNTER"); fi
    n=$((n + 1))
    echo "$n" > "$NPM_PUBLISH_COUNTER"
    exit 0
    ;;
  *)
    exit 0
    ;;
esac
"#,
    )
    .expect("write fake npm");
    std::fs::set_permissions(&npm, std::fs::Permissions::from_mode(0o755)).expect("chmod npm");

    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .env("NPM_TOKEN", "fake-token")
        .crates(vec![demo_crate()])
        .build();
    // One real binary, plus one whose on-disk path is missing. Both map to
    // distinct npm platforms so neither is deduped away.
    add_binary(&mut ctx, tmp.path(), "x86_64-unknown-linux-musl", "demo");
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::UploadableBinary,
        // A path that was never written — simulates an unbuilt platform.
        path: tmp.path().join("demo-aarch64-apple-darwin-MISSING"),
        name: "demo".to_string(),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "demo".to_string(),
        metadata: std::collections::HashMap::new(),
        size: None,
    });
    ctx.config.npms = Some(vec![opt_cfg()]);

    let _g = anodizer_core::test_helpers::env::env_mutex()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let orig_path = std::env::var("PATH").unwrap_or_default();
    // SAFETY: serialised by `#[serial(npm_counter)]` plus the crate-wide
    // env_mutex (the shared PATH coordinator); paired set/restore below.
    unsafe {
        std::env::set_var("PATH", format!("{}:{}", bin_dir.display(), orig_path));
        std::env::set_var("NPM_PUBLISH_COUNTER", counter.display().to_string());
    }

    let p = NpmPublisher::new();
    let evidence = p
        .run(&mut ctx)
        .expect("run records Failed, never bubbles Err");

    // SAFETY: serialised by `#[serial(npm_counter)]` plus the crate-wide
    // env_mutex (the shared PATH coordinator); paired with the set above.
    unsafe {
        std::env::set_var("PATH", orig_path);
        std::env::remove_var("NPM_PUBLISH_COUNTER");
    }

    // NOTHING published: the counter file was never created because the staging
    // pass aborted on the missing binary before any `npm publish` ran.
    assert!(
        !counter.exists(),
        "no `npm publish` may run when a platform binary is missing"
    );

    // No rollback targets recorded (no package is live).
    match &evidence.extra {
        anodizer_core::PublishEvidenceExtra::Npm(n) => assert!(
            n.npm_targets.is_empty(),
            "no targets recorded — nothing was published"
        ),
        anodizer_core::PublishEvidenceExtra::Empty => {}
        other => panic!("unexpected evidence shape: {other:?}"),
    }

    // The failure is surfaced as a Failed outcome (the publisher catches the
    // staging error and records it rather than bubbling Err).
    let outcome = ctx
        .take_pending_outcome()
        .expect("a Failed outcome must be recorded for the aborted publish");
    assert!(
        matches!(outcome, PublisherOutcome::Failed(_)),
        "outcome must be Failed, got {outcome:?}"
    );
}

// -----------------------------------------------------------------------------
// Per-field completeness: Cargo.toml fallback, author, engines, files,
// publishConfig.provenance — validated against the esbuild/biome/swc exemplars.
// -----------------------------------------------------------------------------

/// Build a Context whose `derived_metadata` carries a `Cargo.toml [package]`
/// fallback for each named crate (no top-level `metadata:` block) — the plain
/// Rust crate shape the `meta_*_for(crate)` resolvers must satisfy.
fn ctx_with_derived(crates: &[(&str, MetadataConfig)]) -> anodizer_core::context::Context {
    let mut config = Config {
        project_name: "demo".to_string(),
        crates: crates
            .iter()
            .map(|(name, _)| CrateConfig {
                name: (*name).to_string(),
                path: (*name).to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    for (name, meta) in crates {
        config
            .derived_metadata
            .insert((*name).to_string(), meta.clone());
    }
    anodizer_core::context::Context::new(config, anodizer_core::context::ContextOptions::default())
}

fn meta(desc: &str, home: &str, license: &str, author: &str, repo: &str) -> MetadataConfig {
    MetadataConfig {
        description: Some(desc.to_string()),
        homepage: Some(home.to_string()),
        license: Some(license.to_string()),
        repository: Some(repo.to_string()),
        maintainers: Some(vec![author.to_string()]),
        ..Default::default()
    }
}

#[test]
fn postinstall_metadata_falls_back_to_cargo_toml_per_crate() {
    // A plain Rust crate with NO top-level metadata: block — only the
    // Cargo.toml-derived values are available. All four must resolve.
    let ctx = ctx_with_derived(&[(
        "demo",
        meta(
            "A demo CLI",
            "https://demo.example",
            "MIT",
            "Demo Dev <dev@demo.example>",
            "https://github.com/demo/demo",
        ),
    )]);
    let cfg = NpmConfig {
        mode: NpmMode::Postinstall,
        name: Some("demo".into()),
        ..Default::default()
    };
    let body = render_package_json(&ctx, &cfg, "demo", "demo", "1.0.0", &[], None).expect("render");
    let j: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(j["description"], "A demo CLI");
    assert_eq!(j["homepage"], "https://demo.example");
    assert_eq!(j["license"], "MIT");
    assert_eq!(j["author"], "Demo Dev <dev@demo.example>");
    // repository.url is derived from Cargo.toml [package].repository with no
    // publisher config — required for npm provenance validation to pass.
    assert_eq!(j["repository"]["type"], "git");
    assert_eq!(j["repository"]["url"], "https://github.com/demo/demo");
}

#[test]
fn optional_deps_metadata_per_crate_isolated_in_per_crate_workspace() {
    // Workspace per-crate: two crates, each with its OWN Cargo.toml metadata.
    // Rendering crate `alpha`'s package must emit alpha's metadata, NEVER
    // beta's — the per-crate `meta_*_for(crate)` resolver guarantees isolation.
    let tmp = tempfile::tempdir().expect("tmp");
    let mut ctx = ctx_with_derived(&[
        (
            "alpha",
            meta(
                "Alpha tool",
                "https://alpha.example",
                "MIT",
                "Alpha A <a@x>",
                "https://github.com/acme/alpha",
            ),
        ),
        (
            "beta",
            meta(
                "Beta tool",
                "https://beta.example",
                "Apache-2.0",
                "Beta B <b@x>",
                "https://github.com/acme/beta",
            ),
        ),
    ]);
    // alpha ships a single linux binary.
    let path = tmp.path().join("alpha-bin");
    std::fs::write(&path, b"ELF").expect("write");
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::UploadableBinary,
        path,
        name: "alpha".to_string(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "alpha".to_string(),
        metadata: std::collections::HashMap::new(),
        size: None,
    });
    let cfg = NpmConfig {
        scope: Some("@acme".into()),
        metapackage: Some("alpha".into()),
        bin: Some("alpha".into()),
        ..Default::default()
    };
    let layout = generate_layout(&ctx, &cfg, "alpha", "1.0.0", None, &ctx.logger("publish"))
        .expect("layout");

    let meta_j: serde_json::Value =
        serde_json::from_str(&layout.metapackage_json).expect("meta json");
    assert_eq!(
        meta_j["description"], "Alpha tool",
        "alpha metapackage desc"
    );
    assert_eq!(meta_j["homepage"], "https://alpha.example");
    assert_eq!(meta_j["license"], "MIT");
    assert_eq!(meta_j["author"], "Alpha A <a@x>");
    assert_eq!(meta_j["repository"]["url"], "https://github.com/acme/alpha");
    // Categorically NOT beta's values.
    let s = layout.metapackage_json.clone();
    assert!(!s.contains("Beta tool"), "must not leak beta metadata");
    assert!(!s.contains("Apache-2.0"), "must not leak beta license");
    assert!(!s.contains("acme/beta"), "must not leak beta repository");

    // The per-platform package carries alpha's metadata too — including the
    // repository.url that npm provenance validates against the OIDC repo.
    let plat_j: serde_json::Value =
        serde_json::from_str(&layout.platforms[0].package_json).expect("plat json");
    assert_eq!(plat_j["description"], "Alpha tool");
    assert_eq!(plat_j["license"], "MIT");
    assert_eq!(plat_j["repository"]["url"], "https://github.com/acme/alpha");
}

#[test]
fn engines_default_node_18_and_overridable() {
    // Default: { node: ">=18" } (esbuild/biome/swc floor).
    let (_tmp, ctx_b) = optional_deps_ctx();
    let layout = generate_layout(
        &ctx_b,
        &opt_cfg(),
        "demo",
        "1.0.0",
        None,
        &ctx_b.logger("publish"),
    )
    .expect("layout");
    let meta_j: serde_json::Value = serde_json::from_str(&layout.metapackage_json).expect("json");
    assert_eq!(meta_j["engines"]["node"], ">=18");
    // Per-platform packages also carry engines.
    let plat_j: serde_json::Value =
        serde_json::from_str(&layout.platforms[0].package_json).expect("json");
    assert_eq!(plat_j["engines"]["node"], ">=18");

    // Override via config.
    let mut engines = std::collections::BTreeMap::new();
    engines.insert("node".to_string(), ">=20".to_string());
    let cfg2 = NpmConfig {
        engines: Some(engines),
        ..opt_cfg()
    };
    let layout2 = generate_layout(
        &ctx_b,
        &cfg2,
        "demo",
        "1.0.0",
        None,
        &ctx_b.logger("publish"),
    )
    .expect("layout");
    let j2: serde_json::Value = serde_json::from_str(&layout2.metapackage_json).expect("json");
    assert_eq!(j2["engines"]["node"], ">=20");

    // Empty map suppresses the field.
    let cfg3 = NpmConfig {
        engines: Some(std::collections::BTreeMap::new()),
        ..opt_cfg()
    };
    let layout3 = generate_layout(
        &ctx_b,
        &cfg3,
        "demo",
        "1.0.0",
        None,
        &ctx_b.logger("publish"),
    )
    .expect("layout");
    let j3: serde_json::Value = serde_json::from_str(&layout3.metapackage_json).expect("json");
    assert!(
        j3.get("engines").is_none(),
        "empty engines map suppresses field"
    );
}

#[test]
fn publish_config_provenance_default_true_and_disable() {
    let (_tmp, ctx) = optional_deps_ctx();
    let layout = generate_layout(
        &ctx,
        &opt_cfg(),
        "demo",
        "1.0.0",
        None,
        &ctx.logger("publish"),
    )
    .expect("layout");
    let meta_j: serde_json::Value = serde_json::from_str(&layout.metapackage_json).expect("json");
    assert_eq!(
        meta_j["publishConfig"]["provenance"],
        serde_json::Value::Bool(true),
        "provenance defaults true (biome/swc norm)"
    );

    // access is co-located in publishConfig when set.
    let cfg_access = NpmConfig {
        access: Some("public".into()),
        ..opt_cfg()
    };
    let l2 = generate_layout(
        &ctx,
        &cfg_access,
        "demo",
        "1.0.0",
        None,
        &ctx.logger("publish"),
    )
    .expect("layout");
    let j2: serde_json::Value = serde_json::from_str(&l2.metapackage_json).expect("json");
    assert_eq!(j2["publishConfig"]["access"], "public");
    assert_eq!(
        j2["publishConfig"]["provenance"],
        serde_json::Value::Bool(true)
    );

    // Disable provenance.
    let cfg_off = NpmConfig {
        provenance: Some(false),
        ..opt_cfg()
    };
    let l3 = generate_layout(
        &ctx,
        &cfg_off,
        "demo",
        "1.0.0",
        None,
        &ctx.logger("publish"),
    )
    .expect("layout");
    let j3: serde_json::Value = serde_json::from_str(&l3.metapackage_json).expect("json");
    assert_eq!(
        j3["publishConfig"]["provenance"],
        serde_json::Value::Bool(false)
    );
}

#[test]
fn runner_supports_npm_provenance_across_env_permutations() {
    use anodizer_core::MapEnvSource;

    // Not GitHub Actions at all → supported (leave provenance as configured).
    assert!(runner_supports_npm_provenance(&MapEnvSource::new()));
    assert!(runner_supports_npm_provenance(
        &MapEnvSource::new().with("RUNNER_ENVIRONMENT", "self-hosted")
    ));

    // GitHub Actions + github-hosted runner → supported.
    assert!(runner_supports_npm_provenance(
        &MapEnvSource::new()
            .with("GITHUB_ACTIONS", "true")
            .with("RUNNER_ENVIRONMENT", "github-hosted")
    ));

    // GitHub Actions + self-hosted runner → UNSUPPORTED (the E422 case).
    assert!(!runner_supports_npm_provenance(
        &MapEnvSource::new()
            .with("GITHUB_ACTIONS", "true")
            .with("RUNNER_ENVIRONMENT", "self-hosted")
    ));

    // GitHub Actions but RUNNER_ENVIRONMENT unset → unsupported (conservative:
    // a self-hosted runner that fails to report its environment must not 422).
    assert!(!runner_supports_npm_provenance(
        &MapEnvSource::new().with("GITHUB_ACTIONS", "true")
    ));
}

fn npm_ctx_with_env(
    env: Vec<(&'static str, &'static str)>,
) -> (tempfile::TempDir, anodizer_core::context::Context) {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let mut b = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .crates(vec![demo_crate()])
        // Seal so the runner-detection vars (GITHUB_ACTIONS / RUNNER_ENVIRONMENT)
        // come ONLY from `env`; an empty list must read as "not on a runner",
        // never inherit a GitHub-hosted/self-hosted host's ambient values.
        .sealed_env();
    for (k, v) in env {
        b = b.env(k, v);
    }
    (tmp, b.build())
}

#[test]
fn provenance_emitted_true_when_runner_supports() {
    // No GitHub Actions env → provenance left as configured (true).
    let (_tmp, ctx) = npm_ctx_with_env(vec![]);
    let cfg = npm_cfg();
    let (log, cap) = anodizer_core::log::StageLogger::with_capture(
        "publish",
        anodizer_core::log::Verbosity::Normal,
    );
    let override_ = effective_provenance_override(&ctx, &cfg, "anodize-demo", &log);
    assert_eq!(override_, None, "supported runner emits configured value");

    let body = render_package_json(&ctx, &cfg, "anodize-demo", "demo", "1.0.0", &[], override_)
        .expect("render");
    let j: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(
        j["publishConfig"]["provenance"],
        serde_json::Value::Bool(true)
    );
    assert!(
        cap.warn_messages().is_empty(),
        "no warning on a supported runner"
    );
}

#[test]
fn provenance_degraded_false_with_warning_on_self_hosted_runner() {
    // GitHub Actions self-hosted + provenance requested (default) → degrade to
    // false and warn (the live E422 case this fix addresses).
    let (_tmp, ctx) = npm_ctx_with_env(vec![
        ("GITHUB_ACTIONS", "true"),
        ("RUNNER_ENVIRONMENT", "self-hosted"),
    ]);
    let cfg = npm_cfg();
    let (log, cap) = anodizer_core::log::StageLogger::with_capture(
        "publish",
        anodizer_core::log::Verbosity::Normal,
    );
    let override_ = effective_provenance_override(&ctx, &cfg, "anodize-demo", &log);
    assert_eq!(
        override_,
        Some(false),
        "self-hosted runner forces provenance off"
    );

    let body = render_package_json(&ctx, &cfg, "anodize-demo", "demo", "1.0.0", &[], override_)
        .expect("render");
    let j: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(
        j["publishConfig"]["provenance"],
        serde_json::Value::Bool(false)
    );

    let warns = cap.warn_messages().join("\n");
    assert!(
        warns.contains("RUNNER_ENVIRONMENT=self-hosted")
            && warns.contains("WITHOUT provenance")
            && warns.contains("anodize-demo")
            && warns.contains("GitHub-hosted runner"),
        "warning must be actionable and name the package + the constraint: {warns}"
    );
}

#[test]
fn provenance_explicit_false_stays_false_without_spurious_warning() {
    // Explicit `provenance: false` on a self-hosted runner must NOT warn — the
    // operator already opted out, there is nothing to degrade.
    let (_tmp, ctx) = npm_ctx_with_env(vec![
        ("GITHUB_ACTIONS", "true"),
        ("RUNNER_ENVIRONMENT", "self-hosted"),
    ]);
    let cfg = NpmConfig {
        provenance: Some(false),
        ..npm_cfg()
    };
    let (log, cap) = anodizer_core::log::StageLogger::with_capture(
        "publish",
        anodizer_core::log::Verbosity::Normal,
    );
    let override_ = effective_provenance_override(&ctx, &cfg, "anodize-demo", &log);
    assert_eq!(override_, None, "explicit false needs no override");

    let body = render_package_json(&ctx, &cfg, "anodize-demo", "demo", "1.0.0", &[], override_)
        .expect("render");
    let j: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(
        j["publishConfig"]["provenance"],
        serde_json::Value::Bool(false)
    );
    assert!(
        cap.warn_messages().is_empty(),
        "explicit provenance:false must not warn on any runner"
    );
}

#[test]
fn optional_deps_provenance_degraded_uniformly_on_self_hosted() {
    // The gate applies to the whole optional-deps set (per-platform + meta) so
    // workspace per-crate / lockstep publishes degrade consistently.
    let (_tmp, ctx) = {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let mut ctx = TestContextBuilder::new()
            .project_name("demo")
            .tag("v1.2.3")
            .crates(vec![demo_crate()])
            .env("GITHUB_ACTIONS", "true")
            .env("RUNNER_ENVIRONMENT", "self-hosted")
            .build();
        add_binary(&mut ctx, tmp.path(), "x86_64-unknown-linux-gnu", "demo");
        add_binary(&mut ctx, tmp.path(), "x86_64-apple-darwin", "demo");
        (tmp, ctx)
    };
    let cfg = opt_cfg();
    let log = ctx.logger("publish");
    let override_ = effective_provenance_override(&ctx, &cfg, "demo", &log);
    assert_eq!(override_, Some(false));

    let layout = generate_layout(&ctx, &cfg, "demo", "1.2.3", override_, &log).expect("layout");
    let meta: serde_json::Value = serde_json::from_str(&layout.metapackage_json).expect("json");
    assert_eq!(
        meta["publishConfig"]["provenance"],
        serde_json::Value::Bool(false),
        "metapackage degraded"
    );
    for p in &layout.platforms {
        let j: serde_json::Value = serde_json::from_str(&p.package_json).expect("json");
        assert_eq!(
            j["publishConfig"]["provenance"],
            serde_json::Value::Bool(false),
            "platform {} degraded",
            p.name
        );
    }
}

#[test]
fn files_allowlist_derived_per_package() {
    let (_tmp, ctx) = optional_deps_ctx();
    let layout = generate_layout(
        &ctx,
        &opt_cfg(),
        "demo",
        "1.0.0",
        None,
        &ctx.logger("publish"),
    )
    .expect("layout");

    // Metapackage ships the shim + the default README*/LICENSE* extra_files.
    let meta_j: serde_json::Value = serde_json::from_str(&layout.metapackage_json).expect("json");
    let mfiles = meta_j["files"].as_array().expect("files array");
    let mfiles: Vec<&str> = mfiles.iter().map(|v| v.as_str().unwrap()).collect();
    assert!(mfiles.contains(&"shim.js"), "{mfiles:?}");
    assert!(mfiles.contains(&"README*"), "{mfiles:?}");
    assert!(mfiles.contains(&"LICENSE*"), "{mfiles:?}");

    // Per-platform package ships its binary (basename `demo` or `demo.exe`).
    let win = layout
        .platforms
        .iter()
        .find(|p| p.name == "@anodize/demo-win32-x64")
        .expect("win pkg");
    let j: serde_json::Value = serde_json::from_str(&win.package_json).expect("json");
    let wfiles: Vec<&str> = j["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(wfiles.contains(&"demo.exe"), "{wfiles:?}");
}

#[test]
fn files_explicit_override_and_empty_suppresses() {
    let (_tmp, ctx) = optional_deps_ctx();
    let cfg = NpmConfig {
        files: Some(vec!["only-this".to_string()]),
        ..opt_cfg()
    };
    let layout =
        generate_layout(&ctx, &cfg, "demo", "1.0.0", None, &ctx.logger("publish")).expect("layout");
    let j: serde_json::Value = serde_json::from_str(&layout.metapackage_json).expect("json");
    assert_eq!(j["files"], serde_json::json!(["only-this"]));

    let cfg_empty = NpmConfig {
        files: Some(vec![]),
        ..opt_cfg()
    };
    let l2 = generate_layout(
        &ctx,
        &cfg_empty,
        "demo",
        "1.0.0",
        None,
        &ctx.logger("publish"),
    )
    .expect("layout");
    let j2: serde_json::Value = serde_json::from_str(&l2.metapackage_json).expect("json");
    assert!(
        j2.get("files").is_none(),
        "empty files list suppresses field"
    );
}

#[test]
fn postinstall_js_uses_one_consistent_function_name_no_referenceerror() {
    let body = render_postinstall_js("@anodize/demo");
    // The redirect-follow function is `go`; the call site must invoke `go`,
    // never the historical `follow` (which threw ReferenceError on install).
    assert!(body.contains("function go("), "defines go()");
    assert!(body.contains("go(url, 5)"), "invokes go(url, 5)");
    assert!(
        !body.contains("follow("),
        "must not reference the undefined `follow` — that was the install-time ReferenceError"
    );
}

/// Run `node --check` on the generated postinstall.js (syntax) AND a tiny
/// harness that stubs `https`/`fs`/`crypto` and confirms the redirect function
/// is actually callable end-to-end (no ReferenceError). Skipped only when
/// `node` is not on PATH.
#[test]
fn postinstall_js_executes_redirect_without_referenceerror() {
    if std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("node not on PATH; skipping postinstall.js runtime check");
        return;
    }
    let tmp = tempfile::tempdir().expect("tmp");
    let script = render_postinstall_js("@anodize/demo");
    let script_path = tmp.path().join("postinstall.js");
    std::fs::write(&script_path, &script).expect("write script");

    // 1) Syntax check: node --check must accept the file.
    let check = std::process::Command::new("node")
        .arg("--check")
        .arg(&script_path)
        .output()
        .expect("spawn node --check");
    assert!(
        check.status.success(),
        "node --check failed: {}",
        String::from_utf8_lossy(&check.stderr)
    );

    // 2) Runtime check: a harness that pre-populates the module cache with a
    //    fake `package.json` (one matching binary for THIS platform), a fake
    //    `https` that emits a 302 redirect then a 200 body, and stubs `fs`'s
    //    write path. If the redirect-follow call referenced an undefined
    //    symbol the async download would reject with a ReferenceError; the
    //    harness asserts the script completes (exit 0).
    let harness = format!(
        r#"
const Module = require('module');
const path = require('path');
const os = require('os');
const realFs = require('fs');
const scriptPath = {script_path:?};
const dir = path.dirname(scriptPath);

// Fake package.json with one binary entry matching this runtime.
const fakePkg = {{ anodize: {{ binaries: [
  {{ os: process.platform, cpu: process.arch,
     url: 'https://example.invalid/a.bin', sha256: '', format: 'binary' }}
] }} }};

// 302 redirect then 200 — exercises the redirect-follow path that used to
// throw ReferenceError.
let hop = 0;
const fakeHttps = {{
  get(u, cb) {{
    const res = {{
      on() {{ return res; }},
      pipe(stream) {{ if (stream && stream.__finish) stream.__finish(); }},
    }};
    if (hop++ === 0) {{
      res.statusCode = 302;
      res.headers = {{ location: 'https://example.invalid/final.bin' }};
    }} else {{
      res.statusCode = 200;
      res.headers = {{}};
    }}
    process.nextTick(() => cb(res));
    return {{ on() {{ return this; }} }};
  }}
}};

const fakeFs = Object.assign({{}}, realFs, {{
  createWriteStream() {{
    const handlers = {{}};
    return {{
      __finish() {{ if (handlers.finish) handlers.finish(); }},
      on(ev, h) {{ handlers[ev] = h; return this; }},
      close(cb) {{ if (cb) cb(); }},
    }};
  }},
  mkdirSync() {{}},
  readFileSync() {{ return Buffer.from(''); }},
  copyFileSync() {{}},
  unlinkSync() {{}},
  chmodSync() {{}},
}});

const origResolve = Module._resolveFilename;
Module._resolveFilename = function (request, parent, isMain, opts) {{
  if (request === 'https') return 'https';
  if (request === 'fs') return 'fs';
  if (request === './package.json' || request.endsWith('/package.json'))
    return path.join(dir, '__fake_pkg.json');
  return origResolve.call(this, request, parent, isMain, opts);
}};
require.cache['https'] = {{ id: 'https', exports: fakeHttps, loaded: true }};
require.cache['fs'] = {{ id: 'fs', exports: fakeFs, loaded: true }};
require.cache[path.join(dir, '__fake_pkg.json')] =
  {{ id: 'pkg', exports: fakePkg, loaded: true }};

process.on('exit', (code) => {{
  if (code !== 0) {{ console.error('postinstall exited', code); }}
}});

require(scriptPath);
"#,
        script_path = script_path.to_string_lossy(),
    );
    let harness_path = tmp.path().join("harness.js");
    std::fs::write(&harness_path, harness).expect("write harness");
    let run = std::process::Command::new("node")
        .arg(&harness_path)
        .output()
        .expect("spawn node harness");
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        !stderr.contains("ReferenceError"),
        "postinstall.js threw a ReferenceError at runtime:\n{stderr}"
    );
    assert!(
        run.status.success(),
        "postinstall harness exited non-zero:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&run.stdout),
        stderr
    );
}
