use anodizer_core::config::AnnounceConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::retry::RetryPolicy;
use anyhow::Result;

use crate::dispatch::{DispatchQueue, dispatch};
use crate::helpers::{is_enabled, render_message, require_env_all_with_env};
use crate::twitter;

use super::Announcer;

pub(super) struct TwitterAnnouncer;
impl Announcer for TwitterAnnouncer {
    fn name(&self) -> &'static str {
        "twitter"
    }
    fn enabled(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<bool> {
        match &announce.twitter {
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
            .twitter
            .as_ref()
            .expect("send only called when enabled returned Ok(true)");
        let message = render_message(ctx, cfg.message_template.as_deref())?;
        let creds = require_env_all_with_env(
            "twitter",
            &[
                "TWITTER_CONSUMER_KEY",
                "TWITTER_CONSUMER_SECRET",
                "TWITTER_ACCESS_TOKEN",
                "TWITTER_ACCESS_TOKEN_SECRET",
            ],
            ctx.env_source(),
        )?;
        // Owned (not borrowed from `creds`) so the queued closure is `'static`.
        let consumer_key = creds[0].clone();
        let consumer_secret = creds[1].clone();
        let access_token = creds[2].clone();
        let access_token_secret = creds[3].clone();

        let retry_policy = *retry_policy;
        // Owned clone: the queued closure must be 'static like its other captures.
        let qlog = log.clone();
        dispatch(
            ctx,
            queue,
            "twitter",
            message.clone(),
            key_width,
            move || {
                twitter::send_twitter(
                    &consumer_key,
                    &consumer_secret,
                    &access_token,
                    &access_token_secret,
                    &message,
                    &retry_policy,
                    &qlog,
                )
            },
        )
    }
    fn render_only(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<()> {
        let Some(cfg) = announce.twitter.as_ref() else {
            return Ok(());
        };
        render_message(ctx, cfg.message_template.as_deref())?;
        Ok(())
    }
}
