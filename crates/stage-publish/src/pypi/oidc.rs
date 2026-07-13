//! PyPI Trusted Publishing (OIDC): mint a short-lived upload token from a
//! GitHub Actions OIDC identity, so a release can publish without a stored
//! long-lived `PYPI_TOKEN`.
//!
//! The exchange is two hops, mirroring PyPI's documented Trusted-Publishing
//! flow (and `mcp/auth.rs`'s Actions-OIDC hop):
//!
//! 1. GET `${ACTIONS_ID_TOKEN_REQUEST_URL}&audience=pypi` with
//!    `Authorization: Bearer ${ACTIONS_ID_TOKEN_REQUEST_TOKEN}` → `{"value": <jwt>}`.
//!    Both env vars are set automatically by a GitHub Actions runner granted
//!    `id-token: write`.
//! 2. POST the JWT to the index's `/_/oidc/mint-token` endpoint →
//!    `{"success": true, "token": "pypi-…"}`. That minted token is then used
//!    as the `__token__` Basic-auth password for the legacy upload API,
//!    exactly like a stored token.
//!
//! Unlike npm (where the `npm` CLI performs the exchange and anodizer only
//! threads the request env through), PyPI is uploaded directly over HTTP, so
//! anodizer performs the exchange itself.

use std::time::Duration;

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::redact::redact_bearer_tokens;
use anodizer_core::retry::{RetryLog, RetryPolicy, SuccessClass, retry_http_blocking};
use anyhow::{Context as _, Result, bail};
use serde::Deserialize;

use crate::actions_oidc;

/// The GitHub Actions OIDC request env pair. Preflight requires both when an
/// entry is `auth: oidc`; the run path (via [`actions_oidc`]) errors without
/// them. Aliased here so the pypi preflight and the shared hop-1 request name
/// one source of truth.
pub(crate) const OIDC_ENV_VARS: [&str; 2] = [
    actions_oidc::REQUEST_URL_VAR,
    actions_oidc::REQUEST_TOKEN_VAR,
];

/// PyPI's fixed OIDC audience claim (identical for pypi.org and test.pypi.org;
/// served verbatim by each index's `/_/oidc/audience` endpoint).
const PYPI_AUDIENCE: &str = "pypi";

/// PyPI `mint-token` response. `success` gates on `token`; `message` carries
/// the human-readable reason on failure.
#[derive(Deserialize)]
struct MintResponse {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    token: String,
    #[serde(default)]
    message: String,
}

/// True when an OIDC context is present (both request env vars are non-empty).
/// Used by `auto` mode to decide whether a Trusted-Publishing exchange is even
/// possible before attempting it.
pub(crate) fn oidc_context_available(ctx: &Context) -> bool {
    actions_oidc::context_available(|k| ctx.env_var(k))
}

/// Derive the `/_/oidc/mint-token` endpoint for a resolved upload repository.
/// Returns `None` for a custom index host, which has no Trusted-Publishing
/// contract — the caller turns that into a clear config error.
pub(crate) fn mint_token_url(repository: &str) -> Option<String> {
    let url = reqwest::Url::parse(repository).ok()?;
    match url.host_str()? {
        "upload.pypi.org" | "pypi.org" => Some("https://pypi.org/_/oidc/mint-token".to_string()),
        "test.pypi.org" => Some("https://test.pypi.org/_/oidc/mint-token".to_string()),
        _ => None,
    }
}

/// Exchange the ambient GitHub Actions OIDC identity for a short-lived PyPI
/// upload token. Errors (never falls back to a token) if the request env is
/// absent, the index is not a Trusted-Publishing host, or either hop fails.
pub(crate) fn mint_trusted_publishing_token(
    ctx: &Context,
    repository: &str,
    policy: &RetryPolicy,
    log: &StageLogger,
) -> Result<String> {
    let mint_url = mint_token_url(repository).ok_or_else(|| {
        anyhow::anyhow!(
            "pypi: auth=oidc (Trusted Publishing) is only supported against \
             pypi.org and test.pypi.org, not the custom index '{}' — use a token \
             for a custom index",
            repository
        )
    })?;

    // Hop 1: fetch the Actions id-token for the `pypi` audience (shared with
    // every other OIDC publisher).
    let id_token =
        actions_oidc::request_id_token(|k| ctx.env_var(k), PYPI_AUDIENCE, policy, log, "pypi")?;

    let client = anodizer_core::http::blocking_client(Duration::from_secs(30))
        .context("pypi: build OIDC HTTP client")?;

    // Hop 2: exchange the JWT for a short-lived PyPI upload token.
    let body_json = serde_json::json!({ "token": id_token }).to_string();
    let (_, mint_body) = retry_http_blocking(
        RetryLog::new("pypi: Trusted Publishing mint-token", log),
        policy,
        SuccessClass::Strict,
        |_| {
            client
                .post(&mint_url)
                .header("Content-Type", "application/json")
                .header("Accept", "application/json")
                .body(body_json.clone())
                .send()
        },
        |status, body| {
            format!(
                "pypi: POST {} returned HTTP {}: {}",
                mint_url,
                status,
                redact_bearer_tokens(body)
            )
        },
    )
    // A refused mint that Warehouse returns as HTTP 4xx (e.g. 422) fast-fails
    // here rather than reaching the success:false branch below, so the
    // actionable Trusted-Publisher guidance is attached at both exits.
    .context(
        "pypi: Trusted Publishing mint-token exchange failed — verify the project \
         has a Trusted Publisher (or a pending publisher for a new project) \
         configured for this repository/workflow",
    )?;
    let mint: MintResponse =
        serde_json::from_str(&mint_body).context("pypi: parse mint-token response")?;
    if !mint.success || mint.token.is_empty() {
        bail!(
            "pypi: Trusted Publishing mint-token was refused{} — verify the project \
             has a Trusted Publisher (or a pending publisher for a new project) \
             configured for this repository/workflow",
            if mint.message.is_empty() {
                String::new()
            } else {
                format!(": {}", redact_bearer_tokens(&mint.message))
            }
        );
    }
    log.verbose("minted short-lived PyPI upload token via Trusted Publishing");
    Ok(mint.token)
}
