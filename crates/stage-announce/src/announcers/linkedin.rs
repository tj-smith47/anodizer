use anodizer_core::config::AnnounceConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::retry::RetryPolicy;
use anyhow::Result;

use crate::dispatch::{DispatchQueue, dispatch};
use crate::helpers::{is_enabled, render_message, require_env_with_env};
use crate::linkedin;

use super::Announcer;

pub(super) struct LinkedInAnnouncer;
impl Announcer for LinkedInAnnouncer {
    fn name(&self) -> &'static str {
        "linkedin"
    }
    fn enabled(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<bool> {
        match &announce.linkedin {
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
            .linkedin
            .as_ref()
            .expect("send only called when enabled returned Ok(true)");
        let message = render_message(ctx, cfg.message_template.as_deref())?;
        let access_token =
            require_env_with_env("linkedin", "LINKEDIN_ACCESS_TOKEN", ctx.env_source())?
                .trim()
                .to_string();
        if access_token
            .chars()
            .any(|c| c.is_whitespace() || !c.is_ascii() || c.is_ascii_control())
        {
            anyhow::bail!(
                "announce.linkedin: LINKEDIN_ACCESS_TOKEN contains whitespace or \
                     non-printable characters — check for stray quotes or line wraps"
            );
        }
        linkedin::validate_token_shape(&access_token)?;
        let retry_policy = *retry_policy;
        // Cloned so the queued closure owns its logger.
        let log = log.clone();
        dispatch(
            ctx,
            queue,
            "linkedin",
            message.clone(),
            key_width,
            move || linkedin::send_linkedin(&access_token, &message, &log, &retry_policy),
        )
    }
    fn render_only(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<()> {
        let Some(cfg) = announce.linkedin.as_ref() else {
            return Ok(());
        };
        render_message(ctx, cfg.message_template.as_deref())?;
        Ok(())
    }
}
