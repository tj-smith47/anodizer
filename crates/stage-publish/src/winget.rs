use anodize_core::context::Context;
use anodize_core::log::StageLogger;
use anyhow::{Context as _, Result};
use serde::Serialize;

use crate::util;

// ---------------------------------------------------------------------------
// PackageIdentifier validation
// ---------------------------------------------------------------------------

/// Validate a WinGet PackageIdentifier against the required pattern.
///
/// The identifier must have at least 2 dot-separated segments, and each
/// segment must not contain whitespace or the characters `\`, `/`, `:`, `*`,
/// `?`, `"`, `<`, `>`, `|`.
///
/// Pattern: `^[^\.\s\\\/:\*\?"<>\|]+(\.[^\.\s\\\/:\*\?"<>\|]+){1,7}$`
pub fn validate_package_identifier(id: &str) -> Result<()> {
    let re = regex::Regex::new(r#"^[^\.\s\\/:\*\?"<>\|]+(\.[^\.\s\\/:\*\?"<>\|]+){1,7}$"#)
        .expect("winget: compile PackageIdentifier regex");

    if re.is_match(id) {
        Ok(())
    } else {
        anyhow::bail!(
            "winget: invalid PackageIdentifier '{}'. Must have 2-8 dot-separated segments \
             with no whitespace or special characters (\\/:*?\"<>|).",
            id
        )
    }
}

// ---------------------------------------------------------------------------
// Winget commit message rendering
// ---------------------------------------------------------------------------

/// Render a commit message for WinGet with PackageIdentifier in the context.
/// GoReleaser exposes `PackageIdentifier` as an extra template field
/// (winget.go:291-293).
fn render_winget_commit_msg(
    template: Option<&str>,
    package_id: &str,
    version: &str,
) -> String {
    let default_tmpl = "chore: update {{ name }} manifest to {{ version }}";
    let tmpl = template.unwrap_or(default_tmpl);

    let mut tera = tera::Tera::default();
    tera.autoescape_on(vec![]);
    if tera.add_raw_template("msg", tmpl).is_err() {
        return format!("chore: update {} manifest to {}", package_id, version);
    }
    let mut ctx = tera::Context::new();
    ctx.insert("name", package_id);
    ctx.insert("version", version);
    ctx.insert("PackageIdentifier", package_id);
    tera.render("msg", &ctx)
        .unwrap_or_else(|_| format!("chore: update {} manifest to {}", package_id, version))
}

// ---------------------------------------------------------------------------
// WingetManifestParams
// ---------------------------------------------------------------------------

/// Parameters for generating WinGet YAML manifests.
pub struct WingetManifestParams<'a> {
    pub package_id: &'a str,
    pub name: &'a str,
    /// Display name for the package. Falls back to `name` when not set.
    pub package_name: Option<&'a str>,
    pub version: &'a str,
    pub description: &'a str,
    pub short_description: &'a str,
    pub license: &'a str,
    pub license_url: Option<&'a str>,
    pub publisher: &'a str,
    pub publisher_url: Option<&'a str>,
    pub publisher_support_url: Option<&'a str>,
    pub privacy_url: Option<&'a str>,
    pub author: Option<&'a str>,
    pub copyright: Option<&'a str>,
    pub copyright_url: Option<&'a str>,
    pub homepage: Option<&'a str>,
    pub release_notes: Option<&'a str>,
    pub release_notes_url: Option<&'a str>,
    pub installation_notes: Option<&'a str>,
    pub tags: Option<&'a [String]>,
    pub dependencies: &'a [anodize_core::config::WingetDependency],
    pub installers: Vec<WingetInstallerItem>,
    /// Product code for the installer (used in Add/Remove Programs).
    pub product_code: Option<&'a str>,
    /// Release date in YYYY-MM-DD format.
    pub release_date: Option<&'a str>,
}

/// A single installer entry in the WinGet manifest.
pub struct WingetInstallerItem {
    pub architecture: String,
    pub url: String,
    pub sha256: String,
    /// Installer type: "zip" for archive artifacts, "portable" for bare binaries.
    pub installer_type: String,
    /// Binary names contained in this archive.  When multiple binaries are
    /// present, each gets its own `NestedInstallerFile` entry.
    pub binaries: Vec<String>,
    /// When the archive wraps contents in a top-level directory, this holds that
    /// directory name.  `RelativeFilePath` entries will be prefixed with it.
    pub wrap_in_directory: Option<String>,
    /// Commands for portable binaries (the binary filename without extension).
    pub commands: Vec<String>,
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
    /// Commands for portable binaries (GoReleaser parity: winget.go:477).
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
pub fn generate_manifest(params: &WingetManifestParams<'_>) -> String {
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
    serde_yaml_ng::to_string(&manifest).expect("winget: serialize manifest")
}

/// Generate the 3-file WinGet manifest set: (version, installer, locale).
pub fn generate_manifests(params: &WingetManifestParams<'_>) -> (String, String, String) {
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
    // (GoReleaser parity: winget.go:477 sets Commands on the top-level Installer struct).
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

    const GENERATED_HEADER: &str = "# This file was generated by anodize. DO NOT EDIT.\n";
    const SCHEMA_VERSION: &str = "# yaml-language-server: $schema=https://aka.ms/winget-manifest.version.1.12.0.schema.json\n";
    const SCHEMA_INSTALLER: &str = "# yaml-language-server: $schema=https://aka.ms/winget-manifest.installer.1.12.0.schema.json\n";
    const SCHEMA_LOCALE: &str = "# yaml-language-server: $schema=https://aka.ms/winget-manifest.defaultLocale.1.12.0.schema.json\n";

    (
        format!(
            "{}{}{}",
            GENERATED_HEADER,
            SCHEMA_VERSION,
            serde_yaml_ng::to_string(&version).expect("winget: serialize version manifest")
        ),
        format!(
            "{}{}{}",
            GENERATED_HEADER,
            SCHEMA_INSTALLER,
            serde_yaml_ng::to_string(&installer).expect("winget: serialize installer manifest")
        ),
        format!(
            "{}{}{}",
            GENERATED_HEADER,
            SCHEMA_LOCALE,
            serde_yaml_ng::to_string(&locale).expect("winget: serialize locale manifest")
        ),
    )
}

// ---------------------------------------------------------------------------
// publish_to_winget
// ---------------------------------------------------------------------------

pub fn publish_to_winget(ctx: &Context, crate_name: &str, log: &StageLogger) -> Result<()> {
    let (_crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "winget")?;

    let winget_cfg = publish
        .winget
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("winget: no winget config for '{}'", crate_name))?;

    // Check skip_upload before doing any work.
    if crate::homebrew::should_skip_upload(winget_cfg.skip_upload.as_ref(), ctx) {
        log.status(&format!(
            "winget: skipping upload for '{}' (skip_upload={})",
            crate_name,
            winget_cfg.skip_upload.as_ref().map(|v| v.as_str()).unwrap_or("")
        ));
        return Ok(());
    }

    // Resolve repository config: prefer `repository` over legacy `manifests_repo`.
    let (repo_owner, repo_name) = crate::util::resolve_repo_owner_name(
        winget_cfg.repository.as_ref(),
        winget_cfg.manifests_repo.as_ref().map(|r| r.owner.as_str()),
        winget_cfg.manifests_repo.as_ref().map(|r| r.name.as_str()),
    )
    .ok_or_else(|| {
        anyhow::anyhow!(
            "winget: no repository/manifests_repo config for '{}'",
            crate_name
        )
    })?;

    let name_raw = winget_cfg.name.as_deref().unwrap_or(crate_name);
    let name_rendered = ctx
        .render_template(name_raw)
        .unwrap_or_else(|_| name_raw.to_string());
    let name = name_rendered.as_str();
    let publisher_name = match winget_cfg.publisher.as_deref() {
        Some(p) if !p.is_empty() => p,
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
            repo_owner.as_str()
        }
    };

    // Auto-generate package_identifier if not provided: Publisher.Name
    let auto_pkg_id = format!("{}.{}", publisher_name.replace(' ', ""), name);
    let package_id = winget_cfg
        .package_identifier
        .as_deref()
        .unwrap_or(&auto_pkg_id);

    // Validate PackageIdentifier format before proceeding.
    validate_package_identifier(package_id)?;

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would submit WinGet manifest for '{}' (pkg={}) to {}/{}",
            crate_name, package_id, repo_owner, repo_name
        ));
        return Ok(());
    }

    let version = ctx.version();
    // Replace tabs in descriptions with two spaces (WinGet YAML convention).
    let description_raw_cfg = winget_cfg.description.as_deref().unwrap_or("");
    let description_tmpl = ctx
        .render_template(description_raw_cfg)
        .unwrap_or_else(|_| description_raw_cfg.to_string());
    let description = description_tmpl.replace('\t', "  ");
    let description = description.as_str();
    let short_desc_raw = winget_cfg
        .short_description
        .as_deref()
        .or(winget_cfg.description.as_deref())
        .unwrap_or(crate_name);
    let short_desc = short_desc_raw.replace('\t', "  ");
    let short_desc = short_desc.as_str();
    let license = winget_cfg.license.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "winget: license is required but not configured for '{}'. \
             Set `publish.winget.license` in your config.",
            crate_name
        )
    })?;

    // Find windows Archive artifacts for this crate with IDs + goamd64 filtering.
    let ids_filter = winget_cfg.ids.as_deref();
    let url_template = winget_cfg.url_template.as_deref();
    let goamd64 = winget_cfg.goamd64.as_deref().or(Some("v1"));

    let artifact_kind = util::resolve_artifact_kind(winget_cfg.use_artifact.as_deref());

    // Collect binary names from Binary build artifacts for this crate, keyed
    // by target triple.  Used to populate NestedInstallerFiles in each archive.
    let binary_names_by_target: std::collections::HashMap<String, Vec<String>> = {
        let mut map: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        let win_binaries = ctx
            .artifacts
            .by_kind_and_crate(anodize_core::artifact::ArtifactKind::Binary, crate_name);
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
    };

    // Collect both archive (.zip only) and portable binary artifacts.
    // GoReleaser parity: winget.go:187 filters ByFormats("zip") for archives,
    // plus ByType(UploadableBinary) for portable binaries.
    let archive_artifacts = ctx.artifacts.by_kind_and_crate(artifact_kind, crate_name);
    let binary_artifacts = ctx
        .artifacts
        .by_kind_and_crate(anodize_core::artifact::ArtifactKind::Binary, crate_name);

    let is_windows = |a: &anodize_core::artifact::Artifact| -> bool {
        a.target
            .as_deref()
            .map(|t| t.to_ascii_lowercase().contains("windows"))
            .unwrap_or(false)
            || a.path
                .to_string_lossy()
                .to_ascii_lowercase()
                .contains("windows")
    };
    let matches_ids = |a: &anodize_core::artifact::Artifact| -> bool {
        if let Some(ids) = ids_filter {
            a.metadata
                .get("id")
                .map(|id| ids.iter().any(|i| i == id))
                .unwrap_or(false)
        } else {
            true
        }
    };
    let matches_goamd64 = |a: &anodize_core::artifact::Artifact| -> bool {
        let target = a.target.as_deref().unwrap_or("");
        let (_, arch) = anodize_core::target::map_target(target);
        if arch == "amd64" {
            if let Some(want) = goamd64 {
                return a.metadata.get("goamd64").map_or(true, |v| v == want);
            }
        }
        true
    };

    let mut installers: Vec<WingetInstallerItem> = Vec::new();
    let mut zip_count = 0u32;
    let mut binary_count = 0u32;

    // Archive artifacts: filter to .zip only (GoReleaser parity: winget.go:467)
    for a in archive_artifacts.iter() {
        if !is_windows(a) || !matches_ids(a) || !matches_goamd64(a) {
            continue;
        }
        let format = a.metadata.get("format").map(|f| f.as_str()).unwrap_or("");
        if format != "zip" && !a.path.to_string_lossy().ends_with(".zip") {
            continue; // Reject non-zip archives (tar.gz, 7z, etc.)
        }
        zip_count += 1;

        let target = a.target.as_deref().unwrap_or("");
        let (_, raw_arch) = anodize_core::target::map_target(target);
        let arch = match raw_arch.as_str() {
            "amd64" => "x64",
            "386" | "i686" => "x86",
            "arm64" => "arm64",
            other => other,
        };
        let resolved_url = if let Some(tmpl) = url_template {
            util::render_url_template(tmpl, name, &version, &raw_arch, "windows")
        } else {
            a.metadata
                .get("url")
                .cloned()
                .unwrap_or_else(|| a.path.to_string_lossy().into_owned())
        };
        let sha256 = a.metadata.get("sha256").cloned().unwrap_or_default();
        let wrap_in_directory = a.metadata.get("wrap_in_directory").cloned();
        let binaries = binary_names_by_target
            .get(target)
            .cloned()
            .unwrap_or_default();
        installers.push(WingetInstallerItem {
            architecture: arch.to_string(),
            url: resolved_url,
            sha256,
            installer_type: "zip".to_string(),
            binaries,
            wrap_in_directory,
            commands: Vec::new(),
        });
    }

    // Portable binary artifacts (GoReleaser parity: winget.go:475)
    for a in binary_artifacts.iter() {
        if !is_windows(a) || !matches_ids(a) || !matches_goamd64(a) {
            continue;
        }
        binary_count += 1;

        let target = a.target.as_deref().unwrap_or("");
        let (_, raw_arch) = anodize_core::target::map_target(target);
        let arch = match raw_arch.as_str() {
            "amd64" => "x64",
            "386" | "i686" => "x86",
            "arm64" => "arm64",
            other => other,
        };
        let resolved_url = if let Some(tmpl) = url_template {
            util::render_url_template(tmpl, name, &version, &raw_arch, "windows")
        } else {
            a.metadata
                .get("url")
                .cloned()
                .unwrap_or_else(|| a.path.to_string_lossy().into_owned())
        };
        let sha256 = a.metadata.get("sha256").cloned().unwrap_or_default();
        let cmd = a
            .metadata
            .get("binary")
            .cloned()
            .unwrap_or_else(|| name.to_string());
        installers.push(WingetInstallerItem {
            architecture: arch.to_string(),
            url: resolved_url,
            sha256,
            installer_type: "portable".to_string(),
            binaries: Vec::new(),
            wrap_in_directory: None,
            commands: vec![cmd],
        });
    }

    // Validation: mixed formats (GoReleaser parity: winget.go:488-489)
    if binary_count > 0 && zip_count > 0 {
        anyhow::bail!(
            "winget: found archives with multiple formats (.exe and .zip) for '{}'; \
             use either portable binaries or zip archives, not both",
            crate_name
        );
    }

    // Validation: duplicate architectures (GoReleaser parity: winget.go:492-493)
    {
        let mut arch_counts: std::collections::HashMap<&str, u32> =
            std::collections::HashMap::new();
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
    }

    if installers.is_empty() {
        anyhow::bail!(
            "winget: no Windows archive or binary artifact found for '{}'",
            crate_name
        );
    }

    let deps = winget_cfg.dependencies.as_deref().unwrap_or(&[]);

    // Generate release date from current date if available in context.
    let release_date = ctx.template_vars().get("Date").map(|d| d.to_string());
    let release_date_ref = release_date.as_deref();

    // Template-render all 18 fields (GoReleaser parity: winget.go:115-134).
    let render = |s: Option<&str>| -> Option<String> {
        s.map(|v| ctx.render_template(v).unwrap_or_else(|_| v.to_string()))
    };
    let publisher_rendered = render(Some(publisher_name)).unwrap();
    let publisher_url_rendered = render(winget_cfg.publisher_url.as_deref());
    let publisher_support_rendered = render(winget_cfg.publisher_support_url.as_deref());
    let privacy_url_rendered = render(winget_cfg.privacy_url.as_deref());
    let homepage_rendered = render(winget_cfg.homepage.as_deref());
    let author_rendered = render(winget_cfg.author.as_deref());
    let copyright_rendered = render(winget_cfg.copyright.as_deref());
    let copyright_url_rendered = render(winget_cfg.copyright_url.as_deref());
    let license_rendered = render(Some(license)).unwrap();
    let license_url_rendered = render(winget_cfg.license_url.as_deref());
    let short_desc_rendered = render(Some(short_desc)).unwrap().replace('\t', "  ");
    let release_notes_url_rendered = render(winget_cfg.release_notes_url.as_deref());
    let installation_notes_rendered = render(winget_cfg.installation_notes.as_deref());
    let path_rendered = render(winget_cfg.path.as_deref());
    // GoReleaser defaults PackageName to Name (winget.go:74: cmp.Or).
    let package_name_rendered = render(winget_cfg.package_name.as_deref())
        .or_else(|| Some(name.to_string()));
    // ReleaseNotes: template-rendered (GoReleaser parity: winget.go:173-175).
    // The `ReleaseNotes` template variable (populated from changelog) is already
    // available in the template context, matching GoReleaser's `Changelog` field.
    let release_notes_rendered = render(winget_cfg.release_notes.as_deref());

    let (ver_yaml, inst_yaml, locale_yaml) = generate_manifests(&WingetManifestParams {
        package_id,
        name,
        package_name: package_name_rendered.as_deref(),
        version: &version,
        description,
        short_description: &short_desc_rendered,
        license: &license_rendered,
        license_url: license_url_rendered.as_deref(),
        publisher: &publisher_rendered,
        publisher_url: publisher_url_rendered.as_deref(),
        publisher_support_url: publisher_support_rendered.as_deref(),
        privacy_url: privacy_url_rendered.as_deref(),
        author: author_rendered.as_deref(),
        copyright: copyright_rendered.as_deref(),
        copyright_url: copyright_url_rendered.as_deref(),
        homepage: homepage_rendered.as_deref(),
        release_notes: release_notes_rendered.as_deref(),
        release_notes_url: release_notes_url_rendered.as_deref(),
        installation_notes: installation_notes_rendered.as_deref(),
        tags: winget_cfg.tags.as_deref(),
        dependencies: deps,
        installers,
        product_code: winget_cfg.product_code.as_deref(),
        release_date: release_date_ref,
    });

    let token = util::resolve_repo_token(ctx, winget_cfg.repository.as_ref(), None);

    let tmp_dir = tempfile::tempdir().context("winget: create temp dir")?;
    let repo_path = tmp_dir.path();
    util::clone_repo(
        winget_cfg.repository.as_ref(),
        &repo_owner,
        &repo_name,
        token.as_deref(),
        repo_path,
        "winget",
        log,
    )?;

    // Build manifest path: use custom path (template-rendered) or auto-generate.
    let manifest_dir = if let Some(ref path) = path_rendered {
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
            .join(&version)
    };
    std::fs::create_dir_all(&manifest_dir)
        .with_context(|| format!("winget: create manifest dir {}", manifest_dir.display()))?;

    // Write 3-file manifests
    let ver_path = manifest_dir.join(format!("{}.yaml", package_id));
    let inst_path = manifest_dir.join(format!("{}.installer.yaml", package_id));
    let locale_path = manifest_dir.join(format!("{}.locale.en-US.yaml", package_id));

    std::fs::write(&ver_path, &ver_yaml)?;
    std::fs::write(&inst_path, &inst_yaml)?;
    std::fs::write(&locale_path, &locale_yaml)?;

    log.status(&format!(
        "wrote WinGet manifests to {}",
        manifest_dir.display()
    ));

    // Commit message — GoReleaser adds PackageIdentifier to the template context
    // (winget.go:291-293) in addition to the standard name/version.
    let commit_msg = render_winget_commit_msg(
        winget_cfg.commit_msg_template.as_deref(),
        package_id,
        &version,
    );

    // Use repository.branch if set, otherwise auto-generate from package_id + version.
    let auto_branch = format!("{}-{}", package_id, version);
    let branch_name = util::resolve_branch(winget_cfg.repository.as_ref()).unwrap_or(&auto_branch);
    let commit_opts = util::resolve_commit_opts(winget_cfg.commit_author.as_ref(), None, None);
    util::commit_and_push_with_opts(
        repo_path,
        &["."],
        &commit_msg,
        Some(branch_name),
        "winget",
        &commit_opts,
    )?;

    log.status(&format!(
        "WinGet manifest pushed to {}/{} branch '{}'",
        repo_owner, repo_name, branch_name
    ));

    // Submit a PR.  When `repository.pull_request` is configured and enabled,
    // use the unified PR helper (which respects `base`, `draft`, `body`).
    // Otherwise fall back to the legacy hardcoded "microsoft/winget-pkgs" target.
    let has_pr_config = winget_cfg
        .repository
        .as_ref()
        .and_then(|r| r.pull_request.as_ref())
        .and_then(|pr| pr.enabled)
        .unwrap_or(false);

    if has_pr_config {
        util::maybe_submit_pr(
            repo_path,
            winget_cfg.repository.as_ref(),
            &repo_owner,
            &repo_name,
            branch_name,
            &format!("New version: {} version {}", package_id, version),
            &format!(
                "## Package\n- **Package**: {}\n- **Version**: {}\n\nAutomatically submitted by anodize.",
                package_id, version
            ),
            "winget",
            log,
        );
    } else {
        // Legacy path: always submit a PR to microsoft/winget-pkgs.
        let upstream_slug = winget_cfg
            .repository
            .as_ref()
            .and_then(|r| r.pull_request.as_ref())
            .and_then(|pr| pr.base.as_ref())
            .and_then(|base| {
                let owner = base.owner.as_deref()?;
                let name = base.name.as_deref()?;
                Some(format!("{}/{}", owner, name))
            })
            .unwrap_or_else(|| "microsoft/winget-pkgs".to_string());

        util::submit_pr_via_gh(
            repo_path,
            &upstream_slug,
            &format!("{}:{}", repo_owner, branch_name),
            &format!("New version: {} version {}", package_id, version),
            &format!(
                "## Package\n- **Package**: {}\n- **Version**: {}\n\nAutomatically submitted by anodize.",
                package_id, version
            ),
            "winget",
            log,
        );
    }

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
        let manifest = generate_manifest(&default_params());
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
        let manifest = generate_manifest(&params);
        assert!(!manifest.contains("PublisherUrl:"));
        assert!(manifest.contains("Publisher: My Org"));
    }

    #[test]
    fn test_generate_3file_manifests() {
        let params = default_params();
        let (ver, inst, locale) = generate_manifests(&params);

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
        let deps = vec![anodize_core::config::WingetDependency {
            package_identifier: "Foo.Bar".to_string(),
            minimum_version: Some("1.0.0".to_string()),
        }];
        let mut params = default_params();
        params.dependencies = &deps;
        let (_, inst, _) = generate_manifests(&params);
        assert!(inst.contains("PackageDependencies:"));
        assert!(inst.contains("PackageIdentifier: Foo.Bar"));
        assert!(inst.contains("MinimumVersion: 1.0.0"));
    }

    #[test]
    fn test_generate_manifests_with_tags() {
        let tags = vec!["CLI Tool".to_string(), "Rust".to_string()];
        let mut params = default_params();
        params.tags = Some(&tags);
        let (_, _, locale) = generate_manifests(&params);
        assert!(locale.contains("cli-tool"));
        assert!(locale.contains("rust"));
    }

    // -----------------------------------------------------------------------
    // publish_to_winget dry-run tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_publish_to_winget_dry_run() {
        use anodize_core::config::{
            Config, CrateConfig, PublishConfig, WingetConfig, WingetManifestsRepoConfig,
        };
        use anodize_core::context::{Context, ContextOptions};
        use anodize_core::log::{StageLogger, Verbosity};

        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                winget: Some(WingetConfig {
                    manifests_repo: Some(WingetManifestsRepoConfig {
                        owner: "myorg".to_string(),
                        name: "winget-pkgs".to_string(),
                    }),
                    package_identifier: Some("MyOrg.MyTool".to_string()),
                    description: Some("A great tool".to_string()),
                    publisher: Some("My Org".to_string()),
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
        assert!(publish_to_winget(&ctx, "mytool", &log).is_ok());
    }

    #[test]
    fn test_publish_to_winget_missing_config() {
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

        // Should fail because there's no winget config
        assert!(publish_to_winget(&ctx, "mytool", &log).is_err());
    }

    #[test]
    fn test_publish_to_winget_auto_generates_package_identifier() {
        use anodize_core::config::{
            Config, CrateConfig, PublishConfig, WingetConfig, WingetManifestsRepoConfig,
        };
        use anodize_core::context::{Context, ContextOptions};
        use anodize_core::log::{StageLogger, Verbosity};

        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                winget: Some(WingetConfig {
                    manifests_repo: Some(WingetManifestsRepoConfig {
                        owner: "myorg".to_string(),
                        name: "winget-pkgs".to_string(),
                    }),
                    publisher: Some("My Org".to_string()),
                    package_identifier: None, // Auto-generated from Publisher.Name
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

        // Should succeed - package_identifier auto-generated as "MyOrg.mytool"
        assert!(publish_to_winget(&ctx, "mytool", &log).is_ok());
    }

    #[test]
    fn test_publish_to_winget_missing_manifests_repo() {
        use anodize_core::config::{Config, CrateConfig, PublishConfig, WingetConfig};
        use anodize_core::context::{Context, ContextOptions};
        use anodize_core::log::{StageLogger, Verbosity};

        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                winget: Some(WingetConfig {
                    manifests_repo: None, // Missing
                    package_identifier: Some("Org.Tool".to_string()),
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

        // Should fail because manifests_repo is missing
        assert!(publish_to_winget(&ctx, "mytool", &log).is_err());
    }

    #[test]
    fn test_generate_manifests_all_optional_fields() {
        let deps = vec![anodize_core::config::WingetDependency {
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

        let (ver, inst, locale) = generate_manifests(&params);

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

        let (_ver, inst, _locale) = generate_manifests(&params);
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

        let (_ver, inst, _locale) = generate_manifests(&params);
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

        let (_ver, inst, _locale) = generate_manifests(&params);
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
        assert_eq!(msg, "chore: update Org.MyTool manifest to 1.0.0");
    }

    #[test]
    fn test_winget_commit_msg_with_package_identifier_template() {
        // GoReleaser exposes PackageIdentifier in the template context
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
        let (_, _, locale) = generate_manifests(&params);
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
        let (_, _, locale) = generate_manifests(&params);
        assert!(
            locale.contains("PackageName: My Tool Pro"),
            "PackageName should use the override:\n{locale}"
        );
    }
}
