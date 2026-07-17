use anodizer_core::config::AnnounceConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::retry::RetryPolicy;
use anyhow::Result;

use crate::dispatch::{DispatchQueue, dispatch};
use crate::helpers::{is_enabled, render_message, require_non_empty_env_with_env};
use crate::mastodon;

use super::Announcer;

pub(super) struct MastodonAnnouncer;
impl Announcer for MastodonAnnouncer {
    fn name(&self) -> &'static str {
        "mastodon"
    }
    fn enabled(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<bool> {
        match &announce.mastodon {
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
            .mastodon
            .as_ref()
            .expect("send only called when enabled returned Ok(true)");
        // Skip-when-empty UX: missing or empty `server` warn-and-skip
        // in normal mode, bail in strict mode.
        let server = match cfg.server.as_deref() {
            Some(raw) => ctx.render_template(raw)?,
            None => {
                ctx.strict_guard(log, "missing server in announce.mastodon")?;
                return Ok(());
            }
        };
        if server.is_empty() {
            ctx.strict_guard(log, "server in announce.mastodon must not be empty")?;
            return Ok(());
        }
        let message = render_message(ctx, cfg.message_template.as_deref())?;
        // The mastodon config declares all three
        // env-backed fields (ClientID, ClientSecret, AccessToken) as
        // `notEmpty`, so missing any one of them fails fast at
        // validation time. Anodizer used to require only
        // ACCESS_TOKEN, silently sending without the credentials
        // required for the OAuth refresh flow. Mirror that
        // fail-fast here so misconfigured releases die up front
        // instead of mid-announce.
        let access_token =
            require_non_empty_env_with_env("mastodon", "MASTODON_ACCESS_TOKEN", ctx.env_source())?;
        require_non_empty_env_with_env("mastodon", "MASTODON_CLIENT_ID", ctx.env_source())?;
        require_non_empty_env_with_env("mastodon", "MASTODON_CLIENT_SECRET", ctx.env_source())?;
        let retry_policy = *retry_policy;
        // Owned clone: the queued closure must be 'static like its other captures.
        let qlog = log.clone();
        dispatch(
            ctx,
            queue,
            "mastodon",
            message.clone(),
            key_width,
            move || mastodon::send_mastodon(&server, &access_token, &message, &retry_policy, &qlog),
        )
    }
    fn render_only(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<()> {
        let Some(cfg) = announce.mastodon.as_ref() else {
            return Ok(());
        };
        if let Some(raw) = cfg.server.as_deref() {
            ctx.render_template(raw)?;
        }
        render_message(ctx, cfg.message_template.as_deref())?;
        Ok(())
    }
}
