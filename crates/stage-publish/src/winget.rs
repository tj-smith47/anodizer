use std::sync::LazyLock;

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::template::{self, TemplateVars};
use anodizer_core::util::static_regex;
use anyhow::{Context as _, Result};
use regex::Regex;
use serde::Serialize;

use crate::util;

// winget PackageIdentifier regex:
// `^[^\.\s\\/:\*\?"<>\|\x01-\x1f]{1,32}(\.[^\.\s\\/:\*\?"<>\|\x01-\x1f]{1,32}){1,7}$`
//
// Two delta points vs. the loose regex this replaced:
//   1. Each segment is bounded to 1..=32 chars (live winget validator
//      enforces this; longer segments fail the upstream PR check).
//   2. ASCII control chars `\x01..=\x1f` are excluded explicitly — winget
//      rejects them, so anodizer must too.
//
// `\x00` (NUL) is also rejected by winget but `regex` interprets `\x00`
// inside `[^...]` as the empty boundary; we strip NULs explicitly below
// before applying the regex to keep the engine happy.
static PACKAGE_IDENTIFIER_RE: LazyLock<Regex> = LazyLock::new(|| {
    static_regex(
        r#"^[^\.\s\\/:\*\?"<>\|\x01-\x1f]{1,32}(\.[^\.\s\\/:\*\?"<>\|\x01-\x1f]{1,32}){1,7}$"#,
    )
});

// ---------------------------------------------------------------------------
// PackageIdentifier validation
// ---------------------------------------------------------------------------

/// Validate a WinGet PackageIdentifier against the required pattern.
///
/// The identifier must have 2-8 dot-separated segments, each segment 1-32
/// characters, with no whitespace, ASCII control chars (`\x01-\x1f`), or
/// the characters `\`, `/`, `:`, `*`, `?`, `"`, `<`, `>`, `|`.
///
/// Pattern: `^[^\.\s\\/:\*\?"<>\|\x01-\x1f]{1,32}(\.[^\.\s\\/:\*\?"<>\|\x01-\x1f]{1,32}){1,7}$`
pub fn validate_package_identifier(id: &str) -> Result<()> {
    // NUL (`\x00`) is also forbidden by winget. The regex's character class
    // already excludes `\x01-\x1f` but excluding `\x00` inside an
    // already-negated class is awkward; reject NULs explicitly.
    if !id.contains('\u{0}') && PACKAGE_IDENTIFIER_RE.is_match(id) {
        Ok(())
    } else {
        anyhow::bail!(
            "winget: invalid PackageIdentifier '{}'. Must have 2-8 dot-separated segments, \
             each 1-32 chars, with no whitespace, control chars, or special characters \
             (\\/:*?\"<>|).",
            id
        )
    }
}

// ---------------------------------------------------------------------------
// Winget commit message rendering
// ---------------------------------------------------------------------------

/// Render a commit message for WinGet with PackageIdentifier in the context.
/// `PackageIdentifier` is exposed as an extra template field.
fn render_winget_commit_msg(template: Option<&str>, package_id: &str, version: &str) -> String {
    // Default: "New version: {{ .PackageIdentifier }} {{ .Version }}"
    let default_tmpl = "New version: {{ PackageIdentifier }} {{ Version }}";
    let tmpl = template.unwrap_or(default_tmpl);

    let mut vars = TemplateVars::new();
    vars.set("PackageIdentifier", package_id);
    vars.set("Version", version);
    vars.set("name", package_id);
    vars.set("version", version);
    template::render(tmpl, &vars)
        .unwrap_or_else(|_| format!("New version: {} {}", package_id, version))
}

// ---------------------------------------------------------------------------
// WingetManifestParams
// ---------------------------------------------------------------------------

/// Parameters for generating WinGet YAML manifests.
pub(crate) struct WingetManifestParams<'a> {
    pub(crate) package_id: &'a str,
    pub(crate) name: &'a str,
    /// Display name for the package. Falls back to `name` when not set.
    pub(crate) package_name: Option<&'a str>,
    pub(crate) version: &'a str,
    pub(crate) description: &'a str,
    pub(crate) short_description: &'a str,
    pub(crate) license: &'a str,
    pub(crate) license_url: Option<&'a str>,
    pub(crate) publisher: &'a str,
    pub(crate) publisher_url: Option<&'a str>,
    pub(crate) publisher_support_url: Option<&'a str>,
    pub(crate) privacy_url: Option<&'a str>,
    pub(crate) author: Option<&'a str>,
    pub(crate) copyright: Option<&'a str>,
    pub(crate) copyright_url: Option<&'a str>,
    pub(crate) homepage: Option<&'a str>,
    pub(crate) release_notes: Option<&'a str>,
    pub(crate) release_notes_url: Option<&'a str>,
    pub(crate) installation_notes: Option<&'a str>,
    pub(crate) tags: Option<&'a [String]>,
    pub(crate) dependencies: &'a [anodizer_core::config::WingetDependency],
    pub(crate) installers: Vec<WingetInstallerItem>,
    /// Product code for the installer (used in Add/Remove Programs).
    pub(crate) product_code: Option<&'a str>,
    /// Release date in YYYY-MM-DD format.
    pub(crate) release_date: Option<&'a str>,
}

/// A single installer entry in the WinGet manifest.
pub(crate) struct WingetInstallerItem {
    pub(crate) architecture: String,
    pub(crate) url: String,
    pub(crate) sha256: String,
    /// Installer type: "zip" for archive artifacts, "portable" for bare binaries.
    pub(crate) installer_type: String,
    /// Binary names contained in this archive.  When multiple binaries are
    /// present, each gets its own `NestedInstallerFile` entry.
    pub(crate) binaries: Vec<String>,
    /// When the archive wraps contents in a top-level directory, this holds that
    /// directory name.  `RelativeFilePath` entries will be prefixed with it.
    pub(crate) wrap_in_directory: Option<String>,
    /// Commands for portable binaries (the binary filename without extension).
    pub(crate) commands: Vec<String>,
}

// ---------------------------------------------------------------------------
// Serde structs for WinGet YAML manifests (3-file format)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct VersionManifest {
    package_identifier: String,
    package_version: String,
    default_locale: String,
    manifest_type: String,
    manifest_version: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct InstallerManifest {
    package_identifier: String,
    package_version: String,
    installer_locale: String,
    installer_type: String,
    /// Commands for portable binaries.
    #[serde(skip_serializing_if = "Option::is_none")]
    commands: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    product_code: Option<String>,
    installers: Vec<InstallerEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dependencies: Option<DependenciesBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    release_date: Option<String>,
    manifest_type: String,
    manifest_version: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct InstallerEntry {
    architecture: String,
    installer_url: String,
    #[serde(rename = "InstallerSha256")]
    installer_sha256: String,
    upgrade_behavior: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    nested_installer_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    nested_installer_files: Option<Vec<NestedInstallerFile>>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct NestedInstallerFile {
    relative_file_path: String,
    portable_command_alias: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct DependenciesBlock {
    package_dependencies: Vec<PkgDep>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct PkgDep {
    package_identifier: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    minimum_version: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct LocaleManifest {
    package_identifier: String,
    package_version: String,
    package_locale: String,
    publisher: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    publisher_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    publisher_support_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    privacy_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    author: Option<String>,
    package_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    package_url: Option<String>,
    license: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    license_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    copyright: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    copyright_url: Option<String>,
    short_description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    moniker: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    release_notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    release_notes_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    installation_notes: Option<String>,
    manifest_type: String,
    manifest_version: String,
}

// Legacy single-file manifest for backward compatibility
#[allow(dead_code)]
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct WingetManifest {
    package_identifier: String,
    package_version: String,
    package_name: String,
    publisher: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    publisher_url: Option<String>,
    license: String,
    short_description: String,
    installers: Vec<LegacyInstaller>,
    manifest_type: String,
    manifest_version: String,
}

#[allow(dead_code)]
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct LegacyInstaller {
    architecture: String,
    installer_url: String,
    #[serde(rename = "InstallerSha256")]
    installer_sha256: String,
    installer_type: String,
}

// ---------------------------------------------------------------------------
// generate_manifest (legacy single-file)
// ---------------------------------------------------------------------------

/// Generate a legacy singleton WinGet YAML manifest string.
#[allow(dead_code)]
pub(crate) fn generate_manifest(params: &WingetManifestParams<'_>) -> Result<String> {
    let manifest = WingetManifest {
        package_identifier: params.package_id.to_string(),
        package_version: params.version.to_string(),
        package_name: params.name.to_string(),
        publisher: params.publisher.to_string(),
        publisher_url: params
            .publisher_url
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty()),
        license: params.license.to_string(),
        short_description: params.short_description.to_string(),
        installers: params
            .installers
            .iter()
            .map(|i| LegacyInstaller {
                architecture: i.architecture.clone(),
                installer_url: i.url.clone(),
                installer_sha256: i.sha256.clone(),
                installer_type: "zip".to_string(),
            })
            .collect(),
        manifest_type: "singleton".to_string(),
        manifest_version: "1.12.0".to_string(),
    };
    serde_yaml_ng::to_string(&manifest).context("winget: serialize manifest")
}

/// Generate the 3-file WinGet manifest set: (version, installer, locale).
pub(crate) fn generate_manifests(
    params: &WingetManifestParams<'_>,
) -> Result<(String, String, String)> {
    let version = VersionManifest {
        package_identifier: params.package_id.to_string(),
        package_version: params.version.to_string(),
        default_locale: "en-US".to_string(),
        manifest_type: "version".to_string(),
        manifest_version: "1.12.0".to_string(),
    };

    let deps = if params.dependencies.is_empty() {
        None
    } else {
        Some(DependenciesBlock {
            package_dependencies: params
                .dependencies
                .iter()
                .map(|d| PkgDep {
                    package_identifier: d.package_identifier.clone(),
                    minimum_version: d.minimum_version.clone(),
                })
                .collect(),
        })
    };

    // Determine the top-level installer type from the first item.
    // All items should be the same type (mixed format validation happens earlier).
    let installer_type = params
        .installers
        .first()
        .map(|i| i.installer_type.as_str())
        .unwrap_or("zip");

    // Collect Commands from portable binary installers for top-level placement
    // (Commands sits on the top-level Installer struct).
    let top_commands: Option<Vec<String>> = {
        let cmds: Vec<String> = params
            .installers
            .iter()
            .flat_map(|i| i.commands.iter().cloned())
            .collect();
        if cmds.is_empty() { None } else { Some(cmds) }
    };

    let installer = InstallerManifest {
        package_identifier: params.package_id.to_string(),
        package_version: params.version.to_string(),
        installer_locale: "en-US".to_string(),
        installer_type: installer_type.to_string(),
        commands: top_commands,
        product_code: params.product_code.map(|s| s.to_string()),
        installers: params
            .installers
            .iter()
            .map(|i| {
                let (nested_type, nested_files) = if i.installer_type == "zip" {
                    // ZIP archives: add nested installer info for portable executables.
                    let bins = if i.binaries.is_empty() {
                        vec![params.name.to_string()]
                    } else {
                        i.binaries.clone()
                    };
                    let files: Vec<NestedInstallerFile> = bins
                        .iter()
                        .map(|bin_name| {
                            let exe_name = format!("{}.exe", bin_name);
                            let relative_file_path = match i.wrap_in_directory.as_deref() {
                                Some(dir) if !dir.is_empty() => format!("{}\\{}", dir, exe_name),
                                _ => exe_name,
                            };
                            NestedInstallerFile {
                                relative_file_path,
                                portable_command_alias: bin_name.clone(),
                            }
                        })
                        .collect();
                    (Some("portable".to_string()), Some(files))
                } else {
                    (None, None)
                };

                InstallerEntry {
                    architecture: i.architecture.clone(),
                    installer_url: i.url.clone(),
                    installer_sha256: i.sha256.clone(),
                    upgrade_behavior: "uninstallPrevious".to_string(),
                    nested_installer_type: nested_type,
                    nested_installer_files: nested_files,
                }
            })
            .collect(),
        dependencies: deps,
        release_date: params.release_date.map(|s| s.to_string()),
        manifest_type: "installer".to_string(),
        manifest_version: "1.12.0".to_string(),
    };

    let tags = params.tags.map(|t| {
        t.iter()
            .map(|s| s.to_lowercase().replace(' ', "-"))
            .collect::<Vec<_>>()
    });

    let locale = LocaleManifest {
        package_identifier: params.package_id.to_string(),
        package_version: params.version.to_string(),
        package_locale: "en-US".to_string(),
        publisher: params.publisher.to_string(),
        publisher_url: params
            .publisher_url
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty()),
        publisher_support_url: params
            .publisher_support_url
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty()),
        privacy_url: params
            .privacy_url
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty()),
        author: params
            .author
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty()),
        package_name: params.package_name.unwrap_or(params.name).to_string(),
        package_url: params
            .homepage
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty()),
        license: params.license.to_string(),
        license_url: params
            .license_url
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty()),
        copyright: params
            .copyright
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty()),
        copyright_url: params
            .copyright_url
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty()),
        short_description: params.short_description.to_string(),
        description: if params.description.is_empty() {
            None
        } else {
            Some(params.description.to_string())
        },
        moniker: params.name.to_string(),
        tags,
        release_notes: params
            .release_notes
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty()),
        release_notes_url: params
            .release_notes_url
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty()),
        installation_notes: params
            .installation_notes
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty()),
        manifest_type: "defaultLocale".to_string(),
        manifest_version: "1.12.0".to_string(),
    };

    const GENERATED_HEADER: &str = "# This file was generated by anodizer. DO NOT EDIT.\n";
    const SCHEMA_VERSION: &str = "# yaml-language-server: $schema=https://aka.ms/winget-manifest.version.1.12.0.schema.json\n";
    const SCHEMA_INSTALLER: &str = "# yaml-language-server: $schema=https://aka.ms/winget-manifest.installer.1.12.0.schema.json\n";
    const SCHEMA_LOCALE: &str = "# yaml-language-server: $schema=https://aka.ms/winget-manifest.defaultLocale.1.12.0.schema.json\n";

    let version_yaml =
        serde_yaml_ng::to_string(&version).context("winget: serialize version manifest")?;
    let installer_yaml =
        serde_yaml_ng::to_string(&installer).context("winget: serialize installer manifest")?;
    let locale_yaml =
        serde_yaml_ng::to_string(&locale).context("winget: serialize locale manifest")?;
    Ok((
        format!("{}{}{}", GENERATED_HEADER, SCHEMA_VERSION, version_yaml),
        format!("{}{}{}", GENERATED_HEADER, SCHEMA_INSTALLER, installer_yaml),
        format!("{}{}{}", GENERATED_HEADER, SCHEMA_LOCALE, locale_yaml),
    ))
}

// ---------------------------------------------------------------------------
// publish_to_winget helpers
// ---------------------------------------------------------------------------

/// Resolve the publisher name, falling back to the GitHub repo owner when
/// the config omits an explicit publisher. Errors when both are empty.
fn resolve_winget_publisher_name<'a>(
    winget_cfg: &'a anodizer_core::config::WingetConfig,
    repo_owner: &'a str,
    crate_name: &str,
    log: &StageLogger,
) -> Result<&'a str> {
    match winget_cfg.publisher.as_deref() {
        Some(p) if !p.is_empty() => Ok(p),
        _ => {
            if repo_owner.is_empty() {
                anyhow::bail!(
                    "winget: publisher is required but not configured for '{}', \
                     and repo owner is also empty. Set `publish.winget.publisher` in your config.",
                    crate_name
                );
            }
            log.warn(&format!(
                "winget: publisher not explicitly set for '{}'; falling back to repo owner '{}'",
                crate_name, repo_owner
            ));
            Ok(repo_owner)
        }
    }
}

/// Resolve the package description (with Pro-parity fallback to project
/// `metadata.description`), template-render it, and normalize embedded tabs.
fn resolve_winget_description(
    ctx: &Context,
    winget_cfg: &anodizer_core::config::WingetConfig,
    crate_name: &str,
) -> String {
    let description_raw_cfg = winget_cfg
        .description
        .as_deref()
        .or_else(|| ctx.config.meta_description_for(crate_name))
        .unwrap_or("");
    let description_tmpl = ctx
        .render_template(description_raw_cfg)
        .unwrap_or_else(|_| description_raw_cfg.to_string());
    description_tmpl.replace('\t', "  ")
}

/// Resolve the required short description with the layered winget →
/// winget.description → metadata.description fallback. Never silently
/// substitutes the crate name (a winget reviewer would reject that).
fn resolve_winget_short_description(
    ctx: &Context,
    winget_cfg: &anodizer_core::config::WingetConfig,
    crate_name: &str,
) -> Result<String> {
    let short_desc_raw = winget_cfg
        .short_description
        .as_deref()
        .or(winget_cfg.description.as_deref())
        .or_else(|| ctx.config.meta_description_for(crate_name))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "winget: short_description is required but not configured for \
                 '{crate_name}'. Set `publish.winget.short_description`, or a \
                 fallback via `publish.winget.description` or top-level \
                 `metadata.description`."
            )
        })?;
    Ok(short_desc_raw.replace('\t', "  "))
}

/// Resolve the required license with metadata fallback.
fn resolve_winget_license<'a>(
    ctx: &'a Context,
    winget_cfg: &'a anodizer_core::config::WingetConfig,
    crate_name: &str,
) -> Result<&'a str> {
    winget_cfg
        .license
        .as_deref()
        .or_else(|| ctx.config.meta_license_for(crate_name))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "winget: license is required but not configured for '{}'. \
             Set `publish.winget.license` in your config.",
                crate_name
            )
        })
}

/// Build the target-triple → binary-name map from windows Build-kind
/// artifacts. Drives `NestedInstallerFiles` for each zip installer entry.
fn collect_windows_binary_names_by_target(
    ctx: &Context,
    crate_name: &str,
) -> std::collections::HashMap<String, Vec<String>> {
    let mut map: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    let win_binaries = ctx
        .artifacts
        .by_kind_and_crate(anodizer_core::artifact::ArtifactKind::Binary, crate_name);
    for b in &win_binaries {
        let target = b.target.as_deref().unwrap_or("");
        if !target.to_ascii_lowercase().contains("windows") {
            continue;
        }
        if let Some(bin_name) = b.metadata.get("binary") {
            let entry = map.entry(target.to_string()).or_default();
            if !entry.contains(bin_name) {
                entry.push(bin_name.clone());
            }
        }
    }
    map
}

/// Map a raw architecture string to the WinGet vocabulary.
fn map_winget_arch(raw_arch: &str) -> &str {
    match raw_arch {
        "amd64" => "x64",
        "386" | "i686" => "x86",
        "arm64" => "arm64",
        other => other,
    }
}

/// Resolve the installer download URL for an artifact: prefer the
/// configured `url_template`, fall back to the artifact's `url` metadata,
/// then the on-disk path string.
fn resolve_installer_url(
    ctx: &Context,
    a: &anodizer_core::artifact::Artifact,
    url_template: Option<&str>,
    name: &str,
    version: &str,
    raw_arch: &str,
) -> String {
    if let Some(tmpl) = url_template {
        util::render_url_template_with_ctx(ctx, tmpl, name, version, raw_arch, "windows")
    } else {
        a.metadata
            .get("url")
            .cloned()
            .unwrap_or_else(|| a.path.to_string_lossy().into_owned())
    }
}

/// Artifact-selection filters for windows winget installers: windows-only,
/// optional id allow-list, and amd64_variant selection.
struct WingetArtifactFilters<'a> {
    ids: Option<&'a [String]>,
    amd64_variant: Option<&'a str>,
}

impl<'a> WingetArtifactFilters<'a> {
    fn matches(&self, a: &anodizer_core::artifact::Artifact) -> bool {
        let is_windows = a
            .target
            .as_deref()
            .map(|t| t.to_ascii_lowercase().contains("windows"))
            .unwrap_or(false)
            || a.path
                .to_string_lossy()
                .to_ascii_lowercase()
                .contains("windows");
        if !is_windows {
            return false;
        }
        if let Some(ids) = self.ids {
            let matched = a
                .metadata
                .get("id")
                .map(|id| ids.iter().any(|i| i == id))
                .unwrap_or(false);
            if !matched {
                return false;
            }
        }
        let target = a.target.as_deref().unwrap_or("");
        let (_, arch) = anodizer_core::target::map_target(target);
        if arch == "amd64"
            && let Some(want) = self.amd64_variant
            && a.metadata.get("amd64_variant").is_some_and(|v| v != want)
        {
            return false;
        }
        true
    }
}

/// Build a single zip-archive [`WingetInstallerItem`] from a matching
/// archive artifact. Errors when the archive has no sha256 (which would
/// produce a manifest the winget validator rejects).
fn build_archive_installer(
    ctx: &Context,
    a: &anodizer_core::artifact::Artifact,
    url_template: Option<&str>,
    name: &str,
    version: &str,
    binary_names_by_target: &std::collections::HashMap<String, Vec<String>>,
) -> Result<WingetInstallerItem> {
    let target = a.target.as_deref().unwrap_or("");
    let (_, raw_arch) = anodizer_core::target::map_target(target);
    let arch = map_winget_arch(raw_arch.as_str());
    let resolved_url = resolve_installer_url(ctx, a, url_template, name, version, &raw_arch);
    let sha256 = a.metadata.get("sha256").cloned().unwrap_or_default();
    if sha256.is_empty() {
        anyhow::bail!(
            "winget: archive '{}' has no sha256 metadata; \
             the manifest would publish with InstallerSha256: '' \
             and be rejected by winget validation. \
             Ensure the checksum stage runs before winget, or that \
             the publish flow seeds sha256 onto downloaded assets.",
            a.path.display()
        );
    }
    let wrap_in_directory = a.metadata.get("wrap_in_directory").cloned();
    let binaries = binary_names_by_target
        .get(target)
        .cloned()
        .unwrap_or_default();
    Ok(WingetInstallerItem {
        architecture: arch.to_string(),
        url: resolved_url,
        sha256,
        installer_type: "zip".to_string(),
        binaries,
        wrap_in_directory,
        commands: Vec::new(),
    })
}

/// Build a portable-binary [`WingetInstallerItem`] from a matching
/// UploadableBinary artifact. Errors when sha256 metadata is missing.
fn build_portable_installer(
    ctx: &Context,
    a: &anodizer_core::artifact::Artifact,
    url_template: Option<&str>,
    name: &str,
    version: &str,
) -> Result<WingetInstallerItem> {
    let target = a.target.as_deref().unwrap_or("");
    let (_, raw_arch) = anodizer_core::target::map_target(target);
    let arch = map_winget_arch(raw_arch.as_str());
    let resolved_url = resolve_installer_url(ctx, a, url_template, name, version, &raw_arch);
    let sha256 = a.metadata.get("sha256").cloned().unwrap_or_default();
    if sha256.is_empty() {
        anyhow::bail!(
            "winget: portable binary '{}' has no sha256 metadata; \
             the manifest would publish with InstallerSha256: '' \
             and be rejected by winget validation. \
             Ensure the checksum stage runs before winget, or that \
             the publish flow seeds sha256 onto downloaded assets.",
            a.path.display()
        );
    }
    let cmd = a
        .metadata
        .get("binary")
        .cloned()
        .unwrap_or_else(|| name.to_string());
    Ok(WingetInstallerItem {
        architecture: arch.to_string(),
        url: resolved_url,
        sha256,
        installer_type: "portable".to_string(),
        binaries: Vec::new(),
        wrap_in_directory: None,
        commands: vec![cmd],
    })
}

/// Collect, filter, and validate all windows installers (zip archives +
/// portable binaries) for a crate. Rejects mixed archive/portable formats
/// and duplicate-architecture entries.
fn collect_winget_installers(
    ctx: &Context,
    crate_name: &str,
    winget_cfg: &anodizer_core::config::WingetConfig,
    name: &str,
    version: &str,
) -> Result<Vec<WingetInstallerItem>> {
    let ids_filter = winget_cfg.ids.as_deref();
    let url_template = winget_cfg.url_template.as_deref();
    let amd64_variant = winget_cfg.amd64_variant.as_deref().or(Some("v1"));
    let artifact_kind = util::resolve_artifact_kind(winget_cfg.use_artifact.as_deref());

    let binary_names_by_target = collect_windows_binary_names_by_target(ctx, crate_name);

    let archive_artifacts = ctx.artifacts.by_kind_and_crate(artifact_kind, crate_name);
    let binary_artifacts = ctx.artifacts.by_kind_and_crate(
        anodizer_core::artifact::ArtifactKind::UploadableBinary,
        crate_name,
    );

    let filters = WingetArtifactFilters {
        ids: ids_filter,
        amd64_variant,
    };

    let mut installers: Vec<WingetInstallerItem> = Vec::new();
    let mut zip_count = 0u32;
    let mut binary_count = 0u32;

    for a in archive_artifacts.iter() {
        if !filters.matches(a) {
            continue;
        }
        let format = a.metadata.get("format").map(|f| f.as_str()).unwrap_or("");
        if format != "zip" && !a.path.to_string_lossy().ends_with(".zip") {
            continue;
        }
        zip_count += 1;
        installers.push(build_archive_installer(
            ctx,
            a,
            url_template,
            name,
            version,
            &binary_names_by_target,
        )?);
    }

    for a in binary_artifacts.iter() {
        if !filters.matches(a) {
            continue;
        }
        binary_count += 1;
        installers.push(build_portable_installer(
            ctx,
            a,
            url_template,
            name,
            version,
        )?);
    }

    if binary_count > 0 && zip_count > 0 {
        anyhow::bail!(
            "winget: found archives with multiple formats (.exe and .zip) for '{}'; \
             use either portable binaries or zip archives, not both",
            crate_name
        );
    }

    let mut arch_counts: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
    for i in &installers {
        *arch_counts.entry(&i.architecture).or_default() += 1;
    }
    for (arch, count) in &arch_counts {
        if *count > 1 {
            anyhow::bail!(
                "winget: found multiple archives for the same platform ({arch}) for '{}'",
                crate_name
            );
        }
    }

    if installers.is_empty() {
        anyhow::bail!(
            "winget: no Windows archive or binary artifact found for '{}'",
            crate_name
        );
    }

    Ok(installers)
}

/// Extract a YYYY-MM-DD release date from the template context's `Date`
/// (RFC 3339), returning `None` when the field is missing or malformed.
fn resolve_winget_release_date(ctx: &Context) -> Option<String> {
    ctx.template_vars()
        .get("Date")
        .map(|d| d.chars().take(10).collect::<String>())
        .filter(|s| s.len() == 10 && s.as_bytes()[4] == b'-' && s.as_bytes()[7] == b'-')
}

/// Template-rendered string fields that feed [`WingetManifestParams`].
/// Each field mirrors the same-named winget config entry after running
/// it through the template engine with the standard variable set plus
/// `Changelog` as an extra field.
struct RenderedWingetFields {
    publisher: String,
    publisher_url: Option<String>,
    publisher_support_url: Option<String>,
    privacy_url: Option<String>,
    homepage: Option<String>,
    author: Option<String>,
    copyright: Option<String>,
    copyright_url: Option<String>,
    license: String,
    license_url: Option<String>,
    short_description: String,
    release_notes_url: Option<String>,
    installation_notes: Option<String>,
    path: Option<String>,
    package_name: Option<String>,
    release_notes: Option<String>,
}

/// Template-render all 18 winget config string fields against the live
/// context, injecting `Changelog` as an extra field per render.
fn render_winget_fields(
    ctx: &Context,
    winget_cfg: &anodizer_core::config::WingetConfig,
    crate_name: &str,
    name: &str,
    publisher_name: &str,
    license: &str,
    short_desc: &str,
) -> RenderedWingetFields {
    let release_notes_var = ctx
        .template_vars()
        .get("ReleaseNotes")
        .cloned()
        .unwrap_or_default();
    let render = |s: Option<&str>| -> Option<String> {
        s.map(|v| {
            let mut vars = ctx.template_vars().clone();
            vars.set("Changelog", &release_notes_var);
            anodizer_core::template::render(v, &vars).unwrap_or_else(|_| v.to_string())
        })
    };

    RenderedWingetFields {
        publisher: render(Some(publisher_name)).unwrap_or_else(|| publisher_name.to_string()),
        publisher_url: render(winget_cfg.publisher_url.as_deref()),
        publisher_support_url: render(winget_cfg.publisher_support_url.as_deref()),
        privacy_url: render(winget_cfg.privacy_url.as_deref()),
        homepage: render(
            winget_cfg
                .homepage
                .as_deref()
                .or_else(|| ctx.config.meta_homepage_for(crate_name)),
        ),
        author: render(winget_cfg.author.as_deref()),
        copyright: render(winget_cfg.copyright.as_deref()),
        copyright_url: render(winget_cfg.copyright_url.as_deref()),
        license: render(Some(license)).unwrap_or_else(|| license.to_string()),
        license_url: render(winget_cfg.license_url.as_deref()),
        short_description: render(Some(short_desc))
            .unwrap_or_else(|| short_desc.to_string())
            .replace('\t', "  "),
        release_notes_url: render(winget_cfg.release_notes_url.as_deref()),
        installation_notes: render(winget_cfg.installation_notes.as_deref()),
        path: render(winget_cfg.path.as_deref()),
        package_name: render(winget_cfg.package_name.as_deref()).or_else(|| Some(name.to_string())),
        release_notes: render(winget_cfg.release_notes.as_deref()),
    }
}

/// Compute the on-disk manifest directory inside the cloned winget repo
/// and write the three manifest files. Returns the directory for logging.
fn write_winget_manifests_to_disk(
    repo_path: &std::path::Path,
    package_id: &str,
    version: &str,
    path_rendered: Option<&str>,
    ver_yaml: &str,
    inst_yaml: &str,
    locale_yaml: &str,
) -> Result<std::path::PathBuf> {
    let manifest_dir = if let Some(path) = path_rendered {
        repo_path.join(path)
    } else {
        let first_char = package_id
            .chars()
            .next()
            .unwrap_or('_')
            .to_ascii_lowercase();
        repo_path
            .join("manifests")
            .join(first_char.to_string())
            .join(package_id.replace('.', "/"))
            .join(version)
    };
    std::fs::create_dir_all(&manifest_dir)
        .with_context(|| format!("winget: create manifest dir {}", manifest_dir.display()))?;

    let ver_path = manifest_dir.join(format!("{}.yaml", package_id));
    let inst_path = manifest_dir.join(format!("{}.installer.yaml", package_id));
    let locale_path = manifest_dir.join(format!("{}.locale.en-US.yaml", package_id));

    std::fs::write(&ver_path, ver_yaml)?;
    std::fs::write(&inst_path, inst_yaml)?;
    std::fs::write(&locale_path, locale_yaml)?;

    Ok(manifest_dir)
}

/// Submit (or update) the PR against either a configured `pull_request`
/// upstream or the canonical `microsoft/winget-pkgs` fallback. Returns
/// the optional outcome that must be forwarded to
/// `Context::record_publisher_outcome`.
#[allow(clippy::too_many_arguments)]
#[must_use = "the returned outcome must be forwarded to Context::record_publisher_outcome"]
fn submit_winget_pr(
    repo_path: &std::path::Path,
    repo_for_pr: Option<&anodizer_core::config::RepositoryConfig>,
    repo_owner: &str,
    repo_name: &str,
    branch_name: &str,
    package_id: &str,
    version: &str,
    update_existing_pr: bool,
    log: &StageLogger,
) -> Option<anodizer_core::PublisherOutcome> {
    let has_pr_config = repo_for_pr
        .and_then(|r| r.pull_request.as_ref())
        .and_then(|pr| pr.enabled)
        .unwrap_or(false);

    let title = format!("New version: {} version {}", package_id, version);
    let body = format!(
        "## Package\n- **Package**: {}\n- **Version**: {}\n\nAutomatically submitted by anodizer.",
        package_id, version
    );

    if has_pr_config {
        util::maybe_submit_pr(
            repo_path,
            repo_for_pr,
            &util::PrOrigin {
                repo_owner,
                repo_name,
                branch_name,
                update_existing_pr,
            },
            &title,
            &body,
            "winget",
            log,
        )
    } else {
        let upstream_slug = repo_for_pr
            .and_then(|r| r.pull_request.as_ref())
            .and_then(|pr| pr.base.as_ref())
            .and_then(|base| {
                let owner = base.owner.as_deref()?;
                let name = base.name.as_deref()?;
                Some(format!("{}/{}", owner, name))
            })
            .unwrap_or_else(|| "microsoft/winget-pkgs".to_string());

        util::submit_pr_via_gh_with_opts(
            repo_path,
            &upstream_slug,
            &format!("{}:{}", repo_owner, branch_name),
            &title,
            &body,
            "winget",
            log,
            util::SubmitPrOpts { update_existing_pr },
        )
    }
}

// ---------------------------------------------------------------------------
// publish_to_winget
// ---------------------------------------------------------------------------

pub fn publish_to_winget(ctx: &mut Context, crate_name: &str, log: &StageLogger) -> Result<()> {
    let (_crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "winget")?;

    // Clone the winget config upfront so subsequent helpers do not borrow
    // from `ctx.config`; that frees the later `&mut ctx` call site at the
    // end of the function (`ctx.record_publisher_outcome`).
    let winget_cfg = publish
        .winget
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("winget: no winget config for '{}'", crate_name))?
        .clone();

    let label = format!("winget publisher for crate '{}'", crate_name);
    if crate::util::should_skip_publisher_with_if(
        ctx,
        None,
        winget_cfg.skip_upload.as_ref(),
        winget_cfg.if_condition.as_deref(),
        &label,
        log,
    )? {
        return Ok(());
    }

    let (repo_owner, repo_name) =
        crate::util::resolve_repo_owner_name(winget_cfg.repository.as_ref())
            .ok_or_else(|| anyhow::anyhow!("winget: no repository config for '{}'", crate_name))?;

    let name_raw = winget_cfg.name.as_deref().unwrap_or(crate_name);
    let name_rendered = ctx
        .render_template(name_raw)
        .unwrap_or_else(|_| name_raw.to_string());
    let name = name_rendered.as_str();
    let publisher_name = resolve_winget_publisher_name(&winget_cfg, &repo_owner, crate_name, log)?;

    let auto_pkg_id = format!("{}.{}", publisher_name.replace(' ', ""), name);
    let package_id = winget_cfg
        .package_identifier
        .as_deref()
        .unwrap_or(&auto_pkg_id);

    validate_package_identifier(package_id)?;

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would submit WinGet manifest for '{}' (pkg={}) to {}/{}",
            crate_name, package_id, repo_owner, repo_name
        ));
        return Ok(());
    }

    generate_and_submit_winget_manifest(
        ctx,
        log,
        crate_name,
        &winget_cfg,
        name,
        publisher_name,
        package_id,
        &repo_owner,
        &repo_name,
    )
}

/// Generate WinGet YAML manifests, clone the package repo, commit, push,
/// and open a PR.
#[allow(clippy::too_many_arguments)]
fn generate_and_submit_winget_manifest(
    ctx: &mut Context,
    log: &StageLogger,
    crate_name: &str,
    winget_cfg: &anodizer_core::config::WingetConfig,
    name: &str,
    publisher_name: &str,
    package_id: &str,
    repo_owner: &str,
    repo_name: &str,
) -> Result<()> {
    let version = ctx.version();
    let description = resolve_winget_description(ctx, winget_cfg, crate_name);
    let short_desc = resolve_winget_short_description(ctx, winget_cfg, crate_name)?;
    let license = resolve_winget_license(ctx, winget_cfg, crate_name)?;

    let installers = collect_winget_installers(ctx, crate_name, winget_cfg, name, &version)?;

    let deps = winget_cfg.dependencies.as_deref().unwrap_or(&[]);
    let release_date = resolve_winget_release_date(ctx);
    let release_date_ref = release_date.as_deref();

    let rendered = render_winget_fields(
        ctx,
        winget_cfg,
        crate_name,
        name,
        publisher_name,
        license,
        &short_desc,
    );

    let (ver_yaml, inst_yaml, locale_yaml) = generate_manifests(&WingetManifestParams {
        package_id,
        name,
        package_name: rendered.package_name.as_deref(),
        version: &version,
        description: &description,
        short_description: &rendered.short_description,
        license: &rendered.license,
        license_url: rendered.license_url.as_deref(),
        publisher: &rendered.publisher,
        publisher_url: rendered.publisher_url.as_deref(),
        publisher_support_url: rendered.publisher_support_url.as_deref(),
        privacy_url: rendered.privacy_url.as_deref(),
        author: rendered.author.as_deref(),
        copyright: rendered.copyright.as_deref(),
        copyright_url: rendered.copyright_url.as_deref(),
        homepage: rendered.homepage.as_deref(),
        release_notes: rendered.release_notes.as_deref(),
        release_notes_url: rendered.release_notes_url.as_deref(),
        installation_notes: rendered.installation_notes.as_deref(),
        tags: winget_cfg.tags.as_deref(),
        dependencies: deps,
        installers,
        product_code: winget_cfg.product_code.as_deref(),
        release_date: release_date_ref,
    })?;

    let token = util::resolve_repo_token(
        ctx,
        winget_cfg.repository.as_ref(),
        Some("WINGET_PKGS_TOKEN"),
    );

    let tmp_dir = tempfile::tempdir().context("winget: create temp dir")?;
    let repo_path = tmp_dir.path();
    util::clone_repo(
        winget_cfg.repository.as_ref(),
        repo_owner,
        repo_name,
        token.as_deref(),
        repo_path,
        "winget",
        log,
    )?;

    let manifest_dir = write_winget_manifests_to_disk(
        repo_path,
        package_id,
        &version,
        rendered.path.as_deref(),
        &ver_yaml,
        &inst_yaml,
        &locale_yaml,
    )?;

    log.status(&format!(
        "wrote WinGet manifests to {}",
        manifest_dir.display()
    ));

    let commit_msg = render_winget_commit_msg(
        winget_cfg.commit_msg_template.as_deref(),
        package_id,
        &version,
    );

    let auto_branch = format!("{}-{}", package_id, version);
    let branch_name = util::resolve_branch(winget_cfg.repository.as_ref()).unwrap_or(&auto_branch);
    let commit_opts = util::resolve_commit_opts(ctx, winget_cfg.commit_author.as_ref());
    let outcome = util::commit_and_push_with_opts(
        repo_path,
        &["."],
        &commit_msg,
        Some(branch_name),
        "winget",
        &commit_opts,
    )?;
    match outcome {
        util::CommitOutcome::Pushed => {
            log.status(&format!(
                "WinGet manifest pushed to {}/{} branch '{}'",
                repo_owner, repo_name, branch_name
            ));
        }
        util::CommitOutcome::NoChanges => {
            log.status(&format!(
                "winget: nothing to push, manifest for '{}' already up to date",
                package_id
            ));
        }
    }

    let update_existing_pr = winget_cfg
        .update_existing_pr
        .as_ref()
        .map(|v| {
            v.try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                .unwrap_or(false)
        })
        .unwrap_or(false);

    let pr_outcome = submit_winget_pr(
        repo_path,
        winget_cfg.repository.as_ref(),
        repo_owner,
        repo_name,
        branch_name,
        package_id,
        &version,
        update_existing_pr,
        log,
    );

    if let Some(outcome) = pr_outcome {
        ctx.record_publisher_outcome(outcome);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// WingetPublisher — Publisher trait wrapper (Submitter group)
// ---------------------------------------------------------------------------
//
// WinGet is structurally a Submitter publisher: each successful per-crate
// publish opens a PR against `microsoft/winget-pkgs` (or the upstream the
// `repository.pull_request.base` override names). That PR then goes
// through *automated validation* + *manual maintainer review*. Auto-closing
// a PR mid-validation is unreliable — the validation pipeline interacts
// with PR state in ways that can interfere with `gh pr close` — so unlike
// the krew publisher we do NOT close the PR programmatically on
// rollback. Instead, the rollback path warns per recorded target with
// the upstream coordinates and the operator's fork branch so a human
// can close the PR via the GitHub UI.
//
// CREDENTIAL HANDLING: [`WingetTarget`] stores no auth material. The
// GitHub token feeding the publish path (resolved through
// `repository.git.access_token` / `ANODIZER_GITHUB_TOKEN` /
// `GITHUB_TOKEN`) is irrelevant to a warn-only rollback — we only name
// the env var operators are expected to have set if they want to
// re-run publish, not the resolved value.

// Submitter-group `Publisher` for winget. Wraps the existing per-crate
// `publish_to_winget` entrypoint. Rollback is warn-only — winget PRs
// require manual operator action against `microsoft/winget-pkgs`
// (or the configured `repository.pull_request.base` upstream).
simple_publisher!(
    WingetPublisher,
    "winget",
    anodizer_core::PublisherGroup::Submitter,
    false,
    Some("GITHUB_TOKEN pull_request:write"),
);

/// Aliased to the core-owned snapshot so the evidence schema lives in
/// [`anodizer_core::publish_evidence`] and credential-shaped fields
/// have no slot to land in. See the Submitter rustdoc above for the
/// credential-handling rationale.
type WingetTarget = anodizer_core::publish_evidence::WingetTargetSnapshot;

/// Decode the `winget_targets` array from
/// [`anodizer_core::PublishEvidence::extra`].
fn decode_winget_targets(extra: &anodizer_core::PublishEvidenceExtra) -> Vec<WingetTarget> {
    match extra {
        anodizer_core::PublishEvidenceExtra::Winget(w) => w.winget_targets.clone(),
        _ => Vec::new(),
    }
}

/// Resolve the upstream `<owner>/<repo>` slug for a winget target —
/// mirrors the dispatch logic in `publish_to_winget`: prefer
/// `repository.pull_request.base` when set, else fall back to the
/// canonical `microsoft/winget-pkgs`.
fn resolve_winget_upstream(winget_cfg: &anodizer_core::config::WingetConfig) -> (String, String) {
    if let Some(base) = winget_cfg
        .repository
        .as_ref()
        .and_then(|r| r.pull_request.as_ref())
        .and_then(|pr| pr.base.as_ref())
        && let (Some(o), Some(n)) = (base.owner.as_deref(), base.name.as_deref())
    {
        return (o.to_string(), n.to_string());
    }
    ("microsoft".to_string(), "winget-pkgs".to_string())
}

/// True when the crate has a `publish.winget` block — mirrors the
/// `per_crate!` predicate in `lib.rs`.
fn is_winget_per_crate_configured(ctx: &Context, crate_name: &str) -> bool {
    crate::util::all_crates(ctx)
        .into_iter()
        .any(|c| c.name == crate_name && c.publish.as_ref().is_some_and(|p| p.winget.is_some()))
}

/// Build a [`WingetTarget`] for the given crate. Reads config + the
/// live process version so the recorded coordinates match what
/// `publish_to_winget` will push. Returns `None` when no winget block
/// is configured or when the publisher / repo resolution would itself
/// no-op (matches the publish path's skip semantics).
fn collect_winget_target(ctx: &Context, crate_name: &str) -> Option<WingetTarget> {
    let c = ctx.config.crates.iter().find(|c| c.name == crate_name)?;
    let cfg = c.publish.as_ref().and_then(|p| p.winget.as_ref())?;
    let (repo_owner, _repo_name) = crate::util::resolve_repo_owner_name(cfg.repository.as_ref())?;
    let fork_owner = ctx
        .render_template(&repo_owner)
        .unwrap_or_else(|_| repo_owner.clone());

    let name_raw = cfg.name.as_deref().unwrap_or(crate_name);
    let name_rendered = ctx
        .render_template(name_raw)
        .unwrap_or_else(|_| name_raw.to_string());

    let publisher_name = match cfg.publisher.as_deref() {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => fork_owner.clone(),
    };

    let auto_pkg_id = format!("{}.{}", publisher_name.replace(' ', ""), name_rendered);
    let package_id = cfg
        .package_identifier
        .as_deref()
        .map(|s| s.to_string())
        .unwrap_or(auto_pkg_id);

    let version = ctx.version();
    let auto_branch = format!("{}-{}", package_id, version);
    let branch = crate::util::resolve_branch(cfg.repository.as_ref())
        .map(|b| b.to_string())
        .unwrap_or(auto_branch);

    let (upstream_owner, upstream_repo) = resolve_winget_upstream(cfg);

    Some(WingetTarget {
        target: package_id.clone(),
        crate_name: crate_name.to_string(),
        package_id,
        version,
        upstream_owner,
        upstream_repo,
        fork_owner,
        branch,
    })
}

/// Message emitted at publisher entry. Names how many crates the publisher
/// is iterating over. Factored into a helper so tests can pin the exact
/// substring an operator scans the log for ("winget: starting publish
/// for ...").
pub(crate) fn run_start_message(selected_total: usize) -> String {
    format!(
        "winget: starting publish for {} selected crate(s)",
        selected_total
    )
}

/// Message emitted when a selected crate has no `publish.winget` block.
/// Replaces what used to be a silent `continue` — operators need to see
/// why a per-crate publish was a no-op rather than guess from a blank
/// log.
pub(crate) fn run_skip_unconfigured_message(crate_name: &str) -> String {
    format!(
        "winget: skipping crate '{}' — no winget config block",
        crate_name
    )
}

/// Message emitted just before delegating to `publish_to_winget`.
/// Anchors the winget activity (manifest generation, fork clone, push,
/// PR submission) to a specific crate in the log so multi-crate
/// workspaces are disambiguatable.
pub(crate) fn run_per_crate_start_message(crate_name: &str) -> String {
    format!("winget: starting per-crate publish for '{}'", crate_name)
}

/// Final summary emitted at publisher exit. `processed` is the count of
/// crates the publisher actually invoked `publish_to_winget` on (not
/// the count of successful PRs — `publish_to_winget` has its own skip
/// paths for skip_upload/dry-run/etc., each of which logs its own status
/// line, and the gh CLI submission helper logs its own success/warn).
pub(crate) fn run_done_message(processed: usize) -> String {
    format!("winget: completed — {} crate(s) processed", processed)
}

/// Warning emitted when the publisher was registered (at least one
/// crate has a `publish.winget` block at the config level) but the
/// run path processed zero crates.
///
/// With the implicit-all default in
/// [`crate::publisher_helpers::effective_publish_crates`], an empty
/// `selected_crates` resolves to every crate carrying a
/// `publish.winget` block — so a zero-processed run means `--crate` /
/// `--all` matrix selection was non-empty AND filtered every
/// winget-configured crate out. Operators must see this — otherwise the
/// publisher's `succeeded` status hides the fact that nothing was
/// pushed.
pub(crate) fn run_no_eligible_crates_warning(selected_total: usize) -> String {
    format!(
        "winget: registered but 0 of {} effective crate(s) had a winget \
         config block — nothing pushed. Check that --crate / --all selects a \
         crate whose publish.winget block is set.",
        selected_total
    )
}

impl anodizer_core::Publisher for WingetPublisher {
    fn name(&self) -> &str {
        Self::PUBLISHER_NAME
    }
    fn group(&self) -> anodizer_core::PublisherGroup {
        Self::PUBLISHER_GROUP
    }
    fn required(&self) -> bool {
        Self::resolved_required(self)
    }
    fn rollback_scope_needed(&self) -> Option<&'static str> {
        Self::ROLLBACK_SCOPE
    }

    fn skips_on_nightly(&self) -> bool {
        true
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        let log = ctx.logger("publish");
        let mut targets: Vec<WingetTarget> = Vec::new();
        let selected =
            crate::publisher_helpers::effective_publish_crates(ctx, is_winget_per_crate_configured);
        log.status(&run_start_message(selected.len()));
        for crate_name in &selected {
            // Defensive guard for explicit `--crate=X` selection when X has no
            // publisher block; implicit-all is already filtered by effective_publish_crates above.
            if !is_winget_per_crate_configured(ctx, crate_name) {
                log.status(&run_skip_unconfigured_message(crate_name));
                continue;
            }
            // Snapshot the target shape BEFORE the publish path runs so
            // a mid-publish failure still leaves the operator a manual
            // PR-close pointer.
            if let Some(t) = collect_winget_target(ctx, crate_name) {
                targets.push(t);
            }
            log.status(&run_per_crate_start_message(crate_name));
            publish_to_winget(ctx, crate_name, &log)?;
        }
        let processed = targets.len();
        if processed == 0 {
            log.warn(&run_no_eligible_crates_warning(selected.len()));
        } else {
            log.status(&run_done_message(processed));
        }
        let mut evidence = anodizer_core::PublishEvidence::new("winget");
        if let Some(first) = targets.first() {
            evidence.primary_ref = Some(format!(
                "https://github.com/{}/{}/pulls?q=head%3A{}%3A{}",
                first.upstream_owner, first.upstream_repo, first.fork_owner, first.branch
            ));
        }
        evidence.extra = anodizer_core::PublishEvidenceExtra::Winget(
            anodizer_core::publish_evidence::WingetExtra {
                winget_targets: targets,
            },
        );
        Ok(evidence)
    }

    fn rollback(
        &self,
        ctx: &mut Context,
        evidence: &anodizer_core::PublishEvidence,
    ) -> anyhow::Result<()> {
        let log = ctx.logger("publish");
        let targets = decode_winget_targets(&evidence.extra);
        if targets.is_empty() {
            log.warn(&crate::publisher_helpers::rollback_empty_warning_msg(
                "winget",
                "submitted PR targets",
            ));
            return Ok(());
        }
        // WinGet PRs go through automated validation; auto-close
        // mid-validation is unreliable. Surface a warn per recorded
        // target with the fork-branch query so the operator can find
        // and close the PR manually.
        for t in &targets {
            log.warn(&format!(
                "winget: manual PR closure required for '{}' version '{}'; \
                 visit https://github.com/{}/{}/pulls?q=is%3Apr+head%3A{}%3A{} \
                 and close the PR (winget validation cannot be reliably \
                 cancelled programmatically mid-flight)",
                t.package_id, t.version, t.upstream_owner, t.upstream_repo, t.fork_owner, t.branch
            ));
        }
        log.status(&format!(
            "winget: {} PR(s) require manual closure",
            targets.len()
        ));
        Ok(())
    }

    fn preflight(&self, _ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        Ok(anodizer_core::PreflightCheck::Pass)
    }
}

#[cfg(test)]
mod publisher_tests {
    use super::*;
    use anodizer_core::config::{CrateConfig, PublishConfig, RepositoryConfig, WingetConfig};
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::{PreflightCheck, PublishEvidence, Publisher, PublisherGroup};

    fn winget_crate(crate_name: &str) -> CrateConfig {
        CrateConfig {
            name: crate_name.to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
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

    #[test]
    fn winget_preflight_defaults_to_pass() {
        let ctx = TestContextBuilder::new().build();
        let p = WingetPublisher::new();
        assert!(matches!(
            p.preflight(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        ));
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
        evidence.extra = anodizer_core::PublishEvidenceExtra::Winget(
            anodizer_core::publish_evidence::WingetExtra {
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
            },
        );
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
        let extra = anodizer_core::PublishEvidenceExtra::Winget(
            anodizer_core::publish_evidence::WingetExtra {
                winget_targets: original.clone(),
            },
        );
        let decoded = decode_winget_targets(&extra);
        assert_eq!(decoded, original);
    }

    #[test]
    fn winget_target_extra_carries_no_secret_material() {
        // Structural pin: build a typed-variant evidence and assert
        // (a) no credential-shaped keys appear AND (b) the
        // operator-public PR coordinates are preserved.
        let mut e = PublishEvidence::new("winget");
        e.extra = anodizer_core::PublishEvidenceExtra::Winget(
            anodizer_core::publish_evidence::WingetExtra {
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
            },
        );
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
        let t = collect_winget_target(&ctx, "demo").expect("target");
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
        let t = collect_winget_target(&ctx, "demo").expect("target");
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
    fn run_start_message_names_selected_total() {
        let msg = run_start_message(3);
        assert!(msg.starts_with("winget:"), "{msg}");
        assert!(msg.contains("starting publish"), "{msg}");
        assert!(msg.contains("3 selected"), "{msg}");
    }

    #[test]
    fn run_skip_unconfigured_message_names_crate() {
        let msg = run_skip_unconfigured_message("demo");
        assert!(msg.starts_with("winget:"), "{msg}");
        assert!(msg.contains("skipping crate 'demo'"), "{msg}");
        assert!(msg.contains("no winget config block"), "{msg}");
    }

    #[test]
    fn run_per_crate_start_message_names_crate() {
        let msg = run_per_crate_start_message("demo");
        assert!(msg.starts_with("winget:"), "{msg}");
        assert!(msg.contains("starting per-crate publish"), "{msg}");
        assert!(msg.contains("'demo'"), "{msg}");
    }

    #[test]
    fn run_done_message_reports_processed_count() {
        let msg = run_done_message(2);
        assert!(msg.starts_with("winget:"), "{msg}");
        assert!(msg.contains("completed"), "{msg}");
        assert!(msg.contains("2 crate(s) processed"), "{msg}");
    }

    #[test]
    fn run_no_eligible_crates_warning_names_remediation() {
        let msg = run_no_eligible_crates_warning(5);
        assert!(msg.starts_with("winget:"), "{msg}");
        assert!(msg.contains("registered"), "{msg}");
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
        assert!(msg.starts_with("winget:"), "{msg}");
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
        let mut ctx = TestContextBuilder::new()
            .crates(vec![winget_crate("demo")])
            .selected_crates(vec!["demo".to_string()])
            .dry_run(true)
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
                    tag_template: "v{{ .Version }}".to_string(),
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
        let mut ctx = TestContextBuilder::new()
            .crates(vec![winget_crate("demo")])
            // selected_crates intentionally left at the default Vec::new()
            .dry_run(true)
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
                tag_template: "v{{ .Version }}".to_string(),
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
        let mut ctx = TestContextBuilder::new()
            .crates(vec![winget_crate("demo")])
            .selected_crates(vec!["demo".to_string()])
            .dry_run(true)
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
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
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
            }],
            product_code: None,
            release_date: None,
        }
    }

    #[test]
    fn test_generate_manifest_basic() {
        let manifest = generate_manifest(&default_params()).unwrap();
        assert!(manifest.contains("PackageIdentifier: Org.MyTool"));
        assert!(manifest.contains("PackageVersion: 1.0.0"));
        assert!(manifest.contains("PackageName: mytool"));
        assert!(manifest.contains("Publisher: My Org"));
        assert!(manifest.contains("PublisherUrl: https://example.com"));
        assert!(manifest.contains("License: MIT"));
        assert!(manifest.contains("ShortDescription: A great tool"));
        assert!(manifest.contains("Installers:"));
        assert!(manifest.contains("Architecture: x64"));
        assert!(
            manifest.contains("InstallerUrl: https://example.com/mytool-1.0.0-windows-amd64.zip")
        );
        assert!(manifest.contains("InstallerSha256: deadbeef1234567890abcdef"));
        assert!(manifest.contains("ManifestType: singleton"));
        assert!(manifest.contains("ManifestVersion: 1.12.0"));
    }

    #[test]
    fn test_generate_manifest_no_publisher_url() {
        let mut params = default_params();
        params.publisher_url = None;
        let manifest = generate_manifest(&params).unwrap();
        assert!(!manifest.contains("PublisherUrl:"));
        assert!(manifest.contains("Publisher: My Org"));
    }

    #[test]
    fn test_generate_3file_manifests() {
        let params = default_params();
        let (ver, inst, locale) = generate_manifests(&params).unwrap();

        assert!(ver.contains("ManifestType: version"));
        assert!(ver.contains("PackageIdentifier: Org.MyTool"));

        assert!(inst.contains("ManifestType: installer"));
        assert!(inst.contains("InstallerSha256: deadbeef1234567890abcdef"));
        assert!(inst.contains("UpgradeBehavior: uninstallPrevious"));
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
        }];
        let mut params = default_params();
        params.dependencies = &deps;
        let (_, inst, _) = generate_manifests(&params).unwrap();
        assert!(inst.contains("PackageDependencies:"));
        assert!(inst.contains("PackageIdentifier: Foo.Bar"));
        assert!(inst.contains("MinimumVersion: 1.0.0"));
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
            tag_template: "v{{ .Version }}".to_string(),
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
            }],
            product_code: Some("{12345678-1234-1234-1234-123456789012}"),
            release_date: Some("2026-03-29"),
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
        assert!(
            locale.contains("ReleaseNotesUrl: https://github.com/myorg/mytool/releases/v2.5.0")
        );
        assert!(locale.contains("InstallationNotes: Run 'mytool --help' to get started"));
        assert!(locale.contains("cli"));
        assert!(locale.contains("devops"));
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
            }],
            product_code: None,
            release_date: None,
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
            }],
            product_code: None,
            release_date: None,
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
            }],
            product_code: None,
            release_date: None,
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

    #[test]
    fn test_winget_commit_msg_default() {
        let msg = render_winget_commit_msg(None, "Org.MyTool", "1.0.0");
        assert_eq!(msg, "New version: Org.MyTool 1.0.0");
    }

    #[test]
    fn test_winget_commit_msg_with_package_identifier_template() {
        // PackageIdentifier is exposed in the template context
        let msg = render_winget_commit_msg(
            Some("winget: {{ PackageIdentifier }} v{{ version }}"),
            "Org.MyTool",
            "2.0.0",
        );
        assert_eq!(msg, "winget: Org.MyTool v2.0.0");
    }

    #[test]
    fn test_winget_commit_msg_custom() {
        let msg = render_winget_commit_msg(
            Some("release: {{ name }} {{ version }}"),
            "Org.MyTool",
            "3.0.0",
        );
        assert_eq!(msg, "release: Org.MyTool 3.0.0");
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
                format!(
                    "[package]\nname = \"{name}\"\ndescription = \"{desc}\"\nlicense = \"MIT\"\n"
                ),
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
}
