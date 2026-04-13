use anodize_core::context::Context;
use anodize_core::log::StageLogger;
use anyhow::{Context as _, Result};

use crate::util;

// ---------------------------------------------------------------------------
// XML escaping helper
// ---------------------------------------------------------------------------

/// Escape XML special characters in a string.
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

/// Dual-arch install script (both 32-bit and 64-bit). GoReleaser parity.
const INSTALL_SCRIPT_TEMPLATE_DUAL: &str = r#"$ErrorActionPreference = 'Stop'

$packageName = '{{ name }}'
$url = '{{ url32 }}'
$url64bit = '{{ url64 }}'
$checksum = '{{ hash32 }}'
$checksum64 = '{{ hash64 }}'
$toolsDir = Split-Path -Parent $MyInvocation.MyCommand.Definition
Install-ChocolateyZipPackage $packageName $url $toolsDir $url64bit -Checksum $checksum -ChecksumType 'sha256' -Checksum64 $checksum64 -ChecksumType64 'sha256'
"#;

/// 64-bit-only install script.
const INSTALL_SCRIPT_TEMPLATE_64: &str = r#"$ErrorActionPreference = 'Stop'

$packageArgs = @{
  packageName    = '{{ name }}'
  url64bit       = '{{ url }}'
  checksum64     = '{{ hash }}'
  checksumType64 = 'sha256'
  unzipLocation  = "$(Split-Path -Parent $MyInvocation.MyCommand.Definition)"
}

Install-ChocolateyZipPackage @packageArgs
"#;

/// 32-bit-only install script.
const INSTALL_SCRIPT_TEMPLATE_32: &str = r#"$ErrorActionPreference = 'Stop'

$packageArgs = @{
  packageName   = '{{ name }}'
  url           = '{{ url }}'
  checksum      = '{{ hash }}'
  checksumType  = 'sha256'
  unzipLocation = "$(Split-Path -Parent $MyInvocation.MyCommand.Definition)"
}

Install-ChocolateyZipPackage @packageArgs
"#;

// ---------------------------------------------------------------------------
// generate_nuspec
// ---------------------------------------------------------------------------

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
    tera.autoescape_on(vec![]);

    let mut ctx = tera::Context::new();
    ctx.insert("name", &escape_xml(params.name));
    ctx.insert("version", &escape_xml(params.version));
    ctx.insert("title", &escape_xml(title));
    ctx.insert("authors", &escape_xml(params.authors));
    ctx.insert("description", &escape_xml(params.description));
    ctx.insert("project_url", &escape_xml(params.project_url));
    ctx.insert(
        "icon_url",
        &(!params.icon_url.is_empty()).then_some(escape_xml(params.icon_url)),
    );
    ctx.insert("license_url", &escape_xml(&license_url));
    ctx.insert("tags_str", &escape_xml(&tags_str));
    ctx.insert(
        "package_source_url",
        &escape_xml(params.package_source_url.unwrap_or("")),
    );
    ctx.insert("owners", &escape_xml(params.owners.unwrap_or("")));
    ctx.insert("copyright", &escape_xml(params.copyright.unwrap_or("")));
    ctx.insert(
        "require_license_acceptance",
        &params.require_license_acceptance,
    );
    ctx.insert(
        "project_source_url",
        &escape_xml(params.project_source_url.unwrap_or("")),
    );
    ctx.insert("docs_url", &escape_xml(params.docs_url.unwrap_or("")));
    ctx.insert(
        "bug_tracker_url",
        &escape_xml(params.bug_tracker_url.unwrap_or("")),
    );
    ctx.insert("summary", &escape_xml(params.summary.unwrap_or("")));
    ctx.insert(
        "release_notes",
        &escape_xml(params.release_notes.unwrap_or("")),
    );

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

/// Parameters for a dual-arch install script.
pub struct InstallScriptDual<'a> {
    pub name: &'a str,
    pub url32: &'a str,
    pub hash32: &'a str,
    pub url64: &'a str,
    pub hash64: &'a str,
}

/// Generate a single-arch install script.
pub fn generate_install_script(name: &str, url: &str, hash: &str, is_32bit: bool) -> String {
    let template = if is_32bit {
        INSTALL_SCRIPT_TEMPLATE_32
    } else {
        INSTALL_SCRIPT_TEMPLATE_64
    };
    let mut tera = tera::Tera::default();
    tera.add_raw_template("install", template)
        .expect("chocolatey: parse install script template");
    tera.autoescape_on(vec![]);
    let mut ctx = tera::Context::new();
    ctx.insert("name", name);
    ctx.insert("url", url);
    ctx.insert("hash", hash);
    tera.render("install", &ctx)
        .expect("chocolatey: render install script template")
}

/// Generate a dual-arch install script with both 32-bit and 64-bit URLs.
pub fn generate_install_script_dual(params: &InstallScriptDual<'_>) -> String {
    let mut tera = tera::Tera::default();
    tera.add_raw_template("install", INSTALL_SCRIPT_TEMPLATE_DUAL)
        .expect("chocolatey: parse dual install script template");
    tera.autoescape_on(vec![]);
    let mut ctx = tera::Context::new();
    ctx.insert("name", params.name);
    ctx.insert("url32", params.url32);
    ctx.insert("hash32", params.hash32);
    ctx.insert("url64", params.url64);
    ctx.insert("hash64", params.hash64);
    tera.render("install", &ctx)
        .expect("chocolatey: render dual install script template")
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

    if let Some(ref d) = choco_cfg.disable
        && d.is_disabled(|tmpl| ctx.render_template(tmpl))
    {
        log.status(&format!("chocolatey: disabled for '{}'", crate_name));
        return Ok(());
    }

    let project_repo = choco_cfg.project_repo.as_ref().ok_or_else(|| {
        anyhow::anyhow!("chocolatey: no project_repo config for '{}'", crate_name)
    })?;

    // GoReleaser checks SkipPublish early in Publish(), before any work.
    if choco_cfg.skip_publish == Some(true) {
        log.status(&format!(
            "chocolatey: skipping publish for '{}' (skip_publish)",
            crate_name
        ));
        return Ok(());
    }

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would push Chocolatey package for '{}' to {}/{}",
            crate_name, project_repo.owner, project_repo.name
        ));
        return Ok(());
    }

    let version = ctx.version();
    let description_raw = choco_cfg.description.as_deref().unwrap_or(crate_name);
    let description = ctx
        .render_template(description_raw)
        .unwrap_or_else(|_| description_raw.to_string());
    let license = choco_cfg.license.clone().unwrap_or_default();
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

    // Find both 32-bit and 64-bit Windows artifacts (GoReleaser parity).
    // Apply IDs + goamd64 filter.
    let ids_filter = choco_cfg.ids.as_deref();
    let url_template = choco_cfg.url_template.as_deref();
    let goamd64 = choco_cfg.goamd64.as_deref().or(Some("v1"));
    let artifact_kind = util::resolve_artifact_kind(choco_cfg.use_artifact.as_deref());
    let all_artifacts = ctx.artifacts.by_kind_and_crate(artifact_kind, crate_name);

    let win_artifacts: Vec<_> = all_artifacts
        .into_iter()
        .filter(|a| {
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
        })
        // Filter by goamd64 microarchitecture variant.
        .filter(|a| {
            let target = a.target.as_deref().unwrap_or("");
            let (_, arch) = anodize_core::target::map_target(target);
            if arch == "amd64"
                && let Some(want) = goamd64
            {
                return a.metadata.get("goamd64").is_none_or(|v| v == want);
            }
            true
        })
        .collect();

    let pkg_name = choco_cfg.name.as_deref().unwrap_or(crate_name);

    let is_32bit_target = |target: &str| -> bool {
        let lower = target.to_ascii_lowercase();
        lower.contains("i686")
            || lower.contains("i386")
            || lower.contains("386")
            || (lower.contains("x86") && !lower.contains("x86_64") && !lower.contains("x86-64"))
    };

    let mut artifact_32 = None;
    let mut artifact_64 = None;
    for a in &win_artifacts {
        let target = a.target.as_deref().unwrap_or("");
        if is_32bit_target(target) {
            if artifact_32.is_none() {
                artifact_32 = Some(a);
            }
        } else if artifact_64.is_none() {
            artifact_64 = Some(a);
        }
    }

    let resolve_artifact = |a: &anodize_core::artifact::Artifact| -> (String, String) {
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
    };

    enum InstallMode {
        Dual {
            url32: String,
            hash32: String,
            url64: String,
            hash64: String,
        },
        Single {
            url: String,
            hash: String,
            is_32bit: bool,
        },
    }

    let install_mode = match (artifact_32, artifact_64) {
        (Some(a32), Some(a64)) => {
            let (url32, hash32) = resolve_artifact(a32);
            let (url64, hash64) = resolve_artifact(a64);
            InstallMode::Dual {
                url32,
                hash32,
                url64,
                hash64,
            }
        }
        (Some(a32), None) => {
            let (url, hash) = resolve_artifact(a32);
            InstallMode::Single {
                url,
                hash,
                is_32bit: true,
            }
        }
        (None, Some(a64)) => {
            let (url, hash) = resolve_artifact(a64);
            InstallMode::Single {
                url,
                hash,
                is_32bit: false,
            }
        }
        (None, None) => {
            log.warn(&format!(
                "chocolatey: no windows artifact found for '{}', using placeholder URL",
                crate_name
            ));
            InstallMode::Single {
                url: format!(
                    "https://github.com/{0}/{1}/releases/download/v{2}/{1}-{2}-windows-amd64.zip",
                    project_repo.owner, crate_name, version
                ),
                hash: String::new(),
                is_32bit: false,
            }
        }
    };

    let title_rendered = choco_cfg
        .title
        .as_deref()
        .map(|t| ctx.render_template(t).unwrap_or_else(|_| t.to_string()));

    // Template-render Copyright, Summary, Description, ReleaseNotes
    // (GoReleaser parity: chocolatey.go:218-227). All have access to
    // standard template variables including ReleaseNotes (as "Changelog" equivalent).
    let render = |s: Option<&str>| -> Option<String> {
        s.map(|v| ctx.render_template(v).unwrap_or_else(|_| v.to_string()))
    };
    let copyright_rendered = render(choco_cfg.copyright.as_deref());
    let summary_rendered = render(choco_cfg.summary.as_deref());
    let release_notes_rendered = render(choco_cfg.release_notes.as_deref());

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
        title: title_rendered.as_deref(),
        copyright: copyright_rendered.as_deref(),
        require_license_acceptance: choco_cfg.require_license_acceptance.unwrap_or(false),
        project_source_url: choco_cfg.project_source_url.as_deref(),
        docs_url: choco_cfg.docs_url.as_deref(),
        bug_tracker_url: choco_cfg.bug_tracker_url.as_deref(),
        summary: summary_rendered.as_deref(),
        release_notes: release_notes_rendered.as_deref(),
        dependencies: choco_cfg.dependencies.as_deref().unwrap_or(&[]),
    });
    let install_script = match &install_mode {
        InstallMode::Dual {
            url32,
            hash32,
            url64,
            hash64,
        } => generate_install_script_dual(&InstallScriptDual {
            name: pkg_name,
            url32,
            hash32,
            url64,
            hash64,
        }),
        InstallMode::Single {
            url,
            hash,
            is_32bit,
        } => generate_install_script(pkg_name, url, hash, *is_32bit),
    };

    let tmp_dir = tempfile::tempdir().context("chocolatey: create temp dir")?;
    let pkg_dir = tmp_dir.path();
    let nuspec_path = pkg_dir.join(format!("{}.nuspec", pkg_name));
    std::fs::write(&nuspec_path, &nuspec)
        .with_context(|| format!("chocolatey: write nuspec {}", nuspec_path.display()))?;

    let tools_dir = pkg_dir.join("tools");
    std::fs::create_dir_all(&tools_dir).context("chocolatey: create tools dir")?;

    let install_path = tools_dir.join("chocolateyinstall.ps1");
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

    // Create .nupkg natively (OPC/ZIP format) — no `choco` CLI dependency.
    // A nupkg is a ZIP containing the nuspec, tools/, and OPC metadata files.
    let nupkg_path = pkg_dir.join(format!("{}.{}.nupkg", pkg_name, version));
    create_nupkg(pkg_name, &version, &nuspec_path, &tools_dir, &nupkg_path)?;
    log.status(&format!("created nupkg: {}", nupkg_path.display()));

    // Template-render APIKey (GoReleaser parity: chocolatey.go:184)
    let api_key = choco_cfg
        .api_key
        .as_deref()
        .map(|k| ctx.render_template(k).unwrap_or_else(|_| k.to_string()))
        .or_else(|| std::env::var("CHOCOLATEY_API_KEY").ok())
        .unwrap_or_default();

    if api_key.is_empty() {
        log.warn(&format!(
            "chocolatey: no API key for '{}', skipping push",
            crate_name
        ));
        return Ok(());
    }

    let source = choco_cfg
        .source_repo
        .as_deref()
        .unwrap_or("https://push.chocolatey.org/");

    // Push via NuGet V2 API — same protocol as `choco push`.
    push_nupkg(&nupkg_path, source, &api_key, log)?;

    log.status(&format!("Chocolatey package pushed for '{}'", crate_name));
    Ok(())
}

// ---------------------------------------------------------------------------
// Native nupkg creation (OPC/ZIP format)
// ---------------------------------------------------------------------------

/// Content types XML — required by the OPC (Open Packaging Conventions) spec.
/// Maps file extensions to MIME types within the package.
const CONTENT_TYPES_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml" />
  <Default Extension="nuspec" ContentType="application/octet-stream" />
  <Default Extension="ps1" ContentType="application/octet-stream" />
  <Default Extension="psmdcp" ContentType="application/vnd.openxmlformats-package.core-properties+xml" />
</Types>"#;

/// Package relationships XML — links the nuspec as the package manifest.
fn rels_xml(nuspec_filename: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Type="http://schemas.microsoft.com/packaging/2010/07/manifest" Target="/{}" Id="R1" />
</Relationships>"#,
        nuspec_filename
    )
}

/// Create a .nupkg file (OPC-compliant ZIP) from a nuspec and tools directory.
///
/// The nupkg format is an Open Packaging Conventions (OPC) archive:
/// - `[Content_Types].xml` — MIME type mappings
/// - `_rels/.rels` — package relationships (points to the nuspec)
/// - `{name}.nuspec` — NuGet/Chocolatey package manifest
/// - `tools/**` — package content (install scripts, binaries)
///
/// This replaces the `choco pack` CLI command with native Rust ZIP creation,
/// eliminating the dependency on the Windows-only Chocolatey CLI.
fn create_nupkg(
    name: &str,
    version: &str,
    nuspec_path: &std::path::Path,
    tools_dir: &std::path::Path,
    output_path: &std::path::Path,
) -> Result<()> {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let nuspec_content = std::fs::read(nuspec_path)
        .with_context(|| format!("chocolatey: read nuspec {}", nuspec_path.display()))?;

    let file = std::fs::File::create(output_path)
        .with_context(|| format!("chocolatey: create nupkg {}", output_path.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    // [Content_Types].xml (must be at root of ZIP)
    zip.start_file("[Content_Types].xml", options)?;
    zip.write_all(CONTENT_TYPES_XML.as_bytes())?;

    // _rels/.rels
    let nuspec_filename = format!("{}.nuspec", name);
    zip.start_file("_rels/.rels", options)?;
    zip.write_all(rels_xml(&nuspec_filename).as_bytes())?;

    // {name}.nuspec
    zip.start_file(&nuspec_filename, options)?;
    zip.write_all(&nuspec_content)?;

    // tools/** — walk the tools directory and add all files
    if tools_dir.exists() {
        for entry in walkdir(tools_dir)? {
            let rel_path = entry
                .strip_prefix(tools_dir.parent().unwrap_or(tools_dir))
                .unwrap_or(&entry);
            // Use forward slashes in ZIP paths (per ZIP spec and NuGet convention)
            let zip_path = rel_path.to_string_lossy().replace('\\', "/");
            let content = std::fs::read(&entry)
                .with_context(|| format!("chocolatey: read {}", entry.display()))?;
            zip.start_file(&zip_path, options)?;
            zip.write_all(&content)?;
        }
    }

    zip.finish()?;

    // Validate: the nupkg should be a valid ZIP with reasonable size
    let metadata = std::fs::metadata(output_path)?;
    if metadata.len() == 0 {
        anyhow::bail!(
            "chocolatey: generated nupkg is empty: {}",
            output_path.display()
        );
    }

    // Log the package details (GoReleaser parity: chocolatey.go:167)
    let _nupkg_name = format!("{}.{}.nupkg", name, version);

    Ok(())
}

/// Recursively collect all files in a directory.
fn walkdir(dir: &std::path::Path) -> Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("chocolatey: read dir {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            files.extend(walkdir(&path)?);
        } else {
            files.push(path);
        }
    }
    Ok(files)
}

/// Push a .nupkg to a NuGet V2 API endpoint (Chocolatey, NuGet.org, etc.).
///
/// Uses the same HTTP PUT protocol as `choco push`:
/// - PUT to `{source}/api/v2/package`
/// - `X-NuGet-ApiKey` header for authentication
/// - Raw nupkg bytes as the request body
fn push_nupkg(
    nupkg_path: &std::path::Path,
    source: &str,
    api_key: &str,
    log: &StageLogger,
) -> Result<()> {
    let nupkg_data = std::fs::read(nupkg_path)
        .with_context(|| format!("chocolatey: read nupkg {}", nupkg_path.display()))?;

    // Normalize source URL and construct push endpoint
    let base = source.trim_end_matches('/');
    let push_url = if base.ends_with("/api/v2/package") {
        base.to_string()
    } else if base.ends_with("/api/v2") {
        format!("{}/package", base)
    } else {
        format!("{}/api/v2/package", base)
    };

    log.status(&format!("pushing nupkg to {}", push_url));

    let client = reqwest::blocking::Client::new();
    let response = client
        .put(&push_url)
        .header("X-NuGet-ApiKey", api_key)
        .header("Content-Type", "application/octet-stream")
        .body(nupkg_data)
        .timeout(std::time::Duration::from_secs(300))
        .send()
        .with_context(|| format!("chocolatey: push to {}", push_url))?;

    let status = response.status();
    if status.is_success() || status.as_u16() == 201 {
        Ok(())
    } else {
        let body = response.text().unwrap_or_default();
        anyhow::bail!(
            "chocolatey: push failed with HTTP {} to {}: {}",
            status,
            push_url,
            body
        )
    }
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
    }

    #[test]
    fn test_generate_nuspec_xml_escaping_authors_and_apostrophe() {
        let nuspec = generate_nuspec(&NuspecParams {
            name: "my-tool",
            authors: "O'Brien & Associates",
            description: "Tool for <things> & 'stuff'",
            ..default_nuspec_params()
        });
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
        });
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
    fn test_generate_install_script_32bit() {
        let script = generate_install_script(
            "mytool",
            "https://example.com/mytool-1.0.0-windows-x86.zip",
            "deadbeef",
            true,
        );
        assert!(script.contains("packageName   = 'mytool'"));
        assert!(
            script.contains("url           = 'https://example.com/mytool-1.0.0-windows-x86.zip'")
        );
        assert!(script.contains("checksum      = 'deadbeef'"));
        assert!(!script.contains("url64bit"));
        assert!(!script.contains("checksum64"));
    }

    #[test]
    fn test_generate_install_script_has_unzip_location() {
        let script = generate_install_script("tool", "https://example.com/tool.zip", "abc", false);
        assert!(script.contains("unzipLocation"));
        assert!(script.contains("Split-Path"));
    }

    #[test]
    fn test_generate_install_script_structure() {
        let script =
            generate_install_script("my-app", "https://example.com/my-app.zip", "hash123", false);
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
        });
        assert!(script.contains("$ErrorActionPreference = 'Stop'"));
        assert!(script.contains("$packageName = 'mytool'"));
        assert!(script.contains("$url = 'https://example.com/mytool-1.0.0-windows-386.zip'"));
        assert!(
            script.contains("$url64bit = 'https://example.com/mytool-1.0.0-windows-amd64.zip'")
        );
        assert!(script.contains("$checksum = 'hash32abc'"));
        assert!(script.contains("$checksum64 = 'hash64def'"));
        assert!(
            script.contains("Install-ChocolateyZipPackage $packageName $url $toolsDir $url64bit")
        );
        assert!(script.contains("-Checksum $checksum -ChecksumType 'sha256'"));
        assert!(script.contains("-Checksum64 $checksum64 -ChecksumType64 'sha256'"));
    }

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
                    project_repo: None,
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
        assert!(nuspec.contains(
            "<packageSourceUrl>https://github.com/org/choco-packages</packageSourceUrl>"
        ));
        assert!(nuspec.contains("<owners>jdoe</owners>"));
        assert!(nuspec.contains("<title>My Tool Pro</title>"));
        assert!(nuspec.contains("<copyright>Copyright 2026 Jane Doe</copyright>"));
        assert!(nuspec.contains("<requireLicenseAcceptance>true</requireLicenseAcceptance>"));
        assert!(
            nuspec.contains("<projectSourceUrl>https://github.com/org/my-tool</projectSourceUrl>")
        );
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
    fn test_chocolatey_skip_publish_bool_config() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      chocolatey:
        skip_publish: true
        project_repo:
          owner: org
          name: test
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let choco = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .chocolatey
            .as_ref()
            .unwrap();
        assert_eq!(choco.skip_publish, Some(true));
    }

    #[test]
    fn test_chocolatey_skip_publish_false_config() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      chocolatey:
        skip_publish: false
        project_repo:
          owner: org
          name: test
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let choco = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .chocolatey
            .as_ref()
            .unwrap();
        assert_eq!(choco.skip_publish, Some(false));
    }

    #[test]
    fn test_create_nupkg_produces_valid_opc_zip() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg_dir = tmp.path();

        // Write nuspec
        let nuspec = generate_nuspec(&default_nuspec_params());
        let nuspec_path = pkg_dir.join("mytool.nuspec");
        std::fs::write(&nuspec_path, &nuspec).unwrap();

        // Write install script
        let tools_dir = pkg_dir.join("tools");
        std::fs::create_dir_all(&tools_dir).unwrap();
        let script =
            generate_install_script("mytool", "https://example.com/dl.zip", "abc123", false);
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
}
