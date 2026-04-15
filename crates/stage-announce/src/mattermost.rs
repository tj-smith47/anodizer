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

/// GoReleaser parity (mattermost.go:78-85): always emit an attachment with
/// defaults — title template `{{ .ProjectName }} {{ .Tag }} is out!`, color
/// `#2D313E`, text = message — and leave the top-level `text` as an empty
/// string. Callers should pre-render the title template; this module only
/// applies the `#2D313E` color default when `color` is None.
pub(crate) const MATTERMOST_DEFAULT_COLOR: &str = "#2D313E";

pub(crate) fn mattermost_payload(message: &str, opts: &MattermostOptions<'_>) -> String {
    // Top-level `text` is always emitted as an empty string (GoReleaser zero-
    // value serialises without `omitempty`). The rendered message lives on the
    // attachment.
    let mut payload = json!({ "text": "" });

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

    // GoReleaser always attaches a single attachment block regardless of
    // whether the user supplied color/title. Missing fields get documented
    // defaults; callers render the title template before passing it in.
    let mut attachment = json!({
        "text": message,
        "color": opts.color.unwrap_or(MATTERMOST_DEFAULT_COLOR),
    });
    if let Some(title) = opts.title {
        attachment["title"] = json!(title);
    }
    payload["attachments"] = json!([attachment]);

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
        // GoReleaser always emits an attachment; top-level text stays empty.
        assert_eq!(json["text"], "");
        assert!(json.get("channel").is_none());
        assert!(json.get("username").is_none());
        assert!(json.get("icon_url").is_none());
        assert!(json.get("icon_emoji").is_none());
        let attachments = json["attachments"].as_array().unwrap();
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0]["text"], "myapp v1.0.0 released!");
        assert_eq!(attachments[0]["color"], MATTERMOST_DEFAULT_COLOR);
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
        // GoReleaser includes top-level "text": "" when using attachments.
        assert_eq!(json["text"], "");
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
        // Attachment is always present (GoReleaser parity).
        let attachments = json["attachments"].as_array().unwrap();
        assert_eq!(attachments[0]["text"], "released!");
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
        // GoReleaser includes top-level "text": "" when using attachments.
        assert_eq!(json["text"], "");
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
        // GoReleaser includes top-level "text": "" when using attachments.
        assert_eq!(json["text"], "");
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
        // GoReleaser includes top-level "text": "" when using attachments.
        assert_eq!(json["text"], "");
        let attachments = json["attachments"].as_array().unwrap();
        assert_eq!(attachments[0]["title"], "Release v3.0");
        assert_eq!(attachments[0]["color"], "#36a64f");
        assert_eq!(attachments[0]["text"], "New features!");
    }
}
