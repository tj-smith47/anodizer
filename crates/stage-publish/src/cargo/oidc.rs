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

use std::time::{Duration, Instant};

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::redact::redact_bearer_tokens;
use anodizer_core::retry::{RetryLog, RetryPolicy, SuccessClass, retry_http_blocking_deadline};
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
///
/// `deadline` is the publish sequence's wall-clock retry budget, resolved once
/// by the caller and shared by both hops: `Context::retry_deadline` re-anchors
/// at `now` on every call, so minting one per hop would hand a wedged endpoint
/// `retry.max_elapsed` twice over.
pub(crate) fn mint_trusted_publishing_token(
    ctx: &Context,
    policy: &RetryPolicy,
    deadline: Option<Instant>,
    log: &StageLogger,
) -> Result<String> {
    // Hop 1: fetch the Actions id-token for the `crates.io` audience.
    let id_token = actions_oidc::request_id_token(
        |k| ctx.env_var(k),
        CARGO_AUDIENCE,
        policy,
        deadline,
        log,
        "cargo",
    )?;

    let client = anodizer_core::http::blocking_client(Duration::from_secs(30))
        .context("cargo: build OIDC HTTP client")?;

    // Hop 2: exchange the JWT for a short-lived crates.io token. The request
    // body field is `jwt` (crates.io's contract), NOT `token` (which is pypi's).
    let body_json = serde_json::json!({ "jwt": id_token }).to_string();
    let mint_body = actions_oidc::post_mint_token(
        &client, MINT_URL, &body_json, policy, deadline, log, "cargo",
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
///
/// `deadline` is the same budget the publish (or rollback) sequence resolved,
/// not a fresh one: the wall-clock cap is per sequence, and re-anchoring it for
/// the cleanup would let a wedged endpoint spend `retry.max_elapsed` a second
/// time after the publish already spent it. An exhausted budget cannot skip the
/// revoke — the ladder always makes its first attempt and only declines to
/// sleep for a retry — so the token is still asked to die at most one attempt
/// later than it would otherwise. `None` is the unbounded form, for a caller
/// with no context to resolve a budget from.
pub(crate) fn revoke_trusted_publishing_token(
    token: &str,
    policy: &RetryPolicy,
    deadline: Option<Instant>,
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
    let result = delete_minted_token(&client, MINT_URL, token, policy, deadline, log);
    match result {
        Ok(_) => log.verbose("revoked short-lived crates.io Trusted Publishing token"),
        Err(e) => log.warn(&format!(
            "cargo: best-effort revoke of the Trusted Publishing token failed ({}); it \
             self-expires in ~30 minutes",
            redact_bearer_tokens(&format!("{e:#}"))
        )),
    }
}

/// The revoke request itself: `DELETE mint_url` bearing `token`, retried per
/// `policy` and stopped once the next backoff would cross `deadline`. Split
/// from [`revoke_trusted_publishing_token`] (which owns the client and the
/// best-effort logging) so the wall-clock wiring is exercisable against a local
/// endpoint instead of crates.io.
fn delete_minted_token(
    client: &reqwest::blocking::Client,
    mint_url: &str,
    token: &str,
    policy: &RetryPolicy,
    deadline: Option<Instant>,
    log: &StageLogger,
) -> Result<(reqwest::StatusCode, String)> {
    let bearer = format!("Bearer {token}");
    retry_http_blocking_deadline(
        RetryLog::new("cargo: Trusted Publishing revoke-token", log),
        policy,
        deadline,
        SuccessClass::Strict,
        |_| {
            client
                .delete(mint_url)
                .header("Authorization", &bearer)
                .send()
        },
        |status, body| {
            format!(
                "cargo: DELETE {} returned HTTP {}: {}",
                mint_url,
                status,
                redact_bearer_tokens(body)
            )
        },
    )
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

    mod revoke_deadline {
        use super::*;
        use anodizer_core::log::Verbosity;
        use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
        use std::sync::atomic::Ordering;

        const SERVER_ERROR: &str = "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n";

        fn log() -> StageLogger {
            StageLogger::new("test", Verbosity::Quiet)
        }

        fn client() -> reqwest::blocking::Client {
            anodizer_core::http::blocking_client(Duration::from_secs(5)).expect("client")
        }

        /// A ladder whose attempts are individually cheap but collectively
        /// slow: running it to exhaustion sleeps ~9s across 10 attempts, so one
        /// attempt in milliseconds proves the deadline stopped it.
        fn slow_policy() -> RetryPolicy {
            RetryPolicy {
                max_attempts: 10,
                base_delay: Duration::from_secs(1),
                max_delay: Duration::from_secs(1),
            }
        }

        #[test]
        fn revoke_stops_on_an_already_elapsed_deadline() {
            let (addr, calls) = spawn_oneshot_http_responder(vec![SERVER_ERROR; 10]);
            let url = format!("http://{addr}/api/v1/trusted_publishing/tokens");
            let start = Instant::now();
            let err = delete_minted_token(
                &client(),
                &url,
                "cio-minted-abc",
                &slow_policy(),
                Some(Instant::now()),
                &log(),
            )
            .expect_err("a wedged revoke endpoint must surface an error");
            let chain = format!("{err:#}");
            assert!(
                chain.contains("cargo: DELETE") && chain.contains("503"),
                "{chain}"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "an already-elapsed deadline must stop after ONE attempt, not run the ladder"
            );
            assert!(
                start.elapsed() < Duration::from_secs(1),
                "deadline check must skip the backoff sleep, took {:?}",
                start.elapsed()
            );
        }

        #[test]
        fn revoke_runs_the_full_ladder_without_a_deadline() {
            let (addr, calls) = spawn_oneshot_http_responder(vec![SERVER_ERROR; 3]);
            let url = format!("http://{addr}/api/v1/trusted_publishing/tokens");
            let policy = RetryPolicy {
                max_attempts: 3,
                base_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(2),
            };
            let err = delete_minted_token(&client(), &url, "cio-minted-abc", &policy, None, &log())
                .expect_err("a wedged revoke endpoint must surface an error");
            assert!(format!("{err:#}").contains("cargo: DELETE"), "{err:#}");
            assert_eq!(
                calls.load(Ordering::SeqCst),
                3,
                "no deadline → run the full attempt ladder"
            );
        }
    }
}
