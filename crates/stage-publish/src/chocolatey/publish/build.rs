use std::path::Path;

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};

use crate::util;

use super::super::install::{
    FileType, InstallScriptDual, generate_install_script, generate_install_script_dual,
};
use super::super::nuspec::{NuspecParams, generate_nuspec};
use super::super::package::{FeedHashResult, compute_nupkg_hash, create_nupkg, package_feed_hash};

use super::*;

/// Resolves the required nuspec metadata (description, license, authors,
/// project/icon URLs, tags) by walking `choco.*` → `metadata.*` fallbacks
/// and template-rendering where applicable. Fails when `license` is
/// unresolvable, because the Chocolatey gallery rejects empty licenses.
pub(super) fn resolve_metadata(
    ctx: &Context,
    choco_cfg: &anodizer_core::config::ChocolateyConfig,
    crate_name: &str,
    repo_owner: &str,
    repo_name: &str,
    log: &StageLogger,
) -> Result<ChocoMetadata> {
    // Fall back to project `metadata.*` when choco config unset.
    let description_raw = choco_cfg
        .description
        .as_deref()
        .or_else(|| ctx.config.meta_description_for(crate_name))
        .unwrap_or(crate_name);
    let description =
        crate::util::render_or_warn(ctx, log, "chocolatey.description", description_raw)?;
    let license = choco_cfg
        .license
        .clone()
        .or_else(|| ctx.config.meta_license_for(crate_name).map(str::to_string))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "chocolatey: license is required but not configured for crate '{}'. \
                 The SPDX expression drives the <licenseUrl> derivation \
                 (Chocolatey CLI does not support the NuGet <license> element \
                 — CHCU0002), and Chocolatey gallery moderators expect license \
                 metadata. Set `publish.chocolatey.license` (SPDX \
                 identifier, e.g. \"MIT\" or \"MIT OR Apache-2.0\") or top-level \
                 `metadata.license`.",
                crate_name,
            )
        })?;
    let authors = choco_cfg
        .authors
        .clone()
        .or_else(|| {
            ctx.config
                .meta_first_maintainer_for(crate_name)
                .map(str::to_string)
        })
        .unwrap_or_else(|| crate_name.to_string());
    // The repo URL (`https://github.com/{owner}/{name}`) is the common base
    // for <projectUrl>, <projectSourceUrl>, <bugTrackerUrl>, and the derived
    // <licenseUrl>. None when the release repo is unknown (internal feed).
    let repo_url = (!repo_owner.is_empty() && !repo_name.is_empty())
        .then(|| format!("https://github.com/{}/{}", repo_owner, repo_name));

    // Explicit-config URL values are user-templated and must be rendered;
    // derived defaults (repo_url / derive_license_blob_url) already resolved
    // the release Tag, so they are NOT routed through here (double-render).
    let render_cfg = |field: &str, raw: &str| -> Result<String> {
        crate::util::render_or_warn(ctx, log, field, raw)
    };

    let project_url = match choco_cfg.project_url.as_deref() {
        Some(raw) => render_cfg("chocolatey.project_url", raw)?,
        // <projectUrl> is optional per nuspec.xsd (`xs:element minOccurs="0"`);
        // empty value is suppressed by the Tera `{% if project_url %}` guard
        // in nuspec.rs so no broken tag ships.
        None => repo_url.clone().unwrap_or_default(),
    };

    // <projectSourceUrl> defaults to the repo URL — real packages always set
    // it and moderators expect it; explicit config wins.
    let project_source_url = match choco_cfg.project_source_url.as_deref() {
        Some(raw) => Some(render_cfg("chocolatey.project_source_url", raw)?),
        None => repo_url.clone(),
    };

    // <bugTrackerUrl> defaults to `{repo}/issues`; explicit config wins.
    let bug_tracker_url = match choco_cfg.bug_tracker_url.as_deref() {
        Some(raw) => Some(render_cfg("chocolatey.bug_tracker_url", raw)?),
        None => repo_url.as_ref().map(|u| format!("{u}/issues")),
    };

    // <licenseUrl> — Chocolatey's ONLY license metadata channel (its
    // LicenseMetadataRule warns CHCU0002 on any NuGet <license> element, so
    // no SPDX-expression element is ever emitted): explicit config wins;
    // else derive a real GitHub LICENSE blob URL
    // (`{repo}/blob/{ref}/LICENSE`) at the release tag — what every real
    // exemplar (ripgrep/fd/gh) ships. NEVER synthesize an opensource.org
    // URL: it 404s for compound SPDX and gets the package
    // moderation-rejected. None when the repo is unknown → no <licenseUrl>.
    //
    // A compound SPDX expression (`MIT OR Apache-2.0`) has no single canonical
    // license file, so a derived `…/blob/<ref>/LICENSE` URL would both
    // misrepresent the dual license AND 404 in a dual-licensed repo that ships
    // `LICENSE-MIT` + `LICENSE-APACHE` rather than a bare `LICENSE`. An
    // explicit `license_url` still wins.
    let license_url = match choco_cfg.license_url.as_deref() {
        Some(raw) => Some(render_cfg("chocolatey.license_url", raw)?),
        None if !anodizer_core::license::parse_spdx_expression(&license).is_single() => None,
        None => repo_url.as_ref().map(|u| derive_license_blob_url(ctx, u)),
    };

    // <iconUrl> is optional per nuspec.xsd; nuspec.rs only emits the tag when
    // the value is non-empty (gated on `icon_url` truthy in the Tera template).
    let icon_url = match choco_cfg.icon_url.as_deref() {
        Some(raw) => render_cfg("chocolatey.icon_url", raw)?,
        None => String::new(),
    };
    // <tags> is optional per nuspec.xsd; when no tags are configured the
    // generator falls back to the package name (nuspec.rs line ~93).
    let tags = choco_cfg
        .tags
        .clone()
        .unwrap_or_default()
        .iter()
        .map(|t| render_cfg("chocolatey.tags", t))
        .collect::<Result<Vec<String>>>()?;
    let meta = ChocoMetadata {
        description,
        license,
        license_url,
        authors,
        project_url,
        icon_url,
        tags,
        project_source_url,
        bug_tracker_url,
    };
    if meta.license_url.is_none() {
        let reason = if anodizer_core::license::parse_spdx_expression(&meta.license).is_single() {
            "the release repository is unknown, so no LICENSE blob URL is derivable"
        } else {
            "a compound SPDX expression has no single canonical LICENSE file to derive"
        };
        log.warn(&format!(
            "chocolatey: no <licenseUrl> will be emitted for '{}' — {} and Chocolatey CLI \
             does not support the NuGet <license> element (CHCU0002), so the package ships \
             without license metadata ('{}'). Set `publish.chocolatey.license_url` to a real \
             license URL to include one.",
            crate_name, reason, meta.license,
        ));
    }
    Ok(meta)
}

/// Derives the GitHub `…/blob/<ref>/LICENSE` URL for the release, pinning the
/// blob at the release tag (`template_vars["Tag"]`) so the URL is stable for
/// the published version. Falls back to the `HEAD` ref when no tag is in
/// scope (e.g. a snapshot render before the tag is stamped). The repo's
/// canonical license file is conventionally `LICENSE`; real exemplars point at
/// the repo root LICENSE blob.
fn derive_license_blob_url(ctx: &Context, repo_url: &str) -> String {
    let git_ref = ctx
        .template_vars()
        .get("Tag")
        .filter(|t| !t.is_empty())
        .cloned()
        .unwrap_or_else(|| "HEAD".to_string());
    format!("{repo_url}/blob/{git_ref}/LICENSE")
}

/// Filters the crate's artifacts down to Windows targets (matching by
/// triple substring or path fallback), applies the `ids:` allow-list and
/// the `amd64_variant` microarchitecture selector, and partitions the
/// survivors into the first `386` and first `amd64` artifact. Artifacts
/// for other architectures (arm64, etc.) are logged and dropped because
/// Chocolatey's install script only dispatches on bitness.
pub(super) fn select_windows_artifacts<'a>(
    ctx: &'a Context,
    choco_cfg: &anodizer_core::config::ChocolateyConfig,
    crate_name: &str,
    log: &StageLogger,
) -> (
    Option<&'a anodizer_core::artifact::Artifact>,
    Option<&'a anodizer_core::artifact::Artifact>,
) {
    // Find both 32-bit and 64-bit Windows artifacts.
    // Apply IDs + amd64_variant filter.
    let ids_filter = choco_cfg.ids.as_deref();
    let amd64_variant = choco_cfg.amd64_variant.map_or("v1", |v| v.as_str());
    let artifact_kind = util::resolve_artifact_kind(choco_cfg.use_artifact.as_deref());
    let all_artifacts = ctx.artifacts.by_kind_and_crate(artifact_kind, crate_name);

    // The format the rendered install script is built for, implied by `use:`.
    // `msi` and `nsis` both resolve to `ArtifactKind::Installer`, so a crate
    // that builds BOTH an MSI and an NSIS exe for the same arch carries two
    // indistinguishable-by-kind artifacts. Without this discriminator the
    // first-Installer-wins partition below could route the NSIS exe into a
    // slot the install script then wraps with `-FileType 'msi'` (or vice
    // versa) — an installer run with the wrong silent switches, a broken
    // install, and a moderation rejection. `archive` has no format gate.
    let required_format = match choco_cfg.use_artifact.as_deref() {
        Some("msi") => Some("msi"),
        Some("nsis") => Some("nsis"),
        _ => None,
    };

    let mut win_artifacts: Vec<_> = all_artifacts
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
        // Filter by amd64_variant microarchitecture variant.
        .filter(|a| {
            let target = a.target.as_deref().unwrap_or("");
            let (_, arch) = anodizer_core::target::map_target(target);
            if arch == "amd64" {
                return a
                    .metadata
                    .get("amd64_variant")
                    .is_none_or(|v| v == amd64_variant);
            }
            true
        })
        // Filter by installer format implied by `use:`. An artifact whose
        // `format` is present and does NOT match is excluded; one missing the
        // key is kept (tolerant — an older build stage may not stamp it). The
        // sort below then prefers a present-and-matching artifact over a
        // format-less one for the same arch.
        .filter(|a| match (required_format, a.metadata.get("format")) {
            (Some(want), Some(have)) => have == want,
            _ => true,
        })
        .collect();

    // When a format is required, order present-and-matching artifacts ahead of
    // format-less ones so the first-wins partition below prefers a definite
    // match. Stable so same-format artifacts keep their discovery order.
    if let Some(want) = required_format {
        win_artifacts.sort_by_key(|a| match a.metadata.get("format") {
            Some(have) if have == want => 0u8,
            _ => 1u8,
        });
    }

    // Chocolatey only ships amd64 + 386 install scripts; arm64 (and any
    // other architecture) MUST be filtered out before the per-architecture
    // dispatcher runs. Otherwise the `is_32bit` boolean below routes
    // a non-amd64/non-386 binary into the 64-bit slot, producing an
    // install script that downloads an arm64 archive on x64 systems
    // (broken install — what trips moderator rejection).
    //
    // Classify by the canonical arch token (`amd64` / `386`) from
    // `map_target`, not by string-substring on the triple, so future
    // triple variations can't slip through.
    let mut artifact_32 = None;
    let mut artifact_64 = None;
    for a in win_artifacts {
        let target = a.target.as_deref().unwrap_or("");
        let (_, raw_arch) = anodizer_core::target::map_target(target);
        match raw_arch.as_str() {
            "386" => {
                if artifact_32.is_none() {
                    artifact_32 = Some(a);
                }
            }
            "amd64" => {
                if artifact_64.is_none() {
                    artifact_64 = Some(a);
                }
            }
            other => {
                // arm64 / any other architecture: skip with a log line so
                // the operator sees why their arm64 build wasn't packaged
                // (rather than silently failing on the consumer's machine).
                log.status(&format!(
                    "skipped chocolatey artifact '{}' for '{}' — arch '{}' is not \
                     supported by chocolatey (only amd64/386)",
                    a.name(),
                    crate_name,
                    other
                ));
            }
        }
    }
    (artifact_32, artifact_64)
}

/// Combines the selected 32/64-bit artifacts into the `InstallMode`
/// shape consumed by the install-script renderer. Errors when neither
/// architecture has a usable Windows artifact (no install script could
/// then be constructed). The inner `resolve` closure honors the optional
/// `url_template` and enforces that every artifact carries a non-empty
/// sha256 (an empty `$checksum` ships a broken install script that
/// Chocolatey moderators reject).
pub(super) fn build_install_mode(
    ctx: &Context,
    choco_cfg: &anodizer_core::config::ChocolateyConfig,
    pkg_name: &str,
    version: &str,
    artifact_32: Option<&anodizer_core::artifact::Artifact>,
    artifact_64: Option<&anodizer_core::artifact::Artifact>,
    crate_name: &str,
) -> Result<InstallMode> {
    let url_template = choco_cfg.url_template.as_deref();
    let resolve = |a: &anodizer_core::artifact::Artifact| -> Result<(String, String)> {
        let target = a.target.as_deref().unwrap_or("");
        let (_, raw_arch) = anodizer_core::target::map_target(target);
        let resolved_url = if let Some(tmpl) = url_template {
            util::render_url_template_with_ctx(ctx, tmpl, pkg_name, version, &raw_arch, "windows")
        } else {
            a.metadata
                .get("url")
                .cloned()
                .unwrap_or_else(|| a.path.to_string_lossy().into_owned())
        };
        let sha256 = a
            .metadata
            .get("sha256")
            .cloned()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "chocolatey: artifact '{}' for crate '{}' is missing required \
                     sha256 metadata. The generated chocolateyinstall.ps1 would \
                     embed an empty `$checksum`, which Chocolatey moderators \
                     reject (the install script can't verify the download). \
                     This indicates the artifacts.json catalog dropped the \
                     entry's sha256 before the publish stage. Re-run with \
                     `task release` from a clean dist/ and verify \
                     dist/artifacts.json carries metadata.sha256 for every \
                     Windows artifact.",
                    a.name(),
                    crate_name,
                )
            })?;
        Ok((resolved_url, sha256))
    };
    match (artifact_32, artifact_64) {
        (Some(a32), Some(a64)) => {
            let (url32, hash32) = resolve(a32)?;
            let (url64, hash64) = resolve(a64)?;
            Ok(InstallMode::Dual {
                url32,
                hash32,
                url64,
                hash64,
            })
        }
        (Some(a32), None) => {
            let (url, hash) = resolve(a32)?;
            Ok(InstallMode::Single {
                url,
                hash,
                is_32bit: true,
            })
        }
        (None, Some(a64)) => {
            let (url, hash) = resolve(a64)?;
            Ok(InstallMode::Single {
                url,
                hash,
                is_32bit: false,
            })
        }
        (None, None) => {
            // No Windows artifact = no install script that can possibly
            // verify or download the binary. Pushing a nupkg with an empty
            // checksum and a fabricated GitHub URL is what trips moderator
            // rejection (broken install script), so this case fails loudly.
            anyhow::bail!(
                "chocolatey: no windows artifact found for '{}'. Chocolatey \
                 requires a Windows archive (or msi/nsis when configured via \
                 `use:`) to construct a working install script. Either build \
                 a Windows target for this crate or remove the chocolatey \
                 publisher config.",
                crate_name
            );
        }
    }
}

/// Template-renders the optional `<title>`, `<copyright>`, `<summary>`,
/// and `<releaseNotes>` fields. `summary` falls back to project
/// `metadata.description` and `release_notes` to
/// `metadata.full_description` (typically a README) so the gallery
/// always sees populated tags.
pub(super) fn render_text_fields(
    ctx: &Context,
    choco_cfg: &anodizer_core::config::ChocolateyConfig,
    crate_name: &str,
    log: &StageLogger,
) -> Result<ChocoTextFields> {
    let is_strict = ctx.render_is_strict();
    let title = choco_cfg
        .title
        .as_deref()
        .map(|t| util::render_or_warn(ctx, log, "chocolatey.title", t))
        .transpose()?;

    // Template-render extra: `Changelog` is a tera variable. When the changelog
    // stage has not populated `ReleaseNotes` (e.g. first release with no prior
    // tag), an empty string is the correct default — Tera renders `{{ Changelog }}`
    // as `` and the user's template stays valid.
    let release_notes_var = ctx
        .template_vars()
        .get("ReleaseNotes")
        .cloned()
        .unwrap_or_default();
    let render = |field: &str, s: Option<&str>| -> Result<Option<String>> {
        s.map(|v| {
            let mut vars = ctx.template_vars().clone();
            vars.set("Changelog", &release_notes_var);
            util::render_or_warn_with_vars(&vars, log, field, v, is_strict)
        })
        .transpose()
    };
    let copyright = render("chocolatey.copyright", choco_cfg.copyright.as_deref())?;
    // Summary falls back to project-level
    // `metadata.description` (the 1-line summary), same source the
    // `description` field already falls back to. The Chocolatey gallery
    // requires `<summary>`; without this fallback an unset `summary:` in
    // the choco block emitted an empty tag, which gallery moderators
    // flag as incomplete metadata.
    let summary = match render("chocolatey.summary", choco_cfg.summary.as_deref())? {
        Some(s) => Some(s),
        None => match ctx.config.meta_description_for(crate_name) {
            Some(s) => Some(util::render_or_warn(ctx, log, "chocolatey.summary", s)?),
            None => None,
        },
    };
    // release_notes falls back to the resolved
    // `metadata.full_description` (the long-form body, typically
    // README.md via `from_file:`). Without this fallback an unset
    // `release_notes:` in the choco block left the nuspec
    // `<releaseNotes>` empty even when the project carried a
    // README. `render_template` walks the structured `Metadata` map
    // populated at context bootstrap.
    let release_notes = match render(
        "chocolatey.release_notes",
        choco_cfg.release_notes.as_deref(),
    )? {
        Some(s) => Some(s),
        None => {
            // The full-description fallback is the tool's own fixed template
            // (`{{ Metadata.FullDescription }}`), not user config, so a render
            // failure here is genuinely internal — keep its `.ok()` swallow.
            ctx.render_template("{{ Metadata.FullDescription }}")
                .ok()
                .filter(|s| !s.is_empty())
        }
    };
    // <id>, <packageSourceUrl>, <docsUrl>, <owners> are user-supplied config
    // string fields; render them so a value like `…/blob/{{ .Tag }}/…` resolves
    // instead of shipping the literal delimiters into the nuspec.
    let name = choco_cfg
        .name
        .as_deref()
        .map(|n| util::render_or_warn(ctx, log, "chocolatey.name", n))
        .transpose()?;
    let package_source_url = choco_cfg
        .package_source_url
        .as_deref()
        .map(|u| util::render_or_warn(ctx, log, "chocolatey.package_source_url", u))
        .transpose()?;
    let docs_url = choco_cfg
        .docs_url
        .as_deref()
        .map(|u| util::render_or_warn(ctx, log, "chocolatey.docs_url", u))
        .transpose()?;
    let owners = choco_cfg
        .owners
        .as_deref()
        .map(|o| util::render_or_warn(ctx, log, "chocolatey.owners", o))
        .transpose()?;
    Ok(ChocoTextFields {
        title,
        copyright,
        summary,
        release_notes,
        name,
        package_source_url,
        docs_url,
        owners,
    })
}

/// Renders the `.nuspec` XML body from the resolved metadata + text fields.
pub(super) fn build_nuspec(
    choco_cfg: &anodizer_core::config::ChocolateyConfig,
    crate_name: &str,
    version: &str,
    metadata: &ChocoMetadata,
    text: &ChocoTextFields,
) -> Result<String> {
    generate_nuspec(&NuspecParams {
        name: text.name.as_deref().unwrap_or(crate_name),
        version,
        description: &metadata.description,
        license_url: metadata.license_url.as_deref(),
        authors: &metadata.authors,
        project_url: &metadata.project_url,
        icon_url: &metadata.icon_url,
        tags: &metadata.tags,
        package_source_url: text.package_source_url.as_deref(),
        owners: text.owners.as_deref(),
        title: text.title.as_deref(),
        copyright: text.copyright.as_deref(),
        require_license_acceptance: choco_cfg.require_license_acceptance.unwrap_or(false),
        project_source_url: metadata.project_source_url.as_deref(),
        docs_url: text.docs_url.as_deref(),
        bug_tracker_url: metadata.bug_tracker_url.as_deref(),
        summary: text.summary.as_deref(),
        release_notes: text.release_notes.as_deref(),
        dependencies: choco_cfg.dependencies.as_deref().unwrap_or(&[]),
    })
}

/// Renders `chocolateyinstall.ps1` from the resolved `InstallMode`, routing the
/// emitted cmdlet by the artifact's installer type (`file_type`): a zip is
/// unpacked via `Install-ChocolateyZipPackage`, an msi/nsis-exe is run silently
/// via `Install-ChocolateyPackage`.
pub(super) fn build_install_script(
    pkg_name: &str,
    install_mode: &InstallMode,
    file_type: FileType,
) -> Result<String> {
    match install_mode {
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
            file_type,
        }),
        InstallMode::Single {
            url,
            hash,
            is_32bit,
        } => generate_install_script(pkg_name, url, hash, *is_32bit, file_type),
    }
}

/// Writes the nuspec and install script to a fresh tempdir, then packs
/// them (natively, no `choco` CLI dependency) into a `.nupkg` OPC/ZIP.
/// The returned `StagedPackage` owns the tempdir so its lifetime extends
/// to the push call site.
pub(super) fn stage_package(
    pkg_name: &str,
    version: &str,
    nuspec: &str,
    install_script: &str,
    log: &StageLogger,
) -> Result<StagedPackage> {
    let tmp_dir = tempfile::tempdir().context("chocolatey: create temp dir")?;
    let pkg_dir: &Path = tmp_dir.path();
    let nuspec_path = pkg_dir.join(format!("{}.nuspec", pkg_name));
    std::fs::write(&nuspec_path, nuspec)
        .with_context(|| format!("chocolatey: write nuspec {}", nuspec_path.display()))?;

    let tools_dir = pkg_dir.join("tools");
    std::fs::create_dir_all(&tools_dir).context("chocolatey: create tools dir")?;

    let install_path = tools_dir.join("chocolateyinstall.ps1");
    std::fs::write(&install_path, install_script).with_context(|| {
        format!(
            "chocolatey: write install script {}",
            install_path.display()
        )
    })?;

    log.status(&format!(
        "wrote Chocolatey nuspec {}",
        nuspec_path.display()
    ));
    log.status(&format!(
        "wrote Chocolatey install script {}",
        install_path.display()
    ));

    // Create .nupkg natively (OPC/ZIP format) — no `choco` CLI dependency.
    // A nupkg is a ZIP containing the nuspec, tools/, and OPC metadata files.
    let nupkg_path = pkg_dir.join(format!("{}.{}.nupkg", pkg_name, version));
    create_nupkg(pkg_name, &nuspec_path, &tools_dir, &nupkg_path)?;
    log.status(&format!("created nupkg {}", nupkg_path.display()));

    Ok(StagedPackage {
        _tmp_dir: tmp_dir,
        nupkg_path,
    })
}

/// Resolves the NuGet API key from `choco.api_key` (template-rendered)
/// with `CHOCOLATEY_API_KEY` env-var fallback. An empty result signals
/// "skip push" at the call site.
pub(super) fn resolve_api_key(
    ctx: &Context,
    choco_cfg: &anodizer_core::config::ChocolateyConfig,
    log: &StageLogger,
) -> Result<String> {
    // Template-render APIKey.
    // Empty default is checked by the caller — the empty branch logs a
    // warn and returns Ok(false) (skip push) rather than letting an empty
    // key reach the NuGet push API where it would surface as opaque 403.
    let rendered = choco_cfg
        .api_key
        .as_deref()
        .map(|k| util::render_or_warn(ctx, log, "chocolatey.api_key", k))
        .transpose()?;
    Ok(rendered
        .or_else(|| ctx.env_var("CHOCOLATEY_API_KEY"))
        .unwrap_or_default())
}

/// Inspects the feed for the current `pkg_name@version` and decides
/// whether to short-circuit (already published hash-match, pending
/// moderation without `republish_in_moderation`), bail (rejected,
/// hash drift), or fall through to push.
///
/// `Ok(None)` means "proceed to push". `Ok(Some(b))` means "return `b`
/// immediately from `publish_to_chocolatey`".
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_feed_state(
    ctx: &mut Context,
    choco_cfg: &anodizer_core::config::ChocolateyConfig,
    source: &str,
    pkg_name: &str,
    version: &str,
    nupkg_path: &Path,
    policy: &anodizer_core::retry::RetryPolicy,
    log: &StageLogger,
) -> Result<Option<bool>> {
    match package_feed_hash(source, pkg_name, version, policy, log) {
        FeedHashResult::Present {
            hash,
            algorithm,
            status,
            is_approved,
            published,
        } => {
            // A version in the community moderation queue may or may not
            // accept a re-push depending on the operator's intent.
            //
            // Discriminator: `<d:PackageStatus>` (with `<d:IsApproved>` as
            // fallback). The OData feed does NOT emit `<d:Listed>`, so any
            // state machine keyed on it is dead code. The classifier is
            // shared with the preflight checker so both call sites agree on
            // what "in moderation" means.
            let (reason, in_moderation) =
                crate::chocolatey::package::classify_moderation(status.as_deref(), is_approved);
            if in_moderation {
                let status_label = status.as_deref().unwrap_or("Unknown");
                let published_label = published.as_deref().unwrap_or("");
                if status_label.eq_ignore_ascii_case("Rejected") {
                    anyhow::bail!(
                        "chocolatey: '{}-{}' was REJECTED by the community moderators \
                         (PackageStatus=Rejected, Published={}). Address the rejection \
                         reason on the gallery and bump the version before re-pushing.",
                        pkg_name,
                        version,
                        published_label
                    );
                }
                // `republish_in_moderation: true` opts into replacing the
                // queued nupkg. The Chocolatey API accepts re-pushes of
                // in-moderation versions; the new nupkg displaces the old one.
                let do_republish = match choco_cfg.republish_in_moderation.as_ref() {
                    Some(v) => v
                        .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                        .context("chocolatey: render republish_in_moderation condition")?,
                    None => false,
                };
                if do_republish {
                    log.status(&format!(
                        "chocolatey package '{}-{}' {} (PackageStatus={}, Published={}); \
                         republish_in_moderation=true — replacing in-moderation copy.",
                        pkg_name, version, reason, status_label, published_label
                    ));
                    // Push the new nupkg to displace the queued one. Skip the
                    // hash-equality check below: an in-moderation re-cut
                    // legitimately changes the nupkg (a fail-forward adds a
                    // commit, shifting the changelog embedded in <releaseNotes>),
                    // and bailing on that diff is exactly what
                    // republish_in_moderation opts out of. A still-Submitted
                    // version accepts a re-push; if it has since been Approved
                    // (truly immutable) the push path surfaces the 403 as a
                    // real error.
                    return Ok(None);
                } else {
                    log.warn(&format!(
                        "chocolatey package '{}-{}' {} (PackageStatus={}, Published={}); \
                         skipping push — set republish_in_moderation: true to replace \
                         the in-moderation copy. The gallery will not list the package \
                         until it transitions to Approved.",
                        pkg_name, version, reason, status_label, published_label
                    ));
                    // Tell dispatch this run is "pending moderation", not
                    // a clean success. Without this the summary table
                    // reports `succeeded` and the operator never sees
                    // that the push was actually skipped.
                    ctx.record_publisher_outcome(
                        anodizer_core::PublisherOutcome::PendingModeration,
                    );
                    return Ok(Some(false));
                }
            }
            let local = compute_nupkg_hash(nupkg_path, &algorithm)?;
            if local == hash {
                log.status(&format!(
                    "skipped chocolatey '{}-{}' — already published (hash match)",
                    pkg_name, version
                ));
                return Ok(Some(false));
            }
            anyhow::bail!(
                "chocolatey: '{}-{}' is already on the feed but the local nupkg \
                 differs (feed {}={}, local {}={}). Chocolatey package versions \
                 are immutable once submitted — bump the version before re-releasing.",
                pkg_name,
                version,
                algorithm,
                hash,
                algorithm,
                local
            );
        }
        FeedHashResult::PresentNoHash => {
            // Feed reports the version exists but didn't expose a parseable
            // hash. Conservative path: don't silently skip without
            // verification (silent skip on diverged bytes previously shipped
            // an install script pointing at a stale sha). Log the situation
            // and let the push attempt proceed; Chocolatey returns 403 with
            // a recognizable message if the version is truly immutable, and
            // that surfaces as a real error.
            log.warn(&format!(
                "chocolatey package '{}-{}' exists on feed but hash was unavailable; \
                 attempting push so any conflict surfaces as a real error",
                pkg_name, version
            ));
        }
        FeedHashResult::Absent => {
            // Not on feed — push normally.
        }
    }
    Ok(None)
}
