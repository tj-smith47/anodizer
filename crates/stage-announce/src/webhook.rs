use std::collections::HashMap;

use anyhow::Result;
// ---------------------------------------------------------------------------
// Body builder
// ---------------------------------------------------------------------------

/// Build the request body for a generic HTTP webhook.
///
/// GoReleaser sends the raw template output as the request body regardless of
/// content type. The default template is already valid JSON (`{ "message": "..." }`).
/// We match this behavior: always send the rendered message verbatim.
pub(crate) fn webhook_body(message: &str, _content_type: &str) -> String {
    message.to_string()
}

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
pub fn send_webhook(
    endpoint_url: &str,
    message: &str,
    headers: &HashMap<String, String>,
    content_type: &str,
    skip_tls_verify: bool,
    expected_status_codes: &[u16],
) -> Result<()> {
    let body = webhook_body(message, content_type);
    let effective_ct = if content_type.is_empty() {
        "application/json; charset=utf-8"
    } else {
        content_type
    };

    let client = reqwest::blocking::Client::builder()
        .user_agent(anodize_core::http::USER_AGENT)
        .danger_accept_invalid_certs(skip_tls_verify)
        .build()?;

    let mut builder = client
        .post(endpoint_url)
        .header("Content-Type", effective_ct)
        .body(body);

    for (key, value) in headers {
        builder = builder.header(key.as_str(), value.as_str());
    }

    let resp = builder.send()?;
    let status = resp.status().as_u16();
    if !is_expected_status(status, expected_status_codes) {
        let body = resp.text().unwrap_or_default();
        anyhow::bail!(
            "webhook returned unexpected status {status} (expected one of {expected_status_codes:?}): {body}"
        );
    }
    Ok(())
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
    fn test_webhook_body_json_passthrough() {
        // Valid JSON is passed through verbatim when content_type is application/json
        let body = webhook_body(r#"{"project":"myapp","tag":"v1.0.0"}"#, "application/json");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["project"], "myapp");
        assert_eq!(json["tag"], "v1.0.0");
    }

    #[test]
    fn test_webhook_body_json_raw_passthrough() {
        // GoReleaser sends the raw template output as-is, no wrapping.
        // The default template is already valid JSON.
        let body = webhook_body("Release v1.0.0 is out!", "application/json");
        assert_eq!(body, "Release v1.0.0 is out!");
    }

    #[test]
    fn test_webhook_body_text_plain_raw() {
        // text/plain returns the message as-is
        let body = webhook_body("hello world", "text/plain");
        assert_eq!(body, "hello world");
    }

    #[test]
    fn test_webhook_body_json_with_charset() {
        // "application/json; charset=utf-8" (the default) should still trigger JSON handling.
        let body = webhook_body(r#"{"tag":"v1.0"}"#, "application/json; charset=utf-8");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["tag"], "v1.0");

        // Plain text is also passed through raw (GoReleaser behavior).
        let body2 = webhook_body("plain text", "application/json; charset=utf-8");
        assert_eq!(body2, "plain text");
    }
}
