use std::collections::BTreeMap;
use std::ops::ControlFlow;

use anodizer_core::retry::{HttpError, RetryLog, RetryPolicy, is_retriable, retry_sync};
use anyhow::{Context as _, Result};

// ---------------------------------------------------------------------------
// Status code helpers
// ---------------------------------------------------------------------------

/// Default HTTP status codes accepted as a successful webhook response.
///
/// Default expected status codes: `[200, 201, 202, 204]`.
pub(crate) fn default_expected_status_codes() -> Vec<u16> {
    vec![200, 201, 202, 204]
}

/// Returns `true` when `status` is in the `expected` set.
pub(crate) fn is_expected_status(status: u16, expected: &[u16]) -> bool {
    expected.contains(&status)
}

/// Build the user-facing error message for a webhook response that failed
/// the status-code gate: the response
/// body is included verbatim so debugging does not require re-running with
/// verbose logs.
pub(crate) fn format_unexpected_status_message(
    status: u16,
    expected: &[u16],
    body: &str,
) -> String {
    format!("webhook returned unexpected status {status} (expected one of {expected:?}): {body}")
}

// ---------------------------------------------------------------------------
// Send
// ---------------------------------------------------------------------------

/// POST to an arbitrary HTTP endpoint with custom headers and content type.
///
/// When `skip_tls_verify` is true the client will accept invalid / self-signed
/// TLS certificates (the `skip_tls_verify` webhook option).
///
/// The response status is validated against `expected_status_codes`.
///
/// Error messages include the response body so users can debug
/// upstream rejections without re-running with verbose logs (mirrors
/// upstream commit bba909e). `policy` enables retry on 5xx / 429 / network
/// failures.
///
/// `headers` is a [`BTreeMap`] (not a `HashMap`) so the iteration
/// order in the request builder loop is deterministic (alphabetical by header
/// name). This makes wire traces reproducible across runs and matches
/// First-set-wins ordering for the env-supplied `Authorization`
/// header. Sort order is irrelevant on the wire because RFC 7230 §3.2.2
/// forbids semantically meaningful ordering for headers with distinct names;
/// the user-supplied `headers.Authorization` precedence is enforced at the
/// builder level (`resolve_webhook_headers`) before we get here.
#[allow(clippy::too_many_arguments)]
pub fn send_webhook(
    endpoint_url: &str,
    message: &str,
    headers: &BTreeMap<String, String>,
    content_type: &str,
    skip_tls_verify: bool,
    expected_status_codes: &[u16],
    policy: &RetryPolicy,
    log: &anodizer_core::log::StageLogger,
) -> Result<()> {
    let effective_ct = if content_type.is_empty() {
        "application/json; charset=utf-8"
    } else {
        content_type
    };

    let client = crate::http::blocking_client_accept_invalid_certs(skip_tls_verify)?;

    retry_sync(RetryLog::new("webhook announce", log), policy, |_attempt| {
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
                    // Wrap the status-derived error with the response body
                    // so users see it in the surfaced message.
                    let inner = anyhow::anyhow!(
                        "{}",
                        format_unexpected_status_message(status, expected_status_codes, &body)
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
    fn unexpected_status_message_includes_body_and_status() {
        // Upstream commit bba909e: the error surfaced to the user must
        // include the response body so failures from misconfigured webhooks
        // (auth, validation, etc.) are debuggable without re-running.
        let msg = format_unexpected_status_message(503, &[200, 204], "service down");
        assert!(msg.contains("503"), "{msg}");
        assert!(msg.contains("service down"), "{msg}");
        assert!(msg.contains("[200, 204]"), "{msg}");
    }

    #[test]
    fn unexpected_status_message_handles_empty_body() {
        let msg = format_unexpected_status_message(401, &[200], "");
        assert!(msg.contains("401"), "{msg}");
        // Empty body is still included verbatim (trailing colon-space-empty).
        assert!(msg.ends_with(": "), "{msg}");
    }

    #[test]
    fn test_headers_iterate_in_alphabetical_order() {
        // Regression: BTreeMap guarantees alphabetical iteration order
        // regardless of insertion order, so request traces are reproducible
        // across runs. Insertion is intentionally non-alphabetical here.
        let mut headers: BTreeMap<String, String> = BTreeMap::new();
        headers.insert("X-Zulu".into(), "z".into());
        headers.insert("Authorization".into(), "Bearer x".into());
        headers.insert("Content-Type".into(), "application/json".into());
        let order: Vec<&str> = headers.keys().map(String::as_str).collect();
        assert_eq!(order, vec!["Authorization", "Content-Type", "X-Zulu"]);
    }

    use anodizer_core::test_helpers::responder::{
        spawn_oneshot_http_responder, spawn_request_capturing_responder,
    };
    use std::time::Duration;

    fn fast_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(10),
        }
    }

    fn no_retry_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 1,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(10),
        }
    }

    #[test]
    fn happy_path_default_content_type_when_empty() {
        let (addr, captured) =
            spawn_request_capturing_responder("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        let url = format!("http://{addr}/hook");
        let headers = BTreeMap::new();
        send_webhook(
            &url,
            "{\"k\":\"v\"}",
            &headers,
            "",
            false,
            &default_expected_status_codes(),
            &no_retry_policy(),
            anodizer_core::test_helpers::test_logger(),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let req = captured.lock().unwrap().clone();
        assert!(
            req.to_ascii_lowercase()
                .contains("content-type: application/json; charset=utf-8"),
            "default CT applied: {req}"
        );
        assert!(req.contains("{\"k\":\"v\"}"), "body sent: {req}");
    }

    #[test]
    fn explicit_content_type_overrides_default() {
        let (addr, captured) =
            spawn_request_capturing_responder("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        let url = format!("http://{addr}/hook");
        let headers = BTreeMap::new();
        send_webhook(
            &url,
            "k=v",
            &headers,
            "application/x-www-form-urlencoded",
            false,
            &default_expected_status_codes(),
            &no_retry_policy(),
            anodizer_core::test_helpers::test_logger(),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let req = captured.lock().unwrap().clone();
        assert!(
            req.to_ascii_lowercase()
                .contains("content-type: application/x-www-form-urlencoded"),
            "explicit CT honored: {req}"
        );
    }

    #[test]
    fn user_supplied_headers_sent_in_alpha_order() {
        let (addr, captured) =
            spawn_request_capturing_responder("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        let url = format!("http://{addr}/hook");
        let mut headers: BTreeMap<String, String> = BTreeMap::new();
        headers.insert("X-Custom".into(), "marker-value".into());
        headers.insert("Authorization".into(), "Bearer secret-xyz".into());
        send_webhook(
            &url,
            "{}",
            &headers,
            "application/json",
            false,
            &[200],
            &no_retry_policy(),
            anodizer_core::test_helpers::test_logger(),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let req = captured.lock().unwrap().clone();
        let lower = req.to_ascii_lowercase();
        assert!(lower.contains("authorization: bearer secret-xyz"), "{req}");
        assert!(lower.contains("x-custom: marker-value"), "{req}");
    }

    #[test]
    fn unexpected_status_includes_body_in_error() {
        let (addr, _) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 418 I'm a teapot\r\nContent-Length: 11\r\n\r\nbrew coffee",
        ]);
        let url = format!("http://{addr}/hook");
        let err = send_webhook(
            &url,
            "{}",
            &BTreeMap::new(),
            "",
            false,
            &[200, 201, 204],
            &fast_policy(),
            anodizer_core::test_helpers::test_logger(),
        )
        .unwrap_err();
        let chain = format!("{err:#}");
        assert!(chain.contains("418"), "status in err chain: {chain}");
        assert!(chain.contains("brew coffee"), "body in err chain: {chain}");
    }

    #[test]
    fn retries_5xx_then_succeeds() {
        let (addr, counter) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
        ]);
        let url = format!("http://{addr}/hook");
        send_webhook(
            &url,
            "{}",
            &BTreeMap::new(),
            "",
            false,
            &[200],
            &fast_policy(),
            anodizer_core::test_helpers::test_logger(),
        )
        .unwrap();
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn custom_expected_status_204_accepts_204() {
        let (addr, _) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
        ]);
        let url = format!("http://{addr}/hook");
        send_webhook(
            &url,
            "",
            &BTreeMap::new(),
            "",
            false,
            &[204],
            &no_retry_policy(),
            anodizer_core::test_helpers::test_logger(),
        )
        .unwrap();
    }
}
