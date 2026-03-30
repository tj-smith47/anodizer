use anyhow::{Context as _, Result};
use serde_json::json;

// ---------------------------------------------------------------------------
// Payload builder
// ---------------------------------------------------------------------------

pub(crate) fn telegram_payload(
    chat_id: &str,
    message: &str,
    parse_mode: Option<&str>,
    message_thread_id: Option<i64>,
) -> String {
    let mut payload = json!({
        "chat_id": chat_id,
        "text": message,
    });
    if let Some(mode) = parse_mode {
        payload["parse_mode"] = json!(mode);
    }
    if let Some(thread_id) = message_thread_id {
        payload["message_thread_id"] = json!(thread_id);
    }
    payload.to_string()
}

// ---------------------------------------------------------------------------
// Send
// ---------------------------------------------------------------------------

/// POST to the Telegram Bot API `sendMessage` endpoint.
///
/// Even on HTTP 200, the Telegram API returns `{"ok": false, ...}` for logical
/// errors.  We parse the response body and surface `error_code` + `description`
/// when `ok` is false (matches GoReleaser telegram.go lines 87-94).
pub fn send_telegram(
    bot_token: &str,
    chat_id: &str,
    message: &str,
    parse_mode: Option<&str>,
    message_thread_id: Option<i64>,
) -> Result<()> {
    let url = format!("https://api.telegram.org/bot{bot_token}/sendMessage");
    let payload = telegram_payload(chat_id, message, parse_mode, message_thread_id);

    let client = reqwest::blocking::Client::new();
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .body(payload)
        .send()
        .with_context(|| "telegram: failed to send POST request")?;

    let status = resp.status();
    let body = resp.text().unwrap_or_default();

    if !status.is_success() {
        anyhow::bail!("telegram: HTTP {} — {}", status, body);
    }

    // Telegram can return HTTP 200 with ok:false for logical errors.
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body)
        && json.get("ok") == Some(&serde_json::Value::Bool(false)) {
            let error_code = json.get("error_code")
                .and_then(|v| v.as_i64())
                .map(|c| c.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let description = json.get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("no description");
            anyhow::bail!(
                "telegram: API error (code {}): {}",
                error_code,
                description
            );
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
        let payload = telegram_payload("-100123", "myapp v1.0.0 released!", None, None);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(json["chat_id"], "-100123");
        assert_eq!(json["text"], "myapp v1.0.0 released!");
        assert!(json.get("parse_mode").is_none());
        assert!(json.get("message_thread_id").is_none());
    }

    #[test]
    fn test_telegram_payload_with_parse_mode() {
        let payload =
            telegram_payload("-100123", "myapp v1.0.0 released!", Some("MarkdownV2"), None);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(json["chat_id"], "-100123");
        assert_eq!(json["text"], "myapp v1.0.0 released!");
        assert_eq!(json["parse_mode"], "MarkdownV2");
    }

    #[test]
    fn test_telegram_payload_html_mode() {
        let payload = telegram_payload("@mychannel", "<b>v2.0</b>", Some("HTML"), None);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(json["parse_mode"], "HTML");
    }

    #[test]
    fn test_telegram_payload_with_message_thread_id() {
        let payload = telegram_payload(
            "-100123",
            "released!",
            Some("MarkdownV2"),
            Some(42),
        );
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(json["message_thread_id"], 42);
        assert_eq!(json["parse_mode"], "MarkdownV2");
    }

    #[test]
    fn test_telegram_payload_thread_id_without_parse_mode() {
        let payload = telegram_payload("-100123", "hello", None, Some(99));
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(json["message_thread_id"], 99);
        assert!(json.get("parse_mode").is_none());
    }
}
