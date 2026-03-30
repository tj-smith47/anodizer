use anyhow::Result;
use serde_json::json;

use crate::http::post_json;

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// Optional fields for Discord embed payloads.
pub struct DiscordOptions<'a> {
    pub author: Option<&'a str>,
    pub color: Option<u32>,
    pub icon_url: Option<&'a str>,
}

/// GoReleaser default blue colour.
const DEFAULT_COLOR: u32 = 3_888_754;

// ---------------------------------------------------------------------------
// Payload builder
// ---------------------------------------------------------------------------

pub(crate) fn discord_payload(message: &str, opts: &DiscordOptions<'_>) -> String {
    let color = opts.color.unwrap_or(DEFAULT_COLOR);

    let mut embed = json!({
        "description": message,
        "color": color,
    });

    if let Some(author) = opts.author {
        let mut author_obj = json!({ "name": author });
        if let Some(icon) = opts.icon_url {
            author_obj["icon_url"] = json!(icon);
        }
        embed["author"] = author_obj;
    } else if let Some(icon) = opts.icon_url {
        embed["author"] = json!({ "icon_url": icon });
    }

    json!({ "embeds": [embed] }).to_string()
}

// ---------------------------------------------------------------------------
// Send
// ---------------------------------------------------------------------------

/// POST a Discord webhook with an embed payload.
pub fn send_discord(webhook_url: &str, message: &str, opts: &DiscordOptions<'_>) -> Result<()> {
    let payload = discord_payload(message, opts);
    post_json(webhook_url, &payload, "discord")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_discord_payload_default_embed() {
        let opts = DiscordOptions {
            author: None,
            color: None,
            icon_url: None,
        };
        let payload = discord_payload("myapp v1.0.0 released!", &opts);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        // Should use embeds, not content
        assert!(json.get("content").is_none());
        let embeds = json["embeds"].as_array().unwrap();
        assert_eq!(embeds.len(), 1);
        assert_eq!(embeds[0]["description"], "myapp v1.0.0 released!");
        assert_eq!(embeds[0]["color"], DEFAULT_COLOR);
    }

    #[test]
    fn test_discord_payload_with_author_and_color() {
        let opts = DiscordOptions {
            author: Some("release-bot"),
            color: Some(0xFF0000),
            icon_url: None,
        };
        let payload = discord_payload("v2.0 shipped!", &opts);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let embed = &json["embeds"][0];
        assert_eq!(embed["author"]["name"], "release-bot");
        assert_eq!(embed["color"], 0xFF0000);
    }

    #[test]
    fn test_discord_payload_with_icon_url() {
        let opts = DiscordOptions {
            author: None,
            color: None,
            icon_url: Some("https://example.com/icon.png"),
        };
        let payload = discord_payload("released!", &opts);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let embed = &json["embeds"][0];
        assert_eq!(embed["author"]["icon_url"], "https://example.com/icon.png");
    }

    #[test]
    fn test_discord_payload_with_author_and_icon_url() {
        let opts = DiscordOptions {
            author: Some("release-bot"),
            color: None,
            icon_url: Some("https://example.com/icon.png"),
        };
        let payload = discord_payload("released!", &opts);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let embed = &json["embeds"][0];
        assert_eq!(embed["author"]["name"], "release-bot");
        assert_eq!(embed["author"]["icon_url"], "https://example.com/icon.png");
    }
}
