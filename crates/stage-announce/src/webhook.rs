use std::collections::HashMap;

use anyhow::Result;
// ---------------------------------------------------------------------------
// Body builder
// ---------------------------------------------------------------------------

/// Build the request body for a generic HTTP webhook.
///
/// When content_type is `application/json` and the message is not already valid
/// JSON, wraps the message in a `{"text": ...}` JSON object.  For all other
/// content types the raw message is returned as-is.
pub(crate) fn webhook_body(message: &str, content_type: &str) -> String {
    if content_type == "application/json" {
        // If the message is already valid JSON, send it verbatim.
        if serde_json::from_str::<serde_json::Value>(message).is_ok() {
            return message.to_string();
        }
        // Otherwise wrap in a simple JSON envelope.
        serde_json::json!({ "text": message }).to_string()
    } else {
        message.to_string()
    }
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
    fn test_webhook_body_json_passthrough() {
        // Valid JSON is passed through verbatim when content_type is application/json
        let body = webhook_body(r#"{"project":"myapp","tag":"v1.0.0"}"#, "application/json");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["project"], "myapp");
        assert_eq!(json["tag"], "v1.0.0");
    }

    #[test]
    fn test_webhook_body_json_wraps_plain_text() {
        // Plain text is wrapped in {"text": ...} when content_type is application/json
        let body = webhook_body("Release v1.0.0 is out!", "application/json");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["text"], "Release v1.0.0 is out!");
    }

    #[test]
    fn test_webhook_body_text_plain_raw() {
        // text/plain returns the message as-is
        let body = webhook_body("hello world", "text/plain");
        assert_eq!(body, "hello world");
    }
}
