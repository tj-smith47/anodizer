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
//! Hop 1 is identical across publishers, so it lives here. Hop 2's transport is
//! identical for the two token-minting publishers (cargo and pypi both POST a
//! one-field JSON body and read a JSON body back), so the request itself lives
//! here too as [`post_mint_token`]; the endpoint, the request field name and the
//! response shape stay with each publisher.

use std::time::Duration;

use anodizer_core::log::StageLogger;
use anodizer_core::redact::redact_bearer_tokens;
use anodizer_core::retry::{RetryLog, RetryPolicy, SuccessClass, retry_http_blocking_deadline};
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
///
/// `deadline` is the caller's wall-clock retry budget
/// ([`anodizer_core::context::Context::retry_deadline`]); a stalled Actions
/// token endpoint gives up when the next backoff would cross it instead of
/// spending the publisher's whole ladder before the exchange even begins.
pub(crate) fn request_id_token(
    get_env: impl Fn(&str) -> Option<String>,
    audience: &str,
    policy: &RetryPolicy,
    deadline: Option<std::time::Instant>,
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
    let (_, body) = retry_http_blocking_deadline(
        RetryLog::new(&desc, log),
        policy,
        deadline,
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

/// Hop 2's request: POST `body_json` to a registry's Trusted-Publishing
/// mint-token endpoint and return the raw response body for the caller to
/// parse. `who` prefixes every error/log message. Shared by the cargo and pypi
/// publishers — crates.io and Warehouse take the identical request shape and
/// differ only in the JSON field name (built by the caller) and the response
/// envelope (parsed by the caller).
///
/// `deadline` is the caller's wall-clock retry budget
/// ([`anodizer_core::context::Context::retry_deadline`]), resolved ONCE for the
/// whole exchange: a wedged mint endpoint stops when the next backoff would
/// cross it, rather than running the full attempt ladder after hop 1 already
/// spent part of the budget.
pub(crate) fn post_mint_token(
    client: &reqwest::blocking::Client,
    mint_url: &str,
    body_json: &str,
    policy: &RetryPolicy,
    deadline: Option<std::time::Instant>,
    log: &StageLogger,
    who: &str,
) -> Result<String> {
    let desc = format!("{who}: Trusted Publishing mint-token");
    let (_, body) = retry_http_blocking_deadline(
        RetryLog::new(&desc, log),
        policy,
        deadline,
        SuccessClass::Strict,
        |_| {
            client
                .post(mint_url)
                .header("Content-Type", "application/json")
                .header("Accept", "application/json")
                .body(body_json.to_string())
                .send()
        },
        |status, body| {
            format!(
                "{who}: POST {} returned HTTP {}: {}",
                mint_url,
                status,
                redact_bearer_tokens(body)
            )
        },
    )?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::log::{StageLogger, Verbosity};
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    const SERVER_ERROR: &str = "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n";

    fn log() -> StageLogger {
        StageLogger::new("test", Verbosity::Quiet)
    }

    fn client() -> reqwest::blocking::Client {
        anodizer_core::http::blocking_client(Duration::from_secs(5)).expect("client")
    }

    /// A ladder whose attempts are individually cheap but collectively slow:
    /// running it to exhaustion sleeps ~9s across 10 attempts, so one attempt
    /// in milliseconds proves the deadline (not the attempt count) stopped it.
    fn slow_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 10,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(1),
        }
    }

    #[test]
    fn mint_token_post_stops_on_an_already_elapsed_deadline() {
        let (addr, calls) = spawn_oneshot_http_responder(vec![SERVER_ERROR; 10]);
        let url = format!("http://{addr}/_/oidc/mint-token");
        let start = Instant::now();
        let err = post_mint_token(
            &client(),
            &url,
            r#"{"jwt":"id-token"}"#,
            &slow_policy(),
            Some(Instant::now()),
            &log(),
            "cargo",
        )
        .expect_err("a wedged mint endpoint must surface an error");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("cargo: POST") && chain.contains("503"),
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
    fn mint_token_post_runs_the_full_ladder_without_a_deadline() {
        let (addr, calls) = spawn_oneshot_http_responder(vec![SERVER_ERROR; 3]);
        let url = format!("http://{addr}/_/oidc/mint-token");
        let policy = RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };
        let err = post_mint_token(
            &client(),
            &url,
            r#"{"token":"id-token"}"#,
            &policy,
            None,
            &log(),
            "pypi",
        )
        .expect_err("a wedged mint endpoint must surface an error");
        let chain = format!("{err:#}");
        assert!(chain.contains("pypi: POST"), "{chain}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "no deadline → run the full attempt ladder"
        );
    }
}
