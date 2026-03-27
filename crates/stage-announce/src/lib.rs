use std::collections::HashMap;

use anodize_core::context::Context;
use anodize_core::stage::Stage;
use anyhow::Result;

pub mod discord;
pub mod email;
pub mod mattermost;
pub mod slack;
pub mod teams;
pub mod telegram;
pub mod webhook;

// ---------------------------------------------------------------------------
// Shared helpers to reduce boilerplate across providers
// ---------------------------------------------------------------------------

const DEFAULT_MESSAGE_TEMPLATE: &str = "{{ .ProjectName }} {{ .Tag }} released!";

/// Render a required config field through the template engine, bailing with
/// `provider: missing <field>` when the value is `None`.
fn require_rendered(
    ctx: &mut Context,
    raw: Option<&str>,
    provider: &str,
    field: &str,
) -> Result<String> {
    let value = raw.ok_or_else(|| anyhow::anyhow!("announce.{provider}: missing {field}"))?;
    ctx.render_template(value)
}

/// Render an optional config field through the template engine.
fn render_optional(ctx: &mut Context, raw: Option<&str>) -> Result<Option<String>> {
    match raw {
        Some(v) => Ok(Some(ctx.render_template(v)?)),
        None => Ok(None),
    }
}

/// Render a message template, falling back to the standard default.
fn render_message(ctx: &mut Context, tmpl: Option<&str>) -> Result<String> {
    ctx.render_template(tmpl.unwrap_or(DEFAULT_MESSAGE_TEMPLATE))
}

/// Log and optionally execute a provider send action, respecting dry-run mode.
fn dispatch(
    ctx: &Context,
    provider: &str,
    log_line: &str,
    send: impl FnOnce() -> Result<()>,
) -> Result<()> {
    if ctx.is_dry_run() {
        eprintln!("[announce] (dry-run) {provider}: {log_line}");
    } else {
        eprintln!("[announce] {provider}: {log_line}");
        send()?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// AnnounceStage
// ---------------------------------------------------------------------------

pub struct AnnounceStage;

impl Stage for AnnounceStage {
    fn name(&self) -> &str {
        "announce"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let announce = match ctx.config.announce.clone() {
            Some(a) => a,
            None => {
                eprintln!("[announce] no announce config — skipping");
                return Ok(());
            }
        };

        // ----------------------------------------------------------------
        // Discord
        // ----------------------------------------------------------------
        if let Some(cfg) = &announce.discord
            && cfg.enabled.unwrap_or(false)
        {
            let url = require_rendered(ctx, cfg.webhook_url.as_deref(), "discord", "webhook_url")?;
            let message = render_message(ctx, cfg.message_template.as_deref())?;
            dispatch(ctx, "discord", &message, || {
                discord::send_discord(&url, &message)
            })?;
        }

        // ----------------------------------------------------------------
        // Slack
        // ----------------------------------------------------------------
        if let Some(cfg) = &announce.slack
            && cfg.enabled.unwrap_or(false)
        {
            let url = require_rendered(ctx, cfg.webhook_url.as_deref(), "slack", "webhook_url")?;
            let message = render_message(ctx, cfg.message_template.as_deref())?;
            dispatch(ctx, "slack", &message, || {
                slack::send_slack(&url, &message)
            })?;
        }

        // ----------------------------------------------------------------
        // Generic HTTP webhook
        // ----------------------------------------------------------------
        if let Some(cfg) = &announce.webhook
            && cfg.enabled.unwrap_or(false)
        {
            let url =
                require_rendered(ctx, cfg.endpoint_url.as_deref(), "webhook", "endpoint_url")?;
            let message = render_message(ctx, cfg.message_template.as_deref())?;

            let raw_headers = cfg.headers.clone().unwrap_or_default();
            let mut headers = HashMap::new();
            for (k, v) in &raw_headers {
                headers.insert(k.clone(), ctx.render_template(v)?);
            }
            let content_type = cfg
                .content_type
                .clone()
                .unwrap_or_else(|| "application/json".to_string());

            dispatch(ctx, "webhook", &message, || {
                webhook::send_webhook(&url, &message, &headers, &content_type)
            })?;
        }

        // ----------------------------------------------------------------
        // Telegram
        // ----------------------------------------------------------------
        if let Some(cfg) = &announce.telegram
            && cfg.enabled.unwrap_or(false)
        {
            let bot_token =
                require_rendered(ctx, cfg.bot_token.as_deref(), "telegram", "bot_token")?;
            let chat_id = require_rendered(ctx, cfg.chat_id.as_deref(), "telegram", "chat_id")?;
            let message = render_message(ctx, cfg.message_template.as_deref())?;
            let parse_mode = render_optional(ctx, cfg.parse_mode.as_deref())?;

            dispatch(ctx, "telegram", &message, || {
                telegram::send_telegram(&bot_token, &chat_id, &message, parse_mode.as_deref())
            })?;
        }

        // ----------------------------------------------------------------
        // Microsoft Teams
        // ----------------------------------------------------------------
        if let Some(cfg) = &announce.teams
            && cfg.enabled.unwrap_or(false)
        {
            let url = require_rendered(ctx, cfg.webhook_url.as_deref(), "teams", "webhook_url")?;
            let message = render_message(ctx, cfg.message_template.as_deref())?;
            dispatch(ctx, "teams", &message, || {
                teams::send_teams(&url, &message)
            })?;
        }

        // ----------------------------------------------------------------
        // Mattermost
        // ----------------------------------------------------------------
        if let Some(cfg) = &announce.mattermost
            && cfg.enabled.unwrap_or(false)
        {
            let url =
                require_rendered(ctx, cfg.webhook_url.as_deref(), "mattermost", "webhook_url")?;
            let message = render_message(ctx, cfg.message_template.as_deref())?;
            let channel = render_optional(ctx, cfg.channel.as_deref())?;
            let username = render_optional(ctx, cfg.username.as_deref())?;
            let icon_url = render_optional(ctx, cfg.icon_url.as_deref())?;

            dispatch(ctx, "mattermost", &message, || {
                mattermost::send_mattermost(
                    &url,
                    &message,
                    channel.as_deref(),
                    username.as_deref(),
                    icon_url.as_deref(),
                )
            })?;
        }

        // ----------------------------------------------------------------
        // Email (via sendmail/msmtp)
        // ----------------------------------------------------------------
        if let Some(cfg) = &announce.email
            && cfg.enabled.unwrap_or(false)
        {
            let from = require_rendered(ctx, cfg.from.as_deref(), "email", "from")?;

            if !from.contains('@') {
                anyhow::bail!(
                    "announce.email: 'from' address {:?} does not look like a valid email (missing @)",
                    from
                );
            }

            if cfg.to.is_empty() {
                anyhow::bail!("announce.email: missing to (recipient list)");
            }

            let subject = ctx.render_template(
                cfg.subject_template
                    .as_deref()
                    .unwrap_or("{{ .ProjectName }} {{ .Tag }} released"),
            )?;
            let body = render_message(ctx, cfg.message_template.as_deref())?;

            let log_line = format!("to {}: {}", cfg.to.join(", "), subject);
            dispatch(ctx, "email", &log_line, || {
                email::send_email(&email::EmailParams {
                    from: &from,
                    to: &cfg.to,
                    subject: &subject,
                    body: &body,
                })
            })?;
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use anodize_core::config::{
        AnnounceConfig, AnnounceProviderConfig, Config, EmailAnnounce, MattermostAnnounce,
        TeamsAnnounce, TelegramAnnounce, WebhookConfig,
    };
    use anodize_core::context::{Context, ContextOptions};

    fn make_ctx(announce: Option<AnnounceConfig>) -> Context {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.announce = announce;
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set(
            "ReleaseURL",
            "https://github.com/org/myapp/releases/tag/v1.0.0",
        );
        ctx
    }

    #[test]
    fn test_skips_when_no_announce_config() {
        let mut ctx = make_ctx(None);
        let stage = AnnounceStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_skips_disabled_discord() {
        let announce = AnnounceConfig {
            discord: Some(AnnounceProviderConfig {
                enabled: Some(false),
                webhook_url: Some("https://discord.invalid/webhook".to_string()),
                message_template: None,
            }),
            slack: None,
            webhook: None,
            ..Default::default()
        };
        let mut ctx = make_ctx(Some(announce));
        // Should complete without attempting network I/O.
        assert!(AnnounceStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_skips_disabled_slack() {
        let announce = AnnounceConfig {
            discord: None,
            slack: Some(AnnounceProviderConfig {
                enabled: Some(false),
                webhook_url: Some("https://hooks.slack.invalid/services/T000".to_string()),
                message_template: None,
            }),
            webhook: None,
            ..Default::default()
        };
        let mut ctx = make_ctx(Some(announce));
        assert!(AnnounceStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_skips_disabled_webhook() {
        let announce = AnnounceConfig {
            discord: None,
            slack: None,
            webhook: Some(WebhookConfig {
                enabled: Some(false),
                endpoint_url: Some("https://example.invalid/hook".to_string()),
                headers: None,
                content_type: None,
                message_template: None,
            }),
            ..Default::default()
        };
        let mut ctx = make_ctx(Some(announce));
        assert!(AnnounceStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_dry_run_discord_does_not_send() {
        let announce = AnnounceConfig {
            discord: Some(AnnounceProviderConfig {
                enabled: Some(true),
                webhook_url: Some("https://discord.invalid/webhook".to_string()),
                message_template: Some("{{ .ProjectName }} {{ .Tag }} released!".to_string()),
            }),
            slack: None,
            webhook: None,
            ..Default::default()
        };
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.announce = Some(announce);
        let opts = ContextOptions {
            dry_run: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        // Should not make a network call (URL is `.invalid`), just log.
        assert!(AnnounceStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_dry_run_slack_does_not_send() {
        let announce = AnnounceConfig {
            discord: None,
            slack: Some(AnnounceProviderConfig {
                enabled: Some(true),
                webhook_url: Some("https://hooks.slack.invalid/services/T000".to_string()),
                message_template: Some("{{ .ProjectName }} {{ .Tag }} released!".to_string()),
            }),
            webhook: None,
            ..Default::default()
        };
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.announce = Some(announce);
        let opts = ContextOptions {
            dry_run: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        assert!(AnnounceStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_dry_run_webhook_does_not_send() {
        let announce = AnnounceConfig {
            discord: None,
            slack: None,
            webhook: Some(WebhookConfig {
                enabled: Some(true),
                endpoint_url: Some("https://example.invalid/hook".to_string()),
                headers: None,
                content_type: Some("application/json".to_string()),
                message_template: Some("{{ .ProjectName }} {{ .Tag }} released!".to_string()),
            }),
            ..Default::default()
        };
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.announce = Some(announce);
        let opts = ContextOptions {
            dry_run: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        assert!(AnnounceStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_missing_webhook_url_returns_error() {
        let announce = AnnounceConfig {
            discord: Some(AnnounceProviderConfig {
                enabled: Some(true),
                webhook_url: None, // intentionally missing
                message_template: None,
            }),
            slack: None,
            webhook: None,
            ..Default::default()
        };
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.announce = Some(announce);
        let opts = ContextOptions {
            dry_run: false,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        assert!(AnnounceStage.run(&mut ctx).is_err());
    }

    // ----------------------------------------------------------------
    // Telegram tests
    // ----------------------------------------------------------------

    #[test]
    fn test_skips_disabled_telegram() {
        let announce = AnnounceConfig {
            telegram: Some(TelegramAnnounce {
                enabled: Some(false),
                bot_token: Some("123:ABC".to_string()),
                chat_id: Some("-100123".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = make_ctx(Some(announce));
        assert!(AnnounceStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_dry_run_telegram_does_not_send() {
        let announce = AnnounceConfig {
            telegram: Some(TelegramAnnounce {
                enabled: Some(true),
                bot_token: Some("123:ABC".to_string()),
                chat_id: Some("-100123".to_string()),
                message_template: Some("{{ .ProjectName }} {{ .Tag }} released!".to_string()),
                parse_mode: Some("MarkdownV2".to_string()),
            }),
            ..Default::default()
        };
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.announce = Some(announce);
        let opts = ContextOptions {
            dry_run: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        assert!(AnnounceStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_missing_telegram_bot_token_returns_error() {
        let announce = AnnounceConfig {
            telegram: Some(TelegramAnnounce {
                enabled: Some(true),
                bot_token: None,
                chat_id: Some("-100123".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.announce = Some(announce);
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        assert!(AnnounceStage.run(&mut ctx).is_err());
    }

    #[test]
    fn test_missing_telegram_chat_id_returns_error() {
        let announce = AnnounceConfig {
            telegram: Some(TelegramAnnounce {
                enabled: Some(true),
                bot_token: Some("123:ABC".to_string()),
                chat_id: None,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.announce = Some(announce);
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        assert!(AnnounceStage.run(&mut ctx).is_err());
    }

    // ----------------------------------------------------------------
    // Teams tests
    // ----------------------------------------------------------------

    #[test]
    fn test_skips_disabled_teams() {
        let announce = AnnounceConfig {
            teams: Some(TeamsAnnounce {
                enabled: Some(false),
                webhook_url: Some("https://teams.invalid/webhook".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = make_ctx(Some(announce));
        assert!(AnnounceStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_dry_run_teams_does_not_send() {
        let announce = AnnounceConfig {
            teams: Some(TeamsAnnounce {
                enabled: Some(true),
                webhook_url: Some("https://teams.invalid/webhook".to_string()),
                message_template: Some("{{ .ProjectName }} {{ .Tag }} released!".to_string()),
            }),
            ..Default::default()
        };
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.announce = Some(announce);
        let opts = ContextOptions {
            dry_run: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        assert!(AnnounceStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_missing_teams_webhook_url_returns_error() {
        let announce = AnnounceConfig {
            teams: Some(TeamsAnnounce {
                enabled: Some(true),
                webhook_url: None,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.announce = Some(announce);
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        assert!(AnnounceStage.run(&mut ctx).is_err());
    }

    // ----------------------------------------------------------------
    // Mattermost tests
    // ----------------------------------------------------------------

    #[test]
    fn test_skips_disabled_mattermost() {
        let announce = AnnounceConfig {
            mattermost: Some(MattermostAnnounce {
                enabled: Some(false),
                webhook_url: Some("https://mm.invalid/hooks/xxx".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = make_ctx(Some(announce));
        assert!(AnnounceStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_dry_run_mattermost_does_not_send() {
        let announce = AnnounceConfig {
            mattermost: Some(MattermostAnnounce {
                enabled: Some(true),
                webhook_url: Some("https://mm.invalid/hooks/xxx".to_string()),
                channel: Some("releases".to_string()),
                username: Some("release-bot".to_string()),
                icon_url: Some("https://example.com/icon.png".to_string()),
                message_template: Some("{{ .ProjectName }} {{ .Tag }} released!".to_string()),
            }),
            ..Default::default()
        };
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.announce = Some(announce);
        let opts = ContextOptions {
            dry_run: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        assert!(AnnounceStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_missing_mattermost_webhook_url_returns_error() {
        let announce = AnnounceConfig {
            mattermost: Some(MattermostAnnounce {
                enabled: Some(true),
                webhook_url: None,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.announce = Some(announce);
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        assert!(AnnounceStage.run(&mut ctx).is_err());
    }

    // ----------------------------------------------------------------
    // Email tests
    // ----------------------------------------------------------------

    #[test]
    fn test_skips_disabled_email() {
        let announce = AnnounceConfig {
            email: Some(EmailAnnounce {
                enabled: Some(false),
                from: Some("bot@example.com".to_string()),
                to: vec!["dev@example.com".to_string()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = make_ctx(Some(announce));
        assert!(AnnounceStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_dry_run_email_does_not_send() {
        let announce = AnnounceConfig {
            email: Some(EmailAnnounce {
                enabled: Some(true),
                from: Some("bot@example.com".to_string()),
                to: vec!["dev@example.com".to_string()],
                subject_template: Some("{{ .ProjectName }} {{ .Tag }} released".to_string()),
                message_template: Some("New release!".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.announce = Some(announce);
        let opts = ContextOptions {
            dry_run: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        assert!(AnnounceStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_missing_email_from_returns_error() {
        let announce = AnnounceConfig {
            email: Some(EmailAnnounce {
                enabled: Some(true),
                from: None,
                to: vec!["dev@example.com".to_string()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.announce = Some(announce);
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        assert!(AnnounceStage.run(&mut ctx).is_err());
    }

    #[test]
    fn test_missing_email_to_returns_error() {
        let announce = AnnounceConfig {
            email: Some(EmailAnnounce {
                enabled: Some(true),
                from: Some("bot@example.com".to_string()),
                to: vec![],
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.announce = Some(announce);
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        assert!(AnnounceStage.run(&mut ctx).is_err());
    }

    #[test]
    fn test_invalid_email_from_returns_error() {
        let announce = AnnounceConfig {
            email: Some(EmailAnnounce {
                enabled: Some(true),
                from: Some("not-an-email".to_string()),
                to: vec!["dev@example.com".to_string()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.announce = Some(announce);
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        let err = AnnounceStage.run(&mut ctx).unwrap_err();
        assert!(
            err.to_string().contains("missing @"),
            "expected 'missing @' error, got: {err}"
        );
    }
}
