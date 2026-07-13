//! Shared GitHub Actions OIDC id-token request — hop 1 of any
//! Trusted-Publishing / provenance exchange.
//!
//! A GitHub Actions runner granted `id-token: write` exposes
//! `ACTIONS_ID_TOKEN_REQUEST_URL` + `ACTIONS_ID_TOKEN_REQUEST_TOKEN`; a `GET`
//! against the URL (with an `audience` query and the request token as a bearer)
//! returns the runner's OIDC id-token (a JWT) for that audience. Each publisher
//! then exchanges the JWT at its own registry endpoint (hop 2): the MCP
//! registry's `/v0/auth/github-oidc`, PyPI's `/_/oidc/mint-token`, etc.
//!
//! Hop 1 is identical across publishers, so it lives here; hop 2 stays with
//! each publisher since the endpoint and response shape differ.

use std::time::Duration;

use anodizer_core::log::StageLogger;
use anodizer_core::redact::redact_bearer_tokens;
use anodizer_core::retry::{RetryLog, RetryPolicy, SuccessClass, retry_http_blocking};
use anodizer_core::url::percent_encode_unreserved;
use anyhow::{Context as _, Result, bail};
use serde::Deserialize;

/// The Actions OIDC request env pair, injected by the runner when the job is
/// granted `id-token: write`.
pub(crate) const REQUEST_URL_VAR: &str = "ACTIONS_ID_TOKEN_REQUEST_URL";
pub(crate) const REQUEST_TOKEN_VAR: &str = "ACTIONS_ID_TOKEN_REQUEST_TOKEN";

/// GitHub Actions id-token response (`{"value": "<jwt>"}`).
#[derive(Deserialize)]
struct IdTokenValue {
    #[serde(default)]
    value: String,
}

/// True when both request env vars are present and non-empty — i.e. the job is
/// running under GitHub Actions with `id-token: write`.
pub(crate) fn context_available(get_env: impl Fn(&str) -> Option<String>) -> bool {
    [REQUEST_URL_VAR, REQUEST_TOKEN_VAR]
        .iter()
        .all(|v| get_env(v).is_some_and(|s| !s.is_empty()))
}

/// Fetch the GitHub Actions OIDC id-token (JWT) for `audience`, reading the
/// request env via `get_env`. `who` prefixes every error/log message (e.g.
/// `"pypi"`, `"mcp"`). Never falls back to anything — an absent request env or
/// a failed fetch is an error. The returned JWT is exchanged by the caller at
/// its own registry endpoint.
pub(crate) fn request_id_token(
    get_env: impl Fn(&str) -> Option<String>,
    audience: &str,
    policy: &RetryPolicy,
    log: &StageLogger,
    who: &str,
) -> Result<String> {
    let request_url = get_env(REQUEST_URL_VAR)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{who}: OIDC requires {REQUEST_URL_VAR} (set automatically by a GitHub \
                 Actions runner with id-token: write permission)"
            )
        })?;
    let request_token = get_env(REQUEST_TOKEN_VAR)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{who}: OIDC requires {REQUEST_TOKEN_VAR} (set automatically by a GitHub \
                 Actions runner with id-token: write permission)"
            )
        })?;

    let client = anodizer_core::http::blocking_client(Duration::from_secs(30))
        .with_context(|| format!("{who}: build OIDC HTTP client"))?;
    let separator = if request_url.contains('?') { '&' } else { '?' };
    let url = format!(
        "{request_url}{separator}audience={}",
        percent_encode_unreserved(audience)
    );
    let desc = format!("{who}: GitHub Actions OIDC token");
    let (_, body) = retry_http_blocking(
        RetryLog::new(&desc, log),
        policy,
        SuccessClass::Strict,
        |_| {
            client
                .get(&url)
                .header("Authorization", format!("Bearer {request_token}"))
                .header("Accept", "application/json")
                .send()
        },
        |status, body| {
            // `url` carries the audience query but no secret (the request token
            // rides the Authorization header, not the URL); naming it keeps a
            // misconfigured ACTIONS_ID_TOKEN_REQUEST_URL diagnosable from logs.
            format!(
                "{who}: GET {} returned HTTP {}: {}",
                url,
                status,
                redact_bearer_tokens(body)
            )
        },
    )
    .with_context(|| format!("{who}: fetch GitHub Actions id-token"))?;
    let parsed: IdTokenValue = serde_json::from_str(&body)
        .with_context(|| format!("{who}: parse Actions id-token response"))?;
    if parsed.value.is_empty() {
        bail!("{who}: Actions id-token response missing value");
    }
    Ok(parsed.value)
}
