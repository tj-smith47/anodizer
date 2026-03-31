use anyhow::Result;
use serde_json::json;

use crate::http::post_json;

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// Optional fields for Mattermost webhook payloads.
pub struct MattermostOptions<'a> {
    pub channel: Option<&'a str>,
    pub username: Option<&'a str>,
    pub icon_url: Option<&'a str>,
    pub icon_emoji: Option<&'a str>,
    pub color: Option<&'a str>,
    pub title: Option<&'a str>,
}

// ---------------------------------------------------------------------------
// Payload builder
// ---------------------------------------------------------------------------

pub(crate) fn mattermost_payload(message: &str, opts: &MattermostOptions<'_>) -> String {
    let use_attachments = opts.color.is_some() || opts.title.is_some();

    let mut payload = if use_attachments {
        // When using attachments, do NOT include top-level `text` — message goes
        // in the attachment only.  This matches GoReleaser behaviour.
        json!({})
    } else {
        json!({ "text": message })
    };

    if let Some(ch) = opts.channel {
        payload["channel"] = json!(ch);
    }
    if let Some(user) = opts.username {
        payload["username"] = json!(user);
    }
    if let Some(icon) = opts.icon_url {
        payload["icon_url"] = json!(icon);
    }
    if let Some(emoji) = opts.icon_emoji {
        payload["icon_emoji"] = json!(emoji);
    }

    // Mattermost supports message attachments with optional color bar and title.
    if use_attachments {
        let mut attachment = json!({
            "text": message,
        });
        if let Some(title) = opts.title {
            attachment["title"] = json!(title);
        }
        if let Some(color) = opts.color {
            attachment["color"] = json!(color);
        }
        payload["attachments"] = json!([attachment]);
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
    opts: &MattermostOptions<'_>,
) -> Result<()> {
    let payload = mattermost_payload(message, opts);
    post_json(webhook_url, &payload, "mattermost")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mattermost_payload_minimal() {
        let opts = MattermostOptions {
            channel: None,
            username: None,
            icon_url: None,
            icon_emoji: None,
            color: None,
            title: None,
        };
        let payload = mattermost_payload("myapp v1.0.0 released!", &opts);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(json["text"], "myapp v1.0.0 released!");
        assert!(json.get("channel").is_none());
        assert!(json.get("username").is_none());
        assert!(json.get("icon_url").is_none());
        assert!(json.get("icon_emoji").is_none());
        assert!(json.get("attachments").is_none());
    }

    #[test]
    fn test_mattermost_payload_with_all_options() {
        let opts = MattermostOptions {
            channel: Some("town-square"),
            username: Some("release-bot"),
            icon_url: Some("https://example.com/icon.png"),
            icon_emoji: Some(":rocket:"),
            color: Some("#36a64f"),
            title: Some("Release v1.0"),
        };
        let payload = mattermost_payload("myapp v1.0.0 released!", &opts);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        // When using attachments, top-level text must NOT be present
        assert!(json.get("text").is_none());
        assert_eq!(json["channel"], "town-square");
        assert_eq!(json["username"], "release-bot");
        assert_eq!(json["icon_url"], "https://example.com/icon.png");
        assert_eq!(json["icon_emoji"], ":rocket:");
        let attachments = json["attachments"].as_array().unwrap();
        assert_eq!(attachments[0]["color"], "#36a64f");
        assert_eq!(attachments[0]["title"], "Release v1.0");
        assert_eq!(attachments[0]["text"], "myapp v1.0.0 released!");
    }

    #[test]
    fn test_mattermost_payload_partial_options() {
        let opts = MattermostOptions {
            channel: Some("releases"),
            username: None,
            icon_url: None,
            icon_emoji: None,
            color: None,
            title: None,
        };
        let payload = mattermost_payload("released!", &opts);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(json["channel"], "releases");
        assert!(json.get("username").is_none());
    }

    #[test]
    fn test_mattermost_payload_with_icon_emoji() {
        let opts = MattermostOptions {
            channel: None,
            username: None,
            icon_url: None,
            icon_emoji: Some(":tada:"),
            color: None,
            title: None,
        };
        let payload = mattermost_payload("shipped!", &opts);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(json["icon_emoji"], ":tada:");
    }

    #[test]
    fn test_mattermost_payload_with_color() {
        let opts = MattermostOptions {
            channel: None,
            username: None,
            icon_url: None,
            icon_emoji: None,
            color: Some("#FF0000"),
            title: None,
        };
        let payload = mattermost_payload("alert!", &opts);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        // When using attachments, top-level text must NOT be present
        assert!(json.get("text").is_none());
        let attachments = json["attachments"].as_array().unwrap();
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0]["color"], "#FF0000");
        assert_eq!(attachments[0]["text"], "alert!");
    }

    #[test]
    fn test_mattermost_payload_with_title() {
        let opts = MattermostOptions {
            channel: None,
            username: None,
            icon_url: None,
            icon_emoji: None,
            color: None,
            title: Some("myapp v2.0 is out!"),
        };
        let payload = mattermost_payload("Check the release notes.", &opts);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        // When using attachments, top-level text must NOT be present
        assert!(json.get("text").is_none());
        let attachments = json["attachments"].as_array().unwrap();
        assert_eq!(attachments[0]["title"], "myapp v2.0 is out!");
        assert_eq!(attachments[0]["text"], "Check the release notes.");
    }

    #[test]
    fn test_mattermost_payload_with_title_and_color() {
        let opts = MattermostOptions {
            channel: None,
            username: None,
            icon_url: None,
            icon_emoji: None,
            color: Some("#36a64f"),
            title: Some("Release v3.0"),
        };
        let payload = mattermost_payload("New features!", &opts);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        // When using attachments, top-level text must NOT be present
        assert!(json.get("text").is_none());
        let attachments = json["attachments"].as_array().unwrap();
        assert_eq!(attachments[0]["title"], "Release v3.0");
        assert_eq!(attachments[0]["color"], "#36a64f");
        assert_eq!(attachments[0]["text"], "New features!");
    }
}
