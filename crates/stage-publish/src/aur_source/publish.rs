use anodizer_core::config::AurSourceConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};

use crate::aur::AurRendered;
use crate::util;

use super::*;

/// Shared core logic for publishing a single AUR source entry.
///
/// Both per-crate (`publish_to_aur_source`) and top-level
/// (`publish_top_level_aur_sources`) delegate here after resolving which
/// `AurSourceConfig` to use and after evaluating the skip / `if:` gate.
pub(super) fn publish_aur_source_entry(
    ctx: &mut Context,
    cfg: &AurSourceConfig,
    default_name: &str,
    strip_bin_suffix: bool,
    label: &str,
    log: &StageLogger,
) -> Result<bool> {
    let AurSourceRender {
        rendered: AurRendered {
            pkgbuild, srcinfo, ..
        },
        pkg_name,
        install_filename,
        scoped_vars,
    } = render_aur_source_inner(ctx, cfg, default_name, strip_bin_suffix, label, log)?;

    let version = ctx
        .template_vars()
        .get("Version")
        .cloned()
        .unwrap_or_else(|| "0.0.0".to_string())
        .replace('-', "_");

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would publish AUR source package '{}' ({})",
            pkg_name, label
        ));
        log.verbose(&format!("PKGBUILD:\n{}", pkgbuild));
        return Ok(false);
    }

    // Write files to dist
    let dist = ctx.config.dist.clone();
    let aur_dir = dist.join("aur_source").join(&pkg_name);
    std::fs::create_dir_all(&aur_dir)
        .with_context(|| format!("{}: create dir {}", label, aur_dir.display()))?;

    write_aur_source_files(
        &aur_dir,
        &pkgbuild,
        &srcinfo,
        &install_filename,
        cfg.install.as_deref(),
        label,
    )?;

    // Register artifacts
    ctx.artifacts.add(anodizer_core::artifact::Artifact {
        kind: anodizer_core::artifact::ArtifactKind::SourcePkgBuild,
        name: "PKGBUILD".to_string(),
        path: aur_dir.join("PKGBUILD"),
        target: None,
        crate_name: pkg_name.clone(),
        metadata: {
            let mut m = std::collections::HashMap::new();
            m.insert("id".to_string(), pkg_name.clone());
            m.insert("format".to_string(), "aur_source".to_string());
            m
        },
        size: None,
    });

    ctx.artifacts.add(anodizer_core::artifact::Artifact {
        kind: anodizer_core::artifact::ArtifactKind::SourceSrcInfo,
        name: ".SRCINFO".to_string(),
        path: aur_dir.join(".SRCINFO"),
        target: None,
        crate_name: pkg_name.clone(),
        metadata: {
            let mut m = std::collections::HashMap::new();
            m.insert("id".to_string(), pkg_name.clone());
            m
        },
        size: None,
    });

    // Push to the AUR git repo. An explicit `git_url` is a verbatim
    // override; otherwise derive the canonical AUR remote from the resolved
    // package name (the same `pkgbase` written into PKGBUILD/.SRCINFO) so the
    // push target can never drift from the package name.
    let git_url = aur_source_push_git_url(cfg, &pkg_name);

    let tmp_dir = tempfile::tempdir().context(format!("{}: create temp dir", label))?;
    let repo_path = tmp_dir.path();

    if cfg.private_key.is_some() || cfg.git_ssh_command.is_some() {
        // `private_key` / `git_ssh_command` may be templated
        // (`{{ .Env.AUR_SSH_KEY }}`). Render against the same `Amd64`-scoped
        // vars as the rest of this resource before they reach the SSH clone,
        // or the literal template text is written to the key file and ssh
        // fails with "error in libcrypto".
        let strict = ctx.render_is_strict();
        let rendered_key = match cfg.private_key.as_deref() {
            Some(pk) => Some(util::render_or_warn_with_vars(
                &scoped_vars,
                log,
                "aur_source.private_key",
                pk,
                strict,
            )?),
            None => None,
        };
        let rendered_ssh = match cfg.git_ssh_command.as_deref() {
            Some(sc) => Some(util::render_or_warn_with_vars(
                &scoped_vars,
                log,
                "aur_source.git_ssh_command",
                sc,
                strict,
            )?),
            None => None,
        };
        util::clone_repo_ssh(
            &git_url,
            rendered_key.as_deref(),
            rendered_ssh.as_deref(),
            repo_path,
            label,
            log,
        )?;
    } else {
        util::clone_repo_with_auth(&git_url, None, repo_path, label, log)?;
    }

    let output_dir = if let Some(ref dir) = cfg.directory {
        // Render against the `Amd64`-scoped vars from the inner render so a
        // `directory: "{{ .Amd64 }}/…"` template resolves to the configured
        // variant, consistent with the url_template / hook renders.
        let rendered_dir =
            util::render_or_warn_with_vars(&scoped_vars, log, label, dir, ctx.render_is_strict())?;
        let d = repo_path.join(&rendered_dir);
        std::fs::create_dir_all(&d)?;
        d
    } else {
        repo_path.to_path_buf()
    };

    std::fs::copy(aur_dir.join("PKGBUILD"), output_dir.join("PKGBUILD"))
        .with_context(|| format!("{label}: copy PKGBUILD to output dir"))?;
    std::fs::copy(aur_dir.join(".SRCINFO"), output_dir.join(".SRCINFO"))
        .with_context(|| format!("{label}: copy .SRCINFO to output dir"))?;
    if cfg.install.is_some() {
        std::fs::copy(
            aur_dir.join(&install_filename),
            output_dir.join(&install_filename),
        )
        .with_context(|| format!("{label}: copy {install_filename} to output dir"))?;
    }

    let commit_msg = crate::homebrew::render_commit_msg(
        cfg.commit_msg_template.as_deref(),
        &pkg_name,
        &version,
        "package",
        log,
        ctx.render_is_strict(),
    )?;
    let commit_opts = util::resolve_commit_opts(ctx, cfg.commit_author.as_ref(), log)?;
    let outcome = util::commit_and_push_with_opts(
        repo_path,
        &["."],
        &commit_msg,
        None,
        label,
        &commit_opts,
        log,
    )?;
    match outcome {
        util::CommitOutcome::Pushed => {
            log.status(&format!(
                "pushed package '{}' for {} to {}",
                pkg_name, label, git_url
            ));
        }
        util::CommitOutcome::NoChanges => {
            log.status(&format!(
                "nothing to push for {} — package '{}' already up to date",
                label, pkg_name
            ));
        }
    }
    Ok(outcome.is_pushed())
}

/// Publish AUR source packages for a crate (per-crate config path).
pub fn publish_to_aur_source(
    ctx: &mut Context,
    crate_name: &str,
    log: &StageLogger,
) -> Result<bool> {
    let crate_cfg = crate::util::find_crate_in_universe(ctx, crate_name)
        .ok_or_else(|| anyhow::anyhow!("aur_source: crate '{}' not found", crate_name))?;
    let publish_cfg = crate_cfg
        .publish
        .as_ref()
        .and_then(|p| p.aur_source.as_ref())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "aur_source: no aur_source config for crate '{}'",
                crate_name
            )
        })?
        .clone();

    let label = format!("aur_source: crate '{crate_name}'");
    if crate::util::should_skip_publisher_with_if(
        ctx,
        publish_cfg.skip.as_ref(),
        publish_cfg.skip_upload.as_ref(),
        publish_cfg.if_condition.as_deref(),
        &label,
        log,
    )? {
        return Ok(false);
    }

    publish_aur_source_entry(ctx, &publish_cfg, crate_name, false, "aur_source", log)
}

/// Publish top-level `aur_sources` entries (not tied to a specific crate).
///
/// The AUR-sources publisher reads the `aur_sources` config
/// as a project-wide array. Each entry generates a source PKGBUILD and .SRCINFO,
/// then pushes them to the configured AUR git repo.
pub fn publish_top_level_aur_sources(ctx: &mut Context, log: &StageLogger) -> Result<bool> {
    let entries = match ctx.config.aur_sources {
        Some(ref v) if !v.is_empty() => v.clone(),
        _ => return Ok(false),
    };

    let project_name = ctx
        .template_vars()
        .get("ProjectName")
        .cloned()
        .unwrap_or_default();

    let mut any_pushed = false;
    for (i, cfg) in entries.iter().enumerate() {
        let label = format!("aur_sources[{}]", i);
        if crate::util::should_skip_publisher_with_if(
            ctx,
            cfg.skip.as_ref(),
            cfg.skip_upload.as_ref(),
            cfg.if_condition.as_deref(),
            &label,
            log,
        )? {
            continue;
        }

        any_pushed |= publish_aur_source_entry(ctx, cfg, &project_name, true, &label, log)?;
    }

    Ok(any_pushed)
}
