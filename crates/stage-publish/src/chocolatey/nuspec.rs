//! `.nuspec` XML manifest generation for Chocolatey packages.

use anyhow::{Context as _, Result};

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
    /// `<licenseUrl>` value — Chocolatey's only supported license metadata:
    /// its `LicenseMetadataRule` flags any NuGet `<license>` element as
    /// CHCU0002 ("<license> elements are not supported in Chocolatey CLI,
    /// use <licenseUrl> instead"), so the SPDX expression is never emitted
    /// as an element. When `None`, no `<licenseUrl>` is emitted — anodizer
    /// never synthesizes an `opensource.org/licenses/<spdx>` URL, which 404s
    /// for compound SPDX expressions (the canonical Rust `MIT OR
    /// Apache-2.0`) and gets the package rejected at moderation. The
    /// orchestrator derives a real GitHub `…/blob/<ref>/LICENSE` URL when the
    /// release repo is known.
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
    pub dependencies: &'a [anodizer_core::config::ChocolateyDependency],
}

// ---------------------------------------------------------------------------
// nuspec template
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
{% if project_url %}    <projectUrl>{{ project_url }}</projectUrl>
{% endif %}{% if icon_url %}    <iconUrl>{{ icon_url }}</iconUrl>
{% endif %}{% if copyright %}    <copyright>{{ copyright }}</copyright>
{% endif %}{% if license_url %}    <licenseUrl>{{ license_url }}</licenseUrl>
{% endif %}    <requireLicenseAcceptance>{{ require_license_acceptance }}</requireLicenseAcceptance>
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

// ---------------------------------------------------------------------------
// generate_nuspec
// ---------------------------------------------------------------------------

pub fn generate_nuspec(params: &NuspecParams<'_>) -> Result<String> {
    let tags_str = if params.tags.is_empty() {
        params.name.to_string()
    } else {
        params.tags.join(" ")
    };
    // `<licenseUrl>` is passed through verbatim when set, else omitted.
    // It is Chocolatey's ONLY license metadata channel: choco's
    // LicenseMetadataRule warns CHCU0002 on any NuGet `<license>` element,
    // so no SPDX-expression element is ever emitted. anodizer also NEVER
    // synthesizes `https://opensource.org/licenses/<spdx>`: that URL 404s
    // for every compound SPDX expression (`MIT OR Apache-2.0`,
    // `… WITH LLVM-exception`, `CC0-1.0`), and a 404 licenseUrl gets the
    // package rejected by Chocolatey moderation. A real `<licenseUrl>` (a
    // GitHub LICENSE blob URL) is derived upstream when the release repo is
    // known.
    let license_url = params.license_url.filter(|u| !u.is_empty()).unwrap_or("");
    let title = params.title.unwrap_or(params.name);

    let tera = anodizer_core::template::parse_static("nuspec", NUSPEC_TEMPLATE)
        .context("chocolatey: parse nuspec template")?;

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
    ctx.insert("license_url", &escape_xml(license_url));
    ctx.insert("tags_str", &escape_xml(&tags_str));
    // Each optional field below is wrapped in `{% if foo %}...{% endif %}` in
    // NUSPEC_TEMPLATE; empty string is falsy in Tera so the matching tag is
    // omitted from the rendered XML. nuspec.xsd marks the following elements
    // as `minOccurs="0"`: <packageSourceUrl>, <owners>, <copyright>,
    // <projectSourceUrl>, <docsUrl>, <bugTrackerUrl>, <summary>, <releaseNotes>.
    // Reference: https://learn.microsoft.com/en-us/nuget/reference/nuspec
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

    anodizer_core::template::render_static(&tera, "nuspec", &ctx, "chocolatey")
}
