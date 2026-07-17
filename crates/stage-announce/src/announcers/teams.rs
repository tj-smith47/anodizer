use anodizer_core::config::AnnounceConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::retry::RetryPolicy;
use anyhow::Result;

use crate::dispatch::{DispatchQueue, dispatch};
use crate::helpers::{is_enabled, render_message};
use crate::teams;

use super::Announcer;

pub(super) struct TeamsAnnouncer;
impl Announcer for TeamsAnnouncer {
    fn name(&self) -> &'static str {
        "teams"
    }
    fn enabled(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<bool> {
        match &announce.teams {
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
            .teams
            .as_ref()
            .expect("send only called when enabled returned Ok(true)");
        let url = match cfg.webhook_url.as_deref() {
            Some(u) => ctx.render_template(u)?,
            None => match ctx.env_var("TEAMS_WEBHOOK").filter(|s| !s.is_empty()) {
                Some(env) => env,
                None => {
                    ctx.strict_guard(
                        log,
                        "missing webhook_url in announce.teams (set config or TEAMS_WEBHOOK env var)",
                    )?;
                    return Ok(());
                }
            },
        };
        let message = render_message(ctx, cfg.message_template.as_deref())?;
        let title_template = cfg
            .title_template
            .as_deref()
            .unwrap_or(anodizer_core::config::TEAMS_DEFAULT_TITLE_TEMPLATE);
        let title = Some(ctx.render_template(title_template)?);
        let color_val = cfg.color.clone().unwrap_or_else(|| "#2D313E".to_string());
        let icon_url = ctx.render_template_opt(cfg.icon_url.as_deref())?;
        let retry_policy = *retry_policy;
        // Owned clone: the queued closure must be 'static like its other captures.
        let qlog = log.clone();
        dispatch(ctx, queue, "teams", message.clone(), key_width, move || {
            let opts = teams::TeamsOptions {
                title: title.as_deref(),
                color: Some(color_val.as_str()),
                icon_url: icon_url.as_deref(),
            };
            teams::send_teams(&url, &message, &opts, &retry_policy, &qlog)
        })
    }
    fn render_only(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<()> {
        let Some(cfg) = announce.teams.as_ref() else {
            return Ok(());
        };
        if let Some(raw) = cfg.webhook_url.as_deref() {
            ctx.render_template(raw)?;
        }
        render_message(ctx, cfg.message_template.as_deref())?;
        ctx.render_template(
            cfg.title_template
                .as_deref()
                .unwrap_or(anodizer_core::config::TEAMS_DEFAULT_TITLE_TEMPLATE),
        )?;
        ctx.render_template_opt(cfg.icon_url.as_deref())?;
        Ok(())
    }
}
