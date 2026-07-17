use anodizer_core::config::AnnounceConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::retry::RetryPolicy;
use anyhow::Result;

use crate::dispatch::{DispatchQueue, dispatch};
use crate::helpers::{is_enabled, require_env_all_with_env};
use crate::reddit;

use super::Announcer;

pub(super) struct RedditAnnouncer;
impl Announcer for RedditAnnouncer {
    fn name(&self) -> &'static str {
        "reddit"
    }
    fn enabled(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<bool> {
        match &announce.reddit {
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
            .reddit
            .as_ref()
            .expect("send only called when enabled returned Ok(true)");
        // Skip-when-empty UX: missing required config fields
        // (application_id / username / sub) warn-and-skip in normal
        // mode and bail in strict mode. The required env vars
        // (REDDIT_SECRET, REDDIT_PASSWORD) still hard-bail because
        // missing credentials are a config error, not skip-when-empty.
        let app_id = match cfg.application_id.as_deref() {
            Some(raw) => ctx.render_template(raw)?,
            None => {
                ctx.strict_guard(log, "missing application_id in announce.reddit")?;
                return Ok(());
            }
        };
        let username = match cfg.username.as_deref() {
            Some(raw) => ctx.render_template(raw)?,
            None => {
                ctx.strict_guard(log, "missing username in announce.reddit")?;
                return Ok(());
            }
        };
        let sub = match cfg.sub.as_deref() {
            Some(raw) => ctx.render_template(raw)?,
            None => {
                ctx.strict_guard(log, "missing sub in announce.reddit")?;
                return Ok(());
            }
        };
        let title = ctx.render_template(
            cfg.title_template
                .as_deref()
                .unwrap_or("{{ ProjectName }} {{ Tag }} is out!"),
        )?;
        let url = ctx.render_template(cfg.url_template.as_deref().unwrap_or("{{ ReleaseURL }}"))?;
        let creds = require_env_all_with_env(
            "reddit",
            &["REDDIT_SECRET", "REDDIT_PASSWORD"],
            ctx.env_source(),
        )?;
        // Owned (not borrowed from `creds`) so the queued closure is `'static`.
        let secret = creds[0].clone();
        let password = creds[1].clone();
        let retry_policy = *retry_policy;
        // Cloned so the queued closure owns its logger.
        let log = log.clone();
        dispatch(
            ctx,
            queue,
            "reddit",
            format!("r/{sub}: {title}"),
            key_width,
            move || {
                reddit::send_reddit(
                    &reddit::RedditPost {
                        application_id: &app_id,
                        secret: &secret,
                        username: &username,
                        password: &password,
                        subreddit: &sub,
                        title: &title,
                        url: &url,
                    },
                    &log,
                    &retry_policy,
                )
            },
        )
    }
    fn render_only(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<()> {
        let Some(cfg) = announce.reddit.as_ref() else {
            return Ok(());
        };
        if let Some(raw) = cfg.application_id.as_deref() {
            ctx.render_template(raw)?;
        }
        if let Some(raw) = cfg.username.as_deref() {
            ctx.render_template(raw)?;
        }
        if let Some(raw) = cfg.sub.as_deref() {
            ctx.render_template(raw)?;
        }
        ctx.render_template(
            cfg.title_template
                .as_deref()
                .unwrap_or("{{ ProjectName }} {{ Tag }} is out!"),
        )?;
        ctx.render_template(cfg.url_template.as_deref().unwrap_or("{{ ReleaseURL }}"))?;
        Ok(())
    }
}
