//! Tests for the Chocolatey publisher submodules.

#![allow(clippy::field_reassign_with_default)]

use super::install::{
    FileType, InstallScriptDual, generate_install_script, generate_install_script_dual,
};
use super::nuspec::{NuspecParams, generate_nuspec};
use super::package::{create_nupkg, parse_xml_element};
use super::publish::publish_to_chocolatey;

fn default_nuspec_params<'a>() -> NuspecParams<'a> {
    NuspecParams {
        name: "mytool",
        version: "1.0.0",
        description: "A tool",
        license: "MIT",
        license_url: None,
        authors: "Author",
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
    }
}

#[test]
fn test_generate_nuspec_basic() {
    let tags = vec!["cli".to_string(), "tool".to_string()];
    let nuspec = generate_nuspec(&NuspecParams {
        name: "mytool",
        description: "A great tool",
        authors: "Test Author",
        project_url: "https://github.com/org/mytool",
        icon_url: "https://example.com/icon.png",
        tags: &tags,
        ..default_nuspec_params()
    })
    .unwrap();
    assert!(nuspec.contains("<?xml version=\"1.0\""));
    assert!(nuspec.contains("<id>mytool</id>"));
    assert!(nuspec.contains("<version>1.0.0</version>"));
    assert!(nuspec.contains("<title>mytool</title>"));
    assert!(nuspec.contains("<authors>Test Author</authors>"));
    assert!(nuspec.contains("<description>A great tool</description>"));
    assert!(nuspec.contains("<projectUrl>https://github.com/org/mytool</projectUrl>"));
    assert!(nuspec.contains("<iconUrl>https://example.com/icon.png</iconUrl>"));
    assert!(nuspec.contains("<tags>cli tool</tags>"));
    assert!(nuspec.contains("<file src=\"tools\\**\" target=\"tools\" />"));
}

#[test]
fn test_generate_nuspec_no_icon() {
    let nuspec = generate_nuspec(&default_nuspec_params()).unwrap();
    assert!(!nuspec.contains("<iconUrl>"));
}

#[test]
fn test_generate_nuspec_empty_tags_uses_name() {
    let nuspec = generate_nuspec(&default_nuspec_params()).unwrap();
    assert!(nuspec.contains("<tags>mytool</tags>"));
}

#[test]
fn test_generate_nuspec_xml_escaping() {
    let nuspec = generate_nuspec(&NuspecParams {
        name: "my-tool",
        description: "A tool for <things> & \"stuff\"",
        ..default_nuspec_params()
    })
    .unwrap();
    assert!(nuspec.contains("&lt;things&gt;"));
    assert!(nuspec.contains("&amp;"));
    assert!(nuspec.contains("&quot;stuff&quot;"));
}

#[test]
fn test_generate_nuspec_xml_escaping_authors_and_apostrophe() {
    let nuspec = generate_nuspec(&NuspecParams {
        name: "my-tool",
        authors: "O'Brien & Associates",
        description: "Tool for <things> & 'stuff'",
        ..default_nuspec_params()
    })
    .unwrap();
    assert!(nuspec.contains("<authors>O&apos;Brien &amp; Associates</authors>"));
    assert!(nuspec.contains("&apos;stuff&apos;"));
}

#[test]
fn test_generate_nuspec_xml_escaping_title_and_tags() {
    let tags = vec!["c++ & c#".to_string()];
    let nuspec = generate_nuspec(&NuspecParams {
        name: "my-tool",
        title: Some("My \"Special\" Tool"),
        tags: &tags,
        ..default_nuspec_params()
    })
    .unwrap();
    assert!(nuspec.contains("<title>My &quot;Special&quot; Tool</title>"));
    assert!(nuspec.contains("<tags>c++ &amp; c#</tags>"));
}

/// A single-SPDX license with no explicit `license_url` emits the modern
/// `<license type="expression">` element and NO synthesized `licenseUrl`
/// (the opensource.org URL is never fabricated — it 404s for compound SPDX).
#[test]
fn test_generate_nuspec_emits_license_expression_no_synthesized_url() {
    let nuspec = generate_nuspec(&NuspecParams {
        name: "tool",
        version: "2.0.0",
        license: "Apache-2.0",
        ..default_nuspec_params()
    })
    .unwrap();
    assert!(nuspec.contains("<license type=\"expression\">Apache-2.0</license>"));
    assert!(
        !nuspec.contains("opensource.org"),
        "must never synthesize an opensource.org licenseUrl"
    );
    assert!(
        !nuspec.contains("<licenseUrl>"),
        "no licenseUrl when none is derivable; got: {nuspec}"
    );
}

/// The canonical Rust dual license is a compound SPDX expression — it must
/// land verbatim in `<license type="expression">` (the legacy licenseUrl
/// synthesis 404'd on exactly this value).
#[test]
fn test_generate_nuspec_compound_spdx_expression() {
    let nuspec = generate_nuspec(&NuspecParams {
        name: "tool",
        version: "2.0.0",
        license: "MIT OR Apache-2.0",
        ..default_nuspec_params()
    })
    .unwrap();
    assert!(nuspec.contains("<license type=\"expression\">MIT OR Apache-2.0</license>"));
    assert!(!nuspec.contains("opensource.org"));
}

/// An explicit `license_url` (e.g. a GitHub LICENSE blob URL) is emitted as
/// `<licenseUrl>` alongside the SPDX `<license type="expression">`.
#[test]
fn test_generate_nuspec_real_license_url_alongside_expression() {
    let nuspec = generate_nuspec(&NuspecParams {
        name: "tool",
        version: "2.0.0",
        license: "MIT",
        license_url: Some("https://github.com/org/tool/blob/v2.0.0/LICENSE"),
        ..default_nuspec_params()
    })
    .unwrap();
    assert!(
        nuspec.contains("<licenseUrl>https://github.com/org/tool/blob/v2.0.0/LICENSE</licenseUrl>")
    );
    assert!(nuspec.contains("<license type=\"expression\">MIT</license>"));
    assert!(!nuspec.contains("opensource.org"));
}

#[test]
fn test_generate_nuspec_complete_xml_structure() {
    let tags = vec![
        "release".to_string(),
        "automation".to_string(),
        "ci".to_string(),
    ];
    let nuspec = generate_nuspec(&NuspecParams {
        name: "release-tool",
        version: "3.2.1",
        description: "Release automation",
        authors: "Jane Doe",
        project_url: "https://github.com/org/release-tool",
        icon_url: "https://example.com/icon.png",
        tags: &tags,
        ..default_nuspec_params()
    })
    .unwrap();
    assert!(nuspec.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?>"));
    assert!(nuspec.ends_with("</package>\n"));
    assert!(nuspec.contains("<metadata>"));
    assert!(nuspec.contains("</metadata>"));
    assert!(nuspec.contains("<files>"));
    assert!(nuspec.contains("</files>"));
}

#[test]
fn test_generate_install_script_basic() {
    let script = generate_install_script(
        "mytool",
        "https://example.com/mytool-1.0.0-windows-amd64.zip",
        "deadbeef",
        false,
        FileType::Zip,
    )
    .unwrap();
    assert!(script.contains("$ErrorActionPreference = 'Stop'"));
    assert!(script.contains("packageName    = 'mytool'"));
    assert!(
        script.contains("url64bit       = 'https://example.com/mytool-1.0.0-windows-amd64.zip'")
    );
    assert!(script.contains("checksum64     = 'deadbeef'"));
    assert!(script.contains("checksumType64 = 'sha256'"));
    assert!(script.contains("Install-ChocolateyZipPackage @packageArgs"));
}

#[test]
fn test_generate_install_script_32bit() {
    let script = generate_install_script(
        "mytool",
        "https://example.com/mytool-1.0.0-windows-x86.zip",
        "deadbeef",
        true,
        FileType::Zip,
    )
    .unwrap();
    assert!(script.contains("packageName   = 'mytool'"));
    assert!(script.contains("url           = 'https://example.com/mytool-1.0.0-windows-x86.zip'"));
    assert!(script.contains("checksum      = 'deadbeef'"));
    assert!(!script.contains("url64bit"));
    assert!(!script.contains("checksum64"));
}

#[test]
fn test_generate_install_script_has_unzip_location() {
    let script = generate_install_script(
        "tool",
        "https://example.com/tool.zip",
        "abc",
        false,
        FileType::Zip,
    )
    .unwrap();
    assert!(script.contains("unzipLocation"));
    assert!(script.contains("Split-Path"));
}

#[test]
fn test_generate_install_script_structure() {
    let script = generate_install_script(
        "my-app",
        "https://example.com/my-app.zip",
        "hash123",
        false,
        FileType::Zip,
    )
    .unwrap();
    let lines: Vec<&str> = script.lines().collect();
    assert_eq!(lines[0], "$ErrorActionPreference = 'Stop'");
    assert_eq!(lines[1], "");
    assert_eq!(lines[2], "$packageArgs = @{");
    assert!(
        script
            .trim_end()
            .ends_with("Install-ChocolateyZipPackage @packageArgs")
    );
}

#[test]
fn test_generate_install_script_dual_arch() {
    let script = generate_install_script_dual(&InstallScriptDual {
        name: "mytool",
        url32: "https://example.com/mytool-1.0.0-windows-386.zip",
        hash32: "hash32abc",
        url64: "https://example.com/mytool-1.0.0-windows-amd64.zip",
        hash64: "hash64def",
        file_type: FileType::Zip,
    })
    .unwrap();
    assert!(script.contains("$ErrorActionPreference = 'Stop'"));
    assert!(script.contains("$packageName = 'mytool'"));
    assert!(script.contains("$url = 'https://example.com/mytool-1.0.0-windows-386.zip'"));
    assert!(script.contains("$url64bit = 'https://example.com/mytool-1.0.0-windows-amd64.zip'"));
    assert!(script.contains("$checksum = 'hash32abc'"));
    assert!(script.contains("$checksum64 = 'hash64def'"));
    assert!(script.contains("Install-ChocolateyZipPackage $packageName $url $toolsDir $url64bit"));
    assert!(script.contains("-Checksum $checksum -ChecksumType 'sha256'"));
    assert!(script.contains("-Checksum64 $checksum64 -ChecksumType64 'sha256'"));
}

// ----- fileType routing: msi -----

/// A 64-bit MSI artifact must be RUN (`Install-ChocolateyPackage` with
/// `-FileType 'msi'` + `/qn /norestart` + MSI exit codes), not unpacked.
#[test]
fn test_generate_install_script_msi_64bit() {
    let script = generate_install_script(
        "mytool",
        "https://example.com/mytool-1.0.0-x64.msi",
        "msihash",
        false,
        FileType::Msi,
    )
    .unwrap();
    assert!(script.contains("Install-ChocolateyPackage @packageArgs"));
    assert!(!script.contains("Install-ChocolateyZipPackage"));
    assert!(script.contains("fileType       = 'msi'"));
    assert!(script.contains("url64bit       = 'https://example.com/mytool-1.0.0-x64.msi'"));
    assert!(script.contains("checksum64     = 'msihash'"));
    assert!(script.contains("checksumType64 = 'sha256'"));
    assert!(script.contains("silentArgs     = '/qn /norestart'"));
    assert!(script.contains("validExitCodes = @(0, 1641, 3010)"));
    assert!(!script.contains("unzipLocation"));
    // The `{% if valid_exit_codes %}` line must sit INSIDE the @{ } block:
    // the validExitCodes line is followed by the closing brace, then the
    // cmdlet. Guards a brace-placement regression in the conditional.
    assert!(
        script.contains(
            "validExitCodes = @(0, 1641, 3010)\n}\n\nInstall-ChocolateyPackage @packageArgs"
        ),
        "validExitCodes must close the @packageArgs block before the cmdlet; got: {script}"
    );
    assert!(
        script
            .trim_end()
            .ends_with("}\n\nInstall-ChocolateyPackage @packageArgs"),
        "script must end with the closing brace then the cmdlet; got: {script}"
    );
}

/// A dual-arch MSI routes both URLs through `Install-ChocolateyPackage`.
#[test]
fn test_generate_install_script_msi_dual_arch() {
    let script = generate_install_script_dual(&InstallScriptDual {
        name: "mytool",
        url32: "https://example.com/mytool-x86.msi",
        hash32: "h32",
        url64: "https://example.com/mytool-x64.msi",
        hash64: "h64",
        file_type: FileType::Msi,
    })
    .unwrap();
    assert!(script.contains("Install-ChocolateyPackage @packageArgs"));
    assert!(script.contains("fileType       = 'msi'"));
    assert!(script.contains("url            = 'https://example.com/mytool-x86.msi'"));
    assert!(script.contains("url64bit       = 'https://example.com/mytool-x64.msi'"));
    assert!(script.contains("checksum       = 'h32'"));
    assert!(script.contains("checksum64     = 'h64'"));
    assert!(script.contains("silentArgs     = '/qn /norestart'"));
    assert!(script.contains("validExitCodes = @(0, 1641, 3010)"));
}

// ----- fileType routing: nsis exe -----

/// An NSIS-generated `.exe` must be RUN via `Install-ChocolateyPackage` with
/// `-FileType 'exe'` and the NSIS silent switch `/S`, and NO validExitCodes
/// (those are MSI-specific).
#[test]
fn test_generate_install_script_nsis_64bit() {
    let script = generate_install_script(
        "mytool",
        "https://example.com/mytool-setup.exe",
        "exehash",
        false,
        FileType::NsisExe,
    )
    .unwrap();
    assert!(script.contains("Install-ChocolateyPackage @packageArgs"));
    assert!(!script.contains("Install-ChocolateyZipPackage"));
    assert!(script.contains("fileType       = 'exe'"));
    assert!(script.contains("url64bit       = 'https://example.com/mytool-setup.exe'"));
    assert!(script.contains("checksum64     = 'exehash'"));
    assert!(script.contains("silentArgs     = '/S'"));
    assert!(
        !script.contains("validExitCodes"),
        "validExitCodes is MSI-only; NSIS exe must omit it. got: {script}"
    );
    // With the `{% if valid_exit_codes %}` branch dropped, the silentArgs line
    // must be immediately followed by the closing brace, then the cmdlet — no
    // dangling/missing brace from the omitted conditional.
    assert!(
        script.contains("silentArgs     = '/S'\n}\n\nInstall-ChocolateyPackage @packageArgs"),
        "dropped validExitCodes must leave the @packageArgs brace intact; got: {script}"
    );
    assert!(
        script
            .trim_end()
            .ends_with("}\n\nInstall-ChocolateyPackage @packageArgs"),
        "script must end with the closing brace then the cmdlet; got: {script}"
    );
}

#[test]
fn test_generate_install_script_nsis_32bit() {
    let script = generate_install_script(
        "mytool",
        "https://example.com/mytool-setup-x86.exe",
        "exehash32",
        true,
        FileType::NsisExe,
    )
    .unwrap();
    assert!(script.contains("Install-ChocolateyPackage @packageArgs"));
    assert!(script.contains("fileType     = 'exe'"));
    assert!(script.contains("url          = 'https://example.com/mytool-setup-x86.exe'"));
    assert!(script.contains("checksum     = 'exehash32'"));
    assert!(script.contains("silentArgs   = '/S'"));
    assert!(!script.contains("url64bit"));
    assert!(!script.contains("validExitCodes"));
}

#[test]
fn test_filetype_from_use_mapping() {
    assert_eq!(FileType::from_use(None), FileType::Zip);
    assert_eq!(FileType::from_use(Some("archive")), FileType::Zip);
    assert_eq!(FileType::from_use(Some("zip")), FileType::Zip);
    assert_eq!(FileType::from_use(Some("msi")), FileType::Msi);
    assert_eq!(FileType::from_use(Some("nsis")), FileType::NsisExe);
}

/// Chocolatey only ships amd64/386. When the only Windows
/// artifact is arm64, anodizer must NOT silently package it as a 64-bit
/// archive (the prior `is_32bit_target` heuristic let arm64 fall through
/// to the amd64 slot, producing a broken install script).
#[test]
fn test_publish_to_chocolatey_rejects_arm64_only() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{ChocolateyConfig, Config, CrateConfig, PublishConfig};
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};
    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        publish: Some(PublishConfig {
            chocolatey: Some(ChocolateyConfig {
                repository: Some(anodizer_core::config::RepositoryConfig {
                    owner: Some("myorg".to_string()),
                    name: Some("mytool".to_string()),
                    ..Default::default()
                }),
                description: Some("A great tool".to_string()),
                license: Some("MIT".to_string()),
                api_key: Some("dummy".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }];
    let mut ctx = Context::new(config, ContextOptions::default());
    // Only an arm64 Windows artifact — chocolatey must reject (matches
    // a no-windows-archive error since the goarch filter excludes arm64).
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        path: std::path::PathBuf::from("/tmp/mytool-windows-arm64.zip"),
        name: "mytool-windows-arm64.zip".to_string(),
        target: Some("aarch64-pc-windows-msvc".to_string()),
        crate_name: "mytool".to_string(),
        metadata: {
            let mut m = std::collections::HashMap::new();
            m.insert("sha256".to_string(), "deadbeef".to_string());
            m.insert("url".to_string(), "https://example.com/x.zip".to_string());
            m
        },
        size: None,
    });
    let log = StageLogger::new("publish", Verbosity::Normal);
    let res = publish_to_chocolatey(&mut ctx, "mytool", &log);
    assert!(
        res.is_err(),
        "arm64-only must fail with errNoWindowsArchive equivalent"
    );
    let msg = format!("{:#}", res.unwrap_err());
    assert!(
        msg.contains("no windows artifact"),
        "error must call out missing-windows-artifact, got: {msg}"
    );
}

#[test]
fn test_publish_to_chocolatey_dry_run() {
    use anodizer_core::config::{ChocolateyConfig, Config, CrateConfig, PublishConfig};
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};
    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        publish: Some(PublishConfig {
            chocolatey: Some(ChocolateyConfig {
                repository: Some(anodizer_core::config::RepositoryConfig {
                    owner: Some("myorg".to_string()),
                    name: Some("mytool".to_string()),
                    ..Default::default()
                }),
                description: Some("A great tool".to_string()),
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
    assert!(publish_to_chocolatey(&mut ctx, "mytool", &log).is_ok());
}

#[test]
fn test_publish_to_chocolatey_missing_config() {
    use anodizer_core::config::{Config, CrateConfig, PublishConfig};
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};
    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
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
    assert!(publish_to_chocolatey(&mut ctx, "mytool", &log).is_err());
}

#[test]
fn test_publish_to_chocolatey_missing_repository_is_now_optional() {
    // Chocolatey has no Repository field — choco is a
    // feed-push publisher, only api_key + source_repo are required.
    // anodizer's `repository.owner/name` is only a fallback source for
    // <projectUrl>; it must not block configs that omit it.
    use anodizer_core::config::{ChocolateyConfig, Config, CrateConfig, PublishConfig};
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};
    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        publish: Some(PublishConfig {
            chocolatey: Some(ChocolateyConfig {
                repository: None,
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
    publish_to_chocolatey(&mut ctx, "mytool", &log)
        .expect("dry-run must succeed without repository");
}

#[test]
fn test_generate_nuspec_all_optional_fields() {
    let deps = vec![
        anodizer_core::config::ChocolateyDependency {
            id: "dotnetfx".to_string(),
            version: Some("[4.5.1,)".to_string()),
        },
        anodizer_core::config::ChocolateyDependency {
            id: "vcredist140".to_string(),
            version: None,
        },
    ];
    let tags = vec!["cli".to_string(), "devops".to_string()];
    let nuspec = generate_nuspec(&NuspecParams {
        name: "my-tool",
        version: "2.5.0",
        description: "A tool with all fields",
        license: "MIT",
        license_url: Some("https://example.com/license"),
        authors: "Jane Doe",
        project_url: "https://example.com/my-tool",
        icon_url: "https://example.com/icon.png",
        tags: &tags,
        package_source_url: Some("https://github.com/org/choco-packages"),
        owners: Some("jdoe"),
        title: Some("My Tool Pro"),
        copyright: Some("Copyright 2026 Jane Doe"),
        require_license_acceptance: true,
        project_source_url: Some("https://github.com/org/my-tool"),
        docs_url: Some("https://docs.example.com"),
        bug_tracker_url: Some("https://github.com/org/my-tool/issues"),
        summary: Some("CLI devops tool"),
        release_notes: Some("Added new features"),
        dependencies: &deps,
    })
    .unwrap();
    assert!(
        nuspec
            .contains("<packageSourceUrl>https://github.com/org/choco-packages</packageSourceUrl>")
    );
    assert!(nuspec.contains("<owners>jdoe</owners>"));
    assert!(nuspec.contains("<title>My Tool Pro</title>"));
    assert!(nuspec.contains("<copyright>Copyright 2026 Jane Doe</copyright>"));
    assert!(nuspec.contains("<requireLicenseAcceptance>true</requireLicenseAcceptance>"));
    assert!(nuspec.contains("<projectSourceUrl>https://github.com/org/my-tool</projectSourceUrl>"));
    assert!(nuspec.contains("<docsUrl>https://docs.example.com</docsUrl>"));
    assert!(
        nuspec.contains("<bugTrackerUrl>https://github.com/org/my-tool/issues</bugTrackerUrl>")
    );
    assert!(nuspec.contains("<summary>CLI devops tool</summary>"));
    assert!(nuspec.contains("<releaseNotes>Added new features</releaseNotes>"));
    assert!(nuspec.contains("<licenseUrl>https://example.com/license</licenseUrl>"));
    assert!(nuspec.contains("<dependencies>"));
    assert!(nuspec.contains("<dependency id=\"dotnetfx\" version=\"[4.5.1,)\" />"));
    assert!(nuspec.contains("<dependency id=\"vcredist140\" />"));
}

#[test]
fn test_chocolatey_skip_bool_config() {
    let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      chocolatey:
        skip: true
        repository:
          owner: org
          name: test
"#;
    let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
    let choco = config.crates[0]
        .publish
        .as_ref()
        .unwrap()
        .chocolatey
        .as_ref()
        .unwrap();
    assert!(choco.skip.as_ref().unwrap().as_bool());
}

#[test]
fn test_chocolatey_skip_false_config() {
    let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      chocolatey:
        skip: false
        repository:
          owner: org
          name: test
"#;
    let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
    let choco = config.crates[0]
        .publish
        .as_ref()
        .unwrap()
        .chocolatey
        .as_ref()
        .unwrap();
    assert!(!choco.skip.as_ref().unwrap().as_bool());
}

#[test]
fn test_chocolatey_skip_publish_legacy_alias_still_accepted() {
    // `skip_publish:` was renamed to `skip:` for project-wide
    // canonicalization, but old configs in the wild still spell it
    // `skip_publish:`. The serde alias keeps them parsing.
    let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      chocolatey:
        skip_publish: true
        repository:
          owner: org
          name: test
"#;
    let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
    let choco = config.crates[0]
        .publish
        .as_ref()
        .unwrap()
        .chocolatey
        .as_ref()
        .unwrap();
    assert!(choco.skip.as_ref().unwrap().as_bool());
}

#[test]
fn test_chocolatey_skip_template_string() {
    let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      chocolatey:
        skip: "{{ .IsSnapshot }}"
        repository:
          owner: org
          name: test
"#;
    let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
    let choco = config.crates[0]
        .publish
        .as_ref()
        .unwrap()
        .chocolatey
        .as_ref()
        .unwrap();
    assert!(choco.skip.as_ref().unwrap().is_template());
}

#[test]
fn test_create_nupkg_produces_valid_opc_zip() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg_dir = tmp.path();

    // Write nuspec
    let nuspec = generate_nuspec(&default_nuspec_params()).unwrap();
    let nuspec_path = pkg_dir.join("mytool.nuspec");
    std::fs::write(&nuspec_path, &nuspec).unwrap();

    // Write install script
    let tools_dir = pkg_dir.join("tools");
    std::fs::create_dir_all(&tools_dir).unwrap();
    let script = generate_install_script(
        "mytool",
        "https://example.com/dl.zip",
        "abc123",
        false,
        FileType::Zip,
    )
    .unwrap();
    std::fs::write(tools_dir.join("chocolateyinstall.ps1"), &script).unwrap();

    // Create nupkg
    let nupkg_path = pkg_dir.join("mytool.1.0.0.nupkg");
    create_nupkg("mytool", &nuspec_path, &tools_dir, &nupkg_path).unwrap();

    // Verify it's a valid ZIP with the expected OPC structure
    let file = std::fs::File::open(&nupkg_path).unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();

    let mut names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().to_string())
        .collect();
    names.sort();

    assert!(names.contains(&"[Content_Types].xml".to_string()));
    assert!(names.contains(&"_rels/.rels".to_string()));
    assert!(names.contains(&"mytool.nuspec".to_string()));
    assert!(names.contains(&"tools/chocolateyinstall.ps1".to_string()));

    // Verify file contents by reading each entry in its own scope
    let read_entry = |archive: &mut zip::ZipArchive<std::fs::File>, name: &str| -> String {
        let mut entry = archive.by_name(name).unwrap();
        let mut content = String::new();
        std::io::Read::read_to_string(&mut entry, &mut content).unwrap();
        content
    };

    let ct = read_entry(&mut archive, "[Content_Types].xml");
    assert!(ct.contains("application/vnd.openxmlformats-package.relationships+xml"));
    assert!(ct.contains("nuspec"));

    let rels = read_entry(&mut archive, "_rels/.rels");
    assert!(rels.contains("/mytool.nuspec"));

    let ns = read_entry(&mut archive, "mytool.nuspec");
    assert!(ns.contains("<id>mytool</id>"));
    assert!(ns.contains("<version>1.0.0</version>"));
}

// -----------------------------------------------------------------
// OData feed parsing (FeedHashResult variant matrix)
// -----------------------------------------------------------------

/// OData skeleton mirroring community.chocolatey.org's
/// `Packages(Id='X',Version='Y')` response. Only the fields we parse
/// are populated; everything else is omitted to keep the fixture
/// readable.
///
/// Note: the real feed does NOT emit `<d:Listed>` — moderation state is
/// signalled via `<d:PackageStatus>` and `<d:IsApproved>` only.
fn odata_response(
    version: &str,
    hash: Option<&str>,
    algorithm: Option<&str>,
    status: Option<&str>,
    is_approved: Option<bool>,
    published: Option<&str>,
) -> String {
    let mut props = String::new();
    if let Some(h) = hash {
        props.push_str(&format!("<d:PackageHash>{}</d:PackageHash>", h));
    }
    if let Some(a) = algorithm {
        props.push_str(&format!(
            "<d:PackageHashAlgorithm>{}</d:PackageHashAlgorithm>",
            a
        ));
    }
    if let Some(s) = status {
        props.push_str(&format!("<d:PackageStatus>{}</d:PackageStatus>", s));
    }
    if let Some(a) = is_approved {
        props.push_str(&format!("<d:IsApproved>{}</d:IsApproved>", a));
    }
    if let Some(p) = published {
        props.push_str(&format!("<d:Published>{}</d:Published>", p));
    }
    format!(
        r#"<?xml version="1.0" encoding="utf-8" standalone="yes"?>
<entry>
  <id>https://community.chocolatey.org/api/v2/Packages(Id='cfgd',Version='{}')</id>
  <m:properties>{}</m:properties>
</entry>"#,
        version, props
    )
}

#[test]
fn test_parse_xml_element_handles_namespaced_tags() {
    let body = r#"<m:properties><d:PackageHash>abc</d:PackageHash><d:PackageHashAlgorithm>SHA512</d:PackageHashAlgorithm></m:properties>"#;
    assert_eq!(
        parse_xml_element(body, "PackageHash").as_deref(),
        Some("abc")
    );
    assert_eq!(
        parse_xml_element(body, "PackageHashAlgorithm").as_deref(),
        Some("SHA512")
    );
}

#[test]
fn test_parse_xml_element_handles_status_and_published() {
    let body = odata_response(
        "0.3.5",
        Some("XYZ"),
        Some("SHA512"),
        Some("Submitted"),
        Some(false),
        Some("1900-01-01T00:00:00"),
    );
    assert_eq!(
        parse_xml_element(&body, "PackageStatus").as_deref(),
        Some("Submitted")
    );
    assert_eq!(
        parse_xml_element(&body, "IsApproved").as_deref(),
        Some("false")
    );
    assert_eq!(
        parse_xml_element(&body, "Published").as_deref(),
        Some("1900-01-01T00:00:00")
    );
}

#[test]
fn test_parse_xml_element_returns_none_when_absent() {
    let body = r#"<m:properties><d:PackageHash>abc</d:PackageHash></m:properties>"#;
    assert!(parse_xml_element(body, "PackageStatus").is_none());
    assert!(parse_xml_element(body, "IsApproved").is_none());
}

// ---------------------------------------------------------------------------
// publish_to_chocolatey: additional branches.
// ---------------------------------------------------------------------------

use super::package::{FeedHashResult, classify_moderation, compute_nupkg_hash, package_feed_hash};
use anodizer_core::retry::RetryPolicy;
use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

fn fast_retry() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 1,
        base_delay: std::time::Duration::from_millis(0),
        max_delay: std::time::Duration::from_millis(0),
    }
}

/// `skip: true` causes an immediate Ok(false) skip and no push attempt.
#[test]
fn publish_to_chocolatey_skip_true_returns_false() {
    use anodizer_core::config::{
        ChocolateyConfig, Config, CrateConfig, PublishConfig, StringOrBool,
    };
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};
    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        publish: Some(PublishConfig {
            chocolatey: Some(ChocolateyConfig {
                skip: Some(StringOrBool::Bool(true)),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }];
    let mut ctx = Context::new(config, ContextOptions::default());
    let log = StageLogger::new("publish", Verbosity::Quiet);
    let res = super::publish::publish_to_chocolatey(&mut ctx, "mytool", &log).unwrap();
    assert!(!res, "skip=true must short-circuit to Ok(false)");
}

/// `classify_moderation` returns "approved" / not-in-moderation for
/// `Approved` status; conservative not-in-moderation when neither field
/// is exposed.
#[test]
fn classify_moderation_approved_and_unknown() {
    let (label, in_mod) = classify_moderation(Some("Approved"), Some(true));
    assert!(!in_mod);
    assert!(label.contains("approved"));

    let (label, in_mod) = classify_moderation(None, None);
    assert!(!in_mod);
    assert!(label.contains("on feed"));
}

/// `classify_moderation` flags Submitted / Rejected / Unknown as in-moderation
/// (blockers for unattended publish). Exempted is approved and live — not
/// in-moderation.
#[test]
fn classify_moderation_in_queue_variants() {
    for s in ["Submitted", "Rejected", "Unknown"] {
        let (label, in_mod) = classify_moderation(Some(s), None);
        assert!(in_mod, "{s} must be flagged in-moderation; label={label}");
    }
    let (label, in_mod) = classify_moderation(Some("Exempted"), None);
    assert!(
        !in_mod,
        "Exempted is approved (not in-moderation); label={label}"
    );
    assert!(
        label.contains("exempted"),
        "label should mention exempted; got={label}"
    );
}

/// `classify_moderation` matches Rejected case-insensitively (the OData
/// feed has been observed emitting `rejected` in legacy fixtures).
#[test]
fn classify_moderation_status_is_case_insensitive() {
    let (label, in_mod) = classify_moderation(Some("rejected"), None);
    assert!(in_mod);
    assert!(label.contains("rejected"));
}

/// `classify_moderation` falls back to IsApproved when PackageStatus is
/// missing — `IsApproved=false` => in moderation, `true` => approved.
#[test]
fn classify_moderation_falls_back_to_is_approved() {
    let (_, in_mod) = classify_moderation(None, Some(false));
    assert!(in_mod, "is_approved=false => in moderation");
    let (_, in_mod) = classify_moderation(None, Some(true));
    assert!(!in_mod, "is_approved=true => not in moderation");
}

/// `compute_nupkg_hash` produces a base64 digest whose length is the
/// canonical SHA512 / SHA256 / MD5 length. Pins the algorithm dispatch.
#[test]
fn compute_nupkg_hash_dispatches_algorithm_correctly() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("pkg.nupkg");
    std::fs::write(&p, b"abc").unwrap();
    let sha512 = compute_nupkg_hash(&p, "SHA512").unwrap();
    let sha256 = compute_nupkg_hash(&p, "SHA256").unwrap();
    let md5 = compute_nupkg_hash(&p, "MD5").unwrap();
    // base64(SHA512=64 bytes) = ceil(64/3)*4 = 88 chars.
    assert_eq!(sha512.len(), 88, "sha512 base64 len");
    // base64(SHA256=32 bytes) = 44 chars (with `=` padding).
    assert_eq!(sha256.len(), 44, "sha256 base64 len");
    // base64(MD5=16 bytes) = 24 chars.
    assert_eq!(md5.len(), 24, "md5 base64 len");
}

/// Unsupported algorithm => actionable error naming the bad input.
#[test]
fn compute_nupkg_hash_unsupported_algorithm_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("pkg.nupkg");
    std::fs::write(&p, b"abc").unwrap();
    let err = compute_nupkg_hash(&p, "BLAKE2").unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("BLAKE2"));
    assert!(msg.contains("unsupported"));
}

/// `compute_nupkg_hash` is case-insensitive on the algorithm name (the
/// OData feed sometimes emits "sha512" rather than "SHA512").
#[test]
fn compute_nupkg_hash_algorithm_case_insensitive() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("pkg.nupkg");
    std::fs::write(&p, b"abc").unwrap();
    let upper = compute_nupkg_hash(&p, "SHA256").unwrap();
    let lower = compute_nupkg_hash(&p, "sha256").unwrap();
    let mixed = compute_nupkg_hash(&p, "Sha256").unwrap();
    assert_eq!(upper, lower);
    assert_eq!(upper, mixed);
}

/// `package_feed_hash` handles a 404 / connection failure path by
/// returning `Absent`. Pins the "couldn't reach feed => proceed to push"
/// degrade contract; transport failures must not block legitimate
/// re-pushes of new versions.
#[test]
fn package_feed_hash_absent_when_responder_returns_404() {
    let (addr, _calls) =
        spawn_oneshot_http_responder(vec!["HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"]);
    let url = format!("http://{addr}");
    let got = package_feed_hash(
        &url,
        "mytool",
        "1.0.0",
        &fast_retry(),
        anodizer_core::test_helpers::test_logger(),
    );
    assert_eq!(got, FeedHashResult::Absent);
}

/// `package_feed_hash` returns Absent when the feed body indicates the
/// version is not present (no `<entry>` / version marker).
#[test]
fn package_feed_hash_absent_when_body_lacks_version_marker() {
    let body = "<?xml version=\"1.0\"?><feed></feed>";
    let len = body.len();
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {len}\r\n\r\n{body}"
    );
    let resp_static: &'static str = Box::leak(resp.into_boxed_str());
    let (addr, _calls) = spawn_oneshot_http_responder(vec![resp_static]);
    let url = format!("http://{addr}");
    let got = package_feed_hash(
        &url,
        "mytool",
        "1.0.0",
        &fast_retry(),
        anodizer_core::test_helpers::test_logger(),
    );
    assert_eq!(got, FeedHashResult::Absent);
}

/// `package_feed_hash` returns `Present { hash, algorithm, ... }` when
/// the OData entry includes both `<d:PackageHash>` and
/// `<d:PackageHashAlgorithm>` populated.
#[test]
fn package_feed_hash_present_with_hash_and_algorithm() {
    let body = "<entry><id>https://example.com/api/v2/Packages(Id='mytool',Version='1.0.0')</id>\
        <m:properties>\
        <d:PackageHash>SOMEHASH</d:PackageHash>\
        <d:PackageHashAlgorithm>SHA512</d:PackageHashAlgorithm>\
        <d:PackageStatus>Approved</d:PackageStatus>\
        <d:IsApproved>true</d:IsApproved>\
        </m:properties></entry>";
    let len = body.len();
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {len}\r\n\r\n{body}"
    );
    let resp_static: &'static str = Box::leak(resp.into_boxed_str());
    let (addr, _calls) = spawn_oneshot_http_responder(vec![resp_static]);
    let url = format!("http://{addr}");
    let got = package_feed_hash(
        &url,
        "mytool",
        "1.0.0",
        &fast_retry(),
        anodizer_core::test_helpers::test_logger(),
    );
    match got {
        FeedHashResult::Present {
            hash,
            algorithm,
            status,
            is_approved,
            ..
        } => {
            assert_eq!(hash, "SOMEHASH");
            assert_eq!(algorithm, "SHA512");
            assert_eq!(status.as_deref(), Some("Approved"));
            assert_eq!(is_approved, Some(true));
        }
        other => panic!("expected Present, got {other:?}"),
    }
}

/// `package_feed_hash` returns `PresentNoHash` when the OData entry is
/// present but lacks both PackageHash / PackageHashAlgorithm — drift
/// detection cannot run, so the caller logs and falls through to push.
#[test]
fn package_feed_hash_present_no_hash() {
    let body = "<entry><id>https://example.com/api/v2/Packages(Id='mytool',Version='1.0.0')</id>\
        <m:properties>\
        <d:PackageStatus>Approved</d:PackageStatus>\
        </m:properties></entry>";
    let len = body.len();
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {len}\r\n\r\n{body}"
    );
    let resp_static: &'static str = Box::leak(resp.into_boxed_str());
    let (addr, _calls) = spawn_oneshot_http_responder(vec![resp_static]);
    let url = format!("http://{addr}");
    let got = package_feed_hash(
        &url,
        "mytool",
        "1.0.0",
        &fast_retry(),
        anodizer_core::test_helpers::test_logger(),
    );
    assert_eq!(got, FeedHashResult::PresentNoHash);
}

/// `parse_xml_element` returns the inner text of a single-element body
/// and strips whitespace around the value (matches `body.trim()`).
#[test]
fn parse_xml_element_trims_whitespace() {
    let body = "<d:PackageHash>  spaced-hash  </d:PackageHash>";
    assert_eq!(
        parse_xml_element(body, "PackageHash").as_deref(),
        Some("spaced-hash")
    );
}

/// Building a chocolatey nuspec with neither `publish.chocolatey.license`
/// nor top-level `metadata.license` must bail with an actionable error: the
/// nuspec needs an SPDX expression for its `<license type="expression">`
/// element, which Chocolatey gallery moderators expect. The bail message must
/// name the publisher, the field, and the offending crate.
#[test]
fn chocolatey_license_empty_metadata_bails_with_actionable_error() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{ChocolateyConfig, Config, CrateConfig, PublishConfig};
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};
    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        publish: Some(PublishConfig {
            chocolatey: Some(ChocolateyConfig {
                repository: Some(anodizer_core::config::RepositoryConfig {
                    owner: Some("myorg".to_string()),
                    name: Some("mytool".to_string()),
                    ..Default::default()
                }),
                description: Some("A great tool".to_string()),
                api_key: Some("dummy".to_string()),
                license: None,
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }];
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        path: std::path::PathBuf::from("/tmp/mytool-windows-amd64.zip"),
        name: "mytool-windows-amd64.zip".to_string(),
        target: Some("x86_64-pc-windows-msvc".to_string()),
        crate_name: "mytool".to_string(),
        metadata: {
            let mut m = std::collections::HashMap::new();
            m.insert("sha256".to_string(), "deadbeef".to_string());
            m.insert("url".to_string(), "https://example.com/x.zip".to_string());
            m
        },
        size: None,
    });
    let log = StageLogger::new("publish", Verbosity::Quiet);
    let err = super::publish::publish_to_chocolatey(&mut ctx, "mytool", &log)
        .expect_err("missing license must bail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("chocolatey:") && msg.contains("license"),
        "error must name publisher + field; got: {msg}"
    );
    assert!(
        msg.contains("mytool"),
        "error must name the offending crate; got: {msg}"
    );
    assert!(
        msg.contains("metadata.license") || msg.contains("publish.chocolatey.license"),
        "error must include an actionable next-step hint pointing at config keys; got: {msg}"
    );
}

/// Building a chocolatey install script for a Windows artifact whose
/// `sha256` metadata is empty must bail with an actionable error.
/// Defaulting to `""` would embed an empty `$checksum` in the generated
/// `chocolateyinstall.ps1`, which Chocolatey moderators reject (the
/// install script can't verify the download). The bail message must
/// name the publisher, the field, and the offending artifact.
#[test]
fn chocolatey_sha256_empty_metadata_bails_with_actionable_error() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{ChocolateyConfig, Config, CrateConfig, PublishConfig};
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};
    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        publish: Some(PublishConfig {
            chocolatey: Some(ChocolateyConfig {
                repository: Some(anodizer_core::config::RepositoryConfig {
                    owner: Some("myorg".to_string()),
                    name: Some("mytool".to_string()),
                    ..Default::default()
                }),
                description: Some("A great tool".to_string()),
                license: Some("MIT".to_string()),
                api_key: Some("dummy".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }];
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        path: std::path::PathBuf::from("/tmp/mytool-windows-amd64.zip"),
        name: "mytool-windows-amd64.zip".to_string(),
        target: Some("x86_64-pc-windows-msvc".to_string()),
        crate_name: "mytool".to_string(),
        metadata: {
            let mut m = std::collections::HashMap::new();
            m.insert("url".to_string(), "https://example.com/x.zip".to_string());
            // sha256 deliberately missing — the silent-default trap that
            // would emit an empty `$checksum` in chocolateyinstall.ps1.
            m
        },
        size: None,
    });
    let log = StageLogger::new("publish", Verbosity::Quiet);
    let err = super::publish::publish_to_chocolatey(&mut ctx, "mytool", &log)
        .expect_err("missing sha256 must bail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("chocolatey:") && msg.contains("sha256"),
        "error must name publisher + field; got: {msg}"
    );
    assert!(
        msg.contains("mytool-windows-amd64.zip"),
        "error must name the offending artifact; got: {msg}"
    );
    assert!(
        msg.contains("dist/artifacts.json") || msg.contains("re-run"),
        "error must include a next-step hint; got: {msg}"
    );
}
