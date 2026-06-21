//! Per-crate MSI build orchestration: binary filtering, `.wxs` validation,
//! the WiX compile/link invocation, dry-run logging, artifact creation, and
//! the `before:` / `after:` hook execution.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, Result};

use anodizer_core::arch_path_guard::ArchPathGuard;
use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::context::Context;
use anodizer_core::util::{parse_mod_timestamp, set_file_mtime};

use super::template::{
    build_post_hook_template_vars, compute_msi_filename, default_msi_name_template,
    render_wxs_template, set_msi_template_vars,
};
use super::wix::{
    WixVersion, map_arch_to_msi, msi_command, render_msi_extensions, resolve_wix_version,
};

/// One MSI build selection: `(target, amd64_variant, binary_path)`. The
/// `amd64_variant` is the binary's build-metadata tag (e.g. `v3`) that
/// disambiguates two amd64 builds of one target in the rendered name.
type MsiBinarySelection = (Option<String>, Option<String>, String);

/// Build an MSI `Artifact` and collect archive paths to remove when `replace` is set.
#[allow(clippy::too_many_arguments)]
fn make_msi_artifact(
    msi_path: PathBuf,
    target: &Option<String>,
    amd64_variant: Option<&str>,
    crate_name: &str,
    wix_version: WixVersion,
    product_code: &str,
    msi_cfg: &anodizer_core::config::MsiConfig,
    ctx: &Context,
    archives_to_remove: &mut Vec<PathBuf>,
) -> Artifact {
    let mut metadata = HashMap::from([
        ("format".to_string(), "msi".to_string()),
        ("product_code".to_string(), product_code.to_string()),
        (
            "wix_version".to_string(),
            match wix_version {
                WixVersion::V3 => "v3",
                WixVersion::V4 => "v4",
                WixVersion::Wixl => "wixl",
            }
            .to_string(),
        ),
    ]);
    if let Some(id) = &msi_cfg.id {
        metadata.insert("id".to_string(), id.clone());
    }
    if let Some(v) = amd64_variant {
        metadata.insert("amd64_variant".to_string(), v.to_string());
    }

    // Handle replace option — collect matching archives for removal
    archives_to_remove.extend(anodizer_core::util::collect_if_replace(
        msi_cfg.replace,
        &ctx.artifacts,
        crate_name,
        target.as_deref(),
    ));

    Artifact {
        kind: ArtifactKind::Installer,
        name: String::new(),
        path: msi_path,
        target: target.clone(),
        crate_name: crate_name.to_string(),
        metadata,
        size: None,
    }
}

// ---------------------------------------------------------------------------
// MsiStage

#[allow(clippy::too_many_arguments)]
pub(super) fn process_msi_crate(
    ctx: &mut Context,
    log: &anodizer_core::log::StageLogger,
    krate: &anodizer_core::config::CrateConfig,
    dist: &std::path::Path,
    dry_run: bool,
    new_artifacts: &mut Vec<Artifact>,
    archives_to_remove: &mut Vec<PathBuf>,
) -> Result<()> {
    let Some(msi_configs) = krate.msis.as_ref() else {
        return Ok(());
    };

    let windows_binaries: Vec<_> = ctx
        .artifacts
        .by_kind_and_crate(ArtifactKind::Binary, &krate.name)
        .into_iter()
        .filter(|b| {
            b.target
                .as_deref()
                .map(anodizer_core::target::is_windows)
                .unwrap_or(false)
        })
        .cloned()
        .collect();

    // One guard spans every config of this crate so two configs that render the
    // same dist/windows/<name>.msi (same target, default/identical name) collide
    // loudly instead of the second silently clobbering the first; it resets per
    // crate (this function is called once per crate), so distinct crates are
    // unaffected.
    let mut arch_guard = ArchPathGuard::new();

    let default_name = default_msi_name_template();

    for msi_cfg in msi_configs {
        let msi_id_for_log = msi_cfg.id.as_deref().unwrap_or("default").to_string();

        if should_skip_msi_config(ctx, msi_cfg, &msi_id_for_log, &krate.name, dry_run, log)? {
            continue;
        }

        let Some(effective_binaries) = filter_msi_binaries(
            msi_cfg,
            &windows_binaries,
            &krate.name,
            log,
            ctx.options.show_skipped,
        ) else {
            continue;
        };

        let wxs_path_raw = msi_cfg.wxs.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "msi: `wxs` field is required but missing for crate {}",
                krate.name
            )
        })?;
        // Render the wxs path itself through the template engine so that
        // paths like `./windows/{{ Os }}/app.wxs` resolve correctly.
        let wxs_path_rendered = ctx
            .render_template(wxs_path_raw)
            .with_context(|| format!("msi: render wxs path template for crate {}", krate.name))?;

        for (target, amd64_variant, binary_path) in &effective_binaries {
            let msi_path = build_msi_target(
                ctx,
                log,
                msi_cfg,
                &krate.name,
                target,
                amd64_variant.as_deref(),
                binary_path,
                &wxs_path_rendered,
                dist,
                dry_run,
                new_artifacts,
                archives_to_remove,
                &mut arch_guard,
                &default_name,
            )?;

            // Post-hook runs per-target so it has access to the per-artifact
            // path. The pre-hook runs once per config (before binary filtering)
            // and does not receive artifact vars — no artifact exists yet.
            run_msi_post_hook(
                ctx,
                msi_cfg.hooks.as_ref().and_then(|h| h.post.as_ref()),
                &msi_path,
                &msi_id_for_log,
                &krate.name,
                dry_run,
                log,
            )?;
        }
    }

    Ok(())
}

/// Build (or dry-run) one MSI target: set template vars, compute filename,
/// render WXS, and execute the WiX toolchain.
///
/// Returns the absolute path to the produced (or planned) `.msi` so the
/// caller can forward it to the per-target post-hook.
#[allow(clippy::too_many_arguments)]
fn build_msi_target(
    ctx: &mut Context,
    log: &anodizer_core::log::StageLogger,
    msi_cfg: &anodizer_core::config::MsiConfig,
    crate_name: &str,
    target: &Option<String>,
    amd64_variant: Option<&str>,
    binary_path: &str,
    wxs_path: &str,
    dist: &std::path::Path,
    dry_run: bool,
    new_artifacts: &mut Vec<Artifact>,
    archives_to_remove: &mut Vec<PathBuf>,
    arch_guard: &mut ArchPathGuard,
    default_name: &str,
) -> Result<PathBuf> {
    let (_os, arch) = target
        .as_deref()
        .map(anodizer_core::target::map_target)
        .unwrap_or_else(|| ("windows".to_string(), "amd64".to_string()));
    let msi_arch = map_arch_to_msi(&arch).to_string();

    // Derive the deterministic ProductCode from the same ProjectName the .wxs
    // will render (the template var is rebound to the crate name in workspace
    // per-crate mode), the release version, and the WiX arch. Stable per
    // version+arch, rotating across versions — see `product_code`.
    let project_name = ctx
        .template_vars()
        .get("ProjectName")
        .cloned()
        .unwrap_or_else(|| ctx.config.project_name.clone());
    let product_code =
        super::product_code::derive_product_code(&project_name, &ctx.version(), &msi_arch);

    set_msi_template_vars(
        ctx,
        target.as_deref(),
        &arch,
        &msi_arch,
        binary_path,
        &product_code,
    );
    // Seed the amd64 variant so the default (or a custom) name template
    // disambiguates two amd64 builds of one target.
    anodizer_core::archive_name::seed_amd64_variant_var(ctx.template_vars_mut(), amd64_variant);

    let wix_version = resolve_wix_version(msi_cfg, wxs_path, log);

    let output_dir = dist.join("windows");
    let msi_filename = compute_msi_filename(ctx, msi_cfg, crate_name, target.as_deref())?;
    let msi_path = output_dir.join(&msi_filename);

    arch_guard.check(
        &msi_path,
        "msis",
        "installer",
        msi_cfg.name.as_deref().unwrap_or(default_name),
        &msi_filename,
        crate_name,
    )?;

    let rendered_extensions = render_msi_extensions(ctx, msi_cfg, log);

    // Render mod_timestamp once here so both the wxs mtime and the WiX
    // BindTimestamp flag receive the same evaluated value.
    let rendered_mod_timestamp: Option<String> = msi_cfg
        .mod_timestamp
        .as_deref()
        .map(|tmpl| {
            ctx.render_template(tmpl)
                .with_context(|| "msi: render mod_timestamp template")
        })
        .transpose()?;

    if dry_run {
        log_msi_dry_run(
            log,
            &msi_filename,
            wix_version,
            crate_name,
            target.as_deref(),
            msi_cfg,
            rendered_mod_timestamp.as_deref(),
            &rendered_extensions,
        );
        new_artifacts.push(make_msi_artifact(
            msi_path.clone(),
            target,
            amd64_variant,
            crate_name,
            wix_version,
            &product_code,
            msi_cfg,
            ctx,
            archives_to_remove,
        ));
        return Ok(msi_path);
    }

    fs::create_dir_all(&output_dir)
        .with_context(|| format!("msi: create output dir: {}", output_dir.display()))?;

    let (tmp_dir, rendered_wxs_path) = prepare_wxs_build_context(
        ctx,
        msi_cfg,
        wxs_path,
        rendered_mod_timestamp.as_deref(),
        log,
    )?;

    execute_msi_build(
        wix_version,
        rendered_mod_timestamp.as_deref(),
        &rendered_wxs_path,
        &msi_path,
        &rendered_extensions,
        crate_name,
        target.as_deref(),
        log,
    )?;
    drop(tmp_dir);

    new_artifacts.push(make_msi_artifact(
        msi_path.clone(),
        target,
        amd64_variant,
        crate_name,
        wix_version,
        &product_code,
        msi_cfg,
        ctx,
        archives_to_remove,
    ));

    Ok(msi_path)
}

// ---------------------------------------------------------------------------
// Private helpers — sliced out of `MsiStage::run` to keep the body short.
// ---------------------------------------------------------------------------

/// Evaluate per-config skip predicates (`if`, `skip`) and run the
/// `hooks.before` / `pre` lifecycle hooks. Returns `Ok(true)` when the
/// caller should `continue` (skip this config).
fn should_skip_msi_config(
    ctx: &mut Context,
    msi_cfg: &anodizer_core::config::MsiConfig,
    msi_id_for_log: &str,
    crate_name: &str,
    dry_run: bool,
    log: &anodizer_core::log::StageLogger,
) -> Result<bool> {
    let proceed = anodizer_core::config::evaluate_if_condition(
        msi_cfg.if_condition.as_deref(),
        &format!("msi config '{msi_id_for_log}' for crate '{crate_name}'"),
        |t| ctx.render_template(t),
    )?;
    if !proceed {
        log.status(&format!(
            "skipped msi config '{msi_id_for_log}' for crate {crate_name} — `if` condition evaluated falsy"
        ));
        return Ok(true);
    }

    if let Some(ref d) = msi_cfg.skip {
        let off = d
            .try_evaluates_to_true(|s| ctx.render_template(s))
            .with_context(|| format!("msi: render skip template for crate {}", crate_name))?;
        if off {
            log.status(&format!("MSI config skipped for crate {}", crate_name));
            return Ok(true);
        }
    }

    run_msi_hook(
        ctx,
        msi_cfg.hooks.as_ref().and_then(|h| h.pre.as_ref()),
        "pre-msi",
        msi_id_for_log,
        crate_name,
        dry_run,
        log,
    )?;

    Ok(false)
}

/// Apply the ids + amd64_variant filters to the collected Windows binaries.
/// Returns `Some` with `(target, amd64_variant, binary_path)` tuples to drive
/// the per-target build, or `None` when the caller should `continue` (no
/// matching binaries). The `amd64_variant` is the binary's build-metadata tag
/// (e.g. `v3`), seeded into the `name` template so two amd64 variants of one
/// target render distinct installer names.
fn filter_msi_binaries(
    msi_cfg: &anodizer_core::config::MsiConfig,
    windows_binaries: &[Artifact],
    crate_name: &str,
    log: &anodizer_core::log::StageLogger,
    show_skipped: bool,
) -> Option<Vec<MsiBinarySelection>> {
    let mut filtered: Vec<&Artifact> = windows_binaries.iter().collect();

    if let Some(ref filter_ids) = msi_cfg.ids
        && !filter_ids.is_empty()
    {
        filtered.retain(|b| {
            b.metadata
                .get("id")
                .map(|id| filter_ids.contains(id))
                .unwrap_or(false)
                || b.metadata
                    .get("name")
                    .map(|n| filter_ids.contains(n))
                    .unwrap_or(false)
        });
    }

    if let Some(ref want) = msi_cfg.amd64_variant {
        filtered.retain(|b| {
            let target = b.target.as_deref().unwrap_or("");
            let (_, arch) = anodizer_core::target::map_target(target);
            if arch != "amd64" {
                return true;
            }
            b.metadata
                .get("amd64_variant")
                .map(String::as_str)
                .unwrap_or("v1")
                == want
        });
    }

    if filtered.is_empty() && windows_binaries.is_empty() {
        log.skip_line(
            show_skipped,
            &format!(
                "skipped MSI generation for crate '{}' — no Windows binary \
             artifacts found (expected binaries targeting windows/msvc)",
                crate_name
            ),
        );
        return None;
    }
    if filtered.is_empty() {
        log.warn(&format!(
            "skipped msi for crate '{}' — ids filter {:?} matched no binaries",
            crate_name, msi_cfg.ids
        ));
        return None;
    }

    Some(
        filtered
            .into_iter()
            .map(|b| {
                (
                    b.target.clone(),
                    b.metadata.get("amd64_variant").cloned(),
                    b.path.to_string_lossy().into_owned(),
                )
            })
            .collect(),
    )
}

/// Emit the dry-run logging for a planned MSI build: the headline build
/// line, any `mod_timestamp:`, `extra_files:`, and `extensions:` entries
/// that would be applied.
///
/// `rendered_mod_timestamp` must already be template-rendered by the caller
/// so the logged value shows the resolved timestamp, not the raw template.
#[allow(clippy::too_many_arguments)]
fn log_msi_dry_run(
    log: &anodizer_core::log::StageLogger,
    msi_filename: &str,
    wix_version: WixVersion,
    crate_name: &str,
    target: Option<&str>,
    msi_cfg: &anodizer_core::config::MsiConfig,
    rendered_mod_timestamp: Option<&str>,
    rendered_extensions: &[String],
) {
    log.status(&format!(
        "(dry-run) would build MSI {} (WiX {:?}) for crate {} target {:?}",
        msi_filename, wix_version, crate_name, target
    ));
    if let Some(ts) = rendered_mod_timestamp {
        log.status(&format!("(dry-run) would apply mod_timestamp={ts}"));
    }
    if let Some(ref extras) = msi_cfg.extra_files {
        for f in extras {
            log.status(&format!(
                "(dry-run) would copy extra file '{f}' to build context"
            ));
        }
    }
    for ext in rendered_extensions {
        log.status(&format!("(dry-run) would add WiX extension -ext {ext}"));
    }
}

/// Render the `.wxs` template, write it into a fresh tempdir, copy any
/// configured `extra_files:` next to it, and apply the rendered file's
/// `mod_timestamp:` mtime. Returns the tempdir handle (which must outlive
/// the build) and the path to the rendered `.wxs`.
///
/// `mod_timestamp` must already be template-rendered by the caller.
fn prepare_wxs_build_context(
    ctx: &Context,
    msi_cfg: &anodizer_core::config::MsiConfig,
    wxs_path: &str,
    mod_timestamp: Option<&str>,
    log: &anodizer_core::log::StageLogger,
) -> Result<(tempfile::TempDir, PathBuf)> {
    let rendered_wxs = render_wxs_template(ctx, wxs_path)?;

    let tmp_dir = tempfile::tempdir().context("msi: create temp dir for .wxs")?;
    let rendered_wxs_path = tmp_dir.path().join("rendered.wxs");
    fs::write(&rendered_wxs_path, &rendered_wxs).with_context(|| {
        format!(
            "msi: write rendered .wxs to {}",
            rendered_wxs_path.display()
        )
    })?;

    if let Some(ref extras) = msi_cfg.extra_files {
        for filename in extras {
            let src = PathBuf::from(filename);
            if !src.exists() {
                anyhow::bail!("msi: extra_file '{}' does not exist", filename);
            }
            let dest_name = src
                .file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new(filename));
            let dest = tmp_dir.path().join(dest_name);
            fs::copy(&src, &dest).with_context(|| {
                format!(
                    "msi: copy extra file '{}' to build context '{}'",
                    filename,
                    dest.display()
                )
            })?;
            log.status(&format!(
                "copied extra file '{}' to build context",
                filename
            ));
        }
    }

    if let Some(ts) = mod_timestamp {
        log.status(&format!("applying mod_timestamp={ts} to rendered .wxs"));
        let mtime = parse_mod_timestamp(ts)?;
        set_file_mtime(&rendered_wxs_path, mtime)?;
    }

    Ok((tmp_dir, rendered_wxs_path))
}

/// Compose and execute the WiX build commands (primary + optional link
/// step for v3), then apply `mod_timestamp:` to the resulting `.msi`. The
/// `-d BindTimestamp=<ts>` flag is appended for v4 builds; v3 logs the
/// limitation but otherwise mtime-stamps the same way.
///
/// `mod_timestamp` must already be template-rendered by the caller.
#[allow(clippy::too_many_arguments)]
fn execute_msi_build(
    wix_version: WixVersion,
    mod_timestamp: Option<&str>,
    rendered_wxs_path: &std::path::Path,
    msi_path: &std::path::Path,
    rendered_extensions: &[String],
    crate_name: &str,
    target: Option<&str>,
    log: &anodizer_core::log::StageLogger,
) -> Result<()> {
    if wix_version == WixVersion::Wixl && !rendered_extensions.is_empty() {
        log.warn(&format!(
            "wixl (Linux MSI path) does not support WiX `-ext` extensions; ignoring: {}",
            rendered_extensions.join(", ")
        ));
    }

    let mut commands = msi_command(
        wix_version,
        &rendered_wxs_path.to_string_lossy(),
        &msi_path.to_string_lossy(),
        rendered_extensions,
    );

    if let Some(ts) = mod_timestamp {
        match wix_version {
            WixVersion::V4 => {
                commands.primary.push("-d".to_string());
                commands.primary.push(format!("BindTimestamp={ts}"));
            }
            WixVersion::V3 => {
                log.status(&format!(
                    "mod_timestamp={ts} noted; WiX v3 has limited \
                     timestamp support (applied to .wxs and output .msi)"
                ));
            }
            WixVersion::Wixl => {
                log.status(&format!(
                    "mod_timestamp={ts} noted; wixl has limited \
                     timestamp support (applied to .wxs and output .msi)"
                ));
            }
        }
    }

    log.verbose(&format!("running {}", commands.primary.join(" ")));
    let output = Command::new(&commands.primary[0])
        .args(&commands.primary[1..])
        .output()
        .with_context(|| {
            format!(
                "msi: execute {} for crate {} target {:?}",
                commands.primary[0], crate_name, target
            )
        })?;
    log.check_output(output, &commands.primary[0])?;

    if let Some(link_cmd) = &commands.link {
        log.verbose(&format!("running {}", link_cmd.join(" ")));
        let output = Command::new(&link_cmd[0])
            .args(&link_cmd[1..])
            .output()
            .with_context(|| {
                format!(
                    "msi: execute {} for crate {} target {:?}",
                    link_cmd[0], crate_name, target
                )
            })?;
        log.check_output(output, &link_cmd[0])?;
    }

    if let Some(ts) = mod_timestamp
        && msi_path.exists()
    {
        let mtime = parse_mod_timestamp(ts)?;
        set_file_mtime(msi_path, mtime)?;
        log.status(&format!(
            "applied mod_timestamp={ts} to {}",
            msi_path.display()
        ));
    }

    log.status(&format!(
        "built MSI {}",
        msi_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| msi_path.to_string_lossy().into_owned())
    ));

    Ok(())
}

/// Run the pre-MSI hook chain with the current template-var snapshot.
///
/// Pre-hooks do not receive artifact path variables — no `.msi` exists yet.
/// A failing hook aborts the entire MSI stage for the crate (matching
/// `before:` semantics in adjacent stages).
fn run_msi_hook(
    ctx: &Context,
    hook: Option<&Vec<anodizer_core::config::HookEntry>>,
    kind: &'static str,
    msi_id_for_log: &str,
    crate_name: &str,
    dry_run: bool,
    log: &anodizer_core::log::StageLogger,
) -> Result<()> {
    let Some(hook) = hook else {
        return Ok(());
    };
    let tmpl_vars = ctx.template_vars().clone();
    anodizer_core::hooks::run_hooks(
        hook,
        kind,
        anodizer_core::hooks::HookRunContext::new(dry_run, log, Some(&tmpl_vars)),
    )
    .with_context(|| {
        format!(
            "msi config '{}' for crate '{}': {} hooks failed",
            msi_id_for_log, crate_name, kind
        )
    })
}

/// Run the post-MSI hook chain for one target with artifact path variables
/// injected into a cloned template-var snapshot.
///
/// Post-hooks receive `ArtifactPath` (absolute path to the `.msi`),
/// `ArtifactName` (filename only), and `ArtifactExt` (`.msi`). These are
/// injected into a clone of the current vars so global state is not mutated.
/// A failing hook aborts the stage.
pub(super) fn run_msi_post_hook(
    ctx: &Context,
    hook: Option<&Vec<anodizer_core::config::HookEntry>>,
    msi_path: &std::path::Path,
    msi_id_for_log: &str,
    crate_name: &str,
    dry_run: bool,
    log: &anodizer_core::log::StageLogger,
) -> Result<()> {
    let Some(hook) = hook else {
        return Ok(());
    };
    let tmpl_vars = build_post_hook_template_vars(ctx, msi_path);
    anodizer_core::hooks::run_hooks(
        hook,
        "post-msi",
        anodizer_core::hooks::HookRunContext::new(dry_run, log, Some(&tmpl_vars)),
    )
    .with_context(|| {
        format!(
            "msi config '{}' for crate '{}': post-msi hooks failed",
            msi_id_for_log, crate_name
        )
    })
}
