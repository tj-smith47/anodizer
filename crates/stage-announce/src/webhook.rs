use std::collections::HashMap;

use anyhow::Result;

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
    let effective_ct = if content_type.is_empty() {
        "application/json; charset=utf-8"
    } else {
        content_type
    };

    let client = reqwest::blocking::Client::builder()
        .user_agent(anodizer_core::http::USER_AGENT)
        .danger_accept_invalid_certs(skip_tls_verify)
        .build()?;

    let mut builder = client
        .post(endpoint_url)
        .header("Content-Type", effective_ct)
        .body(message.to_string());

    for (key, value) in headers {
        builder = builder.header(key.as_str(), value.as_str());
    }

    let resp = builder.send()?;
    let status = resp.status().as_u16();
    if !is_expected_status(status, expected_status_codes) {
        let body = anodizer_core::http::body_of_blocking(resp);
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
}
