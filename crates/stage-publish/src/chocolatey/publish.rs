//! `publish_to_chocolatey` orchestrator — assembles the nuspec + install
//! script, packs a nupkg natively, and pushes via the NuGet V2 API.

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};

use crate::util;

use super::install::{InstallScriptDual, generate_install_script, generate_install_script_dual};
use super::nuspec::{NuspecParams, generate_nuspec};
use super::package::{
    FeedHashResult, compute_nupkg_hash, create_nupkg, package_feed_hash, push_nupkg,
};

pub fn publish_to_chocolatey(ctx: &Context, crate_name: &str, log: &StageLogger) -> Result<()> {
    let (_crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "chocolatey")?;

    let choco_cfg = publish
        .chocolatey
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("chocolatey: no chocolatey config for '{}'", crate_name))?;

    let repository = choco_cfg
        .repository
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("chocolatey: no repository config for '{}'", crate_name))?;
    let (repo_owner, repo_name) = match (repository.owner.as_deref(), repository.name.as_deref()) {
        (Some(o), Some(n)) => (o, n),
        _ => anyhow::bail!(
            "chocolatey: repository.owner and repository.name are both required for '{}'",
            crate_name
        ),
    };

    // GoReleaser checks SkipPublish early in Publish(), before any work.
    if let Some(d) = choco_cfg.skip.as_ref() {
        let off = d
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .with_context(|| format!("chocolatey: render skip template for '{}'", crate_name))?;
        if off {
            log.status(&format!(
                "chocolatey: skipping publish for '{}' (skip=true)",
                crate_name
            ));
            return Ok(());
        }
    }

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would push Chocolatey package for '{}' to {}/{}",
            crate_name, repo_owner, repo_name
        ));
        return Ok(());
    }

    let version = ctx.version();
    // GoReleaser Pro parity: fall back to project `metadata.*` when choco config unset.
    let description_raw = choco_cfg
        .description
        .as_deref()
        .or_else(|| ctx.config.meta_description())
        .unwrap_or(crate_name);
    let description = ctx
        .render_template(description_raw)
        .unwrap_or_else(|_| description_raw.to_string());
    let license = choco_cfg
        .license
        .clone()
        .or_else(|| ctx.config.meta_license().map(str::to_string))
        .unwrap_or_default();
    let authors = choco_cfg
        .authors
        .clone()
        .or_else(|| ctx.config.meta_first_maintainer().map(str::to_string))
        .unwrap_or_else(|| crate_name.to_string());
    let project_url = choco_cfg
        .project_url
        .clone()
        .unwrap_or_else(|| format!("https://github.com/{}/{}", repo_owner, repo_name));
    let icon_url = choco_cfg.icon_url.clone().unwrap_or_default();
    let tags = choco_cfg.tags.clone().unwrap_or_default();

    // Find both 32-bit and 64-bit Windows artifacts (GoReleaser parity).
    // Apply IDs + amd64_variant filter.
    let ids_filter = choco_cfg.ids.as_deref();
    let url_template = choco_cfg.url_template.as_deref();
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

    let pkg_name = choco_cfg.name.as_deref().unwrap_or(crate_name);

    let is_32bit_target = |target: &str| -> bool {
        let lower = target.to_ascii_lowercase();
        lower.contains("i686")
            || lower.contains("i386")
            || lower.contains("386")
            || (lower.contains("x86") && !lower.contains("x86_64") && !lower.contains("x86-64"))
    };

    let mut artifact_32 = None;
    let mut artifact_64 = None;
    for a in &win_artifacts {
        let target = a.target.as_deref().unwrap_or("");
        if is_32bit_target(target) {
            if artifact_32.is_none() {
                artifact_32 = Some(a);
            }
        } else if artifact_64.is_none() {
            artifact_64 = Some(a);
        }
    }

    let resolve_artifact = |a: &anodizer_core::artifact::Artifact| -> (String, String) {
        let target = a.target.as_deref().unwrap_or("");
        let (_, raw_arch) = anodizer_core::target::map_target(target);
        let resolved_url = if let Some(tmpl) = url_template {
            util::render_url_template(tmpl, pkg_name, &version, &raw_arch, "windows")
        } else {
            a.metadata
                .get("url")
                .cloned()
                .unwrap_or_else(|| a.path.to_string_lossy().into_owned())
        };
        let sha256 = a.metadata.get("sha256").cloned().unwrap_or_default();
        (resolved_url, sha256)
    };

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

    let install_mode = match (artifact_32, artifact_64) {
        (Some(a32), Some(a64)) => {
            let (url32, hash32) = resolve_artifact(a32);
            let (url64, hash64) = resolve_artifact(a64);
            InstallMode::Dual {
                url32,
                hash32,
                url64,
                hash64,
            }
        }
        (Some(a32), None) => {
            let (url, hash) = resolve_artifact(a32);
            InstallMode::Single {
                url,
                hash,
                is_32bit: true,
            }
        }
        (None, Some(a64)) => {
            let (url, hash) = resolve_artifact(a64);
            InstallMode::Single {
                url,
                hash,
                is_32bit: false,
            }
        }
        (None, None) => {
            // No Windows artifact = no install script that can possibly
            // verify or download the binary. Pushing a nupkg with an empty
            // checksum and a fabricated GitHub URL is what trips moderator
            // rejection (broken install script). GoReleaser fails this case
            // loudly (errNoWindowsArchive at chocolatey.go:21,120); we now
            // match that behavior.
            anyhow::bail!(
                "chocolatey: no windows artifact found for '{}'. Chocolatey \
                 requires a Windows archive (or msi/nsis when configured via \
                 `use:`) to construct a working install script. Either build \
                 a Windows target for this crate or remove the chocolatey \
                 publisher config.",
                crate_name
            );
        }
    };

    let title_rendered = choco_cfg
        .title
        .as_deref()
        .map(|t| ctx.render_template(t).unwrap_or_else(|_| t.to_string()));

    // Template-render Copyright, Summary, Description, ReleaseNotes
    // (GoReleaser parity: chocolatey.go:218-227). `Changelog` is injected
    // as a per-render extra (matching GoReleaser WithExtraFields) so users
    // migrating GoReleaser configs that use `{{ .Changelog }}` work.
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
    let copyright_rendered = render(choco_cfg.copyright.as_deref());
    let summary_rendered = render(choco_cfg.summary.as_deref());
    let release_notes_rendered = render(choco_cfg.release_notes.as_deref());

    let nuspec = generate_nuspec(&NuspecParams {
        name: choco_cfg.name.as_deref().unwrap_or(crate_name),
        version: &version,
        description: &description,
        license: &license,
        license_url: choco_cfg.license_url.as_deref(),
        authors: &authors,
        project_url: &project_url,
        icon_url: &icon_url,
        tags: &tags,
        package_source_url: choco_cfg.package_source_url.as_deref(),
        owners: choco_cfg.owners.as_deref(),
        title: title_rendered.as_deref(),
        copyright: copyright_rendered.as_deref(),
        require_license_acceptance: choco_cfg.require_license_acceptance.unwrap_or(false),
        project_source_url: choco_cfg.project_source_url.as_deref(),
        docs_url: choco_cfg.docs_url.as_deref(),
        bug_tracker_url: choco_cfg.bug_tracker_url.as_deref(),
        summary: summary_rendered.as_deref(),
        release_notes: release_notes_rendered.as_deref(),
        dependencies: choco_cfg.dependencies.as_deref().unwrap_or(&[]),
    })?;
    let install_script = match &install_mode {
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
        })?,
        InstallMode::Single {
            url,
            hash,
            is_32bit,
        } => generate_install_script(pkg_name, url, hash, *is_32bit)?,
    };

    let tmp_dir = tempfile::tempdir().context("chocolatey: create temp dir")?;
    let pkg_dir = tmp_dir.path();
    let nuspec_path = pkg_dir.join(format!("{}.nuspec", pkg_name));
    std::fs::write(&nuspec_path, &nuspec)
        .with_context(|| format!("chocolatey: write nuspec {}", nuspec_path.display()))?;

    let tools_dir = pkg_dir.join("tools");
    std::fs::create_dir_all(&tools_dir).context("chocolatey: create tools dir")?;

    let install_path = tools_dir.join("chocolateyinstall.ps1");
    std::fs::write(&install_path, &install_script).with_context(|| {
        format!(
            "chocolatey: write install script {}",
            install_path.display()
        )
    })?;

    log.status(&format!(
        "wrote Chocolatey nuspec: {}",
        nuspec_path.display()
    ));
    log.status(&format!(
        "wrote Chocolatey install script: {}",
        install_path.display()
    ));

    // Create .nupkg natively (OPC/ZIP format) — no `choco` CLI dependency.
    // A nupkg is a ZIP containing the nuspec, tools/, and OPC metadata files.
    let nupkg_path = pkg_dir.join(format!("{}.{}.nupkg", pkg_name, version));
    create_nupkg(pkg_name, &version, &nuspec_path, &tools_dir, &nupkg_path)?;
    log.status(&format!("created nupkg: {}", nupkg_path.display()));

    // Template-render APIKey (GoReleaser parity: chocolatey.go:184)
    let api_key = choco_cfg
        .api_key
        .as_deref()
        .map(|k| ctx.render_template(k).unwrap_or_else(|_| k.to_string()))
        .or_else(|| std::env::var("CHOCOLATEY_API_KEY").ok())
        .unwrap_or_default();

    if api_key.is_empty() {
        log.warn(&format!(
            "chocolatey: no API key for '{}', skipping push",
            crate_name
        ));
        return Ok(());
    }

    let source = choco_cfg
        .source_repo
        .as_deref()
        .unwrap_or("https://push.chocolatey.org/");

    // Idempotency with drift detection: Chocolatey package versions are
    // immutable once submitted, so re-pushing returns 403. We treat a
    // version-already-on-feed as a skip ONLY when the feed's recorded package
    // hash matches our local nupkg hash. If they differ, our local nupkg has
    // diverged from what the feed has — typically because the same git tag
    // was re-released with different artifact bytes — and silently skipping
    // would publish an install script that points at an archive whose sha
    // no longer matches (Chocolatey's verifier then rejects the package).
    // In that case we fail loudly and tell the user to bump the version.
    match package_feed_hash(source, pkg_name, &version) {
        FeedHashResult::Present {
            hash,
            algorithm,
            status,
            listed,
            published,
        } => {
            // A version stuck in the community moderation queue (Listed=false,
            // status=Submitted/Unknown/Rejected/Exempted) MUST NOT be re-pushed
            // — Chocolatey rejects re-pushes of submitted versions, and the
            // hash-match check below would otherwise silently no-op every CI
            // run while the gallery shows nothing. Surface the queue state
            // explicitly so the operator knows to wait or contact moderators.
            if listed == Some(false) {
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
                log.status(&format!(
                    "chocolatey: '{}-{}' is in the community moderation queue \
                     (PackageStatus={}, Published={}); not re-pushing — waiting on \
                     moderator approval. The gallery will not list the package until \
                     it transitions to Listed=true.",
                    pkg_name, version, status_label, published_label
                ));
                return Ok(());
            }
            let local = compute_nupkg_hash(&nupkg_path, &algorithm)?;
            if local == hash {
                log.status(&format!(
                    "chocolatey: skipping '{}-{}' — already published (hash match)",
                    pkg_name, version
                ));
                return Ok(());
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
            // Feed reports the version exists but didn't expose a hash we
            // could parse. Be conservative: don't silently skip without
            // verification — this is the scenario that bit us before. Log
            // the situation and let the push attempt proceed; Chocolatey
            // will return 403 with a recognizable message if it truly is
            // immutable, and that surfaces as a real error.
            log.warn(&format!(
                "chocolatey: '{}-{}' exists on feed but hash was unavailable; \
                 attempting push so any conflict surfaces as a real error",
                pkg_name, version
            ));
        }
        FeedHashResult::Absent => {
            // Not on feed — push normally.
        }
    }

    // Push via NuGet V2 API — same protocol as `choco push`.
    push_nupkg(&nupkg_path, source, &api_key, log)?;

    log.status(&format!("Chocolatey package pushed for '{}'", crate_name));
    Ok(())
}
