use anodizer_core::config::AnnounceConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::retry::RetryPolicy;
use anyhow::Result;

use super::validators::validate_discord_color;
use crate::discord;
use crate::dispatch::{DispatchQueue, dispatch};
use crate::helpers::{DEFAULT_DISPLAY_NAME, is_enabled, render_message};

use super::Announcer;

pub(super) struct DiscordAnnouncer;
impl Announcer for DiscordAnnouncer {
    fn name(&self) -> &'static str {
        "discord"
    }
    fn enabled(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<bool> {
        match &announce.discord {
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
            .discord
            .as_ref()
            .expect("send only called when enabled returned Ok(true)");
        let id = ctx.env_var("DISCORD_WEBHOOK_ID").filter(|s| !s.is_empty());
        let token = ctx
            .env_var("DISCORD_WEBHOOK_TOKEN")
            .filter(|s| !s.is_empty());
        let url = match (id, token) {
            (Some(id), Some(token)) => {
                let base = ctx
                    .env_var("DISCORD_API")
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "https://discord.com/api".to_string());
                // Build the webhook URL via
                // `url.URL.JoinPath(...)`, which percent-encodes path
                // segments. Discord webhook IDs and tokens are
                // alphanumeric+`_-` in practice, but a malformed env
                // value (`/`, `?`, `#`, …) used to splice straight
                // into the URL and silently corrupt the request.
                // Encoding the segments produces a clean 4xx that
                // can actually be debugged.
                format!(
                    "{}/webhooks/{}/{}",
                    base.trim_end_matches('/'),
                    anodizer_core::url::percent_encode_path_segment(&id),
                    anodizer_core::url::percent_encode_path_segment(&token),
                )
            }
            _ => match cfg.webhook_url.as_deref() {
                Some(raw) => ctx.render_template(raw)?,
                None => {
                    // Skip-when-empty UX policy: in strict mode this
                    // bails (collected by the closure-level wrapper
                    // and reported at end-of-stage); in normal mode
                    // it warns and returns Ok so unrelated announcers
                    // still run.
                    ctx.strict_guard(
                        log,
                        "missing webhook_url in announce.discord \
                             (set discord.webhook_url, or both \
                             DISCORD_WEBHOOK_ID and DISCORD_WEBHOOK_TOKEN env vars)",
                    )?;
                    return Ok(());
                }
            },
        };
        let message = render_message(ctx, cfg.message_template.as_deref())?;
        let author =
            ctx.render_template_opt(cfg.author.as_deref().or(Some(DEFAULT_DISPLAY_NAME)))?;
        let color: Option<u32> = match cfg.color.as_deref() {
            Some(raw) => {
                let rendered = ctx.render_template(raw)?;
                validate_discord_color(&rendered)?
            }
            None => None,
        };
        let icon_url = ctx.render_template_opt(cfg.icon_url.as_deref())?;
        // Owned copy so the queued closure is `'static`.
        let retry_policy = *retry_policy;
        // Owned clone: the queued closure must be 'static like its other captures.
        let qlog = log.clone();
        dispatch(
            ctx,
            queue,
            "discord",
            message.clone(),
            key_width,
            move || {
                let opts = discord::DiscordOptions {
                    author: author.as_deref(),
                    color,
                    icon_url: icon_url.as_deref(),
                };
                discord::send_discord(&url, &message, &opts, &retry_policy, &qlog)
            },
        )
    }
    fn render_only(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<()> {
        let Some(cfg) = announce.discord.as_ref() else {
            return Ok(());
        };
        if let Some(raw) = cfg.webhook_url.as_deref() {
            ctx.render_template(raw)?;
        }
        render_message(ctx, cfg.message_template.as_deref())?;
        ctx.render_template_opt(cfg.author.as_deref().or(Some(DEFAULT_DISPLAY_NAME)))?;
        if let Some(raw) = cfg.color.as_deref() {
            let rendered = ctx.render_template(raw)?;
            validate_discord_color(&rendered)?;
        }
        ctx.render_template_opt(cfg.icon_url.as_deref())?;
        Ok(())
    }
}
