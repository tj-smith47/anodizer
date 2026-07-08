use std::ops::ControlFlow;

use anodizer_core::retry::{HttpError, RetryLog, RetryPolicy, is_retriable, retry_sync};
use anyhow::{Context as _, Result};
use serde_json::json;

/// Replacement marker for the bot token in any error message we surface
/// upstream. The Telegram URL is `…/bot<TOKEN>/sendMessage`, and the
/// `reqwest::Error` Display chain echoes the full URL on transport
/// failure — without redaction the token would leak via every error log.
const REDACTED_BOT_TOKEN_MARKER: &str = "<REDACTED_BOT_TOKEN>";

/// Default Telegram Bot API base. The `sendMessage` URL is built as
/// `{base}/bot{token}/sendMessage`.
const TELEGRAM_API_BASE: &str = "https://api.telegram.org";

/// Resolve the Telegram Bot API base URL. Read at call time so tests can set
/// `ANODIZE_TELEGRAM_API_BASE` to redirect the POST to a local mock; production
/// never sets the variable and so hits the real endpoint.
fn telegram_api_base() -> String {
    std::env::var("ANODIZE_TELEGRAM_API_BASE").unwrap_or_else(|_| TELEGRAM_API_BASE.to_string())
}

/// Strip occurrences of `bot_token` from any error string before it is
/// surfaced upstream. Returns the message unchanged when the token is
/// empty (an empty `String::replace` needle would inject the marker
/// between every byte).
///
/// Composes with [`anodizer_core::redact::redact_url_credentials`] for
/// defense-in-depth: callers should apply both so any URL-shaped
/// secret (userinfo segment) is also scrubbed before the message
/// lands in a log or error chain.
fn redact_bot_token(message: &str, bot_token: &str) -> String {
    let token_stripped = if bot_token.is_empty() {
        message.to_string()
    } else {
        message.replace(bot_token, REDACTED_BOT_TOKEN_MARKER)
    };
    anodizer_core::redact::redact_url_credentials(&token_stripped)
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
/// when `ok` is false.
pub fn send_telegram(
    bot_token: &str,
    chat_id: &str,
    message: &str,
    parse_mode: Option<&str>,
    message_thread_id: Option<i64>,
    policy: &RetryPolicy,
    log: &anodizer_core::log::StageLogger,
) -> Result<()> {
    let url = format!("{}/bot{bot_token}/sendMessage", telegram_api_base());
    let payload = telegram_payload(chat_id, message, parse_mode, message_thread_id);

    let client = crate::http::blocking_client()?;
    retry_sync(
        RetryLog::new("telegram announce", log),
        policy,
        |_attempt| {
            let send_result = client
                .post(&url)
                .header("Content-Type", "application/json")
                .body(payload.clone())
                .send();

            match send_result {
                Err(e) => {
                    let msg = redact_bot_token(&e.to_string(), bot_token);
                    let err = anyhow::Error::new(HttpError::from_response(e, None))
                        .context(format!("telegram: failed to send POST request: {msg}"));
                    if is_retriable(err.as_ref()) {
                        Err(ControlFlow::Continue(err))
                    } else {
                        Err(ControlFlow::Break(err))
                    }
                }
                Ok(resp) => {
                    let status = resp.status();
                    let body =
                        redact_bot_token(&anodizer_core::http::body_of_blocking(resp), bot_token);

                    if !status.is_success() {
                        let inner = anyhow::anyhow!("telegram: HTTP {} — {}", status, body);
                        let wrapped = anyhow::Error::new(HttpError::new(
                            std::io::Error::other(inner.to_string()),
                            status.as_u16(),
                        ))
                        .context(inner);
                        return if is_retriable(wrapped.as_ref()) {
                            Err(ControlFlow::Continue(wrapped))
                        } else {
                            Err(ControlFlow::Break(wrapped))
                        };
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
                        // API errors are not retriable — they describe a logical
                        // rejection (bad chat_id, bad parse_mode, etc.) that
                        // won't change on retry.
                        return Err(ControlFlow::Break(anyhow::anyhow!(
                            "telegram: API error (code {}): {}",
                            error_code,
                            description
                        )));
                    }
                    Ok(())
                }
            }
        },
    )
    .context("telegram: POST exhausted retry attempts")
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

    #[test]
    fn redact_bot_token_also_strips_url_userinfo() {
        // Defense-in-depth: combined redaction also strips
        // `user:pass@host` userinfo from any URL in the message so the
        // (rare) case of a wrapped reqwest::Error carrying both a bot
        // token AND inline URL credentials surfaces neither.
        let msg = "error at https://admin:pw@proxy/bot123:ABC/sendMessage: refused";
        let out = redact_bot_token(msg, "123:ABC");
        assert!(!out.contains("123:ABC"), "token leaked: {out}");
        assert!(!out.contains("admin:pw"), "userinfo leaked: {out}");
        assert!(
            out.contains("<redacted>@proxy"),
            "expected url redaction: {out}"
        );
    }

    // ---- live send over a mock HTTP server -----------------------------
    //
    // These drive `send_telegram` (the real production POST path) against a
    // scripted responder via the `ANODIZE_TELEGRAM_API_BASE` seam. Mutating
    // process env requires `#[serial]` + the shared env_mutex so concurrent
    // tests don't observe each other's override.

    use anodizer_core::retry::RetryPolicy;
    use anodizer_core::test_helpers::env::env_mutex;
    use anodizer_core::test_helpers::scripted_responder::{
        ScriptedRoute, spawn_scripted_responder,
    };

    /// One-attempt policy so an error-path test fails fast instead of
    /// retrying a 5xx ten times against the mock.
    const ONE_SHOT: RetryPolicy = RetryPolicy {
        max_attempts: 1,
        base_delay: std::time::Duration::from_millis(0),
        max_delay: std::time::Duration::from_millis(0),
    };

    struct EnvGuard {
        key: &'static str,
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // env-ok: #[serial(announce_env)] + env_mutex; per-test API-base redirect
            unsafe { std::env::remove_var(self.key) };
        }
    }
    fn set_base(addr: std::net::SocketAddr) -> EnvGuard {
        // env-ok: #[serial(announce_env)] + env_mutex; per-test API-base redirect
        unsafe { std::env::set_var("ANODIZE_TELEGRAM_API_BASE", format!("http://{addr}")) };
        EnvGuard {
            key: "ANODIZE_TELEGRAM_API_BASE",
        }
    }

    /// Build a `&'static` HTTP response with a correct `Content-Length`.
    /// `ScriptedRoute::response` is `&'static str`, so the assembled string is
    /// leaked — acceptable in a short-lived test process and the only way to
    /// hand a computed-length body to the responder.
    fn http_response(status_line: &str, body: &str) -> &'static str {
        let resp = format!(
            "{status_line}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        Box::leak(resp.into_boxed_str())
    }

    #[test]
    #[serial_test::serial(announce_env)]
    fn send_telegram_happy_path_posts_templated_payload() {
        let _g = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/bot123:ABC/sendMessage",
            response: http_response("HTTP/1.1 200 OK", "{\"ok\":true}"),
            times: None,
        }]);
        let _base = set_base(addr);

        send_telegram(
            "123:ABC",
            "-100999",
            "MyApp v1.2.3 is out!",
            Some("HTML"),
            Some(42),
            &ONE_SHOT,
            anodizer_core::test_helpers::test_logger(),
        )
        .expect("happy path should succeed");

        let entries = log.lock().unwrap();
        assert_eq!(entries.len(), 1, "exactly one POST expected");
        let req = &entries[0];
        assert_eq!(req.method, "POST");
        // The bot token must appear in the path, never the body.
        assert_eq!(req.path, "/bot123:ABC/sendMessage");
        assert_eq!(req.header("content-type"), Some("application/json"));
        let body: serde_json::Value = serde_json::from_str(&req.body).expect("json body");
        assert_eq!(body["chat_id"], "-100999");
        assert_eq!(body["text"], "MyApp v1.2.3 is out!");
        assert_eq!(body["parse_mode"], "HTML");
        assert_eq!(body["message_thread_id"], 42);
    }

    #[test]
    #[serial_test::serial(announce_env)]
    fn send_telegram_non_2xx_maps_to_error_with_context() {
        let _g = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/bot123:ABC/sendMessage",
            response: http_response("HTTP/1.1 401 Unauthorized", "{\"description\":\"bad tok\"}"),
            times: None,
        }]);
        let _base = set_base(addr);

        let err = format!(
            "{:#}",
            send_telegram(
                "123:ABC",
                "-1",
                "hi",
                None,
                None,
                &ONE_SHOT,
                anodizer_core::test_helpers::test_logger()
            )
            .unwrap_err()
        );
        assert!(err.contains("401"), "status must surface: {err}");
        assert!(err.contains("bad tok"), "response body must surface: {err}");
    }

    #[test]
    #[serial_test::serial(announce_env)]
    fn send_telegram_http_200_ok_false_surfaces_api_error() {
        let _g = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        // Telegram returns HTTP 200 with `ok:false` for logical rejections;
        // these must NOT be reported as success.
        let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/botT/sendMessage",
            response: http_response(
                "HTTP/1.1 200 OK",
                "{\"ok\":false,\"error_code\":400,\"description\":\"chat not found\"}",
            ),
            times: None,
        }]);
        let _base = set_base(addr);

        let err = format!(
            "{:#}",
            send_telegram(
                "T",
                "-1",
                "hi",
                None,
                None,
                &ONE_SHOT,
                anodizer_core::test_helpers::test_logger()
            )
            .unwrap_err()
        );
        assert!(err.contains("400"), "error_code must surface: {err}");
        assert!(
            err.contains("chat not found"),
            "description must surface: {err}"
        );
    }
}
