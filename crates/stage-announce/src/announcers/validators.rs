use anyhow::Result;

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
pub(super) fn validate_discord_color(rendered: &str) -> Result<Option<u32>> {
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
pub(super) fn validate_webhook_endpoint_url(url: &str) -> Result<()> {
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
pub(super) fn validate_telegram_thread_id(rendered: &str) -> Result<Option<i64>> {
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
pub(super) fn validate_email_from(from: &str) -> Result<()> {
    if !from.contains('@') {
        anyhow::bail!(
            "announce.email: 'from' address {from:?} does not look like a valid email (missing @)"
        );
    }
    Ok(())
}
