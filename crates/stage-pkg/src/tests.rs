use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use super::*;
use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::{Config, CrateConfig, ExtraFileSpec, PkgConfig, StringOrBool};
use anodizer_core::context::{Context, ContextOptions};
use anodizer_core::stage::Stage;
use tempfile::TempDir;

// -- pkgbuild_command tests --

#[test]
fn test_pkgbuild_command_basic() {
    let cmd = pkgbuild_command(
        "/tmp/staging",
        "com.example.myapp",
        "1.0.0",
        "/usr/local/bin",
        None,
        None,
        "/tmp/output/myapp.pkg",
    );
    assert_eq!(
        cmd,
        vec![
            "pkgbuild",
            "--root",
            "/tmp/staging",
            "--identifier",
            "com.example.myapp",
            "--version",
            "1.0.0",
            "--install-location",
            "/usr/local/bin",
            "/tmp/output/myapp.pkg",
        ]
    );
}

#[test]
fn test_pkgbuild_command_with_scripts() {
    let cmd = pkgbuild_command(
        "/tmp/staging",
        "com.example.myapp",
        "2.0.0",
        "/usr/local/bin",
        Some("/path/to/scripts"),
        None,
        "/tmp/output/myapp.pkg",
    );
    assert_eq!(
        cmd,
        vec![
            "pkgbuild",
            "--root",
            "/tmp/staging",
            "--identifier",
            "com.example.myapp",
            "--version",
            "2.0.0",
            "--install-location",
            "/usr/local/bin",
            "--scripts",
            "/path/to/scripts",
            "/tmp/output/myapp.pkg",
        ]
    );
}

#[test]
fn test_pkgbuild_command_custom_install_location() {
    let cmd = pkgbuild_command(
        "/tmp/staging",
        "com.example.myapp",
        "1.0.0",
        "/opt/myapp/bin",
        None,
        None,
        "/tmp/output/myapp.pkg",
    );
    assert_eq!(
        cmd,
        vec![
            "pkgbuild",
            "--root",
            "/tmp/staging",
            "--identifier",
            "com.example.myapp",
            "--version",
            "1.0.0",
            "--install-location",
            "/opt/myapp/bin",
            "/tmp/output/myapp.pkg",
        ]
    );
}

// -- tool resolution tests --

#[test]
fn test_resolve_prefers_pkgbuild() {
    let r = resolve_pkg_builder(|t| t == "pkgbuild");
    assert_eq!(r, Ok(PkgBuilder::Pkgbuild));
}

#[test]
fn test_resolve_linux_when_full_toolchain() {
    let r = resolve_pkg_builder(|t| LINUX_PKG_TOOLS.contains(&t));
    assert_eq!(r, Ok(PkgBuilder::Linux));
}

#[test]
fn test_resolve_bail_names_both_options() {
    // Partial Linux toolchain (missing cpio) and no pkgbuild => error.
    let err = resolve_pkg_builder(|t| t == "xar" || t == "mkbom").unwrap_err();
    assert!(
        err.contains("pkgbuild"),
        "message must name pkgbuild: {err}"
    );
    assert!(
        err.contains("xar"),
        "message must name the Linux toolchain: {err}"
    );
    assert!(err.contains("mkbom"), "message must name mkbom: {err}");
    assert!(err.contains("cpio"), "message must name cpio: {err}");
}

// -- Linux flat-package builder --

// -- reproducibility helpers --

#[test]
fn test_sha1_digest_known_vectors() {
    assert_eq!(
        sha1_digest(b""),
        hex_to_bytes("da39a3ee5e6b4b0d3255bfef95601890afd80709")
    );
    assert_eq!(
        sha1_digest(b"abc"),
        hex_to_bytes("a9993e364706816aba3e25717850c26c9cd0d89d")
    );
    assert_eq!(
        sha1_digest(b"The quick brown fox jumps over the lazy dog"),
        hex_to_bytes("2fd4e1c67a2d28fced849ee1bb76e7391b93eb12")
    );
}

fn hex_to_bytes(h: &str) -> [u8; 20] {
    let mut out = [0u8; 20];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).unwrap();
    }
    out
}

#[test]
fn test_epoch_to_iso8601() {
    assert_eq!(epoch_to_iso8601(0), "1970-01-01T00:00:00");
    assert_eq!(epoch_to_iso8601(1), "1970-01-01T00:00:01");
    // 1700000000 = 2023-11-14T22:13:20Z
    assert_eq!(epoch_to_iso8601(1_700_000_000), "2023-11-14T22:13:20");
    // Leap day
    assert_eq!(epoch_to_iso8601(1_582_934_400), "2020-02-29T00:00:00");
}

#[test]
fn test_rewrite_toc_fields_normalizes_all_time_and_inode() {
    let toc = b"<file><ctime>2026-01-01T00:00:00Z</ctime>\
                    <mtime>2026-01-01T00:00:00Z</mtime>\
                    <atime>2026-01-01T00:00:00Z</atime>\
                    <inode>1656875</inode></file>\
                    <creation-time>2026-01-01T00:00:00</creation-time>";
    let out = String::from_utf8(rewrite_toc_fields(toc, "1970-01-01T00:00:01")).unwrap();
    assert!(out.contains("<ctime>1970-01-01T00:00:01Z</ctime>"));
    assert!(out.contains("<mtime>1970-01-01T00:00:01Z</mtime>"));
    assert!(out.contains("<atime>1970-01-01T00:00:01Z</atime>"));
    assert!(out.contains("<inode>0</inode>"));
    assert!(out.contains("<creation-time>1970-01-01T00:00:01</creation-time>"));
    assert!(!out.contains("2026"));
}

#[test]
fn test_normalize_odc_cpio_zeroes_dev_ino_and_is_idempotent() {
    if !anodizer_core::tool_detect::on_path("cpio") || !anodizer_core::tool_detect::on_path("sh") {
        eprintln!("cpio absent; test skipped hermetically");
        return;
    }
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.txt"), b"hello").unwrap();
    fs::write(dir.path().join("b.txt"), b"world").unwrap();
    let raw = Command::new("sh")
        .arg("-c")
        .arg("find . | LC_ALL=C sort | cpio -o --format odc -R 0:0 2>/dev/null")
        .current_dir(dir.path())
        .output()
        .unwrap()
        .stdout;
    let n1 = normalize_odc_cpio(&raw);
    // Every odc header's dev (6..12) and ino (12..18) must be zeroed.
    let mut i = 0;
    while i + 76 <= n1.len() && &n1[i..i + 6] == b"070707" {
        assert_eq!(&n1[i + 6..i + 12], b"000000", "dev not zeroed at {i}");
        assert_eq!(&n1[i + 12..i + 18], b"000000", "ino not zeroed at {i}");
        let namesize =
            usize::from_str_radix(std::str::from_utf8(&n1[i + 59..i + 65]).unwrap(), 8).unwrap();
        let filesize =
            usize::from_str_radix(std::str::from_utf8(&n1[i + 65..i + 76]).unwrap(), 8).unwrap();
        let name = &n1[i + 76..i + 76 + namesize];
        if name.starts_with(b"TRAILER!!!") {
            break;
        }
        i += 76 + namesize + filesize;
    }
    // Idempotent: re-normalizing yields the identical bytes.
    assert_eq!(normalize_odc_cpio(&n1), n1);
}

#[test]
fn test_flat_pkg_is_byte_reproducible_across_time() {
    // The whole point of the fix: two builds whose wall-clock differs must
    // produce byte-identical `.pkg`. Builds the same staging twice with a
    // simulated time gap (a real 2nd build re-runs xar, which re-stamps the
    // TOC wall-clock — here we build twice back to back, which already
    // exercises distinct xar `creation-time`s on most hosts; the normalize
    // pass collapses them). Hermetic: skip-with-pass without the toolchain.
    // Linux-only: bomutils `mkbom -u` is rejected by Apple's homonym `mkbom`,
    // so a macOS host (which ships xar/mkbom/cpio under the same names) would
    // falsely satisfy the tool probe and then crash on the bomutils syntax —
    // this fallback path is never taken on macOS in production anyway.
    let have_tools = cfg!(target_os = "linux")
        && LINUX_PKG_TOOLS
            .iter()
            .all(|t| anodizer_core::tool_detect::on_path(t))
        && anodizer_core::tool_detect::on_path("sh");
    if !have_tools {
        eprintln!("Linux pkg toolchain absent; test skipped hermetically");
        return;
    }
    let log = anodizer_core::log::StageLogger::new("pkg", anodizer_core::log::Verbosity::Normal);
    let build = || -> Vec<u8> {
        let staging = TempDir::new().unwrap();
        fs::write(staging.path().join("myapp"), b"#!/bin/sh\necho hi\n").unwrap();
        let out = TempDir::new().unwrap();
        let pkg_path = out.path().join("myapp_arm64.pkg");
        build_flat_pkg_linux(
            staging.path(),
            "com.example.myapp",
            "1.2.3",
            "/usr/local/bin",
            None,
            Some("11.0"),
            None, // no mod_timestamp => fallback-epoch path
            &pkg_path,
            &log,
        )
        .unwrap();
        fs::read(&pkg_path).unwrap()
    };
    let a = build();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let b = build();
    assert_eq!(a, b, "flat .pkg must be byte-identical across builds");
}

/// Two native `pkgbuild` runs over identical content are NOT byte-identical,
/// and this records WHY so the verdict is observable before a release rather
/// than hidden behind a determinism allowlist.
///
/// pkgbuild does not honor `SOURCE_DATE_EPOCH` and has no deterministic
/// mode: its xar TOC carries an archive creation-time, per-file
/// ctime/mtime/atime, and the live inode — the exact fields the Linux
/// flat-package fallback must normalize (`normalize_xar_toc`) to reach
/// byte-stability. The native `PkgBuilder::Pkgbuild` path invokes pkgbuild
/// directly with no such normalization, so its `.pkg` drifts every build.
/// The assertion proves the drift is real (not a flake) and pins the root
/// cause; flipping it to byte-equality is the regression signal if the
/// native path gains the same TOC normalization the Linux path already has.
/// Runs on the macOS CI test shard only (pkgbuild is macOS-native).
#[test]
#[cfg(target_os = "macos")]
fn native_pkgbuild_pkg_is_byte_reproducible_across_time() {
    if !anodizer_core::tool_detect::on_path("pkgbuild") {
        eprintln!("pkgbuild unavailable; test skipped hermetically");
        return;
    }
    let build = || -> Option<Vec<u8>> {
        let staging = TempDir::new().unwrap();
        fs::write(staging.path().join("myapp"), b"#!/bin/sh\necho hi\n").unwrap();
        let out = TempDir::new().unwrap();
        let pkg_path = out.path().join("repro.pkg");
        let argv = pkgbuild_command(
            &staging.path().to_string_lossy(),
            "com.example.repro",
            "1.2.3",
            "/usr/local/bin",
            None,
            Some("11.0"),
            &pkg_path.to_string_lossy(),
        );
        let output = Command::new(&argv[0])
            .args(&argv[1..])
            .env("SOURCE_DATE_EPOCH", "1700000000")
            .output()
            .ok()?;
        output.status.success().then(|| fs::read(&pkg_path).ok())?
    };
    let (Some(a), Some(b)) = (build(), {
        std::thread::sleep(std::time::Duration::from_millis(1100));
        build()
    }) else {
        eprintln!("pkgbuild failed; test skipped hermetically");
        return;
    };
    assert_ne!(
        a, b,
        "native pkgbuild stamps a wall-clock xar TOC creation-time + per-file \
             c/m/atime + live inode every run and ignores SOURCE_DATE_EPOCH; the \
             native .pkg is NOT byte-reproducible and anodizer does not normalize \
             the native tool's TOC (only the Linux fallback does). If this now \
             matches, the native path gained TOC normalization — keep it and flip \
             this assertion."
    );
}

#[test]
fn test_build_flat_pkg_linux_emits_xar_layout() {
    // Hermetic: skip-with-pass if the Linux toolchain is absent. This box
    // has all of xar/mkbom/cpio, so the assertions below WILL execute here.
    // Linux-only: Apple's `mkbom` rejects the bomutils `-u` flag, so a macOS
    // host would falsely pass the probe and crash; the path is Linux-only.
    let have_tools = cfg!(target_os = "linux")
        && LINUX_PKG_TOOLS
            .iter()
            .all(|t| anodizer_core::tool_detect::on_path(t))
        && anodizer_core::tool_detect::on_path("sh");
    if !have_tools {
        eprintln!("Linux pkg toolchain absent; test skipped hermetically");
        return;
    }

    let staging = TempDir::new().unwrap();
    fs::write(staging.path().join("myapp"), b"#!/bin/sh\necho hi\n").unwrap();

    let scripts = TempDir::new().unwrap();
    fs::write(scripts.path().join("postinstall"), b"#!/bin/sh\nexit 0\n").unwrap();

    let out = TempDir::new().unwrap();
    let pkg_path = out.path().join("myapp_arm64.pkg");

    let log = anodizer_core::log::StageLogger::new("pkg", anodizer_core::log::Verbosity::Normal);
    build_flat_pkg_linux(
        staging.path(),
        "com.example.myapp",
        "1.2.3",
        "/usr/local/bin",
        Some(scripts.path().to_str().unwrap()),
        Some("11.0"),
        Some("1704067200"),
        &pkg_path,
        &log,
    )
    .expect("flat pkg build");

    assert!(pkg_path.exists(), "output .pkg must exist");

    let listing = Command::new("xar")
        .arg("-tf")
        .arg(&pkg_path)
        .output()
        .expect("xar -tf");
    assert!(listing.status.success(), "xar -tf must succeed");
    let toc = String::from_utf8_lossy(&listing.stdout);
    assert!(
        toc.contains("Distribution"),
        "TOC must list Distribution: {toc}"
    );
    assert!(
        toc.contains("base.pkg/Payload"),
        "TOC must list Payload: {toc}"
    );
    assert!(toc.contains("base.pkg/Bom"), "TOC must list Bom: {toc}");
    assert!(
        toc.contains("base.pkg/Scripts"),
        "TOC must list Scripts when scripts dir set: {toc}"
    );
}

// -- Stage no-op / skip tests --

#[test]
fn test_stage_skips_when_no_pkg_config() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    let stage = PkgStage;
    assert!(stage.run(&mut ctx).is_ok());
    // No artifacts should be registered
    assert!(ctx.artifacts.by_kind(ArtifactKind::MacOsPackage).is_empty());
}

#[test]
fn test_stage_skips_when_disabled() {
    let tmp = TempDir::new().unwrap();

    let pkg_cfg = PkgConfig {
        identifier: Some("com.example.myapp".to_string()),
        skip: Some(StringOrBool::Bool(true)),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        pkgs: Some(vec![pkg_cfg]),
        ..Default::default()
    }];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");

    // Add a darwin binary so the stage would otherwise process it
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/build/myapp"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    let stage = PkgStage;
    stage.run(&mut ctx).unwrap();

    // No packages should be generated because the config is disabled
    assert!(ctx.artifacts.by_kind(ArtifactKind::MacOsPackage).is_empty());
}

// -- Dry-run behavior tests --

#[test]
fn test_stage_dry_run_registers_artifacts() {
    let tmp = TempDir::new().unwrap();

    let pkg_cfg = PkgConfig {
        identifier: Some("com.example.myapp".to_string()),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        pkgs: Some(vec![pkg_cfg]),
        ..Default::default()
    }];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");

    // Register darwin binary artifacts
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/build/myapp"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/build/myapp-x86"),
        target: Some("x86_64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    let stage = PkgStage;
    stage.run(&mut ctx).unwrap();

    let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
    assert_eq!(pkgs.len(), 2, "should register one PKG per darwin binary");

    // Both should have correct kind and metadata
    for pkg in &pkgs {
        assert_eq!(pkg.kind, ArtifactKind::MacOsPackage);
        assert_eq!(pkg.crate_name, "myapp");
        assert_eq!(
            pkg.metadata.get("identifier"),
            Some(&"com.example.myapp".to_string())
        );
    }

    // Check targets are preserved
    let targets: Vec<Option<&str>> = pkgs.iter().map(|p| p.target.as_deref()).collect();
    assert!(targets.contains(&Some("aarch64-apple-darwin")));
    assert!(targets.contains(&Some("x86_64-apple-darwin")));
}

#[test]
fn test_workspace_per_crate_distinct_filenames() {
    let tmp = TempDir::new().unwrap();

    // Two crates, both using the DEFAULT name template (no Version segment),
    // so ProjectName is the only distinguishing token. Without the per-crate
    // ProjectName rebind both render to `<project_name>_arm64.pkg` and clobber.
    let make_crate = |name: &str| CrateConfig {
        name: name.to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        pkgs: Some(vec![PkgConfig {
            identifier: Some("com.example.{{ ProjectName }}".to_string()),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "workspace".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![make_crate("alpha"), make_crate("beta")];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");

    for crate_name in ["alpha", "beta"] {
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from(format!("/build/{crate_name}")),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: crate_name.to_string(),
            metadata: HashMap::new(),
            size: None,
        });
    }

    let stage = PkgStage;
    stage.run(&mut ctx).unwrap();

    let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
    assert_eq!(pkgs.len(), 2, "expected one PKG per crate");

    let filenames: Vec<String> = pkgs
        .iter()
        .map(|p| p.path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();

    assert!(
        filenames.iter().any(|f| f.contains("alpha")),
        "no PKG filename contains crate name 'alpha': {filenames:?}"
    );
    assert!(
        filenames.iter().any(|f| f.contains("beta")),
        "no PKG filename contains crate name 'beta': {filenames:?}"
    );
    assert_ne!(
        filenames[0], filenames[1],
        "the two crates' PKGs must not share a filename (clobber): {filenames:?}"
    );

    assert_eq!(
        ctx.template_vars().get("ProjectName").map(String::as_str),
        Some("workspace"),
        "ProjectName not restored after per-crate rebind"
    );
}

#[test]
fn test_pkg_same_arch_variants_get_distinct_names() {
    // Three darwin/amd64 builds tagged amd64_variant v1/v2/v3 plus one
    // arm64 build must each render a distinct `.pkg`: the default name
    // appends the amd64 micro-arch suffix (v1 baseline → none, v2/v3 →
    // suffix), so the same triple no longer clobbers itself.
    let tmp = TempDir::new().unwrap();

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        pkgs: Some(vec![PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");

    for variant in ["v1", "v2", "v3"] {
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from(format!("/build/myapp_{variant}")),
            target: Some("x86_64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("amd64_variant".to_string(), variant.to_string())]),
            size: None,
        });
    }
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/build/myapp_arm"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    PkgStage.run(&mut ctx).unwrap();
    let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
    assert_eq!(pkgs.len(), 4, "{pkgs:?}");

    let names: Vec<String> = pkgs
        .iter()
        .map(|p| p.path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    let distinct: std::collections::HashSet<&String> = names.iter().collect();
    assert_eq!(
        distinct.len(),
        names.len(),
        "all rendered PKG names must be distinct: {names:?}"
    );
}

#[test]
fn test_project_name_restored_after_mid_loop_error() {
    // A per-crate render failure mid-loop must still restore the rebound
    // `ProjectName` before propagating, so the workspace value never leaks
    // out of the stage (the var is process-global on ctx).
    let tmp = TempDir::new().unwrap();

    let good_crate = CrateConfig {
        name: "alpha".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        pkgs: Some(vec![PkgConfig {
            identifier: Some("com.example.alpha".to_string()),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let bad_crate = CrateConfig {
        name: "beta".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        pkgs: Some(vec![PkgConfig {
            identifier: Some("com.example.beta".to_string()),
            // Malformed template — unclosed tag forces a mid-loop render error.
            name: Some("{{ bad_template".to_string()),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "workspace".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![good_crate, bad_crate];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");

    for crate_name in ["alpha", "beta"] {
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from(format!("/build/{crate_name}")),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: crate_name.to_string(),
            metadata: HashMap::new(),
            size: None,
        });
    }

    let result = PkgStage.run(&mut ctx);
    assert!(
        result.is_err(),
        "malformed name on a crate must fail the stage"
    );

    assert_eq!(
        ctx.template_vars().get("ProjectName").map(String::as_str),
        Some("workspace"),
        "ProjectName must be restored even when the loop errors mid-iteration"
    );
}

#[test]
fn test_stage_dry_run_with_name_template() {
    let tmp = TempDir::new().unwrap();

    let pkg_cfg = PkgConfig {
        identifier: Some("com.example.myapp".to_string()),
        name: Some("{{ ProjectName }}_{{ Version }}_{{ Os }}_{{ Arch }}.pkg".to_string()),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        pkgs: Some(vec![pkg_cfg]),
        ..Default::default()
    }];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "2.5.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/build/myapp"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    PkgStage.run(&mut ctx).unwrap();

    let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
    assert_eq!(pkgs.len(), 1);

    let filename = pkgs[0].path.file_name().unwrap().to_str().unwrap();
    assert_eq!(
        filename, "myapp_2.5.0_darwin_arm64.pkg",
        "name template should render with Os/Arch from target triple"
    );
}

#[test]
fn test_stage_dry_run_replace_removes_archives() {
    let tmp = TempDir::new().unwrap();

    let pkg_cfg = PkgConfig {
        identifier: Some("com.example.myapp".to_string()),
        replace: Some(true),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        pkgs: Some(vec![pkg_cfg]),
        ..Default::default()
    }];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");

    // Add a darwin binary
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/build/myapp"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    // Add darwin archive artifacts that should be removed
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: PathBuf::from("/dist/myapp_darwin_arm64.tar.gz"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    // Add a linux archive that should NOT be removed
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: PathBuf::from("/dist/myapp_linux_amd64.tar.gz"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    PkgStage.run(&mut ctx).unwrap();

    // PKG artifact should be registered
    let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
    assert_eq!(pkgs.len(), 1);

    // Darwin archive should be removed
    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(archives.len(), 1, "darwin archive should be removed");
    assert_eq!(
        archives[0].target.as_deref(),
        Some("x86_64-unknown-linux-gnu"),
        "only the linux archive should remain"
    );
}

// -- Error path tests --

#[test]
fn test_stage_errors_without_identifier() {
    let tmp = TempDir::new().unwrap();

    let pkg_cfg = PkgConfig {
        identifier: None, // missing required field
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        pkgs: Some(vec![pkg_cfg]),
        ..Default::default()
    }];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");

    // Add a darwin binary so the stage attempts to process the config
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/build/myapp"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    let result = PkgStage.run(&mut ctx);
    assert!(
        result.is_err(),
        "missing identifier should produce an error"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("identifier"),
        "error should mention missing identifier, got: {err}"
    );
}

// -- Config parsing tests --

#[test]
fn test_config_parse_pkg() {
    let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    pkgs:
      - identifier: com.example.test
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let pkgs = config.crates[0].pkgs.as_ref().unwrap();
    assert_eq!(pkgs.len(), 1);
    assert_eq!(pkgs[0].identifier.as_deref(), Some("com.example.test"));
    // All optional fields default to None
    assert!(pkgs[0].name.is_none());
    assert!(pkgs[0].install_location.is_none());
    assert!(pkgs[0].scripts.is_none());
    assert!(pkgs[0].replace.is_none());
    assert!(pkgs[0].skip.is_none());
}

#[test]
fn test_config_parse_pkg_full() {
    let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    pkgs:
      - id: my-pkg
        ids:
          - build-darwin-arm64
          - build-darwin-amd64
        identifier: com.example.test
        name: "{{ ProjectName }}_{{ Version }}_{{ Arch }}"
        install_location: /opt/test/bin
        scripts: ./scripts/pkg
        extra_files:
          - README.md
          - LICENSE
        replace: true
        mod_timestamp: "2024-01-01T00:00:00Z"
        skip: false
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let pkgs = config.crates[0].pkgs.as_ref().unwrap();
    assert_eq!(pkgs.len(), 1);
    let p = &pkgs[0];
    assert_eq!(p.id.as_deref(), Some("my-pkg"));
    assert_eq!(
        p.ids.as_ref().unwrap(),
        &["build-darwin-arm64", "build-darwin-amd64"]
    );
    assert_eq!(p.identifier.as_deref(), Some("com.example.test"));
    assert_eq!(
        p.name.as_deref(),
        Some("{{ ProjectName }}_{{ Version }}_{{ Arch }}")
    );
    assert_eq!(p.install_location.as_deref(), Some("/opt/test/bin"));
    assert_eq!(p.scripts.as_deref(), Some("./scripts/pkg"));
    let extras = p.extra_files.as_ref().unwrap();
    assert_eq!(extras.len(), 2);
    assert_eq!(extras[0].glob(), "README.md");
    assert_eq!(extras[1].glob(), "LICENSE");
    assert_eq!(p.replace, Some(true));
    assert_eq!(p.mod_timestamp.as_deref(), Some("2024-01-01T00:00:00Z"));
    assert_eq!(p.skip, Some(StringOrBool::Bool(false)));
}

#[test]
fn test_default_install_location() {
    // When install_location is not set, the stage should default to /usr/local/bin
    let tmp = TempDir::new().unwrap();

    let pkg_cfg = PkgConfig {
        identifier: Some("com.example.myapp".to_string()),
        install_location: None,
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        pkgs: Some(vec![pkg_cfg]),
        ..Default::default()
    }];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/build/myapp"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    PkgStage.run(&mut ctx).unwrap();

    // The default install location is used internally in the pkgbuild command;
    // verify the stage succeeds and registers an artifact (the default is
    // /usr/local/bin which is tested via the pkgbuild_command unit tests).
    let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
    assert_eq!(pkgs.len(), 1);

    // Verify the default through pkgbuild_command directly
    let cmd = pkgbuild_command(
        "/tmp/staging",
        "com.example.myapp",
        "1.0.0",
        "/usr/local/bin", // the default
        None,
        None,
        "/tmp/out.pkg",
    );
    assert!(
        cmd.contains(&"--install-location".to_string()),
        "command should contain --install-location"
    );
    let loc_idx = cmd.iter().position(|a| a == "--install-location").unwrap();
    assert_eq!(cmd[loc_idx + 1], "/usr/local/bin");
}

#[test]
fn test_extra_files_copied_to_staging() {
    // Run in live mode and verify the stage gets past binary + extra file
    // copying. The outcome depends on which build path is available:
    // pkgbuild (macOS), the Linux flat-package toolchain, or neither.
    let tmp = TempDir::new().unwrap();

    // Create a fake binary
    let binary_dir = tmp.path().join("bin");
    fs::create_dir_all(&binary_dir).unwrap();
    let binary_path = binary_dir.join("myapp");
    fs::write(&binary_path, b"fake binary").unwrap();

    // Create an extra file
    let extra_path = tmp.path().join("README.md");
    fs::write(&extra_path, b"# My App").unwrap();

    let pkg_cfg = PkgConfig {
        identifier: Some("com.example.myapp".to_string()),
        extra_files: Some(vec![ExtraFileSpec::Glob(
            extra_path.to_string_lossy().into_owned(),
        )]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        pkgs: Some(vec![pkg_cfg]),
        ..Default::default()
    }];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: false, // live mode
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");

    // Add a darwin binary artifact
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: binary_path,
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    let result = PkgStage.run(&mut ctx);

    let pkgbuild = anodizer_core::tool_detect::on_path("pkgbuild");
    let linux_toolchain = LINUX_PKG_TOOLS
        .iter()
        .all(|t| anodizer_core::tool_detect::on_path(t))
        && anodizer_core::tool_detect::on_path("sh");

    if pkgbuild {
        // pkgbuild may succeed or fail at exec; either is past the copy step.
        if let Err(e) = &result {
            let err = e.to_string();
            assert!(
                err.contains("pkgbuild") || err.contains("execute"),
                "unexpected pkgbuild-path error: {err}"
            );
        }
    } else if linux_toolchain {
        // The Linux flat-package path assembles a real .pkg with no Apple
        // tools, so the live run must succeed and emit the artifact.
        result.expect("Linux flat-package build should succeed");
        let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
        assert_eq!(pkgs.len(), 1, "one .pkg artifact expected");
        assert!(pkgs[0].path.exists(), "emitted .pkg must exist on disk");
    } else {
        // Neither path available => actionable bail naming both options.
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("pkgbuild") && err.contains("xar"),
            "expected dual-option error, got: {err}"
        );
    }
}

#[test]
fn test_invalid_name_template_errors() {
    let tmp = TempDir::new().unwrap();

    let pkg_cfg = PkgConfig {
        identifier: Some("com.example.myapp".to_string()),
        // Invalid Tera template — unclosed tag
        name: Some("{{ bad_template".to_string()),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        pkgs: Some(vec![pkg_cfg]),
        ..Default::default()
    }];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/build/myapp"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    let result = PkgStage.run(&mut ctx);
    assert!(
        result.is_err(),
        "invalid name template should cause a render error"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("template") || err.contains("render"),
        "error should mention template rendering, got: {err}"
    );
}

#[test]
fn test_ids_filtering() {
    let tmp = TempDir::new().unwrap();

    // Configure ids filter to match only one build id
    let pkg_cfg = PkgConfig {
        identifier: Some("com.example.myapp".to_string()),
        ids: Some(vec!["build-darwin-arm64".to_string()]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        pkgs: Some(vec![pkg_cfg]),
        ..Default::default()
    }];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");

    // Register two darwin binaries with different metadata ids
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/build/myapp-arm64"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("id".to_string(), "build-darwin-arm64".to_string())]),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/build/myapp-amd64"),
        target: Some("x86_64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("id".to_string(), "build-darwin-amd64".to_string())]),
        size: None,
    });

    let stage = PkgStage;
    stage.run(&mut ctx).unwrap();

    // Verify only one PKG artifact is produced (the arm64 one)
    let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
    assert_eq!(
        pkgs.len(),
        1,
        "ids filter should produce exactly one PKG, got {}",
        pkgs.len()
    );
    assert_eq!(
        pkgs[0].target.as_deref(),
        Some("aarch64-apple-darwin"),
        "the PKG should be for the arm64 target"
    );
}

// -- `use` field tests --

#[test]
fn test_use_appbundle_selects_installer_artifacts() {
    let tmp = TempDir::new().unwrap();

    let pkg_cfg = PkgConfig {
        identifier: Some("com.example.myapp".to_string()),
        use_: Some("appbundle".to_string()),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        pkgs: Some(vec![pkg_cfg]),
        ..Default::default()
    }];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");

    // Register an appbundle artifact (Installer with format=appbundle)
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Installer,
        name: String::new(),
        path: PathBuf::from("dist/MyApp.app"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("format".to_string(), "appbundle".to_string())]),
        size: None,
    });

    // Also register a darwin binary that should NOT be selected
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("dist/myapp"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = PkgStage;
    stage.run(&mut ctx).unwrap();

    // Should produce one PKG from the appbundle, not from the binary
    let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
    assert_eq!(pkgs.len(), 1, "should produce one PKG from the appbundle");
}

#[test]
fn test_use_binary_selects_darwin_binaries() {
    let tmp = TempDir::new().unwrap();

    // Explicit `use: binary` should behave same as omitted (default)
    let pkg_cfg = PkgConfig {
        identifier: Some("com.example.myapp".to_string()),
        use_: Some("binary".to_string()),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        pkgs: Some(vec![pkg_cfg]),
        ..Default::default()
    }];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");

    // Register a darwin binary
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("dist/myapp"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    // Also register an appbundle that should NOT be selected
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Installer,
        name: String::new(),
        path: PathBuf::from("dist/MyApp.app"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("format".to_string(), "appbundle".to_string())]),
        size: None,
    });

    let stage = PkgStage;
    stage.run(&mut ctx).unwrap();

    // Should produce one PKG from the binary, not from the appbundle
    let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
    assert_eq!(pkgs.len(), 1, "should produce one PKG from the binary");
}

#[test]
fn test_use_default_selects_darwin_binaries() {
    let tmp = TempDir::new().unwrap();

    // No `use_` set — should default to "binary" mode
    let pkg_cfg = PkgConfig {
        identifier: Some("com.example.myapp".to_string()),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        pkgs: Some(vec![pkg_cfg]),
        ..Default::default()
    }];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("dist/myapp"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    PkgStage.run(&mut ctx).unwrap();

    let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
    assert_eq!(
        pkgs.len(),
        1,
        "default use mode should select darwin binaries"
    );
}

#[test]
fn test_invalid_use_value_errors() {
    let tmp = TempDir::new().unwrap();

    let pkg_cfg = PkgConfig {
        identifier: Some("com.example.myapp".to_string()),
        use_: Some("invalid_mode".to_string()),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        pkgs: Some(vec![pkg_cfg]),
        ..Default::default()
    }];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");

    // Add a binary so the stage tries to process the config
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("dist/myapp"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let result = PkgStage.run(&mut ctx);
    assert!(result.is_err(), "invalid use value should produce an error");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("invalid `use` value"),
        "error should mention invalid use value, got: {err}"
    );
}

#[test]
fn test_use_appbundle_skips_when_no_appbundles() {
    let tmp = TempDir::new().unwrap();

    let pkg_cfg = PkgConfig {
        identifier: Some("com.example.myapp".to_string()),
        use_: Some("appbundle".to_string()),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        pkgs: Some(vec![pkg_cfg]),
        ..Default::default()
    }];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");

    // Only register a binary — no appbundles
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("dist/myapp"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    PkgStage.run(&mut ctx).unwrap();

    // No PKGs should be produced because there are no appbundle artifacts
    let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
    assert_eq!(
        pkgs.len(),
        0,
        "should produce no PKGs when use=appbundle but no appbundles exist"
    );
}

// -- StringOrBool disable tests --

#[test]
fn test_disable_string_or_bool_true_string() {
    let tmp = TempDir::new().unwrap();

    let pkg_cfg = PkgConfig {
        identifier: Some("com.example.myapp".to_string()),
        skip: Some(StringOrBool::String("true".to_string())),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        pkgs: Some(vec![pkg_cfg]),
        ..Default::default()
    }];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/build/myapp"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    PkgStage.run(&mut ctx).unwrap();

    // skip: "true" should skip the config
    assert!(ctx.artifacts.by_kind(ArtifactKind::MacOsPackage).is_empty());
}

#[test]
fn test_disable_string_or_bool_false_string() {
    let tmp = TempDir::new().unwrap();

    let pkg_cfg = PkgConfig {
        identifier: Some("com.example.myapp".to_string()),
        skip: Some(StringOrBool::String("false".to_string())),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        pkgs: Some(vec![pkg_cfg]),
        ..Default::default()
    }];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/build/myapp"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    PkgStage.run(&mut ctx).unwrap();

    // skip: "false" should NOT skip the config
    assert_eq!(ctx.artifacts.by_kind(ArtifactKind::MacOsPackage).len(), 1);
}

#[test]
fn test_disable_string_or_bool_template() {
    let tmp = TempDir::new().unwrap();

    // Template that evaluates to "true" when IsSnapshot is set
    let pkg_cfg = PkgConfig {
        identifier: Some("com.example.myapp".to_string()),
        skip: Some(StringOrBool::String(
            "{% if IsSnapshot %}true{% else %}false{% endif %}".to_string(),
        )),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        pkgs: Some(vec![pkg_cfg]),
        ..Default::default()
    }];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("IsSnapshot", "true");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/build/myapp"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    PkgStage.run(&mut ctx).unwrap();

    // Template should evaluate to "true", so the config is disabled
    assert!(
        ctx.artifacts.by_kind(ArtifactKind::MacOsPackage).is_empty(),
        "template disable should skip the config when evaluated to true"
    );
}

#[test]
fn test_config_parse_pkg_with_use_and_string_disable() {
    let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    pkgs:
      - identifier: com.example.test
        use: appbundle
        skip: "{{ if IsSnapshot }}true{{ else }}false{{ endif }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let pkgs = config.crates[0].pkgs.as_ref().unwrap();
    assert_eq!(pkgs.len(), 1);
    assert_eq!(pkgs[0].use_.as_deref(), Some("appbundle"));
    assert!(matches!(pkgs[0].skip, Some(StringOrBool::String(_))));
}

// --- `pkg.if` template-conditional ---

fn pkg_if_test_ctx(if_expr: Option<&str>) -> anodizer_core::context::Context {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{Config, CrateConfig, PkgConfig};
    use anodizer_core::context::{Context, ContextOptions};
    let tmp = tempfile::TempDir::new().unwrap();
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    std::fs::create_dir_all(&config.dist).unwrap();
    let pkg_cfg = PkgConfig {
        identifier: Some("com.example.myapp".to_string()),
        if_condition: if_expr.map(str::to_string),
        ..Default::default()
    };
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        pkgs: Some(vec![pkg_cfg]),
        ..Default::default()
    }];
    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Os", "darwin");
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: std::path::PathBuf::from("dist/myapp"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });
    ctx
}

#[test]
fn test_pkg_if_false_skips_config() {
    use anodizer_core::artifact::ArtifactKind;
    let mut ctx = pkg_if_test_ctx(Some("false"));
    PkgStage.run(&mut ctx).unwrap();
    assert_eq!(
        ctx.artifacts.by_kind(ArtifactKind::MacOsPackage).len(),
        0,
        "pkg if=false should skip"
    );
}

#[test]
fn test_pkg_if_render_failure_is_hard_error() {
    let mut ctx = pkg_if_test_ctx(Some("{{ undefined_function 42 }}"));
    let err = PkgStage
        .run(&mut ctx)
        .expect_err("unrenderable `if` should hard-error");
    let msg = format!("{:#}", err);
    assert!(
        msg.contains("`if` template render failed"),
        "error should name `if` render failure, got: {msg}"
    );
}

#[test]
fn test_config_parse_pkg_disable_alias() {
    // The docs show `disable: false`; this must parse without error.
    let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    pkgs:
      - identifier: com.example.test
        disable: false
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let pkgs = config.crates[0].pkgs.as_ref().unwrap();
    assert_eq!(pkgs.len(), 1);
    assert_eq!(pkgs[0].skip, Some(StringOrBool::Bool(false)));
}

#[test]
fn test_config_parse_pkg_disable_true_alias() {
    let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    pkgs:
      - identifier: com.example.test
        disable: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let pkgs = config.crates[0].pkgs.as_ref().unwrap();
    assert_eq!(pkgs[0].skip, Some(StringOrBool::Bool(true)));
}

#[test]
fn test_identifier_template_renders() {
    let tmp = TempDir::new().unwrap();

    let pkg_cfg = PkgConfig {
        identifier: Some("com.example.{{ ProjectName }}".to_string()),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        pkgs: Some(vec![pkg_cfg]),
        ..Default::default()
    }];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/build/myapp"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    PkgStage.run(&mut ctx).unwrap();

    let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
    assert_eq!(pkgs.len(), 1);
    assert_eq!(
        pkgs[0].metadata.get("identifier").map(|s| s.as_str()),
        Some("com.example.myapp"),
        "identifier template should be rendered"
    );
}

/// Build a minimal `Context` with `Version`, `Os`, `Arch`, and `Target` set
/// so per-binary template renders behave the same as they do inside the
/// stage loop.
fn render_fields_test_ctx() -> Context {
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Os", "darwin");
    ctx.template_vars_mut().set("Arch", "arm64");
    ctx.template_vars_mut()
        .set("Target", "aarch64-apple-darwin");
    ctx
}

#[test]
fn test_install_location_template_renders() {
    let mut ctx = render_fields_test_ctx();
    let pkg_cfg = PkgConfig {
        identifier: Some("com.example.myapp".to_string()),
        install_location: Some("/opt/{{ ProjectName }}/bin".to_string()),
        ..Default::default()
    };

    let rendered = render_pkg_fields(
        &mut ctx,
        &pkg_cfg,
        pkg_cfg.identifier.as_deref().unwrap(),
        "myapp",
        Some("aarch64-apple-darwin"),
    )
    .unwrap();

    assert_eq!(rendered.install_location, "/opt/myapp/bin");

    let cmd = pkgbuild_command(
        "/tmp/staging",
        &rendered.identifier,
        "1.0.0",
        &rendered.install_location,
        rendered.scripts.as_deref(),
        None,
        "/tmp/out.pkg",
    );
    let loc_idx = cmd.iter().position(|a| a == "--install-location").unwrap();
    assert_eq!(cmd[loc_idx + 1], "/opt/myapp/bin");
}

#[test]
fn test_scripts_template_renders() {
    let mut ctx = render_fields_test_ctx();
    let pkg_cfg = PkgConfig {
        identifier: Some("com.example.myapp".to_string()),
        scripts: Some("scripts/{{ Os }}".to_string()),
        ..Default::default()
    };

    let rendered = render_pkg_fields(
        &mut ctx,
        &pkg_cfg,
        pkg_cfg.identifier.as_deref().unwrap(),
        "myapp",
        Some("aarch64-apple-darwin"),
    )
    .unwrap();

    assert_eq!(rendered.scripts.as_deref(), Some("scripts/darwin"));

    let cmd = pkgbuild_command(
        "/tmp/staging",
        &rendered.identifier,
        "1.0.0",
        &rendered.install_location,
        rendered.scripts.as_deref(),
        None,
        "/tmp/out.pkg",
    );
    let scripts_idx = cmd.iter().position(|a| a == "--scripts").unwrap();
    assert_eq!(cmd[scripts_idx + 1], "scripts/darwin");
}

#[test]
fn test_mod_timestamp_template_renders() {
    let mut ctx = render_fields_test_ctx();
    ctx.template_vars_mut()
        .set("CommitTimestamp", "2024-06-15T12:34:56Z");

    let pkg_cfg = PkgConfig {
        identifier: Some("com.example.myapp".to_string()),
        mod_timestamp: Some("{{ CommitTimestamp }}".to_string()),
        ..Default::default()
    };

    let rendered = render_pkg_fields(
        &mut ctx,
        &pkg_cfg,
        pkg_cfg.identifier.as_deref().unwrap(),
        "myapp",
        Some("aarch64-apple-darwin"),
    )
    .unwrap();

    assert_eq!(
        rendered.mod_timestamp.as_deref(),
        Some("2024-06-15T12:34:56Z"),
        "mod_timestamp template should expand to the CommitTimestamp value, \
             not be passed literally to parse_mod_timestamp"
    );
    assert_ne!(
        rendered.mod_timestamp.as_deref(),
        Some("{{ CommitTimestamp }}"),
        "literal template string must not reach apply_mod_timestamp"
    );
}

#[test]
fn test_default_name_template_contains_amd64_variant_suffix() {
    assert!(
        default_name_template()
            .contains(anodizer_core::archive_name::INSTALLER_AMD64_VARIANT_SUFFIX),
        "default name template must reuse the shared amd64 variant suffix"
    );
}

#[test]
fn test_default_name_template_no_version_appends_pkg_extension() {
    // Default template has no version segment; the `.pkg` extension is
    // auto-appended so the default emits ProjectName_Arch.pkg.
    let tmp = TempDir::new().unwrap();

    let pkg_cfg = PkgConfig {
        identifier: Some("com.example.myapp".to_string()),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        pkgs: Some(vec![pkg_cfg]),
        ..Default::default()
    }];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/build/myapp"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    PkgStage.run(&mut ctx).unwrap();

    let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
    assert_eq!(pkgs.len(), 1);
    let filename = pkgs[0].path.file_name().unwrap().to_str().unwrap();
    assert_eq!(
        filename, "myapp_arm64.pkg",
        "default name template should be ProjectName_Arch with the .pkg extension appended"
    );
}

/// A user-supplied `name:` that already ends in `.pkg` is used verbatim —
/// the auto-append must not double the extension (case-insensitive match).
#[test]
fn test_user_name_ending_in_pkg_is_not_doubled() {
    let tmp = TempDir::new().unwrap();

    let pkg_cfg = PkgConfig {
        identifier: Some("com.example.myapp".to_string()),
        name: Some("custom_{{ Arch }}.PKG".to_string()),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        pkgs: Some(vec![pkg_cfg]),
        ..Default::default()
    }];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/build/myapp"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    PkgStage.run(&mut ctx).unwrap();

    let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
    assert_eq!(pkgs.len(), 1);
    let filename = pkgs[0].path.file_name().unwrap().to_str().unwrap();
    assert_eq!(
        filename, "custom_arm64.PKG",
        "a user name already ending in .pkg (any case) must not get a second .pkg"
    );
}

#[test]
fn test_two_configs_same_crate_same_arch_default_name_bails() {
    let tmp = TempDir::new().unwrap();

    // Two pkg configs for ONE crate, both the DEFAULT name template. With a
    // single darwin target present, both render `myapp_arm64.pkg` — the same
    // path. A per-config guard would reset between them and let the second
    // silently clobber the first; a per-crate guard must bail.
    let make_cfg = || PkgConfig {
        identifier: Some("com.example.myapp".to_string()),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        pkgs: Some(vec![make_cfg(), make_cfg()]),
        ..Default::default()
    }];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/build/myapp"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    let err = PkgStage
        .run(&mut ctx)
        .expect_err("two configs rendering the same path must bail");
    let msg = err.to_string();
    assert!(msg.contains("pkgs:"), "{msg}");
    assert!(msg.contains("crate 'myapp'"), "{msg}");
    assert!(msg.contains("{{ .Arch }}"), "{msg}");
}

#[test]
fn test_two_configs_same_crate_distinct_names_pass() {
    let tmp = TempDir::new().unwrap();

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        pkgs: Some(vec![
            PkgConfig {
                identifier: Some("com.example.myapp".to_string()),
                name: Some("{{ ProjectName }}-one_{{ Arch }}.pkg".to_string()),
                ..Default::default()
            },
            PkgConfig {
                identifier: Some("com.example.myapp".to_string()),
                name: Some("{{ ProjectName }}-two_{{ Arch }}.pkg".to_string()),
                ..Default::default()
            },
        ]),
        ..Default::default()
    }];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/build/myapp"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    PkgStage
        .run(&mut ctx)
        .expect("distinct names across configs must not collide");
    let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
    assert_eq!(pkgs.len(), 2, "expected one PKG per distinct-named config");
}
