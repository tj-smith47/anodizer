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
    render_message, render_message_with_default, require_env_all_with_env, require_env_with_env,
    require_non_empty_env_with_env, resolve_smtp_port, resolve_webhook_headers,
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

/// Default email body, shared by `send` and `render_only` so the pre-publish
/// guard exercises the exact default `send` would use, and so the two sites
/// can never drift apart.
const EMAIL_DEFAULT_MESSAGE_TEMPLATE: &str = "You can view details from: {{ ReleaseURL }}";

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
        key_width: usize,
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

/// Shared per-section dispatch state every announcer in one Announcing
/// section writes into: the collected per-provider errors, the
/// idempotency sent-marker (live path only), and the shared kv pad
/// width computed over the providers that will fire.
struct DispatchSink<'a> {
    errors: &'a mut Vec<String>,
    marker: Option<&'a mut crate::sent_marker::AnnounceSentMarker>,
    key_width: usize,
}

/// Run a single announcer, capturing per-provider errors into the
/// sink's `errors` vec.
///
/// The caller guarantees `a` is enabled — [`enabled_announcers`]
/// evaluates every `enabled:` template exactly once (and fails fast
/// before any send when one is broken), so this function must not
/// re-evaluate it.
///
/// When the sink's `marker` is `Some`, the announce is idempotent across
/// re-runs: an announcer already recorded for this version is skipped,
/// and a successful send records the announcer so a later re-run won't
/// re-post. `None` on dry-run paths (nothing is actually sent, so
/// nothing is recorded).
fn run_announcer(
    a: &dyn Announcer,
    ctx: &mut Context,
    announce: &AnnounceConfig,
    retry: &RetryPolicy,
    log: &StageLogger,
    sink: &mut DispatchSink<'_>,
) -> Result<()> {
    // Idempotency gate: a re-run at an already-announced version must not
    // re-post to a channel that already fired.
    if let Some(ref m) = sink.marker
        && m.already_sent(a.name())
    {
        log.status(&format!(
            "skipping {} — already announced this version",
            a.name()
        ));
        return Ok(());
    }
    if let Err(e) = a.send(ctx, announce, retry, log, sink.key_width) {
        // `{e:#}` flattens the anyhow chain into "outer: middle: root"
        // so the announce-stage summary actually names the underlying
        // failure (e.g. a missing template variable or a wrapped tera
        // syntax error) instead of just the outermost wrapper.
        sink.errors.push(format!("{}: {e:#}", a.name()));
        return Ok(());
    }
    // Record the successful send so a re-run skips this channel. Flushed per
    // announcer so a mid-dispatch crash still records what already posted.
    if let Some(m) = sink.marker.as_deref_mut() {
        m.mark_sent(a.name(), log);
    }
    Ok(())
}

/// Dispatch every registered announcer, collecting per-provider errors.
///
/// `marker` carries the per-version sent-marker on the live path (so re-runs
/// are idempotent) and is `None` on dry-run.
pub(crate) fn dispatch_all_announcers(
    ctx: &mut Context,
    announce: &AnnounceConfig,
    retry_policy: &RetryPolicy,
    log: &StageLogger,
    errors: &mut Vec<String>,
    marker: Option<&mut crate::sent_marker::AnnounceSentMarker>,
) -> Result<()> {
    let active = enabled_announcers(ctx, announce, None)?;
    let mut sink = DispatchSink {
        errors,
        marker,
        key_width: shared_key_width(&active),
    };
    for announcer in active {
        run_announcer(announcer, ctx, announce, retry_policy, log, &mut sink)?;
    }

    Ok(())
}

/// Resolve the announcers that will actually fire: apply the name
/// filter (when given), then evaluate each `enabled:` template exactly
/// once. A broken `enabled:` template aborts HERE, before any announcer
/// sends — a half-dispatched section (earlier channels posted, later
/// ones dead) is worse than failing fast with nothing sent.
fn enabled_announcers(
    ctx: &mut Context,
    announce: &AnnounceConfig,
    filter: Option<&AnnounceFilter<'_>>,
) -> Result<Vec<&'static dyn Announcer>> {
    let mut active: Vec<&'static dyn Announcer> = Vec::new();
    for announcer in announcer_registry() {
        let name = announcer.name();
        if let Some(f) = filter
            && (f.include.is_some_and(|inc| !inc.contains(&name)) || f.skip.contains(&name))
        {
            continue;
        }
        if announcer.enabled(ctx, announce)? {
            active.push(*announcer);
        }
    }
    Ok(active)
}

/// Widest `name()` among the announcers that will fire, so every
/// provider kv row in one Announcing section pads to the same column.
fn shared_key_width(active: &[&'static dyn Announcer]) -> usize {
    active
        .iter()
        .map(|a| a.name().chars().count())
        .max()
        .unwrap_or(0)
}

/// Filter descriptor for [`dispatch_filtered_announcers`].
pub(crate) struct AnnounceFilter<'a> {
    /// When `Some`, only announcers whose `name()` appears here are fired.
    /// `None` means all announcers are eligible.
    pub include: Option<&'a [&'a str]>,
    /// Announcers whose `name()` appears here are skipped regardless of
    /// `include`.
    pub skip: &'a [&'a str],
}

/// Like [`dispatch_all_announcers`] but filters by integration name.
pub(crate) fn dispatch_filtered_announcers(
    ctx: &mut Context,
    announce: &AnnounceConfig,
    retry_policy: &RetryPolicy,
    log: &StageLogger,
    errors: &mut Vec<String>,
    marker: Option<&mut crate::sent_marker::AnnounceSentMarker>,
    filter: AnnounceFilter<'_>,
) -> Result<()> {
    let active = enabled_announcers(ctx, announce, Some(&filter))?;
    let mut sink = DispatchSink {
        errors,
        marker,
        key_width: shared_key_width(&active),
    };
    for announcer in active {
        run_announcer(announcer, ctx, announce, retry_policy, log, &mut sink)?;
    }
    Ok(())
}

/// The registered announcer set, in dispatch order. Single source of truth for
/// both [`dispatch_all_announcers`] (which sends) and [`render_all_announcers`]
/// (which only renders), so the pre-publish guard exercises exactly the set the
/// real announce path would.
/// The names of every announcer whose config block is present, for the
/// non-release version guard's error message. Presence-based (not `enabled:`
/// template evaluation) so it is side-effect-free and can run BEFORE any
/// dispatch — it only needs to name the channels a snapshot version was about
/// to broadcast to, not the exact final enabled set.
pub(crate) fn configured_announcer_names(announce: &AnnounceConfig) -> Vec<String> {
    let mut names = Vec::new();
    let mut push = |present: bool, name: &str| {
        if present {
            names.push(name.to_string());
        }
    };
    push(announce.discord.is_some(), "discord");
    push(announce.discourse.is_some(), "discourse");
    push(announce.slack.is_some(), "slack");
    push(announce.webhook.is_some(), "webhook");
    push(announce.telegram.is_some(), "telegram");
    push(announce.teams.is_some(), "teams");
    push(announce.mattermost.is_some(), "mattermost");
    push(announce.reddit.is_some(), "reddit");
    push(announce.twitter.is_some(), "twitter");
    push(announce.mastodon.is_some(), "mastodon");
    push(announce.bluesky.is_some(), "bluesky");
    push(announce.linkedin.is_some(), "linkedin");
    push(announce.opencollective.is_some(), "opencollective");
    push(announce.email.is_some(), "email");
    names
}

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
        key_width: usize,
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
                        "missing webhook_url in announce.discord \
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
        dispatch(ctx, "discord", &message, key_width, || {
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
        key_width: usize,
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
                ctx.strict_guard(log, "missing server in announce.discourse")?;
                return Ok(());
            }
        };
        if server.is_empty() {
            ctx.strict_guard(log, "server in announce.discourse must not be empty")?;
            return Ok(());
        }
        let category_id = match cfg.category_id {
            Some(id) => id,
            None => {
                ctx.strict_guard(log, "missing category_id in announce.discourse")?;
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

        dispatch(ctx, "discourse", &title, key_width, || {
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
        key_width: usize,
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
                        "missing webhook_url in announce.slack (set config or SLACK_WEBHOOK env var)",
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
        dispatch(ctx, "slack", &message, key_width, || {
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
        key_width: usize,
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
        dispatch(ctx, "webhook", &message, key_width, || {
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
        key_width: usize,
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
                        "missing bot_token in announce.telegram (set config or TELEGRAM_TOKEN env var)",
                    )?;
                    return Ok(());
                }
            },
        };
        let chat_id = match cfg.chat_id.as_deref() {
            Some(raw) => ctx.render_template(raw)?,
            None => {
                ctx.strict_guard(log, "missing chat_id in announce.telegram")?;
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
        let message = render_message_with_default(
            ctx,
            cfg.message_template.as_deref(),
            TELEGRAM_DEFAULT_TEMPLATE,
        )?;
        // Default parse_mode to "MarkdownV2".
        // Validate against known values; default to MarkdownV2 with a warning for unknowns.
        let parse_mode_raw = cfg.parse_mode.as_deref().unwrap_or("MarkdownV2");
        let parse_mode_validated = match parse_mode_raw {
            "MarkdownV2" | "HTML" => parse_mode_raw,
            other => {
                log.warn(&format!(
                    "telegram parse_mode {:?} unknown, defaulting to \"MarkdownV2\"",
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

        dispatch(ctx, "telegram", &message, key_width, || {
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
        render_message_with_default(
            ctx,
            cfg.message_template.as_deref(),
            TELEGRAM_DEFAULT_TEMPLATE,
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
        key_width: usize,
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
                        "missing webhook_url in announce.teams (set config or TEAMS_WEBHOOK env var)",
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
        dispatch(ctx, "teams", &message, key_width, || {
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
        key_width: usize,
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
                        "missing webhook_url in announce.mattermost (set config or MATTERMOST_WEBHOOK env var)",
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
        dispatch(ctx, "mattermost", &message, key_width, || {
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
        key_width: usize,
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
                ctx.strict_guard(log, "missing application_id in announce.reddit")?;
                return Ok(());
            }
        };
        let username = match cfg.username.as_deref() {
            Some(raw) => ctx.render_template(raw)?,
            None => {
                ctx.strict_guard(log, "missing username in announce.reddit")?;
                return Ok(());
            }
        };
        let sub = match cfg.sub.as_deref() {
            Some(raw) => ctx.render_template(raw)?,
            None => {
                ctx.strict_guard(log, "missing sub in announce.reddit")?;
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
        dispatch(
            ctx,
            "reddit",
            &format!("r/{sub}: {title}"),
            key_width,
            || {
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
            },
        )
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
        key_width: usize,
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

        dispatch(ctx, "twitter", &message, key_width, || {
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
        key_width: usize,
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
                ctx.strict_guard(log, "missing server in announce.mastodon")?;
                return Ok(());
            }
        };
        if server.is_empty() {
            ctx.strict_guard(log, "server in announce.mastodon must not be empty")?;
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
        dispatch(ctx, "mastodon", &message, key_width, || {
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
        key_width: usize,
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
                ctx.strict_guard(log, "missing username in announce.bluesky")?;
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

        dispatch(ctx, "bluesky", &message, key_width, || {
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
        key_width: usize,
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
        dispatch(ctx, "linkedin", &message, key_width, || {
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
        key_width: usize,
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
                ctx.strict_guard(log, "missing slug in announce.opencollective")?;
                return Ok(());
            }
        };
        if slug.is_empty() {
            ctx.strict_guard(log, "slug in announce.opencollective must not be empty")?;
            return Ok(());
        }
        opencollective::validate_slug(&slug)?;
        let title = ctx.render_template(
            cfg.title_template
                .as_deref()
                .unwrap_or(opencollective::DEFAULT_TITLE_TEMPLATE),
        )?;
        let html = render_message_with_default(
            ctx,
            cfg.message_template.as_deref(),
            opencollective::DEFAULT_MESSAGE_TEMPLATE,
        )?;
        let token =
            require_env_with_env("opencollective", "OPENCOLLECTIVE_TOKEN", ctx.env_source())?;
        opencollective::validate_token_shape(&token)?;
        dispatch(ctx, "opencollective", &title, key_width, || {
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
        render_message_with_default(
            ctx,
            cfg.message_template.as_deref(),
            opencollective::DEFAULT_MESSAGE_TEMPLATE,
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
        key_width: usize,
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
            dispatch(
                ctx,
                "email",
                &format!("via smtp {log_line}"),
                key_width,
                || email::send_smtp(&email_params, &smtp_params, retry_policy),
            )?;
        } else {
            dispatch(
                ctx,
                "email",
                &format!("via sendmail {log_line}"),
                key_width,
                || email::send_sendmail(&email_params, log),
            )?;
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
        render_message_with_default(
            ctx,
            cfg.message_template.as_deref(),
            EMAIL_DEFAULT_MESSAGE_TEMPLATE,
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::MapEnvSource;
    use anodizer_core::config::{
        BlueskyAnnounce, Config, DiscordAnnounce, DiscourseAnnounce, EmailAnnounce,
        EmailEncryption, LinkedInAnnounce, MastodonAnnounce, MattermostAnnounce,
        OpenCollectiveAnnounce, RedditAnnounce, SlackAnnounce, StringOrBool, TeamsAnnounce,
        TelegramAnnounce, TwitterAnnounce, WebhookConfig,
    };
    use anodizer_core::context::ContextOptions;
    use anodizer_core::test_helpers::scripted_responder::{
        ScriptedRoute, spawn_scripted_responder,
    };
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
        dispatch_all_announcers(&mut ctx, &announce, &no_retry(), &log, &mut errors, None).unwrap();
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
        dispatch_all_announcers(&mut ctx, &announce, &no_retry(), &log, &mut errors, None)
            .expect_err("broken enabled: template must abort dispatch");
        assert!(
            errors.is_empty(),
            "no announcer may have attempted a send before the abort: {errors:?}"
        );
    }

    #[test]
    fn run_announcer_collects_send_error_with_provider_prefix() {
        // Point slack at a 500 responder: `send` fails, and the error must
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
        run_announcer(
            &SlackAnnouncer,
            &mut ctx,
            &announce,
            &no_retry(),
            &log,
            &mut DispatchSink {
                errors: &mut errors,
                marker: None,
                key_width: 0,
            },
        )
        .unwrap();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].starts_with("slack: "), "{}", errors[0]);
    }

    #[test]
    fn run_announcer_skips_already_sent_on_rerun() {
        // First run posts to slack; the marker records it. A second run with
        // the SAME marker (simulating a re-run at the same version) must skip
        // the post — the responder sees exactly one request.
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
        // First dispatch: posts and records the send.
        {
            let mut marker =
                crate::sent_marker::AnnounceSentMarker::load(dist.path(), "1.0.0", &log);
            run_announcer(
                &SlackAnnouncer,
                &mut ctx,
                &announce,
                &no_retry(),
                &log,
                &mut DispatchSink {
                    errors: &mut errors,
                    marker: Some(&mut marker),
                    key_width: 0,
                },
            )
            .unwrap();
        }
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(req_log.lock().unwrap().len(), 1, "first run posts once");

        // Second dispatch: a fresh marker loaded from the same dist/version
        // sees the prior send and skips — no second POST.
        {
            let mut marker =
                crate::sent_marker::AnnounceSentMarker::load(dist.path(), "1.0.0", &log);
            run_announcer(
                &SlackAnnouncer,
                &mut ctx,
                &announce,
                &no_retry(),
                &log,
                &mut DispatchSink {
                    errors: &mut errors,
                    marker: Some(&mut marker),
                    key_width: 0,
                },
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
    fn run_announcer_failed_send_is_not_marked_sent() {
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
        run_announcer(
            &SlackAnnouncer,
            &mut ctx,
            &announce,
            &no_retry(),
            &log,
            &mut DispatchSink {
                errors: &mut errors,
                marker: Some(&mut marker),
                key_width: 0,
            },
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
        dispatch_all_announcers(&mut ctx, &announce, &no_retry(), &log, &mut errors, None).unwrap();
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors[0].starts_with("slack: "), "{}", errors[0]);
        assert!(errors[1].starts_with("teams: "), "{}", errors[1]);
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
        SlackAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
            .unwrap();
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
        SlackAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
            .unwrap();
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
        DiscordAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
            .unwrap();
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
        DiscordAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
            .unwrap();
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
        let err = DiscordAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        TeamsAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
            .unwrap();
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
        MattermostAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        WebhookAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
            .unwrap();
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
        WebhookAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
            .unwrap();
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
        let err = WebhookAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        DiscourseAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        let err = DiscourseAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        TelegramAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        let err = MastodonAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        let err = EmailAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        let err = EmailAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        DiscordAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        DiscourseAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        DiscourseAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        DiscourseAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        WebhookAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        WebhookAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
            .unwrap();
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
        TelegramAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        TelegramAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        TeamsAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
            .unwrap();
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
        TeamsAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        MattermostAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        MattermostAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        RedditAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        let err = RedditAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        let err = TwitterAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        MastodonAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        MastodonAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        BlueskyAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        let err = BlueskyAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        BlueskyAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
            .unwrap();
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
        let err = LinkedInAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        let err = LinkedInAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        OpenCollectiveAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        OpenCollectiveAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        let err = OpenCollectiveAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        EmailAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        EmailAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
        let err = EmailAnnouncer
            .send(&mut ctx, &announce, &no_retry(), &logger, 0)
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
            EmailAnnouncer
                .send(&mut ctx, &announce, &no_retry(), &logger, 0)
                .unwrap_err()
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
}
