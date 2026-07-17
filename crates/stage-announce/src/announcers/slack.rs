use anodizer_core::config::AnnounceConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::retry::RetryPolicy;
use anyhow::Result;

use crate::dispatch::{DispatchQueue, dispatch};
use crate::helpers::{DEFAULT_DISPLAY_NAME, is_enabled, render_json_template, render_message};
use crate::slack;

use super::Announcer;

pub(super) struct SlackAnnouncer;
impl Announcer for SlackAnnouncer {
    fn name(&self) -> &'static str {
        "slack"
    }
    fn enabled(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<bool> {
        match &announce.slack {
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
            .slack
            .as_ref()
            .expect("send only called when enabled returned Ok(true)");
        let url = match cfg.webhook_url.as_deref() {
            Some(u) => ctx.render_template(u)?,
            None => match ctx.env_var("SLACK_WEBHOOK").filter(|s| !s.is_empty()) {
                Some(env) => env,
                None => {
                    // Skip-when-empty UX policy: strict_guard bails in
                    // strict mode (collected at end-of-stage); in normal
                    // mode it warns and skips just this announcer.
                    ctx.strict_guard(
                        log,
                        "missing webhook_url in announce.slack (set config or SLACK_WEBHOOK env var)",
                    )?;
                    return Ok(());
                }
            },
        };
        let message = render_message(ctx, cfg.message_template.as_deref())?;
        let channel = ctx.render_template_opt(cfg.channel.as_deref())?;
        let username =
            ctx.render_template_opt(cfg.username.as_deref().or(Some(DEFAULT_DISPLAY_NAME)))?;
        let icon_emoji = ctx.render_template_opt(cfg.icon_emoji.as_deref())?;
        let icon_url = ctx.render_template_opt(cfg.icon_url.as_deref())?;
        let blocks = match cfg.blocks.as_ref() {
            Some(b) => render_json_template(ctx, Some(&serde_json::to_value(b)?))?,
            None => None,
        };
        let attachments = match cfg.attachments.as_ref() {
            Some(a) => render_json_template(ctx, Some(&serde_json::to_value(a)?))?,
            None => None,
        };
        let retry_policy = *retry_policy;
        // Owned clone: the queued closure must be 'static like its other captures.
        let qlog = log.clone();
        dispatch(ctx, queue, "slack", message.clone(), key_width, move || {
            let opts = slack::SlackOptions {
                channel: channel.as_deref(),
                username: username.as_deref(),
                icon_emoji: icon_emoji.as_deref(),
                icon_url: icon_url.as_deref(),
                blocks: blocks.as_ref(),
                attachments: attachments.as_ref(),
            };
            slack::send_slack(&url, &message, &opts, &retry_policy, &qlog)
        })
    }
    fn render_only(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<()> {
        let Some(cfg) = announce.slack.as_ref() else {
            return Ok(());
        };
        if let Some(raw) = cfg.webhook_url.as_deref() {
            ctx.render_template(raw)?;
        }
        render_message(ctx, cfg.message_template.as_deref())?;
        ctx.render_template_opt(cfg.channel.as_deref())?;
        ctx.render_template_opt(cfg.username.as_deref().or(Some(DEFAULT_DISPLAY_NAME)))?;
        ctx.render_template_opt(cfg.icon_emoji.as_deref())?;
        ctx.render_template_opt(cfg.icon_url.as_deref())?;
        if let Some(b) = cfg.blocks.as_ref() {
            render_json_template(ctx, Some(&serde_json::to_value(b)?))?;
        }
        if let Some(a) = cfg.attachments.as_ref() {
            render_json_template(ctx, Some(&serde_json::to_value(a)?))?;
        }
        Ok(())
    }
}
