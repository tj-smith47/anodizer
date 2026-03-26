use std::collections::HashMap;

use anyhow::Result;
// ---------------------------------------------------------------------------
// Body builder
// ---------------------------------------------------------------------------

/// Build the request body for a generic HTTP webhook.
///
/// The user's rendered `message_template` is sent as the raw body.
/// For generic webhooks the user controls the full payload shape.
pub(crate) fn webhook_body(message: &str, _content_type: &str) -> String {
    message.to_string()
}

// ---------------------------------------------------------------------------
// Send
// ---------------------------------------------------------------------------

/// POST to an arbitrary HTTP endpoint with custom headers and content type.
pub fn send_webhook(
    endpoint_url: &str,
    message: &str,
    headers: &HashMap<String, String>,
    content_type: &str,
) -> Result<()> {
    let body = webhook_body(message, content_type);
    let effective_ct = if content_type.is_empty() {
        "application/json"
    } else {
        content_type
    };

    let client = reqwest::blocking::Client::new();
    let mut builder = client
        .post(endpoint_url)
        .header("Content-Type", effective_ct)
        .body(body);

    for (key, value) in headers {
        builder = builder.header(key.as_str(), value.as_str());
    }

    let resp = builder.send()?;
    if !resp.status().is_success() {
        anyhow::bail!("webhook returned non-success status: {}", resp.status());
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
    fn test_webhook_body_is_raw_message() {
        // Generic webhook sends user's template as raw body
        let body = webhook_body(r#"{"project":"myapp","tag":"v1.0.0"}"#, "application/json");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["project"], "myapp");
        assert_eq!(json["tag"], "v1.0.0");
    }
}
