//! `anodizer notify` command.
//! Sends a notification through configured announce integrations.

use anyhow::Result;

use anodizer_core::context::{Context, ContextOptions};
use anodizer_stage_announce::dispatch_filtered_announcers;

use super::helpers;
use anodizer_core::log::{StageLogger, Verbosity};

pub struct NotifyOpts {
    pub message: String,
    /// If Some, only fire these integration names.
    pub publishers: Option<Vec<String>>,
    /// Integration names to omit.
    pub skip: Vec<String>,
    /// Send the message literally, skipping Tera rendering. Set this when the
    /// message contains untrusted text (e.g. error output in an on_error hook)
    /// so an `Env`-reference cannot be expanded into the outbound message.
    pub raw: bool,
    /// Send secrets in the outbound body verbatim, disabling body redaction.
    /// Use ONLY for a trusted private channel; anodizer's own log output stays
    /// redacted regardless.
    pub allow_secrets: bool,
    pub config_override: Option<std::path::PathBuf>,
    pub verbose: bool,
    pub debug: bool,
    pub quiet: bool,
    pub dry_run: bool,
}

pub fn run(opts: NotifyOpts) -> Result<()> {
    let log = StageLogger::new(
        "notify",
        Verbosity::from_flags(opts.quiet, opts.verbose, opts.debug),
    );

    let ctx_opts = ContextOptions {
        dry_run: opts.dry_run,
        quiet: opts.quiet,
        verbose: opts.verbose,
        debug: opts.debug,
        ..Default::default()
    };

    let (_config, mut ctx) =
        helpers::init_merge_stage_ctx(opts.config_override.as_deref(), ctx_opts, &log)?;

    run_with_ctx(
        &mut ctx,
        opts.message,
        opts.publishers,
        opts.skip,
        opts.raw,
        opts.allow_secrets,
    )
}

/// Inner dispatch — takes an already-constructed `Context` so tests can drive
/// it directly without touching the filesystem / git.
pub(crate) fn run_with_ctx(
    ctx: &mut Context,
    message: String,
    publishers: Option<Vec<String>>,
    skip: Vec<String>,
    raw: bool,
    allow_secrets: bool,
) -> Result<()> {
    // Resolve the body ONCE, here, while `ctx.literal_message` is still false:
    // a non-raw message renders through Tera exactly once; a raw message is
    // passed through verbatim (render skipped — the first half of the
    // secret-leak defense for untrusted text).
    let rendered = resolve_message(ctx, message, raw)?;

    let mut announce = match ctx.config.announce.as_ref() {
        Some(a) => a.clone(),
        None => anyhow::bail!("notify: no announce config found"),
    };

    // Inject the already-final body into every provider's `message_template`,
    // overriding the per-provider default so all integrations send the same
    // operator-supplied message.
    inject_message(&mut announce, &rendered);

    // The injected body is FINAL — `render_template` is not idempotent on
    // `{{ ... }}` (it re-expands), so letting each provider render it at send
    // time would double-render the non-raw body AND, for a raw untrusted
    // message, expand a smuggled `Env`-reference into a secret on the wire.
    // `literal_message` makes every provider's send-time body render a no-op,
    // closing both. Set immediately before dispatch so only the announce send
    // path is affected.
    ctx.literal_message = true;
    // Outbound bodies are redacted by default; --allow-secrets opts out for a
    // trusted private channel. anodizer's own log redaction is unaffected.
    ctx.redact_body = !allow_secrets;

    let retry_policy = ctx.retry_policy();
    let log = ctx.logger("notify");

    let include_refs: Option<Vec<&str>> = publishers
        .as_deref()
        .map(|v| v.iter().map(|s| s.as_str()).collect());
    let skip_refs: Vec<&str> = skip.iter().map(|s| s.as_str()).collect();

    let mut errors: Vec<String> = Vec::new();
    dispatch_filtered_announcers(
        ctx,
        &announce,
        &retry_policy,
        &log,
        &mut errors,
        include_refs.as_deref(),
        &skip_refs,
    )?;

    if !errors.is_empty() {
        anyhow::bail!(
            "notify: {} integration(s) failed:\n{}",
            errors.len(),
            errors.join("\n")
        );
    }

    Ok(())
}

/// Resolve the outbound message body, honoring `raw`.
///
/// Non-raw: render `message` through Tera so standard vars (`{{ Tag }}`, …)
/// expand, surfacing a broken template before any network call. Raw: return
/// `message` verbatim — no template is validated.
///
/// This is the FIRST half of the untrusted-text defense: skipping this render
/// for raw text means a smuggled `Env`-reference is never expanded here. The
/// SECOND half is `ctx.literal_message`, set before dispatch, which stops every
/// provider from re-rendering the (already-final) body at send time — without
/// it the skipped render would simply happen again on the provider side.
fn resolve_message(ctx: &mut Context, message: String, raw: bool) -> Result<String> {
    if raw {
        Ok(message)
    } else {
        Ok(ctx.render_template(&message)?)
    }
}

/// Override every configured provider's `message_template` with `msg` so all
/// integrations dispatch the same operator-supplied text.
///
/// Only providers that are already configured (their config block is `Some`)
/// are updated — this preserves the `enabled` / credential fields the provider
/// needs while replacing only the body text.
fn inject_message(announce: &mut anodizer_core::config::AnnounceConfig, msg: &str) {
    macro_rules! set_msg {
        ($field:expr) => {
            if let Some(ref mut cfg) = $field {
                cfg.message_template = Some(msg.to_owned());
            }
        };
    }
    set_msg!(announce.discord);
    set_msg!(announce.discourse);
    set_msg!(announce.slack);
    set_msg!(announce.telegram);
    set_msg!(announce.teams);
    set_msg!(announce.mattermost);
    set_msg!(announce.email);
    // Reddit uses title_template + url_template rather than message_template;
    // notify dispatches it with the configured templates unchanged.
    set_msg!(announce.twitter);
    set_msg!(announce.mastodon);
    set_msg!(announce.bluesky);
    set_msg!(announce.linkedin);
    set_msg!(announce.opencollective);

    // The webhook provider is NOT a plain set_msg!: its `message_template` is
    // the ENTIRE request body, sent verbatim with the configured content_type.
    // The chat providers above serde-wrap the text into their own JSON envelope,
    // so a bare message is safe there; injecting a bare message as the webhook's
    // whole body would ship a non-JSON payload under the JSON-default
    // content_type, which receivers reject (HTTP 400). Build a body that matches
    // the content_type instead.
    if let Some(ref mut cfg) = announce.webhook {
        cfg.message_template = Some(webhook_notify_body(cfg.content_type.as_deref(), msg));
    }
}

/// Build the webhook request body for `anodizer notify`.
///
/// For a JSON content_type (the default, or any `*json*` value) the message is
/// wrapped in a serde-built `{"message": …}` envelope — serde escapes quotes,
/// newlines, and control characters, so an arbitrary operator/error message
/// always yields a valid JSON document. For a non-JSON content_type (e.g.
/// `text/plain`) the message is sent verbatim. The envelope key matches the
/// stage's `WEBHOOK_DEFAULT_MESSAGE_TEMPLATE` so a receiver wired for anodizer's
/// release announcements consumes notify messages unchanged.
fn webhook_notify_body(content_type: Option<&str>, msg: &str) -> String {
    // `None`/empty content_type resolves to application/json at send time
    // (see WebhookAnnouncer::send), so both are treated as JSON here.
    let is_json = match content_type {
        None => true,
        Some(ct) => ct.trim().is_empty() || ct.to_ascii_lowercase().contains("json"),
    };
    if is_json {
        serde_json::json!({ "message": msg }).to_string()
    } else {
        msg.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::Config;
    use anodizer_core::context::ContextOptions;

    fn minimal_ctx() -> Context {
        let config = Config {
            project_name: "test".to_string(),
            ..Default::default()
        };
        Context::new(config, ContextOptions::default())
    }

    #[test]
    fn no_announce_config_bails() {
        let mut ctx = minimal_ctx();
        let err = run_with_ctx(&mut ctx, "hello".to_string(), None, vec![], false, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no announce config found"), "{err}");
    }

    #[test]
    fn publishers_none_means_all() {
        let opts_publishers: Option<Vec<String>> = None;
        // None means all — verify the include_refs mapping is correct.
        let include_refs: Option<Vec<&str>> = opts_publishers
            .as_deref()
            .map(|v| v.iter().map(|s| s.as_str()).collect());
        assert!(include_refs.is_none());
    }

    #[test]
    fn publishers_some_maps_to_include_refs() {
        let publishers: Option<Vec<String>> =
            Some(vec!["slack".to_string(), "discord".to_string()]);
        let include_refs: Option<Vec<&str>> = publishers
            .as_deref()
            .map(|v| v.iter().map(|s| s.as_str()).collect());
        assert_eq!(include_refs, Some(vec!["slack", "discord"]));
    }

    #[test]
    fn skip_maps_to_skip_refs() {
        let skip = ["webhook".to_string()];
        let skip_refs: Vec<&str> = skip.iter().map(|s| s.as_str()).collect();
        assert_eq!(skip_refs, ["webhook"]);
    }

    #[test]
    fn notify_opts_roundtrip() {
        let opts = NotifyOpts {
            message: "test {{ ProjectName }}".to_string(),
            publishers: Some(vec!["slack".to_string()]),
            skip: vec!["discord".to_string()],
            raw: false,
            allow_secrets: false,
            config_override: None,
            verbose: false,
            debug: false,
            quiet: true,
            dry_run: true,
        };
        assert_eq!(opts.skip.len(), 1);
        assert_eq!(opts.skip[0], "discord");
        assert!(opts.publishers.is_some());
    }

    #[test]
    fn inject_message_sets_all_message_templates() {
        use anodizer_core::config::{
            AnnounceConfig, BlueskyAnnounce, DiscordAnnounce, DiscourseAnnounce, EmailAnnounce,
            LinkedInAnnounce, MastodonAnnounce, MattermostAnnounce, OpenCollectiveAnnounce,
            SlackAnnounce, TeamsAnnounce, TelegramAnnounce, TwitterAnnounce, WebhookConfig,
        };
        let mut announce = AnnounceConfig {
            discord: Some(DiscordAnnounce::default()),
            discourse: Some(DiscourseAnnounce::default()),
            slack: Some(SlackAnnounce::default()),
            webhook: Some(WebhookConfig::default()),
            telegram: Some(TelegramAnnounce::default()),
            teams: Some(TeamsAnnounce::default()),
            mattermost: Some(MattermostAnnounce::default()),
            email: Some(EmailAnnounce::default()),
            twitter: Some(TwitterAnnounce::default()),
            mastodon: Some(MastodonAnnounce::default()),
            bluesky: Some(BlueskyAnnounce::default()),
            linkedin: Some(LinkedInAnnounce::default()),
            opencollective: Some(OpenCollectiveAnnounce::default()),
            ..Default::default()
        };
        inject_message(&mut announce, "hello world");
        assert_eq!(
            announce.discord.as_ref().unwrap().message_template,
            Some("hello world".to_owned())
        );
        assert_eq!(
            announce.slack.as_ref().unwrap().message_template,
            Some("hello world".to_owned())
        );
        assert_eq!(
            announce.teams.as_ref().unwrap().message_template,
            Some("hello world".to_owned())
        );
        // reddit is intentionally skipped — no message_template field.
        assert!(announce.reddit.is_none());
    }

    #[test]
    fn webhook_json_default_wraps_message_in_valid_json_envelope() {
        use anodizer_core::config::{AnnounceConfig, WebhookConfig};
        let mut announce = AnnounceConfig {
            webhook: Some(WebhookConfig::default()), // content_type None → JSON
            ..Default::default()
        };
        inject_message(&mut announce, "build failed");
        let body = announce.webhook.unwrap().message_template.unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&body).expect("webhook body must be valid JSON");
        assert_eq!(v["message"], "build failed");
    }

    #[test]
    fn webhook_json_escapes_quotes_and_newlines_in_message() {
        // Error text routinely carries quotes/newlines (HTTP error bodies, git
        // stderr) — a naive plain-text body would produce invalid JSON (the
        // 400-invalid-payload bug). serde-wrapping must keep it parseable.
        use anodizer_core::config::{AnnounceConfig, WebhookConfig};
        let untrusted = "tap push rejected: \"abort\"\nremote: denied";
        let mut announce = AnnounceConfig {
            webhook: Some(WebhookConfig::default()),
            ..Default::default()
        };
        inject_message(&mut announce, untrusted);
        let body = announce.webhook.unwrap().message_template.unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&body).expect("escaped webhook body must be valid JSON");
        assert_eq!(
            v["message"], untrusted,
            "the round-tripped message must equal the original, quotes/newlines intact"
        );
    }

    #[test]
    fn webhook_non_json_content_type_sends_message_verbatim() {
        use anodizer_core::config::{AnnounceConfig, WebhookConfig};
        let mut announce = AnnounceConfig {
            webhook: Some(WebhookConfig {
                content_type: Some("text/plain".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        inject_message(&mut announce, "build failed");
        assert_eq!(
            announce.webhook.unwrap().message_template.unwrap(),
            "build failed",
            "a non-JSON content_type must receive the raw message, not a JSON envelope"
        );
    }

    #[test]
    fn raw_skips_tera_rendering_keeps_message_verbatim() {
        let mut ctx = minimal_ctx();
        // An Env-reference plus arbitrary Tera braces stand in for untrusted
        // error text. In raw mode none of it may be expanded — the secret-leak
        // vector this flag exists to close.
        let untrusted = "boom: {{ Env.CARGO_REGISTRY_TOKEN }} {{ ProjectName }}".to_string();
        let out = resolve_message(&mut ctx, untrusted.clone(), true).unwrap();
        assert_eq!(
            out, untrusted,
            "raw mode must pass the message through verbatim"
        );
    }

    #[test]
    fn non_raw_renders_tera_template() {
        // Contrast: without --raw the same standard var IS expanded. project_name
        // is "test" (see minimal_ctx), so `{{ ProjectName }}` resolves to `test`.
        let mut ctx = minimal_ctx();
        let out = resolve_message(&mut ctx, "hello {{ ProjectName }}".to_string(), false).unwrap();
        assert_eq!(
            out, "hello test",
            "non-raw mode must render the Tera template"
        );
    }

    #[test]
    fn dispatch_with_empty_announce_config_succeeds() {
        use anodizer_core::config::AnnounceConfig;
        let config = anodizer_core::config::Config {
            project_name: "test".to_string(),
            announce: Some(AnnounceConfig::default()),
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        // No providers configured → dispatch fires nothing, returns Ok.
        let result = run_with_ctx(&mut ctx, "hello".to_string(), None, vec![], false, false);
        assert!(result.is_ok(), "{result:?}");
    }

    /// `allow_secrets` drives `ctx.redact_body`: true disables outbound body
    /// redaction, false (the default) keeps it on. Observed on the same ctx
    /// after dispatch (empty announce config dispatches nothing but still runs
    /// the flag-setting path right before dispatch), not by re-reading the arg.
    #[test]
    fn allow_secrets_drives_redact_body() {
        use anodizer_core::config::AnnounceConfig;
        fn run_observe(allow_secrets: bool) -> bool {
            let config = anodizer_core::config::Config {
                project_name: "test".to_string(),
                announce: Some(AnnounceConfig::default()),
                ..Default::default()
            };
            let mut ctx = Context::new(config, ContextOptions::default());
            assert!(ctx.redact_body, "default must be redact-on before dispatch");
            run_with_ctx(
                &mut ctx,
                "hi".to_string(),
                None,
                vec![],
                false,
                allow_secrets,
            )
            .unwrap();
            ctx.redact_body
        }
        assert!(
            !run_observe(true),
            "--allow-secrets must set redact_body = false"
        );
        assert!(
            run_observe(false),
            "default (no --allow-secrets) must keep redact_body = true"
        );
    }
}
