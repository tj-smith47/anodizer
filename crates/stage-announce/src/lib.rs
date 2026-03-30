use std::collections::HashMap;

use anodize_core::context::Context;
use anodize_core::stage::Stage;
use anyhow::Result;

pub mod bluesky;
pub mod discord;
pub mod discourse;
pub mod email;
mod http;
pub mod linkedin;
pub mod mastodon;
pub mod mattermost;
pub mod opencollective;
pub mod reddit;
pub mod slack;
pub mod teams;
pub mod telegram;
pub mod twitter;
pub mod webhook;

// ---------------------------------------------------------------------------
// Shared helpers to reduce boilerplate across providers
// ---------------------------------------------------------------------------

const DEFAULT_MESSAGE_TEMPLATE: &str = "{{ .ProjectName }} {{ .Tag }} is out! Check it out at {{ .ReleaseURL }}";

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

/// Render template variables inside a `serde_json::Value` by serializing to
/// string, running through the template engine, then parsing back.
fn render_json_template(
    ctx: &Context,
    val: Option<&serde_json::Value>,
) -> Result<Option<serde_json::Value>> {
    match val {
        Some(v) => {
            let rendered = ctx.render_template(&v.to_string())?;
            Ok(Some(serde_json::from_str(&rendered)?))
        }
        None => Ok(None),
    }
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
        // Discourse
        // ----------------------------------------------------------------
        if let Some(cfg) = &announce.discourse
            && cfg.enabled.unwrap_or(false)
        {
            let server = require_rendered(ctx, cfg.server.as_deref(), "discourse", "server")?;
            if server.is_empty() {
                anyhow::bail!("announce.discourse: server must not be empty");
            }
            let category_id = cfg.category_id.ok_or_else(|| {
                anyhow::anyhow!("announce.discourse: missing category_id")
            })?;
            if category_id == 0 {
                anyhow::bail!("announce.discourse: category_id must be non-zero");
            }
            let username = cfg.username.as_deref().unwrap_or("system");
            let title = ctx.render_template(
                cfg.title_template.as_deref().unwrap_or("{{ .ProjectName }} {{ .Tag }} is out!"),
            )?;
            let message = render_message(ctx, cfg.message_template.as_deref())?;
            let api_key = std::env::var("DISCOURSE_API_KEY")
                .map_err(|_| anyhow::anyhow!("announce.discourse: DISCOURSE_API_KEY env var is required"))?;
            if api_key.is_empty() {
                anyhow::bail!("announce.discourse: DISCOURSE_API_KEY env var must not be empty");
            }

            dispatch(ctx, "discourse", &title, || {
                discourse::send_discourse(&server, &api_key, username, category_id, &title, &message)
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
            // Convert typed blocks/attachments to serde_json::Value for template rendering
            let blocks_val = cfg.blocks.as_ref()
                .map(|b| serde_json::to_value(b))
                .transpose()?;
            let blocks = render_json_template(ctx, blocks_val.as_ref())?;
            let attachments_val = cfg.attachments.as_ref()
                .map(|a| serde_json::to_value(a))
                .transpose()?;
            let attachments = render_json_template(ctx, attachments_val.as_ref())?;
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
        // Reddit
        // ----------------------------------------------------------------
        if let Some(cfg) = &announce.reddit
            && cfg.enabled.unwrap_or(false)
        {
            let app_id =
                require_rendered(ctx, cfg.application_id.as_deref(), "reddit", "application_id")?;
            let username =
                require_rendered(ctx, cfg.username.as_deref(), "reddit", "username")?;
            let sub = require_rendered(ctx, cfg.sub.as_deref(), "reddit", "sub")?;
            let title = ctx.render_template(
                cfg.title_template
                    .as_deref()
                    .unwrap_or("{{ .ProjectName }} {{ .Tag }} is out!"),
            )?;
            let url = ctx.render_template(
                cfg.url_template
                    .as_deref()
                    .unwrap_or("{{ .ReleaseURL }}"),
            )?;
            let secret = std::env::var("REDDIT_SECRET").map_err(|_| {
                anyhow::anyhow!("announce.reddit: REDDIT_SECRET env var is required")
            })?;
            if secret.is_empty() {
                anyhow::bail!("announce.reddit: REDDIT_SECRET env var must not be empty");
            }
            let password = std::env::var("REDDIT_PASSWORD").map_err(|_| {
                anyhow::anyhow!("announce.reddit: REDDIT_PASSWORD env var is required")
            })?;
            if password.is_empty() {
                anyhow::bail!("announce.reddit: REDDIT_PASSWORD env var must not be empty");
            }

            dispatch(ctx, "reddit", &format!("r/{sub}: {title}"), || {
                reddit::send_reddit(&app_id, &secret, &username, &password, &sub, &title, &url)
            })?;
        }

        // ----------------------------------------------------------------
        // Twitter/X
        // ----------------------------------------------------------------
        if let Some(cfg) = &announce.twitter
            && cfg.enabled.unwrap_or(false)
        {
            let message = render_message(ctx, cfg.message_template.as_deref())?;
            let consumer_key = std::env::var("TWITTER_CONSUMER_KEY").map_err(|_| {
                anyhow::anyhow!("announce.twitter: TWITTER_CONSUMER_KEY env var is required")
            })?;
            let consumer_secret = std::env::var("TWITTER_CONSUMER_SECRET").map_err(|_| {
                anyhow::anyhow!("announce.twitter: TWITTER_CONSUMER_SECRET env var is required")
            })?;
            let access_token = std::env::var("TWITTER_ACCESS_TOKEN").map_err(|_| {
                anyhow::anyhow!("announce.twitter: TWITTER_ACCESS_TOKEN env var is required")
            })?;
            let access_token_secret =
                std::env::var("TWITTER_ACCESS_TOKEN_SECRET").map_err(|_| {
                    anyhow::anyhow!(
                        "announce.twitter: TWITTER_ACCESS_TOKEN_SECRET env var is required"
                    )
                })?;

            dispatch(ctx, "twitter", &message, || {
                twitter::send_twitter(
                    &consumer_key,
                    &consumer_secret,
                    &access_token,
                    &access_token_secret,
                    &message,
                )
            })?;
        }

        // ----------------------------------------------------------------
        // Mastodon
        // ----------------------------------------------------------------
        if let Some(cfg) = &announce.mastodon
            && cfg.enabled.unwrap_or(false)
        {
            let server = require_rendered(ctx, cfg.server.as_deref(), "mastodon", "server")?;
            if server.is_empty() {
                // GoReleaser skips silently when server is empty
                let log = ctx.logger("announce");
                log.status("mastodon: server is empty — skipping");
            } else {
                let message = render_message(ctx, cfg.message_template.as_deref())?;
                // GoReleaser requires all three env vars even though only access_token is used
                let _client_id = std::env::var("MASTODON_CLIENT_ID").map_err(|_| {
                    anyhow::anyhow!("announce.mastodon: MASTODON_CLIENT_ID env var is required")
                })?;
                let _client_secret = std::env::var("MASTODON_CLIENT_SECRET").map_err(|_| {
                    anyhow::anyhow!(
                        "announce.mastodon: MASTODON_CLIENT_SECRET env var is required"
                    )
                })?;
                let access_token = std::env::var("MASTODON_ACCESS_TOKEN").map_err(|_| {
                    anyhow::anyhow!(
                        "announce.mastodon: MASTODON_ACCESS_TOKEN env var is required"
                    )
                })?;
                dispatch(ctx, "mastodon", &message, || {
                    mastodon::send_mastodon(&server, &access_token, &message)
                })?;
            }
        }

        // ----------------------------------------------------------------
        // Bluesky
        // ----------------------------------------------------------------
        if let Some(cfg) = &announce.bluesky
            && cfg.enabled.unwrap_or(false)
        {
            let username = require_rendered(ctx, cfg.username.as_deref(), "bluesky", "username")?;
            let message = render_message(ctx, cfg.message_template.as_deref())?;
            let app_password = std::env::var("BLUESKY_APP_PASSWORD").map_err(|_| {
                anyhow::anyhow!("announce.bluesky: BLUESKY_APP_PASSWORD env var is required")
            })?;
            if app_password.is_empty() {
                anyhow::bail!("announce.bluesky: BLUESKY_APP_PASSWORD env var must not be empty");
            }
            let release_url = ctx
                .template_vars()
                .get("ReleaseURL")
                .map(|s| s.to_string());

            dispatch(ctx, "bluesky", &message, || {
                bluesky::send_bluesky(&username, &app_password, &message, release_url.as_deref())
            })?;
        }

        // ----------------------------------------------------------------
        // LinkedIn
        // ----------------------------------------------------------------
        if let Some(cfg) = &announce.linkedin
            && cfg.enabled.unwrap_or(false)
        {
            let message = render_message(ctx, cfg.message_template.as_deref())?;
            let access_token = std::env::var("LINKEDIN_ACCESS_TOKEN").map_err(|_| {
                anyhow::anyhow!(
                    "announce.linkedin: LINKEDIN_ACCESS_TOKEN env var is required"
                )
            })?;
            if access_token.is_empty() {
                anyhow::bail!(
                    "announce.linkedin: LINKEDIN_ACCESS_TOKEN env var must not be empty"
                );
            }
            dispatch(ctx, "linkedin", &message, || {
                linkedin::send_linkedin(&access_token, &message)
            })?;
        }

        // ----------------------------------------------------------------
        // OpenCollective
        // ----------------------------------------------------------------
        if let Some(cfg) = &announce.opencollective
            && cfg.enabled.unwrap_or(false)
        {
            let slug = require_rendered(ctx, cfg.slug.as_deref(), "opencollective", "slug")?;
            if slug.is_empty() {
                let log = ctx.logger("announce");
                log.status("opencollective: slug is empty — skipping");
            } else {
                let title = ctx.render_template(
                    cfg.title_template
                        .as_deref()
                        .unwrap_or(opencollective::DEFAULT_TITLE_TEMPLATE),
                )?;
                let html = ctx.render_template(
                    cfg.message_template
                        .as_deref()
                        .unwrap_or(opencollective::DEFAULT_MESSAGE_TEMPLATE),
                )?;
                let token = std::env::var("OPENCOLLECTIVE_TOKEN").map_err(|_| {
                    anyhow::anyhow!(
                        "announce.opencollective: OPENCOLLECTIVE_TOKEN env var is required"
                    )
                })?;
                if token.is_empty() {
                    anyhow::bail!(
                        "announce.opencollective: OPENCOLLECTIVE_TOKEN env var must not be empty"
                    );
                }
                dispatch(ctx, "opencollective", &title, || {
                    opencollective::send_opencollective(&token, &slug, &title, &html)
                })?;
            }
        }

        // ----------------------------------------------------------------
        // Email (SMTP or sendmail/msmtp fallback)
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

            let email_params = email::EmailParams {
                from: &from,
                to: &cfg.to,
                subject: &subject,
                body: &body,
            };
            let log_line = format!("to {}: {}", cfg.to.join(", "), subject);

            if let Some(host) = &cfg.host {
                // SMTP transport
                let smtp_username = cfg
                    .username
                    .clone()
                    .or_else(|| std::env::var("SMTP_USERNAME").ok())
                    .unwrap_or_default();
                let smtp_password = std::env::var("SMTP_PASSWORD").map_err(|_| {
                    anyhow::anyhow!(
                        "announce.email: SMTP_PASSWORD env var is required for SMTP transport"
                    )
                })?;
                let port = cfg.port.unwrap_or(587);
                let insecure = cfg.insecure_skip_verify.unwrap_or(false);

                let smtp_params = email::SmtpParams {
                    host,
                    port,
                    username: &smtp_username,
                    password: &smtp_password,
                    insecure_skip_verify: insecure,
                };
                dispatch(ctx, "email (smtp)", &log_line, || {
                    email::send_smtp(&email_params, &smtp_params)
                })?;
            } else {
                // Sendmail fallback
                dispatch(ctx, "email", &log_line, || {
                    email::send_sendmail(&email_params)
                })?;
            }
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
        AnnounceConfig, BlueskyAnnounce, Config, DiscordAnnounce, DiscourseAnnounce,
        EmailAnnounce, LinkedInAnnounce, MastodonAnnounce, MattermostAnnounce,
        OpenCollectiveAnnounce, RedditAnnounce, SlackAnnounce, SlackBlock, SlackTextObject,
        StringOrBool, TeamsAnnounce, TelegramAnnounce, TwitterAnnounce, WebhookConfig,
    };
    use anodize_core::context::{Context, ContextOptions};
    use serial_test::serial;

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
    fn test_slack_blocks_template_rendering() {
        let blocks = vec![SlackBlock {
            block_type: "section".to_string(),
            text: Some(SlackTextObject {
                text_type: "mrkdwn".to_string(),
                text: "{{ .ProjectName }} {{ .Tag }} is out!".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }];
        let announce = AnnounceConfig {
            slack: Some(SlackAnnounce {
                enabled: Some(true),
                webhook_url: Some("https://hooks.slack.invalid/services/T000".to_string()),
                message_template: None,
                blocks: Some(blocks),
                attachments: None,
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
        ctx.template_vars_mut().set("Tag", "v2.0.0");
        ctx.template_vars_mut().set(
            "ReleaseURL",
            "https://github.com/org/myapp/releases/tag/v2.0.0",
        );
        assert!(AnnounceStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_slack_blocks_template_vars_are_expanded() {
        let blocks = vec![SlackBlock {
            block_type: "section".to_string(),
            text: Some(SlackTextObject {
                text_type: "mrkdwn".to_string(),
                text: "{{ .ProjectName }} {{ .Tag }} is out!".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }];
        let blocks_json = serde_json::to_value(&blocks).unwrap();
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v2.0.0");
        // Use the same render_json_template helper
        let rendered = render_json_template(&ctx, Some(&blocks_json)).unwrap().unwrap();
        assert_eq!(rendered[0]["text"]["text"], "myapp v2.0.0 is out!");
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

    // ----------------------------------------------------------------
    // Slack typed blocks YAML deserialization test
    // ----------------------------------------------------------------

    #[test]
    fn test_slack_blocks_yaml_deserialization() {
        let yaml = r#"
blocks:
  - type: header
    text:
      type: plain_text
      text: "{{ .ProjectName }} {{ .Tag }} released!"
  - type: section
    text:
      type: mrkdwn
      text: ":github:  <{{ .ReleaseURL }}|Go to Github Release>  :rocket:"
"#;
        #[derive(serde::Deserialize)]
        struct TestConfig {
            blocks: Vec<SlackBlock>,
        }
        let config: TestConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.blocks.len(), 2);
        assert_eq!(config.blocks[0].block_type, "header");
        assert_eq!(
            config.blocks[0].text.as_ref().unwrap().text_type,
            "plain_text"
        );
        assert_eq!(config.blocks[1].block_type, "section");
        assert_eq!(
            config.blocks[1].text.as_ref().unwrap().text_type,
            "mrkdwn"
        );
    }

    // ----------------------------------------------------------------
    // Reddit tests
    // ----------------------------------------------------------------

    #[test]
    fn test_skips_disabled_reddit() {
        let announce = AnnounceConfig {
            reddit: Some(RedditAnnounce {
                enabled: Some(false),
                application_id: Some("app123".to_string()),
                username: Some("testuser".to_string()),
                sub: Some("rust".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = make_ctx(Some(announce));
        assert!(AnnounceStage.run(&mut ctx).is_ok());
    }

    #[test]
    #[serial]
    fn test_dry_run_reddit() {
        unsafe { std::env::set_var("REDDIT_SECRET", "testsecret") };
        unsafe { std::env::set_var("REDDIT_PASSWORD", "testpass") };
        let announce = AnnounceConfig {
            reddit: Some(RedditAnnounce {
                enabled: Some(true),
                application_id: Some("app123".to_string()),
                username: Some("testuser".to_string()),
                sub: Some("rust".to_string()),
                title_template: Some("{{ .ProjectName }} {{ .Tag }}".to_string()),
                url_template: Some("{{ .ReleaseURL }}".to_string()),
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
        ctx.template_vars_mut()
            .set("ReleaseURL", "https://github.com/org/myapp/releases/tag/v1.0.0");
        assert!(AnnounceStage.run(&mut ctx).is_ok());
        unsafe { std::env::remove_var("REDDIT_SECRET") };
        unsafe { std::env::remove_var("REDDIT_PASSWORD") };
    }

    // ----------------------------------------------------------------
    // Twitter tests
    // ----------------------------------------------------------------

    #[test]
    fn test_skips_disabled_twitter() {
        let announce = AnnounceConfig {
            twitter: Some(TwitterAnnounce {
                enabled: Some(false),
                message_template: None,
            }),
            ..Default::default()
        };
        let mut ctx = make_ctx(Some(announce));
        assert!(AnnounceStage.run(&mut ctx).is_ok());
    }

    #[test]
    #[serial]
    fn test_dry_run_twitter() {
        unsafe { std::env::set_var("TWITTER_CONSUMER_KEY", "ck") };
        unsafe { std::env::set_var("TWITTER_CONSUMER_SECRET", "cs") };
        unsafe { std::env::set_var("TWITTER_ACCESS_TOKEN", "at") };
        unsafe { std::env::set_var("TWITTER_ACCESS_TOKEN_SECRET", "ats") };
        let announce = AnnounceConfig {
            twitter: Some(TwitterAnnounce {
                enabled: Some(true),
                message_template: Some(
                    "{{ .ProjectName }} {{ .Tag }} is out! Check it out at {{ .ReleaseURL }}"
                        .to_string(),
                ),
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
        ctx.template_vars_mut().set(
            "ReleaseURL",
            "https://github.com/org/myapp/releases/tag/v1.0.0",
        );
        assert!(AnnounceStage.run(&mut ctx).is_ok());
        unsafe { std::env::remove_var("TWITTER_CONSUMER_KEY") };
        unsafe { std::env::remove_var("TWITTER_CONSUMER_SECRET") };
        unsafe { std::env::remove_var("TWITTER_ACCESS_TOKEN") };
        unsafe { std::env::remove_var("TWITTER_ACCESS_TOKEN_SECRET") };
    }

    #[test]
    #[serial]
    fn test_twitter_missing_env_var_returns_error() {
        // Ensure env vars are not set
        unsafe { std::env::remove_var("TWITTER_CONSUMER_KEY") };
        unsafe { std::env::remove_var("TWITTER_CONSUMER_SECRET") };
        unsafe { std::env::remove_var("TWITTER_ACCESS_TOKEN") };
        unsafe { std::env::remove_var("TWITTER_ACCESS_TOKEN_SECRET") };
        let announce = AnnounceConfig {
            twitter: Some(TwitterAnnounce {
                enabled: Some(true),
                message_template: None,
            }),
            ..Default::default()
        };
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.announce = Some(announce);
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set(
            "ReleaseURL",
            "https://github.com/org/myapp/releases/tag/v1.0.0",
        );
        let err = AnnounceStage.run(&mut ctx).unwrap_err();
        assert!(
            err.to_string().contains("TWITTER_CONSUMER_KEY"),
            "expected TWITTER_CONSUMER_KEY error, got: {err}"
        );
    }

    // ----------------------------------------------------------------
    // Mastodon tests
    // ----------------------------------------------------------------

    #[test]
    fn test_skips_disabled_mastodon() {
        let announce = AnnounceConfig {
            mastodon: Some(MastodonAnnounce {
                enabled: Some(false),
                server: Some("https://mastodon.social".to_string()),
                message_template: None,
            }),
            ..Default::default()
        };
        let mut ctx = make_ctx(Some(announce));
        assert!(AnnounceStage.run(&mut ctx).is_ok());
    }

    #[test]
    #[serial]
    fn test_dry_run_mastodon() {
        unsafe { std::env::set_var("MASTODON_CLIENT_ID", "test_id") };
        unsafe { std::env::set_var("MASTODON_CLIENT_SECRET", "test_secret") };
        unsafe { std::env::set_var("MASTODON_ACCESS_TOKEN", "test-token") };
        let announce = AnnounceConfig {
            mastodon: Some(MastodonAnnounce {
                enabled: Some(true),
                server: Some("https://mastodon.social".to_string()),
                message_template: Some(
                    "{{ .ProjectName }} {{ .Tag }} is out! Check it out at {{ .ReleaseURL }}"
                        .to_string(),
                ),
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
        ctx.template_vars_mut().set(
            "ReleaseURL",
            "https://github.com/org/myapp/releases/tag/v1.0.0",
        );
        assert!(AnnounceStage.run(&mut ctx).is_ok());
        unsafe { std::env::remove_var("MASTODON_CLIENT_ID") };
        unsafe { std::env::remove_var("MASTODON_CLIENT_SECRET") };
        unsafe { std::env::remove_var("MASTODON_ACCESS_TOKEN") };
    }

    #[test]
    #[serial]
    fn test_mastodon_missing_server_returns_error() {
        unsafe { std::env::set_var("MASTODON_ACCESS_TOKEN", "test-token") };
        let announce = AnnounceConfig {
            mastodon: Some(MastodonAnnounce {
                enabled: Some(true),
                server: None,
                message_template: None,
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
            err.to_string().contains("missing server"),
            "expected 'missing server' error, got: {err}"
        );
        unsafe { std::env::remove_var("MASTODON_ACCESS_TOKEN") };
    }

    #[test]
    #[serial]
    fn test_mastodon_missing_env_var_returns_error() {
        unsafe { std::env::remove_var("MASTODON_CLIENT_ID") };
        unsafe { std::env::remove_var("MASTODON_CLIENT_SECRET") };
        unsafe { std::env::remove_var("MASTODON_ACCESS_TOKEN") };
        let announce = AnnounceConfig {
            mastodon: Some(MastodonAnnounce {
                enabled: Some(true),
                server: Some("https://mastodon.social".to_string()),
                message_template: None,
            }),
            ..Default::default()
        };
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.announce = Some(announce);
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set(
            "ReleaseURL",
            "https://github.com/org/myapp/releases/tag/v1.0.0",
        );
        let err = AnnounceStage.run(&mut ctx).unwrap_err();
        // First missing env var is now MASTODON_CLIENT_ID
        assert!(
            err.to_string().contains("MASTODON_CLIENT_ID"),
            "expected MASTODON_CLIENT_ID error, got: {err}"
        );
    }

    #[test]
    #[serial]
    fn test_mastodon_empty_server_skips() {
        let announce = AnnounceConfig {
            mastodon: Some(MastodonAnnounce {
                enabled: Some(true),
                server: Some("".to_string()),
                message_template: None,
            }),
            ..Default::default()
        };
        let mut ctx = make_ctx(Some(announce));
        // Empty server should cause a silent skip, not an error
        assert!(AnnounceStage.run(&mut ctx).is_ok());
    }

    // ----------------------------------------------------------------
    // Bluesky tests
    // ----------------------------------------------------------------

    #[test]
    fn test_skips_disabled_bluesky() {
        let announce = AnnounceConfig {
            bluesky: Some(BlueskyAnnounce {
                enabled: Some(false),
                username: Some("user.bsky.social".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = make_ctx(Some(announce));
        assert!(AnnounceStage.run(&mut ctx).is_ok());
    }

    #[serial]
    #[test]
    fn test_dry_run_bluesky() {
        unsafe { std::env::set_var("BLUESKY_APP_PASSWORD", "test_pass") };
        let announce = AnnounceConfig {
            bluesky: Some(BlueskyAnnounce {
                enabled: Some(true),
                username: Some("user.bsky.social".to_string()),
                message_template: Some("{{ .ProjectName }} {{ .Tag }}".to_string()),
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
        ctx.template_vars_mut().set(
            "ReleaseURL",
            "https://github.com/org/myapp/releases/tag/v1.0.0",
        );
        assert!(AnnounceStage.run(&mut ctx).is_ok());
        unsafe { std::env::remove_var("BLUESKY_APP_PASSWORD") };
    }

    #[serial]
    #[test]
    fn test_bluesky_missing_username_errors() {
        unsafe { std::env::set_var("BLUESKY_APP_PASSWORD", "test_pass") };
        let announce = AnnounceConfig {
            bluesky: Some(BlueskyAnnounce {
                enabled: Some(true),
                username: None,
                message_template: None,
            }),
            ..Default::default()
        };
        let mut ctx = make_ctx(Some(announce));
        let err = AnnounceStage.run(&mut ctx).unwrap_err();
        assert!(
            err.to_string().contains("missing username"),
            "expected 'missing username' error, got: {err}"
        );
        unsafe { std::env::remove_var("BLUESKY_APP_PASSWORD") };
    }

    #[serial]
    #[test]
    fn test_bluesky_missing_env_var_errors() {
        unsafe { std::env::remove_var("BLUESKY_APP_PASSWORD") };
        let announce = AnnounceConfig {
            bluesky: Some(BlueskyAnnounce {
                enabled: Some(true),
                username: Some("user.bsky.social".to_string()),
                message_template: None,
            }),
            ..Default::default()
        };
        let mut ctx = make_ctx(Some(announce));
        let err = AnnounceStage.run(&mut ctx).unwrap_err();
        assert!(
            err.to_string().contains("BLUESKY_APP_PASSWORD"),
            "expected BLUESKY_APP_PASSWORD error, got: {err}"
        );
    }

    #[serial]
    #[test]
    fn test_bluesky_empty_env_var_errors() {
        unsafe { std::env::set_var("BLUESKY_APP_PASSWORD", "") };
        let announce = AnnounceConfig {
            bluesky: Some(BlueskyAnnounce {
                enabled: Some(true),
                username: Some("user.bsky.social".to_string()),
                message_template: None,
            }),
            ..Default::default()
        };
        let mut ctx = make_ctx(Some(announce));
        let err = AnnounceStage.run(&mut ctx).unwrap_err();
        assert!(
            err.to_string().contains("must not be empty"),
            "expected 'must not be empty' error, got: {err}"
        );
        unsafe { std::env::remove_var("BLUESKY_APP_PASSWORD") };
    }

    // ----------------------------------------------------------------
    // LinkedIn tests
    // ----------------------------------------------------------------

    #[test]
    fn test_skips_disabled_linkedin() {
        let announce = AnnounceConfig {
            linkedin: Some(LinkedInAnnounce {
                enabled: Some(false),
                message_template: None,
            }),
            ..Default::default()
        };
        let mut ctx = make_ctx(Some(announce));
        assert!(AnnounceStage.run(&mut ctx).is_ok());
    }

    #[serial]
    #[test]
    fn test_dry_run_linkedin() {
        unsafe { std::env::set_var("LINKEDIN_ACCESS_TOKEN", "test_token") };
        let announce = AnnounceConfig {
            linkedin: Some(LinkedInAnnounce {
                enabled: Some(true),
                message_template: Some(
                    "{{ .ProjectName }} {{ .Tag }} is out! Check it out at {{ .ReleaseURL }}"
                        .to_string(),
                ),
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
        ctx.template_vars_mut().set(
            "ReleaseURL",
            "https://github.com/org/myapp/releases/tag/v1.0.0",
        );
        assert!(AnnounceStage.run(&mut ctx).is_ok());
        unsafe { std::env::remove_var("LINKEDIN_ACCESS_TOKEN") };
    }

    #[serial]
    #[test]
    fn test_linkedin_missing_env_var_errors() {
        unsafe { std::env::remove_var("LINKEDIN_ACCESS_TOKEN") };
        let announce = AnnounceConfig {
            linkedin: Some(LinkedInAnnounce {
                enabled: Some(true),
                message_template: None,
            }),
            ..Default::default()
        };
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.announce = Some(announce);
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set(
            "ReleaseURL",
            "https://github.com/org/myapp/releases/tag/v1.0.0",
        );
        let err = AnnounceStage.run(&mut ctx).unwrap_err();
        assert!(
            err.to_string().contains("LINKEDIN_ACCESS_TOKEN"),
            "expected LINKEDIN_ACCESS_TOKEN error, got: {err}"
        );
    }

    #[serial]
    #[test]
    fn test_linkedin_empty_env_var_errors() {
        unsafe { std::env::set_var("LINKEDIN_ACCESS_TOKEN", "") };
        let announce = AnnounceConfig {
            linkedin: Some(LinkedInAnnounce {
                enabled: Some(true),
                message_template: None,
            }),
            ..Default::default()
        };
        let mut ctx = make_ctx(Some(announce));
        let err = AnnounceStage.run(&mut ctx).unwrap_err();
        assert!(
            err.to_string().contains("must not be empty"),
            "expected 'must not be empty' error, got: {err}"
        );
        unsafe { std::env::remove_var("LINKEDIN_ACCESS_TOKEN") };
    }

    // ----------------------------------------------------------------
    // OpenCollective tests
    // ----------------------------------------------------------------

    #[test]
    fn test_skips_disabled_opencollective() {
        let announce = AnnounceConfig {
            opencollective: Some(OpenCollectiveAnnounce {
                enabled: Some(false),
                slug: Some("my-project".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = make_ctx(Some(announce));
        assert!(AnnounceStage.run(&mut ctx).is_ok());
    }

    #[serial]
    #[test]
    fn test_dry_run_opencollective() {
        unsafe { std::env::set_var("OPENCOLLECTIVE_TOKEN", "test_token") };
        let announce = AnnounceConfig {
            opencollective: Some(OpenCollectiveAnnounce {
                enabled: Some(true),
                slug: Some("my-project".to_string()),
                title_template: Some("{{ .Tag }}".to_string()),
                message_template: Some(
                    "{{ .ProjectName }} {{ .Tag }} is out!".to_string(),
                ),
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
        ctx.template_vars_mut().set(
            "ReleaseURL",
            "https://github.com/org/myapp/releases/tag/v1.0.0",
        );
        assert!(AnnounceStage.run(&mut ctx).is_ok());
        unsafe { std::env::remove_var("OPENCOLLECTIVE_TOKEN") };
    }

    #[test]
    fn test_opencollective_missing_slug_errors() {
        let announce = AnnounceConfig {
            opencollective: Some(OpenCollectiveAnnounce {
                enabled: Some(true),
                slug: None,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = make_ctx(Some(announce));
        let err = AnnounceStage.run(&mut ctx).unwrap_err();
        assert!(
            err.to_string().contains("missing slug"),
            "expected 'missing slug' error, got: {err}"
        );
    }

    #[test]
    fn test_opencollective_empty_slug_skips() {
        let announce = AnnounceConfig {
            opencollective: Some(OpenCollectiveAnnounce {
                enabled: Some(true),
                slug: Some("".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = make_ctx(Some(announce));
        // Empty slug should cause a silent skip, not an error
        assert!(AnnounceStage.run(&mut ctx).is_ok());
    }

    #[serial]
    #[test]
    fn test_opencollective_missing_env_var_errors() {
        unsafe { std::env::remove_var("OPENCOLLECTIVE_TOKEN") };
        let announce = AnnounceConfig {
            opencollective: Some(OpenCollectiveAnnounce {
                enabled: Some(true),
                slug: Some("my-project".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.announce = Some(announce);
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set(
            "ReleaseURL",
            "https://github.com/org/myapp/releases/tag/v1.0.0",
        );
        let err = AnnounceStage.run(&mut ctx).unwrap_err();
        assert!(
            err.to_string().contains("OPENCOLLECTIVE_TOKEN"),
            "expected OPENCOLLECTIVE_TOKEN error, got: {err}"
        );
    }

    #[serial]
    #[test]
    fn test_opencollective_empty_env_var_errors() {
        unsafe { std::env::set_var("OPENCOLLECTIVE_TOKEN", "") };
        let announce = AnnounceConfig {
            opencollective: Some(OpenCollectiveAnnounce {
                enabled: Some(true),
                slug: Some("my-project".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = make_ctx(Some(announce));
        let err = AnnounceStage.run(&mut ctx).unwrap_err();
        assert!(
            err.to_string().contains("must not be empty"),
            "expected 'must not be empty' error, got: {err}"
        );
        unsafe { std::env::remove_var("OPENCOLLECTIVE_TOKEN") };
    }

    // ----------------------------------------------------------------
    // Discourse tests
    // ----------------------------------------------------------------

    #[test]
    fn test_skips_disabled_discourse() {
        let announce = AnnounceConfig {
            discourse: Some(DiscourseAnnounce {
                enabled: Some(false),
                server: Some("https://forum.example.com".to_string()),
                category_id: Some(5),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = make_ctx(Some(announce));
        assert!(AnnounceStage.run(&mut ctx).is_ok());
    }

    #[serial]
    #[test]
    fn test_dry_run_discourse() {
        unsafe { std::env::set_var("DISCOURSE_API_KEY", "test_key") };
        let announce = AnnounceConfig {
            discourse: Some(DiscourseAnnounce {
                enabled: Some(true),
                server: Some("https://forum.example.com".to_string()),
                category_id: Some(5),
                username: Some("release-bot".to_string()),
                title_template: Some("{{ .ProjectName }} {{ .Tag }} is out!".to_string()),
                message_template: Some(
                    "{{ .ProjectName }} {{ .Tag }} is out! Check it out at {{ .ReleaseURL }}"
                        .to_string(),
                ),
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
        ctx.template_vars_mut().set(
            "ReleaseURL",
            "https://github.com/org/myapp/releases/tag/v1.0.0",
        );
        assert!(AnnounceStage.run(&mut ctx).is_ok());
        unsafe { std::env::remove_var("DISCOURSE_API_KEY") };
    }

    #[serial]
    #[test]
    fn test_missing_discourse_server_returns_error() {
        unsafe { std::env::set_var("DISCOURSE_API_KEY", "test_key") };
        let announce = AnnounceConfig {
            discourse: Some(DiscourseAnnounce {
                enabled: Some(true),
                server: None,
                category_id: Some(5),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = make_ctx(Some(announce));
        let err = AnnounceStage.run(&mut ctx).unwrap_err();
        assert!(
            err.to_string().contains("missing server"),
            "expected 'missing server' error, got: {err}"
        );
        unsafe { std::env::remove_var("DISCOURSE_API_KEY") };
    }

    #[serial]
    #[test]
    fn test_missing_discourse_category_id_returns_error() {
        unsafe { std::env::set_var("DISCOURSE_API_KEY", "test_key") };
        let announce = AnnounceConfig {
            discourse: Some(DiscourseAnnounce {
                enabled: Some(true),
                server: Some("https://forum.example.com".to_string()),
                category_id: None,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = make_ctx(Some(announce));
        let err = AnnounceStage.run(&mut ctx).unwrap_err();
        assert!(
            err.to_string().contains("missing category_id"),
            "expected 'missing category_id' error, got: {err}"
        );
        unsafe { std::env::remove_var("DISCOURSE_API_KEY") };
    }

    #[serial]
    #[test]
    fn test_zero_discourse_category_id_returns_error() {
        unsafe { std::env::set_var("DISCOURSE_API_KEY", "test_key") };
        let announce = AnnounceConfig {
            discourse: Some(DiscourseAnnounce {
                enabled: Some(true),
                server: Some("https://forum.example.com".to_string()),
                category_id: Some(0),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = make_ctx(Some(announce));
        let err = AnnounceStage.run(&mut ctx).unwrap_err();
        assert!(
            err.to_string().contains("category_id must be non-zero"),
            "expected 'category_id must be non-zero' error, got: {err}"
        );
        unsafe { std::env::remove_var("DISCOURSE_API_KEY") };
    }

    #[serial]
    #[test]
    fn test_discourse_missing_env_var_errors() {
        unsafe { std::env::remove_var("DISCOURSE_API_KEY") };
        let announce = AnnounceConfig {
            discourse: Some(DiscourseAnnounce {
                enabled: Some(true),
                server: Some("https://forum.example.com".to_string()),
                category_id: Some(5),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.announce = Some(announce);
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set(
            "ReleaseURL",
            "https://github.com/org/myapp/releases/tag/v1.0.0",
        );
        let err = AnnounceStage.run(&mut ctx).unwrap_err();
        assert!(
            err.to_string().contains("DISCOURSE_API_KEY"),
            "expected DISCOURSE_API_KEY error, got: {err}"
        );
    }

    #[serial]
    #[test]
    fn test_discourse_empty_env_var_errors() {
        unsafe { std::env::set_var("DISCOURSE_API_KEY", "") };
        let announce = AnnounceConfig {
            discourse: Some(DiscourseAnnounce {
                enabled: Some(true),
                server: Some("https://forum.example.com".to_string()),
                category_id: Some(5),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = make_ctx(Some(announce));
        let err = AnnounceStage.run(&mut ctx).unwrap_err();
        assert!(
            err.to_string().contains("must not be empty"),
            "expected 'must not be empty' error, got: {err}"
        );
        unsafe { std::env::remove_var("DISCOURSE_API_KEY") };
    }
}
