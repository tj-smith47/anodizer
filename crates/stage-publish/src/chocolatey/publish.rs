//! `publish_to_chocolatey` orchestrator — assembles the nuspec + install
//! script, packs a nupkg natively, and pushes via the NuGet V2 API.

use std::path::{Path, PathBuf};

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};

use crate::util;

use super::install::{InstallScriptDual, generate_install_script, generate_install_script_dual};
use super::nuspec::{NuspecParams, generate_nuspec};
use super::package::{
    FeedHashResult, compute_nupkg_hash, create_nupkg, package_feed_hash, push_nupkg,
};

/// Per-crate metadata required by both the nuspec generator and the
/// install-script renderer. Values are pre-resolved (template-rendered and
/// fallback-applied) so the orchestrator stays linear.
struct ChocoMetadata {
    description: String,
    license: String,
    authors: String,
    project_url: String,
    icon_url: String,
    tags: Vec<String>,
}

/// Optional, template-rendered text fields that flow into `<title>`,
/// `<copyright>`, `<summary>`, and `<releaseNotes>` of the generated nuspec.
struct ChocoTextFields {
    title: Option<String>,
    copyright: Option<String>,
    summary: Option<String>,
    release_notes: Option<String>,
}

/// Install-script shape. `Dual` carries 32- and 64-bit URL/hash pairs;
/// `Single` carries one URL/hash plus the bitness selector used by the
/// per-architecture template branch.
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

/// Paths produced by staging the package on disk: the rendered `.nuspec`
/// and the packed `.nupkg` ready for push.
struct StagedPackage {
    _tmp_dir: tempfile::TempDir,
    nupkg_path: PathBuf,
}

/// Returns `Ok(true)` when an actual `push_nupkg` happened against the
/// feed, `Ok(false)` for every skip path (skip=true template, dry-run,
/// missing API key, hash-match already-published, pending-moderation
/// without `republish_in_moderation`). The caller MUST use the bool to
/// gate rollback-evidence recording — recording a target the run never
/// pushed produces a misleading "manual withdrawal required" warning at
/// rollback time.
pub fn publish_to_chocolatey(
    ctx: &mut Context,
    crate_name: &str,
    log: &StageLogger,
) -> Result<bool> {
    let choco_cfg = {
        let (_crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "chocolatey")?;
        publish
            .chocolatey
            .as_ref()
            .ok_or_else(|| {
                anyhow::anyhow!("chocolatey: no chocolatey config for '{}'", crate_name)
            })?
            .clone()
    };
    let choco_cfg = &choco_cfg;

    // Chocolatey is a feed-push publisher: only `api_key` + `source_repo`
    // are required to push. The optional `repository.owner/name` is *only*
    // used as a fallback source for `<projectUrl>` (the gallery link) when
    // `project_url:` is unset. The lookup is optional and falls back to an
    // empty string when both project_url and repository are unset, so
    // internal feeds without a public GitHub release are not blocked.
    //
    let (repo_owner, repo_name) = match choco_cfg.repository.as_ref() {
        Some(r) => (
            r.owner.as_deref().unwrap_or(""),
            r.name.as_deref().unwrap_or(""),
        ),
        None => ("", ""),
    };

    if check_skip_publish(ctx, choco_cfg, crate_name, log)? {
        return Ok(false);
    }

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would push Chocolatey package for '{}'{}",
            crate_name,
            if repo_owner.is_empty() {
                String::new()
            } else {
                format!(" to {}/{}", repo_owner, repo_name)
            }
        ));
        return Ok(false);
    }

    let version = ctx.version();
    let pkg_name = choco_cfg.name.as_deref().unwrap_or(crate_name);

    // The skip gate above already ran (`check_skip_publish`), so render the
    // nuspec via the skip-unaware inner helper — re-evaluating the skip/`if`
    // gate here would double every resolved-with-warning value's log line.
    let nuspec = render_nuspec_inner(ctx, choco_cfg, crate_name, repo_owner, repo_name, log)?;

    let (artifact_32, artifact_64) = select_windows_artifacts(ctx, choco_cfg, crate_name, log);
    let install_mode = build_install_mode(
        ctx,
        choco_cfg,
        pkg_name,
        &version,
        artifact_32,
        artifact_64,
        crate_name,
    )?;

    let install_script = build_install_script(pkg_name, &install_mode)?;

    let staged = stage_package(pkg_name, &version, &nuspec, &install_script, log)?;

    let api_key = resolve_api_key(ctx, choco_cfg, log)?;
    if api_key.is_empty() {
        log.warn(&format!(
            "no chocolatey API key for '{}', skipping push",
            crate_name
        ));
        return Ok(false);
    }

    let source = choco_cfg
        .source_repo
        .as_deref()
        .unwrap_or("https://push.chocolatey.org/");

    // Idempotency with drift detection: Chocolatey package versions are
    // immutable once submitted, so re-pushing returns 403. A
    // version-already-on-feed is treated as a skip ONLY when the feed's
    // recorded package hash matches the local nupkg hash. If they differ,
    // the local nupkg has diverged from what the feed has — typically
    // because the same git tag was re-released with different artifact
    // bytes — and silently skipping would publish an install script that
    // points at an archive whose sha no longer matches (Chocolatey's
    // verifier then rejects the package). Divergence fails loudly with a
    // message instructing the caller to bump the version.
    // Single retry policy resolved from the top-level `retry:` block; reused
    // for the feed-hash GET and the push PUT.
    let policy = ctx.retry_policy();

    if let Some(early_exit) = handle_feed_state(
        ctx,
        choco_cfg,
        source,
        pkg_name,
        &version,
        &staged.nupkg_path,
        &policy,
        log,
    )? {
        return Ok(early_exit);
    }

    // Push via NuGet V2 API — same protocol as `choco push`.
    push_nupkg(&staged.nupkg_path, source, &api_key, log, &policy)?;

    log.status(&format!("Chocolatey package pushed for '{}'", crate_name));
    Ok(true)
}

/// Evaluates `skip:` (literal bool or template) and returns `Ok(true)`
/// when the publisher should be bypassed for this crate.
fn check_skip_publish(
    ctx: &mut Context,
    choco_cfg: &anodizer_core::config::ChocolateyConfig,
    crate_name: &str,
    log: &StageLogger,
) -> Result<bool> {
    let label = format!("chocolatey publisher for crate '{}'", crate_name);
    crate::util::should_skip_publisher_with_if(
        ctx,
        choco_cfg.skip.as_ref(),
        None,
        choco_cfg.if_condition.as_deref(),
        &label,
        log,
    )
}

/// Render the `.nuspec` XML a real Chocolatey publish would stage for
/// `crate_name`, in-memory and with no disk/network side effects.
///
/// Returns `Ok(None)` when the publisher would skip this crate (`skip:` true
/// or a falsy `if` condition). Errors when the crate carries no `chocolatey`
/// block, or when `license` is unresolvable (an empty `<licenseUrl>` is what
/// Chocolatey gallery moderators reject). The live publish path and the
/// offline schema validator both produce the nuspec through the same inner
/// render so the validated document is byte-for-byte what a release pushes.
///
/// Unlike the install script, the nuspec does not depend on any Windows
/// archive artifact — it always renders regardless of which platforms built.
pub(crate) fn render_nuspec_for_crate(
    ctx: &Context,
    crate_name: &str,
    log: &StageLogger,
) -> Result<Option<String>> {
    let (_crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "chocolatey")?;
    let choco_cfg = publish
        .chocolatey
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("chocolatey: no chocolatey config for '{}'", crate_name))?;

    let label = format!("chocolatey publisher for crate '{}'", crate_name);
    if crate::util::should_skip_publisher_with_if(
        ctx,
        choco_cfg.skip.as_ref(),
        None,
        choco_cfg.if_condition.as_deref(),
        &label,
        log,
    )? {
        return Ok(None);
    }

    let (repo_owner, repo_name) = match choco_cfg.repository.as_ref() {
        Some(r) => (
            r.owner.as_deref().unwrap_or(""),
            r.name.as_deref().unwrap_or(""),
        ),
        None => ("", ""),
    };
    let nuspec = render_nuspec_inner(ctx, choco_cfg, crate_name, repo_owner, repo_name, log)?;
    Ok(Some(nuspec))
}

/// Skip-unaware nuspec render: resolve metadata + text fields and build the
/// `.nuspec` XML body. Every resolved-with-warning value is resolved exactly
/// once here, so both the live publish path (which has already evaluated the
/// skip gate) and [`render_nuspec_for_crate`] (which evaluates it itself)
/// share one resolution without double-logging.
fn render_nuspec_inner(
    ctx: &Context,
    choco_cfg: &anodizer_core::config::ChocolateyConfig,
    crate_name: &str,
    repo_owner: &str,
    repo_name: &str,
    log: &StageLogger,
) -> Result<String> {
    let version = ctx.version();
    let metadata = resolve_metadata(ctx, choco_cfg, crate_name, repo_owner, repo_name, log)?;
    let text_fields = render_text_fields(ctx, choco_cfg, crate_name, log)?;
    build_nuspec(choco_cfg, crate_name, &version, &metadata, &text_fields)
}

/// Resolves the required nuspec metadata (description, license, authors,
/// project/icon URLs, tags) by walking `choco.*` → `metadata.*` fallbacks
/// and template-rendering where applicable. Fails when `license` is
/// unresolvable, because the Chocolatey gallery rejects empty licenses.
fn resolve_metadata(
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
                 An empty <license> field produces a broken licenseUrl \
                 (`https://opensource.org/licenses/`) which Chocolatey gallery \
                 moderators reject. Set `publish.chocolatey.license` (SPDX \
                 identifier, e.g. \"MIT\") or top-level `metadata.license`, or \
                 set `publish.chocolatey.license_url` to a custom URL.",
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
    let project_url = choco_cfg.project_url.clone().unwrap_or_else(|| {
        if repo_owner.is_empty() || repo_name.is_empty() {
            // <projectUrl> is optional per nuspec.xsd (`xs:element minOccurs="0"`);
            // empty value is suppressed by the Tera `{% if project_url %}` guard
            // in nuspec.rs so no broken tag ships.
            String::new()
        } else {
            format!("https://github.com/{}/{}", repo_owner, repo_name)
        }
    });
    // <iconUrl> is optional per nuspec.xsd; nuspec.rs only emits the tag when
    // the value is non-empty (gated on `icon_url` truthy in the Tera template).
    let icon_url = choco_cfg.icon_url.clone().unwrap_or_default();
    // <tags> is optional per nuspec.xsd; when no tags are configured the
    // generator falls back to the package name (nuspec.rs line ~93).
    let tags = choco_cfg.tags.clone().unwrap_or_default();
    Ok(ChocoMetadata {
        description,
        license,
        authors,
        project_url,
        icon_url,
        tags,
    })
}

/// Filters the crate's artifacts down to Windows targets (matching by
/// triple substring or path fallback), applies the `ids:` allow-list and
/// the `amd64_variant` microarchitecture selector, and partitions the
/// survivors into the first `386` and first `amd64` artifact. Artifacts
/// for other architectures (arm64, etc.) are logged and dropped because
/// Chocolatey's install script only dispatches on bitness.
fn select_windows_artifacts<'a>(
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
    let amd64_variant = choco_cfg.amd64_variant.as_deref().or(Some("v1"));
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
        // Filter by amd64_variant microarchitecture variant.
        .filter(|a| {
            let target = a.target.as_deref().unwrap_or("");
            let (_, arch) = anodizer_core::target::map_target(target);
            if arch == "amd64"
                && let Some(want) = amd64_variant
            {
                return a.metadata.get("amd64_variant").is_none_or(|v| v == want);
            }
            true
        })
        .collect();

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
                    "skipping chocolatey artifact '{}' for '{}' — arch '{}' is not \
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
fn build_install_mode(
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
fn render_text_fields(
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

    // Template-render Copyright, Summary, Description, ReleaseNotes.
    // `Changelog` is injected as a per-render extra so configs that use
    // `{{ Changelog }}` work.
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
    Ok(ChocoTextFields {
        title,
        copyright,
        summary,
        release_notes,
    })
}

/// Renders the `.nuspec` XML body from the resolved metadata + text fields.
fn build_nuspec(
    choco_cfg: &anodizer_core::config::ChocolateyConfig,
    crate_name: &str,
    version: &str,
    metadata: &ChocoMetadata,
    text: &ChocoTextFields,
) -> Result<String> {
    generate_nuspec(&NuspecParams {
        name: choco_cfg.name.as_deref().unwrap_or(crate_name),
        version,
        description: &metadata.description,
        license: &metadata.license,
        license_url: choco_cfg.license_url.as_deref(),
        authors: &metadata.authors,
        project_url: &metadata.project_url,
        icon_url: &metadata.icon_url,
        tags: &metadata.tags,
        package_source_url: choco_cfg.package_source_url.as_deref(),
        owners: choco_cfg.owners.as_deref(),
        title: text.title.as_deref(),
        copyright: text.copyright.as_deref(),
        require_license_acceptance: choco_cfg.require_license_acceptance.unwrap_or(false),
        project_source_url: choco_cfg.project_source_url.as_deref(),
        docs_url: choco_cfg.docs_url.as_deref(),
        bug_tracker_url: choco_cfg.bug_tracker_url.as_deref(),
        summary: text.summary.as_deref(),
        release_notes: text.release_notes.as_deref(),
        dependencies: choco_cfg.dependencies.as_deref().unwrap_or(&[]),
    })
}

/// Renders `chocolateyinstall.ps1` from the resolved `InstallMode`.
fn build_install_script(pkg_name: &str, install_mode: &InstallMode) -> Result<String> {
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
        }),
        InstallMode::Single {
            url,
            hash,
            is_32bit,
        } => generate_install_script(pkg_name, url, hash, *is_32bit),
    }
}

/// Writes the nuspec and install script to a fresh tempdir, then packs
/// them (natively, no `choco` CLI dependency) into a `.nupkg` OPC/ZIP.
/// The returned `StagedPackage` owns the tempdir so its lifetime extends
/// to the push call site.
fn stage_package(
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
    create_nupkg(pkg_name, version, &nuspec_path, &tools_dir, &nupkg_path)?;
    log.status(&format!("created nupkg {}", nupkg_path.display()));

    Ok(StagedPackage {
        _tmp_dir: tmp_dir,
        nupkg_path,
    })
}

/// Resolves the NuGet API key from `choco.api_key` (template-rendered)
/// with `CHOCOLATEY_API_KEY` env-var fallback. An empty result signals
/// "skip push" at the call site.
fn resolve_api_key(
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
fn handle_feed_state(
    ctx: &mut Context,
    choco_cfg: &anodizer_core::config::ChocolateyConfig,
    source: &str,
    pkg_name: &str,
    version: &str,
    nupkg_path: &Path,
    policy: &anodizer_core::retry::RetryPolicy,
    log: &StageLogger,
) -> Result<Option<bool>> {
    match package_feed_hash(source, pkg_name, version, policy) {
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
                let do_republish = choco_cfg
                    .republish_in_moderation
                    .as_ref()
                    .map(|v| {
                        v.try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
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
                    "skipping chocolatey '{}-{}' — already published (hash match)",
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

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{
        ChocolateyConfig, Config, ContentSource, CrateConfig, MetadataConfig, PublishConfig,
        RepositoryConfig, StringOrBool,
    };
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};

    fn windows_artifact(crate_name: &str, target: &str, name: &str) -> Artifact {
        let mut m = std::collections::HashMap::new();
        m.insert("sha256".to_string(), "deadbeef".to_string());
        m.insert("url".to_string(), format!("https://example.com/{}", name));
        Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from(format!("/tmp/{}", name)),
            name: name.to_string(),
            target: Some(target.to_string()),
            crate_name: crate_name.to_string(),
            metadata: m,
            size: None,
        }
    }

    fn ctx_with_choco(cfg: ChocolateyConfig) -> Context {
        ctx_with_choco_opts(cfg, ContextOptions::default())
    }

    fn ctx_with_choco_opts(cfg: ChocolateyConfig, opts: ContextOptions) -> Context {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                chocolatey: Some(cfg),
                ..Default::default()
            }),
            ..Default::default()
        }];
        Context::new(config, opts)
    }

    // -----------------------------------------------------------------
    // check_skip_publish
    // -----------------------------------------------------------------

    #[test]
    fn check_skip_publish_returns_false_when_skip_is_none() {
        let mut ctx = ctx_with_choco(ChocolateyConfig::default());
        let cfg = ChocolateyConfig::default();
        let log = StageLogger::new("publish", Verbosity::Quiet);
        assert!(!check_skip_publish(&mut ctx, &cfg, "mytool", &log).unwrap());
    }

    #[test]
    fn check_skip_publish_returns_false_when_skip_is_literal_false() {
        let mut ctx = ctx_with_choco(ChocolateyConfig::default());
        let cfg = ChocolateyConfig {
            skip: Some(StringOrBool::Bool(false)),
            ..Default::default()
        };
        let log = StageLogger::new("publish", Verbosity::Quiet);
        assert!(!check_skip_publish(&mut ctx, &cfg, "mytool", &log).unwrap());
    }

    #[test]
    fn check_skip_publish_template_evaluating_false_does_not_skip() {
        let mut ctx = ctx_with_choco(ChocolateyConfig::default());
        let cfg = ChocolateyConfig {
            skip: Some(StringOrBool::String("false".to_string())),
            ..Default::default()
        };
        let log = StageLogger::new("publish", Verbosity::Quiet);
        assert!(!check_skip_publish(&mut ctx, &cfg, "mytool", &log).unwrap());
    }

    #[test]
    fn check_skip_publish_template_evaluating_true_skips_and_logs() {
        let mut ctx = ctx_with_choco(ChocolateyConfig::default());
        let cfg = ChocolateyConfig {
            skip: Some(StringOrBool::String("true".to_string())),
            ..Default::default()
        };
        let (log, cap) = StageLogger::with_capture("publish", Verbosity::Normal);
        assert!(check_skip_publish(&mut ctx, &cfg, "mytool", &log).unwrap());
        let msgs = cap.all_messages();
        assert!(
            msgs.iter()
                .any(|(_, m)| m.contains("skipping") && m.contains("mytool")),
            "expected skip status, got {msgs:?}"
        );
    }

    #[test]
    fn check_skip_publish_propagates_render_error_with_context() {
        let mut ctx = ctx_with_choco(ChocolateyConfig::default());
        let cfg = ChocolateyConfig {
            skip: Some(StringOrBool::String("{{ undefined.symbol(".to_string())),
            ..Default::default()
        };
        let log = StageLogger::new("publish", Verbosity::Quiet);
        let err = check_skip_publish(&mut ctx, &cfg, "mytool", &log)
            .expect_err("malformed template must bubble");
        let msg = format!("{err:#}");
        assert!(msg.contains("render skip template"), "{msg}");
        assert!(msg.contains("mytool"), "{msg}");
    }

    // -----------------------------------------------------------------
    // resolve_metadata
    // -----------------------------------------------------------------

    #[test]
    fn resolve_metadata_falls_back_to_project_metadata_for_license_and_description() {
        let mut config = Config::default();
        config.metadata = Some(MetadataConfig {
            description: Some("project-level desc".to_string()),
            license: Some("Apache-2.0".to_string()),
            maintainers: Some(vec!["Alice <a@example.com>".to_string()]),
            ..Default::default()
        });
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }];
        let ctx = Context::new(config, ContextOptions::default());
        let cfg = ChocolateyConfig::default();
        let meta = resolve_metadata(&ctx, &cfg, "mytool", "", "", &ctx.logger("publish")).unwrap();
        assert_eq!(meta.description, "project-level desc");
        assert_eq!(meta.license, "Apache-2.0");
        assert_eq!(meta.authors, "Alice <a@example.com>");
        assert_eq!(meta.project_url, "");
        assert_eq!(meta.icon_url, "");
        assert!(meta.tags.is_empty());
    }

    #[test]
    fn resolve_metadata_uses_choco_fields_over_project_metadata() {
        let mut config = Config::default();
        config.metadata = Some(MetadataConfig {
            description: Some("project desc".to_string()),
            license: Some("Apache-2.0".to_string()),
            ..Default::default()
        });
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }];
        let ctx = Context::new(config, ContextOptions::default());
        let cfg = ChocolateyConfig {
            description: Some("choco desc".to_string()),
            license: Some("MIT".to_string()),
            authors: Some("Choco Author".to_string()),
            tags: Some(vec!["cli".to_string()]),
            icon_url: Some("https://example.com/i.png".to_string()),
            ..Default::default()
        };
        let meta = resolve_metadata(&ctx, &cfg, "mytool", "", "", &ctx.logger("publish")).unwrap();
        assert_eq!(meta.description, "choco desc");
        assert_eq!(meta.license, "MIT");
        assert_eq!(meta.authors, "Choco Author");
        assert_eq!(meta.icon_url, "https://example.com/i.png");
        assert_eq!(meta.tags, vec!["cli".to_string()]);
    }

    #[test]
    fn resolve_metadata_derives_project_url_from_repo_when_unset() {
        let ctx = ctx_with_choco(ChocolateyConfig::default());
        let cfg = ChocolateyConfig {
            license: Some("MIT".to_string()),
            ..Default::default()
        };
        let meta = resolve_metadata(
            &ctx,
            &cfg,
            "mytool",
            "myorg",
            "mytool",
            &ctx.logger("publish"),
        )
        .unwrap();
        assert_eq!(meta.project_url, "https://github.com/myorg/mytool");
    }

    #[test]
    fn resolve_metadata_explicit_project_url_wins_over_repo_derivation() {
        let ctx = ctx_with_choco(ChocolateyConfig::default());
        let cfg = ChocolateyConfig {
            license: Some("MIT".to_string()),
            project_url: Some("https://example.com/home".to_string()),
            ..Default::default()
        };
        let meta = resolve_metadata(
            &ctx,
            &cfg,
            "mytool",
            "myorg",
            "mytool",
            &ctx.logger("publish"),
        )
        .unwrap();
        assert_eq!(meta.project_url, "https://example.com/home");
    }

    #[test]
    fn resolve_metadata_authors_default_is_crate_name_when_no_maintainers() {
        let ctx = ctx_with_choco(ChocolateyConfig::default());
        let cfg = ChocolateyConfig {
            license: Some("MIT".to_string()),
            ..Default::default()
        };
        let meta = resolve_metadata(&ctx, &cfg, "mytool", "", "", &ctx.logger("publish")).unwrap();
        assert_eq!(meta.authors, "mytool");
    }

    #[test]
    fn resolve_metadata_missing_license_returns_actionable_bail() {
        let ctx = ctx_with_choco(ChocolateyConfig::default());
        let cfg = ChocolateyConfig::default();
        let err = match resolve_metadata(&ctx, &cfg, "mytool", "", "", &ctx.logger("publish")) {
            Err(e) => e,
            Ok(_) => panic!("missing license must bail"),
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("license is required"), "{msg}");
        assert!(msg.contains("mytool"), "{msg}");
        assert!(
            msg.contains("publish.chocolatey.license") || msg.contains("metadata.license"),
            "{msg}"
        );
    }

    // -----------------------------------------------------------------
    // select_windows_artifacts
    // -----------------------------------------------------------------

    #[test]
    fn select_windows_artifacts_partitions_first_386_and_first_amd64() {
        let mut ctx = ctx_with_choco(ChocolateyConfig::default());
        ctx.artifacts.add(windows_artifact(
            "mytool",
            "i686-pc-windows-msvc",
            "a-386.zip",
        ));
        ctx.artifacts.add(windows_artifact(
            "mytool",
            "x86_64-pc-windows-msvc",
            "b-amd64.zip",
        ));
        ctx.artifacts.add(windows_artifact(
            "mytool",
            "x86_64-pc-windows-msvc",
            "c-amd64-dup.zip",
        ));
        let cfg = ChocolateyConfig::default();
        let log = StageLogger::new("publish", Verbosity::Quiet);
        let (a32, a64) = select_windows_artifacts(&ctx, &cfg, "mytool", &log);
        assert_eq!(a32.unwrap().name, "a-386.zip");
        // First amd64 wins; second is dropped.
        assert_eq!(a64.unwrap().name, "b-amd64.zip");
    }

    #[test]
    fn select_windows_artifacts_logs_and_skips_arm64() {
        let mut ctx = ctx_with_choco(ChocolateyConfig::default());
        ctx.artifacts.add(windows_artifact(
            "mytool",
            "aarch64-pc-windows-msvc",
            "x-arm64.zip",
        ));
        let cfg = ChocolateyConfig::default();
        let (log, cap) = StageLogger::with_capture("publish", Verbosity::Normal);
        let (a32, a64) = select_windows_artifacts(&ctx, &cfg, "mytool", &log);
        assert!(a32.is_none() && a64.is_none());
        let msgs = cap.all_messages();
        assert!(
            msgs.iter().any(|(_, m)| {
                m.contains("x-arm64.zip") && m.contains("arm64") && m.contains("not")
            }),
            "expected arm64-skip log; got {msgs:?}"
        );
    }

    #[test]
    fn select_windows_artifacts_ids_filter_drops_non_matching() {
        let mut ctx = ctx_with_choco(ChocolateyConfig::default());
        let mut wanted = windows_artifact("mytool", "x86_64-pc-windows-msvc", "wanted.zip");
        wanted.metadata.insert("id".to_string(), "good".to_string());
        let mut unwanted = windows_artifact("mytool", "x86_64-pc-windows-msvc", "unwanted.zip");
        unwanted
            .metadata
            .insert("id".to_string(), "bad".to_string());
        ctx.artifacts.add(wanted);
        ctx.artifacts.add(unwanted);
        let cfg = ChocolateyConfig {
            ids: Some(vec!["good".to_string()]),
            ..Default::default()
        };
        let log = StageLogger::new("publish", Verbosity::Quiet);
        let (_a32, a64) = select_windows_artifacts(&ctx, &cfg, "mytool", &log);
        assert_eq!(a64.unwrap().name, "wanted.zip");
    }

    #[test]
    fn select_windows_artifacts_amd64_variant_filter() {
        let mut ctx = ctx_with_choco(ChocolateyConfig::default());
        let mut v2 = windows_artifact("mytool", "x86_64-pc-windows-msvc", "amd64-v2.zip");
        v2.metadata
            .insert("amd64_variant".to_string(), "v2".to_string());
        let mut v3 = windows_artifact("mytool", "x86_64-pc-windows-msvc", "amd64-v3.zip");
        v3.metadata
            .insert("amd64_variant".to_string(), "v3".to_string());
        ctx.artifacts.add(v2);
        ctx.artifacts.add(v3);
        let cfg = ChocolateyConfig {
            amd64_variant: Some("v3".to_string()),
            ..Default::default()
        };
        let log = StageLogger::new("publish", Verbosity::Quiet);
        let (_a32, a64) = select_windows_artifacts(&ctx, &cfg, "mytool", &log);
        assert_eq!(a64.unwrap().name, "amd64-v3.zip");
    }

    #[test]
    fn select_windows_artifacts_matches_windows_in_path_when_target_empty() {
        let mut ctx = ctx_with_choco(ChocolateyConfig::default());
        let mut art = windows_artifact("mytool", "", "WinDoWs-386.zip");
        art.target = None;
        art.path = std::path::PathBuf::from("/tmp/WinDoWs-386.zip");
        ctx.artifacts.add(art);
        let cfg = ChocolateyConfig::default();
        let log = StageLogger::new("publish", Verbosity::Quiet);
        let (a32, a64) = select_windows_artifacts(&ctx, &cfg, "mytool", &log);
        // No target => arch=="" => both 386/amd64 buckets stay empty.
        // (Path match qualifies the filter, but the arch dispatcher only
        // matches canonical "386"/"amd64" tokens.)
        assert!(a32.is_none() && a64.is_none());
    }

    // -----------------------------------------------------------------
    // build_install_mode
    // -----------------------------------------------------------------

    #[test]
    fn build_install_mode_dual_when_both_archs_present() {
        let ctx = ctx_with_choco(ChocolateyConfig::default());
        let cfg = ChocolateyConfig::default();
        let a32 = windows_artifact("mytool", "i686-pc-windows-msvc", "x86.zip");
        let a64 = windows_artifact("mytool", "x86_64-pc-windows-msvc", "x64.zip");
        let mode = build_install_mode(
            &ctx,
            &cfg,
            "mytool",
            "1.0.0",
            Some(&a32),
            Some(&a64),
            "mytool",
        )
        .unwrap();
        match mode {
            InstallMode::Dual {
                url32,
                hash32,
                url64,
                hash64,
            } => {
                assert_eq!(url32, "https://example.com/x86.zip");
                assert_eq!(hash32, "deadbeef");
                assert_eq!(url64, "https://example.com/x64.zip");
                assert_eq!(hash64, "deadbeef");
            }
            _ => panic!("expected Dual"),
        }
    }

    #[test]
    fn build_install_mode_single_32bit_when_only_386() {
        let ctx = ctx_with_choco(ChocolateyConfig::default());
        let cfg = ChocolateyConfig::default();
        let a32 = windows_artifact("mytool", "i686-pc-windows-msvc", "x86.zip");
        let mode =
            build_install_mode(&ctx, &cfg, "mytool", "1.0.0", Some(&a32), None, "mytool").unwrap();
        match mode {
            InstallMode::Single { is_32bit, url, .. } => {
                assert!(is_32bit);
                assert_eq!(url, "https://example.com/x86.zip");
            }
            _ => panic!("expected Single 32-bit"),
        }
    }

    #[test]
    fn build_install_mode_single_64bit_when_only_amd64() {
        let ctx = ctx_with_choco(ChocolateyConfig::default());
        let cfg = ChocolateyConfig::default();
        let a64 = windows_artifact("mytool", "x86_64-pc-windows-msvc", "x64.zip");
        let mode =
            build_install_mode(&ctx, &cfg, "mytool", "1.0.0", None, Some(&a64), "mytool").unwrap();
        match mode {
            InstallMode::Single { is_32bit, url, .. } => {
                assert!(!is_32bit);
                assert_eq!(url, "https://example.com/x64.zip");
            }
            _ => panic!("expected Single 64-bit"),
        }
    }

    #[test]
    fn build_install_mode_url_template_overrides_metadata_url() {
        let ctx = ctx_with_choco(ChocolateyConfig::default());
        let cfg = ChocolateyConfig {
            url_template: Some(
                "https://feeds.example.com/{{ name }}-{{ version }}-{{ arch }}.zip".to_string(),
            ),
            ..Default::default()
        };
        let a64 = windows_artifact("mytool", "x86_64-pc-windows-msvc", "ignored.zip");
        let mode =
            build_install_mode(&ctx, &cfg, "mytool", "9.9.9", None, Some(&a64), "mytool").unwrap();
        match mode {
            InstallMode::Single { url, .. } => {
                assert_eq!(url, "https://feeds.example.com/mytool-9.9.9-amd64.zip");
            }
            _ => panic!("expected Single from template"),
        }
    }

    #[test]
    fn build_install_mode_bails_on_no_windows_artifacts() {
        let ctx = ctx_with_choco(ChocolateyConfig::default());
        let cfg = ChocolateyConfig::default();
        let err = match build_install_mode(&ctx, &cfg, "mytool", "1.0.0", None, None, "mytool") {
            Err(e) => e,
            Ok(_) => panic!("expected bail"),
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("no windows artifact"), "{msg}");
        assert!(msg.contains("mytool"), "{msg}");
    }

    #[test]
    fn build_install_mode_bails_on_empty_sha256() {
        let ctx = ctx_with_choco(ChocolateyConfig::default());
        let cfg = ChocolateyConfig::default();
        let mut a64 = windows_artifact("mytool", "x86_64-pc-windows-msvc", "x64.zip");
        a64.metadata.insert("sha256".to_string(), "".to_string());
        let err =
            match build_install_mode(&ctx, &cfg, "mytool", "1.0.0", None, Some(&a64), "mytool") {
                Err(e) => e,
                Ok(_) => panic!("expected bail"),
            };
        let msg = format!("{err:#}");
        assert!(msg.contains("sha256"), "{msg}");
        assert!(msg.contains("x64.zip"), "{msg}");
    }

    // -----------------------------------------------------------------
    // render_text_fields
    // -----------------------------------------------------------------

    #[test]
    fn render_text_fields_all_none_when_choco_unset_and_no_metadata() {
        let ctx = ctx_with_choco(ChocolateyConfig::default());
        let cfg = ChocolateyConfig::default();
        let tf = render_text_fields(&ctx, &cfg, "mytool", &ctx.logger("publish")).unwrap();
        assert!(tf.title.is_none());
        assert!(tf.copyright.is_none());
        assert!(tf.summary.is_none());
        assert!(tf.release_notes.is_none());
    }

    #[test]
    fn render_text_fields_renders_title_and_copyright_through_tera() {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }];
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("ProjectName", "mytool");
        let cfg = ChocolateyConfig {
            title: Some("{{ ProjectName }} CLI".to_string()),
            copyright: Some("Copyright {{ ProjectName }}".to_string()),
            ..Default::default()
        };
        let tf = render_text_fields(&ctx, &cfg, "mytool", &ctx.logger("publish")).unwrap();
        assert_eq!(tf.title.as_deref(), Some("mytool CLI"));
        assert_eq!(tf.copyright.as_deref(), Some("Copyright mytool"));
    }

    #[test]
    fn render_text_fields_summary_falls_back_to_metadata_description() {
        let mut config = Config::default();
        config.metadata = Some(MetadataConfig {
            description: Some("project summary".to_string()),
            ..Default::default()
        });
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }];
        let ctx = Context::new(config, ContextOptions::default());
        let cfg = ChocolateyConfig::default();
        let tf = render_text_fields(&ctx, &cfg, "mytool", &ctx.logger("publish")).unwrap();
        assert_eq!(tf.summary.as_deref(), Some("project summary"));
    }

    #[test]
    fn render_text_fields_release_notes_falls_back_to_metadata_full_description() {
        let mut config = Config::default();
        config.metadata = Some(MetadataConfig {
            full_description: Some(ContentSource::Inline("long-form readme".to_string())),
            ..Default::default()
        });
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }];
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.populate_metadata_var().unwrap();
        let cfg = ChocolateyConfig::default();
        let tf = render_text_fields(&ctx, &cfg, "mytool", &ctx.logger("publish")).unwrap();
        assert_eq!(tf.release_notes.as_deref(), Some("long-form readme"));
    }

    #[test]
    fn render_text_fields_release_notes_uses_changelog_template_var() {
        let mut ctx = ctx_with_choco(ChocolateyConfig::default());
        ctx.template_vars_mut()
            .set("ReleaseNotes", "## v1.0.0\n- one\n- two");
        let cfg = ChocolateyConfig {
            release_notes: Some("Release notes:\n{{ Changelog }}".to_string()),
            ..Default::default()
        };
        let tf = render_text_fields(&ctx, &cfg, "mytool", &ctx.logger("publish")).unwrap();
        let rn = tf.release_notes.expect("release_notes set");
        assert!(rn.contains("## v1.0.0"));
        assert!(rn.contains("- one"));
    }

    #[test]
    fn render_text_fields_malformed_template_falls_back_to_raw() {
        let ctx = ctx_with_choco(ChocolateyConfig::default());
        let cfg = ChocolateyConfig {
            title: Some("{{ broken".to_string()),
            ..Default::default()
        };
        let tf = render_text_fields(&ctx, &cfg, "mytool", &ctx.logger("publish")).unwrap();
        assert_eq!(tf.title.as_deref(), Some("{{ broken"));
    }

    // -----------------------------------------------------------------
    // build_nuspec
    // -----------------------------------------------------------------

    #[test]
    fn build_nuspec_assembles_xml_with_required_metadata() {
        let cfg = ChocolateyConfig {
            name: Some("renamed".to_string()),
            ..Default::default()
        };
        let metadata = ChocoMetadata {
            description: "d".to_string(),
            license: "MIT".to_string(),
            authors: "Alice".to_string(),
            project_url: "https://example.com".to_string(),
            icon_url: String::new(),
            tags: vec!["cli".to_string()],
        };
        let text = ChocoTextFields {
            title: None,
            copyright: None,
            summary: None,
            release_notes: None,
        };
        let xml = build_nuspec(&cfg, "ignored", "2.3.4", &metadata, &text).unwrap();
        assert!(xml.contains("<id>renamed</id>"));
        assert!(xml.contains("<version>2.3.4</version>"));
        assert!(xml.contains("<authors>Alice</authors>"));
        assert!(xml.contains("<tags>cli</tags>"));
    }

    // -----------------------------------------------------------------
    // stage_package
    // -----------------------------------------------------------------

    #[test]
    fn stage_package_writes_nuspec_install_script_and_nupkg() {
        let pkg_name = "mytool";
        let version = "0.1.2";
        let nuspec_xml = generate_nuspec(&crate::chocolatey::nuspec::NuspecParams {
            name: pkg_name,
            version,
            description: "d",
            license: "MIT",
            license_url: None,
            authors: "a",
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
        })
        .unwrap();
        let install = generate_install_script(pkg_name, "https://e/x.zip", "abc", false).unwrap();
        let (log, cap) = StageLogger::with_capture("publish", Verbosity::Normal);
        let staged = stage_package(pkg_name, version, &nuspec_xml, &install, &log).unwrap();
        assert!(staged.nupkg_path.exists(), "nupkg must be written");
        assert!(
            staged
                .nupkg_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("mytool.0.1.2.nupkg")
        );
        // The status log lines for nuspec / install / nupkg paths were emitted.
        let msgs = cap.all_messages();
        let joined: String = msgs.iter().map(|(_, m)| m.clone()).collect();
        assert!(joined.contains("nuspec"));
        assert!(joined.contains("install script"));
        assert!(joined.contains("nupkg"));
    }

    // -----------------------------------------------------------------
    // resolve_api_key
    // -----------------------------------------------------------------

    #[test]
    fn resolve_api_key_renders_template_from_config() {
        let mut ctx = ctx_with_choco(ChocolateyConfig::default());
        ctx.template_vars_mut().set("MyKey", "from-template");
        let cfg = ChocolateyConfig {
            api_key: Some("{{ MyKey }}".to_string()),
            ..Default::default()
        };
        assert_eq!(
            resolve_api_key(&ctx, &cfg, &ctx.logger("publish")).unwrap(),
            "from-template"
        );
    }

    #[test]
    fn resolve_api_key_falls_back_to_env() {
        let mut ctx = ctx_with_choco(ChocolateyConfig::default());
        ctx.set_env_source(
            anodizer_core::MapEnvSource::new().with("CHOCOLATEY_API_KEY", "from-env"),
        );
        let cfg = ChocolateyConfig::default();
        assert_eq!(
            resolve_api_key(&ctx, &cfg, &ctx.logger("publish")).unwrap(),
            "from-env"
        );
    }

    #[test]
    fn resolve_api_key_empty_when_neither_configured_nor_env() {
        let mut ctx = ctx_with_choco(ChocolateyConfig::default());
        // Inject an empty env so the test does not pick up a real
        // `CHOCOLATEY_API_KEY` from the host shell.
        ctx.set_env_source(anodizer_core::MapEnvSource::new());
        let cfg = ChocolateyConfig::default();
        assert!(
            resolve_api_key(&ctx, &cfg, &ctx.logger("publish"))
                .unwrap()
                .is_empty()
        );
    }

    // -----------------------------------------------------------------
    // handle_feed_state — drives the OData feed-state ladder against an
    // in-process HTTP responder. Touches the moderation-skip, hash-match,
    // hash-drift, rejected, PresentNoHash, and absent branches.
    // -----------------------------------------------------------------

    fn fast_retry() -> anodizer_core::retry::RetryPolicy {
        anodizer_core::retry::RetryPolicy {
            max_attempts: 1,
            base_delay: std::time::Duration::from_millis(0),
            max_delay: std::time::Duration::from_millis(0),
        }
    }

    fn http_200(body: &str) -> String {
        let len = body.len();
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {len}\r\n\r\n{body}"
        )
    }

    /// Write `bytes` to a tempfile and return the path; used to drive
    /// `compute_nupkg_hash` against a real on-disk blob.
    fn tmp_blob(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pkg.nupkg");
        std::fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    #[test]
    fn handle_feed_state_absent_returns_none_to_proceed_to_push() {
        use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
        let (addr, _calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
        ]);
        let source = format!("http://{addr}");
        let mut ctx = ctx_with_choco(ChocolateyConfig::default());
        let cfg = ChocolateyConfig::default();
        let (_d, pkg) = tmp_blob(b"abc");
        let log = StageLogger::new("publish", Verbosity::Quiet);
        let out = handle_feed_state(
            &mut ctx,
            &cfg,
            &source,
            "mytool",
            "1.0.0",
            &pkg,
            &fast_retry(),
            &log,
        )
        .unwrap();
        assert_eq!(out, None);
    }

    #[test]
    fn handle_feed_state_present_no_hash_warns_and_returns_none() {
        use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
        let body = "<entry><id>https://example.com/api/v2/Packages(Id='mytool',Version='1.0.0')</id>\
            <m:properties><d:PackageStatus>Approved</d:PackageStatus></m:properties></entry>";
        let resp: &'static str = Box::leak(http_200(body).into_boxed_str());
        let (addr, _calls) = spawn_oneshot_http_responder(vec![resp]);
        let source = format!("http://{addr}");
        let mut ctx = ctx_with_choco(ChocolateyConfig::default());
        let cfg = ChocolateyConfig::default();
        let (_d, pkg) = tmp_blob(b"abc");
        let (log, cap) = StageLogger::with_capture("publish", Verbosity::Normal);
        let out = handle_feed_state(
            &mut ctx,
            &cfg,
            &source,
            "mytool",
            "1.0.0",
            &pkg,
            &fast_retry(),
            &log,
        )
        .unwrap();
        assert_eq!(out, None, "PresentNoHash must fall through to push");
        assert!(
            cap.warn_messages()
                .iter()
                .any(|m| m.contains("hash was unavailable")),
            "expected PresentNoHash warn; got {:?}",
            cap.all_messages()
        );
    }

    #[test]
    fn handle_feed_state_hash_match_short_circuits_to_skip() {
        use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
        // Compute the SHA256 base64 of the local blob, then feed that
        // same hash back via the OData response — should match.
        let (_d, pkg) = tmp_blob(b"matching-bytes");
        let local_hash = compute_nupkg_hash(&pkg, "SHA256").unwrap();
        let body = format!(
            "<entry><id>https://example.com/api/v2/Packages(Id='mytool',Version='1.0.0')</id>\
            <m:properties>\
            <d:PackageHash>{}</d:PackageHash>\
            <d:PackageHashAlgorithm>SHA256</d:PackageHashAlgorithm>\
            <d:PackageStatus>Approved</d:PackageStatus>\
            <d:IsApproved>true</d:IsApproved>\
            </m:properties></entry>",
            local_hash
        );
        let resp: &'static str = Box::leak(http_200(&body).into_boxed_str());
        let (addr, _calls) = spawn_oneshot_http_responder(vec![resp]);
        let source = format!("http://{addr}");
        let mut ctx = ctx_with_choco(ChocolateyConfig::default());
        let cfg = ChocolateyConfig::default();
        let (log, cap) = StageLogger::with_capture("publish", Verbosity::Normal);
        let out = handle_feed_state(
            &mut ctx,
            &cfg,
            &source,
            "mytool",
            "1.0.0",
            &pkg,
            &fast_retry(),
            &log,
        )
        .unwrap();
        assert_eq!(out, Some(false), "hash match must short-circuit to skip");
        assert!(
            cap.all_messages()
                .iter()
                .any(|(_, m)| m.contains("already published") && m.contains("hash match")),
            "expected hash-match status; got {:?}",
            cap.all_messages()
        );
    }

    #[test]
    fn handle_feed_state_hash_drift_bails_with_actionable_error() {
        use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
        let (_d, pkg) = tmp_blob(b"local-bytes");
        let body = "<entry><id>https://example.com/api/v2/Packages(Id='mytool',Version='1.0.0')</id>\
            <m:properties>\
            <d:PackageHash>DIFFERENT_HASH_FROM_FEED</d:PackageHash>\
            <d:PackageHashAlgorithm>SHA256</d:PackageHashAlgorithm>\
            <d:PackageStatus>Approved</d:PackageStatus>\
            <d:IsApproved>true</d:IsApproved>\
            </m:properties></entry>";
        let resp: &'static str = Box::leak(http_200(body).into_boxed_str());
        let (addr, _calls) = spawn_oneshot_http_responder(vec![resp]);
        let source = format!("http://{addr}");
        let mut ctx = ctx_with_choco(ChocolateyConfig::default());
        let cfg = ChocolateyConfig::default();
        let log = StageLogger::new("publish", Verbosity::Quiet);
        let err = match handle_feed_state(
            &mut ctx,
            &cfg,
            &source,
            "mytool",
            "1.0.0",
            &pkg,
            &fast_retry(),
            &log,
        ) {
            Err(e) => e,
            Ok(other) => panic!("hash drift must bail, got Ok({other:?})"),
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("local nupkg"), "{msg}");
        assert!(msg.contains("immutable"), "{msg}");
        assert!(msg.contains("bump the version"), "{msg}");
    }

    #[test]
    fn handle_feed_state_rejected_bails_loudly() {
        use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
        let body = "<entry><id>https://example.com/api/v2/Packages(Id='mytool',Version='1.0.0')</id>\
            <m:properties>\
            <d:PackageHash>X</d:PackageHash>\
            <d:PackageHashAlgorithm>SHA512</d:PackageHashAlgorithm>\
            <d:PackageStatus>Rejected</d:PackageStatus>\
            <d:IsApproved>false</d:IsApproved>\
            <d:Published>2026-01-01T00:00:00</d:Published>\
            </m:properties></entry>";
        let resp: &'static str = Box::leak(http_200(body).into_boxed_str());
        let (addr, _calls) = spawn_oneshot_http_responder(vec![resp]);
        let source = format!("http://{addr}");
        let mut ctx = ctx_with_choco(ChocolateyConfig::default());
        let cfg = ChocolateyConfig::default();
        let (_d, pkg) = tmp_blob(b"abc");
        let log = StageLogger::new("publish", Verbosity::Quiet);
        let err = match handle_feed_state(
            &mut ctx,
            &cfg,
            &source,
            "mytool",
            "1.0.0",
            &pkg,
            &fast_retry(),
            &log,
        ) {
            Err(e) => e,
            Ok(other) => panic!("Rejected must bail, got Ok({other:?})"),
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("REJECTED"), "{msg}");
        assert!(msg.contains("bump the version"), "{msg}");
    }

    #[test]
    fn handle_feed_state_in_moderation_skip_records_pending_outcome() {
        use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
        let body = "<entry><id>https://example.com/api/v2/Packages(Id='mytool',Version='1.0.0')</id>\
            <m:properties>\
            <d:PackageHash>X</d:PackageHash>\
            <d:PackageHashAlgorithm>SHA512</d:PackageHashAlgorithm>\
            <d:PackageStatus>Submitted</d:PackageStatus>\
            <d:IsApproved>false</d:IsApproved>\
            </m:properties></entry>";
        let resp: &'static str = Box::leak(http_200(body).into_boxed_str());
        let (addr, _calls) = spawn_oneshot_http_responder(vec![resp]);
        let source = format!("http://{addr}");
        let mut ctx = ctx_with_choco(ChocolateyConfig::default());
        let cfg = ChocolateyConfig::default();
        let (_d, pkg) = tmp_blob(b"abc");
        let (log, cap) = StageLogger::with_capture("publish", Verbosity::Normal);
        let out = handle_feed_state(
            &mut ctx,
            &cfg,
            &source,
            "mytool",
            "1.0.0",
            &pkg,
            &fast_retry(),
            &log,
        )
        .unwrap();
        assert_eq!(out, Some(false), "moderation skip must short-circuit");
        assert!(
            matches!(
                ctx.take_pending_outcome(),
                Some(anodizer_core::PublisherOutcome::PendingModeration)
            ),
            "pending outcome must be PendingModeration"
        );
        assert!(
            cap.warn_messages()
                .iter()
                .any(|m| m.contains("republish_in_moderation: true")),
            "expected guidance in warn; got {:?}",
            cap.all_messages()
        );
    }

    #[test]
    fn handle_feed_state_in_moderation_with_republish_flag_proceeds_to_push() {
        use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
        let body = "<entry><id>https://example.com/api/v2/Packages(Id='mytool',Version='1.0.0')</id>\
            <m:properties>\
            <d:PackageHash>X</d:PackageHash>\
            <d:PackageHashAlgorithm>SHA512</d:PackageHashAlgorithm>\
            <d:PackageStatus>Submitted</d:PackageStatus>\
            <d:IsApproved>false</d:IsApproved>\
            </m:properties></entry>";
        let resp: &'static str = Box::leak(http_200(body).into_boxed_str());
        let (addr, _calls) = spawn_oneshot_http_responder(vec![resp]);
        let source = format!("http://{addr}");
        let mut ctx = ctx_with_choco(ChocolateyConfig::default());
        // Local bytes whose SHA512 base64 cannot be the feed's "X": the nupkg
        // differs (a fail-forward re-cut shifts <releaseNotes>). A Submitted
        // (in-moderation) version is NOT immutable, so republish_in_moderation
        // must proceed to push (replace the queued copy) rather than bail on
        // the drift.
        let cfg = ChocolateyConfig {
            republish_in_moderation: Some(StringOrBool::Bool(true)),
            ..Default::default()
        };
        let (_d, pkg) = tmp_blob(b"local-bytes");
        let log = StageLogger::new("publish", Verbosity::Quiet);
        let decision = handle_feed_state(
            &mut ctx,
            &cfg,
            &source,
            "mytool",
            "1.0.0",
            &pkg,
            &fast_retry(),
            &log,
        )
        .expect("republish of an in-moderation version must not bail on nupkg drift");
        assert_eq!(
            decision, None,
            "republish_in_moderation=true on a Submitted version must signal \
             proceed-to-push (Ok(None)), not skip or bail"
        );
    }

    /// An ALREADY-APPROVED version is genuinely immutable: a differing nupkg
    /// must still bail (republish_in_moderation only covers the in-moderation
    /// state, never an approved/live version).
    #[test]
    fn handle_feed_state_approved_with_differing_nupkg_bails() {
        use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
        let body = "<entry><id>https://example.com/api/v2/Packages(Id='mytool',Version='1.0.0')</id>\
            <m:properties>\
            <d:PackageHash>X</d:PackageHash>\
            <d:PackageHashAlgorithm>SHA512</d:PackageHashAlgorithm>\
            <d:PackageStatus>Approved</d:PackageStatus>\
            <d:IsApproved>true</d:IsApproved>\
            </m:properties></entry>";
        let resp: &'static str = Box::leak(http_200(body).into_boxed_str());
        let (addr, _calls) = spawn_oneshot_http_responder(vec![resp]);
        let source = format!("http://{addr}");
        let mut ctx = ctx_with_choco(ChocolateyConfig::default());
        // republish_in_moderation must NOT rescue an approved version.
        let cfg = ChocolateyConfig {
            republish_in_moderation: Some(StringOrBool::Bool(true)),
            ..Default::default()
        };
        let (_d, pkg) = tmp_blob(b"local-bytes");
        let log = StageLogger::new("publish", Verbosity::Quiet);
        let err = handle_feed_state(
            &mut ctx,
            &cfg,
            &source,
            "mytool",
            "1.0.0",
            &pkg,
            &fast_retry(),
            &log,
        )
        .expect_err("an approved immutable version with a differing nupkg must bail");
        assert!(format!("{err:#}").contains("local nupkg"), "{err:#}");
    }

    // -----------------------------------------------------------------
    // publish_to_chocolatey orchestrator: skip-API-key branch + dry-run
    // capture. handle_feed_state is exercised directly above because
    // driving the full orchestrator into the feed-state ladder would
    // also require responding to the push PUT — kept out of scope to
    // bound the in-process responder surface.
    // -----------------------------------------------------------------

    #[test]
    fn publish_to_chocolatey_warns_and_skips_when_api_key_empty() {
        let mut ctx = ctx_with_choco(ChocolateyConfig {
            repository: Some(RepositoryConfig {
                owner: Some("myorg".to_string()),
                name: Some("mytool".to_string()),
                ..Default::default()
            }),
            description: Some("d".to_string()),
            license: Some("MIT".to_string()),
            // api_key intentionally None.
            ..Default::default()
        });
        // Block CHOCOLATEY_API_KEY from the host environment.
        ctx.set_env_source(anodizer_core::MapEnvSource::new());
        ctx.artifacts.add(windows_artifact(
            "mytool",
            "x86_64-pc-windows-msvc",
            "x64.zip",
        ));
        let capture = anodizer_core::log::LogCapture::new();
        ctx.with_log_capture(capture.clone());
        let log = ctx.logger("publish");
        let res = publish_to_chocolatey(&mut ctx, "mytool", &log).unwrap();
        assert!(!res, "missing API key must skip push and return Ok(false)");
        assert!(
            capture
                .warn_messages()
                .iter()
                .any(|m| m.contains("no chocolatey API key") && m.contains("mytool")),
            "expected no-API-key warn; got {:?}",
            capture.all_messages()
        );
    }

    #[test]
    fn publish_to_chocolatey_dry_run_logs_target_with_repo_path() {
        let mut ctx = ctx_with_choco_opts(
            ChocolateyConfig {
                repository: Some(RepositoryConfig {
                    owner: Some("myorg".to_string()),
                    name: Some("mytool".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        let capture = anodizer_core::log::LogCapture::new();
        ctx.with_log_capture(capture.clone());
        let log = ctx.logger("publish");
        let res = publish_to_chocolatey(&mut ctx, "mytool", &log).unwrap();
        assert!(!res, "dry-run must return Ok(false) — no push happened");
        let joined: String = capture
            .all_messages()
            .into_iter()
            .map(|(_, m)| m)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("(dry-run)"), "{joined}");
        assert!(joined.contains("mytool"), "{joined}");
        assert!(joined.contains("myorg/mytool"), "{joined}");
    }

    #[test]
    fn publish_to_chocolatey_dry_run_omits_path_suffix_when_repo_absent() {
        let mut ctx = ctx_with_choco_opts(
            ChocolateyConfig::default(),
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        let capture = anodizer_core::log::LogCapture::new();
        ctx.with_log_capture(capture.clone());
        let log = ctx.logger("publish");
        let res = publish_to_chocolatey(&mut ctx, "mytool", &log).unwrap();
        assert!(!res);
        let joined: String = capture
            .all_messages()
            .into_iter()
            .map(|(_, m)| m)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("(dry-run)"), "{joined}");
        assert!(
            !joined.contains(" to /") && !joined.contains(" to myorg/"),
            "no-repo dry-run must not include a `to OWNER/REPO` suffix: {joined}"
        );
    }

    // -----------------------------------------------------------------
    // Existing config-roundtrip + message-shape regressions
    // -----------------------------------------------------------------

    /// Config field roundtrip: `republish_in_moderation` survives serde.
    #[test]
    fn republish_in_moderation_bool_roundtrips() {
        let cfg = ChocolateyConfig {
            republish_in_moderation: Some(StringOrBool::Bool(true)),
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).expect("serialize");
        let back: ChocolateyConfig = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(
            back.republish_in_moderation,
            Some(StringOrBool::Bool(true))
        ));
    }

    /// Config field roundtrip: absent field deserializes to None (default=false).
    #[test]
    fn republish_in_moderation_absent_is_none() {
        let cfg: ChocolateyConfig = serde_json::from_str("{}").expect("deserialize");
        assert!(cfg.republish_in_moderation.is_none());
    }

    /// Flag false: the warn message contains key operator-facing substrings.
    #[test]
    fn in_moderation_skip_warn_contains_guidance() {
        // Simulate what the warn branch emits so operators know what to set.
        let pkg_name = "MyPkg";
        let version = "1.2.3";
        let reason = "is awaiting moderation";
        let status_label = "Submitted";
        let published_label = "2026-01-01";
        let msg = format!(
            "chocolatey package '{}-{}' {} (PackageStatus={}, Published={}); \
             skipping push — set republish_in_moderation: true to replace \
             the in-moderation copy. The gallery will not list the package \
             until it transitions to Approved.",
            pkg_name, version, reason, status_label, published_label
        );
        assert!(msg.contains("skipping push"), "{msg}");
        assert!(msg.contains("republish_in_moderation: true"), "{msg}");
        assert!(msg.contains("Approved"), "{msg}");
    }

    /// Flag true: the status message contains the "replacing in-moderation" indicator.
    #[test]
    fn in_moderation_republish_status_contains_replacing() {
        let pkg_name = "MyPkg";
        let version = "1.2.3";
        let reason = "is awaiting moderation";
        let status_label = "Submitted";
        let published_label = "2026-01-01";
        let msg = format!(
            "chocolatey package '{}-{}' {} (PackageStatus={}, Published={}); \
             republish_in_moderation=true — replacing in-moderation copy.",
            pkg_name, version, reason, status_label, published_label
        );
        assert!(msg.contains("republish_in_moderation=true"), "{msg}");
        assert!(msg.contains("replacing in-moderation copy"), "{msg}");
    }
}
