use std::collections::HashMap;

use anodize_core::context::Context;
use anodize_core::stage::Stage;
use anyhow::Result;

pub mod discord;
pub mod email;
mod http;
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
    let log = ctx.logger("announce");
    if ctx.is_dry_run() {
        log.status(&format!("(dry-run) {provider}: {log_line}"));
    } else {
        log.status(&format!("{provider}: {log_line}"));
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
        let log = ctx.logger("announce");
        let announce = match ctx.config.announce.clone() {
            Some(a) => a,
            None => {
                log.status("no announce config — skipping");
                return Ok(());
            }
        };

        // Evaluate template-conditional skip.
        if let Some(ref skip_val) = announce.skip {
            let should_skip = skip_val.is_disabled(|tmpl| ctx.render_template(tmpl));
            if should_skip {
                log.status("announce.skip evaluated to true — skipping");
                return Ok(());
            }
        }

        // ----------------------------------------------------------------
        // Discord
        // ----------------------------------------------------------------
        if let Some(cfg) = &announce.discord
            && cfg.enabled.unwrap_or(false)
        {
            let url = require_rendered(ctx, cfg.webhook_url.as_deref(), "discord", "webhook_url")?;
            let message = render_message(ctx, cfg.message_template.as_deref())?;
            let author = render_optional(ctx, cfg.author.as_deref())?;
            let color = cfg.color;
            let icon_url = render_optional(ctx, cfg.icon_url.as_deref())?;
            let opts = discord::DiscordOptions {
                author: author.as_deref(),
                color,
                icon_url: icon_url.as_deref(),
            };
            dispatch(ctx, "discord", &message, || {
                discord::send_discord(&url, &message, &opts)
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
            let channel = render_optional(ctx, cfg.channel.as_deref())?;
            let username = render_optional(ctx, cfg.username.as_deref())?;
            let icon_emoji = cfg.icon_emoji.clone();
            let icon_url = cfg.icon_url.clone();
            let blocks = cfg.blocks.clone();
            let attachments = cfg.attachments.clone();
            dispatch(ctx, "slack", &message, || {
                let opts = slack::SlackOptions {
                    channel: channel.as_deref(),
                    username: username.as_deref(),
                    icon_emoji: icon_emoji.as_deref(),
                    icon_url: icon_url.as_deref(),
                    blocks: blocks.as_ref(),
                    attachments: attachments.as_ref(),
                };
                slack::send_slack(&url, &message, &opts)
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

            let skip_tls = cfg.skip_tls_verify.unwrap_or(false);
            let expected_codes = if cfg.expected_status_codes.is_empty() {
                webhook::default_expected_status_codes()
            } else {
                cfg.expected_status_codes.clone()
            };
            dispatch(ctx, "webhook", &message, || {
                webhook::send_webhook(&url, &message, &headers, &content_type, skip_tls, &expected_codes)
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
            // Default parse_mode to "MarkdownV2" to match GoReleaser behaviour.
            let parse_mode_raw = cfg
                .parse_mode
                .as_deref()
                .or(Some("MarkdownV2"));
            let parse_mode = render_optional(ctx, parse_mode_raw)?;
            let message_thread_id = cfg.message_thread_id;

            dispatch(ctx, "telegram", &message, || {
                telegram::send_telegram(
                    &bot_token,
                    &chat_id,
                    &message,
                    parse_mode.as_deref(),
                    message_thread_id,
                )
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
            let title = render_optional(ctx, cfg.title_template.as_deref())?;
            let color = cfg.color.clone();
            let icon_url = render_optional(ctx, cfg.icon_url.as_deref())?;
            let opts = teams::TeamsOptions {
                title: title.as_deref(),
                color: color.as_deref(),
                icon_url: icon_url.as_deref(),
            };
            dispatch(ctx, "teams", &message, || {
                teams::send_teams(&url, &message, &opts)
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
            let icon_emoji = render_optional(ctx, cfg.icon_emoji.as_deref())?;
            let color = cfg.color.clone();
            let title = render_optional(ctx, cfg.title_template.as_deref())?;

            let opts = mattermost::MattermostOptions {
                channel: channel.as_deref(),
                username: username.as_deref(),
                icon_url: icon_url.as_deref(),
                icon_emoji: icon_emoji.as_deref(),
                color: color.as_deref(),
                title: title.as_deref(),
            };
            dispatch(ctx, "mattermost", &message, || {
                mattermost::send_mattermost(&url, &message, &opts)
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
        AnnounceConfig, Config, DiscordAnnounce, EmailAnnounce, MattermostAnnounce, SlackAnnounce,
        StringOrBool, TeamsAnnounce, TelegramAnnounce, WebhookConfig,
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
            discord: Some(DiscordAnnounce {
                enabled: Some(false),
                webhook_url: Some("https://discord.invalid/webhook".to_string()),
                message_template: None,
                ..Default::default()
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
            slack: Some(SlackAnnounce {
                enabled: Some(false),
                webhook_url: Some("https://hooks.slack.invalid/services/T000".to_string()),
                ..Default::default()
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
                skip_tls_verify: None,
                expected_status_codes: vec![],
            }),
            ..Default::default()
        };
        let mut ctx = make_ctx(Some(announce));
        assert!(AnnounceStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_dry_run_discord_does_not_send() {
        let announce = AnnounceConfig {
            discord: Some(DiscordAnnounce {
                enabled: Some(true),
                webhook_url: Some("https://discord.invalid/webhook".to_string()),
                message_template: Some("{{ .ProjectName }} {{ .Tag }} released!".to_string()),
                ..Default::default()
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
            slack: Some(SlackAnnounce {
                enabled: Some(true),
                webhook_url: Some("https://hooks.slack.invalid/services/T000".to_string()),
                message_template: Some("{{ .ProjectName }} {{ .Tag }} released!".to_string()),
                ..Default::default()
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
                skip_tls_verify: None,
                expected_status_codes: vec![],
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
            discord: Some(DiscordAnnounce {
                enabled: Some(true),
                webhook_url: None, // intentionally missing
                message_template: None,
                ..Default::default()
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
                message_thread_id: None,
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
                icon_emoji: None,
                color: None,
                message_template: Some("{{ .ProjectName }} {{ .Tag }} released!".to_string()),
                title_template: None,
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

    // ----------------------------------------------------------------
    // Config struct field tests
    // ----------------------------------------------------------------

    #[test]
    fn test_discord_announce_new_fields() {
        let cfg = DiscordAnnounce {
            enabled: Some(true),
            webhook_url: Some("https://discord.com/api/webhooks/123/abc".to_string()),
            message_template: None,
            author: Some("release-bot".to_string()),
            color: Some(16711680),
            icon_url: Some("https://example.com/icon.png".to_string()),
        };
        assert_eq!(cfg.author.as_deref(), Some("release-bot"));
        assert_eq!(cfg.color, Some(16711680));
        assert_eq!(cfg.icon_url.as_deref(), Some("https://example.com/icon.png"));
    }

    #[test]
    fn test_webhook_skip_tls_verify_field() {
        let cfg = WebhookConfig {
            enabled: Some(true),
            endpoint_url: Some("https://internal.example.com/hook".to_string()),
            skip_tls_verify: Some(true),
            ..Default::default()
        };
        assert_eq!(cfg.skip_tls_verify, Some(true));
    }

    #[test]
    fn test_telegram_message_thread_id_field() {
        let cfg = TelegramAnnounce {
            enabled: Some(true),
            bot_token: Some("123:ABC".to_string()),
            chat_id: Some("-100123".to_string()),
            message_thread_id: Some(42),
            ..Default::default()
        };
        assert_eq!(cfg.message_thread_id, Some(42));
    }

    #[test]
    fn test_teams_title_and_color_fields() {
        let cfg = TeamsAnnounce {
            enabled: Some(true),
            webhook_url: Some("https://teams.example.com/webhook".to_string()),
            title_template: Some("Release v1.0".to_string()),
            color: Some("0076D7".to_string()),
            ..Default::default()
        };
        assert_eq!(cfg.title_template.as_deref(), Some("Release v1.0"));
        assert_eq!(cfg.color.as_deref(), Some("0076D7"));
    }

    #[test]
    fn test_mattermost_icon_emoji_and_color_fields() {
        let cfg = MattermostAnnounce {
            enabled: Some(true),
            webhook_url: Some("https://mm.example.com/hooks/xxx".to_string()),
            icon_emoji: Some(":rocket:".to_string()),
            color: Some("#36a64f".to_string()),
            ..Default::default()
        };
        assert_eq!(cfg.icon_emoji.as_deref(), Some(":rocket:"));
        assert_eq!(cfg.color.as_deref(), Some("#36a64f"));
    }

    #[test]
    fn test_dry_run_telegram_defaults_parse_mode_to_markdownv2() {
        // When parse_mode is not explicitly set, it should default to "MarkdownV2".
        let announce = AnnounceConfig {
            telegram: Some(TelegramAnnounce {
                enabled: Some(true),
                bot_token: Some("123:ABC".to_string()),
                chat_id: Some("-100123".to_string()),
                message_template: Some("{{ .ProjectName }} released!".to_string()),
                parse_mode: None, // not set -- should default to MarkdownV2
                message_thread_id: None,
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
        // Should succeed in dry-run without error.
        assert!(AnnounceStage.run(&mut ctx).is_ok());
    }

    // ----------------------------------------------------------------
    // announce.skip tests
    // ----------------------------------------------------------------

    #[test]
    fn test_announce_skip_true_skips_all() {
        let announce = AnnounceConfig {
            skip: Some(StringOrBool::Bool(true)),
            discord: Some(DiscordAnnounce {
                enabled: Some(true),
                webhook_url: Some("https://discord.invalid/webhook".to_string()),
                message_template: Some("test".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = make_ctx(Some(announce));
        // Should succeed without attempting any provider (discord URL is invalid).
        assert!(AnnounceStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_announce_skip_false_does_not_skip() {
        let announce = AnnounceConfig {
            skip: Some(StringOrBool::Bool(false)),
            ..Default::default()
        };
        let mut ctx = make_ctx(Some(announce));
        assert!(AnnounceStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_announce_skip_template_evaluated() {
        let announce = AnnounceConfig {
            skip: Some(StringOrBool::String("{{ .IsNightly }}".to_string())),
            discord: Some(DiscordAnnounce {
                enabled: Some(true),
                webhook_url: Some("https://discord.invalid/webhook".to_string()),
                message_template: Some("test".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.announce = Some(announce);
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set("IsNightly", "true");
        // Should skip because IsNightly renders to "true".
        // Discord would fail on the invalid URL if skip didn't work.
        assert!(AnnounceStage.run(&mut ctx).is_ok());
    }
}
