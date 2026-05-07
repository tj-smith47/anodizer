use anyhow::Result;
use serde_json::json;

/// Replacement marker for the bot token in any error message we surface
/// upstream. The Telegram URL is `…/bot<TOKEN>/sendMessage`, and the
/// `reqwest::Error` Display chain echoes the full URL on transport
/// failure — without redaction the token would leak via every error log.
const REDACTED_BOT_TOKEN_MARKER: &str = "<REDACTED_BOT_TOKEN>";

/// Strip occurrences of `bot_token` from any error string before it is
/// surfaced upstream. Returns the message unchanged when the token is
/// empty (an empty `String::replace` needle would inject the marker
/// between every byte).
fn redact_bot_token(message: &str, bot_token: &str) -> String {
    if bot_token.is_empty() {
        return message.to_string();
    }
    message.replace(bot_token, REDACTED_BOT_TOKEN_MARKER)
}

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
        .map_err(|e| {
            // `reqwest::Error::Display` echoes the full request URL,
            // which contains the bot token in the path segment
            // (`…/bot<TOKEN>/sendMessage`). We must redact before the
            // error chain is rendered into anodizer's log.
            let msg = redact_bot_token(&e.to_string(), bot_token);
            anyhow::anyhow!("telegram: failed to send POST request: {msg}")
        })?;

    let status = resp.status();
    let body = redact_bot_token(&anodizer_core::http::body_of_blocking(resp), bot_token);

    if !status.is_success() {
        anyhow::bail!("telegram: HTTP {} — {}", status, body);
    }

    // Telegram can return HTTP 200 with ok:false for logical errors.
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body)
        && json.get("ok") == Some(&serde_json::Value::Bool(false))
    {
        let error_code = json
            .get("error_code")
            .and_then(|v| v.as_i64())
            .map(|c| c.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let description = json
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("no description");
        anyhow::bail!("telegram: API error (code {}): {}", error_code, description);
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
        let payload = telegram_payload(
            "-100123",
            "myapp v1.0.0 released!",
            Some("MarkdownV2"),
            None,
        );
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
        let payload = telegram_payload("-100123", "released!", Some("MarkdownV2"), Some(42));
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

    // ---- token redaction (I2 regression) ------------------------------

    #[test]
    fn redact_bot_token_strips_token_from_message() {
        // Simulate a `reqwest::Error` Display chain that has echoed the
        // full request URL with the bot token in it.
        let err_msg = "error sending request for url \
                       (https://api.telegram.org/bot123:ABC/sendMessage): \
                       connection refused";
        let redacted = redact_bot_token(err_msg, "123:ABC");
        assert!(
            !redacted.contains("123:ABC"),
            "redacted message must not contain the token: {redacted}"
        );
        assert!(
            redacted.contains("<REDACTED_BOT_TOKEN>"),
            "redacted message must contain the marker: {redacted}"
        );
    }

    #[test]
    fn redact_bot_token_empty_token_passthrough() {
        // A bot_token of `""` would, with naive `String::replace`, inject
        // the marker between every byte. Guard against that.
        let msg = "abc";
        let out = redact_bot_token(msg, "");
        assert_eq!(out, msg);
    }

    #[test]
    fn redact_bot_token_no_token_in_message_passthrough() {
        let msg = "no secrets here";
        let out = redact_bot_token(msg, "123:ABC");
        assert_eq!(out, msg);
    }
}
