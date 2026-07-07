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
///
/// Strict-aware via [`util::render_or_warn_with_vars`]: a malformed
/// `commit_msg_template` errors under the guard / `--strict`, else warns and
/// falls back to the default-shaped raw message.
fn render_winget_commit_msg(
    template: Option<&str>,
    package_id: &str,
    version: &str,
    log: &StageLogger,
    is_strict: bool,
) -> Result<String> {
    // Default: "New version: {{ .PackageIdentifier }} {{ .Version }}"
    let default_tmpl = "New version: {{ PackageIdentifier }} {{ Version }}";
    let tmpl = template.unwrap_or(default_tmpl);

    let mut vars = TemplateVars::new();
    vars.set("PackageIdentifier", package_id);
    vars.set("ProjectName", package_id);
    vars.set("Tag", version);
    vars.set("Version", version);
    vars.set("name", package_id);
    vars.set("version", version);
    match template::render(tmpl, &vars) {
        Ok(rendered) => Ok(rendered),
        Err(e) => {
            if is_strict {
                anyhow::bail!("failed to render winget.commit_msg_template {tmpl:?}: {e}");
            }
            log.warn(&format!(
                "failed to render winget.commit_msg_template {tmpl:?}: {e}; \
                 falling back to default commit message"
            ));
            Ok(format!("New version: {} {}", package_id, version))
        }
    }
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
    /// Short invoke alias (`Moniker`). `None` omits the key entirely.
    pub(crate) moniker: Option<&'a str>,
    /// `UpgradeBehavior` for every installer entry (default resolved upstream).
    pub(crate) upgrade_behavior: &'a str,
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
struct InstallerSwitches {
    silent: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Documentation {
    document_label: String,
    document_url: String,
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
fn resolve_installer_switches(
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
fn dependencies_for_arch(
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
        default_locale: "en-US".to_string(),
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
        moniker: params
            .moniker
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty()),
        tags,
        release_notes: params
            .release_notes
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty()),
        release_notes_url: params
            .release_notes_url
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty()),
        documentations: if params.documentations.is_empty() {
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
                "winget publisher not explicitly set for '{}'; falling back to repo owner '{}'",
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
    log: &StageLogger,
) -> Result<String> {
    let description_raw_cfg = winget_cfg
        .description
        .as_deref()
        .or_else(|| ctx.config.meta_description_for(crate_name))
        .unwrap_or("");
    let description_tmpl =
        util::render_or_warn(ctx, log, "winget.description", description_raw_cfg)?;
    Ok(description_tmpl.replace('\t', "  "))
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

/// Resolve the winget `Moniker` (short invoke alias).
///
/// Precedence: explicit `winget.moniker` config override → the single
/// published binary name when exactly one is built → `None` (omit the key)
/// when multiple binaries exist and no override is set. winget treats the
/// Moniker as the command users type (`rg`, `fd`), never the package name, so
/// defaulting to the crate name (the old behavior) was wrong; omitting is the
/// honest default real ripgrep/fd manifests follow when ambiguous.
fn resolve_winget_moniker(
    ctx: &Context,
    crate_name: &str,
    winget_cfg: &anodizer_core::config::WingetConfig,
) -> Option<String> {
    if let Some(m) = winget_cfg.moniker.as_deref().filter(|s| !s.is_empty()) {
        return Some(m.to_string());
    }
    // Collect the distinct binary names across all windows Build artifacts.
    // A BTreeSet dedups in O(n log n) and yields a deterministic ordering
    // (the source HashMap's value iteration order is not stable), so the
    // single-binary derivation is reproducible across runs.
    let by_target = collect_windows_binary_names_by_target(ctx, crate_name);
    let distinct: std::collections::BTreeSet<&String> = by_target.values().flatten().collect();
    match distinct.into_iter().collect::<Vec<_>>().as_slice() {
        [single] => Some((*single).clone()),
        _ => None,
    }
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

    /// Derive the windows artifact filters from a crate's winget config,
    /// applying the `amd64_variant` default (`v1`) once so the live collector
    /// and the schema validator's shard-guard cannot disagree on which
    /// artifacts are eligible.
    fn from_config(winget_cfg: &'a anodizer_core::config::WingetConfig) -> Self {
        WingetArtifactFilters {
            ids: winget_cfg.ids.as_deref(),
            amd64_variant: Some(winget_cfg.amd64_variant.map_or("v1", |v| v.as_str())),
        }
    }
}

/// True when an archive artifact is the zip form winget consumes as an
/// installer: either tagged `format: zip` in its metadata or named `*.zip`.
///
/// The single home for this predicate so [`collect_winget_installers`] (the
/// live publish path) and [`crate_has_winget_installer_artifacts`] (the schema
/// validator's snapshot-shard guard) classify archives identically — if winget
/// later accepts another archive kind, both update together rather than the
/// guard silently suppressing validation of an artifact that would publish.
fn is_winget_zip_archive(a: &anodizer_core::artifact::Artifact) -> bool {
    a.metadata.get("format").map(|f| f.as_str()) == Some("zip")
        || a.path.to_string_lossy().ends_with(".zip")
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
    silent_switch: Option<&str>,
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
        silent_switch_override: silent_switch.map(str::to_string),
    })
}

/// Map an installer artifact's `format` metadata (as stamped by the installer
/// stages — `msi` from stage-msi, `nsis` from stage-nsis) to the winget
/// `InstallerType`. The `use` selector disambiguates `msi` vs `wix` (both are
/// Windows Installer packages with `/quiet` switches, but winget distinguishes
/// the authoring toolchain): `use: wix` keeps the `wix` type while every other
/// MSI maps to `msi`. An `exe`-format installer (a non-NSIS self-extractor)
/// maps to the generic `exe` type. Falls back to the `use` selector when the
/// artifact carries no `format` stamp.
fn installer_type_for(format: Option<&str>, use_artifact: Option<&str>) -> &'static str {
    match format {
        Some("msi") => {
            if use_artifact == Some("wix") {
                "wix"
            } else {
                "msi"
            }
        }
        Some("nsis") => "nsis",
        Some("exe") => "exe",
        // No format stamp: trust the `use` selector that routed this artifact
        // (it is `ArtifactKind::Installer`, so it is one of these kinds).
        _ => match use_artifact {
            Some("wix") => "wix",
            Some("nsis") => "nsis",
            Some("exe") => "exe",
            _ => "msi",
        },
    }
}

/// Build an actual-installer [`WingetInstallerItem`] (msi/wix/nsis/exe) from a
/// matching `Installer` artifact. Unlike the zip path, winget runs the
/// installer directly, so the entry carries no `NestedInstallerFile` block; the
/// silent switch is derived from `installer_type` (or the config override) by
/// [`resolve_installer_switches`] downstream. Errors when sha256 metadata is
/// missing (a manifest with `InstallerSha256: ''` is rejected by winget).
fn build_executable_installer(
    ctx: &Context,
    a: &anodizer_core::artifact::Artifact,
    url_template: Option<&str>,
    name: &str,
    version: &str,
    installer_type: &str,
    silent_switch: Option<&str>,
) -> Result<WingetInstallerItem> {
    let target = a.target.as_deref().unwrap_or("");
    let (_, raw_arch) = anodizer_core::target::map_target(target);
    let arch = map_winget_arch(raw_arch.as_str());
    let resolved_url = resolve_installer_url(ctx, a, url_template, name, version, &raw_arch);
    let sha256 = a.metadata.get("sha256").cloned().unwrap_or_default();
    if sha256.is_empty() {
        anyhow::bail!(
            "winget: installer '{}' has no sha256 metadata; \
             the manifest would publish with InstallerSha256: '' \
             and be rejected by winget validation. \
             Ensure the checksum stage runs before winget, or that \
             the publish flow seeds sha256 onto downloaded assets.",
            a.path.display()
        );
    }
    Ok(WingetInstallerItem {
        architecture: arch.to_string(),
        url: resolved_url,
        sha256,
        installer_type: installer_type.to_string(),
        binaries: Vec::new(),
        wrap_in_directory: None,
        commands: Vec::new(),
        silent_switch_override: silent_switch.map(str::to_string),
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
    silent_switch: Option<&str>,
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
        silent_switch_override: silent_switch.map(str::to_string),
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
    log: &StageLogger,
) -> Result<Vec<WingetInstallerItem>> {
    let url_template = winget_cfg.url_template.as_deref();
    let artifact_kind = util::resolve_artifact_kind(winget_cfg.use_artifact.as_deref());
    let use_artifact = winget_cfg.use_artifact.as_deref();

    let binary_names_by_target = collect_windows_binary_names_by_target(ctx, crate_name);

    let filters = WingetArtifactFilters::from_config(winget_cfg);
    let silent_switch = winget_cfg.silent_switch.as_deref();

    let mut installers: Vec<WingetInstallerItem> = Vec::new();

    // `use: msi`/`nsis` (and `wix`/`exe`) resolve to `ArtifactKind::Installer`:
    // winget runs the real installer, so select those artifacts directly and
    // derive the `InstallerType` from each artifact's `format` stamp. The
    // zip/portable archive path below is for the default (archive) config only.
    if artifact_kind == anodizer_core::artifact::ArtifactKind::Installer {
        let installer_artifacts = ctx
            .artifacts
            .by_kind_and_crate(anodizer_core::artifact::ArtifactKind::Installer, crate_name);
        for a in installer_artifacts.iter() {
            if !filters.matches(a) {
                continue;
            }
            // The macOS `.app` directory bundle shares ArtifactKind::Installer
            // but is never a Windows installer winget can run.
            if anodizer_core::artifact::is_directory_bundle_artifact(a) {
                continue;
            }
            let installer_type =
                installer_type_for(a.metadata.get("format").map(String::as_str), use_artifact);
            installers.push(build_executable_installer(
                ctx,
                a,
                url_template,
                name,
                version,
                installer_type,
                silent_switch,
            )?);
        }
    } else {
        let archive_artifacts = ctx.artifacts.by_kind_and_crate(artifact_kind, crate_name);
        let binary_artifacts = ctx.artifacts.by_kind_and_crate(
            anodizer_core::artifact::ArtifactKind::UploadableBinary,
            crate_name,
        );

        let mut zip_count = 0u32;
        let mut binary_count = 0u32;

        for a in archive_artifacts.iter() {
            if !filters.matches(a) {
                continue;
            }
            if !is_winget_zip_archive(a) {
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
                silent_switch,
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
                silent_switch,
            )?);
        }

        if binary_count > 0 && zip_count > 0 {
            anyhow::bail!(
                "winget: found archives with multiple formats (.exe and .zip) for '{}'; \
                 use either portable binaries or zip archives, not both",
                crate_name
            );
        }
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

    if silent_switch.is_some()
        && !installers
            .iter()
            .any(|i| is_executable_installer_type(&i.installer_type))
    {
        log.warn(&format!(
            "winget.silent_switch ignored for '{crate_name}': no installer-type artifact \
             (msi/wix/exe/nsis); portable/zip artifacts are unpacked, not run, so there is \
             no installer to pass switches to"
        ));
    }

    Ok(installers)
}

/// Resolve the winget manifest `ProductCode`: explicit `winget.product_code`
/// config wins (it is the author's authoritative override); otherwise fall back
/// to the deterministic `product_code` anodizer's MSI stage stamped onto the
/// selected `.msi` installer artifact, so the manifest's AppsAndFeaturesEntries
/// can detect upgrades without the author hand-copying a GUID. Returns `None`
/// when neither source supplies one (e.g. a zip/portable-only winget config).
fn resolve_winget_product_code(
    ctx: &Context,
    crate_name: &str,
    winget_cfg: &anodizer_core::config::WingetConfig,
) -> Option<String> {
    if let Some(pc) = winget_cfg.product_code.as_deref().filter(|s| !s.is_empty()) {
        return Some(pc.to_string());
    }
    let filters = WingetArtifactFilters::from_config(winget_cfg);
    ctx.artifacts
        .by_kind_and_crate(anodizer_core::artifact::ArtifactKind::Installer, crate_name)
        .into_iter()
        .filter(|a| filters.matches(a))
        .filter(|a| !anodizer_core::artifact::is_directory_bundle_artifact(a))
        .filter(|a| a.metadata.get("format").map(String::as_str) == Some("msi"))
        .find_map(|a| a.metadata.get("product_code").cloned())
        .filter(|s| !s.is_empty())
}

/// True for winget `InstallerType` values that name an actual installer
/// program (one that `InstallerSwitches.Silent` is passed to), as opposed to
/// `zip`/`portable`, which winget unpacks itself without running an installer.
fn is_executable_installer_type(installer_type: &str) -> bool {
    matches!(installer_type, "msi" | "wix" | "exe" | "nsis")
}

/// True when `crate_name` has at least one Windows installer artifact this run
/// would feed into a winget manifest (a zip archive of the configured `use`
/// kind, or a portable `UploadableBinary`), after the same id / amd64-variant
/// filters [`collect_winget_installers`] applies.
///
/// A real release always produces these (the publish path errors otherwise),
/// but a single-target / sharded snapshot legitimately builds only one platform
/// — so the offline schema validator consults this to skip a crate whose
/// Windows installer was not built in the current shard rather than fail on the
/// publisher's own "no Windows artifact" guard.
pub(crate) fn crate_has_winget_installer_artifacts(
    ctx: &Context,
    crate_name: &str,
    winget_cfg: &anodizer_core::config::WingetConfig,
) -> bool {
    let filters = WingetArtifactFilters::from_config(winget_cfg);
    let artifact_kind = util::resolve_artifact_kind(winget_cfg.use_artifact.as_deref());

    // `use: msi`/`nsis` selects real `Installer` artifacts directly (no zip
    // gate); mirror the collector's branch so the shard guard and the live
    // path agree on what a winget Windows installer is.
    if artifact_kind == anodizer_core::artifact::ArtifactKind::Installer {
        return ctx
            .artifacts
            .by_kind_and_crate(anodizer_core::artifact::ArtifactKind::Installer, crate_name)
            .iter()
            .any(|a| {
                filters.matches(a) && !anodizer_core::artifact::is_directory_bundle_artifact(a)
            });
    }

    let has_zip = ctx
        .artifacts
        .by_kind_and_crate(artifact_kind, crate_name)
        .iter()
        .any(|a| filters.matches(a) && is_winget_zip_archive(a));
    let has_portable = ctx
        .artifacts
        .by_kind_and_crate(
            anodizer_core::artifact::ArtifactKind::UploadableBinary,
            crate_name,
        )
        .iter()
        .any(|a| filters.matches(a));

    has_zip || has_portable
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
///
/// Each field renders strict-aware via [`util::render_or_warn_with_vars`]: a
/// malformed field template errors under the guard / `--strict`, else warns
/// and falls back to its raw value.
#[allow(clippy::too_many_arguments)]
fn render_winget_fields(
    ctx: &Context,
    winget_cfg: &anodizer_core::config::WingetConfig,
    crate_name: &str,
    name: &str,
    publisher_name: &str,
    license: &str,
    short_desc: &str,
    log: &StageLogger,
) -> Result<RenderedWingetFields> {
    let release_notes_var = ctx
        .template_vars()
        .get("ReleaseNotes")
        .cloned()
        .unwrap_or_default();
    let is_strict = ctx.render_is_strict();
    let render = |field: &str, s: Option<&str>| -> Result<Option<String>> {
        s.map(|v| {
            let mut vars = ctx.template_vars().clone();
            vars.set("Changelog", &release_notes_var);
            util::render_or_warn_with_vars(&vars, log, field, v, is_strict)
        })
        .transpose()
    };

    Ok(RenderedWingetFields {
        publisher: render("winget.publisher", Some(publisher_name))?
            .unwrap_or_else(|| publisher_name.to_string()),
        publisher_url: render("winget.publisher_url", winget_cfg.publisher_url.as_deref())?,
        publisher_support_url: render(
            "winget.publisher_support_url",
            winget_cfg.publisher_support_url.as_deref(),
        )?,
        privacy_url: render("winget.privacy_url", winget_cfg.privacy_url.as_deref())?,
        homepage: render(
            "winget.homepage",
            winget_cfg
                .homepage
                .as_deref()
                .or_else(|| ctx.config.meta_homepage_for(crate_name)),
        )?,
        author: render("winget.author", winget_cfg.author.as_deref())?,
        copyright: render("winget.copyright", winget_cfg.copyright.as_deref())?,
        copyright_url: render("winget.copyright_url", winget_cfg.copyright_url.as_deref())?,
        license: render("winget.license", Some(license))?.unwrap_or_else(|| license.to_string()),
        license_url: render("winget.license_url", winget_cfg.license_url.as_deref())?,
        short_description: render("winget.short_description", Some(short_desc))?
            .unwrap_or_else(|| short_desc.to_string())
            .replace('\t', "  "),
        release_notes_url: render(
            "winget.release_notes_url",
            winget_cfg.release_notes_url.as_deref(),
        )?,
        installation_notes: render(
            "winget.installation_notes",
            winget_cfg.installation_notes.as_deref(),
        )?,
        path: render("winget.path", winget_cfg.path.as_deref())?,
        package_name: render("winget.package_name", winget_cfg.package_name.as_deref())?
            .or_else(|| Some(name.to_string())),
        release_notes: render("winget.release_notes", winget_cfg.release_notes.as_deref())?,
    })
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
    render: &dyn Fn(&str) -> String,
    env: &dyn anodizer_core::EnvSource,
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
        util::maybe_submit_pr_with_env(
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
            render,
            env,
        )
    } else {
        // A templated `base.owner` / `base.name` must render before it forms
        // the upstream PR slug sent to the GitHub API.
        let upstream_slug = repo_for_pr
            .and_then(|r| r.pull_request.as_ref())
            .and_then(|pr| pr.base.as_ref())
            .and_then(|base| {
                let owner = render(base.owner.as_deref()?);
                let name = render(base.name.as_deref()?);
                Some(format!("{}/{}", owner, name))
            })
            .unwrap_or_else(|| "microsoft/winget-pkgs".to_string());

        util::submit_pr_via_gh_with_opts_with_env(
            repo_path,
            &upstream_slug,
            &format!("{}:{}", repo_owner, branch_name),
            &title,
            &body,
            "winget",
            log,
            util::SubmitPrOpts { update_existing_pr },
            env,
        )
    }
}

// ---------------------------------------------------------------------------
// publish_to_winget
// ---------------------------------------------------------------------------

/// The side-effect-free product of rendering a crate's WinGet manifests: the
/// three YAML documents plus the resolved identity fields the downstream
/// commit/PR steps need. Produced by [`render_winget_manifests_for_crate`] so
/// the live publish path and the offline schema validator render from one
/// source of truth.
pub(crate) struct RenderedWingetManifests {
    /// Version manifest YAML (the `<PackageIdentifier>.yaml` file).
    pub(crate) version_yaml: String,
    /// Installer manifest YAML (the `<PackageIdentifier>.installer.yaml` file).
    pub(crate) installer_yaml: String,
    /// Locale manifest YAML (the `<PackageIdentifier>.locale.en-US.yaml` file).
    pub(crate) locale_yaml: String,
    /// Resolved fork repository owner the manifests are pushed under.
    pub(crate) repo_owner: String,
    /// Resolved fork repository name the manifests are pushed under.
    pub(crate) repo_name: String,
    /// Resolved WinGet `PackageIdentifier`.
    pub(crate) package_id: String,
    /// Crate path-rendering override (`winget.path`), already template-rendered.
    pub(crate) path: Option<String>,
}

/// The publisher's resolved identity for a crate: the package coordinates and
/// fork-repo target, derived before any manifest content is rendered. Shared by
/// the dry-run short-circuit (which only needs the coordinates to log) and the
/// full manifest render.
struct WingetIdentity {
    repo_owner: String,
    repo_name: String,
    name: String,
    publisher_name: String,
    package_id: String,
}

/// Resolve a crate's WinGet identity (repo, name, publisher, validated
/// `PackageIdentifier`), or `Ok(None)` when the publisher would skip the crate
/// (`skip_upload` / a falsy `if`). Errors when the crate carries no `winget`
/// block — callers must guarantee the block is present.
fn resolve_winget_identity(
    ctx: &Context,
    crate_name: &str,
    winget_cfg: &anodizer_core::config::WingetConfig,
    log: &StageLogger,
) -> Result<Option<WingetIdentity>> {
    let label = format!("winget publisher for crate '{}'", crate_name);
    if crate::util::should_skip_publisher_with_if(
        ctx,
        None,
        winget_cfg.skip_upload.as_ref(),
        winget_cfg.if_condition.as_deref(),
        &label,
        log,
    )? {
        return Ok(None);
    }

    let (repo_owner, repo_name) =
        crate::util::resolve_repo_owner_name(winget_cfg.repository.as_ref())
            .ok_or_else(|| anyhow::anyhow!("winget: no repository config for '{}'", crate_name))?;

    let name_raw = winget_cfg.name.as_deref().unwrap_or(crate_name);
    let name = util::render_or_warn(ctx, log, "winget.name", name_raw)?;
    let publisher_name =
        resolve_winget_publisher_name(winget_cfg, &repo_owner, crate_name, log)?.to_string();

    let auto_pkg_id = format!("{}.{}", publisher_name.replace(' ', ""), name);
    let package_id = winget_cfg
        .package_identifier
        .as_deref()
        .unwrap_or(&auto_pkg_id)
        .to_string();

    validate_package_identifier(&package_id)?;

    Ok(Some(WingetIdentity {
        repo_owner,
        repo_name,
        name,
        publisher_name,
        package_id,
    }))
}

/// Resolve a crate's WinGet config and render its three manifests in-memory,
/// with no disk, clone, or network side effects.
///
/// Returns `Ok(None)` when the publisher would skip this crate (`skip_upload`
/// or a falsy `if` condition). Errors when the crate carries no `winget` block.
/// The live publish path and the offline schema validator both call this so the
/// validated documents are byte-for-byte what a real publish would push.
pub(crate) fn render_winget_manifests_for_crate(
    ctx: &Context,
    crate_name: &str,
    log: &StageLogger,
) -> Result<Option<RenderedWingetManifests>> {
    let (_crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "winget")?;
    let winget_cfg = publish
        .winget
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("winget: no winget config for '{}'", crate_name))?;

    let Some(identity) = resolve_winget_identity(ctx, crate_name, winget_cfg, log)? else {
        return Ok(None);
    };
    Ok(Some(render_winget_manifests_with_identity(
        ctx, crate_name, winget_cfg, &identity, log,
    )?))
}

/// Render a crate's three WinGet manifests from a pre-resolved
/// [`WingetIdentity`].
///
/// Split out so the live publish path can reuse the identity it already
/// resolved (for the dry-run short-circuit) rather than re-resolving it —
/// re-resolution would re-emit `resolve_winget_publisher_name`'s
/// fallback-to-repo-owner warning a second time per publish.
fn render_winget_manifests_with_identity(
    ctx: &Context,
    crate_name: &str,
    winget_cfg: &anodizer_core::config::WingetConfig,
    identity: &WingetIdentity,
    log: &StageLogger,
) -> Result<RenderedWingetManifests> {
    let name = identity.name.as_str();
    let publisher_name = identity.publisher_name.as_str();
    let package_id = identity.package_id.as_str();

    let version = ctx.version();
    let description = resolve_winget_description(ctx, winget_cfg, crate_name, log)?;
    let short_desc = resolve_winget_short_description(ctx, winget_cfg, crate_name)?;
    let license = resolve_winget_license(ctx, winget_cfg, crate_name)?;

    let installers = collect_winget_installers(ctx, crate_name, winget_cfg, name, &version, log)?;
    let product_code = resolve_winget_product_code(ctx, crate_name, winget_cfg);

    let deps = winget_cfg.dependencies.as_deref().unwrap_or(&[]);
    let release_date = resolve_winget_release_date(ctx);
    let release_date_ref = release_date.as_deref();

    let moniker = resolve_winget_moniker(ctx, crate_name, winget_cfg);
    // winget upgrade behavior: default `install` (correct for portable-zip
    // tools); `uninstallPrevious` forces a clobbering reinstall.
    let upgrade_behavior = winget_cfg
        .upgrade_behavior
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("install");
    let documentations = winget_cfg.documentations.as_deref().unwrap_or(&[]);

    let rendered = render_winget_fields(
        ctx,
        winget_cfg,
        crate_name,
        name,
        publisher_name,
        license,
        &short_desc,
        log,
    )?;

    let (version_yaml, installer_yaml, locale_yaml) = generate_manifests(&WingetManifestParams {
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
        product_code: product_code.as_deref(),
        release_date: release_date_ref,
        moniker: moniker.as_deref(),
        upgrade_behavior,
        documentations,
    })?;

    Ok(RenderedWingetManifests {
        version_yaml,
        installer_yaml,
        locale_yaml,
        repo_owner: identity.repo_owner.clone(),
        repo_name: identity.repo_name.clone(),
        package_id: package_id.to_string(),
        path: rendered.path,
    })
}

pub fn publish_to_winget(ctx: &mut Context, crate_name: &str, log: &StageLogger) -> Result<()> {
    // Clone the winget config upfront so subsequent helpers do not borrow from
    // `ctx.config`; that frees the `&mut ctx` call site at the end of the
    // function (`ctx.record_publisher_outcome`).
    let (_crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "winget")?;
    let winget_cfg = publish
        .winget
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("winget: no winget config for '{}'", crate_name))?
        .clone();

    // Resolve identity first so dry-run short-circuits BEFORE the full manifest
    // render (which requires short_description/license/installers): a dry-run
    // only reports the coordinates it would push, exactly as before.
    let Some(identity) = resolve_winget_identity(ctx, crate_name, &winget_cfg, log)? else {
        return Ok(());
    };

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would submit WinGet manifest for '{}' (pkg={}) to {}/{}",
            crate_name, identity.package_id, identity.repo_owner, identity.repo_name
        ));
        return Ok(());
    }

    // Reuse the identity already resolved above so the manifest render does not
    // re-run `resolve_winget_publisher_name` (which would re-emit its
    // fallback-to-repo-owner warning a second time per publish).
    let rendered =
        render_winget_manifests_with_identity(ctx, crate_name, &winget_cfg, &identity, log)?;

    submit_winget_manifests(ctx, log, &winget_cfg, &rendered)
}

/// Clone the package repo, write the pre-rendered manifests, commit, push, and
/// open a PR. The manifests are produced upstream by
/// [`render_winget_manifests_for_crate`] so this function performs only the
/// side-effecting steps.
fn submit_winget_manifests(
    ctx: &mut Context,
    log: &StageLogger,
    winget_cfg: &anodizer_core::config::WingetConfig,
    rendered: &RenderedWingetManifests,
) -> Result<()> {
    let version = ctx.version();
    let repo_owner = rendered.repo_owner.as_str();
    let repo_name = rendered.repo_name.as_str();
    let package_id = rendered.package_id.as_str();
    let ver_yaml = rendered.version_yaml.as_str();
    let inst_yaml = rendered.installer_yaml.as_str();
    let locale_yaml = rendered.locale_yaml.as_str();

    // Guard before the fork clone: a residual delimiter must bail with no
    // clone/commit/push side effect, not just no push.
    util::guard_no_unrendered(ctx, log, "winget version manifest", ver_yaml)?;
    util::guard_no_unrendered(ctx, log, "winget installer manifest", inst_yaml)?;
    util::guard_no_unrendered(ctx, log, "winget locale manifest", locale_yaml)?;

    let token = util::resolve_repo_token(
        ctx,
        winget_cfg.repository.as_ref(),
        Some("WINGET_PKGS_TOKEN"),
    );

    let tmp_dir = tempfile::tempdir().context("winget: create temp dir")?;
    let repo_path = tmp_dir.path();
    util::clone_repo(
        ctx,
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
        ver_yaml,
        inst_yaml,
        locale_yaml,
    )?;

    log.status(&format!(
        "wrote WinGet manifests to {}",
        manifest_dir.display()
    ));

    let commit_msg = render_winget_commit_msg(
        winget_cfg.commit_msg_template.as_deref(),
        package_id,
        &version,
        log,
        ctx.render_is_strict(),
    )?;

    let auto_branch = format!("{}-{}", package_id, version);
    let branch_name =
        util::resolve_branch(ctx, winget_cfg.repository.as_ref()).unwrap_or(auto_branch);
    let branch_name = branch_name.as_str();
    let commit_opts = util::resolve_commit_opts(ctx, winget_cfg.commit_author.as_ref(), log)?;
    let outcome = util::commit_and_push_with_opts(
        repo_path,
        &["."],
        &commit_msg,
        Some(branch_name),
        "winget",
        &commit_opts,
        log,
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
                "nothing to push, winget manifest for '{}' already up to date",
                package_id
            ));
        }
    }

    let update_existing_pr = match winget_cfg.update_existing_pr.as_ref() {
        Some(v) => v
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .context("winget: render update_existing_pr condition")?,
        None => false,
    };

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
        &|s| ctx.render_template(s).unwrap_or_else(|_| s.to_string()),
        ctx.env_source(),
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

/// The crate-level `publish.winget` block — the single accessor the
/// registry gate, the gate-override collapse, and the per-crate dispatch
/// predicate all key on.
pub(crate) fn block(
    p: &anodizer_core::config::PublishConfig,
) -> Option<&anodizer_core::config::WingetConfig> {
    p.winget.as_ref()
}

pub(crate) fn is_winget_per_crate_configured(ctx: &Context, crate_name: &str) -> bool {
    crate::publisher_helpers::is_per_crate_block_configured(ctx, crate_name, block)
}

/// Build a [`WingetTarget`] for the given crate. Reads config + the
/// live process version so the recorded coordinates match what
/// `publish_to_winget` will push. Returns `None` when no winget block
/// is configured or when the publisher / repo resolution would itself
/// no-op (matches the publish path's skip semantics).
fn collect_winget_target(
    ctx: &Context,
    crate_name: &str,
    log: &StageLogger,
) -> Result<Option<WingetTarget>> {
    let Some(c) = crate::util::find_crate_in_universe(ctx, crate_name) else {
        return Ok(None);
    };
    let Some(cfg) = c.publish.as_ref().and_then(|p| p.winget.as_ref()) else {
        return Ok(None);
    };
    let Some((repo_owner, _repo_name)) =
        crate::util::resolve_repo_owner_name(cfg.repository.as_ref())
    else {
        return Ok(None);
    };
    let fork_owner = util::render_or_warn(ctx, log, "winget.repository.owner", &repo_owner)?;

    let name_raw = cfg.name.as_deref().unwrap_or(crate_name);
    let name_rendered = util::render_or_warn(ctx, log, "winget.name", name_raw)?;

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
    let branch = crate::util::resolve_branch(ctx, cfg.repository.as_ref()).unwrap_or(auto_branch);

    let (upstream_owner, upstream_repo) = resolve_winget_upstream(cfg);

    Ok(Some(WingetTarget {
        target: package_id.clone(),
        crate_name: crate_name.to_string(),
        package_id,
        version,
        upstream_owner,
        upstream_repo,
        fork_owner,
        branch,
    }))
}

/// Message emitted just before delegating to `publish_to_winget`.
/// Anchors the winget activity (manifest generation, fork clone, push,
/// PR submission) to a specific crate in the log so multi-crate
/// workspaces are disambiguatable.
pub(crate) fn run_per_crate_start_message(crate_name: &str) -> String {
    format!("starting per-crate winget publish for '{}'", crate_name)
}

/// Final summary emitted at publisher exit. `processed` is the count of
/// crates the publisher actually invoked `publish_to_winget` on (not
/// the count of successful PRs — `publish_to_winget` has its own skip
/// paths for skip_upload/dry-run/etc., each of which logs its own status
/// line, and the gh CLI submission helper logs its own success/warn).
pub(crate) fn run_done_message(processed: usize) -> String {
    format!(
        "finished winget publish — {} configured crate(s) processed",
        processed
    )
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
        "winget publisher registered but 0 of {} effective crate(s) had a winget \
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

    fn retain_on_rollback(&self) -> bool {
        Self::resolved_retain_on_rollback(self)
    }

    fn requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        ctx.config
            .crate_universe()
            .into_iter()
            .filter_map(|c| c.publish.as_ref()?.winget.as_ref())
            .filter(|w| {
                !crate::publisher_helpers::entry_inactive(
                    ctx,
                    None,
                    w.skip_upload.as_ref(),
                    w.if_condition.as_deref(),
                )
            })
            .flat_map(|w| {
                crate::publisher_helpers::git_repo_requirements(
                    ctx,
                    w.repository.as_ref(),
                    Some("WINGET_PKGS_TOKEN"),
                )
            })
            .collect()
    }

    fn advisory_requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        // Every winget publish lands as a PR against the upstream index;
        // `gh pr create` is the preferred transport with a full REST-API
        // fallback, so `gh` is a recommendation, never a gate failure.
        let any_active = ctx
            .config
            .crate_universe()
            .into_iter()
            .filter_map(|c| c.publish.as_ref()?.winget.as_ref())
            .any(|w| {
                !crate::publisher_helpers::entry_inactive(
                    ctx,
                    None,
                    w.skip_upload.as_ref(),
                    w.if_condition.as_deref(),
                )
            });
        if !any_active {
            return Vec::new();
        }
        vec![anodizer_core::EnvRequirement::Tool {
            name: "gh".to_string(),
        }]
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        let log = ctx.logger("publish");
        let mut targets: Vec<WingetTarget> = Vec::new();
        let selected =
            crate::publisher_helpers::effective_publish_crates(ctx, is_winget_per_crate_configured);
        log.status(&crate::publisher_helpers::run_start_message(
            "winget",
            selected.len(),
        ));
        for crate_name in &selected {
            // Defensive guard for explicit `--crate=X` selection when X has no
            // publisher block; implicit-all is already filtered by effective_publish_crates above.
            if !is_winget_per_crate_configured(ctx, crate_name) {
                log.skip_line(
                    ctx.options.show_skipped,
                    &crate::publisher_helpers::no_config_block_message("winget", crate_name),
                );
                continue;
            }
            log.verbose(&run_per_crate_start_message(crate_name));
            // Re-scope the version/name template vars to THIS crate's own tag so
            // the rendered manifest — AND the snapshot target's version/branch —
            // carry the crate's version, not the first crate's (workspace
            // per-crate independent-version mode). The target snapshot is taken
            // BEFORE the publish path runs (inside the same scope) so a
            // mid-publish failure still leaves the operator a manual PR-close
            // pointer whose recorded branch matches the one actually pushed.
            let target = crate::publisher_helpers::with_published_crate_scope(
                ctx,
                crate_name,
                &anodizer_core::crate_scope::resolve_crate_tag,
                |ctx| {
                    let target = collect_winget_target(ctx, crate_name, &log)?;
                    publish_to_winget(ctx, crate_name, &log)?;
                    Ok(target)
                },
            )?;
            if let Some(t) = target {
                targets.push(t);
            }
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
                "manual winget PR closure required for '{}' version '{}'; \
                 visit https://github.com/{}/{}/pulls?q=is%3Apr+head%3A{}%3A{} \
                 and close the PR (winget validation cannot be reliably \
                 cancelled programmatically mid-flight)",
                t.package_id, t.version, t.upstream_owner, t.upstream_repo, t.fork_owner, t.branch
            ));
        }
        log.status(&format!(
            "{} winget PR(s) require manual closure",
            targets.len()
        ));
        Ok(())
    }

    /// Probe every active winget-pkgs fork for existence + push scope before
    /// any publisher runs: a missing fork or a token without PR scope fails
    /// before the moderation boundary, after sibling publishers may already
    /// have shipped. (A duplicate open PR is covered by the state-query checker.)
    fn preflight(&self, ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        // Best-effort pre-publish gate uses the shallow probe policy.
        let policy = anodizer_core::retry::RetryPolicy::PREFLIGHT;
        Ok(crate::publisher_preflight::for_each_active_github_repo(
            ctx,
            &policy,
            "WINGET_PKGS_TOKEN",
            ctx.config
                .crate_universe()
                .into_iter()
                .filter_map(|c| c.publish.as_ref().and_then(|p| p.winget.as_ref())),
            |w| {
                // Winget has no `skip` field; gate on skip_upload + if only.
                !crate::publisher_helpers::entry_inactive(
                    ctx,
                    None,
                    w.skip_upload.as_ref(),
                    w.if_condition.as_deref(),
                )
            },
            |w| w.repository.as_ref(),
        ))
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

    /// Add an `UploadableBinary` (the portable-installer source) for `crate_name`
    /// on `target`, carrying the sha256 + binary metadata winget needs.
    fn add_uploadable_binary(ctx: &mut Context, crate_name: &str, binary: &str, target: &str) {
        let mut meta = std::collections::HashMap::new();
        meta.insert("sha256".to_string(), "a".repeat(64));
        meta.insert("binary".to_string(), binary.to_string());
        ctx.artifacts.add(anodizer_core::artifact::Artifact {
            kind: anodizer_core::artifact::ArtifactKind::UploadableBinary,
            path: std::path::PathBuf::from(format!("/dist/{crate_name}-{target}")),
            name: format!("{crate_name}-{target}"),
            target: Some(target.to_string()),
            crate_name: crate_name.to_string(),
            metadata: meta,
            size: None,
        });
    }

    /// A per-crate winget crate carrying its own `tag_template` and
    /// `package_identifier`, for the independent-version live-path test.
    fn winget_crate_with(crate_name: &str, tag_template: &str, package_id: &str) -> CrateConfig {
        CrateConfig {
            name: crate_name.to_string(),
            path: ".".to_string(),
            tag_template: tag_template.to_string(),
            publish: Some(PublishConfig {
                winget: Some(WingetConfig {
                    publisher: Some("AcmeCo".to_string()),
                    package_identifier: Some(package_id.to_string()),
                    short_description: Some("A widget management tool".to_string()),
                    license: Some("MIT".to_string()),
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

    /// Add a Windows zip archive (with the sha256 + url metadata the installer
    /// manifest needs) for `crate_name`.
    fn add_windows_zip(ctx: &mut Context, crate_name: &str) {
        let target = "x86_64-pc-windows-msvc";
        let mut meta = std::collections::HashMap::new();
        meta.insert(
            "url".to_string(),
            format!(
                "https://github.com/acme/widget/releases/download/v1.0.0/{crate_name}-{target}.zip"
            ),
        );
        meta.insert("sha256".to_string(), "a".repeat(64));
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
        let mut bin_meta = std::collections::HashMap::new();
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

    /// Add a Windows `Installer` artifact (the `use: msi` / `use: nsis`
    /// source) for `crate_name` on `target`, carrying the `format`, `sha256`,
    /// and `url` metadata winget's installer manifest reads. `format` is the
    /// installer-stage stamp (`msi` from stage-msi, `nsis` from stage-nsis);
    /// `ext` is the on-disk artifact extension (`msi` / `exe`).
    fn add_windows_installer(
        ctx: &mut Context,
        crate_name: &str,
        target: &str,
        format: &str,
        ext: &str,
    ) {
        let mut meta = std::collections::HashMap::new();
        meta.insert(
            "url".to_string(),
            format!(
                "https://github.com/acme/widget/releases/download/v1.0.0/{crate_name}-{target}.{ext}"
            ),
        );
        meta.insert("sha256".to_string(), "b".repeat(64));
        meta.insert("format".to_string(), format.to_string());
        ctx.artifacts.add(anodizer_core::artifact::Artifact {
            kind: anodizer_core::artifact::ArtifactKind::Installer,
            path: std::path::PathBuf::from(format!("/dist/{crate_name}-{target}.{ext}")),
            name: format!("{crate_name}-{target}.{ext}"),
            target: Some(target.to_string()),
            crate_name: crate_name.to_string(),
            metadata: meta,
            size: None,
        });
    }

    /// Add a Windows MSI `Installer` artifact that also carries the
    /// deterministic `product_code` metadata stamp the MSI stage emits.
    fn add_windows_msi_with_product_code(
        ctx: &mut Context,
        crate_name: &str,
        target: &str,
        product_code: &str,
    ) {
        let mut meta = std::collections::HashMap::new();
        meta.insert(
            "url".to_string(),
            format!(
                "https://github.com/acme/widget/releases/download/v1.0.0/{crate_name}-{target}.msi"
            ),
        );
        meta.insert("sha256".to_string(), "b".repeat(64));
        meta.insert("format".to_string(), "msi".to_string());
        meta.insert("product_code".to_string(), product_code.to_string());
        ctx.artifacts.add(anodizer_core::artifact::Artifact {
            kind: anodizer_core::artifact::ArtifactKind::Installer,
            path: std::path::PathBuf::from(format!("/dist/{crate_name}-{target}.msi")),
            name: format!("{crate_name}-{target}.msi"),
            target: Some(target.to_string()),
            crate_name: crate_name.to_string(),
            metadata: meta,
            size: None,
        });
    }

    /// derive-don't-require: with no `winget.product_code` configured, the
    /// resolver falls back to the MSI artifact's stamped `product_code`.
    #[test]
    fn resolve_winget_product_code_derives_from_msi_metadata() {
        let cfg = WingetConfig {
            publisher: Some("AcmeCo".to_string()),
            ..Default::default()
        };
        let mut ctx = TestContextBuilder::new()
            .crates(vec![winget_crate("widget")])
            .build();
        add_windows_msi_with_product_code(
            &mut ctx,
            "widget",
            "x86_64-pc-windows-msvc",
            "{DERIVED-1234}",
        );

        assert_eq!(
            resolve_winget_product_code(&ctx, "widget", &cfg),
            Some("{DERIVED-1234}".to_string()),
        );
    }

    /// Explicit `winget.product_code` always wins over the derived MSI stamp.
    #[test]
    fn resolve_winget_product_code_explicit_config_wins() {
        let cfg = WingetConfig {
            publisher: Some("AcmeCo".to_string()),
            product_code: Some("{EXPLICIT-9999}".to_string()),
            ..Default::default()
        };
        let mut ctx = TestContextBuilder::new()
            .crates(vec![winget_crate("widget")])
            .build();
        add_windows_msi_with_product_code(
            &mut ctx,
            "widget",
            "x86_64-pc-windows-msvc",
            "{DERIVED-1234}",
        );

        assert_eq!(
            resolve_winget_product_code(&ctx, "widget", &cfg),
            Some("{EXPLICIT-9999}".to_string()),
        );
    }

    /// No config and no MSI artifact (e.g. a zip/portable winget config) yields
    /// no ProductCode rather than a fabricated one.
    #[test]
    fn resolve_winget_product_code_none_without_msi_or_config() {
        let cfg = WingetConfig {
            publisher: Some("AcmeCo".to_string()),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new()
            .crates(vec![winget_crate("widget")])
            .build();

        assert_eq!(resolve_winget_product_code(&ctx, "widget", &cfg), None);
    }

    /// `use: msi` must select the real `.msi` `Installer` artifacts, assign
    /// `installer_type: msi`, map the arch, and emit them — NOT bail on "no
    /// Windows archive". Regression guard for the dead-code installer path
    /// (the zip-only filter previously discarded every Installer artifact).
    #[test]
    fn collect_winget_installers_selects_msi_installer() {
        let mut cfg = WingetConfig {
            publisher: Some("AcmeCo".to_string()),
            ..Default::default()
        };
        cfg.use_artifact = Some("msi".to_string());
        let mut ctx = TestContextBuilder::new()
            .crates(vec![winget_crate("widget")])
            .build();
        add_windows_installer(&mut ctx, "widget", "x86_64-pc-windows-msvc", "msi", "msi");

        let installers = collect_winget_installers(
            &ctx,
            "widget",
            &cfg,
            "widget",
            "1.0.0",
            &ctx.logger("publish"),
        )
        .expect("use: msi must collect the real installer artifact");

        assert_eq!(installers.len(), 1, "exactly one installer for one arch");
        assert_eq!(installers[0].installer_type, "msi");
        assert_eq!(installers[0].architecture, "x64");
        assert!(installers[0].url.ends_with(".msi"));
        assert_eq!(installers[0].sha256, "b".repeat(64));
    }

    /// `use: msi` over both x64 and arm64 installers emits a per-arch entry for
    /// each, reusing `map_winget_arch`, with no spurious duplicate-arch bail.
    #[test]
    fn collect_winget_installers_msi_per_arch_x64_and_arm64() {
        let mut cfg = WingetConfig {
            publisher: Some("AcmeCo".to_string()),
            ..Default::default()
        };
        cfg.use_artifact = Some("msi".to_string());
        let mut ctx = TestContextBuilder::new()
            .crates(vec![winget_crate("widget")])
            .build();
        add_windows_installer(&mut ctx, "widget", "x86_64-pc-windows-msvc", "msi", "msi");
        add_windows_installer(&mut ctx, "widget", "aarch64-pc-windows-msvc", "msi", "msi");

        let installers = collect_winget_installers(
            &ctx,
            "widget",
            &cfg,
            "widget",
            "1.0.0",
            &ctx.logger("publish"),
        )
        .expect("two-arch msi must collect both");

        let mut arches: Vec<&str> = installers.iter().map(|i| i.architecture.as_str()).collect();
        arches.sort_unstable();
        assert_eq!(arches, vec!["arm64", "x64"]);
        assert!(installers.iter().all(|i| i.installer_type == "msi"));
    }

    /// `use: nsis` selects the `.exe` NSIS `Installer` artifacts and assigns
    /// `installer_type: nsis` (so the silent switch resolves to `/S`).
    #[test]
    fn collect_winget_installers_selects_nsis_installer() {
        let mut cfg = WingetConfig {
            publisher: Some("AcmeCo".to_string()),
            ..Default::default()
        };
        cfg.use_artifact = Some("nsis".to_string());
        let mut ctx = TestContextBuilder::new()
            .crates(vec![winget_crate("widget")])
            .build();
        add_windows_installer(&mut ctx, "widget", "x86_64-pc-windows-msvc", "nsis", "exe");

        let installers = collect_winget_installers(
            &ctx,
            "widget",
            &cfg,
            "widget",
            "1.0.0",
            &ctx.logger("publish"),
        )
        .expect("use: nsis must collect the real installer artifact");

        assert_eq!(installers.len(), 1);
        assert_eq!(installers[0].installer_type, "nsis");
        assert_eq!(installers[0].architecture, "x64");
        assert!(installers[0].url.ends_with(".exe"));
    }

    /// The selected msi installer feeds an end-to-end manifest carrying the
    /// derived silent switch (`/quiet`) — the install logic that was reachable
    /// only via synthetic test data before FIX 1.
    #[test]
    fn msi_installer_manifest_emits_silent_switch() {
        let mut cfg = WingetConfig {
            publisher: Some("AcmeCo".to_string()),
            ..Default::default()
        };
        cfg.use_artifact = Some("msi".to_string());
        let mut ctx = TestContextBuilder::new()
            .crates(vec![winget_crate("widget")])
            .build();
        add_windows_installer(&mut ctx, "widget", "x86_64-pc-windows-msvc", "msi", "msi");

        let installers = collect_winget_installers(
            &ctx,
            "widget",
            &cfg,
            "widget",
            "1.0.0",
            &ctx.logger("publish"),
        )
        .expect("collect ok");

        let params = WingetManifestParams {
            package_id: "AcmeCo.Widget",
            name: "widget",
            package_name: None,
            version: "1.0.0",
            description: "An app",
            short_description: "An app",
            license: "MIT",
            license_url: None,
            publisher: "AcmeCo",
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
            installers,
            product_code: None,
            release_date: None,
            moniker: None,
            upgrade_behavior: "install",
            documentations: &[],
        };
        let (_ver, inst, _locale) = generate_manifests(&params).unwrap();
        assert!(inst.contains("InstallerType: msi"), "got:\n{inst}");
        assert!(inst.contains("Silent: /quiet"), "got:\n{inst}");
        assert!(!inst.contains("NestedInstallerType"), "msi is not nested");
    }

    /// A `silent_switch` is only meaningful for an actual installer
    /// (msi/wix/exe/nsis) winget runs. When the only Windows artifacts are
    /// zip/portable (which winget unpacks, never runs), the switch is dead
    /// config and the render must warn once so the author isn't misled into
    /// thinking silencing is in effect.
    #[test]
    fn silent_switch_warns_when_only_zip_artifacts() {
        let mut crate_cfg = winget_crate_with("widget", "v{{ .Version }}", "AcmeCo.Widget");
        crate_cfg
            .publish
            .as_mut()
            .unwrap()
            .winget
            .as_mut()
            .unwrap()
            .silent_switch = Some("/qn".to_string());

        let capture = anodizer_core::log::LogCapture::new();
        let mut ctx = TestContextBuilder::new().crates(vec![crate_cfg]).build();
        ctx.with_log_capture(capture.clone());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("RawVersion", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        add_windows_zip(&mut ctx, "widget");

        render_winget_manifests_for_crate(&ctx, "widget", &ctx.logger("publish"))
            .expect("render ok")
            .expect("widget not skipped");

        let warns = capture.warn_messages();
        assert!(
            warns.iter().any(|m| m.contains("widget")
                && m.contains("silent_switch")
                && m.contains("no installer-type artifact")),
            "expected one WARN that silent_switch is ignored with no installer artifact; \
             got: {warns:?}"
        );
    }

    /// Mirror: with no `silent_switch` configured, the zip-only render must
    /// stay silent — the diagnostic is gated on the switch actually being set.
    #[test]
    fn no_silent_switch_warning_when_unset() {
        let crate_cfg = winget_crate_with("widget", "v{{ .Version }}", "AcmeCo.Widget");

        let capture = anodizer_core::log::LogCapture::new();
        let mut ctx = TestContextBuilder::new().crates(vec![crate_cfg]).build();
        ctx.with_log_capture(capture.clone());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("RawVersion", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        add_windows_zip(&mut ctx, "widget");

        render_winget_manifests_for_crate(&ctx, "widget", &ctx.logger("publish"))
            .expect("render ok")
            .expect("widget not skipped");

        let warns = capture.warn_messages();
        assert!(
            !warns.iter().any(|m| m.contains("silent_switch")),
            "no silent_switch configured → no silent_switch warning; got: {warns:?}"
        );
    }

    /// LIVE PATH, workspace per-crate INDEPENDENT-version mode: the publisher's
    /// per-crate render must stamp EACH crate's OWN version, not the first
    /// crate's. The live `run` loop wraps each `publish_to_winget` in
    /// `with_published_crate_scope`; this drives that same helper and asserts the
    /// rendered manifest carries the scoped crate's version. Fails against the
    /// pre-fix code that rendered every crate against the global first-crate
    /// `Version`.
    #[test]
    fn live_per_crate_render_stamps_each_crate_own_version() {
        let alpha = winget_crate_with("alpha", "alpha-v{{ .Version }}", "AcmeCo.Alpha");
        let beta = winget_crate_with("beta", "beta-v{{ .Version }}", "AcmeCo.Beta");

        // One ctx, both crates, global Version = first crate's (2.0.0).
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![alpha, beta])
            .build();
        ctx.template_vars_mut().set("Version", "2.0.0");
        ctx.template_vars_mut().set("RawVersion", "2.0.0");
        ctx.template_vars_mut().set("Tag", "alpha-v2.0.0");
        add_windows_zip(&mut ctx, "alpha");
        add_windows_zip(&mut ctx, "beta");

        // Per-crate resolver: alpha @ 2.0.0, beta @ 3.1.0.
        let resolver = |_: &Context, c: &CrateConfig| {
            Some(match c.name.as_str() {
                "beta" => "3.1.0".to_string(),
                _ => "2.0.0".to_string(),
            })
        };

        // beta renders UNDER ITS OWN SCOPE → 3.1.0, the version a real release
        // would stamp; never the global first-crate 2.0.0.
        let beta_yaml = crate::publisher_helpers::with_published_crate_scope(
            &mut ctx,
            "beta",
            &resolver,
            |ctx| {
                let r = render_winget_manifests_for_crate(ctx, "beta", &ctx.logger("publish"))?
                    .expect("beta not skipped");
                Ok(r.version_yaml)
            },
        )
        .expect("scoped render ok");
        assert!(
            beta_yaml.contains("PackageVersion: 3.1.0"),
            "live per-crate render must stamp beta's OWN version 3.1.0; got:\n{beta_yaml}"
        );
        assert!(
            !beta_yaml.contains("PackageVersion: 2.0.0"),
            "beta's live manifest must NOT carry the first crate's version; got:\n{beta_yaml}"
        );

        // alpha renders 2.0.0 under its own scope (single/lockstep parity: the
        // per-crate scope reproduces the same version it already had).
        let alpha_yaml = crate::publisher_helpers::with_published_crate_scope(
            &mut ctx,
            "alpha",
            &resolver,
            |ctx| {
                let r = render_winget_manifests_for_crate(ctx, "alpha", &ctx.logger("publish"))?
                    .expect("alpha not skipped");
                Ok(r.version_yaml)
            },
        )
        .expect("scoped render ok");
        assert!(
            alpha_yaml.contains("PackageVersion: 2.0.0"),
            "alpha must render its own 2.0.0; got:\n{alpha_yaml}"
        );
    }

    /// Per-crate, no leakage: each crate's Moniker derives from its OWN
    /// single binary name (`add_windows_zip` stamps `binary = crate_name`).
    /// alpha's manifest must carry `Moniker: alpha` and never `beta`, and
    /// vice-versa — the recurring cross-crate-leakage bug family.
    #[test]
    fn live_per_crate_moniker_no_leakage() {
        let alpha = winget_crate_with("alpha", "alpha-v{{ .Version }}", "AcmeCo.Alpha");
        let beta = winget_crate_with("beta", "beta-v{{ .Version }}", "AcmeCo.Beta");

        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![alpha, beta])
            .build();
        ctx.template_vars_mut().set("Version", "2.0.0");
        ctx.template_vars_mut().set("RawVersion", "2.0.0");
        ctx.template_vars_mut().set("Tag", "alpha-v2.0.0");
        add_windows_zip(&mut ctx, "alpha");
        add_windows_zip(&mut ctx, "beta");

        let alpha_locale = render_winget_manifests_for_crate(&ctx, "alpha", &ctx.logger("publish"))
            .unwrap()
            .expect("alpha not skipped")
            .locale_yaml;
        let beta_locale = render_winget_manifests_for_crate(&ctx, "beta", &ctx.logger("publish"))
            .unwrap()
            .expect("beta not skipped")
            .locale_yaml;

        assert!(
            alpha_locale.contains("Moniker: alpha"),
            "alpha must derive its own moniker; got:\n{alpha_locale}"
        );
        assert!(
            !alpha_locale.contains("Moniker: beta"),
            "alpha manifest must NOT carry beta's moniker; got:\n{alpha_locale}"
        );
        assert!(
            beta_locale.contains("Moniker: beta"),
            "beta must derive its own moniker; got:\n{beta_locale}"
        );
        assert!(
            !beta_locale.contains("Moniker: alpha"),
            "beta manifest must NOT carry alpha's moniker; got:\n{beta_locale}"
        );
    }

    /// Single-crate live path: the default install behavior and a
    /// derived moniker both surface through the real per-crate render.
    #[test]
    fn live_single_crate_default_moniker_and_upgrade_behavior() {
        let demo = winget_crate_with("demo", "v{{ .Version }}", "AcmeCo.Demo");
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![demo])
            .build();
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("RawVersion", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        add_windows_zip(&mut ctx, "demo");

        let rendered = render_winget_manifests_for_crate(&ctx, "demo", &ctx.logger("publish"))
            .unwrap()
            .expect("demo not skipped");
        assert!(
            rendered.locale_yaml.contains("Moniker: demo"),
            "single-binary moniker derives from the bin name; got:\n{}",
            rendered.locale_yaml
        );
        assert!(
            rendered.installer_yaml.contains("UpgradeBehavior: install"),
            "default upgrade behavior must be install; got:\n{}",
            rendered.installer_yaml
        );
        assert!(
            !rendered.installer_yaml.contains("uninstallPrevious"),
            "default must not be the clobbering uninstallPrevious"
        );
    }

    /// The shard-guard and the live collector must agree on what a winget
    /// Windows installer is. A linux-only `UploadableBinary` (the portable
    /// path) is NOT a Windows installer, so the guard returns `false` — letting
    /// the schema validator skip the crate rather than drive
    /// `collect_winget_installers` into its "no Windows artifact" bail. The
    /// shared `WingetArtifactFilters::matches` Windows predicate keeps the two
    /// from drifting; this pins the portable-binary branch of that agreement.
    #[test]
    fn guard_skips_linux_only_portable_binary() {
        let cfg = WingetConfig {
            publisher: Some("AcmeCo".to_string()),
            ..Default::default()
        };
        let mut ctx = TestContextBuilder::new()
            .crates(vec![winget_crate("demo")])
            .build();
        add_uploadable_binary(&mut ctx, "demo", "demo", "x86_64-unknown-linux-gnu");
        assert!(
            !crate_has_winget_installer_artifacts(&ctx, "demo", &cfg),
            "a linux-only portable binary is not a Windows installer; the guard \
             must return false so the validator skips rather than bails"
        );
    }

    /// The positive half: a Windows `UploadableBinary` IS a winget installer, so
    /// the guard returns `true` and validation proceeds. Confirms the Windows
    /// predicate the guard shares with the collector counts the real case.
    #[test]
    fn guard_counts_windows_portable_binary() {
        let cfg = WingetConfig {
            publisher: Some("AcmeCo".to_string()),
            ..Default::default()
        };
        let mut ctx = TestContextBuilder::new()
            .crates(vec![winget_crate("demo")])
            .build();
        add_uploadable_binary(&mut ctx, "demo", "demo", "x86_64-pc-windows-msvc");
        assert!(
            crate_has_winget_installer_artifacts(&ctx, "demo", &cfg),
            "a windows portable binary is a winget installer; the guard must count it"
        );
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

    /// Every winget publish lands as a PR against the upstream index;
    /// `gh pr create` is the preferred transport with a full REST-API
    /// fallback, so `gh` is ADVISORY — recommended, never a blocker.
    #[test]
    fn winget_advisory_requirements_emit_gh_when_active() {
        let ctx = TestContextBuilder::new()
            .crates(vec![winget_crate("demo")])
            .build();
        let reqs = WingetPublisher::new().advisory_requirements(&ctx);
        assert!(
            reqs.iter().any(|r| matches!(
                r,
                anodizer_core::EnvRequirement::Tool { name } if name == "gh"
            )),
            "active winget entry must recommend gh: {reqs:?}"
        );
    }

    #[test]
    fn winget_advisory_requirements_empty_when_all_entries_skipped() {
        let mut c = winget_crate("demo");
        if let Some(w) = c.publish.as_mut().and_then(|p| p.winget.as_mut()) {
            w.skip_upload = Some(anodizer_core::config::StringOrBool::Bool(true));
        }
        let ctx = TestContextBuilder::new().crates(vec![c]).build();
        let reqs = WingetPublisher::new().advisory_requirements(&ctx);
        assert!(
            reqs.is_empty(),
            "every entry skipped ⇒ no advisory recommendations: {reqs:?}"
        );
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
        let t = collect_winget_target(&ctx, "demo", &ctx.logger("publish"))
            .expect("render ok")
            .expect("target");
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
        let t = collect_winget_target(&ctx, "demo", &ctx.logger("publish"))
            .expect("render ok")
            .expect("target");
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
    fn run_per_crate_start_message_names_crate() {
        let msg = run_per_crate_start_message("demo");
        assert!(
            msg.starts_with("starting per-crate winget publish"),
            "{msg}"
        );
        assert!(msg.contains("'demo'"), "{msg}");
    }

    #[test]
    fn run_done_message_reports_processed_count() {
        let msg = run_done_message(2);
        assert!(msg.starts_with("finished winget publish"), "{msg}");
        assert!(msg.contains("2 configured crate(s) processed"), "{msg}");
    }

    #[test]
    fn run_no_eligible_crates_warning_names_remediation() {
        let msg = run_no_eligible_crates_warning(5);
        assert!(msg.starts_with("winget publisher registered"), "{msg}");
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
        assert!(msg.starts_with("winget publisher registered"), "{msg}");
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
        let repo = crate::testing::hermetic_tagged_repo();
        let mut ctx = TestContextBuilder::new()
            .crates(vec![winget_crate("demo")])
            .selected_crates(vec!["demo".to_string()])
            .dry_run(true)
            .project_root(repo.path().to_path_buf())
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
        let repo = crate::testing::hermetic_tagged_repo();
        let mut ctx = TestContextBuilder::new()
            .crates(vec![winget_crate("demo")])
            // selected_crates intentionally left at the default Vec::new()
            .dry_run(true)
            .project_root(repo.path().to_path_buf())
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
        let repo = crate::testing::hermetic_tagged_repo();
        let mut ctx = TestContextBuilder::new()
            .crates(vec![winget_crate("demo")])
            .selected_crates(vec!["demo".to_string()])
            .dry_run(true)
            .project_root(repo.path().to_path_buf())
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
                silent_switch_override: None,
            }],
            product_code: None,
            release_date: None,
            moniker: Some("mytool"),
            upgrade_behavior: "install",
            documentations: &[],
        }
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
        let msg =
            render_winget_commit_msg(None, "Org.MyTool", "1.0.0", &commit_msg_logger(), false)
                .expect("default template renders");
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
        use std::sync::OnceLock;

        fn quiet() -> StageLogger {
            StageLogger::new("publish", Verbosity::Quiet)
        }

        /// Give the test process a git identity + non-interactive credential
        /// behaviour so the publish path's `git commit` works on a bare CI
        /// runner. One-shot per process.
        fn ensure_git_identity() {
            static INIT: OnceLock<()> = OnceLock::new();
            INIT.get_or_init(|| {
                // SAFETY: runs once per process under OnceLock; constants only.
                unsafe {
                    std::env::set_var("GIT_AUTHOR_NAME", "Anodize Test"); // env-ok: idempotent OnceLock set of constant git identity, never mutated after
                    std::env::set_var("GIT_AUTHOR_EMAIL", "test@anodize.local"); // env-ok: idempotent OnceLock set of constant git identity, never mutated after
                    std::env::set_var("GIT_COMMITTER_NAME", "Anodize Test"); // env-ok: idempotent OnceLock set of constant git identity, never mutated after
                    std::env::set_var("GIT_COMMITTER_EMAIL", "test@anodize.local"); // env-ok: idempotent OnceLock set of constant git identity, never mutated after
                    std::env::set_var("GIT_TERMINAL_PROMPT", "0"); // env-ok: idempotent OnceLock set of constant git identity, never mutated after
                }
            });
        }

        fn git_ok(dir: &Path, args: &[&str]) {
            let out = anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(args).current_dir(dir);
                    cmd
                },
                "git",
            );
            assert!(out.status.success(), "git {args:?} failed");
        }

        fn git_stdout(dir: &Path, args: &[&str]) -> String {
            let out = anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(args).current_dir(dir);
                    cmd
                },
                "git",
            );
            assert!(out.status.success(), "git {args:?} failed: {out:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }

        /// Build a bare "winget-pkgs fork" repo with one commit on `main`
        /// (the branch the publish path's `--depth=1` clone defaults to).
        /// Returns `(bare_path_string, _bare_holder)`. The live publish
        /// clones this, writes the 3-file manifest set, commits a versioned
        /// branch, and pushes it back here.
        fn init_bare_fork() -> (String, tempfile::TempDir) {
            ensure_git_identity();
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
                tag_template: "v{{ .Version }}".to_string(),
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
            let payload: serde_json::Value =
                serde_json::from_str(&entries[0].body).expect("JSON body");
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

            let err = publish_to_winget(&mut ctx, "widget", &quiet())
                .expect_err("missing sha256 must bail");
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
}
