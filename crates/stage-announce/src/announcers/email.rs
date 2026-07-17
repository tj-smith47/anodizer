use anodizer_core::config::AnnounceConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::retry::RetryPolicy;
use anyhow::Result;

use super::validators::validate_email_from;
use crate::dispatch::{DispatchQueue, dispatch};
use crate::email;
use crate::helpers::{
    is_enabled, render_message_with_default, require_env_with_env, resolve_smtp_port,
};

use super::Announcer;

/// Default email body, shared by `send` and `render_only` so the pre-publish
/// guard exercises the exact default `send` would use, and so the two sites
/// can never drift apart.
const EMAIL_DEFAULT_MESSAGE_TEMPLATE: &str = "You can view details from: {{ ReleaseURL }}";

pub(super) struct EmailAnnouncer;
impl Announcer for EmailAnnouncer {
    fn name(&self) -> &'static str {
        "email"
    }
    fn enabled(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<bool> {
        match &announce.email {
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
            .email
            .as_ref()
            .expect("send only called when enabled returned Ok(true)");
        // Skip-when-empty UX: missing `from` or empty `to` skips the
        // email announcer in normal mode (warn) and bails in strict
        // mode. A configured-but-malformed `from` (string set but no
        // `@`) is a config error, not skip-when-empty, so it stays a
        // hard bail regardless of strict mode.
        let from = match cfg.from.as_deref() {
            Some(raw) => ctx.render_template(raw)?,
            None => {
                ctx.strict_guard(log, "missing from in announce.email")?;
                return Ok(());
            }
        };

        validate_email_from(&from)?;

        if cfg.to.is_empty() {
            ctx.strict_guard(log, "missing to (recipient list) in announce.email")?;
            return Ok(());
        }

        let subject = ctx.render_template(
            cfg.subject_template
                .as_deref()
                .unwrap_or("{{ ProjectName }} {{ Tag }} is out!"),
        )?;
        let body = render_message_with_default(
            ctx,
            cfg.message_template.as_deref(),
            EMAIL_DEFAULT_MESSAGE_TEMPLATE,
        )?;

        // Owned (cloned off `cfg`) so the queued closures are `'static`; each
        // closure builds its own `EmailParams` from these moved-in values.
        let to = cfg.to.clone();
        let log_line = format!("to {}: {}", to.join(", "), subject);
        let retry_policy = *retry_policy;

        // Support SMTP_HOST and SMTP_PORT env vars as fallbacks.
        let smtp_host = cfg
            .host
            .clone()
            .or_else(|| ctx.env_var("SMTP_HOST").filter(|s| !s.is_empty()));
        let smtp_port_env = ctx.env_var("SMTP_PORT").and_then(|s| s.parse::<u16>().ok());
        let smtp_port = resolve_smtp_port(cfg.port, smtp_port_env);

        if let Some(host) = smtp_host {
            let smtp_username = cfg
                .username
                .clone()
                .or_else(|| ctx.env_var("SMTP_USERNAME"))
                .unwrap_or_default();
            if smtp_username.is_empty() {
                anyhow::bail!("announce.email: SMTP username is required");
            }
            let encryption = cfg.encryption.unwrap_or_default();
            let needs_password = !matches!(
                email::resolve_encryption(encryption, smtp_port),
                anodizer_core::config::EmailEncryption::None
            );
            let smtp_password = if needs_password {
                require_env_with_env("email", "SMTP_PASSWORD", ctx.env_source())?
            } else {
                ctx.env_var("SMTP_PASSWORD").unwrap_or_default()
            };
            let port = smtp_port;
            let insecure = cfg.insecure_skip_verify.unwrap_or(false);

            // Owned clone: the queued closure must be 'static like its other captures.
            let qlog = log.clone();
            dispatch(
                ctx,
                queue,
                "email",
                format!("via smtp {log_line}"),
                key_width,
                move || {
                    let email_params = email::EmailParams {
                        from: &from,
                        to: &to,
                        subject: &subject,
                        body: &body,
                    };
                    let smtp_params = email::SmtpParams {
                        host: &host,
                        port,
                        username: &smtp_username,
                        password: &smtp_password,
                        insecure_skip_verify: insecure,
                        encryption,
                    };
                    email::send_smtp(&email_params, &smtp_params, &retry_policy, &qlog)
                },
            )?;
        } else {
            // Cloned so the queued closure owns its logger.
            let log = log.clone();
            dispatch(
                ctx,
                queue,
                "email",
                format!("via sendmail {log_line}"),
                key_width,
                move || {
                    let email_params = email::EmailParams {
                        from: &from,
                        to: &to,
                        subject: &subject,
                        body: &body,
                    };
                    email::send_sendmail(&email_params, &log)
                },
            )?;
        }
        Ok(())
    }
    fn render_only(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<()> {
        let Some(cfg) = announce.email.as_ref() else {
            return Ok(());
        };
        // Config-shape guard (no env, no secret): when the config selects the
        // SMTP path by setting `host:`, an explicitly EMPTY `username: ""` is an
        // unambiguous typo — `send` hard-bails "SMTP username is required" on it.
        // A `username: None` is NOT flagged here: SMTP_USERNAME may legitimately
        // supply it at send time, and the prepublish guard runs without env, so
        // rejecting an absent username would false-positive on a valid
        // env-supplied config (env/secret-dependent checks stay post-publish).
        if cfg.host.is_some() && cfg.username.as_deref() == Some("") {
            anyhow::bail!("announce.email: SMTP username is required");
        }
        if let Some(raw) = cfg.from.as_deref() {
            let from = ctx.render_template(raw)?;
            validate_email_from(&from)?;
        }
        ctx.render_template(
            cfg.subject_template
                .as_deref()
                .unwrap_or("{{ ProjectName }} {{ Tag }} is out!"),
        )?;
        render_message_with_default(
            ctx,
            cfg.message_template.as_deref(),
            EMAIL_DEFAULT_MESSAGE_TEMPLATE,
        )?;
        Ok(())
    }
}
