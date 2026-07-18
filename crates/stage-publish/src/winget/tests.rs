#![allow(clippy::field_reassign_with_default)]

use super::*;

// -----------------------------------------------------------------------
// generate_manifest tests
// -----------------------------------------------------------------------

fn default_params<'a>() -> WingetManifestParams<'a> {
    WingetManifestParams {
        package_id: "Org.MyTool",
        name: "mytool",
        package_name: None,
        version: "1.0.0",
        description: "A great tool",
        short_description: "A great tool",
        license: "MIT",
        license_url: None,
        publisher: "My Org",
        publisher_url: Some("https://example.com"),
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
        installers: vec![WingetInstallerItem {
            architecture: "x64".to_string(),
            url: "https://example.com/mytool-1.0.0-windows-amd64.zip".to_string(),
            sha256: "deadbeef1234567890abcdef".to_string(),
            installer_type: "zip".to_string(),
            binaries: vec![],
            wrap_in_directory: None,
            commands: vec![],
            silent_switch_override: None,
        }],
        product_code: None,
        release_date: None,
        moniker: Some("mytool"),
        upgrade_behavior: "install",
        documentations: &[],
        default_locale: "en-US",
    }
}

/// Default locale stays `en-US` in all three manifests (byte-level
/// stability for existing configs).
#[test]
fn test_generate_manifests_default_locale_en_us() {
    let params = default_params();
    let (ver, inst, locale) = generate_manifests(&params).unwrap();
    assert!(
        ver.contains("DefaultLocale: en-US"),
        "version manifest:\n{ver}"
    );
    assert!(
        inst.contains("InstallerLocale: en-US"),
        "installer manifest:\n{inst}"
    );
    assert!(
        locale.contains("PackageLocale: en-US"),
        "locale manifest:\n{locale}"
    );
}

/// A configured `default_locale` reaches the version, installer, and
/// locale manifests, with no en-US residue.
#[test]
fn test_generate_manifests_custom_locale() {
    let mut params = default_params();
    params.default_locale = "pt-BR";
    let (ver, inst, locale) = generate_manifests(&params).unwrap();
    assert!(
        ver.contains("DefaultLocale: pt-BR"),
        "version manifest:\n{ver}"
    );
    assert!(
        inst.contains("InstallerLocale: pt-BR"),
        "installer manifest:\n{inst}"
    );
    assert!(
        locale.contains("PackageLocale: pt-BR"),
        "locale manifest:\n{locale}"
    );
    for y in [&ver, &inst, &locale] {
        assert!(!y.contains("en-US"), "no en-US residue expected:\n{y}");
    }
}

/// The locale manifest file name carries the configured locale
/// (`<PackageIdentifier>.locale.<locale>.yaml`).
#[test]
fn test_write_manifests_locale_filename() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = write_winget_manifests_to_disk(
        tmp.path(),
        "Org.MyTool",
        "1.0.0",
        None,
        "pt-BR",
        "v",
        "i",
        "l",
    )
    .unwrap();
    assert!(dir.join("Org.MyTool.locale.pt-BR.yaml").is_file());
    assert!(dir.join("Org.MyTool.yaml").is_file());
    assert!(dir.join("Org.MyTool.installer.yaml").is_file());
    assert!(!dir.join("Org.MyTool.locale.en-US.yaml").exists());
}

#[test]
fn test_generate_3file_manifests() {
    let params = default_params();
    let (ver, inst, locale) = generate_manifests(&params).unwrap();

    assert!(ver.contains("ManifestType: version"));
    assert!(ver.contains("PackageIdentifier: Org.MyTool"));

    assert!(inst.contains("ManifestType: installer"));
    assert!(inst.contains("InstallerSha256: deadbeef1234567890abcdef"));
    // Default upgrade behavior is `install` (correct for portable-zip
    // tools); never the clobbering `uninstallPrevious`.
    assert!(inst.contains("UpgradeBehavior: install"));
    assert!(!inst.contains("uninstallPrevious"));
    // zip/portable installers carry NO silent switch.
    assert!(!inst.contains("InstallerSwitches"));
    // Nested installer fields for zip type
    assert!(inst.contains("NestedInstallerType: portable"));
    assert!(inst.contains("RelativeFilePath: mytool.exe"));
    assert!(inst.contains("PortableCommandAlias: mytool"));

    assert!(locale.contains("ManifestType: defaultLocale"));
    assert!(locale.contains("ShortDescription: A great tool"));
    assert!(locale.contains("Moniker: mytool"));
}

#[test]
fn test_generate_manifests_with_deps() {
    let deps = vec![anodizer_core::config::WingetDependency {
        package_identifier: "Foo.Bar".to_string(),
        minimum_version: Some("1.0.0".to_string()),
        ..Default::default()
    }];
    let mut params = default_params();
    params.dependencies = &deps;
    let (_, inst, _) = generate_manifests(&params).unwrap();
    assert!(inst.contains("PackageDependencies:"));
    assert!(inst.contains("PackageIdentifier: Foo.Bar"));
    assert!(inst.contains("MinimumVersion: 1.0.0"));
}

/// A `default_params()` clone carrying both an `x64` and an `arm64`
/// portable installer, so per-installer dependency scoping can be asserted.
fn dual_arch_installers() -> Vec<WingetInstallerItem> {
    vec![
        WingetInstallerItem {
            architecture: "x64".to_string(),
            url: "https://example.com/mytool-1.0.0-windows-amd64.zip".to_string(),
            sha256: "deadbeefx64".to_string(),
            installer_type: "zip".to_string(),
            binaries: vec![],
            wrap_in_directory: None,
            commands: vec![],
            silent_switch_override: None,
        },
        WingetInstallerItem {
            architecture: "arm64".to_string(),
            url: "https://example.com/mytool-1.0.0-windows-arm64.zip".to_string(),
            sha256: "deadbeefarm64".to_string(),
            installer_type: "zip".to_string(),
            binaries: vec![],
            wrap_in_directory: None,
            commands: vec![],
            silent_switch_override: None,
        },
    ]
}

/// Count how many times `needle` appears in `haystack`.
fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

/// An unscoped dependency (no `architectures`) attaches to EVERY installer.
#[test]
fn winget_unscoped_dependency_attaches_to_all_installers() {
    let deps = vec![anodizer_core::config::WingetDependency {
        package_identifier: "Acme.CommonRuntime".to_string(),
        minimum_version: None,
        architectures: None,
    }];
    let mut params = default_params();
    params.installers = dual_arch_installers();
    params.dependencies = &deps;
    let (_, inst, _) = generate_manifests(&params).unwrap();
    // Both the x64 and arm64 installers carry the dependency → two blocks.
    assert_eq!(count_occurrences(&inst, "PackageDependencies:"), 2);
    assert_eq!(
        count_occurrences(&inst, "PackageIdentifier: Acme.CommonRuntime"),
        2
    );
}

/// An empty `architectures: []` is treated identically to unset — applies
/// to all installers (an empty scope is not "scope to nothing").
#[test]
fn winget_empty_arch_scope_attaches_to_all_installers() {
    let deps = vec![anodizer_core::config::WingetDependency {
        package_identifier: "Acme.CommonRuntime".to_string(),
        minimum_version: None,
        architectures: Some(vec![]),
    }];
    let mut params = default_params();
    params.installers = dual_arch_installers();
    params.dependencies = &deps;
    let (_, inst, _) = generate_manifests(&params).unwrap();
    assert_eq!(
        count_occurrences(&inst, "PackageIdentifier: Acme.CommonRuntime"),
        2
    );
}

/// A scoped dependency attaches ONLY to the matching-architecture installer.
#[test]
fn winget_scoped_dependency_attaches_only_to_matching_installer() {
    let deps = vec![anodizer_core::config::WingetDependency {
        package_identifier: "Microsoft.VCRedist.2015+.x64".to_string(),
        minimum_version: Some("14.0.0".to_string()),
        architectures: Some(vec!["x64".to_string()]),
    }];
    let mut params = default_params();
    params.installers = dual_arch_installers();
    params.dependencies = &deps;
    let (_, inst, _) = generate_manifests(&params).unwrap();
    // Exactly one installer (x64) carries the dependency.
    assert_eq!(count_occurrences(&inst, "PackageDependencies:"), 1);
    assert_eq!(
        count_occurrences(&inst, "PackageIdentifier: Microsoft.VCRedist.2015+.x64"),
        1
    );
}

/// Regression for the original bug: an `x64`-scoped VCRedist must NOT
/// attach to the native `arm64` installer (which would pull the wrong
/// runtime → STATUS_DLL_NOT_FOUND on a clean arm64 box). We assert the
/// dependency lands under the x64 installer entry and that the arm64 entry
/// carries no Dependencies block.
#[test]
fn winget_arm64_installer_does_not_get_x64_scoped_dependency() {
    let deps = vec![anodizer_core::config::WingetDependency {
        package_identifier: "Microsoft.VCRedist.2015+.x64".to_string(),
        minimum_version: None,
        architectures: Some(vec!["x64".to_string()]),
    }];
    let mut params = default_params();
    params.installers = dual_arch_installers();
    params.dependencies = &deps;
    let (_, inst, _) = generate_manifests(&params).unwrap();

    // Split the rendered Installers[] on the arm64 entry's Architecture key
    // and confirm the x64-scoped dep does not appear after it.
    let arm64_pos = inst
        .find("Architecture: arm64")
        .expect("arm64 installer entry present");
    let after_arm64 = &inst[arm64_pos..];
    assert!(
        !after_arm64.contains("Microsoft.VCRedist.2015+.x64"),
        "x64-scoped VCRedist leaked onto the arm64 installer:\n{inst}"
    );
    // And it IS present overall (attached to the x64 installer).
    assert!(inst.contains("Microsoft.VCRedist.2015+.x64"));
    assert_eq!(count_occurrences(&inst, "PackageDependencies:"), 1);
}

/// Mixed scoped + unscoped: the unscoped dep is on both installers, the
/// arm64-scoped dep only on arm64. Proves multiple deps compose per arch.
#[test]
fn winget_mixed_scoped_and_unscoped_dependencies_compose_per_installer() {
    let deps = vec![
        anodizer_core::config::WingetDependency {
            package_identifier: "Microsoft.VCRedist.2015+.arm64".to_string(),
            minimum_version: None,
            architectures: Some(vec!["arm64".to_string()]),
        },
        anodizer_core::config::WingetDependency {
            package_identifier: "Acme.CommonRuntime".to_string(),
            minimum_version: None,
            architectures: None,
        },
    ];
    let mut params = default_params();
    params.installers = dual_arch_installers();
    params.dependencies = &deps;
    let (_, inst, _) = generate_manifests(&params).unwrap();

    // Common runtime on both installers.
    assert_eq!(
        count_occurrences(&inst, "PackageIdentifier: Acme.CommonRuntime"),
        2
    );
    // arm64 VCRedist only once, and only after the arm64 entry.
    assert_eq!(
        count_occurrences(&inst, "PackageIdentifier: Microsoft.VCRedist.2015+.arm64"),
        1
    );
    let x64_pos = inst.find("Architecture: x64").unwrap();
    let arm64_pos = inst.find("Architecture: arm64").unwrap();
    let x64_segment = if x64_pos < arm64_pos {
        &inst[x64_pos..arm64_pos]
    } else {
        &inst[x64_pos..]
    };
    assert!(
        !x64_segment.contains("Microsoft.VCRedist.2015+.arm64"),
        "arm64-scoped dep leaked onto the x64 installer"
    );
}

/// A dependency scoped to an architecture absent from the installer set
/// (`x86` when only x64+arm64 installers exist) matches no installer, so
/// NO `Dependencies` block is emitted anywhere and the other installers are
/// unaffected (the `skip_serializing_if`/None path on each installer entry).
/// Locks the current behavior; config validation rejects unknown arch names
/// (`amd64`/`X64`/…), but a valid-but-absent arch like `x86` is legitimate
/// (e.g. a future x86 installer) and must simply attach to nothing here.
#[test]
fn winget_dependency_scoped_to_absent_arch_emits_no_block() {
    let deps = vec![anodizer_core::config::WingetDependency {
        package_identifier: "Acme.X86Runtime".to_string(),
        minimum_version: None,
        architectures: Some(vec!["x86".to_string()]),
    }];
    let mut params = default_params();
    params.installers = dual_arch_installers();
    params.dependencies = &deps;
    let (_, inst, _) = generate_manifests(&params).unwrap();

    // No installer matches x86 → no Dependencies block at all, and the
    // dependency identifier never appears in the rendered manifest.
    assert_eq!(
        count_occurrences(&inst, "PackageDependencies:"),
        0,
        "x86-scoped dep must not emit a Dependencies block on x64/arm64 installers:\n{inst}"
    );
    assert!(
        !inst.contains("Acme.X86Runtime"),
        "x86-scoped dep leaked into the manifest:\n{inst}"
    );
    // Both installers are still present and unaffected.
    assert!(inst.contains("Architecture: x64"));
    assert!(inst.contains("Architecture: arm64"));
}

#[test]
fn test_generate_manifests_with_tags() {
    let tags = vec!["CLI Tool".to_string(), "Rust".to_string()];
    let mut params = default_params();
    params.tags = Some(&tags);
    let (_, _, locale) = generate_manifests(&params).unwrap();
    assert!(locale.contains("cli-tool"));
    assert!(locale.contains("rust"));
}

// -----------------------------------------------------------------------
// publish_to_winget dry-run tests
// -----------------------------------------------------------------------

/// Regression: when short_description, description, and meta.description are all
/// unset, winget must hard-fail with an actionable error. The old
/// lenient fallback to `crate_name` produced a meaningless manifest.
#[test]
fn test_publish_to_winget_missing_config() {
    use anodizer_core::config::{Config, CrateConfig, PublishConfig};
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};

    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        publish: Some(PublishConfig::default()),
        ..Default::default()
    }];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    let log = StageLogger::new("publish", Verbosity::Normal);

    // Should fail because there's no winget config
    assert!(publish_to_winget(&mut ctx, "mytool", &log).is_err());
}

#[test]
fn test_publish_to_winget_missing_manifests_repo() {
    use anodizer_core::config::{Config, CrateConfig, PublishConfig, WingetConfig};
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};

    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        publish: Some(PublishConfig {
            winget: Some(WingetConfig {
                repository: None, // Missing
                package_identifier: Some("Org.Tool".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    let log = StageLogger::new("publish", Verbosity::Normal);

    // Should fail because manifests_repo is missing
    assert!(publish_to_winget(&mut ctx, "mytool", &log).is_err());
}

#[test]
fn test_generate_manifests_all_optional_fields() {
    let deps = vec![anodizer_core::config::WingetDependency {
        package_identifier: "Microsoft.VCRedist.2015+.x64".to_string(),
        minimum_version: Some("14.0.0".to_string()),
        ..Default::default()
    }];
    let tags = vec!["CLI".to_string(), "DevOps".to_string()];
    let params = WingetManifestParams {
        package_id: "MyOrg.MyTool",
        name: "mytool",
        package_name: Some("My Tool Pro"),
        version: "2.5.0",
        description: "A comprehensive tool",
        short_description: "CLI tool",
        license: "Apache-2.0",
        license_url: Some("https://example.com/license"),
        publisher: "My Org Inc",
        publisher_url: Some("https://myorg.com"),
        publisher_support_url: Some("https://myorg.com/support"),
        privacy_url: Some("https://myorg.com/privacy"),
        author: Some("Jane Doe"),
        copyright: Some("Copyright 2026 My Org Inc"),
        copyright_url: Some("https://myorg.com/copyright"),
        homepage: Some("https://mytool.dev"),
        release_notes: Some("Added new features in v2.5.0"),
        release_notes_url: Some("https://github.com/myorg/mytool/releases/v2.5.0"),
        installation_notes: Some("Run 'mytool --help' to get started"),
        tags: Some(&tags),
        dependencies: &deps,
        installers: vec![WingetInstallerItem {
            architecture: "x64".to_string(),
            url: "https://example.com/mytool-2.5.0-windows-amd64.zip".to_string(),
            sha256: "abc123def456".to_string(),
            installer_type: "zip".to_string(),
            binaries: vec![],
            wrap_in_directory: None,
            commands: vec![],
            silent_switch_override: None,
        }],
        product_code: Some("{12345678-1234-1234-1234-123456789012}"),
        release_date: Some("2026-03-29"),
        moniker: Some("mytool"),
        upgrade_behavior: "install",
        documentations: &[],
        default_locale: "en-US",
    };

    let (ver, inst, locale) = generate_manifests(&params).unwrap();

    // Version manifest
    assert!(ver.contains("PackageIdentifier: MyOrg.MyTool"));
    assert!(ver.contains("PackageVersion: 2.5.0"));
    assert!(ver.contains("ManifestType: version"));

    // Installer manifest
    assert!(
        inst.contains("ProductCode:"),
        "installer manifest should contain ProductCode"
    );
    assert!(
        inst.contains("{12345678-1234-1234-1234-123456789012}"),
        "installer manifest should contain the product code value"
    );
    assert!(
        inst.contains("ReleaseDate:"),
        "installer manifest should contain ReleaseDate"
    );
    assert!(
        inst.contains("2026-03-29"),
        "installer manifest should contain the release date value"
    );
    assert!(inst.contains("PackageDependencies:"));
    assert!(inst.contains("PackageIdentifier: Microsoft.VCRedist.2015+.x64"));
    assert!(inst.contains("MinimumVersion: 14.0.0"));
    assert!(inst.contains("NestedInstallerType: portable"));
    assert!(inst.contains("RelativeFilePath: mytool.exe"));

    // Locale manifest
    assert!(locale.contains("PackageName: My Tool Pro"));
    assert!(locale.contains("Publisher: My Org Inc"));
    assert!(locale.contains("PublisherUrl: https://myorg.com"));
    assert!(locale.contains("PublisherSupportUrl: https://myorg.com/support"));
    assert!(locale.contains("PrivacyUrl: https://myorg.com/privacy"));
    assert!(locale.contains("Author: Jane Doe"));
    assert!(locale.contains("Copyright: Copyright 2026 My Org Inc"));
    assert!(locale.contains("CopyrightUrl: https://myorg.com/copyright"));
    assert!(locale.contains("PackageUrl: https://mytool.dev"));
    assert!(locale.contains("License: Apache-2.0"));
    assert!(locale.contains("LicenseUrl: https://example.com/license"));
    assert!(locale.contains("ShortDescription: CLI tool"));
    assert!(locale.contains("Description: A comprehensive tool"));
    assert!(locale.contains("ReleaseNotes: Added new features in v2.5.0"));
    assert!(locale.contains("ReleaseNotesUrl: https://github.com/myorg/mytool/releases/v2.5.0"));
    assert!(locale.contains("InstallationNotes: Run 'mytool --help' to get started"));
    assert!(locale.contains("cli"));
    assert!(locale.contains("devops"));
}

/// winget's `License` is a freeform display string; a dual
/// `MIT OR Apache-2.0` SPDX expression passes through verbatim into the
/// locale manifest's `License:` field, never split or rejected.
#[test]
fn compound_spdx_license_emitted_verbatim() {
    let mut params = default_params();
    params.license = "MIT OR Apache-2.0";
    let (_, _, locale) = generate_manifests(&params).unwrap();
    assert!(
        locale.contains("License: MIT OR Apache-2.0"),
        "compound license must pass through verbatim, got:\n{locale}"
    );
}

// -----------------------------------------------------------------------
// Moniker / UpgradeBehavior / Documentations / InstallerSwitches
// -----------------------------------------------------------------------

/// A configured Moniker is emitted as the short invoke alias, matching
/// real ripgrep's `Moniker: rg` (NOT the package name `ripgrep`).
#[test]
fn test_winget_moniker_emitted_as_alias() {
    let mut params = default_params();
    params.moniker = Some("rg");
    let (_, _, locale) = generate_manifests(&params).unwrap();
    assert!(
        locale.contains("Moniker: rg"),
        "Moniker must be the invoke alias, got:\n{locale}"
    );
}

/// With no Moniker resolvable (multi-binary, no override) the key is
/// omitted entirely — never defaulted to the crate name.
#[test]
fn test_winget_moniker_omitted_when_none() {
    let mut params = default_params();
    params.moniker = None;
    let (_, _, locale) = generate_manifests(&params).unwrap();
    assert!(
        !locale.contains("Moniker:"),
        "Moniker must be omitted when unresolved, got:\n{locale}"
    );
}

/// Default UpgradeBehavior is `install`; the override is honored.
#[test]
fn test_winget_upgrade_behavior_override() {
    let mut params = default_params();
    params.upgrade_behavior = "uninstallPrevious";
    let (_, inst, _) = generate_manifests(&params).unwrap();
    assert!(inst.contains("UpgradeBehavior: uninstallPrevious"));
}

/// Documentations[] renders `DocumentLabel`/`DocumentUrl` pairs, the
/// exact shape real ripgrep's locale manifest carries (`FAQ`, `User Guide`).
#[test]
fn test_winget_documentations_emitted() {
    let docs = vec![
        anodizer_core::config::WingetDocumentation {
            label: "FAQ".to_string(),
            url: "https://github.com/owner/repo/blob/master/FAQ.md".to_string(),
        },
        anodizer_core::config::WingetDocumentation {
            label: "User Guide".to_string(),
            url: "https://github.com/owner/repo/blob/master/GUIDE.md".to_string(),
        },
    ];
    let mut params = default_params();
    params.documentations = &docs;
    let (_, _, locale) = generate_manifests(&params).unwrap();
    assert!(locale.contains("Documentations:"));
    assert!(locale.contains("DocumentLabel: FAQ"));
    assert!(locale.contains("DocumentUrl: https://github.com/owner/repo/blob/master/FAQ.md"));
    assert!(locale.contains("DocumentLabel: User Guide"));
    assert!(locale.contains("DocumentUrl: https://github.com/owner/repo/blob/master/GUIDE.md"));
}

/// An empty documentations list omits the key entirely.
#[test]
fn test_winget_documentations_omitted_when_empty() {
    let params = default_params();
    let (_, _, locale) = generate_manifests(&params).unwrap();
    assert!(!locale.contains("Documentations:"));
}

/// Zip/portable installers carry NO InstallerSwitches.
#[test]
fn test_winget_installer_switches_absent_for_zip() {
    let params = default_params();
    let (_, inst, _) = generate_manifests(&params).unwrap();
    assert!(!inst.contains("InstallerSwitches"));
    assert!(!inst.contains("Silent:"));
}

/// An actual installer (msi) derives `/quiet`; exe/nsis derive `/S`;
/// the config override wins. zip/portable always omit the switch.
#[test]
fn test_resolve_installer_switches_per_type() {
    let msi = resolve_installer_switches("msi", None).expect("msi gets a switch");
    assert_eq!(msi.silent, "/quiet");
    let wix = resolve_installer_switches("wix", None).expect("wix gets a switch");
    assert_eq!(wix.silent, "/quiet");
    let exe = resolve_installer_switches("exe", None).expect("exe gets a switch");
    assert_eq!(exe.silent, "/S");
    let nsis = resolve_installer_switches("nsis", None).expect("nsis gets a switch");
    assert_eq!(nsis.silent, "/S");
    assert!(resolve_installer_switches("zip", None).is_none());
    assert!(resolve_installer_switches("portable", None).is_none());
    // Override wins for an actual installer.
    let overridden = resolve_installer_switches("msi", Some("/qn")).expect("override");
    assert_eq!(overridden.silent, "/qn");
    // ...but an override on zip is still suppressed (zip is unpacked).
    assert!(resolve_installer_switches("zip", Some("/qn")).is_none());
}

/// An msi installer entry renders `InstallerSwitches.Silent: /quiet`.
#[test]
fn test_winget_installer_switches_emitted_for_msi() {
    let mut params = default_params();
    params.installers = vec![WingetInstallerItem {
        architecture: "x64".to_string(),
        url: "https://example.com/mytool-1.0.0-windows-amd64.msi".to_string(),
        sha256: "deadbeef".to_string(),
        installer_type: "msi".to_string(),
        binaries: vec![],
        wrap_in_directory: None,
        commands: vec![],
        silent_switch_override: None,
    }];
    let (_, inst, _) = generate_manifests(&params).unwrap();
    assert!(inst.contains("InstallerSwitches:"));
    assert!(inst.contains("Silent: /quiet"));
}

// -----------------------------------------------------------------------
// wrap_in_directory tests
// -----------------------------------------------------------------------

#[test]
fn test_winget_wrap_in_directory_prefixes_relative_file_path() {
    let params = WingetManifestParams {
        package_id: "Org.MyApp",
        name: "myapp",
        package_name: None,
        version: "1.0.0",
        description: "An app",
        short_description: "An app",
        license: "MIT",
        license_url: None,
        publisher: "Org",
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
        installers: vec![WingetInstallerItem {
            architecture: "x64".to_string(),
            url: "https://example.com/myapp-1.0.0.zip".to_string(),
            sha256: "abc123".to_string(),
            installer_type: "zip".to_string(),
            binaries: vec!["myapp".to_string()],
            wrap_in_directory: Some("myapp-1.0.0".to_string()),
            commands: vec![],
            silent_switch_override: None,
        }],
        product_code: None,
        release_date: None,
        moniker: Some("myapp"),
        upgrade_behavior: "install",
        documentations: &[],
        default_locale: "en-US",
    };

    let (_ver, inst, _locale) = generate_manifests(&params).unwrap();
    assert!(
        inst.contains("RelativeFilePath: myapp-1.0.0\\myapp.exe"),
        "RelativeFilePath should include wrap_in_directory prefix, got:\n{}",
        inst
    );
}

#[test]
fn test_winget_no_wrap_keeps_plain_relative_file_path() {
    let params = WingetManifestParams {
        package_id: "Org.MyApp",
        name: "myapp",
        package_name: None,
        version: "1.0.0",
        description: "An app",
        short_description: "An app",
        license: "MIT",
        license_url: None,
        publisher: "Org",
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
        installers: vec![WingetInstallerItem {
            architecture: "x64".to_string(),
            url: "https://example.com/myapp-1.0.0.zip".to_string(),
            sha256: "abc123".to_string(),
            installer_type: "zip".to_string(),
            binaries: vec!["myapp".to_string()],
            wrap_in_directory: None,
            commands: vec![],
            silent_switch_override: None,
        }],
        product_code: None,
        release_date: None,
        moniker: Some("myapp"),
        upgrade_behavior: "install",
        documentations: &[],
        default_locale: "en-US",
    };

    let (_ver, inst, _locale) = generate_manifests(&params).unwrap();
    assert!(
        inst.contains("RelativeFilePath: myapp.exe"),
        "Without wrap_in_directory, RelativeFilePath should be plain, got:\n{}",
        inst
    );
    assert!(
        !inst.contains("\\myapp.exe"),
        "Without wrap_in_directory, no backslash prefix should appear"
    );
}

#[test]
fn test_winget_wrap_in_directory_multiple_binaries() {
    let params = WingetManifestParams {
        package_id: "Org.Suite",
        name: "suite",
        package_name: None,
        version: "2.0.0",
        description: "A suite",
        short_description: "A suite",
        license: "MIT",
        license_url: None,
        publisher: "Org",
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
        installers: vec![WingetInstallerItem {
            architecture: "x64".to_string(),
            url: "https://example.com/suite-2.0.0.zip".to_string(),
            sha256: "def456".to_string(),
            installer_type: "zip".to_string(),
            binaries: vec!["cli".to_string(), "daemon".to_string()],
            wrap_in_directory: Some("suite-2.0.0".to_string()),
            commands: vec![],
            silent_switch_override: None,
        }],
        product_code: None,
        release_date: None,
        moniker: None,
        upgrade_behavior: "install",
        documentations: &[],
        default_locale: "en-US",
    };

    let (_ver, inst, _locale) = generate_manifests(&params).unwrap();
    assert!(
        inst.contains("RelativeFilePath: suite-2.0.0\\cli.exe"),
        "First binary should have wrap prefix, got:\n{}",
        inst
    );
    assert!(
        inst.contains("RelativeFilePath: suite-2.0.0\\daemon.exe"),
        "Second binary should have wrap prefix, got:\n{}",
        inst
    );
}

// -----------------------------------------------------------------------
// PackageIdentifier validation tests
// -----------------------------------------------------------------------

#[test]
fn test_validate_package_identifier_valid() {
    assert!(validate_package_identifier("Org.Tool").is_ok());
    assert!(validate_package_identifier("Microsoft.VisualStudioCode").is_ok());
    assert!(validate_package_identifier("My.Multi.Segment.Id").is_ok());
    assert!(validate_package_identifier("A.B.C.D.E.F.G.H").is_ok()); // 8 segments max
}

#[test]
fn test_validate_package_identifier_invalid_single_segment() {
    assert!(validate_package_identifier("JustOneName").is_err());
}

#[test]
fn test_validate_package_identifier_invalid_special_chars() {
    assert!(validate_package_identifier("Org.Tool:Bad").is_err());
    assert!(validate_package_identifier("Org.Tool<Bad>").is_err());
    assert!(validate_package_identifier("Org.Tool|Bad").is_err());
    assert!(validate_package_identifier("Org.Tool*Bad").is_err());
    assert!(validate_package_identifier("Org.Tool?Bad").is_err());
}

#[test]
fn test_validate_package_identifier_invalid_whitespace() {
    assert!(validate_package_identifier("Org.Tool Name").is_err());
    assert!(validate_package_identifier("Org .Tool").is_err());
}

#[test]
fn test_validate_package_identifier_too_many_segments() {
    // 9 segments (more than 8) should fail
    assert!(validate_package_identifier("A.B.C.D.E.F.G.H.I").is_err());
}

#[test]
fn test_validate_package_identifier_segment_length_limit() {
    // The regex pins each segment to 1..=32 chars.
    let segment_32 = "A".repeat(32);
    let segment_33 = "A".repeat(33);
    // OK: a 32-char segment is the upper bound.
    assert!(validate_package_identifier(&format!("{segment_32}.OK")).is_ok());
    assert!(validate_package_identifier(&format!("Org.{segment_32}")).is_ok());
    // FAIL: a 33-char segment trips the live winget validator.
    assert!(validate_package_identifier(&format!("{segment_33}.OK")).is_err());
    assert!(validate_package_identifier(&format!("Org.{segment_33}")).is_err());
}

#[test]
fn test_validate_package_identifier_rejects_control_chars() {
    // Live winget rejects ASCII control chars (`\x01-\x1f`); anodizer
    // must block them too so the upstream PR isn't auto-rejected.
    assert!(validate_package_identifier("Org.\u{0001}Bad").is_err());
    assert!(validate_package_identifier("Org.Bad\u{001f}").is_err());
    // NUL is not in `\x01-\x1f` but is also forbidden upstream.
    assert!(validate_package_identifier("Org.\u{0000}Bad").is_err());
}

#[test]
fn test_validate_package_identifier_empty_segment() {
    assert!(validate_package_identifier("Org..Tool").is_err());
    assert!(validate_package_identifier(".Org.Tool").is_err());
    assert!(validate_package_identifier("Org.Tool.").is_err());
}

// -----------------------------------------------------------------------
// Winget commit message with PackageIdentifier
// -----------------------------------------------------------------------

fn commit_msg_logger() -> StageLogger {
    StageLogger::new("publish", anodizer_core::log::Verbosity::Normal)
}

#[test]
fn test_winget_commit_msg_default() {
    let msg = render_winget_commit_msg(None, "Org.MyTool", "1.0.0", &commit_msg_logger(), false)
        .expect("default template renders");
    assert_eq!(msg, "New version: Org.MyTool 1.0.0");
}

#[test]
fn test_winget_commit_msg_malformed_template_errors_under_strict() {
    // A malformed commit-message template is a hard error under the
    // guard/`--strict` — a broken title must not silently ship.
    let err = render_winget_commit_msg(
        Some("{{ Version | no_such_filter_xyz }}"),
        "Org.MyTool",
        "1.0.0",
        &commit_msg_logger(),
        true,
    )
    .expect_err("malformed template must fail under strict");
    assert!(
        err.to_string().contains("commit_msg_template"),
        "error should name the offending field: {err:#}"
    );
}

#[test]
fn test_winget_commit_msg_malformed_template_falls_back_when_lenient() {
    // Outside strict mode the same failure warns and falls back to the
    // default-shaped message rather than aborting the publish.
    let msg = render_winget_commit_msg(
        Some("{{ Version | no_such_filter_xyz }}"),
        "Org.MyTool",
        "1.0.0",
        &commit_msg_logger(),
        false,
    )
    .expect("lenient mode falls back, not errors");
    assert_eq!(msg, "New version: Org.MyTool 1.0.0");
}

#[test]
fn test_winget_commit_msg_with_package_identifier_template() {
    // PackageIdentifier is exposed in the template context
    let msg = render_winget_commit_msg(
        Some("winget: {{ PackageIdentifier }} v{{ version }}"),
        "Org.MyTool",
        "2.0.0",
        &commit_msg_logger(),
        false,
    )
    .expect("template renders");
    assert_eq!(msg, "winget: Org.MyTool v2.0.0");
}

#[test]
fn test_winget_commit_msg_custom() {
    let msg = render_winget_commit_msg(
        Some("release: {{ name }} {{ version }}"),
        "Org.MyTool",
        "3.0.0",
        &commit_msg_logger(),
        false,
    )
    .expect("template renders");
    assert_eq!(msg, "release: Org.MyTool 3.0.0");
}

#[test]
fn test_winget_commit_msg_tag_and_version_vars() {
    // Regression: `.Tag`/`.Version` (the standard cross-publisher vars)
    // must resolve in winget's commit-msg context — not error and fall
    // back to the default. Mirrors the v0.6.0 production warning.
    let msg = render_winget_commit_msg(
        Some("x {{ Tag }} {{ Version }}"),
        "Org.MyTool",
        "1.2.3",
        &commit_msg_logger(),
        // strict: ensure an unregistered var would surface as an error
        // rather than be silently swallowed by the warn-and-default path.
        true,
    )
    .expect("Tag/Version registered in winget commit-msg context");
    assert_eq!(msg, "x 1.2.3 1.2.3");
}

#[test]
fn test_winget_commit_msg_project_name_var() {
    // `.ProjectName` is registered alongside `PackageIdentifier` so a
    // template migrated from another publisher renders unchanged.
    let msg = render_winget_commit_msg(
        Some("{{ ProjectName }} {{ Tag }}"),
        "Org.MyTool",
        "4.5.6",
        &commit_msg_logger(),
        true,
    )
    .expect("ProjectName registered in winget commit-msg context");
    assert_eq!(msg, "Org.MyTool 4.5.6");
}

#[test]
fn test_winget_package_name_fallback_to_name() {
    // When package_name is None, it should fall back to name
    let params = WingetManifestParams {
        package_id: "Org.MyTool",
        name: "mytool",
        package_name: None,
        version: "1.0.0",
        description: "desc",
        short_description: "short",
        license: "MIT",
        ..default_params()
    };
    let (_, _, locale) = generate_manifests(&params).unwrap();
    // PackageName should be "mytool" (fallback from name)
    assert!(
        locale.contains("PackageName: mytool"),
        "PackageName should fall back to name:\n{locale}"
    );
}

#[test]
fn test_winget_package_name_override() {
    let params = WingetManifestParams {
        package_id: "Org.MyTool",
        name: "mytool",
        package_name: Some("My Tool Pro"),
        version: "1.0.0",
        description: "desc",
        short_description: "short",
        license: "MIT",
        ..default_params()
    };
    let (_, _, locale) = generate_manifests(&params).unwrap();
    assert!(
        locale.contains("PackageName: My Tool Pro"),
        "PackageName should use the override:\n{locale}"
    );
}

use anodizer_core::config::{CrateConfig, WingetConfig};
use anodizer_core::test_helpers::TestContextBuilder;

/// Write a `Cargo.toml [package]` into `<dir>/<crate_path>` and populate
/// `ctx.config.derived_metadata` from it, exercising the real
/// Cargo.toml → derived_metadata → `meta_*_for` path.
fn derive_into(ctx: &mut Context, dir: &std::path::Path, crate_name: &str, cargo_toml: &str) {
    let crate_dir = dir.join(crate_name);
    std::fs::create_dir_all(&crate_dir).unwrap();
    std::fs::write(crate_dir.join("Cargo.toml"), cargo_toml).unwrap();
    ctx.config.crates = vec![CrateConfig {
        name: crate_name.to_string(),
        path: crate_name.to_string(),
        ..Default::default()
    }];
    ctx.config.populate_derived_metadata(dir);
}

#[test]
fn winget_required_fields_resolve_from_cargo_toml_when_no_metadata_block() {
    // No top-level `metadata:` YAML — Cargo.toml [package] supplies the
    // values the winget required-field paths previously bailed on.
    let mut ctx = TestContextBuilder::new().build();
    assert!(ctx.config.metadata.is_none(), "no metadata: block present");
    let tmp = tempfile::tempdir().unwrap();
    derive_into(
        &mut ctx,
        tmp.path(),
        "demo",
        r#"
[package]
name = "demo"
description = "A demo CLI for winget"
license = "MIT"
"#,
    );
    let winget_cfg = WingetConfig::default();

    // short_description previously: "short_description is required".
    let short = resolve_winget_short_description(&ctx, &winget_cfg, "demo")
        .expect("short_description resolves from Cargo.toml description");
    assert_eq!(short, "A demo CLI for winget");

    // license previously: "license is required".
    let license = resolve_winget_license(&ctx, &winget_cfg, "demo")
        .expect("license resolves from Cargo.toml [package].license");
    assert_eq!(license, "MIT");
}

#[test]
fn winget_license_file_only_crate_still_errors_on_missing_license() {
    // A crate using `license-file` (no SPDX `license`) must NOT have a
    // license synthesised — the genuine-missing-license error must fire.
    let mut ctx = TestContextBuilder::new().build();
    let tmp = tempfile::tempdir().unwrap();
    derive_into(
        &mut ctx,
        tmp.path(),
        "demo",
        r#"
[package]
name = "demo"
description = "has a description but only a license-file"
license-file = "LICENSE.txt"
"#,
    );
    let winget_cfg = WingetConfig::default();

    // description IS present, so short_description resolves...
    assert_eq!(
        resolve_winget_short_description(&ctx, &winget_cfg, "demo").unwrap(),
        "has a description but only a license-file"
    );
    // ...but license-file is not an SPDX id, so license MUST still error.
    let err = resolve_winget_license(&ctx, &winget_cfg, "demo")
        .expect_err("license-file-only crate must still error on missing license");
    assert!(
        err.to_string().contains("license is required"),
        "expected genuine missing-license error; got: {err}"
    );
}

#[test]
fn winget_per_crate_resolves_each_crates_own_description() {
    // Two crates, different Cargo.toml descriptions: each resolves ITS OWN.
    let mut ctx = TestContextBuilder::new().build();
    let tmp = tempfile::tempdir().unwrap();
    for (name, desc) in [("alpha", "Alpha tool"), ("beta", "Beta tool")] {
        let crate_dir = tmp.path().join(name);
        std::fs::create_dir_all(&crate_dir).unwrap();
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\ndescription = \"{desc}\"\nlicense = \"MIT\"\n"),
        )
        .unwrap();
    }
    ctx.config.crates = ["alpha", "beta"]
        .iter()
        .map(|n| CrateConfig {
            name: n.to_string(),
            path: n.to_string(),
            ..Default::default()
        })
        .collect();
    ctx.config.populate_derived_metadata(tmp.path());

    let cfg = WingetConfig::default();
    assert_eq!(
        resolve_winget_short_description(&ctx, &cfg, "alpha").unwrap(),
        "Alpha tool"
    );
    assert_eq!(
        resolve_winget_short_description(&ctx, &cfg, "beta").unwrap(),
        "Beta tool"
    );
}

// -----------------------------------------------------------------------
// map_winget_arch
// -----------------------------------------------------------------------

#[test]
fn map_winget_arch_translates_known_archs() {
    assert_eq!(map_winget_arch("amd64"), "x64");
    assert_eq!(map_winget_arch("386"), "x86");
    assert_eq!(map_winget_arch("i686"), "x86");
    assert_eq!(map_winget_arch("arm64"), "arm64");
}

#[test]
fn map_winget_arch_passes_through_unknown() {
    assert_eq!(map_winget_arch("riscv64"), "riscv64");
}

// -----------------------------------------------------------------------
// is_winget_zip_archive
// -----------------------------------------------------------------------

fn archive_with(path: &str, format: Option<&str>) -> anodizer_core::artifact::Artifact {
    let mut metadata = std::collections::HashMap::new();
    if let Some(f) = format {
        metadata.insert("format".to_string(), f.to_string());
    }
    anodizer_core::artifact::Artifact {
        kind: anodizer_core::artifact::ArtifactKind::Archive,
        path: std::path::PathBuf::from(path),
        name: path.rsplit('/').next().unwrap_or(path).to_string(),
        target: Some("x86_64-pc-windows-msvc".to_string()),
        crate_name: "demo".to_string(),
        metadata,
        size: None,
    }
}

#[test]
fn is_winget_zip_archive_true_on_format_metadata() {
    assert!(is_winget_zip_archive(&archive_with(
        "/dist/demo.tar",
        Some("zip")
    )));
}

#[test]
fn is_winget_zip_archive_true_on_zip_extension() {
    assert!(is_winget_zip_archive(&archive_with("/dist/demo.zip", None)));
}

#[test]
fn is_winget_zip_archive_false_for_tarball() {
    assert!(!is_winget_zip_archive(&archive_with(
        "/dist/demo.tar.gz",
        Some("tar.gz")
    )));
}

// -----------------------------------------------------------------------
// resolve_winget_upstream
// -----------------------------------------------------------------------

#[test]
fn resolve_winget_upstream_defaults_to_microsoft() {
    let cfg = WingetConfig::default();
    assert_eq!(
        resolve_winget_upstream(&cfg),
        ("microsoft".to_string(), "winget-pkgs".to_string())
    );
}

#[test]
fn resolve_winget_upstream_honors_pull_request_base() {
    use anodizer_core::config::{PullRequestBaseConfig, PullRequestConfig, RepositoryConfig};
    let cfg = WingetConfig {
        repository: Some(RepositoryConfig {
            pull_request: Some(PullRequestConfig {
                base: Some(PullRequestBaseConfig {
                    owner: Some("acme".to_string()),
                    name: Some("winget-mirror".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(
        resolve_winget_upstream(&cfg),
        ("acme".to_string(), "winget-mirror".to_string())
    );
}

#[test]
fn resolve_winget_upstream_partial_base_falls_back_to_default() {
    use anodizer_core::config::{PullRequestBaseConfig, PullRequestConfig, RepositoryConfig};
    // owner set but name missing -> default upstream, not a half-built repo.
    let cfg = WingetConfig {
        repository: Some(RepositoryConfig {
            pull_request: Some(PullRequestConfig {
                base: Some(PullRequestBaseConfig {
                    owner: Some("acme".to_string()),
                    name: None,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(
        resolve_winget_upstream(&cfg),
        ("microsoft".to_string(), "winget-pkgs".to_string())
    );
}

// -----------------------------------------------------------------------
// resolve_winget_publisher_name
// -----------------------------------------------------------------------

#[test]
fn resolve_winget_publisher_name_prefers_explicit_publisher() {
    use anodizer_core::log::{StageLogger, Verbosity};
    let log = StageLogger::new("publish", Verbosity::Quiet);
    let cfg = WingetConfig {
        publisher: Some("AcmeCo".to_string()),
        ..Default::default()
    };
    assert_eq!(
        resolve_winget_publisher_name(&cfg, "ignored-owner", "demo", &log).unwrap(),
        "AcmeCo"
    );
}

#[test]
fn resolve_winget_publisher_name_falls_back_to_repo_owner() {
    use anodizer_core::log::{StageLogger, Verbosity};
    let log = StageLogger::new("publish", Verbosity::Quiet);
    let cfg = WingetConfig::default();
    assert_eq!(
        resolve_winget_publisher_name(&cfg, "acme-owner", "demo", &log).unwrap(),
        "acme-owner"
    );
}

#[test]
fn resolve_winget_publisher_name_errors_when_publisher_and_owner_empty() {
    use anodizer_core::log::{StageLogger, Verbosity};
    let log = StageLogger::new("publish", Verbosity::Quiet);
    let cfg = WingetConfig {
        publisher: Some(String::new()),
        ..Default::default()
    };
    let err = resolve_winget_publisher_name(&cfg, "", "demo", &log)
        .expect_err("empty publisher + empty owner must error");
    assert!(
        err.to_string().contains("publisher is required"),
        "got: {err}"
    );
}

// -----------------------------------------------------------------------
// resolve_winget_description
// -----------------------------------------------------------------------

#[test]
fn resolve_winget_description_uses_explicit_and_normalizes_tabs() {
    let ctx = TestContextBuilder::new().build();
    let cfg = WingetConfig {
        description: Some("line\twith\ttabs".to_string()),
        ..Default::default()
    };
    assert_eq!(
        resolve_winget_description(&ctx, &cfg, "demo", &ctx.logger("publish")).unwrap(),
        "line  with  tabs"
    );
}

#[test]
fn resolve_winget_description_falls_back_to_cargo_metadata() {
    let mut ctx = TestContextBuilder::new().build();
    let tmp = tempfile::tempdir().unwrap();
    derive_into(
        &mut ctx,
        tmp.path(),
        "demo",
        "[package]\nname = \"demo\"\ndescription = \"derived blurb\"\n",
    );
    let cfg = WingetConfig::default();
    assert_eq!(
        resolve_winget_description(&ctx, &cfg, "demo", &ctx.logger("publish")).unwrap(),
        "derived blurb"
    );
}

#[test]
fn resolve_winget_description_empty_when_nothing_configured() {
    let ctx = TestContextBuilder::new().build();
    let cfg = WingetConfig::default();
    assert_eq!(
        resolve_winget_description(&ctx, &cfg, "demo", &ctx.logger("publish")).unwrap(),
        ""
    );
}

// -----------------------------------------------------------------------
// installer_type_for — (format stamp, `use` selector) → winget InstallerType
// -----------------------------------------------------------------------

/// The `format` stamp wins when present: `msi` maps to `msi` unless the `use`
/// selector nominates `wix` (both are Windows Installer packages, but winget
/// distinguishes the authoring toolchain).
#[test]
fn installer_type_for_format_stamp_precedence() {
    assert_eq!(installer_type_for(Some("msi"), None), "msi");
    assert_eq!(installer_type_for(Some("msi"), Some("wix")), "wix");
    // A non-wix `use` selector does not demote a real MSI stamp.
    assert_eq!(installer_type_for(Some("msi"), Some("exe")), "msi");
    assert_eq!(installer_type_for(Some("nsis"), None), "nsis");
    assert_eq!(installer_type_for(Some("exe"), None), "exe");
    // An unrecognized format stamp falls through to the `use`-selector arm.
    assert_eq!(installer_type_for(Some("weird"), Some("nsis")), "nsis");
}

/// With no `format` stamp the `use` selector routes the type; a fully-absent
/// pair defaults to `msi` (the artifact is `ArtifactKind::Installer`, so it is
/// one of the installer kinds).
#[test]
fn installer_type_for_falls_back_to_use_selector() {
    assert_eq!(installer_type_for(None, Some("wix")), "wix");
    assert_eq!(installer_type_for(None, Some("nsis")), "nsis");
    assert_eq!(installer_type_for(None, Some("exe")), "exe");
    assert_eq!(installer_type_for(None, Some("msi")), "msi");
    assert_eq!(installer_type_for(None, None), "msi");
    assert_eq!(installer_type_for(None, Some("unknown")), "msi");
}

// -----------------------------------------------------------------------
// is_executable_installer_type — which types get a silent switch
// -----------------------------------------------------------------------

/// Only real installer programs (`msi`/`wix`/`exe`/`nsis`) are "executable";
/// `zip`/`portable` archives winget unpacks itself are not.
#[test]
fn is_executable_installer_type_matrix() {
    for t in ["msi", "wix", "exe", "nsis"] {
        assert!(
            is_executable_installer_type(t),
            "{t} should be an executable installer type"
        );
    }
    for t in ["zip", "portable", "", "msix"] {
        assert!(
            !is_executable_installer_type(t),
            "{t} must NOT be an executable installer type"
        );
    }
}

// -----------------------------------------------------------------------
// auto_package_identifier — <publisher-without-spaces>.<name>
// -----------------------------------------------------------------------

/// The auto-id strips every space from the publisher (winget ids forbid
/// whitespace) but leaves the package name untouched.
#[test]
fn auto_package_identifier_strips_publisher_spaces() {
    assert_eq!(auto_package_identifier("My Org", "mytool"), "MyOrg.mytool");
    assert_eq!(
        auto_package_identifier("Acme Co", "widget"),
        "AcmeCo.widget"
    );
    assert_eq!(auto_package_identifier("Acme", "widget"), "Acme.widget");
    assert_eq!(
        auto_package_identifier("A B C Corp", "tool"),
        "ABCCorp.tool"
    );
}

// -----------------------------------------------------------------------
// static_package_identifier — context-free id resolution
// -----------------------------------------------------------------------

/// An explicit `package_identifier` with no template syntax is returned
/// verbatim; one carrying `{{` is unresolvable without a context → `None`.
#[test]
fn static_package_identifier_explicit_and_templated() {
    let explicit = anodizer_core::config::WingetConfig {
        package_identifier: Some("Acme.Widget".to_string()),
        ..Default::default()
    };
    assert_eq!(
        static_package_identifier("widget", &explicit),
        Some("Acme.Widget".to_string())
    );

    let templated = anodizer_core::config::WingetConfig {
        package_identifier: Some("Acme.{{ .ProjectName }}".to_string()),
        ..Default::default()
    };
    assert_eq!(static_package_identifier("widget", &templated), None);
}

/// With no explicit id, the auto-derivation combines the publisher and the
/// name (falling back to the crate name); a templated name/publisher, or a
/// missing repository owner with no publisher, yields `None`.
#[test]
fn static_package_identifier_auto_derivation() {
    // Explicit publisher + explicit name.
    let cfg = anodizer_core::config::WingetConfig {
        publisher: Some("Acme Co".to_string()),
        name: Some("widget".to_string()),
        ..Default::default()
    };
    assert_eq!(
        static_package_identifier("crate-x", &cfg),
        Some("AcmeCo.widget".to_string())
    );

    // Publisher set, name falls back to crate_name.
    let cfg = anodizer_core::config::WingetConfig {
        publisher: Some("Acme".to_string()),
        ..Default::default()
    };
    assert_eq!(
        static_package_identifier("crate-x", &cfg),
        Some("Acme.crate-x".to_string())
    );

    // Publisher falls back to the repository owner when unset.
    let cfg = anodizer_core::config::WingetConfig {
        name: Some("widget".to_string()),
        repository: Some(anodizer_core::config::RepositoryConfig {
            owner: Some("acme".to_string()),
            name: Some("winget-pkgs-fork".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(
        static_package_identifier("crate-x", &cfg),
        Some("acme.widget".to_string())
    );

    // A templated name is unresolvable context-free → None.
    let cfg = anodizer_core::config::WingetConfig {
        publisher: Some("Acme".to_string()),
        name: Some("{{ .ProjectName }}".to_string()),
        ..Default::default()
    };
    assert_eq!(static_package_identifier("crate-x", &cfg), None);

    // No publisher AND no repository → nothing to derive from → None.
    let cfg = anodizer_core::config::WingetConfig {
        name: Some("widget".to_string()),
        ..Default::default()
    };
    assert_eq!(static_package_identifier("crate-x", &cfg), None);
}

// -----------------------------------------------------------------------
// resolve_winget_release_date — YYYY-MM-DD from the template `Date` var
// -----------------------------------------------------------------------

fn ctx_with_date(date: Option<&str>) -> anodizer_core::context::Context {
    let mut ctx = anodizer_core::context::Context::new(
        anodizer_core::config::Config::default(),
        anodizer_core::context::ContextOptions::default(),
    );
    if let Some(d) = date {
        ctx.template_vars_mut().set("Date", d);
    }
    ctx
}

/// A valid RFC-3339 `Date` yields the leading `YYYY-MM-DD`; a malformed or
/// missing value yields `None` (the manifest omits `ReleaseDate`).
#[test]
fn resolve_winget_release_date_extracts_or_omits() {
    let ctx = ctx_with_date(Some("2026-07-18T12:34:56Z"));
    assert_eq!(
        resolve_winget_release_date(&ctx),
        Some("2026-07-18".to_string())
    );

    // A bare date (already YYYY-MM-DD) passes through.
    let ctx = ctx_with_date(Some("2025-01-02"));
    assert_eq!(
        resolve_winget_release_date(&ctx),
        Some("2025-01-02".to_string())
    );

    // Malformed: no hyphens at positions 4/7 → None.
    let ctx = ctx_with_date(Some("not-a-real-date"));
    assert_eq!(resolve_winget_release_date(&ctx), None);

    // Too short to carry a full date → None.
    let ctx = ctx_with_date(Some("2026"));
    assert_eq!(resolve_winget_release_date(&ctx), None);

    // Absent Date var → None.
    let ctx = ctx_with_date(None);
    assert_eq!(resolve_winget_release_date(&ctx), None);
}

// -----------------------------------------------------------------------
// resolve_winget_moniker — override → single-binary derivation → None
// -----------------------------------------------------------------------

fn add_windows_binary(
    ctx: &mut anodizer_core::context::Context,
    crate_name: &str,
    binary: &str,
    target: &str,
) {
    let mut meta = std::collections::HashMap::new();
    meta.insert("binary".to_string(), binary.to_string());
    ctx.artifacts.add(anodizer_core::artifact::Artifact {
        kind: anodizer_core::artifact::ArtifactKind::Binary,
        path: std::path::PathBuf::from(format!("/dist/{binary}.exe")),
        name: format!("{binary}.exe"),
        target: Some(target.to_string()),
        crate_name: crate_name.to_string(),
        metadata: meta,
        size: None,
    });
}

/// An explicit `winget.moniker` override always wins, even when multiple
/// binaries would otherwise force omission.
#[test]
fn resolve_winget_moniker_override_wins() {
    let mut ctx = anodizer_core::context::Context::new(
        anodizer_core::config::Config::default(),
        anodizer_core::context::ContextOptions::default(),
    );
    add_windows_binary(&mut ctx, "app", "one", "x86_64-pc-windows-msvc");
    add_windows_binary(&mut ctx, "app", "two", "aarch64-pc-windows-msvc");
    let cfg = anodizer_core::config::WingetConfig {
        moniker: Some("rg".to_string()),
        ..Default::default()
    };
    assert_eq!(
        resolve_winget_moniker(&ctx, "app", &cfg),
        Some("rg".to_string())
    );
}

/// With exactly one distinct windows binary name and no override, the moniker
/// derives to that binary; multiple distinct names or none → `None` (winget
/// treats the Moniker as the invoke alias, ambiguous when >1 binary).
#[test]
fn resolve_winget_moniker_derives_single_and_omits_ambiguous() {
    let cfg = anodizer_core::config::WingetConfig::default();
    let new_ctx = || {
        anodizer_core::context::Context::new(
            anodizer_core::config::Config::default(),
            anodizer_core::context::ContextOptions::default(),
        )
    };

    // One binary (across two arches, same name) → derives that name.
    let mut ctx = new_ctx();
    add_windows_binary(&mut ctx, "app", "app", "x86_64-pc-windows-msvc");
    add_windows_binary(&mut ctx, "app", "app", "aarch64-pc-windows-msvc");
    assert_eq!(
        resolve_winget_moniker(&ctx, "app", &cfg),
        Some("app".to_string())
    );

    // Two DISTINCT binary names → ambiguous → None.
    let mut ctx = new_ctx();
    add_windows_binary(&mut ctx, "app", "one", "x86_64-pc-windows-msvc");
    add_windows_binary(&mut ctx, "app", "two", "x86_64-pc-windows-msvc");
    assert_eq!(resolve_winget_moniker(&ctx, "app", &cfg), None);

    // No windows binaries at all → None.
    let ctx = new_ctx();
    assert_eq!(resolve_winget_moniker(&ctx, "app", &cfg), None);
}

// =====================================================================
// LIVE push + PR flow — drives `publish_to_winget` / `submit_winget_pr`
// against a local bare git repo (no network), forcing the GitHub REST
// API PR transport by installing a failing `gh` stub and injecting
// `ANODIZER_GITHUB_API_BASE` at an in-process scripted responder.
//
// Pattern mirrors `krew.rs`'s PrDirect harness. The winget PR path
// threads submission through the Context's injectable `EnvSource`
// (`maybe_submit_pr_with_env` / `submit_pr_via_gh_with_opts_with_env`),
// so the responder address is a per-Context value set via
// `inject_api_base` — not a process-global mutation. Each test still
// mutates PATH (the `gh` stub), so each is `#[serial(path_env)]`.
// =====================================================================
mod live_pr {
    use super::*;
    #[cfg(unix)]
    use anodizer_core::config::PullRequestBaseConfig;
    use anodizer_core::config::{
        Config, GitRepoConfig, PublishConfig, PullRequestConfig, RepositoryConfig,
    };
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};
    use anodizer_core::test_helpers::fake_tool::FakeToolDir;
    use anodizer_core::test_helpers::scripted_responder::{
        ScriptedRoute, spawn_scripted_responder,
    };
    use serial_test::serial;
    use std::collections::HashMap;
    use std::path::Path;
    use std::process::Command;

    fn quiet() -> StageLogger {
        StageLogger::new("publish", Verbosity::Quiet)
    }

    fn git_ok(dir: &Path, args: &[&str]) {
        anodizer_core::test_helpers::git_test_ok(dir, args)
    }

    fn git_stdout(dir: &Path, args: &[&str]) -> String {
        anodizer_core::test_helpers::git_test_stdout(dir, args)
    }

    /// Build a bare "winget-pkgs fork" repo with one commit on `main`
    /// (the branch the publish path's `--depth=1` clone defaults to).
    /// Returns `(bare_path_string, _bare_holder)`. The live publish
    /// clones this, writes the 3-file manifest set, commits a versioned
    /// branch, and pushes it back here.
    fn init_bare_fork() -> (String, tempfile::TempDir) {
        let bare = tempfile::tempdir().expect("bare tempdir");
        let seed = tempfile::tempdir().expect("seed tempdir");
        git_ok(bare.path(), &["init", "--bare", "-b", "main"]);
        git_ok(seed.path(), &["init", "-b", "main"]);
        git_ok(seed.path(), &["config", "user.email", "t@example.invalid"]);
        git_ok(seed.path(), &["config", "user.name", "Test"]);
        git_ok(seed.path(), &["config", "commit.gpgsign", "false"]);
        std::fs::write(seed.path().join("README"), "winget-pkgs\n").unwrap();
        git_ok(seed.path(), &["add", "README"]);
        git_ok(seed.path(), &["commit", "-m", "seed"]);
        assert!(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["remote", "add", "origin"])
                        .arg(bare.path())
                        .current_dir(seed.path());
                    cmd
                },
                "git",
            )
            .status
            .success()
        );
        git_ok(seed.path(), &["push", "-u", "origin", "main"]);
        (bare.path().to_string_lossy().into_owned(), bare)
    }

    /// A `gh` stub that exits non-zero on `--version` so
    /// `gh_is_available()` is false → the PR transport falls to the API
    /// path. Returns the on-disk stub holder + the PATH guard (which
    /// also holds the env mutex for the test's lifetime).
    fn gh_absent() -> (
        FakeToolDir,
        anodizer_core::test_helpers::fake_tool::PathGuard,
    ) {
        let tools = FakeToolDir::new();
        tools.tool("gh").exit(1).install();
        let guard = tools.activate();
        (tools, guard)
    }

    /// A SUCCEEDING `gh` stub: exits 0 for both `gh --version` (so
    /// `gh_is_available()` is true → the PR transport takes the
    /// `gh pr create` CLI arm, NOT the reqwest API) and the subsequent
    /// `gh pr create`. The canonical-fallback / base-override winget PR
    /// path (`submit_pr_via_gh_with_opts`) resolves its token from the
    /// *env* (`ANODIZER_GITHUB_TOKEN` / `GITHUB_TOKEN`) — which these
    /// tests do not set — so without a real `gh` it would classify as
    /// `NoneAvailable` and never touch any transport. The success stub
    /// is what exercises the real CLI submission. Returns the holder
    /// (for `.calls("gh")` argv assertions) + the PATH guard (holds the
    /// env mutex for the `#[serial]` test).
    #[cfg(unix)]
    fn gh_present() -> (
        FakeToolDir,
        anodizer_core::test_helpers::fake_tool::PathGuard,
    ) {
        let tools = FakeToolDir::new();
        tools
            .tool("gh")
            .stdout("https://github.com/microsoft/winget-pkgs/pull/1\n")
            .exit(0)
            .install();
        let guard = tools.activate();
        (tools, guard)
    }

    /// Point the scripted responder's address at the winget PR path by
    /// injecting `ANODIZER_GITHUB_API_BASE` into the Context's env source.
    /// The base is per-Context, not process-global, so no env mutation and
    /// no teardown is needed; PATH stays process-global via the
    /// `gh_absent`/`gh_present` `PathGuard`.
    fn inject_api_base(ctx: &mut Context, addr: &std::net::SocketAddr) {
        ctx.set_env_source(
            anodizer_core::MapEnvSource::new()
                .with("ANODIZER_GITHUB_API_BASE", format!("http://{addr}")),
        );
    }

    /// Return the value that immediately follows `flag` in a recorded
    /// `gh` argv (e.g. the `microsoft/winget-pkgs` after `--repo`), or
    /// `None` if the flag is absent or has no following token.
    #[cfg(unix)]
    fn gh_arg(argv: &[String], flag: &str) -> Option<String> {
        argv.iter()
            .position(|a| a == flag)
            .and_then(|i| argv.get(i + 1))
            .cloned()
    }

    /// Register a Windows zip archive (carrying the `url` / `sha256` /
    /// `format` metadata the installer manifest reads) for `crate_name`.
    fn add_windows_zip(ctx: &mut Context, crate_name: &str, sha: &str) {
        let target = "x86_64-pc-windows-msvc";
        let mut meta = HashMap::new();
        meta.insert(
            "url".to_string(),
            format!(
                "https://github.com/acme/widget/releases/download/v1.0.0/{crate_name}-{target}.zip"
            ),
        );
        meta.insert("sha256".to_string(), sha.to_string());
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
        let mut bin_meta = HashMap::new();
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

    /// A crate whose winget block clones from the local bare repo
    /// (`git.url`) and PRs same-repo (no cross-repo fork-sync), forcing
    /// the API transport when `gh` is absent. `pull_request.enabled` is
    /// true so the `maybe_submit_pr` path (not the canonical
    /// `microsoft/winget-pkgs` fallback) is taken; with no `base`, the
    /// upstream == the fork, so the PR is same-repo.
    fn live_winget_crate(crate_name: &str, bare_url: &str) -> CrateConfig {
        CrateConfig {
            name: crate_name.to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            publish: Some(PublishConfig {
                winget: Some(WingetConfig {
                    publisher: Some("AcmeCo".to_string()),
                    short_description: Some("Manage widgets".to_string()),
                    license: Some("MIT".to_string()),
                    repository: Some(RepositoryConfig {
                        owner: Some("fork-owner".to_string()),
                        name: Some("winget-pkgs".to_string()),
                        token: Some("ghp_test".to_string()),
                        git: Some(GitRepoConfig {
                            url: Some(bare_url.to_string()),
                            ssh_command: None,
                            private_key: None,
                        }),
                        pull_request: Some(PullRequestConfig {
                            enabled: Some(true),
                            base: None,
                            draft: None,
                            body: None,
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn build_ctx(crates: Vec<CrateConfig>, version: &str) -> Context {
        let config = Config {
            crates,
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", version);
        ctx.template_vars_mut().set("RawVersion", version);
        ctx.template_vars_mut().set("Tag", &format!("v{version}"));
        ctx
    }

    /// The on-disk manifest path the publish path computes for an
    /// auto-`path` package: `manifests/<l>/<Pub>/<Pkg>/<Version>/`.
    fn manifest_show(bare: &Path, branch: &str, file: &str) -> String {
        git_stdout(bare, &["show", &format!("{branch}:{file}")])
    }

    /// FULL single-crate live publish: clone the (local) fork, write the
    /// 3-file manifest set under `manifests/a/AcmeCo/widget/1.0.0/`,
    /// commit the `AcmeCo.widget-1.0.0` branch, push it to the bare repo,
    /// then submit the PR via the API transport. Asserts BOTH real side
    /// effects:
    ///   (1) the bare repo gained the versioned branch carrying the three
    ///       manifest files at the right winget path, the version /
    ///       installer manifests carrying the crate's real sha256 +
    ///       PackageIdentifier, and
    ///   (2) the PR-create POST reached the responder at the same-repo
    ///       `/repos/fork-owner/winget-pkgs/pulls` with head = fork:branch.
    #[cfg(unix)]
    #[test]
    #[serial(path_env)]
    fn publish_pushes_three_manifests_and_opens_pr() {
        let (_tools, _guard) = gh_absent();
        let (bare_url, bare) = init_bare_fork();
        let (addr, req_log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/fork-owner/winget-pkgs/pulls",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
            times: Some(1),
        }]);
        let c = live_winget_crate("widget", &bare_url);
        let mut ctx = build_ctx(vec![c], "1.0.0");
        inject_api_base(&mut ctx, &addr);
        let sha = "c".repeat(64);
        add_windows_zip(&mut ctx, "widget", &sha);

        publish_to_winget(&mut ctx, "widget", &quiet()).expect("publish ok");

        // (1) The versioned branch landed in the bare repo.
        let branch = "AcmeCo.widget-1.0.0";
        let branches = git_stdout(bare.path(), &["branch", "--list"]);
        assert!(
            branches.contains(branch),
            "publish must push the versioned branch; bare branches:\n{branches}"
        );

        // The 3-file manifest set landed at the canonical winget path.
        let dir = "manifests/a/AcmeCo/widget/1.0.0";
        let ver = manifest_show(bare.path(), branch, &format!("{dir}/AcmeCo.widget.yaml"));
        assert!(
            ver.contains("PackageIdentifier: AcmeCo.widget")
                && ver.contains("PackageVersion: 1.0.0")
                && ver.contains("ManifestType: version"),
            "version manifest content wrong:\n{ver}"
        );
        let inst = manifest_show(
            bare.path(),
            branch,
            &format!("{dir}/AcmeCo.widget.installer.yaml"),
        );
        assert!(
            inst.contains(&format!("InstallerSha256: {}", sha.to_uppercase()))
                || inst.contains(&format!("InstallerSha256: {sha}")),
            "installer manifest must carry the crate's real sha256; got:\n{inst}"
        );
        assert!(
            inst.contains("Architecture: x64"),
            "amd64 must map to winget x64 in the pushed manifest:\n{inst}"
        );
        let locale = manifest_show(
            bare.path(),
            branch,
            &format!("{dir}/AcmeCo.widget.locale.en-US.yaml"),
        );
        assert!(
            locale.contains("ShortDescription: Manage widgets")
                && locale.contains("ManifestType: defaultLocale"),
            "locale manifest content wrong:\n{locale}"
        );

        // (2) The PR-create POST hit the same-repo upstream slug with the
        //     fork:branch head.
        let entries = req_log.lock().unwrap();
        assert_eq!(entries.len(), 1, "exactly one PR-create POST expected");
        assert_eq!(entries[0].path, "/repos/fork-owner/winget-pkgs/pulls");
        let payload: serde_json::Value = serde_json::from_str(&entries[0].body).expect("JSON body");
        assert_eq!(
            payload["head"], "fork-owner:AcmeCo.widget-1.0.0",
            "head must be fork-owner:<package_id>-<version>"
        );
        assert_eq!(
            payload["base"], "main",
            "base branch must be the fork default"
        );
        drop(entries);
        drop(bare);
    }

    /// A custom `commit_msg_template` referencing `{{ PackageIdentifier }}`
    /// / `{{ Version }}` must be rendered into the pushed commit's
    /// subject. Pins `render_winget_commit_msg` end-to-end through the
    /// push (the in-process render test only proves the string; this
    /// proves it reaches the actual git commit).
    #[test]
    #[serial(path_env)]
    fn publish_renders_custom_commit_message_into_pushed_commit() {
        let (_tools, _guard) = gh_absent();
        let (bare_url, bare) = init_bare_fork();
        let (addr, _l) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/fork-owner/winget-pkgs/pulls",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
            times: None,
        }]);
        let mut c = live_winget_crate("widget", &bare_url);
        if let Some(w) = c.publish.as_mut().and_then(|p| p.winget.as_mut()) {
            w.commit_msg_template =
                Some("Bump {{ PackageIdentifier }} to {{ Version }}".to_string());
        }
        let mut ctx = build_ctx(vec![c], "2.5.0");
        inject_api_base(&mut ctx, &addr);
        add_windows_zip(&mut ctx, "widget", &"d".repeat(64));

        publish_to_winget(&mut ctx, "widget", &quiet()).expect("publish ok");

        let subject = git_stdout(
            bare.path(),
            &["log", "-1", "--format=%s", "AcmeCo.widget-2.5.0"],
        );
        assert_eq!(
            subject, "Bump AcmeCo.widget to 2.5.0",
            "pushed commit subject must carry the rendered custom template"
        );
        drop(bare);
    }

    /// The PR-already-exists path: the API transport returns 422 "already
    /// exists" and the publisher records a `PendingValidation` override
    /// (so the dispatch summary tells the truth instead of `succeeded`).
    /// The branch push still happened first.
    #[cfg(unix)]
    #[test]
    #[serial(path_env)]
    fn publish_already_exists_records_pending_validation() {
        let (_tools, _guard) = gh_absent();
        let (bare_url, bare) = init_bare_fork();
        let body = "{\"message\":\"Validation Failed\",\"errors\":[{\"message\":\"A pull request already exists for fork-owner:AcmeCo.widget-1.0.0.\"}]}";
        let (addr, _l) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/fork-owner/winget-pkgs/pulls",
            response: Box::leak(
                format!(
                    "HTTP/1.1 422 Unprocessable Entity\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                )
                .into_boxed_str(),
            ),
            times: Some(1),
        }]);
        let c = live_winget_crate("widget", &bare_url);
        let mut ctx = build_ctx(vec![c], "1.0.0");
        inject_api_base(&mut ctx, &addr);
        add_windows_zip(&mut ctx, "widget", &"e".repeat(64));

        publish_to_winget(&mut ctx, "widget", &quiet()).expect("publish ok");

        // The branch push happened before the PR call.
        let branches = git_stdout(bare.path(), &["branch", "--list"]);
        assert!(
            branches.contains("AcmeCo.widget-1.0.0"),
            "branch push must precede the PR call:\n{branches}"
        );
        let pending = ctx.take_pending_outcome();
        assert!(
            matches!(
                pending,
                Some(anodizer_core::PublisherOutcome::PendingValidation)
            ),
            "422 already-exists must record PendingValidation, got {pending:?}"
        );
        drop(bare);
    }

    /// A non-success, non-422 HTTP status from the PR-create POST must be
    /// surfaced as a `Failed` outcome (silent-fail would let dispatch
    /// record `succeeded`). The branch still pushed.
    #[test]
    #[serial(path_env)]
    fn publish_pr_http_error_records_failed_outcome() {
        let (_tools, _guard) = gh_absent();
        let (bare_url, bare) = init_bare_fork();
        let (addr, _l) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/fork-owner/winget-pkgs/pulls",
            response: "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 3\r\n\r\nboo",
            times: Some(1),
        }]);
        let c = live_winget_crate("widget", &bare_url);
        let mut ctx = build_ctx(vec![c], "1.0.0");
        inject_api_base(&mut ctx, &addr);
        add_windows_zip(&mut ctx, "widget", &"f".repeat(64));

        publish_to_winget(&mut ctx, "widget", &quiet()).expect("publish returns Ok");

        let pending = ctx.take_pending_outcome();
        assert!(
            matches!(pending, Some(anodizer_core::PublisherOutcome::Failed(_))),
            "a 500 from PR-create must record Failed, got {pending:?}"
        );
        drop(bare);
    }

    /// Idempotent re-publish: a second publish of the identical manifest
    /// onto the same branch finds the remote tree already matching, so
    /// `commit_and_push_with_opts` reports `NoChanges` and nothing is
    /// re-pushed. Proves the publish path does not blindly force a commit.
    #[test]
    #[serial(path_env)]
    fn publish_idempotent_second_run_no_changes() {
        let (_tools, _guard) = gh_absent();
        let (bare_url, bare) = init_bare_fork();
        let (addr, _l) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/fork-owner/winget-pkgs/pulls",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
            times: None,
        }]);
        let sha = "1".repeat(64);
        let build = || {
            let c = live_winget_crate("widget", &bare_url);
            let mut ctx = build_ctx(vec![c], "1.0.0");
            inject_api_base(&mut ctx, &addr);
            add_windows_zip(&mut ctx, "widget", &sha);
            ctx
        };

        let mut ctx1 = build();
        publish_to_winget(&mut ctx1, "widget", &quiet()).expect("first publish");
        let head1 = git_stdout(bare.path(), &["rev-parse", "AcmeCo.widget-1.0.0"]);

        let mut ctx2 = build();
        publish_to_winget(&mut ctx2, "widget", &quiet()).expect("second publish");
        let head2 = git_stdout(bare.path(), &["rev-parse", "AcmeCo.widget-1.0.0"]);
        assert_eq!(
            head1, head2,
            "re-publishing the identical manifest must not advance the branch tip"
        );
        drop(bare);
    }

    /// Workspace per-crate mode: two winget crates sharing one bare fork
    /// must each land their OWN 3-file manifest set under their OWN
    /// `<package_id>` path on their OWN `<package_id>-<version>` branch —
    /// proving per-crate name/package-id/branch resolution is not
    /// clobbered by a sibling.
    #[test]
    #[serial(path_env)]
    fn publish_workspace_per_crate_distinct_branches_and_paths() {
        let (_tools, _guard) = gh_absent();
        let (bare_url, bare) = init_bare_fork();
        let (addr, _l) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/fork-owner/winget-pkgs/pulls",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
            times: None,
        }]);
        let alpha = live_winget_crate("alpha", &bare_url);
        let beta = live_winget_crate("beta", &bare_url);
        let mut ctx = build_ctx(vec![alpha, beta], "3.1.0");
        inject_api_base(&mut ctx, &addr);
        add_windows_zip(&mut ctx, "alpha", &"a".repeat(64));
        add_windows_zip(&mut ctx, "beta", &"b".repeat(64));

        publish_to_winget(&mut ctx, "alpha", &quiet()).expect("publish alpha");
        publish_to_winget(&mut ctx, "beta", &quiet()).expect("publish beta");

        let branches = git_stdout(bare.path(), &["branch", "--list"]);
        assert!(
            branches.contains("AcmeCo.alpha-3.1.0"),
            "alpha branch missing; got:\n{branches}"
        );
        assert!(
            branches.contains("AcmeCo.beta-3.1.0"),
            "beta branch missing; got:\n{branches}"
        );
        // Each branch carries only its own package's manifest path.
        let alpha_ver = manifest_show(
            bare.path(),
            "AcmeCo.alpha-3.1.0",
            "manifests/a/AcmeCo/alpha/3.1.0/AcmeCo.alpha.yaml",
        );
        assert!(
            alpha_ver.contains("PackageIdentifier: AcmeCo.alpha")
                && alpha_ver.contains("PackageVersion: 3.1.0"),
            "alpha manifest wrong:\n{alpha_ver}"
        );
        let beta_ver = manifest_show(
            bare.path(),
            "AcmeCo.beta-3.1.0",
            "manifests/a/AcmeCo/beta/3.1.0/AcmeCo.beta.yaml",
        );
        assert!(
            beta_ver.contains("PackageIdentifier: AcmeCo.beta"),
            "beta manifest wrong:\n{beta_ver}"
        );
        drop(bare);
    }

    /// The canonical-upstream fallback: with `pull_request.enabled` unset,
    /// `submit_winget_pr` submits the PR against `microsoft/winget-pkgs`
    /// (the live winget index) via the `gh pr create` CLI, head =
    /// fork:branch. The branch still lands in the local bare fork.
    ///
    /// Transport reality: this arm calls `submit_pr_via_gh_with_opts`,
    /// which dispatches via `classify_pr_transport(gh_is_available(),
    /// token.is_some())` and resolves its token from the ENV, not the
    /// config. With a succeeding `gh` stub on PATH the transport is the
    /// CLI (`gh pr create ... --repo microsoft/winget-pkgs`), so the PR
    /// shape is asserted from the recorded `gh` argv — there is no
    /// reqwest API POST on this path. The default-branch GET still fires
    /// (it runs before the transport match) and feeds `--base`.
    #[cfg(unix)]
    #[test]
    #[serial(path_env)]
    fn publish_without_pr_config_targets_microsoft_winget_pkgs() {
        let (tools, _guard) = gh_present();
        let (bare_url, bare) = init_bare_fork();
        // `submit_pr_via_gh_with_opts` resolves the upstream default
        // branch via this GET (token-less, but the request still fires)
        // before invoking `gh pr create`; it feeds `--base`.
        let (addr, _req_log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/microsoft/winget-pkgs",
            response: "HTTP/1.1 200 OK\r\nContent-Length: 27\r\n\r\n{\"default_branch\":\"master\"}",
            times: Some(1),
        }]);
        let mut c = live_winget_crate("widget", &bare_url);
        // Drop the pull_request block so the canonical-fallback arm runs.
        if let Some(w) = c.publish.as_mut().and_then(|p| p.winget.as_mut())
            && let Some(r) = w.repository.as_mut()
        {
            r.pull_request = None;
        }
        let mut ctx = build_ctx(vec![c], "1.0.0");
        inject_api_base(&mut ctx, &addr);
        add_windows_zip(&mut ctx, "widget", &"9".repeat(64));

        publish_to_winget(&mut ctx, "widget", &quiet()).expect("publish ok");

        let branches = git_stdout(bare.path(), &["branch", "--list"]);
        assert!(
            branches.contains("AcmeCo.widget-1.0.0"),
            "branch must still push to the fork:\n{branches}"
        );

        // The PR was submitted via the `gh pr create` CLI; find that
        // invocation (the first recorded `gh` call is `--version` from
        // `gh_is_available()`).
        let gh_calls = tools.calls("gh");
        let pr_create = gh_calls
            .iter()
            .find(|argv| argv.first().map(String::as_str) == Some("pr"))
            .expect("a `gh pr create` invocation must be recorded");
        assert_eq!(
            &pr_create[0..2],
            &["pr".to_string(), "create".to_string()],
            "must be `gh pr create`; got: {pr_create:?}"
        );
        assert_eq!(
            gh_arg(pr_create, "--repo").as_deref(),
            Some("microsoft/winget-pkgs"),
            "PR must target the canonical winget index; got: {pr_create:?}"
        );
        assert_eq!(
            gh_arg(pr_create, "--head").as_deref(),
            Some("fork-owner:AcmeCo.widget-1.0.0"),
            "head must be fork:<package_id>-<version>; got: {pr_create:?}"
        );
        // `--base` is the upstream default branch the GET resolved.
        assert_eq!(
            gh_arg(pr_create, "--base").as_deref(),
            Some("master"),
            "base must be the resolved upstream default branch; got: {pr_create:?}"
        );
        drop(bare);
    }

    /// A configured `pull_request.base` overrides the canonical upstream:
    /// the `gh pr create` invocation must target the configured mirror
    /// slug (`acme/winget-mirror`), not `microsoft/winget-pkgs`. Exercises
    /// `submit_winget_pr`'s `has_pr_config=false` + explicit-base arm
    /// (base set but `enabled` unset), which submits via the `gh` CLI
    /// (token resolved from env, absent here, so a succeeding `gh` stub
    /// drives the real transport — there is no reqwest API POST on this
    /// path). The default-branch GET fires against the OVERRIDDEN slug
    /// and feeds `--base`.
    #[cfg(unix)]
    #[test]
    #[serial(path_env)]
    fn publish_honors_pull_request_base_override() {
        let (tools, _guard) = gh_present();
        let (bare_url, bare) = init_bare_fork();
        let (addr, _req_log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/acme/winget-mirror",
            response: "HTTP/1.1 200 OK\r\nContent-Length: 27\r\n\r\n{\"default_branch\":\"master\"}",
            times: Some(1),
        }]);
        let mut c = live_winget_crate("widget", &bare_url);
        if let Some(w) = c.publish.as_mut().and_then(|p| p.winget.as_mut())
            && let Some(r) = w.repository.as_mut()
        {
            // base set, enabled left unset → canonical-fallback arm picks
            // up the explicit base slug instead of microsoft/winget-pkgs.
            r.pull_request = Some(PullRequestConfig {
                enabled: None,
                base: Some(PullRequestBaseConfig {
                    owner: Some("acme".to_string()),
                    name: Some("winget-mirror".to_string()),
                    branch: None,
                }),
                draft: None,
                body: None,
            });
        }
        let mut ctx = build_ctx(vec![c], "1.0.0");
        inject_api_base(&mut ctx, &addr);
        add_windows_zip(&mut ctx, "widget", &"7".repeat(64));

        publish_to_winget(&mut ctx, "widget", &quiet()).expect("publish ok");

        // The PR was submitted via `gh pr create` against the OVERRIDDEN
        // slug. The first recorded `gh` call is `--version`.
        let gh_calls = tools.calls("gh");
        let pr_create = gh_calls
            .iter()
            .find(|argv| argv.first().map(String::as_str) == Some("pr"))
            .expect("a `gh pr create` invocation must be recorded");
        assert_eq!(
            gh_arg(pr_create, "--repo").as_deref(),
            Some("acme/winget-mirror"),
            "PR must target the configured base override, not microsoft; got: {pr_create:?}"
        );
        assert_eq!(
            gh_arg(pr_create, "--head").as_deref(),
            Some("fork-owner:AcmeCo.widget-1.0.0"),
            "head must be fork:<package_id>-<version>; got: {pr_create:?}"
        );
        assert_eq!(
            gh_arg(pr_create, "--base").as_deref(),
            Some("master"),
            "base must be the overridden upstream's resolved default branch; got: {pr_create:?}"
        );
        drop(bare);
    }

    /// A custom `path` override redirects the written manifests away from
    /// the auto `manifests/<l>/<Pub>/<Pkg>/<Version>/` layout to the
    /// operator-chosen subtree. The pushed branch must carry the manifest
    /// files under that path, proving `write_winget_manifests_to_disk`'s
    /// path-override branch runs through the real push.
    #[test]
    #[serial(path_env)]
    fn publish_honors_path_override() {
        let (_tools, _guard) = gh_absent();
        let (bare_url, bare) = init_bare_fork();
        let (addr, _l) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/fork-owner/winget-pkgs/pulls",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
            times: None,
        }]);
        let mut c = live_winget_crate("widget", &bare_url);
        if let Some(w) = c.publish.as_mut().and_then(|p| p.winget.as_mut()) {
            w.path = Some("custom/manifests/here".to_string());
        }
        let mut ctx = build_ctx(vec![c], "1.0.0");
        inject_api_base(&mut ctx, &addr);
        add_windows_zip(&mut ctx, "widget", &"3".repeat(64));

        publish_to_winget(&mut ctx, "widget", &quiet()).expect("publish ok");

        let ver = manifest_show(
            bare.path(),
            "AcmeCo.widget-1.0.0",
            "custom/manifests/here/AcmeCo.widget.yaml",
        );
        assert!(
            ver.contains("PackageIdentifier: AcmeCo.widget"),
            "manifest must land under the custom path override:\n{ver}"
        );
        drop(bare);
    }

    /// A `skip_upload: true` short-circuits `publish_to_winget` BEFORE any
    /// clone/push: the bare fork gains no new branch and no PR POST fires.
    /// Proves the skip gate guards the whole side-effecting flow.
    #[test]
    #[serial(path_env)]
    fn publish_skip_upload_true_performs_no_side_effects() {
        let (_tools, _guard) = gh_absent();
        let (bare_url, bare) = init_bare_fork();
        let (addr, req_log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/fork-owner/winget-pkgs/pulls",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
            times: None,
        }]);
        let mut c = live_winget_crate("widget", &bare_url);
        if let Some(w) = c.publish.as_mut().and_then(|p| p.winget.as_mut()) {
            w.skip_upload = Some(anodizer_core::config::StringOrBool::Bool(true));
        }
        let mut ctx = build_ctx(vec![c], "1.0.0");
        inject_api_base(&mut ctx, &addr);
        add_windows_zip(&mut ctx, "widget", &"5".repeat(64));

        publish_to_winget(&mut ctx, "widget", &quiet()).expect("publish ok");

        let branches = git_stdout(bare.path(), &["branch", "--list"]);
        assert!(
            !branches.contains("AcmeCo.widget-1.0.0"),
            "skip_upload must push no branch; got:\n{branches}"
        );
        assert!(
            req_log.lock().unwrap().is_empty(),
            "skip_upload must fire no PR POST"
        );
        drop(bare);
    }

    /// Dry-run mode short-circuits AFTER identity resolution but BEFORE
    /// clone/push/PR: no branch, no POST. Pins the `ctx.is_dry_run()`
    /// guard in `publish_to_winget`.
    #[test]
    #[serial(path_env)]
    fn publish_dry_run_performs_no_side_effects() {
        let (_tools, _guard) = gh_absent();
        let (bare_url, bare) = init_bare_fork();
        let (addr, req_log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/fork-owner/winget-pkgs/pulls",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
            times: None,
        }]);
        let c = live_winget_crate("widget", &bare_url);
        let config = Config {
            crates: vec![c],
            ..Default::default()
        };
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        inject_api_base(&mut ctx, &addr);
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("RawVersion", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        add_windows_zip(&mut ctx, "widget", &"2".repeat(64));

        publish_to_winget(&mut ctx, "widget", &quiet()).expect("dry-run ok");

        let branches = git_stdout(bare.path(), &["branch", "--list"]);
        assert!(
            !branches.contains("AcmeCo.widget-1.0.0"),
            "dry-run must push no branch; got:\n{branches}"
        );
        assert!(
            req_log.lock().unwrap().is_empty(),
            "dry-run must fire no PR POST"
        );
        drop(bare);
    }

    /// A windows archive missing its `sha256` metadata must hard-fail the
    /// publish BEFORE any push: a manifest with `InstallerSha256: ''` is
    /// rejected by winget validation, so anodizer must error rather than
    /// push it. Pins `build_archive_installer`'s sha256 guard through the
    /// live entrypoint, and confirms no branch leaked to the fork.
    #[test]
    #[serial(path_env)]
    fn publish_missing_sha256_errors_before_push() {
        let (_tools, _guard) = gh_absent();
        let (bare_url, bare) = init_bare_fork();
        let (addr, req_log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/fork-owner/winget-pkgs/pulls",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
            times: None,
        }]);
        let c = live_winget_crate("widget", &bare_url);
        let mut ctx = build_ctx(vec![c], "1.0.0");
        inject_api_base(&mut ctx, &addr);
        // Archive with NO sha256 metadata.
        let target = "x86_64-pc-windows-msvc";
        let mut meta = HashMap::new();
        meta.insert("format".to_string(), "zip".to_string());
        meta.insert(
            "url".to_string(),
            "https://example.com/widget.zip".to_string(),
        );
        ctx.artifacts.add(anodizer_core::artifact::Artifact {
            kind: anodizer_core::artifact::ArtifactKind::Archive,
            path: std::path::PathBuf::from("/dist/widget.zip"),
            name: "widget.zip".to_string(),
            target: Some(target.to_string()),
            crate_name: "widget".to_string(),
            metadata: meta,
            size: None,
        });

        let err =
            publish_to_winget(&mut ctx, "widget", &quiet()).expect_err("missing sha256 must bail");
        assert!(
            format!("{err:#}").contains("no sha256"),
            "error must name the missing sha256; got: {err:#}"
        );
        // No branch / PR side effect leaked.
        let branches = git_stdout(bare.path(), &["branch", "--list"]);
        assert!(
            !branches.contains("AcmeCo.widget-1.0.0"),
            "a sha256 bail must leave no pushed branch:\n{branches}"
        );
        assert!(
            req_log.lock().unwrap().is_empty(),
            "a sha256 bail must fire no PR POST"
        );
        drop(bare);
    }

    /// No Windows artifact at all → the publish bails with the
    /// "no Windows archive or binary artifact" error before any push.
    /// Pins `collect_winget_installers`'s empty guard through the live
    /// entrypoint.
    #[test]
    #[serial(path_env)]
    fn publish_no_windows_artifact_errors_before_push() {
        let (_tools, _guard) = gh_absent();
        let (bare_url, bare) = init_bare_fork();
        let (addr, req_log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/fork-owner/winget-pkgs/pulls",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
            times: None,
        }]);
        let c = live_winget_crate("widget", &bare_url);
        let mut ctx = build_ctx(vec![c], "1.0.0");
        inject_api_base(&mut ctx, &addr);
        // A LINUX archive only — not a winget Windows installer.
        let mut meta = HashMap::new();
        meta.insert("format".to_string(), "tar.gz".to_string());
        meta.insert("sha256".to_string(), "a".repeat(64));
        ctx.artifacts.add(anodizer_core::artifact::Artifact {
            kind: anodizer_core::artifact::ArtifactKind::Archive,
            path: std::path::PathBuf::from("/dist/widget-linux.tar.gz"),
            name: "widget-linux.tar.gz".to_string(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "widget".to_string(),
            metadata: meta,
            size: None,
        });

        let err = publish_to_winget(&mut ctx, "widget", &quiet())
            .expect_err("no windows artifact must bail");
        assert!(
            format!("{err:#}").contains("no Windows archive or binary artifact"),
            "got: {err:#}"
        );
        assert!(
            req_log.lock().unwrap().is_empty(),
            "an artifact bail must fire no PR POST"
        );
        drop(bare);
    }

    /// `update_existing_pr` is a no-op for the API transport (it cannot
    /// force-push without a working tree), so an existing-PR 422 still
    /// records `PendingValidation` even with the flag set, and the
    /// publisher warns that `gh` CLI is required for in-place updates.
    /// The branch push still happened. Pins the API-transport arm of the
    /// `update_existing_pr` semantics.
    #[cfg(unix)]
    #[test]
    #[serial(path_env)]
    fn publish_update_existing_pr_via_api_is_noop_records_pending() {
        let (_tools, _guard) = gh_absent();
        let (bare_url, bare) = init_bare_fork();
        let body = "{\"message\":\"Validation Failed\",\"errors\":[{\"message\":\"A pull request already exists for fork-owner:AcmeCo.widget-1.0.0.\"}]}";
        let (addr, _l) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/fork-owner/winget-pkgs/pulls",
            response: Box::leak(
                format!(
                    "HTTP/1.1 422 Unprocessable Entity\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                )
                .into_boxed_str(),
            ),
            times: Some(1),
        }]);
        let mut c = live_winget_crate("widget", &bare_url);
        if let Some(w) = c.publish.as_mut().and_then(|p| p.winget.as_mut()) {
            w.update_existing_pr = Some(anodizer_core::config::StringOrBool::Bool(true));
        }
        let mut ctx = build_ctx(vec![c], "1.0.0");
        inject_api_base(&mut ctx, &addr);
        add_windows_zip(&mut ctx, "widget", &"8".repeat(64));

        publish_to_winget(&mut ctx, "widget", &quiet()).expect("publish ok");

        let pending = ctx.take_pending_outcome();
        assert!(
            matches!(
                pending,
                Some(anodizer_core::PublisherOutcome::PendingValidation)
            ),
            "update_existing_pr over the API transport must still record \
                 PendingValidation on a 422, got {pending:?}"
        );
        drop(bare);
    }

    /// A `winget.description` template that fails to render (undefined
    /// field) falls back to its raw `{{ }}` text via `render_or_warn` and
    /// lands in the locale manifest — `guard_no_unrendered` must hard-fail
    /// the real publish before any branch is pushed, naming the manifest.
    #[test]
    #[serial(path_env)]
    fn publish_residual_description_template_errors_before_push() {
        let (_tools, _guard) = gh_absent();
        let (bare_url, bare) = init_bare_fork();
        let (addr, req_log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/fork-owner/winget-pkgs/pulls",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
            times: None,
        }]);
        let mut c = live_winget_crate("widget", &bare_url);
        if let Some(w) = c.publish.as_mut().and_then(|p| p.winget.as_mut()) {
            w.description = Some("{{ .NoSuchField }}".to_string());
        }
        let mut ctx = build_ctx(vec![c], "1.0.0");
        inject_api_base(&mut ctx, &addr);
        add_windows_zip(&mut ctx, "widget", &"e".repeat(64));

        let err = publish_to_winget(&mut ctx, "widget", &quiet())
            .expect_err("residual {{ }} in the locale manifest must hard-fail");
        assert!(
            format!("{err:#}").contains("winget locale manifest"),
            "error must name the manifest label; got: {err:#}"
        );
        let branches = git_stdout(bare.path(), &["branch", "--list"]);
        assert!(
            !branches.contains("AcmeCo.widget-1.0.0"),
            "a residual-delimiter bail must leave no pushed branch:\n{branches}"
        );
        assert!(
            req_log.lock().unwrap().is_empty(),
            "a residual-delimiter bail must fire no PR POST"
        );
        drop(bare);
    }

    /// The same residual `winget.description` template stays lenient in
    /// dry-run: `publish_to_winget` early-returns before the manifest
    /// render (and therefore before the guard) so it must still report
    /// `Ok`, not surface the residual as an error.
    #[test]
    #[serial(path_env)]
    fn publish_residual_description_template_dry_run_stays_lenient() {
        let (_tools, _guard) = gh_absent();
        let (bare_url, _bare) = init_bare_fork();
        let mut c = live_winget_crate("widget", &bare_url);
        if let Some(w) = c.publish.as_mut().and_then(|p| p.winget.as_mut()) {
            w.description = Some("{{ .NoSuchField }}".to_string());
        }
        let config = Config {
            crates: vec![c],
            ..Default::default()
        };
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("RawVersion", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        add_windows_zip(&mut ctx, "widget", &"f".repeat(64));

        publish_to_winget(&mut ctx, "widget", &quiet())
            .expect("dry-run must stay lenient on a residual template");
    }

    /// A broken `winget.url_template` (referencing an undefined field)
    /// fails `render_url_template_with_ctx`'s Tera pass and falls back to
    /// its own raw `{{ }}` text (the silent, non-strict-aware fallback in
    /// `resolve_installer_url`), landing the residual in the InstallerUrl
    /// of the INSTALLER manifest only — never the version or locale
    /// manifest. `guard_no_unrendered` must hard-fail the real publish
    /// before the fork clone, naming `"winget installer manifest"`.
    #[test]
    #[serial(path_env)]
    fn publish_residual_url_template_errors_before_push() {
        let (_tools, _guard) = gh_absent();
        let (bare_url, bare) = init_bare_fork();
        let (addr, req_log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/fork-owner/winget-pkgs/pulls",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
            times: None,
        }]);
        let mut c = live_winget_crate("widget", &bare_url);
        if let Some(w) = c.publish.as_mut().and_then(|p| p.winget.as_mut()) {
            w.url_template = Some("{{ .NoSuchField }}".to_string());
        }
        let mut ctx = build_ctx(vec![c], "1.0.0");
        inject_api_base(&mut ctx, &addr);
        add_windows_zip(&mut ctx, "widget", &"a".repeat(64));

        let err = publish_to_winget(&mut ctx, "widget", &quiet())
            .expect_err("residual {{ }} in the installer manifest must hard-fail");
        assert!(
            format!("{err:#}").contains("winget installer manifest"),
            "error must name the installer manifest, not version/locale; got: {err:#}"
        );
        // The guard now runs before `clone_repo`, so the same
        // no-push/no-PR evidence also proves no clone happened: a clone
        // would need to succeed before any commit could exist to push.
        let branches = git_stdout(bare.path(), &["branch", "--list"]);
        assert!(
            !branches.contains("AcmeCo.widget-1.0.0"),
            "a residual-delimiter bail must leave no pushed branch:\n{branches}"
        );
        assert!(
            req_log.lock().unwrap().is_empty(),
            "a residual-delimiter bail must fire no PR POST"
        );
        drop(bare);
    }

    /// The same broken `winget.url_template` stays lenient in dry-run:
    /// `publish_to_winget` early-returns before the manifest render (and
    /// therefore before the guard), so it must still report `Ok`.
    #[test]
    #[serial(path_env)]
    fn publish_residual_url_template_dry_run_stays_lenient() {
        let (_tools, _guard) = gh_absent();
        let (bare_url, _bare) = init_bare_fork();
        let mut c = live_winget_crate("widget", &bare_url);
        if let Some(w) = c.publish.as_mut().and_then(|p| p.winget.as_mut()) {
            w.url_template = Some("{{ .NoSuchField }}".to_string());
        }
        let config = Config {
            crates: vec![c],
            ..Default::default()
        };
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("RawVersion", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        add_windows_zip(&mut ctx, "widget", &"b".repeat(64));

        publish_to_winget(&mut ctx, "widget", &quiet())
            .expect("dry-run must stay lenient on a residual template");
    }
}
