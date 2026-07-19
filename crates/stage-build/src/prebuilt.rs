//! `builder: prebuilt` planning, split from `run.rs`.

use anodizer_core::artifact::ArtifactKind;
use anodizer_core::build_env::{
    clear_build_target_vars, prebuilt_amd64_variant, seed_build_target_vars,
};
use anodizer_core::config::{BuildConfig, BuildIgnore};
use anodizer_core::context::Context;
use anodizer_core::target::map_target;
use anyhow::{Context as _, Result};

use super::command::crate_has_binary_target;
use super::targets::is_target_ignored;
use crate::run::PlanInputs;

/// Diagnostic reason a crate gets no default `--bin <crate>` build: a pure
/// library (no binary targets at all) versus a library that carries only
/// helper binaries whose names don't match the crate (so cargo would reject
/// `--bin <crate>`). Surfaced in the skip line so a consumer can tell the two
/// apart at a glance.
pub(crate) fn no_default_binary_reason(crate_path: &str, crate_name: &str) -> String {
    if crate_has_binary_target(crate_path) {
        format!("no binary target named '{crate_name}' (only differently-named helper binaries)")
    } else {
        format!("no binary target named '{crate_name}' (library crate)")
    }
}

/// Plan a single `builder: prebuilt` build by rendering its
/// `prebuilt.path` template per target, stat()-ing the rendered path,
/// and registering an `ArtifactKind::Binary` directly in `ctx.artifacts`.
///
/// No `BuildJob` is emitted — the cargo runner has nothing to do for an
/// imported binary. Hooks (`pre`/`post`), `skip:`, target filters
/// (`--single-target`, `--split`, `ignore`), and the per-target
/// template-var lifecycle (Os, Arch, Target, Amd64, ArtifactExt,
/// ArtifactID) are all honoured the same way as the cargo path so
/// downstream stages see a uniform artifact shape regardless of which
/// builder produced the bytes.
///
/// Cargo-only knobs (`features`, `no_default_features`, `command`,
/// `cross_tool`, `flags`, `reproducible`) are rejected at config-load
/// time by [`anodizer_core::config::validate_builds`]; the planner can
/// therefore assume the build entry is well-formed by the time it gets
/// here. `targets:` is also required-explicit by that validator.
pub(crate) fn plan_prebuilt_build(
    ctx: &mut Context,
    log: &anodizer_core::log::StageLogger,
    crate_cfg: &anodizer_core::config::CrateConfig,
    build: &BuildConfig,
    inputs: &PlanInputs<'_>,
) -> Result<()> {
    let binary_field: String = build
        .binary
        .clone()
        .unwrap_or_else(|| crate_cfg.name.clone());

    let should_skip = match build.skip.as_ref() {
        Some(s) => s
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .with_context(|| {
                format!(
                    "build: render skip template for prebuilt build '{}'",
                    build.id.as_deref().unwrap_or(&binary_field)
                )
            })?,
        None => false,
    };
    if should_skip {
        log.status(&format!(
            "skipped prebuilt build '{}' — skip: true",
            build.id.as_deref().unwrap_or(&binary_field)
        ));
        return Ok(());
    }

    let prebuilt = build.prebuilt.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "internal: prebuilt build '{}' reached the planner without a `prebuilt:` block \
             (validate_builds should have rejected this at config-load)",
            build.id.as_deref().unwrap_or(&binary_field)
        )
    })?;
    let path_template = prebuilt.path.clone();

    // `targets:` is required-explicit for prebuilt builds (enforced by
    // validate_builds). Honour `--single-target` / `--split` the same
    // way the cargo path does so operators can shard prebuilt imports.
    let mut targets: Vec<String> = build.targets.clone().unwrap_or_default();
    if let Some(ref single) = ctx.options.single_target {
        let original = targets.clone();
        targets.retain(|t| t == single);
        if targets.is_empty()
            && let Some(matched) = anodizer_core::partial::find_runtime_target(single, &original)
        {
            log.verbose(&format!(
                "host '{}' matched configured prebuilt target '{}' via alias table (--single-target)",
                single, matched
            ));
            targets.push(matched);
        }
        if targets.is_empty() {
            anyhow::bail!(
                "--single-target: host triple '{}' is not in configured prebuilt targets for {}/{} \
                 (configured: [{}]).",
                single,
                crate_cfg.name,
                binary_field,
                original.join(", ")
            );
        }
    }
    if let Some(ref partial) = ctx.options.partial_target {
        targets = partial.filter_targets(&targets);
        if targets.is_empty() {
            log.verbose(&format!(
                "skipped {}/{} — no prebuilt targets match partial filter",
                crate_cfg.name, binary_field
            ));
            return Ok(());
        }
    }

    let build_ignores: Vec<BuildIgnore> = build
        .ignore
        .clone()
        .unwrap_or_else(|| inputs.default_ignores.to_vec());

    for target in &targets {
        if is_target_ignored(target, &build_ignores) {
            log.verbose(&format!(
                "ignoring prebuilt target {} (matched ignore rule)",
                target
            ));
            continue;
        }

        let (os, _arch) = map_target(target);

        seed_build_target_vars(
            ctx.template_vars_mut(),
            target,
            &os,
            build.id.as_deref().unwrap_or(""),
        );
        let binary_name = ctx.render_template(&binary_field).unwrap_or_else(|e| {
            log.warn(&format!(
                "failed to render binary template '{}': {}, using raw value",
                binary_field, e
            ));
            binary_field.clone()
        });

        let rendered_path = ctx.render_template(&path_template).with_context(|| {
            format!(
                "build: render prebuilt.path template '{}' for target {}",
                path_template, target
            )
        })?;

        clear_build_target_vars(ctx.template_vars_mut());

        let staged_path = std::path::PathBuf::from(&rendered_path);
        let dry_run = ctx.options.dry_run;
        if !dry_run {
            std::fs::metadata(&staged_path).with_context(|| {
                format!(
                    "prebuilt: failed to stat imported binary at '{}' (rendered from \
                     `prebuilt.path: {}`) for target '{}'. Stage the binary before running \
                     `anodize build`, or check the path template renders to a real file.",
                    rendered_path, path_template, target
                )
            })?;
        }

        let amd64_variant = prebuilt_amd64_variant(build, target);

        let dist_dir = ctx.config.dist.clone();
        crate::run_helpers::add_artifact(
            ctx,
            &dist_dir,
            dry_run,
            &staged_path,
            ArtifactKind::Binary,
            target,
            &crate_cfg.name,
            &binary_name,
            &build.id,
            false,
            &amd64_variant,
        )?;

        if dry_run {
            log.status(&format!(
                "(dry-run) would import prebuilt {}/{} ({}) from {}",
                crate_cfg.name,
                binary_name,
                target,
                staged_path.display()
            ));
        } else {
            log.status(&format!(
                "imported prebuilt {}/{} ({}) from {}",
                crate_cfg.name,
                binary_name,
                target,
                staged_path.display()
            ));
        }
    }

    Ok(())
}
