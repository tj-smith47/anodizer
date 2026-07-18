use super::*;

/// Artifact-selection filters for windows winget installers: windows-only,
/// optional id allow-list, and amd64_variant selection.
pub(crate) struct WingetArtifactFilters<'a> {
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
pub(crate) fn is_winget_zip_archive(a: &anodizer_core::artifact::Artifact) -> bool {
    a.metadata.get("format").map(|f| f.as_str()) == Some("zip")
        || a.path.to_string_lossy().ends_with(".zip")
}

/// Build a single zip-archive [`WingetInstallerItem`] from a matching
/// archive artifact. Errors when the archive has no sha256 (which would
/// produce a manifest the winget validator rejects).
pub(crate) fn build_archive_installer(
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
pub(crate) fn installer_type_for(format: Option<&str>, use_artifact: Option<&str>) -> &'static str {
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
pub(crate) fn build_executable_installer(
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
pub(crate) fn build_portable_installer(
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
pub(crate) fn collect_winget_installers(
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
pub(crate) fn resolve_winget_product_code(
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
pub(crate) fn is_executable_installer_type(installer_type: &str) -> bool {
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
pub(crate) fn resolve_winget_release_date(ctx: &Context) -> Option<String> {
    ctx.template_vars()
        .get("Date")
        .map(|d| d.chars().take(10).collect::<String>())
        .filter(|s| s.len() == 10 && s.as_bytes()[4] == b'-' && s.as_bytes()[7] == b'-')
}
