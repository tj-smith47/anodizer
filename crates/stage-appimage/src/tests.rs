use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use anodizer_core::arch_path_guard::ArchPathGuard;
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
        &[],
    );
    let strs: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().to_string())
        .collect();
    // No `-i`: the icon is pre-placed into the AppDir icon-theme tree and
    // resolved from the desktop `Icon=` key (see `linuxdeploy_args` docs).
    assert_eq!(
        strs,
        vec![
            "--appdir",
            "/dist/MyApp.AppDir",
            "-d",
            "/dist/MyApp.AppDir/MyApp.desktop",
            "--output",
            "appimage",
        ]
    );
    assert!(
        !strs.iter().any(|s| s == "-i"),
        "icon must not be passed via -i (linuxdeploy would resolution-reject it)"
    );
}

#[test]
fn linuxdeploy_args_appends_extra_args() {
    let args = linuxdeploy_args(
        Path::new("/a/X.AppDir"),
        Path::new("/a/X.desktop"),
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
    let env = linuxdeploy_env(
        "1.2.3",
        "x86_64",
        "MyApp",
        "MyApp-1.2.3-x86_64.AppImage",
        None,
    );
    let map: std::collections::HashMap<_, _> = env.into_iter().collect();
    assert_eq!(map.get("VERSION").unwrap(), "1.2.3");
    assert_eq!(map.get("ARCH").unwrap(), "x86_64");
    assert_eq!(map.get("APP").unwrap(), "MyApp");
    // OUTPUT / LDAI_OUTPUT are the output FILENAME (both names for plugin
    // version compat), NOT a plugin selector.
    assert_eq!(map.get("OUTPUT").unwrap(), "MyApp-1.2.3-x86_64.AppImage");
    assert_eq!(
        map.get("LDAI_OUTPUT").unwrap(),
        "MyApp-1.2.3-x86_64.AppImage"
    );
    assert!(
        !map.contains_key("UPDATE_INFORMATION"),
        "UPDATE_INFORMATION must be absent when update_information is unset"
    );
}

#[test]
fn linuxdeploy_env_sets_update_information_when_present() {
    let ui = "gh-releases-zsync|helix-editor|helix|latest|helix-*.AppImage.zsync";
    let env = linuxdeploy_env(
        "1.2.3",
        "aarch64",
        "helix",
        "helix-1.2.3-aarch64.AppImage",
        Some(ui),
    );
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
    let (name, resolved_template) =
        resolve_appimage_filename(&ctx, None, "myapp", "1.2.3", "x86_64").unwrap();
    assert_eq!(name, "myapp-1.2.3-x86_64.AppImage");
    // The default path reports the composed default as the resolved template
    // (never an empty string) so a guard clobber error names a real template.
    assert_eq!(resolved_template, "myapp-1.2.3-x86_64.AppImage");
    assert!(!resolved_template.is_empty());
}

/// Drift guard: the amd64 suffix the default filename appends is rendered from
/// the shared core const, so a `v3`-seeded build appends `v3` while a baseline
/// (no `Amd64` var / `v1`) appends nothing — preserving the historical name.
#[test]
fn filename_default_appends_amd64_variant_suffix() {
    let mut ctx = ctx_v("1.2.3");

    // Baseline (v1) and None both render the unsuffixed historical name.
    anodizer_core::archive_name::seed_amd64_variant_var(ctx.template_vars_mut(), Some("v1"));
    assert_eq!(
        resolve_appimage_filename(&ctx, None, "myapp", "1.2.3", "x86_64")
            .unwrap()
            .0,
        "myapp-1.2.3-x86_64.AppImage"
    );

    // A v3 build appends the variant before the extension.
    anodizer_core::archive_name::seed_amd64_variant_var(ctx.template_vars_mut(), Some("v3"));
    let (name, resolved_template) =
        resolve_appimage_filename(&ctx, None, "myapp", "1.2.3", "x86_64").unwrap();
    assert_eq!(name, "myapp-1.2.3-x86_64v3.AppImage");
    // The composed default carries the v3 suffix, so the guard cites the
    // suffixed template — not an empty string.
    assert_eq!(resolved_template, "myapp-1.2.3-x86_64v3.AppImage");
}

#[test]
fn filename_template_appends_extension() {
    let ctx = ctx_v("1.2.3");
    let (name, resolved_template) = resolve_appimage_filename(
        &ctx,
        Some("custom-{{ Version }}"),
        "myapp",
        "1.2.3",
        "x86_64",
    )
    .unwrap();
    assert_eq!(name, "custom-1.2.3.AppImage");
    // A user `filename:` is reported verbatim as the resolved template.
    assert_eq!(resolved_template, "custom-{{ Version }}");
}

#[test]
fn filename_template_keeps_existing_extension() {
    let ctx = ctx_v("1.2.3");
    let (name, _) = resolve_appimage_filename(
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
        amd64_variant: None,
        sde_epoch: None,
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
    // Icon also pre-placed into the icon-theme tree (no real PNG header in the
    // fixture → 256x256 fallback dir) so linuxdeploy resolves it without `-i`.
    assert!(
        job.appdir_root
            .join("usr/share/icons/hicolor/256x256/apps/myapp.png")
            .is_file()
    );
}

#[test]
fn assemble_appdir_deploys_icon_under_desktop_icon_key() {
    // Real-config shape: the icon FILE basename (logo) differs from the desktop
    // `Icon=` key (myapp). The icon must deploy under the `Icon=` name, both at
    // the AppDir root and in the theme tree, or linuxdeploy can't resolve it.
    let tmp = TempDir::new().unwrap();
    let mut job = sample_job(tmp.path(), vec![]);
    let desktop = tmp.path().join("src/MyApp.desktop");
    write(&desktop, b"[Desktop Entry]\nName=MyApp\nIcon=myapp\n");
    let icon = tmp.path().join("src/logo.png");
    write(&icon, b"PNG-not-a-real-header");
    job.desktop_src = desktop;
    job.icon_src = icon;

    let (_desktop_dst, icon_dst) = assemble_appdir(&job.appdir_root, &job).unwrap();

    assert_eq!(icon_dst, job.appdir_root.join("myapp.png"));
    assert!(
        job.appdir_root
            .join("usr/share/icons/hicolor/256x256/apps/myapp.png")
            .is_file(),
        "icon must land in the theme tree under the desktop Icon= key"
    );
    assert!(
        !job.appdir_root.join("logo.png").exists(),
        "icon must NOT keep its source basename when Icon= differs"
    );
}

#[test]
fn png_dimensions_reads_real_header() {
    // Minimal valid PNG header: signature + IHDR length + "IHDR" + 1024x1024.
    let tmp = TempDir::new().unwrap();
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend_from_slice(&[0, 0, 0, 13]); // IHDR chunk length
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&1024u32.to_be_bytes()); // width
    bytes.extend_from_slice(&1024u32.to_be_bytes()); // height
    bytes.extend_from_slice(&[8, 6, 0, 0, 0]); // bit depth + color type + rest
    let p = tmp.path().join("logo.png");
    write(&p, &bytes);

    assert_eq!(png_dimensions(&p), Some((1024, 1024)));
    assert_eq!(icon_theme_subdir(&p), "1024x1024");
}

#[test]
fn png_dimensions_none_for_non_png() {
    let tmp = TempDir::new().unwrap();
    let p = tmp.path().join("x.png");
    write(&p, b"not a png");
    assert_eq!(png_dimensions(&p), None);
    assert_eq!(icon_theme_subdir(&p), "256x256");
}

#[test]
fn icon_theme_subdir_scalable_for_svg() {
    assert_eq!(icon_theme_subdir(Path::new("/a/logo.svg")), "scalable");
    assert_eq!(icon_theme_subdir(Path::new("/a/logo.SVG")), "scalable");
}

#[test]
fn desktop_icon_name_parses_and_strips_extension() {
    let tmp = TempDir::new().unwrap();
    let bare = tmp.path().join("bare.desktop");
    write(&bare, b"[Desktop Entry]\nName=X\nIcon=anodizer\n");
    assert_eq!(desktop_icon_name(&bare).as_deref(), Some("anodizer"));

    let withext = tmp.path().join("ext.desktop");
    write(&withext, b"[Desktop Entry]\nIcon=logo.png\n");
    assert_eq!(desktop_icon_name(&withext).as_deref(), Some("logo"));

    let none = tmp.path().join("none.desktop");
    write(&none, b"[Desktop Entry]\nName=X\n");
    assert_eq!(desktop_icon_name(&none), None);
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

/// The harvest vars (`ArtifactPath` / `HarvestDir`) must NOT leak into a
/// later AppImage render (desktop / icon / filename template). Guards the
/// Task-3 precedent: a stale host-binary path bleeding into the archive
/// name_template shipped a per-run worktree prefix into downstream output.
/// This asserts that a subsequent render of a `{{ .ArtifactPath }}` /
/// `{{ .HarvestDir }}`-bearing template resolves EMPTY after the harvest
/// render. Mutation check: removing the post-render clear in
/// `render_harvest_command` makes this fail (the render then resolves to the
/// host-binary path / harvest dir instead of empty strings).
#[test]
fn harvest_vars_do_not_leak_into_later_render() {
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
    render_harvest_command(
        &mut ctx,
        "{{ .ArtifactPath }} --populate-runtime {{ .HarvestDir }}",
        &host,
        &PathBuf::from("/dist/.appimage-runtime/helix"),
    )
    .unwrap();

    // A downstream filename/desktop template that references the harvest vars
    // must render them empty — they were cleared after the harvest render.
    let rendered = ctx
        .render_template("app[{{ .ArtifactPath }}][{{ .HarvestDir }}].desktop")
        .unwrap();
    assert_eq!(
        rendered, "app[][].desktop",
        "harvest vars leaked into a later render: {rendered}"
    );
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
    let mut arch_guard = ArchPathGuard::new();
    collect_config_jobs(
        &mut ctx,
        &log,
        &cfg,
        &dist,
        "1.2.3",
        "myapp",
        None,
        false,
        &mut arch_guard,
        &mut jobs,
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

/// Three x86_64 builds tagged amd64_variant v1/v2/v3 plus one aarch64 build
/// must each yield a distinct `.AppImage` job (no ArchPathGuard error): the
/// default filename appends the amd64 micro-arch suffix (v1 → no suffix, v2 →
/// `…x86_64v2`, v3 → `…x86_64v3`), so the same triple no longer clobbers
/// itself.
#[test]
fn same_triple_multi_variant_produces_distinct_filenames() {
    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.2.3")
        .populate_git_vars(true)
        .dist(dist.clone())
        .build();
    fs::create_dir_all(&dist).unwrap();

    for variant in ["v1", "v2", "v3"] {
        let p = dist.join(format!("myapp-{variant}"));
        fs::write(&p, b"bin").unwrap();
        let mut metadata = std::collections::HashMap::new();
        metadata.insert("binary".to_string(), "myapp".to_string());
        metadata.insert("amd64_variant".to_string(), variant.to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: format!("myapp-{variant}"),
            path: p,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata,
            size: None,
        });
    }
    let arm = dist.join("myapp-arm");
    fs::write(&arm, b"bin").unwrap();
    let mut arm_meta = std::collections::HashMap::new();
    arm_meta.insert("binary".to_string(), "myapp".to_string());
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: "myapp-arm".to_string(),
        path: arm,
        target: Some("aarch64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: arm_meta,
        size: None,
    });

    let cfg = appimage_fixture(tmp.path());
    let log = ctx.logger("appimage");
    let mut jobs = Vec::new();
    let mut arch_guard = ArchPathGuard::new();
    collect_config_jobs(
        &mut ctx,
        &log,
        &cfg,
        &dist,
        "1.2.3",
        "myapp",
        None,
        false,
        &mut arch_guard,
        &mut jobs,
    )
    .expect("multi-variant build must not clobber");

    assert_eq!(jobs.len(), 4, "one AppImage per variant + arm64");
    let names: std::collections::BTreeSet<String> =
        jobs.iter().map(|j| j.filename.clone()).collect();
    assert_eq!(
        names,
        [
            "myapp-1.2.3-aarch64.AppImage",
            "myapp-1.2.3-x86_64.AppImage",
            "myapp-1.2.3-x86_64v2.AppImage",
            "myapp-1.2.3-x86_64v3.AppImage",
        ]
        .into_iter()
        .map(String::from)
        .collect()
    );
    // Distinct AppDir + output paths (no clobber).
    let paths: std::collections::BTreeSet<_> = jobs.iter().map(|j| j.output_path.clone()).collect();
    assert_eq!(paths.len(), jobs.len());
    let appdirs: std::collections::BTreeSet<_> =
        jobs.iter().map(|j| j.appdir_root.clone()).collect();
    assert_eq!(appdirs.len(), jobs.len(), "AppDir per variant must differ");
    // The variant is recorded on the job for downstream artifact metadata.
    let variants: std::collections::BTreeSet<Option<String>> =
        jobs.iter().map(|j| j.amd64_variant.clone()).collect();
    assert!(variants.contains(&Some("v2".to_string())));
    assert!(variants.contains(&Some("v3".to_string())));
}

/// A constant `filename:` (no `{{ .Arch }}` / `{{ .Amd64 }}` discriminator)
/// renders the same `.AppImage` path for two amd64 variants — the
/// ArchPathGuard must error loudly rather than let the second job clobber the
/// first.
#[test]
fn constant_filename_bails_across_variants() {
    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.2.3")
        .populate_git_vars(true)
        .dist(dist.clone())
        .build();
    fs::create_dir_all(&dist).unwrap();

    for variant in ["v1", "v3"] {
        let p = dist.join(format!("myapp-{variant}"));
        fs::write(&p, b"bin").unwrap();
        let mut metadata = std::collections::HashMap::new();
        metadata.insert("binary".to_string(), "myapp".to_string());
        metadata.insert("amd64_variant".to_string(), variant.to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: format!("myapp-{variant}"),
            path: p,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata,
            size: None,
        });
    }

    let mut cfg = appimage_fixture(tmp.path());
    // Constant — no per-target discriminator.
    cfg.filename = Some("myapp-installer".to_string());

    let log = ctx.logger("appimage");
    let mut jobs = Vec::new();
    let mut arch_guard = ArchPathGuard::new();
    let err = collect_config_jobs(
        &mut ctx,
        &log,
        &cfg,
        &dist,
        "1.2.3",
        "myapp",
        None,
        false,
        &mut arch_guard,
        &mut jobs,
    )
    .unwrap_err()
    .to_string();

    assert!(err.contains("appimage:"), "{err}");
    assert!(err.contains("crate 'myapp'"), "{err}");
    assert!(err.contains("{{ .Amd64 }}"), "{err}");
}

/// Two `appimages:` configs (distinct `id`, both the default filename) render
/// the same `.AppImage` path for one arch. The guard now spans both configs of
/// the project, so the second config bails loudly instead of silently
/// clobbering the first config's artifact.
#[test]
fn two_configs_same_default_name_bail_across_configs() {
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

    let mut cfg_a = appimage_fixture(tmp.path());
    cfg_a.id = Some("first".to_string());
    let mut cfg_b = appimage_fixture(tmp.path());
    cfg_b.id = Some("second".to_string());

    let log = ctx.logger("appimage");
    let mut jobs = Vec::new();
    // One guard threaded across both configs, exactly as `run()` does.
    let mut arch_guard = ArchPathGuard::new();
    collect_config_jobs(
        &mut ctx,
        &log,
        &cfg_a,
        &dist,
        "1.2.3",
        "myapp",
        None,
        false,
        &mut arch_guard,
        &mut jobs,
    )
    .expect("first config must pass");

    let err = collect_config_jobs(
        &mut ctx,
        &log,
        &cfg_b,
        &dist,
        "1.2.3",
        "myapp",
        None,
        false,
        &mut arch_guard,
        &mut jobs,
    )
    .unwrap_err()
    .to_string();

    assert!(err.contains("appimage:"), "{err}");
    assert!(err.contains("crate 'myapp'"), "{err}");
    assert!(err.contains("{{ .Arch }}"), "{err}");
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
    let mut arch_guard = ArchPathGuard::new();
    collect_config_jobs(
        &mut ctx,
        &log,
        &cfg,
        &dist,
        "1.2.3",
        "myapp",
        None,
        false,
        &mut arch_guard,
        &mut jobs,
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
        &jobs[0].filename,
        jobs[0].update_information.as_deref(),
    );
    let map: std::collections::HashMap<_, _> = env.into_iter().collect();
    assert!(map.contains_key("UPDATE_INFORMATION"));
}

/// SOURCE_DATE_EPOCH is resolved in the serial phase and threaded onto every
/// job so the parallel phase can pin AppDir mtimes + forward it to
/// linuxdeploy/appimagetool for a reproducible squashfs.
#[test]
fn sde_epoch_threads_into_job() {
    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    let mut ctx = build_ctx_with_binaries(&dist, &["x86_64-unknown-linux-gnu"]);
    let cfg = appimage_fixture(tmp.path());
    let log = ctx.logger("appimage");
    let mut jobs = Vec::new();
    let mut arch_guard = ArchPathGuard::new();
    collect_config_jobs(
        &mut ctx,
        &log,
        &cfg,
        &dist,
        "1.2.3",
        "myapp",
        Some(1_577_836_800),
        false,
        &mut arch_guard,
        &mut jobs,
    )
    .unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].sde_epoch, Some(1_577_836_800));
}

/// `pin_appdir_mtimes` rewrites every staged file's mtime to the epoch so the
/// squashfs payload is byte-stable across runs.
#[test]
fn pin_appdir_mtimes_sets_every_file() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("AppDir");
    write(&root.join("usr/bin/app"), b"bin");
    write(&root.join("usr/share/icons/app.png"), b"png");
    write(&root.join("app.desktop"), b"[Desktop Entry]");

    let epoch = 1_577_836_800i64; // 2020-01-01T00:00:00Z
    pin_appdir_mtimes(&root, epoch).unwrap();

    for rel in ["usr/bin/app", "usr/share/icons/app.png", "app.desktop"] {
        let m = fs::metadata(root.join(rel)).unwrap();
        let secs = m
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(secs as i64, epoch, "mtime not pinned on {rel}");
    }
}

/// `locate_built_appimage` prefers the entry whose basename equals the
/// expected output filename, ignoring stale `.AppImage` files left in the
/// work dir by a prior failed attempt (the old first-`read_dir`-entry pick
/// could grab the wrong one).
#[test]
fn locate_built_appimage_prefers_expected_name() {
    let tmp = TempDir::new().unwrap();
    let work = tmp.path().join("work");
    let appdir = work.join("myapp.AppDir");
    fs::create_dir_all(&appdir).unwrap();
    // A stale output from a previous run, plus the freshly-built expected one.
    write(&work.join("stale-old.AppImage"), b"stale");
    write(&work.join("myapp-1.2.3-x86_64.AppImage"), b"fresh");

    let found = locate_built_appimage(&appdir, "myapp-1.2.3-x86_64.AppImage").unwrap();
    assert_eq!(
        found.file_name().and_then(|n| n.to_str()),
        Some("myapp-1.2.3-x86_64.AppImage"),
        "must select the expected output, not an arbitrary read_dir entry"
    );
}

/// With no exact-name match, `locate_built_appimage` falls back to the newest
/// `.AppImage` rather than an arbitrary `read_dir` entry.
#[test]
fn locate_built_appimage_falls_back_to_newest() {
    let tmp = TempDir::new().unwrap();
    let work = tmp.path().join("work");
    let appdir = work.join("app.AppDir");
    fs::create_dir_all(&appdir).unwrap();

    let old = work.join("App-x86_64.AppImage");
    write(&old, b"old");
    anodizer_core::util::set_file_mtime_epoch(&old, 1_000_000_000).unwrap();
    let new = work.join("App-aarch64.AppImage");
    write(&new, b"new");
    anodizer_core::util::set_file_mtime_epoch(&new, 2_000_000_000).unwrap();

    let found = locate_built_appimage(&appdir, "does-not-match.AppImage").unwrap();
    assert_eq!(
        found.file_name().and_then(|n| n.to_str()),
        Some("App-aarch64.AppImage"),
        "must pick the newest .AppImage when no exact name matches"
    );
}

/// `group_by_platform` groups by `os_arch` and iterates in deterministic
/// (alphabetical) key order via `BTreeMap`, pinning the order-regression
/// guard so `dist/artifacts.json` does not drift between runs.
#[test]
fn group_by_platform_is_deterministic() {
    let mk = |target: &str| Artifact {
        kind: ArtifactKind::Binary,
        name: "app".into(),
        path: PathBuf::from(format!("/dist/{target}")),
        target: Some(target.into()),
        crate_name: "app".into(),
        metadata: std::collections::HashMap::new(),
        size: None,
    };
    // Input order is reversed relative to the expected sorted key order.
    let inputs = [
        mk("x86_64-unknown-linux-gnu"),
        mk("aarch64-unknown-linux-gnu"),
    ];
    let groups = group_by_platform(&inputs);
    let keys: Vec<_> = groups.keys().cloned().collect();
    // Neither binary carries amd64_variant metadata, so the variant half of
    // each key is None; ordering is still deterministic (BTreeMap, sorted).
    assert_eq!(
        keys,
        vec![
            ("linux_amd64".to_string(), None),
            ("linux_arm64".to_string(), None),
        ]
    );
}

/// Build a Context holding a gnu and a musl x86_64 binary tagged with the
/// build ids the dogfood config uses.
fn ctx_with_gnu_and_musl(dist: &Path) -> Context {
    let mut ctx = TestContextBuilder::new()
        .project_name("anodizer")
        .tag("v1.0.0")
        .populate_git_vars(true)
        .dist(dist.to_path_buf())
        .dry_run(true)
        .build();
    fs::create_dir_all(dist).unwrap();
    for (target, id) in [
        ("x86_64-unknown-linux-gnu", "anodizer"),
        ("x86_64-unknown-linux-musl", "anodizer-musl"),
    ] {
        let p = dist.join(format!("anodizer-{id}"));
        fs::write(&p, b"bin").unwrap();
        let mut metadata = std::collections::HashMap::new();
        metadata.insert("binary".to_string(), "anodizer".to_string());
        metadata.insert("id".to_string(), id.to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: format!("anodizer-{id}"),
            path: p,
            target: Some(target.to_string()),
            crate_name: "anodizer".to_string(),
            metadata,
            size: None,
        });
    }
    ctx
}

#[test]
fn ids_bind_keeps_gnu_excludes_musl() {
    // The AppImage wraps glibc libraries (linuxdeploy). gnu and musl x86_64
    // both group to linux_amd64 and the filename renders only Arch (amd64),
    // so without an `ids:` bind the musl binary clobbers the gnu AppImage.
    // `ids: [anodizer]` must keep ONLY the gnu binary.
    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    let ctx = ctx_with_gnu_and_musl(&dist);

    let cfg = AppImageConfig {
        id: Some("default".to_string()),
        ids: Some(vec!["anodizer".to_string()]),
        ..Default::default()
    };
    let bins = collect_matching_binaries(&ctx, &cfg, &["linux".to_string()]);
    assert_eq!(bins.len(), 1, "only the gnu build survives the bind");
    let t = bins[0].target.as_deref().unwrap_or("");
    assert!(
        t.contains("-linux-gnu"),
        "bound AppImage build must be gnu, got {t:?}"
    );
}

#[test]
fn no_ids_admits_both_builds_the_collision_we_guard_against() {
    // Documents the auto-collect hazard: with `ids` unset both x86_64 builds
    // are admitted and group to the same linux_amd64 key — the collision the
    // `ids: [anodizer]` bind on `appimages:` prevents.
    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    let ctx = ctx_with_gnu_and_musl(&dist);

    let cfg = AppImageConfig {
        id: Some("default".to_string()),
        ids: None,
        ..Default::default()
    };
    let bins = collect_matching_binaries(&ctx, &cfg, &["linux".to_string()]);
    assert_eq!(bins.len(), 2, "auto-collect admits both (the hazard)");
    let groups = group_by_platform(&bins);
    assert_eq!(
        groups.get(&("linux_amd64".to_string(), None)).map(Vec::len),
        Some(2),
        "both x86_64 builds collide on the linux_amd64 group key"
    );
}
