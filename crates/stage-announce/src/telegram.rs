use anyhow::Result;
use serde_json::json;

// ---------------------------------------------------------------------------
// Payload builder
// ---------------------------------------------------------------------------

pub(crate) fn telegram_payload(chat_id: &str, message: &str, parse_mode: Option<&str>) -> String {
    let mut payload = json!({
        "chat_id": chat_id,
        "text": message,
    });
    if let Some(mode) = parse_mode {
        payload["parse_mode"] = json!(mode);
    }
    payload.to_string()
}

// ---------------------------------------------------------------------------
// Send
// ---------------------------------------------------------------------------

/// POST to the Telegram Bot API `sendMessage` endpoint.
pub fn send_telegram(
    bot_token: &str,
    chat_id: &str,
    message: &str,
    parse_mode: Option<&str>,
) -> Result<()> {
    let url = format!("https://api.telegram.org/bot{bot_token}/sendMessage");
    let payload = telegram_payload(chat_id, message, parse_mode);
    let client = reqwest::blocking::Client::new();
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .body(payload)
        .send()?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        anyhow::bail!("telegram sendMessage returned non-success status {status}: {body}");
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
    fn test_telegram_payload_without_parse_mode() {
        let payload = telegram_payload("-100123", "myapp v1.0.0 released!", None);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(json["chat_id"], "-100123");
        assert_eq!(json["text"], "myapp v1.0.0 released!");
        assert!(json.get("parse_mode").is_none());
    }

    #[test]
    fn test_telegram_payload_with_parse_mode() {
        let payload = telegram_payload("-100123", "myapp v1.0.0 released!", Some("MarkdownV2"));
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(json["chat_id"], "-100123");
        assert_eq!(json["text"], "myapp v1.0.0 released!");
        assert_eq!(json["parse_mode"], "MarkdownV2");
    }

    #[test]
    fn test_telegram_payload_html_mode() {
        let payload = telegram_payload("@mychannel", "<b>v2.0</b>", Some("HTML"));
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(json["parse_mode"], "HTML");
    }
}
