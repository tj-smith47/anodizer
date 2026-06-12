//! Snapcraft build + publish stages.
//!
//! - [`SnapcraftStage`]: pre-stages binaries into a prime directory, writes
//!   `snap.yaml`, then runs `snapcraft pack` per platform group.
//! - [`SnapcraftPublishStage`]: uploads built `.snap` artifacts via
//!   `snapcraft upload --release=...` and records its own
//!   `PublisherResult` directly into `ctx.publish_report` (mirrors the
//!   BlobStage pattern; a trait-based `SnapcraftPublisher` would
//!   double-publish through the generic dispatch path).

mod arch;
mod build_stage;
mod command;
mod generate;
mod publish_stage;
mod targets;
mod yaml;

#[cfg(test)]
mod tests;

pub use build_stage::{SnapcraftStage, snapcraft_snap_yamls_for_crate};
pub use command::{
    is_retriable_snap_push, resolve_effective_channels, snapcraft_command, snapcraft_upload_command,
};
pub use generate::generate_snap_yaml;
pub use publish_stage::SnapcraftPublishStage;

/// Environment requirements for the snapcraft build stage: the `snapcraft`
/// CLI (plus `unsquashfs`, which snapcraft's pack path depends on) whenever
/// any crate declares `snapcrafts:`.
pub fn build_env_requirements(
    ctx: &anodizer_core::context::Context,
) -> Vec<anodizer_core::EnvRequirement> {
    let any = anodizer_core::env_preflight::crate_universe(&ctx.config)
        .into_iter()
        .flat_map(|c| c.snapcrafts.iter().flatten())
        .any(|s| !snap_entry_skipped(ctx, s));
    if !any {
        return Vec::new();
    }
    vec![
        anodizer_core::EnvRequirement::Tool {
            name: "snapcraft".to_string(),
        },
        anodizer_core::EnvRequirement::Tool {
            name: "unsquashfs".to_string(),
        },
    ]
}

/// Environment requirements for the snapcraft-publish stage: the `snapcraft`
/// CLI plus the store login (`SNAPCRAFT_STORE_CREDENTIALS`, which the
/// snapcraft CLI itself consumes) whenever any snap opts into `publish`.
pub fn publish_env_requirements(
    ctx: &anodizer_core::context::Context,
) -> Vec<anodizer_core::EnvRequirement> {
    let any = anodizer_core::env_preflight::crate_universe(&ctx.config)
        .into_iter()
        .flat_map(|c| c.snapcrafts.iter().flatten())
        .any(|s| s.publish == Some(true) && !snap_entry_skipped(ctx, s));
    if !any {
        return Vec::new();
    }
    vec![
        anodizer_core::EnvRequirement::Tool {
            name: "snapcraft".to_string(),
        },
        anodizer_core::EnvRequirement::EnvAllOf {
            vars: vec!["SNAPCRAFT_STORE_CREDENTIALS".to_string()],
        },
    ]
}

/// True when a `snapcrafts:` entry's `skip:` template resolves truthy at
/// preflight time (unrenderable templates count as active so preflight
/// over-collects rather than under-collects).
fn snap_entry_skipped(
    ctx: &anodizer_core::context::Context,
    s: &anodizer_core::config::SnapcraftConfig,
) -> bool {
    s.skip.as_ref().is_some_and(|v| {
        v.try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .unwrap_or(false)
    })
}
