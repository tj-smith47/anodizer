//! Strict in-memory snapcraft.yaml render guard for the prepublish phase.
//!
//! The snapcraft build is a build stage, not a publisher, so it is absent from
//! [`anodizer_stage_publish::validate_publisher_schemas`]'s always-strict
//! prepublish dry-run. Without this validator a residual unrendered template
//! delimiter in a snapcraft.yaml would only WARN under a normal (non-strict)
//! release and still ship — the exact failure mode the residual-delimiter
//! guard exists to eliminate for every other publisher.
//!
//! [`validate_snapcraft_templates`] renders every selected crate's
//! snapcraft.yaml strictly and in-memory (writing nothing, spawning nothing),
//! so a leak fails the release BEFORE the snapcraft build runs. The
//! prepublish-guard calls it under its strict render pass, alongside the
//! publisher and announce validators.

use anodizer_core::config::CrateConfig;
use anodizer_core::context::Context;
use anodizer_core::crate_scope::with_crate_scope;
use anodizer_core::log::StageLogger;
use anyhow::Result;

use crate::yaml::snapcraft_snap_yamls_for_crate;

/// Per-crate tag resolver, threaded so each crate's snapcraft.yaml renders
/// against its OWN version (workspace per-crate independent-version mode).
/// Mirrors [`anodizer_stage_publish`]'s `TagResolver`.
pub type TagResolver<'a> = &'a dyn Fn(&Context, &CrateConfig) -> Option<String>;

/// Render every selected crate's snapcraft.yaml strictly, in-memory, returning
/// `Err` on the first crate whose manifest carries a residual unrendered
/// template delimiter (or otherwise fails to render).
///
/// The render path ([`snapcraft_snap_yamls_for_crate`]) runs the same
/// residual-delimiter guard the live build uses; under the prepublish-guard's
/// strict render pass that guard returns `Err` instead of warning, so a leak
/// aborts the release before the snapcraft build (let alone publish) fires.
///
/// Crate selection mirrors [`crate::SnapcraftStage`]'s build walk exactly
/// (`selected_crates` filter + a non-empty `snapcrafts:` block), so the
/// validated set equals the built set across single-crate, workspace-lockstep,
/// and workspace per-crate modes. `resolve_tag` scopes each crate's render to
/// its own version.
pub fn validate_snapcraft_templates(
    ctx: &mut Context,
    log: &StageLogger,
    resolve_tag: TagResolver<'_>,
) -> Result<()> {
    let selected = ctx.options.selected_crates.clone();
    let crates: Vec<CrateConfig> = ctx
        .config
        .crate_universe()
        .into_iter()
        .filter(|c| selected.is_empty() || selected.contains(&c.name))
        .filter(|c| c.snapcrafts.is_some())
        .cloned()
        .collect();

    for krate in &crates {
        // Render under THIS crate's own version so per-crate independent-version
        // mode validates each crate's snapcraft.yaml against its own tag.
        with_crate_scope(ctx, krate, resolve_tag, |ctx| {
            let yamls = snapcraft_snap_yamls_for_crate(ctx, &krate.name)?;
            log.verbose(&format!(
                "snapcraft: crate '{}' rendered {} snap.yaml manifest(s) cleanly",
                krate.name,
                yamls.len()
            ));
            Ok(())
        })?;
    }
    Ok(())
}
