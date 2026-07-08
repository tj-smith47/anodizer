use anodizer_core::retry::RetryPolicy;
use anyhow::Result;
use serde_json::json;

use crate::http::post_json;

// ---------------------------------------------------------------------------
// Slack options
// ---------------------------------------------------------------------------

/// Optional overrides for a Slack incoming-webhook payload.
#[derive(Default)]
pub struct SlackOptions<'a> {
    pub channel: Option<&'a str>,
    pub username: Option<&'a str>,
    pub icon_emoji: Option<&'a str>,
    pub icon_url: Option<&'a str>,
    pub blocks: Option<&'a serde_json::Value>,
    pub attachments: Option<&'a serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Payload builder
// ---------------------------------------------------------------------------

pub(crate) fn slack_payload(message: &str, opts: &SlackOptions<'_>) -> String {
    let mut obj = serde_json::Map::new();
    obj.insert("text".into(), json!(message));
    if let Some(ch) = opts.channel {
        obj.insert("channel".into(), json!(ch));
    }
    if let Some(u) = opts.username {
        obj.insert("username".into(), json!(u));
    }
    if let Some(e) = opts.icon_emoji {
        obj.insert("icon_emoji".into(), json!(e));
    }
    if let Some(u) = opts.icon_url {
        obj.insert("icon_url".into(), json!(u));
    }
    if let Some(b) = opts.blocks {
        obj.insert("blocks".into(), b.clone());
    }
    if let Some(a) = opts.attachments {
        obj.insert("attachments".into(), a.clone());
    }
    serde_json::Value::Object(obj).to_string()
}

// ---------------------------------------------------------------------------
// Send
// ---------------------------------------------------------------------------

/// POST a Slack incoming-webhook payload with optional overrides.
///
/// `policy` controls retry behaviour for transport-level / 5xx / 429 failures.
pub fn send_slack(
    webhook_url: &str,
    message: &str,
    opts: &SlackOptions<'_>,
    policy: &RetryPolicy,
    log: &anodizer_core::log::StageLogger,
) -> Result<()> {
    let payload = slack_payload(message, opts);
    post_json(webhook_url, &payload, "slack", policy, log)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slack_payload_text_only() {
        let payload = slack_payload("myapp v1.0.0 released!", &SlackOptions::default());
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(json["text"], "myapp v1.0.0 released!");
        assert!(json.get("channel").is_none());
        assert!(json.get("username").is_none());
        assert!(json.get("icon_emoji").is_none());
        assert!(json.get("icon_url").is_none());
        assert!(json.get("blocks").is_none());
        assert!(json.get("attachments").is_none());
    }

    #[test]
    fn test_slack_payload_all_fields() {
        let blocks = serde_json::json!([
            {
                "type": "section",
                "text": { "type": "mrkdwn", "text": "New release!" }
            }
        ]);
        let attachments = serde_json::json!([
            {
                "color": "#36a64f",
                "text": "Details here"
            }
        ]);
        let opts = SlackOptions {
            channel: Some("#releases"),
            username: Some("release-bot"),
            icon_emoji: Some(":rocket:"),
            icon_url: Some("https://example.com/icon.png"),
            blocks: Some(&blocks),
            attachments: Some(&attachments),
        };
        let payload = slack_payload("myapp v1.0.0 released!", &opts);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(json["text"], "myapp v1.0.0 released!");
        assert_eq!(json["channel"], "#releases");
        assert_eq!(json["username"], "release-bot");
        assert_eq!(json["icon_emoji"], ":rocket:");
        assert_eq!(json["icon_url"], "https://example.com/icon.png");
        assert_eq!(json["blocks"][0]["type"], "section");
        assert_eq!(json["attachments"][0]["color"], "#36a64f");
    }

    #[test]
    fn test_slack_payload_channel_only() {
        let opts = SlackOptions {
            channel: Some("#general"),
            ..SlackOptions::default()
        };
        let payload = slack_payload("myapp v1.0.0 released!", &opts);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(json["text"], "myapp v1.0.0 released!");
        assert_eq!(json["channel"], "#general");
        assert!(json.get("username").is_none());
        assert!(json.get("icon_emoji").is_none());
        assert!(json.get("icon_url").is_none());
        assert!(json.get("blocks").is_none());
        assert!(json.get("attachments").is_none());
    }
}
