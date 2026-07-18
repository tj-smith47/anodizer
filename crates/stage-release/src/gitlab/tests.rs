use super::*;

// -- gitlab_project_id ---------------------------------------------------

#[test]
fn project_id_with_owner_and_name() {
    assert_eq!(
        gitlab_project_id("mygroup", "myproject"),
        "mygroup/myproject"
    );
}

#[test]
fn project_id_with_empty_owner() {
    assert_eq!(gitlab_project_id("", "myproject"), "myproject");
}

#[test]
fn project_id_with_nested_group() {
    assert_eq!(
        gitlab_project_id("org/subgroup", "repo"),
        "org/subgroup/repo"
    );
}

// -- encode_project_id ---------------------------------------------------

#[test]
fn encode_simple_project_id() {
    assert_eq!(
        encode_project_id("mygroup/myproject"),
        "mygroup%2Fmyproject"
    );
}

#[test]
fn encode_nested_project_id() {
    assert_eq!(
        encode_project_id("org/subgroup/repo"),
        "org%2Fsubgroup%2Frepo"
    );
}

#[test]
fn encode_project_id_no_slash() {
    // A project without an owner should pass through mostly unchanged.
    assert_eq!(encode_project_id("myproject"), "myproject");
}

// -- encode_tag ---------------------------------------------------------

#[test]
fn encode_tag_simple() {
    assert_eq!(encode_tag("v1.0.0"), "v1.0.0");
}

#[test]
fn encode_tag_with_plus() {
    // `+` must be encoded to avoid breaking URL path segments.
    assert_eq!(encode_tag("v1.0.0+build.1"), "v1.0.0%2Bbuild.1");
}

#[test]
fn encode_tag_with_special_chars() {
    // `#`, `?`, and spaces must all be encoded.
    assert_eq!(encode_tag("v1 beta#2?rc"), "v1%20beta%232%3Frc");
}

// -- encode_path_segment -------------------------------------------------

#[test]
fn encode_path_segment_simple() {
    assert_eq!(encode_path_segment("myproject"), "myproject");
}

#[test]
fn encode_path_segment_with_slash() {
    assert_eq!(encode_path_segment("my/project"), "my%2Fproject");
}

#[test]
fn encode_path_segment_preserves_dots_and_dashes() {
    assert_eq!(encode_path_segment("my-project.v2"), "my-project.v2");
}

// -- is_pre_v17 (version parsing) ------------------------------------------

#[test]
fn is_pre_v17_with_v16() {
    assert!(is_pre_v17("16.11.0"));
}

#[test]
fn is_pre_v17_with_v15() {
    assert!(is_pre_v17("15.0.0"));
}

#[test]
fn is_pre_v17_with_v17() {
    assert!(!is_pre_v17("17.0.0"));
}

#[test]
fn is_pre_v17_with_v18() {
    assert!(!is_pre_v17("18.1.2"));
}

#[test]
fn is_pre_v17_with_empty() {
    assert!(!is_pre_v17(""));
}

#[test]
fn is_pre_v17_with_garbage() {
    assert!(!is_pre_v17("not-a-version"));
}

// -- gitlab_release_url --------------------------------------------------

#[test]
fn release_url_with_owner() {
    let url = gitlab_release_url("https://gitlab.com", "mygroup", "myproject", "v1.0.0");
    assert_eq!(
        url,
        "https://gitlab.com/mygroup/myproject/-/releases/v1.0.0"
    );
}

#[test]
fn release_url_without_owner() {
    let url = gitlab_release_url("https://gitlab.com", "", "myproject", "v1.0.0");
    assert_eq!(url, "https://gitlab.com/myproject/-/releases/v1.0.0");
}

#[test]
fn release_url_trailing_slash_stripped() {
    let url = gitlab_release_url("https://gitlab.example.com/", "org", "repo", "v2.0.0");
    assert_eq!(url, "https://gitlab.example.com/org/repo/-/releases/v2.0.0");
}

// -- build_gitlab_client -------------------------------------------------

#[test]
fn build_client_with_private_token() {
    let client = build_gitlab_client("glpat-xxxx", false, false);
    assert!(client.is_ok());
}

#[test]
fn build_client_with_job_token() {
    let client = build_gitlab_client("job-token-value", false, true);
    assert!(client.is_ok());
}

#[test]
fn build_client_with_skip_tls() {
    let client = build_gitlab_client("glpat-xxxx", true, false);
    assert!(client.is_ok());
}

#[test]
fn build_client_with_all_options() {
    let client = build_gitlab_client("job-token", true, true);
    assert!(client.is_ok());
}

// -- gitlab_head_asset_size host guard ------------------------------------

#[test]
fn link_host_guard_matches_api_and_download_hosts_only() {
    let bases = [
        "https://gitlab.example.com/api/v4",
        "https://dl.example.com",
    ];
    assert!(link_url_on_configured_host(
        "https://gitlab.example.com/uploads/x/demo.tar.gz",
        &bases
    ));
    assert!(link_url_on_configured_host(
        "https://dl.example.com/demo.tar.gz",
        &bases
    ));
    // Off-host, attacker-chosen link targets must never be probed.
    assert!(!link_url_on_configured_host(
        "https://evil.example.net/demo.tar.gz",
        &bases
    ));
    // Same host on a different port is a different origin.
    assert!(!link_url_on_configured_host(
        "https://gitlab.example.com:8443/demo.tar.gz",
        &bases
    ));
    // Same host, same effective port (explicit :443), different scheme:
    // matching would send the token over cleartext http.
    assert!(!link_url_on_configured_host(
        "http://gitlab.example.com:443/demo.tar.gz",
        &bases
    ));
    // Unparsable link URLs fail closed.
    assert!(!link_url_on_configured_host("not a url", &bases));
}

#[tokio::test]
async fn head_probe_skips_off_host_link_without_any_request() {
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
    // A foreign responder standing in for an attacker-chosen link
    // target: the probe must return None WITHOUT connecting to it (the
    // client's default headers carry the token).
    let (foreign_addr, foreign_calls) =
        spawn_oneshot_http_responder(vec!["HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\n"]);
    let client = build_gitlab_probe_client("glpat-secret", false, false).expect("client");
    let size = gitlab_head_asset_size(
        &client,
        &format!("http://{foreign_addr}/demo.tar.gz"),
        &["https://gitlab.example.com/api/v4"],
    )
    .await;
    assert_eq!(size, None, "off-host link must degrade to size-unknown");
    // Give any (buggy) in-flight request time to land before asserting.
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(
        foreign_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no request may reach the off-host link target"
    );
}

#[tokio::test]
async fn head_probe_skips_scheme_downgrade_link_without_any_request() {
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
    // Same host and explicit port as the configured https base, but an
    // http link scheme: matching would carry the token cleartext, so
    // the guard must reject before any connection is attempted.
    let (addr, calls) =
        spawn_oneshot_http_responder(vec!["HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\n"]);
    let client = build_gitlab_probe_client("glpat-secret", false, false).expect("client");
    let https_base = format!("https://{addr}/api/v4");
    let size = gitlab_head_asset_size(
        &client,
        &format!("http://{addr}/uploads/x/demo.tar.gz"),
        &[https_base.as_str()],
    )
    .await;
    assert_eq!(
        size, None,
        "scheme-downgrade link must degrade to size-unknown"
    );
    // Give any (buggy) in-flight request time to land before asserting.
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no request may travel over the downgraded scheme"
    );
}

#[tokio::test]
async fn head_probe_reads_size_from_on_host_link() {
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
    let (addr, calls) =
        spawn_oneshot_http_responder(vec!["HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\n"]);
    let client = build_gitlab_probe_client("glpat-secret", false, false).expect("client");
    let api_base = format!("http://{addr}/api/v4");
    let size = gitlab_head_asset_size(
        &client,
        &format!("http://{addr}/uploads/x/demo.tar.gz"),
        &[api_base.as_str()],
    )
    .await;
    assert_eq!(size, Some(7), "on-host link still probes");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn head_probe_does_not_follow_redirects() {
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder_with;
    // Object storage with proxy_download off answers the on-host link
    // with a 302 to an external pre-signed URL. The probe client must
    // not follow it (reqwest keeps custom default headers — the token —
    // across cross-host redirects) and must degrade to size-unknown.
    let (foreign_addr, foreign_calls) =
        spawn_oneshot_http_responder(vec!["HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\n"]);
    let (addr, calls) = spawn_oneshot_http_responder_with(|_| {
        vec![format!(
            "HTTP/1.1 302 Found\r\nLocation: http://{foreign_addr}/presigned/demo.tar.gz\r\nContent-Length: 0\r\n\r\n"
        )]
    });
    let client = build_gitlab_probe_client("glpat-secret", false, false).expect("client");
    let api_base = format!("http://{addr}/api/v4");
    let size = gitlab_head_asset_size(
        &client,
        &format!("http://{addr}/uploads/x/demo.tar.gz"),
        &[api_base.as_str()],
    )
    .await;
    assert_eq!(size, None, "a redirect answer must degrade to size-unknown");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(
        foreign_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the redirect target must never be contacted"
    );
}

// -- auth_header ---------------------------------------------------------

#[test]
fn auth_header_private_token() {
    assert_eq!(auth_header(false), "PRIVATE-TOKEN");
}

#[test]
fn auth_header_job_token() {
    assert_eq!(auth_header(true), "JOB-TOKEN");
}

// -- resolve_use_job_token -----------------------------------------------
// Drives the `CI_JOB_TOKEN`-based branches via injected
// `MapEnvSource` — no `unsafe set_var`, no env-mutex serialization.

use anodizer_core::MapEnvSource;

#[test]
fn resolve_use_job_token_in_ci_flag_on_tokens_match() {
    let env = MapEnvSource::new().with("CI_JOB_TOKEN", "real-ci-token");
    assert!(resolve_use_job_token_with_env(true, "real-ci-token", &env));
}

#[test]
fn resolve_use_job_token_in_ci_flag_on_tokens_differ() {
    let env = MapEnvSource::new().with("CI_JOB_TOKEN", "real-ci-token");
    assert!(!resolve_use_job_token_with_env(true, "glpat-xyz", &env));
}

#[test]
fn resolve_use_job_token_in_ci_flag_off() {
    let env = MapEnvSource::new().with("CI_JOB_TOKEN", "real-ci-token");
    assert!(!resolve_use_job_token_with_env(
        false,
        "real-ci-token",
        &env
    ));
}

#[test]
fn resolve_use_job_token_no_ci_env() {
    let env = MapEnvSource::new();
    assert!(!resolve_use_job_token_with_env(true, "glpat-xyz", &env));
}

#[test]
fn resolve_use_job_token_empty_ci_env() {
    let env = MapEnvSource::new().with("CI_JOB_TOKEN", "");
    assert!(!resolve_use_job_token_with_env(true, "", &env));
}

// -- gitlab_create_release retry behaviour (P1.4) ------------------------
//
// Pin: a 503 on the GET-release-by-tag probe must be retried (transient
// GitLab 5xx), not fast-failed. Mirror the equivalent core::retry test
// (`retry_http_async_retries_5xx_then_succeeds`) but at the publisher
// layer so the caller-supplied policy reaches the helper.

use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

#[tokio::test]
async fn gitlab_create_release_retries_5xx_on_get_probe() {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    // Sequence: 503 on the GET probe, then 200 with an empty release JSON
    // (release exists), then 200 on the PUT update. The retry helper
    // should swallow the 503 and proceed.
    let (addr, calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 23\r\n\r\n{\"description\":\"old\"}\r\n",
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
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

    let ctx = GitlabCtx {
        client: &client,
        api_url: &api_url,
        project_id: "myorg/myproj",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let spec = GitlabReleaseSpec {
        tag: "v1.0.0",
        name: "Release v1.0.0",
        body: "new body",
        commit: "abc123",
        release_mode: "replace",
    };
    let result = gitlab_create_release(&ctx, &spec).await;

    assert!(
        result.is_ok(),
        "expected success after 5xx retry, got: {:?}",
        result.err().map(|e| format!("{e:#}"))
    );
    // Three connections total: one retried GET (1 503 + 1 200 = 2) plus
    // one PUT = 3.
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "expected 3 connections (503-retry GET, 200 GET, 200 PUT)"
    );
}

/// Defense-in-depth: a GitLab API 4xx response that echoes our
/// `Authorization: Bearer <PAT>` header back must not leak the token
/// into the user-visible error chain. Exercises the
/// `gitlab_create_release` GET-probe error-message closure on the
/// 401-fast-fail path. Other gitlab.rs body-interpolation sites share
/// the same redaction wrap.
#[tokio::test]
async fn gitlab_create_release_redacts_bearer_in_error_body() {
    use std::time::Duration;

    let leaky =
        r#"{"message":"401 Unauthorized: Authorization: Bearer ghp_FAKETOKEN1234567890abcdefg"}"#;
    let body_len = leaky.len();
    // 401 fast-fails (not 403/404 which are the "release missing" signal).
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
    let ctx = GitlabCtx {
        client: &client,
        api_url: &api_url,
        project_id: "myorg/myproj",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let spec = GitlabReleaseSpec {
        tag: "v1.0.0",
        name: "Release v1.0.0",
        body: "new body",
        commit: "abc123",
        release_mode: "replace",
    };
    let err = gitlab_create_release(&ctx, &spec)
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
async fn gitlab_release_tag_empty_bails_with_actionable_error() {
    // GitLab's `POST /projects/:id/releases` rejects empty `tag_name`
    // with a vague 400; the helper must bail upfront (before the GET
    // probe URL is constructed) so users see the real cause. Bail
    // message must name the project and include an actionable hint.
    use std::time::Duration;
    let client = reqwest::Client::builder().build().expect("client");
    let policy = RetryPolicy {
        max_attempts: 1,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    };
    let ctx = GitlabCtx {
        client: &client,
        api_url: "http://unused.invalid",
        project_id: "myorg/myproj",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let spec = GitlabReleaseSpec {
        tag: "",
        name: "Release",
        body: "body",
        commit: "abc123",
        release_mode: "replace",
    };
    let err = gitlab_create_release(&ctx, &spec)
        .await
        .expect_err("empty tag must bail before any HTTP call");
    let chain = format!("{err:#}");
    assert!(
        chain.contains("gitlab:"),
        "error must carry the gitlab: prefix, got: {chain}"
    );
    assert!(
        chain.contains("tag_name"),
        "error must name the rejected field, got: {chain}"
    );
    assert!(
        chain.contains("myorg/myproj"),
        "error must name the project, got: {chain}"
    );
    assert!(
        chain.contains("release.tag:") || chain.contains("snapshot"),
        "error must include an actionable hint, got: {chain}"
    );
}

#[tokio::test]
async fn gitlab_release_commit_empty_bails_with_actionable_error() {
    // The create-branch path requires `ref` (commit SHA). Empty `ref`
    // surfaces as a vague GitLab 400 (`ref is missing`); bail upfront
    // so the user sees that `ctx.git_info.commit` was not populated.
    // Use a hermetic responder that 404s the GET probe so the
    // create-branch path is reached without hitting a real GitLab.
    use std::time::Duration;
    let (addr, _calls) =
        spawn_oneshot_http_responder(vec!["HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"]);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");
    let policy = RetryPolicy {
        max_attempts: 1,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    };
    let api_url = format!("http://{addr}");
    let ctx = GitlabCtx {
        client: &client,
        api_url: &api_url,
        project_id: "myorg/myproj",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let spec = GitlabReleaseSpec {
        tag: "v1.0.0",
        name: "Release v1.0.0",
        body: "body",
        commit: "",
        release_mode: "replace",
    };
    let err = gitlab_create_release(&ctx, &spec)
        .await
        .expect_err("empty commit must bail in create-branch path");
    let chain = format!("{err:#}");
    assert!(
        chain.contains("gitlab:"),
        "error must carry the gitlab: prefix, got: {chain}"
    );
    assert!(
        chain.contains("ref"),
        "error must name the rejected field, got: {chain}"
    );
    assert!(
        chain.contains("commit") || chain.contains("git_info"),
        "error must mention the missing-commit cause, got: {chain}"
    );
    assert!(
        chain.contains("git working tree") || chain.contains("GITHUB_SHA"),
        "error must include an actionable hint, got: {chain}"
    );
}

/// When `replace_existing` is true and the release-link POST returns 422
/// (duplicate), the function must: list existing links, DELETE the
/// conflicting one, then retry the POST. Exercises the full
/// delete-and-retry code path in `gitlab_upload_asset`.
#[tokio::test]
async fn gitlab_upload_asset_replace_existing_422_deletes_and_retries() {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    let version_body = r#"{"version":"17.0.0"}"#;
    let version_len = version_body.len();
    let version_resp: &'static str = Box::leak(
        format!(
            "HTTP/1.1 200 OK\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: {version_len}\r\n\r\n\
                 {version_body}"
        )
        .into_boxed_str(),
    );

    let links_body = r#"[{"id":42,"name":"asset.tar.gz","url":"https://example.com/old"}]"#;
    let links_len = links_body.len();
    let links_resp: &'static str = Box::leak(
        format!(
            "HTTP/1.1 200 OK\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: {links_len}\r\n\r\n\
                 {links_body}"
        )
        .into_boxed_str(),
    );

    // Sequence:
    //   1. PUT upload to package registry → 200
    //   2. GET /version → 200 (v17 detection)
    //   3. POST create link → 422 (duplicate)
    //   4. GET list links → 200 with matching link id=42
    //   5. DELETE link/42 → 200
    //   6. POST create link retry → 201
    let (addr, calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
        version_resp,
        "HTTP/1.1 422 Unprocessable Entity\r\nContent-Length: 0\r\n\r\n",
        links_resp,
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
        "HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n",
    ]);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .pool_idle_timeout(Duration::ZERO)
        .build()
        .expect("client");
    let policy = RetryPolicy {
        max_attempts: 2,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    };
    let api_url = format!("http://{addr}");

    let ctx = GitlabCtx {
        client: &client,
        api_url: &api_url,
        project_id: "myorg/myproj",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };

    let tmp = tempfile::NamedTempFile::new().expect("create temp file");
    std::fs::write(tmp.path(), b"fake-asset-bytes").expect("write temp file");

    let asset = GitlabAssetSpec {
        file_path: tmp.path(),
        file_name: "asset.tar.gz",
    };
    let pkg = GitlabPackageRegistrySpec {
        project_name: "myproj",
        version: "1.0.0",
    };

    let result = gitlab_upload_asset(
        &ctx,
        "v1.0.0",
        &asset,
        Some(&pkg),
        "https://gitlab.com/myorg/myproj",
        true,
    )
    .await;

    assert!(
        result.is_ok(),
        "expected success after 422 delete-and-retry, got: {:?}",
        result.err().map(|e| format!("{e:#}"))
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        6,
        "expected 6 connections (PUT upload, GET version, POST 422, GET links, DELETE, POST retry)"
    );
}

// -- HTTP-flow tests (route-aware) --------------------------------------
//
// Where the `spawn_oneshot_http_responder` tests above serve responses in
// strict arrival order (blind to URL), these point `GitlabCtx.api_url` at a
// `spawn_scripted_responder` and assert on the recorded request log: the
// exact method/path/body of every GET probe, PUT update, POST create,
// package-registry PUT, project-uploads POST, version probe, link POST,
// link list, and link DELETE the backend issues against GitLab's
// `/projects/...` surface. Project IDs encode the namespace slash as
// `%2F` (e.g. `myorg/myproj` -> `myorg%2Fmyproj`); tags keep their dots.

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

/// Wrap a JSON body in a response with the right `Content-Length`. Leaks
/// because the responder needs `&'static str`.
fn http_json(status: &str, body: String) -> &'static str {
    let len = body.len();
    Box::leak(
            format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {len}\r\n\r\n{body}"
            )
            .into_boxed_str(),
        )
}

// -- gitlab_create_release: create path (release absent) ----------------

/// A 404 on the GET-release-by-tag probe is the "release does not exist"
/// signal: the backend falls through to a POST create against the
/// un-suffixed `.../releases` endpoint, sending tag_name, ref, name and
/// description in the body.
#[tokio::test]
async fn create_release_posts_when_get_probe_404s() {
    let (addr, log) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/projects/myorg%2Fmyproj/releases/v1.0.0",
            response: "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/projects/myorg%2Fmyproj/releases",
            response: http_json(
                "201 Created",
                serde_json::json!({"tag_name": "v1.0.0"}).to_string(),
            ),
            times: None,
        },
    ]);

    let client = test_client();
    let policy = fast_policy(2);
    let api_url = format!("http://{addr}");
    let ctx = GitlabCtx {
        client: &client,
        api_url: &api_url,
        project_id: "myorg/myproj",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let spec = GitlabReleaseSpec {
        tag: "v1.0.0",
        name: "Release v1.0.0",
        body: "the body",
        commit: "deadbeef",
        release_mode: "replace",
    };

    let tag = gitlab_create_release(&ctx, &spec)
        .await
        .expect("create should succeed");
    assert_eq!(tag, "v1.0.0", "create returns the tag name as release id");

    let entries = log.lock().unwrap();
    assert_eq!(entries.len(), 2, "one GET probe + one POST create");
    assert_eq!(entries[0].method, "GET");
    assert_eq!(entries[1].method, "POST");
    assert_eq!(
        entries[1].path, "/projects/myorg%2Fmyproj/releases",
        "create POSTs to the un-suffixed releases endpoint"
    );
    let payload: serde_json::Value =
        serde_json::from_str(&entries[1].body).expect("POST body is JSON");
    assert_eq!(payload["tag_name"], "v1.0.0");
    assert_eq!(
        payload["ref"], "deadbeef",
        "create sends the commit SHA as `ref`"
    );
    assert_eq!(payload["name"], "Release v1.0.0");
    assert_eq!(payload["description"], "the body");
}

/// A 403 on the GET probe is treated identically to 404 (GitLab returns
/// 403 for a missing release on some self-managed instances): the backend
/// proceeds to create rather than propagating the 403.
#[tokio::test]
async fn create_release_treats_403_probe_as_absent() {
    let (addr, log) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/projects/o%2Fr/releases/v2.0.0",
            response: "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n",
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/projects/o%2Fr/releases",
            response: http_json("201 Created", "{}".to_string()),
            times: None,
        },
    ]);

    let client = test_client();
    let policy = fast_policy(2);
    let api_url = format!("http://{addr}");
    let ctx = GitlabCtx {
        client: &client,
        api_url: &api_url,
        project_id: "o/r",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let spec = GitlabReleaseSpec {
        tag: "v2.0.0",
        name: "n",
        body: "b",
        commit: "abc",
        release_mode: "replace",
    };

    gitlab_create_release(&ctx, &spec)
        .await
        .expect("403 probe must route to create, not error");
    let entries = log.lock().unwrap();
    assert_eq!(entries.len(), 2, "403 probe then POST create");
    assert_eq!(entries[1].method, "POST");
}

/// A 401 on the GET probe is neither 403 nor 404, so it propagates as an
/// error (no create POST is issued).
#[tokio::test]
async fn create_release_propagates_non_404_probe_error() {
    let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "GET",
        path_pattern: "/projects/o%2Fr/releases/v1.0.0",
        response: http_json(
            "401 Unauthorized",
            serde_json::json!({"message": "401 Unauthorized"}).to_string(),
        ),
        times: None,
    }]);

    let client = test_client();
    let policy = fast_policy(1);
    let api_url = format!("http://{addr}");
    let ctx = GitlabCtx {
        client: &client,
        api_url: &api_url,
        project_id: "o/r",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let spec = GitlabReleaseSpec {
        tag: "v1.0.0",
        name: "n",
        body: "b",
        commit: "abc",
        release_mode: "replace",
    };

    let err = gitlab_create_release(&ctx, &spec)
        .await
        .expect_err("401 probe must propagate");
    assert!(
        format!("{err:#}").contains("HTTP 401"),
        "error must carry the 401 status, got: {err:#}"
    );
    let entries = log.lock().unwrap();
    assert!(
        entries.iter().all(|e| e.method != "POST"),
        "a propagated probe error must not fall through to create"
    );
}

// -- gitlab_create_release: update path (release exists) ----------------

/// A 200 on the GET probe means the release exists: the backend PUTs the
/// same `.../releases/{tag}` path with the composed body. `replace` mode
/// sends the new description verbatim (existing body ignored).
#[tokio::test]
async fn update_release_puts_existing_replace_mode() {
    let existing = serde_json::json!({"description": "old body"}).to_string();
    let (addr, log) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/projects/o%2Fr/releases/v1.0.0",
            response: http_json("200 OK", existing),
            times: None,
        },
        ScriptedRoute {
            method: "PUT",
            path_pattern: "/projects/o%2Fr/releases/v1.0.0",
            response: http_json("200 OK", "{}".to_string()),
            times: None,
        },
    ]);

    let client = test_client();
    let policy = fast_policy(2);
    let api_url = format!("http://{addr}");
    let ctx = GitlabCtx {
        client: &client,
        api_url: &api_url,
        project_id: "o/r",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let spec = GitlabReleaseSpec {
        tag: "v1.0.0",
        name: "rel",
        body: "new body",
        commit: "abc",
        release_mode: "replace",
    };

    let tag = gitlab_create_release(&ctx, &spec)
        .await
        .expect("update should succeed");
    assert_eq!(tag, "v1.0.0");

    let entries = log.lock().unwrap();
    assert!(
        entries.iter().all(|e| e.method != "POST"),
        "existing release must be PUT-updated, never POSTed"
    );
    let put = entries
        .iter()
        .find(|e| e.method == "PUT")
        .expect("a PUT was issued");
    assert_eq!(put.path, "/projects/o%2Fr/releases/v1.0.0");
    let payload: serde_json::Value = serde_json::from_str(&put.body).expect("PUT body is JSON");
    assert_eq!(
        payload["description"], "new body",
        "replace mode sends the new body verbatim"
    );
    assert_eq!(payload["name"], "rel");
}

/// The `prepend` release mode composes existing + new into the PUT
/// payload (new body first, then the existing description).
#[tokio::test]
async fn update_release_prepend_mode_composes_body() {
    let existing = serde_json::json!({"description": "EXISTING"}).to_string();
    let (addr, log) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/projects/o%2Fr/releases/v3.0.0",
            response: http_json("200 OK", existing),
            times: None,
        },
        ScriptedRoute {
            method: "PUT",
            path_pattern: "/projects/o%2Fr/releases/v3.0.0",
            response: http_json("200 OK", "{}".to_string()),
            times: None,
        },
    ]);

    let client = test_client();
    let policy = fast_policy(2);
    let api_url = format!("http://{addr}");
    let ctx = GitlabCtx {
        client: &client,
        api_url: &api_url,
        project_id: "o/r",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let spec = GitlabReleaseSpec {
        tag: "v3.0.0",
        name: "rel",
        body: "NEW",
        commit: "abc",
        release_mode: "prepend",
    };

    gitlab_create_release(&ctx, &spec)
        .await
        .expect("update should succeed");

    let entries = log.lock().unwrap();
    let put = entries
        .iter()
        .find(|e| e.method == "PUT")
        .expect("a PUT was issued");
    let payload: serde_json::Value = serde_json::from_str(&put.body).expect("PUT body is JSON");
    let desc = payload["description"].as_str().expect("description string");
    assert!(
        desc.contains("NEW") && desc.contains("EXISTING"),
        "prepend keeps both bodies, got: {desc}"
    );
    assert!(
        desc.find("NEW") < desc.find("EXISTING"),
        "prepend puts the new body before the existing one, got: {desc}"
    );
}

/// A 5xx on the PUT update is retried through `retry_http_async` and then
/// succeeds; the log records both PUTs.
#[tokio::test]
async fn update_release_retries_5xx_on_put() {
    let existing = serde_json::json!({"description": "old"}).to_string();
    let (addr, log) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/projects/o%2Fr/releases/v1.0.0",
            response: http_json("200 OK", existing),
            times: None,
        },
        ScriptedRoute {
            method: "PUT",
            path_pattern: "/projects/o%2Fr/releases/v1.0.0",
            response: "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
            times: Some(1),
        },
        ScriptedRoute {
            method: "PUT",
            path_pattern: "/projects/o%2Fr/releases/v1.0.0",
            response: http_json("200 OK", "{}".to_string()),
            times: None,
        },
    ]);

    let client = test_client();
    let policy = fast_policy(3);
    let api_url = format!("http://{addr}");
    let ctx = GitlabCtx {
        client: &client,
        api_url: &api_url,
        project_id: "o/r",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let spec = GitlabReleaseSpec {
        tag: "v1.0.0",
        name: "rel",
        body: "b",
        commit: "abc",
        release_mode: "replace",
    };

    gitlab_create_release(&ctx, &spec)
        .await
        .expect("update should succeed after 5xx retry");
    let entries = log.lock().unwrap();
    let puts = entries.iter().filter(|e| e.method == "PUT").count();
    assert_eq!(puts, 2, "503 PUT retried once, then 200");
}

// -- gitlab_upload_asset: project-uploads (markdown) path ---------------

/// With `pkg == None`, the file is uploaded via the project Markdown
/// Uploads endpoint (POST multipart to `.../uploads`), the returned
/// `full_path` is joined onto the download base to form the link URL, and
/// a release link is then POSTed to `.../assets/links` carrying that URL.
/// On a v17 server the path field is `direct_asset_path`.
#[tokio::test]
async fn upload_asset_project_uploads_creates_link() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("asset.tar.gz");
    tokio::fs::write(&file, b"ARTIFACT-BYTES")
        .await
        .expect("write fixture");

    let upload_resp = serde_json::json!({
        "full_path": "/uploads/abc123/asset.tar.gz",
        "url": "/uploads/abc123/asset.tar.gz"
    })
    .to_string();
    let (addr, log) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "POST",
            path_pattern: "/projects/myorg%2Fmyproj/uploads",
            response: http_json("201 Created", upload_resp),
            times: None,
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/version",
            response: http_json(
                "200 OK",
                serde_json::json!({"version": "17.0.0"}).to_string(),
            ),
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/projects/myorg%2Fmyproj/releases/v1.0.0/assets/links",
            response: http_json("201 Created", serde_json::json!({"id": 1}).to_string()),
            times: None,
        },
    ]);

    let client = test_client();
    let policy = fast_policy(2);
    let api_url = format!("http://{addr}");
    let ctx = GitlabCtx {
        client: &client,
        api_url: &api_url,
        project_id: "myorg/myproj",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let asset = GitlabAssetSpec {
        file_path: &file,
        file_name: "asset.tar.gz",
    };

    // download_url is the base the returned full_path is joined onto.
    let download_url = format!("http://{addr}");
    gitlab_upload_asset(&ctx, "v1.0.0", &asset, None, &download_url, false)
        .await
        .expect("project-uploads upload should succeed");

    let entries = log.lock().unwrap();
    let upload = entries
        .iter()
        .find(|e| e.path == "/projects/myorg%2Fmyproj/uploads")
        .expect("project-uploads POST issued");
    assert_eq!(upload.method, "POST");
    assert!(
        upload.body.contains("name=\"file\""),
        "markdown upload uses the `file` form field, got: {}",
        upload.body
    );
    assert!(
        upload.body.contains("ARTIFACT-BYTES"),
        "multipart body carries the file contents"
    );

    let link = entries
        .iter()
        .find(|e| e.path == "/projects/myorg%2Fmyproj/releases/v1.0.0/assets/links")
        .expect("link POST issued");
    let payload: serde_json::Value = serde_json::from_str(&link.body).expect("link body is JSON");
    assert_eq!(payload["name"], "asset.tar.gz");
    assert_eq!(
        payload["url"],
        format!("{download_url}/uploads/abc123/asset.tar.gz"),
        "link url is download base + returned full_path"
    );
    assert_eq!(
        payload["direct_asset_path"], "/asset.tar.gz",
        "v17 server uses `direct_asset_path`"
    );
    assert!(
        payload.get("filepath").is_none(),
        "v17 must not emit the legacy `filepath` field"
    );
}

/// On a pre-v17 server the version probe reports 16.x, so the link
/// payload uses the legacy `filepath` field name instead of
/// `direct_asset_path`.
#[tokio::test]
async fn upload_asset_pre_v17_uses_legacy_filepath_field() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("a.bin");
    tokio::fs::write(&file, b"xyz")
        .await
        .expect("write fixture");

    let upload_resp = serde_json::json!({"full_path": "/uploads/x/a.bin"}).to_string();
    let (addr, log) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "POST",
            path_pattern: "/projects/o%2Fr/uploads",
            response: http_json("201 Created", upload_resp),
            times: None,
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/version",
            response: http_json(
                "200 OK",
                serde_json::json!({"version": "16.11.0"}).to_string(),
            ),
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/projects/o%2Fr/releases/v1.0.0/assets/links",
            response: http_json("201 Created", serde_json::json!({"id": 1}).to_string()),
            times: None,
        },
    ]);

    let client = test_client();
    let policy = fast_policy(2);
    let api_url = format!("http://{addr}");
    let ctx = GitlabCtx {
        client: &client,
        api_url: &api_url,
        project_id: "o/r",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let asset = GitlabAssetSpec {
        file_path: &file,
        file_name: "a.bin",
    };
    let download_url = format!("http://{addr}");

    gitlab_upload_asset(&ctx, "v1.0.0", &asset, None, &download_url, false)
        .await
        .expect("upload should succeed");

    let entries = log.lock().unwrap();
    let link = entries
        .iter()
        .find(|e| e.path == "/projects/o%2Fr/releases/v1.0.0/assets/links")
        .expect("link POST issued");
    let payload: serde_json::Value = serde_json::from_str(&link.body).expect("link body is JSON");
    assert_eq!(
        payload["filepath"], "/a.bin",
        "pre-v17 server uses the legacy `filepath` field"
    );
    assert!(
        payload.get("direct_asset_path").is_none(),
        "pre-v17 must not emit the v17 `direct_asset_path` field"
    );
}

/// A project-uploads response missing the `full_path` field surfaces an
/// explicit error rather than constructing a broken link URL.
#[tokio::test]
async fn upload_asset_project_uploads_missing_full_path_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("a.bin");
    tokio::fs::write(&file, b"xyz")
        .await
        .expect("write fixture");

    let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/projects/o%2Fr/uploads",
        response: http_json(
            "201 Created",
            serde_json::json!({"url": "/uploads/x"}).to_string(),
        ),
        times: None,
    }]);

    let client = test_client();
    let policy = fast_policy(1);
    let api_url = format!("http://{addr}");
    let ctx = GitlabCtx {
        client: &client,
        api_url: &api_url,
        project_id: "o/r",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let asset = GitlabAssetSpec {
        file_path: &file,
        file_name: "a.bin",
    };
    let download_url = format!("http://{addr}");

    let err = gitlab_upload_asset(&ctx, "v1.0.0", &asset, None, &download_url, false)
        .await
        .expect_err("missing full_path must error");
    assert!(
        format!("{err:#}").contains("missing 'full_path' field"),
        "error must name the missing field, got: {err:#}"
    );
}

// -- gitlab_upload_asset: package-registry path -------------------------

/// With a `GitlabPackageRegistrySpec`, the file is PUT to the Generic
/// Package Registry under `.../packages/generic/{project}/{version}/{file}`
/// with the raw bytes, and the resulting upload URL is used verbatim as
/// the release link's `url`.
#[tokio::test]
async fn upload_asset_package_registry_puts_then_links() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("asset.tar.gz");
    tokio::fs::write(&file, b"RAW-BYTES")
        .await
        .expect("write fixture");

    let (addr, log) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "PUT",
            path_pattern: "/projects/myorg%2Fmyproj/packages/generic/myproj/1.0.0/asset.tar.gz",
            response: http_json("201 Created", "{}".to_string()),
            times: None,
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/version",
            response: http_json(
                "200 OK",
                serde_json::json!({"version": "17.2.0"}).to_string(),
            ),
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/projects/myorg%2Fmyproj/releases/v1.0.0/assets/links",
            response: http_json("201 Created", serde_json::json!({"id": 1}).to_string()),
            times: None,
        },
    ]);

    let client = test_client();
    let policy = fast_policy(2);
    let api_url = format!("http://{addr}");
    let ctx = GitlabCtx {
        client: &client,
        api_url: &api_url,
        project_id: "myorg/myproj",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let asset = GitlabAssetSpec {
        file_path: &file,
        file_name: "asset.tar.gz",
    };
    let pkg = GitlabPackageRegistrySpec {
        project_name: "myproj",
        version: "1.0.0",
    };

    gitlab_upload_asset(
        &ctx,
        "v1.0.0",
        &asset,
        Some(&pkg),
        "https://gitlab.com/myorg/myproj",
        false,
    )
    .await
    .expect("package-registry upload should succeed");

    let entries = log.lock().unwrap();
    let put = entries
        .iter()
        .find(|e| e.method == "PUT")
        .expect("package-registry PUT issued");
    assert_eq!(
        put.path, "/projects/myorg%2Fmyproj/packages/generic/myproj/1.0.0/asset.tar.gz",
        "PUT targets the generic package registry path"
    );
    assert!(
        put.body.contains("RAW-BYTES"),
        "registry PUT carries the raw file bytes (not multipart)"
    );

    let link = entries
        .iter()
        .find(|e| e.path == "/projects/myorg%2Fmyproj/releases/v1.0.0/assets/links")
        .expect("link POST issued");
    let payload: serde_json::Value = serde_json::from_str(&link.body).expect("link body is JSON");
    assert_eq!(
        payload["url"],
        format!("{api_url}/projects/myorg%2Fmyproj/packages/generic/myproj/1.0.0/asset.tar.gz"),
        "registry link url is the upload URL verbatim"
    );
}

/// A 5xx on the package-registry PUT is retried; the second attempt
/// succeeds and the flow proceeds to the version probe + link POST.
#[tokio::test]
async fn upload_asset_package_registry_retries_5xx_on_put() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("a.bin");
    tokio::fs::write(&file, b"xyz")
        .await
        .expect("write fixture");

    let (addr, log) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "PUT",
            path_pattern: "/projects/o%2Fr/packages/generic/p/1.0.0/a.bin",
            response: "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
            times: Some(1),
        },
        ScriptedRoute {
            method: "PUT",
            path_pattern: "/projects/o%2Fr/packages/generic/p/1.0.0/a.bin",
            response: http_json("201 Created", "{}".to_string()),
            times: None,
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/version",
            response: http_json(
                "200 OK",
                serde_json::json!({"version": "17.0.0"}).to_string(),
            ),
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/projects/o%2Fr/releases/v1.0.0/assets/links",
            response: http_json("201 Created", serde_json::json!({"id": 1}).to_string()),
            times: None,
        },
    ]);

    let client = test_client();
    let policy = fast_policy(3);
    let api_url = format!("http://{addr}");
    let ctx = GitlabCtx {
        client: &client,
        api_url: &api_url,
        project_id: "o/r",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let asset = GitlabAssetSpec {
        file_path: &file,
        file_name: "a.bin",
    };
    let pkg = GitlabPackageRegistrySpec {
        project_name: "p",
        version: "1.0.0",
    };

    gitlab_upload_asset(&ctx, "v1.0.0", &asset, Some(&pkg), "https://x", false)
        .await
        .expect("upload should succeed after 5xx retry");
    let entries = log.lock().unwrap();
    let puts = entries.iter().filter(|e| e.method == "PUT").count();
    assert_eq!(puts, 2, "503 registry PUT retried once, then 201");
}

// -- gitlab_upload_asset: link-creation error handling ------------------

/// A link POST that returns a non-success status with `replace_existing`
/// FALSE bails immediately (no list/delete/retry) and surfaces the asset
/// name + status in the error.
#[tokio::test]
async fn upload_asset_link_conflict_without_replace_bails() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("a.bin");
    tokio::fs::write(&file, b"xyz")
        .await
        .expect("write fixture");

    let (addr, log) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "PUT",
            path_pattern: "/projects/o%2Fr/packages/generic/p/1.0.0/a.bin",
            response: http_json("201 Created", "{}".to_string()),
            times: None,
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/version",
            response: http_json(
                "200 OK",
                serde_json::json!({"version": "17.0.0"}).to_string(),
            ),
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/projects/o%2Fr/releases/v1.0.0/assets/links",
            response: http_json(
                "400 Bad Request",
                serde_json::json!({"message": "already exists"}).to_string(),
            ),
            times: None,
        },
    ]);

    let client = test_client();
    let policy = fast_policy(2);
    let api_url = format!("http://{addr}");
    let ctx = GitlabCtx {
        client: &client,
        api_url: &api_url,
        project_id: "o/r",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let asset = GitlabAssetSpec {
        file_path: &file,
        file_name: "a.bin",
    };
    let pkg = GitlabPackageRegistrySpec {
        project_name: "p",
        version: "1.0.0",
    };

    let err = gitlab_upload_asset(&ctx, "v1.0.0", &asset, Some(&pkg), "https://x", false)
        .await
        .expect_err("400 link with replace=false must bail");
    let chain = format!("{err:#}");
    assert!(
        chain.contains("create release link for 'a.bin' failed (HTTP 400"),
        "error must name asset + status, got: {chain}"
    );
    let entries = log.lock().unwrap();
    assert!(
        entries.iter().all(|e| e.method != "DELETE"),
        "replace=false must not list/delete the conflicting link"
    );
}

/// A link POST returning 500 (server error, not a 400/422 duplicate) with
/// `replace_existing` TRUE still bails — the delete-and-retry path is
/// reserved for 400/422 conflicts, not 5xx.
#[tokio::test]
async fn upload_asset_link_500_with_replace_still_bails() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("a.bin");
    tokio::fs::write(&file, b"xyz")
        .await
        .expect("write fixture");

    let (addr, log) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "PUT",
            path_pattern: "/projects/o%2Fr/packages/generic/p/1.0.0/a.bin",
            response: http_json("201 Created", "{}".to_string()),
            times: None,
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/version",
            response: http_json(
                "200 OK",
                serde_json::json!({"version": "17.0.0"}).to_string(),
            ),
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/projects/o%2Fr/releases/v1.0.0/assets/links",
            response: "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
            times: None,
        },
    ]);

    let client = test_client();
    let policy = fast_policy(2);
    let api_url = format!("http://{addr}");
    let ctx = GitlabCtx {
        client: &client,
        api_url: &api_url,
        project_id: "o/r",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let asset = GitlabAssetSpec {
        file_path: &file,
        file_name: "a.bin",
    };
    let pkg = GitlabPackageRegistrySpec {
        project_name: "p",
        version: "1.0.0",
    };

    let err = gitlab_upload_asset(&ctx, "v1.0.0", &asset, Some(&pkg), "https://x", true)
        .await
        .expect_err("500 link must bail even with replace=true");
    assert!(
        format!("{err:#}").contains("HTTP 500"),
        "error must carry the 500 status, got: {err:#}"
    );
    let entries = log.lock().unwrap();
    assert!(
        entries.iter().all(|e| e.method != "DELETE"),
        "a 500 (not 400/422) must not trigger the delete-and-retry path"
    );
}

// -- detect_pre_v17_gitlab_with_env -------------------------------------

/// The `CI_SERVER_VERSION` env var short-circuits the version detection:
/// no `/version` HTTP call is made when the env reports a version.
#[tokio::test]
async fn detect_pre_v17_env_short_circuits_without_http() {
    // Responder serves nothing useful; if a /version call escaped it would
    // 404 and (conservatively) report pre-v17 — so the assertions below
    // distinguish the env path from the HTTP path.
    let (addr, log) = spawn_scripted_responder(vec![]);
    let client = test_client();
    let api_url = format!("http://{addr}");

    let env16 = MapEnvSource::new().with("CI_SERVER_VERSION", "16.5.0");
    assert!(
        detect_pre_v17_gitlab_with_env(&client, &api_url, &env16).await,
        "16.x via env => pre-v17"
    );

    let env17 = MapEnvSource::new().with("CI_SERVER_VERSION", "17.1.0");
    assert!(
        !detect_pre_v17_gitlab_with_env(&client, &api_url, &env17).await,
        "17.x via env => not pre-v17"
    );

    assert!(
        log.lock().unwrap().is_empty(),
        "env short-circuit must make zero HTTP calls"
    );
}

/// With no `CI_SERVER_VERSION` env, detection falls back to a GET
/// `/version` API call and parses the `version` field.
#[tokio::test]
async fn detect_pre_v17_falls_back_to_version_api() {
    let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "GET",
        path_pattern: "/version",
        response: http_json(
            "200 OK",
            serde_json::json!({"version": "16.11.0"}).to_string(),
        ),
        times: None,
    }]);
    let client = test_client();
    let api_url = format!("http://{addr}");
    let env = MapEnvSource::new();

    assert!(
        detect_pre_v17_gitlab_with_env(&client, &api_url, &env).await,
        "16.x from /version API => pre-v17"
    );
    let entries = log.lock().unwrap();
    assert_eq!(entries.len(), 1, "exactly one /version GET");
    assert_eq!(entries[0].path, "/version");
}

/// When the `/version` API call fails (no matching route => 404), the
/// detector conservatively defaults to pre-v17 (`true`).
#[tokio::test]
async fn detect_pre_v17_defaults_true_on_api_failure() {
    let (addr, _log) = spawn_scripted_responder(vec![]);
    let client = test_client();
    let api_url = format!("http://{addr}");
    let env = MapEnvSource::new();

    assert!(
        detect_pre_v17_gitlab_with_env(&client, &api_url, &env).await,
        "an unreachable/failed /version probe defaults to pre-v17"
    );
}

/// A `/version` 200 whose JSON lacks a string `version` field cannot be
/// parsed into a major number, so the detector falls through to the
/// conservative pre-v17 default (`true`) rather than panicking.
#[tokio::test]
async fn detect_pre_v17_unparseable_version_field_defaults_true() {
    let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "GET",
        path_pattern: "/version",
        // `version` is a number, not the expected string — `as_str()`
        // yields None, so the body-parse branch defaults to pre-v17.
        response: http_json("200 OK", serde_json::json!({"version": 17}).to_string()),
        times: None,
    }]);
    let client = test_client();
    let api_url = format!("http://{addr}");
    let env = MapEnvSource::new();

    assert!(
        detect_pre_v17_gitlab_with_env(&client, &api_url, &env).await,
        "a 200 /version with a non-string version field defaults to pre-v17"
    );
    let entries = log.lock().unwrap();
    assert_eq!(entries.len(), 1, "exactly one /version GET was issued");
}

// -- gitlab_create_release: POST create error closure -------------------

/// A 404 GET probe routes to the create POST; when that POST returns a
/// 4xx the create-error closure formats the failing message (asset of
/// line range 349-354) — the error names the create call and carries the
/// HTTP status, and `max_attempts: 1` proves the 4xx fast-fails.
#[tokio::test]
async fn create_release_post_4xx_surfaces_error() {
    let (addr, log) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/projects/o%2Fr/releases/v1.0.0",
            response: "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/projects/o%2Fr/releases",
            response: http_json(
                "400 Bad Request",
                serde_json::json!({"message": "tag_name already exists"}).to_string(),
            ),
            times: None,
        },
    ]);

    let client = test_client();
    let policy = fast_policy(1);
    let api_url = format!("http://{addr}");
    let ctx = GitlabCtx {
        client: &client,
        api_url: &api_url,
        project_id: "o/r",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let spec = GitlabReleaseSpec {
        tag: "v1.0.0",
        name: "n",
        body: "b",
        commit: "abc",
        release_mode: "replace",
    };

    let err = gitlab_create_release(&ctx, &spec)
        .await
        .expect_err("create POST 400 must surface");
    assert!(
        format!("{err:#}").contains("create release failed (HTTP 400"),
        "error must name the create call + status, got: {err:#}"
    );
    let entries = log.lock().unwrap();
    let posts = entries.iter().filter(|e| e.method == "POST").count();
    assert_eq!(posts, 1, "a 4xx create POST fast-fails (no retry)");
}

// -- gitlab_upload_asset: project-uploads POST error closure ------------

/// A 4xx on the project-uploads POST formats the multipart-upload error
/// closure (asset of line range 713-724): the error names the failing
/// upload + status and no version probe / link POST is attempted.
#[tokio::test]
async fn upload_asset_project_uploads_4xx_surfaces_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("a.bin");
    tokio::fs::write(&file, b"xyz")
        .await
        .expect("write fixture");

    let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/projects/o%2Fr/uploads",
        response: http_json(
            "413 Payload Too Large",
            serde_json::json!({"message": "too big"}).to_string(),
        ),
        times: None,
    }]);

    let client = test_client();
    let policy = fast_policy(1);
    let api_url = format!("http://{addr}");
    let ctx = GitlabCtx {
        client: &client,
        api_url: &api_url,
        project_id: "o/r",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let asset = GitlabAssetSpec {
        file_path: &file,
        file_name: "a.bin",
    };
    let download_url = format!("http://{addr}");

    let err = gitlab_upload_asset(&ctx, "v1.0.0", &asset, None, &download_url, false)
        .await
        .expect_err("413 project-uploads POST must surface");
    assert!(
        format!("{err:#}").contains("project upload 'a.bin' failed (HTTP 413"),
        "error must name the upload + status, got: {err:#}"
    );
    let entries = log.lock().unwrap();
    assert!(
        entries.iter().all(|e| e.path != "/version"),
        "a failed upload must not proceed to the version probe / link POST"
    );
}

// -- gitlab_upload_asset: replace path — list-links failure -------------

/// When the link POST 422s and `replace_existing` is true, the backend
/// lists existing links; if that list GET fails (4xx), the list-error
/// closure fires and the function bails with the ORIGINAL link-conflict
/// status (asset of line range 472-535 Err arm), never issuing a DELETE.
#[tokio::test]
async fn upload_asset_replace_list_links_failure_bails_with_original_status() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("a.bin");
    tokio::fs::write(&file, b"xyz")
        .await
        .expect("write fixture");

    let (addr, log) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "PUT",
            path_pattern: "/projects/o%2Fr/packages/generic/p/1.0.0/a.bin",
            response: http_json("201 Created", "{}".to_string()),
            times: None,
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/version",
            response: http_json(
                "200 OK",
                serde_json::json!({"version": "17.0.0"}).to_string(),
            ),
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/projects/o%2Fr/releases/v1.0.0/assets/links",
            response: http_json(
                "422 Unprocessable Entity",
                serde_json::json!({"message": "already exists"}).to_string(),
            ),
            times: None,
        },
        // The list GET fails with a 4xx so the Err arm reports the
        // original 422 conflict.
        ScriptedRoute {
            method: "GET",
            path_pattern: "/projects/o%2Fr/releases/v1.0.0/assets/links",
            response: http_json(
                "403 Forbidden",
                serde_json::json!({"message": "no access"}).to_string(),
            ),
            times: None,
        },
    ]);

    let client = test_client();
    let policy = fast_policy(1);
    let api_url = format!("http://{addr}");
    let ctx = GitlabCtx {
        client: &client,
        api_url: &api_url,
        project_id: "o/r",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let asset = GitlabAssetSpec {
        file_path: &file,
        file_name: "a.bin",
    };
    let pkg = GitlabPackageRegistrySpec {
        project_name: "p",
        version: "1.0.0",
    };

    let err = gitlab_upload_asset(&ctx, "v1.0.0", &asset, Some(&pkg), "https://x", true)
        .await
        .expect_err("a failed link-list must bail");
    assert!(
        format!("{err:#}").contains("create release link for 'a.bin' failed (HTTP 422"),
        "the bail reports the ORIGINAL conflict status, got: {err:#}"
    );
    let entries = log.lock().unwrap();
    assert!(
        entries.iter().all(|e| e.method != "DELETE"),
        "a failed link-list must not proceed to DELETE"
    );
}

/// When replace_existing is true and the listed links contain NO entry
/// matching the asset name, no DELETE fires but the POST is still retried
/// (asset of line range 523-535 retry-after-delete closure). The retry
/// then succeeds.
#[tokio::test]
async fn upload_asset_replace_no_matching_link_retries_post() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("a.bin");
    tokio::fs::write(&file, b"xyz")
        .await
        .expect("write fixture");

    let other_links = serde_json::json!([{"id": 1, "name": "other.bin"}]).to_string();
    let (addr, log) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "PUT",
            path_pattern: "/projects/o%2Fr/packages/generic/p/1.0.0/a.bin",
            response: http_json("201 Created", "{}".to_string()),
            times: None,
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/version",
            response: http_json(
                "200 OK",
                serde_json::json!({"version": "17.0.0"}).to_string(),
            ),
            times: None,
        },
        // First link POST conflicts (422)...
        ScriptedRoute {
            method: "POST",
            path_pattern: "/projects/o%2Fr/releases/v1.0.0/assets/links",
            response: http_json(
                "422 Unprocessable Entity",
                serde_json::json!({"message": "already exists"}).to_string(),
            ),
            times: Some(1),
        },
        // ...list returns only a non-matching link (no DELETE)...
        ScriptedRoute {
            method: "GET",
            path_pattern: "/projects/o%2Fr/releases/v1.0.0/assets/links",
            response: http_json("200 OK", other_links),
            times: None,
        },
        // ...retry POST succeeds.
        ScriptedRoute {
            method: "POST",
            path_pattern: "/projects/o%2Fr/releases/v1.0.0/assets/links",
            response: http_json("201 Created", serde_json::json!({"id": 9}).to_string()),
            times: None,
        },
    ]);

    let client = test_client();
    let policy = fast_policy(2);
    let api_url = format!("http://{addr}");
    let ctx = GitlabCtx {
        client: &client,
        api_url: &api_url,
        project_id: "o/r",
        policy: &policy,
        deadline: None,
        log: tlog(),
    };
    let asset = GitlabAssetSpec {
        file_path: &file,
        file_name: "a.bin",
    };
    let pkg = GitlabPackageRegistrySpec {
        project_name: "p",
        version: "1.0.0",
    };

    gitlab_upload_asset(&ctx, "v1.0.0", &asset, Some(&pkg), "https://x", true)
        .await
        .expect("retry POST after a no-match list should succeed");
    let entries = log.lock().unwrap();
    assert!(
        entries.iter().all(|e| e.method != "DELETE"),
        "no name match => no DELETE, but the POST is still retried"
    );
    let posts = entries
        .iter()
        .filter(|e| e.method == "POST" && e.path.ends_with("/assets/links"))
        .count();
    assert_eq!(posts, 2, "the conflicting POST is retried exactly once");
}

// -- run_gitlab_backend orchestration -----------------------------------
//
// These drive the production orchestrator (token resolution, URL
// resolution, create-release, parallel upload loop, html_url
// composition) against the scripted responder. The Context is built
// with token_type=GitLab so `resolve_release_repo` reads
// `release.gitlab`, and `gitlab_urls.{api,download}` point at the
// loopback so every API call is observable. Mirrors github::backend's
// `run_github_backend` end-to-end tests.

use anodizer_core::config::{
    CrateConfig, GitLabUrlsConfig, ReleaseConfig, RetryConfig, ScmRepoConfig,
};
use anodizer_core::context::Context;
use anodizer_core::log::{StageLogger, Verbosity};

fn tlog() -> &'static StageLogger {
    anodizer_core::test_helpers::test_logger()
}
use anodizer_core::scm::ScmTokenType;
use anodizer_core::test_helpers::TestContextBuilder;

/// Build a GitLab-flavoured Context: token_type=GitLab, a fast retry
/// policy, and `gitlab_urls.{api,download}` pointed at the loopback so
/// every API call routes through the scripted responder.
fn build_gitlab_ctx(api_base: &str, use_pkg_registry: bool) -> Context {
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.0.0")
        .commit("deadbeef")
        .token(Some("glpat-test".to_string()))
        .build();
    ctx.token_type = ScmTokenType::GitLab;
    ctx.config.gitlab_urls = Some(GitLabUrlsConfig {
        api: Some(api_base.to_string()),
        download: Some(api_base.to_string()),
        skip_tls_verify: None,
        use_package_registry: Some(use_pkg_registry),
        use_job_token: None,
    });
    ctx.config.retry = Some(RetryConfig {
        attempts: 3,
        delay: anodizer_core::config::HumanDuration(std::time::Duration::from_millis(1)),
        max_delay: anodizer_core::config::HumanDuration(std::time::Duration::from_millis(2)),
        max_elapsed: None,
    });
    ctx
}

/// A `CrateConfig` whose `release.gitlab` points at owner=o, name=r.
fn build_gitlab_crate_cfg() -> CrateConfig {
    let mut crate_cfg = CrateConfig {
        name: "demo".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ Version }}".to_string()),
        ..Default::default()
    };
    crate_cfg.release = Some(ReleaseConfig {
        gitlab: Some(ScmRepoConfig {
            owner: "o".to_string(),
            name: "r".to_string(),
            token: None,
        }),
        mode: Some("replace".to_string()),
        ..Default::default()
    });
    crate_cfg
}

fn default_gitlab_spec() -> GitlabBackendSpec<'static> {
    GitlabBackendSpec {
        tag: "v1.0.0",
        release_name: "Release v1.0.0",
        release_body: "the body",
        release_mode: "replace",
        skip_upload: false,
        replace_existing_draft: false,
        use_existing_draft: false,
        replace_existing_artifacts: false,
    }
}

/// End-to-end: a fresh release (404 GET probe → POST create) plus one
/// package-registry asset upload. Asserts the success payload
/// `(html_url, download, owner, repo)` and that the create POST + the
/// registry PUT + the link POST all hit the loopback.
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
            path_pattern: "/projects/o%2Fr/releases/v1.0.0",
            response: "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/projects/o%2Fr/releases",
            response: http_json(
                "201 Created",
                serde_json::json!({"tag_name": "v1.0.0"}).to_string(),
            ),
            times: None,
        },
        // The shared upload loop's pre-upload probe lists the release's
        // links; an empty list classifies the asset Absent so the upload
        // proceeds.
        ScriptedRoute {
            method: "GET",
            path_pattern: "/projects/o%2Fr/releases/v1.0.0/assets/links",
            response: "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]",
            times: None,
        },
        ScriptedRoute {
            method: "PUT",
            path_pattern: "/projects/o%2Fr/packages/generic/demo/1.0.0/demo.tar.gz",
            response: http_json("201 Created", "{}".to_string()),
            times: None,
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/version",
            response: http_json(
                "200 OK",
                serde_json::json!({"version": "17.0.0"}).to_string(),
            ),
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/projects/o%2Fr/releases/v1.0.0/assets/links",
            response: http_json("201 Created", serde_json::json!({"id": 1}).to_string()),
            times: None,
        },
    ];
    let (_addr, log) = spawn_scripted_responder_on(listener, |_| routes);

    let api_base = format!("http://{addr}");
    let ctx = build_gitlab_ctx(&api_base, true);
    let crate_cfg = build_gitlab_crate_cfg();
    let release_cfg = crate_cfg.release.as_ref().expect("release cfg");
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let log_stage = StageLogger::new("release", Verbosity::Normal);
    let token = Some("glpat-test".to_string());
    let env = GitlabBackendEnv {
        rt: &rt,
        ctx: &ctx,
        log: &log_stage,
        token: &token,
    };
    let artifacts = vec![(artifact, Some("demo.tar.gz".to_string()))];

    let out = run_gitlab_backend(
        &env,
        &crate_cfg,
        release_cfg,
        &default_gitlab_spec(),
        &artifacts,
    )
    .expect("run_gitlab_backend should succeed")
    .expect("returns Some on success");
    let (html_url, download, owner, repo) = out;
    assert_eq!(owner, "o");
    assert_eq!(repo, "r");
    assert_eq!(
        download, api_base,
        "download base echoes gitlab_urls.download"
    );
    assert_eq!(
        html_url,
        format!("{api_base}/o/r/-/releases/v1.0.0"),
        "html_url composes from download base + owner/repo/-/releases/tag"
    );

    let entries = log.lock().unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e.method == "POST" && e.path == "/projects/o%2Fr/releases"),
        "the create POST hit the loopback"
    );
    let put = entries
        .iter()
        .find(|e| e.method == "PUT")
        .expect("the package-registry PUT was issued");
    assert!(
        put.body.contains("PAYLOAD"),
        "the registry PUT carried the artifact bytes"
    );
    assert!(
        entries
            .iter()
            .any(|e| e.method == "POST" && e.path.ends_with("/assets/links")),
        "the release-link POST was issued"
    );
}

/// The resume-idempotency pin: a re-run whose artifact is ALREADY linked
/// on the release with byte-identical size must NOT re-upload — no
/// package-registry PUT, no link POST, no link DELETE. Before the shared
/// upload loop, run_gitlab_backend had no pre-upload probe at all, so
/// this exact scenario re-uploaded the bytes and then hit the duplicate
/// link (this test fails on that code: the PUT/POST routes below get
/// exercised).
#[test]
fn run_backend_rerun_with_identical_remote_asset_skips_upload() {
    let dir = tempfile::tempdir().expect("tempdir");
    let artifact = dir.path().join("demo.tar.gz");
    std::fs::write(&artifact, b"PAYLOAD").expect("write artifact");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    let links_json = http_json(
        "200 OK",
        serde_json::json!([{
            "name": "demo.tar.gz",
            "id": 9,
            "url": format!("http://{addr}/head/demo.tar.gz"),
        }])
        .to_string(),
    );
    let routes = vec![
        // Re-run: the release already exists, so the create path GETs it
        // and PUTs the mode-composed update.
        ScriptedRoute {
            method: "GET",
            path_pattern: "/projects/o%2Fr/releases/v1.0.0",
            response: http_json(
                "200 OK",
                serde_json::json!({"description": "old body"}).to_string(),
            ),
            times: None,
        },
        ScriptedRoute {
            method: "PUT",
            path_pattern: "/projects/o%2Fr/releases/v1.0.0",
            response: http_json("200 OK", "{}".to_string()),
            times: None,
        },
        // Pre-upload probe: the link inventory already carries the asset.
        ScriptedRoute {
            method: "GET",
            path_pattern: "/projects/o%2Fr/releases/v1.0.0/assets/links",
            response: links_json,
            times: None,
        },
        // Size probe: byte-identical to the local artifact (7 bytes).
        ScriptedRoute {
            method: "HEAD",
            path_pattern: "/head/demo.tar.gz",
            response: "HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\n",
            times: None,
        },
        // The routes a re-upload WOULD hit: they must stay untouched.
        ScriptedRoute {
            method: "PUT",
            path_pattern: "/projects/o%2Fr/packages/generic/demo/1.0.0/demo.tar.gz",
            response: http_json("201 Created", "{}".to_string()),
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/projects/o%2Fr/releases/v1.0.0/assets/links",
            response: http_json("201 Created", serde_json::json!({"id": 1}).to_string()),
            times: None,
        },
    ];
    let (_addr, log) = spawn_scripted_responder_on(listener, |_| routes);

    let api_base = format!("http://{addr}");
    let ctx = build_gitlab_ctx(&api_base, true);
    let crate_cfg = build_gitlab_crate_cfg();
    let release_cfg = crate_cfg.release.as_ref().expect("release cfg");
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let (log_stage, cap) = StageLogger::with_capture("release", Verbosity::Normal);
    let token = Some("glpat-test".to_string());
    let env = GitlabBackendEnv {
        rt: &rt,
        ctx: &ctx,
        log: &log_stage,
        token: &token,
    };
    let artifacts = vec![(artifact, Some("demo.tar.gz".to_string()))];

    run_gitlab_backend(
        &env,
        &crate_cfg,
        release_cfg,
        &default_gitlab_spec(),
        &artifacts,
    )
    .expect("run_gitlab_backend should succeed")
    .expect("returns Some on success");

    let messages = cap.all_messages();
    assert!(
        messages
            .iter()
            .any(|(_, m)| m == "skipped byte-identical asset demo.tar.gz — already uploaded"),
        "the drain must report the idempotent skip: {messages:?}"
    );
    assert!(
        !messages
            .iter()
            .any(|(_, m)| m.contains("uploaded artifact")),
        "an idempotent skip must not claim an upload: {messages:?}"
    );

    let entries = log.lock().unwrap();
    assert!(
        entries.iter().any(|e| e.method == "HEAD"),
        "the size probe HEAD was issued"
    );
    assert!(
        !entries
            .iter()
            .any(|e| e.method == "PUT" && e.path.contains("/packages/generic/")),
        "byte-identical remote asset must NOT be re-uploaded, got: {:?}",
        entries
            .iter()
            .map(|e| format!("{} {}", e.method, e.path))
            .collect::<Vec<_>>()
    );
    assert!(
        !entries
            .iter()
            .any(|e| e.method == "POST" && e.path.ends_with("/assets/links")),
        "no duplicate link POST on an idempotent re-run"
    );
    assert!(
        !entries.iter().any(|e| e.method == "DELETE"),
        "an idempotent skip must not delete the published link"
    );
}

/// Size mismatch + `replace_existing_artifacts: true`: the shared loop
/// deletes the stale link first, then re-uploads and re-links.
#[test]
fn run_backend_rerun_with_differing_remote_asset_replaces_when_opted_in() {
    let dir = tempfile::tempdir().expect("tempdir");
    let artifact = dir.path().join("demo.tar.gz");
    std::fs::write(&artifact, b"PAYLOAD").expect("write artifact");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    let links_json = http_json(
        "200 OK",
        serde_json::json!([{
            "name": "demo.tar.gz",
            "id": 9,
            "url": format!("http://{addr}/head/demo.tar.gz"),
        }])
        .to_string(),
    );
    let routes = vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/projects/o%2Fr/releases/v1.0.0",
            response: http_json(
                "200 OK",
                serde_json::json!({"description": "old body"}).to_string(),
            ),
            times: None,
        },
        ScriptedRoute {
            method: "PUT",
            path_pattern: "/projects/o%2Fr/releases/v1.0.0",
            response: http_json("200 OK", "{}".to_string()),
            times: None,
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/projects/o%2Fr/releases/v1.0.0/assets/links",
            response: links_json,
            times: None,
        },
        // Remote size 999 ≠ local 7 → stale bytes, user opted into
        // replacement.
        ScriptedRoute {
            method: "HEAD",
            path_pattern: "/head/demo.tar.gz",
            response: "HTTP/1.1 200 OK\r\nContent-Length: 999\r\n\r\n",
            times: None,
        },
        ScriptedRoute {
            method: "DELETE",
            path_pattern: "/projects/o%2Fr/releases/v1.0.0/assets/links/9",
            response: http_json("200 OK", "{}".to_string()),
            times: None,
        },
        ScriptedRoute {
            method: "PUT",
            path_pattern: "/projects/o%2Fr/packages/generic/demo/1.0.0/demo.tar.gz",
            response: http_json("201 Created", "{}".to_string()),
            times: None,
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/version",
            response: http_json(
                "200 OK",
                serde_json::json!({"version": "17.0.0"}).to_string(),
            ),
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/projects/o%2Fr/releases/v1.0.0/assets/links",
            response: http_json("201 Created", serde_json::json!({"id": 10}).to_string()),
            times: None,
        },
    ];
    let (_addr, log) = spawn_scripted_responder_on(listener, |_| routes);

    let api_base = format!("http://{addr}");
    let ctx = build_gitlab_ctx(&api_base, true);
    let crate_cfg = build_gitlab_crate_cfg();
    let release_cfg = crate_cfg.release.as_ref().expect("release cfg");
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let log_stage = StageLogger::new("release", Verbosity::Normal);
    let token = Some("glpat-test".to_string());
    let env = GitlabBackendEnv {
        rt: &rt,
        ctx: &ctx,
        log: &log_stage,
        token: &token,
    };
    let artifacts = vec![(artifact, Some("demo.tar.gz".to_string()))];
    let mut spec = default_gitlab_spec();
    spec.replace_existing_artifacts = true;

    run_gitlab_backend(&env, &crate_cfg, release_cfg, &spec, &artifacts)
        .expect("run_gitlab_backend should succeed")
        .expect("returns Some on success");

    let entries = log.lock().unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e.method == "DELETE" && e.path.ends_with("/assets/links/9")),
        "the stale link must be deleted before re-upload"
    );
    let put = entries
        .iter()
        .find(|e| e.method == "PUT" && e.path.contains("/packages/generic/"))
        .expect("the replacement upload PUT was issued");
    assert!(
        put.body.contains("PAYLOAD"),
        "re-upload carries fresh bytes"
    );
    assert!(
        entries
            .iter()
            .any(|e| e.method == "POST" && e.path.ends_with("/assets/links")),
        "the replacement link POST was issued"
    );
}

/// With `skip_upload` set, the orchestrator creates the release but
/// issues no upload calls (no PUT / no /version / no assets/links).
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
            path_pattern: "/projects/o%2Fr/releases/v1.0.0",
            response: "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/projects/o%2Fr/releases",
            response: http_json(
                "201 Created",
                serde_json::json!({"tag_name": "v1.0.0"}).to_string(),
            ),
            times: None,
        },
    ];
    let (_addr, log) = spawn_scripted_responder_on(listener, |_| routes);

    let api_base = format!("http://{addr}");
    let ctx = build_gitlab_ctx(&api_base, true);
    let crate_cfg = build_gitlab_crate_cfg();
    let release_cfg = crate_cfg.release.as_ref().expect("release cfg");
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let log_stage = StageLogger::new("release", Verbosity::Normal);
    let token = Some("glpat-test".to_string());
    let env = GitlabBackendEnv {
        rt: &rt,
        ctx: &ctx,
        log: &log_stage,
        token: &token,
    };
    let mut spec = default_gitlab_spec();
    spec.skip_upload = true;
    let artifacts = vec![(artifact, Some("demo.tar.gz".to_string()))];

    run_gitlab_backend(&env, &crate_cfg, release_cfg, &spec, &artifacts)
        .expect("run_gitlab_backend should succeed")
        .expect("returns Some");

    let entries = log.lock().unwrap();
    assert!(
        entries
            .iter()
            .all(|e| e.method != "PUT" && e.path != "/version"),
        "skip_upload must issue no upload calls, got: {:?}",
        entries.iter().map(|e| &e.path).collect::<Vec<_>>()
    );
}

/// GitLab has no draft releases, so `replace_existing_draft` /
/// `use_existing_draft` are no-ops that only emit a warning. With both set
/// the orchestrator still creates the release and uploads the asset (the
/// draft flags change nothing about the issued HTTP calls).
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
            path_pattern: "/projects/o%2Fr/releases/v1.0.0",
            response: "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/projects/o%2Fr/releases",
            response: http_json(
                "201 Created",
                serde_json::json!({"tag_name": "v1.0.0"}).to_string(),
            ),
            times: None,
        },
        // The shared upload loop's pre-upload probe lists the release's
        // links; an empty list classifies the asset Absent so the upload
        // proceeds.
        ScriptedRoute {
            method: "GET",
            path_pattern: "/projects/o%2Fr/releases/v1.0.0/assets/links",
            response: "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]",
            times: None,
        },
        ScriptedRoute {
            method: "PUT",
            path_pattern: "/projects/o%2Fr/packages/generic/demo/1.0.0/demo.tar.gz",
            response: http_json("201 Created", "{}".to_string()),
            times: None,
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/version",
            response: http_json(
                "200 OK",
                serde_json::json!({"version": "17.0.0"}).to_string(),
            ),
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/projects/o%2Fr/releases/v1.0.0/assets/links",
            response: http_json("201 Created", serde_json::json!({"id": 1}).to_string()),
            times: None,
        },
    ];
    let (_addr, log) = spawn_scripted_responder_on(listener, |_| routes);

    let api_base = format!("http://{addr}");
    let ctx = build_gitlab_ctx(&api_base, true);
    let crate_cfg = build_gitlab_crate_cfg();
    let release_cfg = crate_cfg.release.as_ref().expect("release cfg");
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let log_stage = StageLogger::new("release", Verbosity::Normal);
    let token = Some("glpat-test".to_string());
    let env = GitlabBackendEnv {
        rt: &rt,
        ctx: &ctx,
        log: &log_stage,
        token: &token,
    };
    let mut spec = default_gitlab_spec();
    spec.replace_existing_draft = true;
    spec.use_existing_draft = true;
    let artifacts = vec![(artifact, Some("demo.tar.gz".to_string()))];

    run_gitlab_backend(&env, &crate_cfg, release_cfg, &spec, &artifacts)
        .expect("draft flags must not abort the backend")
        .expect("returns Some");

    let entries = log.lock().unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e.method == "POST" && e.path == "/projects/o%2Fr/releases"),
        "the release is still created despite the no-op draft flags"
    );
    assert!(
        entries.iter().any(|e| e.method == "PUT"),
        "the asset upload still proceeds"
    );
}

/// A missing GitLab token short-circuits before any HTTP call with an
/// actionable bail naming GITLAB_TOKEN.
#[test]
fn run_backend_missing_token_bails() {
    let ctx = build_gitlab_ctx("http://unused.invalid", false);
    let crate_cfg = build_gitlab_crate_cfg();
    let release_cfg = crate_cfg.release.as_ref().expect("release cfg");
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let log_stage = StageLogger::new("release", Verbosity::Normal);
    let token: Option<String> = None;
    let env = GitlabBackendEnv {
        rt: &rt,
        ctx: &ctx,
        log: &log_stage,
        token: &token,
    };
    let artifacts: Vec<(std::path::PathBuf, Option<String>)> = Vec::new();

    let err = run_gitlab_backend(
        &env,
        &crate_cfg,
        release_cfg,
        &default_gitlab_spec(),
        &artifacts,
    )
    .expect_err("a missing token must bail");
    assert!(
        format!("{err:#}").contains("GITLAB_TOKEN"),
        "bail must name the missing env var, got: {err:#}"
    );
}

/// A crate without any `release.gitlab`/`release.github` config returns
/// `Ok(None)` (the caller `continue`s) rather than erroring.
#[test]
fn run_backend_no_gitlab_config_returns_none() {
    let ctx = build_gitlab_ctx("http://unused.invalid", false);
    let mut crate_cfg = build_gitlab_crate_cfg();
    // Strip the release config so resolve_release_repo yields None.
    crate_cfg.release = Some(ReleaseConfig {
        mode: Some("replace".to_string()),
        ..Default::default()
    });
    let release_cfg = crate_cfg.release.as_ref().expect("release cfg");
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let log_stage = StageLogger::new("release", Verbosity::Normal);
    let token = Some("glpat-test".to_string());
    let env = GitlabBackendEnv {
        rt: &rt,
        ctx: &ctx,
        log: &log_stage,
        token: &token,
    };
    let artifacts: Vec<(std::path::PathBuf, Option<String>)> = Vec::new();

    let out = run_gitlab_backend(
        &env,
        &crate_cfg,
        release_cfg,
        &default_gitlab_spec(),
        &artifacts,
    )
    .expect("no-config is not an error");
    assert!(out.is_none(), "absent gitlab config => Ok(None)");
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
            path_pattern: "/projects/o%2Fr/releases/v1.0.0",
            response: "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/projects/o%2Fr/releases",
            response: http_json(
                "201 Created",
                serde_json::json!({"tag_name": "v1.0.0"}).to_string(),
            ),
            times: None,
        },
    ];
    let (_addr, _log) = spawn_scripted_responder_on(listener, |_| routes);

    let api_base = format!("http://{addr}");
    let ctx = build_gitlab_ctx(&api_base, true);
    let crate_cfg = build_gitlab_crate_cfg();
    let release_cfg = crate_cfg.release.as_ref().expect("release cfg");
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let log_stage = StageLogger::new("release", Verbosity::Normal);
    let token = Some("glpat-test".to_string());
    let env = GitlabBackendEnv {
        rt: &rt,
        ctx: &ctx,
        log: &log_stage,
        token: &token,
    };
    let missing = std::path::PathBuf::from("/nonexistent/anodizer-test/missing.tar.gz");
    let artifacts = vec![(missing, Some("missing.tar.gz".to_string()))];

    let err = run_gitlab_backend(
        &env,
        &crate_cfg,
        release_cfg,
        &default_gitlab_spec(),
        &artifacts,
    )
    .expect_err("a missing artifact file must abort the upload loop");
    assert!(
        format!("{err:#}").contains("missing"),
        "error must report the missing artifact, got: {err:#}"
    );
}
