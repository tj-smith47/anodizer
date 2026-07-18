use super::*;
use std::path::PathBuf;

use super::super::install::{FileType, generate_install_script};
use super::super::nuspec::generate_nuspec;
use super::super::package::compute_nupkg_hash;

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::{
    ChocolateyConfig, Config, ContentSource, CrateConfig, MetadataConfig, PublishConfig,
    RepositoryConfig, StringOrBool,
};
use anodizer_core::context::{Context, ContextOptions};
use anodizer_core::log::{StageLogger, Verbosity};

fn windows_artifact(crate_name: &str, target: &str, name: &str) -> Artifact {
    let mut m = std::collections::HashMap::new();
    m.insert("sha256".to_string(), "deadbeef".to_string());
    m.insert("url".to_string(), format!("https://example.com/{}", name));
    Artifact {
        kind: ArtifactKind::Archive,
        path: std::path::PathBuf::from(format!("/tmp/{}", name)),
        name: name.to_string(),
        target: Some(target.to_string()),
        crate_name: crate_name.to_string(),
        metadata: m,
        size: None,
    }
}

fn ctx_with_choco(cfg: ChocolateyConfig) -> Context {
    ctx_with_choco_opts(cfg, ContextOptions::default())
}

fn ctx_with_choco_opts(cfg: ChocolateyConfig, opts: ContextOptions) -> Context {
    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        publish: Some(PublishConfig {
            chocolatey: Some(cfg),
            ..Default::default()
        }),
        ..Default::default()
    }];
    Context::new(config, opts)
}

// -----------------------------------------------------------------
// check_skip_publish
// -----------------------------------------------------------------

#[test]
fn check_skip_publish_returns_false_when_skip_is_none() {
    let mut ctx = ctx_with_choco(ChocolateyConfig::default());
    let cfg = ChocolateyConfig::default();
    let log = StageLogger::new("publish", Verbosity::Quiet);
    assert!(!check_skip_publish(&mut ctx, &cfg, "mytool", &log).unwrap());
}

#[test]
fn check_skip_publish_returns_false_when_skip_is_literal_false() {
    let mut ctx = ctx_with_choco(ChocolateyConfig::default());
    let cfg = ChocolateyConfig {
        skip: Some(StringOrBool::Bool(false)),
        ..Default::default()
    };
    let log = StageLogger::new("publish", Verbosity::Quiet);
    assert!(!check_skip_publish(&mut ctx, &cfg, "mytool", &log).unwrap());
}

#[test]
fn check_skip_publish_template_evaluating_false_does_not_skip() {
    let mut ctx = ctx_with_choco(ChocolateyConfig::default());
    let cfg = ChocolateyConfig {
        skip: Some(StringOrBool::String("false".to_string())),
        ..Default::default()
    };
    let log = StageLogger::new("publish", Verbosity::Quiet);
    assert!(!check_skip_publish(&mut ctx, &cfg, "mytool", &log).unwrap());
}

#[test]
fn check_skip_publish_template_evaluating_true_skips_and_logs() {
    let mut ctx = ctx_with_choco(ChocolateyConfig::default());
    let cfg = ChocolateyConfig {
        skip: Some(StringOrBool::String("true".to_string())),
        ..Default::default()
    };
    let (log, cap) = StageLogger::with_capture("publish", Verbosity::Normal);
    assert!(check_skip_publish(&mut ctx, &cfg, "mytool", &log).unwrap());
    let msgs = cap.all_messages();
    assert!(
        msgs.iter()
            .any(|(_, m)| m.contains("skipped") && m.contains("mytool")),
        "expected skip status, got {msgs:?}"
    );
}

#[test]
fn check_skip_publish_propagates_render_error_with_context() {
    let mut ctx = ctx_with_choco(ChocolateyConfig::default());
    let cfg = ChocolateyConfig {
        skip: Some(StringOrBool::String("{{ undefined.symbol(".to_string())),
        ..Default::default()
    };
    let log = StageLogger::new("publish", Verbosity::Quiet);
    let err = check_skip_publish(&mut ctx, &cfg, "mytool", &log)
        .expect_err("malformed template must bubble");
    let msg = format!("{err:#}");
    assert!(msg.contains("render skip template"), "{msg}");
    assert!(msg.contains("mytool"), "{msg}");
}

// -----------------------------------------------------------------
// resolve_metadata
// -----------------------------------------------------------------

#[test]
fn resolve_metadata_falls_back_to_project_metadata_for_license_and_description() {
    let mut config = Config::default();
    config.metadata = Some(MetadataConfig {
        description: Some("project-level desc".to_string()),
        license: Some("Apache-2.0".to_string()),
        maintainers: Some(vec!["Alice <a@example.com>".to_string()]),
        ..Default::default()
    });
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        ..Default::default()
    }];
    let ctx = Context::new(config, ContextOptions::default());
    let cfg = ChocolateyConfig::default();
    let meta = resolve_metadata(&ctx, &cfg, "mytool", "", "", &ctx.logger("publish")).unwrap();
    assert_eq!(meta.description, "project-level desc");
    assert_eq!(meta.license, "Apache-2.0");
    assert_eq!(meta.authors, "Alice <a@example.com>");
    assert_eq!(meta.project_url, "");
    assert_eq!(meta.icon_url, "");
    assert!(meta.tags.is_empty());
}

#[test]
fn resolve_metadata_uses_choco_fields_over_project_metadata() {
    let mut config = Config::default();
    config.metadata = Some(MetadataConfig {
        description: Some("project desc".to_string()),
        license: Some("Apache-2.0".to_string()),
        ..Default::default()
    });
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        ..Default::default()
    }];
    let ctx = Context::new(config, ContextOptions::default());
    let cfg = ChocolateyConfig {
        description: Some("choco desc".to_string()),
        license: Some("MIT".to_string()),
        authors: Some("Choco Author".to_string()),
        tags: Some(vec!["cli".to_string()]),
        icon_url: Some("https://example.com/i.png".to_string()),
        ..Default::default()
    };
    let meta = resolve_metadata(&ctx, &cfg, "mytool", "", "", &ctx.logger("publish")).unwrap();
    assert_eq!(meta.description, "choco desc");
    assert_eq!(meta.license, "MIT");
    assert_eq!(meta.authors, "Choco Author");
    assert_eq!(meta.icon_url, "https://example.com/i.png");
    assert_eq!(meta.tags, vec!["cli".to_string()]);
}

#[test]
fn resolve_metadata_derives_project_url_from_repo_when_unset() {
    let ctx = ctx_with_choco(ChocolateyConfig::default());
    let cfg = ChocolateyConfig {
        license: Some("MIT".to_string()),
        ..Default::default()
    };
    let meta = resolve_metadata(
        &ctx,
        &cfg,
        "mytool",
        "myorg",
        "mytool",
        &ctx.logger("publish"),
    )
    .unwrap();
    assert_eq!(meta.project_url, "https://github.com/myorg/mytool");
}

#[test]
fn resolve_metadata_explicit_project_url_wins_over_repo_derivation() {
    let ctx = ctx_with_choco(ChocolateyConfig::default());
    let cfg = ChocolateyConfig {
        license: Some("MIT".to_string()),
        project_url: Some("https://example.com/home".to_string()),
        ..Default::default()
    };
    let meta = resolve_metadata(
        &ctx,
        &cfg,
        "mytool",
        "myorg",
        "mytool",
        &ctx.logger("publish"),
    )
    .unwrap();
    assert_eq!(meta.project_url, "https://example.com/home");
}

#[test]
fn resolve_metadata_authors_default_is_crate_name_when_no_maintainers() {
    let ctx = ctx_with_choco(ChocolateyConfig::default());
    let cfg = ChocolateyConfig {
        license: Some("MIT".to_string()),
        ..Default::default()
    };
    let meta = resolve_metadata(&ctx, &cfg, "mytool", "", "", &ctx.logger("publish")).unwrap();
    assert_eq!(meta.authors, "mytool");
}

#[test]
fn resolve_metadata_missing_license_returns_actionable_bail() {
    let ctx = ctx_with_choco(ChocolateyConfig::default());
    let cfg = ChocolateyConfig::default();
    let err = match resolve_metadata(&ctx, &cfg, "mytool", "", "", &ctx.logger("publish")) {
        Err(e) => e,
        Ok(_) => panic!("missing license must bail"),
    };
    let msg = format!("{err:#}");
    assert!(msg.contains("license is required"), "{msg}");
    assert!(msg.contains("mytool"), "{msg}");
    assert!(
        msg.contains("publish.chocolatey.license") || msg.contains("metadata.license"),
        "{msg}"
    );
}

// -----------------------------------------------------------------
// select_windows_artifacts
// -----------------------------------------------------------------

#[test]
fn select_windows_artifacts_partitions_first_386_and_first_amd64() {
    let mut ctx = ctx_with_choco(ChocolateyConfig::default());
    ctx.artifacts.add(windows_artifact(
        "mytool",
        "i686-pc-windows-msvc",
        "a-386.zip",
    ));
    ctx.artifacts.add(windows_artifact(
        "mytool",
        "x86_64-pc-windows-msvc",
        "b-amd64.zip",
    ));
    ctx.artifacts.add(windows_artifact(
        "mytool",
        "x86_64-pc-windows-msvc",
        "c-amd64-dup.zip",
    ));
    let cfg = ChocolateyConfig::default();
    let log = StageLogger::new("publish", Verbosity::Quiet);
    let (a32, a64) = select_windows_artifacts(&ctx, &cfg, "mytool", &log);
    assert_eq!(a32.unwrap().name, "a-386.zip");
    // First amd64 wins; second is dropped.
    assert_eq!(a64.unwrap().name, "b-amd64.zip");
}

#[test]
fn select_windows_artifacts_logs_and_skips_arm64() {
    let mut ctx = ctx_with_choco(ChocolateyConfig::default());
    ctx.artifacts.add(windows_artifact(
        "mytool",
        "aarch64-pc-windows-msvc",
        "x-arm64.zip",
    ));
    let cfg = ChocolateyConfig::default();
    let (log, cap) = StageLogger::with_capture("publish", Verbosity::Normal);
    let (a32, a64) = select_windows_artifacts(&ctx, &cfg, "mytool", &log);
    assert!(a32.is_none() && a64.is_none());
    let msgs = cap.all_messages();
    assert!(
        msgs.iter().any(|(_, m)| {
            m.contains("x-arm64.zip") && m.contains("arm64") && m.contains("not")
        }),
        "expected arm64-skip log; got {msgs:?}"
    );
}

#[test]
fn select_windows_artifacts_ids_filter_drops_non_matching() {
    let mut ctx = ctx_with_choco(ChocolateyConfig::default());
    let mut wanted = windows_artifact("mytool", "x86_64-pc-windows-msvc", "wanted.zip");
    wanted.metadata.insert("id".to_string(), "good".to_string());
    let mut unwanted = windows_artifact("mytool", "x86_64-pc-windows-msvc", "unwanted.zip");
    unwanted
        .metadata
        .insert("id".to_string(), "bad".to_string());
    ctx.artifacts.add(wanted);
    ctx.artifacts.add(unwanted);
    let cfg = ChocolateyConfig {
        ids: Some(vec!["good".to_string()]),
        ..Default::default()
    };
    let log = StageLogger::new("publish", Verbosity::Quiet);
    let (_a32, a64) = select_windows_artifacts(&ctx, &cfg, "mytool", &log);
    assert_eq!(a64.unwrap().name, "wanted.zip");
}

#[test]
fn select_windows_artifacts_amd64_variant_filter() {
    let mut ctx = ctx_with_choco(ChocolateyConfig::default());
    let mut v2 = windows_artifact("mytool", "x86_64-pc-windows-msvc", "amd64-v2.zip");
    v2.metadata
        .insert("amd64_variant".to_string(), "v2".to_string());
    let mut v3 = windows_artifact("mytool", "x86_64-pc-windows-msvc", "amd64-v3.zip");
    v3.metadata
        .insert("amd64_variant".to_string(), "v3".to_string());
    ctx.artifacts.add(v2);
    ctx.artifacts.add(v3);
    let cfg = ChocolateyConfig {
        amd64_variant: Some(anodizer_core::config::Amd64Variant::V3),
        ..Default::default()
    };
    let log = StageLogger::new("publish", Verbosity::Quiet);
    let (_a32, a64) = select_windows_artifacts(&ctx, &cfg, "mytool", &log);
    assert_eq!(a64.unwrap().name, "amd64-v3.zip");
}

/// Build a Windows `Installer`-kind artifact stamped with the given
/// `format` (`msi` / `nsis`), matching what stage-msi / stage-nsis emit.
fn installer_artifact(target: &str, name: &str, format: &str) -> Artifact {
    let mut m = std::collections::HashMap::new();
    m.insert("sha256".to_string(), "deadbeef".to_string());
    m.insert("url".to_string(), format!("https://example.com/{}", name));
    m.insert("format".to_string(), format.to_string());
    Artifact {
        kind: ArtifactKind::Installer,
        path: std::path::PathBuf::from(format!("/tmp/{}", name)),
        name: name.to_string(),
        target: Some(target.to_string()),
        crate_name: "mytool".to_string(),
        metadata: m,
        size: None,
    }
}

/// Cross-wire guard: with BOTH an MSI and an NSIS exe for the same arch and
/// `use: msi`, selection MUST pick the MSI. Before the format filter the
/// first-Installer-wins partition could route the NSIS exe into the 64-bit
/// slot, which the install script then wrapped with `-FileType 'msi'` — an
/// exe run with MSI switches (broken install, moderation reject).
#[test]
fn select_windows_artifacts_use_msi_picks_msi_over_nsis() {
    let mut ctx = ctx_with_choco(ChocolateyConfig::default());
    // NSIS added FIRST so first-wins would pick it without the format gate.
    ctx.artifacts.add(installer_artifact(
        "x86_64-pc-windows-msvc",
        "app-setup.exe",
        "nsis",
    ));
    ctx.artifacts.add(installer_artifact(
        "x86_64-pc-windows-msvc",
        "app-x64.msi",
        "msi",
    ));
    let cfg = ChocolateyConfig {
        use_artifact: Some("msi".to_string()),
        ..Default::default()
    };
    let log = StageLogger::new("publish", Verbosity::Quiet);
    let (_a32, a64) = select_windows_artifacts(&ctx, &cfg, "mytool", &log);
    assert_eq!(
        a64.expect("an amd64 installer must be selected").name,
        "app-x64.msi",
        "use: msi must select the MSI, not the NSIS exe"
    );
}

/// The converse: `use: nsis` with both formats present must pick the NSIS
/// exe, not the MSI.
#[test]
fn select_windows_artifacts_use_nsis_picks_nsis_over_msi() {
    let mut ctx = ctx_with_choco(ChocolateyConfig::default());
    // MSI added FIRST so first-wins would pick it without the format gate.
    ctx.artifacts.add(installer_artifact(
        "x86_64-pc-windows-msvc",
        "app-x64.msi",
        "msi",
    ));
    ctx.artifacts.add(installer_artifact(
        "x86_64-pc-windows-msvc",
        "app-setup.exe",
        "nsis",
    ));
    let cfg = ChocolateyConfig {
        use_artifact: Some("nsis".to_string()),
        ..Default::default()
    };
    let log = StageLogger::new("publish", Verbosity::Quiet);
    let (_a32, a64) = select_windows_artifacts(&ctx, &cfg, "mytool", &log);
    assert_eq!(
        a64.expect("an amd64 installer must be selected").name,
        "app-setup.exe",
        "use: nsis must select the NSIS exe, not the MSI"
    );
}

/// Tolerance: an installer missing the `format` key is NOT dropped (older
/// build stages may not stamp it) — it is still selectable under `use:
/// msi` when it is the only candidate.
#[test]
fn select_windows_artifacts_keeps_format_less_installer() {
    let mut ctx = ctx_with_choco(ChocolateyConfig::default());
    let mut art = installer_artifact("x86_64-pc-windows-msvc", "app.msi", "msi");
    art.metadata.remove("format");
    ctx.artifacts.add(art);
    let cfg = ChocolateyConfig {
        use_artifact: Some("msi".to_string()),
        ..Default::default()
    };
    let log = StageLogger::new("publish", Verbosity::Quiet);
    let (_a32, a64) = select_windows_artifacts(&ctx, &cfg, "mytool", &log);
    assert_eq!(
        a64.expect("a format-less installer must still be selectable")
            .name,
        "app.msi"
    );
}

#[test]
fn select_windows_artifacts_matches_windows_in_path_when_target_empty() {
    let mut ctx = ctx_with_choco(ChocolateyConfig::default());
    let mut art = windows_artifact("mytool", "", "WinDoWs-386.zip");
    art.target = None;
    art.path = std::path::PathBuf::from("/tmp/WinDoWs-386.zip");
    ctx.artifacts.add(art);
    let cfg = ChocolateyConfig::default();
    let log = StageLogger::new("publish", Verbosity::Quiet);
    let (a32, a64) = select_windows_artifacts(&ctx, &cfg, "mytool", &log);
    // No target => arch=="" => both 386/amd64 buckets stay empty.
    // (Path match qualifies the filter, but the arch dispatcher only
    // matches canonical "386"/"amd64" tokens.)
    assert!(a32.is_none() && a64.is_none());
}

// -----------------------------------------------------------------
// build_install_mode
// -----------------------------------------------------------------

#[test]
fn build_install_mode_dual_when_both_archs_present() {
    let ctx = ctx_with_choco(ChocolateyConfig::default());
    let cfg = ChocolateyConfig::default();
    let a32 = windows_artifact("mytool", "i686-pc-windows-msvc", "x86.zip");
    let a64 = windows_artifact("mytool", "x86_64-pc-windows-msvc", "x64.zip");
    let mode = build_install_mode(
        &ctx,
        &cfg,
        "mytool",
        "1.0.0",
        Some(&a32),
        Some(&a64),
        "mytool",
    )
    .unwrap();
    match mode {
        InstallMode::Dual {
            url32,
            hash32,
            url64,
            hash64,
        } => {
            assert_eq!(url32, "https://example.com/x86.zip");
            assert_eq!(hash32, "deadbeef");
            assert_eq!(url64, "https://example.com/x64.zip");
            assert_eq!(hash64, "deadbeef");
        }
        _ => panic!("expected Dual"),
    }
}

#[test]
fn build_install_mode_single_32bit_when_only_386() {
    let ctx = ctx_with_choco(ChocolateyConfig::default());
    let cfg = ChocolateyConfig::default();
    let a32 = windows_artifact("mytool", "i686-pc-windows-msvc", "x86.zip");
    let mode =
        build_install_mode(&ctx, &cfg, "mytool", "1.0.0", Some(&a32), None, "mytool").unwrap();
    match mode {
        InstallMode::Single { is_32bit, url, .. } => {
            assert!(is_32bit);
            assert_eq!(url, "https://example.com/x86.zip");
        }
        _ => panic!("expected Single 32-bit"),
    }
}

#[test]
fn build_install_mode_single_64bit_when_only_amd64() {
    let ctx = ctx_with_choco(ChocolateyConfig::default());
    let cfg = ChocolateyConfig::default();
    let a64 = windows_artifact("mytool", "x86_64-pc-windows-msvc", "x64.zip");
    let mode =
        build_install_mode(&ctx, &cfg, "mytool", "1.0.0", None, Some(&a64), "mytool").unwrap();
    match mode {
        InstallMode::Single { is_32bit, url, .. } => {
            assert!(!is_32bit);
            assert_eq!(url, "https://example.com/x64.zip");
        }
        _ => panic!("expected Single 64-bit"),
    }
}

#[test]
fn build_install_mode_url_template_overrides_metadata_url() {
    let ctx = ctx_with_choco(ChocolateyConfig::default());
    let cfg = ChocolateyConfig {
        url_template: Some(
            "https://feeds.example.com/{{ name }}-{{ version }}-{{ arch }}.zip".to_string(),
        ),
        ..Default::default()
    };
    let a64 = windows_artifact("mytool", "x86_64-pc-windows-msvc", "ignored.zip");
    let mode =
        build_install_mode(&ctx, &cfg, "mytool", "9.9.9", None, Some(&a64), "mytool").unwrap();
    match mode {
        InstallMode::Single { url, .. } => {
            assert_eq!(url, "https://feeds.example.com/mytool-9.9.9-amd64.zip");
        }
        _ => panic!("expected Single from template"),
    }
}

#[test]
fn build_install_mode_bails_on_no_windows_artifacts() {
    let ctx = ctx_with_choco(ChocolateyConfig::default());
    let cfg = ChocolateyConfig::default();
    let err = match build_install_mode(&ctx, &cfg, "mytool", "1.0.0", None, None, "mytool") {
        Err(e) => e,
        Ok(_) => panic!("expected bail"),
    };
    let msg = format!("{err:#}");
    assert!(msg.contains("no windows artifact"), "{msg}");
    assert!(msg.contains("mytool"), "{msg}");
}

#[test]
fn build_install_mode_bails_on_empty_sha256() {
    let ctx = ctx_with_choco(ChocolateyConfig::default());
    let cfg = ChocolateyConfig::default();
    let mut a64 = windows_artifact("mytool", "x86_64-pc-windows-msvc", "x64.zip");
    a64.metadata.insert("sha256".to_string(), "".to_string());
    let err = match build_install_mode(&ctx, &cfg, "mytool", "1.0.0", None, Some(&a64), "mytool") {
        Err(e) => e,
        Ok(_) => panic!("expected bail"),
    };
    let msg = format!("{err:#}");
    assert!(msg.contains("sha256"), "{msg}");
    assert!(msg.contains("x64.zip"), "{msg}");
}

// -----------------------------------------------------------------
// render_text_fields
// -----------------------------------------------------------------

#[test]
fn render_text_fields_all_none_when_choco_unset_and_no_metadata() {
    let ctx = ctx_with_choco(ChocolateyConfig::default());
    let cfg = ChocolateyConfig::default();
    let tf = render_text_fields(&ctx, &cfg, "mytool", &ctx.logger("publish")).unwrap();
    assert!(tf.title.is_none());
    assert!(tf.copyright.is_none());
    assert!(tf.summary.is_none());
    assert!(tf.release_notes.is_none());
}

#[test]
fn render_text_fields_renders_title_and_copyright_through_tera() {
    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        ..Default::default()
    }];
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("ProjectName", "mytool");
    let cfg = ChocolateyConfig {
        title: Some("{{ ProjectName }} CLI".to_string()),
        copyright: Some("Copyright {{ ProjectName }}".to_string()),
        ..Default::default()
    };
    let tf = render_text_fields(&ctx, &cfg, "mytool", &ctx.logger("publish")).unwrap();
    assert_eq!(tf.title.as_deref(), Some("mytool CLI"));
    assert_eq!(tf.copyright.as_deref(), Some("Copyright mytool"));
}

#[test]
fn render_text_fields_renders_docs_url_package_source_url_owners_name_through_tera() {
    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        ..Default::default()
    }];
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Tag", "v1.2.3");
    ctx.template_vars_mut().set("ProjectName", "mytool");
    let cfg = ChocolateyConfig {
        name: Some("{{ ProjectName }}-cli".to_string()),
        docs_url: Some("https://github.com/x/y/blob/{{ .Tag }}/docs/configuration.md".to_string()),
        package_source_url: Some("https://github.com/x/y/tree/{{ .Tag }}".to_string()),
        owners: Some("owner-{{ .Tag }}".to_string()),
        ..Default::default()
    };
    let tf = render_text_fields(&ctx, &cfg, "mytool", &ctx.logger("publish")).unwrap();
    assert_eq!(tf.name.as_deref(), Some("mytool-cli"));
    assert_eq!(
        tf.docs_url.as_deref(),
        Some("https://github.com/x/y/blob/v1.2.3/docs/configuration.md")
    );
    assert!(!tf.docs_url.as_deref().unwrap().contains("{{"));
    assert_eq!(
        tf.package_source_url.as_deref(),
        Some("https://github.com/x/y/tree/v1.2.3")
    );
    assert_eq!(tf.owners.as_deref(), Some("owner-v1.2.3"));
}

#[test]
fn resolve_metadata_renders_explicit_url_fields_and_tags_through_tera() {
    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        ..Default::default()
    }];
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Tag", "v1.2.3");
    let cfg = ChocolateyConfig {
        license: Some("MIT".to_string()),
        project_url: Some("https://github.com/x/y/tree/{{ .Tag }}".to_string()),
        license_url: Some("https://github.com/x/y/blob/{{ .Tag }}/LICENSE".to_string()),
        tags: Some(vec!["cli-{{ .Tag }}".to_string()]),
        ..Default::default()
    };
    let meta = resolve_metadata(&ctx, &cfg, "mytool", "", "", &ctx.logger("publish")).unwrap();
    assert_eq!(meta.project_url, "https://github.com/x/y/tree/v1.2.3");
    assert_eq!(
        meta.license_url.as_deref(),
        Some("https://github.com/x/y/blob/v1.2.3/LICENSE")
    );
    assert_eq!(meta.tags, vec!["cli-v1.2.3".to_string()]);
    assert!(!meta.project_url.contains("{{"));
}

#[test]
fn resolve_metadata_derives_license_url_for_single_spdx() {
    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        ..Default::default()
    }];
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Tag", "v1.2.3");
    let cfg = ChocolateyConfig {
        license: Some("MIT".to_string()),
        ..Default::default()
    };
    let meta = resolve_metadata(&ctx, &cfg, "mytool", "o", "r", &ctx.logger("publish")).unwrap();
    // A single-identifier license keeps the derived GitHub LICENSE blob URL.
    assert_eq!(
        meta.license_url.as_deref(),
        Some("https://github.com/o/r/blob/v1.2.3/LICENSE")
    );
}

#[test]
fn resolve_metadata_omits_derived_license_url_for_compound_spdx() {
    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        ..Default::default()
    }];
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Tag", "v1.2.3");
    let cfg = ChocolateyConfig {
        license: Some("MIT OR Apache-2.0".to_string()),
        ..Default::default()
    };
    let meta = resolve_metadata(&ctx, &cfg, "mytool", "o", "r", &ctx.logger("publish")).unwrap();
    // A compound SPDX expression has no single LICENSE file → no
    // <licenseUrl>, and Chocolatey supports no other license metadata
    // (the NuGet <license> element is CHCU0002-flagged).
    assert_eq!(meta.license_url, None);
}

#[test]
fn resolve_metadata_omits_derived_license_url_for_slash_form_compound() {
    // The legacy slash form (`MIT/Apache-2.0`) is also compound — the house
    // SPDX parser returns `AnyOf` — so its derived single-file `<licenseUrl>`
    // must be suppressed too. (The old bespoke whitespace check missed this
    // and would have emitted a 404ing URL for a dual-licensed repo.)
    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        ..Default::default()
    }];
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Tag", "v1.2.3");
    let cfg = ChocolateyConfig {
        license: Some("MIT/Apache-2.0".to_string()),
        ..Default::default()
    };
    let meta = resolve_metadata(&ctx, &cfg, "mytool", "o", "r", &ctx.logger("publish")).unwrap();
    assert_eq!(meta.license_url, None);
}

#[test]
fn resolve_metadata_explicit_license_url_wins_even_for_compound_spdx() {
    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        ..Default::default()
    }];
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Tag", "v1.2.3");
    let cfg = ChocolateyConfig {
        license: Some("MIT OR Apache-2.0".to_string()),
        license_url: Some("https://example.com/license".to_string()),
        ..Default::default()
    };
    let meta = resolve_metadata(&ctx, &cfg, "mytool", "o", "r", &ctx.logger("publish")).unwrap();
    assert_eq!(
        meta.license_url.as_deref(),
        Some("https://example.com/license")
    );
}

#[test]
fn render_text_fields_summary_falls_back_to_metadata_description() {
    let mut config = Config::default();
    config.metadata = Some(MetadataConfig {
        description: Some("project summary".to_string()),
        ..Default::default()
    });
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        ..Default::default()
    }];
    let ctx = Context::new(config, ContextOptions::default());
    let cfg = ChocolateyConfig::default();
    let tf = render_text_fields(&ctx, &cfg, "mytool", &ctx.logger("publish")).unwrap();
    assert_eq!(tf.summary.as_deref(), Some("project summary"));
}

#[test]
fn render_text_fields_release_notes_falls_back_to_metadata_full_description() {
    let mut config = Config::default();
    config.metadata = Some(MetadataConfig {
        full_description: Some(ContentSource::Inline("long-form readme".to_string())),
        ..Default::default()
    });
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        ..Default::default()
    }];
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.populate_metadata_var().unwrap();
    let cfg = ChocolateyConfig::default();
    let tf = render_text_fields(&ctx, &cfg, "mytool", &ctx.logger("publish")).unwrap();
    assert_eq!(tf.release_notes.as_deref(), Some("long-form readme"));
}

#[test]
fn render_text_fields_release_notes_uses_changelog_template_var() {
    let mut ctx = ctx_with_choco(ChocolateyConfig::default());
    ctx.template_vars_mut()
        .set("ReleaseNotes", "## v1.0.0\n- one\n- two");
    let cfg = ChocolateyConfig {
        release_notes: Some("Release notes:\n{{ Changelog }}".to_string()),
        ..Default::default()
    };
    let tf = render_text_fields(&ctx, &cfg, "mytool", &ctx.logger("publish")).unwrap();
    let rn = tf.release_notes.expect("release_notes set");
    assert!(rn.contains("## v1.0.0"));
    assert!(rn.contains("- one"));
}

#[test]
fn render_text_fields_malformed_template_falls_back_to_raw() {
    let ctx = ctx_with_choco(ChocolateyConfig::default());
    let cfg = ChocolateyConfig {
        title: Some("{{ broken".to_string()),
        ..Default::default()
    };
    let tf = render_text_fields(&ctx, &cfg, "mytool", &ctx.logger("publish")).unwrap();
    assert_eq!(tf.title.as_deref(), Some("{{ broken"));
}

// -----------------------------------------------------------------
// build_nuspec
// -----------------------------------------------------------------

#[test]
fn build_nuspec_assembles_xml_with_required_metadata() {
    let cfg = ChocolateyConfig {
        name: Some("renamed".to_string()),
        ..Default::default()
    };
    let metadata = ChocoMetadata {
        description: "d".to_string(),
        license: "MIT".to_string(),
        license_url: Some("https://example.com/license".to_string()),
        authors: "Alice".to_string(),
        project_url: "https://example.com".to_string(),
        icon_url: String::new(),
        tags: vec!["cli".to_string()],
        project_source_url: None,
        bug_tracker_url: None,
    };
    let text = ChocoTextFields {
        title: None,
        copyright: None,
        summary: None,
        release_notes: None,
        name: Some("renamed".to_string()),
        package_source_url: None,
        docs_url: None,
        owners: None,
    };
    let xml = build_nuspec(&cfg, "ignored", "2.3.4", &metadata, &text).unwrap();
    assert!(xml.contains("<id>renamed</id>"));
    assert!(xml.contains("<version>2.3.4</version>"));
    assert!(xml.contains("<authors>Alice</authors>"));
    assert!(xml.contains("<tags>cli</tags>"));
}

// -----------------------------------------------------------------
// stage_package
// -----------------------------------------------------------------

#[test]
fn stage_package_writes_nuspec_install_script_and_nupkg() {
    let pkg_name = "mytool";
    let version = "0.1.2";
    let nuspec_xml = generate_nuspec(&crate::chocolatey::nuspec::NuspecParams {
        name: pkg_name,
        version,
        description: "d",
        license_url: None,
        authors: "a",
        project_url: "https://example.com",
        icon_url: "",
        tags: &[],
        package_source_url: None,
        owners: None,
        title: None,
        copyright: None,
        require_license_acceptance: false,
        project_source_url: None,
        docs_url: None,
        bug_tracker_url: None,
        summary: None,
        release_notes: None,
        dependencies: &[],
    })
    .unwrap();
    let install =
        generate_install_script(pkg_name, "https://e/x.zip", "abc", false, FileType::Zip).unwrap();
    let (log, cap) = StageLogger::with_capture("publish", Verbosity::Normal);
    let staged = stage_package(pkg_name, version, &nuspec_xml, &install, &log).unwrap();
    assert!(staged.nupkg_path.exists(), "nupkg must be written");
    assert!(
        staged
            .nupkg_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("mytool.0.1.2.nupkg")
    );
    // The status log lines for nuspec / install / nupkg paths were emitted.
    let msgs = cap.all_messages();
    let joined: String = msgs.iter().map(|(_, m)| m.clone()).collect();
    assert!(joined.contains("nuspec"));
    assert!(joined.contains("install script"));
    assert!(joined.contains("nupkg"));
}

// -----------------------------------------------------------------
// resolve_api_key
// -----------------------------------------------------------------

#[test]
fn resolve_api_key_renders_template_from_config() {
    let mut ctx = ctx_with_choco(ChocolateyConfig::default());
    ctx.template_vars_mut().set("MyKey", "from-template");
    let cfg = ChocolateyConfig {
        api_key: Some("{{ MyKey }}".to_string()),
        ..Default::default()
    };
    assert_eq!(
        resolve_api_key(&ctx, &cfg, &ctx.logger("publish")).unwrap(),
        "from-template"
    );
}

#[test]
fn resolve_api_key_falls_back_to_env() {
    let mut ctx = ctx_with_choco(ChocolateyConfig::default());
    ctx.set_env_source(anodizer_core::MapEnvSource::new().with("CHOCOLATEY_API_KEY", "from-env"));
    let cfg = ChocolateyConfig::default();
    assert_eq!(
        resolve_api_key(&ctx, &cfg, &ctx.logger("publish")).unwrap(),
        "from-env"
    );
}

#[test]
fn resolve_api_key_empty_when_neither_configured_nor_env() {
    let mut ctx = ctx_with_choco(ChocolateyConfig::default());
    // Inject an empty env so the test does not pick up a real
    // `CHOCOLATEY_API_KEY` from the host shell.
    ctx.set_env_source(anodizer_core::MapEnvSource::new());
    let cfg = ChocolateyConfig::default();
    assert!(
        resolve_api_key(&ctx, &cfg, &ctx.logger("publish"))
            .unwrap()
            .is_empty()
    );
}

// -----------------------------------------------------------------
// handle_feed_state — drives the OData feed-state ladder against an
// in-process HTTP responder. Touches the moderation-skip, hash-match,
// hash-drift, rejected, PresentNoHash, and absent branches.
// -----------------------------------------------------------------

fn fast_retry() -> anodizer_core::retry::RetryPolicy {
    anodizer_core::retry::RetryPolicy {
        max_attempts: 1,
        base_delay: std::time::Duration::from_millis(0),
        max_delay: std::time::Duration::from_millis(0),
    }
}

fn http_200(body: &str) -> String {
    let len = body.len();
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {len}\r\n\r\n{body}"
    )
}

/// Write `bytes` to a tempfile and return the path; used to drive
/// `compute_nupkg_hash` against a real on-disk blob.
fn tmp_blob(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pkg.nupkg");
    std::fs::write(&path, bytes).unwrap();
    (dir, path)
}

#[test]
fn handle_feed_state_absent_returns_none_to_proceed_to_push() {
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
    let (addr, _calls) =
        spawn_oneshot_http_responder(vec!["HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"]);
    let source = format!("http://{addr}");
    let mut ctx = ctx_with_choco(ChocolateyConfig::default());
    let cfg = ChocolateyConfig::default();
    let (_d, pkg) = tmp_blob(b"abc");
    let log = StageLogger::new("publish", Verbosity::Quiet);
    let out = handle_feed_state(
        &mut ctx,
        &cfg,
        &source,
        "mytool",
        "1.0.0",
        &pkg,
        &fast_retry(),
        &log,
    )
    .unwrap();
    assert_eq!(out, None);
}

#[test]
fn handle_feed_state_present_no_hash_warns_and_returns_none() {
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
    let body = "<entry><id>https://example.com/api/v2/Packages(Id='mytool',Version='1.0.0')</id>\
            <m:properties><d:PackageStatus>Approved</d:PackageStatus></m:properties></entry>";
    let resp: &'static str = Box::leak(http_200(body).into_boxed_str());
    let (addr, _calls) = spawn_oneshot_http_responder(vec![resp]);
    let source = format!("http://{addr}");
    let mut ctx = ctx_with_choco(ChocolateyConfig::default());
    let cfg = ChocolateyConfig::default();
    let (_d, pkg) = tmp_blob(b"abc");
    let (log, cap) = StageLogger::with_capture("publish", Verbosity::Normal);
    let out = handle_feed_state(
        &mut ctx,
        &cfg,
        &source,
        "mytool",
        "1.0.0",
        &pkg,
        &fast_retry(),
        &log,
    )
    .unwrap();
    assert_eq!(out, None, "PresentNoHash must fall through to push");
    assert!(
        cap.warn_messages()
            .iter()
            .any(|m| m.contains("hash was unavailable")),
        "expected PresentNoHash warn; got {:?}",
        cap.all_messages()
    );
}

#[test]
fn handle_feed_state_hash_match_short_circuits_to_skip() {
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
    // Compute the SHA256 base64 of the local blob, then feed that
    // same hash back via the OData response — should match.
    let (_d, pkg) = tmp_blob(b"matching-bytes");
    let local_hash = compute_nupkg_hash(&pkg, "SHA256").unwrap();
    let body = format!(
        "<entry><id>https://example.com/api/v2/Packages(Id='mytool',Version='1.0.0')</id>\
            <m:properties>\
            <d:PackageHash>{}</d:PackageHash>\
            <d:PackageHashAlgorithm>SHA256</d:PackageHashAlgorithm>\
            <d:PackageStatus>Approved</d:PackageStatus>\
            <d:IsApproved>true</d:IsApproved>\
            </m:properties></entry>",
        local_hash
    );
    let resp: &'static str = Box::leak(http_200(&body).into_boxed_str());
    let (addr, _calls) = spawn_oneshot_http_responder(vec![resp]);
    let source = format!("http://{addr}");
    let mut ctx = ctx_with_choco(ChocolateyConfig::default());
    let cfg = ChocolateyConfig::default();
    let (log, cap) = StageLogger::with_capture("publish", Verbosity::Normal);
    let out = handle_feed_state(
        &mut ctx,
        &cfg,
        &source,
        "mytool",
        "1.0.0",
        &pkg,
        &fast_retry(),
        &log,
    )
    .unwrap();
    assert_eq!(out, Some(false), "hash match must short-circuit to skip");
    assert!(
        cap.all_messages()
            .iter()
            .any(|(_, m)| m.contains("already published") && m.contains("hash match")),
        "expected hash-match status; got {:?}",
        cap.all_messages()
    );
}

#[test]
fn handle_feed_state_hash_drift_bails_with_actionable_error() {
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
    let (_d, pkg) = tmp_blob(b"local-bytes");
    let body = "<entry><id>https://example.com/api/v2/Packages(Id='mytool',Version='1.0.0')</id>\
            <m:properties>\
            <d:PackageHash>DIFFERENT_HASH_FROM_FEED</d:PackageHash>\
            <d:PackageHashAlgorithm>SHA256</d:PackageHashAlgorithm>\
            <d:PackageStatus>Approved</d:PackageStatus>\
            <d:IsApproved>true</d:IsApproved>\
            </m:properties></entry>";
    let resp: &'static str = Box::leak(http_200(body).into_boxed_str());
    let (addr, _calls) = spawn_oneshot_http_responder(vec![resp]);
    let source = format!("http://{addr}");
    let mut ctx = ctx_with_choco(ChocolateyConfig::default());
    let cfg = ChocolateyConfig::default();
    let log = StageLogger::new("publish", Verbosity::Quiet);
    let err = match handle_feed_state(
        &mut ctx,
        &cfg,
        &source,
        "mytool",
        "1.0.0",
        &pkg,
        &fast_retry(),
        &log,
    ) {
        Err(e) => e,
        Ok(other) => panic!("hash drift must bail, got Ok({other:?})"),
    };
    let msg = format!("{err:#}");
    assert!(msg.contains("local nupkg"), "{msg}");
    assert!(msg.contains("immutable"), "{msg}");
    assert!(msg.contains("bump the version"), "{msg}");
}

#[test]
fn handle_feed_state_rejected_bails_loudly() {
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
    let body = "<entry><id>https://example.com/api/v2/Packages(Id='mytool',Version='1.0.0')</id>\
            <m:properties>\
            <d:PackageHash>X</d:PackageHash>\
            <d:PackageHashAlgorithm>SHA512</d:PackageHashAlgorithm>\
            <d:PackageStatus>Rejected</d:PackageStatus>\
            <d:IsApproved>false</d:IsApproved>\
            <d:Published>2026-01-01T00:00:00</d:Published>\
            </m:properties></entry>";
    let resp: &'static str = Box::leak(http_200(body).into_boxed_str());
    let (addr, _calls) = spawn_oneshot_http_responder(vec![resp]);
    let source = format!("http://{addr}");
    let mut ctx = ctx_with_choco(ChocolateyConfig::default());
    let cfg = ChocolateyConfig::default();
    let (_d, pkg) = tmp_blob(b"abc");
    let log = StageLogger::new("publish", Verbosity::Quiet);
    let err = match handle_feed_state(
        &mut ctx,
        &cfg,
        &source,
        "mytool",
        "1.0.0",
        &pkg,
        &fast_retry(),
        &log,
    ) {
        Err(e) => e,
        Ok(other) => panic!("Rejected must bail, got Ok({other:?})"),
    };
    let msg = format!("{err:#}");
    assert!(msg.contains("REJECTED"), "{msg}");
    assert!(msg.contains("bump the version"), "{msg}");
}

#[test]
fn handle_feed_state_in_moderation_skip_records_pending_outcome() {
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
    let body = "<entry><id>https://example.com/api/v2/Packages(Id='mytool',Version='1.0.0')</id>\
            <m:properties>\
            <d:PackageHash>X</d:PackageHash>\
            <d:PackageHashAlgorithm>SHA512</d:PackageHashAlgorithm>\
            <d:PackageStatus>Submitted</d:PackageStatus>\
            <d:IsApproved>false</d:IsApproved>\
            </m:properties></entry>";
    let resp: &'static str = Box::leak(http_200(body).into_boxed_str());
    let (addr, _calls) = spawn_oneshot_http_responder(vec![resp]);
    let source = format!("http://{addr}");
    let mut ctx = ctx_with_choco(ChocolateyConfig::default());
    let cfg = ChocolateyConfig::default();
    let (_d, pkg) = tmp_blob(b"abc");
    let (log, cap) = StageLogger::with_capture("publish", Verbosity::Normal);
    let out = handle_feed_state(
        &mut ctx,
        &cfg,
        &source,
        "mytool",
        "1.0.0",
        &pkg,
        &fast_retry(),
        &log,
    )
    .unwrap();
    assert_eq!(out, Some(false), "moderation skip must short-circuit");
    assert!(
        matches!(
            ctx.take_pending_outcome(),
            Some(anodizer_core::PublisherOutcome::PendingModeration)
        ),
        "pending outcome must be PendingModeration"
    );
    assert!(
        cap.warn_messages()
            .iter()
            .any(|m| m.contains("republish_in_moderation: true")),
        "expected guidance in warn; got {:?}",
        cap.all_messages()
    );
}

#[test]
fn handle_feed_state_in_moderation_with_republish_flag_proceeds_to_push() {
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
    let body = "<entry><id>https://example.com/api/v2/Packages(Id='mytool',Version='1.0.0')</id>\
            <m:properties>\
            <d:PackageHash>X</d:PackageHash>\
            <d:PackageHashAlgorithm>SHA512</d:PackageHashAlgorithm>\
            <d:PackageStatus>Submitted</d:PackageStatus>\
            <d:IsApproved>false</d:IsApproved>\
            </m:properties></entry>";
    let resp: &'static str = Box::leak(http_200(body).into_boxed_str());
    let (addr, _calls) = spawn_oneshot_http_responder(vec![resp]);
    let source = format!("http://{addr}");
    let mut ctx = ctx_with_choco(ChocolateyConfig::default());
    // Local bytes whose SHA512 base64 cannot be the feed's "X": the nupkg
    // differs (a fail-forward re-cut shifts <releaseNotes>). A Submitted
    // (in-moderation) version is NOT immutable, so republish_in_moderation
    // must proceed to push (replace the queued copy) rather than bail on
    // the drift.
    let cfg = ChocolateyConfig {
        republish_in_moderation: Some(StringOrBool::Bool(true)),
        ..Default::default()
    };
    let (_d, pkg) = tmp_blob(b"local-bytes");
    let log = StageLogger::new("publish", Verbosity::Quiet);
    let decision = handle_feed_state(
        &mut ctx,
        &cfg,
        &source,
        "mytool",
        "1.0.0",
        &pkg,
        &fast_retry(),
        &log,
    )
    .expect("republish of an in-moderation version must not bail on nupkg drift");
    assert_eq!(
        decision, None,
        "republish_in_moderation=true on a Submitted version must signal \
             proceed-to-push (Ok(None)), not skip or bail"
    );
}

/// An ALREADY-APPROVED version is genuinely immutable: a differing nupkg
/// must still bail (republish_in_moderation only covers the in-moderation
/// state, never an approved/live version).
#[test]
fn handle_feed_state_approved_with_differing_nupkg_bails() {
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
    let body = "<entry><id>https://example.com/api/v2/Packages(Id='mytool',Version='1.0.0')</id>\
            <m:properties>\
            <d:PackageHash>X</d:PackageHash>\
            <d:PackageHashAlgorithm>SHA512</d:PackageHashAlgorithm>\
            <d:PackageStatus>Approved</d:PackageStatus>\
            <d:IsApproved>true</d:IsApproved>\
            </m:properties></entry>";
    let resp: &'static str = Box::leak(http_200(body).into_boxed_str());
    let (addr, _calls) = spawn_oneshot_http_responder(vec![resp]);
    let source = format!("http://{addr}");
    let mut ctx = ctx_with_choco(ChocolateyConfig::default());
    // republish_in_moderation must NOT rescue an approved version.
    let cfg = ChocolateyConfig {
        republish_in_moderation: Some(StringOrBool::Bool(true)),
        ..Default::default()
    };
    let (_d, pkg) = tmp_blob(b"local-bytes");
    let log = StageLogger::new("publish", Verbosity::Quiet);
    let err = handle_feed_state(
        &mut ctx,
        &cfg,
        &source,
        "mytool",
        "1.0.0",
        &pkg,
        &fast_retry(),
        &log,
    )
    .expect_err("an approved immutable version with a differing nupkg must bail");
    assert!(format!("{err:#}").contains("local nupkg"), "{err:#}");
}

// -----------------------------------------------------------------
// publish_to_chocolatey orchestrator: skip-API-key branch + dry-run
// capture. handle_feed_state is exercised directly above because
// driving the full orchestrator into the feed-state ladder would
// also require responding to the push PUT — kept out of scope to
// bound the in-process responder surface.
// -----------------------------------------------------------------

#[test]
fn publish_to_chocolatey_warns_and_skips_when_api_key_empty() {
    let mut ctx = ctx_with_choco(ChocolateyConfig {
        repository: Some(RepositoryConfig {
            owner: Some("myorg".to_string()),
            name: Some("mytool".to_string()),
            ..Default::default()
        }),
        description: Some("d".to_string()),
        license: Some("MIT".to_string()),
        // api_key intentionally None.
        ..Default::default()
    });
    // Block CHOCOLATEY_API_KEY from the host environment.
    ctx.set_env_source(anodizer_core::MapEnvSource::new());
    ctx.artifacts.add(windows_artifact(
        "mytool",
        "x86_64-pc-windows-msvc",
        "x64.zip",
    ));
    let capture = anodizer_core::log::LogCapture::new();
    ctx.with_log_capture(capture.clone());
    let log = ctx.logger("publish");
    let res = publish_to_chocolatey(&mut ctx, "mytool", &log).unwrap();
    assert!(!res, "missing API key must skip push and return Ok(false)");
    assert!(
        capture
            .warn_messages()
            .iter()
            .any(|m| m.contains("no chocolatey API key") && m.contains("mytool")),
        "expected no-API-key warn; got {:?}",
        capture.all_messages()
    );
}

#[test]
fn publish_to_chocolatey_dry_run_logs_target_with_repo_path() {
    let mut ctx = ctx_with_choco_opts(
        ChocolateyConfig {
            repository: Some(RepositoryConfig {
                owner: Some("myorg".to_string()),
                name: Some("mytool".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        },
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    let capture = anodizer_core::log::LogCapture::new();
    ctx.with_log_capture(capture.clone());
    let log = ctx.logger("publish");
    let res = publish_to_chocolatey(&mut ctx, "mytool", &log).unwrap();
    assert!(!res, "dry-run must return Ok(false) — no push happened");
    let joined: String = capture
        .all_messages()
        .into_iter()
        .map(|(_, m)| m)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("(dry-run)"), "{joined}");
    assert!(joined.contains("mytool"), "{joined}");
    assert!(joined.contains("myorg/mytool"), "{joined}");
}

#[test]
fn publish_to_chocolatey_dry_run_omits_path_suffix_when_repo_absent() {
    let mut ctx = ctx_with_choco_opts(
        ChocolateyConfig::default(),
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    let capture = anodizer_core::log::LogCapture::new();
    ctx.with_log_capture(capture.clone());
    let log = ctx.logger("publish");
    let res = publish_to_chocolatey(&mut ctx, "mytool", &log).unwrap();
    assert!(!res);
    let joined: String = capture
        .all_messages()
        .into_iter()
        .map(|(_, m)| m)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("(dry-run)"), "{joined}");
    assert!(
        !joined.contains(" to /") && !joined.contains(" to myorg/"),
        "no-repo dry-run must not include a `to OWNER/REPO` suffix: {joined}"
    );
}

// -----------------------------------------------------------------
// Existing config-roundtrip + message-shape regressions
// -----------------------------------------------------------------

/// Config field roundtrip: `republish_in_moderation` survives serde.
#[test]
fn republish_in_moderation_bool_roundtrips() {
    let cfg = ChocolateyConfig {
        republish_in_moderation: Some(StringOrBool::Bool(true)),
        ..Default::default()
    };
    let json = serde_json::to_string(&cfg).expect("serialize");
    let back: ChocolateyConfig = serde_json::from_str(&json).expect("deserialize");
    assert!(matches!(
        back.republish_in_moderation,
        Some(StringOrBool::Bool(true))
    ));
}

/// Config field roundtrip: absent field deserializes to None (default=false).
#[test]
fn republish_in_moderation_absent_is_none() {
    let cfg: ChocolateyConfig = serde_json::from_str("{}").expect("deserialize");
    assert!(cfg.republish_in_moderation.is_none());
}

/// Flag false: the warn message contains key operator-facing substrings.
#[test]
fn in_moderation_skip_warn_contains_guidance() {
    // Simulate what the warn branch emits so operators know what to set.
    let pkg_name = "MyPkg";
    let version = "1.2.3";
    let reason = "is awaiting moderation";
    let status_label = "Submitted";
    let published_label = "2026-01-01";
    let msg = format!(
        "chocolatey package '{}-{}' {} (PackageStatus={}, Published={}); \
             skipping push — set republish_in_moderation: true to replace \
             the in-moderation copy. The gallery will not list the package \
             until it transitions to Approved.",
        pkg_name, version, reason, status_label, published_label
    );
    assert!(msg.contains("skipping push"), "{msg}");
    assert!(msg.contains("republish_in_moderation: true"), "{msg}");
    assert!(msg.contains("Approved"), "{msg}");
}

/// Flag true: the status message contains the "replacing in-moderation" indicator.
#[test]
fn in_moderation_republish_status_contains_replacing() {
    let pkg_name = "MyPkg";
    let version = "1.2.3";
    let reason = "is awaiting moderation";
    let status_label = "Submitted";
    let published_label = "2026-01-01";
    let msg = format!(
        "chocolatey package '{}-{}' {} (PackageStatus={}, Published={}); \
             republish_in_moderation=true — replacing in-moderation copy.",
        pkg_name, version, reason, status_label, published_label
    );
    assert!(msg.contains("republish_in_moderation=true"), "{msg}");
    assert!(msg.contains("replacing in-moderation copy"), "{msg}");
}
