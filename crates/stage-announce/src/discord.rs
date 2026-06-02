use anodizer_core::retry::RetryPolicy;
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

/// Default blue colour.
const DEFAULT_COLOR: u32 = 3_888_754;

// ---------------------------------------------------------------------------
// Payload builder
// ---------------------------------------------------------------------------

pub(crate) fn discord_payload(message: &str, opts: &DiscordOptions<'_>) -> String {
    let color = opts.color.unwrap_or(DEFAULT_COLOR);

    let mut embed = serde_json::Map::new();
    embed.insert("description".into(), json!(message));
    embed.insert("color".into(), json!(color));

    // Discord rejects an `author` object without a `name` — it must always
    // accompany an `icon_url`. Suppress the embed.author entirely when
    // `name` is absent rather than building a payload Discord will reject.
    if let Some(name) = opts.author {
        let mut author_obj = serde_json::Map::new();
        author_obj.insert("name".into(), json!(name));
        if let Some(icon) = opts.icon_url {
            author_obj.insert("icon_url".into(), json!(icon));
        }
        embed.insert("author".into(), json!(author_obj));
    }

    json!({ "embeds": [serde_json::Value::Object(embed)] }).to_string()
}

// ---------------------------------------------------------------------------
// Send
// ---------------------------------------------------------------------------

/// POST a Discord webhook with an embed payload.
///
/// `policy` controls retry behaviour for transport-level / 5xx / 429 failures.
pub fn send_discord(
    webhook_url: &str,
    message: &str,
    opts: &DiscordOptions<'_>,
    policy: &RetryPolicy,
) -> Result<()> {
    let payload = discord_payload(message, opts);
    post_json(webhook_url, &payload, "discord", policy)
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
    fn test_discord_payload_icon_url_without_author_drops_author() {
        // Discord requires `author.name`; without it we must not emit an
        // `author` object at all (an icon-only author is invalid and 400s).
        let opts = DiscordOptions {
            author: None,
            color: None,
            icon_url: Some("https://example.com/icon.png"),
        };
        let payload = discord_payload("released!", &opts);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let embed = &json["embeds"][0];
        assert!(
            embed.get("author").is_none(),
            "author object must be omitted"
        );
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
