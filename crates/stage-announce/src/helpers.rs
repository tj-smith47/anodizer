use std::collections::{BTreeMap, HashMap};

use anodizer_core::config::StringOrBool;
use anodizer_core::context::Context;
use anodizer_core::retry::{RetryPolicy, SuccessClass, retry_http_blocking};
use anodizer_core::{EnvSource, ProcessEnvSource};
use anyhow::{Context as _, Result};

// ---------------------------------------------------------------------------
// Shared helpers to reduce boilerplate across providers
// ---------------------------------------------------------------------------

/// Display name shown to recipients in chat-platform announcements
/// (Discord embed `author`, Slack/Mattermost webhook `username`).
///
/// **Brand-default policy**: anodizer keeps its own attribution. The
/// message *is* from anodize, and impersonating a different release tool in
/// someone's release channels is wrong UX.
///
/// Companion decision: discord/teams `icon_url` defaults stay `None` rather
/// than pointing at a third-party avatar URL — anodizer does not host an
/// avatar URL today, and an image it does not control is a worse default
/// than no image. Revisit when the docsite ships an avatar.
///
/// `User-Agent` has its own const (`anodizer_core::http::USER_AGENT`) which
/// includes the version suffix; this one is the bare display string.
pub(crate) const DEFAULT_DISPLAY_NAME: &str = "anodizer";

pub(crate) const DEFAULT_MESSAGE_TEMPLATE: &str =
    "{{ ProjectName }} {{ Tag }} is out! Check it out at {{ ReleaseURL }}";

/// the webhook default payload wraps the
/// message in a JSON envelope so the receiver always gets a valid JSON body.
pub(crate) const WEBHOOK_DEFAULT_MESSAGE_TEMPLATE: &str =
    r#"{"message":"{{ ProjectName }} {{ Tag }} is out! Check it out at {{ ReleaseURL }}"}"#;

/// Evaluate an `enabled` field (now `Option<StringOrBool>`) through the template
/// engine. Returns `Ok(true)` only when the value is present and resolves to
/// truthy. Surfaces template render errors instead of silently treating them
/// as "not enabled".
pub(crate) fn is_enabled(ctx: &mut Context, enabled: Option<&StringOrBool>) -> Result<bool> {
    match enabled {
        None => Ok(false),
        Some(val) => val
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .with_context(|| "announce: render enabled template"),
    }
}

/// Read a required env var, bailing with a unified message when it is missing
/// or empty after trim. Avoids the duplicated `var(...).map_err(...)?; if
/// empty bail!()` pattern across every provider.
#[allow(dead_code)]
pub(crate) fn require_env(provider: &str, name: &str) -> Result<String> {
    require_env_with_env(provider, name, &ProcessEnvSource)
}

/// Env-injectable form of [`require_env`]. Production wires up
/// [`ProcessEnvSource`]; tests inject a
/// [`anodizer_core::MapEnvSource`] so the missing/empty branches can
/// be exercised without mutating the process env.
pub(crate) fn require_env_with_env<E: EnvSource + ?Sized>(
    provider: &str,
    name: &str,
    env: &E,
) -> Result<String> {
    let value = env
        .var(name)
        .ok_or_else(|| anyhow::anyhow!("announce.{provider}: {name} env var is required"))?;
    if value.trim().is_empty() {
        anyhow::bail!("announce.{provider}: {name} env var must not be empty");
    }
    Ok(value)
}

/// Read an env var that is required and must not be empty, returning a clear
/// error message identifying both the announcer and the missing variable.
///
/// Env-tag presence validation, which fail-fasts before
/// any network calls when a required credential env var is missing.
///
/// Distinct from [`require_env`]: the former bails on missing OR empty (after
/// trim), and is used for env vars that are *the* credential (a single token).
/// This helper is intentionally stricter — it bails on **empty after trim**
/// just like `require_env`, but exists as a named entry-point for the
/// required-env tag set so call sites read like the config they mirror.
#[allow(dead_code)]
pub(crate) fn require_non_empty_env(provider: &str, name: &str) -> Result<String> {
    require_non_empty_env_with_env(provider, name, &ProcessEnvSource)
}

/// Env-injectable form of [`require_non_empty_env`]. Production wires
/// up [`ProcessEnvSource`]; tests inject a
/// [`anodizer_core::MapEnvSource`].
pub(crate) fn require_non_empty_env_with_env<E: EnvSource + ?Sized>(
    provider: &str,
    name: &str,
    env: &E,
) -> Result<String> {
    match env.var(name) {
        Some(v) if !v.trim().is_empty() => Ok(v),
        _ => Err(anyhow::anyhow!(
            "announce.{provider}: {name} env var is required and must not be empty"
        )),
    }
}

/// Read multiple required env vars in one shot, returning a single error that
/// lists every missing/empty var so users can fix them all at once instead of
/// hitting one error per CI run.
#[allow(dead_code)]
pub(crate) fn require_env_all(provider: &str, names: &[&str]) -> Result<Vec<String>> {
    require_env_all_with_env(provider, names, &ProcessEnvSource)
}

/// Env-injectable form of [`require_env_all`]. Production wires up
/// [`ProcessEnvSource`]; tests inject a
/// [`anodizer_core::MapEnvSource`] so the missing/empty aggregation
/// can be driven deterministically.
pub(crate) fn require_env_all_with_env<E: EnvSource + ?Sized>(
    provider: &str,
    names: &[&str],
    env: &E,
) -> Result<Vec<String>> {
    let mut missing: Vec<String> = Vec::new();
    let mut values: Vec<String> = Vec::with_capacity(names.len());
    for name in names {
        match env.var(name) {
            Some(v) if !v.trim().is_empty() => values.push(v),
            Some(_) => {
                missing.push(format!("{name} (empty)"));
                values.push(String::new());
            }
            None => {
                missing.push((*name).to_string());
                values.push(String::new());
            }
        }
    }
    if !missing.is_empty() {
        anyhow::bail!(
            "announce.{provider}: required env vars missing or empty: {}",
            missing.join(", ")
        );
    }
    Ok(values)
}

/// Render a message-BODY template, falling back to `default` when unset.
///
/// The single send-time chokepoint for announce message bodies. When
/// [`Context::literal_message`](anodizer_core::context::Context::literal_message)
/// is set (by `anodizer notify`), the body is returned verbatim — already-final
/// operator text that must NOT be run through Tera again, so an untrusted
/// message cannot expand an `Env`-reference into a secret at send time.
/// Otherwise the body renders through Tera as usual. Per-provider message
/// defaults differ (plain-text vs the webhook JSON envelope vs the Telegram
/// MarkdownV2 form), so callers pass their own `default`.
pub(crate) fn render_message_with_default(
    ctx: &mut Context,
    tmpl: Option<&str>,
    default: &str,
) -> Result<String> {
    let t = tmpl.unwrap_or(default);
    if ctx.literal_message {
        Ok(t.to_owned())
    } else {
        ctx.render_template(t)
    }
}

/// Render a message-BODY template, falling back to the standard plain-text
/// default. Thin wrapper over [`render_message_with_default`] for the providers
/// that use [`DEFAULT_MESSAGE_TEMPLATE`].
pub(crate) fn render_message(ctx: &mut Context, tmpl: Option<&str>) -> Result<String> {
    render_message_with_default(ctx, tmpl, DEFAULT_MESSAGE_TEMPLATE)
}

/// Resolve the effective SMTP port from (config, SMTP_PORT env, default).
///
/// Anodize-additive UX win (locked 2026-04-28): when both `cfg.port` and
/// `SMTP_PORT` are unset we default to **587** — the IETF submission port,
/// the conventional STARTTLS endpoint exposed by virtually every modern
/// SMTP relay (Postfix, Exim, sendgrid, mailgun, AWS SES, …). The
/// An unset port would otherwise be an error; the default-587 path is
/// tradeoff-free because operators who need a
/// different port set it explicitly, and the `auto` encryption mode then
/// picks STARTTLS for 587 (matching the wire reality). Pinned by
/// `test_email_smtp_port_defaults_to_587`.
pub(crate) fn resolve_smtp_port(cfg_port: Option<u16>, env_port: Option<u16>) -> u16 {
    cfg_port.or(env_port).unwrap_or(587)
}

/// Render template variables inside a `serde_json::Value` by serializing to
/// string, running through the template engine, then parsing back. Skips the
/// round-trip when the serialised form has no template markers, since a
/// no-op render would still pay for two JSON parses.
pub(crate) fn render_json_template(
    ctx: &Context,
    val: Option<&serde_json::Value>,
) -> Result<Option<serde_json::Value>> {
    match val {
        Some(v) => {
            let json_str = v.to_string().replace("\\\"", "\"");
            if !json_str.contains("{{") && !json_str.contains("{%") {
                return Ok(Some(v.clone()));
            }
            let rendered = ctx.render_template(&json_str)?;
            Ok(Some(serde_json::from_str(&rendered)?))
        }
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Webhook header resolver
// ---------------------------------------------------------------------------

/// Run a closure that performs a single HTTP request, classifying transport
/// errors and HTTP status codes via [`anodizer_core::retry`] and retrying
/// 5xx / 429 / network failures up to `policy.max_attempts`. The closure
/// returns the response body on success.
///
/// Used by every announcer that doesn't go through `crate::http::post_json`
/// (bluesky, reddit, twitter, discourse, …) so the retry policy is consistent
/// across providers.
///
/// Thin adapter over [`retry_http_blocking`] that drops the status from the
/// `(StatusCode, String)` return tuple to keep the announce-callsite
/// signature (`-> Result<String>`) backward-compatible. The classification
/// logic (HttpError + is_retriable + as_ref vs root_cause) lives in the core
/// helper — pinned by `crates/core/src/retry.rs::classifier_5xx_via_anyhow_chain_uses_as_ref`.
pub(crate) fn retry_http<F>(
    provider: &str,
    stage: &str,
    policy: &RetryPolicy,
    mut send: F,
) -> Result<String>
where
    F: FnMut() -> reqwest::Result<reqwest::blocking::Response>,
{
    let label = format!("{provider}: {stage}");
    let (_status, body) = retry_http_blocking(
        &label,
        policy,
        SuccessClass::Strict,
        |_attempt| send(),
        |status, body| format!("{provider}: {stage} failed ({status}): {body}"),
    )?;
    Ok(body)
}

/// Resolve the effective webhook header set: start with user-supplied
/// `headers`, then apply anodizer's defaults for `Authorization` (from
/// `BASIC_AUTH_HEADER_VALUE` or `BEARER_TOKEN_HEADER_VALUE`) and
/// `User-Agent` (`anodizer_core::http::USER_AGENT`) only when the user did
/// not already supply that header.
///
/// HTTP header names are case-insensitive (RFC 7230 §3.2). A user who
/// writes `headers: { authorization: "user-foo" }` (lowercase) expects
/// their value to win over anodizer's default — but a naive
/// `headers.contains_key("Authorization")` lookup would miss the lowercase
/// key, push BOTH the user's `authorization` AND anodizer's `Authorization`
/// onto the wire, and let reqwest send two competing headers. This
/// helper case-folds the lookup so any spelling of `authorization` /
/// `user-agent` (or any other override) is honored.
///
/// Pinned by `test_resolve_webhook_headers_*` — drift back to a
/// case-sensitive `contains_key` will trip those tests.
///
/// Q-wh1: returns a [`BTreeMap`] (not a `HashMap`) so the downstream
/// `send_webhook` iteration order is alphabetical / deterministic. The
/// callers convert their YAML-derived `HashMap<String, String>` user headers
/// via this helper so the deterministic order propagates through the whole
/// webhook pipeline.
pub(crate) fn resolve_webhook_headers(
    user_headers: HashMap<String, String>,
    basic_auth: Option<&str>,
    bearer_token: Option<&str>,
    user_agent_default: &str,
) -> BTreeMap<String, String> {
    let mut headers: BTreeMap<String, String> = user_headers.into_iter().collect();
    // O(n) per lookup, O(n²) over the precedence walk. Fine for webhook
    // header counts (typically <10); a future optimizer should not reach
    // for `HeaderMap` reflexively.
    let has_user_key = |target: &str, h: &BTreeMap<String, String>| -> bool {
        h.keys().any(|k| k.eq_ignore_ascii_case(target))
    };

    if !has_user_key("Authorization", &headers) {
        if let Some(basic) = basic_auth.filter(|s| !s.is_empty()) {
            headers.insert("Authorization".to_string(), basic.to_string());
        } else if let Some(bearer) = bearer_token.filter(|s| !s.is_empty()) {
            headers.insert("Authorization".to_string(), bearer.to_string());
        }
    }

    if !has_user_key("User-Agent", &headers) {
        headers.insert("User-Agent".to_string(), user_agent_default.to_string());
    }

    headers
}
