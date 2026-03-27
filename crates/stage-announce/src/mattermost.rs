use anyhow::Result;
use serde_json::json;

// ---------------------------------------------------------------------------
// Payload builder
// ---------------------------------------------------------------------------

pub(crate) fn mattermost_payload(
    message: &str,
    channel: Option<&str>,
    username: Option<&str>,
    icon_url: Option<&str>,
) -> String {
    let mut payload = json!({ "text": message });
    if let Some(ch) = channel {
        payload["channel"] = json!(ch);
    }
    if let Some(user) = username {
        payload["username"] = json!(user);
    }
    if let Some(icon) = icon_url {
        payload["icon_url"] = json!(icon);
    }
    payload.to_string()
}

// ---------------------------------------------------------------------------
// Send
// ---------------------------------------------------------------------------

/// POST to a Mattermost incoming webhook.
pub fn send_mattermost(
    webhook_url: &str,
    message: &str,
    channel: Option<&str>,
    username: Option<&str>,
    icon_url: Option<&str>,
) -> Result<()> {
    let payload = mattermost_payload(message, channel, username, icon_url);
    let client = reqwest::blocking::Client::new();
    let resp = client
        .post(webhook_url)
        .header("Content-Type", "application/json")
        .body(payload)
        .send()?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        anyhow::bail!("mattermost webhook returned non-success status {status}: {body}");
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
    fn test_mattermost_payload_minimal() {
        let payload = mattermost_payload("myapp v1.0.0 released!", None, None, None);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(json["text"], "myapp v1.0.0 released!");
        assert!(json.get("channel").is_none());
        assert!(json.get("username").is_none());
        assert!(json.get("icon_url").is_none());
    }

    #[test]
    fn test_mattermost_payload_with_all_options() {
        let payload = mattermost_payload(
            "myapp v1.0.0 released!",
            Some("town-square"),
            Some("release-bot"),
            Some("https://example.com/icon.png"),
        );
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(json["text"], "myapp v1.0.0 released!");
        assert_eq!(json["channel"], "town-square");
        assert_eq!(json["username"], "release-bot");
        assert_eq!(json["icon_url"], "https://example.com/icon.png");
    }

    #[test]
    fn test_mattermost_payload_partial_options() {
        let payload =
            mattermost_payload("released!", Some("releases"), None, None);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(json["channel"], "releases");
        assert!(json.get("username").is_none());
    }
}
