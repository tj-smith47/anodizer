#![cfg(test)]
#![allow(clippy::field_reassign_with_default)]

use std::collections::HashMap;

use anodizer_core::config::{
    AnnounceConfig, BlueskyAnnounce, Config, DiscordAnnounce, DiscourseAnnounce, EmailAnnounce,
    LinkedInAnnounce, MastodonAnnounce, MattermostAnnounce, OpenCollectiveAnnounce, RedditAnnounce,
    SlackAnnounce, SlackBlock, SlackTextObject, StringOrBool, TeamsAnnounce, TelegramAnnounce,
    TwitterAnnounce, WebhookConfig,
};
use anodizer_core::context::{Context, ContextOptions};
use anodizer_core::stage::Stage;
use serial_test::serial;

use crate::AnnounceStage;
use crate::helpers::{render_json_template, resolve_smtp_port, resolve_webhook_headers};

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
            enabled: Some(StringOrBool::Bool(false)),
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
            enabled: Some(StringOrBool::Bool(false)),
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
            enabled: Some(StringOrBool::Bool(false)),
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
            enabled: Some(StringOrBool::Bool(true)),
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
            enabled: Some(StringOrBool::Bool(true)),
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
            enabled: Some(StringOrBool::Bool(true)),
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
    let rendered = render_json_template(&ctx, Some(&blocks_json))
        .unwrap()
        .unwrap();
    assert_eq!(rendered[0]["text"]["text"], "myapp v2.0.0 is out!");
}

#[test]
fn test_dry_run_webhook_does_not_send() {
    let announce = AnnounceConfig {
        discord: None,
        slack: None,
        webhook: Some(WebhookConfig {
            enabled: Some(StringOrBool::Bool(true)),
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
            enabled: Some(StringOrBool::Bool(true)),
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
    // Skip-when-empty UX: missing webhook_url only hard-errors in strict
    // mode. Normal mode warns and skips this announcer (covered by
    // test_missing_discord_webhook_url_warn_and_skip below).
    let opts = ContextOptions {
        dry_run: false,
        strict: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    assert!(AnnounceStage.run(&mut ctx).is_err());
}

#[test]
fn test_missing_discord_webhook_url_warn_and_skip() {
    // Skip-when-empty UX: in normal (non-strict) mode, a missing
    // webhook_url should warn and skip the discord announcer cleanly
    // (Ok result), letting unrelated announcers continue.
    let announce = AnnounceConfig {
        discord: Some(DiscordAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            webhook_url: None,
            message_template: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.announce = Some(announce);
    let opts = ContextOptions {
        dry_run: false,
        strict: false,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    AnnounceStage
        .run(&mut ctx)
        .expect("normal-mode missing webhook_url must skip cleanly, not error");
}

// ----------------------------------------------------------------
// Telegram tests
// ----------------------------------------------------------------

#[test]
fn test_skips_disabled_telegram() {
    let announce = AnnounceConfig {
        telegram: Some(TelegramAnnounce {
            enabled: Some(StringOrBool::Bool(false)),
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
            enabled: Some(StringOrBool::Bool(true)),
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
#[serial]
fn test_missing_telegram_bot_token_returns_error() {
    unsafe { std::env::remove_var("TELEGRAM_TOKEN") };
    let announce = AnnounceConfig {
        telegram: Some(TelegramAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            bot_token: None,
            chat_id: Some("-100123".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.announce = Some(announce);
    // Skip-when-empty UX: hard error in strict mode only.
    let opts = ContextOptions {
        strict: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    assert!(AnnounceStage.run(&mut ctx).is_err());
}

#[test]
#[serial]
fn test_missing_telegram_bot_token_warn_and_skip() {
    unsafe { std::env::remove_var("TELEGRAM_TOKEN") };
    let announce = AnnounceConfig {
        telegram: Some(TelegramAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
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
    AnnounceStage
        .run(&mut ctx)
        .expect("normal-mode missing telegram bot_token must skip cleanly, not error");
}

#[test]
fn test_missing_telegram_chat_id_returns_error() {
    let announce = AnnounceConfig {
        telegram: Some(TelegramAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            bot_token: Some("123:ABC".to_string()),
            chat_id: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.announce = Some(announce);
    // Skip-when-empty UX: hard error in strict mode only.
    let opts = ContextOptions {
        strict: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    assert!(AnnounceStage.run(&mut ctx).is_err());
}

#[test]
fn test_missing_telegram_chat_id_warn_and_skip() {
    let announce = AnnounceConfig {
        telegram: Some(TelegramAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
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
    AnnounceStage
        .run(&mut ctx)
        .expect("normal-mode missing telegram chat_id must skip cleanly, not error");
}

// ----------------------------------------------------------------
// Teams tests
// ----------------------------------------------------------------

#[test]
fn test_skips_disabled_teams() {
    let announce = AnnounceConfig {
        teams: Some(TeamsAnnounce {
            enabled: Some(StringOrBool::Bool(false)),
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
            enabled: Some(StringOrBool::Bool(true)),
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
            enabled: Some(StringOrBool::Bool(true)),
            webhook_url: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.announce = Some(announce);
    // Skip-when-empty UX: missing webhook_url is a hard error only in
    // strict mode; normal mode warns + skips.
    let opts = ContextOptions {
        strict: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
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
            enabled: Some(StringOrBool::Bool(false)),
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
            enabled: Some(StringOrBool::Bool(true)),
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
            enabled: Some(StringOrBool::Bool(true)),
            webhook_url: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.announce = Some(announce);
    // Skip-when-empty UX: hard error in strict mode only.
    let opts = ContextOptions {
        strict: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
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
            enabled: Some(StringOrBool::Bool(false)),
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
            enabled: Some(StringOrBool::Bool(true)),
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
            enabled: Some(StringOrBool::Bool(true)),
            from: None,
            to: vec!["dev@example.com".to_string()],
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.announce = Some(announce);
    // Skip-when-empty UX: hard error in strict mode only.
    let opts = ContextOptions {
        strict: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    assert!(AnnounceStage.run(&mut ctx).is_err());
}

#[test]
fn test_missing_email_to_returns_error() {
    let announce = AnnounceConfig {
        email: Some(EmailAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            from: Some("bot@example.com".to_string()),
            to: vec![],
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.announce = Some(announce);
    // Skip-when-empty UX: hard error in strict mode only.
    let opts = ContextOptions {
        strict: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    assert!(AnnounceStage.run(&mut ctx).is_err());
}

#[test]
fn test_invalid_email_from_returns_error() {
    let announce = AnnounceConfig {
        email: Some(EmailAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
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
        enabled: Some(StringOrBool::Bool(true)),
        webhook_url: Some("https://discord.com/api/webhooks/123/abc".to_string()),
        message_template: None,
        author: Some("release-bot".to_string()),
        color: Some("16711680".to_string()),
        icon_url: Some("https://example.com/icon.png".to_string()),
    };
    assert_eq!(cfg.author.as_deref(), Some("release-bot"));
    assert_eq!(cfg.color.as_deref(), Some("16711680"));
    assert_eq!(
        cfg.icon_url.as_deref(),
        Some("https://example.com/icon.png")
    );
}

#[test]
fn test_webhook_skip_tls_verify_field() {
    let cfg = WebhookConfig {
        enabled: Some(StringOrBool::Bool(true)),
        endpoint_url: Some("https://internal.example.com/hook".to_string()),
        skip_tls_verify: Some(true),
        ..Default::default()
    };
    assert_eq!(cfg.skip_tls_verify, Some(true));
}

#[test]
fn test_telegram_message_thread_id_field() {
    let cfg = TelegramAnnounce {
        enabled: Some(StringOrBool::Bool(true)),
        bot_token: Some("123:ABC".to_string()),
        chat_id: Some("-100123".to_string()),
        message_thread_id: Some("42".to_string()),
        ..Default::default()
    };
    assert_eq!(cfg.message_thread_id.as_deref(), Some("42"));
}

#[test]
fn test_teams_title_and_color_fields() {
    let cfg = TeamsAnnounce {
        enabled: Some(StringOrBool::Bool(true)),
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
        enabled: Some(StringOrBool::Bool(true)),
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
            enabled: Some(StringOrBool::Bool(true)),
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
            enabled: Some(StringOrBool::Bool(true)),
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
            enabled: Some(StringOrBool::Bool(true)),
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
    assert_eq!(config.blocks[1].text.as_ref().unwrap().text_type, "mrkdwn");
}

// ----------------------------------------------------------------
// Reddit tests
// ----------------------------------------------------------------

#[test]
fn test_skips_disabled_reddit() {
    let announce = AnnounceConfig {
        reddit: Some(RedditAnnounce {
            enabled: Some(StringOrBool::Bool(false)),
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
            enabled: Some(StringOrBool::Bool(true)),
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
    ctx.template_vars_mut().set(
        "ReleaseURL",
        "https://github.com/org/myapp/releases/tag/v1.0.0",
    );
    assert!(AnnounceStage.run(&mut ctx).is_ok());
    unsafe { std::env::remove_var("REDDIT_SECRET") };
    unsafe { std::env::remove_var("REDDIT_PASSWORD") };
}

#[test]
#[serial]
fn test_missing_reddit_application_id_returns_error() {
    unsafe { std::env::set_var("REDDIT_SECRET", "testsecret") };
    unsafe { std::env::set_var("REDDIT_PASSWORD", "testpass") };
    let announce = AnnounceConfig {
        reddit: Some(RedditAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            application_id: None,
            username: Some("testuser".to_string()),
            sub: Some("rust".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.announce = Some(announce);
    // Skip-when-empty UX: hard error in strict mode only.
    let opts = ContextOptions {
        strict: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    let err = AnnounceStage.run(&mut ctx).unwrap_err();
    assert!(
        err.to_string().contains("missing application_id"),
        "expected 'missing application_id' error, got: {err}"
    );
    unsafe { std::env::remove_var("REDDIT_SECRET") };
    unsafe { std::env::remove_var("REDDIT_PASSWORD") };
}

#[test]
#[serial]
fn test_missing_reddit_application_id_warn_and_skip() {
    unsafe { std::env::set_var("REDDIT_SECRET", "testsecret") };
    unsafe { std::env::set_var("REDDIT_PASSWORD", "testpass") };
    let announce = AnnounceConfig {
        reddit: Some(RedditAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            application_id: None,
            username: Some("testuser".to_string()),
            sub: Some("rust".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = make_ctx(Some(announce));
    AnnounceStage
        .run(&mut ctx)
        .expect("normal-mode missing reddit application_id must skip cleanly, not error");
    unsafe { std::env::remove_var("REDDIT_SECRET") };
    unsafe { std::env::remove_var("REDDIT_PASSWORD") };
}

#[test]
#[serial]
fn test_missing_reddit_username_warn_and_skip() {
    unsafe { std::env::set_var("REDDIT_SECRET", "testsecret") };
    unsafe { std::env::set_var("REDDIT_PASSWORD", "testpass") };
    let announce = AnnounceConfig {
        reddit: Some(RedditAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            application_id: Some("app123".to_string()),
            username: None,
            sub: Some("rust".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = make_ctx(Some(announce));
    AnnounceStage
        .run(&mut ctx)
        .expect("normal-mode missing reddit username must skip cleanly, not error");
    unsafe { std::env::remove_var("REDDIT_SECRET") };
    unsafe { std::env::remove_var("REDDIT_PASSWORD") };
}

#[test]
#[serial]
fn test_missing_reddit_username_returns_error() {
    unsafe { std::env::set_var("REDDIT_SECRET", "testsecret") };
    unsafe { std::env::set_var("REDDIT_PASSWORD", "testpass") };
    let announce = AnnounceConfig {
        reddit: Some(RedditAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            application_id: Some("app123".to_string()),
            username: None,
            sub: Some("rust".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.announce = Some(announce);
    // Skip-when-empty UX: hard error in strict mode only.
    let opts = ContextOptions {
        strict: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    let err = AnnounceStage.run(&mut ctx).unwrap_err();
    assert!(
        err.to_string().contains("missing username"),
        "expected 'missing username' error, got: {err}"
    );
    unsafe { std::env::remove_var("REDDIT_SECRET") };
    unsafe { std::env::remove_var("REDDIT_PASSWORD") };
}

#[test]
#[serial]
fn test_missing_reddit_sub_warn_and_skip() {
    unsafe { std::env::set_var("REDDIT_SECRET", "testsecret") };
    unsafe { std::env::set_var("REDDIT_PASSWORD", "testpass") };
    let announce = AnnounceConfig {
        reddit: Some(RedditAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            application_id: Some("app123".to_string()),
            username: Some("testuser".to_string()),
            sub: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = make_ctx(Some(announce));
    AnnounceStage
        .run(&mut ctx)
        .expect("normal-mode missing reddit sub must skip cleanly, not error");
    unsafe { std::env::remove_var("REDDIT_SECRET") };
    unsafe { std::env::remove_var("REDDIT_PASSWORD") };
}

#[test]
#[serial]
fn test_missing_reddit_sub_returns_error() {
    unsafe { std::env::set_var("REDDIT_SECRET", "testsecret") };
    unsafe { std::env::set_var("REDDIT_PASSWORD", "testpass") };
    let announce = AnnounceConfig {
        reddit: Some(RedditAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            application_id: Some("app123".to_string()),
            username: Some("testuser".to_string()),
            sub: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.announce = Some(announce);
    // Skip-when-empty UX: hard error in strict mode only.
    let opts = ContextOptions {
        strict: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    let err = AnnounceStage.run(&mut ctx).unwrap_err();
    assert!(
        err.to_string().contains("missing sub"),
        "expected 'missing sub' error, got: {err}"
    );
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
            enabled: Some(StringOrBool::Bool(false)),
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
            enabled: Some(StringOrBool::Bool(true)),
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
            enabled: Some(StringOrBool::Bool(true)),
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
            enabled: Some(StringOrBool::Bool(false)),
            server: Some("https://mastodon.social".to_string()),
            message_template: None,
        }),
        ..Default::default()
    };
    let mut ctx = make_ctx(Some(announce));
    assert!(AnnounceStage.run(&mut ctx).is_ok());
}

/// Q-mast1: GoReleaser marks `MASTODON_CLIENT_ID` and
/// `MASTODON_CLIENT_SECRET` as `notEmpty` alongside `MASTODON_ACCESS_TOKEN`.
/// Tests that just need a happy-path Mastodon dry-run go through this helper
/// so all three env vars are set in lockstep.
fn set_mastodon_creds() {
    unsafe {
        std::env::set_var("MASTODON_ACCESS_TOKEN", "test-token");
        std::env::set_var("MASTODON_CLIENT_ID", "test-client-id");
        std::env::set_var("MASTODON_CLIENT_SECRET", "test-client-secret");
    }
}

fn clear_mastodon_creds() {
    unsafe {
        std::env::remove_var("MASTODON_ACCESS_TOKEN");
        std::env::remove_var("MASTODON_CLIENT_ID");
        std::env::remove_var("MASTODON_CLIENT_SECRET");
    }
}

#[test]
#[serial]
fn test_dry_run_mastodon() {
    set_mastodon_creds();
    let announce = AnnounceConfig {
        mastodon: Some(MastodonAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
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
    clear_mastodon_creds();
}

#[test]
#[serial]
fn test_mastodon_missing_server_returns_error() {
    set_mastodon_creds();
    let announce = AnnounceConfig {
        mastodon: Some(MastodonAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            server: None,
            message_template: None,
        }),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.announce = Some(announce);
    // Skip-when-empty UX: hard error in strict mode only.
    let opts = ContextOptions {
        strict: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    let err = AnnounceStage.run(&mut ctx).unwrap_err();
    assert!(
        err.to_string().contains("missing server"),
        "expected 'missing server' error, got: {err}"
    );
    clear_mastodon_creds();
}

#[test]
#[serial]
fn test_mastodon_missing_server_warn_and_skip() {
    set_mastodon_creds();
    let announce = AnnounceConfig {
        mastodon: Some(MastodonAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
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
    AnnounceStage
        .run(&mut ctx)
        .expect("normal-mode missing mastodon server must skip cleanly, not error");
    clear_mastodon_creds();
}

#[test]
#[serial]
fn test_mastodon_missing_env_var_returns_error() {
    clear_mastodon_creds();
    let announce = AnnounceConfig {
        mastodon: Some(MastodonAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
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
    assert!(
        err.to_string().contains("MASTODON_ACCESS_TOKEN"),
        "expected MASTODON_ACCESS_TOKEN error, got: {err}"
    );
}

#[test]
#[serial]
fn test_mastodon_empty_server_skips() {
    let announce = AnnounceConfig {
        mastodon: Some(MastodonAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            server: Some("".to_string()),
            message_template: None,
        }),
        ..Default::default()
    };
    let mut ctx = make_ctx(Some(announce));
    // Empty server should cause a silent skip, not an error
    assert!(AnnounceStage.run(&mut ctx).is_ok());
}

/// Q-mast1: GoReleaser marks `MASTODON_CLIENT_ID` as `notEmpty`. Anodizer
/// must fail-fast when it is missing instead of silently proceeding with
/// only the access token.
#[test]
#[serial]
fn test_mastodon_missing_client_id_returns_error() {
    clear_mastodon_creds();
    unsafe {
        std::env::set_var("MASTODON_ACCESS_TOKEN", "test-token");
        std::env::set_var("MASTODON_CLIENT_SECRET", "test-secret");
        // MASTODON_CLIENT_ID intentionally unset
    }
    let announce = AnnounceConfig {
        mastodon: Some(MastodonAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
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
    assert!(
        err.to_string().contains("MASTODON_CLIENT_ID"),
        "expected MASTODON_CLIENT_ID error, got: {err}"
    );
    clear_mastodon_creds();
}

/// Q-mast1 (the second half): `MASTODON_CLIENT_SECRET` is `notEmpty` in GR.
#[test]
#[serial]
fn test_mastodon_missing_client_secret_returns_error() {
    clear_mastodon_creds();
    unsafe {
        std::env::set_var("MASTODON_ACCESS_TOKEN", "test-token");
        std::env::set_var("MASTODON_CLIENT_ID", "test-id");
        // MASTODON_CLIENT_SECRET intentionally unset
    }
    let announce = AnnounceConfig {
        mastodon: Some(MastodonAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
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
    assert!(
        err.to_string().contains("MASTODON_CLIENT_SECRET"),
        "expected MASTODON_CLIENT_SECRET error, got: {err}"
    );
    clear_mastodon_creds();
}

/// Q-tg1: the Telegram default template MUST NOT use Tera's `~` concatenation
/// operator. Copy-pasting the default into a custom user template tends to
/// mix it with GR-style `print` blocks (Tera then refuses to parse `print`)
/// or rewrite the `~` and break the filter pipeline. Drives a dry-run end
/// to end with `message_template: None` so the default path actually fires.
#[test]
#[serial]
fn test_telegram_default_template_renders_without_tilde() {
    let announce = AnnounceConfig {
        telegram: Some(TelegramAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            bot_token: Some("123:ABC".to_string()),
            chat_id: Some("-100123".to_string()),
            message_template: None,
            parse_mode: None,
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
    ctx.template_vars_mut()
        .set("ReleaseURL", "https://example.com/r/v1.0.0");
    // Should succeed in dry-run without error — exercises the default
    // template-rendering path end to end.
    assert!(AnnounceStage.run(&mut ctx).is_ok());
}

/// Q-disc1: Discord webhook URL must percent-encode the env-derived
/// id+token segments. GoReleaser uses `url.URL.JoinPath`, which escapes path
/// segments; we mirror that via `percent_encode_path_segment`. Unit test of
/// the helper boundary — `crates/core/src/url.rs` already pins the encoding
/// table; this verifies the segments we feed it round-trip safely.
#[test]
fn test_discord_webhook_url_percent_encodes_id_and_token() {
    let id = "id/with?weird#chars";
    let token = "tok+plus space";
    let encoded_id = anodizer_core::url::percent_encode_path_segment(id);
    let encoded_token = anodizer_core::url::percent_encode_path_segment(token);
    let url = format!(
        "{}/webhooks/{}/{}",
        "https://discord.com/api", encoded_id, encoded_token
    );
    assert!(
        !url.contains('?'),
        "literal `?` must be encoded to %3F: {url}"
    );
    assert!(
        !url.contains('#'),
        "literal `#` must be encoded to %23: {url}"
    );
    assert!(url.contains("%2F"), "literal `/` in id should be encoded");
    assert!(
        url.contains("%2B"),
        "literal `+` in token should be encoded"
    );
}

// ----------------------------------------------------------------
// Bluesky tests
// ----------------------------------------------------------------

#[test]
fn test_skips_disabled_bluesky() {
    let announce = AnnounceConfig {
        bluesky: Some(BlueskyAnnounce {
            enabled: Some(StringOrBool::Bool(false)),
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
            enabled: Some(StringOrBool::Bool(true)),
            username: Some("user.bsky.social".to_string()),
            message_template: Some("{{ .ProjectName }} {{ .Tag }}".to_string()),
            pds_url: None,
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
            enabled: Some(StringOrBool::Bool(true)),
            username: None,
            message_template: None,
            pds_url: None,
        }),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.announce = Some(announce);
    // Skip-when-empty UX: hard error in strict mode only.
    let opts = ContextOptions {
        strict: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    let err = AnnounceStage.run(&mut ctx).unwrap_err();
    assert!(
        err.to_string().contains("missing username"),
        "expected 'missing username' error, got: {err}"
    );
    unsafe { std::env::remove_var("BLUESKY_APP_PASSWORD") };
}

#[serial]
#[test]
fn test_bluesky_missing_username_warn_and_skip() {
    unsafe { std::env::set_var("BLUESKY_APP_PASSWORD", "test_pass") };
    let announce = AnnounceConfig {
        bluesky: Some(BlueskyAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            username: None,
            message_template: None,
            pds_url: None,
        }),
        ..Default::default()
    };
    let mut ctx = make_ctx(Some(announce));
    AnnounceStage
        .run(&mut ctx)
        .expect("normal-mode missing bluesky username must skip cleanly, not error");
    unsafe { std::env::remove_var("BLUESKY_APP_PASSWORD") };
}

#[serial]
#[test]
fn test_bluesky_missing_env_var_errors() {
    unsafe { std::env::remove_var("BLUESKY_APP_PASSWORD") };
    let announce = AnnounceConfig {
        bluesky: Some(BlueskyAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            username: Some("user.bsky.social".to_string()),
            message_template: None,
            pds_url: None,
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
            enabled: Some(StringOrBool::Bool(true)),
            username: Some("user.bsky.social".to_string()),
            message_template: None,
            pds_url: None,
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
            enabled: Some(StringOrBool::Bool(false)),
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
    unsafe {
        std::env::set_var(
            "LINKEDIN_ACCESS_TOKEN",
            "AQXopaque_test_token_long_enough_to_pass_validation_xx",
        )
    };
    let announce = AnnounceConfig {
        linkedin: Some(LinkedInAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
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
            enabled: Some(StringOrBool::Bool(true)),
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
            enabled: Some(StringOrBool::Bool(true)),
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
            enabled: Some(StringOrBool::Bool(false)),
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
    unsafe {
        std::env::set_var(
            "OPENCOLLECTIVE_TOKEN",
            "test_token_long_enough_to_pass_validation_check_xx",
        )
    };
    let announce = AnnounceConfig {
        opencollective: Some(OpenCollectiveAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            slug: Some("my-project".to_string()),
            title_template: Some("{{ .Tag }}".to_string()),
            message_template: Some("{{ .ProjectName }} {{ .Tag }} is out!".to_string()),
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
            enabled: Some(StringOrBool::Bool(true)),
            slug: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.announce = Some(announce);
    // Skip-when-empty UX: hard error in strict mode only.
    let opts = ContextOptions {
        strict: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    let err = AnnounceStage.run(&mut ctx).unwrap_err();
    assert!(
        err.to_string().contains("missing slug"),
        "expected 'missing slug' error, got: {err}"
    );
}

#[test]
fn test_opencollective_missing_slug_warn_and_skip() {
    let announce = AnnounceConfig {
        opencollective: Some(OpenCollectiveAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            slug: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = make_ctx(Some(announce));
    AnnounceStage
        .run(&mut ctx)
        .expect("normal-mode missing opencollective slug must skip cleanly, not error");
}

#[test]
fn test_opencollective_empty_slug_skips() {
    let announce = AnnounceConfig {
        opencollective: Some(OpenCollectiveAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
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
            enabled: Some(StringOrBool::Bool(true)),
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
            enabled: Some(StringOrBool::Bool(true)),
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
            enabled: Some(StringOrBool::Bool(false)),
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
            enabled: Some(StringOrBool::Bool(true)),
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
            enabled: Some(StringOrBool::Bool(true)),
            server: None,
            category_id: Some(5),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.announce = Some(announce);
    // Skip-when-empty UX: hard error in strict mode only.
    let opts = ContextOptions {
        strict: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    let err = AnnounceStage.run(&mut ctx).unwrap_err();
    assert!(
        err.to_string().contains("missing server"),
        "expected 'missing server' error, got: {err}"
    );
    unsafe { std::env::remove_var("DISCOURSE_API_KEY") };
}

#[serial]
#[test]
fn test_missing_discourse_server_warn_and_skip() {
    unsafe { std::env::set_var("DISCOURSE_API_KEY", "test_key") };
    let announce = AnnounceConfig {
        discourse: Some(DiscourseAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            server: None,
            category_id: Some(5),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = make_ctx(Some(announce));
    AnnounceStage
        .run(&mut ctx)
        .expect("normal-mode missing discourse server must skip cleanly, not error");
    unsafe { std::env::remove_var("DISCOURSE_API_KEY") };
}

#[serial]
#[test]
fn test_missing_discourse_category_id_returns_error() {
    unsafe { std::env::set_var("DISCOURSE_API_KEY", "test_key") };
    let announce = AnnounceConfig {
        discourse: Some(DiscourseAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            server: Some("https://forum.example.com".to_string()),
            category_id: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.announce = Some(announce);
    // Skip-when-empty UX: hard error in strict mode only.
    let opts = ContextOptions {
        strict: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    let err = AnnounceStage.run(&mut ctx).unwrap_err();
    assert!(
        err.to_string().contains("missing category_id"),
        "expected 'missing category_id' error, got: {err}"
    );
    unsafe { std::env::remove_var("DISCOURSE_API_KEY") };
}

#[serial]
#[test]
fn test_missing_discourse_category_id_warn_and_skip() {
    unsafe { std::env::set_var("DISCOURSE_API_KEY", "test_key") };
    let announce = AnnounceConfig {
        discourse: Some(DiscourseAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            server: Some("https://forum.example.com".to_string()),
            category_id: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = make_ctx(Some(announce));
    AnnounceStage
        .run(&mut ctx)
        .expect("normal-mode missing discourse category_id must skip cleanly, not error");
    unsafe { std::env::remove_var("DISCOURSE_API_KEY") };
}

#[serial]
#[test]
fn test_zero_discourse_category_id_returns_error() {
    unsafe { std::env::set_var("DISCOURSE_API_KEY", "test_key") };
    let announce = AnnounceConfig {
        discourse: Some(DiscourseAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
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
            enabled: Some(StringOrBool::Bool(true)),
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
            enabled: Some(StringOrBool::Bool(true)),
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

// ----------------------------------------------------------------
// Anodize-additive UX divergences from GoReleaser
// ----------------------------------------------------------------

/// Pins the webhook User-Agent as `anodizer/<crate-version>` (anodize-
/// additive UX win documented in lib.rs near the User-Agent header
/// fallback). GoReleaser sends a static `User-Agent: goreleaser`; the
/// version-suffixed variant is debuggable on the receiving end without
/// any wire-shape cost.
#[test]
fn test_webhook_user_agent_is_anodizer_versioned() {
    let ua = anodizer_core::http::USER_AGENT;
    assert!(
        ua.starts_with("anodizer/"),
        "webhook User-Agent must start with 'anodizer/' (anodize-additive UX divergence \
             from GoReleaser's static 'goreleaser' UA), got: {ua:?}"
    );
    let suffix = ua.trim_start_matches("anodizer/");
    assert!(
        !suffix.is_empty() && suffix.chars().any(|c| c.is_ascii_digit()),
        "webhook User-Agent must include a version suffix (e.g. anodizer/1.2.3), got: {ua:?}"
    );
}

/// Pins the SMTP port default at 587 (anodize-additive UX win
/// documented on `EmailAnnounce::port`). When both the config field
/// and the SMTP_PORT env var are unset, the announcer defaults to the
/// IETF submission port instead of GoReleaser's `errNoPort` bail.
#[test]
fn test_email_smtp_port_defaults_to_587() {
    // No config port, no env override → submission port.
    assert_eq!(resolve_smtp_port(None, None), 587);
    // Config wins over env and over the default.
    assert_eq!(resolve_smtp_port(Some(2525), None), 2525);
    assert_eq!(resolve_smtp_port(Some(2525), Some(465)), 2525);
    // Env wins over the default when config is absent.
    assert_eq!(resolve_smtp_port(None, Some(465)), 465);
}

/// Pins Mattermost `channel` as template-rendered (anodize-additive UX
/// win documented near the mattermost render block). GoReleaser passes
/// `channel` raw — no template substitution. Anodize renders it through
/// the engine, unlocking per-tag channel routing like
/// `channel: "release-{{ Tag }}"`. We pin this by feeding a malformed
/// template that would only error if rendering is invoked.
#[test]
fn test_mattermost_renders_channel_template() {
    let announce = AnnounceConfig {
        mattermost: Some(MattermostAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            webhook_url: Some("https://mm.invalid/hooks/xxx".to_string()),
            // Unclosed Tera tag — will only surface as a render error if
            // the channel field is actually run through the engine.
            channel: Some("release-{{ Tag".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.announce = Some(announce);
    // Strict mode so the per-announcer error surfaces as a stage-level
    // failure (rather than being swallowed by any soft skip path).
    let opts = ContextOptions {
        strict: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    let err = AnnounceStage.run(&mut ctx).unwrap_err();
    // Structural assertion against anodizer-controlled wrapping strings only:
    // - "announce errors:" comes from the stage aggregator (lib.rs:1046)
    // - "mattermost:" comes from the per-announcer error tag (lib.rs:703)
    // - "failed to render template" comes from the anodizer template engine's
    //   `with_context` wrapper (`crates/core/src/template.rs:1552`)
    // None of these depend on Tera's internal error wording, so this test
    // stays green if Tera renames "syntax error" → "parse error" (or similar)
    // upstream. The previous 5-token disjunction (`template`/`render`/`parse`/
    // `syntax`/`tera`) leaned on Tera-internal phrases for some branches and
    // would silently degrade if Tera reworded them.
    //
    // Note: we cannot use `err.chain().any(|e| e.is::<tera::Error>())` here
    // because the per-announcer block at lib.rs:703 collapses each inner
    // error to a String via `format!("mattermost: {e}")` before re-bailing
    // through `anyhow::bail!`, flattening the chain to a single layer.
    let msg = err.to_string();
    assert!(
        msg.contains("announce errors:")
            && msg.contains("mattermost:")
            && msg.contains("failed to render template"),
        "expected mattermost template render error proving channel rendering is invoked, got: {err}"
    );
}

/// Pins Mattermost `channel` rendering on the *success* path: a valid
/// `{{ Tag }}` template must resolve cleanly during dry-run.
#[test]
fn test_mattermost_channel_template_resolves_on_dry_run() {
    let announce = AnnounceConfig {
        mattermost: Some(MattermostAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            webhook_url: Some("https://mm.invalid/hooks/xxx".to_string()),
            channel: Some("release-{{ Tag }}".to_string()),
            username: Some("bot-{{ ProjectName }}".to_string()),
            icon_url: Some("https://cdn.invalid/{{ Tag }}.png".to_string()),
            icon_emoji: Some(":{{ ProjectName }}:".to_string()),
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
    ctx.template_vars_mut().set(
        "ReleaseURL",
        "https://github.com/org/myapp/releases/tag/v1.0.0",
    );
    AnnounceStage.run(&mut ctx).expect(
        "mattermost channel/username/icon_url/icon_emoji template rendering must \
                     succeed in dry-run with valid templates",
    );
}

// -----------------------------------------------------------------------
// Webhook header resolver — case-insensitive precedence
//
// Pre-2026-04-28 the header-precedence logic used
// `headers.contains_key("Authorization")` and a HashMap `entry("User-
// Agent")` to gate anodizer's defaults. HTTP header names are case-
// insensitive (RFC 7230); a user who wrote `headers: { authorization:
// "user-foo" }` (lowercase) bypassed the gate, anodizer pushed its own
// `Authorization` header, and reqwest emitted BOTH on the wire. The
// resolver now case-folds the override check; these tests pin that
// behavior. Source: Group G review deferral 2026-04-28.
// -----------------------------------------------------------------------

/// Lowercase `authorization` from the user must suppress anodizer's
/// `BASIC_AUTH_HEADER_VALUE` / `BEARER_TOKEN_HEADER_VALUE` default and
/// be the SOLE Authorization key (no duplicate of any case).
#[test]
fn test_resolve_webhook_headers_lowercase_authorization_wins() {
    let mut user = HashMap::new();
    user.insert("authorization".to_string(), "user-foo".to_string());

    let resolved = resolve_webhook_headers(user, Some("Basic ZGVmYXVsdA=="), None, "ua/1.0");

    // Sole Authorization key, case-folded count == 1.
    let auth_count = resolved
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("Authorization"))
        .count();
    assert_eq!(
        auth_count, 1,
        "exactly one Authorization-equivalent key must be present, got {resolved:?}"
    );

    // The value must be the user's, not anodizer's basic_auth default.
    let (_, val) = resolved
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Authorization"))
        .expect("Authorization key present");
    assert_eq!(
        val, "user-foo",
        "user-supplied lowercase authorization must win over basic_auth env default"
    );
}

/// Lowercase `user-agent` from the user must suppress anodizer's
/// `User-Agent` default and be the SOLE User-Agent key.
#[test]
fn test_resolve_webhook_headers_lowercase_user_agent_wins() {
    let mut user = HashMap::new();
    user.insert("user-agent".to_string(), "custom-ua/9.9".to_string());

    let resolved = resolve_webhook_headers(user, None, None, "anodizer/1.2.3");

    let ua_count = resolved
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("User-Agent"))
        .count();
    assert_eq!(
        ua_count, 1,
        "exactly one User-Agent-equivalent key must be present, got {resolved:?}"
    );

    let (_, val) = resolved
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("User-Agent"))
        .expect("User-Agent key present");
    assert_eq!(
        val, "custom-ua/9.9",
        "user-supplied lowercase user-agent must win over anodizer default"
    );
}

/// Mixed-case `aUtHoRiZaTiOn` likewise wins (defensive: HTTP header
/// names are case-insensitive across the spec, not just the two
/// canonical spellings).
#[test]
fn test_resolve_webhook_headers_mixed_case_authorization_wins() {
    let mut user = HashMap::new();
    user.insert("aUtHoRiZaTiOn".to_string(), "weird-case".to_string());

    let resolved = resolve_webhook_headers(user, None, Some("Bearer xyz"), "ua/1.0");

    let auth_count = resolved
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("Authorization"))
        .count();
    assert_eq!(auth_count, 1, "got {resolved:?}");

    let (_, val) = resolved
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Authorization"))
        .expect("Authorization key present");
    assert_eq!(
        val, "weird-case",
        "user-supplied mixed-case Authorization must win over bearer_token env default"
    );
}

/// When the user supplies neither, anodizer's defaults populate both
/// `Authorization` (from basic_auth) and `User-Agent`.
#[test]
fn test_resolve_webhook_headers_defaults_apply_when_user_silent() {
    let resolved =
        resolve_webhook_headers(HashMap::new(), Some("Basic abc"), None, "anodizer/1.2.3");

    assert_eq!(
        resolved.get("Authorization").map(String::as_str),
        Some("Basic abc")
    );
    assert_eq!(
        resolved.get("User-Agent").map(String::as_str),
        Some("anodizer/1.2.3")
    );
}

/// Basic auth takes priority over bearer token when both env vars are
/// set and the user has not supplied an Authorization header.
#[test]
fn test_resolve_webhook_headers_basic_auth_priority_over_bearer() {
    let resolved = resolve_webhook_headers(
        HashMap::new(),
        Some("Basic abc"),
        Some("Bearer xyz"),
        "anodizer/1.2.3",
    );

    assert_eq!(
        resolved.get("Authorization").map(String::as_str),
        Some("Basic abc"),
        "basic auth must take priority over bearer token"
    );
}

/// Cross-announcer regression pin for the `is_retriable(...root_cause())`
/// bug: the announce stage wraps `HttpError(status=503)` in an
/// `anyhow::Error` via `.context(...)`. A leaf-walking classifier
/// (`root_cause()`) misses the `HttpError` and would (incorrectly)
/// classify the failure as non-retriable. The correct API is
/// `as_ref()` (returns the top of the chain).
///
/// Failing this test on every change in announce/* retry-classifier sites
/// is what would have caught the bug at PR-time. Drift here is the canary.
#[test]
fn announce_retry_classifier_matches_5xx_via_anyhow_chain() {
    use anodizer_core::retry::{HttpError, is_retriable};
    let inner = anyhow::anyhow!("provider: HTTP 503 — body");
    let wrapped = anyhow::Error::new(HttpError::new(
        std::io::Error::other(inner.to_string()),
        503,
    ))
    .context(inner);
    assert!(
        is_retriable(wrapped.as_ref()),
        "5xx must classify retriable via as_ref()"
    );
    // Drift-guard: prove root_cause() walks past HttpError to the leaf
    // — that's the exact API mistake the announce sites had.
    assert!(
        !is_retriable(wrapped.root_cause()),
        "root_cause() reaches the leaf — wrong API for chain-walk classification"
    );
}
