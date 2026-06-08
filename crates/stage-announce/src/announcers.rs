//! The `Announcer` trait, the single-announcer runner, the per-provider
//! impls (one struct per platform), and the dispatch loop that runs them
//! all and collects per-provider errors.
//!
//! Each impl delegates the actual side effect to the matching per-platform
//! submodule (`crate::discord`, `crate::slack`, …) via the shared
//! [`crate::dispatch::dispatch`] helper; this module owns only the trait
//! wiring and the registration order.

use std::collections::HashMap;

use anodizer_core::config::AnnounceConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::retry::RetryPolicy;
use anyhow::Result;

use crate::dispatch::dispatch;
use crate::helpers::{
    DEFAULT_DISPLAY_NAME, WEBHOOK_DEFAULT_MESSAGE_TEMPLATE, is_enabled, render_json_template,
    render_message, require_env_all_with_env, require_env_with_env, require_non_empty_env_with_env,
    resolve_smtp_port, resolve_webhook_headers,
};
use crate::{
    bluesky, discord, discourse, email, linkedin, mastodon, mattermost, opencollective, reddit,
    slack, teams, telegram, twitter, webhook,
};

/// Telegram's default message template. MarkdownV2 parse mode requires each
/// dynamic value to pass through the `mdv2escape` filter and literal `!` to be
/// backslash-escaped; the rendered output is byte-equivalent to the upstream
/// `{{ print … | mdv2escape }}` Go-template form. Shared by `send` and
/// `render_only` so the pre-publish guard exercises the exact default `send`
/// would use. Pinned by `test_telegram_default_template_renders_without_tilde`.
const TELEGRAM_DEFAULT_TEMPLATE: &str = "{{ ProjectName | mdv2escape }} {{ Tag | mdv2escape }} is out\\! Check it out at {{ ReleaseURL | mdv2escape }}";

// ---------------------------------------------------------------------------
// Rendered-value validators — shared by `send` and `render_only`
//
// Each validator runs the SAME check `send` performs on a post-render value,
// with NO network call, so the pre-publish guard (`render_only`) rejects a
// config whose template renders to an invalid value BEFORE any one-way
// publisher fires. Both call sites delegate here so they cannot drift.
// ---------------------------------------------------------------------------

/// Validate (and parse) a rendered discord `color`: a base-10 integer in the
/// 24-bit RGB space `0..=0xFFFFFF`. An empty/whitespace render is "unset"
/// (`Ok(None)`), matching `send`'s skip-when-empty handling.
fn validate_discord_color(rendered: &str) -> Result<Option<u32>> {
    let trimmed = rendered.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let parsed = trimmed
        .parse::<i64>()
        .map_err(|e| anyhow::anyhow!("announce.discord: invalid color {trimmed:?}: {e}"))?;
    if !(0..=0xFFFFFF).contains(&parsed) {
        anyhow::bail!(
            "announce.discord: color {parsed} out of range \
                 (must be 0..=16777215, the 24-bit RGB space)"
        );
    }
    Ok(Some(parsed as u32))
}

/// Validate a rendered webhook `endpoint_url`: parseable, http/https scheme,
/// and a present host. Embedded userinfo is redacted from error messages so a
/// `https://user:pass@host` template can't leak credentials into the chain.
fn validate_webhook_endpoint_url(url: &str) -> Result<()> {
    let safe_url = anodizer_core::redact::redact_url_credentials(url);
    let parsed = reqwest::Url::parse(url).map_err(|e| {
        anyhow::anyhow!("announce.webhook: endpoint_url {safe_url:?} is not a valid URL: {e}")
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        anyhow::bail!(
            "announce.webhook: endpoint_url {safe_url:?} must use http or https \
                 (got scheme {:?})",
            parsed.scheme()
        );
    }
    if parsed.host().is_none() {
        anyhow::bail!("announce.webhook: endpoint_url {safe_url:?} must include a host");
    }
    Ok(())
}

/// Validate (and parse) a rendered telegram `message_thread_id` as `i64`. An
/// empty/whitespace render is "unset" (`Ok(None)`), matching `send`.
fn validate_telegram_thread_id(rendered: &str) -> Result<Option<i64>> {
    let trimmed = rendered.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let parsed = trimmed.parse::<i64>().map_err(|e| {
        anyhow::anyhow!("announce.telegram: invalid message_thread_id {trimmed:?}: {e}")
    })?;
    Ok(Some(parsed))
}

/// Validate a rendered email `from` address looks like an email (contains `@`).
fn validate_email_from(from: &str) -> Result<()> {
    if !from.contains('@') {
        anyhow::bail!(
            "announce.email: 'from' address {from:?} does not look like a valid email (missing @)"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Announcer trait + dispatch helper
// ---------------------------------------------------------------------------

/// Per-provider announce dispatch.
///
/// `enabled` decides whether the provider's config block is present and
/// the provider opted in (rendering the `enabled:` template if any).
/// `send` performs the side effect; per-provider errors are collected
/// at the call site rather than fast-failing the stage.
trait Announcer: Sync {
    fn name(&self) -> &'static str;
    fn enabled(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<bool>;
    fn send(
        &self,
        ctx: &mut Context,
        announce: &AnnounceConfig,
        retry: &RetryPolicy,
        log: &StageLogger,
    ) -> Result<()>;

    /// Render — but do not send — exactly the templates this announcer's
    /// [`send`](Announcer::send) would render, so a broken template
    /// (`{{ ReleaseURL }}` typo, undefined var, malformed Tera) surfaces as
    /// an `Err` BEFORE any irreversible publisher fires.
    ///
    /// Reads ZERO credentials/env — only [`send`](Announcer::send) touches
    /// the network and secrets — so the pre-publish guard runs on a CI box
    /// without announce secrets. Each impl must render every template field
    /// `send` renders (`message_template`, `title_template`, `enabled`,
    /// `url`/`icon_url`, …); a field rendered by `send` but skipped here is a
    /// hole in the guard. The default `Ok(())` is overridden per provider.
    fn render_only(&self, _ctx: &mut Context, _announce: &AnnounceConfig) -> Result<()> {
        Ok(())
    }
}

/// Run a single announcer: skip when disabled, capture per-provider
/// errors into the shared `errors` vec, propagate `enabled:` template
/// errors (matching the historical `?`-in-let-chain behavior).
fn run_announcer(
    a: &dyn Announcer,
    ctx: &mut Context,
    announce: &AnnounceConfig,
    retry: &RetryPolicy,
    log: &StageLogger,
    errors: &mut Vec<String>,
) -> Result<()> {
    if !a.enabled(ctx, announce)? {
        return Ok(());
    }
    if let Err(e) = a.send(ctx, announce, retry, log) {
        // `{e:#}` flattens the anyhow chain into "outer: middle: root"
        // so the announce-stage summary actually names the underlying
        // failure (e.g. a missing template variable or a wrapped tera
        // syntax error) instead of just the outermost wrapper.
        errors.push(format!("{}: {e:#}", a.name()));
    }
    Ok(())
}

/// Dispatch every registered announcer, collecting per-provider errors.
pub(crate) fn dispatch_all_announcers(
    ctx: &mut Context,
    announce: &AnnounceConfig,
    retry_policy: &RetryPolicy,
    log: &StageLogger,
    errors: &mut Vec<String>,
) -> Result<()> {
    for announcer in announcer_registry() {
        run_announcer(*announcer, ctx, announce, retry_policy, log, errors)?;
    }

    Ok(())
}

/// The registered announcer set, in dispatch order. Single source of truth for
/// both [`dispatch_all_announcers`] (which sends) and [`render_all_announcers`]
/// (which only renders), so the pre-publish guard exercises exactly the set the
/// real announce path would.
fn announcer_registry() -> &'static [&'static dyn Announcer] {
    &[
        &DiscordAnnouncer,
        &DiscourseAnnouncer,
        &SlackAnnouncer,
        &WebhookAnnouncer,
        &TelegramAnnouncer,
        &TeamsAnnouncer,
        &MattermostAnnouncer,
        &RedditAnnouncer,
        &TwitterAnnouncer,
        &MastodonAnnouncer,
        &BlueskyAnnouncer,
        &LinkedInAnnouncer,
        &OpenCollectiveAnnouncer,
        &EmailAnnouncer,
    ]
}

/// Dry-render every ENABLED announcer's templates, collecting a per-provider
/// error (`"<provider>: <chain>"`) for any that fail to render. Sends nothing
/// and reads no credentials — the pre-publish guard's announce half.
///
/// An announcer whose `enabled` template is falsy (or whose config block is
/// absent) is skipped, matching the real dispatch loop, so the guard never
/// flags a provider the live run would not touch.
pub(crate) fn render_all_announcers(
    ctx: &mut Context,
    announce: &AnnounceConfig,
    errors: &mut Vec<String>,
) -> Result<()> {
    for announcer in announcer_registry() {
        if !announcer.enabled(ctx, announce)? {
            continue;
        }
        if let Err(e) = announcer.render_only(ctx, announce) {
            errors.push(format!("{}: {e:#}", announcer.name()));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Announcer impls — one per provider. Bodies preserve the historical
// arm semantics exactly; only the surrounding `if let Some(cfg) = ...`
// / closure / errors.push boilerplate moves out to `run_announcer`.
// ---------------------------------------------------------------------------

struct DiscordAnnouncer;
impl Announcer for DiscordAnnouncer {
    fn name(&self) -> &'static str {
        "discord"
    }
    fn enabled(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<bool> {
        match &announce.discord {
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
    ) -> Result<()> {
        let cfg = announce
            .discord
            .as_ref()
            .expect("send only called when enabled returned Ok(true)");
        let id = ctx.env_var("DISCORD_WEBHOOK_ID").filter(|s| !s.is_empty());
        let token = ctx
            .env_var("DISCORD_WEBHOOK_TOKEN")
            .filter(|s| !s.is_empty());
        let url = match (id, token) {
            (Some(id), Some(token)) => {
                let base = ctx
                    .env_var("DISCORD_API")
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "https://discord.com/api".to_string());
                // Build the webhook URL via
                // `url.URL.JoinPath(...)`, which percent-encodes path
                // segments. Discord webhook IDs and tokens are
                // alphanumeric+`_-` in practice, but a malformed env
                // value (`/`, `?`, `#`, …) used to splice straight
                // into the URL and silently corrupt the request.
                // Encoding the segments produces a clean 4xx that
                // can actually be debugged.
                format!(
                    "{}/webhooks/{}/{}",
                    base.trim_end_matches('/'),
                    anodizer_core::url::percent_encode_path_segment(&id),
                    anodizer_core::url::percent_encode_path_segment(&token),
                )
            }
            _ => match cfg.webhook_url.as_deref() {
                Some(raw) => ctx.render_template(raw)?,
                None => {
                    // Skip-when-empty UX policy: in strict mode this
                    // bails (collected by the closure-level wrapper
                    // and reported at end-of-stage); in normal mode
                    // it warns and returns Ok so unrelated announcers
                    // still run.
                    ctx.strict_guard(
                        log,
                        "announce.discord: missing webhook_url \
                             (set discord.webhook_url, or both \
                             DISCORD_WEBHOOK_ID and DISCORD_WEBHOOK_TOKEN env vars)",
                    )?;
                    return Ok(());
                }
            },
        };
        let message = render_message(ctx, cfg.message_template.as_deref())?;
        let author =
            ctx.render_template_opt(cfg.author.as_deref().or(Some(DEFAULT_DISPLAY_NAME)))?;
        let color: Option<u32> = match cfg.color.as_deref() {
            Some(raw) => {
                let rendered = ctx.render_template(raw)?;
                validate_discord_color(&rendered)?
            }
            None => None,
        };
        let icon_url = ctx.render_template_opt(cfg.icon_url.as_deref())?;
        let opts = discord::DiscordOptions {
            author: author.as_deref(),
            color,
            icon_url: icon_url.as_deref(),
        };
        dispatch(ctx, "discord", &message, || {
            discord::send_discord(&url, &message, &opts, retry_policy)
        })
    }
    fn render_only(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<()> {
        let Some(cfg) = announce.discord.as_ref() else {
            return Ok(());
        };
        if let Some(raw) = cfg.webhook_url.as_deref() {
            ctx.render_template(raw)?;
        }
        render_message(ctx, cfg.message_template.as_deref())?;
        ctx.render_template_opt(cfg.author.as_deref().or(Some(DEFAULT_DISPLAY_NAME)))?;
        if let Some(raw) = cfg.color.as_deref() {
            let rendered = ctx.render_template(raw)?;
            validate_discord_color(&rendered)?;
        }
        ctx.render_template_opt(cfg.icon_url.as_deref())?;
        Ok(())
    }
}

struct DiscourseAnnouncer;
impl Announcer for DiscourseAnnouncer {
    fn name(&self) -> &'static str {
        "discourse"
    }
    fn enabled(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<bool> {
        match &announce.discourse {
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
    ) -> Result<()> {
        let cfg = announce
            .discourse
            .as_ref()
            .expect("send only called when enabled returned Ok(true)");
        // Skip-when-empty UX policy: missing or empty `server` /
        // missing `category_id` warn-and-skip in normal mode and bail
        // in strict mode. A configured-but-zero `category_id` is a
        // config error, not skip-when-empty, so it stays a hard bail.
        let server = match cfg.server.as_deref() {
            Some(raw) => ctx.render_template(raw)?,
            None => {
                ctx.strict_guard(log, "announce.discourse: missing server")?;
                return Ok(());
            }
        };
        if server.is_empty() {
            ctx.strict_guard(log, "announce.discourse: server must not be empty")?;
            return Ok(());
        }
        let category_id = match cfg.category_id {
            Some(id) => id,
            None => {
                ctx.strict_guard(log, "announce.discourse: missing category_id")?;
                return Ok(());
            }
        };
        if category_id == 0 {
            anyhow::bail!("announce.discourse: category_id must be non-zero");
        }
        let username = cfg.username.as_deref().unwrap_or("system");
        let title = ctx.render_template(
            cfg.title_template
                .as_deref()
                .unwrap_or("{{ ProjectName }} {{ Tag }} is out!"),
        )?;
        let message = render_message(ctx, cfg.message_template.as_deref())?;
        let api_key = require_env_with_env("discourse", "DISCOURSE_API_KEY", ctx.env_source())?;

        dispatch(ctx, "discourse", &title, || {
            discourse::send_discourse(
                &server,
                &api_key,
                username,
                category_id,
                &title,
                &message,
                retry_policy,
            )
        })
    }
    fn render_only(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<()> {
        let Some(cfg) = announce.discourse.as_ref() else {
            return Ok(());
        };
        if let Some(raw) = cfg.server.as_deref() {
            ctx.render_template(raw)?;
        }
        ctx.render_template(
            cfg.title_template
                .as_deref()
                .unwrap_or("{{ ProjectName }} {{ Tag }} is out!"),
        )?;
        render_message(ctx, cfg.message_template.as_deref())?;
        Ok(())
    }
}

struct SlackAnnouncer;
impl Announcer for SlackAnnouncer {
    fn name(&self) -> &'static str {
        "slack"
    }
    fn enabled(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<bool> {
        match &announce.slack {
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
    ) -> Result<()> {
        let cfg = announce
            .slack
            .as_ref()
            .expect("send only called when enabled returned Ok(true)");
        let url = match cfg.webhook_url.as_deref() {
            Some(u) => ctx.render_template(u)?,
            None => match ctx.env_var("SLACK_WEBHOOK").filter(|s| !s.is_empty()) {
                Some(env) => env,
                None => {
                    // Skip-when-empty UX policy: strict_guard bails in
                    // strict mode (collected at end-of-stage); in normal
                    // mode it warns and skips just this announcer.
                    ctx.strict_guard(
                        log,
                        "announce.slack: missing webhook_url (set config or SLACK_WEBHOOK env var)",
                    )?;
                    return Ok(());
                }
            },
        };
        let message = render_message(ctx, cfg.message_template.as_deref())?;
        let channel = ctx.render_template_opt(cfg.channel.as_deref())?;
        let username =
            ctx.render_template_opt(cfg.username.as_deref().or(Some(DEFAULT_DISPLAY_NAME)))?;
        let icon_emoji = ctx.render_template_opt(cfg.icon_emoji.as_deref())?;
        let icon_url = ctx.render_template_opt(cfg.icon_url.as_deref())?;
        let blocks = match cfg.blocks.as_ref() {
            Some(b) => render_json_template(ctx, Some(&serde_json::to_value(b)?))?,
            None => None,
        };
        let attachments = match cfg.attachments.as_ref() {
            Some(a) => render_json_template(ctx, Some(&serde_json::to_value(a)?))?,
            None => None,
        };
        dispatch(ctx, "slack", &message, || {
            let opts = slack::SlackOptions {
                channel: channel.as_deref(),
                username: username.as_deref(),
                icon_emoji: icon_emoji.as_deref(),
                icon_url: icon_url.as_deref(),
                blocks: blocks.as_ref(),
                attachments: attachments.as_ref(),
            };
            slack::send_slack(&url, &message, &opts, retry_policy)
        })
    }
    fn render_only(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<()> {
        let Some(cfg) = announce.slack.as_ref() else {
            return Ok(());
        };
        if let Some(raw) = cfg.webhook_url.as_deref() {
            ctx.render_template(raw)?;
        }
        render_message(ctx, cfg.message_template.as_deref())?;
        ctx.render_template_opt(cfg.channel.as_deref())?;
        ctx.render_template_opt(cfg.username.as_deref().or(Some(DEFAULT_DISPLAY_NAME)))?;
        ctx.render_template_opt(cfg.icon_emoji.as_deref())?;
        ctx.render_template_opt(cfg.icon_url.as_deref())?;
        if let Some(b) = cfg.blocks.as_ref() {
            render_json_template(ctx, Some(&serde_json::to_value(b)?))?;
        }
        if let Some(a) = cfg.attachments.as_ref() {
            render_json_template(ctx, Some(&serde_json::to_value(a)?))?;
        }
        Ok(())
    }
}

struct WebhookAnnouncer;
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
                ctx.strict_guard(log, "announce.webhook: missing endpoint_url")?;
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
        let message = ctx.render_template(
            cfg.message_template
                .as_deref()
                .unwrap_or(WEBHOOK_DEFAULT_MESSAGE_TEMPLATE),
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
        dispatch(ctx, "webhook", &message, || {
            webhook::send_webhook(
                &url,
                &message,
                &headers,
                &content_type,
                skip_tls,
                &expected_codes,
                retry_policy,
            )
        })
    }
    fn render_only(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<()> {
        let Some(cfg) = announce.webhook.as_ref() else {
            return Ok(());
        };
        if let Some(raw) = cfg.endpoint_url.as_deref() {
            let url = ctx.render_template(raw)?;
            validate_webhook_endpoint_url(&url)?;
        }
        ctx.render_template(
            cfg.message_template
                .as_deref()
                .unwrap_or(WEBHOOK_DEFAULT_MESSAGE_TEMPLATE),
        )?;
        for (_, v) in cfg.headers.clone().unwrap_or_default() {
            ctx.render_template(&v)?;
        }
        Ok(())
    }
}

struct TelegramAnnouncer;
impl Announcer for TelegramAnnouncer {
    fn name(&self) -> &'static str {
        "telegram"
    }
    fn enabled(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<bool> {
        match &announce.telegram {
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
    ) -> Result<()> {
        let cfg = announce
            .telegram
            .as_ref()
            .expect("send only called when enabled returned Ok(true)");
        let bot_token = match cfg.bot_token.as_deref() {
            Some(t) => ctx.render_template(t)?,
            None => match ctx.env_var("TELEGRAM_TOKEN").filter(|s| !s.is_empty()) {
                Some(env) => env,
                None => {
                    // Skip-when-empty UX: warn-and-skip in normal mode,
                    // bail in strict mode.
                    ctx.strict_guard(
                        log,
                        "announce.telegram: missing bot_token (set config or TELEGRAM_TOKEN env var)",
                    )?;
                    return Ok(());
                }
            },
        };
        let chat_id = match cfg.chat_id.as_deref() {
            Some(raw) => ctx.render_template(raw)?,
            None => {
                ctx.strict_guard(log, "announce.telegram: missing chat_id")?;
                return Ok(());
            }
        };
        // Telegram defaults to MarkdownV2 parse mode, so the default
        // message template must apply the mdv2escape filter.
        //
        // The upstream telegram template uses Go-template syntax:
        //   `{{ print .ProjectName " " .Tag " is out! ... " .ReleaseURL | mdv2escape }}`
        // anodizer renders via Tera. Previously this template used
        // Tera's `~` concatenation operator (`{{ A ~ " " ~ B | filter }}`)
        // — which works, but is hostile to copy-paste: a user pulling
        // the default into a custom template tends to mix it with
        // `print` blocks (Tera then refuses to parse `print`)
        // or rewrite the `~` and break the filter pipeline.
        //
        // The new form uses one `mdv2escape` filter per dynamic value
        // plus pre-escaped literal text (`is out\!` — `!` must be
        // backslash-escaped in MarkdownV2 per the Telegram docs). The
        // rendered output is byte-equivalent to the upstream
        // `{{ print … | mdv2escape }}` form, but the template itself
        // is `{{ … }}`-only and copy-pastes cleanly into custom
        // user templates. Pinned by
        // `test_telegram_default_template_renders_without_tilde`.
        let message = ctx.render_template(
            cfg.message_template
                .as_deref()
                .unwrap_or(TELEGRAM_DEFAULT_TEMPLATE),
        )?;
        // Default parse_mode to "MarkdownV2".
        // Validate against known values; default to MarkdownV2 with a warning for unknowns.
        let parse_mode_raw = cfg.parse_mode.as_deref().unwrap_or("MarkdownV2");
        let parse_mode_validated = match parse_mode_raw {
            "MarkdownV2" | "HTML" => parse_mode_raw,
            other => {
                log.warn(&format!(
                    "telegram: unknown parse_mode {:?}, defaulting to \"MarkdownV2\"",
                    other
                ));
                "MarkdownV2"
            }
        };
        let parse_mode = ctx.render_template_opt(Some(parse_mode_validated))?;
        // message_thread_id is now a String supporting template expressions;
        // render and parse to i64 at runtime.
        let message_thread_id: Option<i64> = match cfg.message_thread_id.as_deref() {
            Some(raw) => {
                let rendered = ctx.render_template(raw)?;
                validate_telegram_thread_id(&rendered)?
            }
            None => None,
        };

        dispatch(ctx, "telegram", &message, || {
            telegram::send_telegram(
                &bot_token,
                &chat_id,
                &message,
                parse_mode.as_deref(),
                message_thread_id,
                retry_policy,
            )
        })
    }
    fn render_only(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<()> {
        let Some(cfg) = announce.telegram.as_ref() else {
            return Ok(());
        };
        if let Some(raw) = cfg.bot_token.as_deref() {
            ctx.render_template(raw)?;
        }
        if let Some(raw) = cfg.chat_id.as_deref() {
            ctx.render_template(raw)?;
        }
        ctx.render_template(
            cfg.message_template
                .as_deref()
                .unwrap_or(TELEGRAM_DEFAULT_TEMPLATE),
        )?;
        let parse_mode_raw = cfg.parse_mode.as_deref().unwrap_or("MarkdownV2");
        let parse_mode_validated = match parse_mode_raw {
            "MarkdownV2" | "HTML" => parse_mode_raw,
            _ => "MarkdownV2",
        };
        ctx.render_template_opt(Some(parse_mode_validated))?;
        if let Some(raw) = cfg.message_thread_id.as_deref() {
            let rendered = ctx.render_template(raw)?;
            validate_telegram_thread_id(&rendered)?;
        }
        Ok(())
    }
}

struct TeamsAnnouncer;
impl Announcer for TeamsAnnouncer {
    fn name(&self) -> &'static str {
        "teams"
    }
    fn enabled(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<bool> {
        match &announce.teams {
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
    ) -> Result<()> {
        let cfg = announce
            .teams
            .as_ref()
            .expect("send only called when enabled returned Ok(true)");
        let url = match cfg.webhook_url.as_deref() {
            Some(u) => ctx.render_template(u)?,
            None => match ctx.env_var("TEAMS_WEBHOOK").filter(|s| !s.is_empty()) {
                Some(env) => env,
                None => {
                    ctx.strict_guard(
                        log,
                        "announce.teams: missing webhook_url (set config or TEAMS_WEBHOOK env var)",
                    )?;
                    return Ok(());
                }
            },
        };
        let message = render_message(ctx, cfg.message_template.as_deref())?;
        let title_template = cfg
            .title_template
            .as_deref()
            .unwrap_or(anodizer_core::config::TEAMS_DEFAULT_TITLE_TEMPLATE);
        let title = Some(ctx.render_template(title_template)?);
        let color_val = cfg.color.clone().unwrap_or_else(|| "#2D313E".to_string());
        let icon_url = ctx.render_template_opt(cfg.icon_url.as_deref())?;
        let opts = teams::TeamsOptions {
            title: title.as_deref(),
            color: Some(color_val.as_str()),
            icon_url: icon_url.as_deref(),
        };
        dispatch(ctx, "teams", &message, || {
            teams::send_teams(&url, &message, &opts, retry_policy)
        })
    }
    fn render_only(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<()> {
        let Some(cfg) = announce.teams.as_ref() else {
            return Ok(());
        };
        if let Some(raw) = cfg.webhook_url.as_deref() {
            ctx.render_template(raw)?;
        }
        render_message(ctx, cfg.message_template.as_deref())?;
        ctx.render_template(
            cfg.title_template
                .as_deref()
                .unwrap_or(anodizer_core::config::TEAMS_DEFAULT_TITLE_TEMPLATE),
        )?;
        ctx.render_template_opt(cfg.icon_url.as_deref())?;
        Ok(())
    }
}

struct MattermostAnnouncer;
impl Announcer for MattermostAnnouncer {
    fn name(&self) -> &'static str {
        "mattermost"
    }
    fn enabled(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<bool> {
        match &announce.mattermost {
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
    ) -> Result<()> {
        let cfg = announce
            .mattermost
            .as_ref()
            .expect("send only called when enabled returned Ok(true)");
        let url = match cfg.webhook_url.as_deref() {
            Some(u) => ctx.render_template(u)?,
            None => match ctx.env_var("MATTERMOST_WEBHOOK").filter(|s| !s.is_empty()) {
                Some(env) => env,
                None => {
                    ctx.strict_guard(
                        log,
                        "announce.mattermost: missing webhook_url (set config or MATTERMOST_WEBHOOK env var)",
                    )?;
                    return Ok(());
                }
            },
        };
        let message = render_message(ctx, cfg.message_template.as_deref())?;
        // Anodize-additive UX win (locked 2026-04-28): channel,
        // username, icon_url, and icon_emoji all run through the
        // template engine. The conventional behaviour passes these
        // fields raw — no template substitution. Rendering is
        // tradeoff-free (raw strings still pass through unchanged)
        // and unlocks per-tag channel routing like
        // `channel: "release-{{ Tag }}"`. Render errors surface via
        // the strict_guard collected-errors path, same as message.
        // Pinned by `test_mattermost_renders_channel_template`.
        let channel = ctx.render_template_opt(cfg.channel.as_deref())?;
        // Default username to DEFAULT_DISPLAY_NAME (brand-default policy
        // keeps anodizer's own attribution).
        let username =
            ctx.render_template_opt(cfg.username.as_deref().or(Some(DEFAULT_DISPLAY_NAME)))?;
        let icon_url = ctx.render_template_opt(cfg.icon_url.as_deref())?;
        let icon_emoji = ctx.render_template_opt(cfg.icon_emoji.as_deref())?;
        // Default color to "#2D313E". Reads
        // from `MattermostAnnounce.color` — anodizer always has, even
        // before a cross-pipe bug was fixed upstream
        // where mattermost mistakenly consulted `TeamsAnnounce.Color`.
        // Pinned by `test_mattermost_reads_own_color_not_teams`.
        let color_val = cfg.color.clone().unwrap_or_else(|| "#2D313E".to_string());
        // Default title to "{{ ProjectName }} {{ Tag }} is out!".
        let title_template = cfg
            .title_template
            .as_deref()
            .unwrap_or("{{ ProjectName }} {{ Tag }} is out!");
        let title = Some(ctx.render_template(title_template)?);

        let opts = mattermost::MattermostOptions {
            channel: channel.as_deref(),
            username: username.as_deref(),
            icon_url: icon_url.as_deref(),
            icon_emoji: icon_emoji.as_deref(),
            color: Some(color_val.as_str()),
            title: title.as_deref(),
        };
        dispatch(ctx, "mattermost", &message, || {
            mattermost::send_mattermost(&url, &message, &opts, retry_policy)
        })
    }
    fn render_only(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<()> {
        let Some(cfg) = announce.mattermost.as_ref() else {
            return Ok(());
        };
        if let Some(raw) = cfg.webhook_url.as_deref() {
            ctx.render_template(raw)?;
        }
        render_message(ctx, cfg.message_template.as_deref())?;
        ctx.render_template_opt(cfg.channel.as_deref())?;
        ctx.render_template_opt(cfg.username.as_deref().or(Some(DEFAULT_DISPLAY_NAME)))?;
        ctx.render_template_opt(cfg.icon_url.as_deref())?;
        ctx.render_template_opt(cfg.icon_emoji.as_deref())?;
        ctx.render_template(
            cfg.title_template
                .as_deref()
                .unwrap_or("{{ ProjectName }} {{ Tag }} is out!"),
        )?;
        Ok(())
    }
}

struct RedditAnnouncer;
impl Announcer for RedditAnnouncer {
    fn name(&self) -> &'static str {
        "reddit"
    }
    fn enabled(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<bool> {
        match &announce.reddit {
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
    ) -> Result<()> {
        let cfg = announce
            .reddit
            .as_ref()
            .expect("send only called when enabled returned Ok(true)");
        // Skip-when-empty UX: missing required config fields
        // (application_id / username / sub) warn-and-skip in normal
        // mode and bail in strict mode. The required env vars
        // (REDDIT_SECRET, REDDIT_PASSWORD) still hard-bail because
        // missing credentials are a config error, not skip-when-empty.
        let app_id = match cfg.application_id.as_deref() {
            Some(raw) => ctx.render_template(raw)?,
            None => {
                ctx.strict_guard(log, "announce.reddit: missing application_id")?;
                return Ok(());
            }
        };
        let username = match cfg.username.as_deref() {
            Some(raw) => ctx.render_template(raw)?,
            None => {
                ctx.strict_guard(log, "announce.reddit: missing username")?;
                return Ok(());
            }
        };
        let sub = match cfg.sub.as_deref() {
            Some(raw) => ctx.render_template(raw)?,
            None => {
                ctx.strict_guard(log, "announce.reddit: missing sub")?;
                return Ok(());
            }
        };
        let title = ctx.render_template(
            cfg.title_template
                .as_deref()
                .unwrap_or("{{ ProjectName }} {{ Tag }} is out!"),
        )?;
        let url = ctx.render_template(cfg.url_template.as_deref().unwrap_or("{{ ReleaseURL }}"))?;
        let creds = require_env_all_with_env(
            "reddit",
            &["REDDIT_SECRET", "REDDIT_PASSWORD"],
            ctx.env_source(),
        )?;
        let secret = &creds[0];
        let password = &creds[1];
        dispatch(ctx, "reddit", &format!("r/{sub}: {title}"), || {
            reddit::send_reddit(
                &reddit::RedditPost {
                    application_id: &app_id,
                    secret,
                    username: &username,
                    password,
                    subreddit: &sub,
                    title: &title,
                    url: &url,
                },
                log,
                retry_policy,
            )
        })
    }
    fn render_only(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<()> {
        let Some(cfg) = announce.reddit.as_ref() else {
            return Ok(());
        };
        if let Some(raw) = cfg.application_id.as_deref() {
            ctx.render_template(raw)?;
        }
        if let Some(raw) = cfg.username.as_deref() {
            ctx.render_template(raw)?;
        }
        if let Some(raw) = cfg.sub.as_deref() {
            ctx.render_template(raw)?;
        }
        ctx.render_template(
            cfg.title_template
                .as_deref()
                .unwrap_or("{{ ProjectName }} {{ Tag }} is out!"),
        )?;
        ctx.render_template(cfg.url_template.as_deref().unwrap_or("{{ ReleaseURL }}"))?;
        Ok(())
    }
}

struct TwitterAnnouncer;
impl Announcer for TwitterAnnouncer {
    fn name(&self) -> &'static str {
        "twitter"
    }
    fn enabled(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<bool> {
        match &announce.twitter {
            Some(cfg) => is_enabled(ctx, cfg.enabled.as_ref()),
            None => Ok(false),
        }
    }
    fn send(
        &self,
        ctx: &mut Context,
        announce: &AnnounceConfig,
        retry_policy: &RetryPolicy,
        _log: &StageLogger,
    ) -> Result<()> {
        let cfg = announce
            .twitter
            .as_ref()
            .expect("send only called when enabled returned Ok(true)");
        let message = render_message(ctx, cfg.message_template.as_deref())?;
        let creds = require_env_all_with_env(
            "twitter",
            &[
                "TWITTER_CONSUMER_KEY",
                "TWITTER_CONSUMER_SECRET",
                "TWITTER_ACCESS_TOKEN",
                "TWITTER_ACCESS_TOKEN_SECRET",
            ],
            ctx.env_source(),
        )?;
        let consumer_key = &creds[0];
        let consumer_secret = &creds[1];
        let access_token = &creds[2];
        let access_token_secret = &creds[3];

        dispatch(ctx, "twitter", &message, || {
            twitter::send_twitter(
                consumer_key,
                consumer_secret,
                access_token,
                access_token_secret,
                &message,
                retry_policy,
            )
        })
    }
    fn render_only(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<()> {
        let Some(cfg) = announce.twitter.as_ref() else {
            return Ok(());
        };
        render_message(ctx, cfg.message_template.as_deref())?;
        Ok(())
    }
}

struct MastodonAnnouncer;
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
                ctx.strict_guard(log, "announce.mastodon: missing server")?;
                return Ok(());
            }
        };
        if server.is_empty() {
            ctx.strict_guard(log, "announce.mastodon: server must not be empty")?;
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
        dispatch(ctx, "mastodon", &message, || {
            mastodon::send_mastodon(&server, &access_token, &message, retry_policy)
        })
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

struct BlueskyAnnouncer;
impl Announcer for BlueskyAnnouncer {
    fn name(&self) -> &'static str {
        "bluesky"
    }
    fn enabled(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<bool> {
        match &announce.bluesky {
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
    ) -> Result<()> {
        let cfg = announce
            .bluesky
            .as_ref()
            .expect("send only called when enabled returned Ok(true)");
        // Skip-when-empty UX: missing `username` warn-and-skips in
        // normal mode and bails in strict mode. BLUESKY_APP_PASSWORD
        // missing still hard-bails (credential, not skip-when-empty).
        let username = match cfg.username.as_deref() {
            Some(raw) => ctx.render_template(raw)?,
            None => {
                ctx.strict_guard(log, "announce.bluesky: missing username")?;
                return Ok(());
            }
        };
        let message = render_message(ctx, cfg.message_template.as_deref())?;
        let app_password =
            require_env_with_env("bluesky", "BLUESKY_APP_PASSWORD", ctx.env_source())?;
        let release_url = ctx.template_vars().get("ReleaseURL").map(|s| s.to_string());
        let pds_url = cfg
            .pds_url
            .as_deref()
            .map(|u| ctx.render_template(u))
            .transpose()?;

        dispatch(ctx, "bluesky", &message, || {
            bluesky::send_bluesky(
                &username,
                &app_password,
                &message,
                release_url.as_deref(),
                pds_url.as_deref(),
                retry_policy,
            )
        })
    }
    fn render_only(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<()> {
        let Some(cfg) = announce.bluesky.as_ref() else {
            return Ok(());
        };
        if let Some(raw) = cfg.username.as_deref() {
            ctx.render_template(raw)?;
        }
        render_message(ctx, cfg.message_template.as_deref())?;
        if let Some(raw) = cfg.pds_url.as_deref() {
            ctx.render_template(raw)?;
        }
        Ok(())
    }
}

struct LinkedInAnnouncer;
impl Announcer for LinkedInAnnouncer {
    fn name(&self) -> &'static str {
        "linkedin"
    }
    fn enabled(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<bool> {
        match &announce.linkedin {
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
    ) -> Result<()> {
        let cfg = announce
            .linkedin
            .as_ref()
            .expect("send only called when enabled returned Ok(true)");
        let message = render_message(ctx, cfg.message_template.as_deref())?;
        let access_token =
            require_env_with_env("linkedin", "LINKEDIN_ACCESS_TOKEN", ctx.env_source())?
                .trim()
                .to_string();
        if access_token
            .chars()
            .any(|c| c.is_whitespace() || !c.is_ascii() || c.is_ascii_control())
        {
            anyhow::bail!(
                "announce.linkedin: LINKEDIN_ACCESS_TOKEN contains whitespace or \
                     non-printable characters — check for stray quotes or line wraps"
            );
        }
        linkedin::validate_token_shape(&access_token)?;
        dispatch(ctx, "linkedin", &message, || {
            linkedin::send_linkedin(&access_token, &message, log, retry_policy)
        })
    }
    fn render_only(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<()> {
        let Some(cfg) = announce.linkedin.as_ref() else {
            return Ok(());
        };
        render_message(ctx, cfg.message_template.as_deref())?;
        Ok(())
    }
}

struct OpenCollectiveAnnouncer;
impl Announcer for OpenCollectiveAnnouncer {
    fn name(&self) -> &'static str {
        "opencollective"
    }
    fn enabled(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<bool> {
        match &announce.opencollective {
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
    ) -> Result<()> {
        let cfg = announce
            .opencollective
            .as_ref()
            .expect("send only called when enabled returned Ok(true)");
        // Skip-when-empty UX: missing or empty `slug` warn-and-skip in
        // normal mode, bail in strict mode.
        let slug = match cfg.slug.as_deref() {
            Some(raw) => ctx.render_template(raw)?,
            None => {
                ctx.strict_guard(log, "announce.opencollective: missing slug")?;
                return Ok(());
            }
        };
        if slug.is_empty() {
            ctx.strict_guard(log, "announce.opencollective: slug must not be empty")?;
            return Ok(());
        }
        opencollective::validate_slug(&slug)?;
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
        let token =
            require_env_with_env("opencollective", "OPENCOLLECTIVE_TOKEN", ctx.env_source())?;
        opencollective::validate_token_shape(&token)?;
        dispatch(ctx, "opencollective", &title, || {
            opencollective::send_opencollective(&token, &slug, &title, &html, retry_policy)
        })
    }
    fn render_only(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<()> {
        let Some(cfg) = announce.opencollective.as_ref() else {
            return Ok(());
        };
        if let Some(raw) = cfg.slug.as_deref() {
            let slug = ctx.render_template(raw)?;
            // Mirror `send`: a non-empty slug must satisfy the format rules.
            // An empty render is skip-when-empty in `send`, so don't reject it.
            if !slug.is_empty() {
                opencollective::validate_slug(&slug)?;
            }
        }
        ctx.render_template(
            cfg.title_template
                .as_deref()
                .unwrap_or(opencollective::DEFAULT_TITLE_TEMPLATE),
        )?;
        ctx.render_template(
            cfg.message_template
                .as_deref()
                .unwrap_or(opencollective::DEFAULT_MESSAGE_TEMPLATE),
        )?;
        Ok(())
    }
}

struct EmailAnnouncer;
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
                ctx.strict_guard(log, "announce.email: missing from")?;
                return Ok(());
            }
        };

        validate_email_from(&from)?;

        if cfg.to.is_empty() {
            ctx.strict_guard(log, "announce.email: missing to (recipient list)")?;
            return Ok(());
        }

        let subject = ctx.render_template(
            cfg.subject_template
                .as_deref()
                .unwrap_or("{{ ProjectName }} {{ Tag }} is out!"),
        )?;
        let body = ctx.render_template(
            cfg.message_template
                .as_deref()
                .unwrap_or("You can view details from: {{ ReleaseURL }}"),
        )?;

        let email_params = email::EmailParams {
            from: &from,
            to: &cfg.to,
            subject: &subject,
            body: &body,
        };
        let log_line = format!("to {}: {}", cfg.to.join(", "), subject);

        // Support SMTP_HOST and SMTP_PORT env vars as fallbacks.
        let smtp_host = cfg
            .host
            .clone()
            .or_else(|| ctx.env_var("SMTP_HOST").filter(|s| !s.is_empty()));
        let smtp_port_env = ctx.env_var("SMTP_PORT").and_then(|s| s.parse::<u16>().ok());
        let smtp_port = resolve_smtp_port(cfg.port, smtp_port_env);

        if let Some(host) = &smtp_host {
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

            let smtp_params = email::SmtpParams {
                host,
                port,
                username: &smtp_username,
                password: &smtp_password,
                insecure_skip_verify: insecure,
                encryption,
            };
            dispatch(ctx, "email (smtp)", &log_line, || {
                email::send_smtp(&email_params, &smtp_params, retry_policy)
            })?;
        } else {
            dispatch(ctx, "email", &log_line, || {
                email::send_sendmail(&email_params)
            })?;
        }
        Ok(())
    }
    fn render_only(&self, ctx: &mut Context, announce: &AnnounceConfig) -> Result<()> {
        let Some(cfg) = announce.email.as_ref() else {
            return Ok(());
        };
        if let Some(raw) = cfg.from.as_deref() {
            let from = ctx.render_template(raw)?;
            validate_email_from(&from)?;
        }
        ctx.render_template(
            cfg.subject_template
                .as_deref()
                .unwrap_or("{{ ProjectName }} {{ Tag }} is out!"),
        )?;
        ctx.render_template(
            cfg.message_template
                .as_deref()
                .unwrap_or("You can view details from: {{ ReleaseURL }}"),
        )?;
        Ok(())
    }
}
