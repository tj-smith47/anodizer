use super::*;

/// Resolve the publisher name, falling back to the GitHub repo owner when
/// the config omits an explicit publisher. Errors when both are empty.
pub(crate) fn resolve_winget_publisher_name<'a>(
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
pub(crate) fn resolve_winget_description(
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
pub(crate) fn resolve_winget_short_description(
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
pub(crate) fn resolve_winget_license<'a>(
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
pub(crate) fn collect_windows_binary_names_by_target(
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
pub(crate) fn resolve_winget_moniker(
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
pub(crate) fn map_winget_arch(raw_arch: &str) -> &str {
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
pub(crate) fn resolve_installer_url(
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
