use std::collections::HashMap;

use anodizer_core::config::AnnounceGate;
use anodizer_core::context::Context;
use anodizer_core::publish_report::{PublishReport, PublisherOutcome, SkipReason};
use anodizer_core::stage::Stage;
use anyhow::{Context as _, Result};

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

/// Evaluate the announce-stage gate against the supplied PublishReport.
///
/// Returns `true` when AnnounceStage must skip and `false` when it
/// should proceed. Pure / report-only so unit tests can drive every
/// gate × report combination without touching the stage body.
pub(crate) fn evaluate_gate(report: Option<&PublishReport>, gate: AnnounceGate) -> bool {
    match gate {
        AnnounceGate::None => false,
        AnnounceGate::RequiredPublishers => report.is_some_and(|r| r.required_failures() > 0),
        AnnounceGate::AllPublishers => report.is_some_and(|r| {
            // Only *failure-like* outcomes gate announce. A naive
            // `!Succeeded` rule would treat happy-path pending states
            // (`PendingModeration` from chocolatey, `PendingValidation`
            // from winget) and `Skipped(NotConfigured)` as failures,
            // silently defeating announce on any release that included
            // a moderated publisher.
            //
            // # Exhaustiveness
            //
            // Each variant is named explicitly (no `_` wildcard) so
            // adding a new `PublisherOutcome` variant is a compile
            // error here — the reviewer of the new variant has to
            // decide whether it gates announce. `matches!` itself
            // does NOT enforce exhaustiveness; an explicit `match`
            // does, which is the shape used below.
            r.results.iter().any(|res| match &res.outcome {
                PublisherOutcome::Failed(_)
                | PublisherOutcome::RollbackFailed(_)
                | PublisherOutcome::Skipped(SkipReason::SubmitterGated) => true,
                PublisherOutcome::Succeeded
                | PublisherOutcome::Skipped(SkipReason::NotConfigured)
                | PublisherOutcome::Skipped(SkipReason::Snapshot)
                | PublisherOutcome::Skipped(SkipReason::DryRun)
                | PublisherOutcome::RolledBack
                | PublisherOutcome::RollbackSkippedNoScope
                | PublisherOutcome::PendingModeration
                | PublisherOutcome::PendingValidation
                | PublisherOutcome::PublishedNoRollback => false,
            })
        }),
    }
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
        // `emit_summary` is invoked by `Pipeline::run` (single source
        // of truth), not here. The pipeline-layer call ensures the
        // summary fires even when announce is operator-skipped via
        // `--skip=announce` (the stage's `run` is never invoked in
        // that case). A fallback call here would double-write the
        // file; leaving ownership at the pipeline-level scope-guard
        // keeps the contract single-homed.
        announce_body(self, ctx)
    }
}

/// Body of `AnnounceStage::run` — kept separated from the trait `run`
/// to make the boundary explicit: the trait `run` is "announce body
/// only" while `Pipeline::run` is responsible for `emit_summary`.
fn announce_body(_stage: &AnnounceStage, ctx: &mut Context) -> Result<()> {
    let log = ctx.logger("announce");
    if ctx.skip_in_snapshot(&log, "announce") {
        return Ok(());
    }

    // Refresh Artifacts template var so announce templates can iterate artifacts.
    ctx.refresh_artifacts_var();

    let announce = match ctx.config.announce.clone() {
        Some(a) => a,
        None => {
            log.status("no announce config — skipping");
            return Ok(());
        }
    };

    // Evaluate template-conditional skip.
    if let Some(ref skip_val) = announce.skip {
        let should_skip = skip_val
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .with_context(|| "announce: render skip template")?;
        if should_skip {
            log.status("announce.skip evaluated to true — skipping");
            return Ok(());
        }
    }

    // PublishReport-driven gate: skip when configured required (or all)
    // publishers didn't succeed. The flag on PublishReport lets the
    // run-summary JSON expose the skip cleanly to CI.
    let gate_on = announce.gate_on;
    let report_ref = ctx.publish_report.as_ref();
    if evaluate_gate(report_ref, gate_on) {
        let required_failures = report_ref.map_or(0, |r| r.required_failures());
        log.status(&format!(
            "announce skipped via gate_on={gate_on:?}; publish_report has \
                 {required_failures} required-failure(s)"
        ));
        if let Some(report_mut) = ctx.publish_report.as_mut() {
            report_mut.announce_gated = true;
        }
        return Ok(());
    }

    // Collect errors from all providers instead of failing fast on the first one.
    let mut errors: Vec<String> = vec![];

    // P1.3 — wire `Config.retry` into every announcer that makes a
    // network call. `RetryConfig::default()` matches GR's defaults
    // (10 attempts × 10s base × 5m cap); per-call retry classifies 5xx
    // / 429 / transport failures as retriable and 4xx as fast-fail via
    // `core::retry::is_retriable` + `HttpError`.
    let retry_policy = ctx.retry_policy();

    // ----------------------------------------------------------------
    // Discord
    // ----------------------------------------------------------------
    if let Some(cfg) = &announce.discord
        && is_enabled(ctx, cfg.enabled.as_ref())?
        && let Err(e) = (|| -> Result<()> {
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
                    // Q-disc1: GoReleaser builds the webhook URL via
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
                            &log,
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
                    let trimmed = rendered.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        let parsed = trimmed.parse::<i64>().map_err(|e| {
                            anyhow::anyhow!("announce.discord: invalid color {trimmed:?}: {e}")
                        })?;
                        if !(0..=0xFFFFFF).contains(&parsed) {
                            anyhow::bail!(
                                "announce.discord: color {parsed} out of range \
                                     (must be 0..=16777215, the 24-bit RGB space)"
                            );
                        }
                        Some(parsed as u32)
                    }
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
                discord::send_discord(&url, &message, &opts, &retry_policy)
            })
        })()
    {
        errors.push(format!("discord: {e}"));
    }

    // ----------------------------------------------------------------
    // Discourse
    // ----------------------------------------------------------------
    if let Some(cfg) = &announce.discourse
        && is_enabled(ctx, cfg.enabled.as_ref())?
        && let Err(e) = (|| -> Result<()> {
            // Skip-when-empty UX policy: missing or empty `server` /
            // missing `category_id` warn-and-skip in normal mode and bail
            // in strict mode. A configured-but-zero `category_id` is a
            // config error, not skip-when-empty, so it stays a hard bail.
            let server = match cfg.server.as_deref() {
                Some(raw) => ctx.render_template(raw)?,
                None => {
                    ctx.strict_guard(&log, "announce.discourse: missing server")?;
                    return Ok(());
                }
            };
            if server.is_empty() {
                ctx.strict_guard(&log, "announce.discourse: server must not be empty")?;
                return Ok(());
            }
            let category_id = match cfg.category_id {
                Some(id) => id,
                None => {
                    ctx.strict_guard(&log, "announce.discourse: missing category_id")?;
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
                    &retry_policy,
                )
            })
        })()
    {
        errors.push(format!("discourse: {e}"));
    }

    // ----------------------------------------------------------------
    // Slack
    // ----------------------------------------------------------------
    if let Some(cfg) = &announce.slack
        && is_enabled(ctx, cfg.enabled.as_ref())?
        && let Err(e) = (|| -> Result<()> {
            let url = match cfg.webhook_url.as_deref() {
                Some(u) => ctx.render_template(u)?,
                None => match ctx.env_var("SLACK_WEBHOOK").filter(|s| !s.is_empty()) {
                    Some(env) => env,
                    None => {
                        // Skip-when-empty UX policy: strict_guard bails in
                        // strict mode (collected at end-of-stage); in normal
                        // mode it warns and we skip just this announcer.
                        ctx.strict_guard(
                                &log,
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
                slack::send_slack(&url, &message, &opts, &retry_policy)
            })
        })()
    {
        errors.push(format!("slack: {e}"));
    }

    // ----------------------------------------------------------------
    // Generic HTTP webhook
    // ----------------------------------------------------------------
    if let Some(cfg) = &announce.webhook
        && is_enabled(ctx, cfg.enabled.as_ref())?
        && let Err(e) = (|| -> Result<()> {
            // Skip-when-empty UX: missing endpoint_url skips this announcer
            // in normal mode (warn) and bails in strict mode.
            let url = match cfg.endpoint_url.as_deref() {
                Some(raw) => ctx.render_template(raw)?,
                None => {
                    ctx.strict_guard(&log, "announce.webhook: missing endpoint_url")?;
                    return Ok(());
                }
            };
            // Strip embedded userinfo (`https://user:pass@host`) before
            // the URL lands in any operator-facing error message — the
            // raw template can carry inline credentials and the error
            // chain is the easiest place for them to leak.
            let safe_url = anodizer_core::redact::redact_url_credentials(&url);
            let parsed = reqwest::Url::parse(&url).map_err(|e| {
                anyhow::anyhow!(
                    "announce.webhook: endpoint_url {safe_url:?} is not a valid URL: {e}"
                )
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
            // Anodize-additive UX win (locked 2026-04-28): we also send
            // `User-Agent: anodizer/<version>` (unless the user overrides)
            // so operators can attribute incoming webhooks to anodizer for
            // routing, rate-limiting, and audit-log tagging. GoReleaser
            // (`internal/pipe/webhook/webhook.go`) sends a static
            // `User-Agent: goreleaser` with no version suffix; the
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

            // GoReleaser defaults to "application/json; charset=utf-8".
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
                    &retry_policy,
                )
            })
        })()
    {
        errors.push(format!("webhook: {e}"));
    }

    // ----------------------------------------------------------------
    // Telegram
    // ----------------------------------------------------------------
    if let Some(cfg) = &announce.telegram
        && is_enabled(ctx, cfg.enabled.as_ref())?
        && let Err(e) = (|| -> Result<()> {
            let bot_token = match cfg.bot_token.as_deref() {
                Some(t) => ctx.render_template(t)?,
                None => match ctx.env_var("TELEGRAM_TOKEN").filter(|s| !s.is_empty()) {
                    Some(env) => env,
                    None => {
                        // Skip-when-empty UX: warn-and-skip in normal mode,
                        // bail in strict mode.
                        ctx.strict_guard(
                                &log,
                                "announce.telegram: missing bot_token (set config or TELEGRAM_TOKEN env var)",
                            )?;
                        return Ok(());
                    }
                },
            };
            let chat_id = match cfg.chat_id.as_deref() {
                Some(raw) => ctx.render_template(raw)?,
                None => {
                    ctx.strict_guard(&log, "announce.telegram: missing chat_id")?;
                    return Ok(());
                }
            };
            // Telegram defaults to MarkdownV2 parse mode, so the default
            // message template must apply the mdv2escape filter.
            //
            // Q-tg1: GoReleaser telegram.go:18 uses Go-template syntax:
            //   `{{ print .ProjectName " " .Tag " is out! ... " .ReleaseURL | mdv2escape }}`
            // anodizer renders via Tera. Previously this template used
            // Tera's `~` concatenation operator (`{{ A ~ " " ~ B | filter }}`)
            // — which works, but is hostile to copy-paste: a user pulling
            // the default into a custom template tends to mix it with
            // GR-style `print` blocks (Tera then refuses to parse `print`)
            // or rewrite the `~` and break the filter pipeline.
            //
            // The new form uses one `mdv2escape` filter per dynamic value
            // plus pre-escaped literal text (`is out\!` — `!` must be
            // backslash-escaped in MarkdownV2 per the Telegram docs). The
            // rendered output is byte-equivalent to GR's
            // `{{ print … | mdv2escape }}` form, but the template itself
            // is `{{ … }}`-only and copy-pastes cleanly into custom
            // user templates. Pinned by
            // `test_telegram_default_template_renders_without_tilde`.
            const TELEGRAM_DEFAULT_TEMPLATE: &str = "{{ ProjectName | mdv2escape }} {{ Tag | mdv2escape }} is out\\! Check it out at {{ ReleaseURL | mdv2escape }}";
            let message = ctx.render_template(
                cfg.message_template
                    .as_deref()
                    .unwrap_or(TELEGRAM_DEFAULT_TEMPLATE),
            )?;
            // Default parse_mode to "MarkdownV2" to match GoReleaser behaviour.
            // Validate against known values; default to MarkdownV2 with a warning for unknowns.
            let parse_mode_raw = cfg.parse_mode.as_deref().unwrap_or("MarkdownV2");
            let parse_mode_validated = match parse_mode_raw {
                "MarkdownV2" | "HTML" => parse_mode_raw,
                other => {
                    let log = ctx.logger("announce");
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
                    let trimmed = rendered.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.parse::<i64>().map_err(|e| {
                            anyhow::anyhow!(
                                "announce.telegram: invalid message_thread_id {:?}: {}",
                                trimmed,
                                e
                            )
                        })?)
                    }
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
                    &retry_policy,
                )
            })
        })()
    {
        errors.push(format!("telegram: {e}"));
    }

    // ----------------------------------------------------------------
    // Microsoft Teams
    // ----------------------------------------------------------------
    if let Some(cfg) = &announce.teams
        && is_enabled(ctx, cfg.enabled.as_ref())?
        && let Err(e) = (|| -> Result<()> {
            let url = match cfg.webhook_url.as_deref() {
                Some(u) => ctx.render_template(u)?,
                None => match ctx.env_var("TEAMS_WEBHOOK").filter(|s| !s.is_empty()) {
                    Some(env) => env,
                    None => {
                        ctx.strict_guard(
                                &log,
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
                teams::send_teams(&url, &message, &opts, &retry_policy)
            })
        })()
    {
        errors.push(format!("teams: {e}"));
    }

    // ----------------------------------------------------------------
    // Mattermost
    // ----------------------------------------------------------------
    if let Some(cfg) = &announce.mattermost
        && is_enabled(ctx, cfg.enabled.as_ref())?
        && let Err(e) = (|| -> Result<()> {
            let url = match cfg.webhook_url.as_deref() {
                Some(u) => ctx.render_template(u)?,
                None => match ctx.env_var("MATTERMOST_WEBHOOK").filter(|s| !s.is_empty()) {
                    Some(env) => env,
                    None => {
                        ctx.strict_guard(
                                &log,
                                "announce.mattermost: missing webhook_url (set config or MATTERMOST_WEBHOOK env var)",
                            )?;
                        return Ok(());
                    }
                },
            };
            let message = render_message(ctx, cfg.message_template.as_deref())?;
            // Anodize-additive UX win (locked 2026-04-28): channel,
            // username, icon_url, and icon_emoji all run through the
            // template engine. GoReleaser
            // (`internal/pipe/mattermost/mattermost.go`) passes these
            // fields raw — no template substitution. Rendering is
            // tradeoff-free (raw strings still pass through unchanged)
            // and unlocks per-tag channel routing like
            // `channel: "release-{{ Tag }}"`. Render errors surface via
            // the strict_guard collected-errors path, same as message.
            // Pinned by `test_mattermost_renders_channel_template`.
            let channel = ctx.render_template_opt(cfg.channel.as_deref())?;
            // Default username to DEFAULT_DISPLAY_NAME (GoReleaser defaults to
            // "GoReleaser"; brand-default policy keeps anodizer's own attribution).
            let username =
                ctx.render_template_opt(cfg.username.as_deref().or(Some(DEFAULT_DISPLAY_NAME)))?;
            let icon_url = ctx.render_template_opt(cfg.icon_url.as_deref())?;
            let icon_emoji = ctx.render_template_opt(cfg.icon_emoji.as_deref())?;
            // Default color to "#2D313E" (GoReleaser default). We read
            // from `MattermostAnnounce.color` — anodizer always has, even
            // before upstream commit 7e7f9b2 fixed the GR cross-pipe bug
            // where mattermost mistakenly consulted `TeamsAnnounce.Color`.
            // Pinned by `test_mattermost_reads_own_color_not_teams`.
            let color_val = cfg.color.clone().unwrap_or_else(|| "#2D313E".to_string());
            // Default title to "{{ ProjectName }} {{ Tag }} is out!" (GoReleaser default).
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
                mattermost::send_mattermost(&url, &message, &opts, &retry_policy)
            })
        })()
    {
        errors.push(format!("mattermost: {e}"));
    }

    // ----------------------------------------------------------------
    // Reddit
    // ----------------------------------------------------------------
    if let Some(cfg) = &announce.reddit
        && is_enabled(ctx, cfg.enabled.as_ref())?
        && let Err(e) = (|| -> Result<()> {
            // Skip-when-empty UX: missing required config fields
            // (application_id / username / sub) warn-and-skip in normal
            // mode and bail in strict mode. The required env vars
            // (REDDIT_SECRET, REDDIT_PASSWORD) still hard-bail because
            // missing credentials are a config error, not skip-when-empty.
            let app_id = match cfg.application_id.as_deref() {
                Some(raw) => ctx.render_template(raw)?,
                None => {
                    ctx.strict_guard(&log, "announce.reddit: missing application_id")?;
                    return Ok(());
                }
            };
            let username = match cfg.username.as_deref() {
                Some(raw) => ctx.render_template(raw)?,
                None => {
                    ctx.strict_guard(&log, "announce.reddit: missing username")?;
                    return Ok(());
                }
            };
            let sub = match cfg.sub.as_deref() {
                Some(raw) => ctx.render_template(raw)?,
                None => {
                    ctx.strict_guard(&log, "announce.reddit: missing sub")?;
                    return Ok(());
                }
            };
            let title = ctx.render_template(
                cfg.title_template
                    .as_deref()
                    .unwrap_or("{{ ProjectName }} {{ Tag }} is out!"),
            )?;
            let url =
                ctx.render_template(cfg.url_template.as_deref().unwrap_or("{{ ReleaseURL }}"))?;
            let creds = require_env_all_with_env(
                "reddit",
                &["REDDIT_SECRET", "REDDIT_PASSWORD"],
                ctx.env_source(),
            )?;
            let secret = &creds[0];
            let password = &creds[1];
            let log = ctx.logger("announce");
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
                    &log,
                    &retry_policy,
                )
            })
        })()
    {
        errors.push(format!("reddit: {e}"));
    }

    // ----------------------------------------------------------------
    // Twitter/X
    // ----------------------------------------------------------------
    if let Some(cfg) = &announce.twitter
        && is_enabled(ctx, cfg.enabled.as_ref())?
        && let Err(e) = (|| -> Result<()> {
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
                    &retry_policy,
                )
            })
        })()
    {
        errors.push(format!("twitter: {e}"));
    }

    // ----------------------------------------------------------------
    // Mastodon
    // ----------------------------------------------------------------
    if let Some(cfg) = &announce.mastodon
        && is_enabled(ctx, cfg.enabled.as_ref())?
        && let Err(e) = (|| -> Result<()> {
            // Skip-when-empty UX: missing or empty `server` warn-and-skip
            // in normal mode, bail in strict mode.
            let server = match cfg.server.as_deref() {
                Some(raw) => ctx.render_template(raw)?,
                None => {
                    ctx.strict_guard(&log, "announce.mastodon: missing server")?;
                    return Ok(());
                }
            };
            if server.is_empty() {
                ctx.strict_guard(&log, "announce.mastodon: server must not be empty")?;
                return Ok(());
            }
            let message = render_message(ctx, cfg.message_template.as_deref())?;
            // Q-mast1: GoReleaser's `mastodon.Config` declares all three
            // env-backed fields (ClientID, ClientSecret, AccessToken) as
            // `notEmpty`, so missing any one of them fails fast at
            // validation time. Anodizer used to require only
            // ACCESS_TOKEN, silently sending without the credentials GR
            // requires for its OAuth refresh flow. Mirror the GR
            // fail-fast here so misconfigured releases die up front
            // instead of mid-announce.
            let access_token = require_non_empty_env_with_env(
                "mastodon",
                "MASTODON_ACCESS_TOKEN",
                ctx.env_source(),
            )?;
            require_non_empty_env_with_env("mastodon", "MASTODON_CLIENT_ID", ctx.env_source())?;
            require_non_empty_env_with_env("mastodon", "MASTODON_CLIENT_SECRET", ctx.env_source())?;
            dispatch(ctx, "mastodon", &message, || {
                mastodon::send_mastodon(&server, &access_token, &message, &retry_policy)
            })
        })()
    {
        errors.push(format!("mastodon: {e}"));
    }

    // ----------------------------------------------------------------
    // Bluesky
    // ----------------------------------------------------------------
    if let Some(cfg) = &announce.bluesky
        && is_enabled(ctx, cfg.enabled.as_ref())?
        && let Err(e) = (|| -> Result<()> {
            // Skip-when-empty UX: missing `username` warn-and-skips in
            // normal mode and bails in strict mode. BLUESKY_APP_PASSWORD
            // missing still hard-bails (credential, not skip-when-empty).
            let username = match cfg.username.as_deref() {
                Some(raw) => ctx.render_template(raw)?,
                None => {
                    ctx.strict_guard(&log, "announce.bluesky: missing username")?;
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
                    &retry_policy,
                )
            })
        })()
    {
        errors.push(format!("bluesky: {e}"));
    }

    // ----------------------------------------------------------------
    // LinkedIn
    // ----------------------------------------------------------------
    if let Some(cfg) = &announce.linkedin
        && is_enabled(ctx, cfg.enabled.as_ref())?
        && let Err(e) = (|| -> Result<()> {
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
            let log = ctx.logger("announce");
            dispatch(ctx, "linkedin", &message, || {
                linkedin::send_linkedin(&access_token, &message, &log, &retry_policy)
            })
        })()
    {
        errors.push(format!("linkedin: {e}"));
    }

    // ----------------------------------------------------------------
    // OpenCollective
    // ----------------------------------------------------------------
    if let Some(cfg) = &announce.opencollective
        && is_enabled(ctx, cfg.enabled.as_ref())?
        && let Err(e) = (|| -> Result<()> {
            // Skip-when-empty UX: missing or empty `slug` warn-and-skip in
            // normal mode, bail in strict mode.
            let slug = match cfg.slug.as_deref() {
                Some(raw) => ctx.render_template(raw)?,
                None => {
                    ctx.strict_guard(&log, "announce.opencollective: missing slug")?;
                    return Ok(());
                }
            };
            if slug.is_empty() {
                ctx.strict_guard(&log, "announce.opencollective: slug must not be empty")?;
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
                opencollective::send_opencollective(&token, &slug, &title, &html, &retry_policy)
            })
        })()
    {
        errors.push(format!("opencollective: {e}"));
    }

    // ----------------------------------------------------------------
    // Email (SMTP or sendmail/msmtp fallback)
    // ----------------------------------------------------------------
    if let Some(cfg) = &announce.email
        && is_enabled(ctx, cfg.enabled.as_ref())?
        && let Err(e) = (|| -> Result<()> {
            // Skip-when-empty UX: missing `from` or empty `to` skips the
            // email announcer in normal mode (warn) and bails in strict
            // mode. A configured-but-malformed `from` (string set but no
            // `@`) is a config error, not skip-when-empty, so it stays a
            // hard bail regardless of strict mode.
            let from = match cfg.from.as_deref() {
                Some(raw) => ctx.render_template(raw)?,
                None => {
                    ctx.strict_guard(&log, "announce.email: missing from")?;
                    return Ok(());
                }
            };

            if !from.contains('@') {
                anyhow::bail!(
                    "announce.email: 'from' address {:?} does not look like a valid email (missing @)",
                    from
                );
            }

            if cfg.to.is_empty() {
                ctx.strict_guard(&log, "announce.email: missing to (recipient list)")?;
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

            // Support SMTP_HOST and SMTP_PORT env vars as fallbacks (like GoReleaser).
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
                    email::send_smtp(&email_params, &smtp_params, &retry_policy)
                })?;
            } else {
                dispatch(ctx, "email", &log_line, || {
                    email::send_sendmail(&email_params)
                })?;
            }
            Ok(())
        })()
    {
        errors.push(format!("email: {e}"));
    }

    // Report all collected errors together.
    if !errors.is_empty() {
        anyhow::bail!("announce errors:\n{}", errors.join("\n"));
    }

    Ok(())
}

/// Write `--summary-json` (if configured) and pretty-print the
/// per-publisher status table to stderr.
///
/// Called unconditionally at the end of `AnnounceStage::run` because
/// the audit trail is most valuable when partial failures occur —
/// dropping it on the early-return / gate-fire paths would defeat
/// the point. Errors writing the file are warned, never fatal: a
/// secondary observability channel must not be able to fail the
/// release.
/// End-of-pipeline run-summary emitter.
///
/// Always invoked by `Pipeline::run` at the very end (success or
/// failure) so the per-publisher status table prints to stderr and
/// `--summary-json=<path>` is honored regardless of whether the
/// announce stage itself ran. Owned at the pipeline layer so
/// `--skip=announce` does not silently drop the summary write.
///
/// Best-effort: a `summary.json` write failure logs a warn but never
/// escalates to a pipeline error — the release itself is unaffected
/// by a missing observability artifact.
pub fn emit_summary(ctx: &mut Context) {
    let summary = anodizer_stage_publish::run_summary::RunSummary::from_context(ctx);
    if let Some(path) = ctx.options.summary_json_path.clone() {
        let log = ctx.logger("announce");
        match anodizer_stage_publish::run_summary::write_summary_json(&summary, &path) {
            Ok(()) => log.status(&format!("summary: wrote {}", path.display())),
            Err(err) => log.warn(&format!(
                "summary: failed to write {}: {err}",
                path.display()
            )),
        }
    }
    // Always pretty-print the status table to stderr so non-CI runs
    // see the per-publisher outcome at a glance.
    let mut stderr = std::io::stderr();
    let _ = anodizer_stage_publish::run_summary::print_status_table(&summary, &mut stderr);
}

#[cfg(test)]
mod gate_tests {
    use super::*;
    use anodizer_core::config::{AnnounceConfig, Config};
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::publish_report::{
        PublishReport, PublisherGroup, PublisherOutcome, PublisherResult, SkipReason,
    };
    use anodizer_core::stage::Stage;

    fn failed_result(name: &str, group: PublisherGroup, required: bool) -> PublisherResult {
        PublisherResult {
            name: name.to_string(),
            group,
            required,
            outcome: PublisherOutcome::Failed("boom".to_string()),
            evidence: None,
        }
    }

    fn make_ctx(gate_on: AnnounceGate, report: Option<PublishReport>) -> Context {
        let config = Config {
            project_name: "myapp".to_string(),
            announce: Some(AnnounceConfig {
                gate_on,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.publish_report = report;
        ctx
    }

    // ---- helper coverage -----------------------------------------------

    #[test]
    fn evaluate_gate_none_never_skips() {
        let mut r = PublishReport::default();
        r.results
            .push(failed_result("p", PublisherGroup::Manager, true));
        assert!(!evaluate_gate(None, AnnounceGate::None));
        assert!(!evaluate_gate(Some(&r), AnnounceGate::None));
    }

    #[test]
    fn evaluate_gate_required_skips_only_on_required_failure() {
        let mut r = PublishReport::default();
        r.results
            .push(failed_result("p", PublisherGroup::Manager, false));
        assert!(!evaluate_gate(Some(&r), AnnounceGate::RequiredPublishers));

        r.results
            .push(failed_result("q", PublisherGroup::Submitter, true));
        assert!(evaluate_gate(Some(&r), AnnounceGate::RequiredPublishers));
    }

    #[test]
    fn evaluate_gate_all_skips_on_any_failure() {
        // AllPublishers gates on Failed (regardless of required), not
        // on "any non-Succeeded". This test covers the base failure
        // case; the dedicated tests below pin every happy-path-pending
        // / skip-not-configured variant to NOT gate.
        let mut r = PublishReport::default();
        r.results
            .push(failed_result("p", PublisherGroup::Manager, false));
        assert!(evaluate_gate(Some(&r), AnnounceGate::AllPublishers));
    }

    // ---- happy-path-pending outcomes must NOT gate announce ----------

    /// Construct a `PublisherResult` with an arbitrary outcome — used by
    /// the variant-specific tests below where we need to exercise outcomes
    /// the basic `failed_result` helper doesn't reach.
    fn result_with_outcome(
        name: &str,
        group: PublisherGroup,
        required: bool,
        outcome: PublisherOutcome,
    ) -> PublisherResult {
        PublisherResult {
            name: name.to_string(),
            group,
            required,
            outcome,
            evidence: None,
        }
    }

    #[test]
    fn evaluate_gate_all_does_not_gate_on_pending_moderation() {
        // Chocolatey publishers always end on PendingModeration on a
        // first run — gating announce on this would defeat the stage
        // for any release that includes chocolatey.
        let mut r = PublishReport::default();
        r.results.push(result_with_outcome(
            "choco",
            PublisherGroup::Submitter,
            true,
            PublisherOutcome::PendingModeration,
        ));
        assert!(!evaluate_gate(Some(&r), AnnounceGate::AllPublishers));
    }

    #[test]
    fn evaluate_gate_all_does_not_gate_on_pending_validation() {
        // winget publishers always end on PendingValidation while the
        // microsoft/winget-pkgs PR pipeline runs — same rationale as
        // PendingModeration above.
        let mut r = PublishReport::default();
        r.results.push(result_with_outcome(
            "winget",
            PublisherGroup::Submitter,
            true,
            PublisherOutcome::PendingValidation,
        ));
        assert!(!evaluate_gate(Some(&r), AnnounceGate::AllPublishers));
    }

    #[test]
    fn evaluate_gate_all_does_not_gate_on_skipped_not_configured() {
        // "No work to do" is not a failure.
        let mut r = PublishReport::default();
        r.results.push(result_with_outcome(
            "p",
            PublisherGroup::Manager,
            false,
            PublisherOutcome::Skipped(SkipReason::NotConfigured),
        ));
        assert!(!evaluate_gate(Some(&r), AnnounceGate::AllPublishers));
    }

    #[test]
    fn evaluate_gate_all_does_not_gate_on_skipped_snapshot_or_dry_run() {
        for reason in [SkipReason::Snapshot, SkipReason::DryRun] {
            let mut r = PublishReport::default();
            r.results.push(result_with_outcome(
                "p",
                PublisherGroup::Manager,
                false,
                PublisherOutcome::Skipped(reason),
            ));
            assert!(
                !evaluate_gate(Some(&r), AnnounceGate::AllPublishers),
                "skipped(reason={reason:?}) must not gate announce",
            );
        }
    }

    #[test]
    fn evaluate_gate_all_does_not_gate_on_rolled_back_or_published_no_rollback() {
        // Both outcomes represent "publisher reached a clean terminal
        // state without escalating to Failed". They are NOT failures.
        for outcome in [
            PublisherOutcome::RolledBack,
            PublisherOutcome::PublishedNoRollback,
            PublisherOutcome::RollbackSkippedNoScope,
        ] {
            let mut r = PublishReport::default();
            r.results.push(result_with_outcome(
                "p",
                PublisherGroup::Manager,
                false,
                outcome.clone(),
            ));
            assert!(
                !evaluate_gate(Some(&r), AnnounceGate::AllPublishers),
                "outcome={outcome:?} must not gate announce",
            );
        }
    }

    #[test]
    fn evaluate_gate_all_gates_on_rollback_failed() {
        // RollbackFailed IS a failure-like outcome — the operator
        // needs to know announce was suppressed pending recovery.
        let mut r = PublishReport::default();
        r.results.push(result_with_outcome(
            "p",
            PublisherGroup::Manager,
            false,
            PublisherOutcome::RollbackFailed("transient".into()),
        ));
        assert!(evaluate_gate(Some(&r), AnnounceGate::AllPublishers));
    }

    #[test]
    fn evaluate_gate_all_gates_on_submitter_gated_skip() {
        // SubmitterGated means a Submitter publisher was held back
        // because an upstream Assets/Manager failure tripped the
        // dispatch-time submitter gate. From announce's perspective
        // this is a failure-like outcome — the release did not reach
        // every configured channel and announcing as-is would be
        // misleading.
        let mut r = PublishReport::default();
        r.results.push(result_with_outcome(
            "p",
            PublisherGroup::Submitter,
            true,
            PublisherOutcome::Skipped(SkipReason::SubmitterGated),
        ));
        assert!(evaluate_gate(Some(&r), AnnounceGate::AllPublishers));
    }

    #[test]
    fn evaluate_gate_all_mixed_happy_path_pending_alongside_succeeded() {
        // Realistic post-release report: chocolatey pending, cargo
        // succeeded, krew skipped(not_configured). No gating expected.
        let mut r = PublishReport::default();
        r.results.push(result_with_outcome(
            "choco",
            PublisherGroup::Submitter,
            true,
            PublisherOutcome::PendingModeration,
        ));
        r.results.push(result_with_outcome(
            "cargo",
            PublisherGroup::Submitter,
            true,
            PublisherOutcome::Succeeded,
        ));
        r.results.push(result_with_outcome(
            "krew",
            PublisherGroup::Submitter,
            false,
            PublisherOutcome::Skipped(SkipReason::NotConfigured),
        ));
        assert!(!evaluate_gate(Some(&r), AnnounceGate::AllPublishers));
    }

    #[test]
    fn evaluate_gate_returns_false_when_report_is_none() {
        // No report ≡ no failures, so announce proceeds under any gate.
        assert!(!evaluate_gate(None, AnnounceGate::RequiredPublishers));
        assert!(!evaluate_gate(None, AnnounceGate::AllPublishers));
        assert!(!evaluate_gate(None, AnnounceGate::None));
    }

    // ---- stage-level coverage ------------------------------------------

    #[test]
    fn announce_runs_when_gate_is_none() {
        let mut r = PublishReport::default();
        r.results
            .push(failed_result("p", PublisherGroup::Submitter, true));
        let mut ctx = make_ctx(AnnounceGate::None, Some(r));
        assert!(AnnounceStage.run(&mut ctx).is_ok());
        // Stage proceeded past the gate, so announce_gated must remain false.
        assert!(!ctx.publish_report.as_ref().unwrap().announce_gated);
    }

    #[test]
    fn announce_skips_when_gate_required_and_required_failure() {
        let mut r = PublishReport::default();
        r.results
            .push(failed_result("p", PublisherGroup::Submitter, true));
        let mut ctx = make_ctx(AnnounceGate::RequiredPublishers, Some(r));
        assert!(AnnounceStage.run(&mut ctx).is_ok());
        assert!(ctx.publish_report.as_ref().unwrap().announce_gated);
    }

    #[test]
    fn announce_runs_when_gate_required_and_only_optional_failed() {
        let mut r = PublishReport::default();
        r.results
            .push(failed_result("p", PublisherGroup::Manager, false));
        let mut ctx = make_ctx(AnnounceGate::RequiredPublishers, Some(r));
        assert!(AnnounceStage.run(&mut ctx).is_ok());
        assert!(!ctx.publish_report.as_ref().unwrap().announce_gated);
    }

    #[test]
    fn announce_skips_when_gate_all_and_any_failure() {
        let mut r = PublishReport::default();
        r.results
            .push(failed_result("p", PublisherGroup::Manager, false));
        let mut ctx = make_ctx(AnnounceGate::AllPublishers, Some(r));
        assert!(AnnounceStage.run(&mut ctx).is_ok());
        assert!(ctx.publish_report.as_ref().unwrap().announce_gated);
    }

    #[test]
    fn announce_runs_when_report_is_none() {
        // No report on Context (publish stage --skip'd). All three gates
        // resolve to "proceed" because no failures means nothing to gate on.
        for gate in [
            AnnounceGate::RequiredPublishers,
            AnnounceGate::AllPublishers,
            AnnounceGate::None,
        ] {
            let mut ctx = make_ctx(gate, None);
            assert!(AnnounceStage.run(&mut ctx).is_ok(), "gate={gate:?}");
            assert!(ctx.publish_report.is_none(), "gate={gate:?}");
        }
    }

    #[test]
    fn announce_gate_serializes_as_snake_case() {
        let s = serde_json::to_string(&AnnounceGate::RequiredPublishers).expect("serialize");
        assert_eq!(s, "\"required_publishers\"");
        let s = serde_json::to_string(&AnnounceGate::AllPublishers).expect("serialize");
        assert_eq!(s, "\"all_publishers\"");
        let s = serde_json::to_string(&AnnounceGate::None).expect("serialize");
        assert_eq!(s, "\"none\"");
    }

    #[test]
    fn announce_config_default_gate_is_required_publishers() {
        assert_eq!(
            AnnounceConfig::default().gate_on,
            AnnounceGate::RequiredPublishers
        );
    }
}

#[cfg(test)]
mod summary_tests {
    //! End-of-pipeline run-summary emission. Verifies the summary is
    //! produced regardless of how AnnounceStage resolved (no-op skip,
    //! gate fire, etc.) and that a write failure never escalates into a
    //! pipeline error.

    use super::*;
    use anodizer_core::config::{AnnounceConfig, AnnounceGate, Config};
    use anodizer_core::context::ContextOptions;
    use anodizer_core::publish_report::{
        PublishReport, PublisherGroup, PublisherOutcome, PublisherResult,
    };
    use anodizer_core::stage::Stage;
    use anodizer_stage_publish::run_summary::RunSummary;
    use std::fs;

    fn ctx_with(
        opts: ContextOptions,
        announce_cfg: Option<AnnounceConfig>,
        report: Option<PublishReport>,
    ) -> Context {
        let config = Config {
            project_name: "myapp".to_string(),
            announce: announce_cfg,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v1.2.3");
        ctx.publish_report = report;
        ctx
    }

    fn opts_with_summary_path(p: std::path::PathBuf) -> ContextOptions {
        ContextOptions {
            summary_json_path: Some(p),
            ..ContextOptions::default()
        }
    }

    fn parse_summary(p: &std::path::Path) -> RunSummary {
        let text = fs::read_to_string(p).expect("read summary.json");
        serde_json::from_str(&text).expect("parse summary.json")
    }

    // `emit_summary` is invoked at the pipeline layer (see
    // `crates/cli/src/pipeline.rs::Pipeline::run`), not from inside
    // `AnnounceStage::run`. These tests exercise `emit_summary`
    // directly to keep the stage-level contract pinned; the
    // pipeline-layer integration that ensures the call always fires
    // (including under `--skip=announce`) is covered by the
    // integration test
    // `announce_skipped_via_skip_flag_still_emits_summary` in
    // `crates/cli/tests`.

    #[test]
    fn emit_summary_writes_summary_when_path_set() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let summary_path = tmp.path().join("summary.json");

        // No announce config — irrelevant to emit_summary; we are
        // testing the summary-emission contract directly.
        let mut ctx = ctx_with(
            opts_with_summary_path(summary_path.clone()),
            None,
            Some(PublishReport::default()),
        );
        emit_summary(&mut ctx);

        assert!(summary_path.exists(), "summary.json must be written");
        let summary = parse_summary(&summary_path);
        assert_eq!(summary.schema_version, RunSummary::CURRENT_SCHEMA_VERSION);
        assert_eq!(summary.tag, "v1.2.3");
    }

    #[test]
    fn emit_summary_does_not_panic_when_write_fails() {
        // Path points at a directory — `fs::write` will fail with EISDIR.
        // emit_summary must NOT panic (the write is best-effort); the
        // caller (pipeline) treats it as an observability channel, not
        // a release gate. The function returns `()` so there is no
        // outcome to assert beyond "did not panic."
        let tmp = tempfile::tempdir().expect("tempdir");
        let bad_path = tmp.path().to_path_buf(); // existing directory

        let mut ctx = ctx_with(opts_with_summary_path(bad_path), None, None);
        emit_summary(&mut ctx);
        // No panic = pass.
    }

    #[test]
    fn emit_summary_writes_when_gate_would_fire() {
        // Mirrors the original `announce_stage_emits_summary_when_gate_fires`
        // intent: the summary must be emitted even when announce was
        // gated off. We drive `AnnounceStage.run` first (which sets
        // `announce_gated = true` via the gate path), then invoke
        // `emit_summary` — the order the pipeline layer enforces.
        let tmp = tempfile::tempdir().expect("tempdir");
        let summary_path = tmp.path().join("summary.json");

        // Required failure + gate=required => gate fires inside
        // AnnounceStage::run, which sets announce_gated=true and
        // returns Ok.
        let mut report = PublishReport::default();
        report.results.push(PublisherResult {
            name: "cargo".to_string(),
            group: PublisherGroup::Submitter,
            required: true,
            outcome: PublisherOutcome::Failed("boom".to_string()),
            evidence: None,
        });
        let mut ctx = ctx_with(
            opts_with_summary_path(summary_path.clone()),
            Some(AnnounceConfig {
                gate_on: AnnounceGate::RequiredPublishers,
                ..Default::default()
            }),
            Some(report),
        );
        AnnounceStage.run(&mut ctx).expect("run");
        emit_summary(&mut ctx);

        assert!(
            summary_path.exists(),
            "summary written even when gate fires"
        );
        let summary = parse_summary(&summary_path);
        assert!(
            summary.announce_gated,
            "announce_gated must be set by the gate-fire path"
        );
        assert_eq!(summary.results.len(), 1);
        assert_eq!(summary.results[0].status, "failed");
    }

    #[test]
    fn emit_summary_skips_summary_when_path_unset() {
        // summary_json_path = None => no write; the status table still
        // prints to stderr but that's not asserted here (covered by the
        // dedicated print_status_table test in stage-publish).
        let mut ctx = ctx_with(ContextOptions::default(), None, None);
        emit_summary(&mut ctx);
        // The absence of a panic / error is the assertion; nothing to
        // grep on disk because no path was configured.
    }

    #[test]
    fn emit_summary_writes_when_announce_stage_was_not_called() {
        // Regression: a release that operator-skipped announce entirely
        // (`--skip=announce` in the pipeline) STILL gets a summary
        // write, because emit_summary lives on Pipeline rather than
        // inside AnnounceStage. We model "AnnounceStage.run never
        // invoked" by simply not calling it.
        let tmp = tempfile::tempdir().expect("tempdir");
        let summary_path = tmp.path().join("summary.json");

        let mut ctx = ctx_with(
            opts_with_summary_path(summary_path.clone()),
            None,
            Some(PublishReport::default()),
        );
        // Do NOT call AnnounceStage.run — simulate `--skip=announce`.
        emit_summary(&mut ctx);

        assert!(
            summary_path.exists(),
            "summary must be written even when AnnounceStage::run was skipped",
        );
    }
}
