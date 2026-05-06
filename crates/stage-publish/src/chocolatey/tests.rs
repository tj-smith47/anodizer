//! Tests for the Chocolatey publisher submodules.

#![allow(clippy::field_reassign_with_default)]

use super::install::{InstallScriptDual, generate_install_script, generate_install_script_dual};
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

#[test]
fn test_generate_nuspec_has_license_url_default() {
    let nuspec = generate_nuspec(&NuspecParams {
        name: "tool",
        version: "2.0.0",
        license: "Apache-2.0",
        ..default_nuspec_params()
    })
    .unwrap();
    assert!(nuspec.contains("<licenseUrl>https://opensource.org/licenses/Apache-2.0</licenseUrl>"));
}

#[test]
fn test_generate_nuspec_custom_license_url() {
    let nuspec = generate_nuspec(&NuspecParams {
        name: "tool",
        version: "2.0.0",
        license: "Proprietary",
        license_url: Some("https://example.com/license"),
        ..default_nuspec_params()
    })
    .unwrap();
    assert!(nuspec.contains("<licenseUrl>https://example.com/license</licenseUrl>"));
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
    let script =
        generate_install_script("tool", "https://example.com/tool.zip", "abc", false).unwrap();
    assert!(script.contains("unzipLocation"));
    assert!(script.contains("Split-Path"));
}

#[test]
fn test_generate_install_script_structure() {
    let script =
        generate_install_script("my-app", "https://example.com/my-app.zip", "hash123", false)
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
    let ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    let log = StageLogger::new("publish", Verbosity::Normal);
    assert!(publish_to_chocolatey(&ctx, "mytool", &log).is_ok());
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
    let ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    let log = StageLogger::new("publish", Verbosity::Normal);
    assert!(publish_to_chocolatey(&ctx, "mytool", &log).is_err());
}

#[test]
fn test_publish_to_chocolatey_missing_repository() {
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
    let ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    let log = StageLogger::new("publish", Verbosity::Normal);
    assert!(publish_to_chocolatey(&ctx, "mytool", &log).is_err());
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
    // DEC-12 (post-WAVE 5+) renamed `skip_publish:` -> `skip:` for project-wide
    // canonicalization, but old configs in the wild still spell it `skip_publish:`.
    // The serde alias keeps them parsing.
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
    let script =
        generate_install_script("mytool", "https://example.com/dl.zip", "abc123", false).unwrap();
    std::fs::write(tools_dir.join("chocolateyinstall.ps1"), &script).unwrap();

    // Create nupkg
    let nupkg_path = pkg_dir.join("mytool.1.0.0.nupkg");
    create_nupkg("mytool", "1.0.0", &nuspec_path, &tools_dir, &nupkg_path).unwrap();

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
fn odata_response(
    version: &str,
    hash: Option<&str>,
    algorithm: Option<&str>,
    status: Option<&str>,
    listed: Option<bool>,
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
    if let Some(l) = listed {
        props.push_str(&format!("<d:Listed>{}</d:Listed>", l));
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
fn test_parse_xml_element_handles_listed_and_published() {
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
    assert_eq!(parse_xml_element(&body, "Listed").as_deref(), Some("false"));
    assert_eq!(
        parse_xml_element(&body, "Published").as_deref(),
        Some("1900-01-01T00:00:00")
    );
}

#[test]
fn test_parse_xml_element_returns_none_when_absent() {
    let body = r#"<m:properties><d:PackageHash>abc</d:PackageHash></m:properties>"#;
    assert!(parse_xml_element(body, "PackageStatus").is_none());
    assert!(parse_xml_element(body, "Listed").is_none());
}
