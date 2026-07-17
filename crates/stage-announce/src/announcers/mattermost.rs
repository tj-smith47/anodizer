use anodizer_core::config::AnnounceConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::retry::RetryPolicy;
use anyhow::Result;

use crate::dispatch::{DispatchQueue, dispatch};
use crate::helpers::{DEFAULT_DISPLAY_NAME, is_enabled, render_message};
use crate::mattermost;

use super::Announcer;

pub(super) struct MattermostAnnouncer;
impl Announcer for MattermostAnnouncer {
    fn name(&self) -> &'static str {
        "mattermost"
    }
    fn enabled(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<bool> {
        match &announce.mattermost {
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
            .mattermost
            .as_ref()
            .expect("send only called when enabled returned Ok(true)");
        let url = match cfg.webhook_url.as_deref() {
            Some(u) => ctx.render_template(u)?,
            None => match ctx.env_var("MATTERMOST_WEBHOOK").filter(|s| !s.is_empty()) {
                Some(env) => env,
                None => {
                    ctx.strict_guard(
                        log,
                        "missing webhook_url in announce.mattermost (set config or MATTERMOST_WEBHOOK env var)",
                    )?;
                    return Ok(());
                }
            },
        };
        let message = render_message(ctx, cfg.message_template.as_deref())?;
        // Anodize-additive UX win (locked 2026-04-28): channel,
        // username, icon_url, and icon_emoji all run through the
        // template engine. The conventional behaviour passes these
        // fields raw — no template substitution. Rendering is
        // tradeoff-free (raw strings still pass through unchanged)
        // and unlocks per-tag channel routing like
        // `channel: "release-{{ Tag }}"`. Render errors surface via
        // the strict_guard collected-errors path, same as message.
        // Pinned by `test_mattermost_renders_channel_template`.
        let channel = ctx.render_template_opt(cfg.channel.as_deref())?;
        // Default username to DEFAULT_DISPLAY_NAME (brand-default policy
        // keeps anodizer's own attribution).
        let username =
            ctx.render_template_opt(cfg.username.as_deref().or(Some(DEFAULT_DISPLAY_NAME)))?;
        let icon_url = ctx.render_template_opt(cfg.icon_url.as_deref())?;
        let icon_emoji = ctx.render_template_opt(cfg.icon_emoji.as_deref())?;
        // Default color to "#2D313E". Reads
        // from `MattermostAnnounce.color` — anodizer always has, even
        // before a cross-pipe bug was fixed upstream
        // where mattermost mistakenly consulted `TeamsAnnounce.Color`.
        // Pinned by `test_mattermost_reads_own_color_not_teams`.
        let color_val = cfg.color.clone().unwrap_or_else(|| "#2D313E".to_string());
        // Default title to "{{ ProjectName }} {{ Tag }} is out!".
        let title_template = cfg
            .title_template
            .as_deref()
            .unwrap_or("{{ ProjectName }} {{ Tag }} is out!");
        let title = Some(ctx.render_template(title_template)?);

        let retry_policy = *retry_policy;
        // Owned clone: the queued closure must be 'static like its other captures.
        let qlog = log.clone();
        dispatch(
            ctx,
            queue,
            "mattermost",
            message.clone(),
            key_width,
            move || {
                let opts = mattermost::MattermostOptions {
                    channel: channel.as_deref(),
                    username: username.as_deref(),
                    icon_url: icon_url.as_deref(),
                    icon_emoji: icon_emoji.as_deref(),
                    color: Some(color_val.as_str()),
                    title: title.as_deref(),
                };
                mattermost::send_mattermost(&url, &message, &opts, &retry_policy, &qlog)
            },
        )
    }
    fn render_only(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<()> {
        let Some(cfg) = announce.mattermost.as_ref() else {
            return Ok(());
        };
        if let Some(raw) = cfg.webhook_url.as_deref() {
            ctx.render_template(raw)?;
        }
        render_message(ctx, cfg.message_template.as_deref())?;
        ctx.render_template_opt(cfg.channel.as_deref())?;
        ctx.render_template_opt(cfg.username.as_deref().or(Some(DEFAULT_DISPLAY_NAME)))?;
        ctx.render_template_opt(cfg.icon_url.as_deref())?;
        ctx.render_template_opt(cfg.icon_emoji.as_deref())?;
        ctx.render_template(
            cfg.title_template
                .as_deref()
                .unwrap_or("{{ ProjectName }} {{ Tag }} is out!"),
        )?;
        Ok(())
    }
}
