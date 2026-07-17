use super::validators::{
    validate_discord_color, validate_email_from, validate_telegram_thread_id,
    validate_webhook_endpoint_url,
};
use super::*;
use crate::helpers::{DEFAULT_DISPLAY_NAME, render_message_with_default};
use anodizer_core::MapEnvSource;
use anodizer_core::config::{
    BlueskyAnnounce, Config, DiscordAnnounce, DiscourseAnnounce, EmailAnnounce, EmailEncryption,
    LinkedInAnnounce, MastodonAnnounce, MattermostAnnounce, OpenCollectiveAnnounce, RedditAnnounce,
    SlackAnnounce, StringOrBool, TeamsAnnounce, TelegramAnnounce, TwitterAnnounce, WebhookConfig,
};
use anodizer_core::context::ContextOptions;
use anodizer_core::test_helpers::scripted_responder::{ScriptedRoute, spawn_scripted_responder};
use std::collections::HashMap;
use std::time::Duration;

/// A retry policy that fails after one attempt so error-path tests don't
/// sleep through three retries against a 4xx/5xx responder.
fn no_retry() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 1,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(5),
    }
}

/// A generous aggregate deadline for tests that exercise the (fast, local)
/// scripted responders — long enough never to abandon a healthy send, so a
/// test asserting on collected errors/markers sees the real outcome.
fn test_deadline() -> Duration {
    Duration::from_secs(30)
}

/// Run one announcer end-to-end: render+enqueue via `send`, then drain the
/// queue so the network action actually fires (it now runs on a worker, not
/// inline). A render-phase failure surfaces directly; a send-phase failure
/// surfaces as the first drained error. Collapses the two-phase model back
/// into a single `Result` so the per-provider tests keep their
/// `.unwrap()` / `.unwrap_err()` shape.
fn send_drained(
    announcer: &dyn Announcer,
    ctx: &mut Context,
    announce: &AnnounceConfig,
    retry: &RetryPolicy,
    log: &StageLogger,
) -> Result<()> {
    let mut queue = DispatchQueue::new();
    announcer.send(ctx, announce, retry, log, 0, &mut queue)?;
    let out = run_queue(queue, Duration::from_secs(30));
    if let Some((_, msg)) = out.errors.into_iter().next() {
        anyhow::bail!(msg);
    }
    Ok(())
}

/// Build a live (non-dry-run) Context with the supplied announce config,
/// an empty injected env source (so process-env leakage can't satisfy a
/// credential assertion), and the standard Tag / ReleaseURL template vars.
fn live_ctx(announce: AnnounceConfig, env: &[(&str, &str)]) -> Context {
    let config = Config {
        project_name: "myapp".to_string(),
        announce: Some(announce),
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    let mut src = MapEnvSource::new();
    for (k, v) in env {
        src = src.with(*k, *v);
    }
    ctx.set_env_source(src);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    ctx.template_vars_mut().set(
        "ReleaseURL",
        "https://github.com/org/myapp/releases/tag/v1.0.0",
    );
    ctx
}

/// Build a non-live Context (no env / no network) for `enabled` and
/// `render_only` unit tests.
fn render_ctx(announce: AnnounceConfig) -> Context {
    let config = Config {
        project_name: "myapp".to_string(),
        announce: Some(announce),
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.set_env_source(MapEnvSource::new());
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    ctx.template_vars_mut().set(
        "ReleaseURL",
        "https://github.com/org/myapp/releases/tag/v1.0.0",
    );
    ctx
}

fn ok_route(path: &'static str) -> ScriptedRoute {
    ScriptedRoute {
        method: "POST",
        path_pattern: path,
        response: "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
        times: None,
    }
}

// ------------------------------------------------------------------
// Rendered-value validators (validate_discord_color,
// validate_webhook_endpoint_url, validate_telegram_thread_id,
// validate_email_from)
// ------------------------------------------------------------------

#[test]
fn discord_color_empty_render_is_unset() {
    assert_eq!(validate_discord_color("   ").unwrap(), None);
    assert_eq!(validate_discord_color("").unwrap(), None);
}

#[test]
fn discord_color_parses_in_range_value() {
    assert_eq!(
        validate_discord_color(" 16711680 ").unwrap(),
        Some(16711680)
    );
}

#[test]
fn discord_color_rejects_out_of_range() {
    let err = validate_discord_color("16777216").unwrap_err().to_string();
    assert!(err.contains("out of range"), "{err}");
}

#[test]
fn discord_color_rejects_non_integer() {
    let err = validate_discord_color("#ff0000").unwrap_err().to_string();
    assert!(err.contains("invalid color"), "{err}");
}

#[test]
fn webhook_endpoint_url_rejects_non_http_scheme() {
    let err = validate_webhook_endpoint_url("ftp://example.com/hook")
        .unwrap_err()
        .to_string();
    assert!(err.contains("must use http or https"), "{err}");
}

#[test]
fn webhook_endpoint_url_redacts_credentials_in_error() {
    // A relative URL has no host; the error must not echo the password.
    let err = validate_webhook_endpoint_url("https://user:hunter2@")
        .unwrap_err()
        .to_string();
    assert!(!err.contains("hunter2"), "password leaked: {err}");
}

#[test]
fn webhook_endpoint_url_accepts_plain_https() {
    assert!(validate_webhook_endpoint_url("https://example.com/hook").is_ok());
}

#[test]
fn telegram_thread_id_empty_is_unset() {
    assert_eq!(validate_telegram_thread_id("  ").unwrap(), None);
}

#[test]
fn telegram_thread_id_parses_negative_i64() {
    assert_eq!(validate_telegram_thread_id("-42").unwrap(), Some(-42));
}

#[test]
fn telegram_thread_id_rejects_non_integer() {
    let err = validate_telegram_thread_id("abc").unwrap_err().to_string();
    assert!(err.contains("invalid message_thread_id"), "{err}");
}

#[test]
fn email_from_rejects_missing_at_sign() {
    let err = validate_email_from("not-an-email").unwrap_err().to_string();
    assert!(err.contains("missing @"), "{err}");
}

// ------------------------------------------------------------------
// enabled() evaluation across providers
// ------------------------------------------------------------------

#[test]
fn enabled_false_when_config_block_absent() {
    let announce = AnnounceConfig::default();
    let mut ctx = render_ctx(announce.clone());
    assert!(!DiscordAnnouncer.enabled(&mut ctx, &announce).unwrap());
    assert!(!SlackAnnouncer.enabled(&mut ctx, &announce).unwrap());
    assert!(!EmailAnnouncer.enabled(&mut ctx, &announce).unwrap());
}

#[test]
fn enabled_respects_bool_flag() {
    let announce = AnnounceConfig {
        discord: Some(DiscordAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            webhook_url: Some("http://x/y".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = render_ctx(announce.clone());
    assert!(DiscordAnnouncer.enabled(&mut ctx, &announce).unwrap());
}

#[test]
fn enabled_renders_template_to_truthy() {
    // The `enabled` string must be rendered through the template engine,
    // not compared raw: a var that renders to "true" enables the provider.
    let announce = AnnounceConfig {
        slack: Some(SlackAnnounce {
            enabled: Some(StringOrBool::String("{{ AnnounceOn }}".to_string())),
            webhook_url: Some("http://x/y".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = render_ctx(announce.clone());
    ctx.template_vars_mut().set("AnnounceOn", "true");
    assert!(SlackAnnouncer.enabled(&mut ctx, &announce).unwrap());
}

#[test]
fn enabled_template_falsy_disables() {
    let announce = AnnounceConfig {
        slack: Some(SlackAnnounce {
            enabled: Some(StringOrBool::String("{{ AnnounceOn }}".to_string())),
            webhook_url: Some("http://x/y".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = render_ctx(announce.clone());
    ctx.template_vars_mut().set("AnnounceOn", "false");
    assert!(!SlackAnnouncer.enabled(&mut ctx, &announce).unwrap());
}

#[test]
fn enabled_propagates_template_error() {
    let announce = AnnounceConfig {
        slack: Some(SlackAnnounce {
            enabled: Some(StringOrBool::String("{{ NoSuchVar }}".to_string())),
            webhook_url: Some("http://x/y".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = render_ctx(announce.clone());
    assert!(SlackAnnouncer.enabled(&mut ctx, &announce).is_err());
}

// ------------------------------------------------------------------
// run_announcer / dispatch loop
// ------------------------------------------------------------------

#[test]
fn dispatch_loop_never_sends_for_disabled_announcer() {
    // A disabled announcer with a bogus URL must never reach `send`
    // (no panic from the unroutable host, no error collected). The
    // enabled gate lives in `enabled_announcers` (evaluated once per
    // dispatch), so the pin drives the real dispatch loop.
    let announce = AnnounceConfig {
        slack: Some(SlackAnnounce {
            enabled: Some(StringOrBool::Bool(false)),
            webhook_url: Some("http://127.0.0.1:1/never".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let log = ctx.logger("announce");
    let mut errors = vec![];
    dispatch_all_announcers(
        &mut ctx,
        &announce,
        &no_retry(),
        &log,
        test_deadline(),
        &mut errors,
        None,
    )
    .unwrap();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn enabled_announcers_respect_include_and_skip_filters() {
    // Two enabled providers; the filter narrows which ones fire and
    // therefore which names the shared kv pad width is computed from.
    let announce = AnnounceConfig {
        slack: Some(SlackAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            webhook_url: Some("http://127.0.0.1:1/never".to_string()),
            ..Default::default()
        }),
        telegram: Some(TelegramAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);

    let all = enabled_announcers(&mut ctx, &announce, None).unwrap();
    let names: Vec<&str> = all.iter().map(|a| a.name()).collect();
    assert_eq!(names, vec!["slack", "telegram"], "registry order");
    assert_eq!(shared_key_width(&all), "telegram".len());

    let only_slack = enabled_announcers(
        &mut ctx,
        &announce,
        Some(&AnnounceFilter {
            include: Some(&["slack"]),
            skip: &[],
        }),
    )
    .unwrap();
    assert_eq!(only_slack.len(), 1);
    assert_eq!(only_slack[0].name(), "slack");
    assert_eq!(shared_key_width(&only_slack), "slack".len());

    let skip_slack = enabled_announcers(
        &mut ctx,
        &announce,
        Some(&AnnounceFilter {
            include: None,
            skip: &["slack"],
        }),
    )
    .unwrap();
    assert_eq!(skip_slack.len(), 1);
    assert_eq!(skip_slack[0].name(), "telegram");
}

#[test]
fn enabled_template_error_aborts_before_any_send() {
    // Discord registers BEFORE slack in the dispatch order. With the
    // single up-front `enabled:` evaluation, slack's broken template
    // must abort the whole dispatch before discord attempts its send
    // — a half-dispatched section would otherwise leave earlier
    // channels posted and later ones dead. An attempted discord send
    // would have pushed a per-provider entry into `errors`.
    let announce = AnnounceConfig {
        discord: Some(DiscordAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            ..Default::default()
        }),
        slack: Some(SlackAnnounce {
            enabled: Some(StringOrBool::String("{{ NoSuchVar }}".to_string())),
            webhook_url: Some("http://127.0.0.1:1/never".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let log = ctx.logger("announce");
    let mut errors = vec![];
    dispatch_all_announcers(
        &mut ctx,
        &announce,
        &no_retry(),
        &log,
        test_deadline(),
        &mut errors,
        None,
    )
    .expect_err("broken enabled: template must abort dispatch");
    assert!(
        errors.is_empty(),
        "no announcer may have attempted a send before the abort: {errors:?}"
    );
}

#[test]
fn dispatch_collects_send_error_with_provider_prefix() {
    // Point slack at a 500 responder: the send fails, and the error must
    // be collected (not propagated) and prefixed with the provider name.
    let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/hook",
        response: "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 4\r\n\r\nboom",
        times: None,
    }]);
    let announce = AnnounceConfig {
        slack: Some(SlackAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            webhook_url: Some(format!("http://{addr}/hook")),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let log = ctx.logger("announce");
    let mut errors = vec![];
    dispatch_all_announcers(
        &mut ctx,
        &announce,
        &no_retry(),
        &log,
        test_deadline(),
        &mut errors,
        None,
    )
    .unwrap();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].starts_with("slack: "), "{}", errors[0]);
}

#[test]
fn dispatch_skips_already_sent_on_rerun() {
    // First run posts to slack; the marker records it. A second run with
    // the SAME dist/version (simulating a re-run) must skip the post — the
    // responder sees exactly one request.
    let (addr, req_log) = spawn_scripted_responder(vec![ok_route("/hook")]);
    let announce = AnnounceConfig {
        slack: Some(SlackAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            webhook_url: Some(format!("http://{addr}/hook")),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let log = ctx.logger("announce");
    let dist = tempfile::tempdir().unwrap();

    let mut errors = vec![];
    // First dispatch: posts and records the send (marker fold runs after
    // the concurrent join inside dispatch_all_announcers).
    {
        let mut marker = crate::sent_marker::AnnounceSentMarker::load(dist.path(), "1.0.0", &log);
        dispatch_all_announcers(
            &mut ctx,
            &announce,
            &no_retry(),
            &log,
            test_deadline(),
            &mut errors,
            Some(&mut marker),
        )
        .unwrap();
    }
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(req_log.lock().unwrap().len(), 1, "first run posts once");

    // Second dispatch: a fresh marker loaded from the same dist/version
    // sees the prior send and skips — no second POST.
    {
        let mut marker = crate::sent_marker::AnnounceSentMarker::load(dist.path(), "1.0.0", &log);
        dispatch_all_announcers(
            &mut ctx,
            &announce,
            &no_retry(),
            &log,
            test_deadline(),
            &mut errors,
            Some(&mut marker),
        )
        .unwrap();
    }
    assert!(errors.is_empty(), "re-run is a clean skip: {errors:?}");
    assert_eq!(
        req_log.lock().unwrap().len(),
        1,
        "re-run must NOT re-post — still exactly one request"
    );
}

#[test]
fn dispatch_failed_send_is_not_marked_sent() {
    // A failed send must NOT be recorded — a subsequent re-run must retry
    // it (a dropped announcement is worse than a duplicate).
    let (addr, _l) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/hook",
        response: "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 4\r\n\r\nboom",
        times: None,
    }]);
    let announce = AnnounceConfig {
        slack: Some(SlackAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            webhook_url: Some(format!("http://{addr}/hook")),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let log = ctx.logger("announce");
    let dist = tempfile::tempdir().unwrap();
    let mut errors = vec![];
    let mut marker = crate::sent_marker::AnnounceSentMarker::load(dist.path(), "1.0.0", &log);
    dispatch_all_announcers(
        &mut ctx,
        &announce,
        &no_retry(),
        &log,
        test_deadline(),
        &mut errors,
        Some(&mut marker),
    )
    .unwrap();
    assert_eq!(errors.len(), 1, "send failed: {errors:?}");
    assert!(
        !marker.already_sent("slack"),
        "a failed send must NOT be recorded as sent"
    );
}

#[test]
fn enabled_announcers_propagate_enabled_template_error() {
    let announce = AnnounceConfig {
        slack: Some(SlackAnnounce {
            enabled: Some(StringOrBool::String("{{ NoSuchVar }}".to_string())),
            webhook_url: Some("http://x/y".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    // enabled() errors must bubble up as the resolver's Err, not be
    // swallowed into an empty active set.
    assert!(enabled_announcers(&mut ctx, &announce, None).is_err());
}

#[test]
fn dispatch_all_collects_errors_from_two_failing_providers() {
    // Slack (500) and Teams (500) both fail; discord is disabled and
    // must not appear. The dispatch loop continues past the first
    // failure and collects both, in registry order (discord before
    // slack before teams).
    let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/hook",
        response: "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
        times: None,
    }]);
    let url = format!("http://{addr}/hook");
    let announce = AnnounceConfig {
        discord: Some(DiscordAnnounce {
            enabled: Some(StringOrBool::Bool(false)),
            webhook_url: Some(url.clone()),
            ..Default::default()
        }),
        slack: Some(SlackAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            webhook_url: Some(url.clone()),
            ..Default::default()
        }),
        teams: Some(TeamsAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            webhook_url: Some(url.clone()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let log = ctx.logger("announce");
    let mut errors = vec![];
    dispatch_all_announcers(
        &mut ctx,
        &announce,
        &no_retry(),
        &log,
        test_deadline(),
        &mut errors,
        None,
    )
    .unwrap();
    assert_eq!(errors.len(), 2, "{errors:?}");
    // Errors arrive in concurrent-completion order, not registry order, so
    // assert on membership rather than position.
    assert!(
        errors.iter().any(|e| e.starts_with("slack: ")),
        "{errors:?}"
    );
    assert!(
        errors.iter().any(|e| e.starts_with("teams: ")),
        "{errors:?}"
    );
}

// ------------------------------------------------------------------
// Live send() request-body assertions (the major uncovered surface)
// ------------------------------------------------------------------

/// Slack `send` resolves `webhook_url`, renders the message, and POSTs the
/// slack JSON envelope with channel/username overrides wired in.
#[test]
fn slack_send_posts_rendered_message_and_channel() {
    let (addr, log) = spawn_scripted_responder(vec![ok_route("/services/T000")]);
    let announce = AnnounceConfig {
        slack: Some(SlackAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            webhook_url: Some(format!("http://{addr}/services/T000")),
            message_template: Some("{{ ProjectName }} {{ Tag }} released!".to_string()),
            channel: Some("release-{{ Tag }}".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let logger = ctx.logger("announce");
    send_drained(&SlackAnnouncer, &mut ctx, &announce, &no_retry(), &logger).unwrap();
    let entries = log.lock().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path, "/services/T000");
    let body: serde_json::Value = serde_json::from_str(&entries[0].body).unwrap();
    assert_eq!(body["text"], "myapp v1.0.0 released!");
    assert_eq!(body["channel"], "release-v1.0.0");
}

/// Slack falls back to the `SLACK_WEBHOOK` env var when `webhook_url` is
/// absent.
#[test]
fn slack_send_uses_slack_webhook_env_fallback() {
    let (addr, log) = spawn_scripted_responder(vec![ok_route("/env-hook")]);
    let announce = AnnounceConfig {
        slack: Some(SlackAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            webhook_url: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let env_url = format!("http://{addr}/env-hook");
    let mut ctx = live_ctx(announce.clone(), &[("SLACK_WEBHOOK", &env_url)]);
    let logger = ctx.logger("announce");
    send_drained(&SlackAnnouncer, &mut ctx, &announce, &no_retry(), &logger).unwrap();
    assert_eq!(log.lock().unwrap()[0].path, "/env-hook");
}

/// Discord `send` defaults the author to the brand display name and POSTs
/// an embed envelope (not a plain `content` field).
#[test]
fn discord_send_posts_embed_with_default_author() {
    let (addr, log) = spawn_scripted_responder(vec![ok_route("/hook")]);
    let announce = AnnounceConfig {
        discord: Some(DiscordAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            webhook_url: Some(format!("http://{addr}/hook")),
            message_template: Some("{{ Tag }} is live".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let logger = ctx.logger("announce");
    send_drained(&DiscordAnnouncer, &mut ctx, &announce, &no_retry(), &logger).unwrap();
    let entries = log.lock().unwrap();
    let body: serde_json::Value = serde_json::from_str(&entries[0].body).unwrap();
    assert!(body.get("content").is_none(), "{body}");
    let embed = &body["embeds"][0];
    assert_eq!(embed["description"], "v1.0.0 is live");
    assert_eq!(embed["author"]["name"], DEFAULT_DISPLAY_NAME);
}

/// Discord builds the webhook URL from `DISCORD_WEBHOOK_ID` /
/// `DISCORD_WEBHOOK_TOKEN` against the `DISCORD_API` base, percent-encoding
/// the id/token path segments.
#[test]
fn discord_send_builds_url_from_id_token_env() {
    let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/webhooks/wid/wtok",
        response: "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
        times: None,
    }]);
    let announce = AnnounceConfig {
        discord: Some(DiscordAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            webhook_url: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let base = format!("http://{addr}");
    let mut ctx = live_ctx(
        announce.clone(),
        &[
            ("DISCORD_API", &base),
            ("DISCORD_WEBHOOK_ID", "wid"),
            ("DISCORD_WEBHOOK_TOKEN", "wtok"),
        ],
    );
    let logger = ctx.logger("announce");
    send_drained(&DiscordAnnouncer, &mut ctx, &announce, &no_retry(), &logger).unwrap();
    assert_eq!(log.lock().unwrap()[0].path, "/webhooks/wid/wtok");
}

/// Discord rejects a `color` template that renders out of the 24-bit range
/// before any send fires.
#[test]
fn discord_send_rejects_invalid_color() {
    let announce = AnnounceConfig {
        discord: Some(DiscordAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            webhook_url: Some("http://127.0.0.1:1/x".to_string()),
            color: Some("99999999".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let logger = ctx.logger("announce");
    let err = send_drained(&DiscordAnnouncer, &mut ctx, &announce, &no_retry(), &logger)
        .unwrap_err()
        .to_string();
    assert!(err.contains("out of range"), "{err}");
}

/// Teams `send` resolves the webhook, renders the default title, and POSTs
/// the adaptive-card envelope carrying the message text.
#[test]
fn teams_send_posts_card_with_message() {
    let (addr, log) = spawn_scripted_responder(vec![ok_route("/teams")]);
    let announce = AnnounceConfig {
        teams: Some(TeamsAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            webhook_url: Some(format!("http://{addr}/teams")),
            message_template: Some("{{ ProjectName }} shipped".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let logger = ctx.logger("announce");
    send_drained(&TeamsAnnouncer, &mut ctx, &announce, &no_retry(), &logger).unwrap();
    let entries = log.lock().unwrap();
    assert_eq!(entries[0].path, "/teams");
    assert!(
        entries[0].body.contains("myapp shipped"),
        "message in card: {}",
        entries[0].body
    );
}

/// Mattermost `send` renders the `channel` template (anodizer-additive UX)
/// and POSTs it in the payload.
#[test]
fn mattermost_send_renders_channel_template() {
    let (addr, log) = spawn_scripted_responder(vec![ok_route("/mm")]);
    let announce = AnnounceConfig {
        mattermost: Some(MattermostAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            webhook_url: Some(format!("http://{addr}/mm")),
            channel: Some("rel-{{ Tag }}".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let logger = ctx.logger("announce");
    send_drained(
        &MattermostAnnouncer,
        &mut ctx,
        &announce,
        &no_retry(),
        &logger,
    )
    .unwrap();
    let body: serde_json::Value = serde_json::from_str(&log.lock().unwrap()[0].body).unwrap();
    assert_eq!(body["channel"], "rel-v1.0.0");
}

/// Webhook `send` renders a templated custom header (exercising the
/// per-header render loop) and POSTs the rendered message body. The
/// responder log captures method/path/body, so the body is asserted
/// directly; the header render path runs without panicking.
#[test]
fn webhook_send_renders_headers_and_posts_message_body() {
    let (addr, log) = spawn_scripted_responder(vec![ok_route("/wh")]);
    let mut headers = HashMap::new();
    headers.insert("X-Release-Tag".to_string(), "tag-{{ Tag }}".to_string());
    let announce = AnnounceConfig {
        webhook: Some(WebhookConfig {
            enabled: Some(StringOrBool::Bool(true)),
            endpoint_url: Some(format!("http://{addr}/wh")),
            headers: Some(headers),
            content_type: None,
            message_template: Some("body-{{ Tag }}".to_string()),
            skip_tls_verify: None,
            expected_status_codes: vec![],
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let logger = ctx.logger("announce");
    send_drained(&WebhookAnnouncer, &mut ctx, &announce, &no_retry(), &logger).unwrap();
    let entries = log.lock().unwrap();
    assert_eq!(entries[0].path, "/wh");
    assert_eq!(entries[0].body, "body-v1.0.0");
}

/// Webhook `send` honors a non-default `expected_status_codes` list: a 202
/// response that would normally be unexpected is accepted.
#[test]
fn webhook_send_accepts_configured_expected_status() {
    let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/wh",
        response: "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n",
        times: None,
    }]);
    let announce = AnnounceConfig {
        webhook: Some(WebhookConfig {
            enabled: Some(StringOrBool::Bool(true)),
            endpoint_url: Some(format!("http://{addr}/wh")),
            headers: None,
            content_type: None,
            message_template: Some("hi".to_string()),
            skip_tls_verify: None,
            expected_status_codes: vec![202],
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let logger = ctx.logger("announce");
    send_drained(&WebhookAnnouncer, &mut ctx, &announce, &no_retry(), &logger).unwrap();
}

/// Webhook `send` rejects a rendered endpoint with a non-http scheme
/// before firing.
#[test]
fn webhook_send_rejects_bad_scheme() {
    let announce = AnnounceConfig {
        webhook: Some(WebhookConfig {
            enabled: Some(StringOrBool::Bool(true)),
            endpoint_url: Some("ftp://host/x".to_string()),
            headers: None,
            content_type: None,
            message_template: None,
            skip_tls_verify: None,
            expected_status_codes: vec![],
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let logger = ctx.logger("announce");
    let err = send_drained(&WebhookAnnouncer, &mut ctx, &announce, &no_retry(), &logger)
        .unwrap_err()
        .to_string();
    assert!(err.contains("must use http or https"), "{err}");
}

/// Discourse `send` POSTs to `<server>/posts.json` with the API-key header
/// and a category id wired in.
#[test]
fn discourse_send_posts_to_posts_json() {
    let (addr, log) = spawn_scripted_responder(vec![ok_route("/posts.json")]);
    let announce = AnnounceConfig {
        discourse: Some(DiscourseAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            server: Some(format!("http://{addr}")),
            category_id: Some(5),
            username: None,
            title_template: None,
            message_template: Some("body {{ Tag }}".to_string()),
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[("DISCOURSE_API_KEY", "k")]);
    let logger = ctx.logger("announce");
    send_drained(
        &DiscourseAnnouncer,
        &mut ctx,
        &announce,
        &no_retry(),
        &logger,
    )
    .unwrap();
    let entries = log.lock().unwrap();
    assert_eq!(entries[0].path, "/posts.json");
    assert!(
        entries[0].body.contains("body v1.0.0"),
        "{}",
        entries[0].body
    );
}

/// Discourse `send` hard-bails (regardless of strict mode) when
/// `category_id` is configured as zero — a config error, not
/// skip-when-empty.
#[test]
fn discourse_send_rejects_zero_category_id() {
    let announce = AnnounceConfig {
        discourse: Some(DiscourseAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            server: Some("http://127.0.0.1:1".to_string()),
            category_id: Some(0),
            username: None,
            title_template: None,
            message_template: None,
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[("DISCOURSE_API_KEY", "k")]);
    let logger = ctx.logger("announce");
    let err = send_drained(
        &DiscourseAnnouncer,
        &mut ctx,
        &announce,
        &no_retry(),
        &logger,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("category_id must be non-zero"), "{err}");
}

/// Telegram `send` warn-and-skips in normal mode when `chat_id` is absent:
/// no network call, Ok result (skip-when-empty UX), even though bot_token
/// is present.
#[test]
fn telegram_send_missing_chat_id_warn_and_skips() {
    // Non-strict mode: a missing chat_id warns and skips cleanly — no
    // network call, Ok result.
    let announce = AnnounceConfig {
        telegram: Some(TelegramAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            bot_token: Some("123:ABC".to_string()),
            chat_id: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let logger = ctx.logger("announce");
    send_drained(
        &TelegramAnnouncer,
        &mut ctx,
        &announce,
        &no_retry(),
        &logger,
    )
    .expect("missing chat_id must skip cleanly in normal mode");
}

/// Mastodon `send` fail-fasts when MASTODON_CLIENT_ID is unset even though
/// MASTODON_ACCESS_TOKEN is present (all three OAuth fields are required).
#[test]
fn mastodon_send_requires_all_three_credentials() {
    let announce = AnnounceConfig {
        mastodon: Some(MastodonAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            server: Some("https://mastodon.example".to_string()),
            message_template: None,
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[("MASTODON_ACCESS_TOKEN", "tok")]);
    let logger = ctx.logger("announce");
    let err = send_drained(
        &MastodonAnnouncer,
        &mut ctx,
        &announce,
        &no_retry(),
        &logger,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("MASTODON_CLIENT_ID"), "{err}");
}

/// Email `send` routes through SMTP when a host is configured, and bails
/// when the SMTP username is missing (host present, no username).
#[test]
fn email_send_smtp_requires_username() {
    let announce = AnnounceConfig {
        email: Some(EmailAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            host: Some("smtp.example.com".to_string()),
            from: Some("rel@example.com".to_string()),
            to: vec!["dev@example.com".to_string()],
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let logger = ctx.logger("announce");
    let err = send_drained(&EmailAnnouncer, &mut ctx, &announce, &no_retry(), &logger)
        .unwrap_err()
        .to_string();
    assert!(err.contains("SMTP username is required"), "{err}");
}

/// Email `send` hard-bails on a malformed `from` (no `@`) even in normal
/// mode — it's a config error, not skip-when-empty.
#[test]
fn email_send_rejects_malformed_from() {
    let announce = AnnounceConfig {
        email: Some(EmailAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            from: Some("noatsign".to_string()),
            to: vec!["dev@example.com".to_string()],
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let logger = ctx.logger("announce");
    let err = send_drained(&EmailAnnouncer, &mut ctx, &announce, &no_retry(), &logger)
        .unwrap_err()
        .to_string();
    assert!(err.contains("missing @"), "{err}");
}

// ------------------------------------------------------------------
// render_only / render_all_announcers (the pre-publish guard)
// ------------------------------------------------------------------

/// A broken `message_template` (undefined var) surfaces from
/// `render_only` as an Err without any network call.
#[test]
fn render_only_surfaces_broken_message_template() {
    let announce = AnnounceConfig {
        slack: Some(SlackAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            webhook_url: Some("http://x/y".to_string()),
            message_template: Some("{{ NoSuchVar }}".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = render_ctx(announce.clone());
    assert!(SlackAnnouncer.render_only(&mut ctx, &announce).is_err());
}

/// `literal_message` makes email's BODY render a no-op end-to-end: a body
/// that would error (or expand a secret) when rendered passes untouched,
/// proving email's send/render_only routes its body through the
/// `render_message` chokepoint rather than a raw `render_template`. Guards
/// against the leak reopening on a provider we dogfood as an on_error hook.
#[test]
fn email_body_render_is_literal_under_literal_message() {
    let announce = AnnounceConfig {
        email: Some(EmailAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            from: Some("bot@example.com".to_string()),
            to: vec!["dev@example.com".to_string()],
            // An undefined var would surface from a real body render; in
            // literal mode the body is never rendered, so it does not.
            message_template: Some("{{ NoSuchVar }}".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = render_ctx(announce.clone());
    assert!(
        EmailAnnouncer.render_only(&mut ctx, &announce).is_err(),
        "control: a broken body must surface when rendered"
    );
    ctx.literal_message = true;
    assert!(
        EmailAnnouncer.render_only(&mut ctx, &announce).is_ok(),
        "literal_message must skip the email body render — no expansion, no leak"
    );
}

/// Default body redaction (`redact_body == true`) masks a known-secret env
/// value that a templated `Env`-reference (or untrusted on_error text)
/// smuggled into the body, so the channel never receives the raw secret.
/// Drives the universal body chokepoint directly.
#[test]
fn render_message_with_default_redacts_secret_by_default() {
    let mut ctx = render_ctx(AnnounceConfig::default());
    ctx.template_vars_mut()
        .set_env("CARGO_REGISTRY_TOKEN", "ghp_realsecretvalue");
    // literal_message so the secret travels in the body verbatim up to the
    // redaction step — this isolates redaction from Tera expansion.
    ctx.literal_message = true;
    assert!(ctx.redact_body, "default must be redact-on");
    let out = render_message_with_default(
        &mut ctx,
        Some("release done: ghp_realsecretvalue shipped"),
        "default",
    )
    .unwrap();
    assert_eq!(out, "release done: $CARGO_REGISTRY_TOKEN shipped");
    assert!(
        !out.contains("ghp_realsecretvalue"),
        "raw secret must not reach the channel: {out}"
    );
}

/// `redact_body == false` (the `--allow-secrets` path) sends the secret
/// verbatim — the deliberate opt-out for a trusted private channel.
#[test]
fn render_message_with_default_allow_secrets_keeps_verbatim() {
    let mut ctx = render_ctx(AnnounceConfig::default());
    ctx.template_vars_mut()
        .set_env("CARGO_REGISTRY_TOKEN", "ghp_realsecretvalue");
    ctx.literal_message = true;
    ctx.redact_body = false;
    let out = render_message_with_default(
        &mut ctx,
        Some("release done: ghp_realsecretvalue shipped"),
        "default",
    )
    .unwrap();
    assert_eq!(
        out, "release done: ghp_realsecretvalue shipped",
        "--allow-secrets must leave the secret untouched"
    );
}

/// Same regression guard for opencollective's body render.
#[test]
fn opencollective_body_render_is_literal_under_literal_message() {
    let announce = AnnounceConfig {
        opencollective: Some(OpenCollectiveAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            message_template: Some("{{ NoSuchVar }}".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = render_ctx(announce.clone());
    assert!(
        OpenCollectiveAnnouncer
            .render_only(&mut ctx, &announce)
            .is_err(),
        "control: a broken body must surface when rendered"
    );
    ctx.literal_message = true;
    assert!(
        OpenCollectiveAnnouncer
            .render_only(&mut ctx, &announce)
            .is_ok(),
        "literal_message must skip the opencollective body render"
    );
}

/// `render_only` runs the SAME color validator `send` does: an
/// out-of-range color template is caught by the guard.
#[test]
fn render_only_validates_discord_color() {
    let announce = AnnounceConfig {
        discord: Some(DiscordAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            webhook_url: Some("http://x/y".to_string()),
            color: Some("{{ 16777216 }}".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = render_ctx(announce.clone());
    let err = DiscordAnnouncer
        .render_only(&mut ctx, &announce)
        .unwrap_err()
        .to_string();
    assert!(err.contains("out of range"), "{err}");
}

/// `render_all_announcers` skips a disabled announcer's render entirely:
/// a disabled slack with a broken template must NOT surface an error,
/// matching the live dispatch loop's skip behavior.
#[test]
fn render_all_skips_disabled_announcer() {
    let announce = AnnounceConfig {
        slack: Some(SlackAnnounce {
            enabled: Some(StringOrBool::Bool(false)),
            webhook_url: Some("http://x/y".to_string()),
            message_template: Some("{{ NoSuchVar }}".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = render_ctx(announce.clone());
    let mut errors = vec![];
    render_all_announcers(&mut ctx, &announce, &mut errors).unwrap();
    assert!(errors.is_empty(), "{errors:?}");
}

/// `render_all_announcers` collects a per-provider error (prefixed with
/// the provider name) for an ENABLED announcer whose template is broken.
#[test]
fn render_all_collects_enabled_provider_render_error() {
    let announce = AnnounceConfig {
        webhook: Some(WebhookConfig {
            enabled: Some(StringOrBool::Bool(true)),
            endpoint_url: Some("{{ NoSuchVar }}".to_string()),
            headers: None,
            content_type: None,
            message_template: None,
            skip_tls_verify: None,
            expected_status_codes: vec![],
        }),
        ..Default::default()
    };
    let mut ctx = render_ctx(announce.clone());
    let mut errors = vec![];
    render_all_announcers(&mut ctx, &announce, &mut errors).unwrap();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].starts_with("webhook: "), "{}", errors[0]);
}

// ------------------------------------------------------------------
// send() skip-when-empty / env-fallback / default-template paths
// ------------------------------------------------------------------

/// Discord `send` warn-and-skips (normal mode) when neither `webhook_url`
/// nor the ID/TOKEN env pair is present: no network call, Ok result, no
/// panic against a missing URL.
#[test]
fn discord_send_missing_webhook_warn_and_skips() {
    let announce = AnnounceConfig {
        discord: Some(DiscordAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            webhook_url: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let logger = ctx.logger("announce");
    send_drained(&DiscordAnnouncer, &mut ctx, &announce, &no_retry(), &logger)
        .expect("missing webhook_url must skip cleanly in normal mode");
}

/// Discourse `send` warn-and-skips when `server` is configured but renders
/// empty — distinct from the missing-server skip.
#[test]
fn discourse_send_empty_server_warn_and_skips() {
    let announce = AnnounceConfig {
        discourse: Some(DiscourseAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            server: Some("".to_string()),
            category_id: Some(5),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[("DISCOURSE_API_KEY", "k")]);
    let logger = ctx.logger("announce");
    send_drained(
        &DiscourseAnnouncer,
        &mut ctx,
        &announce,
        &no_retry(),
        &logger,
    )
    .expect("empty server must skip cleanly in normal mode");
}

/// Discourse `send` warn-and-skips when `category_id` is absent.
#[test]
fn discourse_send_missing_category_id_warn_and_skips() {
    let announce = AnnounceConfig {
        discourse: Some(DiscourseAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            server: Some("http://127.0.0.1:1".to_string()),
            category_id: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[("DISCOURSE_API_KEY", "k")]);
    let logger = ctx.logger("announce");
    send_drained(
        &DiscourseAnnouncer,
        &mut ctx,
        &announce,
        &no_retry(),
        &logger,
    )
    .expect("missing category_id must skip cleanly in normal mode");
}

/// Discourse `send` posts the rendered default title (`{{ ProjectName }}
/// {{ Tag }} is out!`) when `title_template` is unset, exercising the
/// default-title branch.
#[test]
fn discourse_send_uses_default_title() {
    let (addr, log) = spawn_scripted_responder(vec![ok_route("/posts.json")]);
    let announce = AnnounceConfig {
        discourse: Some(DiscourseAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            server: Some(format!("http://{addr}")),
            category_id: Some(7),
            title_template: None,
            message_template: Some("body".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[("DISCOURSE_API_KEY", "k")]);
    let logger = ctx.logger("announce");
    send_drained(
        &DiscourseAnnouncer,
        &mut ctx,
        &announce,
        &no_retry(),
        &logger,
    )
    .unwrap();
    let entries = log.lock().unwrap();
    assert!(
        entries[0].body.contains("myapp v1.0.0 is out!"),
        "default title in body: {}",
        entries[0].body
    );
}

/// Webhook `send` warn-and-skips when `endpoint_url` is absent.
#[test]
fn webhook_send_missing_endpoint_warn_and_skips() {
    let announce = AnnounceConfig {
        webhook: Some(WebhookConfig {
            enabled: Some(StringOrBool::Bool(true)),
            endpoint_url: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let logger = ctx.logger("announce");
    send_drained(&WebhookAnnouncer, &mut ctx, &announce, &no_retry(), &logger)
        .expect("missing endpoint_url must skip cleanly in normal mode");
}

/// Webhook `send` posts the rendered DEFAULT (JSON-envelope) message body
/// when `message_template` is unset, exercising the
/// `WEBHOOK_DEFAULT_MESSAGE_TEMPLATE` branch — the body must be parseable
/// JSON referencing the release tag.
#[test]
fn webhook_send_uses_default_json_envelope_message() {
    let (addr, log) = spawn_scripted_responder(vec![ok_route("/wh")]);
    let announce = AnnounceConfig {
        webhook: Some(WebhookConfig {
            enabled: Some(StringOrBool::Bool(true)),
            endpoint_url: Some(format!("http://{addr}/wh")),
            message_template: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let logger = ctx.logger("announce");
    send_drained(&WebhookAnnouncer, &mut ctx, &announce, &no_retry(), &logger).unwrap();
    let entries = log.lock().unwrap();
    let body: serde_json::Value = serde_json::from_str(&entries[0].body).unwrap_or_else(|e| {
        panic!(
            "default webhook body must be JSON: {e}: {}",
            entries[0].body
        )
    });
    assert!(
        body.to_string().contains("v1.0.0"),
        "default body references the tag: {body}"
    );
}

/// Telegram `send` falls back to the `TELEGRAM_TOKEN` env var for the bot
/// token, then warn-and-skips on the still-missing `chat_id` — proving the
/// token-env-fallback branch ran (no credential error surfaced).
#[test]
fn telegram_send_uses_token_env_then_skips_on_missing_chat_id() {
    let announce = AnnounceConfig {
        telegram: Some(TelegramAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            bot_token: None,
            chat_id: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[("TELEGRAM_TOKEN", "123:ABC")]);
    let logger = ctx.logger("announce");
    send_drained(
        &TelegramAnnouncer,
        &mut ctx,
        &announce,
        &no_retry(),
        &logger,
    )
    .expect("token from env, then skip on missing chat_id");
}

/// Telegram `send` warn-and-skips when no bot token is available (neither
/// config nor `TELEGRAM_TOKEN` env).
#[test]
fn telegram_send_missing_bot_token_warn_and_skips() {
    let announce = AnnounceConfig {
        telegram: Some(TelegramAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            bot_token: None,
            chat_id: Some("42".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let logger = ctx.logger("announce");
    send_drained(
        &TelegramAnnouncer,
        &mut ctx,
        &announce,
        &no_retry(),
        &logger,
    )
    .expect("missing bot_token must skip cleanly in normal mode");
}

/// Teams `send` falls back to the `TEAMS_WEBHOOK` env var and POSTs the
/// rendered card to it.
#[test]
fn teams_send_uses_teams_webhook_env_fallback() {
    let (addr, log) = spawn_scripted_responder(vec![ok_route("/teams-env")]);
    let announce = AnnounceConfig {
        teams: Some(TeamsAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            webhook_url: None,
            message_template: Some("hi".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let env_url = format!("http://{addr}/teams-env");
    let mut ctx = live_ctx(announce.clone(), &[("TEAMS_WEBHOOK", &env_url)]);
    let logger = ctx.logger("announce");
    send_drained(&TeamsAnnouncer, &mut ctx, &announce, &no_retry(), &logger).unwrap();
    assert_eq!(log.lock().unwrap()[0].path, "/teams-env");
}

/// Teams `send` warn-and-skips when neither `webhook_url` nor
/// `TEAMS_WEBHOOK` is present.
#[test]
fn teams_send_missing_webhook_warn_and_skips() {
    let announce = AnnounceConfig {
        teams: Some(TeamsAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            webhook_url: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let logger = ctx.logger("announce");
    send_drained(&TeamsAnnouncer, &mut ctx, &announce, &no_retry(), &logger)
        .expect("missing teams webhook must skip cleanly in normal mode");
}

/// Mattermost `send` warn-and-skips when neither `webhook_url` nor
/// `MATTERMOST_WEBHOOK` is present.
#[test]
fn mattermost_send_missing_webhook_warn_and_skips() {
    let announce = AnnounceConfig {
        mattermost: Some(MattermostAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            webhook_url: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let logger = ctx.logger("announce");
    send_drained(
        &MattermostAnnouncer,
        &mut ctx,
        &announce,
        &no_retry(),
        &logger,
    )
    .expect("missing mattermost webhook must skip cleanly in normal mode");
}

/// Mattermost `send` defaults `color` to `#2D313E` and emits it in the
/// attachment payload when the config sets no color.
#[test]
fn mattermost_send_emits_default_color() {
    let (addr, log) = spawn_scripted_responder(vec![ok_route("/mm")]);
    let announce = AnnounceConfig {
        mattermost: Some(MattermostAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            webhook_url: Some(format!("http://{addr}/mm")),
            color: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let logger = ctx.logger("announce");
    send_drained(
        &MattermostAnnouncer,
        &mut ctx,
        &announce,
        &no_retry(),
        &logger,
    )
    .unwrap();
    let body: serde_json::Value = serde_json::from_str(&log.lock().unwrap()[0].body).unwrap();
    assert_eq!(body["attachments"][0]["color"], "#2D313E");
}

/// Reddit `send` warn-and-skips when `application_id` is absent.
#[test]
fn reddit_send_missing_application_id_warn_and_skips() {
    let announce = AnnounceConfig {
        reddit: Some(RedditAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            application_id: None,
            username: Some("u".to_string()),
            sub: Some("rust".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let logger = ctx.logger("announce");
    send_drained(&RedditAnnouncer, &mut ctx, &announce, &no_retry(), &logger)
        .expect("missing application_id must skip cleanly in normal mode");
}

/// Reddit `send` hard-bails (config error, not skip-when-empty) when the
/// required `REDDIT_SECRET` / `REDDIT_PASSWORD` env vars are missing, even
/// though all config fields are present.
#[test]
fn reddit_send_missing_credentials_bails() {
    let announce = AnnounceConfig {
        reddit: Some(RedditAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            application_id: Some("app".to_string()),
            username: Some("u".to_string()),
            sub: Some("rust".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let logger = ctx.logger("announce");
    let err = send_drained(&RedditAnnouncer, &mut ctx, &announce, &no_retry(), &logger)
        .unwrap_err()
        .to_string();
    assert!(err.contains("REDDIT_SECRET"), "{err}");
}

/// Twitter `send` hard-bails when the four OAuth env credentials are
/// missing — the credential requirement runs before any network call.
#[test]
fn twitter_send_missing_credentials_bails() {
    let announce = AnnounceConfig {
        twitter: Some(TwitterAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let logger = ctx.logger("announce");
    let err = send_drained(&TwitterAnnouncer, &mut ctx, &announce, &no_retry(), &logger)
        .unwrap_err()
        .to_string();
    assert!(err.contains("TWITTER_CONSUMER_KEY"), "{err}");
}

/// Mastodon `send` warn-and-skips when `server` is configured but renders
/// empty.
#[test]
fn mastodon_send_empty_server_warn_and_skips() {
    let announce = AnnounceConfig {
        mastodon: Some(MastodonAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            server: Some("".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let logger = ctx.logger("announce");
    send_drained(
        &MastodonAnnouncer,
        &mut ctx,
        &announce,
        &no_retry(),
        &logger,
    )
    .expect("empty mastodon server must skip cleanly in normal mode");
}

/// Mastodon `send` POSTs the rendered status to `<server>/api/v1/statuses`
/// when all three OAuth credentials are present (full dispatch path).
#[test]
fn mastodon_send_posts_status_to_server() {
    let (addr, log) = spawn_scripted_responder(vec![ok_route("/api/v1/statuses")]);
    let announce = AnnounceConfig {
        mastodon: Some(MastodonAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            server: Some(format!("http://{addr}")),
            message_template: Some("{{ ProjectName }} {{ Tag }}".to_string()),
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(
        announce.clone(),
        &[
            ("MASTODON_ACCESS_TOKEN", "tok"),
            ("MASTODON_CLIENT_ID", "cid"),
            ("MASTODON_CLIENT_SECRET", "sec"),
        ],
    );
    let logger = ctx.logger("announce");
    send_drained(
        &MastodonAnnouncer,
        &mut ctx,
        &announce,
        &no_retry(),
        &logger,
    )
    .unwrap();
    let entries = log.lock().unwrap();
    assert_eq!(entries[0].path, "/api/v1/statuses");
    // The status is form-encoded (`status=...`); the space in
    // "myapp v1.0.0" URL-encodes to `+`.
    assert!(
        entries[0].body.contains("myapp+v1.0.0"),
        "status in form body: {}",
        entries[0].body
    );
}

/// Bluesky `send` warn-and-skips when `username` is absent.
#[test]
fn bluesky_send_missing_username_warn_and_skips() {
    let announce = AnnounceConfig {
        bluesky: Some(BlueskyAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            username: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[("BLUESKY_APP_PASSWORD", "pw")]);
    let logger = ctx.logger("announce");
    send_drained(&BlueskyAnnouncer, &mut ctx, &announce, &no_retry(), &logger)
        .expect("missing bluesky username must skip cleanly in normal mode");
}

/// Bluesky `send` hard-bails when `BLUESKY_APP_PASSWORD` is missing even
/// though `username` is present (credential, not skip-when-empty).
#[test]
fn bluesky_send_missing_password_bails() {
    let announce = AnnounceConfig {
        bluesky: Some(BlueskyAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            username: Some("me.bsky.social".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let logger = ctx.logger("announce");
    let err = send_drained(&BlueskyAnnouncer, &mut ctx, &announce, &no_retry(), &logger)
        .unwrap_err()
        .to_string();
    assert!(err.contains("BLUESKY_APP_PASSWORD"), "{err}");
}

/// Bluesky `send` runs the full two-step flow against a custom `pds_url`:
/// createSession (returns the session JWT/DID) then createRecord carrying
/// the rendered post text.
#[test]
fn bluesky_send_posts_session_then_record_to_pds() {
    let (addr, log) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "POST",
            path_pattern: "/xrpc/com.atproto.server.createSession",
            response: "HTTP/1.1 200 OK\r\nContent-Length: 39\r\n\r\n{\"accessJwt\":\"jwt\",\"did\":\"did:plc:abc\"}",
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/xrpc/com.atproto.repo.createRecord",
            response: "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
            times: None,
        },
    ]);
    let announce = AnnounceConfig {
        bluesky: Some(BlueskyAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            username: Some("me.bsky.social".to_string()),
            pds_url: Some(format!("http://{addr}")),
            message_template: Some("{{ Tag }} out".to_string()),
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[("BLUESKY_APP_PASSWORD", "pw")]);
    let logger = ctx.logger("announce");
    send_drained(&BlueskyAnnouncer, &mut ctx, &announce, &no_retry(), &logger).unwrap();
    let entries = log.lock().unwrap();
    assert_eq!(entries.len(), 2, "{entries:?}");
    assert_eq!(entries[0].path, "/xrpc/com.atproto.server.createSession");
    assert_eq!(entries[1].path, "/xrpc/com.atproto.repo.createRecord");
    let record: serde_json::Value = serde_json::from_str(&entries[1].body).unwrap();
    assert_eq!(record["record"]["text"], "v1.0.0 out");
}

/// LinkedIn `send` hard-bails before any network call when the token from
/// `LINKEDIN_ACCESS_TOKEN` contains embedded whitespace/control characters.
#[test]
fn linkedin_send_rejects_token_with_internal_whitespace() {
    let announce = AnnounceConfig {
        linkedin: Some(LinkedInAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            message_template: None,
        }),
        ..Default::default()
    };
    // Internal space survives the outer .trim(); the char-scan must reject.
    let mut ctx = live_ctx(
        announce.clone(),
        &[("LINKEDIN_ACCESS_TOKEN", "abcdefgh ijklmnop")],
    );
    let logger = ctx.logger("announce");
    let err = send_drained(
        &LinkedInAnnouncer,
        &mut ctx,
        &announce,
        &no_retry(),
        &logger,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("whitespace or"), "{err}");
}

/// LinkedIn `send` hard-bails when `LINKEDIN_ACCESS_TOKEN` is unset (the
/// credential requirement runs before the network).
#[test]
fn linkedin_send_missing_token_bails() {
    let announce = AnnounceConfig {
        linkedin: Some(LinkedInAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            message_template: None,
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let logger = ctx.logger("announce");
    let err = send_drained(
        &LinkedInAnnouncer,
        &mut ctx,
        &announce,
        &no_retry(),
        &logger,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("LINKEDIN_ACCESS_TOKEN"), "{err}");
}

/// OpenCollective `send` warn-and-skips when `slug` is absent.
#[test]
fn opencollective_send_missing_slug_warn_and_skips() {
    let announce = AnnounceConfig {
        opencollective: Some(OpenCollectiveAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            slug: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[("OPENCOLLECTIVE_TOKEN", "t")]);
    let logger = ctx.logger("announce");
    send_drained(
        &OpenCollectiveAnnouncer,
        &mut ctx,
        &announce,
        &no_retry(),
        &logger,
    )
    .expect("missing slug must skip cleanly in normal mode");
}

/// OpenCollective `send` warn-and-skips when `slug` renders empty.
#[test]
fn opencollective_send_empty_slug_warn_and_skips() {
    let announce = AnnounceConfig {
        opencollective: Some(OpenCollectiveAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            slug: Some("".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[("OPENCOLLECTIVE_TOKEN", "t")]);
    let logger = ctx.logger("announce");
    send_drained(
        &OpenCollectiveAnnouncer,
        &mut ctx,
        &announce,
        &no_retry(),
        &logger,
    )
    .expect("empty slug must skip cleanly in normal mode");
}

/// OpenCollective `send` validates the rendered slug format and bails on an
/// invalid slug before any credential lookup or network call.
#[test]
fn opencollective_send_rejects_invalid_slug() {
    let announce = AnnounceConfig {
        opencollective: Some(OpenCollectiveAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            slug: Some("Invalid Slug!".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[("OPENCOLLECTIVE_TOKEN", "t")]);
    let logger = ctx.logger("announce");
    let err = send_drained(
        &OpenCollectiveAnnouncer,
        &mut ctx,
        &announce,
        &no_retry(),
        &logger,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("slug"), "{err}");
}

/// Email `send` warn-and-skips when `from` is absent.
#[test]
fn email_send_missing_from_warn_and_skips() {
    let announce = AnnounceConfig {
        email: Some(EmailAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            from: None,
            to: vec!["dev@example.com".to_string()],
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let logger = ctx.logger("announce");
    send_drained(&EmailAnnouncer, &mut ctx, &announce, &no_retry(), &logger)
        .expect("missing from must skip cleanly in normal mode");
}

/// Email `send` warn-and-skips when the recipient list (`to`) is empty.
#[test]
fn email_send_empty_to_warn_and_skips() {
    let announce = AnnounceConfig {
        email: Some(EmailAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            from: Some("rel@example.com".to_string()),
            to: vec![],
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let logger = ctx.logger("announce");
    send_drained(&EmailAnnouncer, &mut ctx, &announce, &no_retry(), &logger)
        .expect("empty recipient list must skip cleanly in normal mode");
}

/// Email `send` over SMTP requires a password for an encrypted transport:
/// with a username present and STARTTLS forced, the missing `SMTP_PASSWORD`
/// env hard-bails — exercising the SMTP-branch credential resolution past
/// the username check.
#[test]
fn email_send_smtp_requires_password_for_encrypted_transport() {
    let announce = AnnounceConfig {
        email: Some(EmailAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            host: Some("smtp.example.com".to_string()),
            username: Some("mailer".to_string()),
            from: Some("rel@example.com".to_string()),
            to: vec!["dev@example.com".to_string()],
            encryption: Some(EmailEncryption::Starttls),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = live_ctx(announce.clone(), &[]);
    let logger = ctx.logger("announce");
    let err = send_drained(&EmailAnnouncer, &mut ctx, &announce, &no_retry(), &logger)
        .unwrap_err()
        .to_string();
    assert!(err.contains("SMTP_PASSWORD"), "{err}");
}

/// Email `send` resolves the SMTP host from the `SMTP_HOST` env var when
/// the config omits `host`: with `encryption: none` no password is needed,
/// so it reaches the transport and fails connecting to the dead port —
/// proving the env-host + SmtpParams construction path ran.
#[test]
fn email_send_resolves_smtp_host_from_env() {
    let announce = AnnounceConfig {
        email: Some(EmailAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            host: None,
            port: Some(1),
            username: Some("mailer".to_string()),
            from: Some("rel@example.com".to_string()),
            to: vec!["dev@example.com".to_string()],
            encryption: Some(EmailEncryption::None),
            ..Default::default()
        }),
        ..Default::default()
    };
    // 127.0.0.1:1 is reliably connection-refused; the SMTP branch must
    // construct SmtpParams and attempt the transport, then surface the
    // retry-exhaustion context from `send_smtp`.
    let mut ctx = live_ctx(announce.clone(), &[("SMTP_HOST", "127.0.0.1")]);
    let logger = ctx.logger("announce");
    let err = format!(
        "{:#}",
        send_drained(&EmailAnnouncer, &mut ctx, &announce, &no_retry(), &logger).unwrap_err()
    );
    assert!(err.contains("smtp: send exhausted retry attempts"), "{err}");
}

// ------------------------------------------------------------------
// render_only per-provider template-failure coverage
// ------------------------------------------------------------------

/// Discourse `render_only` surfaces a broken `server` template without a
/// network call.
#[test]
fn render_only_discourse_surfaces_broken_server() {
    let announce = AnnounceConfig {
        discourse: Some(DiscourseAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            server: Some("{{ NoSuchVar }}".to_string()),
            category_id: Some(1),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = render_ctx(announce.clone());
    assert!(DiscourseAnnouncer.render_only(&mut ctx, &announce).is_err());
}

/// Webhook `render_only` runs the same endpoint validator `send` does: a
/// rendered non-http scheme is rejected by the guard.
#[test]
fn render_only_webhook_validates_endpoint_scheme() {
    let announce = AnnounceConfig {
        webhook: Some(WebhookConfig {
            enabled: Some(StringOrBool::Bool(true)),
            endpoint_url: Some("ftp://host/x".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = render_ctx(announce.clone());
    let err = WebhookAnnouncer
        .render_only(&mut ctx, &announce)
        .unwrap_err()
        .to_string();
    assert!(err.contains("must use http or https"), "{err}");
}

/// Telegram `render_only` validates a rendered `message_thread_id`: a
/// non-integer render is rejected by the guard.
#[test]
fn render_only_telegram_validates_thread_id() {
    let announce = AnnounceConfig {
        telegram: Some(TelegramAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            bot_token: Some("123:ABC".to_string()),
            chat_id: Some("42".to_string()),
            message_thread_id: Some("not-an-int".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = render_ctx(announce.clone());
    let err = TelegramAnnouncer
        .render_only(&mut ctx, &announce)
        .unwrap_err()
        .to_string();
    assert!(err.contains("invalid message_thread_id"), "{err}");
}

/// Teams `render_only` surfaces a broken `title_template`.
#[test]
fn render_only_teams_surfaces_broken_title() {
    let announce = AnnounceConfig {
        teams: Some(TeamsAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            webhook_url: Some("http://x/y".to_string()),
            title_template: Some("{{ NoSuchVar }}".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = render_ctx(announce.clone());
    assert!(TeamsAnnouncer.render_only(&mut ctx, &announce).is_err());
}

/// Mattermost `render_only` surfaces a broken `channel` template.
#[test]
fn render_only_mattermost_surfaces_broken_channel() {
    let announce = AnnounceConfig {
        mattermost: Some(MattermostAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            webhook_url: Some("http://x/y".to_string()),
            channel: Some("{{ NoSuchVar }}".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = render_ctx(announce.clone());
    assert!(
        MattermostAnnouncer
            .render_only(&mut ctx, &announce)
            .is_err()
    );
}

/// Reddit `render_only` surfaces a broken `url_template`.
#[test]
fn render_only_reddit_surfaces_broken_url_template() {
    let announce = AnnounceConfig {
        reddit: Some(RedditAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            application_id: Some("app".to_string()),
            username: Some("u".to_string()),
            sub: Some("rust".to_string()),
            url_template: Some("{{ NoSuchVar }}".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = render_ctx(announce.clone());
    assert!(RedditAnnouncer.render_only(&mut ctx, &announce).is_err());
}

/// Twitter `render_only` surfaces a broken `message_template` (it renders
/// only the message, reading no credentials).
#[test]
fn render_only_twitter_surfaces_broken_message() {
    let announce = AnnounceConfig {
        twitter: Some(TwitterAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            message_template: Some("{{ NoSuchVar }}".to_string()),
        }),
        ..Default::default()
    };
    let mut ctx = render_ctx(announce.clone());
    assert!(TwitterAnnouncer.render_only(&mut ctx, &announce).is_err());
}

/// Mastodon `render_only` surfaces a broken `server` template.
#[test]
fn render_only_mastodon_surfaces_broken_server() {
    let announce = AnnounceConfig {
        mastodon: Some(MastodonAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            server: Some("{{ NoSuchVar }}".to_string()),
            message_template: None,
        }),
        ..Default::default()
    };
    let mut ctx = render_ctx(announce.clone());
    assert!(MastodonAnnouncer.render_only(&mut ctx, &announce).is_err());
}

/// Bluesky `render_only` surfaces a broken `pds_url` template.
#[test]
fn render_only_bluesky_surfaces_broken_pds_url() {
    let announce = AnnounceConfig {
        bluesky: Some(BlueskyAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            username: Some("me.bsky.social".to_string()),
            pds_url: Some("{{ NoSuchVar }}".to_string()),
            message_template: None,
        }),
        ..Default::default()
    };
    let mut ctx = render_ctx(announce.clone());
    assert!(BlueskyAnnouncer.render_only(&mut ctx, &announce).is_err());
}

/// LinkedIn `render_only` surfaces a broken `message_template` (it renders
/// only the message, reading no credentials).
#[test]
fn render_only_linkedin_surfaces_broken_message() {
    let announce = AnnounceConfig {
        linkedin: Some(LinkedInAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            message_template: Some("{{ NoSuchVar }}".to_string()),
        }),
        ..Default::default()
    };
    let mut ctx = render_ctx(announce.clone());
    assert!(LinkedInAnnouncer.render_only(&mut ctx, &announce).is_err());
}

/// OpenCollective `render_only` validates a non-empty rendered slug's
/// format, rejecting an invalid slug before send.
#[test]
fn render_only_opencollective_rejects_invalid_slug() {
    let announce = AnnounceConfig {
        opencollective: Some(OpenCollectiveAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            slug: Some("Bad Slug!".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = render_ctx(announce.clone());
    assert!(
        OpenCollectiveAnnouncer
            .render_only(&mut ctx, &announce)
            .is_err()
    );
}

/// OpenCollective `render_only` does NOT reject an empty-rendered slug
/// (that's skip-when-empty in `send`), but still renders the title/message
/// templates — a broken message template surfaces.
#[test]
fn render_only_opencollective_surfaces_broken_message() {
    let announce = AnnounceConfig {
        opencollective: Some(OpenCollectiveAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            slug: Some("good-slug".to_string()),
            message_template: Some("{{ NoSuchVar }}".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = render_ctx(announce.clone());
    assert!(
        OpenCollectiveAnnouncer
            .render_only(&mut ctx, &announce)
            .is_err()
    );
}

/// Email `render_only` runs the same `from` validator `send` does: a
/// rendered address with no `@` is rejected by the guard.
#[test]
fn render_only_email_validates_from() {
    let announce = AnnounceConfig {
        email: Some(EmailAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            from: Some("noatsign".to_string()),
            to: vec!["dev@example.com".to_string()],
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = render_ctx(announce.clone());
    let err = EmailAnnouncer
        .render_only(&mut ctx, &announce)
        .unwrap_err()
        .to_string();
    assert!(err.contains("missing @"), "{err}");
}

/// Email `render_only` surfaces a broken `subject_template`.
#[test]
fn render_only_email_surfaces_broken_subject() {
    let announce = AnnounceConfig {
        email: Some(EmailAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            from: Some("rel@example.com".to_string()),
            to: vec!["dev@example.com".to_string()],
            subject_template: Some("{{ NoSuchVar }}".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = render_ctx(announce.clone());
    assert!(EmailAnnouncer.render_only(&mut ctx, &announce).is_err());
}
