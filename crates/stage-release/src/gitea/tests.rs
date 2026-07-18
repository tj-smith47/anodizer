use super::*;

// -- gitea_release_url --------------------------------------------------

#[test]
fn release_url_basic() {
    let url = gitea_release_url("https://gitea.example.com", "myorg", "myapp", "v1.0.0");
    assert_eq!(
        url,
        "https://gitea.example.com/myorg/myapp/releases/tag/v1.0.0"
    );
}

#[test]
fn release_url_trailing_slash_stripped() {
    let url = gitea_release_url("https://gitea.example.com/", "org", "repo", "v2.0.0");
    assert_eq!(
        url,
        "https://gitea.example.com/org/repo/releases/tag/v2.0.0"
    );
}

#[test]
fn release_url_special_chars_in_tag() {
    let url = gitea_release_url(
        "https://gitea.example.com",
        "myorg",
        "myapp",
        "v1.0.0+build.1",
    );
    assert_eq!(
        url,
        "https://gitea.example.com/myorg/myapp/releases/tag/v1.0.0%2Bbuild.1"
    );
}

#[test]
fn release_url_special_chars_in_owner_and_repo() {
    let url = gitea_release_url("https://gitea.example.com", "my org", "my repo", "v1.0.0");
    assert!(url.contains("my%20org"), "owner should be percent-encoded");
    assert!(url.contains("my%20repo"), "repo should be percent-encoded");
}

// -- encode_segment -----------------------------------------------------

#[test]
fn encode_segment_simple() {
    assert_eq!(encode_segment("v1.0.0"), "v1.0.0");
}

#[test]
fn encode_segment_with_plus() {
    assert_eq!(encode_segment("v1.0.0+build.1"), "v1.0.0%2Bbuild.1");
}

#[test]
fn encode_segment_with_special_chars() {
    assert_eq!(encode_segment("v1 beta#2?rc"), "v1%20beta%232%3Frc");
}

#[test]
fn encode_segment_preserves_dots_dashes_underscores() {
    assert_eq!(encode_segment("my-project_v2.0"), "my-project_v2.0");
}

// -- build_gitea_client -------------------------------------------------

#[test]
fn build_client_normal() {
    let client = build_gitea_client("giteatok-xxxx", false);
    assert!(client.is_ok());
}

#[test]
fn build_client_skip_tls() {
    let client = build_gitea_client("giteatok-xxxx", true);
    assert!(client.is_ok());
}

// -- Gitea auth header format -------------------------------------------

#[test]
fn gitea_auth_header_format() {
    // A normal token forms a valid `token <value>` Authorization header, so
    // the client builds.
    assert!(build_gitea_client("my-gitea-token", false).is_ok());

    // A token carrying a control character cannot form a valid header value;
    // build_gitea_client must surface that as an error (with its context)
    // rather than panic on the internal HeaderValue::from_str.
    let err = build_gitea_client("bad\ntoken", false).unwrap_err();
    assert!(
        format!("{err:#}").contains("invalid token value"),
        "a control-char token must surface the Authorization header error: {err:#}"
    );
}

// -- gitea_create_release retry behaviour (P1.4) -------------------------
//
// Pin: a 503 on the find-release-by-tag GET must retry through
// `retry_http_async` rather than fast-fail. Mirrors the gitlab equivalent
// and the core retry::tests::retry_http_async_retries_5xx_then_succeeds
// test, but exercises the policy plumbing end-to-end at the publisher.

use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

#[tokio::test]
async fn gitea_create_release_retries_5xx_on_list_releases() {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    // Sequence: 503 on the GET releases list, then 200 with an empty
    // array (release does not exist), then 201 on the POST create with
    // a fake id. The retry helper should retry past the 503 and the
    // create succeeds.
    let (addr, calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]",
        "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: 9\r\n\r\n{\"id\":42}",
    ]);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");
    let policy = RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    };
    let api_url = format!("http://{addr}");

    let ctx = GiteaCtx {
        client: &client,
        api_url: &api_url,
        owner: "myorg",
        repo: "myrepo",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let spec = GiteaReleaseSpec {
        tag: "v1.0.0",
        commit: "abc123",
        name: "Release v1.0.0",
        body: "release body",
        draft: false,
        prerelease: false,
        release_mode: "replace",
    };
    let result = gitea_create_release(&ctx, &spec).await;

    match result {
        Ok(id) => assert_eq!(id, 42, "release id should be parsed from create response"),
        Err(e) => panic!("expected success after 5xx retry, got: {e:#}"),
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "expected 3 connections (503-retry GET, 200 GET, 201 POST)"
    );
}

/// Defense-in-depth: a Gitea API 4xx response that echoes our
/// `Authorization: Bearer <PAT>` header back must not leak the token
/// into the user-visible error chain. Exercises the
/// `find_release_by_tag` GET error path on the 401-fast-fail path.
/// All gitea.rs body-interpolation sites share the same redaction wrap.
#[tokio::test]
async fn gitea_create_release_redacts_bearer_in_error_body() {
    use std::time::Duration;

    let leaky =
        r#"{"message":"401 Unauthorized: Authorization: Bearer ghp_FAKETOKEN1234567890abcdefg"}"#;
    let body_len = leaky.len();
    let resp: &'static str = Box::leak(
            format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {body_len}\r\n\r\n{leaky}"
            )
            .into_boxed_str(),
        );
    let (addr, _calls) = spawn_oneshot_http_responder(vec![resp]);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");
    let policy = RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    };
    let api_url = format!("http://{addr}");

    let ctx = GiteaCtx {
        client: &client,
        api_url: &api_url,
        owner: "myorg",
        repo: "myrepo",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let spec = GiteaReleaseSpec {
        tag: "v1.0.0",
        commit: "abc123",
        name: "Release v1.0.0",
        body: "release body",
        draft: false,
        prerelease: false,
        release_mode: "replace",
    };
    let err = gitea_create_release(&ctx, &spec)
        .await
        .expect_err("401 must fast-fail");
    let chain = format!("{err:#}");
    assert!(
        !chain.contains("ghp_FAKETOKEN1234567890abcdefg"),
        "bearer token leaked into error chain: {chain}"
    );
    assert!(
        chain.contains("<redacted>"),
        "expected `<redacted>` marker in error chain: {chain}"
    );
}

#[tokio::test]
async fn gitea_release_tag_empty_bails_with_actionable_error() {
    // Gitea's `POST /repos/{owner}/{repo}/releases` rejects empty
    // `tag_name` with a vague 422; the helper must bail upfront
    // (before listing existing releases) so users see the real
    // cause. Bail message must name owner/repo and include an
    // actionable hint about the snapshot/template state.
    use std::time::Duration;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");
    let policy = RetryPolicy {
        max_attempts: 1,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    };
    let ctx = GiteaCtx {
        client: &client,
        api_url: "http://unused.invalid",
        owner: "myorg",
        repo: "myrepo",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let spec = GiteaReleaseSpec {
        tag: "",
        commit: "abc123",
        name: "Release",
        body: "body",
        draft: false,
        prerelease: false,
        release_mode: "replace",
    };
    let err = gitea_create_release(&ctx, &spec)
        .await
        .expect_err("empty tag must bail before any HTTP call");
    let chain = format!("{err:#}");
    assert!(
        chain.contains("gitea:"),
        "error must carry the gitea: prefix, got: {chain}"
    );
    assert!(
        chain.contains("tag_name"),
        "error must name the rejected field, got: {chain}"
    );
    assert!(
        chain.contains("myorg/myrepo"),
        "error must name the owner/repo, got: {chain}"
    );
    assert!(
        chain.contains("release.tag:") || chain.contains("snapshot"),
        "error must include an actionable hint, got: {chain}"
    );
}

#[tokio::test]
async fn gitea_release_commit_empty_bails_with_actionable_error() {
    // Gitea's create endpoint uses `target_commitish` to create the
    // tag when it doesn't already exist; empty values surface as a
    // 422. Bail upfront so users see that `ctx.git_info.commit` was
    // not populated.
    use std::time::Duration;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");
    let policy = RetryPolicy {
        max_attempts: 1,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    };
    let ctx = GiteaCtx {
        client: &client,
        api_url: "http://unused.invalid",
        owner: "myorg",
        repo: "myrepo",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let spec = GiteaReleaseSpec {
        tag: "v1.0.0",
        commit: "",
        name: "Release",
        body: "body",
        draft: false,
        prerelease: false,
        release_mode: "replace",
    };
    let err = gitea_create_release(&ctx, &spec)
        .await
        .expect_err("empty commit must bail before any HTTP call");
    let chain = format!("{err:#}");
    assert!(
        chain.contains("gitea:"),
        "error must carry the gitea: prefix, got: {chain}"
    );
    assert!(
        chain.contains("target_commitish"),
        "error must name the rejected field, got: {chain}"
    );
    assert!(
        chain.contains("git working tree") || chain.contains("git_info"),
        "error must include an actionable hint, got: {chain}"
    );
}

// -- HTTP release flow against the scripted responder -------------------
//
// The flat `spawn_oneshot_http_responder` above serves responses in
// arrival order regardless of URL; it cannot assert WHICH endpoint each
// call hit. These tests instead use the route-aware
// `spawn_scripted_responder`, point `GiteaCtx.api_url` at
// `http://{addr}`, and assert on the recorded request log: the exact
// method/path/body of every create, find, update, upload, list, and
// delete call the backend issues against Gitea's `/api/v1/...` surface.

use anodizer_core::test_helpers::scripted_responder::{
    ScriptedRoute, spawn_scripted_responder, spawn_scripted_responder_on,
};

/// Build a fast retry policy for the HTTP-flow tests (millisecond
/// backoff so a retried 5xx doesn't stall the suite).
fn fast_policy(max_attempts: u32) -> RetryPolicy {
    RetryPolicy {
        max_attempts,
        base_delay: std::time::Duration::from_millis(1),
        max_delay: std::time::Duration::from_millis(2),
    }
}

/// Build a reqwest client with a short timeout for the HTTP-flow tests.
fn test_client() -> Client {
    Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .expect("client")
}

/// Wrap a JSON body in a `200 OK` response with the right
/// `Content-Length`. Leaks because the responder needs `&'static str`.
fn http_json(status: &str, body: String) -> &'static str {
    let len = body.len();
    Box::leak(
            format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {len}\r\n\r\n{body}"
            )
            .into_boxed_str(),
        )
}

// -- gitea_create_release: create path (no existing release) ------------

/// With no existing release on the first listing page, the backend
/// POSTs to `.../releases` with the full create payload and parses the
/// numeric `id` out of the 201 response.
#[tokio::test]
async fn create_release_posts_when_absent() {
    let (addr, log) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/api/v1/repos/myorg/myrepo/releases?page=1&limit=50",
            response: "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]",
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/api/v1/repos/myorg/myrepo/releases",
            response: http_json("201 Created", serde_json::json!({"id": 99}).to_string()),
            times: None,
        },
    ]);

    let client = test_client();
    let policy = fast_policy(2);
    let api_url = format!("http://{addr}");
    let ctx = GiteaCtx {
        client: &client,
        api_url: &api_url,
        owner: "myorg",
        repo: "myrepo",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let spec = GiteaReleaseSpec {
        tag: "v1.0.0",
        commit: "deadbeef",
        name: "Release v1.0.0",
        body: "the body",
        draft: true,
        prerelease: true,
        release_mode: "replace",
    };

    let id = gitea_create_release(&ctx, &spec)
        .await
        .expect("create should succeed");
    assert_eq!(id, 99, "release id parsed from POST 201 response");

    let entries = log.lock().unwrap();
    assert_eq!(entries.len(), 2, "one GET list + one POST create");
    assert_eq!(entries[0].method, "GET");
    assert_eq!(entries[1].method, "POST");
    assert_eq!(
        entries[1].path, "/api/v1/repos/myorg/myrepo/releases",
        "create POSTs to the un-suffixed releases endpoint"
    );
    let payload: serde_json::Value =
        serde_json::from_str(&entries[1].body).expect("POST body is JSON");
    assert_eq!(payload["tag_name"], "v1.0.0");
    assert_eq!(payload["target_commitish"], "deadbeef");
    assert_eq!(payload["name"], "Release v1.0.0");
    assert_eq!(payload["body"], "the body");
    assert_eq!(payload["draft"], true);
    assert_eq!(payload["prerelease"], true);
}

/// A 422 from the create POST surfaces as an error (not a retry —
/// `max_attempts: 1` proves the 4xx is fast-failed) and carries the
/// gitea create-release context.
#[tokio::test]
async fn create_release_surfaces_422() {
    let (addr, log) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/api/v1/repos/o/r/releases?page=1&limit=50",
            response: "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]",
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/api/v1/repos/o/r/releases",
            response: http_json(
                "422 Unprocessable Entity",
                serde_json::json!({"message": "tag already exists"}).to_string(),
            ),
            times: None,
        },
    ]);

    let client = test_client();
    let policy = fast_policy(1);
    let api_url = format!("http://{addr}");
    let ctx = GiteaCtx {
        client: &client,
        api_url: &api_url,
        owner: "o",
        repo: "r",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let spec = GiteaReleaseSpec {
        tag: "v1.0.0",
        commit: "abc",
        name: "rel",
        body: "b",
        draft: false,
        prerelease: false,
        release_mode: "replace",
    };

    let err = gitea_create_release(&ctx, &spec)
        .await
        .expect_err("422 must surface as an error");
    let chain = format!("{err:#}");
    assert!(
        chain.contains("create release failed (HTTP 422"),
        "error must name the failing create call + status, got: {chain}"
    );
    let entries = log.lock().unwrap();
    assert_eq!(entries.len(), 2, "GET list + single POST (no retry on 4xx)");
}

/// A 503 on the POST create retries through `retry_http_async` and then
/// succeeds on the second attempt; the request log records both POSTs.
#[tokio::test]
async fn create_release_retries_5xx_on_post() {
    let (addr, log) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/api/v1/repos/o/r/releases?page=1&limit=50",
            response: "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]",
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/api/v1/repos/o/r/releases",
            response: "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
            times: Some(1),
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/api/v1/repos/o/r/releases",
            response: http_json("201 Created", serde_json::json!({"id": 7}).to_string()),
            times: None,
        },
    ]);

    let client = test_client();
    let policy = fast_policy(3);
    let api_url = format!("http://{addr}");
    let ctx = GiteaCtx {
        client: &client,
        api_url: &api_url,
        owner: "o",
        repo: "r",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let spec = GiteaReleaseSpec {
        tag: "v1.0.0",
        commit: "abc",
        name: "rel",
        body: "b",
        draft: false,
        prerelease: false,
        release_mode: "replace",
    };

    let id = gitea_create_release(&ctx, &spec)
        .await
        .expect("create should succeed after 5xx retry");
    assert_eq!(id, 7);
    let entries = log.lock().unwrap();
    let posts = entries.iter().filter(|e| e.method == "POST").count();
    assert_eq!(posts, 2, "503 POST retried once, then 201");
}

/// The create-response JSON missing an `id` field surfaces an
/// explicit parse error rather than silently returning 0.
#[tokio::test]
async fn create_release_missing_id_errors() {
    let (addr, _log) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/api/v1/repos/o/r/releases?page=1&limit=50",
            response: "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]",
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/api/v1/repos/o/r/releases",
            response: http_json(
                "201 Created",
                serde_json::json!({"name": "rel"}).to_string(),
            ),
            times: None,
        },
    ]);

    let client = test_client();
    let policy = fast_policy(1);
    let api_url = format!("http://{addr}");
    let ctx = GiteaCtx {
        client: &client,
        api_url: &api_url,
        owner: "o",
        repo: "r",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let spec = GiteaReleaseSpec {
        tag: "v1.0.0",
        commit: "abc",
        name: "rel",
        body: "b",
        draft: false,
        prerelease: false,
        release_mode: "replace",
    };

    let err = gitea_create_release(&ctx, &spec)
        .await
        .expect_err("missing id must error");
    assert!(
        format!("{err:#}").contains("missing 'id' field"),
        "error must name the missing id field, got: {err:#}"
    );
}

// -- gitea_create_release: update path (existing release) ---------------

/// When a release with the tag already exists, the backend PATCHes its
/// numeric id and returns that id — no POST create is issued. The
/// `replace` mode sends the new body verbatim.
#[tokio::test]
async fn update_release_patches_existing_replace_mode() {
    let existing = serde_json::json!([
        {"id": 5, "tag_name": "v1.0.0", "body": "old body"}
    ])
    .to_string();
    let (addr, log) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/api/v1/repos/o/r/releases?page=1&limit=50",
            response: http_json("200 OK", existing),
            times: None,
        },
        ScriptedRoute {
            method: "PATCH",
            path_pattern: "/api/v1/repos/o/r/releases/5",
            response: http_json("200 OK", serde_json::json!({"id": 5}).to_string()),
            times: None,
        },
    ]);

    let client = test_client();
    let policy = fast_policy(2);
    let api_url = format!("http://{addr}");
    let ctx = GiteaCtx {
        client: &client,
        api_url: &api_url,
        owner: "o",
        repo: "r",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let spec = GiteaReleaseSpec {
        tag: "v1.0.0",
        commit: "abc",
        name: "rel",
        body: "new body",
        draft: false,
        prerelease: false,
        release_mode: "replace",
    };

    let id = gitea_create_release(&ctx, &spec)
        .await
        .expect("update should succeed");
    assert_eq!(id, 5, "returns the existing release id");

    let entries = log.lock().unwrap();
    assert!(
        entries.iter().all(|e| e.method != "POST"),
        "existing release must be PATCHed, never POSTed"
    );
    let patch = entries
        .iter()
        .find(|e| e.method == "PATCH")
        .expect("a PATCH was issued");
    assert_eq!(patch.path, "/api/v1/repos/o/r/releases/5");
    let payload: serde_json::Value = serde_json::from_str(&patch.body).expect("PATCH body is JSON");
    assert_eq!(
        payload["body"], "new body",
        "replace mode sends the new body verbatim"
    );
}

/// The `append` release mode composes the existing body and the new
/// body into the PATCH payload (existing first, blank line, new).
#[tokio::test]
async fn update_release_append_mode_composes_body() {
    let existing = serde_json::json!([
        {"id": 8, "tag_name": "v2.0.0", "body": "EXISTING"}
    ])
    .to_string();
    let (addr, log) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/api/v1/repos/o/r/releases?page=1&limit=50",
            response: http_json("200 OK", existing),
            times: None,
        },
        ScriptedRoute {
            method: "PATCH",
            path_pattern: "/api/v1/repos/o/r/releases/8",
            response: http_json("200 OK", serde_json::json!({"id": 8}).to_string()),
            times: None,
        },
    ]);

    let client = test_client();
    let policy = fast_policy(2);
    let api_url = format!("http://{addr}");
    let ctx = GiteaCtx {
        client: &client,
        api_url: &api_url,
        owner: "o",
        repo: "r",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let spec = GiteaReleaseSpec {
        tag: "v2.0.0",
        commit: "abc",
        name: "rel",
        body: "ADDED",
        draft: false,
        prerelease: false,
        release_mode: "append",
    };

    gitea_create_release(&ctx, &spec)
        .await
        .expect("update should succeed");

    let entries = log.lock().unwrap();
    let patch = entries
        .iter()
        .find(|e| e.method == "PATCH")
        .expect("a PATCH was issued");
    let payload: serde_json::Value = serde_json::from_str(&patch.body).expect("PATCH body is JSON");
    assert_eq!(
        payload["body"], "EXISTING\n\nADDED",
        "append mode joins existing + new with a blank line"
    );
}

/// A 503 on the PATCH update retries and then succeeds.
#[tokio::test]
async fn update_release_retries_5xx_on_patch() {
    let existing = serde_json::json!([
        {"id": 3, "tag_name": "v1.0.0", "body": null}
    ])
    .to_string();
    let (addr, log) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/api/v1/repos/o/r/releases?page=1&limit=50",
            response: http_json("200 OK", existing),
            times: None,
        },
        ScriptedRoute {
            method: "PATCH",
            path_pattern: "/api/v1/repos/o/r/releases/3",
            response: "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
            times: Some(1),
        },
        ScriptedRoute {
            method: "PATCH",
            path_pattern: "/api/v1/repos/o/r/releases/3",
            response: http_json("200 OK", serde_json::json!({"id": 3}).to_string()),
            times: None,
        },
    ]);

    let client = test_client();
    let policy = fast_policy(3);
    let api_url = format!("http://{addr}");
    let ctx = GiteaCtx {
        client: &client,
        api_url: &api_url,
        owner: "o",
        repo: "r",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let spec = GiteaReleaseSpec {
        tag: "v1.0.0",
        commit: "abc",
        name: "rel",
        body: "b",
        draft: false,
        prerelease: false,
        release_mode: "replace",
    };

    let id = gitea_create_release(&ctx, &spec)
        .await
        .expect("update should succeed after 5xx retry");
    assert_eq!(id, 3);
    let entries = log.lock().unwrap();
    let patches = entries.iter().filter(|e| e.method == "PATCH").count();
    assert_eq!(patches, 2, "503 PATCH retried once, then 200");
}

// -- find_release_by_tag: pagination ------------------------------------

/// A full first page (50 entries) that does not contain the tag forces
/// a second GET; the match on page 2 returns its id + body without a
/// third page request.
#[tokio::test]
async fn find_release_paginates_to_second_page() {
    let mut page1: Vec<serde_json::Value> = Vec::new();
    for i in 0..50u64 {
        page1.push(serde_json::json!({
            "id": 1000 + i,
            "tag_name": format!("other-{i}"),
            "body": null,
        }));
    }
    let page1_body = serde_json::Value::Array(page1).to_string();
    let page2_body = serde_json::json!([
        {"id": 4242, "tag_name": "v9.9.9", "body": "found me"}
    ])
    .to_string();

    let (addr, log) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/api/v1/repos/o/r/releases?page=1&limit=50",
            response: http_json("200 OK", page1_body),
            times: None,
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/api/v1/repos/o/r/releases?page=2&limit=50",
            response: http_json("200 OK", page2_body),
            times: None,
        },
    ]);

    let client = test_client();
    let policy = fast_policy(2);
    let api_url = format!("http://{addr}");
    let found = find_release_by_tag(&client, &api_url, "o", "r", "v9.9.9", &policy, tlog())
        .await
        .expect("listing should succeed");
    assert_eq!(
        found,
        Some((4242, Some("found me".to_string()))),
        "tag matched on page 2 returns its id + body"
    );

    let entries = log.lock().unwrap();
    let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(
        paths,
        vec![
            "/api/v1/repos/o/r/releases?page=1&limit=50",
            "/api/v1/repos/o/r/releases?page=2&limit=50",
        ],
        "exactly two pages fetched, in order"
    );
}

/// A short first page (fewer than `PAGE_SIZE` entries) with no match
/// stops pagination and returns `None` — no second page is requested.
#[tokio::test]
async fn find_release_short_page_stops_and_returns_none() {
    let body = serde_json::json!([
        {"id": 1, "tag_name": "v0.1.0", "body": null}
    ])
    .to_string();
    let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "GET",
        path_pattern: "/api/v1/repos/o/r/releases?page=1&limit=50",
        response: http_json("200 OK", body),
        times: None,
    }]);

    let client = test_client();
    let policy = fast_policy(2);
    let api_url = format!("http://{addr}");
    let found = find_release_by_tag(&client, &api_url, "o", "r", "v2.0.0", &policy, tlog())
        .await
        .expect("listing should succeed");
    assert_eq!(found, None, "tag absent on a short page => None");

    let entries = log.lock().unwrap();
    assert_eq!(
        entries.len(),
        1,
        "a short first page must not trigger a second GET"
    );
}

/// A matched release object missing its `id` field surfaces an explicit
/// error from the listing parse rather than a silent skip.
#[tokio::test]
async fn find_release_missing_id_errors() {
    let body = serde_json::json!([
        {"tag_name": "v1.0.0", "body": "no id here"}
    ])
    .to_string();
    let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "GET",
        path_pattern: "/api/v1/repos/o/r/releases?page=1&limit=50",
        response: http_json("200 OK", body),
        times: None,
    }]);

    let client = test_client();
    let policy = fast_policy(1);
    let api_url = format!("http://{addr}");
    let err = find_release_by_tag(&client, &api_url, "o", "r", "v1.0.0", &policy, tlog())
        .await
        .expect_err("matched-but-id-less release must error");
    assert!(
        format!("{err:#}").contains("release missing 'id' field"),
        "got: {err:#}"
    );
}

// -- gitea_upload_asset -------------------------------------------------

/// Uploading an asset POSTs the file bytes (multipart) to
/// `.../releases/{id}/assets?name={file}` and the request body carries
/// the multipart `attachment` part + the file contents.
#[tokio::test]
async fn upload_asset_posts_multipart_to_assets_endpoint() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("anodizer-x86_64.tar.gz");
    tokio::fs::write(&file, b"ARTIFACT-BYTES")
        .await
        .expect("write fixture");

    let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/api/v1/repos/o/r/releases/77/assets?name=anodizer-x86_64.tar.gz",
        response: http_json("201 Created", serde_json::json!({"id": 1}).to_string()),
        times: None,
    }]);

    let client = test_client();
    let policy = fast_policy(2);
    let api_url = format!("http://{addr}");
    let ctx = GiteaCtx {
        client: &client,
        api_url: &api_url,
        owner: "o",
        repo: "r",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let asset = GiteaAssetSpec {
        file_path: &file,
        file_name: "anodizer-x86_64.tar.gz",
    };

    gitea_upload_asset(&ctx, 77, &asset)
        .await
        .expect("upload should succeed");

    let entries = log.lock().unwrap();
    assert_eq!(entries.len(), 1, "exactly one upload POST");
    assert_eq!(entries[0].method, "POST");
    assert_eq!(
        entries[0].path, "/api/v1/repos/o/r/releases/77/assets?name=anodizer-x86_64.tar.gz",
        "name is carried in the query string, release id in the path"
    );
    assert!(
        entries[0].body.contains("name=\"attachment\""),
        "multipart body uses the `attachment` form field, got: {}",
        entries[0].body
    );
    assert!(
        entries[0].body.contains("ARTIFACT-BYTES"),
        "multipart body carries the file contents"
    );
}

/// A 503 on the asset POST retries (rebuilding the move-only multipart
/// form per attempt) and then succeeds.
#[tokio::test]
async fn upload_asset_retries_5xx() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("a.bin");
    tokio::fs::write(&file, b"xyz")
        .await
        .expect("write fixture");

    let (addr, log) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "POST",
            path_pattern: "/api/v1/repos/o/r/releases/1/assets?name=a.bin",
            response: "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n",
            times: Some(1),
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/api/v1/repos/o/r/releases/1/assets?name=a.bin",
            response: http_json("201 Created", serde_json::json!({"id": 2}).to_string()),
            times: None,
        },
    ]);

    let client = test_client();
    let policy = fast_policy(3);
    let api_url = format!("http://{addr}");
    let ctx = GiteaCtx {
        client: &client,
        api_url: &api_url,
        owner: "o",
        repo: "r",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let asset = GiteaAssetSpec {
        file_path: &file,
        file_name: "a.bin",
    };

    gitea_upload_asset(&ctx, 1, &asset)
        .await
        .expect("upload should succeed after 5xx retry");
    let entries = log.lock().unwrap();
    assert_eq!(entries.len(), 2, "502 upload retried once, then 201");
}

/// A 4xx on the asset POST surfaces an error naming the asset, the
/// release id, and the status.
#[tokio::test]
async fn upload_asset_surfaces_4xx() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("a.bin");
    tokio::fs::write(&file, b"xyz")
        .await
        .expect("write fixture");

    let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/api/v1/repos/o/r/releases/4/assets?name=a.bin",
        response: http_json(
            "400 Bad Request",
            serde_json::json!({"message": "bad asset"}).to_string(),
        ),
        times: None,
    }]);

    let client = test_client();
    let policy = fast_policy(1);
    let api_url = format!("http://{addr}");
    let ctx = GiteaCtx {
        client: &client,
        api_url: &api_url,
        owner: "o",
        repo: "r",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let asset = GiteaAssetSpec {
        file_path: &file,
        file_name: "a.bin",
    };

    let err = gitea_upload_asset(&ctx, 4, &asset)
        .await
        .expect_err("400 must surface");
    let chain = format!("{err:#}");
    assert!(
        chain.contains("upload asset 'a.bin' to release 4 failed (HTTP 400"),
        "error must name asset + release + status, got: {chain}"
    );
}

// -- gitea_delete_asset_by_name -----------------------------------------

/// Deleting by name lists the release's assets, matches the name, then
/// DELETEs `.../assets/{asset_id}` and returns `true`.
#[tokio::test]
async fn delete_asset_by_name_lists_then_deletes() {
    let assets = serde_json::json!([
        {"id": 11, "name": "other.bin", "size": 1},
        {"id": 22, "name": "target.bin", "size": 2}
    ])
    .to_string();
    let (addr, log) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/api/v1/repos/o/r/releases/9/assets",
            response: http_json("200 OK", assets),
            times: None,
        },
        ScriptedRoute {
            method: "DELETE",
            path_pattern: "/api/v1/repos/o/r/releases/9/assets/22",
            response: "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
            times: None,
        },
    ]);

    let client = test_client();
    let policy = fast_policy(2);
    let api_url = format!("http://{addr}");
    let ctx = GiteaCtx {
        client: &client,
        api_url: &api_url,
        owner: "o",
        repo: "r",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };

    let deleted = gitea_delete_asset_by_name(&ctx, 9, "target.bin")
        .await
        .expect("delete should succeed");
    assert!(deleted, "matching asset reported as deleted");

    let entries = log.lock().unwrap();
    assert_eq!(entries.len(), 2, "one list GET + one DELETE");
    assert_eq!(entries[0].method, "GET");
    assert_eq!(entries[1].method, "DELETE");
    assert_eq!(
        entries[1].path, "/api/v1/repos/o/r/releases/9/assets/22",
        "DELETE targets the matched asset's numeric id, not its name"
    );
}

/// When no listed asset matches the name, the backend issues no DELETE
/// and returns `false`.
#[tokio::test]
async fn delete_asset_by_name_absent_returns_false() {
    let assets = serde_json::json!([
        {"id": 11, "name": "other.bin", "size": 1}
    ])
    .to_string();
    let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "GET",
        path_pattern: "/api/v1/repos/o/r/releases/9/assets",
        response: http_json("200 OK", assets),
        times: None,
    }]);

    let client = test_client();
    let policy = fast_policy(2);
    let api_url = format!("http://{addr}");
    let ctx = GiteaCtx {
        client: &client,
        api_url: &api_url,
        owner: "o",
        repo: "r",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };

    let deleted = gitea_delete_asset_by_name(&ctx, 9, "missing.bin")
        .await
        .expect("listing should succeed");
    assert!(!deleted, "no match => false");

    let entries = log.lock().unwrap();
    assert_eq!(entries.len(), 1, "only the list GET, no DELETE");
    assert!(entries.iter().all(|e| e.method != "DELETE"));
}

/// A 503 on the asset-list GET retries before the delete proceeds.
#[tokio::test]
async fn delete_asset_by_name_retries_5xx_on_list() {
    let assets = serde_json::json!([
        {"id": 33, "name": "t.bin", "size": 1}
    ])
    .to_string();
    let (addr, log) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/api/v1/repos/o/r/releases/2/assets",
            response: "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
            times: Some(1),
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/api/v1/repos/o/r/releases/2/assets",
            response: http_json("200 OK", assets),
            times: None,
        },
        ScriptedRoute {
            method: "DELETE",
            path_pattern: "/api/v1/repos/o/r/releases/2/assets/33",
            response: "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
            times: None,
        },
    ]);

    let client = test_client();
    let policy = fast_policy(3);
    let api_url = format!("http://{addr}");
    let ctx = GiteaCtx {
        client: &client,
        api_url: &api_url,
        owner: "o",
        repo: "r",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };

    let deleted = gitea_delete_asset_by_name(&ctx, 2, "t.bin")
        .await
        .expect("delete should succeed after list retry");
    assert!(deleted);
    let entries = log.lock().unwrap();
    let gets = entries.iter().filter(|e| e.method == "GET").count();
    assert_eq!(gets, 2, "503 list GET retried once before the DELETE");
    assert_eq!(entries.iter().filter(|e| e.method == "DELETE").count(), 1);
}

/// When the matched asset's DELETE returns a 4xx, the DELETE error closure
/// fires: the function bails with a message naming the asset, its id, the
/// release id, and the status — never returning `true`. `max_attempts: 1`
/// proves the 4xx fast-fails rather than retrying.
#[tokio::test]
async fn delete_asset_by_name_surfaces_delete_failure() {
    let assets = serde_json::json!([
        {"id": 44, "name": "target.bin", "size": 7}
    ])
    .to_string();
    let (addr, log) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/api/v1/repos/o/r/releases/3/assets",
            response: http_json("200 OK", assets),
            times: None,
        },
        ScriptedRoute {
            method: "DELETE",
            path_pattern: "/api/v1/repos/o/r/releases/3/assets/44",
            response: http_json(
                "403 Forbidden",
                serde_json::json!({"message": "no delete access"}).to_string(),
            ),
            times: None,
        },
    ]);

    let client = test_client();
    let policy = fast_policy(1);
    let api_url = format!("http://{addr}");
    let ctx = GiteaCtx {
        client: &client,
        api_url: &api_url,
        owner: "o",
        repo: "r",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };

    let err = gitea_delete_asset_by_name(&ctx, 3, "target.bin")
        .await
        .expect_err("a 403 on the DELETE must surface as an error");
    let chain = format!("{err:#}");
    assert!(
        chain.contains("delete asset 'target.bin' (id=44) from release 3 failed (HTTP 403"),
        "error must name asset + id + release + status, got: {chain}"
    );
    let entries = log.lock().unwrap();
    assert_eq!(
        entries.iter().filter(|e| e.method == "DELETE").count(),
        1,
        "a 4xx DELETE fast-fails (no retry)"
    );
}

// -- gitea_find_asset_size ----------------------------------------------

/// The size probe returns the matched asset's `size` field.
#[tokio::test]
async fn find_asset_size_returns_matched_size() {
    let assets = serde_json::json!([
        {"id": 1, "name": "a.bin", "size": 10},
        {"id": 2, "name": "b.bin", "size": 4096}
    ])
    .to_string();
    let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "GET",
        path_pattern: "/api/v1/repos/o/r/releases/5/assets",
        response: http_json("200 OK", assets),
        times: None,
    }]);

    let client = test_client();
    let policy = fast_policy(2);
    let api_url = format!("http://{addr}");
    let ctx = GiteaCtx {
        client: &client,
        api_url: &api_url,
        owner: "o",
        repo: "r",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };

    let size = gitea_find_asset_size(&ctx, 5, "b.bin")
        .await
        .expect("probe should succeed");
    assert_eq!(size, Some(4096), "returns the matched asset's byte size");
}

/// The size probe returns `None` when no asset matches the name.
#[tokio::test]
async fn find_asset_size_absent_returns_none() {
    let assets = serde_json::json!([
        {"id": 1, "name": "a.bin", "size": 10}
    ])
    .to_string();
    let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "GET",
        path_pattern: "/api/v1/repos/o/r/releases/5/assets",
        response: http_json("200 OK", assets),
        times: None,
    }]);

    let client = test_client();
    let policy = fast_policy(2);
    let api_url = format!("http://{addr}");
    let ctx = GiteaCtx {
        client: &client,
        api_url: &api_url,
        owner: "o",
        repo: "r",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };

    let size = gitea_find_asset_size(&ctx, 5, "missing.bin")
        .await
        .expect("probe should succeed");
    assert_eq!(size, None, "no name match => None");
}

/// A non-numeric / absent `size` field on the matched asset is treated
/// as "unknown size" (`None`), which the caller maps to
/// delete-and-reupload.
#[tokio::test]
async fn find_asset_size_non_numeric_size_is_none() {
    let assets = serde_json::json!([
        {"id": 1, "name": "a.bin", "size": "not-a-number"}
    ])
    .to_string();
    let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "GET",
        path_pattern: "/api/v1/repos/o/r/releases/5/assets",
        response: http_json("200 OK", assets),
        times: None,
    }]);

    let client = test_client();
    let policy = fast_policy(2);
    let api_url = format!("http://{addr}");
    let ctx = GiteaCtx {
        client: &client,
        api_url: &api_url,
        owner: "o",
        repo: "r",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };

    let size = gitea_find_asset_size(&ctx, 5, "a.bin")
        .await
        .expect("probe should succeed");
    assert_eq!(
        size, None,
        "matched-but-unparseable size falls through to None"
    );
}

/// A 4xx on the size-probe asset-list GET fires the size-probe list error
/// closure and bails (it is the size-probe variant of the list message,
/// distinct from the delete path's list). `max_attempts: 1` proves the
/// 4xx fast-fails.
#[tokio::test]
async fn find_asset_size_list_failure_surfaces_error() {
    let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "GET",
        path_pattern: "/api/v1/repos/o/r/releases/8/assets",
        response: http_json(
            "401 Unauthorized",
            serde_json::json!({"message": "bad token"}).to_string(),
        ),
        times: None,
    }]);

    let client = test_client();
    let policy = fast_policy(1);
    let api_url = format!("http://{addr}");
    let ctx = GiteaCtx {
        client: &client,
        api_url: &api_url,
        owner: "o",
        repo: "r",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };

    let err = gitea_find_asset_size(&ctx, 8, "a.bin")
        .await
        .expect_err("a 401 on the size-probe list must surface");
    assert!(
        format!("{err:#}").contains("list release assets failed (HTTP 401"),
        "error must name the failing list call + status, got: {err:#}"
    );
    assert_eq!(
        log.lock().unwrap().len(),
        1,
        "a 4xx list GET fast-fails (no retry)"
    );
}

// -- run_gitea_backend orchestration ------------------------------------
//
// These drive the production orchestrator (token resolution, URL
// resolution, create-release, the per-asset idempotency probe +
// delete-then-upload decision, and html_url composition) against the
// scripted responder. The Context is built with token_type=Gitea so
// `resolve_release_repo` reads `release.gitea`, and `gitea_urls.{api,
// download}` point at the loopback so every API call is observable.
// Mirrors the gitlab.rs `run_gitlab_backend` end-to-end tests.

use anodizer_core::config::{
    CrateConfig, GiteaUrlsConfig, ReleaseConfig, RetryConfig, ScmRepoConfig,
};
use anodizer_core::context::Context;
use anodizer_core::log::{StageLogger, Verbosity};

fn tlog() -> &'static StageLogger {
    anodizer_core::test_helpers::test_logger()
}
use anodizer_core::scm::ScmTokenType;
use anodizer_core::test_helpers::TestContextBuilder;

/// Build a Gitea-flavoured Context: token_type=Gitea, a fast retry policy,
/// and `gitea_urls.{api,download}` pointed at the loopback base so the URL
/// builder's `/api/v1/...` suffix lands on the scripted responder.
fn build_gitea_ctx(api_base: &str) -> Context {
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.0.0")
        .commit("deadbeef")
        .token(Some("gitea-test".to_string()))
        .build();
    ctx.token_type = ScmTokenType::Gitea;
    ctx.config.gitea_urls = Some(GiteaUrlsConfig {
        api: Some(api_base.to_string()),
        download: Some(api_base.to_string()),
        skip_tls_verify: None,
    });
    ctx.config.retry = Some(RetryConfig {
        attempts: 3,
        delay: anodizer_core::config::HumanDuration(std::time::Duration::from_millis(1)),
        max_delay: anodizer_core::config::HumanDuration(std::time::Duration::from_millis(2)),
        max_elapsed: None,
    });
    ctx
}

/// A `CrateConfig` whose `release.gitea` points at owner=o, name=r.
fn build_gitea_crate_cfg() -> CrateConfig {
    let mut crate_cfg = CrateConfig {
        name: "demo".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ Version }}".to_string()),
        ..Default::default()
    };
    crate_cfg.release = Some(ReleaseConfig {
        gitea: Some(ScmRepoConfig {
            owner: "o".to_string(),
            name: "r".to_string(),
            token: None,
        }),
        mode: Some("replace".to_string()),
        ..Default::default()
    });
    crate_cfg
}

fn default_gitea_spec() -> GiteaBackendSpec<'static> {
    GiteaBackendSpec {
        tag: "v1.0.0",
        release_name: "Release v1.0.0",
        release_body: "the body",
        release_mode: "replace",
        draft: false,
        prerelease: false,
        skip_upload: false,
        replace_existing_draft: false,
        use_existing_draft: false,
        replace_existing_artifacts: false,
    }
}

/// End-to-end: a fresh release (empty list GET → POST create) plus one
/// asset whose size probe finds no remote match, so the upload proceeds.
/// Asserts the success payload `(html_url, download, owner, repo)` and that
/// the create POST, the size-probe GET, and the upload POST all hit the
/// loopback.
#[test]
fn run_backend_creates_release_and_uploads_one_asset() {
    let dir = tempfile::tempdir().expect("tempdir");
    let artifact = dir.path().join("demo.tar.gz");
    std::fs::write(&artifact, b"PAYLOAD").expect("write artifact");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let routes = vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/api/v1/repos/o/r/releases?page=1&limit=50",
            response: "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]",
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/api/v1/repos/o/r/releases",
            response: http_json("201 Created", serde_json::json!({"id": 7}).to_string()),
            times: None,
        },
        // size probe: no assets yet => upload proceeds.
        ScriptedRoute {
            method: "GET",
            path_pattern: "/api/v1/repos/o/r/releases/7/assets",
            response: "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]",
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/api/v1/repos/o/r/releases/7/assets?name=demo.tar.gz",
            response: http_json("201 Created", serde_json::json!({"id": 1}).to_string()),
            times: None,
        },
    ];
    let (_addr, log) = spawn_scripted_responder_on(listener, |_| routes);

    let api_base = format!("http://{addr}");
    let ctx = build_gitea_ctx(&api_base);
    let crate_cfg = build_gitea_crate_cfg();
    let release_cfg = crate_cfg.release.as_ref().expect("release cfg");
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let log_stage = StageLogger::new("release", Verbosity::Normal);
    let token = Some("gitea-test".to_string());
    let env = GiteaBackendEnv {
        rt: &rt,
        ctx: &ctx,
        log: &log_stage,
        token: &token,
    };
    let artifacts = vec![(artifact, Some("demo.tar.gz".to_string()))];

    let out = run_gitea_backend(
        &env,
        &crate_cfg,
        release_cfg,
        &default_gitea_spec(),
        &artifacts,
    )
    .expect("run_gitea_backend should succeed")
    .expect("returns Some on success");
    let (html_url, download, owner, repo) = out;
    assert_eq!(owner, "o");
    assert_eq!(repo, "r");
    assert_eq!(
        download, api_base,
        "download base echoes gitea_urls.download"
    );
    assert_eq!(
        html_url,
        format!("{api_base}/o/r/releases/tag/v1.0.0"),
        "html_url composes from download base + owner/repo/releases/tag/tag"
    );

    let entries = log.lock().unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e.method == "POST" && e.path == "/api/v1/repos/o/r/releases"),
        "the create POST hit the loopback"
    );
    let upload = entries
        .iter()
        .find(|e| e.method == "POST" && e.path.contains("/assets?name=demo.tar.gz"))
        .expect("the upload POST was issued");
    assert!(
        upload.body.contains("PAYLOAD"),
        "the upload POST carried the artifact bytes"
    );
}

/// With `skip_upload` set, the orchestrator creates the release but issues
/// no size probe and no upload POST.
#[test]
fn run_backend_skip_upload_creates_release_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    let artifact = dir.path().join("demo.tar.gz");
    std::fs::write(&artifact, b"PAYLOAD").expect("write artifact");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let routes = vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/api/v1/repos/o/r/releases?page=1&limit=50",
            response: "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]",
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/api/v1/repos/o/r/releases",
            response: http_json("201 Created", serde_json::json!({"id": 7}).to_string()),
            times: None,
        },
    ];
    let (_addr, log) = spawn_scripted_responder_on(listener, |_| routes);

    let api_base = format!("http://{addr}");
    let ctx = build_gitea_ctx(&api_base);
    let crate_cfg = build_gitea_crate_cfg();
    let release_cfg = crate_cfg.release.as_ref().expect("release cfg");
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let log_stage = StageLogger::new("release", Verbosity::Normal);
    let token = Some("gitea-test".to_string());
    let env = GiteaBackendEnv {
        rt: &rt,
        ctx: &ctx,
        log: &log_stage,
        token: &token,
    };
    let mut spec = default_gitea_spec();
    spec.skip_upload = true;
    let artifacts = vec![(artifact, Some("demo.tar.gz".to_string()))];

    run_gitea_backend(&env, &crate_cfg, release_cfg, &spec, &artifacts)
        .expect("run_gitea_backend should succeed")
        .expect("returns Some");

    let entries = log.lock().unwrap();
    assert!(
        entries.iter().all(|e| !e.path.contains("/assets")),
        "skip_upload must issue no size probe / upload calls, got: {:?}",
        entries.iter().map(|e| &e.path).collect::<Vec<_>>()
    );
}

/// When the size probe finds a same-size remote asset, the upload is
/// skipped (idempotent no-op): no DELETE, no upload POST — only the create
/// flow plus the size probe GET.
#[test]
fn run_backend_idempotent_skip_when_size_matches() {
    let dir = tempfile::tempdir().expect("tempdir");
    let artifact = dir.path().join("demo.tar.gz");
    std::fs::write(&artifact, b"PAYLOAD").expect("write artifact");
    let local_size = std::fs::metadata(&artifact).expect("stat").len();

    let existing_assets =
        serde_json::json!([{"id": 1, "name": "demo.tar.gz", "size": local_size}]).to_string();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let routes = vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/api/v1/repos/o/r/releases?page=1&limit=50",
            response: "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]",
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/api/v1/repos/o/r/releases",
            response: http_json("201 Created", serde_json::json!({"id": 7}).to_string()),
            times: None,
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/api/v1/repos/o/r/releases/7/assets",
            response: http_json("200 OK", existing_assets),
            times: None,
        },
    ];
    let (_addr, log) = spawn_scripted_responder_on(listener, |_| routes);

    let api_base = format!("http://{addr}");
    let ctx = build_gitea_ctx(&api_base);
    let crate_cfg = build_gitea_crate_cfg();
    let release_cfg = crate_cfg.release.as_ref().expect("release cfg");
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let log_stage = StageLogger::new("release", Verbosity::Normal);
    let token = Some("gitea-test".to_string());
    let env = GiteaBackendEnv {
        rt: &rt,
        ctx: &ctx,
        log: &log_stage,
        token: &token,
    };
    let artifacts = vec![(artifact, Some("demo.tar.gz".to_string()))];

    run_gitea_backend(
        &env,
        &crate_cfg,
        release_cfg,
        &default_gitea_spec(),
        &artifacts,
    )
    .expect("run_gitea_backend should succeed")
    .expect("returns Some");

    let entries = log.lock().unwrap();
    assert!(
        entries
            .iter()
            .all(|e| !(e.method == "POST" && e.path.contains("/assets?name="))),
        "a same-size remote asset must skip the upload POST entirely"
    );
    assert!(
        entries.iter().all(|e| e.method != "DELETE"),
        "an idempotent skip issues no DELETE"
    );
}

/// With `replace_existing_artifacts` and a DIFFERENT-size remote asset, the
/// orchestrator deletes the conflicting asset (GET list + DELETE) and then
/// re-uploads it (upload POST).
#[test]
fn run_backend_replace_existing_deletes_then_uploads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let artifact = dir.path().join("demo.tar.gz");
    std::fs::write(&artifact, b"PAYLOAD-NEW-LONGER").expect("write artifact");

    // Remote reports a different size => DeleteThenUpload.
    let existing_assets =
        serde_json::json!([{"id": 5, "name": "demo.tar.gz", "size": 3}]).to_string();
    let list_again = serde_json::json!([{"id": 5, "name": "demo.tar.gz", "size": 3}]).to_string();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let routes = vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/api/v1/repos/o/r/releases?page=1&limit=50",
            response: "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]",
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/api/v1/repos/o/r/releases",
            response: http_json("201 Created", serde_json::json!({"id": 7}).to_string()),
            times: None,
        },
        // size probe (find differing size).
        ScriptedRoute {
            method: "GET",
            path_pattern: "/api/v1/repos/o/r/releases/7/assets",
            response: http_json("200 OK", existing_assets),
            times: Some(1),
        },
        // delete-by-name list (matches the same asset id) ...
        ScriptedRoute {
            method: "GET",
            path_pattern: "/api/v1/repos/o/r/releases/7/assets",
            response: http_json("200 OK", list_again),
            times: None,
        },
        // ... then the DELETE.
        ScriptedRoute {
            method: "DELETE",
            path_pattern: "/api/v1/repos/o/r/releases/7/assets/5",
            response: "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
            times: None,
        },
        // ... then the re-upload.
        ScriptedRoute {
            method: "POST",
            path_pattern: "/api/v1/repos/o/r/releases/7/assets?name=demo.tar.gz",
            response: http_json("201 Created", serde_json::json!({"id": 9}).to_string()),
            times: None,
        },
    ];
    let (_addr, log) = spawn_scripted_responder_on(listener, |_| routes);

    let api_base = format!("http://{addr}");
    let ctx = build_gitea_ctx(&api_base);
    let crate_cfg = build_gitea_crate_cfg();
    let release_cfg = crate_cfg.release.as_ref().expect("release cfg");
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let log_stage = StageLogger::new("release", Verbosity::Normal);
    let token = Some("gitea-test".to_string());
    let env = GiteaBackendEnv {
        rt: &rt,
        ctx: &ctx,
        log: &log_stage,
        token: &token,
    };
    let mut spec = default_gitea_spec();
    spec.replace_existing_artifacts = true;
    let artifacts = vec![(artifact, Some("demo.tar.gz".to_string()))];

    run_gitea_backend(&env, &crate_cfg, release_cfg, &spec, &artifacts)
        .expect("run_gitea_backend should succeed")
        .expect("returns Some");

    let entries = log.lock().unwrap();
    assert_eq!(
        entries
            .iter()
            .filter(|e| e.method == "DELETE" && e.path == "/api/v1/repos/o/r/releases/7/assets/5")
            .count(),
        1,
        "the differing remote asset must be DELETEd before re-upload"
    );
    assert_eq!(
        entries
            .iter()
            .filter(|e| e.method == "POST" && e.path.contains("/assets?name=demo.tar.gz"))
            .count(),
        1,
        "the asset is re-uploaded after the delete"
    );
}

/// Gitea's draft support is limited, so `replace_existing_draft` /
/// `use_existing_draft` are no-ops that only emit a warning. With both set
/// the orchestrator still creates the release and uploads the asset.
#[test]
fn run_backend_draft_flags_warn_but_create_proceeds() {
    let dir = tempfile::tempdir().expect("tempdir");
    let artifact = dir.path().join("demo.tar.gz");
    std::fs::write(&artifact, b"PAYLOAD").expect("write artifact");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let routes = vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/api/v1/repos/o/r/releases?page=1&limit=50",
            response: "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]",
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/api/v1/repos/o/r/releases",
            response: http_json("201 Created", serde_json::json!({"id": 7}).to_string()),
            times: None,
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/api/v1/repos/o/r/releases/7/assets",
            response: "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]",
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/api/v1/repos/o/r/releases/7/assets?name=demo.tar.gz",
            response: http_json("201 Created", serde_json::json!({"id": 1}).to_string()),
            times: None,
        },
    ];
    let (_addr, log) = spawn_scripted_responder_on(listener, |_| routes);

    let api_base = format!("http://{addr}");
    let ctx = build_gitea_ctx(&api_base);
    let crate_cfg = build_gitea_crate_cfg();
    let release_cfg = crate_cfg.release.as_ref().expect("release cfg");
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let log_stage = StageLogger::new("release", Verbosity::Normal);
    let token = Some("gitea-test".to_string());
    let env = GiteaBackendEnv {
        rt: &rt,
        ctx: &ctx,
        log: &log_stage,
        token: &token,
    };
    let mut spec = default_gitea_spec();
    spec.replace_existing_draft = true;
    spec.use_existing_draft = true;
    let artifacts = vec![(artifact, Some("demo.tar.gz".to_string()))];

    run_gitea_backend(&env, &crate_cfg, release_cfg, &spec, &artifacts)
        .expect("draft flags must not abort the backend")
        .expect("returns Some");

    let entries = log.lock().unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e.method == "POST" && e.path == "/api/v1/repos/o/r/releases"),
        "the release is still created despite the no-op draft flags"
    );
    assert!(
        entries
            .iter()
            .any(|e| e.method == "POST" && e.path.contains("/assets?name=demo.tar.gz")),
        "the asset upload still proceeds"
    );
}

/// A missing Gitea token short-circuits before any HTTP call with an
/// actionable bail naming GITEA_TOKEN.
#[test]
fn run_backend_missing_token_bails() {
    let ctx = build_gitea_ctx("http://unused.invalid");
    let crate_cfg = build_gitea_crate_cfg();
    let release_cfg = crate_cfg.release.as_ref().expect("release cfg");
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let log_stage = StageLogger::new("release", Verbosity::Normal);
    let token: Option<String> = None;
    let env = GiteaBackendEnv {
        rt: &rt,
        ctx: &ctx,
        log: &log_stage,
        token: &token,
    };
    let artifacts: Vec<(std::path::PathBuf, Option<String>)> = Vec::new();

    let err = run_gitea_backend(
        &env,
        &crate_cfg,
        release_cfg,
        &default_gitea_spec(),
        &artifacts,
    )
    .expect_err("a missing token must bail");
    assert!(
        format!("{err:#}").contains("GITEA_TOKEN"),
        "bail must name the missing env var, got: {err:#}"
    );
}

/// A crate without any `release.gitea`/`release.github` config returns
/// `Ok(None)` (the caller `continue`s) rather than erroring.
#[test]
fn run_backend_no_gitea_config_returns_none() {
    let ctx = build_gitea_ctx("http://unused.invalid");
    let mut crate_cfg = build_gitea_crate_cfg();
    crate_cfg.release = Some(ReleaseConfig {
        mode: Some("replace".to_string()),
        ..Default::default()
    });
    let release_cfg = crate_cfg.release.as_ref().expect("release cfg");
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let log_stage = StageLogger::new("release", Verbosity::Normal);
    let token = Some("gitea-test".to_string());
    let env = GiteaBackendEnv {
        rt: &rt,
        ctx: &ctx,
        log: &log_stage,
        token: &token,
    };
    let artifacts: Vec<(std::path::PathBuf, Option<String>)> = Vec::new();

    let out = run_gitea_backend(
        &env,
        &crate_cfg,
        release_cfg,
        &default_gitea_spec(),
        &artifacts,
    )
    .expect("no-config is not an error");
    assert!(out.is_none(), "absent gitea config => Ok(None)");
}

/// A missing artifact file (path does not exist) aborts the upload loop
/// with a "files are missing" error AFTER the release is created.
#[test]
fn run_backend_missing_artifact_file_errors() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let routes = vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/api/v1/repos/o/r/releases?page=1&limit=50",
            response: "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]",
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/api/v1/repos/o/r/releases",
            response: http_json("201 Created", serde_json::json!({"id": 7}).to_string()),
            times: None,
        },
    ];
    let (_addr, _log) = spawn_scripted_responder_on(listener, |_| routes);

    let api_base = format!("http://{addr}");
    let ctx = build_gitea_ctx(&api_base);
    let crate_cfg = build_gitea_crate_cfg();
    let release_cfg = crate_cfg.release.as_ref().expect("release cfg");
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let log_stage = StageLogger::new("release", Verbosity::Normal);
    let token = Some("gitea-test".to_string());
    let env = GiteaBackendEnv {
        rt: &rt,
        ctx: &ctx,
        log: &log_stage,
        token: &token,
    };
    let missing = std::path::PathBuf::from("/nonexistent/anodizer-test/missing.tar.gz");
    let artifacts = vec![(missing, Some("missing.tar.gz".to_string()))];

    let err = run_gitea_backend(
        &env,
        &crate_cfg,
        release_cfg,
        &default_gitea_spec(),
        &artifacts,
    )
    .expect_err("a missing artifact file must abort the upload loop");
    assert!(
        format!("{err:#}").contains("missing"),
        "error must report the missing artifact, got: {err:#}"
    );
}
