use super::*;
use anodizer_core::config::{
    CrateConfig, PublishConfig, RepositoryConfig, StringOrBool, WingetConfig,
};
use anodizer_core::test_helpers::TestContextBuilder;
use anodizer_core::{PreflightCheck, PublishEvidence, Publisher, PublisherGroup};

fn winget_crate(crate_name: &str) -> CrateConfig {
    CrateConfig {
        name: crate_name.to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        publish: Some(PublishConfig {
            winget: Some(WingetConfig {
                publisher: Some("AcmeCo".to_string()),
                repository: Some(RepositoryConfig {
                    owner: Some("acme".to_string()),
                    name: Some("winget-pkgs-fork".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Add an `UploadableBinary` (the portable-installer source) for `crate_name`
/// on `target`, carrying the sha256 + binary metadata winget needs.
fn add_uploadable_binary(ctx: &mut Context, crate_name: &str, binary: &str, target: &str) {
    let mut meta = std::collections::HashMap::new();
    meta.insert("sha256".to_string(), "a".repeat(64));
    meta.insert("binary".to_string(), binary.to_string());
    ctx.artifacts.add(anodizer_core::artifact::Artifact {
        kind: anodizer_core::artifact::ArtifactKind::UploadableBinary,
        path: std::path::PathBuf::from(format!("/dist/{crate_name}-{target}")),
        name: format!("{crate_name}-{target}"),
        target: Some(target.to_string()),
        crate_name: crate_name.to_string(),
        metadata: meta,
        size: None,
    });
}

/// A per-crate winget crate carrying its own `tag_template` and
/// `package_identifier`, for the independent-version live-path test.
fn winget_crate_with(crate_name: &str, tag_template: &str, package_id: &str) -> CrateConfig {
    CrateConfig {
        name: crate_name.to_string(),
        path: ".".to_string(),
        tag_template: Some(tag_template.to_string()),
        publish: Some(PublishConfig {
            winget: Some(WingetConfig {
                publisher: Some("AcmeCo".to_string()),
                package_identifier: Some(package_id.to_string()),
                short_description: Some("A widget management tool".to_string()),
                license: Some("MIT".to_string()),
                repository: Some(RepositoryConfig {
                    owner: Some("acme".to_string()),
                    name: Some("winget-pkgs-fork".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Add a Windows zip archive (with the sha256 + url metadata the installer
/// manifest needs) for `crate_name`.
fn add_windows_zip(ctx: &mut Context, crate_name: &str) {
    let target = "x86_64-pc-windows-msvc";
    let mut meta = std::collections::HashMap::new();
    meta.insert(
        "url".to_string(),
        format!(
            "https://github.com/acme/widget/releases/download/v1.0.0/{crate_name}-{target}.zip"
        ),
    );
    meta.insert("sha256".to_string(), "a".repeat(64));
    meta.insert("format".to_string(), "zip".to_string());
    ctx.artifacts.add(anodizer_core::artifact::Artifact {
        kind: anodizer_core::artifact::ArtifactKind::Archive,
        path: std::path::PathBuf::from(format!("/dist/{crate_name}-{target}.zip")),
        name: format!("{crate_name}-{target}.zip"),
        target: Some(target.to_string()),
        crate_name: crate_name.to_string(),
        metadata: meta,
        size: None,
    });
    let mut bin_meta = std::collections::HashMap::new();
    bin_meta.insert("binary".to_string(), crate_name.to_string());
    ctx.artifacts.add(anodizer_core::artifact::Artifact {
        kind: anodizer_core::artifact::ArtifactKind::Binary,
        path: std::path::PathBuf::from(format!("/dist/{crate_name}.exe")),
        name: format!("{crate_name}.exe"),
        target: Some(target.to_string()),
        crate_name: crate_name.to_string(),
        metadata: bin_meta,
        size: None,
    });
}

/// Add a Windows `Installer` artifact (the `use: msi` / `use: nsis`
/// source) for `crate_name` on `target`, carrying the `format`, `sha256`,
/// and `url` metadata winget's installer manifest reads. `format` is the
/// installer-stage stamp (`msi` from stage-msi, `nsis` from stage-nsis);
/// `ext` is the on-disk artifact extension (`msi` / `exe`).
fn add_windows_installer(
    ctx: &mut Context,
    crate_name: &str,
    target: &str,
    format: &str,
    ext: &str,
) {
    let mut meta = std::collections::HashMap::new();
    meta.insert(
        "url".to_string(),
        format!(
            "https://github.com/acme/widget/releases/download/v1.0.0/{crate_name}-{target}.{ext}"
        ),
    );
    meta.insert("sha256".to_string(), "b".repeat(64));
    meta.insert("format".to_string(), format.to_string());
    ctx.artifacts.add(anodizer_core::artifact::Artifact {
        kind: anodizer_core::artifact::ArtifactKind::Installer,
        path: std::path::PathBuf::from(format!("/dist/{crate_name}-{target}.{ext}")),
        name: format!("{crate_name}-{target}.{ext}"),
        target: Some(target.to_string()),
        crate_name: crate_name.to_string(),
        metadata: meta,
        size: None,
    });
}

/// Add a Windows MSI `Installer` artifact that also carries the
/// deterministic `product_code` metadata stamp the MSI stage emits.
fn add_windows_msi_with_product_code(
    ctx: &mut Context,
    crate_name: &str,
    target: &str,
    product_code: &str,
) {
    let mut meta = std::collections::HashMap::new();
    meta.insert(
        "url".to_string(),
        format!(
            "https://github.com/acme/widget/releases/download/v1.0.0/{crate_name}-{target}.msi"
        ),
    );
    meta.insert("sha256".to_string(), "b".repeat(64));
    meta.insert("format".to_string(), "msi".to_string());
    meta.insert("product_code".to_string(), product_code.to_string());
    ctx.artifacts.add(anodizer_core::artifact::Artifact {
        kind: anodizer_core::artifact::ArtifactKind::Installer,
        path: std::path::PathBuf::from(format!("/dist/{crate_name}-{target}.msi")),
        name: format!("{crate_name}-{target}.msi"),
        target: Some(target.to_string()),
        crate_name: crate_name.to_string(),
        metadata: meta,
        size: None,
    });
}

/// derive-don't-require: with no `winget.product_code` configured, the
/// resolver falls back to the MSI artifact's stamped `product_code`.
#[test]
fn resolve_winget_product_code_derives_from_msi_metadata() {
    let cfg = WingetConfig {
        publisher: Some("AcmeCo".to_string()),
        ..Default::default()
    };
    let mut ctx = TestContextBuilder::new()
        .crates(vec![winget_crate("widget")])
        .build();
    add_windows_msi_with_product_code(
        &mut ctx,
        "widget",
        "x86_64-pc-windows-msvc",
        "{DERIVED-1234}",
    );

    assert_eq!(
        resolve_winget_product_code(&ctx, "widget", &cfg),
        Some("{DERIVED-1234}".to_string()),
    );
}

/// Explicit `winget.product_code` always wins over the derived MSI stamp.
#[test]
fn resolve_winget_product_code_explicit_config_wins() {
    let cfg = WingetConfig {
        publisher: Some("AcmeCo".to_string()),
        product_code: Some("{EXPLICIT-9999}".to_string()),
        ..Default::default()
    };
    let mut ctx = TestContextBuilder::new()
        .crates(vec![winget_crate("widget")])
        .build();
    add_windows_msi_with_product_code(
        &mut ctx,
        "widget",
        "x86_64-pc-windows-msvc",
        "{DERIVED-1234}",
    );

    assert_eq!(
        resolve_winget_product_code(&ctx, "widget", &cfg),
        Some("{EXPLICIT-9999}".to_string()),
    );
}

/// No config and no MSI artifact (e.g. a zip/portable winget config) yields
/// no ProductCode rather than a fabricated one.
#[test]
fn resolve_winget_product_code_none_without_msi_or_config() {
    let cfg = WingetConfig {
        publisher: Some("AcmeCo".to_string()),
        ..Default::default()
    };
    let ctx = TestContextBuilder::new()
        .crates(vec![winget_crate("widget")])
        .build();

    assert_eq!(resolve_winget_product_code(&ctx, "widget", &cfg), None);
}

/// `use: msi` must select the real `.msi` `Installer` artifacts, assign
/// `installer_type: msi`, map the arch, and emit them — NOT bail on "no
/// Windows archive". Regression guard for the dead-code installer path
/// (the zip-only filter previously discarded every Installer artifact).
#[test]
fn collect_winget_installers_selects_msi_installer() {
    let mut cfg = WingetConfig {
        publisher: Some("AcmeCo".to_string()),
        ..Default::default()
    };
    cfg.use_artifact = Some("msi".to_string());
    let mut ctx = TestContextBuilder::new()
        .crates(vec![winget_crate("widget")])
        .build();
    add_windows_installer(&mut ctx, "widget", "x86_64-pc-windows-msvc", "msi", "msi");

    let installers = collect_winget_installers(
        &ctx,
        "widget",
        &cfg,
        "widget",
        "1.0.0",
        &ctx.logger("publish"),
    )
    .expect("use: msi must collect the real installer artifact");

    assert_eq!(installers.len(), 1, "exactly one installer for one arch");
    assert_eq!(installers[0].installer_type, "msi");
    assert_eq!(installers[0].architecture, "x64");
    assert!(installers[0].url.ends_with(".msi"));
    assert_eq!(installers[0].sha256, "b".repeat(64));
}

/// `use: msi` over both x64 and arm64 installers emits a per-arch entry for
/// each, reusing `map_winget_arch`, with no spurious duplicate-arch bail.
#[test]
fn collect_winget_installers_msi_per_arch_x64_and_arm64() {
    let mut cfg = WingetConfig {
        publisher: Some("AcmeCo".to_string()),
        ..Default::default()
    };
    cfg.use_artifact = Some("msi".to_string());
    let mut ctx = TestContextBuilder::new()
        .crates(vec![winget_crate("widget")])
        .build();
    add_windows_installer(&mut ctx, "widget", "x86_64-pc-windows-msvc", "msi", "msi");
    add_windows_installer(&mut ctx, "widget", "aarch64-pc-windows-msvc", "msi", "msi");

    let installers = collect_winget_installers(
        &ctx,
        "widget",
        &cfg,
        "widget",
        "1.0.0",
        &ctx.logger("publish"),
    )
    .expect("two-arch msi must collect both");

    let mut arches: Vec<&str> = installers.iter().map(|i| i.architecture.as_str()).collect();
    arches.sort_unstable();
    assert_eq!(arches, vec!["arm64", "x64"]);
    assert!(installers.iter().all(|i| i.installer_type == "msi"));
}

/// `use: nsis` selects the `.exe` NSIS `Installer` artifacts and assigns
/// `installer_type: nsis` (so the silent switch resolves to `/S`).
#[test]
fn collect_winget_installers_selects_nsis_installer() {
    let mut cfg = WingetConfig {
        publisher: Some("AcmeCo".to_string()),
        ..Default::default()
    };
    cfg.use_artifact = Some("nsis".to_string());
    let mut ctx = TestContextBuilder::new()
        .crates(vec![winget_crate("widget")])
        .build();
    add_windows_installer(&mut ctx, "widget", "x86_64-pc-windows-msvc", "nsis", "exe");

    let installers = collect_winget_installers(
        &ctx,
        "widget",
        &cfg,
        "widget",
        "1.0.0",
        &ctx.logger("publish"),
    )
    .expect("use: nsis must collect the real installer artifact");

    assert_eq!(installers.len(), 1);
    assert_eq!(installers[0].installer_type, "nsis");
    assert_eq!(installers[0].architecture, "x64");
    assert!(installers[0].url.ends_with(".exe"));
}

/// The selected msi installer feeds an end-to-end manifest carrying the
/// derived silent switch (`/quiet`) — the install logic that was reachable
/// only via synthetic test data before FIX 1.
#[test]
fn msi_installer_manifest_emits_silent_switch() {
    let mut cfg = WingetConfig {
        publisher: Some("AcmeCo".to_string()),
        ..Default::default()
    };
    cfg.use_artifact = Some("msi".to_string());
    let mut ctx = TestContextBuilder::new()
        .crates(vec![winget_crate("widget")])
        .build();
    add_windows_installer(&mut ctx, "widget", "x86_64-pc-windows-msvc", "msi", "msi");

    let installers = collect_winget_installers(
        &ctx,
        "widget",
        &cfg,
        "widget",
        "1.0.0",
        &ctx.logger("publish"),
    )
    .expect("collect ok");

    let params = WingetManifestParams {
        package_id: "AcmeCo.Widget",
        name: "widget",
        package_name: None,
        version: "1.0.0",
        description: "An app",
        short_description: "An app",
        license: "MIT",
        license_url: None,
        publisher: "AcmeCo",
        publisher_url: None,
        publisher_support_url: None,
        privacy_url: None,
        author: None,
        copyright: None,
        copyright_url: None,
        homepage: None,
        release_notes: None,
        release_notes_url: None,
        installation_notes: None,
        tags: None,
        dependencies: &[],
        installers,
        product_code: None,
        release_date: None,
        moniker: None,
        upgrade_behavior: "install",
        documentations: &[],
        default_locale: "en-US",
    };
    let (_ver, inst, _locale) = generate_manifests(&params).unwrap();
    assert!(inst.contains("InstallerType: msi"), "got:\n{inst}");
    assert!(inst.contains("Silent: /quiet"), "got:\n{inst}");
    assert!(!inst.contains("NestedInstallerType"), "msi is not nested");
}

/// A `silent_switch` is only meaningful for an actual installer
/// (msi/wix/exe/nsis) winget runs. When the only Windows artifacts are
/// zip/portable (which winget unpacks, never runs), the switch is dead
/// config and the render must warn once so the author isn't misled into
/// thinking silencing is in effect.
#[test]
fn silent_switch_warns_when_only_zip_artifacts() {
    let mut crate_cfg = winget_crate_with("widget", "v{{ .Version }}", "AcmeCo.Widget");
    crate_cfg
        .publish
        .as_mut()
        .unwrap()
        .winget
        .as_mut()
        .unwrap()
        .silent_switch = Some("/qn".to_string());

    let capture = anodizer_core::log::LogCapture::new();
    let mut ctx = TestContextBuilder::new().crates(vec![crate_cfg]).build();
    ctx.with_log_capture(capture.clone());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("RawVersion", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    add_windows_zip(&mut ctx, "widget");

    render_winget_manifests_for_crate(&ctx, "widget", &ctx.logger("publish"))
        .expect("render ok")
        .expect("widget not skipped");

    let warns = capture.warn_messages();
    assert!(
        warns.iter().any(|m| m.contains("widget")
            && m.contains("silent_switch")
            && m.contains("no installer-type artifact")),
        "expected one WARN that silent_switch is ignored with no installer artifact; \
             got: {warns:?}"
    );
}

/// Mirror: with no `silent_switch` configured, the zip-only render must
/// stay silent — the diagnostic is gated on the switch actually being set.
#[test]
fn no_silent_switch_warning_when_unset() {
    let crate_cfg = winget_crate_with("widget", "v{{ .Version }}", "AcmeCo.Widget");

    let capture = anodizer_core::log::LogCapture::new();
    let mut ctx = TestContextBuilder::new().crates(vec![crate_cfg]).build();
    ctx.with_log_capture(capture.clone());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("RawVersion", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    add_windows_zip(&mut ctx, "widget");

    render_winget_manifests_for_crate(&ctx, "widget", &ctx.logger("publish"))
        .expect("render ok")
        .expect("widget not skipped");

    let warns = capture.warn_messages();
    assert!(
        !warns.iter().any(|m| m.contains("silent_switch")),
        "no silent_switch configured → no silent_switch warning; got: {warns:?}"
    );
}

/// LIVE PATH, workspace per-crate INDEPENDENT-version mode: the publisher's
/// per-crate render must stamp EACH crate's OWN version, not the first
/// crate's. The live `run` loop wraps each `publish_to_winget` in
/// `with_published_crate_scope`; this drives that same helper and asserts the
/// rendered manifest carries the scoped crate's version. Fails against the
/// pre-fix code that rendered every crate against the global first-crate
/// `Version`.
#[test]
fn live_per_crate_render_stamps_each_crate_own_version() {
    let alpha = winget_crate_with("alpha", "alpha-v{{ .Version }}", "AcmeCo.Alpha");
    let beta = winget_crate_with("beta", "beta-v{{ .Version }}", "AcmeCo.Beta");

    // One ctx, both crates, global Version = first crate's (2.0.0).
    let mut ctx = TestContextBuilder::new()
        .snapshot(true)
        .crates(vec![alpha, beta])
        .build();
    ctx.template_vars_mut().set("Version", "2.0.0");
    ctx.template_vars_mut().set("RawVersion", "2.0.0");
    ctx.template_vars_mut().set("Tag", "alpha-v2.0.0");
    add_windows_zip(&mut ctx, "alpha");
    add_windows_zip(&mut ctx, "beta");

    // Per-crate resolver: alpha @ 2.0.0, beta @ 3.1.0.
    let resolver = |_: &Context, c: &CrateConfig| {
        Some(match c.name.as_str() {
            "beta" => "3.1.0".to_string(),
            _ => "2.0.0".to_string(),
        })
    };

    // beta renders UNDER ITS OWN SCOPE → 3.1.0, the version a real release
    // would stamp; never the global first-crate 2.0.0.
    let beta_yaml =
        crate::publisher_helpers::with_published_crate_scope(&mut ctx, "beta", &resolver, |ctx| {
            let r = render_winget_manifests_for_crate(ctx, "beta", &ctx.logger("publish"))?
                .expect("beta not skipped");
            Ok(r.version_yaml)
        })
        .expect("scoped render ok");
    assert!(
        beta_yaml.contains("PackageVersion: 3.1.0"),
        "live per-crate render must stamp beta's OWN version 3.1.0; got:\n{beta_yaml}"
    );
    assert!(
        !beta_yaml.contains("PackageVersion: 2.0.0"),
        "beta's live manifest must NOT carry the first crate's version; got:\n{beta_yaml}"
    );

    // alpha renders 2.0.0 under its own scope (single/lockstep parity: the
    // per-crate scope reproduces the same version it already had).
    let alpha_yaml =
        crate::publisher_helpers::with_published_crate_scope(&mut ctx, "alpha", &resolver, |ctx| {
            let r = render_winget_manifests_for_crate(ctx, "alpha", &ctx.logger("publish"))?
                .expect("alpha not skipped");
            Ok(r.version_yaml)
        })
        .expect("scoped render ok");
    assert!(
        alpha_yaml.contains("PackageVersion: 2.0.0"),
        "alpha must render its own 2.0.0; got:\n{alpha_yaml}"
    );
}

/// Per-crate, no leakage: each crate's Moniker derives from its OWN
/// single binary name (`add_windows_zip` stamps `binary = crate_name`).
/// alpha's manifest must carry `Moniker: alpha` and never `beta`, and
/// vice-versa — the recurring cross-crate-leakage bug family.
#[test]
fn live_per_crate_moniker_no_leakage() {
    let alpha = winget_crate_with("alpha", "alpha-v{{ .Version }}", "AcmeCo.Alpha");
    let beta = winget_crate_with("beta", "beta-v{{ .Version }}", "AcmeCo.Beta");

    let mut ctx = TestContextBuilder::new()
        .snapshot(true)
        .crates(vec![alpha, beta])
        .build();
    ctx.template_vars_mut().set("Version", "2.0.0");
    ctx.template_vars_mut().set("RawVersion", "2.0.0");
    ctx.template_vars_mut().set("Tag", "alpha-v2.0.0");
    add_windows_zip(&mut ctx, "alpha");
    add_windows_zip(&mut ctx, "beta");

    let alpha_locale = render_winget_manifests_for_crate(&ctx, "alpha", &ctx.logger("publish"))
        .unwrap()
        .expect("alpha not skipped")
        .locale_yaml;
    let beta_locale = render_winget_manifests_for_crate(&ctx, "beta", &ctx.logger("publish"))
        .unwrap()
        .expect("beta not skipped")
        .locale_yaml;

    assert!(
        alpha_locale.contains("Moniker: alpha"),
        "alpha must derive its own moniker; got:\n{alpha_locale}"
    );
    assert!(
        !alpha_locale.contains("Moniker: beta"),
        "alpha manifest must NOT carry beta's moniker; got:\n{alpha_locale}"
    );
    assert!(
        beta_locale.contains("Moniker: beta"),
        "beta must derive its own moniker; got:\n{beta_locale}"
    );
    assert!(
        !beta_locale.contains("Moniker: alpha"),
        "beta manifest must NOT carry alpha's moniker; got:\n{beta_locale}"
    );
}

/// Single-crate live path: the default install behavior and a
/// derived moniker both surface through the real per-crate render.
#[test]
fn live_single_crate_default_moniker_and_upgrade_behavior() {
    let demo = winget_crate_with("demo", "v{{ .Version }}", "AcmeCo.Demo");
    let mut ctx = TestContextBuilder::new()
        .snapshot(true)
        .crates(vec![demo])
        .build();
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("RawVersion", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    add_windows_zip(&mut ctx, "demo");

    let rendered = render_winget_manifests_for_crate(&ctx, "demo", &ctx.logger("publish"))
        .unwrap()
        .expect("demo not skipped");
    assert!(
        rendered.locale_yaml.contains("Moniker: demo"),
        "single-binary moniker derives from the bin name; got:\n{}",
        rendered.locale_yaml
    );
    assert!(
        rendered.installer_yaml.contains("UpgradeBehavior: install"),
        "default upgrade behavior must be install; got:\n{}",
        rendered.installer_yaml
    );
    assert!(
        !rendered.installer_yaml.contains("uninstallPrevious"),
        "default must not be the clobbering uninstallPrevious"
    );
}

/// The shard-guard and the live collector must agree on what a winget
/// Windows installer is. A linux-only `UploadableBinary` (the portable
/// path) is NOT a Windows installer, so the guard returns `false` — letting
/// the schema validator skip the crate rather than drive
/// `collect_winget_installers` into its "no Windows artifact" bail. The
/// shared `WingetArtifactFilters::matches` Windows predicate keeps the two
/// from drifting; this pins the portable-binary branch of that agreement.
#[test]
fn guard_skips_linux_only_portable_binary() {
    let cfg = WingetConfig {
        publisher: Some("AcmeCo".to_string()),
        ..Default::default()
    };
    let mut ctx = TestContextBuilder::new()
        .crates(vec![winget_crate("demo")])
        .build();
    add_uploadable_binary(&mut ctx, "demo", "demo", "x86_64-unknown-linux-gnu");
    assert!(
        !crate_has_winget_installer_artifacts(&ctx, "demo", &cfg),
        "a linux-only portable binary is not a Windows installer; the guard \
             must return false so the validator skips rather than bails"
    );
}

/// The positive half: a Windows `UploadableBinary` IS a winget installer, so
/// the guard returns `true` and validation proceeds. Confirms the Windows
/// predicate the guard shares with the collector counts the real case.
#[test]
fn guard_counts_windows_portable_binary() {
    let cfg = WingetConfig {
        publisher: Some("AcmeCo".to_string()),
        ..Default::default()
    };
    let mut ctx = TestContextBuilder::new()
        .crates(vec![winget_crate("demo")])
        .build();
    add_uploadable_binary(&mut ctx, "demo", "demo", "x86_64-pc-windows-msvc");
    assert!(
        crate_has_winget_installer_artifacts(&ctx, "demo", &cfg),
        "a windows portable binary is a winget installer; the guard must count it"
    );
}

#[test]
fn winget_publisher_classification() {
    let p = WingetPublisher::new();
    assert_eq!(p.name(), "winget");
    assert_eq!(p.group(), PublisherGroup::Submitter);
    assert!(!p.required());
    assert_eq!(
        p.rollback_scope_needed(),
        Some("GITHUB_TOKEN pull_request:write")
    );
}

/// `--crate x` selects only the skip_upload:true entry; an active
/// sibling `y` outside the selection must not keep the publisher live.
#[test]
fn config_fully_inactive_true_when_selected_crate_is_skipped_sibling_active() {
    let mut skipped = winget_crate("x");
    skipped
        .publish
        .as_mut()
        .unwrap()
        .winget
        .as_mut()
        .unwrap()
        .skip_upload = Some(StringOrBool::Bool(true));
    let ctx = TestContextBuilder::new()
        .crates(vec![skipped, winget_crate("y")])
        .selected_crates(vec!["x".to_string()])
        .build();

    assert!(
        WingetPublisher::new().config_fully_inactive(&ctx),
        "--crate x selects only the skip_upload:true entry; active sibling y is \
             out of scope and must not keep the publisher live"
    );
}

/// Empty `--crate` selection means "all crates" — an active entry with
/// no `--crate` filter applied must keep the publisher live.
#[test]
fn config_fully_inactive_false_with_empty_selection_and_active_entry() {
    let ctx = TestContextBuilder::new()
        .crates(vec![winget_crate("x")])
        .build();

    assert!(
        !WingetPublisher::new().config_fully_inactive(&ctx),
        "empty selection means \"all crates\"; an active entry must keep the \
             publisher live"
    );
}

#[test]
fn winget_preflight_defaults_to_pass() {
    let ctx = TestContextBuilder::new().build();
    let p = WingetPublisher::new();
    assert!(matches!(
        p.preflight(&ctx).expect("preflight ok"),
        PreflightCheck::Pass
    ));
}

/// Every winget publish lands as a PR against the upstream index;
/// `gh pr create` is the preferred transport with a full REST-API
/// fallback, so `gh` is ADVISORY — recommended, never a blocker.
#[test]
fn winget_advisory_requirements_emit_gh_when_active() {
    let ctx = TestContextBuilder::new()
        .crates(vec![winget_crate("demo")])
        .build();
    let reqs = WingetPublisher::new().advisory_requirements(&ctx);
    assert!(
        reqs.iter().any(|r| matches!(
            r,
            anodizer_core::EnvRequirement::Tool { name } if name == "gh"
        )),
        "active winget entry must recommend gh: {reqs:?}"
    );
}

#[test]
fn winget_advisory_requirements_empty_when_all_entries_skipped() {
    let mut c = winget_crate("demo");
    if let Some(w) = c.publish.as_mut().and_then(|p| p.winget.as_mut()) {
        w.skip_upload = Some(anodizer_core::config::StringOrBool::Bool(true));
    }
    let ctx = TestContextBuilder::new().crates(vec![c]).build();
    let reqs = WingetPublisher::new().advisory_requirements(&ctx);
    assert!(
        reqs.is_empty(),
        "every entry skipped ⇒ no advisory recommendations: {reqs:?}"
    );
}

#[test]
fn winget_rollback_warns_when_no_targets_recorded() {
    let capture = anodizer_core::log::LogCapture::new();
    let mut ctx = TestContextBuilder::new().build();
    ctx.with_log_capture(capture.clone());
    let evidence = PublishEvidence::new("winget");
    let p = WingetPublisher::new();
    assert!(p.rollback(&mut ctx, &evidence).is_ok());

    let warns = capture.warn_messages();
    assert!(
        warns.iter().any(|m| m.contains("winget")
            && m.contains("submitted PR targets")
            && m.contains("verify")),
        "expected captured warn naming publisher + target-noun + 'verify'; got: {warns:?}"
    );
}

#[test]
fn winget_rollback_warns_per_target_when_evidence_present() {
    let mut ctx = TestContextBuilder::new().build();
    let mut evidence = PublishEvidence::new("winget");
    evidence.extra =
        anodizer_core::PublishEvidenceExtra::Winget(anodizer_core::publish_evidence::WingetExtra {
            winget_targets: vec![
                WingetTarget {
                    target: "AcmeCo.demo".into(),
                    crate_name: "demo".into(),
                    package_id: "AcmeCo.demo".into(),
                    version: "1.2.3".into(),
                    upstream_owner: "microsoft".into(),
                    upstream_repo: "winget-pkgs".into(),
                    fork_owner: "acme".into(),
                    branch: "AcmeCo.demo-1.2.3".into(),
                },
                WingetTarget {
                    target: "AcmeCo.widget".into(),
                    crate_name: "widget".into(),
                    package_id: "AcmeCo.widget".into(),
                    version: "1.2.3".into(),
                    upstream_owner: "microsoft".into(),
                    upstream_repo: "winget-pkgs".into(),
                    fork_owner: "acme".into(),
                    branch: "AcmeCo.widget-1.2.3".into(),
                },
            ],
        });
    let p = WingetPublisher::new();
    assert!(p.rollback(&mut ctx, &evidence).is_ok());
    assert_eq!(decode_winget_targets(&evidence.extra).len(), 2);
}

#[test]
fn winget_target_extra_roundtrips() {
    let original = vec![WingetTarget {
        target: "AcmeCo.demo".into(),
        crate_name: "demo".into(),
        package_id: "AcmeCo.demo".into(),
        version: "1.2.3".into(),
        upstream_owner: "microsoft".into(),
        upstream_repo: "winget-pkgs".into(),
        fork_owner: "acme".into(),
        branch: "AcmeCo.demo-1.2.3".into(),
    }];
    let extra =
        anodizer_core::PublishEvidenceExtra::Winget(anodizer_core::publish_evidence::WingetExtra {
            winget_targets: original.clone(),
        });
    let decoded = decode_winget_targets(&extra);
    assert_eq!(decoded, original);
}

#[test]
fn winget_target_extra_carries_no_secret_material() {
    // Structural pin: build a typed-variant evidence and assert
    // (a) no credential-shaped keys appear AND (b) the
    // operator-public PR coordinates are preserved.
    let mut e = PublishEvidence::new("winget");
    e.extra =
        anodizer_core::PublishEvidenceExtra::Winget(anodizer_core::publish_evidence::WingetExtra {
            winget_targets: vec![WingetTarget {
                target: "AcmeCo.demo".into(),
                crate_name: "demo".into(),
                package_id: "AcmeCo.demo".into(),
                version: "1.2.3".into(),
                upstream_owner: "microsoft".into(),
                upstream_repo: "winget-pkgs".into(),
                fork_owner: "acme".into(),
                branch: "AcmeCo.demo-1.2.3".into(),
            }],
        });
    let s = serde_json::to_string(&e).expect("serialize");
    assert!(!s.contains("\"token\":"), "{s}");
    assert!(!s.contains("\"pat\":"), "{s}");
    assert!(!s.contains("\"auth\":"), "{s}");
    assert!(!s.contains("\"password\":"), "{s}");
    assert!(!s.contains("\"secret\":"), "{s}");
    assert!(!s.contains("\"api_key\":"), "{s}");
    // Positive shape: PR coordinates present.
    assert!(s.contains("\"package_id\":\"AcmeCo.demo\""), "{s}");
    assert!(s.contains("\"upstream_owner\":\"microsoft\""), "{s}");
    assert!(s.contains("\"upstream_repo\":\"winget-pkgs\""), "{s}");
    assert!(s.contains("\"fork_owner\":\"acme\""), "{s}");
    assert!(s.contains("\"branch\":\"AcmeCo.demo-1.2.3\""), "{s}");
}

#[test]
fn winget_collect_target_uses_explicit_package_identifier() {
    let mut c = winget_crate("demo");
    if let Some(p) = c.publish.as_mut()
        && let Some(w) = p.winget.as_mut()
    {
        w.package_identifier = Some("ExplicitOrg.Demo".to_string());
    }
    let ctx = TestContextBuilder::new().crates(vec![c]).build();
    let t = collect_winget_target(&ctx, "demo", &ctx.logger("publish"))
        .expect("render ok")
        .expect("target");
    assert_eq!(t.package_id, "ExplicitOrg.Demo");
    assert_eq!(t.upstream_owner, "microsoft");
    assert_eq!(t.upstream_repo, "winget-pkgs");
    assert_eq!(t.fork_owner, "acme");
}

#[test]
fn winget_collect_target_auto_generates_package_identifier() {
    let ctx = TestContextBuilder::new()
        .crates(vec![winget_crate("demo")])
        .build();
    let t = collect_winget_target(&ctx, "demo", &ctx.logger("publish"))
        .expect("render ok")
        .expect("target");
    // Publisher "AcmeCo" + name "demo" → "AcmeCo.demo".
    assert_eq!(t.package_id, "AcmeCo.demo");
    assert!(t.branch.starts_with("AcmeCo.demo-"));
}

// Log-message helpers — the operator-facing log strings the publisher
// emits at each boundary. The failure mode these guard against: a
// publisher whose iteration loop hits only silently-`continue`d
// crates returns Ok with an empty evidence record, which the
// dispatch table then reports as "succeeded" — indistinguishable
// from a real PR push. Every helper below must produce a line the
// operator can grep the publish log for.

#[test]
fn run_per_crate_start_message_names_crate() {
    let msg = run_per_crate_start_message("demo");
    assert!(
        msg.starts_with("starting per-crate winget publish"),
        "{msg}"
    );
    assert!(msg.contains("'demo'"), "{msg}");
}

#[test]
fn run_done_message_reports_processed_count() {
    let msg = run_done_message(2);
    assert!(msg.starts_with("finished winget publish"), "{msg}");
    assert!(msg.contains("2 configured crate(s) processed"), "{msg}");
}

#[test]
fn run_no_eligible_crates_warning_names_remediation() {
    let msg = run_no_eligible_crates_warning(5);
    assert!(msg.starts_with("winget publisher registered"), "{msg}");
    assert!(msg.contains("0 of 5 effective"), "{msg}");
    assert!(msg.contains("nothing pushed"), "{msg}");
    // The warning must point the operator at the remediation surface
    // (--crate / --all selection) — otherwise it's noise.
    assert!(msg.contains("--crate"), "{msg}");
    assert!(msg.contains("--all"), "{msg}");
}

#[test]
fn run_no_eligible_crates_warning_handles_empty_selection() {
    // The zero-effective case (no crate carries a `publish.winget`
    // block) must produce the remediation string with a 0/0 count.
    // The warn helper must not panic or omit the remediation text in
    // this shape.
    let msg = run_no_eligible_crates_warning(0);
    assert!(msg.starts_with("winget publisher registered"), "{msg}");
    assert!(msg.contains("0 of 0 effective"), "{msg}");
    assert!(msg.contains("nothing pushed"), "{msg}");
    assert!(msg.contains("--crate"), "{msg}");
    assert!(msg.contains("--all"), "{msg}");
}

/// Run the publisher end-to-end in dry-run mode against a context
/// that selects a winget-configured crate. Verifies the run path is
/// wired (returns Ok, records target evidence). The log lines
/// themselves are written to stderr and asserted indirectly via the
/// helper-string tests above.
#[test]
fn winget_publisher_run_dry_run_records_target() {
    let repo = crate::testing::hermetic_tagged_repo();
    let mut ctx = TestContextBuilder::new()
        .crates(vec![winget_crate("demo")])
        .selected_crates(vec!["demo".to_string()])
        .dry_run(true)
        .project_root(repo.path().to_path_buf())
        .build();
    let p = WingetPublisher::new();
    let evidence = p.run(&mut ctx).expect("dry-run publisher.run");
    // primary_ref + extra.winget_targets must reflect that the run
    // path actually visited the demo crate (not silently skipped).
    // Without these the publisher would report "succeeded" with
    // nothing recorded.
    let primary = evidence
        .primary_ref
        .as_deref()
        .expect("primary_ref must be set after a real run");
    assert!(
        primary.starts_with("https://github.com/microsoft/winget-pkgs/pulls?q=head%3Aacme%3A"),
        "primary_ref shape: {primary}"
    );
    let targets = decode_winget_targets(&evidence.extra);
    assert_eq!(targets.len(), 1, "{:?}", targets);
    assert_eq!(targets[0].crate_name, "demo");
}

/// When the publisher is registered (a crate has a winget block) but
/// the selected-crates filter excludes every winget-configured
/// crate, the run path must still return Ok (so the dispatch chain
/// doesn't abort), but record no targets — and the operator-facing
/// warning helper must produce a remediation-pointing string.
#[test]
fn winget_publisher_run_no_eligible_crates_returns_empty_evidence() {
    let mut ctx = TestContextBuilder::new()
        .crates(vec![
            winget_crate("demo"),
            CrateConfig {
                name: "other".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                publish: Some(PublishConfig::default()),
                ..Default::default()
            },
        ])
        // Select only the non-winget crate — the publisher should
        // still be registered (because `demo` has a block) but its
        // run path will iterate zero winget-configured crates.
        .selected_crates(vec!["other".to_string()])
        .dry_run(true)
        .build();
    let p = WingetPublisher::new();
    let evidence = p.run(&mut ctx).expect("publisher.run ok");
    assert!(
        evidence.primary_ref.is_none(),
        "no winget-eligible crate selected, primary_ref must be unset"
    );
    let targets = decode_winget_targets(&evidence.extra);
    assert!(
        targets.is_empty(),
        "no winget-eligible crate selected, targets must be empty"
    );
}

/// Default-empty `selected_crates` (the `ContextOptions::default()`
/// shape, produced by `release --publish-only` with no
/// `--crate`/`--all`) MUST resolve to implicit-all over every crate
/// carrying a `publish.winget` block. Without this the publisher
/// would emit `run_done_message(0)` and report `succeeded` with zero
/// winget activity in the publish log — the root-cause failure mode
/// this regression test pins against.
#[test]
fn winget_publisher_run_empty_selection_publishes_all_configured() {
    let repo = crate::testing::hermetic_tagged_repo();
    let mut ctx = TestContextBuilder::new()
        .crates(vec![winget_crate("demo")])
        // selected_crates intentionally left at the default Vec::new()
        .dry_run(true)
        .project_root(repo.path().to_path_buf())
        .build();
    let p = WingetPublisher::new();
    let evidence = p.run(&mut ctx).expect("publisher.run ok");
    let primary = evidence
        .primary_ref
        .as_deref()
        .expect("empty selection must implicitly publish every winget-configured crate");
    assert!(
        primary.starts_with("https://github.com/microsoft/winget-pkgs/pulls?q=head%3Aacme%3A"),
        "primary_ref shape: {primary}"
    );
    let targets = decode_winget_targets(&evidence.extra);
    assert_eq!(
        targets.len(),
        1,
        "empty selection must produce one target per winget-configured crate"
    );
    assert_eq!(targets[0].crate_name, "demo");
}

/// Implicit-all must still produce empty evidence when zero crates
/// carry a `publish.winget` block — the warn helper fires on
/// "registered but nothing eligible", which is meaningful only when
/// no crate is configured at all.
#[test]
fn winget_publisher_run_empty_selection_with_no_configured_crate_returns_empty_evidence() {
    let mut ctx = TestContextBuilder::new()
        .crates(vec![CrateConfig {
            name: "other".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            publish: Some(PublishConfig::default()),
            ..Default::default()
        }])
        .dry_run(true)
        .build();
    let p = WingetPublisher::new();
    let evidence = p.run(&mut ctx).expect("publisher.run ok");
    assert!(
        evidence.primary_ref.is_none(),
        "no winget-configured crate present, primary_ref must be unset"
    );
    let targets = decode_winget_targets(&evidence.extra);
    assert!(
        targets.is_empty(),
        "no winget-configured crate present, targets must be empty"
    );
}

#[test]
fn winget_publisher_visible_work_contract() {
    use crate::testing::assert_publisher_visible_work_contract;
    let repo = crate::testing::hermetic_tagged_repo();
    let mut ctx = TestContextBuilder::new()
        .crates(vec![winget_crate("demo")])
        .selected_crates(vec!["demo".to_string()])
        .dry_run(true)
        .project_root(repo.path().to_path_buf())
        .build();
    let p = WingetPublisher::new();
    assert_publisher_visible_work_contract(&p, &mut ctx);
}

/// A windows archive that arrives at the winget publisher without
/// `sha256` metadata MUST bail with an actionable error, not emit
/// `InstallerSha256: ''` (which the winget validation pipeline
/// rejects). Pins the bail message + the downstream-consequence
/// hint pointing the operator at the checksum stage.
#[test]
fn winget_archive_without_sha256_metadata_bails_with_actionable_error() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use std::collections::HashMap;

    let mut crate_cfg = winget_crate("demo");
    // publish_to_winget requires license + short_description (no implicit fallbacks).
    if let Some(pub_cfg) = crate_cfg.publish.as_mut()
        && let Some(w) = pub_cfg.winget.as_mut()
    {
        w.license = Some("MIT".to_string());
        w.short_description = Some("Demo tool".to_string());
    }

    let mut ctx = TestContextBuilder::new()
        .crates(vec![crate_cfg])
        .selected_crates(vec!["demo".to_string()])
        .build();

    let mut md = HashMap::new();
    md.insert("format".to_string(), "zip".to_string());
    md.insert("url".to_string(), "https://example.com/x.zip".to_string());
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: std::path::PathBuf::from("dist/demo-1.0.0-windows-amd64.zip"),
        target: Some("x86_64-pc-windows-msvc".to_string()),
        crate_name: "demo".to_string(),
        metadata: md,
        size: None,
    });

    use anodizer_core::log::Verbosity;
    let log = StageLogger::new("test-stage", Verbosity::Normal);
    let err = publish_to_winget(&mut ctx, "demo", &log)
        .expect_err("publish_to_winget must bail on empty sha256");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("no sha256 metadata"),
        "error must name the missing-sha256 root cause, got: {msg}"
    );
    assert!(
        msg.contains("checksum stage"),
        "error must point operator at the checksum stage, got: {msg}"
    );
    assert!(
        msg.contains("rejected by winget validation"),
        "error must explain downstream consequence, got: {msg}"
    );
}
