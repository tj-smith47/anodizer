//! Announce stage — broadcasts a release across configured providers.
//!
//! Per-provider modules (`bluesky`, `discord`, …) own their wire formats and
//! send loops; [`AnnounceStage`] in [`run`] is a fan-out dispatcher that walks
//! the `announce.<provider>` config blocks, renders messages via the shared
//! [`helpers`], and collects errors so one provider failure doesn't block the
//! others.

// Provider modules — already split, untouched by this carve.
pub mod bluesky;
pub mod discord;
pub mod discourse;
pub mod email;
mod http;
pub mod linkedin;
pub mod mastodon;
pub mod mattermost;
pub mod opencollective;
pub mod reddit;
pub mod slack;
pub mod teams;
pub mod telegram;
pub mod twitter;
mod util;
pub mod webhook;

// Stage orchestration — extracted by the lib.rs carve.
mod announcers;
mod dispatch;
mod helpers;
pub mod render_check;
mod run;
mod sent_marker;

#[cfg(test)]
mod tests;

pub use render_check::validate_announce_templates;
pub use run::{AnnounceStage, emit_summary};

/// Dispatch a filtered subset of configured announcers without an idempotency
/// sent-marker (suitable for ad-hoc notifications outside the release pipeline).
///
/// Fire a filtered subset of announce integrations.
///
/// `include` — when `Some`, only fire announcers whose name appears in the
/// slice. `skip` — omit these integration names regardless of `include`.
/// Per-provider errors are collected into `errors` rather than short-circuiting,
/// so one failing integration does not block the others.
pub fn dispatch_filtered_announcers(
    ctx: &mut anodizer_core::context::Context,
    announce: &anodizer_core::config::AnnounceConfig,
    retry_policy: &anodizer_core::retry::RetryPolicy,
    log: &anodizer_core::log::StageLogger,
    errors: &mut Vec<String>,
    include: Option<&[&str]>,
    skip: &[&str],
) -> anyhow::Result<()> {
    announcers::dispatch_filtered_announcers(
        ctx,
        announce,
        retry_policy,
        log,
        errors,
        None,
        announcers::AnnounceFilter { include, skip },
    )
}
