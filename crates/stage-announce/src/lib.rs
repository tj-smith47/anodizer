//! Announce stage — broadcasts a release across configured providers.
//!
//! Per-provider modules (`bluesky`, `discord`, …) own their wire formats and
//! send loops; [`AnnounceStage`] in [`run`] is a fan-out dispatcher that walks
//! the `announce.<provider>` config blocks, renders messages via the shared
//! [`helpers`], and collects errors so one provider failure doesn't block the
//! others.

// Provider modules — already split, untouched by this carve.
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
mod util;
pub mod webhook;

// Stage orchestration — extracted by the lib.rs carve.
mod announcers;
mod dispatch;
mod helpers;
pub mod render_check;
mod run;
mod sent_marker;

#[cfg(test)]
mod tests;

pub use render_check::validate_announce_templates;
pub use run::{AnnounceStage, emit_summary};

/// Dispatch a filtered subset of configured announcers without an idempotency
/// sent-marker (suitable for ad-hoc notifications outside the release pipeline).
///
/// Fire a filtered subset of announce integrations.
///
/// `include` — when `Some`, only fire announcers whose name appears in the
/// slice. `skip` — omit these integration names regardless of `include`.
/// Per-provider errors are collected into `errors` rather than short-circuiting,
/// so one failing integration does not block the others. Announcers run
/// concurrently, bounded by the announce config's
/// [`deadline`](anodizer_core::config::AnnounceConfig::deadline_duration);
/// stragglers past it are abandoned with a warning.
pub fn dispatch_filtered_announcers(
    ctx: &mut anodizer_core::context::Context,
    announce: &anodizer_core::config::AnnounceConfig,
    retry_policy: &anodizer_core::retry::RetryPolicy,
    log: &anodizer_core::log::StageLogger,
    errors: &mut Vec<String>,
    include: Option<&[&str]>,
    skip: &[&str],
) -> anyhow::Result<()> {
    let deadline = announce.deadline_duration();
    announcers::dispatch_filtered_announcers(
        ctx,
        announce,
        retry_policy,
        log,
        deadline,
        errors,
        None,
        announcers::AnnounceFilter { include, skip },
    )
}

/// Environment requirements for the announce stage, derived from the same
/// per-announcer credential resolution `send` performs: per enabled
/// announcer, the env vars its run path reads (webhook URLs and tokens,
/// either as `{{ .Env.X }}` refs in the configured value or as the
/// announcer's documented fallback var). A missing SMTP_PASSWORD or webhook
/// secret otherwise only surfaces at the very END of an otherwise-green
/// release — exactly the failure class preflight exists to kill. Values
/// are never echoed — only referenced env-var names.
///
/// Gating mirrors the run path: nothing when `announce:` is absent, its
/// `skip:` is truthy, or its `if:` renders falsy; an announcer contributes
/// only when its block is present AND its `enabled:` evaluates truthy
/// (absent `enabled:` means off, matching `is_enabled`). An unrenderable
/// `enabled:` template counts as enabled so preflight over-collects.
pub fn env_requirements(
    ctx: &anodizer_core::context::Context,
) -> Vec<anodizer_core::EnvRequirement> {
    use anodizer_core::EnvRequirement as Req;
    use anodizer_core::env_preflight::{entry_inactive, secret_requirement, template_env_refs};

    let Some(a) = ctx.config.announce.as_ref() else {
        return Vec::new();
    };
    if entry_inactive(ctx, a.skip.as_ref(), None, a.if_condition.as_deref()) {
        return Vec::new();
    }

    let active = |enabled: Option<&anodizer_core::config::StringOrBool>| match enabled {
        None => false,
        Some(v) => v
            .try_evaluates_to_true(|t| ctx.render_template(t))
            .unwrap_or(true),
    };
    let all = |vars: &[&str]| Req::EnvAllOf {
        vars: vars.iter().map(|s| (*s).to_string()).collect(),
    };
    let refs_of = |value: &str| {
        let refs = template_env_refs(value);
        (!refs.is_empty()).then_some(Req::EnvAllOf { vars: refs })
    };

    let mut out = Vec::new();

    // discord: DISCORD_WEBHOOK_ID + DISCORD_WEBHOOK_TOKEN env pair, unless
    // a webhook_url is configured (then only its env refs matter).
    if let Some(cfg) = a.discord.as_ref().filter(|c| active(c.enabled.as_ref())) {
        match cfg.webhook_url.as_deref().filter(|v| !v.is_empty()) {
            Some(url) => out.extend(refs_of(url)),
            None => out.push(all(&["DISCORD_WEBHOOK_ID", "DISCORD_WEBHOOK_TOKEN"])),
        }
    }
    if a.discourse
        .as_ref()
        .is_some_and(|c| active(c.enabled.as_ref()))
    {
        out.push(all(&["DISCOURSE_API_KEY"]));
    }
    if let Some(cfg) = a.slack.as_ref().filter(|c| active(c.enabled.as_ref())) {
        out.extend(secret_requirement(
            cfg.webhook_url.as_deref(),
            "SLACK_WEBHOOK",
        ));
    }
    // webhook: endpoint_url is config-supplied (its env refs are required);
    // BASIC_AUTH_HEADER_VALUE / BEARER_TOKEN_HEADER_VALUE are optional auth
    // and deliberately NOT demanded.
    if let Some(cfg) = a.webhook.as_ref().filter(|c| active(c.enabled.as_ref())) {
        if let Some(url) = cfg.endpoint_url.as_deref() {
            out.extend(refs_of(url));
        }
        for value in cfg.headers.iter().flat_map(|h| h.values()) {
            out.extend(refs_of(value));
        }
    }
    if let Some(cfg) = a.telegram.as_ref().filter(|c| active(c.enabled.as_ref())) {
        out.extend(secret_requirement(
            cfg.bot_token.as_deref(),
            "TELEGRAM_TOKEN",
        ));
    }
    if let Some(cfg) = a.teams.as_ref().filter(|c| active(c.enabled.as_ref())) {
        out.extend(secret_requirement(
            cfg.webhook_url.as_deref(),
            "TEAMS_WEBHOOK",
        ));
    }
    if let Some(cfg) = a.mattermost.as_ref().filter(|c| active(c.enabled.as_ref())) {
        out.extend(secret_requirement(
            cfg.webhook_url.as_deref(),
            "MATTERMOST_WEBHOOK",
        ));
    }
    if a.reddit
        .as_ref()
        .is_some_and(|c| active(c.enabled.as_ref()))
    {
        out.push(all(&["REDDIT_SECRET", "REDDIT_PASSWORD"]));
    }
    if a.twitter
        .as_ref()
        .is_some_and(|c| active(c.enabled.as_ref()))
    {
        out.push(all(&[
            "TWITTER_CONSUMER_KEY",
            "TWITTER_CONSUMER_SECRET",
            "TWITTER_ACCESS_TOKEN",
            "TWITTER_ACCESS_TOKEN_SECRET",
        ]));
    }
    if a.mastodon
        .as_ref()
        .is_some_and(|c| active(c.enabled.as_ref()))
    {
        out.push(all(&[
            "MASTODON_ACCESS_TOKEN",
            "MASTODON_CLIENT_ID",
            "MASTODON_CLIENT_SECRET",
        ]));
    }
    if a.bluesky
        .as_ref()
        .is_some_and(|c| active(c.enabled.as_ref()))
    {
        out.push(all(&["BLUESKY_APP_PASSWORD"]));
    }
    if a.linkedin
        .as_ref()
        .is_some_and(|c| active(c.enabled.as_ref()))
    {
        out.push(all(&["LINKEDIN_ACCESS_TOKEN"]));
    }
    if a.opencollective
        .as_ref()
        .is_some_and(|c| active(c.enabled.as_ref()))
    {
        out.push(all(&["OPENCOLLECTIVE_TOKEN"]));
    }
    // email: host and username may come from config or env; the password
    // is required whenever the resolved encryption mode is not `none`
    // (mirroring the send path's needs_password derivation; the SMTP_PORT
    // env fallback is unknowable at derivation time, so the config port —
    // or the same default resolution — decides).
    if let Some(cfg) = a.email.as_ref().filter(|c| active(c.enabled.as_ref())) {
        if cfg.host.as_deref().filter(|h| !h.is_empty()).is_none() {
            out.push(all(&["SMTP_HOST"]));
        }
        if cfg.username.as_deref().filter(|u| !u.is_empty()).is_none() {
            out.push(all(&["SMTP_USERNAME"]));
        }
        let port = helpers::resolve_smtp_port(cfg.port, None);
        let needs_password = !matches!(
            email::resolve_encryption(cfg.encryption.unwrap_or_default(), port),
            anodizer_core::config::EmailEncryption::None
        );
        if needs_password {
            out.push(all(&["SMTP_PASSWORD"]));
        }
    }
    out
}
