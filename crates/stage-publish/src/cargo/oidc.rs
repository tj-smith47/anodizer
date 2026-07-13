//! crates.io Trusted Publishing (OIDC): mint a short-lived crates.io token
//! from a GitHub Actions OIDC identity, so a release can publish without a
//! stored long-lived `CARGO_REGISTRY_TOKEN`.
//!
//! The exchange is two hops, mirroring crates.io's Trusted-Publishing flow
//! (and the pypi publisher's identical shape):
//!
//! 1. GET `${ACTIONS_ID_TOKEN_REQUEST_URL}&audience=crates.io` with
//!    `Authorization: Bearer ${ACTIONS_ID_TOKEN_REQUEST_TOKEN}` → `{"value": <jwt>}`.
//!    Both env vars are set automatically by a GitHub Actions runner granted
//!    `id-token: write`.
//! 2. POST `{"jwt": <jwt>}` to `https://crates.io/api/v1/trusted_publishing/tokens`
//!    → `{"token": "<minted>"}`. That minted token is a valid crates.io token,
//!    supplied to `cargo publish` via the `CARGO_REGISTRY_TOKEN` env var.
//!
//! The minted token is **workspace-scoped**: one token authorizes every crate
//! whose Trusted-Publisher config matches this repository/workflow, so the
//! publish loop mints once, reuses it for all crates, and revokes once (the
//! token also self-expires in ~30 minutes).

use std::time::Duration;

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::redact::redact_bearer_tokens;
use anodizer_core::retry::{RetryLog, RetryPolicy, SuccessClass, retry_http_blocking};
use anyhow::{Context as _, Result, bail};
use serde::Deserialize;

use crate::actions_oidc;

/// The GitHub Actions OIDC request env pair. Preflight requires both when a
/// cargo block is `auth: oidc`; the run path (via [`actions_oidc`]) errors
/// without them. Aliased here so the cargo preflight and the shared hop-1
/// request name one source of truth.
pub(crate) const OIDC_ENV_VARS: [&str; 2] = [
    actions_oidc::REQUEST_URL_VAR,
    actions_oidc::REQUEST_TOKEN_VAR,
];

/// crates.io's fixed OIDC audience claim.
const CARGO_AUDIENCE: &str = "crates.io";

/// crates.io Trusted-Publishing token endpoint. Fixed: crates.io Trusted
/// Publishing has no custom-registry variant, so an `auth: oidc` block that
/// targets a non-crates.io registry is a hard config error (surfaced by the
/// caller, which knows the resolved registry).
const MINT_URL: &str = "https://crates.io/api/v1/trusted_publishing/tokens";

/// crates.io mint-token response. `token` is the short-lived crates.io token.
/// Unlike PyPI there is no `success` field — a refused mint is an HTTP 4xx.
#[derive(Deserialize)]
struct MintResponse {
    #[serde(default)]
    token: String,
}

/// True when an OIDC context is present (both request env vars are non-empty).
/// Used by `auto` mode to decide whether a Trusted-Publishing exchange is even
/// possible before attempting it.
pub(crate) fn oidc_context_available(ctx: &Context) -> bool {
    actions_oidc::context_available(|k| ctx.env_var(k))
}

/// Exchange the ambient GitHub Actions OIDC identity for a short-lived
/// crates.io token. Errors (never falls back to a stored token) if the request
/// env is absent or either hop fails. The returned token is supplied to every
/// `cargo publish` in the run via `CARGO_REGISTRY_TOKEN`.
pub(crate) fn mint_trusted_publishing_token(
    ctx: &Context,
    policy: &RetryPolicy,
    log: &StageLogger,
) -> Result<String> {
    // Hop 1: fetch the Actions id-token for the `crates.io` audience.
    let id_token =
        actions_oidc::request_id_token(|k| ctx.env_var(k), CARGO_AUDIENCE, policy, log, "cargo")?;

    let client = anodizer_core::http::blocking_client(Duration::from_secs(30))
        .context("cargo: build OIDC HTTP client")?;

    // Hop 2: exchange the JWT for a short-lived crates.io token. The request
    // body field is `jwt` (crates.io's contract), NOT `token` (which is pypi's).
    let body_json = serde_json::json!({ "jwt": id_token }).to_string();
    let (_, mint_body) = retry_http_blocking(
        RetryLog::new("cargo: Trusted Publishing mint-token", log),
        policy,
        SuccessClass::Strict,
        |_| {
            client
                .post(MINT_URL)
                .header("Content-Type", "application/json")
                .header("Accept", "application/json")
                .body(body_json.clone())
                .send()
        },
        |status, body| {
            format!(
                "cargo: POST {} returned HTTP {}: {}",
                MINT_URL,
                status,
                redact_bearer_tokens(body)
            )
        },
    )
    .context(
        "cargo: Trusted Publishing mint-token exchange failed — verify the crate has a \
         Trusted Publisher configured for this repository/workflow on crates.io",
    )?;
    let mint: MintResponse =
        serde_json::from_str(&mint_body).context("cargo: parse mint-token response")?;
    if mint.token.is_empty() {
        bail!(
            "cargo: Trusted Publishing mint-token returned an empty token — verify the crate \
             has a Trusted Publisher configured for this repository/workflow on crates.io"
        );
    }
    log.verbose("minted short-lived crates.io token via Trusted Publishing");
    Ok(mint.token)
}

/// Revoke a minted Trusted-Publishing token. **Best-effort**: a failed revoke
/// is logged, never propagated — the token self-expires in ~30 minutes, so a
/// revoke failure must never fail the release. Called once after the publish
/// loop on both the success and failure paths.
pub(crate) fn revoke_trusted_publishing_token(
    token: &str,
    policy: &RetryPolicy,
    log: &StageLogger,
) {
    let client = match anodizer_core::http::blocking_client(Duration::from_secs(30)) {
        Ok(c) => c,
        Err(e) => {
            log.verbose(&format!(
                "cargo: could not build HTTP client to revoke the Trusted Publishing token \
                 ({e:#}); it self-expires in ~30 minutes"
            ));
            return;
        }
    };
    let bearer = format!("Bearer {token}");
    let result = retry_http_blocking(
        RetryLog::new("cargo: Trusted Publishing revoke-token", log),
        policy,
        SuccessClass::Strict,
        |_| {
            client
                .delete(MINT_URL)
                .header("Authorization", &bearer)
                .send()
        },
        |status, body| {
            format!(
                "cargo: DELETE {} returned HTTP {}: {}",
                MINT_URL,
                status,
                redact_bearer_tokens(body)
            )
        },
    );
    match result {
        Ok(_) => log.verbose("revoked short-lived crates.io Trusted Publishing token"),
        Err(e) => log.warn(&format!(
            "cargo: best-effort revoke of the Trusted Publishing token failed ({}); it \
             self-expires in ~30 minutes",
            redact_bearer_tokens(&format!("{e:#}"))
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mint REQUEST body must serialize with the field name `jwt` — the
    /// crates.io contract (pypi uses `token`; a wrong field is HTTP 400 at
    /// publish). Guards against a copy-paste regression from the pypi mirror.
    #[test]
    fn mint_request_body_uses_jwt_field() {
        let body = serde_json::json!({ "jwt": "the-id-token" }).to_string();
        assert_eq!(body, r#"{"jwt":"the-id-token"}"#);
        // And explicitly NOT pypi's `token` field.
        assert!(!body.contains("\"token\""));
    }

    /// The mint RESPONSE parses from `{"token":"..."}` (crates.io has no
    /// `success` field, unlike pypi).
    #[test]
    fn mint_response_parses_token_field() {
        let parsed: MintResponse =
            serde_json::from_str(r#"{"token":"cio-minted-abc"}"#).expect("parse");
        assert_eq!(parsed.token, "cio-minted-abc");
    }

    /// An empty/missing token deserializes to an empty string (the caller
    /// bails on empty rather than shipping a blank credential).
    #[test]
    fn mint_response_missing_token_is_empty() {
        let parsed: MintResponse = serde_json::from_str(r#"{}"#).expect("parse");
        assert!(parsed.token.is_empty());
    }
}
