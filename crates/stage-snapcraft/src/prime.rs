use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use anodizer_core::artifact::Artifact;
use anodizer_core::config::SnapcraftConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;

use crate::build_stage::{copy_snap_icon, resolve_icon_path};
use crate::yaml::render_snap_yaml;

/// Stage the snapcraft prime dir: write `snap.yaml`, copy icon /
/// binaries / extra files / templated extras / completers, and apply
/// `mod_timestamp`. Returns the owning `TempDir` (its `Drop` reaps the
/// staged tree once the worker finishes) and the `prime/` subdirectory.
#[allow(clippy::too_many_arguments)]
pub(crate) fn stage_prime_dir(
    ctx: &Context,
    log: &StageLogger,
    snap_cfg: &SnapcraftConfig,
    krate_name: &str,
    snap_name: &str,
    target_binaries: &[&Artifact],
    target: Option<&str>,
    version: &str,
) -> Result<(tempfile::TempDir, PathBuf)> {
    let tmp_dir = tempfile::tempdir().context("create temp dir for snapcraft build")?;
    let prime_dir = tmp_dir.path().join("prime");
    let meta_dir = prime_dir.join("meta");
    fs::create_dir_all(&meta_dir)
        .with_context(|| format!("create prime/meta dir: {}", meta_dir.display()))?;

    let all_binary_names: Vec<String> = target_binaries
        .iter()
        .map(|b| {
            b.path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("binary")
                .to_string()
        })
        .collect();
    let binary_name_refs: Vec<&str> = all_binary_names.iter().map(|s| s.as_str()).collect();

    // Generate and write snap.yaml to prime/meta/snap.yaml via the shared
    // render path the offline schema validator also calls, so the staged
    // metadata is byte-identical to what validation checks.
    let project_name = &ctx.config.project_name;
    let yaml_content = render_snap_yaml(
        ctx,
        snap_cfg,
        krate_name,
        version,
        &binary_name_refs,
        target,
        Some(project_name.as_str()),
    )?;
    let yaml_path = meta_dir.join("snap.yaml");
    fs::write(&yaml_path, &yaml_content)
        .with_context(|| format!("write snap.yaml to {}", yaml_path.display()))?;

    // Copy icon into meta/gui/ so snapcraft picks it up via the GUI
    // metadata channel without touching snap.yaml. The Snap Store
    // rejects snap.json with an `icon:` key, so the field is
    // intentionally omitted from snap.yaml (see generate_snap_yaml).
    if let Some(ref icon_src_str) = snap_cfg.icon {
        let icon_src = resolve_icon_path(icon_src_str, ctx.options.project_root.as_ref());
        let dest_rel = copy_snap_icon(&icon_src, &meta_dir, snap_name)?;
        log.status(&format!("wrote snap icon to {}", dest_rel));
    }

    copy_binaries_into_prime(target_binaries, &prime_dir)?;
    copy_extra_files(snap_cfg, &prime_dir)?;
    copy_completer_files(ctx, snap_cfg, &prime_dir)?;

    if let Some(ref tpl_specs) = snap_cfg.templated_extra_files
        && !tpl_specs.is_empty()
    {
        anodizer_core::templated_files::process_templated_extra_files(
            tpl_specs,
            ctx,
            &prime_dir,
            "snapcraft",
        )?;
    }

    if let Some(ts) = &snap_cfg.mod_timestamp {
        anodizer_core::util::apply_mod_timestamp(&prime_dir, ts, log)?;
    }

    Ok((tmp_dir, prime_dir))
}

/// Copy binaries into the prime dir root with mode 0555.
fn copy_binaries_into_prime(target_binaries: &[&Artifact], prime_dir: &Path) -> Result<()> {
    for bin_artifact in target_binaries {
        let bin_name = bin_artifact
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("binary");
        let binary_dest = prime_dir.join(bin_name);
        let bin_path_str = bin_artifact.path.to_string_lossy();
        fs::copy(&bin_artifact.path, &binary_dest).with_context(|| {
            format!("copy binary {} to {}", bin_path_str, binary_dest.display())
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o555);
            std::fs::set_permissions(&binary_dest, perms)
                .with_context(|| format!("set binary mode 0555 on {}", binary_dest.display()))?;
        }
    }
    Ok(())
}

/// Copy each entry of `extra_files` into the prime dir at its
/// destination path, applying the configured file mode (default 0644).
fn copy_extra_files(snap_cfg: &SnapcraftConfig, prime_dir: &Path) -> Result<()> {
    let Some(extra_files) = &snap_cfg.extra_files else {
        return Ok(());
    };
    for extra in extra_files {
        let src = PathBuf::from(extra.source());
        let dest_rel = extra.destination().unwrap_or_else(|| extra.source());
        let dest = prime_dir.join(dest_rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create dir for extra file: {}", parent.display()))?;
        }
        fs::copy(&src, &dest)
            .with_context(|| format!("copy extra file {} to {}", src.display(), dest.display()))?;
        let mode = extra.mode().unwrap_or(0o644);
        if mode > 0o7777 {
            anyhow::bail!(
                "snapcraft: invalid file mode {:o} for '{}' — \
                 must be in range 0-7777 (octal)",
                mode,
                src.display()
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(mode);
            std::fs::set_permissions(&dest, perms)
                .with_context(|| format!("set mode {:o} on {}", mode, dest.display()))?;
        }
    }
    Ok(())
}

/// Copy per-app completer scripts into the prime dir. The `completer:`
/// path is used twice (source AND destination) — an absolute value
/// collapses the two because `Path::join(absolute)` discards the prefix
/// on every platform, so reject absolute paths at the contract boundary.
fn copy_completer_files(ctx: &Context, snap_cfg: &SnapcraftConfig, prime_dir: &Path) -> Result<()> {
    let Some(ref apps_map) = snap_cfg.apps else {
        return Ok(());
    };
    for (app_name, app_cfg) in apps_map.iter() {
        let Some(ref completer_path) = app_cfg.completer else {
            continue;
        };
        if Path::new(completer_path).is_absolute() {
            anyhow::bail!(
                "snapcraft: app '{}' completer path '{}' must be \
                 relative to the project root (the same path is also \
                 used as the destination inside the snap's prime dir; \
                 absolute paths collapse source and destination)",
                app_name,
                completer_path,
            );
        }
        let src = ctx
            .options
            .project_root
            .as_deref()
            .unwrap_or(Path::new("."))
            .join(completer_path);
        let dest = prime_dir.join(completer_path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("snapcraft: create dir for completer {}", parent.display())
            })?;
        }
        if src.exists() {
            fs::copy(&src, &dest).with_context(|| {
                format!(
                    "snapcraft: copy completer {} to {}",
                    src.display(),
                    dest.display()
                )
            })?;
        }
    }
    Ok(())
}
