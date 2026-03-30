use anodize_core::context::Context;
use anodize_core::log::StageLogger;
use anyhow::{Context as _, Result};

use crate::util;

// ---------------------------------------------------------------------------
// XML escaping helper
// ---------------------------------------------------------------------------

/// Escape XML special characters in a string.
///
/// This ensures text content is safe for inclusion in XML elements and
/// attributes.  The five predefined XML entities are escaped:
///
/// - `&` -> `&amp;`
/// - `<` -> `&lt;`
/// - `>` -> `&gt;`
/// - `"` -> `&quot;`
/// - `'` -> `&apos;`
fn escape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

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
    pub package_source_url: Option<&'a str>,
    pub owners: Option<&'a str>,
    pub title: Option<&'a str>,
    pub copyright: Option<&'a str>,
    pub require_license_acceptance: bool,
    pub project_source_url: Option<&'a str>,
    pub docs_url: Option<&'a str>,
    pub bug_tracker_url: Option<&'a str>,
    pub summary: Option<&'a str>,
    pub release_notes: Option<&'a str>,
    pub dependencies: &'a [anodize_core::config::ChocolateyDependency],
}

// ---------------------------------------------------------------------------
// Tera templates for Chocolatey
// ---------------------------------------------------------------------------

/// Nuspec XML template.  Auto-escaping is disabled because URLs contain `/`
/// which Tera escapes to `&#x2F;`.  All text content fields are pre-escaped
/// via [`escape_xml`] before being inserted into the Tera context.
const NUSPEC_TEMPLATE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://schemas.microsoft.com/packaging/2015/06/nuspec.xsd">
  <metadata>
    <id>{{ name }}</id>
    <version>{{ version }}</version>
{% if package_source_url %}    <packageSourceUrl>{{ package_source_url }}</packageSourceUrl>
{% endif %}{% if owners %}    <owners>{{ owners }}</owners>
{% endif %}    <title>{{ title }}</title>
    <authors>{{ authors }}</authors>
    <projectUrl>{{ project_url }}</projectUrl>
{% if icon_url %}    <iconUrl>{{ icon_url }}</iconUrl>
{% endif %}{% if copyright %}    <copyright>{{ copyright }}</copyright>
{% endif %}    <licenseUrl>{{ license_url }}</licenseUrl>
    <requireLicenseAcceptance>{{ require_license_acceptance }}</requireLicenseAcceptance>
{% if project_source_url %}    <projectSourceUrl>{{ project_source_url }}</projectSourceUrl>
{% endif %}{% if docs_url %}    <docsUrl>{{ docs_url }}</docsUrl>
{% endif %}{% if bug_tracker_url %}    <bugTrackerUrl>{{ bug_tracker_url }}</bugTrackerUrl>
{% endif %}    <tags>{{ tags_str }}</tags>
{% if summary %}    <summary>{{ summary }}</summary>
{% endif %}    <description>{{ description }}</description>
{% if release_notes %}    <releaseNotes>{{ release_notes }}</releaseNotes>
{% endif %}{% if has_dependencies %}    <dependencies>
{% for dep in dependencies %}      <dependency id="{{ dep.id }}"{% if dep.version %} version="{{ dep.version }}"{% endif %} />
{% endfor %}    </dependencies>
{% endif %}  </metadata>
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

    let title = params.title.unwrap_or(params.name);

    let mut tera = tera::Tera::default();
    tera.add_raw_template("nuspec", NUSPEC_TEMPLATE)
        .expect("chocolatey: parse nuspec template");

    // Disable auto-escaping — URLs contain `/` which Tera encodes as &#x2F;.
    tera.autoescape_on(vec![]);

    let mut ctx = tera::Context::new();
    // XML-escape all text content fields to prevent injection.
    // URL fields are left unescaped since they are valid URI strings.
    ctx.insert("name", &escape_xml(params.name));
    ctx.insert("version", &escape_xml(params.version));
    ctx.insert("title", &escape_xml(title));
    ctx.insert("authors", &escape_xml(params.authors));
    ctx.insert("description", &escape_xml(params.description));
    ctx.insert("project_url", params.project_url);
    ctx.insert(
        "icon_url",
        &(!params.icon_url.is_empty()).then_some(params.icon_url),
    );
    ctx.insert("license_url", &license_url);
    ctx.insert("tags_str", &escape_xml(&tags_str));
    ctx.insert("package_source_url", &params.package_source_url.unwrap_or(""));
    ctx.insert("owners", &escape_xml(params.owners.unwrap_or("")));
    ctx.insert("copyright", &escape_xml(params.copyright.unwrap_or("")));
    ctx.insert("require_license_acceptance", &params.require_license_acceptance);
    ctx.insert("project_source_url", &params.project_source_url.unwrap_or(""));
    ctx.insert("docs_url", &params.docs_url.unwrap_or(""));
    ctx.insert("bug_tracker_url", &params.bug_tracker_url.unwrap_or(""));
    ctx.insert("summary", &escape_xml(params.summary.unwrap_or("")));
    ctx.insert("release_notes", &escape_xml(params.release_notes.unwrap_or("")));

    // Dependencies
    #[derive(serde::Serialize)]
    struct DepEntry {
        id: String,
        version: Option<String>,
    }
    let dep_entries: Vec<DepEntry> = params
        .dependencies
        .iter()
        .map(|d| DepEntry {
            id: escape_xml(&d.id),
            version: d.version.as_ref().map(|v| escape_xml(v)),
        })
        .collect();
    ctx.insert("has_dependencies", &!dep_entries.is_empty());
    ctx.insert("dependencies", &dep_entries);

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
    // SAFETY: INSTALL_SCRIPT_TEMPLATE is a compile-time constant; parse cannot fail.
    tera.add_raw_template("install", INSTALL_SCRIPT_TEMPLATE)
        .expect("chocolatey: parse install script template");

    // Disable autoescaping for PowerShell script
    tera.autoescape_on(vec![]);

    let mut ctx = tera::Context::new();
    ctx.insert("name", name);
    ctx.insert("url", url);
    ctx.insert("hash", hash);

    // SAFETY: All context variables are inserted above; rendering is infallible.
    tera.render("install", &ctx)
        .expect("chocolatey: render install script template")
}

// ---------------------------------------------------------------------------
// publish_to_chocolatey
// ---------------------------------------------------------------------------

pub fn publish_to_chocolatey(ctx: &Context, crate_name: &str, log: &StageLogger) -> Result<()> {
    let (_crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "chocolatey")?;

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

    let version = ctx.version();

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

    // Find the windows Archive artifact with IDs filtering and url_template support.
    let ids_filter = choco_cfg.ids.as_deref();
    let url_template = choco_cfg.url_template.as_deref();

    let artifact_kind = util::resolve_artifact_kind(choco_cfg.use_artifact.as_deref());
    let all_artifacts = ctx
        .artifacts
        .by_kind_and_crate(artifact_kind, crate_name);

    let win_artifact = all_artifacts
        .into_iter()
        .find(|a| {
            (a.target
                .as_deref()
                .map(|t| t.to_ascii_lowercase().contains("windows"))
                .unwrap_or(false)
                || a.path
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .contains("windows"))
                && if let Some(ids) = ids_filter {
                    a.metadata
                        .get("id")
                        .map(|id| ids.iter().any(|i| i == id))
                        .unwrap_or(false)
                } else {
                    true
                }
        });

    let pkg_name = choco_cfg.name.as_deref().unwrap_or(crate_name);

    let (url, hash) = if let Some(a) = win_artifact {
        let target = a.target.as_deref().unwrap_or("");
        let (_, raw_arch) = anodize_core::target::map_target(target);

        let resolved_url = if let Some(tmpl) = url_template {
            util::render_url_template(tmpl, pkg_name, &version, &raw_arch, "windows")
        } else {
            a.metadata
                .get("url")
                .cloned()
                .unwrap_or_else(|| a.path.to_string_lossy().into_owned())
        };

        let sha256 = a.metadata.get("sha256").cloned().unwrap_or_default();
        (resolved_url, sha256)
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
        name: choco_cfg.name.as_deref().unwrap_or(crate_name),
        version: &version,
        description: &description,
        license: &license,
        license_url: choco_cfg.license_url.as_deref(),
        authors: &authors,
        project_url: &project_url,
        icon_url: &icon_url,
        tags: &tags,
        package_source_url: choco_cfg.package_source_url.as_deref(),
        owners: choco_cfg.owners.as_deref(),
        title: choco_cfg.title.as_deref(),
        copyright: choco_cfg.copyright.as_deref(),
        require_license_acceptance: choco_cfg.require_license_acceptance.unwrap_or(false),
        project_source_url: choco_cfg.project_source_url.as_deref(),
        docs_url: choco_cfg.docs_url.as_deref(),
        bug_tracker_url: choco_cfg.bug_tracker_url.as_deref(),
        summary: choco_cfg.summary.as_deref(),
        release_notes: choco_cfg.release_notes.as_deref(),
        dependencies: choco_cfg.dependencies.as_deref().unwrap_or(&[]),
    });
    let install_script = generate_install_script(pkg_name, &url, &hash);

    // Create temp directory, write files, run choco pack + push.
    let tmp_dir = tempfile::tempdir().context("chocolatey: create temp dir")?;
    let pkg_dir = tmp_dir.path();

    let nuspec_path = pkg_dir.join(format!("{}.nuspec", pkg_name));
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
    util::run_cmd_in(
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

    // Check skip_publish
    if choco_cfg.skip_publish.unwrap_or(false) {
        log.status(&format!(
            "chocolatey: skipping push for '{}' (skip_publish=true)",
            crate_name
        ));
        return Ok(());
    }

    // choco push
    let source = choco_cfg
        .source_repo
        .as_deref()
        .unwrap_or("https://push.chocolatey.org/");
    let nupkg = pkg_dir.join(format!("{}.{}.nupkg", pkg_name, version));
    util::run_cmd_in(
        pkg_dir,
        "choco",
        &[
            "push",
            &nupkg.to_string_lossy(),
            "--source",
            source,
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

    // -----------------------------------------------------------------------
    // generate_nuspec tests
    // -----------------------------------------------------------------------

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
        let nuspec = generate_nuspec(&default_nuspec_params());
        assert!(!nuspec.contains("<iconUrl>"));
    }

    #[test]
    fn test_generate_nuspec_empty_tags_uses_name() {
        let nuspec = generate_nuspec(&default_nuspec_params());
        assert!(nuspec.contains("<tags>mytool</tags>"));
    }

    #[test]
    fn test_generate_nuspec_xml_escaping() {
        let nuspec = generate_nuspec(&NuspecParams {
            name: "my-tool",
            description: "A tool for <things> & \"stuff\"",
            ..default_nuspec_params()
        });

        assert!(nuspec.contains("&lt;things&gt;"));
        assert!(nuspec.contains("&amp;"));
        assert!(nuspec.contains("&quot;stuff&quot;"));
        assert!(nuspec.contains("<?xml version=\"1.0\""));
        assert!(nuspec.contains("</package>"));
    }

    #[test]
    fn test_generate_nuspec_xml_escaping_authors_and_apostrophe() {
        let nuspec = generate_nuspec(&NuspecParams {
            name: "my-tool",
            authors: "O'Brien & Associates",
            description: "Tool for <things> & 'stuff'",
            ..default_nuspec_params()
        });

        // Authors should be escaped (apostrophe, ampersand)
        assert!(
            nuspec.contains("<authors>O&apos;Brien &amp; Associates</authors>"),
            "authors field should have XML-escaped apostrophe and ampersand"
        );
        // Description should also escape apostrophes
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
        });

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
            license: "Proprietary",
            license_url: Some("https://example.com/license"),
            ..default_nuspec_params()
        });

        assert!(nuspec.contains("<licenseUrl>https://example.com/license</licenseUrl>"));
        assert!(!nuspec.contains("opensource.org"));
    }

    #[test]
    fn test_generate_nuspec_complete_xml_structure() {
        let tags = vec!["release".to_string(), "automation".to_string(), "ci".to_string()];
        let nuspec = generate_nuspec(&NuspecParams {
            name: "release-tool",
            version: "3.2.1",
            description: "Release automation",
            authors: "Jane Doe",
            project_url: "https://github.com/org/release-tool",
            icon_url: "https://example.com/icon.png",
            tags: &tags,
            ..default_nuspec_params()
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

    #[test]
    fn test_generate_nuspec_all_optional_fields() {
        let deps = vec![
            anodize_core::config::ChocolateyDependency {
                id: "dotnetfx".to_string(),
                version: Some("[4.5.1,)".to_string()),
            },
            anodize_core::config::ChocolateyDependency {
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
        });

        // Verify all optional fields are present
        assert!(nuspec.contains("<packageSourceUrl>https://github.com/org/choco-packages</packageSourceUrl>"));
        assert!(nuspec.contains("<owners>jdoe</owners>"));
        assert!(nuspec.contains("<title>My Tool Pro</title>"));
        assert!(nuspec.contains("<copyright>Copyright 2026 Jane Doe</copyright>"));
        assert!(nuspec.contains("<requireLicenseAcceptance>true</requireLicenseAcceptance>"));
        assert!(nuspec.contains("<projectSourceUrl>https://github.com/org/my-tool</projectSourceUrl>"));
        assert!(nuspec.contains("<docsUrl>https://docs.example.com</docsUrl>"));
        assert!(nuspec.contains("<bugTrackerUrl>https://github.com/org/my-tool/issues</bugTrackerUrl>"));
        assert!(nuspec.contains("<summary>CLI devops tool</summary>"));
        assert!(nuspec.contains("<releaseNotes>Added new features</releaseNotes>"));
        assert!(nuspec.contains("<licenseUrl>https://example.com/license</licenseUrl>"));
        assert!(nuspec.contains("<iconUrl>https://example.com/icon.png</iconUrl>"));
        assert!(nuspec.contains("<tags>cli devops</tags>"));

        // Verify dependencies
        assert!(nuspec.contains("<dependencies>"));
        assert!(nuspec.contains("<dependency id=\"dotnetfx\" version=\"[4.5.1,)\" />"));
        assert!(nuspec.contains("<dependency id=\"vcredist140\" />"));
        assert!(nuspec.contains("</dependencies>"));
    }
}
