use anodizer_core::config::SnapcraftConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};

use crate::build_stage::resolve_icon_path;
use crate::command::first_channel_rejected_for_prerelease_snap;
use crate::yaml::DEFAULT_SNAP_NAME_TEMPLATE;

/// Evaluate a snap config's `skip:` / `if:` gates against the render context.
///
/// Returns `Ok(true)` when the config is suppressed — `skip:` rendered truthy
/// or the `if:` condition rendered falsy — so the caller skips it. Read-only
/// (`&Context`), so both the build's `validate_and_check_skip` and the offline
/// `snapcraft_snap_yamls_for_crate` renderer share one gate and never diverge
/// on which configs a run suppresses.
pub(crate) fn snap_cfg_skipped(
    ctx: &Context,
    log: &StageLogger,
    snap_cfg: &SnapcraftConfig,
    krate_name: &str,
) -> Result<bool> {
    if let Some(ref d) = snap_cfg.skip {
        let off = d
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .with_context(|| format!("snapcraft: render skip template for crate {}", krate_name))?;
        if off {
            log.status(&format!(
                "skipped snapcraft config for crate {} — skip=true",
                krate_name
            ));
            return Ok(true);
        }
    }
    let proceed = anodizer_core::config::evaluate_if_condition(
        snap_cfg.if_condition.as_deref(),
        &format!("snapcraft config for crate '{krate_name}'"),
        |t| ctx.render_template(t),
    )?;
    if !proceed {
        log.status(&format!(
            "skipped snapcraft config for crate {krate_name} — `if` condition evaluated falsy"
        ));
        return Ok(true);
    }
    Ok(false)
}

/// Validate per-config fields and honour `skip:`. Returns `Ok(true)` when
/// the caller should `continue` to the next snap config (skip evaluated
/// true). Bails on invalid confinement / grade / icon settings.
pub(crate) fn validate_and_check_skip(
    ctx: &mut Context,
    log: &StageLogger,
    snap_cfg: &SnapcraftConfig,
    krate_name: &str,
) -> Result<bool> {
    if snap_cfg_skipped(ctx, log, snap_cfg, krate_name)? {
        return Ok(true);
    }

    if let Some(conf) = &snap_cfg.confinement {
        match conf.as_str() {
            "strict" | "devmode" | "classic" => {}
            other => anyhow::bail!(
                "snapcraft: invalid confinement '{}' for crate '{}'. \
                 Valid values are: strict, devmode, classic",
                other,
                krate_name
            ),
        }
    }

    if let Some(grade) = &snap_cfg.grade {
        match grade.as_str() {
            "stable" | "devel" => {}
            other => anyhow::bail!(
                "snapcraft: invalid grade '{}' for crate '{}'. \
                 Valid values are: stable, devel",
                other,
                krate_name
            ),
        }
    }

    // Confinement/grade vs. channel cross-check: the Snap Store rejects a
    // devmode-confined or devel-grade snap ("not ready for general use")
    // pushed to candidate/stable. Catch this at preflight, before any
    // build/upload work, rather than surfacing it as an upload-time Store
    // rejection. An unset `channel_templates` auto-populates to edge/beta
    // for these snaps (see `resolve_effective_channels`) and never reaches
    // this branch — only an explicit, conflicting channel is bailed on.
    //
    // This check runs against the RAW, unrendered `channel_templates` and
    // `grade` strings, and only when the build stage itself executes — a
    // template that resolves to a restricted channel only after rendering,
    // or a `--publish-only` run (which skips the build stage entirely),
    // never reaches it. `run_uploads` in `publish_stage.rs` re-runs the same
    // classifier against the RENDERED values immediately before every
    // upload, which is the only check both paths always hit.
    let confinement_is_devmode = snap_cfg.confinement.as_deref() == Some("devmode");
    let grade_is_devel = snap_cfg.grade.as_deref() == Some("devel");
    if confinement_is_devmode || grade_is_devel {
        if let Some(channels) = snap_cfg.channel_templates.as_deref() {
            if let Some(rejected) = first_channel_rejected_for_prerelease_snap(channels) {
                let reason = match (confinement_is_devmode, grade_is_devel) {
                    (true, true) => "devmode confinement and devel grade",
                    (true, false) => "devmode confinement",
                    (false, true) => "devel grade",
                    (false, false) => unreachable!("guarded by the outer if"),
                };
                anyhow::bail!(
                    "snapcraft: crate '{krate_name}' configures {reason} together \
                     with channel '{rejected}', which the Snap Store rejects — a \
                     snap with {reason} may only be pushed to pre-release channels \
                     (edge, beta). Remove '{rejected}' from channel_templates or \
                     drop the setting that produces {reason}."
                );
            }
        }
    }

    // Icon validation: when `icon` is set, check the source file exists
    // AND its extension is in snapcraft's allowed set (png/svg) before
    // staging binaries. snapcraft pack silently rejects other formats
    // at pack time, after the operator already burned minutes on the run.
    if let Some(ref icon_src_str) = snap_cfg.icon {
        let icon_src = resolve_icon_path(icon_src_str, ctx.options.project_root.as_ref());
        let ext_lower = icon_src
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase());
        match ext_lower.as_deref() {
            Some("png") | Some("svg") => {}
            _ => {
                anyhow::bail!(
                    "snapcraft: icon '{}' configured for crate '{}' has \
                     unsupported extension (resolved to '{}'). Snapcraft \
                     only accepts .png or .svg snap icons; rename or \
                     convert the source file.",
                    icon_src_str,
                    krate_name,
                    icon_src.display()
                );
            }
        }
        if !icon_src.exists() {
            anyhow::bail!(
                "snapcraft: icon '{}' configured for crate '{}' does not exist \
                 (resolved to '{}'). Create the file or correct the path in \
                 the snapcrafts.icon config field.",
                icon_src_str,
                krate_name,
                icon_src.display()
            );
        }
    }

    Ok(false)
}

/// Render `snap_cfg.name_template` (or the default template)
/// with per-target `Os` / `Arch` / `Arm` / `Amd64` / `Mips` / `Target`
/// substitutions. Saves and restores `ProjectName` around the render so
/// subsequent stages observe the same template-var state.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compute_snap_filename(
    ctx: &mut Context,
    snap_cfg: &SnapcraftConfig,
    krate_name: &str,
    snap_name: &str,
    target: Option<&str>,
    os: &str,
    arch: &str,
    amd64_variant: Option<&str>,
) -> Result<String> {
    let saved_project_name = ctx
        .template_vars()
        .get("ProjectName")
        .cloned()
        .unwrap_or_default();
    ctx.template_vars_mut().set("ProjectName", snap_name);
    match target {
        // The archive-name seeding policy verbatim (arm split, variant vars
        // empty) — the snap default IS core's default asset-name template, so
        // the vars it reads must be seeded identically.
        Some(t) => anodizer_core::archive_name::seed_target_vars(ctx, t),
        // Host-target build (no triple): seed the caller-derived Os/Arch and
        // clear ALL variant vars — resetting a subset would leak a previous
        // target's `Arm64`/`I386` into a user name_template. An empty
        // `{{ .Target }}` renders as the empty string in user templates.
        None => {
            ctx.template_vars_mut().set("Os", os);
            ctx.template_vars_mut().set("Arch", arch);
            ctx.template_vars_mut().set("Target", "");
            anodizer_core::archive_name::reset_variant_vars(ctx.template_vars_mut());
        }
    }
    // The amd64 micro-architecture variant comes from the built binary's
    // metadata, not the go-arch. The default template's Amd64 clause
    // suppresses the `v1` baseline so `None`/`"v1"` preserve historical
    // single-variant snap names, while a non-`v1` variant (e.g. `"v3"`)
    // appends the suffix.
    anodizer_core::archive_name::seed_amd64_variant_var(
        ctx.template_vars_mut(),
        arch,
        amd64_variant,
    );
    let tmpl = snap_cfg
        .name_template
        .as_deref()
        .unwrap_or(DEFAULT_SNAP_NAME_TEMPLATE);
    let render_result = ctx.render_template(tmpl).with_context(|| {
        format!(
            "snapcraft: render name_template for crate {} target {:?}",
            krate_name, target
        )
    });
    ctx.template_vars_mut()
        .set("ProjectName", &saved_project_name);
    let rendered = render_result?;
    Ok(if rendered.to_lowercase().ends_with(".snap") {
        rendered
    } else {
        format!("{rendered}.snap")
    })
}
