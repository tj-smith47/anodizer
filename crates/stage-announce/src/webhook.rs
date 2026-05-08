use std::collections::BTreeMap;
use std::ops::ControlFlow;

use anodizer_core::retry::{HttpError, RetryPolicy, is_retriable, retry_sync};
use anyhow::{Context as _, Result};

// ---------------------------------------------------------------------------
// Status code helpers
// ---------------------------------------------------------------------------

/// Default HTTP status codes accepted as a successful webhook response.
///
/// Matches GoReleaser's `ExpectedStatusCodes` default: `[200, 201, 202, 204]`.
pub(crate) fn default_expected_status_codes() -> Vec<u16> {
    vec![200, 201, 202, 204]
}

/// Returns `true` when `status` is in the `expected` set.
pub(crate) fn is_expected_status(status: u16, expected: &[u16]) -> bool {
    expected.contains(&status)
}

// ---------------------------------------------------------------------------
// Send
// ---------------------------------------------------------------------------

/// POST to an arbitrary HTTP endpoint with custom headers and content type.
///
/// When `skip_tls_verify` is true the client will accept invalid / self-signed
/// TLS certificates (mirrors GoReleaser's `skip_tls_verify` webhook option).
///
/// The response status is validated against `expected_status_codes`.
///
/// Q7.1 — error messages include the response body so users can debug
/// upstream rejections without re-running with verbose logs (mirrors
/// upstream commit bba909e). `policy` enables retry on 5xx / 429 / network
/// failures (P1.3).
///
/// Q-wh1 — `headers` is a [`BTreeMap`] (not a `HashMap`) so the iteration
/// order in the request builder loop is deterministic (alphabetical by header
/// name). This makes wire traces reproducible across runs and matches
/// GoReleaser's first-set-wins ordering for the env-supplied `Authorization`
/// header. Sort order is irrelevant on the wire because RFC 7230 §3.2.2
/// forbids semantically meaningful ordering for headers with distinct names;
/// the user-supplied `headers.Authorization` precedence is enforced at the
/// builder level (`resolve_webhook_headers`) before we get here.
pub fn send_webhook(
    endpoint_url: &str,
    message: &str,
    headers: &BTreeMap<String, String>,
    content_type: &str,
    skip_tls_verify: bool,
    expected_status_codes: &[u16],
    policy: &RetryPolicy,
) -> Result<()> {
    let effective_ct = if content_type.is_empty() {
        "application/json; charset=utf-8"
    } else {
        content_type
    };

    let client = reqwest::blocking::Client::builder()
        .user_agent(anodizer_core::http::USER_AGENT)
        .danger_accept_invalid_certs(skip_tls_verify)
        .build()
        .context("webhook: build HTTP client")?;

    retry_sync(policy, |_attempt| {
        let mut builder = client
            .post(endpoint_url)
            .header("Content-Type", effective_ct)
            .body(message.to_string());
        for (key, value) in headers {
            builder = builder.header(key.as_str(), value.as_str());
        }

        match builder.send() {
            Err(e) => {
                let err = anyhow::Error::new(HttpError::from_response(e, None))
                    .context("webhook: failed to send POST request");
                if is_retriable(err.as_ref()) {
                    Err(ControlFlow::Continue(err))
                } else {
                    Err(ControlFlow::Break(err))
                }
            }
            Ok(resp) => {
                let status = resp.status().as_u16();
                if is_expected_status(status, expected_status_codes) {
                    Ok(())
                } else {
                    let body = anodizer_core::http::body_of_blocking(resp);
                    // Q7.1 mirror — wrap the status-derived error with the
                    // response body so users see it in the surfaced message.
                    let inner = anyhow::anyhow!(
                        "webhook returned unexpected status {status} (expected one of \
                         {expected_status_codes:?}): {body}"
                    );
                    let wrapped = anyhow::Error::new(HttpError::new(
                        std::io::Error::other(inner.to_string()),
                        status,
                    ))
                    .context(inner);
                    if is_retriable(wrapped.as_ref()) {
                        Err(ControlFlow::Continue(wrapped))
                    } else {
                        Err(ControlFlow::Break(wrapped))
                    }
                }
            }
        }
    })
    .context("webhook: POST exhausted retry attempts")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_expected_status() {
        let expected = vec![200, 201, 204];
        assert!(is_expected_status(200, &expected));
        assert!(is_expected_status(201, &expected));
        assert!(is_expected_status(204, &expected));
        assert!(!is_expected_status(500, &expected));
        assert!(!is_expected_status(403, &expected));
    }

    #[test]
    fn test_default_expected_status_codes() {
        let defaults = default_expected_status_codes();
        assert_eq!(defaults, vec![200, 201, 202, 204]);
    }

    #[test]
    fn test_headers_iterate_in_alphabetical_order() {
        // Q-wh1 regression: BTreeMap guarantees alphabetical iteration order
        // regardless of insertion order, so request traces are reproducible
        // across runs. Insertion is intentionally non-alphabetical here.
        let mut headers: BTreeMap<String, String> = BTreeMap::new();
        headers.insert("X-Zulu".into(), "z".into());
        headers.insert("Authorization".into(), "Bearer x".into());
        headers.insert("Content-Type".into(), "application/json".into());
        let order: Vec<&str> = headers.keys().map(String::as_str).collect();
        assert_eq!(order, vec!["Authorization", "Content-Type", "X-Zulu"]);
    }
}
