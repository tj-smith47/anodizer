use anodizer_core::config::AnnounceConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::retry::RetryPolicy;
use anyhow::Result;

use crate::discourse;
use crate::dispatch::{DispatchQueue, dispatch};
use crate::helpers::{is_enabled, render_message, require_env_with_env};

use super::Announcer;

pub(super) struct DiscourseAnnouncer;
impl Announcer for DiscourseAnnouncer {
    fn name(&self) -> &'static str {
        "discourse"
    }
    fn enabled(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<bool> {
        match &announce.discourse {
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
            .discourse
            .as_ref()
            .expect("send only called when enabled returned Ok(true)");
        // Skip-when-empty UX policy: missing or empty `server` /
        // missing `category_id` warn-and-skip in normal mode and bail
        // in strict mode. A configured-but-zero `category_id` is a
        // config error, not skip-when-empty, so it stays a hard bail.
        let server = match cfg.server.as_deref() {
            Some(raw) => ctx.render_template(raw)?,
            None => {
                ctx.strict_guard(log, "missing server in announce.discourse")?;
                return Ok(());
            }
        };
        if server.is_empty() {
            ctx.strict_guard(log, "server in announce.discourse must not be empty")?;
            return Ok(());
        }
        let category_id = match cfg.category_id {
            Some(id) => id,
            None => {
                ctx.strict_guard(log, "missing category_id in announce.discourse")?;
                return Ok(());
            }
        };
        if category_id == 0 {
            anyhow::bail!("announce.discourse: category_id must be non-zero");
        }
        // Owned (not borrowed from `cfg`) so the queued closure is `'static`.
        let username = cfg.username.as_deref().unwrap_or("system").to_string();
        let title = ctx.render_template(
            cfg.title_template
                .as_deref()
                .unwrap_or("{{ ProjectName }} {{ Tag }} is out!"),
        )?;
        let message = render_message(ctx, cfg.message_template.as_deref())?;
        let api_key = require_env_with_env("discourse", "DISCOURSE_API_KEY", ctx.env_source())?;

        let retry_policy = *retry_policy;
        // Owned clone: the queued closure must be 'static like its other captures.
        let qlog = log.clone();
        dispatch(
            ctx,
            queue,
            "discourse",
            title.clone(),
            key_width,
            move || {
                discourse::send_discourse(
                    &server,
                    &api_key,
                    &username,
                    category_id,
                    &title,
                    &message,
                    &retry_policy,
                    &qlog,
                )
            },
        )
    }
    fn render_only(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<()> {
        let Some(cfg) = announce.discourse.as_ref() else {
            return Ok(());
        };
        // Config-shape guard (no env, no secret): a configured-but-zero
        // `category_id` is an unambiguous typo, not skip-when-empty. `send`
        // hard-bails on it; surfacing the same check here fails it at the
        // prepublish guard, before any irreversible publisher fires, instead of
        // silently warning post-publish. A `None` category_id is the
        // skip-when-empty case (warn-and-skip in `send`) and is left untouched.
        if cfg.category_id == Some(0) {
            anyhow::bail!("announce.discourse: category_id must be non-zero");
        }
        if let Some(raw) = cfg.server.as_deref() {
            ctx.render_template(raw)?;
        }
        ctx.render_template(
            cfg.title_template
                .as_deref()
                .unwrap_or("{{ ProjectName }} {{ Tag }} is out!"),
        )?;
        render_message(ctx, cfg.message_template.as_deref())?;
        Ok(())
    }
}
