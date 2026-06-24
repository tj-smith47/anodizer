use anodizer_core::retry::RetryPolicy;
use anyhow::Result;

use crate::helpers::retry_http;

/// Post a status (toot) to a Mastodon instance via the v1 statuses API.
///
/// `policy` enables retry on 5xx / 429 / network failures (P1.3). Routed
/// through the shared `retry_http` helper so the retry-classification logic
/// lives in exactly one place across the announce stage.
pub fn send_mastodon(
    server: &str,
    access_token: &str,
    message: &str,
    policy: &RetryPolicy,
) -> Result<()> {
    let url = format!("{}/api/v1/statuses", server.trim_end_matches('/'));
    let client = crate::http::blocking_client()?;

    let _ = retry_http("mastodon", "POST /api/v1/statuses", policy, || {
        client
            .post(&url)
            .bearer_auth(access_token)
            .form(&[("status", message)])
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
    fn happy_path_posts_to_statuses_endpoint() {
        let (addr, captured) =
            spawn_request_capturing_responder("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
        let server = format!("http://{addr}");
        let result = send_mastodon(&server, "tok", "hello world", &no_retry_policy());
        assert!(result.is_ok(), "send failed: {result:?}");
        std::thread::sleep(Duration::from_millis(50));
        let req = captured.lock().unwrap().clone();
        assert!(
            req.contains("/api/v1/statuses"),
            "endpoint missing: {req:?}"
        );
        assert!(
            req.to_ascii_lowercase()
                .contains("authorization: bearer tok"),
            "bearer auth missing: {req:?}"
        );
        assert!(req.contains("status=hello"), "form body missing: {req:?}");
    }

    #[test]
    fn trailing_slash_on_server_is_stripped() {
        let (addr, captured) =
            spawn_request_capturing_responder("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
        // Pass a trailing slash; URL should not double-slash before /api/v1/statuses.
        let server = format!("http://{addr}/");
        send_mastodon(&server, "tok", "msg", &no_retry_policy()).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let req = captured.lock().unwrap().clone();
        assert!(
            req.contains("POST /api/v1/statuses HTTP"),
            "double slash leaked into request line: {req:?}"
        );
    }

    #[test]
    fn retries_on_5xx_then_succeeds() {
        let (addr, counter) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
        ]);
        let server = format!("http://{addr}");
        send_mastodon(&server, "tok", "hi", &fast_policy()).unwrap();
        let attempts = counter.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(attempts, 2, "expected exactly 2 attempts, got {attempts}");
    }

    #[test]
    fn fails_fast_on_4xx() {
        let (addr, _counter) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n",
        ]);
        let server = format!("http://{addr}");
        let err = send_mastodon(&server, "bad-tok", "hi", &fast_policy()).unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("401"),
            "expected 401 in error chain: {chain}"
        );
    }
}
