use anyhow::Result;
use serde_json::json;

use crate::http::post_json;

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// Optional fields for Microsoft Teams Adaptive Card payloads.
pub struct TeamsOptions<'a> {
    pub title: Option<&'a str>,
    pub color: Option<&'a str>,
    pub icon_url: Option<&'a str>,
}

// ---------------------------------------------------------------------------
// Payload builder
// ---------------------------------------------------------------------------

/// Build a Microsoft Teams Adaptive Card payload with optional title, color, and icon.
pub(crate) fn teams_payload(message: &str, opts: &TeamsOptions<'_>) -> String {
    let mut body_items: Vec<serde_json::Value> = Vec::new();

    // Add title/icon header block(s) based on what's provided.
    match (opts.title, opts.icon_url) {
        (Some(title), Some(icon)) => {
            body_items.push(json!({
                "type": "ColumnSet",
                "columns": [
                    {
                        "type": "Column",
                        "width": "auto",
                        "items": [{
                            "type": "Image",
                            "url": icon,
                            "size": "Small",
                            "style": "Person",
                        }]
                    },
                    {
                        "type": "Column",
                        "width": "stretch",
                        "items": [{
                            "type": "TextBlock",
                            "text": title,
                            "weight": "Bolder",
                            "size": "Medium",
                            "wrap": true,
                        }]
                    }
                ]
            }));
        }
        (Some(title), None) => {
            body_items.push(json!({
                "type": "TextBlock",
                "text": title,
                "weight": "Bolder",
                "size": "Medium",
                "wrap": true,
            }));
        }
        (None, Some(icon)) => {
            body_items.push(json!({
                "type": "Image",
                "url": icon,
                "size": "Small",
            }));
        }
        (None, None) => {}
    }

    body_items.push(json!({
        "type": "TextBlock",
        "text": message,
        "wrap": true,
    }));

    let card = json!({
        "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
        "type": "AdaptiveCard",
        "version": "1.4",
        "body": body_items,
    });

    // msteams extensions support a themeColor on the outer message.
    let mut outer = json!({
        "type": "message",
        "attachments": [{
            "contentType": "application/vnd.microsoft.card.adaptive",
            "contentUrl": null,
            "content": card,
        }],
    });

    if let Some(color) = opts.color {
        outer["themeColor"] = json!(color);
    }

    outer.to_string()
}

// ---------------------------------------------------------------------------
// Send
// ---------------------------------------------------------------------------

/// POST to a Microsoft Teams incoming webhook using an Adaptive Card.
pub fn send_teams(webhook_url: &str, message: &str, opts: &TeamsOptions<'_>) -> Result<()> {
    let payload = teams_payload(message, opts);
    post_json(webhook_url, &payload, "teams")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_teams_payload_structure() {
        let opts = TeamsOptions {
            title: None,
            color: None,
            icon_url: None,
        };
        let payload = teams_payload("myapp v1.0.0 released!", &opts);
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
        assert_eq!(body.len(), 1);
        assert_eq!(body[0]["type"], "TextBlock");
        assert_eq!(body[0]["text"], "myapp v1.0.0 released!");
        assert_eq!(body[0]["wrap"], true);
    }

    #[test]
    fn test_teams_payload_with_title() {
        let opts = TeamsOptions {
            title: Some("Release Announcement"),
            color: None,
            icon_url: None,
        };
        let payload = teams_payload("v2.0 is out!", &opts);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let body = json["attachments"][0]["content"]["body"]
            .as_array()
            .unwrap();
        assert_eq!(body.len(), 2);
        assert_eq!(body[0]["text"], "Release Announcement");
        assert_eq!(body[0]["weight"], "Bolder");
        assert_eq!(body[1]["text"], "v2.0 is out!");
    }

    #[test]
    fn test_teams_payload_with_color() {
        let opts = TeamsOptions {
            title: None,
            color: Some("0076D7"),
            icon_url: None,
        };
        let payload = teams_payload("released!", &opts);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(json["themeColor"], "0076D7");
    }

    #[test]
    fn test_teams_payload_with_title_and_color() {
        let opts = TeamsOptions {
            title: Some("New Release"),
            color: Some("FF0000"),
            icon_url: None,
        };
        let payload = teams_payload("v3.0", &opts);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(json["themeColor"], "FF0000");
        let body = json["attachments"][0]["content"]["body"]
            .as_array()
            .unwrap();
        assert_eq!(body[0]["text"], "New Release");
        assert_eq!(body[1]["text"], "v3.0");
    }

    #[test]
    fn test_teams_payload_with_icon_url() {
        let opts = TeamsOptions {
            title: Some("Release"),
            color: None,
            icon_url: Some("https://example.com/icon.png"),
        };
        let payload = teams_payload("v1.0", &opts);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let body = json["attachments"][0]["content"]["body"]
            .as_array()
            .unwrap();
        let first = &body[0];
        assert_eq!(first["type"], "ColumnSet");
        let columns = first["columns"].as_array().unwrap();
        assert_eq!(columns[0]["items"][0]["type"], "Image");
        assert_eq!(
            columns[0]["items"][0]["url"],
            "https://example.com/icon.png"
        );
        assert_eq!(columns[0]["items"][0]["style"], "Person");
        assert_eq!(columns[1]["items"][0]["type"], "TextBlock");
        assert_eq!(columns[1]["items"][0]["text"], "Release");
    }

    #[test]
    fn test_teams_payload_with_icon_url_only() {
        let opts = TeamsOptions {
            title: None,
            color: None,
            icon_url: Some("https://example.com/icon.png"),
        };
        let payload = teams_payload("v1.0", &opts);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let body = json["attachments"][0]["content"]["body"]
            .as_array()
            .unwrap();
        assert_eq!(body[0]["type"], "Image");
        assert_eq!(body[0]["url"], "https://example.com/icon.png");
        assert_eq!(body[0]["size"], "Small");
        // No "style": "Person" when icon is standalone (no title context).
        assert_eq!(body[1]["type"], "TextBlock");
        assert_eq!(body[1]["text"], "v1.0");
    }
}
