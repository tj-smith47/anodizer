use anodizer_core::config::AnnounceConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::retry::RetryPolicy;
use anyhow::Result;

use super::validators::validate_webhook_endpoint_url;
use crate::dispatch::{DispatchQueue, dispatch};
use crate::helpers::{
    WEBHOOK_DEFAULT_MESSAGE_TEMPLATE, is_enabled, render_message_with_default,
    resolve_webhook_headers,
};
use crate::webhook;
use std::collections::HashMap;

use super::Announcer;

pub(super) struct WebhookAnnouncer;
impl Announcer for WebhookAnnouncer {
    fn name(&self) -> &'static str {
        "webhook"
    }
    fn enabled(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<bool> {
        match &announce.webhook {
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
            .webhook
            .as_ref()
            .expect("send only called when enabled returned Ok(true)");
        // Skip-when-empty UX: missing endpoint_url skips this announcer
        // in normal mode (warn) and bails in strict mode.
        let url = match cfg.endpoint_url.as_deref() {
            Some(raw) => ctx.render_template(raw)?,
            None => {
                ctx.strict_guard(log, "missing endpoint_url in announce.webhook")?;
                return Ok(());
            }
        };
        // Strip embedded userinfo (`https://user:pass@host`) before the URL
        // lands in any operator-facing error message — handled inside the
        // shared validator, which the pre-publish guard also calls.
        validate_webhook_endpoint_url(&url)?;
        // webhook uses a JSON-envelope
        // default distinct from the plain-text default used by other
        // providers; receivers expect a parseable JSON body.
        let message = render_message_with_default(
            ctx,
            cfg.message_template.as_deref(),
            WEBHOOK_DEFAULT_MESSAGE_TEMPLATE,
        )?;

        let raw_headers = cfg.headers.clone().unwrap_or_default();
        let mut user_headers = HashMap::new();
        for (k, v) in &raw_headers {
            user_headers.insert(k.clone(), ctx.render_template(v)?);
        }

        // `BASIC_AUTH_HEADER_VALUE` / `BEARER_TOKEN_HEADER_VALUE` populate
        // `Authorization` only when the config didn't already set one —
        // user-supplied `headers.Authorization` wins (case-insensitive,
        // per RFC 7230). Basic auth takes priority over bearer token.
        //
        // Anodize-additive UX win: a `User-Agent: anodizer/<version>`
        // header is appended (unless the user overrides) so operators
        // can attribute incoming webhooks to anodizer for routing,
        // rate-limiting, and audit-log tagging. A static user-agent with
        // no version suffix would be the baseline; the
        // version-suffixed variant is tradeoff-free (same wire shape,
        // strictly more debuggable). Pinned by
        // `test_webhook_user_agent_is_anodizer_versioned`.
        let basic_auth_env = ctx.env_var("BASIC_AUTH_HEADER_VALUE");
        let bearer_token_env = ctx.env_var("BEARER_TOKEN_HEADER_VALUE");
        let headers = resolve_webhook_headers(
            user_headers,
            basic_auth_env.as_deref(),
            bearer_token_env.as_deref(),
            anodizer_core::http::USER_AGENT,
        );

        // Default content-type is "application/json; charset=utf-8".
        let content_type = cfg
            .content_type
            .clone()
            .unwrap_or_else(|| "application/json; charset=utf-8".to_string());

        let skip_tls = cfg.skip_tls_verify.unwrap_or(false);
        let expected_codes = if cfg.expected_status_codes.is_empty() {
            webhook::default_expected_status_codes()
        } else {
            cfg.expected_status_codes.clone()
        };
        let retry_policy = *retry_policy;
        // Owned clone: the queued closure must be 'static like its other captures.
        let qlog = log.clone();
        dispatch(
            ctx,
            queue,
            "webhook",
            message.clone(),
            key_width,
            move || {
                webhook::send_webhook(
                    &url,
                    &message,
                    &headers,
                    &content_type,
                    skip_tls,
                    &expected_codes,
                    &retry_policy,
                    &qlog,
                )
            },
        )
    }
    fn render_only(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<()> {
        let Some(cfg) = announce.webhook.as_ref() else {
            return Ok(());
        };
        if let Some(raw) = cfg.endpoint_url.as_deref() {
            let url = ctx.render_template(raw)?;
            validate_webhook_endpoint_url(&url)?;
        }
        render_message_with_default(
            ctx,
            cfg.message_template.as_deref(),
            WEBHOOK_DEFAULT_MESSAGE_TEMPLATE,
        )?;
        for (_, v) in cfg.headers.clone().unwrap_or_default() {
            ctx.render_template(&v)?;
        }
        Ok(())
    }
}
