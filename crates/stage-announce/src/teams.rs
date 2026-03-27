use anyhow::Result;
use serde_json::json;

// ---------------------------------------------------------------------------
// Payload builder
// ---------------------------------------------------------------------------

/// Build a Microsoft Teams Adaptive Card payload.
pub(crate) fn teams_payload(message: &str) -> String {
    json!({
        "type": "message",
        "attachments": [{
            "contentType": "application/vnd.microsoft.card.adaptive",
            "contentUrl": null,
            "content": {
                "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
                "type": "AdaptiveCard",
                "version": "1.4",
                "body": [{
                    "type": "TextBlock",
                    "text": message,
                    "wrap": true,
                }],
            },
        }],
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// Send
// ---------------------------------------------------------------------------

/// POST to a Microsoft Teams incoming webhook using an Adaptive Card.
pub fn send_teams(webhook_url: &str, message: &str) -> Result<()> {
    let payload = teams_payload(message);
    let client = reqwest::blocking::Client::new();
    let resp = client
        .post(webhook_url)
        .header("Content-Type", "application/json")
        .body(payload)
        .send()?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        anyhow::bail!("teams webhook returned non-success status {status}: {body}");
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
    fn test_teams_payload_structure() {
        let payload = teams_payload("myapp v1.0.0 released!");
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(json["type"], "message");
        let attachments = json["attachments"].as_array().unwrap();
        assert_eq!(attachments.len(), 1);
        assert_eq!(
            attachments[0]["contentType"],
            "application/vnd.microsoft.card.adaptive"
        );
        let content = &attachments[0]["content"];
        assert_eq!(content["type"], "AdaptiveCard");
        assert_eq!(content["version"], "1.4");
        let body = content["body"].as_array().unwrap();
        assert_eq!(body[0]["type"], "TextBlock");
        assert_eq!(body[0]["text"], "myapp v1.0.0 released!");
        assert_eq!(body[0]["wrap"], true);
    }
}
