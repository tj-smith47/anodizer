use std::error::Error as StdError;
use std::fmt;
use std::ops::ControlFlow;

use anodizer_core::log::StageLogger;
use anodizer_core::retry::{HttpError, RetryLog, RetryPolicy, is_retriable, retry_sync};
use anyhow::Result;
use serde_json::json;

const API_BASE: &str = "https://api.linkedin.com";

/// Build the user-facing error message for a LinkedIn HTTP failure on the
/// given endpoint stage ("share", "GET /v2/userinfo", "GET /v2/me"). The
/// response body is included verbatim:
/// LinkedIn's JSON error envelope (`{ "message": "...", "serviceErrorCode": ... }`)
/// is the only actionable signal the user gets, so it must reach them.
pub(crate) fn format_linkedin_http_error(
    endpoint: &str,
    status: reqwest::StatusCode,
    body: &str,
) -> String {
    format!("linkedin: {endpoint} failed ({status}): {body}")
}

/// Typed sentinel signaling that `/v2/userinfo` returned 403 and the caller
/// should fall back to the legacy `/v2/me` endpoint. Replaces the previous
/// `__linkedin_fallback__` magic-string sentinel routed through error
/// messages; typed downcast is robust to message rewrites.
#[derive(Debug)]
struct LinkedinFallback;

impl fmt::Display for LinkedinFallback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("linkedin: /v2/userinfo returned 403, fall back to /v2/me")
    }
}

impl StdError for LinkedinFallback {}

/// Loose structural check on a LinkedIn access token. LinkedIn issues
/// signed JWTs (3 dot-separated base64url segments) and opaque OAuth tokens
/// (long alphanumeric blobs). We accept either shape and only reject values
/// that are obviously not credentials so that an early bail beats a 401
/// from the API.
pub fn validate_token_shape(token: &str) -> Result<()> {
    crate::util::validate_token_min_length("linkedin", "LINKEDIN_ACCESS_TOKEN", token, 16)?;
    let dot_segments = token.split('.').count();
    if dot_segments == 3 {
        for (idx, seg) in token.split('.').enumerate() {
            if seg.is_empty() {
                anyhow::bail!(
                    "announce.linkedin: LINKEDIN_ACCESS_TOKEN looks like a JWT but \
                     segment {} is empty",
                    idx + 1
                );
            }
            if !seg
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '=')
            {
                anyhow::bail!(
                    "announce.linkedin: LINKEDIN_ACCESS_TOKEN looks like a JWT but \
                     segment {} contains non-base64url characters",
                    idx + 1
                );
            }
        }
    } else if dot_segments != 1 {
        anyhow::bail!(
            "announce.linkedin: LINKEDIN_ACCESS_TOKEN has {dot_segments} dot-separated \
             segments, expected 1 (opaque token) or 3 (JWT)"
        );
    }
    Ok(())
}

/// Post a share to LinkedIn via the v2 Share API.
///
/// Two-step flow:
/// 1. Resolve the profile URN via `/v2/userinfo` (newer, uses `sub` field).
///    Falls back to `/v2/me` (legacy, uses `id` field) only on 403 Forbidden.
/// 2. POST the share to `/v2/shares`.
///
/// Error categorisation mirrors upstream commit 0944b0e: API errors
/// (HTTP 4xx/5xx) wrap the response body in the surfaced error message,
/// transport errors are classified separately. `policy` enables retry on
/// 5xx / 429 / network failures via `retryx.HTTP` semantics.
pub fn send_linkedin(
    access_token: &str,
    message: &str,
    log: &StageLogger,
    policy: &RetryPolicy,
) -> Result<()> {
    send_linkedin_to(API_BASE, access_token, message, log, policy)
}

/// Base-URL-injectable form of [`send_linkedin`]. Production passes
/// [`API_BASE`]; unit tests point `api_base` at a local responder so the
/// full share flow (URN resolution, 403 fallback, retry classification)
/// runs without touching the real LinkedIn API.
fn send_linkedin_to(
    api_base: &str,
    access_token: &str,
    message: &str,
    log: &StageLogger,
    policy: &RetryPolicy,
) -> Result<()> {
    let client = crate::http::blocking_client()?;
    let profile_urn = get_profile_urn(&client, api_base, access_token, policy, log)?;

    let share = json!({
        "owner": profile_urn,
        "text": { "text": message },
        "distribution": { "linkedInDistributionTarget": {} }
    });

    let resp_text = retry_sync(RetryLog::new("linkedin share", log), policy, |_attempt| {
        let send_result = client
            .post(format!("{api_base}/v2/shares"))
            .bearer_auth(access_token)
            .header("Content-Type", "application/json")
            .header("X-Restli-Protocol-Version", "2.0.0")
            .body(share.to_string())
            .send();

        match send_result {
            Err(e) => {
                let err = anyhow::Error::new(HttpError::from_response(e, None))
                    .context("linkedin: POST /v2/shares transport error");
                if is_retriable(err.as_ref()) {
                    Err(ControlFlow::Continue(err))
                } else {
                    Err(ControlFlow::Break(err))
                }
            }
            Ok(resp) => {
                let status = resp.status();
                let body = anodizer_core::http::body_of_blocking(resp);
                if status.is_success() {
                    Ok(body)
                } else {
                    // Include the body in the error message so users can
                    // see LinkedIn's structured error response.
                    let inner =
                        anyhow::anyhow!("{}", format_linkedin_http_error("share", status, &body));
                    let wrapped = anyhow::Error::new(HttpError::new(
                        std::io::Error::other(inner.to_string()),
                        status.as_u16(),
                    ))
                    .context(inner);
                    if is_retriable(wrapped.as_ref()) {
                        Err(ControlFlow::Continue(wrapped))
                    } else {
                        Err(ControlFlow::Break(wrapped))
                    }
                }
            }
        }
    })?;

    let resp_json: serde_json::Value = serde_json::from_str(&resp_text)
        .map_err(|e| anyhow::anyhow!("linkedin: failed to parse share response: {e}"))?;
    let activity = resp_json
        .get("activity")
        .and_then(|a| a.as_str())
        .ok_or_else(|| anyhow::anyhow!("linkedin: could not find 'activity' in share response"))?;
    log.status(&format!(
        "linkedin post available at https://www.linkedin.com/feed/update/{activity}"
    ));

    Ok(())
}

/// Resolve the LinkedIn profile URN (`urn:li:person:<id>`).
///
/// Tries `/v2/userinfo` first (newer endpoint, `sub` field).  Falls back to
/// `/v2/me` (legacy, `id` field) only when the newer endpoint returns 403.
fn get_profile_urn(
    client: &reqwest::blocking::Client,
    api_base: &str,
    access_token: &str,
    policy: &RetryPolicy,
    log: &StageLogger,
) -> Result<String> {
    let outcome = retry_sync(
        RetryLog::new("linkedin profile lookup", log),
        policy,
        |_attempt| {
            match client
                .get(format!("{api_base}/v2/userinfo"))
                .bearer_auth(access_token)
                .send()
            {
                Err(e) => {
                    let err = anyhow::Error::new(HttpError::from_response(e, None))
                        .context("linkedin: GET /v2/userinfo transport error");
                    if is_retriable(err.as_ref()) {
                        Err(ControlFlow::Continue(err))
                    } else {
                        Err(ControlFlow::Break(err))
                    }
                }
                Ok(resp) => {
                    let status = resp.status();
                    if status == reqwest::StatusCode::FORBIDDEN {
                        // Typed sentinel: 403 means "fall back to legacy
                        // endpoint" rather than retry. The downcast at the
                        // call-site is robust to error-message rewrites.
                        return Err(ControlFlow::Break(anyhow::Error::new(LinkedinFallback)));
                    }
                    let body = anodizer_core::http::body_of_blocking(resp);
                    if status.is_success() {
                        Ok(body)
                    } else {
                        let inner = anyhow::anyhow!(
                            "{}",
                            format_linkedin_http_error("GET /v2/userinfo", status, &body)
                        );
                        let wrapped = anyhow::Error::new(HttpError::new(
                            std::io::Error::other(inner.to_string()),
                            status.as_u16(),
                        ))
                        .context(inner);
                        if is_retriable(wrapped.as_ref()) {
                            Err(ControlFlow::Continue(wrapped))
                        } else {
                            Err(ControlFlow::Break(wrapped))
                        }
                    }
                }
            }
        },
    );

    let text = match outcome {
        Ok(text) => text,
        Err(e) if e.downcast_ref::<LinkedinFallback>().is_some() => {
            return get_profile_urn_legacy(client, api_base, access_token, policy, log);
        }
        Err(e) => return Err(e),
    };

    let json: serde_json::Value = serde_json::from_str(&text)?;
    let sub = json["sub"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("linkedin: missing 'sub' in /v2/userinfo response"))?;
    Ok(format!("urn:li:person:{sub}"))
}

/// Legacy fallback: resolve profile URN via `/v2/me`.
fn get_profile_urn_legacy(
    client: &reqwest::blocking::Client,
    api_base: &str,
    access_token: &str,
    policy: &RetryPolicy,
    log: &StageLogger,
) -> Result<String> {
    let text = retry_sync(
        RetryLog::new("linkedin legacy profile lookup", log),
        policy,
        |_attempt| match client
            .get(format!("{api_base}/v2/me"))
            .bearer_auth(access_token)
            .send()
        {
            Err(e) => {
                let err = anyhow::Error::new(HttpError::from_response(e, None))
                    .context("linkedin: GET /v2/me transport error");
                if is_retriable(err.as_ref()) {
                    Err(ControlFlow::Continue(err))
                } else {
                    Err(ControlFlow::Break(err))
                }
            }
            Ok(resp) => {
                let status = resp.status();
                if status == reqwest::StatusCode::FORBIDDEN {
                    return Err(ControlFlow::Break(anyhow::anyhow!(
                        "linkedin: forbidden, please check your permissions"
                    )));
                }
                let body = anodizer_core::http::body_of_blocking(resp);
                if status.is_success() {
                    Ok(body)
                } else {
                    let inner = anyhow::anyhow!(
                        "{}",
                        format_linkedin_http_error("GET /v2/me", status, &body)
                    );
                    let wrapped = anyhow::Error::new(HttpError::new(
                        std::io::Error::other(inner.to_string()),
                        status.as_u16(),
                    ))
                    .context(inner);
                    if is_retriable(wrapped.as_ref()) {
                        Err(ControlFlow::Continue(wrapped))
                    } else {
                        Err(ControlFlow::Break(wrapped))
                    }
                }
            }
        },
    )?;

    let json: serde_json::Value = serde_json::from_str(&text)?;
    let id = json["id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("linkedin: missing 'id' in /v2/me response"))?;
    Ok(format!("urn:li:person:{id}"))
}

#[cfg(test)]
mod tests {
    use super::{LinkedinFallback, format_linkedin_http_error, validate_token_shape};
    use serde_json::json;

    /// The typed 403-fallback sentinel's `Display` names the endpoint and the
    /// fall-back target so a surfaced error (when the downcast misses) is still
    /// actionable. Pins the exact text the `LinkedinFallback` sentinel renders.
    #[test]
    fn fallback_sentinel_display_names_endpoints() {
        let msg = LinkedinFallback.to_string();
        assert_eq!(
            msg,
            "linkedin: /v2/userinfo returned 403, fall back to /v2/me"
        );
    }

    #[test]
    fn http_error_includes_endpoint_status_and_body() {
        // Upstream commit 0944b0e: the error surfaced to the user must
        // name the endpoint, the HTTP status, AND the response body so
        // LinkedIn's structured error (`message`, `serviceErrorCode`) reaches
        // the user.
        let msg = format_linkedin_http_error(
            "share",
            reqwest::StatusCode::FORBIDDEN,
            r#"{"message":"insufficient scope"}"#,
        );
        assert!(msg.contains("share"), "{msg}");
        assert!(msg.contains("403"), "{msg}");
        assert!(msg.contains("insufficient scope"), "{msg}");
    }

    #[test]
    fn http_error_includes_endpoint_for_userinfo_and_me() {
        let userinfo = format_linkedin_http_error(
            "GET /v2/userinfo",
            reqwest::StatusCode::UNAUTHORIZED,
            "not authorized",
        );
        assert!(userinfo.contains("GET /v2/userinfo"), "{userinfo}");

        let me = format_linkedin_http_error(
            "GET /v2/me",
            reqwest::StatusCode::BAD_REQUEST,
            "bad request",
        );
        assert!(me.contains("GET /v2/me"), "{me}");
    }

    #[test]
    fn test_share_payload_structure() {
        let payload = json!({
            "owner": "urn:li:person:abc123",
            "text": { "text": "myapp v1.0 released" },
            "distribution": { "linkedInDistributionTarget": {} }
        });
        assert_eq!(payload["owner"], "urn:li:person:abc123");
        assert_eq!(payload["text"]["text"], "myapp v1.0 released");
        assert!(payload["distribution"]["linkedInDistributionTarget"].is_object());
    }

    #[test]
    fn token_shape_accepts_jwt_format() {
        let jwt = "eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiIxMjMifQ.signature_blob_padded";
        validate_token_shape(jwt).unwrap();
    }

    #[test]
    fn token_shape_accepts_opaque_token() {
        validate_token_shape("AQXopaque_long_alphanumeric_token_value_1234567890abcdef").unwrap();
    }

    #[test]
    fn token_shape_rejects_too_short() {
        let err = validate_token_shape("abc").unwrap_err().to_string();
        assert!(err.contains("too short"), "{err}");
    }

    #[test]
    fn token_shape_rejects_two_segments() {
        let err = validate_token_shape("abcdefghijklmnop.qrstuvwxyz")
            .unwrap_err()
            .to_string();
        assert!(err.contains("dot-separated segments"), "{err}");
    }

    #[test]
    fn token_shape_rejects_jwt_with_empty_segment() {
        let err = validate_token_shape("eyJhbGciOiJIUzI1NiJ9..signature_blob_padded")
            .unwrap_err()
            .to_string();
        assert!(err.contains("segment"), "{err}");
    }

    #[test]
    fn token_shape_rejects_jwt_with_non_base64url_segment() {
        // Segment 2 contains a '@' which is not in the base64url alphabet
        // (A-Z, a-z, 0-9, '-', '_', '='). Catching this early beats a
        // server-side 401 with no actionable signal.
        let bad = "eyJhbGciOiJIUzI1NiJ9.bad@segment.signature_blob_padded";
        let err = validate_token_shape(bad).unwrap_err().to_string();
        assert!(err.contains("non-base64url"), "{err}");
    }

    #[test]
    fn token_shape_rejects_jwt_with_more_than_three_segments() {
        let err = validate_token_shape("aaaaaaaaaaaaaaaaaa.b.c.d.e")
            .unwrap_err()
            .to_string();
        assert!(err.contains("5"), "{err}");
        assert!(err.contains("dot-separated"), "{err}");
    }

    #[test]
    fn http_error_handles_empty_body() {
        let msg =
            format_linkedin_http_error("share", reqwest::StatusCode::INTERNAL_SERVER_ERROR, "");
        assert!(msg.contains("share"), "{msg}");
        assert!(msg.contains("500"), "{msg}");
    }

    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use anodizer_core::log::{StageLogger, Verbosity};
    use anodizer_core::retry::RetryPolicy;
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder_with;

    use super::send_linkedin_to;

    const USERINFO_OK: &str = r#"{"sub":"abc123"}"#;
    const ME_OK: &str = r#"{"id":"legacy456"}"#;
    const SHARE_OK: &str = r#"{"activity":"urn:li:activity:6543"}"#;

    fn http(status_line: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    fn fast_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 2,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
        }
    }

    /// Drive the full share flow against a local responder serving the given
    /// responses in connection order. Returns the outcome and the number of
    /// canned responses actually consumed.
    fn run_share(responses: Vec<String>) -> (Result<(), anyhow::Error>, u32) {
        let (addr, calls) = spawn_oneshot_http_responder_with(|_| responses);
        let log = StageLogger::new("announce", Verbosity::Quiet);
        let result = send_linkedin_to(
            &format!("http://{addr}"),
            "opaque_token_value_123456",
            "myapp v1.0 released",
            &log,
            &fast_policy(),
        );
        (result, calls.load(Ordering::SeqCst))
    }

    #[test]
    fn share_happy_path_resolves_urn_then_posts() {
        let (result, calls) =
            run_share(vec![http("200 OK", USERINFO_OK), http("200 OK", SHARE_OK)]);
        result.expect("share must succeed");
        assert_eq!(calls, 2, "exactly userinfo + share");
    }

    #[test]
    fn share_userinfo_403_falls_back_to_legacy_me() {
        let (result, calls) = run_share(vec![
            http("403 Forbidden", ""),
            http("200 OK", ME_OK),
            http("200 OK", SHARE_OK),
        ]);
        result.expect("403 on /v2/userinfo must fall back to /v2/me");
        assert_eq!(calls, 3);
    }

    #[test]
    fn share_me_403_surfaces_permission_error() {
        let (result, calls) = run_share(vec![http("403 Forbidden", ""), http("403 Forbidden", "")]);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("forbidden"), "{err}");
        assert!(err.contains("permissions"), "{err}");
        assert_eq!(calls, 2);
    }

    #[test]
    fn share_userinfo_4xx_fast_fails_with_body() {
        let (result, calls) = run_share(vec![http(
            "401 Unauthorized",
            r#"{"message":"token expired","serviceErrorCode":65601}"#,
        )]);
        let err = format!("{:#}", result.unwrap_err());
        assert!(err.contains("GET /v2/userinfo"), "{err}");
        assert!(err.contains("401"), "{err}");
        assert!(err.contains("token expired"), "{err}");
        assert_eq!(calls, 1, "4xx must not retry");
    }

    #[test]
    fn share_userinfo_500_retries_then_succeeds() {
        let (result, calls) = run_share(vec![
            http("500 Internal Server Error", ""),
            http("200 OK", USERINFO_OK),
            http("200 OK", SHARE_OK),
        ]);
        result.expect("5xx on userinfo must retry");
        assert_eq!(calls, 3);
    }

    #[test]
    fn share_post_4xx_fast_fails_with_body() {
        let (result, calls) = run_share(vec![
            http("200 OK", USERINFO_OK),
            http(
                "422 Unprocessable Entity",
                r#"{"message":"duplicate share"}"#,
            ),
        ]);
        let err = format!("{:#}", result.unwrap_err());
        assert!(err.contains("share"), "{err}");
        assert!(err.contains("422"), "{err}");
        assert!(err.contains("duplicate share"), "{err}");
        assert_eq!(calls, 2);
    }

    #[test]
    fn share_post_500_retries_then_succeeds() {
        let (result, calls) = run_share(vec![
            http("200 OK", USERINFO_OK),
            http("500 Internal Server Error", ""),
            http("200 OK", SHARE_OK),
        ]);
        result.expect("5xx on share must retry");
        assert_eq!(calls, 3);
    }

    #[test]
    fn share_response_missing_activity_errors() {
        let (result, _) = run_share(vec![http("200 OK", USERINFO_OK), http("200 OK", "{}")]);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("could not find 'activity'"), "{err}");
    }

    #[test]
    fn share_response_invalid_json_errors() {
        let (result, _) = run_share(vec![
            http("200 OK", USERINFO_OK),
            http("200 OK", "<html>not json</html>"),
        ]);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("failed to parse share response"), "{err}");
    }

    #[test]
    fn share_userinfo_missing_sub_errors() {
        let (result, _) = run_share(vec![http("200 OK", "{}")]);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("missing 'sub'"), "{err}");
    }

    #[test]
    fn share_legacy_me_missing_id_errors() {
        let (result, _) = run_share(vec![http("403 Forbidden", ""), http("200 OK", "{}")]);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("missing 'id'"), "{err}");
    }

    #[test]
    fn share_legacy_me_4xx_fast_fails_with_body() {
        let (result, calls) = run_share(vec![
            http("403 Forbidden", ""),
            http("400 Bad Request", r#"{"message":"bad projection"}"#),
        ]);
        let err = format!("{:#}", result.unwrap_err());
        assert!(err.contains("GET /v2/me"), "{err}");
        assert!(err.contains("400"), "{err}");
        assert!(err.contains("bad projection"), "{err}");
        assert_eq!(calls, 2, "4xx on /v2/me must not retry");
    }

    #[test]
    fn share_legacy_me_500_retries_then_succeeds() {
        let (result, calls) = run_share(vec![
            http("403 Forbidden", ""),
            http("500 Internal Server Error", ""),
            http("200 OK", ME_OK),
            http("200 OK", SHARE_OK),
        ]);
        result.expect("5xx on /v2/me must retry");
        assert_eq!(calls, 4);
    }

    #[test]
    fn share_transport_error_is_classified_and_surfaced() {
        // Bind a listener to reserve a port, then drop it: the connect gets
        // ECONNREFUSED, which the userinfo arm classifies as a (retriable)
        // transport error and surfaces after attempts are exhausted.
        let addr = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        };
        let log = StageLogger::new("announce", Verbosity::Quiet);
        let err = send_linkedin_to(
            &format!("http://{addr}"),
            "opaque_token_value_123456",
            "msg",
            &log,
            &fast_policy(),
        )
        .unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("GET /v2/userinfo transport error"),
            "{chain}"
        );
    }

    /// `distribution.linkedInDistributionTarget` (camelCase, empty
    /// object) is the wire shape LinkedIn requires; the typo
    /// `linkedinDistributionTarget` is a 4xx-class regression.
    #[test]
    fn share_payload_distribution_field_is_camel_case() {
        let owner = "urn:li:person:abc";
        let payload = json!({
            "owner": owner,
            "text": { "text": "hello" },
            "distribution": { "linkedInDistributionTarget": {} }
        });
        let s = payload.to_string();
        assert!(
            s.contains("linkedInDistributionTarget"),
            "camelCase field: {s}"
        );
        assert!(!s.contains("linkedinDistributionTarget"), "wrong case: {s}");
    }
}
