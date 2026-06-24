use std::time::Duration;

use anodizer_core::retry::RetryPolicy;
use anyhow::{Context as _, Result};

use crate::helpers::retry_http;

/// Bounded per-request timeout applied to every announce HTTP client and the
/// SMTP transport, so an unresponsive endpoint cannot hang the announce stage
/// indefinitely.
pub(crate) const ANNOUNCE_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Build the canonical announce blocking client: the shared
/// [`anodizer_core::http::blocking_client`] policy (UA + roots) plus the
/// announce timeout. Single chokepoint so no announcer can construct a
/// timeout-less client.
pub(crate) fn blocking_client() -> Result<reqwest::blocking::Client> {
    anodizer_core::http::blocking_client(ANNOUNCE_HTTP_TIMEOUT)
}

/// Announce blocking client that accepts invalid / self-signed TLS certs
/// (the webhook `skip_tls_verify` option). Carries the same bounded timeout
/// as [`blocking_client`]; `anodizer_core::http::blocking_client` cannot set
/// `danger_accept_invalid_certs`, so the builder is reconstructed here while
/// the timeout policy stays identical.
pub(crate) fn blocking_client_accept_invalid_certs(
    accept_invalid_certs: bool,
) -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(anodizer_core::http::USER_AGENT)
        .timeout(ANNOUNCE_HTTP_TIMEOUT)
        .danger_accept_invalid_certs(accept_invalid_certs)
        .build()
        .context("announce: build HTTP client")
}

/// POST a JSON payload to `url`, returning an error that includes the
/// provider name, HTTP status, and response body on failure.
///
/// The URL is intentionally NOT included in error messages because it may
/// contain secrets (e.g. Telegram bot tokens embedded in the path).
///
/// `policy` controls retry behaviour: 5xx / 429 / transport-level failures
/// retry up to `policy.max_attempts` with exponential backoff; 4xx fast-fails.
/// Pass `RetryConfig::default().to_policy()` (or
/// `ctx.config.retry.unwrap_or_default().to_policy()`) for canonical
/// defaults (10 attempts × 10s base × 5m cap).
///
/// Routed through the shared `retry_http` helper so all chat-webhook
/// announcers (`discord`, `slack`, `mattermost`, `teams`) share one
/// retry-classification surface.
pub(crate) fn post_json(
    url: &str,
    payload: &str,
    provider: &str,
    policy: &RetryPolicy,
) -> Result<()> {
    let client = blocking_client()?;
    let _ = retry_http(provider, "POST", policy, || {
        client
            .post(url)
            .header("Content-Type", "application/json")
            .body(payload.to_string())
            .send()
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ANNOUNCE_HTTP_TIMEOUT, blocking_client, blocking_client_accept_invalid_certs};
    use anodizer_core::retry::{HttpError, is_retriable};
    use std::time::Duration;

    #[test]
    fn announce_http_timeout_is_bounded_and_nonzero() {
        // A timeout-less client can hang the announce stage indefinitely; the
        // bound must stay finite and non-zero.
        assert!(ANNOUNCE_HTTP_TIMEOUT > Duration::ZERO);
        assert_eq!(ANNOUNCE_HTTP_TIMEOUT, Duration::from_secs(30));
    }

    #[test]
    fn announce_blocking_clients_construct() {
        // Both client chokepoints feed ANNOUNCE_HTTP_TIMEOUT into the
        // builder; a successful build proves the timeout-setting path runs.
        blocking_client().expect("default announce client builds");
        blocking_client_accept_invalid_certs(true).expect("skip-tls announce client builds");
        blocking_client_accept_invalid_certs(false).expect("verifying announce client builds");
    }

    /// Reproduce the exact error shape `post_json` constructs for an HTTP
    /// failure response and confirm the retry classifier sees the wrapped
    /// `HttpError` via the anyhow chain. This is the regression test for
    /// the original `root_cause()`-mis-classification bug: 5xx must
    /// retry, 4xx must fast-fail.
    ///
    /// Replaces an earlier no-op test that exercised only `fast_policy()`
    /// without asserting any behaviour. We avoid pulling in `wiremock` /
    /// `mockito` as dev-deps by exercising the classifier directly with
    /// the same wrapping shape `post_json` produces on the wire.
    fn wrap_status_like_post_json(status: u16) -> anyhow::Error {
        let inner = anyhow::anyhow!("provider: HTTP {status} — body");
        anyhow::Error::new(HttpError::new(
            std::io::Error::other(inner.to_string()),
            status,
        ))
        .context(inner)
    }

    #[test]
    fn post_json_classifier_retries_5xx() {
        let wrapped = wrap_status_like_post_json(503);
        assert!(
            is_retriable(wrapped.as_ref()),
            "503 must classify retriable through the anyhow chain"
        );
    }

    #[test]
    fn post_json_classifier_retries_429() {
        let wrapped = wrap_status_like_post_json(429);
        assert!(
            is_retriable(wrapped.as_ref()),
            "429 must classify retriable through the anyhow chain"
        );
    }

    #[test]
    fn post_json_classifier_fastfails_4xx() {
        for status in [400u16, 401, 403, 404, 422] {
            let wrapped = wrap_status_like_post_json(status);
            assert!(
                !is_retriable(wrapped.as_ref()),
                "{status} must classify fast-fail through the anyhow chain"
            );
        }
    }

    #[test]
    fn post_json_classifier_drift_guard_root_cause_misses_http_error() {
        // Drift-guard pin: `root_cause()` walks past `HttpError` to the
        // leaf, which has no status; this is precisely the bug that
        // motivated the `as_ref()` fix. If a future refactor restores
        // `root_cause()` here, this test fails.
        let wrapped = wrap_status_like_post_json(503);
        assert!(
            !is_retriable(wrapped.root_cause()),
            "root_cause() reaches the leaf — wrong API for chain-walk classification"
        );
    }
}
