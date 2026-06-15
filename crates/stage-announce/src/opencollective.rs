use std::ops::ControlFlow;

use anodizer_core::retry::{HttpError, RetryPolicy, is_retriable, retry_sync};
use anyhow::{Context as _, Result};
use serde_json::json;

/// Default OpenCollective GraphQL v2 endpoint.
const GRAPHQL_URL: &str = "https://api.opencollective.com/graphql/v2";

/// Resolve the GraphQL endpoint. Read at call time so tests can set
/// `ANODIZE_OPENCOLLECTIVE_API_BASE` to redirect the two-step mutation flow at
/// a local mock; production never sets the variable.
fn opencollective_graphql_base() -> String {
    std::env::var("ANODIZE_OPENCOLLECTIVE_API_BASE").unwrap_or_else(|_| GRAPHQL_URL.to_string())
}

pub const DEFAULT_TITLE_TEMPLATE: &str = "{{ Tag }}";
pub const DEFAULT_MESSAGE_TEMPLATE: &str = r#"{{ ProjectName }} {{ Tag }} is out!<br/>Check it out at <a href="{{ ReleaseURL }}">{{ ReleaseURL }}</a>"#;

/// Validate an OpenCollective collective slug. Slugs are lowercase
/// alphanumeric with hyphens, 1–48 characters, no leading/trailing hyphen
/// and no consecutive hyphens. Catching format errors here avoids a wasted
/// GraphQL round-trip for an unresolvable slug.
pub fn validate_slug(slug: &str) -> Result<()> {
    if slug.is_empty() || slug.len() > 48 {
        anyhow::bail!(
            "opencollective: slug {slug:?} must be 1–48 characters (got {})",
            slug.len()
        );
    }
    if slug.starts_with('-') || slug.ends_with('-') {
        anyhow::bail!("opencollective: slug {slug:?} must not start or end with '-'");
    }
    if slug.contains("--") {
        anyhow::bail!("opencollective: slug {slug:?} must not contain consecutive hyphens");
    }
    if !slug
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        anyhow::bail!(
            "opencollective: slug {slug:?} must contain only lowercase letters, digits, and hyphens"
        );
    }
    Ok(())
}

/// Loose check on the Personal-Token header value. OpenCollective tokens are
/// long opaque strings; reject anything obviously malformed (whitespace,
/// non-printable bytes, very short) so we surface the misconfiguration before
/// the API rejects us with an opaque 401.
pub fn validate_token_shape(token: &str) -> Result<()> {
    crate::util::validate_token_min_length("opencollective", "OPENCOLLECTIVE_TOKEN", token, 16)?;
    if token.chars().any(|c| c.is_whitespace() || c.is_control()) {
        anyhow::bail!(
            "opencollective: OPENCOLLECTIVE_TOKEN contains whitespace or control characters, \
             check for stray quotes or line wraps"
        );
    }
    Ok(())
}

const CREATE_QUERY: &str =
    r#"mutation($update: UpdateCreateInput!) { createUpdate(update: $update) { id } }"#;

const PUBLISH_QUERY: &str = r#"mutation($id: String!, $audience: UpdateAudience) { publishUpdate(id: $id, notificationAudience: $audience) { id } }"#;

pub(crate) fn build_create_body(slug: &str, title: &str, html: &str) -> serde_json::Value {
    json!({
        "query": CREATE_QUERY,
        "variables": {
            "update": {
                "title": title,
                "html": html,
                "account": { "slug": slug }
            }
        }
    })
}

pub(crate) fn build_publish_body(update_id: &str) -> serde_json::Value {
    json!({
        "query": PUBLISH_QUERY,
        "variables": {"id": update_id, "audience": "ALL"}
    })
}

/// Decode the GraphQL `errors` array from a response body and produce a
/// joined error message. Mirrors upstream `graphqlResponse.err()` (PR #6512):
/// GraphQL APIs return HTTP 200 even on mutation failures, so the caller
/// must inspect the `errors` array independently of the status code.
///
/// Returns `Ok(())` when the response carries no errors. Returns
/// `Err(message)` joining all error messages with `; ` so the surfaced
/// failure quotes every reason the server gave.
pub(crate) fn decode_graphql_errors(body: &serde_json::Value) -> Result<(), String> {
    let Some(errs) = body.get("errors").and_then(|v| v.as_array()) else {
        return Ok(());
    };
    if errs.is_empty() {
        return Ok(());
    }
    let msgs: Vec<String> = errs
        .iter()
        .filter_map(|e| e.get("message").and_then(|m| m.as_str()).map(String::from))
        .collect();
    if msgs.is_empty() {
        Err(format!("opencollective graphql error: {body}"))
    } else {
        Err(format!("opencollective graphql error: {}", msgs.join("; ")))
    }
}

/// Extract the `data.createUpdate.id` field from a createUpdate response,
/// raising the appropriate parity error when it is missing or empty.
///
/// Mirrors the upstream PR #6512 sequence:
///   1. If the body carries a non-empty `errors` array, surface it.
///   2. If `data.createUpdate.id` is absent, fail with "missing update ID".
///   3. If `data.createUpdate.id` is the empty string, fail with the upstream
///      message "opencollective returned empty update id".
pub(crate) fn extract_create_update_id(body: &serde_json::Value) -> Result<String, String> {
    decode_graphql_errors(body)?;
    let id = body
        .get("data")
        .and_then(|d| d.get("createUpdate"))
        .and_then(|c| c.get("id"))
        .and_then(|v| v.as_str());
    match id {
        None => Err("opencollective: missing update ID in createUpdate response".to_string()),
        Some("") => Err("opencollective returned empty update id".to_string()),
        Some(id) => Ok(id.to_string()),
    }
}

/// Categorise an OpenCollective HTTP response into a structured error.
///
/// Mirrors upstream commit 206120a (#6512): callers see distinct
/// messages for 401-unauthorized, 5xx-server-error, and other 4xx
/// rejections. GraphQL APIs return HTTP 200 even on mutation failures
/// (errors are in the response body, not the status code), so this only
/// classifies HTTP-level failures (`!status.is_success()`).
pub(crate) fn classify_opencollective_status(
    stage: &str,
    status: reqwest::StatusCode,
    body: &str,
) -> String {
    match status.as_u16() {
        401 => format!(
            "opencollective: {stage} unauthorized (401), check OPENCOLLECTIVE_TOKEN: {body}"
        ),
        403 => format!(
            "opencollective: {stage} forbidden (403), token lacks the required scope: {body}"
        ),
        s if (500..600).contains(&s) => format!(
            "opencollective: {stage} server error ({status}), upstream is unhealthy, retrying: {body}"
        ),
        _ => format!("opencollective: {stage} failed ({status}): {body}"),
    }
}

/// Single-shot HTTP POST with retry + categorised error wrapping.
fn do_mutation(
    client: &reqwest::blocking::Client,
    stage: &str,
    token: &str,
    body_payload: String,
    policy: &RetryPolicy,
) -> Result<String> {
    retry_sync(policy, |_attempt| {
        match client
            .post(opencollective_graphql_base())
            .header("Personal-Token", token)
            .header("Content-Type", "application/json")
            .body(body_payload.clone())
            .send()
        {
            Err(e) => {
                let err = anyhow::Error::new(HttpError::from_response(e, None))
                    .context(format!("opencollective: {stage} transport error"));
                if is_retriable(err.as_ref()) {
                    Err(ControlFlow::Continue(err))
                } else {
                    Err(ControlFlow::Break(err))
                }
            }
            Ok(resp) => {
                let status = resp.status();
                let body = resp
                    .text()
                    .unwrap_or_else(|e| format!("<body read failed: {e}>"));
                if status.is_success() {
                    Ok(body)
                } else {
                    let msg = classify_opencollective_status(stage, status, &body);
                    let wrapped = anyhow::Error::new(HttpError::new(
                        std::io::Error::other(msg.clone()),
                        status.as_u16(),
                    ))
                    .context(msg);
                    if is_retriable(wrapped.as_ref()) {
                        Err(ControlFlow::Continue(wrapped))
                    } else {
                        Err(ControlFlow::Break(wrapped))
                    }
                }
            }
        }
    })
    .with_context(|| format!("opencollective: {stage} exhausted retry attempts"))
}

/// Create and publish an update on OpenCollective.
///
/// Two-step GraphQL flow:
/// 1. `createUpdate` mutation: creates a draft update with title and HTML body
/// 2. `publishUpdate` mutation: publishes the update to all collective members
///
/// Error categorisation mirrors upstream commit 206120a: 401, 5xx, and
/// other 4xx rejections all surface distinct messages, GraphQL `errors` arrays
/// are decoded and reported, and malformed JSON responses are caught with a
/// dedicated error rather than panicking.
///
/// Any non-empty GraphQL `errors` array on the createUpdate response aborts
/// before the publish step, matching upstream PR #6512's
/// TestCreateUpdateGraphqlError. Only a clean response with a non-empty `id`
/// proceeds to publishUpdate.
pub fn send_opencollective(
    token: &str,
    slug: &str,
    title: &str,
    html: &str,
    policy: &RetryPolicy,
) -> Result<()> {
    let client = reqwest::blocking::Client::new();

    let resp_text = do_mutation(
        &client,
        "createUpdate",
        token,
        build_create_body(slug, title, html).to_string(),
        policy,
    )?;
    let resp_json: serde_json::Value = serde_json::from_str(&resp_text).with_context(|| {
        format!("opencollective: createUpdate response was not valid JSON: {resp_text}")
    })?;
    let update_id = extract_create_update_id(&resp_json).map_err(|e| anyhow::anyhow!(e))?;

    let publish_text = do_mutation(
        &client,
        "publishUpdate",
        token,
        build_publish_body(&update_id).to_string(),
        policy,
    )?;
    let publish_json: serde_json::Value =
        serde_json::from_str(&publish_text).with_context(|| {
            format!("opencollective: publishUpdate response was not valid JSON: {publish_text}")
        })?;
    decode_graphql_errors(&publish_json).map_err(|e| anyhow::anyhow!(e))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_create_body_shape() {
        let body = build_create_body("my-project", "v1.0.0", "Project v1.0.0 is out!");
        assert_eq!(body["query"], CREATE_QUERY);
        assert_eq!(body["variables"]["update"]["account"]["slug"], "my-project");
        assert_eq!(body["variables"]["update"]["title"], "v1.0.0");
        assert!(
            body["variables"]["update"]["html"]
                .as_str()
                .unwrap()
                .contains("is out!")
        );
    }

    #[test]
    fn test_build_publish_body_shape() {
        let body = build_publish_body("UPD-123");
        assert_eq!(body["query"], PUBLISH_QUERY);
        assert_eq!(body["variables"]["id"], "UPD-123");
        assert_eq!(body["variables"]["audience"], "ALL");
    }

    #[test]
    fn slug_accepts_well_formed() {
        validate_slug("my-project").unwrap();
        validate_slug("opensource").unwrap();
        validate_slug("a1-b2-c3").unwrap();
    }

    #[test]
    fn slug_rejects_bad_format() {
        assert!(validate_slug("").is_err());
        assert!(validate_slug("-leading").is_err());
        assert!(validate_slug("trailing-").is_err());
        assert!(validate_slug("double--hyphen").is_err());
        assert!(validate_slug("UpperCase").is_err());
        assert!(validate_slug("under_score").is_err());
        assert!(validate_slug(&"x".repeat(49)).is_err());
    }

    #[test]
    fn token_shape_accepts_long_opaque() {
        validate_token_shape(&"a".repeat(64)).unwrap();
    }

    #[test]
    fn token_shape_rejects_short() {
        assert!(validate_token_shape("short").is_err());
    }

    #[test]
    fn token_shape_rejects_whitespace() {
        let err = validate_token_shape("token with spaces inside it 123456789012345")
            .unwrap_err()
            .to_string();
        assert!(err.contains("whitespace"), "{err}");
    }

    // -----------------------------------------------------------------------
    // GraphQL error decoding regression tests
    //
    // Mirrors upstream PR #6512 test cases:
    //   - TestGraphqlResponseErr (no errors / single / multiple)
    //   - TestCreateUpdateGraphqlError
    //   - TestCreateUpdateEmptyID
    //   - TestPublishUpdateGraphqlError
    //   - TestNonOKStatus (classify_opencollective_status covers this)
    // -----------------------------------------------------------------------

    #[test]
    fn graphql_response_no_errors_is_ok() {
        let body = serde_json::json!({"data": {"createUpdate": {"id": "abc"}}});
        assert!(decode_graphql_errors(&body).is_ok());
    }

    #[test]
    fn graphql_response_empty_errors_array_is_ok() {
        let body = serde_json::json!({"errors": []});
        assert!(decode_graphql_errors(&body).is_ok());
    }

    #[test]
    fn graphql_response_single_error_joins_message() {
        let body = serde_json::json!({"errors": [{"message": "not authorized"}]});
        let err = decode_graphql_errors(&body).unwrap_err();
        assert_eq!(err, "opencollective graphql error: not authorized");
    }

    #[test]
    fn graphql_response_multiple_errors_joined_with_semicolon() {
        let body = serde_json::json!({"errors": [
            {"message": "not authorized"},
            {"message": "invalid slug"},
        ]});
        let err = decode_graphql_errors(&body).unwrap_err();
        assert_eq!(
            err,
            "opencollective graphql error: not authorized; invalid slug"
        );
    }

    #[test]
    fn create_update_graphql_error_surfaces() {
        // Upstream TestCreateUpdateGraphqlError: HTTP 200 + errors body.
        let body = serde_json::json!({
            "errors": [{"message": "You need to be logged in as an admin of this collective"}],
            "data": {"createUpdate": null},
        });
        let err = extract_create_update_id(&body).unwrap_err();
        assert_eq!(
            err,
            "opencollective graphql error: You need to be logged in as an admin of this collective"
        );
    }

    #[test]
    fn create_update_empty_id_surfaces() {
        // Upstream TestCreateUpdateEmptyID: HTTP 200 + empty id + no errors.
        let body = serde_json::json!({"data": {"createUpdate": {"id": ""}}});
        let err = extract_create_update_id(&body).unwrap_err();
        assert_eq!(err, "opencollective returned empty update id");
    }

    #[test]
    fn create_update_valid_id_returned() {
        let body = serde_json::json!({"data": {"createUpdate": {"id": "UPD-123"}}});
        let id = extract_create_update_id(&body).unwrap();
        assert_eq!(id, "UPD-123");
    }

    #[test]
    fn create_update_missing_id_surfaces() {
        let body = serde_json::json!({"data": {"createUpdate": {}}});
        let err = extract_create_update_id(&body).unwrap_err();
        assert!(
            err.contains("missing update ID"),
            "expected 'missing update ID' marker in: {err}"
        );
    }

    #[test]
    fn publish_update_graphql_error_surfaces() {
        // Upstream TestPublishUpdateGraphqlError.
        let body = serde_json::json!({"errors": [{"message": "Update not found"}]});
        let err = decode_graphql_errors(&body).unwrap_err();
        assert_eq!(err, "opencollective graphql error: Update not found");
    }

    #[test]
    fn classify_status_401_mentions_token() {
        let msg = classify_opencollective_status(
            "createUpdate",
            reqwest::StatusCode::UNAUTHORIZED,
            "Unauthorized",
        );
        assert!(msg.contains("401"), "{msg}");
        assert!(msg.contains("OPENCOLLECTIVE_TOKEN"), "{msg}");
        assert!(msg.contains("Unauthorized"), "{msg}");
    }

    #[test]
    fn classify_status_5xx_marks_retriable_context() {
        let msg = classify_opencollective_status(
            "createUpdate",
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "boom",
        );
        assert!(
            msg.contains("server error") || msg.contains("retrying"),
            "expected 5xx framing in: {msg}"
        );
        assert!(msg.contains("boom"), "body must be included: {msg}");
    }

    // ---- live send over a mock HTTP server -----------------------------
    //
    // Drive `send_opencollective` (the real two-step createUpdate →
    // publishUpdate GraphQL POST path) against a scripted responder via the
    // `ANODIZE_OPENCOLLECTIVE_API_BASE` seam. Mutating process env requires
    // `#[serial]` + the shared env_mutex.

    use anodizer_core::test_helpers::env::env_mutex;
    use anodizer_core::test_helpers::scripted_responder::{
        ScriptedRoute, spawn_scripted_responder,
    };

    const ONE_SHOT: RetryPolicy = RetryPolicy {
        max_attempts: 1,
        base_delay: std::time::Duration::from_millis(0),
        max_delay: std::time::Duration::from_millis(0),
    };

    struct EnvGuard;
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var("ANODIZE_OPENCOLLECTIVE_API_BASE") };
        }
    }
    fn set_base(addr: std::net::SocketAddr) -> EnvGuard {
        unsafe {
            std::env::set_var(
                "ANODIZE_OPENCOLLECTIVE_API_BASE",
                format!("http://{addr}/graphql/v2"),
            )
        };
        EnvGuard
    }
    fn http_response(status_line: &str, body: &str) -> &'static str {
        let resp = format!(
            "{status_line}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        Box::leak(resp.into_boxed_str())
    }

    #[test]
    #[serial_test::serial]
    fn send_opencollective_two_step_flow_posts_create_then_publish() {
        let _g = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        // Both mutations POST to the same path; `times: Some(1)` on the first
        // route serves createUpdate then exhausts, so the second request
        // falls through to the publishUpdate route.
        let (addr, log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "POST",
                path_pattern: "/graphql/v2",
                response: http_response(
                    "HTTP/1.1 200 OK",
                    "{\"data\":{\"createUpdate\":{\"id\":\"UPD-9\"}}}",
                ),
                times: Some(1),
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/graphql/v2",
                response: http_response(
                    "HTTP/1.1 200 OK",
                    "{\"data\":{\"publishUpdate\":{\"id\":\"UPD-9\"}}}",
                ),
                times: None,
            },
        ]);
        let _base = set_base(addr);

        send_opencollective(
            "tok-aaaaaaaaaaaaaaaa",
            "my-project",
            "MyApp v1.2.3",
            "MyApp v1.2.3 is out! see https://example.com/r",
            &ONE_SHOT,
        )
        .expect("two-step flow should succeed");

        let entries = log.lock().unwrap();
        assert_eq!(entries.len(), 2, "createUpdate + publishUpdate expected");

        // Leg 1: createUpdate — auth header + templated title/html on the wire.
        let create = &entries[0];
        assert_eq!(create.method, "POST");
        assert_eq!(create.path, "/graphql/v2");
        assert_eq!(
            create.header("personal-token"),
            Some("tok-aaaaaaaaaaaaaaaa")
        );
        assert_eq!(create.header("content-type"), Some("application/json"));
        let cbody: serde_json::Value = serde_json::from_str(&create.body).expect("json body");
        assert!(
            cbody["query"].as_str().unwrap().contains("createUpdate"),
            "create query: {}",
            create.body
        );
        let update = &cbody["variables"]["update"];
        assert_eq!(update["title"], "MyApp v1.2.3");
        assert_eq!(
            update["html"],
            "MyApp v1.2.3 is out! see https://example.com/r"
        );
        assert_eq!(update["account"]["slug"], "my-project");

        // Leg 2: publishUpdate — carries the id returned by leg 1.
        let publish = &entries[1];
        let pbody: serde_json::Value = serde_json::from_str(&publish.body).expect("json body");
        assert!(
            pbody["query"].as_str().unwrap().contains("publishUpdate"),
            "publish query: {}",
            publish.body
        );
        assert_eq!(pbody["variables"]["id"], "UPD-9");
        assert_eq!(pbody["variables"]["audience"], "ALL");
    }

    #[test]
    #[serial_test::serial]
    fn send_opencollective_create_401_aborts_before_publish() {
        let _g = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/graphql/v2",
            response: http_response("HTTP/1.1 401 Unauthorized", "Unauthorized"),
            times: None,
        }]);
        let _base = set_base(addr);

        let err = format!(
            "{:#}",
            send_opencollective("tok-aaaaaaaaaaaaaaaa", "my-project", "t", "h", &ONE_SHOT)
                .unwrap_err()
        );
        assert!(err.contains("401"), "status must surface: {err}");
        assert!(
            err.contains("OPENCOLLECTIVE_TOKEN"),
            "401 must name the token env var: {err}"
        );
        // A failed createUpdate must NOT fire publishUpdate.
        let entries = log.lock().unwrap();
        assert_eq!(entries.len(), 1, "publish must not fire after create 401");
    }

    #[test]
    #[serial_test::serial]
    fn send_opencollective_create_graphql_errors_aborts_before_publish() {
        let _g = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        // HTTP 200 + a non-empty `errors` array on createUpdate must abort.
        let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/graphql/v2",
            response: http_response(
                "HTTP/1.1 200 OK",
                "{\"errors\":[{\"message\":\"need admin\"}],\"data\":{\"createUpdate\":null}}",
            ),
            times: None,
        }]);
        let _base = set_base(addr);

        let err = format!(
            "{:#}",
            send_opencollective("tok-aaaaaaaaaaaaaaaa", "my-project", "t", "h", &ONE_SHOT)
                .unwrap_err()
        );
        assert!(
            err.contains("need admin"),
            "graphql error must surface: {err}"
        );
        let entries = log.lock().unwrap();
        assert_eq!(
            entries.len(),
            1,
            "publish must not fire after create graphql error"
        );
    }

    #[test]
    fn classify_status_other_4xx_includes_body() {
        let msg = classify_opencollective_status(
            "publishUpdate",
            reqwest::StatusCode::BAD_REQUEST,
            "bad request body",
        );
        assert!(msg.contains("400"), "{msg}");
        assert!(msg.contains("bad request body"), "{msg}");
    }
}
