use super::*;

/// Artifact kind per nfpm packager format: `msix` is a Windows app package
/// and rides the same `Installer` kind as MSI/NSIS outputs (checksummed,
/// signed, released); everything else is a Linux package.
pub(crate) fn artifact_kind_for_format(format: &str) -> ArtifactKind {
    if format == "msix" {
        ArtifactKind::Installer
    } else {
        ArtifactKind::LinuxPackage
    }
}

/// Return the file extension for a given nfpm packager format.
pub(crate) fn format_extension(format: &str) -> &str {
    match format {
        "deb" => ".deb",
        // GoReleaser keeps the full format as the extension for Termux
        // (`ext := "." + format`, explicitly skipping the deb packager's
        // ConventionalExtension), so Termux repos can distinguish the file.
        "termux.deb" => ".termux.deb",
        "rpm" => ".rpm",
        "apk" => ".apk",
        "archlinux" => ".pkg.tar.zst",
        "ipk" => ".ipk",
        "msix" => ".msix",
        _ => "",
    }
}

/// Emit a Debian `lintian` override file and inject the matching content
/// entry into the rendered nfpm config, then clear the now-orphaned
/// `lintian_overrides:` field so the YAML output stays clean.
///
/// Lintian-override setup.
/// writes a file to `<dist>/<format>/<package>_<arch>/lintian` whose body
/// is one `<package>: <override>` line per entry in `deb.lintian_overrides`,
/// then appends a `Content` mapping that path into the package at
/// `/usr/share/lintian/overrides/<package>` (mode 0644, packager-scoped to
/// `"deb"`). Anodizer previously parsed `deb.lintian_overrides` into a YAML
/// key but `nfpm` itself does not consume that key, so the override file
/// was silently dropped from the resulting `.deb` / `termux.deb`.
///
/// This helper performs the file emission and content injection in
/// emitted in lockstep. When `dry_run` is true the on-disk write is skipped
/// (the content entry is still injected so the generated YAML reflects
/// what would ship). The helper is a no-op for non-deb formats and for
/// configs where `lintian_overrides` is unset / empty.
///
/// Returns an error only when the on-disk write fails — a configured
/// override list always reaches the `contents:` array.
pub(crate) fn setup_lintian_overrides(
    rendered_cfg: &mut anodizer_core::config::NfpmConfig,
    format: &str,
    pkg_name: &str,
    arch: &str,
    dist: &std::path::Path,
    dry_run: bool,
) -> Result<()> {
    if format != "deb" && format != "termux.deb" {
        return Ok(());
    }
    let Some(deb_cfg) = rendered_cfg.deb.as_mut() else {
        return Ok(());
    };
    let Some(overrides) = deb_cfg.lintian_overrides.take() else {
        return Ok(());
    };
    if overrides.is_empty() {
        return Ok(());
    }

    let pkg_dir = dist.join(format).join(format!("{pkg_name}_{arch}"));
    let lintian_path = pkg_dir.join("lintian");
    if !dry_run {
        fs::create_dir_all(&pkg_dir)
            .with_context(|| format!("nfpm lintian: create dir {}", pkg_dir.display()))?;
        let body: String = overrides
            .iter()
            .map(|ov| format!("{pkg_name}: {ov}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&lintian_path, body)
            .with_context(|| format!("nfpm lintian: write {}", lintian_path.display()))?;
    }

    let entry = anodizer_core::config::NfpmContent {
        src: lintian_path.to_string_lossy().into_owned(),
        dst: format!("/usr/share/lintian/overrides/{pkg_name}"),
        content_type: None,
        file_info: Some(anodizer_core::config::NfpmFileInfo {
            owner: None,
            group: None,
            mode: Some(anodizer_core::config::StringOrU32(0o644)),
            mtime: None,
        }),
        packager: Some("deb".to_string()),
        expand: None,
    };
    rendered_cfg
        .contents
        .get_or_insert_with(Vec::new)
        .push(entry);
    Ok(())
}

// ---------------------------------------------------------------------------
// Private helpers — sliced out of `run` to keep the body navigable.
// ---------------------------------------------------------------------------

pub(crate) fn validate_unique_config_ids(
    crates: &[anodizer_core::config::CrateConfig],
) -> Result<()> {
    let mut seen_ids = std::collections::HashSet::new();
    for krate in crates {
        if let Some(ref nfpm_configs) = krate.nfpms {
            for cfg in nfpm_configs {
                let id = cfg.id.as_deref().unwrap_or("default");
                if !seen_ids.insert(id.to_string()) {
                    bail!(
                        "nfpm: duplicate config ID '{}' (each nfpm config must have a unique ID)",
                        id
                    );
                }
            }
        }
    }
    Ok(())
}

/// Evaluate per-config skip predicates (`if`, empty `formats`).
///
/// Returns `Ok(true)` when the caller should `continue` (skip this config),
/// `Ok(false)` to proceed. The deb/apk maintainer requirement is enforced
/// later, per-format, in [`require_deb_apk_maintainer`] — only once the
/// Cargo-derived fallback has been applied and the format is known, so a
/// derivable maintainer or an rpm-only build doesn't trip it.
pub(crate) fn should_skip_nfpm_config(
    ctx: &mut Context,
    nfpm_cfg: &anodizer_core::config::NfpmConfig,
    nfpm_id_for_log: &str,
    log: &anodizer_core::log::StageLogger,
) -> Result<bool> {
    if !nfpm_config_if_proceeds(ctx, nfpm_cfg, nfpm_id_for_log)? {
        let reason = "`if` condition evaluated falsy".to_string();
        log.verbose(&format!(
            "skipped nfpm config '{}' — {}",
            nfpm_id_for_log, reason
        ));
        ctx.remember_skip("nfpm", nfpm_id_for_log, &reason);
        return Ok(true);
    }

    if nfpm_cfg.formats.is_empty() {
        ctx.strict_guard(
            log,
            &format!(
                "skipped nfpm config '{}' — no output formats configured",
                nfpm_id_for_log
            ),
        )?;
        return Ok(true);
    }

    Ok(false)
}

/// Returns `true` when a packaging format requires a non-empty `Maintainer`
/// to be installable. The deb family carries a mandatory `Maintainer` control
/// field:
///
/// - `deb` / `termux.deb` — Debian Policy 5.3 makes `Maintainer` mandatory;
///   lintian rejects an empty field and apt renders the package as "unknown".
/// - `apk` — Alpine's `APKINDEX` carries the maintainer the same way.
/// - `ipk` — the opkg control file is deb-derived and carries a `Maintainer`
///   line; nfpm warns and substitutes a placeholder when it is unset, so an
///   ipk with no maintainer ships incomplete metadata just like its deb sibling.
///
/// `rpm` and `archlinux` tolerate a missing packager differently (rpm's
/// `Packager` tag is optional; an Arch `.PKGINFO` has no required maintainer),
/// so they are not gated.
fn format_requires_maintainer(format: &str) -> bool {
    matches!(format, "deb" | "termux.deb" | "apk" | "ipk")
}

/// Resolve the effective maintainer for a crate's nfpm config: the explicit
/// `nfpm.maintainer`, else the first Cargo `authors` entry (via
/// `meta_first_maintainer_for`). Returns the trimmed value, or `None` when
/// neither source supplies one.
///
/// Mirrors the derivation order in [`render_nfpm_config_fields`] so the
/// pre-flight check and the rendered YAML agree on what the maintainer is.
fn resolve_effective_maintainer<'a>(
    config: &'a anodizer_core::config::Config,
    nfpm_cfg: &'a anodizer_core::config::NfpmConfig,
    crate_name: &str,
) -> Option<&'a str> {
    nfpm_cfg
        .maintainer
        .as_deref()
        .or_else(|| config.meta_first_maintainer_for(crate_name))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Hard-fail when a deb-family package (`deb`/`termux.deb`/`apk`/`ipk`) is
/// being built but no maintainer can be resolved — neither from
/// `nfpm.maintainer` nor a derivable Cargo `authors` entry. These formats all
/// carry a mandatory `Maintainer` control field; an empty one ships incomplete
/// metadata the repository index marks "unknown", so shipping it is a release
/// defect, not a warning. Scoped via [`format_requires_maintainer`]: an
/// rpm-only or archlinux-only build still succeeds.
///
/// This is a Rust-additive correctness improvement beyond GoReleaser (which
/// only warns), per the repo rule against advisory/continue-on-error on a
/// genuinely-broken output.
/// nfpm's msix packager hard-requires `publisher`, `properties.logo`, and at
/// least one application with `id` + `executable`, rejecting the package at
/// pack time otherwise; failing here, before any packaging work, turns those
/// mid-run errors into actionable config diagnostics. An absent
/// `applications:` list is fine — anodizer derives one per packaged binary —
/// but an explicitly-supplied entry must be complete.
pub(crate) fn require_msix_essentials(
    nfpm_cfg: &anodizer_core::config::NfpmConfig,
    crate_name: &str,
    format: &str,
) -> Result<()> {
    if format != "msix" {
        return Ok(());
    }
    let id = nfpm_cfg.id.as_deref().unwrap_or("default");
    let msix = nfpm_cfg.msix.as_ref();
    let publisher = msix
        .and_then(|m| m.publisher.as_deref())
        .map(str::trim)
        .unwrap_or("");
    if publisher.is_empty() {
        bail!(
            "nfpm config '{id}' builds an 'msix' package for crate '{crate_name}' but \
             `msix.publisher` is empty — nfpm requires the publisher identity (it must \
             match the signing certificate subject). Set `msix.publisher` \
             (e.g. `publisher: \"CN=My Company, O=My Company, C=US\"`)."
        );
    }
    let logo = msix
        .and_then(|m| m.properties.as_ref())
        .and_then(|p| p.logo.as_deref())
        .map(str::trim)
        .unwrap_or("");
    if logo.is_empty() {
        bail!(
            "nfpm config '{id}' builds an 'msix' package for crate '{crate_name}' but \
             `msix.properties.logo` is empty — nfpm requires a logo image for MSIX packages \
             and rejects the package without one. Set `msix.properties.logo` to a path to a \
             PNG (e.g. `logo: assets/logo.png`)."
        );
    }
    if let Some(apps) = msix.and_then(|m| m.applications.as_ref()) {
        for (i, app) in apps.iter().enumerate() {
            let missing = if app.id.as_deref().map(str::trim).unwrap_or("").is_empty() {
                Some("id")
            } else if app
                .executable
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
            {
                Some("executable")
            } else {
                None
            };
            if let Some(field) = missing {
                bail!(
                    "nfpm config '{id}' builds an 'msix' package for crate '{crate_name}' but \
                     `msix.applications[{i}].{field}` is empty — nfpm requires every \
                     application to declare both `id` and `executable`. Fill in the field, or \
                     omit `msix.applications` entirely to let anodizer derive one application \
                     per packaged binary."
                );
            }
        }
    }
    Ok(())
}

pub(crate) fn require_deb_apk_maintainer(
    config: &anodizer_core::config::Config,
    nfpm_cfg: &anodizer_core::config::NfpmConfig,
    crate_name: &str,
    format: &str,
) -> Result<()> {
    if !format_requires_maintainer(format) {
        return Ok(());
    }
    if resolve_effective_maintainer(config, nfpm_cfg, crate_name).is_some() {
        return Ok(());
    }
    let id = nfpm_cfg.id.as_deref().unwrap_or("default");
    bail!(
        "nfpm config '{id}' builds a '{format}' package for crate '{crate_name}' but its \
         Maintainer field is empty and could not be derived. A '{format}' package with no \
         Maintainer ships incomplete metadata — the repository index marks it \"unknown\" \
         (and for deb, lintian rejects it). Set it via the `maintainer:` field on this nfpm \
         config (e.g. `maintainer: \"Jane Doe <jane@example.com>\"`) or add an `authors` \
         entry to the crate's Cargo.toml so anodizer can derive it."
    );
}

/// Evaluate one nfpm config's `if:` gate against the current template vars.
///
/// `Ok(true)` means the config proceeds; `Ok(false)` means a falsy `if:`
/// suppresses it (the build skips it, and the offline renderer emits no
/// YAML for it). Shared by the build's `should_skip_nfpm_config` and the
/// offline `nfpm_yaml_configs_for_crate` so a single render decides both.
pub(crate) fn nfpm_config_if_proceeds(
    ctx: &Context,
    nfpm_cfg: &anodizer_core::config::NfpmConfig,
    nfpm_id_for_log: &str,
) -> Result<bool> {
    anodizer_core::config::evaluate_if_condition(
        nfpm_cfg.if_condition.as_deref(),
        &format!("nfpm config '{nfpm_id_for_log}'"),
        |t| ctx.render_template(t),
    )
}

/// Collect the packaging-eligible artifacts for one crate: every Binary /
/// Header / CArchive / CShared artifact whose target triple nfpm can package
/// (`is_nfpm_target`). Both the build's `run` loop and the offline
/// `nfpm_yaml_configs_for_crate` renderer start from this exact set so the
/// validated (config × target × format) universe equals the built one.
pub(crate) fn nfpm_eligible_artifacts(ctx: &Context, crate_name: &str) -> Vec<Artifact> {
    let nfpm_artifact_kinds = &[
        ArtifactKind::Binary,
        ArtifactKind::Header,
        ArtifactKind::CArchive,
        ArtifactKind::CShared,
    ];
    ctx.artifacts
        .by_kinds_and_crate(nfpm_artifact_kinds, crate_name)
        .into_iter()
        .filter(|b| {
            b.target
                .as_deref()
                // Windows binaries are eligible solely for the msix format;
                // the per-format windows↔msix gate skips them everywhere else.
                .map(|t| {
                    anodizer_core::target::is_nfpm_target(t) || anodizer_core::target::is_windows(t)
                })
                .unwrap_or(false)
        })
        .cloned()
        .collect()
}
