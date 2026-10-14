use super::*;

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
    /// Short invoke alias (`Moniker`). `None` omits the key entirely.
    pub(crate) moniker: Option<&'a str>,
    /// `UpgradeBehavior` for every installer entry (default resolved upstream).
    pub(crate) upgrade_behavior: &'a str,
    /// Manifest locale (`DefaultLocale` / `InstallerLocale` / `PackageLocale`,
    /// default resolved upstream).
    pub(crate) default_locale: &'a str,
    /// Documentation links (`Documentations[]`). Empty omits the key.
    pub(crate) documentations: &'a [anodizer_core::config::WingetDocumentation],
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
    /// Explicit silent-install switch override from config. When `None`, an
    /// actual-installer entry (`wix`/`msi`/`exe`/`nsis`) derives its switch
    /// from the installer type; `zip`/`portable` entries emit no switch.
    pub(crate) silent_switch_override: Option<String>,
}

// Serde structs for WinGet YAML manifests (3-file format)

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct VersionManifest {
    package_identifier: String,
    package_version: String,
    default_locale: String,
    manifest_type: String,
    manifest_version: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct InstallerManifest {
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
    release_date: Option<String>,
    manifest_type: String,
    manifest_version: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct InstallerEntry {
    architecture: String,
    installer_url: String,
    #[serde(rename = "InstallerSha256")]
    installer_sha256: String,
    upgrade_behavior: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    installer_switches: Option<InstallerSwitches>,
    #[serde(skip_serializing_if = "Option::is_none")]
    nested_installer_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    nested_installer_files: Option<Vec<NestedInstallerFile>>,
    /// Dependencies scoped to this installer's architecture. winget allows a
    /// `Dependencies` block per installer; emitting it here (not only at the
    /// manifest root) lets an architecture-specific runtime attach to just the
    /// matching installer.
    #[serde(skip_serializing_if = "Option::is_none")]
    dependencies: Option<DependenciesBlock>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct InstallerSwitches {
    pub(crate) silent: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct Documentation {
    document_label: String,
    document_url: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct NestedInstallerFile {
    relative_file_path: String,
    portable_command_alias: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct DependenciesBlock {
    package_dependencies: Vec<PkgDep>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct PkgDep {
    package_identifier: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    minimum_version: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct LocaleManifest {
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
    #[serde(skip_serializing_if = "Option::is_none")]
    moniker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    release_notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    release_notes_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    documentations: Option<Vec<Documentation>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    installation_notes: Option<String>,
    manifest_type: String,
    manifest_version: String,
}

/// Resolve `InstallerSwitches.Silent` for an installer entry.
///
/// Only actual installers carry a silent switch — `zip`/`portable` archives
/// are unpacked, not run, so emitting a switch would be meaningless. The
/// config override wins when set; otherwise the switch is derived from the
/// installer type: `/quiet` for msi (Windows Installer), `/S` for the NSIS /
/// Inno-style exe installers.
pub(crate) fn resolve_installer_switches(
    installer_type: &str,
    override_switch: Option<&str>,
) -> Option<InstallerSwitches> {
    // zip / portable archives are unpacked, not executed — never a silent
    // switch, even when an override is set (it would be meaningless), so the
    // type gate comes before the override.
    let derived = match installer_type {
        "wix" | "msi" => "/quiet",
        "exe" | "nsis" => "/S",
        _ => return None,
    };
    Some(InstallerSwitches {
        silent: override_switch.unwrap_or(derived).to_string(),
    })
}

/// Build the per-installer `Dependencies` block for an installer of the given
/// WinGet architecture (`x64`/`arm64`/`x86`).
///
/// A dependency with no `architectures` scope (or an empty list) applies to
/// every installer — preserving the historical manifest-wide behavior. A
/// scoped dependency is included only when `arch` is one of its listed
/// architectures, so an `x64`-scoped VC++ runtime never attaches to the native
/// `arm64` installer. Returns `None` when no dependency matches, so the
/// `Dependencies` key is omitted entirely for that installer.
pub(crate) fn dependencies_for_arch(
    deps: &[anodizer_core::config::WingetDependency],
    arch: &str,
) -> Option<DependenciesBlock> {
    let matched: Vec<PkgDep> = deps
        .iter()
        .filter(|d| match d.architectures.as_deref() {
            None | Some([]) => true,
            Some(scopes) => scopes.iter().any(|s| s == arch),
        })
        .map(|d| PkgDep {
            package_identifier: d.package_identifier.clone(),
            minimum_version: d.minimum_version.clone(),
        })
        .collect();
    if matched.is_empty() {
        None
    } else {
        Some(DependenciesBlock {
            package_dependencies: matched,
        })
    }
}

/// Generate the 3-file WinGet manifest set: (version, installer, locale).
pub(crate) fn generate_manifests(
    params: &WingetManifestParams<'_>,
) -> Result<(String, String, String)> {
    let version = VersionManifest {
        package_identifier: params.package_id.to_string(),
        package_version: params.version.to_string(),
        default_locale: params.default_locale.to_string(),
        manifest_type: "version".to_string(),
        manifest_version: "1.12.0".to_string(),
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
        installer_locale: params.default_locale.to_string(),
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
                    upgrade_behavior: params.upgrade_behavior.to_string(),
                    installer_switches: resolve_installer_switches(
                        &i.installer_type,
                        i.silent_switch_override.as_deref(),
                    ),
                    nested_installer_type: nested_type,
                    nested_installer_files: nested_files,
                    dependencies: dependencies_for_arch(params.dependencies, &i.architecture),
                }
            })
            .collect(),
        release_date: params.release_date.map(|s| s.to_string()),
        manifest_type: "installer".to_string(),
        manifest_version: "1.12.0".to_string(),
    };

    let locale = build_locale_manifest(params, params.default_locale, None);

    let version_yaml =
        serde_yaml_ng::to_string(&version).context("winget: serialize version manifest")?;
    let installer_yaml =
        serde_yaml_ng::to_string(&installer).context("winget: serialize installer manifest")?;
    let locale_yaml =
        serde_yaml_ng::to_string(&locale).context("winget: serialize locale manifest")?;
    Ok((
        with_headers(SCHEMA_VERSION, &version_yaml),
        with_headers(SCHEMA_INSTALLER, &installer_yaml),
        with_headers(SCHEMA_DEFAULT_LOCALE, &locale_yaml),
    ))
}

const SCHEMA_VERSION: &str =
    "# yaml-language-server: $schema=https://aka.ms/winget-manifest.version.1.12.0.schema.json\n";
const SCHEMA_INSTALLER: &str =
    "# yaml-language-server: $schema=https://aka.ms/winget-manifest.installer.1.12.0.schema.json\n";
const SCHEMA_DEFAULT_LOCALE: &str = "# yaml-language-server: $schema=https://aka.ms/winget-manifest.defaultLocale.1.12.0.schema.json\n";
const SCHEMA_LOCALE: &str =
    "# yaml-language-server: $schema=https://aka.ms/winget-manifest.locale.1.12.0.schema.json\n";

fn with_headers(schema: &str, yaml: &str) -> String {
    format!("{}\n{}{}", crate::util::GENERATED_FILE_HEADER, schema, yaml)
}

/// The rendered field overrides of one `additional_locales[]` entry. A
/// `None` or empty field takes the default locale's value.
#[derive(Debug, Default, Clone)]
pub(crate) struct WingetLocaleOverrides {
    pub(crate) publisher: Option<String>,
    pub(crate) publisher_url: Option<String>,
    pub(crate) publisher_support_url: Option<String>,
    pub(crate) privacy_url: Option<String>,
    pub(crate) author: Option<String>,
    pub(crate) package_name: Option<String>,
    pub(crate) homepage: Option<String>,
    pub(crate) license: Option<String>,
    pub(crate) license_url: Option<String>,
    pub(crate) copyright: Option<String>,
    pub(crate) copyright_url: Option<String>,
    pub(crate) short_description: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) tags: Option<Vec<String>>,
    pub(crate) release_notes: Option<String>,
    pub(crate) release_notes_url: Option<String>,
    pub(crate) installation_notes: Option<String>,
}

/// Generate the `<PackageIdentifier>.locale.<locale>.yaml` manifest of one
/// additional locale: `ManifestType: locale`, every field the entry left
/// unset inherited from the default locale.
pub(crate) fn generate_locale_manifest(
    params: &WingetManifestParams<'_>,
    locale: &str,
    overrides: &WingetLocaleOverrides,
) -> Result<String> {
    let manifest = build_locale_manifest(params, locale, Some(overrides));
    let yaml = serde_yaml_ng::to_string(&manifest)
        .with_context(|| format!("winget: serialize locale manifest for '{locale}'"))?;
    Ok(with_headers(SCHEMA_LOCALE, &yaml))
}

/// The override when it is set and non-empty, else the default, else nothing.
fn first_non_empty(over: Option<&str>, default: Option<&str>) -> Option<String> {
    over.filter(|s| !s.is_empty())
        .or(default)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn normalize_tags(tags: &[String]) -> Vec<String> {
    tags.iter()
        .map(|s| s.to_lowercase().replace(' ', "-"))
        .collect()
}

/// Build the locale manifest for `locale`. With no overrides this is the
/// default locale manifest (`defaultLocale`, carrying `Documentations`);
/// with overrides it is an additional locale (`locale`), which winget
/// resolves against the default locale, so `Documentations` is left to it.
fn build_locale_manifest(
    params: &WingetManifestParams<'_>,
    locale: &str,
    overrides: Option<&WingetLocaleOverrides>,
) -> LocaleManifest {
    let over = overrides.cloned().unwrap_or_default();
    let tags = over
        .tags
        .as_deref()
        .filter(|t| !t.is_empty())
        .or(params.tags)
        .map(normalize_tags);
    let description = first_non_empty(over.description.as_deref(), Some(params.description))
        .map(|d| d.replace('\t', "  "));

    LocaleManifest {
        package_identifier: params.package_id.to_string(),
        package_version: params.version.to_string(),
        package_locale: locale.to_string(),
        publisher: first_non_empty(over.publisher.as_deref(), Some(params.publisher))
            .unwrap_or_default(),
        publisher_url: first_non_empty(over.publisher_url.as_deref(), params.publisher_url),
        publisher_support_url: first_non_empty(
            over.publisher_support_url.as_deref(),
            params.publisher_support_url,
        ),
        privacy_url: first_non_empty(over.privacy_url.as_deref(), params.privacy_url),
        author: first_non_empty(over.author.as_deref(), params.author),
        package_name: first_non_empty(
            over.package_name.as_deref(),
            Some(params.package_name.unwrap_or(params.name)),
        )
        .unwrap_or_default(),
        package_url: first_non_empty(over.homepage.as_deref(), params.homepage),
        license: first_non_empty(over.license.as_deref(), Some(params.license)).unwrap_or_default(),
        license_url: first_non_empty(over.license_url.as_deref(), params.license_url),
        copyright: first_non_empty(over.copyright.as_deref(), params.copyright),
        copyright_url: first_non_empty(over.copyright_url.as_deref(), params.copyright_url),
        short_description: first_non_empty(
            over.short_description.as_deref(),
            Some(params.short_description),
        )
        .unwrap_or_default(),
        description,
        moniker: params
            .moniker
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty()),
        tags,
        release_notes: first_non_empty(over.release_notes.as_deref(), params.release_notes),
        release_notes_url: first_non_empty(
            over.release_notes_url.as_deref(),
            params.release_notes_url,
        ),
        documentations: if overrides.is_some() || params.documentations.is_empty() {
            None
        } else {
            Some(
                params
                    .documentations
                    .iter()
                    .map(|d| Documentation {
                        document_label: d.label.clone(),
                        document_url: d.url.clone(),
                    })
                    .collect(),
            )
        },
        installation_notes: first_non_empty(
            over.installation_notes.as_deref(),
            params.installation_notes,
        ),
        manifest_type: if overrides.is_some() {
            "locale"
        } else {
            "defaultLocale"
        }
        .to_string(),
        manifest_version: "1.12.0".to_string(),
    }
}

// publish_to_winget helpers
