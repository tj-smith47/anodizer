use anodizer_core::retry::RetryPolicy;
use anyhow::Result;

use crate::helpers::retry_http;

/// POST a JSON payload to `url`, returning an error that includes the
/// provider name, HTTP status, and response body on failure.
///
/// The URL is intentionally NOT included in error messages because it may
/// contain secrets (e.g. Telegram bot tokens embedded in the path).
///
/// `policy` controls retry behaviour: 5xx / 429 / transport-level failures
/// retry up to `policy.max_attempts` with exponential backoff; 4xx fast-fails.
/// Pass `RetryConfig::default().to_policy()` (or
/// `ctx.config.retry.unwrap_or_default().to_policy()`) for GoReleaser-aligned
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
    let client = reqwest::blocking::Client::new();
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
    use anodizer_core::retry::{HttpError, is_retriable};

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
