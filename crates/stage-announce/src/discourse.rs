use anodizer_core::retry::RetryPolicy;
use anyhow::Result;
use serde_json::json;

use crate::helpers::retry_http;

/// Create a new topic on a Discourse forum.
///
/// Posts to `{server}/posts.json` with API key authentication.
/// The topic is created in the specified category with the given title and message.
///
/// `policy` enables retry on 5xx / 429 / network failures (P1.3).
#[allow(clippy::too_many_arguments)]
pub fn send_discourse(
    server: &str,
    api_key: &str,
    username: &str,
    category_id: u64,
    title: &str,
    message: &str,
    policy: &RetryPolicy,
    log: &anodizer_core::log::StageLogger,
) -> Result<()> {
    let url = format!("{}/posts.json", server.trim_end_matches('/'));
    let body = json!({
        "title": title,
        "raw": message,
        "category": category_id,
    })
    .to_string();

    let client = crate::http::blocking_client()?;
    let _ = retry_http("discourse", "create topic", policy, log, || {
        client
            .post(&url)
            .header("Api-Key", api_key)
            .header("Api-Username", username)
            .header("Content-Type", "application/json")
            .header("User-Agent", anodizer_core::http::USER_AGENT)
            .body(body.clone())
            .send()
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::test_helpers::responder::{
        spawn_oneshot_http_responder, spawn_request_capturing_responder,
    };
    use std::time::Duration;

    fn fast_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(10),
        }
    }

    fn no_retry_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 1,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(10),
        }
    }

    #[test]
    fn test_url_construction_strips_trailing_slash() {
        let server = "https://forum.example.com/";
        let url = format!("{}/posts.json", server.trim_end_matches('/'));
        assert_eq!(url, "https://forum.example.com/posts.json");
    }

    #[test]
    fn test_url_construction_no_trailing_slash() {
        let server = "https://forum.example.com";
        let url = format!("{}/posts.json", server.trim_end_matches('/'));
        assert_eq!(url, "https://forum.example.com/posts.json");
    }

    #[test]
    fn happy_path_posts_with_api_headers() {
        let (addr, captured) =
            spawn_request_capturing_responder("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
        let server = format!("http://{addr}");
        send_discourse(
            &server,
            "api-key",
            "user",
            7,
            "Title",
            "Body",
            &no_retry_policy(),
            anodizer_core::test_helpers::test_logger(),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let req = captured.lock().unwrap().clone();
        assert!(req.contains("POST /posts.json"), "endpoint: {req:?}");
        let lower = req.to_ascii_lowercase();
        assert!(
            lower.contains("api-key: api-key"),
            "api-key header: {req:?}"
        );
        assert!(
            lower.contains("api-username: user"),
            "api-username: {req:?}"
        );
        assert!(
            lower.contains("content-type: application/json"),
            "content-type: {req:?}"
        );
        // Body content: serde_json field order is insertion-order for json!
        assert!(
            req.contains("\"title\":\"Title\""),
            "title in body: {req:?}"
        );
        assert!(req.contains("\"category\":7"), "category in body: {req:?}");
    }

    #[test]
    fn retries_5xx_then_succeeds() {
        let (addr, counter) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
        ]);
        let server = format!("http://{addr}");
        send_discourse(
            &server,
            "k",
            "u",
            1,
            "t",
            "b",
            &fast_policy(),
            anodizer_core::test_helpers::test_logger(),
        )
        .unwrap();
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn fast_fails_on_403() {
        let (addr, _) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 403 Forbidden\r\nContent-Length: 7\r\n\r\nnope :(",
        ]);
        let server = format!("http://{addr}");
        let err = send_discourse(
            &server,
            "k",
            "u",
            1,
            "t",
            "b",
            &fast_policy(),
            anodizer_core::test_helpers::test_logger(),
        )
        .unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("403"),
            "expected 403 in error chain: {chain}"
        );
    }
}
