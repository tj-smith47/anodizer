use anodizer_core::config::AnnounceConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::retry::RetryPolicy;
use anyhow::Result;

use crate::dispatch::{DispatchQueue, dispatch};
use crate::helpers::{is_enabled, render_message_with_default, require_env_with_env};
use crate::opencollective;

use super::Announcer;

pub(super) struct OpenCollectiveAnnouncer;
impl Announcer for OpenCollectiveAnnouncer {
    fn name(&self) -> &'static str {
        "opencollective"
    }
    fn enabled(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<bool> {
        match &announce.opencollective {
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
            .opencollective
            .as_ref()
            .expect("send only called when enabled returned Ok(true)");
        // Skip-when-empty UX: missing or empty `slug` warn-and-skip in
        // normal mode, bail in strict mode.
        let slug = match cfg.slug.as_deref() {
            Some(raw) => ctx.render_template(raw)?,
            None => {
                ctx.strict_guard(log, "missing slug in announce.opencollective")?;
                return Ok(());
            }
        };
        if slug.is_empty() {
            ctx.strict_guard(log, "slug in announce.opencollective must not be empty")?;
            return Ok(());
        }
        opencollective::validate_slug(&slug)?;
        let title = ctx.render_template(
            cfg.title_template
                .as_deref()
                .unwrap_or(opencollective::DEFAULT_TITLE_TEMPLATE),
        )?;
        let html = render_message_with_default(
            ctx,
            cfg.message_template.as_deref(),
            opencollective::DEFAULT_MESSAGE_TEMPLATE,
        )?;
        let token =
            require_env_with_env("opencollective", "OPENCOLLECTIVE_TOKEN", ctx.env_source())?;
        opencollective::validate_token_shape(&token)?;
        let retry_policy = *retry_policy;
        // Owned clone: the queued closure must be 'static like its other captures.
        let qlog = log.clone();
        dispatch(
            ctx,
            queue,
            "opencollective",
            title.clone(),
            key_width,
            move || {
                opencollective::send_opencollective(
                    &token,
                    &slug,
                    &title,
                    &html,
                    &retry_policy,
                    &qlog,
                )
            },
        )
    }
    fn render_only(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<()> {
        let Some(cfg) = announce.opencollective.as_ref() else {
            return Ok(());
        };
        if let Some(raw) = cfg.slug.as_deref() {
            let slug = ctx.render_template(raw)?;
            // Mirror `send`: a non-empty slug must satisfy the format rules.
            // An empty render is skip-when-empty in `send`, so don't reject it.
            if !slug.is_empty() {
                opencollective::validate_slug(&slug)?;
            }
        }
        ctx.render_template(
            cfg.title_template
                .as_deref()
                .unwrap_or(opencollective::DEFAULT_TITLE_TEMPLATE),
        )?;
        render_message_with_default(
            ctx,
            cfg.message_template.as_deref(),
            opencollective::DEFAULT_MESSAGE_TEMPLATE,
        )?;
        Ok(())
    }
}
