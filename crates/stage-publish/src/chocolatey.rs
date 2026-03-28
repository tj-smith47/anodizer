use anodize_core::context::Context;
use anodize_core::log::StageLogger;
use anyhow::{Context as _, Result};

use crate::util::{find_windows_artifact, run_cmd_in};

// ---------------------------------------------------------------------------
// NuspecParams
// ---------------------------------------------------------------------------

/// Parameters for generating a Chocolatey `.nuspec` XML manifest.
pub struct NuspecParams<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub description: &'a str,
    pub license: &'a str,
    pub license_url: Option<&'a str>,
    pub authors: &'a str,
    pub project_url: &'a str,
    pub icon_url: &'a str,
    pub tags: &'a [String],
}

// ---------------------------------------------------------------------------
// Tera templates for Chocolatey
// ---------------------------------------------------------------------------

/// Nuspec XML template.  Auto-escaping is disabled because URLs contain `/`
/// which Tera escapes to `&#x2F;`.  The `description` field uses the
/// `xml_escape` filter for safety.
const NUSPEC_TEMPLATE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://schemas.microsoft.com/packaging/2015/06/nuspec.xsd">
  <metadata>
    <id>{{ name }}</id>
    <version>{{ version }}</version>
    <title>{{ name }}</title>
    <authors>{{ authors }}</authors>
    <description>{{ description | escape }}</description>
    <projectUrl>{{ project_url }}</projectUrl>
{% if icon_url %}    <iconUrl>{{ icon_url }}</iconUrl>
{% endif %}    <licenseUrl>{{ license_url }}</licenseUrl>
    <tags>{{ tags_str }}</tags>
  </metadata>
  <files>
    <file src="tools\**" target="tools" />
  </files>
</package>
"#;

/// PowerShell install script template.
const INSTALL_SCRIPT_TEMPLATE: &str = r#"$ErrorActionPreference = 'Stop'

$packageArgs = @{
  packageName    = '{{ name }}'
  url64bit       = '{{ url }}'
  checksum64     = '{{ hash }}'
  checksumType64 = 'sha256'
  unzipLocation  = "$(Split-Path -Parent $MyInvocation.MyCommand.Definition)"
}

Install-ChocolateyZipPackage @packageArgs
"#;

// ---------------------------------------------------------------------------
// generate_nuspec
// ---------------------------------------------------------------------------

/// Generate a Chocolatey `.nuspec` XML manifest string.
///
/// Uses a Tera template with automatic XML escaping.
pub fn generate_nuspec(params: &NuspecParams<'_>) -> String {
    let tags_str = if params.tags.is_empty() {
        params.name.to_string()
    } else {
        params.tags.join(" ")
    };

    let license_url = match params.license_url {
        Some(url) if !url.is_empty() => url.to_string(),
        _ => format!("https://opensource.org/licenses/{}", params.license),
    };

    let mut tera = tera::Tera::default();
    tera.add_raw_template("nuspec", NUSPEC_TEMPLATE)
        .expect("chocolatey: parse nuspec template");

    // Disable auto-escaping — URLs contain `/` which Tera encodes as &#x2F;.
    // The description field uses the `| escape` filter explicitly in the template.
    tera.autoescape_on(vec![]);

    let mut ctx = tera::Context::new();
    ctx.insert("name", params.name);
    ctx.insert("version", params.version);
    ctx.insert("authors", params.authors);
    ctx.insert("description", params.description);
    ctx.insert("project_url", params.project_url);
    ctx.insert(
        "icon_url",
        &(!params.icon_url.is_empty()).then_some(params.icon_url),
    );
    ctx.insert("license_url", &license_url);
    ctx.insert("tags_str", &tags_str);

    tera.render("nuspec", &ctx)
        .expect("chocolatey: render nuspec template")
}

// ---------------------------------------------------------------------------
// generate_install_script
// ---------------------------------------------------------------------------

/// Generate a `chocolateyInstall.ps1` PowerShell script string.
///
/// NOTE: This currently only generates a 64-bit download URL (`url64bit`).
/// This is acceptable because Rust primarily targets x86_64 on Windows;
/// 32-bit Windows support is uncommon for modern Rust CLI tools.
pub fn generate_install_script(name: &str, url: &str, hash: &str) -> String {
    let mut tera = tera::Tera::default();
    tera.add_raw_template("install", INSTALL_SCRIPT_TEMPLATE)
        .expect("chocolatey: parse install script template");

    // Disable autoescaping for PowerShell script
    tera.autoescape_on(vec![]);

    let mut ctx = tera::Context::new();
    ctx.insert("name", name);
    ctx.insert("url", url);
    ctx.insert("hash", hash);

    tera.render("install", &ctx)
        .expect("chocolatey: render install script template")
}

// ---------------------------------------------------------------------------
// publish_to_chocolatey
// ---------------------------------------------------------------------------

pub fn publish_to_chocolatey(ctx: &Context, crate_name: &str, log: &StageLogger) -> Result<()> {
    let crate_cfg = ctx
        .config
        .crates
        .iter()
        .find(|c| c.name == crate_name)
        .ok_or_else(|| anyhow::anyhow!("chocolatey: crate '{}' not found in config", crate_name))?;

    let publish = crate_cfg
        .publish
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("chocolatey: no publish config for '{}'", crate_name))?;

    let choco_cfg = publish
        .chocolatey
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("chocolatey: no chocolatey config for '{}'", crate_name))?;

    let project_repo = choco_cfg.project_repo.as_ref().ok_or_else(|| {
        anyhow::anyhow!("chocolatey: no project_repo config for '{}'", crate_name)
    })?;

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would push Chocolatey package for '{}' to {}/{}",
            crate_name, project_repo.owner, project_repo.name
        ));
        return Ok(());
    }

    // Resolve version.
    let version = ctx
        .template_vars()
        .get("Version")
        .cloned()
        .unwrap_or_default();

    let description = choco_cfg
        .description
        .clone()
        .unwrap_or_else(|| crate_name.to_string());
    let license = choco_cfg
        .license
        .clone()
        .unwrap_or_else(|| "MIT".to_string());
    let authors = choco_cfg
        .authors
        .clone()
        .unwrap_or_else(|| crate_name.to_string());
    let project_url = choco_cfg.project_url.clone().unwrap_or_else(|| {
        format!(
            "https://github.com/{}/{}",
            project_repo.owner, project_repo.name
        )
    });
    let icon_url = choco_cfg.icon_url.clone().unwrap_or_default();
    let tags = choco_cfg.tags.clone().unwrap_or_default();

    // Find the windows Archive artifact.
    let (url, hash) = if let Some(found) = find_windows_artifact(ctx, crate_name) {
        found
    } else {
        log.warn(&format!(
            "chocolatey: no windows artifact found for '{}', using placeholder URL",
            crate_name
        ));
        (
            format!(
                "https://github.com/{0}/{1}/releases/download/v{2}/{1}-{2}-windows-amd64.zip",
                project_repo.owner, crate_name, version
            ),
            String::new(),
        )
    };

    let nuspec = generate_nuspec(&NuspecParams {
        name: crate_name,
        version: &version,
        description: &description,
        license: &license,
        license_url: choco_cfg.license_url.as_deref(),
        authors: &authors,
        project_url: &project_url,
        icon_url: &icon_url,
        tags: &tags,
    });
    let install_script = generate_install_script(crate_name, &url, &hash);

    // Create temp directory, write files, run choco pack + push.
    let tmp_dir = tempfile::tempdir().context("chocolatey: create temp dir")?;
    let pkg_dir = tmp_dir.path();

    let nuspec_path = pkg_dir.join(format!("{}.nuspec", crate_name));
    std::fs::write(&nuspec_path, &nuspec)
        .with_context(|| format!("chocolatey: write nuspec {}", nuspec_path.display()))?;

    let tools_dir = pkg_dir.join("tools");
    std::fs::create_dir_all(&tools_dir).context("chocolatey: create tools dir")?;

    let install_path = tools_dir.join("chocolateyInstall.ps1");
    std::fs::write(&install_path, &install_script).with_context(|| {
        format!(
            "chocolatey: write install script {}",
            install_path.display()
        )
    })?;

    log.status(&format!(
        "wrote Chocolatey nuspec: {}",
        nuspec_path.display()
    ));
    log.status(&format!(
        "wrote Chocolatey install script: {}",
        install_path.display()
    ));

    // choco pack
    run_cmd_in(
        pkg_dir,
        "choco",
        &["pack", &nuspec_path.to_string_lossy()],
        "chocolatey: choco pack",
    )?;

    // Resolve the API key from config or environment.
    let api_key = choco_cfg
        .api_key
        .clone()
        .or_else(|| std::env::var("CHOCOLATEY_API_KEY").ok())
        .unwrap_or_default();

    // choco push
    let nupkg = pkg_dir.join(format!("{}.{}.nupkg", crate_name, version));
    run_cmd_in(
        pkg_dir,
        "choco",
        &[
            "push",
            &nupkg.to_string_lossy(),
            "--source",
            "https://push.chocolatey.org/",
            "--api-key",
            &api_key,
        ],
        "chocolatey: choco push",
    )?;

    log.status(&format!("Chocolatey package pushed for '{}'", crate_name));

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // generate_nuspec tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_generate_nuspec_basic() {
        let nuspec = generate_nuspec(&NuspecParams {
            name: "mytool",
            version: "1.0.0",
            description: "A great tool",
            license: "MIT",
            license_url: None,
            authors: "Test Author",
            project_url: "https://github.com/org/mytool",
            icon_url: "https://example.com/icon.png",
            tags: &["cli".to_string(), "tool".to_string()],
        });

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
        let nuspec = generate_nuspec(&NuspecParams {
            name: "mytool",
            version: "1.0.0",
            description: "A tool",
            license: "MIT",
            license_url: None,
            authors: "Author",
            project_url: "https://example.com",
            icon_url: "",
            tags: &[],
        });

        assert!(!nuspec.contains("<iconUrl>"));
    }

    #[test]
    fn test_generate_nuspec_empty_tags_uses_name() {
        let nuspec = generate_nuspec(&NuspecParams {
            name: "mytool",
            version: "1.0.0",
            description: "A tool",
            license: "MIT",
            license_url: None,
            authors: "Author",
            project_url: "https://example.com",
            icon_url: "",
            tags: &[],
        });

        assert!(nuspec.contains("<tags>mytool</tags>"));
    }

    #[test]
    fn test_generate_nuspec_xml_escaping() {
        let nuspec = generate_nuspec(&NuspecParams {
            name: "my-tool",
            version: "1.0.0",
            description: "A tool for <things> & \"stuff\"",
            license: "MIT",
            license_url: None,
            authors: "Author",
            project_url: "https://example.com",
            icon_url: "",
            tags: &[],
        });

        assert!(nuspec.contains("&lt;things&gt;"));
        assert!(nuspec.contains("&amp;"));
        assert!(nuspec.contains("&quot;stuff&quot;"));
        // Should still be valid XML structure
        assert!(nuspec.contains("<?xml version=\"1.0\""));
        assert!(nuspec.contains("</package>"));
    }

    #[test]
    fn test_generate_nuspec_has_license_url_default() {
        let nuspec = generate_nuspec(&NuspecParams {
            name: "tool",
            version: "2.0.0",
            description: "desc",
            license: "Apache-2.0",
            license_url: None,
            authors: "Author",
            project_url: "https://example.com",
            icon_url: "",
            tags: &[],
        });

        assert!(
            nuspec.contains("<licenseUrl>https://opensource.org/licenses/Apache-2.0</licenseUrl>")
        );
    }

    #[test]
    fn test_generate_nuspec_custom_license_url() {
        let nuspec = generate_nuspec(&NuspecParams {
            name: "tool",
            version: "2.0.0",
            description: "desc",
            license: "Proprietary",
            license_url: Some("https://example.com/license"),
            authors: "Author",
            project_url: "https://example.com",
            icon_url: "",
            tags: &[],
        });

        assert!(nuspec.contains("<licenseUrl>https://example.com/license</licenseUrl>"));
        assert!(!nuspec.contains("opensource.org"));
    }

    #[test]
    fn test_generate_nuspec_complete_xml_structure() {
        let nuspec = generate_nuspec(&NuspecParams {
            name: "release-tool",
            version: "3.2.1",
            description: "Release automation",
            license: "MIT",
            license_url: None,
            authors: "Jane Doe",
            project_url: "https://github.com/org/release-tool",
            icon_url: "https://example.com/icon.png",
            tags: &[
                "release".to_string(),
                "automation".to_string(),
                "ci".to_string(),
            ],
        });

        // Verify the XML starts and ends correctly
        assert!(nuspec.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?>"));
        assert!(nuspec.ends_with("</package>\n"));

        // Verify metadata section
        assert!(nuspec.contains("<metadata>"));
        assert!(nuspec.contains("</metadata>"));

        // Verify files section
        assert!(nuspec.contains("<files>"));
        assert!(nuspec.contains("</files>"));
    }

    // -----------------------------------------------------------------------
    // generate_install_script tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_generate_install_script_basic() {
        let script = generate_install_script(
            "mytool",
            "https://example.com/mytool-1.0.0-windows-amd64.zip",
            "deadbeef",
        );

        assert!(script.contains("$ErrorActionPreference = 'Stop'"));
        assert!(script.contains("packageName    = 'mytool'"));
        assert!(
            script
                .contains("url64bit       = 'https://example.com/mytool-1.0.0-windows-amd64.zip'")
        );
        assert!(script.contains("checksum64     = 'deadbeef'"));
        assert!(script.contains("checksumType64 = 'sha256'"));
        assert!(script.contains("Install-ChocolateyZipPackage @packageArgs"));
    }

    #[test]
    fn test_generate_install_script_has_unzip_location() {
        let script = generate_install_script("tool", "https://example.com/tool.zip", "abc");

        assert!(script.contains("unzipLocation"));
        assert!(script.contains("Split-Path"));
    }

    #[test]
    fn test_generate_install_script_structure() {
        let script = generate_install_script("my-app", "https://example.com/my-app.zip", "hash123");

        // Verify the script has the expected structure
        let lines: Vec<&str> = script.lines().collect();
        assert_eq!(lines[0], "$ErrorActionPreference = 'Stop'");
        // There should be a blank line after ErrorActionPreference
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "$packageArgs = @{");
        // Script should end with the Install command
        assert!(
            script
                .trim_end()
                .ends_with("Install-ChocolateyZipPackage @packageArgs")
        );
    }

    // -----------------------------------------------------------------------
    // publish_to_chocolatey dry-run tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_publish_to_chocolatey_dry_run() {
        use anodize_core::config::{
            ChocolateyConfig, ChocolateyRepoConfig, Config, CrateConfig, PublishConfig,
        };
        use anodize_core::context::{Context, ContextOptions};
        use anodize_core::log::{StageLogger, Verbosity};

        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                chocolatey: Some(ChocolateyConfig {
                    project_repo: Some(ChocolateyRepoConfig {
                        owner: "myorg".to_string(),
                        name: "mytool".to_string(),
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

        // dry-run should succeed without any network/command calls
        assert!(publish_to_chocolatey(&ctx, "mytool", &log).is_ok());
    }

    #[test]
    fn test_publish_to_chocolatey_missing_config() {
        use anodize_core::config::{Config, CrateConfig, PublishConfig};
        use anodize_core::context::{Context, ContextOptions};
        use anodize_core::log::{StageLogger, Verbosity};

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

        // Should fail because there's no chocolatey config
        assert!(publish_to_chocolatey(&ctx, "mytool", &log).is_err());
    }

    #[test]
    fn test_publish_to_chocolatey_missing_project_repo() {
        use anodize_core::config::{ChocolateyConfig, Config, CrateConfig, PublishConfig};
        use anodize_core::context::{Context, ContextOptions};
        use anodize_core::log::{StageLogger, Verbosity};

        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                chocolatey: Some(ChocolateyConfig {
                    project_repo: None, // Missing
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

        // Should fail because project_repo is missing
        assert!(publish_to_chocolatey(&ctx, "mytool", &log).is_err());
    }
}
