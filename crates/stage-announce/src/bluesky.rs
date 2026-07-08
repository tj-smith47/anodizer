use anodizer_core::retry::RetryPolicy;
use anyhow::{Context as _, Result};
use serde_json::json;

use crate::helpers::retry_http;

/// Default Bluesky PDS (Personal Data Server). Override via
/// `bluesky.pds_url` in config to target a self-hosted PDS.
pub const DEFAULT_PDS_URL: &str = "https://bsky.social";

pub fn send_bluesky(
    username: &str,
    app_password: &str,
    message: &str,
    release_url: Option<&str>,
    pds_url: Option<&str>,
    policy: &RetryPolicy,
    log: &anodizer_core::log::StageLogger,
) -> Result<()> {
    let pds_url = pds_url
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| DEFAULT_PDS_URL.to_string());
    let client = crate::http::blocking_client()?;

    let session_payload = json!({
        "identifier": username,
        "password": app_password,
    })
    .to_string();
    let session_text = retry_http("bluesky", "createSession", policy, log, || {
        client
            .post(format!("{pds_url}/xrpc/com.atproto.server.createSession"))
            .header("Content-Type", "application/json")
            .body(session_payload.clone())
            .send()
    })?;
    let session: serde_json::Value = serde_json::from_str(&session_text)
        .context("bluesky: createSession response was not valid JSON")?;
    let access_jwt = session["accessJwt"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("bluesky: missing accessJwt in session response"))?;
    let did = session["did"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("bluesky: missing did in session response"))?;

    let now = anodizer_core::sde::resolve_now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut record = json!({
        "$type": "app.bsky.feed.post",
        "text": message,
        "createdAt": now,
    });

    // Add link facet if release_url is found in message
    if let Some(url) = release_url
        && let Some(byte_start) = message.find(url)
    {
        let byte_end = byte_start + url.len();
        record["facets"] = json!([{
            "index": {"byteStart": byte_start, "byteEnd": byte_end},
            "features": [{"$type": "app.bsky.richtext.facet#link", "uri": url}]
        }]);
    }

    let create_body = json!({
        "repo": did,
        "collection": "app.bsky.feed.post",
        "record": record,
    })
    .to_string();

    let _ = retry_http("bluesky", "createRecord", policy, log, || {
        client
            .post(format!("{pds_url}/xrpc/com.atproto.repo.createRecord"))
            .bearer_auth(access_jwt)
            .header("Content-Type", "application/json")
            .body(create_body.clone())
            .send()
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::test_helpers::scripted_responder::{
        ScriptedRoute, spawn_scripted_responder,
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
    fn test_link_facet_detection() {
        let message =
            "myapp v1.0.0 is out! Check it out at https://github.com/org/repo/releases/tag/v1.0.0";
        let url = "https://github.com/org/repo/releases/tag/v1.0.0";
        let byte_start = message.find(url).unwrap();
        let byte_end = byte_start + url.len();
        assert_eq!(byte_start, 37);
        assert_eq!(byte_end, 37 + url.len());
    }

    #[test]
    fn test_link_facet_not_found() {
        let message = "myapp v1.0.0 is out!";
        let url = "https://github.com/org/repo/releases/tag/v1.0.0";
        assert!(message.find(url).is_none());
    }

    /// Two-step flow: createSession returns accessJwt+did, then createRecord
    /// receives the post envelope.
    #[test]
    fn happy_path_creates_session_then_record() {
        let (addr, log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "POST",
                path_pattern: "/xrpc/com.atproto.server.createSession",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 54\r\n\r\n{\"accessJwt\":\"jwt-token\",\"did\":\"did:plc:abc123xyz456\"}",
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/xrpc/com.atproto.repo.createRecord",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 16\r\n\r\n{\"uri\":\"at://x\"}",
                times: None,
            },
        ]);
        let pds = format!("http://{addr}");
        send_bluesky(
            "alice.bsky.social",
            "app-pw",
            "hi there",
            None,
            Some(&pds),
            &no_retry_policy(),
            anodizer_core::test_helpers::test_logger(),
        )
        .unwrap();
        let entries = log.lock().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "/xrpc/com.atproto.server.createSession");
        assert!(
            entries[0]
                .body
                .contains("\"identifier\":\"alice.bsky.social\""),
            "identifier in session body: {:?}",
            entries[0].body
        );
        assert!(
            entries[0].body.contains("\"password\":\"app-pw\""),
            "app-password in session body: {:?}",
            entries[0].body
        );
        assert_eq!(entries[1].path, "/xrpc/com.atproto.repo.createRecord");
        assert!(
            entries[1]
                .body
                .contains("\"repo\":\"did:plc:abc123xyz456\""),
            "did wired into createRecord: {:?}",
            entries[1].body
        );
        assert!(
            entries[1].body.contains("\"text\":\"hi there\""),
            "text in record: {:?}",
            entries[1].body
        );
    }

    /// When a release URL appears in the message text, the resulting record
    /// must carry a `facets` array pointing the byte range at the URL.
    #[test]
    fn release_url_in_message_emits_link_facet() {
        let (addr, log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "POST",
                path_pattern: "/xrpc/com.atproto.server.createSession",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 54\r\n\r\n{\"accessJwt\":\"jwt-token\",\"did\":\"did:plc:abc123xyz456\"}",
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/xrpc/com.atproto.repo.createRecord",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 16\r\n\r\n{\"uri\":\"at://x\"}",
                times: None,
            },
        ]);
        let pds = format!("http://{addr}");
        let url = "https://example.com/release";
        let message = format!("released! {url}");
        send_bluesky(
            "a",
            "p",
            &message,
            Some(url),
            Some(&pds),
            &no_retry_policy(),
            anodizer_core::test_helpers::test_logger(),
        )
        .unwrap();
        let entries = log.lock().unwrap();
        let body = &entries[1].body;
        assert!(body.contains("facets"), "facets emitted: {body:?}");
        assert!(body.contains("byteStart"), "byteStart in facets: {body:?}");
        assert!(body.contains(url), "url present in facet: {body:?}");
    }

    /// Missing `accessJwt` in the session response must bail with a clear
    /// message rather than passing `None` into the next request.
    #[test]
    fn missing_access_jwt_bails() {
        let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/xrpc/com.atproto.server.createSession",
            response: "HTTP/1.1 200 OK\r\nContent-Length: 19\r\n\r\n{\"did\":\"did:plc:x\"}",
            times: None,
        }]);
        let pds = format!("http://{addr}");
        let err = send_bluesky(
            "a",
            "p",
            "msg",
            None,
            Some(&pds),
            &no_retry_policy(),
            anodizer_core::test_helpers::test_logger(),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("accessJwt"),
            "expected accessJwt in err: {err}"
        );
    }

    /// Missing `did` in the session response must bail before reaching
    /// createRecord.
    #[test]
    fn missing_did_bails() {
        let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/xrpc/com.atproto.server.createSession",
            response: "HTTP/1.1 200 OK\r\nContent-Length: 21\r\n\r\n{\"accessJwt\":\"jwt-x\"}",
            times: None,
        }]);
        let pds = format!("http://{addr}");
        let err = send_bluesky(
            "a",
            "p",
            "msg",
            None,
            Some(&pds),
            &no_retry_policy(),
            anodizer_core::test_helpers::test_logger(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("did"), "expected did in err: {err}");
    }

    /// Non-JSON session response is reported with the "not valid JSON" hint
    /// so users debugging a wrong PDS hostname see the cause.
    #[test]
    fn invalid_json_session_response_bails() {
        let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/xrpc/com.atproto.server.createSession",
            response: "HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\nnot a json!",
            times: None,
        }]);
        let pds = format!("http://{addr}");
        let err = send_bluesky(
            "a",
            "p",
            "msg",
            None,
            Some(&pds),
            &no_retry_policy(),
            anodizer_core::test_helpers::test_logger(),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("not valid JSON"),
            "expected invalid-json hint: {err}"
        );
    }

    /// 5xx on createSession is classified as retriable; second attempt
    /// succeeds.
    #[test]
    fn retries_session_5xx_then_succeeds() {
        let (addr, log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "POST",
                path_pattern: "/xrpc/com.atproto.server.createSession",
                response: "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
                times: Some(1),
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/xrpc/com.atproto.server.createSession",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 54\r\n\r\n{\"accessJwt\":\"jwt-token\",\"did\":\"did:plc:abc123xyz456\"}",
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/xrpc/com.atproto.repo.createRecord",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 16\r\n\r\n{\"uri\":\"at://x\"}",
                times: None,
            },
        ]);
        let pds = format!("http://{addr}");
        send_bluesky(
            "a",
            "p",
            "msg",
            None,
            Some(&pds),
            &fast_policy(),
            anodizer_core::test_helpers::test_logger(),
        )
        .unwrap();
        let entries = log.lock().unwrap();
        // 2 session attempts + 1 record attempt.
        assert_eq!(entries.len(), 3, "{entries:?}");
    }
}
