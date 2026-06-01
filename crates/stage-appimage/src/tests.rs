use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::{AppImageConfig, RuntimeHarvest};
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;
use anodizer_core::test_helpers::TestContextBuilder;

use super::*;

// ---------------------------------------------------------------------------
// linuxdeploy argv + env construction (pure)
// ---------------------------------------------------------------------------

#[test]
fn linuxdeploy_args_canonical_order() {
    let args = linuxdeploy_args(
        Path::new("/dist/MyApp.AppDir"),
        Path::new("/dist/MyApp.AppDir/MyApp.desktop"),
        Path::new("/dist/MyApp.AppDir/MyApp.png"),
        &[],
    );
    let strs: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().to_string())
        .collect();
    assert_eq!(
        strs,
        vec![
            "--appdir",
            "/dist/MyApp.AppDir",
            "-d",
            "/dist/MyApp.AppDir/MyApp.desktop",
            "-i",
            "/dist/MyApp.AppDir/MyApp.png",
            "--output",
            "appimage",
        ]
    );
}

#[test]
fn linuxdeploy_args_appends_extra_args() {
    let args = linuxdeploy_args(
        Path::new("/a/X.AppDir"),
        Path::new("/a/X.desktop"),
        Path::new("/a/X.png"),
        &["--custom-arg".to_string(), "value".to_string()],
    );
    let strs: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().to_string())
        .collect();
    assert_eq!(strs.last().unwrap(), "value");
    assert!(strs.contains(&"--custom-arg".to_string()));
    // extra args must come AFTER --output appimage.
    let out_idx = strs.iter().position(|s| s == "appimage").unwrap();
    let custom_idx = strs.iter().position(|s| s == "--custom-arg").unwrap();
    assert!(custom_idx > out_idx);
}

#[test]
fn linuxdeploy_env_sets_required_vars() {
    let env = linuxdeploy_env("1.2.3", "x86_64", "MyApp", None);
    let map: std::collections::HashMap<_, _> = env.into_iter().collect();
    assert_eq!(map.get("VERSION").unwrap(), "1.2.3");
    assert_eq!(map.get("ARCH").unwrap(), "x86_64");
    assert_eq!(map.get("APP").unwrap(), "MyApp");
    assert_eq!(map.get("OUTPUT").unwrap(), "appimage");
    assert!(
        !map.contains_key("UPDATE_INFORMATION"),
        "UPDATE_INFORMATION must be absent when update_information is unset"
    );
}

#[test]
fn linuxdeploy_env_sets_update_information_when_present() {
    let ui = "gh-releases-zsync|helix-editor|helix|latest|helix-*.AppImage.zsync";
    let env = linuxdeploy_env("1.2.3", "aarch64", "helix", Some(ui));
    let map: std::collections::HashMap<_, _> = env.into_iter().collect();
    assert_eq!(map.get("UPDATE_INFORMATION").unwrap(), ui);
    assert_eq!(map.get("ARCH").unwrap(), "aarch64");
}

#[test]
fn appimage_arch_maps_triples() {
    assert_eq!(appimage_arch("x86_64-unknown-linux-gnu"), "x86_64");
    assert_eq!(appimage_arch("aarch64-unknown-linux-gnu"), "aarch64");
    assert_eq!(appimage_arch("armv7-unknown-linux-gnueabihf"), "armhf");
    assert_eq!(appimage_arch("i686-unknown-linux-gnu"), "i686");
}

// ---------------------------------------------------------------------------
// filename rendering
// ---------------------------------------------------------------------------

fn ctx_v(version: &str) -> Context {
    TestContextBuilder::new()
        .project_name("myapp")
        .tag(&format!("v{version}"))
        .populate_git_vars(true)
        .build()
}

#[test]
fn filename_default_composite() {
    let ctx = ctx_v("1.2.3");
    let name = resolve_appimage_filename(&ctx, None, "myapp", "1.2.3", "x86_64").unwrap();
    assert_eq!(name, "myapp-1.2.3-x86_64.AppImage");
}

#[test]
fn filename_template_appends_extension() {
    let ctx = ctx_v("1.2.3");
    let name = resolve_appimage_filename(
        &ctx,
        Some("custom-{{ Version }}"),
        "myapp",
        "1.2.3",
        "x86_64",
    )
    .unwrap();
    assert_eq!(name, "custom-1.2.3.AppImage");
}

#[test]
fn filename_template_keeps_existing_extension() {
    let ctx = ctx_v("1.2.3");
    let name = resolve_appimage_filename(
        &ctx,
        Some("custom-{{ Version }}.AppImage"),
        "myapp",
        "1.2.3",
        "x86_64",
    )
    .unwrap();
    assert_eq!(name, "custom-1.2.3.AppImage");
}

// ---------------------------------------------------------------------------
// AppDir assembly
// ---------------------------------------------------------------------------

fn write(p: &Path, bytes: &[u8]) {
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(p, bytes).unwrap();
}

fn sample_job(tmp: &Path, entries: Vec<AppDirEntry>) -> AppImageJob {
    let bin = tmp.join("src/myapp");
    write(&bin, b"#!/bin/sh\n");
    let desktop = tmp.join("src/MyApp.desktop");
    write(&desktop, b"[Desktop Entry]\nName=MyApp\n");
    let icon = tmp.join("src/myapp.png");
    write(&icon, b"PNG");

    AppImageJob {
        id: "default".to_string(),
        filename: "myapp-1.2.3-x86_64.AppImage".to_string(),
        app_name: "myapp".to_string(),
        version: "1.2.3".to_string(),
        arch_token: "x86_64".to_string(),
        update_information: None,
        extra_args: vec![],
        appdir_root: tmp.join("dist/appimage/default/linux_amd64/myapp.AppDir"),
        output_path: tmp.join("dist/myapp-1.2.3-x86_64.AppImage"),
        binary_src: bin,
        binary_name: "myapp".to_string(),
        desktop_src: desktop,
        icon_src: icon,
        appdir_entries: entries,
        primary_target: Some("x86_64-unknown-linux-gnu".to_string()),
        primary_crate_name: "myapp".to_string(),
    }
}

#[test]
fn assemble_appdir_places_core_files() {
    let tmp = TempDir::new().unwrap();
    let job = sample_job(tmp.path(), vec![]);
    let (desktop_dst, icon_dst) = assemble_appdir(&job.appdir_root, &job).unwrap();

    assert!(job.appdir_root.join("usr/bin/myapp").is_file());
    assert!(desktop_dst.is_file());
    assert_eq!(desktop_dst, job.appdir_root.join("MyApp.desktop"));
    assert!(icon_dst.is_file());
    assert_eq!(icon_dst, job.appdir_root.join("myapp.png"));
}

#[test]
fn assemble_appdir_copies_extra_dir_to_dst() {
    let tmp = TempDir::new().unwrap();
    // A runtime dir with nested content.
    let runtime = tmp.path().join("runtime/grammars");
    write(&runtime.join("rust.so"), b"grammar");
    write(&tmp.path().join("runtime/themes/dark.toml"), b"theme");

    let entry = AppDirEntry {
        src: tmp.path().join("runtime"),
        dst: "usr/lib/helix/runtime".to_string(),
    };
    let job = sample_job(tmp.path(), vec![entry]);
    assemble_appdir(&job.appdir_root, &job).unwrap();

    let bundled = job.appdir_root.join("usr/lib/helix/runtime");
    assert!(bundled.join("grammars/rust.so").is_file());
    assert!(bundled.join("themes/dark.toml").is_file());
}

#[test]
fn assemble_appdir_copies_extra_file_to_dst() {
    let tmp = TempDir::new().unwrap();
    let extra = tmp.path().join("LICENSE");
    write(&extra, b"MIT");
    let entry = AppDirEntry {
        src: extra,
        dst: "usr/share/doc/myapp/LICENSE".to_string(),
    };
    let job = sample_job(tmp.path(), vec![entry]);
    assemble_appdir(&job.appdir_root, &job).unwrap();
    assert!(
        job.appdir_root
            .join("usr/share/doc/myapp/LICENSE")
            .is_file()
    );
}

// ---------------------------------------------------------------------------
// runtime harvest command rendering
// ---------------------------------------------------------------------------

#[test]
fn harvest_command_renders_artifact_and_dir_vars() {
    let mut ctx = ctx_v("1.2.3");
    let host = Artifact {
        kind: ArtifactKind::Binary,
        name: "hx".to_string(),
        path: PathBuf::from("/build/hx"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "helix".to_string(),
        metadata: Default::default(),
        size: None,
    };
    let harvest_dir = PathBuf::from("/dist/.appimage-runtime/helix");
    let cmd = render_harvest_command(
        &mut ctx,
        "{{ .ArtifactPath }} --populate-runtime {{ .HarvestDir }}",
        &host,
        &harvest_dir,
    )
    .unwrap();
    assert_eq!(
        cmd,
        "/build/hx --populate-runtime /dist/.appimage-runtime/helix"
    );

    // Transient vars must be cleared after rendering.
    assert_eq!(
        ctx.template_vars().get("ArtifactPath"),
        Some(&String::new())
    );
    assert_eq!(ctx.template_vars().get("HarvestDir"), Some(&String::new()));
}

// ---------------------------------------------------------------------------
// host-binary resolution
// ---------------------------------------------------------------------------

#[test]
fn resolve_host_binary_finds_host_target() {
    let host_target = anodizer_core::partial::detect_host_target().unwrap();
    let binaries = vec![
        Artifact {
            kind: ArtifactKind::Binary,
            name: "hx".to_string(),
            path: PathBuf::from("/build/hx"),
            target: Some(host_target.clone()),
            crate_name: "helix".to_string(),
            metadata: Default::default(),
            size: None,
        },
        Artifact {
            kind: ArtifactKind::Binary,
            name: "hx".to_string(),
            path: PathBuf::from("/build/cross/hx"),
            target: Some("aarch64-unknown-linux-gnu".to_string()),
            crate_name: "helix".to_string(),
            metadata: Default::default(),
            size: None,
        },
    ];
    let resolved = resolve_host_binary(&binaries).expect("host binary");
    assert_eq!(resolved.target.as_deref(), Some(host_target.as_str()));
}

#[test]
fn resolve_host_binary_none_for_pure_cross() {
    // No artifact targets the host (assumes host is not this exotic triple).
    let binaries = vec![Artifact {
        kind: ArtifactKind::Binary,
        name: "hx".to_string(),
        path: PathBuf::from("/build/hx"),
        target: Some("mips64-unknown-linux-gnuabi64".to_string()),
        crate_name: "helix".to_string(),
        metadata: Default::default(),
        size: None,
    }];
    assert!(resolve_host_binary(&binaries).is_none());
}

// ---------------------------------------------------------------------------
// validation
// ---------------------------------------------------------------------------

#[test]
fn validate_rejects_missing_desktop() {
    let cfg = AppImageConfig {
        icon: Some("icon.png".to_string()),
        ..Default::default()
    };
    let err = validate_config_fields(&cfg, "default").unwrap_err();
    assert!(err.to_string().contains("'desktop' is required"));
}

#[test]
fn validate_rejects_missing_icon() {
    let cfg = AppImageConfig {
        desktop: Some("app.desktop".to_string()),
        ..Default::default()
    };
    let err = validate_config_fields(&cfg, "default").unwrap_err();
    assert!(err.to_string().contains("'icon' is required"));
}

#[test]
fn validate_rejects_duplicate_ids() {
    let configs = vec![
        AppImageConfig {
            id: Some("a".to_string()),
            ..Default::default()
        },
        AppImageConfig {
            id: Some("a".to_string()),
            ..Default::default()
        },
    ];
    assert!(validate_unique_ids(&configs).is_err());
}

// ---------------------------------------------------------------------------
// run-path: dry-run, per-arch naming, host-missing harvest
// (these exercise collect_config_jobs without spawning linuxdeploy)
// ---------------------------------------------------------------------------

fn build_ctx_with_binaries(dist: &Path, targets: &[&str]) -> Context {
    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.2.3")
        .populate_git_vars(true)
        .dist(dist.to_path_buf())
        .dry_run(true)
        .build();
    fs::create_dir_all(dist).unwrap();
    for t in targets {
        let p = dist.join(format!("myapp-{t}"));
        fs::write(&p, b"bin").unwrap();
        let mut metadata = std::collections::HashMap::new();
        metadata.insert("binary".to_string(), "myapp".to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: format!("myapp-{t}"),
            path: p,
            target: Some(t.to_string()),
            crate_name: "myapp".to_string(),
            metadata,
            size: None,
        });
    }
    ctx
}

fn appimage_fixture(tmp: &Path) -> AppImageConfig {
    let desktop = tmp.join("MyApp.desktop");
    write(&desktop, b"[Desktop Entry]\nName=MyApp\n");
    let icon = tmp.join("myapp.png");
    write(&icon, b"PNG");
    AppImageConfig {
        id: Some("default".to_string()),
        desktop: Some(desktop.to_string_lossy().to_string()),
        icon: Some(icon.to_string_lossy().to_string()),
        ..Default::default()
    }
}

#[test]
fn run_noop_when_no_configs() {
    let mut ctx = Context::test_fixture();
    assert!(AppImageStage.run(&mut ctx).is_ok());
}

#[test]
fn dry_run_does_not_spawn_and_does_not_register() {
    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    let mut ctx = build_ctx_with_binaries(&dist, &["x86_64-unknown-linux-gnu"]);
    ctx.config.appimages = vec![appimage_fixture(tmp.path())];

    AppImageStage.run(&mut ctx).expect("dry-run must succeed");
    assert!(
        ctx.artifacts.by_kind(ArtifactKind::AppImage).is_empty(),
        "dry-run registers no artifacts"
    );
}

#[test]
fn multi_arch_produces_distinct_filenames() {
    // Collect jobs directly to inspect per-arch output names without a
    // dry-run early-return. Use a non-dry-run ctx but stop before exec.
    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.2.3")
        .populate_git_vars(true)
        .dist(dist.clone())
        .build();
    fs::create_dir_all(&dist).unwrap();
    for t in ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"] {
        let p = dist.join(format!("myapp-{t}"));
        fs::write(&p, b"bin").unwrap();
        let mut metadata = std::collections::HashMap::new();
        metadata.insert("binary".to_string(), "myapp".to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: format!("myapp-{t}"),
            path: p,
            target: Some(t.to_string()),
            crate_name: "myapp".to_string(),
            metadata,
            size: None,
        });
    }
    let cfg = appimage_fixture(tmp.path());

    let log = ctx.logger("appimage");
    let mut jobs = Vec::new();
    collect_config_jobs(
        &mut ctx, &log, &cfg, &dist, "1.2.3", "myapp", false, &mut jobs,
    )
    .unwrap();

    let names: std::collections::BTreeSet<String> =
        jobs.iter().map(|j| j.filename.clone()).collect();
    assert_eq!(
        names,
        [
            "myapp-1.2.3-aarch64.AppImage",
            "myapp-1.2.3-x86_64.AppImage"
        ]
        .into_iter()
        .map(String::from)
        .collect()
    );
    // No two jobs share an output path (no clobber).
    let paths: std::collections::BTreeSet<_> = jobs.iter().map(|j| j.output_path.clone()).collect();
    assert_eq!(paths.len(), jobs.len());
}

#[test]
fn harvest_missing_host_binary_errors() {
    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    let mut ctx = build_ctx_with_binaries(&dist, &["mips64-unknown-linux-gnuabi64"]);
    let mut cfg = appimage_fixture(tmp.path());
    cfg.runtime_harvest = Some(RuntimeHarvest {
        command: "{{ .ArtifactPath }} --populate {{ .HarvestDir }}".to_string(),
        dir: "usr/lib/myapp/runtime".to_string(),
    });
    ctx.config.appimages = vec![cfg];

    let err = AppImageStage.run(&mut ctx).unwrap_err();
    assert!(
        err.to_string()
            .contains("no built artifact matches the host target"),
        "expected clear host-missing error, got: {err}"
    );
}

#[test]
fn update_information_threads_into_job() {
    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.2.3")
        .populate_git_vars(true)
        .dist(dist.clone())
        .build();
    fs::create_dir_all(&dist).unwrap();
    let p = dist.join("myapp-x86_64-unknown-linux-gnu");
    fs::write(&p, b"bin").unwrap();
    let mut metadata = std::collections::HashMap::new();
    metadata.insert("binary".to_string(), "myapp".to_string());
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: "myapp-x86_64-unknown-linux-gnu".to_string(),
        path: p,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata,
        size: None,
    });
    let mut cfg = appimage_fixture(tmp.path());
    cfg.update_information =
        Some("gh-releases-zsync|me|myapp|latest|myapp-*.AppImage.zsync".to_string());

    let log = ctx.logger("appimage");
    let mut jobs = Vec::new();
    collect_config_jobs(
        &mut ctx, &log, &cfg, &dist, "1.2.3", "myapp", false, &mut jobs,
    )
    .unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        jobs[0].update_information.as_deref(),
        Some("gh-releases-zsync|me|myapp|latest|myapp-*.AppImage.zsync")
    );
    let env = linuxdeploy_env(
        &jobs[0].version,
        &jobs[0].arch_token,
        &jobs[0].app_name,
        jobs[0].update_information.as_deref(),
    );
    let map: std::collections::HashMap<_, _> = env.into_iter().collect();
    assert!(map.contains_key("UPDATE_INFORMATION"));
}
