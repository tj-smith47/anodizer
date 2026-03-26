use std::collections::HashMap;

use anodize_core::context::Context;
use anodize_core::stage::Stage;
use anyhow::Result;

pub mod discord;
pub mod slack;
pub mod webhook;

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
        if let Some(discord_cfg) = &announce.discord
            && discord_cfg.enabled.unwrap_or(false)
        {
            let raw_url = discord_cfg
                .webhook_url
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("announce.discord: missing webhook_url"))?;
            let url = ctx.render_template(raw_url)?;

            let tmpl = discord_cfg
                .message_template
                .as_deref()
                .unwrap_or("{{ .ProjectName }} {{ .Tag }} released!");

            let message = ctx.render_template(tmpl)?;

            if ctx.is_dry_run() {
                eprintln!("[announce] (dry-run) discord: {}", message);
            } else {
                eprintln!("[announce] discord: {}", message);
                discord::send_discord(&url, &message)?;
            }
        }

        // ----------------------------------------------------------------
        // Slack
        // ----------------------------------------------------------------
        if let Some(slack_cfg) = &announce.slack
            && slack_cfg.enabled.unwrap_or(false)
        {
            let raw_url = slack_cfg
                .webhook_url
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("announce.slack: missing webhook_url"))?;
            let url = ctx.render_template(raw_url)?;

            let tmpl = slack_cfg
                .message_template
                .as_deref()
                .unwrap_or("{{ .ProjectName }} {{ .Tag }} released!");

            let message = ctx.render_template(tmpl)?;

            if ctx.is_dry_run() {
                eprintln!("[announce] (dry-run) slack: {}", message);
            } else {
                eprintln!("[announce] slack: {}", message);
                slack::send_slack(&url, &message)?;
            }
        }

        // ----------------------------------------------------------------
        // Generic HTTP webhook
        // ----------------------------------------------------------------
        if let Some(webhook_cfg) = &announce.webhook
            && webhook_cfg.enabled.unwrap_or(false)
        {
            let raw_url = webhook_cfg
                .endpoint_url
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("announce.webhook: missing endpoint_url"))?;
            let url = ctx.render_template(raw_url)?;

            let tmpl = webhook_cfg
                .message_template
                .as_deref()
                .unwrap_or("{{ .ProjectName }} {{ .Tag }} released!");

            let message = ctx.render_template(tmpl)?;

            let raw_headers = webhook_cfg.headers.clone().unwrap_or_default();
            let mut headers = HashMap::new();
            for (k, v) in &raw_headers {
                headers.insert(k.clone(), ctx.render_template(v)?);
            }

            let content_type = webhook_cfg
                .content_type
                .clone()
                .unwrap_or_else(|| "application/json".to_string());

            if ctx.is_dry_run() {
                eprintln!("[announce] (dry-run) webhook: {}", message);
            } else {
                eprintln!("[announce] webhook: {}", message);
                webhook::send_webhook(&url, &message, &headers, &content_type)?;
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
    use anodize_core::config::{AnnounceConfig, AnnounceProviderConfig, Config, WebhookConfig};
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
}
