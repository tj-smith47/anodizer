use anodizer_core::config::AnnounceConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::retry::RetryPolicy;
use anyhow::Result;

use crate::bluesky;
use crate::dispatch::{DispatchQueue, dispatch};
use crate::helpers::{is_enabled, render_message, require_env_with_env};

use super::Announcer;

pub(super) struct BlueskyAnnouncer;
impl Announcer for BlueskyAnnouncer {
    fn name(&self) -> &'static str {
        "bluesky"
    }
    fn enabled(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<bool> {
        match &announce.bluesky {
            Some(cfg) => is_enabled(ctx, cfg.enabled.as_ref()),
            None => Ok(false),
        }
    }
    fn send(
        &self,
        ctx: &mut Context,
        announce: &AnnounceConfig,
        retry_policy: &RetryPolicy,
        log: &StageLogger,
        key_width: usize,
        queue: &mut DispatchQueue,
    ) -> Result<()> {
        let cfg = announce
            .bluesky
            .as_ref()
            .expect("send only called when enabled returned Ok(true)");
        // Skip-when-empty UX: missing `username` warn-and-skips in
        // normal mode and bails in strict mode. BLUESKY_APP_PASSWORD
        // missing still hard-bails (credential, not skip-when-empty).
        let username = match cfg.username.as_deref() {
            Some(raw) => ctx.render_template(raw)?,
            None => {
                ctx.strict_guard(log, "missing username in announce.bluesky")?;
                return Ok(());
            }
        };
        let message = render_message(ctx, cfg.message_template.as_deref())?;
        let app_password =
            require_env_with_env("bluesky", "BLUESKY_APP_PASSWORD", ctx.env_source())?;
        let release_url = ctx.template_vars().get("ReleaseURL").map(|s| s.to_string());
        let pds_url = cfg
            .pds_url
            .as_deref()
            .map(|u| ctx.render_template(u))
            .transpose()?;

        let retry_policy = *retry_policy;
        // Owned clone: the queued closure must be 'static like its other captures.
        let qlog = log.clone();
        dispatch(
            ctx,
            queue,
            "bluesky",
            message.clone(),
            key_width,
            move || {
                bluesky::send_bluesky(
                    &username,
                    &app_password,
                    &message,
                    release_url.as_deref(),
                    pds_url.as_deref(),
                    &retry_policy,
                    &qlog,
                )
            },
        )
    }
    fn render_only(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<()> {
        let Some(cfg) = announce.bluesky.as_ref() else {
            return Ok(());
        };
        if let Some(raw) = cfg.username.as_deref() {
            ctx.render_template(raw)?;
        }
        render_message(ctx, cfg.message_template.as_deref())?;
        if let Some(raw) = cfg.pds_url.as_deref() {
            ctx.render_template(raw)?;
        }
        Ok(())
    }
}
